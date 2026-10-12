//! The last line of defense against a local path in a public response.
//!
//! Public result types are built so that they cannot carry a storage locator,
//! and process proofs are projected without their boundary identity. A message
//! assembled from a lower-layer error can still quote a local spelling, though —
//! an I/O failure naming the file it could not open, for instance. Every tool
//! result, error and text resource therefore passes through [`PrivatePaths`]
//! before it leaves the kernel: the held project's display root is rewritten to
//! its `res://` spelling and the private state root to an opaque marker.

use std::collections::BTreeMap;
use std::path::Path;

use godot_mcp_domain::contracts::{GodotValue, OutcomeError};
use serde_json::Value;

/// Marker that replaces the private state root in any public text.
pub const PRIVATE_STATE_MARKER: &str = "<private-state>";

/// Marker that replaces a bootstrap server's workspace location.
pub const WORKSPACE_MARKER: &str = "<workspace>";

/// One rewrite: a private spelling, its public replacement, and whether the
/// spelling is a bare directory name that must end at a path boundary.
#[derive(Debug, Clone)]
struct Rule {
    private: String,
    public: String,
    bare: bool,
}

/// The absolute local spellings one kernel must never publish, longest first.
#[derive(Debug, Clone, Default)]
pub struct PrivatePaths {
    rules: Vec<Rule>,
}

impl PrivatePaths {
    /// Rules for one held project: the state root (which lives inside the
    /// project root) is matched before the project root itself.
    #[must_use]
    pub fn for_project(project_root: &Path, state_root: &Path) -> Self {
        Self::rewriting([
            (state_root, PRIVATE_STATE_MARKER.to_owned()),
            (project_root, "res://".to_owned()),
        ])
    }

    /// Rules for a bootstrap server: the held workspace's own location is
    /// replaced by an opaque marker, and so is everything beneath it.
    #[must_use]
    pub fn for_workspace(workspace_root: &Path) -> Self {
        Self::rewriting([(workspace_root, WORKSPACE_MARKER.to_owned())])
    }

    fn rewriting<'a>(pairs: impl IntoIterator<Item = (&'a Path, String)>) -> Self {
        let mut rules = Vec::new();
        for (path, replacement) in pairs {
            let spelled = path.display().to_string();
            if spelled.is_empty() {
                continue;
            }
            let separator = std::path::MAIN_SEPARATOR;
            let trailing = if replacement.ends_with('/') {
                replacement.clone()
            } else {
                format!("{replacement}/")
            };
            rules.push(Rule {
                private: format!("{spelled}{separator}"),
                public: trailing.clone(),
                bare: false,
            });
            if separator != '/' {
                rules.push(Rule {
                    private: format!("{spelled}/"),
                    public: trailing,
                    bare: false,
                });
            }
            rules.push(Rule {
                private: spelled,
                public: replacement,
                bare: true,
            });
        }
        rules.sort_by_key(|rule| std::cmp::Reverse(rule.private.len()));
        Self { rules }
    }

    /// Whether any rule exists.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Rewrite every private spelling inside `text`.
    #[must_use]
    pub fn redact_str(&self, text: &str) -> String {
        let mut redacted = text.to_owned();
        for rule in &self.rules {
            if !redacted.contains(rule.private.as_str()) {
                continue;
            }
            redacted = if rule.bare {
                replace_at_boundaries(&redacted, &rule.private, &rule.public)
            } else {
                redacted.replace(rule.private.as_str(), &rule.public)
            };
        }
        redacted
    }

    /// Rewrite every string inside a JSON value, keys included.
    pub fn redact_value(&self, value: &mut Value) {
        if self.rules.is_empty() {
            return;
        }
        match value {
            Value::String(text) => {
                let redacted = self.redact_str(text);
                if redacted != *text {
                    *text = redacted;
                }
            }
            Value::Array(items) => {
                for item in items {
                    self.redact_value(item);
                }
            }
            Value::Object(map) => {
                let entries = std::mem::take(map);
                for (key, mut item) in entries {
                    self.redact_value(&mut item);
                    map.insert(self.redact_str(&key), item);
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }

    /// Rewrite the message and every string detail of an error.
    #[must_use]
    pub fn redact_error(&self, error: OutcomeError) -> OutcomeError {
        if self.rules.is_empty() {
            return error;
        }
        let details: BTreeMap<String, GodotValue> = error
            .details()
            .iter()
            .map(|(key, value)| (self.redact_str(key), self.redact_godot(value)))
            .collect();
        OutcomeError::new(
            error.kind(),
            self.redact_str(error.message()),
            error.retryable(),
            details,
        )
    }

    fn redact_godot(&self, value: &GodotValue) -> GodotValue {
        match value {
            GodotValue::String(text) => GodotValue::String(self.redact_str(text)),
            GodotValue::StringName(text) => GodotValue::StringName(self.redact_str(text)),
            GodotValue::NodePath(text) => GodotValue::NodePath(self.redact_str(text)),
            GodotValue::Array(items) => {
                GodotValue::Array(items.iter().map(|item| self.redact_godot(item)).collect())
            }
            GodotValue::Dictionary(map) => GodotValue::Dictionary(
                map.iter()
                    .map(|(key, item)| (self.redact_str(key), self.redact_godot(item)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
}

/// Replace `private` only where the next character cannot continue a path
/// component, so `/srv/game` is rewritten inside `"/srv/game"` but not inside
/// `/srv/gamebook`.
fn replace_at_boundaries(text: &str, private: &str, public: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find(private) {
        let after = &rest[index + private.len()..];
        let continues = after
            .chars()
            .next()
            .is_some_and(|next| next.is_alphanumeric() || matches!(next, '_' | '-' | '.'));
        output.push_str(&rest[..index]);
        output.push_str(if continues { private } else { public });
        rest = after;
    }
    output.push_str(rest);
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use godot_mcp_domain::ErrorKind;
    use serde_json::json;

    fn paths() -> PrivatePaths {
        PrivatePaths::for_project(
            Path::new("/home/user/game"),
            Path::new("/home/user/game/.godot-mcp"),
        )
    }

    #[test]
    fn project_and_state_spellings_are_rewritten_everywhere() {
        let mut value = json!({
            "message": "cannot open /home/user/game/scripts/a.gd",
            "nested": ["/home/user/game/.godot-mcp/journal/x", "/home/user/game"],
            "/home/user/game/key": true
        });

        paths().redact_value(&mut value);

        let rendered = value.to_string();
        assert!(!rendered.contains("/home/user"), "{rendered}");
        assert_eq!(value["message"], json!("cannot open res://scripts/a.gd"));
        assert_eq!(value["nested"][0], json!("<private-state>/journal/x"));
        assert_eq!(value["nested"][1], json!("res://"));
        assert_eq!(value["res://key"], json!(true));
    }

    #[test]
    fn an_unrelated_sibling_spelling_is_left_alone() {
        assert_eq!(
            paths().redact_str("/home/user/gamebook/notes"),
            "/home/user/gamebook/notes"
        );
    }

    #[test]
    fn error_messages_and_details_are_redacted() {
        let mut details = BTreeMap::new();
        details.insert(
            "path".to_owned(),
            GodotValue::String("/home/user/game/.godot-mcp/launches".to_owned()),
        );
        let error = OutcomeError::new(
            ErrorKind::Internal,
            "failed at /home/user/game/scenes/main.tscn",
            false,
            details,
        );

        let redacted = paths().redact_error(error);

        assert_eq!(redacted.message(), "failed at res://scenes/main.tscn");
        assert_eq!(
            redacted.details().get("path"),
            Some(&GodotValue::String("<private-state>/launches".to_owned()))
        );
    }
}
