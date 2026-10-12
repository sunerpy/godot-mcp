//! Godot's `export_presets.cfg`, read without the engine.
//!
//! The file is a Godot `ConfigFile`. `[preset.N]` sections carry a preset's
//! identity and filters, `[preset.N.options]` its platform options, and every
//! value is a Variant literal: a quoted string that may span lines and carry
//! `\"` escapes, a boolean, a number, or a constructor or container literal
//! that may span lines too. This reader keeps the scalar facts an export needs
//! and keeps every other value as its raw text, so a later key is never misread
//! because an earlier value held a newline, a `=` or a `[`.

use std::collections::BTreeMap;

/// One value of a preset field or option.
#[derive(Debug, Clone, PartialEq)]
pub enum PresetValue {
    /// A quoted string, unescaped.
    String(String),
    /// `true` or `false`.
    Bool(bool),
    /// An integer.
    Int(i64),
    /// Anything else, as written.
    Raw(String),
}

impl PresetValue {
    /// Whether the value carries content: a non-empty string, or anything
    /// that is not a string at all.
    #[must_use]
    pub fn is_set(&self) -> bool {
        match self {
            Self::String(text) => !text.is_empty(),
            Self::Raw(text) => !text.trim().is_empty(),
            Self::Bool(_) | Self::Int(_) => true,
        }
    }
}

/// One export preset.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportPreset {
    /// The `N` of `[preset.N]`.
    pub index: u32,
    /// The fields of `[preset.N]`.
    pub fields: BTreeMap<String, PresetValue>,
    /// The fields of `[preset.N.options]`.
    pub options: BTreeMap<String, PresetValue>,
}

impl ExportPreset {
    fn field_str(&self, key: &str) -> Option<&str> {
        match self.fields.get(key) {
            Some(PresetValue::String(text)) => Some(text),
            _ => None,
        }
    }

    /// The preset's name.
    #[must_use]
    pub fn name(&self) -> &str {
        self.field_str("name").unwrap_or_default()
    }

    /// The platform the preset exports for, as Godot spells it
    /// (`Linux`, `Windows Desktop`, ...).
    #[must_use]
    pub fn platform(&self) -> &str {
        self.field_str("platform").unwrap_or_default()
    }

    /// Whether the preset is the platform's runnable one.
    #[must_use]
    pub fn runnable(&self) -> bool {
        matches!(self.fields.get("runnable"), Some(PresetValue::Bool(true)))
    }

    /// The export path the preset names, as written.
    #[must_use]
    pub fn export_path(&self) -> &str {
        self.field_str("export_path").unwrap_or_default()
    }

    /// One option, when it is a string.
    #[must_use]
    pub fn option_str(&self, key: &str) -> Option<&str> {
        match self.options.get(key) {
            Some(PresetValue::String(text)) => Some(text),
            _ => None,
        }
    }

    /// One option, when it is a boolean.
    #[must_use]
    pub fn option_bool(&self, key: &str) -> Option<bool> {
        match self.options.get(key) {
            Some(PresetValue::Bool(value)) => Some(*value),
            _ => None,
        }
    }

    /// A custom template the preset names for `debug` or release exports.
    #[must_use]
    pub fn custom_template(&self, debug: bool) -> Option<&str> {
        let key = if debug {
            "custom_template/debug"
        } else {
            "custom_template/release"
        };
        self.option_str(key).filter(|path| !path.is_empty())
    }

    /// Every field and option that carries a secret inline, by name.
    ///
    /// Godot keeps sensitive export options out of this file, in the
    /// project's private credentials store; a secret written here instead is
    /// readable by anything that can read the project.
    #[must_use]
    pub fn secret_bearing(&self) -> Vec<String> {
        let fields = self
            .fields
            .iter()
            .filter(|(key, value)| is_secret_key(key) && value.is_set())
            .map(|(key, _)| key.clone());
        let options = self
            .options
            .iter()
            .filter(|(key, value)| is_secret_key(key) && value.is_set())
            .map(|(key, _)| format!("options/{key}"));
        fields.chain(options).collect()
    }
}

/// Whether a field or option name denotes a secret: a password, a
/// passphrase, a token, a secret, or a key (`script_encryption_key`,
/// `notarization/api_key`, ...).
#[must_use]
pub fn is_secret_key(key: &str) -> bool {
    let last = key.rsplit('/').next().unwrap_or(key).to_ascii_lowercase();
    ["password", "passphrase", "secret", "token"]
        .iter()
        .any(|word| last.contains(word))
        || last == "key"
        || last.ends_with("_key")
}

/// Why `export_presets.cfg` could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PresetError {
    /// A line is neither a section, a `key=value` pair, a comment nor blank.
    #[error("export_presets.cfg line {line} is not a section or a key=value pair")]
    Malformed {
        /// The one-based line.
        line: usize,
    },
    /// A quoted string or a bracketed literal never ends.
    #[error("export_presets.cfg line {line} starts a value that never ends")]
    Unterminated {
        /// The one-based line the value starts on.
        line: usize,
    },
}

/// Read every preset of `export_presets.cfg`, in index order.
///
/// # Errors
/// Returns [`PresetError`] for a line that is not a section, a pair, a
/// comment or blank, and for a value that never ends.
pub fn parse_presets(text: &str) -> Result<Vec<ExportPreset>, PresetError> {
    let mut presets: BTreeMap<u32, ExportPreset> = BTreeMap::new();
    let mut section: Option<(u32, bool)> = None;
    let lines: Vec<&str> = text.lines().collect();
    let mut at = 0;
    while at < lines.len() {
        let line_number = at + 1;
        let line = lines[at].trim();
        at += 1;
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            section = preset_section(name);
            if let Some((index, _)) = section {
                presets.entry(index).or_insert_with(|| ExportPreset {
                    index,
                    fields: BTreeMap::new(),
                    options: BTreeMap::new(),
                });
            }
            continue;
        }
        let Some((key, first)) = line.split_once('=') else {
            return Err(PresetError::Malformed { line: line_number });
        };
        // A value continues onto the next lines until its quotes and brackets
        // close.
        let mut raw = first.to_owned();
        while !balanced(&raw) {
            let Some(next) = lines.get(at) else {
                return Err(PresetError::Unterminated { line: line_number });
            };
            raw.push('\n');
            raw.push_str(next);
            at += 1;
        }
        if let Some((index, options)) = section
            && let Some(preset) = presets.get_mut(&index)
        {
            let target = if options {
                &mut preset.options
            } else {
                &mut preset.fields
            };
            target.insert(key.trim().to_owned(), value_of(raw.trim()));
        }
    }
    Ok(presets.into_values().collect())
}

/// `preset.N` or `preset.N.options`, as `(N, options)`.
fn preset_section(name: &str) -> Option<(u32, bool)> {
    let rest = name.strip_prefix("preset.")?;
    let (index, options) = match rest.strip_suffix(".options") {
        Some(index) => (index, true),
        None => (rest, false),
    };
    index.parse().ok().map(|index| (index, options))
}

/// Whether every quote and bracket `raw` opens is closed.
fn balanced(raw: &str) -> bool {
    let mut in_string = false;
    let mut escaped = false;
    let mut depth = 0_i64;
    for character in raw.chars() {
        if in_string {
            match character {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match character {
            '"' => in_string = true,
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            _ => {}
        }
    }
    !in_string && depth <= 0
}

fn value_of(raw: &str) -> PresetValue {
    if let Some(inner) = raw
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .filter(|_| raw.len() >= 2)
    {
        return PresetValue::String(unescape(inner));
    }
    match raw {
        "true" => PresetValue::Bool(true),
        "false" => PresetValue::Bool(false),
        _ => raw
            .parse::<i64>()
            .map_or_else(|_| PresetValue::Raw(raw.to_owned()), PresetValue::Int),
    }
}

fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        match characters.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A preset file the way Godot 4 writes one: a multi-line run script with
    /// escaped quotes and a `=` inside it, and a constructor literal.
    const PRESETS: &str = r##"[preset.0]

name="Linux"
platform="Linux"
runnable=true
dedicated_server=false
custom_features=""
export_filter="all_resources"
export_path="build/game.x86_64"
patches=PackedStringArray()
encrypt_pck=false

[preset.0.options]

custom_template/debug=""
custom_template/release=""
binary_format/embed_pck=true
binary_format/architecture="x86_64"
ssh_remote_deploy/run_script="#!/usr/bin/env bash
export DISPLAY=:0
unzip -o -q \"{temp_dir}/{archive_name}\" -d \"{temp_dir}\"
\"{temp_dir}/{exe_name}\" {cmd_args}"
ssh_remote_deploy/host="user@host_ip"

[preset.1]

name="Windows"
platform="Windows Desktop"
runnable=false
export_path="build/game.exe"
script_encryption_key="0123456789abcdef"

[preset.1.options]

binary_format/architecture="x86_64"
codesign/password="hunter2"
codesign/enable=false
"##;

    #[test]
    fn presets_are_read_with_their_fields_and_options() {
        let presets = parse_presets(PRESETS).expect("parse");
        assert_eq!(presets.len(), 2);
        let linux = &presets[0];
        assert_eq!(
            (linux.index, linux.name(), linux.platform()),
            (0, "Linux", "Linux")
        );
        assert!(linux.runnable());
        assert_eq!(linux.export_path(), "build/game.x86_64");
        assert_eq!(linux.option_bool("binary_format/embed_pck"), Some(true));
        assert_eq!(
            linux.option_str("binary_format/architecture"),
            Some("x86_64")
        );
        // The multi-line run script did not swallow or split the next key.
        assert_eq!(
            linux.option_str("ssh_remote_deploy/host"),
            Some("user@host_ip")
        );
        assert!(
            linux
                .option_str("ssh_remote_deploy/run_script")
                .is_some_and(|script| script.contains("\"{temp_dir}/{exe_name}\""))
        );
        assert_eq!(
            linux.fields.get("patches"),
            Some(&PresetValue::Raw("PackedStringArray()".to_owned()))
        );
        assert_eq!(linux.custom_template(false), None);
        assert!(linux.secret_bearing().is_empty());
        let windows = &presets[1];
        assert_eq!(windows.platform(), "Windows Desktop");
        assert!(!windows.runnable());
    }

    #[test]
    fn a_secret_written_into_the_presets_is_named_never_shown() {
        let presets = parse_presets(PRESETS).expect("parse");
        assert_eq!(
            presets[1].secret_bearing(),
            ["script_encryption_key", "options/codesign/password"]
        );
        for key in [
            "keystore/release_password",
            "notarization/api_key",
            "script_encryption_key",
            "auth_token",
        ] {
            assert!(is_secret_key(key), "{key}");
        }
        for key in [
            "keystore/release",
            "notarization/api_key_id",
            "codesign/enable",
            "binary_format/architecture",
        ] {
            assert!(!is_secret_key(key), "{key}");
        }
        // An empty secret field is no secret.
        let empty =
            parse_presets("[preset.0]\nname=\"A\"\nscript_encryption_key=\"\"\n").expect("parse");
        assert!(empty[0].secret_bearing().is_empty());
    }

    #[test]
    fn a_malformed_or_unterminated_file_is_refused() {
        assert_eq!(
            parse_presets("[preset.0]\nname=\"A\nplatform=\"Linux\"\n"),
            Err(PresetError::Unterminated { line: 2 })
        );
        assert_eq!(
            parse_presets("[preset.0]\nthis is not a pair\n"),
            Err(PresetError::Malformed { line: 2 })
        );
        assert_eq!(parse_presets("; nothing here\n"), Ok(Vec::new()));
    }
}
