//! The two configuration files the reference views read besides
//! `project.godot`: `export_presets.cfg` and an editor plugin's `plugin.cfg`.
//!
//! Both are Godot `ConfigFile` text. Only the keys the views need are read,
//! with a deliberately small reader: section headers, `key=value` lines,
//! quoted strings and `PackedStringArray(...)` lists. Anything else is ignored
//! rather than guessed at.

use serde::Serialize;

use super::glob::godot_wildcard_match;

/// One `[preset.N]` section of `export_presets.cfg`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExportPreset {
    /// The section name (`preset.0`).
    pub section: String,
    /// `name`.
    pub name: Option<String>,
    /// `platform`.
    pub platform: Option<String>,
    /// `export_filter`: `all_resources`, `resources`, `scenes`, `exclude` or
    /// `customized`.
    pub export_filter: Option<String>,
    /// `include_filter`, split on commas.
    pub include_filters: Vec<String>,
    /// `exclude_filter`, split on commas.
    pub exclude_filters: Vec<String>,
    /// `export_files`, project-relative.
    pub export_files: Vec<String>,
    /// 1-based line of the section header.
    pub line: u32,
}

impl ExportPreset {
    /// Whether the preset's `include_filter` selects `path` (project-relative),
    /// exactly as the editor evaluates it: case-insensitive `String::matchn`
    /// against both `res://<path>` and `<path>`.
    #[must_use]
    pub fn includes(&self, path: &str) -> Option<&str> {
        let prefixed = format!("res://{path}");
        self.include_filters
            .iter()
            .find(|filter| {
                godot_wildcard_match(filter, &prefixed, false)
                    || godot_wildcard_match(filter, path, false)
            })
            .map(String::as_str)
    }

    /// Whether `export_files` lists files the preset exports *instead of*
    /// everything (the `resources` and `scenes` modes), rather than files it
    /// leaves out (`exclude`).
    #[must_use]
    pub fn selects_listed_files(&self) -> bool {
        matches!(self.export_filter.as_deref(), Some("resources" | "scenes"))
    }
}

/// Parse `export_presets.cfg`.
#[must_use]
pub fn parse_export_presets(text: &str) -> Vec<ExportPreset> {
    let mut presets: Vec<ExportPreset> = Vec::new();
    let mut current: Option<ExportPreset> = None;
    for (number, key, value) in entries(text) {
        match key {
            Entry::Section(section) => {
                if let Some(preset) = current.take() {
                    presets.push(preset);
                }
                let is_preset = section.strip_prefix("preset.").is_some_and(|rest| {
                    !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit())
                });
                if is_preset {
                    current = Some(ExportPreset {
                        section: section.to_owned(),
                        name: None,
                        platform: None,
                        export_filter: None,
                        include_filters: Vec::new(),
                        exclude_filters: Vec::new(),
                        export_files: Vec::new(),
                        line: number,
                    });
                }
            }
            Entry::Value(key) => {
                let Some(preset) = current.as_mut() else {
                    continue;
                };
                match key {
                    "name" => preset.name = string_value(value),
                    "platform" => preset.platform = string_value(value),
                    "export_filter" => preset.export_filter = string_value(value),
                    "include_filter" => preset.include_filters = filters(value),
                    "exclude_filter" => preset.exclude_filters = filters(value),
                    "export_files" => {
                        preset.export_files = quoted_strings(value)
                            .into_iter()
                            .map(|path| path.strip_prefix("res://").unwrap_or(&path).to_owned())
                            .collect();
                    }
                    _ => {}
                }
            }
        }
    }
    if let Some(preset) = current {
        presets.push(preset);
    }
    presets
}

/// The script a `plugin.cfg` names, resolved against the descriptor's
/// directory the way the editor resolves a relative path, as a `res://` URI.
#[must_use]
pub fn plugin_script(descriptor_path: &str, text: &str) -> Option<(String, u32)> {
    let mut in_plugin = false;
    for (number, key, value) in entries(text) {
        match key {
            Entry::Section(section) => in_plugin = section == "plugin",
            Entry::Value("script") if in_plugin => {
                let script = string_value(value)?;
                if script.starts_with("res://") || script.starts_with("uid://") {
                    return Some((script, number));
                }
                let directory = descriptor_path
                    .rsplit_once('/')
                    .map_or("", |(directory, _)| directory);
                let joined = if directory.is_empty() {
                    format!("res://{script}")
                } else {
                    format!("res://{directory}/{script}")
                };
                return Some((joined, number));
            }
            Entry::Value(_) => {}
        }
    }
    None
}

enum Entry<'t> {
    Section(&'t str),
    Value(&'t str),
}

/// `(line, entry, raw value)` for every section header and `key=value` line.
fn entries(text: &str) -> impl Iterator<Item = (u32, Entry<'_>, &str)> {
    text.lines().enumerate().filter_map(|(index, line)| {
        let number = u32::try_from(index + 1).unwrap_or(u32::MAX);
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with(';') || trimmed.starts_with('#') {
            return None;
        }
        if let Some(section) = trimmed
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            return Some((number, Entry::Section(section.trim()), ""));
        }
        let (key, value) = trimmed.split_once('=')?;
        Some((number, Entry::Value(key.trim()), value.trim()))
    })
}

fn string_value(raw: &str) -> Option<String> {
    quoted_strings(raw).into_iter().next()
}

fn filters(raw: &str) -> Vec<String> {
    string_value(raw)
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|filter| !filter.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Every double-quoted string in `raw`, with `\"` and `\\` unescaped.
fn quoted_strings(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = raw.chars();
    while let Some(current) = chars.next() {
        if current != '"' {
            continue;
        }
        let mut value = String::new();
        let mut closed = false;
        while let Some(inner) = chars.next() {
            match inner {
                '\\' => {
                    if let Some(escaped) = chars.next() {
                        value.push(escaped);
                    }
                }
                '"' => {
                    closed = true;
                    break;
                }
                other => value.push(other),
            }
        }
        if closed {
            out.push(value);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRESETS: &str = r#"[preset.0]

name="Linux"
platform="Linux"
runnable=true
export_filter="all_resources"
include_filter="*.json, data/*"
exclude_filter="tests/*"
export_path=""

[preset.0.options]

custom_template/debug=""

[preset.1]

name="Demo"
platform="Web"
export_filter="scenes"
export_files=PackedStringArray("res://main.tscn", "res://levels/one.tscn")
include_filter=""
"#;

    #[test]
    fn presets_keep_their_filters_and_selected_files() {
        let presets = parse_export_presets(PRESETS);
        assert_eq!(presets.len(), 2);
        assert_eq!(presets[0].name.as_deref(), Some("Linux"));
        assert_eq!(presets[0].include_filters, vec!["*.json", "data/*"]);
        assert_eq!(presets[0].exclude_filters, vec!["tests/*"]);
        assert_eq!(presets[0].line, 1);
        assert!(!presets[0].selects_listed_files());
        assert_eq!(
            presets[1].export_files,
            vec!["main.tscn", "levels/one.tscn"]
        );
        assert!(presets[1].selects_listed_files());
        assert_eq!(presets[0].includes("config/levels.JSON"), Some("*.json"));
        assert_eq!(presets[0].includes("data/deep/a.bin"), Some("data/*"));
        assert_eq!(presets[0].includes("scenes/a.tscn"), None);
    }

    #[test]
    fn plugin_scripts_resolve_against_the_descriptor_directory() {
        let descriptor = "[plugin]\n\nname=\"Tools\"\nscript=\"plugin.gd\"\n";
        assert_eq!(
            plugin_script("addons/tools/plugin.cfg", descriptor),
            Some(("res://addons/tools/plugin.gd".to_owned(), 4))
        );
        let absolute = "[plugin]\nscript=\"res://elsewhere/main.gd\"\n";
        assert_eq!(
            plugin_script("addons/x/plugin.cfg", absolute).map(|(script, _)| script),
            Some("res://elsewhere/main.gd".to_owned())
        );
        assert_eq!(
            plugin_script("addons/x/plugin.cfg", "[other]\nscript=\"a.gd\"\n"),
            None
        );
    }
}
