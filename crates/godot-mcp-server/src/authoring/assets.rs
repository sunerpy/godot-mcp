//! `asset.put`, `asset.delete` and `asset.status`: binary project assets and
//! their import state.
//!
//! A small asset is uploaded inline. A larger one arrives as ordered chunks
//! staged in the private state root — bound to the uploading principal, bounded
//! in size and count, and expiring — and is published as one journaled file
//! effect only once every byte arrived and the whole hashes to the declared
//! SHA-256. Nothing is published from a partial upload.
//!
//! Import state is read from the engine's own records: the `.import` sidecar
//! and the files it names under the engine cache. Those paths are never
//! returned; an artifact is described by its kind, length and SHA-256.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::catalog::AuthoringOperation;
use godot_mcp_domain::contracts::OutcomeError;
use godot_mcp_domain::safe_root::{SafeRoot, SafeRootError};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::paths;
use super::{
    AuthoringRuntime, CallContext, Envelope, Mutation, already_exists, base64, files, md5,
    object_schema, optional_u64, outcome_error, path_property, precondition_mismatch, required_str,
    sha256_argument, sha256_hex, sha256_property,
};
use crate::effect::{PlannedWrite, WritePrecondition};
use crate::journal::FileOperation;

/// Largest asset `asset.put` accepts.
const MAX_ASSET_BYTES: u64 = 64 * 1024 * 1024;
/// Most uploads that may be in progress at once.
const MAX_UPLOADS: usize = 8;
/// How long an unfinished upload is kept.
const UPLOAD_LIFETIME_MS: u64 = 60 * 60 * 1000;

/// Where one chunk lands: the upload it continues and its byte offset. The
/// first chunk of an upload names no `upload` and starts at offset 0.
fn chunk_position(arguments: &Map<String, Value>) -> Result<(Option<&str>, u64), OutcomeError> {
    let upload = match arguments.get("upload") {
        None => return Ok((None, 0)),
        Some(Value::Object(upload)) => upload,
        Some(_) => {
            return Err(outcome_error(
                ErrorKind::InvalidInput,
                "`upload` must be an object with the upload's `id` and this chunk's `offset`",
            ));
        }
    };
    if let Some(unknown) = upload
        .keys()
        .find(|key| !matches!(key.as_str(), "id" | "offset"))
    {
        return Err(outcome_error(
            ErrorKind::InvalidInput,
            format!("`upload` does not accept `{unknown}`"),
        ));
    }
    let id = required_str(upload, "id")?;
    if !Uploads::valid_id(id) {
        return Err(outcome_error(
            ErrorKind::InvalidInput,
            "`upload.id` is not an upload this server issued",
        ));
    }
    let offset = optional_u64(upload, "offset")?.ok_or_else(|| {
        outcome_error(
            ErrorKind::InvalidInput,
            "`upload.offset` is required: this chunk's byte offset",
        )
    })?;
    Ok((Some(id), offset))
}

pub(super) fn put_schema() -> Value {
    object_schema(
        json!({
            "path": path_property("Project-relative path of the asset."),
            "contentBase64": {
                "type": "string",
                "description": "Base64 bytes: the whole asset, or one chunk of an upload."
            },
            "sha256": sha256_property("SHA-256 of the complete asset."),
            "totalBytes": {
                "type": "integer",
                "minimum": 0,
                "maximum": MAX_ASSET_BYTES,
                "description": "Complete asset length; larger than this chunk for a chunked upload."
            },
            "upload": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "id": {
                        "type": "string",
                        "pattern": "^[0-9a-f]{32}$",
                        "description": "The upload_id the upload's first chunk returned."
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "This chunk's byte offset: the next_offset the previous \
                                        chunk returned."
                    }
                },
                "required": ["id", "offset"],
                "description": "Continues a chunked upload; omitted on its first chunk, which \
                                starts at offset 0."
            },
            "expectedSha256": sha256_property(
                "SHA-256 of the asset being replaced; omit to create a new asset."
            )
        }),
        &["path", "contentBase64", "sha256"],
        true,
    )
}

pub(super) fn delete_schema() -> Value {
    object_schema(
        json!({
            "path": path_property("Project-relative path of the asset to delete."),
            "expectedSha256": sha256_property("SHA-256 of the asset's current bytes.")
        }),
        &["path", "expectedSha256"],
        true,
    )
}

pub(super) fn status_schema() -> Value {
    object_schema(
        json!({ "path": path_property("Project-relative path of the asset.") }),
        &["path"],
        false,
    )
}

/// Refuse a path the asset tools do not own.
fn asset_path(raw: &str) -> Result<String, OutcomeError> {
    let path = paths::normalize(raw, "path")?;
    match paths::writable(&path)? {
        paths::WriteOwner::Script => Err(outcome_error(
            ErrorKind::InvalidInput,
            format!("{path} is a GDScript source; it is written through script.create"),
        )),
        _ if matches!(
            paths::extension(&path).as_str(),
            "tscn" | "tres" | "gdshader" | "gdshaderinc"
        ) =>
        {
            Err(outcome_error(
                ErrorKind::InvalidInput,
                format!("{path} is a text source; it is written through file.create"),
            ))
        }
        _ => Ok(path),
    }
}

// ---------------------------------------------------------------------------
// Chunked uploads
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct UploadRecord {
    path: String,
    total_bytes: u64,
    sha256: String,
    expected_sha256: Option<String>,
    principal: String,
    created_at_unix_ms: u64,
}

/// The state of an upload after one chunk.
enum Progress {
    Partial { upload_id: String, received: u64 },
    Complete { upload_id: String, bytes: Vec<u8> },
}

/// One chunk of a chunked upload.
struct Chunk<'a> {
    upload_id: Option<&'a str>,
    record: UploadRecord,
    offset: u64,
    bytes: &'a [u8],
    now_unix_ms: u64,
}

/// Private, principal-bound staging for chunked uploads.
pub(super) struct Uploads {
    dir: PathBuf,
    guard: Mutex<()>,
    counter: AtomicU64,
}

fn upload_error(error: impl std::fmt::Display) -> OutcomeError {
    outcome_error(
        ErrorKind::Internal,
        format!("the private upload staging failed: {error}"),
    )
}

impl Uploads {
    pub(super) fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            guard: Mutex::new(()),
            counter: AtomicU64::new(0),
        }
    }

    fn open_dir(&self) -> Result<&Path, OutcomeError> {
        godot_mcp_domain::sysfs::open_private_state_dir(&self.dir).map_err(upload_error)?;
        Ok(&self.dir)
    }

    fn valid_id(upload_id: &str) -> bool {
        upload_id.len() == 32
            && upload_id
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    }

    fn read_record(dir: &Path, upload_id: &str) -> Option<UploadRecord> {
        let text = std::fs::read(dir.join(format!("{upload_id}.json"))).ok()?;
        serde_json::from_slice(&text).ok()
    }

    fn discard(dir: &Path, upload_id: &str) {
        let _ = std::fs::remove_file(dir.join(format!("{upload_id}.part")));
        let _ = std::fs::remove_file(dir.join(format!("{upload_id}.json")));
    }

    /// Remove every expired upload; return how many remain.
    fn prune(dir: &Path, now_unix_ms: u64) -> usize {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };
        let mut live = 0;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(upload_id) = name.to_str().and_then(|name| name.strip_suffix(".json")) else {
                continue;
            };
            match Self::read_record(dir, upload_id) {
                Some(record)
                    if now_unix_ms.saturating_sub(record.created_at_unix_ms)
                        < UPLOAD_LIFETIME_MS =>
                {
                    live += 1;
                }
                _ => Self::discard(dir, upload_id),
            }
        }
        live
    }

    /// Drop a finished upload's staging.
    fn finish(&self, upload_id: &str) {
        let _held = self
            .guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Self::discard(&self.dir, upload_id);
    }

    fn append(&self, chunk: &Chunk<'_>) -> Result<Progress, OutcomeError> {
        use std::io::Write as _;

        let _held = self
            .guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = self.open_dir()?;
        let live = Self::prune(dir, chunk.now_unix_ms);
        let upload_id = match chunk.upload_id {
            Some(upload_id) => {
                if !Self::valid_id(upload_id) {
                    return Err(outcome_error(
                        ErrorKind::InvalidInput,
                        "`upload.id` is not an upload this server issued",
                    ));
                }
                let recorded = Self::read_record(dir, upload_id).ok_or_else(|| {
                    outcome_error(
                        ErrorKind::NotFound,
                        "the upload does not exist or expired; start again at offset 0",
                    )
                })?;
                if recorded.principal != chunk.record.principal {
                    return Err(outcome_error(
                        ErrorKind::PermissionDenied,
                        "the upload belongs to another principal",
                    ));
                }
                let expected = UploadRecord {
                    created_at_unix_ms: recorded.created_at_unix_ms,
                    ..chunk.record.clone()
                };
                if recorded != expected {
                    return Err(outcome_error(
                        ErrorKind::Conflict,
                        "this chunk names a different path, length or hash than its upload",
                    ));
                }
                upload_id.to_owned()
            }
            None => {
                if chunk.offset != 0 {
                    return Err(outcome_error(
                        ErrorKind::InvalidInput,
                        "the first chunk of an upload starts at offset 0",
                    ));
                }
                if live >= MAX_UPLOADS {
                    return Err(outcome_error(
                        ErrorKind::Unavailable,
                        format!("{MAX_UPLOADS} uploads are already in progress; finish one first"),
                    ));
                }
                let serial = self.counter.fetch_add(1, Ordering::Relaxed);
                let seed = format!(
                    "{}|{}|{}|{serial}|{}",
                    chunk.record.principal,
                    chunk.record.path,
                    chunk.now_unix_ms,
                    std::process::id()
                );
                let upload_id = sha256_hex(seed.as_bytes())[..32].to_owned();
                let record = serde_json::to_vec(&chunk.record).map_err(upload_error)?;
                std::fs::write(dir.join(format!("{upload_id}.json")), record)
                    .map_err(upload_error)?;
                std::fs::write(dir.join(format!("{upload_id}.part")), b"").map_err(upload_error)?;
                upload_id
            }
        };
        let part = dir.join(format!("{upload_id}.part"));
        let received = std::fs::metadata(&part).map_err(upload_error)?.len();
        let end = chunk.offset.saturating_add(chunk.bytes.len() as u64);
        if end > chunk.record.total_bytes {
            return Err(outcome_error(
                ErrorKind::InvalidInput,
                "the chunk runs past the declared totalBytes",
            ));
        }
        if chunk.offset == received {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&part)
                .map_err(upload_error)?;
            file.write_all(chunk.bytes).map_err(upload_error)?;
            file.sync_data().map_err(upload_error)?;
        } else if end <= received {
            // A retried chunk: it must repeat exactly the bytes already held.
            let held = std::fs::read(&part).map_err(upload_error)?;
            let start = usize::try_from(chunk.offset).unwrap_or(usize::MAX);
            if held.get(start..start + chunk.bytes.len()) != Some(chunk.bytes) {
                return Err(outcome_error(
                    ErrorKind::Conflict,
                    "a retried chunk differs from the bytes already received at that offset",
                ));
            }
        } else {
            return Err(outcome_error(
                ErrorKind::Conflict,
                format!("the upload expects the chunk at offset {received}"),
            ));
        }
        let received = std::fs::metadata(&part).map_err(upload_error)?.len();
        if received < chunk.record.total_bytes {
            return Ok(Progress::Partial {
                upload_id,
                received,
            });
        }
        let bytes = std::fs::read(&part).map_err(upload_error)?;
        Ok(Progress::Complete { upload_id, bytes })
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

pub(super) async fn put(
    runtime: &AuthoringRuntime,
    arguments: &Map<String, Value>,
    envelope: &Envelope,
    canonical: &Value,
    context: &CallContext<'_>,
) -> Result<Value, OutcomeError> {
    let path = asset_path(required_str(arguments, "path")?)?;
    let sha256 = sha256_argument(arguments, "sha256")?;
    let expected = if arguments.contains_key("expectedSha256") {
        Some(sha256_argument(arguments, "expectedSha256")?)
    } else {
        None
    };
    let chunk = base64::decode(required_str(arguments, "contentBase64")?).ok_or_else(|| {
        outcome_error(
            ErrorKind::InvalidInput,
            "`contentBase64` is not canonical padded base64",
        )
    })?;
    let total = optional_u64(arguments, "totalBytes")?.unwrap_or(chunk.len() as u64);
    if total > MAX_ASSET_BYTES {
        return Err(outcome_error(
            ErrorKind::InvalidInput,
            format!("an asset is at most {MAX_ASSET_BYTES} bytes"),
        ));
    }
    let (upload_id, offset) = chunk_position(arguments)?;
    let inline = upload_id.is_none() && chunk.len() as u64 == total;

    let (bytes, finished_upload) = if inline {
        (chunk, None)
    } else {
        if envelope.dry_run {
            return Err(outcome_error(
                ErrorKind::InvalidInput,
                "dryRun is available for a single-call upload only",
            ));
        }
        let record = UploadRecord {
            path: path.clone(),
            total_bytes: total,
            sha256: sha256.clone(),
            expected_sha256: expected.clone(),
            principal: context.access.mode_principal().to_journal_string(),
            created_at_unix_ms: context.access.observed_at_unix_ms(),
        };
        match runtime.uploads.append(&Chunk {
            upload_id,
            record,
            offset,
            bytes: &chunk,
            now_unix_ms: context.access.observed_at_unix_ms(),
        })? {
            Progress::Partial {
                upload_id,
                received,
            } => {
                return Ok(json!({
                    "path": path,
                    "upload_id": upload_id,
                    "received_bytes": received,
                    "total_bytes": total,
                    "complete": false,
                    "next_offset": received,
                }));
            }
            Progress::Complete { upload_id, bytes } => (bytes, Some(upload_id)),
        }
    };
    let result = publish(
        runtime,
        &path,
        &sha256,
        expected.as_deref(),
        bytes,
        envelope,
        canonical,
        context,
    )
    .await;
    if let Some(upload_id) = &finished_upload {
        runtime.uploads.finish(upload_id);
    }
    let mut value = result?;
    if let (Value::Object(object), Some(upload_id)) = (&mut value, finished_upload) {
        object.insert("upload_id".to_owned(), Value::String(upload_id));
        object.insert("complete".to_owned(), Value::Bool(true));
    }
    Ok(value)
}

#[allow(clippy::too_many_arguments)]
async fn publish(
    runtime: &AuthoringRuntime,
    path: &str,
    sha256: &str,
    expected: Option<&str>,
    bytes: Vec<u8>,
    envelope: &Envelope,
    canonical: &Value,
    context: &CallContext<'_>,
) -> Result<Value, OutcomeError> {
    let observed = sha256_hex(&bytes);
    if observed != sha256 {
        return Err(outcome_error(
            ErrorKind::InvalidInput,
            format!("the uploaded bytes hash to {observed}, not the declared sha256 {sha256}"),
        ));
    }
    let (snapshot, current) = runtime.coherent_read(&[path.to_owned()])?;
    envelope.check_revision(&snapshot)?;
    let current = current.into_iter().next().flatten();
    let (operation, precondition) = match (&current, expected) {
        (None, None) => (FileOperation::Create, WritePrecondition::Absent),
        (Some(existing), None) if sha256_hex(existing) == sha256 => {
            // Re-uploading the bytes already there changes nothing.
            return Ok(json!({
                "path": path,
                "before_sha256": sha256,
                "after_sha256": sha256,
                "bytes": bytes.len(),
                "changed": false,
                "dry_run": envelope.dry_run,
                "workspace": snapshot.to_value(),
            }));
        }
        (Some(existing), None) => return Err(already_exists(path, existing)),
        (Some(existing), Some(expected)) if sha256_hex(existing) == expected => (
            FileOperation::Update,
            WritePrecondition::Sha256(expected.to_owned()),
        ),
        (current, Some(expected)) => {
            return Err(precondition_mismatch(path, expected, current.as_deref()));
        }
    };
    let mut result = Map::new();
    result.insert("path".to_owned(), Value::String(path.to_owned()));
    result.insert(
        "before_sha256".to_owned(),
        current
            .as_deref()
            .map_or(Value::Null, |bytes| Value::String(sha256_hex(bytes))),
    );
    result.insert("after_sha256".to_owned(), Value::String(sha256.to_owned()));
    result.insert("bytes".to_owned(), json!(bytes.len()));
    result.insert("changed".to_owned(), Value::Bool(true));
    result.insert(
        "import".to_owned(),
        json!({ "state": "pending", "next": "run asset.import to import the new bytes" }),
    );
    if envelope.dry_run {
        result.insert("dry_run".to_owned(), Value::Bool(true));
        result.insert("workspace".to_owned(), snapshot.to_value());
        return Ok(Value::Object(result));
    }
    let committed = runtime
        .commit(
            Mutation {
                tool: AuthoringOperation::AssetPut.tool_name(),
                primary_path: path.to_owned(),
                writes: vec![PlannedWrite {
                    path: path.to_owned(),
                    operation,
                    bytes: Some(bytes),
                    precondition,
                }],
                expected_revision: envelope.expected_revision,
            },
            canonical,
            context,
        )
        .await?;
    Ok(committed.into_value(result))
}

pub(super) async fn delete(
    runtime: &AuthoringRuntime,
    arguments: &Map<String, Value>,
    envelope: &Envelope,
    canonical: &Value,
    context: &CallContext<'_>,
) -> Result<Value, OutcomeError> {
    let path = asset_path(required_str(arguments, "path")?)?;
    let expected = sha256_argument(arguments, "expectedSha256")?;
    let files::DeletePlan {
        snapshot,
        writes,
        removed,
        ..
    } = files::plan_delete(runtime, &path, &expected, envelope)?;
    let mut result = Map::new();
    result.insert("path".to_owned(), Value::String(path.clone()));
    result.insert("before_sha256".to_owned(), Value::String(expected));
    result.insert(
        "removed_sidecars".to_owned(),
        Value::Array(removed.into_iter().map(Value::String).collect()),
    );
    if envelope.dry_run {
        result.insert("dry_run".to_owned(), Value::Bool(true));
        result.insert("workspace".to_owned(), snapshot.to_value());
        return Ok(Value::Object(result));
    }
    let committed = runtime
        .commit(
            Mutation {
                tool: AuthoringOperation::AssetDelete.tool_name(),
                primary_path: path,
                writes,
                expected_revision: envelope.expected_revision,
            },
            canonical,
            context,
        )
        .await?;
    Ok(committed.into_value(result))
}

pub(super) fn status(
    runtime: &AuthoringRuntime,
    arguments: &Map<String, Value>,
) -> Result<Value, OutcomeError> {
    let path = paths::normalize(required_str(arguments, "path")?, "path")?;
    paths::readable(&path)?;
    let (snapshot, contents) = runtime.coherent_read(std::slice::from_ref(&path))?;
    if contents.first().is_none_or(Option::is_none) {
        return Err(outcome_error(
            ErrorKind::NotFound,
            format!("{path} does not exist"),
        ));
    }
    let mut value = import_status_value(&runtime.root, &path)?;
    if let Value::Object(object) = &mut value {
        object.insert("workspace".to_owned(), snapshot.to_value());
    }
    Ok(value)
}

// ---------------------------------------------------------------------------
// Import state
// ---------------------------------------------------------------------------

fn read_optional(root: &SafeRoot, path: &str) -> Result<Option<Vec<u8>>, OutcomeError> {
    match root.read(Path::new(path)) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(SafeRootError::StalePreimage { .. }) => Ok(None),
        Err(error) => Err(outcome_error(
            error.error_kind(),
            format!("the import state of {path} could not be read"),
        )),
    }
}

/// One `key=value` of a Godot config document, by section.
fn config_value<'a>(text: &'a str, section: &str, key: &str) -> Option<&'a str> {
    let mut current = "";
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            current = &trimmed[1..trimmed.len() - 1];
            continue;
        }
        if current == section
            && let Some((name, value)) = trimmed.split_once('=')
            && name.trim() == key
        {
            return Some(value.trim());
        }
    }
    None
}

fn quoted(value: &str) -> Option<String> {
    value
        .trim()
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .map(str::to_owned)
}

/// The `res://` strings of an `[...]` array literal.
fn quoted_list(value: &str) -> Vec<String> {
    value
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .filter_map(quoted)
        .collect()
}

/// The engine's import state of one asset, without any engine cache path.
///
/// # Errors
/// Returns a classified error when the asset or its records cannot be read.
pub(crate) fn import_status_value(root: &SafeRoot, path: &str) -> Result<Value, OutcomeError> {
    let source = read_optional(root, path)?
        .ok_or_else(|| outcome_error(ErrorKind::NotFound, format!("{path} does not exist")))?;
    let sidecar_path = format!("{path}.import");
    let Some(sidecar) = read_optional(root, &sidecar_path)? else {
        return Ok(json!({
            "path": path,
            "sha256": sha256_hex(&source),
            "bytes": source.len(),
            "import": {
                "state": "not_imported",
                "importer": Value::Null,
                "type": Value::Null,
                "uid": Value::Null,
                "artifacts": [],
                "source_matches_import": Value::Null,
            },
        }));
    };
    let text = String::from_utf8_lossy(&sidecar);
    let importer = config_value(&text, "remap", "importer").and_then(quoted);
    let resource_type = config_value(&text, "remap", "type").and_then(quoted);
    let uid = config_value(&text, "remap", "uid").and_then(quoted);
    let mut destinations = config_value(&text, "deps", "dest_files")
        .map(quoted_list)
        .unwrap_or_default();
    if destinations.is_empty()
        && let Some(single) = config_value(&text, "remap", "path").and_then(quoted)
    {
        destinations.push(single);
    }
    let mut artifacts = Vec::new();
    let mut all_present = true;
    for destination in &destinations {
        let Some(relative) = destination.strip_prefix("res://") else {
            continue;
        };
        let kind = paths::extension(relative);
        match read_optional(root, relative)? {
            Some(bytes) => artifacts.push(json!({
                "kind": kind,
                "bytes": bytes.len(),
                "sha256": sha256_hex(&bytes),
                "present": true,
            })),
            None => {
                all_present = false;
                artifacts.push(json!({
                    "kind": kind,
                    "bytes": Value::Null,
                    "sha256": Value::Null,
                    "present": false,
                }));
            }
        }
    }
    // The importer records the MD5 of the bytes it imported beside the
    // artifact; comparing it says whether the import reflects these bytes.
    let source_matches_import = destinations
        .first()
        .and_then(|first| first.strip_prefix("res://"))
        .and_then(|first| {
            first
                .rsplit_once('.')
                .map(|(base, _)| format!("{base}.md5"))
        })
        .map(|md5_path| read_optional(root, &md5_path))
        .transpose()?
        .flatten()
        .and_then(|record| {
            config_value(&String::from_utf8_lossy(&record), "", "source_md5")
                .and_then(quoted)
                .map(|recorded| recorded == md5::hex(&source))
        });
    let state = if destinations.is_empty() {
        "imported_without_artifacts"
    } else if !all_present {
        "import_incomplete"
    } else if source_matches_import == Some(false) {
        "stale"
    } else {
        "imported"
    };
    Ok(json!({
        "path": path,
        "sha256": sha256_hex(&source),
        "bytes": source.len(),
        "import": {
            "state": state,
            "importer": importer,
            "type": resource_type,
            "uid": uid,
            "sidecar_sha256": sha256_hex(&sidecar),
            "artifacts": artifacts,
            "source_matches_import": source_matches_import,
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `asset.put` schema and the chunk check give the same verdict on
    /// where a chunk lands, and it is the intended one: the first chunk of an
    /// upload names no upload and starts at offset 0; a later chunk names the
    /// upload's `id` and its own `offset`.
    #[test]
    fn the_asset_schema_and_the_chunk_check_agree() {
        let schema = Value::Object(crate::kernel::tool_schema(
            "asset.put",
            crate::core::CoreLimits::default(),
        ));
        let id = "0123456789abcdef0123456789abcdef";
        let cases = [
            (json!({}), true),
            (json!({ "upload": { "id": id, "offset": 10 } }), true),
            (json!({ "upload": { "id": id, "offset": 0 } }), true),
            (json!({ "upload": { "id": id } }), false),
            (json!({ "upload": { "offset": 10 } }), false),
            (json!({ "upload": { "id": id, "offset": -1 } }), false),
            (
                json!({ "upload": { "id": "not-an-upload", "offset": 1 } }),
                false,
            ),
            (
                json!({ "upload": { "id": id.to_uppercase(), "offset": 1 } }),
                false,
            ),
            (
                json!({ "upload": { "id": id, "offset": 1, "colour": "red" } }),
                false,
            ),
            (json!({ "upload": id }), false),
        ];
        for (case, expected) in &cases {
            let mut arguments = json!({
                "path": "art/icon.png",
                "contentBase64": "YWJj",
                "sha256": "0".repeat(64),
                "idempotencyId": "asset-1"
            });
            for (key, value) in case.as_object().expect("an object") {
                arguments[key] = value.clone();
            }
            assert_eq!(
                crate::schema::conformance::conforms(&schema, &arguments),
                *expected,
                "the schema's verdict on {arguments}"
            );
            let handed = arguments.as_object().cloned().expect("an object");
            assert_eq!(
                chunk_position(&handed).is_ok(),
                *expected,
                "the chunk check's verdict on {arguments}"
            );
        }
    }

    const SIDECAR: &str = "[remap]\n\nimporter=\"texture\"\ntype=\"CompressedTexture2D\"\n\
        uid=\"uid://b2n8vbxuqyxk7\"\npath=\"res://.godot/imported/icon.png-0123.ctex\"\n\
        metadata={\n\"vram_texture\": false\n}\n\n[deps]\n\n\
        source_file=\"res://icon.png\"\ndest_files=[\"res://.godot/imported/icon.png-0123.ctex\"]\n";

    #[test]
    fn sidecar_fields_are_read_by_section() {
        assert_eq!(
            config_value(SIDECAR, "remap", "importer").and_then(quoted),
            Some("texture".to_owned())
        );
        assert_eq!(
            config_value(SIDECAR, "deps", "dest_files").map(quoted_list),
            Some(vec!["res://.godot/imported/icon.png-0123.ctex".to_owned()])
        );
        assert_eq!(config_value(SIDECAR, "deps", "importer"), None);
        assert_eq!(
            config_value("source_md5=\"abc\"\ndest_md5=\"def\"\n", "", "source_md5")
                .and_then(quoted),
            Some("abc".to_owned())
        );
    }
}
