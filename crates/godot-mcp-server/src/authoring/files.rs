//! `project.files.list`, `file.get` and the generic text-file mutations.

use std::sync::Arc;

use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::catalog::AuthoringOperation;
use godot_mcp_domain::contracts::OutcomeError;
use serde_json::{Map, Value, json};

use super::diff::{BoundedDiff, unified};
use super::patch::TextChange;
use super::paths::{self, WriteOwner};
use super::{
    AuthoringRuntime, CallContext, Envelope, Mutation, already_exists, base64, object_schema,
    optional_str, optional_u64, outcome_error, path_property, precondition_mismatch, required_str,
    sha256_argument, sha256_hex, sha256_property, utf8_text,
};
use crate::effect::{PlannedWrite, WritePrecondition};
use crate::journal::FileOperation;
use crate::workspace::WorkspaceSnapshot;

/// Largest text file the generic tools create or produce.
pub(super) const MAX_TEXT_BYTES: usize = 1024 * 1024;
/// Default and largest `project.files.list` page.
const DEFAULT_PAGE: u64 = 200;
const MAX_PAGE: u64 = 1_000;
/// Default and largest `file.get` window.
const DEFAULT_READ_BYTES: u64 = 256 * 1024;
const MAX_READ_BYTES: u64 = 1024 * 1024;

pub(super) fn list_schema() -> Value {
    object_schema(
        json!({
            "prefix": {
                "type": "string",
                "maxLength": 1024,
                "description": "Only list files under this project-relative directory."
            },
            "offset": {
                "type": "integer",
                "minimum": 0,
                "description": "Number of matching files to skip."
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_PAGE,
                "description": "Maximum number of files returned."
            }
        }),
        &[],
        false,
    )
}

pub(super) fn get_schema() -> Value {
    object_schema(
        json!({
            "path": path_property("Project-relative file path, optionally res://-prefixed."),
            "offset": {
                "type": "integer",
                "minimum": 0,
                "description": "Byte offset to start the returned window at."
            },
            "maxBytes": {
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_READ_BYTES,
                "description": "Largest window to return; the SHA-256 always covers the whole file."
            }
        }),
        &["path"],
        false,
    )
}

pub(super) fn create_schema() -> Value {
    object_schema(
        json!({
            "path": path_property("Project-relative path of the new file; it must be absent."),
            "content": {
                "type": "string",
                "maxLength": MAX_TEXT_BYTES,
                "description": "The complete UTF-8 text of the new file."
            }
        }),
        &["path", "content"],
        true,
    )
}

/// The `change` of `file.patch` and `script.patch`: exactly one of a whole
/// replacement, line edits and a unified diff, as
/// [`TextChange::from_arguments`](super::patch::TextChange::from_arguments)
/// reads it.
pub(super) fn change_property() -> Value {
    let forms = [
        (
            "content",
            json!({
                "type": "string",
                "maxLength": MAX_TEXT_BYTES,
                "description": "Replace the whole file with this UTF-8 text."
            }),
        ),
        (
            "edits",
            json!({
                "type": "array",
                "minItems": 1,
                "maxItems": 200,
                "description": "Replace 1-based line ranges of the current file; \
                                endLine = startLine - 1 inserts.",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "startLine": { "type": "integer", "minimum": 1 },
                        "endLine": { "type": "integer", "minimum": 0 },
                        "text": { "type": "string" }
                    },
                    "required": ["startLine", "endLine", "text"]
                }
            }),
        ),
        (
            "unifiedDiff",
            json!({
                "type": "string",
                "maxLength": MAX_TEXT_BYTES,
                "description": "A unified diff against the current file, applied exactly \
                                with no fuzz."
            }),
        ),
    ];
    let properties: Map<String, Value> = forms
        .into_iter()
        .map(|(name, schema)| (name.to_owned(), schema))
        .collect();
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": properties,
        "minProperties": 1,
        "maxProperties": 1,
        "description": "The change: exactly one of a whole replacement (`content`), line \
                        `edits` and a strict `unifiedDiff`."
    })
}

pub(super) fn patch_schema() -> Value {
    object_schema(
        json!({
            "path": path_property("Project-relative path of the file to change."),
            "expectedSha256": sha256_property("SHA-256 of the file's current bytes."),
            "change": change_property()
        }),
        &["path", "expectedSha256", "change"],
        true,
    )
}

pub(super) fn move_schema() -> Value {
    object_schema(
        json!({
            "from": path_property("Project-relative path of the file to move."),
            "to": path_property("Project-relative destination; it must be absent."),
            "expectedSha256": sha256_property("SHA-256 of the source file's current bytes.")
        }),
        &["from", "to", "expectedSha256"],
        true,
    )
}

pub(super) fn delete_schema() -> Value {
    object_schema(
        json!({
            "path": path_property("Project-relative path of the file to delete."),
            "expectedSha256": sha256_property("SHA-256 of the file's current bytes.")
        }),
        &["path", "expectedSha256"],
        true,
    )
}

/// The public role of one project file.
pub(super) fn file_kind(path: &str) -> &'static str {
    if path == "project.godot" {
        return "manifest";
    }
    match paths::extension(path).as_str() {
        "gd" => "script",
        "tscn" => "text_scene",
        "tres" => "text_resource",
        "scn" => "binary_scene",
        "res" => "binary_resource",
        "import" => "import_metadata",
        "uid" => "uid_sidecar",
        "gdshader" | "gdshaderinc" => "shader",
        extension if paths::is_binary_asset_extension(extension) => "asset",
        _ => "file",
    }
}

fn page(arguments: &Map<String, Value>) -> Result<(usize, usize), OutcomeError> {
    let offset = optional_u64(arguments, "offset")?.unwrap_or(0);
    let limit = optional_u64(arguments, "limit")?.unwrap_or(DEFAULT_PAGE);
    if limit == 0 || limit > MAX_PAGE {
        return Err(outcome_error(
            ErrorKind::InvalidInput,
            format!("`limit` must be between 1 and {MAX_PAGE}"),
        ));
    }
    Ok((
        usize::try_from(offset).unwrap_or(usize::MAX),
        usize::try_from(limit).unwrap_or(usize::MAX),
    ))
}

pub(super) fn list(
    runtime: &AuthoringRuntime,
    arguments: &Map<String, Value>,
) -> Result<Value, OutcomeError> {
    let prefix = match optional_str(arguments, "prefix")? {
        Some(raw) if !raw.trim().is_empty() && raw.trim() != "res://" => {
            Some(paths::normalize(raw, "prefix")?)
        }
        _ => None,
    };
    let (offset, limit) = page(arguments)?;
    let snapshot = runtime.workspace.snapshot();
    let matching: Vec<String> = snapshot
        .inventory()
        .paths()
        .filter(|path| {
            prefix.as_deref().is_none_or(|prefix| {
                path.strip_prefix(prefix)
                    .is_some_and(|rest| rest.starts_with('/'))
            })
        })
        .map(str::to_owned)
        .collect();
    let page: Vec<String> = matching.iter().skip(offset).take(limit).cloned().collect();
    let (snapshot, contents) = runtime.coherent_read(&page)?;
    let files: Vec<Value> = page
        .iter()
        .zip(&contents)
        .map(|(path, bytes)| match bytes {
            Some(bytes) => json!({
                "path": path,
                "kind": file_kind(path),
                "bytes": bytes.len(),
                "sha256": sha256_hex(bytes),
            }),
            None => json!({
                "path": path,
                "kind": file_kind(path),
                "bytes": Value::Null,
                "sha256": Value::Null,
            }),
        })
        .collect();
    let next = offset.saturating_add(page.len());
    Ok(json!({
        "files": files,
        "offset": offset,
        "limit": limit,
        "total": matching.len(),
        "truncated": next < matching.len(),
        "next_offset": (next < matching.len()).then_some(next),
        "workspace": snapshot.to_value(),
    }))
}

pub(super) fn get(
    runtime: &AuthoringRuntime,
    arguments: &Map<String, Value>,
) -> Result<Value, OutcomeError> {
    let path = paths::normalize(required_str(arguments, "path")?, "path")?;
    paths::readable(&path)?;
    let offset = optional_u64(arguments, "offset")?.unwrap_or(0);
    let max_bytes = optional_u64(arguments, "maxBytes")?.unwrap_or(DEFAULT_READ_BYTES);
    if max_bytes == 0 || max_bytes > MAX_READ_BYTES {
        return Err(outcome_error(
            ErrorKind::InvalidInput,
            format!("`maxBytes` must be between 1 and {MAX_READ_BYTES}"),
        ));
    }
    let (snapshot, contents) = runtime.coherent_read(std::slice::from_ref(&path))?;
    let bytes = contents
        .into_iter()
        .next()
        .flatten()
        .ok_or_else(|| outcome_error(ErrorKind::NotFound, format!("{path} does not exist")))?;
    let start = usize::try_from(offset)
        .unwrap_or(usize::MAX)
        .min(bytes.len());
    let mut end = start
        .saturating_add(usize::try_from(max_bytes).unwrap_or(usize::MAX))
        .min(bytes.len());
    let (encoding, content) = match std::str::from_utf8(&bytes) {
        Ok(text) => {
            if !text.is_char_boundary(start) {
                return Err(outcome_error(
                    ErrorKind::InvalidInput,
                    "`offset` falls inside a UTF-8 character; use the previous next_offset",
                ));
            }
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            ("utf8", Value::String(text[start..end].to_owned()))
        }
        Err(_) => ("base64", Value::String(base64::encode(&bytes[start..end]))),
    };
    Ok(json!({
        "path": path,
        "kind": file_kind(&path),
        "sha256": sha256_hex(&bytes),
        "bytes": bytes.len(),
        "encoding": encoding,
        "offset": start,
        "content": content,
        "truncated": end < bytes.len(),
        "next_offset": (end < bytes.len()).then_some(end),
        "workspace": snapshot.to_value(),
    }))
}

/// One prepared text change: the bytes before and after, and the
/// precondition the publication is bound to.
pub(super) struct TextPlan {
    pub(super) path: String,
    pub(super) before: Option<String>,
    pub(super) after: String,
    pub(super) snapshot: Arc<WorkspaceSnapshot>,
}

impl TextPlan {
    fn diff(&self) -> BoundedDiff {
        unified(
            self.before.as_ref().map(|_| self.path.as_str()),
            Some(&self.path),
            self.before.as_deref().unwrap_or_default(),
            &self.after,
        )
    }
}

fn check_text_size(text: &str, path: &str) -> Result<(), OutcomeError> {
    if text.len() > MAX_TEXT_BYTES {
        return Err(outcome_error(
            ErrorKind::InvalidInput,
            format!("{path} would exceed the {MAX_TEXT_BYTES}-byte text limit"),
        ));
    }
    Ok(())
}

/// Plan creating `path` with `content`; the path must be absent.
pub(super) fn plan_create(
    runtime: &AuthoringRuntime,
    path: &str,
    content: &str,
    envelope: &Envelope,
) -> Result<TextPlan, OutcomeError> {
    check_text_size(content, path)?;
    let (snapshot, current) = runtime.coherent_read(&[path.to_owned()])?;
    envelope.check_revision(&snapshot)?;
    if let Some(existing) = current.into_iter().next().flatten() {
        return Err(already_exists(path, &existing));
    }
    Ok(TextPlan {
        path: path.to_owned(),
        before: None,
        after: content.to_owned(),
        snapshot,
    })
}

/// Plan changing `path`, whose current bytes must hash to `expected`.
pub(super) fn plan_patch(
    runtime: &AuthoringRuntime,
    path: &str,
    expected: &str,
    change: &TextChange,
    envelope: &Envelope,
) -> Result<TextPlan, OutcomeError> {
    let (snapshot, current) = runtime.coherent_read(&[path.to_owned()])?;
    envelope.check_revision(&snapshot)?;
    let current = current.into_iter().next().flatten();
    let bytes = match current {
        Some(bytes) if sha256_hex(&bytes) == expected => bytes,
        other => return Err(precondition_mismatch(path, expected, other.as_deref())),
    };
    let before = utf8_text(&bytes, path)?;
    let after = change.apply(&before)?;
    check_text_size(&after, path)?;
    Ok(TextPlan {
        path: path.to_owned(),
        before: Some(before),
        after,
        snapshot,
    })
}

/// Publish a planned text change, or describe it for a dry run.
pub(super) async fn publish_text(
    runtime: &AuthoringRuntime,
    tool: &'static str,
    plan: TextPlan,
    mut result: Map<String, Value>,
    envelope: &Envelope,
    canonical: &Value,
    context: &CallContext<'_>,
) -> Result<Value, OutcomeError> {
    let before_sha256 = plan
        .before
        .as_deref()
        .map(|text| sha256_hex(text.as_bytes()));
    let after_sha256 = sha256_hex(plan.after.as_bytes());
    let unchanged = before_sha256.as_deref() == Some(after_sha256.as_str());
    result.insert("path".to_owned(), Value::String(plan.path.clone()));
    result.insert(
        "operation".to_owned(),
        Value::String(
            if plan.before.is_some() {
                "update"
            } else {
                "create"
            }
            .to_owned(),
        ),
    );
    result.insert(
        "before_sha256".to_owned(),
        before_sha256.clone().map_or(Value::Null, Value::String),
    );
    result.insert("after_sha256".to_owned(), Value::String(after_sha256));
    result.insert("bytes".to_owned(), json!(plan.after.len()));
    result.insert("changed".to_owned(), Value::Bool(!unchanged));
    result.insert("diff".to_owned(), plan.diff().to_value());
    if envelope.dry_run || unchanged {
        result.insert("dry_run".to_owned(), Value::Bool(envelope.dry_run));
        result.insert("workspace".to_owned(), plan.snapshot.to_value());
        return Ok(Value::Object(result));
    }
    let (operation, precondition) = match before_sha256 {
        Some(sha256) => (FileOperation::Update, WritePrecondition::Sha256(sha256)),
        None => (FileOperation::Create, WritePrecondition::Absent),
    };
    let committed = runtime
        .commit(
            Mutation {
                tool,
                primary_path: plan.path.clone(),
                writes: vec![PlannedWrite {
                    path: plan.path,
                    operation,
                    bytes: Some(plan.after.into_bytes()),
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

/// Refuse a binary asset in a text tool, naming the tool that owns it.
fn text_target(path: &str) -> Result<WriteOwner, OutcomeError> {
    match paths::writable(path)? {
        WriteOwner::Asset => Err(outcome_error(
            ErrorKind::InvalidInput,
            format!("{path} is a binary asset; it is written through asset.put"),
        )),
        owner => Ok(owner),
    }
}

pub(super) async fn create(
    runtime: &AuthoringRuntime,
    arguments: &Map<String, Value>,
    envelope: &Envelope,
    canonical: &Value,
    context: &CallContext<'_>,
) -> Result<Value, OutcomeError> {
    let path = paths::normalize(required_str(arguments, "path")?, "path")?;
    text_target(&path)?;
    let content = required_str(arguments, "content")?;
    let plan = plan_create(runtime, &path, content, envelope)?;
    publish_text(
        runtime,
        AuthoringOperation::FileCreate.tool_name(),
        plan,
        Map::new(),
        envelope,
        canonical,
        context,
    )
    .await
}

pub(super) async fn patch(
    runtime: &AuthoringRuntime,
    arguments: &Map<String, Value>,
    envelope: &Envelope,
    canonical: &Value,
    context: &CallContext<'_>,
) -> Result<Value, OutcomeError> {
    let path = paths::normalize(required_str(arguments, "path")?, "path")?;
    text_target(&path)?;
    let expected = sha256_argument(arguments, "expectedSha256")?;
    let change = TextChange::from_arguments(arguments)?;
    let plan = plan_patch(runtime, &path, &expected, &change, envelope)?;
    publish_text(
        runtime,
        AuthoringOperation::FilePatch.tool_name(),
        plan,
        Map::new(),
        envelope,
        canonical,
        context,
    )
    .await
}

/// Refuse a move or delete source/target the generic tools may not touch.
fn relocatable(path: &str) -> Result<(), OutcomeError> {
    paths::readable(path)?;
    if path == "project.godot" {
        return Err(outcome_error(
            ErrorKind::InvalidInput,
            "project.godot is the project itself; it is not moved or deleted",
        ));
    }
    if matches!(paths::extension(path).as_str(), "import" | "uid") {
        return Err(outcome_error(
            ErrorKind::InvalidInput,
            format!("{path} is an engine-owned sidecar; it travels with its source file"),
        ));
    }
    Ok(())
}

pub(super) async fn relocate(
    runtime: &AuthoringRuntime,
    arguments: &Map<String, Value>,
    envelope: &Envelope,
    canonical: &Value,
    context: &CallContext<'_>,
) -> Result<Value, OutcomeError> {
    let from = paths::normalize(required_str(arguments, "from")?, "from")?;
    let to = paths::normalize(required_str(arguments, "to")?, "to")?;
    relocatable(&from)?;
    relocatable(&to)?;
    if from == to {
        return Err(outcome_error(
            ErrorKind::InvalidInput,
            "`from` and `to` name the same file",
        ));
    }
    let expected = sha256_argument(arguments, "expectedSha256")?;
    let from_sidecars = paths::sidecars(&from);
    let to_sidecars = paths::sidecars(&to);
    let mut read: Vec<String> = vec![from.clone(), to.clone()];
    read.extend(from_sidecars.iter().cloned());
    read.extend(to_sidecars.iter().cloned());
    let (snapshot, contents) = runtime.coherent_read(&read)?;
    envelope.check_revision(&snapshot)?;
    let source = match &contents[0] {
        Some(bytes) if sha256_hex(bytes) == expected => bytes.clone(),
        other => return Err(precondition_mismatch(&from, &expected, other.as_deref())),
    };
    if let Some(existing) = &contents[1] {
        return Err(already_exists(&to, existing));
    }
    let mut writes = vec![
        PlannedWrite {
            path: to.clone(),
            operation: FileOperation::Create,
            bytes: Some(source.clone()),
            precondition: WritePrecondition::Absent,
        },
        PlannedWrite {
            path: from.clone(),
            operation: FileOperation::Delete,
            bytes: None,
            precondition: WritePrecondition::Sha256(expected.clone()),
        },
    ];
    let mut moved_sidecars = Vec::new();
    for (index, (sidecar, destination)) in from_sidecars.iter().zip(&to_sidecars).enumerate() {
        let Some(bytes) = &contents[2 + index] else {
            continue;
        };
        if let Some(existing) = &contents[4 + index] {
            return Err(already_exists(destination, existing));
        }
        writes.push(PlannedWrite {
            path: destination.clone(),
            operation: FileOperation::Create,
            bytes: Some(bytes.clone()),
            precondition: WritePrecondition::Absent,
        });
        writes.push(PlannedWrite {
            path: sidecar.clone(),
            operation: FileOperation::Delete,
            bytes: None,
            precondition: WritePrecondition::Sha256(sha256_hex(bytes)),
        });
        moved_sidecars.push(json!({ "from": sidecar, "to": destination }));
    }
    let mut result = Map::new();
    result.insert("from".to_owned(), Value::String(from.clone()));
    result.insert("to".to_owned(), Value::String(to.clone()));
    result.insert("sha256".to_owned(), Value::String(expected));
    result.insert("moved_sidecars".to_owned(), Value::Array(moved_sidecars));
    result.insert("references_rewritten".to_owned(), Value::Bool(false));
    if envelope.dry_run {
        result.insert("dry_run".to_owned(), Value::Bool(true));
        result.insert("workspace".to_owned(), snapshot.to_value());
        return Ok(Value::Object(result));
    }
    let committed = runtime
        .commit(
            Mutation {
                tool: AuthoringOperation::FileMove.tool_name(),
                primary_path: to,
                writes,
                expected_revision: envelope.expected_revision,
            },
            canonical,
            context,
        )
        .await?;
    Ok(committed.into_value(result))
}

/// One planned delete: the snapshot it was planned against, the removed
/// bytes, and the journaled writes for the file and every sidecar present.
pub(super) struct DeletePlan {
    pub(super) snapshot: Arc<WorkspaceSnapshot>,
    pub(super) bytes: Vec<u8>,
    pub(super) writes: Vec<PlannedWrite>,
    pub(super) removed: Vec<String>,
}

/// Plan deleting `path` (hash `expected`) with every sidecar present, or
/// refuse. Shared with `asset.delete`.
pub(super) fn plan_delete(
    runtime: &AuthoringRuntime,
    path: &str,
    expected: &str,
    envelope: &Envelope,
) -> Result<DeletePlan, OutcomeError> {
    let sidecars = paths::sidecars(path);
    let mut read = vec![path.to_owned()];
    read.extend(sidecars.iter().cloned());
    let (snapshot, contents) = runtime.coherent_read(&read)?;
    envelope.check_revision(&snapshot)?;
    let bytes = match &contents[0] {
        Some(bytes) if sha256_hex(bytes) == expected => bytes.clone(),
        other => return Err(precondition_mismatch(path, expected, other.as_deref())),
    };
    let mut writes = vec![PlannedWrite {
        path: path.to_owned(),
        operation: FileOperation::Delete,
        bytes: None,
        precondition: WritePrecondition::Sha256(expected.to_owned()),
    }];
    let mut removed = Vec::new();
    for (sidecar, sidecar_bytes) in sidecars.iter().zip(&contents[1..]) {
        if let Some(sidecar_bytes) = sidecar_bytes {
            writes.push(PlannedWrite {
                path: sidecar.clone(),
                operation: FileOperation::Delete,
                bytes: None,
                precondition: WritePrecondition::Sha256(sha256_hex(sidecar_bytes)),
            });
            removed.push(sidecar.clone());
        }
    }
    Ok(DeletePlan {
        snapshot,
        bytes,
        writes,
        removed,
    })
}

pub(super) async fn delete(
    runtime: &AuthoringRuntime,
    arguments: &Map<String, Value>,
    envelope: &Envelope,
    canonical: &Value,
    context: &CallContext<'_>,
) -> Result<Value, OutcomeError> {
    let path = paths::normalize(required_str(arguments, "path")?, "path")?;
    relocatable(&path)?;
    let expected = sha256_argument(arguments, "expectedSha256")?;
    let DeletePlan {
        snapshot,
        bytes,
        writes,
        removed,
    } = plan_delete(runtime, &path, &expected, envelope)?;
    let mut result = Map::new();
    result.insert("path".to_owned(), Value::String(path.clone()));
    result.insert("operation".to_owned(), Value::String("delete".to_owned()));
    result.insert("before_sha256".to_owned(), Value::String(expected));
    result.insert(
        "removed_sidecars".to_owned(),
        Value::Array(removed.into_iter().map(Value::String).collect()),
    );
    result.insert(
        "diff".to_owned(),
        match std::str::from_utf8(&bytes) {
            Ok(text) => unified(Some(&path), None, text, "").to_value(),
            Err(_) => Value::Null,
        },
    );
    if envelope.dry_run {
        result.insert("dry_run".to_owned(), Value::Bool(true));
        result.insert("workspace".to_owned(), snapshot.to_value());
        return Ok(Value::Object(result));
    }
    let committed = runtime
        .commit(
            Mutation {
                tool: AuthoringOperation::FileDelete.tool_name(),
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
