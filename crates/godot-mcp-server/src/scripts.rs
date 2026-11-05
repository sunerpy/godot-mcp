//! What every script tool shares (ADR 0012): the operator's switch, the
//! longest source a call takes, Godot's spelling of a returned value, and the
//! comparison that lists every project file a run added, changed or removed.

use std::collections::BTreeMap;
use std::path::Path;

use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::OutcomeError;
use godot_mcp_domain::safe_root::{SafeRoot, SafeRootError};
use godot_mcp_engine::supervision::journal::hex_sha256;
use serde_json::{Value, json};

fn outcome_error(kind: ErrorKind, message: impl Into<String>) -> OutcomeError {
    OutcomeError::new(kind, message.into(), false, BTreeMap::new())
}

/// The longest source a script tool takes, in characters, as a schema counts
/// them.
pub(crate) const MAX_SOURCE_CHARS: usize = 262_144;

/// The most files one comparison reads, and the most bytes. A project larger
/// than either is not compared, and the answer says so.
pub(crate) const MAX_COMPARED_FILES: usize = 100_000;
pub(crate) const MAX_COMPARED_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// The most changed files one answer lists.
pub(crate) const MAX_LISTED_CHANGES: usize = 1_000;

/// The refusal of a script tool on a server whose operator did not turn
/// script execution on.
pub(crate) fn scripts_off() -> OutcomeError {
    outcome_error(
        ErrorKind::PermissionDenied,
        "script execution is off: the operator turns it on for a launch with `godot-mcp serve \
         --allow-scripts`, or `allow_scripts = true` in the --config file. A script runs with \
         the user's privileges, and nothing confines what it does. Nothing was run.",
    )
}

/// The content of one compared file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Fingerprint {
    pub(crate) sha256: String,
    pub(crate) bytes: u64,
}

/// Every compared file of one tree, by project-relative path.
pub(crate) type Contents = BTreeMap<String, Fingerprint>;

/// The content of every inventoried file of `snapshot`, read through the held
/// root; `None` when the inventory is larger than a comparison reads.
pub(crate) fn contents_of_inventory(
    root: &SafeRoot,
    snapshot: &crate::workspace::WorkspaceSnapshot,
) -> Result<Option<Contents>, OutcomeError> {
    let inventory = snapshot.inventory();
    if inventory.len() > MAX_COMPARED_FILES {
        return Ok(None);
    }
    let mut contents = Contents::new();
    let mut total = 0_u64;
    for path in inventory.paths() {
        let bytes = match root.read(Path::new(path)) {
            Ok(bytes) => bytes,
            // Removed since the snapshot was taken: absent from this side.
            Err(SafeRootError::StalePreimage { .. }) => continue,
            Err(error) => {
                return Err(outcome_error(error.error_kind(), error.to_string()));
            }
        };
        total += bytes.len() as u64;
        if total > MAX_COMPARED_BYTES {
            return Ok(None);
        }
        contents.insert(
            path.to_owned(),
            Fingerprint {
                sha256: hex_sha256(&bytes),
                bytes: bytes.len() as u64,
            },
        );
    }
    Ok(Some(contents))
}

/// What a project run changed, or why it was not compared: `before` is the
/// tree before the run, `after` the tree read back after it.
pub(crate) fn project_changes(
    before: Option<&Contents>,
    after: Result<Option<Contents>, OutcomeError>,
) -> Result<Value, String> {
    match (before, after) {
        (Some(before), Ok(Some(after))) => Ok(changes_between(before, &after)),
        (None, _) | (_, Ok(None)) => Err(format!(
            "the project holds more than {MAX_COMPARED_FILES} files or {MAX_COMPARED_BYTES} \
             bytes, which are not compared"
        )),
        (_, Err(error)) => Err(format!(
            "the project could not be read back: {}",
            error.message()
        )),
    }
}

/// Every file `after` adds, changes or removes against `before`.
pub(crate) fn changes_between(before: &Contents, after: &Contents) -> Value {
    let file = |path: &str, fingerprint: &Fingerprint| json!({ "path": path, "sha256": fingerprint.sha256, "bytes": fingerprint.bytes });
    let mut added = Vec::new();
    let mut modified = Vec::new();
    let mut removed = Vec::new();
    for (path, now) in after {
        match before.get(path) {
            None => added.push(file(path, now)),
            Some(was) if was != now => modified.push(file(path, now)),
            Some(_) => {}
        }
    }
    for path in before.keys() {
        if !after.contains_key(path) {
            removed.push(json!({ "path": path }));
        }
    }
    let total = added.len() + modified.len() + removed.len();
    let mut budget = MAX_LISTED_CHANGES;
    let mut take = |list: Vec<Value>| -> Vec<Value> {
        let kept: Vec<Value> = list.into_iter().take(budget).collect();
        budget -= kept.len();
        kept
    };
    json!({
        "added": take(added),
        "modified": take(modified),
        "removed": take(removed),
        "total": total,
        "truncated": total > MAX_LISTED_CHANGES,
    })
}

/// The paths a change list names, in its order.
pub(crate) fn changed_paths(changes: &Value) -> Vec<String> {
    ["added", "modified", "removed"]
        .into_iter()
        .flat_map(|kind| {
            changes[kind]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|file| file["path"].as_str().map(str::to_owned))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The error a run that ended without an answer reports: `error`, with the
/// project files the run changed until then and the plain statement that
/// nothing is undone. `ended` says how the run ended.
pub(crate) fn with_changes(
    error: OutcomeError,
    before: Option<&Contents>,
    after: Result<Option<Contents>, OutcomeError>,
    ended: &str,
) -> OutcomeError {
    use godot_mcp_domain::contracts::GodotValue;

    let mut details = error.details().clone();
    let changed = match project_changes(before, after) {
        Ok(changes) => {
            let listed: Vec<GodotValue> = changed_paths(&changes)
                .into_iter()
                .map(GodotValue::String)
                .collect();
            let total = changes["total"].as_u64().unwrap_or_default();
            details.insert("changed_paths".to_owned(), GodotValue::Array(listed));
            details.insert(
                "changed_total".to_owned(),
                GodotValue::Integer(i64::try_from(total).unwrap_or(i64::MAX)),
            );
            format!("{total} project files differ from before the run (`changed_paths`)")
        }
        Err(reason) => format!("what it changed in the project is not known: {reason}"),
    };
    details.insert("applied".to_owned(), GodotValue::Bool(true));
    details.insert("rolled_back".to_owned(), GodotValue::Bool(false));
    OutcomeError::new(
        error.kind(),
        format!(
            "{} {ended}; nothing it did is undone: {changed}. Its other effects are not known.",
            error.message()
        ),
        error.retryable(),
        details,
    )
}

/// A returned value from Godot's spelling of it: the value, or why it has no
/// plain value.
pub(crate) fn returned_value(
    literal: Option<&str>,
    length: Option<u64>,
) -> (Value, Option<String>) {
    match (literal.map(godot_mcp_domain::variant_text::parse), length) {
        (Some(Ok(value)), _) => (serde_json::to_value(value).unwrap_or(Value::Null), None),
        (Some(Err(_)), _) => (
            Value::Null,
            Some(
                "the value holds an object or a resource, which has no plain value; \
                 `result_literal` is Godot's spelling of it"
                    .to_owned(),
            ),
        ),
        (None, Some(length)) => (
            Value::Null,
            Some(format!(
                "Godot's spelling of the value is {length} characters, more than an answer \
                 carries"
            )),
        ),
        (None, None) => (Value::Null, Some("no value was reported".to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fingerprint(sha: &str, bytes: u64) -> Fingerprint {
        Fingerprint {
            sha256: sha.to_owned(),
            bytes,
        }
    }

    #[test]
    fn changes_name_every_added_changed_and_removed_file() {
        let before = Contents::from([
            ("a.gd".to_owned(), fingerprint("1", 1)),
            ("b.tscn".to_owned(), fingerprint("2", 2)),
            ("c.txt".to_owned(), fingerprint("3", 3)),
        ]);
        let after = Contents::from([
            ("a.gd".to_owned(), fingerprint("1", 1)),
            ("b.tscn".to_owned(), fingerprint("9", 4)),
            ("d.png".to_owned(), fingerprint("4", 5)),
        ]);
        let changes = changes_between(&before, &after);
        assert_eq!(
            changes,
            json!({
                "added": [{ "path": "d.png", "sha256": "4", "bytes": 5 }],
                "modified": [{ "path": "b.tscn", "sha256": "9", "bytes": 4 }],
                "removed": [{ "path": "c.txt" }],
                "total": 3,
                "truncated": false,
            })
        );
        assert_eq!(changed_paths(&changes), ["d.png", "b.tscn", "c.txt"]);
        assert_eq!(changes_between(&before, &before)["total"], json!(0));
    }

    #[test]
    fn a_long_change_list_is_cut_at_the_listed_most() {
        let before = Contents::new();
        let after: Contents = (0..=MAX_LISTED_CHANGES)
            .map(|index| (format!("f{index:05}"), fingerprint("1", 1)))
            .collect();
        let changes = changes_between(&before, &after);
        assert_eq!(changes["total"], json!(MAX_LISTED_CHANGES + 1));
        assert_eq!(changes["truncated"], json!(true));
        assert_eq!(
            changes["added"].as_array().expect("added").len(),
            MAX_LISTED_CHANGES
        );
    }

    #[test]
    fn a_value_with_an_object_has_only_its_spelling() {
        assert_eq!(
            returned_value(Some("12"), Some(2)),
            (json!({ "type": "integer", "value": 12 }), None)
        );
        let (value, reason) = returned_value(
            Some("Object(Gradient,\"offsets\":PackedFloat32Array(0, 1))"),
            Some(46),
        );
        assert_eq!(value, Value::Null);
        assert!(reason.is_some_and(|reason| reason.contains("result_literal")));
        let (value, reason) = returned_value(None, Some(300_000));
        assert_eq!(value, Value::Null);
        assert!(reason.is_some_and(|reason| reason.contains("300000")));
    }
}
