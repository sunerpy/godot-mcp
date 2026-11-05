//! The export tools: `export.presets.list`, `project.export` and
//! `project.export.status`. Cancelling an export is a session stop and runs
//! through [`SessionHandler::command`](super::SessionHandler).

use std::path::{Path, PathBuf};
use std::time::Duration;

use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::{ByteRange, OutcomeError};
use godot_mcp_engine::export::pack::{PackError, read_pack};
use godot_mcp_engine::export::presets::{ExportPreset, PresetValue, parse_presets};
use godot_mcp_engine::export::templates::{
    TemplateError, TemplateSet, default_templates_root, exports_android, target_for,
};
use godot_mcp_engine::supervision::journal::{
    OperationFamily as EngineOperationFamily, ProjectIdentityClaim, TransactionEnvelope, hex_sha256,
};
use godot_mcp_engine::supervision::protocol::{LaunchRequest, OutputCaps};
use serde_json::{Map, Value, json};

use super::{
    DEFAULT_EXPORT_SECONDS, EXPORT_FLAGS, KernelToolCall, MAX_EXPORT_SECONDS, PACK_FLAGS,
    PATCH_FLAGS, SessionHandler, SessionKind, boolean, integer, invalid, outcome_error,
    required_string, string,
};
use crate::artifacts::sha256_hex;
use crate::effect::ProcessEffectIntent;
use crate::effect::boundary::SessionStartRequest;
use crate::engine_plane::EngineRuntime;
use crate::engine_plane::runtime::{engine_transport, journal_transport};
use crate::engine_plane::session::android::{
    AndroidExport, ensure_debug_keystore, export_environment, prepare_export, sdk_error,
};
use crate::engine_plane::session::export::{
    BasePack, ExportKind, ExportOutput, ExportPlan, MAX_EXPORT_BYTES, archive_output_name,
    link_templates, output_name, remove_staging, template_label_of,
};
use crate::engine_plane::session::{SessionManager, new_session_id, now_unix_ms};
use crate::journal::OperationFamily;
use crate::router::{RouteEffect, RouteRequest};

/// The presets file, project-relative.
const PRESETS_FILE: &str = "export_presets.cfg";
/// Godot's credentials store, where it keeps a preset's private options.
const CREDENTIALS_FILE: &str = ".godot/export_credentials.cfg";
/// The scheme and authority of an artifact URI.
const ARTIFACT_URI_PREFIX: &str = "godot-mcp://artifacts/";

/// A list of strings argument.
fn string_list(
    arguments: &Map<String, Value>,
    key: &str,
) -> Result<Option<Vec<String>>, OutcomeError> {
    let Some(value) = arguments.get(key) else {
        return Ok(None);
    };
    let items = value
        .as_array()
        .filter(|items| (1..=16).contains(&items.len()))
        .ok_or_else(|| invalid(format!("`{key}` must be a list of 1 to 16 strings")))?;
    items
        .iter()
        .map(|item| {
            item.as_str()
                .filter(|text| (1..=1024).contains(&text.len()))
                .map(str::to_owned)
                .ok_or_else(|| invalid(format!("`{key}` must be a list of 1 to 16 strings")))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

/// Write `bytes` to a new private file at `path`.
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|error| error.to_string())?;
    std::io::Write::write_all(&mut file, bytes).map_err(|error| error.to_string())
}

/// The private session directory of an export that has not started yet. It
/// is removed when the start does not create the session; once it does, the
/// session owns it, and its staged files go when the export has ended.
struct Staging {
    directory: PathBuf,
    armed: bool,
}

impl Staging {
    fn open(directory: &Path) -> Result<Self, OutcomeError> {
        let private = |path: &Path| {
            godot_mcp_domain::sysfs::open_private_state_dir(path)
                .map(|_| ())
                .map_err(|error| outcome_error(ErrorKind::Internal, error.to_string()))
        };
        if let Some(sessions) = directory.parent() {
            private(sessions)?;
        }
        private(directory)?;
        Ok(Self {
            directory: directory.to_path_buf(),
            armed: true,
        })
    }

    /// Hand the directory over to session `id` when the start created it.
    fn settle(mut self, sessions: &SessionManager, id: &str) {
        if let Ok(session) = sessions.session(id) {
            self.armed = false;
            if session.state().is_terminal() {
                remove_staging(&self.directory);
            }
        }
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }
}

/// Where Godot keeps export templates on this host, from the server's own
/// environment, the way Godot itself looks.
fn templates_root() -> Option<PathBuf> {
    let variable = |name: &str| std::env::var_os(name).map(PathBuf::from);
    default_templates_root(
        variable("XDG_DATA_HOME").as_deref(),
        variable("HOME").as_deref(),
        variable("APPDATA").as_deref(),
    )
}

/// The export path a preset names, when it is safe to show: a relative one.
fn shown_export_path(preset: &ExportPreset) -> Value {
    let path = preset.export_path();
    if path.is_empty() || path.starts_with('/') || path.contains(':') || path.contains('\\') {
        Value::Null
    } else {
        Value::String(path.to_owned())
    }
}

fn template_refusal(error: &TemplateError) -> OutcomeError {
    let kind = match error {
        TemplateError::Unsupported(_) | TemplateError::CustomTemplate { .. } => {
            ErrorKind::Unsupported
        }
        TemplateError::Missing { .. }
        | TemplateError::WrongVersion { .. }
        | TemplateError::MissingFile { .. } => ErrorKind::Unavailable,
    };
    outcome_error(kind, error.to_string())
}

impl SessionHandler {
    /// The project's presets, or none when it has no presets file.
    fn presets(&self) -> Result<Option<Vec<ExportPreset>>, OutcomeError> {
        let root = self.engine.root()?;
        let bytes = match root.read(Path::new(PRESETS_FILE)) {
            Ok(bytes) => bytes,
            Err(godot_mcp_domain::safe_root::SafeRootError::StalePreimage { .. }) => {
                return Ok(None);
            }
            Err(error) => {
                return Err(outcome_error(
                    error.error_kind(),
                    format!("{PRESETS_FILE} could not be read as project source"),
                ));
            }
        };
        let text = String::from_utf8(bytes)
            .map_err(|_| invalid(format!("{PRESETS_FILE} is not UTF-8 text")))?;
        parse_presets(&text)
            .map(Some)
            .map_err(|error| invalid(error.to_string()))
    }

    pub(super) async fn export_presets(&self) -> Result<Value, OutcomeError> {
        let Some(presets) = self.presets()? else {
            return Ok(json!({ "presets_file": false, "presets": [] }));
        };
        // Template facts need the exact build; without one they stay unknown.
        let label = self
            .engine
            .resolve_engine()
            .await
            .ok()
            .and_then(|resolved| template_label_of(&resolved.version_line));
        let root = templates_root();
        let listed: Vec<Value> = presets
            .iter()
            .map(|preset| {
                let template = match target_for(preset, false) {
                    Err(error) => json!({ "unsupported": error.to_string() }),
                    Ok(target) => match (&label, &root) {
                        (Some(label), Some(root)) => {
                            let resolved = TemplateSet::resolve(root, label)
                                .and_then(|set| set.template(&target));
                            json!({
                                "file": target.template,
                                "label": label,
                                "installed": resolved.is_ok(),
                                "reason": resolved.err().map(|error| error.to_string()),
                                "runnable_here": target.runnable_here,
                            })
                        }
                        _ => json!({
                            "file": target.template,
                            "label": label,
                            "installed": Value::Null,
                            "reason": "no Godot build or template directory resolved, so which \
                                       templates match is unknown",
                            "runnable_here": target.runnable_here,
                        }),
                    },
                };
                json!({
                    "name": preset.name(),
                    "platform": preset.platform(),
                    "runnable": preset.runnable(),
                    "export_path": shown_export_path(preset),
                    "template": template,
                    "secret_options": preset.secret_bearing(),
                })
            })
            .collect();
        Ok(json!({ "presets_file": true, "presets": listed }))
    }

    pub(super) fn export_status(
        &self,
        access: &crate::kernel::AccessContext,
        arguments: &Map<String, Value>,
    ) -> Result<Value, OutcomeError> {
        let principal = access.principal().as_str().to_owned();
        let store = self.sessions().export_store();
        let now = now_unix_ms();
        let describe = |session: &crate::engine_plane::session::ManagedSession| -> Value {
            let mut value = session.public();
            if let (Value::Object(object), Some(export)) =
                (&mut value, session.export_public(store.as_deref(), now))
            {
                object.insert("export".to_owned(), export);
            }
            value
        };
        let Some(id) = string(arguments, "export")? else {
            let exports: Vec<Value> = self
                .sessions()
                .sessions_of(SessionKind::Export)
                .iter()
                .filter(|session| session.belongs_to(&principal))
                .map(|session| describe(session))
                .collect();
            return Ok(json!({ "exports": exports }));
        };
        let session = self.sessions().session(id)?;
        if !session.belongs_to(&principal) {
            return Err(not_this_callers(id));
        }
        if session.kind() != SessionKind::Export {
            return Err(invalid(format!(
                "{id} is a {} session, not an export",
                session.kind().as_str()
            )));
        }
        let mut value = describe(&session);
        if session.state() == crate::engine_plane::session::SessionState::Running
            && let Value::Object(object) = &mut value
        {
            object.insert(
                "progress_percent".to_owned(),
                json!(self.sessions().export_progress(id)),
            );
        }
        Ok(value)
    }

    pub(super) async fn export_start(&self, call: &KernelToolCall) -> Result<Value, OutcomeError> {
        let tool = self.operation.tool_name();
        let idempotency_id = self.idempotency(call)?;
        let arguments = &call.arguments;
        let access = &call.access;
        let engine = self.engine.as_ref();
        let name = required_string(arguments, "preset")?;
        let debug = match string(arguments, "mode")?.unwrap_or("release") {
            "release" => false,
            "debug" => true,
            _ => return Err(invalid("`mode` must be release or debug")),
        };
        let smoke = boolean(arguments, "smoke")?.unwrap_or(false);
        let lifetime = integer(arguments, "lifetimeSeconds", 1, MAX_EXPORT_SECONDS)?
            .unwrap_or(DEFAULT_EXPORT_SECONDS);
        let kind = match string(arguments, "kind")?.unwrap_or("project") {
            "project" => ExportKind::Project,
            "pack" => ExportKind::Pack,
            "patch" => ExportKind::Patch,
            _ => return Err(invalid("`kind` must be project, pack or patch")),
        };
        let pack_format = match string(arguments, "packFormat")? {
            None => None,
            Some("pck") => Some("pck"),
            Some("zip") => Some("zip"),
            Some(_) => return Err(invalid("`packFormat` must be pck or zip")),
        };
        let patches = string_list(arguments, "patches")?;
        if kind != ExportKind::Project {
            if debug {
                return Err(invalid(
                    "`mode` debug exports a build through the debug template; a pack or patch \
                     export holds the project's data and has no debug variant",
                ));
            }
            if smoke {
                return Err(invalid(
                    "`smoke` runs an exported build; a pack or patch export is data a game \
                     loads, not a program",
                ));
            }
        }
        if pack_format.is_some() && kind != ExportKind::Pack {
            return Err(invalid(
                "`packFormat` applies to a pack export; a patch is always a PCK",
            ));
        }
        match (&patches, kind) {
            (None, ExportKind::Patch) => {
                return Err(invalid(
                    "a patch export needs `patches`: the base packs its changed files are \
                     compared with",
                ));
            }
            (Some(_), ExportKind::Project | ExportKind::Pack) => {
                return Err(invalid("`patches` applies to a patch export only"));
            }
            _ => {}
        }

        // Everything that can refuse does so before anything is journaled or
        // launched.
        let presets = self.presets()?.ok_or_else(|| {
            outcome_error(
                ErrorKind::NotFound,
                format!("the project has no {PRESETS_FILE}"),
            )
        })?;
        let mut matching = presets.iter().filter(|preset| preset.name() == name);
        let preset = matching.next().ok_or_else(|| {
            outcome_error(
                ErrorKind::NotFound,
                format!("{PRESETS_FILE} has no preset named `{name}`"),
            )
        })?;
        if matching.next().is_some() {
            return Err(invalid(format!(
                "{PRESETS_FILE} has more than one preset named `{name}`"
            )));
        }
        let secrets = preset.secret_bearing();
        if !secrets.is_empty() {
            return Err(invalid(format!(
                "preset `{name}` carries secrets written into {PRESETS_FILE} ({}); this server \
                 does not export a preset whose file anyone who reads the project can read the \
                 secrets of — keep them in Godot's credentials store instead",
                secrets.join(", ")
            )));
        }
        // A pack holds the project's data whatever the platform; a build goes
        // through the platform's template.
        let target = match kind {
            ExportKind::Project => {
                Some(target_for(preset, debug).map_err(|error| template_refusal(&error))?)
            }
            ExportKind::Pack | ExportKind::Patch => None,
        };
        let resolved = engine.resolve_engine().await?;
        let flags: &[&str] = match kind {
            ExportKind::Project => &EXPORT_FLAGS,
            ExportKind::Pack => &PACK_FLAGS,
            ExportKind::Patch => &PATCH_FLAGS,
        };
        if let Some(flag) = flags
            .iter()
            .find(|flag| !resolved.binding.capabilities.contains(**flag))
        {
            return Err(outcome_error(
                ErrorKind::Unsupported,
                format!(
                    "this Godot build does not advertise {flag}, which a {} export needs; use an \
                     editor build that does",
                    kind.as_str()
                ),
            ));
        }
        let label = template_label_of(&resolved.version_line).ok_or_else(|| {
            outcome_error(
                ErrorKind::Unsupported,
                "this Godot build's version does not say which export templates match it",
            )
        })?;
        let templates = match &target {
            Some(target) => {
                let root = templates_root().ok_or_else(|| {
                    outcome_error(
                        ErrorKind::Unavailable,
                        "no data directory is set, so there is nowhere to find export templates",
                    )
                })?;
                let set = TemplateSet::resolve(&root, &label)
                    .map_err(|error| template_refusal(&error))?;
                set.template(target)
                    .map_err(|error| template_refusal(&error))?;
                Some(set)
            }
            None => None,
        };
        let android = target
            .as_ref()
            .is_some_and(|_| exports_android(preset.platform()));
        if android && !cfg!(target_os = "linux") {
            return Err(outcome_error(
                ErrorKind::Unsupported,
                "an Android export is implemented and verified on Linux hosts only",
            ));
        }
        let mut environment = resolved.binding.environment.clone();
        let engine_home = environment
            .extra
            .get("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| outcome_error(ErrorKind::Internal, "the engine has no private home"))?;

        let session_id = new_session_id(SessionKind::Export);
        let paths = self.sessions().paths(&session_id);
        // What the export needs before it starts lives in its own private
        // session directory, which is removed again unless the export took it
        // over.
        let staging = Staging::open(&paths.directory)?;
        let bases = match &patches {
            Some(patches) => {
                self.base_packs(access.principal().as_str(), patches, &paths.directory)?
            }
            None => Vec::new(),
        };
        let android_plan = if android {
            Some(
                self.android_export(preset, debug, &label, &paths.directory)
                    .await?,
            )
        } else {
            None
        };
        if let Some(plan) = &android_plan {
            for (variable, value) in export_environment(plan) {
                environment.extra.insert(variable, value);
            }
        }
        if let (Some(target), Some(set)) = (&target, &templates) {
            // The engine runs with a private home, which is given the build's
            // templates where that host's Godot looks for them.
            let home = android_plan
                .as_ref()
                .map_or(engine_home.clone(), |plan| plan.home.clone());
            link_templates(
                &home,
                &label,
                &set.directory,
                &["version.txt", &target.template],
            )
            .map_err(|error| outcome_error(ErrorKind::Internal, error))?;
        }

        // Godot packs an app bundle into a zip only for an output ending in
        // `.zip`, and picks a pack's format from its extension; any other name
        // would leave a directory or the wrong format.
        let output = match (&target, kind) {
            (Some(target), _) if target.app_bundle || android => {
                archive_output_name(preset.export_path(), name, target.extension)
            }
            (Some(target), _) => output_name(preset.export_path(), name, target.extension),
            (None, ExportKind::Pack) => {
                archive_output_name(preset.export_path(), name, pack_format.unwrap_or("pck"))
            }
            (None, _) => archive_output_name(preset.export_path(), name, "pck"),
        };
        let mut argv: Vec<String> = vec![
            "--headless".to_owned(),
            "--path".to_owned(),
            self.sessions().project_root().display().to_string(),
            match kind {
                ExportKind::Project if debug => "--export-debug".to_owned(),
                ExportKind::Project => "--export-release".to_owned(),
                ExportKind::Pack => "--export-pack".to_owned(),
                ExportKind::Patch => "--export-patch".to_owned(),
            },
            name.to_owned(),
            paths.output.join(&output).display().to_string(),
        ];
        if kind == ExportKind::Patch {
            let listed: Vec<String> = bases
                .iter()
                .map(|base| base.copy.display().to_string())
                .collect();
            if listed.iter().any(|path| path.contains(',')) {
                return Err(outcome_error(
                    ErrorKind::Unsupported,
                    "the server's state directory path holds a comma, which Godot's --patches \
                     list cannot carry",
                ));
            }
            argv.push("--patches".to_owned());
            argv.push(listed.join(","));
        }
        let observed = access.observed_at_unix_ms();
        let outer_id = engine.router.next_transaction_id(tool, observed);
        let inner_id = engine
            .router
            .next_transaction_id("supervised-launch", observed);
        let principal = access.mode_principal().to_journal_string();
        let connection = Some(access.connection_session().to_owned());
        let canonical = Value::Object(arguments.clone());
        let canonical_bytes = serde_json::to_vec(&canonical)
            .map_err(|error| outcome_error(ErrorKind::Internal, error.to_string()))?;
        let intent = ProcessEffectIntent {
            transaction_id: outer_id.clone(),
            tool_name: tool.to_owned(),
            operation_family: OperationFamily::DestructiveExport,
            idempotency_id: idempotency_id.clone(),
            transport_mode: journal_transport(access.transport_mode()),
            principal: Some(principal.clone()),
            session: connection.clone(),
            arguments: canonical.clone(),
            required_process_transaction: inner_id.clone(),
            opened_at_unix_ms: observed,
        };
        let request = SessionStartRequest {
            session_id: session_id.clone(),
            kind: SessionKind::Export,
            launch: LaunchRequest {
                envelope: TransactionEnvelope {
                    transaction_id: inner_id,
                    project: ProjectIdentityClaim {
                        object_identity: engine.held.object_identity.clone(),
                        root_object: engine.held.root_object.clone(),
                        file_object: engine.held.file_object.clone(),
                        generation: engine.held.generation,
                    },
                    transport_mode: engine_transport(access.transport_mode()),
                    principal: Some(principal),
                    session: connection,
                    operation_family: EngineOperationFamily::DestructiveExport,
                    tool_name: tool.to_owned(),
                    idempotency_id: format!("launch-{idempotency_id}"),
                    canonical_request_hash: hex_sha256(&canonical_bytes),
                    canonical_request_bytes: canonical_bytes.len() as u64,
                    opened_at_unix_ms: observed,
                },
                trust: resolved.binding.trust,
                program: resolved.binding.program,
                arguments: argv,
                binary_sha256: resolved.binding.binary_sha256,
                supervisor_binary: crate::engine_plane::process_supervisor_binary(),
                project_root_display: self.sessions().project_root().to_path_buf(),
                environment,
                caps: OutputCaps {
                    stdout_bytes: 1_048_576,
                    stderr_bytes: 1_048_576,
                },
                deadline: Duration::from_secs(lifetime),
                work_dir: self.sessions().work_dir().to_path_buf(),
                evidence_path: None,
            },
            scene: None,
            display: "headless".to_owned(),
            lifetime: Duration::from_secs(lifetime),
            readiness: Duration::ZERO,
            godot_version: None,
            lsp_port: None,
            dap_port: None,
            export: Some(ExportPlan {
                preset: name.to_owned(),
                platform: preset.platform().to_owned(),
                mode: if debug { "debug" } else { "release" },
                template_label: label,
                template: target
                    .as_ref()
                    .map(|target| target.template.clone())
                    .unwrap_or_default(),
                output_name: output,
                smoke,
                runnable_here: target.as_ref().is_some_and(|target| target.runnable_here),
                owner_principal: Some(access.principal().as_str().to_owned()),
                owner_session: access.session().map(|session| session.as_str().to_owned()),
                kind,
                bases,
                android: android_plan,
            }),
        };
        let route = RouteRequest {
            tool: tool.to_owned(),
            idempotency_id,
            transaction_id: Some(outer_id),
            path: String::new(),
            bytes: Vec::new(),
            arguments: canonical,
            observation: engine.route_observation(),
            cancellation: call.cancellation.clone(),
            queued_at_unix_ms: observed,
            identity: Some(EngineRuntime::route_identity(access)),
            effect: Some(RouteEffect::SessionStart {
                intent,
                request: Box::new(request),
            }),
            audit: Some(call.audit.clone()),
        };
        let routed = engine.router.route(&route).await;
        staging.settle(self.sessions(), &session_id);
        let report = routed.map_err(|refusal| refusal.to_outcome())?;
        self.start_answer(&report, "export")
    }

    /// The base packs of a patch export, each read through what names it,
    /// checked to be a readable PCK, and copied into the export's private
    /// directory for Godot to read.
    fn base_packs(
        &self,
        principal: &str,
        patches: &[String],
        session_directory: &Path,
    ) -> Result<Vec<BasePack>, OutcomeError> {
        let directory = session_directory.join("bases");
        godot_mcp_domain::sysfs::open_private_state_dir(&directory)
            .map_err(|error| outcome_error(ErrorKind::Internal, error.to_string()))?;
        let mut bases = Vec::with_capacity(patches.len());
        for (index, source) in patches.iter().enumerate() {
            let bytes = if source.starts_with(ARTIFACT_URI_PREFIX) {
                self.artifact_base(principal, source)?
            } else {
                self.project_base(source)?
            };
            if bytes.len() as u64 > MAX_EXPORT_BYTES {
                return Err(invalid(format!(
                    "the base pack {source} is {} bytes, more than the {MAX_EXPORT_BYTES} bytes \
                     an export may produce",
                    bytes.len()
                )));
            }
            match read_pack(&bytes) {
                Ok(_) => {}
                Err(PackError::NotAPack) => {
                    return Err(invalid(format!(
                        "the base pack {source} is not a PCK; a patch is compared with PCK base \
                         packs only"
                    )));
                }
                Err(error @ (PackError::EncryptedDirectory | PackError::UnsupportedFormat(_))) => {
                    return Err(outcome_error(
                        ErrorKind::Unsupported,
                        format!("the base pack {source} cannot be read: {error}"),
                    ));
                }
                Err(error) => {
                    return Err(invalid(format!(
                        "the base pack {source} is not a readable PCK: {error}"
                    )));
                }
            }
            let copy = directory.join(format!("base-{index}.pck"));
            write_private(&copy, &bytes)
                .map_err(|error| outcome_error(ErrorKind::Internal, error))?;
            bases.push(BasePack {
                source: source.clone(),
                sha256: sha256_hex(&bytes),
                bytes: bytes.len() as u64,
                copy,
            });
        }
        Ok(bases)
    }

    /// A base pack named by the artifact URI of an output of one of this
    /// caller's exports that is still served.
    fn artifact_base(&self, principal: &str, uri: &str) -> Result<Vec<u8>, OutcomeError> {
        let now = now_unix_ms();
        let store = self.sessions().export_store();
        let output = self
            .sessions()
            .sessions_of(SessionKind::Export)
            .iter()
            .filter(|session| session.belongs_to(principal))
            .filter_map(|session| session.export_record())
            .flat_map(|record| record.outputs)
            .find(|output| output.artifact == uri);
        let live = |output: &ExportOutput| {
            output.expires_at_unix_ms > now
                && store
                    .as_ref()
                    .is_some_and(|store| store.contract(&output.artifact).is_some())
        };
        let Some(output) = output.filter(live) else {
            return Err(outcome_error(
                ErrorKind::NotFound,
                format!("{uri} is not a served output of an export of yours"),
            ));
        };
        let store = store.ok_or_else(|| {
            outcome_error(
                ErrorKind::Unavailable,
                "this server keeps no artifact store",
            )
        })?;
        let range = ByteRange::new(0, output.bytes.saturating_sub(1))
            .map_err(|error| outcome_error(ErrorKind::Internal, error.to_string()))?;
        let bytes = store.read_range(uri, &range)?;
        if sha256_hex(&bytes) != output.sha256 {
            return Err(outcome_error(
                ErrorKind::Internal,
                format!("{uri} no longer has the SHA-256 its export recorded"),
            ));
        }
        Ok(bytes)
    }

    /// A base pack named by a project-relative path, read through the held
    /// project.
    fn project_base(&self, source: &str) -> Result<Vec<u8>, OutcomeError> {
        let relative = source.strip_prefix("res://").unwrap_or(source);
        let unusable = relative.is_empty()
            || relative.starts_with('/')
            || relative.contains(['\\', ':', '\0'])
            || relative.split('/').any(|part| part == "..");
        if unusable {
            return Err(invalid(format!(
                "the base pack {source} is neither an artifact URI nor a project-relative path"
            )));
        }
        if relative.split('/').next() == Some(crate::stdio::STATE_DIR_NAME) {
            return Err(invalid(format!(
                "the base pack {source} is inside the server's private state, which is not \
                 project content"
            )));
        }
        let root = self.engine.root()?;
        root.read(Path::new(relative)).map_err(|error| match error {
            godot_mcp_domain::safe_root::SafeRootError::StalePreimage { .. } => outcome_error(
                ErrorKind::NotFound,
                format!("the base pack {source} does not exist in the project"),
            ),
            error => outcome_error(
                error.error_kind(),
                format!("the base pack {source} could not be read as project content"),
            ),
        })
    }

    /// What Godot's credentials store holds for preset `index`, when the
    /// project has one.
    fn credentials(&self, index: u32) -> Result<Option<ExportPreset>, OutcomeError> {
        let root = self.engine.root()?;
        let bytes = match root.read(Path::new(CREDENTIALS_FILE)) {
            Ok(bytes) => bytes,
            Err(godot_mcp_domain::safe_root::SafeRootError::StalePreimage { .. }) => {
                return Ok(None);
            }
            Err(error) => {
                return Err(outcome_error(
                    error.error_kind(),
                    format!("{CREDENTIALS_FILE} could not be read"),
                ));
            }
        };
        let text = String::from_utf8(bytes)
            .map_err(|_| invalid(format!("{CREDENTIALS_FILE} is not UTF-8 text")))?;
        let presets = parse_presets(&text).map_err(|_| {
            invalid(format!(
                "{CREDENTIALS_FILE} is not a Godot configuration file"
            ))
        })?;
        Ok(presets.into_iter().find(|preset| preset.index == index))
    }

    /// Prepare an Android export: the configured SDK and JDK, the keystore it
    /// signs with, and its private Godot home in `session_directory`.
    async fn android_export(
        &self,
        preset: &ExportPreset,
        debug: bool,
        label: &str,
        session_directory: &Path,
    ) -> Result<AndroidExport, OutcomeError> {
        let tools = self.sessions().android().clone();
        let sdk = tools.sdk()?;
        let jdk = tools.jdk()?;
        // Godot signs with the SDK's apksigner, which runs the JDK's java.
        sdk.apksigner().map_err(|error| sdk_error(&error))?;
        jdk.java().map_err(|error| sdk_error(&error))?;
        let credentials = self.credentials(preset.index)?;
        let set = |key: &str| {
            credentials
                .as_ref()
                .and_then(|credentials| credentials.options.get(key))
                .is_some_and(PresetValue::is_set)
        };
        let mode = if debug { "debug" } else { "release" };
        let named = [
            format!("keystore/{mode}"),
            format!("keystore/{mode}_user"),
            format!("keystore/{mode}_password"),
        ];
        let present = named.iter().filter(|key| set(key)).count();
        let (keystore, keystore_kind) = match (debug, present) {
            (_, 3) if debug => (None, "project-debug"),
            (_, 3) => (None, "project-release"),
            (true, 0) => {
                let dirs = self.sessions().android_dirs().clone();
                let jdk = jdk.clone();
                let keystore =
                    tokio::task::spawn_blocking(move || ensure_debug_keystore(&dirs, &jdk))
                        .await
                        .map_err(|error| outcome_error(ErrorKind::Internal, error.to_string()))??;
                (Some(keystore), "server-debug")
            }
            (true, _) => {
                return Err(outcome_error(
                    ErrorKind::Unavailable,
                    format!(
                        "{CREDENTIALS_FILE} names a debug keystore for preset `{}` without all of \
                         keystore/debug, keystore/debug_user and keystore/debug_password",
                        preset.name()
                    ),
                ));
            }
            (false, _) => {
                return Err(outcome_error(
                    ErrorKind::Unavailable,
                    format!(
                        "a release Android export signs with the release keystore Godot's \
                         credentials store names for preset `{}`: keystore/release, \
                         keystore/release_user and keystore/release_password in \
                         {CREDENTIALS_FILE}; it names {present} of the three",
                        preset.name()
                    ),
                ));
            }
        };
        prepare_export(
            session_directory,
            label,
            &sdk,
            &jdk,
            keystore.as_deref(),
            keystore_kind,
        )
    }
}

/// The answer for an export that exists but is not this caller's: the same as
/// for one that does not exist, so its existence is not disclosed.
pub(super) fn not_this_callers(id: &str) -> OutcomeError {
    OutcomeError::new(
        ErrorKind::NotFound,
        format!("{id} is not a session of this server"),
        false,
        std::collections::BTreeMap::new(),
    )
}
