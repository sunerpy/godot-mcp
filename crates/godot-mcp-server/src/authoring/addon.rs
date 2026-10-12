//! `editor.addon.status`, `editor.addon.install`, `editor.addon.update` and
//! `editor.addon.remove`: the bundled `godot_mcp` editor add-on as project
//! content with recorded ownership.
//!
//! The ownership manifest, `addons/godot_mcp/install-manifest.json`, records
//! the SHA-256 of every file this server wrote. A file is then one of:
//!
//! * **current** — owned, unchanged since it was written, and byte-identical
//!   to the bundled add-on;
//! * **outdated** — owned and unchanged, but the bundled add-on differs: an
//!   update replaces it;
//! * **edited** — owned, but its bytes are no longer what was written: an
//!   update keeps the edit as `<name>.godot-mcp-backup` before replacing it,
//!   and a removal leaves it in place;
//! * **missing** — owned, but gone: an update writes it again;
//! * **foreign** — present but never written by this server: install and
//!   update refuse rather than overwrite it.
//!
//! Godot 4.4+ writes a `<script>.uid` file beside each script it indexes;
//! those are derived from owned scripts, never a conflict, and removed with
//! them. Every change is one journaled transaction through the outcome router,
//! with an absence or content-hash precondition on every path it touches, so a
//! concurrent edit refuses rather than being overwritten.

use std::collections::BTreeMap;

use godot_mcp_bridge::addon::{ADDON_DIRECTORY, ADDON_VERSION, FILES, PLUGIN_CONFIG};
use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::catalog::AuthoringOperation;
use godot_mcp_domain::contracts::{GodotValue, OutcomeError};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::{
    AuthoringRuntime, CallContext, Envelope, Mutation, object_schema, optional_bool, outcome_error,
    outcome_error_with, settings, sha256_hex,
};
use crate::effect::{PlannedWrite, WritePrecondition};
use crate::journal::FileOperation;

/// The ownership manifest, inside [`ADDON_DIRECTORY`].
const MANIFEST_NAME: &str = "install-manifest.json";
const MANIFEST_SCHEMA: &str = "1.0.0";
/// Suffix of the copy an update keeps of an edited owned file.
const BACKUP_SUFFIX: &str = ".godot-mcp-backup";
/// The project settings file the plugin is enabled in.
const PROJECT_FILE: &str = "project.godot";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnedFile {
    path: String,
    sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BackupRecord {
    path: String,
    backup: String,
    sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnershipManifest {
    schema_version: String,
    package_version: String,
    files: Vec<OwnedFile>,
    #[serde(default)]
    backups: Vec<BackupRecord>,
}

impl OwnershipManifest {
    fn bytes(&self) -> Vec<u8> {
        let mut text = serde_json::to_string_pretty(self).expect("a manifest serializes");
        text.push('\n');
        text.into_bytes()
    }
}

fn manifest_path() -> String {
    format!("{ADDON_DIRECTORY}/{MANIFEST_NAME}")
}

fn addon_path(name: &str) -> String {
    format!("{ADDON_DIRECTORY}/{name}")
}

/// What is on disk for one bundled file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileState {
    Current,
    Outdated,
    Edited,
    Missing,
    Foreign,
    Absent,
}

impl FileState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Outdated => "outdated",
            Self::Edited => "edited",
            Self::Missing => "missing",
            Self::Foreign => "foreign",
            Self::Absent => "absent",
        }
    }
}

/// Everything the four tools decide from.
struct Survey {
    manifest: Option<(OwnershipManifest, Vec<u8>)>,
    files: Vec<SurveyedFile>,
    /// Owned paths the bundled add-on no longer ships.
    retired: Vec<(OwnedFile, Option<Vec<u8>>)>,
    project: Vec<u8>,
}

struct SurveyedFile {
    path: String,
    bundled: &'static [u8],
    current: Option<Vec<u8>>,
    state: FileState,
}

fn read(runtime: &AuthoringRuntime, path: &str) -> Result<Option<Vec<u8>>, OutcomeError> {
    runtime.read(path)
}

fn survey(runtime: &AuthoringRuntime) -> Result<Survey, OutcomeError> {
    let manifest = match read(runtime, &manifest_path())? {
        None => None,
        Some(bytes) => {
            let parsed: OwnershipManifest = serde_json::from_slice(&bytes).map_err(|error| {
                outcome_error(
                    ErrorKind::Conflict,
                    format!(
                        "{} is not an add-on ownership manifest ({error}); move it aside first",
                        manifest_path()
                    ),
                )
            })?;
            Some((parsed, bytes))
        }
    };
    let owned: BTreeMap<String, String> = manifest
        .as_ref()
        .map(|(manifest, _)| {
            manifest
                .files
                .iter()
                .map(|file| (file.path.clone(), file.sha256.clone()))
                .collect()
        })
        .unwrap_or_default();
    let mut files = Vec::with_capacity(FILES.len());
    for file in FILES {
        let path = addon_path(file.name);
        let current = read(runtime, &path)?;
        let state = match (owned.get(&path), &current) {
            (None, None) => FileState::Absent,
            (None, Some(_)) => FileState::Foreign,
            (Some(_), None) => FileState::Missing,
            (Some(recorded), Some(bytes)) if &sha256_hex(bytes) != recorded => FileState::Edited,
            (Some(_), Some(bytes)) if bytes.as_slice() == file.bytes => FileState::Current,
            (Some(_), Some(_)) => FileState::Outdated,
        };
        files.push(SurveyedFile {
            path,
            bundled: file.bytes,
            current,
            state,
        });
    }
    let mut retired = Vec::new();
    if let Some((manifest, _)) = &manifest {
        for file in &manifest.files {
            if !files.iter().any(|surveyed| surveyed.path == file.path) {
                retired.push((file.clone(), read(runtime, &file.path)?));
            }
        }
    }
    let project = read(runtime, PROJECT_FILE)?.unwrap_or_default();
    Ok(Survey {
        manifest,
        files,
        retired,
        project,
    })
}

fn enabled(project: &[u8]) -> bool {
    settings::plugin_enabled(&String::from_utf8_lossy(project), PLUGIN_CONFIG)
}

fn file_report(survey: &Survey) -> Vec<Value> {
    survey
        .files
        .iter()
        .map(|file| {
            json!({
                "path": file.path,
                "state": file.state.as_str(),
                "sha256": file.current.as_deref().map(sha256_hex),
                "bundled_sha256": sha256_hex(file.bundled),
            })
        })
        .collect()
}

pub(super) fn status_schema() -> Value {
    object_schema(json!({}), &[], false)
}

pub(super) fn install_schema() -> Value {
    object_schema(
        json!({
            "enable": {
                "type": "boolean",
                "description": "Also enable the plugin in project.godot (default false)."
            }
        }),
        &[],
        true,
    )
}

pub(super) fn update_schema() -> Value {
    object_schema(json!({}), &[], true)
}

pub(super) fn remove_schema() -> Value {
    object_schema(json!({}), &[], true)
}

pub(super) fn status(runtime: &AuthoringRuntime) -> Result<Value, OutcomeError> {
    let survey = survey(runtime)?;
    let conflicts: Vec<&str> = survey
        .files
        .iter()
        .filter(|file| file.state == FileState::Foreign)
        .map(|file| file.path.as_str())
        .collect();
    let update_changes = survey.manifest.is_some()
        && (survey
            .files
            .iter()
            .any(|file| file.state != FileState::Current)
            || !survey.retired.is_empty());
    Ok(json!({
        "directory": ADDON_DIRECTORY,
        "installed": survey.manifest.is_some(),
        "installed_version": survey.manifest.as_ref().map(|(manifest, _)| manifest.package_version.clone()),
        "bundled_version": ADDON_VERSION,
        "enabled": enabled(&survey.project),
        "plugin": PLUGIN_CONFIG,
        "files": file_report(&survey),
        "retired": survey.retired.iter().map(|(file, _)| file.path.clone()).collect::<Vec<_>>(),
        "conflicts": conflicts,
        "install_allowed": survey.manifest.is_none() && survey.files.iter().all(|file| file.state == FileState::Absent),
        "update_changes": update_changes,
    }))
}

fn foreign_conflict(paths: &[&str]) -> OutcomeError {
    outcome_error_with(
        ErrorKind::Conflict,
        format!(
            "{} {} not written by this server; move {} aside, then retry. Nothing was written.",
            paths.join(", "),
            if paths.len() == 1 { "is" } else { "are" },
            if paths.len() == 1 { "it" } else { "them" }
        ),
        [(
            "paths",
            GodotValue::Array(
                paths
                    .iter()
                    .map(|path| GodotValue::String((*path).to_owned()))
                    .collect(),
            ),
        )],
    )
}

fn project_write(survey: &Survey, enable: bool) -> Option<PlannedWrite> {
    let text = String::from_utf8_lossy(&survey.project);
    settings::with_plugin(&text, PLUGIN_CONFIG, enable).map(|patched| PlannedWrite {
        path: PROJECT_FILE.to_owned(),
        operation: FileOperation::Update,
        bytes: Some(patched.into_bytes()),
        precondition: WritePrecondition::Sha256(sha256_hex(&survey.project)),
    })
}

fn manifest_for(files: Vec<OwnedFile>, backups: Vec<BackupRecord>) -> OwnershipManifest {
    OwnershipManifest {
        schema_version: MANIFEST_SCHEMA.to_owned(),
        package_version: ADDON_VERSION.to_owned(),
        files,
        backups,
    }
}

async fn commit(
    runtime: &AuthoringRuntime,
    tool: &'static str,
    writes: Vec<PlannedWrite>,
    mut result: Map<String, Value>,
    envelope: &Envelope,
    canonical: &Value,
    context: &CallContext<'_>,
) -> Result<Value, OutcomeError> {
    result.insert(
        "writes".to_owned(),
        Value::Array(
            writes
                .iter()
                .map(|write| {
                    json!({
                        "path": write.path,
                        "operation": match write.operation {
                            FileOperation::Create => "create",
                            FileOperation::Update => "update",
                            FileOperation::Delete => "delete",
                        },
                    })
                })
                .collect(),
        ),
    );
    if envelope.dry_run {
        result.insert("dry_run".to_owned(), Value::Bool(true));
        result.insert(
            "workspace".to_owned(),
            runtime.workspace.snapshot().to_value(),
        );
        return Ok(Value::Object(result));
    }
    if writes.is_empty() {
        result.insert("changed".to_owned(), Value::Bool(false));
        result.insert("dry_run".to_owned(), Value::Bool(false));
        result.insert(
            "workspace".to_owned(),
            runtime.workspace.snapshot().to_value(),
        );
        return Ok(Value::Object(result));
    }
    result.insert("changed".to_owned(), Value::Bool(true));
    let committed = runtime
        .commit(
            Mutation {
                tool,
                primary_path: addon_path("plugin.cfg"),
                writes,
                expected_revision: envelope.expected_revision,
            },
            canonical,
            context,
        )
        .await?;
    Ok(committed.into_value(result))
}

pub(super) async fn install(
    runtime: &AuthoringRuntime,
    arguments: &Map<String, Value>,
    envelope: &Envelope,
    canonical: &Value,
    context: &CallContext<'_>,
) -> Result<Value, OutcomeError> {
    let enable = optional_bool(arguments, "enable")?.unwrap_or(false);
    let survey = survey(runtime)?;
    if survey.manifest.is_some() {
        return Err(outcome_error(
            ErrorKind::Conflict,
            "the godot_mcp add-on is already installed; use editor.addon.update",
        ));
    }
    let occupied: Vec<&str> = survey
        .files
        .iter()
        .filter(|file| file.state != FileState::Absent)
        .map(|file| file.path.as_str())
        .collect();
    if !occupied.is_empty() {
        return Err(foreign_conflict(&occupied));
    }
    let mut writes: Vec<PlannedWrite> = survey
        .files
        .iter()
        .map(|file| PlannedWrite {
            path: file.path.clone(),
            operation: FileOperation::Create,
            bytes: Some(file.bundled.to_vec()),
            precondition: WritePrecondition::Absent,
        })
        .collect();
    let owned = survey
        .files
        .iter()
        .map(|file| OwnedFile {
            path: file.path.clone(),
            sha256: sha256_hex(file.bundled),
        })
        .collect();
    writes.push(PlannedWrite {
        path: manifest_path(),
        operation: FileOperation::Create,
        bytes: Some(manifest_for(owned, Vec::new()).bytes()),
        precondition: WritePrecondition::Absent,
    });
    if enable && let Some(write) = project_write(&survey, true) {
        writes.push(write);
    }
    let mut result = Map::new();
    result.insert(
        "version".to_owned(),
        Value::String(ADDON_VERSION.to_owned()),
    );
    result.insert("plugin".to_owned(), Value::String(PLUGIN_CONFIG.to_owned()));
    result.insert(
        "enabled".to_owned(),
        Value::Bool(enable || enabled(&survey.project)),
    );
    commit(
        runtime,
        AuthoringOperation::AddonInstall.tool_name(),
        writes,
        result,
        envelope,
        canonical,
        context,
    )
    .await
}

pub(super) async fn update(
    runtime: &AuthoringRuntime,
    _arguments: &Map<String, Value>,
    envelope: &Envelope,
    canonical: &Value,
    context: &CallContext<'_>,
) -> Result<Value, OutcomeError> {
    let survey = survey(runtime)?;
    let Some((manifest, manifest_bytes)) = &survey.manifest else {
        return Err(outcome_error(
            ErrorKind::Conflict,
            "the godot_mcp add-on is not installed; use editor.addon.install",
        ));
    };
    let foreign: Vec<&str> = survey
        .files
        .iter()
        .filter(|file| file.state == FileState::Foreign)
        .map(|file| file.path.as_str())
        .collect();
    if !foreign.is_empty() {
        return Err(foreign_conflict(&foreign));
    }
    let mut writes = Vec::new();
    let mut backups = manifest.backups.clone();
    let mut kept = Vec::new();
    for file in &survey.files {
        match file.state {
            FileState::Current | FileState::Foreign => {}
            FileState::Missing | FileState::Absent => writes.push(PlannedWrite {
                path: file.path.clone(),
                operation: FileOperation::Create,
                bytes: Some(file.bundled.to_vec()),
                precondition: WritePrecondition::Absent,
            }),
            FileState::Outdated | FileState::Edited => {
                let current = file.current.as_deref().unwrap_or_default();
                if file.state == FileState::Edited {
                    let backup = format!("{}{BACKUP_SUFFIX}", file.path);
                    if read(runtime, &backup)?.is_some() {
                        return Err(foreign_conflict(&[backup.as_str()]));
                    }
                    writes.push(PlannedWrite {
                        path: backup.clone(),
                        operation: FileOperation::Create,
                        bytes: Some(current.to_vec()),
                        precondition: WritePrecondition::Absent,
                    });
                    backups.push(BackupRecord {
                        path: file.path.clone(),
                        backup: backup.clone(),
                        sha256: sha256_hex(current),
                    });
                    kept.push(backup);
                }
                writes.push(PlannedWrite {
                    path: file.path.clone(),
                    operation: FileOperation::Update,
                    bytes: Some(file.bundled.to_vec()),
                    precondition: WritePrecondition::Sha256(sha256_hex(current)),
                });
            }
        }
    }
    let mut owned: Vec<OwnedFile> = survey
        .files
        .iter()
        .map(|file| OwnedFile {
            path: file.path.clone(),
            sha256: sha256_hex(file.bundled),
        })
        .collect();
    for (retired, current) in &survey.retired {
        match current {
            Some(bytes) if sha256_hex(bytes) == retired.sha256 => writes.push(PlannedWrite {
                path: retired.path.clone(),
                operation: FileOperation::Delete,
                bytes: None,
                precondition: WritePrecondition::Sha256(retired.sha256.clone()),
            }),
            // An edited retired file stays, and stays recorded as owned.
            Some(_) => owned.push(retired.clone()),
            None => {}
        }
    }
    if !writes.is_empty() {
        writes.push(PlannedWrite {
            path: manifest_path(),
            operation: FileOperation::Update,
            bytes: Some(manifest_for(owned, backups).bytes()),
            precondition: WritePrecondition::Sha256(sha256_hex(manifest_bytes)),
        });
    }
    let mut result = Map::new();
    result.insert(
        "version".to_owned(),
        Value::String(ADDON_VERSION.to_owned()),
    );
    result.insert(
        "previous_version".to_owned(),
        Value::String(manifest.package_version.clone()),
    );
    result.insert(
        "backups".to_owned(),
        Value::Array(kept.into_iter().map(Value::String).collect()),
    );
    commit(
        runtime,
        AuthoringOperation::AddonUpdate.tool_name(),
        writes,
        result,
        envelope,
        canonical,
        context,
    )
    .await
}

pub(super) async fn remove(
    runtime: &AuthoringRuntime,
    _arguments: &Map<String, Value>,
    envelope: &Envelope,
    canonical: &Value,
    context: &CallContext<'_>,
) -> Result<Value, OutcomeError> {
    let survey = survey(runtime)?;
    let Some((manifest, manifest_bytes)) = &survey.manifest else {
        return Err(outcome_error(
            ErrorKind::Conflict,
            "the godot_mcp add-on is not installed here",
        ));
    };
    let mut writes = Vec::new();
    let mut retained = Vec::new();
    for owned in &manifest.files {
        let Some(current) = read(runtime, &owned.path)? else {
            continue;
        };
        if sha256_hex(&current) != owned.sha256 {
            retained.push(owned.path.clone());
            continue;
        }
        writes.push(PlannedWrite {
            path: owned.path.clone(),
            operation: FileOperation::Delete,
            bytes: None,
            precondition: WritePrecondition::Sha256(owned.sha256.clone()),
        });
        // The UID file Godot derives for a script goes with it.
        let sidecar = format!("{}.uid", owned.path);
        if let Some(uid) = read(runtime, &sidecar)? {
            writes.push(PlannedWrite {
                path: sidecar,
                operation: FileOperation::Delete,
                bytes: None,
                precondition: WritePrecondition::Sha256(sha256_hex(&uid)),
            });
        }
    }
    writes.push(PlannedWrite {
        path: manifest_path(),
        operation: FileOperation::Delete,
        bytes: None,
        precondition: WritePrecondition::Sha256(sha256_hex(manifest_bytes)),
    });
    if let Some(write) = project_write(&survey, false) {
        writes.push(write);
    }
    let mut result = Map::new();
    result.insert(
        "retained".to_owned(),
        Value::Array(retained.into_iter().map(Value::String).collect()),
    );
    result.insert("enabled".to_owned(), Value::Bool(false));
    commit(
        runtime,
        AuthoringOperation::AddonRemove.tool_name(),
        writes,
        result,
        envelope,
        canonical,
        context,
    )
    .await
}
