//! Managed exports: one supervised `--export-release` or `--export-debug` run
//! of one preset, owned like any other managed session.
//!
//! An export starts only once its preset is known to carry no inline secret,
//! its platform is one this server exports, and exactly the resolved build's
//! template is installed — so a refusal happens before anything runs. The
//! export writes into the session's private output directory, never into the
//! project. When the process ends, every output file of a successful export is
//! checked against the artifact budget, hashed and registered as an artifact
//! bound to the principal that asked for it, and a build this host
//! runs is smoke-run once if asked: a Linux or Windows build itself, a macOS
//! app bundle unpacked from its zip and run from the executable its
//! `Info.plist` names. A failed or cancelled export registers nothing and
//! removes its partial output; an export a dead server left running keeps its
//! partial output privately, as recovery evidence that is never served.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::{
    ArtifactContract, ArtifactId, ArtifactKind, ArtifactMetadata, ArtifactResourceUri,
    OutcomeError, ProjectIdentity, RangeSemantics,
};
use godot_mcp_engine::export::bundle::unpack_app;
use godot_mcp_engine::export::pack::{
    PackEntry, PackError, PatchChange, compare_patch, read_pack, read_zip_pack,
};
use godot_mcp_engine::export::templates::exports_app_bundle;
use godot_mcp_engine::supervision::protocol::{
    FaultInjection, LaunchDiagnostics, LaunchProtocol, LaunchRequest, SilentObserver,
    TerminationKind,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::android::{AndroidExport, ApkRecord, verify_apk};
use super::record::SessionRecord;
use crate::artifacts::{ArtifactStore, sha256_hex};
use crate::engine_plane::journal::EngineJournal;

/// The most bytes one export may produce: the artifact store's whole budget.
pub(crate) const MAX_EXPORT_BYTES: u64 = 256 * 1024 * 1024;
/// The most pack entries an export's status lists.
pub(crate) const MAX_LISTED_ENTRIES: usize = 500;
/// The most identical paths a patch's status names.
const MAX_IDENTICAL_PATHS: usize = 100;
/// What Godot 4.4.1 to 4.7.1 print when a patch has no file to export
/// (measured), before exiting 1.
pub(crate) const NOTHING_TO_EXPORT: &str = "No files or changes to export";
/// How long the smoke run of an exported build may take.
const SMOKE_DEADLINE: Duration = Duration::from_secs(60);

/// What an export produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ExportKind {
    /// The preset's build, through its template (`--export-release` or
    /// `--export-debug`).
    #[default]
    Project,
    /// The project's data as one pack (`--export-pack`).
    Pack,
    /// The files that changed against base packs (`--export-patch`).
    Patch,
}

impl ExportKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Pack => "pack",
            Self::Patch => "patch",
        }
    }
}

/// One base pack of a patch export: named by the caller, copied into the
/// export's private directory before it starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BasePack {
    /// The artifact URI or project-relative path the caller named.
    pub(crate) source: String,
    pub(crate) sha256: String,
    pub(crate) bytes: u64,
    /// The private copy Godot reads. Never shown.
    pub(crate) copy: PathBuf,
}

/// What one export is asked to do, fixed when it starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExportPlan {
    pub(crate) preset: String,
    pub(crate) platform: String,
    /// `release` or `debug`.
    pub(crate) mode: &'static str,
    pub(crate) template_label: String,
    pub(crate) template: String,
    /// The output's file name inside the session's output directory.
    pub(crate) output_name: String,
    pub(crate) smoke: bool,
    /// Whether this host can run the output.
    pub(crate) runnable_here: bool,
    pub(crate) owner_principal: Option<String>,
    pub(crate) owner_session: Option<String>,
    pub(crate) kind: ExportKind,
    pub(crate) bases: Vec<BasePack>,
    pub(crate) android: Option<AndroidExport>,
}

/// One file a successful export produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ExportOutput {
    pub(crate) name: String,
    pub(crate) bytes: u64,
    pub(crate) sha256: String,
    pub(crate) artifact: String,
    pub(crate) expires_at_unix_ms: u64,
}

/// How the smoke run of an exported build ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SmokeFacts {
    pub(crate) ran: bool,
    pub(crate) passed: bool,
    pub(crate) exit_code: Option<i32>,
    pub(crate) termination: Option<String>,
    pub(crate) reason: Option<String>,
    /// The file that ran, relative to the output: the output itself, or the
    /// executable inside an app bundle.
    #[serde(default)]
    pub(crate) executable: Option<String>,
}

impl SmokeFacts {
    fn not_run(reason: String) -> Self {
        Self {
            ran: false,
            passed: false,
            exit_code: None,
            termination: None,
            reason: Some(reason),
            executable: None,
        }
    }
}

/// The durable facts of one export.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ExportRecord {
    pub(crate) preset: String,
    pub(crate) platform: String,
    pub(crate) mode: String,
    pub(crate) template_label: String,
    pub(crate) template: String,
    pub(crate) output_name: String,
    pub(crate) smoke_requested: bool,
    pub(crate) runnable_here: bool,
    pub(crate) owner_principal: Option<String>,
    pub(crate) owner_session: Option<String>,
    /// `succeeded`, `failed` or `cancelled`, once the export ended.
    pub(crate) outcome: Option<String>,
    pub(crate) failure: Option<String>,
    pub(crate) outputs: Vec<ExportOutput>,
    pub(crate) smoke: Option<SmokeFacts>,
    /// Bytes of partial output kept privately after a dead server's export.
    pub(crate) recovery_evidence_bytes: Option<u64>,
    #[serde(default)]
    pub(crate) kind: ExportKind,
    #[serde(default)]
    pub(crate) bases: Vec<BasePack>,
    /// What an Android export ran with; private.
    #[serde(default)]
    pub(crate) android: Option<AndroidExport>,
    /// What the pack a pack or patch export wrote holds.
    #[serde(default)]
    pub(crate) pack: Option<Value>,
    /// What the APK an Android export wrote declares.
    #[serde(default)]
    pub(crate) apk: Option<ApkRecord>,
    /// Whether a patch export failed because nothing changed.
    #[serde(default)]
    pub(crate) nothing_changed: bool,
}

impl ExportRecord {
    /// Whether the caller `principal` may see and cancel this export: its
    /// output is bound to the principal that started it.
    pub(crate) fn belongs_to(&self, principal: &str) -> bool {
        self.owner_principal
            .as_deref()
            .is_none_or(|owner| owner == principal)
    }

    pub(crate) fn planned(plan: &ExportPlan) -> Self {
        Self {
            preset: plan.preset.clone(),
            platform: plan.platform.clone(),
            mode: plan.mode.to_owned(),
            template_label: plan.template_label.clone(),
            template: plan.template.clone(),
            output_name: plan.output_name.clone(),
            smoke_requested: plan.smoke,
            runnable_here: plan.runnable_here,
            owner_principal: plan.owner_principal.clone(),
            owner_session: plan.owner_session.clone(),
            outcome: None,
            failure: None,
            outputs: Vec::new(),
            smoke: None,
            recovery_evidence_bytes: None,
            kind: plan.kind,
            bases: plan.bases.clone(),
            android: plan.android.clone(),
            pack: None,
            apk: None,
            nothing_changed: false,
        }
    }

    /// The output an Android export's deploy installs, with its artifact
    /// when it is still served.
    pub(crate) fn apk_output(&self) -> Option<&ExportOutput> {
        let apk = self.apk.as_ref()?;
        self.outputs.iter().find(|output| output.name == apk.output)
    }

    /// The path-free projection a client may see. An output whose artifact
    /// has expired keeps its facts and loses its URI.
    pub(crate) fn public(&self, store: Option<&ArtifactStore>, now_unix_ms: u64) -> Value {
        let outputs: Vec<Value> = self
            .outputs
            .iter()
            .map(|output| {
                let live = output.expires_at_unix_ms > now_unix_ms
                    && store.is_none_or(|store| store.contract(&output.artifact).is_some());
                json!({
                    "name": output.name,
                    "bytes": output.bytes,
                    "sha256": output.sha256,
                    "artifact": live.then(|| output.artifact.clone()),
                    "expired": !live,
                    "expires_at_unix_ms": output.expires_at_unix_ms,
                })
            })
            .collect();
        let bases: Vec<Value> = self
            .bases
            .iter()
            .map(|base| {
                json!({
                    "source": base.source,
                    "sha256": base.sha256,
                    "bytes": base.bytes,
                })
            })
            .collect();
        json!({
            "kind": self.kind.as_str(),
            "preset": self.preset,
            "platform": self.platform,
            "mode": self.mode,
            "template_label": self.template_label,
            // A pack or patch export goes through no template.
            "template": (!self.template.is_empty()).then_some(&self.template),
            "outcome": self.outcome,
            "failure": self.failure,
            "nothing_changed": self.nothing_changed,
            "bases": bases,
            "outputs": outputs,
            "pack": self.pack,
            "apk": self.apk.as_ref().map(ApkRecord::public),
            "smoke": self.smoke,
            "recovery_evidence": self.recovery_evidence_bytes.map(|bytes| json!({
                "retained": true,
                "bytes": bytes,
            })),
        })
    }
}

/// The parts of the server an export's completion registers artifacts in.
#[derive(Clone)]
pub(crate) struct ExportArtifacts {
    pub(crate) store: Arc<ArtifactStore>,
    pub(crate) project: ProjectIdentity,
    pub(crate) retention_ms: u64,
    pub(crate) max_range_bytes: u64,
}

impl std::fmt::Debug for ExportArtifacts {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExportArtifacts")
            .finish_non_exhaustive()
    }
}

/// The `version.status` label a build's `--version` line names, the way its
/// template directory is named: `4.7.1.stable.official.<hash>` is
/// `4.7.1.stable`, and `4.4.stable.official` is `4.4.stable`.
pub(crate) fn template_label_of(version_line: &str) -> Option<String> {
    let segments: Vec<&str> = version_line.trim().split('.').collect();
    let status = segments
        .iter()
        .position(|segment| !segment.bytes().all(|byte| byte.is_ascii_digit()))?;
    if !(2..=3).contains(&status) || segments[status].is_empty() {
        return None;
    }
    Some(segments[..=status].join("."))
}

/// A file name that is safe inside the output directory: the last component
/// of `export_path` with anything but letters, digits, `.`, `_` and `-`
/// replaced, or `<preset>.<extension>` when that leaves nothing usable.
pub(crate) fn output_name(export_path: &str, preset: &str, extension: &str) -> String {
    let clean = |text: &str| -> String {
        text.chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                    character
                } else {
                    '_'
                }
            })
            .collect::<String>()
            .trim_matches('.')
            .to_owned()
    };
    let named = export_path
        .rsplit(['/', '\\'])
        .next()
        .map(clean)
        .unwrap_or_default();
    if named.is_empty() || named.len() > 128 {
        let base = clean(preset);
        let base = if base.is_empty() {
            "export".to_owned()
        } else {
            base
        };
        format!("{}.{extension}", &base[..base.len().min(96)])
    } else {
        named
    }
}

/// The output file name of an export whose extension decides its format, such
/// as a macOS app bundle packed as a zip: [`output_name`], always ending in
/// `.extension`.
pub(crate) fn archive_output_name(export_path: &str, preset: &str, extension: &str) -> String {
    let named = output_name(export_path, preset, extension);
    let stem = match named.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => named.as_str(),
    };
    format!("{stem}.{extension}")
}

/// Where a host's Godot looks for export templates under its home: the data
/// directory Godot 4 itself uses — `$XDG_DATA_HOME/godot` on Linux, which the
/// private engine environment leaves at `$HOME/.local/share`,
/// `$HOME/Library/Application Support/Godot` on macOS, `%APPDATA%\Godot` on
/// Windows, where the private engine environment points `APPDATA` at
/// `<home>\AppData\Roaming`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TemplateHost {
    Linux,
    MacOs,
    Windows,
}

impl TemplateHost {
    pub(crate) const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(windows) {
            Self::Windows
        } else {
            Self::Linux
        }
    }
}

/// The entry for one build's templates in the engine's private home.
pub(crate) fn templates_entry(host: TemplateHost, engine_home: &Path, label: &str) -> PathBuf {
    let data: &[&str] = match host {
        TemplateHost::Linux => &[".local", "share", "godot"],
        TemplateHost::MacOs => &["Library", "Application Support", "Godot"],
        TemplateHost::Windows => &["AppData", "Roaming", "Godot"],
    };
    let mut entry = engine_home.to_path_buf();
    entry.extend(data);
    entry.push("export_templates");
    entry.push(label);
    entry
}

/// Marks a template entry this server assembled itself, so a later export may
/// replace it and a directory anyone else put there is never touched.
const PLACED_MARKER: &str = ".godot-mcp-placed";

/// Make the build's exact template directory visible to the engine, which
/// runs with the server's private home. `files` are the ones this export reads.
pub(crate) fn link_templates(
    engine_home: &Path,
    label: &str,
    directory: &Path,
    files: &[&str],
) -> Result<(), String> {
    link_templates_for(
        TemplateHost::current(),
        engine_home,
        label,
        directory,
        files,
    )
}

/// [`link_templates`] for `host`. On Unix the entry is one link to the verified
/// directory. Windows makes a directory link only with a privilege an ordinary
/// user lacks, so the entry there is a directory of its own holding exactly
/// `files`, hard-linked from the verified directory where both share a volume
/// and copied where they do not.
pub(crate) fn link_templates_for(
    host: TemplateHost,
    engine_home: &Path,
    label: &str,
    directory: &Path,
    files: &[&str],
) -> Result<(), String> {
    let entry = templates_entry(host, engine_home, label);
    let parent = entry
        .parent()
        .ok_or_else(|| "the template entry has no parent".to_owned())?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    match std::fs::symlink_metadata(&entry) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            std::fs::remove_file(&entry).map_err(|error| error.to_string())?;
        }
        Ok(metadata) if metadata.is_dir() && entry.join(PLACED_MARKER).is_file() => {
            std::fs::remove_dir_all(&entry).map_err(|error| error.to_string())?;
        }
        Ok(_) => {
            return Err(format!(
                "the private template entry for {label} is not one this server placed"
            ));
        }
        Err(_) => {}
    }
    if host == TemplateHost::Windows {
        return place_template_files(&entry, directory, files);
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(directory, &entry).map_err(|error| error.to_string())
    }
    #[cfg(not(unix))]
    {
        place_template_files(&entry, directory, files)
    }
}

fn place_template_files(entry: &Path, directory: &Path, files: &[&str]) -> Result<(), String> {
    std::fs::create_dir(entry).map_err(|error| error.to_string())?;
    std::fs::write(entry.join(PLACED_MARKER), b"").map_err(|error| error.to_string())?;
    for name in files {
        let source = directory.join(name);
        let target = entry.join(name);
        if std::fs::hard_link(&source, &target).is_err() {
            std::fs::copy(&source, &target)
                .map_err(|error| format!("the template {name} could not be placed: {error}"))?;
        }
    }
    Ok(())
}

/// Every regular file directly inside `directory`, by name, with its length.
fn output_files(directory: &Path) -> Vec<(String, PathBuf, u64)> {
    let Ok(listing) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut files: Vec<(String, PathBuf, u64)> = listing
        .flatten()
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            metadata.is_file().then(|| {
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    entry.path(),
                    metadata.len(),
                )
            })
        })
        .collect();
    files.sort_by(|left, right| left.0.cmp(&right.0));
    files
}

/// The bytes of partial output left in `directory`.
pub(crate) fn evidence_bytes(directory: &Path) -> u64 {
    output_files(directory)
        .iter()
        .map(|(_, _, bytes)| bytes)
        .sum()
}

/// Remove everything an export wrote.
pub(crate) fn discard_outputs(directory: &Path) {
    for (_, path, _) in output_files(directory) {
        let _ = std::fs::remove_file(path);
    }
}

fn internal(message: impl Into<String>) -> OutcomeError {
    OutcomeError::new(ErrorKind::Internal, message, false, BTreeMap::new())
}

/// Register one output file as an artifact owned by the export's caller.
fn register(
    artifacts: &ExportArtifacts,
    record: &ExportRecord,
    session_id: &str,
    name: &str,
    bytes: Vec<u8>,
    now_unix_ms: u64,
) -> Result<ExportOutput, OutcomeError> {
    let sha256 = sha256_hex(&bytes);
    let token = sha256_hex(format!("{session_id}\n{name}\n{sha256}").as_bytes());
    let id = ArtifactId::parse(format!("artifact_{token}"))
        .map_err(|error| internal(error.to_string()))?;
    let owner = crate::capture::artifact_owner(
        record.owner_principal.as_deref(),
        record.owner_session.as_deref(),
    );
    let length = bytes.len() as u64;
    let expires = now_unix_ms.saturating_add(artifacts.retention_ms.max(1));
    let violation =
        |error: godot_mcp_domain::contracts::ContractViolation| internal(error.to_string());
    let contract = ArtifactContract::new(
        id.clone(),
        ArtifactResourceUri::for_artifact(&id),
        artifacts.project.clone(),
        owner,
        ArtifactMetadata {
            kind: ArtifactKind::Export,
            media_type: "application/octet-stream".to_owned(),
            byte_length: length,
            sha256: sha256.clone(),
            created_at_unix_ms: now_unix_ms,
        },
        expires,
        RangeSemantics::new(
            length.max(1),
            length.clamp(1, artifacts.max_range_bytes.max(1)),
            true,
        )
        .map_err(violation)?,
    )
    .map_err(violation)?;
    let uri = contract.resource_uri().as_str().to_owned();
    artifacts.store.register(contract, bytes)?;
    Ok(ExportOutput {
        name: name.to_owned(),
        bytes: length,
        sha256,
        artifact: uri,
        expires_at_unix_ms: expires,
    })
}

/// Register every output file of one export, or none of them: when one cannot
/// be registered, the ones registered before it are taken back.
fn register_all(
    artifacts: &ExportArtifacts,
    record: &ExportRecord,
    session_id: &str,
    files: &[(String, PathBuf, u64)],
    now_unix_ms: u64,
) -> Result<Vec<ExportOutput>, String> {
    let mut outputs: Vec<ExportOutput> = Vec::with_capacity(files.len());
    for (name, path, _) in files {
        let registered = std::fs::read(path)
            .map_err(|error| internal(format!("{name} could not be read: {error}")))
            .and_then(|bytes| register(artifacts, record, session_id, name, bytes, now_unix_ms));
        match registered {
            Ok(output) => outputs.push(output),
            Err(error) => {
                for output in &outputs {
                    artifacts.store.withdraw(&output.artifact);
                }
                return Err(format!(
                    "{name} could not be registered as an artifact: {}",
                    error.message()
                ));
            }
        }
    }
    Ok(outputs)
}

fn flag_names(entry: &PackEntry) -> Vec<&'static str> {
    let mut flags = Vec::new();
    if entry.removal() {
        flags.push("removal");
    }
    if entry.delta() {
        flags.push("delta");
    }
    if entry.encrypted() {
        flags.push("encrypted");
    }
    flags
}

/// Read the pack a pack or patch export wrote, and compare a patch with its
/// base packs: the facts its status shows, or why the pack is not a result.
fn pack_facts(
    output: &str,
    bytes: &[u8],
    kind: ExportKind,
    bases: &[BasePack],
) -> Result<Value, String> {
    if output.ends_with(".zip") {
        let entries = read_zip_pack(bytes)
            .map_err(|error| format!("the ZIP pack Godot wrote cannot be read: {error}"))?;
        if let Some(entry) = entries.iter().find(|entry| !entry.verified) {
            return Err(format!(
                "the ZIP pack Godot wrote is damaged: `{}` does not match its CRC-32",
                entry.path
            ));
        }
        let listed: Vec<Value> = entries
            .iter()
            .take(MAX_LISTED_ENTRIES)
            .map(|entry| {
                json!({
                    "path": entry.path,
                    "size": entry.size,
                    "crc32": format!("{:08x}", entry.crc32),
                })
            })
            .collect();
        return Ok(json!({
            "format": "zip",
            "readable": true,
            "files": entries.len(),
            "entries": listed,
            "truncated": entries.len() > MAX_LISTED_ENTRIES,
        }));
    }
    let pack = match read_pack(bytes) {
        Ok(pack) => pack,
        Err(error @ (PackError::EncryptedDirectory | PackError::UnsupportedFormat(_))) => {
            return Ok(json!({
                "format": "pck",
                "readable": false,
                "reason": error.to_string(),
            }));
        }
        Err(error) => return Err(format!("the pack Godot wrote cannot be read: {error}")),
    };
    if let Some(entry) = pack.mismatches().first() {
        return Err(format!(
            "the pack Godot wrote is damaged: the bytes of `{}` do not have the MD5 its \
             directory records",
            entry.path
        ));
    }
    let comparison = if kind == ExportKind::Patch {
        let mut tables = Vec::with_capacity(bases.len());
        for base in bases {
            let bytes = std::fs::read(&base.copy).map_err(|error| {
                format!(
                    "the base pack {} cannot be read again: {error}",
                    base.source
                )
            })?;
            tables.push(read_pack(&bytes).map_err(|error| {
                format!("the base pack {} cannot be read: {error}", base.source)
            })?);
        }
        Some(compare_patch(&pack, &tables))
    } else {
        None
    };
    let listed: Vec<Value> = pack
        .entries
        .iter()
        .enumerate()
        .take(MAX_LISTED_ENTRIES)
        .map(|(index, entry)| {
            let mut value = json!({
                "path": entry.path,
                "size": entry.size,
                "md5": entry.md5_hex(),
                "flags": flag_names(entry),
                "digest": entry.digest.as_str(),
            });
            if let Some(comparison) = &comparison {
                value["change"] = json!(comparison.changes[index].1.as_str());
            }
            value
        })
        .collect();
    let (major, minor, patch) = pack.engine_version;
    let mut facts = json!({
        "format": "pck",
        "readable": true,
        "format_version": pack.format_version,
        "engine_version": format!("{major}.{minor}.{patch}"),
        "files": pack.entries.len(),
        "entries": listed,
        "truncated": pack.entries.len() > MAX_LISTED_ENTRIES,
    });
    if let Some(comparison) = comparison {
        let identical = comparison.identical();
        facts["patch"] = json!({
            "bases": bases.len(),
            "new": comparison.count(PatchChange::New),
            "changed": comparison.count(PatchChange::Changed),
            "delta": comparison.count(PatchChange::Delta),
            "removed": comparison.count(PatchChange::Removed),
            "removed_absent": comparison.count(PatchChange::RemovedAbsent),
            "identical": identical.len(),
            "identical_paths": identical.iter().take(MAX_IDENTICAL_PATHS).collect::<Vec<_>>(),
            "changed_files_only": identical.is_empty(),
        });
    }
    Ok(facts)
}

/// Whether a patch export's captured output says Godot found nothing to
/// export.
fn nothing_to_export(diagnostics: &LaunchDiagnostics) -> bool {
    [&diagnostics.stdout.bytes, &diagnostics.stderr.bytes]
        .iter()
        .any(|bytes| String::from_utf8_lossy(bytes).contains(NOTHING_TO_EXPORT))
}

/// Remove what an export staged for itself: its private Godot home, its SDK
/// view, its copies of base packs and its tool scratch.
pub(crate) fn remove_staging(session_directory: &Path) {
    for name in ["android-home", "android-sdk", "bases", "scratch"] {
        let path = session_directory.join(name);
        if std::fs::symlink_metadata(&path).is_ok() {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// What a finished export produced, settled into its record: registered
/// outputs, or the reason none were, and the smoke run.
pub(super) struct Completion<'a> {
    pub(super) session_id: &'a str,
    pub(super) output_dir: &'a Path,
    /// Where an app bundle is unpacked to be smoke-run; it does not exist
    /// before and is removed after.
    pub(super) bundle_dir: &'a Path,
    pub(super) artifacts: Option<&'a ExportArtifacts>,
    pub(super) journal: &'a EngineJournal,
    pub(super) provider: &'a dyn godot_mcp_engine::supervision::boundary::BoundaryProvider,
    pub(super) launch: &'a LaunchRequest,
    pub(super) now_unix_ms: u64,
}

impl Completion<'_> {
    /// Settle `record` after the export process ended with `diagnostics`,
    /// `cancelled` when a stop ended it.
    pub(super) fn settle(
        &self,
        record: &mut SessionRecord,
        diagnostics: Option<&LaunchDiagnostics>,
        cancelled: bool,
    ) {
        let Some(export) = record.export.as_mut() else {
            return;
        };
        if cancelled {
            discard_outputs(self.output_dir);
            export.outcome = Some("cancelled".to_owned());
            return;
        }
        let clean = diagnostics
            .is_some_and(|diagnostics| diagnostics.termination == TerminationKind::ExitedClean);
        let files = output_files(self.output_dir);
        let total: u64 = files.iter().map(|(_, _, bytes)| bytes).sum();
        let nothing_changed = !clean
            && export.kind == ExportKind::Patch
            && diagnostics.is_some_and(nothing_to_export);
        let mut failure = if nothing_changed {
            export.nothing_changed = true;
            Some(format!(
                "Godot found no file that differs from the base packs: it reported \"{NOTHING_TO_EXPORT}.\" \
                 and {}; any file it left was discarded",
                diagnostics.map_or_else(
                    || "did not exit cleanly".to_owned(),
                    |diagnostics| termination(&diagnostics.termination)
                )
            ))
        } else if !clean {
            Some(diagnostics.map_or_else(
                || "the export process did not run to completion".to_owned(),
                |diagnostics| {
                    format!(
                        "the export failed ({}); its output is in project.run.logs",
                        termination(&diagnostics.termination)
                    )
                },
            ))
        } else if !files
            .iter()
            .any(|(name, _, bytes)| name == &export.output_name && *bytes > 0)
        {
            Some("the export exited cleanly but produced no output file".to_owned())
        } else if total > MAX_EXPORT_BYTES {
            Some(format!(
                "the export produced {total} bytes, more than the {MAX_EXPORT_BYTES}-byte \
                 artifact budget; nothing was registered"
            ))
        } else {
            None
        };
        if failure.is_none() {
            failure = self.verify(export).err();
        }
        if let Some(failure) = failure {
            discard_outputs(self.output_dir);
            export.outcome = Some("failed".to_owned());
            export.failure = Some(failure);
            export.pack = None;
            export.apk = None;
            return;
        }
        let Some(artifacts) = self.artifacts else {
            discard_outputs(self.output_dir);
            export.outcome = Some("failed".to_owned());
            export.failure = Some("this server keeps no artifact store".to_owned());
            return;
        };
        let outputs =
            match register_all(artifacts, export, self.session_id, &files, self.now_unix_ms) {
                Ok(outputs) => outputs,
                Err(failure) => {
                    discard_outputs(self.output_dir);
                    export.outcome = Some("failed".to_owned());
                    export.failure = Some(failure);
                    return;
                }
            };
        export.outcome = Some("succeeded".to_owned());
        if export.smoke_requested {
            let main = self.output_dir.join(&export.output_name);
            let main_sha = outputs
                .iter()
                .find(|output| output.name == export.output_name)
                .map(|output| output.sha256.clone())
                .unwrap_or_default();
            export.smoke = Some(if export.kind != ExportKind::Project {
                SmokeFacts::not_run("a pack is data a game loads, not a program to run".to_owned())
            } else if !export.runnable_here {
                SmokeFacts::not_run(format!("this host cannot run a {} build", export.platform))
            } else if exports_app_bundle(&export.platform) {
                self.smoke_bundle(&main)
            } else {
                self.smoke(&main, &main_sha, export.output_name.clone())
            });
        }
        export.outputs = outputs;
        // The registered artifacts hold the bytes now.
        discard_outputs(self.output_dir);
    }

    /// Read what the export wrote before it becomes the result: the pack of
    /// a pack or patch export, the APK of an Android export.
    fn verify(&self, export: &mut ExportRecord) -> Result<(), String> {
        let android = export.android.clone();
        if export.kind == ExportKind::Project && android.is_none() {
            return Ok(());
        }
        let main = self.output_dir.join(&export.output_name);
        let bytes = std::fs::read(&main)
            .map_err(|error| format!("{} could not be read back: {error}", export.output_name))?;
        if export.kind == ExportKind::Project {
            let Some(plan) = android else {
                return Ok(());
            };
            let scratch = self.bundle_dir.parent().map_or_else(
                || self.output_dir.join("scratch"),
                |parent| parent.join("scratch"),
            );
            std::fs::create_dir_all(&scratch)
                .map_err(|error| format!("the private scratch directory: {error}"))?;
            let record = verify_apk(&export.output_name, &main, &bytes, &plan, &scratch)
                .map_err(|reason| format!("the exported APK was not accepted: {reason}"))?;
            export.apk = Some(record);
            return Ok(());
        }
        export.pack = Some(pack_facts(
            &export.output_name,
            &bytes,
            export.kind,
            &export.bases,
        )?);
        Ok(())
    }

    /// Unpack the app bundle an export packed into `archive` and run its
    /// executable once; the unpacked bundle is removed after.
    fn smoke_bundle(&self, archive: &Path) -> SmokeFacts {
        let facts = match unpack_app(archive, self.bundle_dir) {
            Ok(executable) => match crate::artifacts::file_sha256(&executable) {
                Some(sha256) => {
                    let shown = executable
                        .strip_prefix(self.bundle_dir)
                        .map(|relative| {
                            relative
                                .components()
                                .map(|part| part.as_os_str().to_string_lossy())
                                .collect::<Vec<_>>()
                                .join("/")
                        })
                        .unwrap_or_default();
                    self.smoke(&executable, &sha256, shown)
                }
                None => SmokeFacts::not_run(
                    "the bundle's executable could not be read after unpacking".to_owned(),
                ),
            },
            Err(error) => SmokeFacts::not_run(error.to_string()),
        };
        let _ = std::fs::remove_dir_all(self.bundle_dir);
        facts
    }

    /// Run the exported build once, headless, inside its own boundary;
    /// `shown` names it relative to the output.
    fn smoke(&self, binary: &Path, sha256: &str, shown: String) -> SmokeFacts {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if let Ok(metadata) = std::fs::metadata(binary) {
                let mut permissions = metadata.permissions();
                permissions.set_mode(permissions.mode() | 0o700);
                let _ = std::fs::set_permissions(binary, permissions);
            }
        }
        let mut request = self.launch.clone();
        // The smoke run is its own journaled launch, with a transaction ID
        // derived from the export's.
        let digest =
            sha256_hex(format!("smoke:{}", self.launch.envelope.transaction_id).as_bytes());
        request.envelope.transaction_id = format!(
            "{}-{}-4{}-8{}-{}",
            &digest[0..8],
            &digest[8..12],
            &digest[13..16],
            &digest[17..20],
            &digest[20..32]
        );
        request.envelope.idempotency_id = format!("{}-smoke", self.launch.envelope.idempotency_id);
        request.program = binary.to_path_buf();
        request.binary_sha256 = sha256.to_owned();
        request.arguments = vec!["--headless".to_owned(), "--quit".to_owned()];
        request.deadline = SMOKE_DEADLINE;
        match LaunchProtocol::new(self.journal).launch(
            &request,
            self.provider,
            &FaultInjection::none(),
            &SilentObserver,
        ) {
            Ok(outcome) => SmokeFacts {
                ran: true,
                passed: outcome.diagnostics.termination == TerminationKind::ExitedClean,
                exit_code: outcome.diagnostics.exit_code,
                termination: Some(termination(&outcome.diagnostics.termination)),
                reason: None,
                executable: Some(shown),
            },
            Err(error) => SmokeFacts::not_run(error.to_string()),
        }
    }
}

fn termination(kind: &TerminationKind) -> String {
    match kind {
        TerminationKind::ExitedClean => "exited_clean".to_owned(),
        TerminationKind::NonzeroExit { code } => format!("nonzero_exit({code})"),
        TerminationKind::Signaled { signal } => format!("signaled({signal})"),
        TerminationKind::TimedOut => "lifetime_ended".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_version_line_names_the_template_directory() {
        assert_eq!(
            template_label_of("4.7.1.stable.official.a13da4feb"),
            Some("4.7.1.stable".to_owned())
        );
        assert_eq!(
            template_label_of("4.4.stable.official.4c311cbee"),
            Some("4.4.stable".to_owned())
        );
        assert_eq!(
            template_label_of("4.7.1.stable.official.fakegame"),
            Some("4.7.1.stable".to_owned())
        );
        assert_eq!(template_label_of("4.7.1"), None);
        assert_eq!(template_label_of("garbage"), None);
    }

    #[test]
    fn an_output_name_never_leaves_the_output_directory() {
        assert_eq!(
            output_name("build/game.x86_64", "Linux", "x86_64"),
            "game.x86_64"
        );
        assert_eq!(output_name("../../etc/passwd", "Linux", "x86_64"), "passwd");
        assert_eq!(output_name("C:\\out\\Game.exe", "Win", "exe"), "Game.exe");
        assert_eq!(output_name("", "My Game", "x86_64"), "My_Game.x86_64");
        assert_eq!(output_name("build/..", "Linux", "x86_64"), "Linux.x86_64");
        assert_eq!(output_name("bad name!.exe", "W", "exe"), "bad_name_.exe");
    }

    #[test]
    fn an_app_bundle_is_always_exported_as_one_zip() {
        for (export_path, expected) in [
            ("build/game.app", "game.zip"),
            ("build/game.dmg", "game.zip"),
            ("build/game.zip", "game.zip"),
            ("build/game", "game.zip"),
            ("build/My.Game.app", "My.Game.zip"),
            ("../../outside.app", "outside.zip"),
            ("", "macOS.zip"),
            ("build/..", "macOS.zip"),
        ] {
            assert_eq!(
                archive_output_name(export_path, "macOS", "zip"),
                expected,
                "{export_path:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_engine_home_gets_exactly_one_link_per_build() {
        let home = tempfile::tempdir().expect("home");
        let templates = tempfile::tempdir().expect("templates");
        let files = ["version.txt"];
        link_templates_for(
            TemplateHost::Linux,
            home.path(),
            "4.7.1.stable",
            templates.path(),
            &files,
        )
        .expect("link");
        link_templates_for(
            TemplateHost::Linux,
            home.path(),
            "4.7.1.stable",
            templates.path(),
            &files,
        )
        .expect("relink");
        let link = home
            .path()
            .join(".local/share/godot/export_templates/4.7.1.stable");
        assert_eq!(std::fs::read_link(&link).expect("a link"), templates.path());
        // A real directory there is not replaced.
        let other = home
            .path()
            .join(".local/share/godot/export_templates/4.6.1.stable");
        std::fs::create_dir_all(&other).expect("directory");
        assert!(
            link_templates_for(
                TemplateHost::Linux,
                home.path(),
                "4.6.1.stable",
                templates.path(),
                &files
            )
            .is_err()
        );
    }

    #[test]
    fn each_host_finds_its_templates_where_godot_itself_looks() {
        let home = Path::new("/state/engine-home");
        for (host, expected) in [
            (
                TemplateHost::Linux,
                ".local/share/godot/export_templates/4.7.1.stable",
            ),
            (
                TemplateHost::MacOs,
                "Library/Application Support/Godot/export_templates/4.7.1.stable",
            ),
            (
                TemplateHost::Windows,
                "AppData/Roaming/Godot/export_templates/4.7.1.stable",
            ),
        ] {
            let mut path = home.to_path_buf();
            path.extend(expected.split('/'));
            assert_eq!(
                templates_entry(host, home, "4.7.1.stable"),
                path,
                "{host:?}"
            );
        }
    }

    #[test]
    fn a_host_without_directory_links_gets_its_own_copies_of_the_templates_it_needs() {
        let home = tempfile::tempdir().expect("home");
        let templates = tempfile::tempdir().expect("templates");
        std::fs::write(templates.path().join("version.txt"), b"4.7.1.stable\n").expect("version");
        std::fs::write(
            templates.path().join("windows_release_x86_64.exe"),
            b"template",
        )
        .expect("template");
        std::fs::write(templates.path().join("android_release.apk"), b"unneeded")
            .expect("another template");
        let files = ["version.txt", "windows_release_x86_64.exe"];
        let entry = templates_entry(TemplateHost::Windows, home.path(), "4.7.1.stable");
        for _ in 0..2 {
            link_templates_for(
                TemplateHost::Windows,
                home.path(),
                "4.7.1.stable",
                templates.path(),
                &files,
            )
            .expect("the templates are placed");
            assert_eq!(
                std::fs::read(entry.join("windows_release_x86_64.exe")).expect("placed"),
                b"template"
            );
            assert!(
                !entry.join("android_release.apk").exists(),
                "only what the export needs"
            );
        }
        // A directory there that this server did not place is never replaced.
        let foreign = templates_entry(TemplateHost::Windows, home.path(), "4.6.1.stable");
        std::fs::create_dir_all(&foreign).expect("a foreign directory");
        assert!(
            link_templates_for(
                TemplateHost::Windows,
                home.path(),
                "4.6.1.stable",
                templates.path(),
                &files,
            )
            .is_err()
        );
    }

    #[test]
    fn an_export_whose_outputs_do_not_all_register_keeps_none_of_them() {
        use godot_mcp_domain::objid::{LeaseKey, StableObjectId};

        let output = tempfile::tempdir().expect("output");
        std::fs::write(output.path().join("a.pck"), b"four").expect("first output");
        std::fs::write(output.path().join("b.x86_64"), b"eight by").expect("second output");
        let store = Arc::new(ArtifactStore::in_memory());
        store.attach_limits(Arc::new(
            crate::limits::LimitEnforcer::new(crate::limits::LimitPolicy {
                max_artifact_total_bytes: 10,
                ..crate::limits::LimitPolicy::default()
            })
            .expect("limits"),
        ));
        let artifacts = ExportArtifacts {
            store: Arc::clone(&store),
            project: ProjectIdentity::from_lease_key(
                LeaseKey {
                    root: StableObjectId::Unix { dev: 1, ino: 2 },
                    project_file: StableObjectId::Unix { dev: 1, ino: 3 },
                },
                1,
            ),
            retention_ms: 60_000,
            max_range_bytes: 1024,
        };
        let record = ExportRecord::planned(&ExportPlan {
            preset: "Linux".to_owned(),
            platform: "Linux".to_owned(),
            mode: "release",
            template_label: "4.7.1.stable".to_owned(),
            template: "linux_release.x86_64".to_owned(),
            output_name: "b.x86_64".to_owned(),
            smoke: false,
            runnable_here: true,
            owner_principal: None,
            owner_session: None,
            kind: ExportKind::Project,
            bases: Vec::new(),
            android: None,
        });
        let refused = register_all(
            &artifacts,
            &record,
            "export_0123456789abcdef0123456789abcdef",
            &output_files(output.path()),
            1_000,
        )
        .expect_err("the second output does not fit the budget");
        assert!(refused.contains("b.x86_64"), "{refused}");
        assert!(
            store.contracts().is_empty(),
            "nothing of an export that failed to register stays registered"
        );
    }

    #[test]
    fn an_export_belongs_to_the_principal_that_started_it() {
        let plan = |owner: Option<&str>| ExportPlan {
            preset: "Linux".to_owned(),
            platform: "Linux".to_owned(),
            mode: "release",
            template_label: "4.7.1.stable".to_owned(),
            template: "linux_release.x86_64".to_owned(),
            output_name: "game.x86_64".to_owned(),
            smoke: false,
            runnable_here: true,
            owner_principal: owner.map(ToOwned::to_owned),
            owner_session: None,
            kind: ExportKind::Project,
            bases: Vec::new(),
            android: None,
        };
        let owned = ExportRecord::planned(&plan(Some("principal-alice")));
        assert!(owned.belongs_to("principal-alice"));
        assert!(!owned.belongs_to("principal-bob"));
        let unowned = ExportRecord::planned(&plan(None));
        assert!(
            unowned.belongs_to("principal-bob"),
            "an unowned record is anyone's"
        );
    }

    fn pck(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
        use godot_mcp_engine::export::pack::fixture::{Entry, pck};
        let entries: Vec<Entry<'_>> = entries
            .iter()
            .map(|(path, data, flags)| Entry {
                path,
                data,
                flags: *flags,
            })
            .collect();
        pck(4, (4, 7, 1), &entries)
    }

    #[test]
    fn a_pack_is_answered_with_its_files_and_their_checked_digests() {
        let bytes = pck(&[
            ("data/levels.json", b"{}", 0),
            ("project.binary", b"ECFG", 0),
        ]);
        let facts = pack_facts("game.pck", &bytes, ExportKind::Pack, &[]).expect("a pack");
        assert_eq!(facts["readable"], json!(true));
        assert_eq!(facts["format_version"], json!(4));
        assert_eq!(facts["engine_version"], json!("4.7.1"));
        assert_eq!(facts["files"], json!(2));
        assert_eq!(facts["entries"][0]["path"], json!("data/levels.json"));
        assert_eq!(facts["entries"][0]["digest"], json!("verified"));
        assert_eq!(facts["truncated"], json!(false));
        assert!(facts.get("patch").is_none());
        let mut damaged = bytes.clone();
        damaged[0x70] ^= 1;
        let refused = pack_facts("game.pck", &damaged, ExportKind::Pack, &[])
            .expect_err("a damaged pack is not a result");
        assert!(refused.contains("data/levels.json"), "{refused}");
        let mut encrypted = bytes;
        encrypted[20] |= 1;
        let unreadable = pack_facts("game.pck", &encrypted, ExportKind::Pack, &[])
            .expect("an encrypted directory is answered, not guessed at");
        assert_eq!(unreadable["readable"], json!(false));
        assert!(
            unreadable["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("encrypted"))
        );
    }

    #[test]
    fn a_patch_counts_its_changes_and_flags_an_unchanged_file() {
        let directory = tempfile::tempdir().expect("tempdir");
        let base_path = directory.path().join("base.pck");
        std::fs::write(
            &base_path,
            pck(&[
                ("data/levels.json", b"{}", 0),
                ("project.binary", b"ECFG", 0),
            ]),
        )
        .expect("base");
        let base = BasePack {
            source: "base.pck".to_owned(),
            sha256: String::new(),
            bytes: 0,
            copy: base_path,
        };
        let patch = pck(&[
            ("data/extra.json", b"[]", 0),
            ("data/levels.json", b"{\"a\": 1}", 0),
            ("project.binary", b"ECFG", 0),
        ]);
        let facts = pack_facts(
            "patch.pck",
            &patch,
            ExportKind::Patch,
            std::slice::from_ref(&base),
        )
        .expect("a patch");
        let summary = &facts["patch"];
        assert_eq!(summary["new"], json!(1), "{facts}");
        assert_eq!(summary["changed"], json!(1), "{facts}");
        assert_eq!(summary["identical"], json!(1), "{facts}");
        assert_eq!(summary["identical_paths"], json!(["project.binary"]));
        assert_eq!(summary["changed_files_only"], json!(false));
        assert_eq!(facts["entries"][2]["change"], json!("identical"));
    }

    #[test]
    fn a_zip_pack_lists_its_files() {
        use godot_mcp_engine::export::bundle::fixture::{Entry, stored_zip};
        let archive = stored_zip(&[Entry {
            name: "data/levels.json",
            data: b"{}",
            mode: 0o100_644,
        }]);
        let facts = pack_facts("game.zip", &archive, ExportKind::Pack, &[]).expect("a ZIP pack");
        assert_eq!(facts["format"], json!("zip"));
        assert_eq!(facts["files"], json!(1));
        assert_eq!(facts["entries"][0]["path"], json!("data/levels.json"));
    }

    #[test]
    fn godots_nothing_to_export_line_is_recognised_on_either_stream() {
        use godot_mcp_engine::supervision::gate::CapturedStream;
        let stream = |text: &str| CapturedStream {
            bytes: text.as_bytes().to_vec(),
            total_bytes: text.len() as u64,
            truncated: false,
        };
        let diagnostics = |stdout: &str, stderr: &str| LaunchDiagnostics {
            termination: TerminationKind::NonzeroExit { code: 1 },
            exit_code: Some(1),
            signal: None,
            stdout: stream(stdout),
            stderr: stream(stderr),
        };
        assert!(nothing_to_export(&diagnostics(
            "",
            "ERROR: Save PCK: No files or changes to export.\n"
        )));
        assert!(nothing_to_export(&diagnostics(
            "ERROR: Save PCK: No files or changes to export.",
            ""
        )));
        assert!(!nothing_to_export(&diagnostics(
            "",
            "ERROR: Project export failed."
        )));
    }
}
