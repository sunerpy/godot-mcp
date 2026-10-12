//! The export tools: `export.presets.list`, `project.export` and
//! `project.export.status`. Cancelling an export is a session stop and runs
//! through [`SessionHandler::command`](super::SessionHandler).

use std::path::{Path, PathBuf};
use std::time::Duration;

use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::OutcomeError;
use godot_mcp_engine::export::presets::{ExportPreset, parse_presets};
use godot_mcp_engine::export::templates::{
    TemplateError, TemplateSet, default_templates_root, target_for,
};
use godot_mcp_engine::supervision::journal::{
    OperationFamily as EngineOperationFamily, ProjectIdentityClaim, TransactionEnvelope, hex_sha256,
};
use godot_mcp_engine::supervision::protocol::{LaunchRequest, OutputCaps};
use serde_json::{Map, Value, json};

use super::{
    DEFAULT_EXPORT_SECONDS, EXPORT_FLAGS, KernelToolCall, MAX_EXPORT_SECONDS, SessionHandler,
    SessionKind, boolean, integer, invalid, outcome_error, required_string, string,
};
use crate::effect::ProcessEffectIntent;
use crate::effect::boundary::SessionStartRequest;
use crate::engine_plane::EngineRuntime;
use crate::engine_plane::runtime::{engine_transport, journal_transport};
use crate::engine_plane::session::export::{
    ExportPlan, archive_output_name, link_templates, output_name, template_label_of,
};
use crate::engine_plane::session::{new_session_id, now_unix_ms};
use crate::journal::OperationFamily;
use crate::router::{RouteEffect, RouteRequest};

/// The presets file, project-relative.
const PRESETS_FILE: &str = "export_presets.cfg";

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
        let target = target_for(preset, debug).map_err(|error| template_refusal(&error))?;
        let resolved = engine.resolve_engine().await?;
        if let Some(flag) = EXPORT_FLAGS
            .iter()
            .find(|flag| !resolved.binding.capabilities.contains(**flag))
        {
            return Err(outcome_error(
                ErrorKind::Unsupported,
                format!(
                    "this Godot build does not advertise {flag}, which an export needs; use an \
                     editor build"
                ),
            ));
        }
        let label = template_label_of(&resolved.version_line).ok_or_else(|| {
            outcome_error(
                ErrorKind::Unsupported,
                "this Godot build's version does not say which export templates match it",
            )
        })?;
        let root = templates_root().ok_or_else(|| {
            outcome_error(
                ErrorKind::Unavailable,
                "no data directory is set, so there is nowhere to find export templates",
            )
        })?;
        let set = TemplateSet::resolve(&root, &label).map_err(|error| template_refusal(&error))?;
        set.template(&target)
            .map_err(|error| template_refusal(&error))?;
        let home = resolved
            .binding
            .environment
            .extra
            .get("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| outcome_error(ErrorKind::Internal, "the engine has no private home"))?;
        // The engine runs with the server's private home, which is given the
        // build's templates where that host's Godot looks for them.
        link_templates(
            &home,
            &label,
            &set.directory,
            &["version.txt", &target.template],
        )
        .map_err(|error| outcome_error(ErrorKind::Internal, error))?;

        let session_id = new_session_id(SessionKind::Export);
        let paths = self.sessions().paths(&session_id);
        // Godot packs an app bundle into a zip only for an output ending in
        // `.zip`; any other name would leave a directory, not one file.
        let output = if target.app_bundle {
            archive_output_name(preset.export_path(), name, target.extension)
        } else {
            output_name(preset.export_path(), name, target.extension)
        };
        let argv: Vec<String> = vec![
            "--headless".to_owned(),
            "--path".to_owned(),
            self.sessions().project_root().display().to_string(),
            if debug {
                "--export-debug".to_owned()
            } else {
                "--export-release".to_owned()
            },
            name.to_owned(),
            paths.output.join(&output).display().to_string(),
        ];
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
            session_id,
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
                environment: resolved.binding.environment,
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
                template: target.template.clone(),
                output_name: output,
                smoke,
                runnable_here: target.runnable_here,
                owner_principal: Some(access.principal().as_str().to_owned()),
                owner_session: access.session().map(|session| session.as_str().to_owned()),
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
        let report = engine
            .router
            .route(&route)
            .await
            .map_err(|refusal| refusal.to_outcome())?;
        self.start_answer(&report, "export")
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
