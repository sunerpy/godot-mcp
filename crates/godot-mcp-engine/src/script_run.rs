//! The `script.run` host: what a headless Godot is started with to run one
//! caller-written GDScript, and how the report it leaves is read back.
//!
//! The host ([`HOST_SCRIPT`]) is the engine's main loop. It compiles the
//! caller's source with `GDScript.reload()`, calls the source's `run()` once the
//! scene tree and the project's autoloads are up, and writes one JSON report to
//! a file the server named. Compile and runtime errors are the engine's own
//! diagnostics, printed on its streams as for any other run; the report carries
//! only the stage reached and the returned value, in Godot's `var_to_str`
//! spelling.
//!
//! Nothing here contains the code: it runs with the user's privileges, and a
//! value it returns is only data. The process boundary, the deadline and the
//! journal are the launch protocol's, as for every engine run.

use std::path::Path;

use serde::Deserialize;

/// The host script, written beside the caller's source for each run.
pub const HOST_SCRIPT: &str = include_str!("../scripts/script_run/host.gd");

/// The longest return-value spelling a report carries, in characters; the host
/// reports only the length of a longer one.
pub const MAX_LITERAL_CHARS: u64 = 262_144;

/// The largest report the server reads back, in bytes: a literal of
/// [`MAX_LITERAL_CHARS`] characters of up to four bytes each, JSON-escaped, and
/// the report's own fields.
pub const MAX_REPORT_BYTES: usize = 2 * 1024 * 1024;

/// The flags a run needs the build to advertise.
pub const REQUIRED_CAPABILITIES: &[&str] = &["--headless", "--path", "--script"];

/// The project-relative files of one run, under the server-owned helper area
/// of the tree the engine runs against, which no tool reads or lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunFiles {
    /// The host, the engine's main loop.
    pub host: String,
    /// The caller's source.
    pub source: String,
    /// The report the host writes.
    pub report: String,
}

impl RunFiles {
    /// The files of the run `run_id`, which names nothing but letters, digits
    /// and dashes.
    #[must_use]
    pub fn of(run_id: &str) -> Self {
        let directory = format!("{}/script_run", crate::canonical::helper::HELPER_ROOT);
        Self {
            host: format!("{directory}/{run_id}.host.gd"),
            source: format!("{directory}/{run_id}.source.gd"),
            report: format!("{directory}/{run_id}.report.json"),
        }
    }

    /// The engine arguments of the run: headless, against `project_root`, with
    /// the host as the main loop and the source and report files as its user
    /// arguments. Each is one array element; nothing is ever joined into a
    /// shell string.
    #[must_use]
    pub fn host_arguments(&self, project_root: &Path) -> Vec<String> {
        vec![
            "--headless".to_owned(),
            "--path".to_owned(),
            project_root.display().to_string(),
            "--script".to_owned(),
            format!("res://{}", self.host),
            "--".to_owned(),
            format!("res://{}", self.source),
            format!("res://{}", self.report),
        ]
    }
}

/// What the host reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostReport {
    /// The source did not compile, cannot be instantiated or defines no
    /// `run()`; `run()` was never called.
    NotRun { reason: String },
    /// `run()` returned. `literal` is its value spelled by `var_to_str`, or
    /// `None` when that spelling is `length` characters, more than
    /// [`MAX_LITERAL_CHARS`].
    Returned {
        literal: Option<String>,
        length: Option<u64>,
    },
}

/// Why a report could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReportError {
    #[error("the run's report is {0} bytes, more than the {MAX_REPORT_BYTES} a report may be")]
    TooLarge(usize),
    #[error("the run's report is not a report the host writes: {0}")]
    Malformed(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawReport {
    stage: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    literal: Option<String>,
    #[serde(default)]
    literal_length: Option<u64>,
}

/// Read the report the host wrote.
///
/// # Errors
/// [`ReportError`] when the bytes are larger than any report the host writes or
/// are not one; a run killed while writing leaves such a file.
pub fn read_report(bytes: &[u8]) -> Result<HostReport, ReportError> {
    if bytes.len() > MAX_REPORT_BYTES {
        return Err(ReportError::TooLarge(bytes.len()));
    }
    let raw: RawReport =
        serde_json::from_slice(bytes).map_err(|error| ReportError::Malformed(error.to_string()))?;
    match raw.stage.as_str() {
        "compile" => Ok(HostReport::NotRun {
            reason: raw.reason.ok_or_else(|| {
                ReportError::Malformed("a compile report names no reason".to_owned())
            })?,
        }),
        "done" => match (raw.literal, raw.literal_length) {
            (Some(literal), None) => Ok(HostReport::Returned {
                literal: Some(literal),
                length: None,
            }),
            (None, Some(length)) if length > MAX_LITERAL_CHARS => Ok(HostReport::Returned {
                literal: None,
                length: Some(length),
            }),
            _ => Err(ReportError::Malformed(
                "a done report carries either the literal or its length".to_owned(),
            )),
        },
        other => Err(ReportError::Malformed(format!("unknown stage `{other}`"))),
    }
}

/// The 1-based line of the caller's source an engine position names, when the
/// position is inside the compiled source: Godot spells a script compiled from
/// text, as the host compiles the source, `gdscript://<id>.gd`.
#[must_use]
pub fn source_line(engine_location: &str) -> Option<u32> {
    let rest = engine_location.strip_prefix("gdscript://")?;
    let (_, line) = rest.rsplit_once(".gd:")?;
    line.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_arguments_are_separate_elements_after_the_user_separator() {
        let arguments = RunFiles::of("run-1").host_arguments(Path::new("/tmp/a project"));
        assert_eq!(
            arguments,
            [
                "--headless",
                "--path",
                "/tmp/a project",
                "--script",
                "res://godot_mcp_helpers/script_run/run-1.host.gd",
                "--",
                "res://godot_mcp_helpers/script_run/run-1.source.gd",
                "res://godot_mcp_helpers/script_run/run-1.report.json",
            ]
        );
    }

    #[test]
    fn reports_read_as_the_host_writes_them() {
        assert_eq!(
            read_report(br#"{"stage":"done","literal":"{\n\"a\": 1\n}"}"#),
            Ok(HostReport::Returned {
                literal: Some("{\n\"a\": 1\n}".to_owned()),
                length: None
            })
        );
        assert_eq!(
            read_report(br#"{"stage":"done","literal":null,"literal_length":262145}"#),
            Ok(HostReport::Returned {
                literal: None,
                length: Some(262_145)
            })
        );
        assert_eq!(
            read_report(br#"{"stage":"compile","reason":"the source defines no run() method"}"#),
            Ok(HostReport::NotRun {
                reason: "the source defines no run() method".to_owned()
            })
        );
        for malformed in [
            &br#"{"stage":"done"}"#[..],
            br#"{"stage":"done","literal":"1","literal_length":3}"#,
            br#"{"stage":"done","literal":null,"literal_length":3}"#,
            br#"{"stage":"compile"}"#,
            br#"{"stage":"later","literal":"1"}"#,
            br#"{"stage":"done","literal":"1","extra":true}"#,
            br#"{"stage":"done","lit"#,
            b"",
        ] {
            assert!(
                matches!(read_report(malformed), Err(ReportError::Malformed(_))),
                "{}",
                String::from_utf8_lossy(malformed)
            );
        }
        assert_eq!(
            read_report(&vec![b' '; MAX_REPORT_BYTES + 1]),
            Err(ReportError::TooLarge(MAX_REPORT_BYTES + 1))
        );
    }

    #[test]
    fn a_compiled_source_position_names_its_line() {
        assert_eq!(source_line("gdscript://-9223372010111892021.gd:4"), Some(4));
        assert_eq!(source_line("res://main.gd:4"), None);
        assert_eq!(source_line("core/object/object.cpp:2536"), None);
        assert_eq!(source_line("gdscript://x.gd"), None);
    }

    #[test]
    fn the_host_writes_what_read_report_reads() {
        for field in [
            "\"stage\": \"compile\"",
            "\"stage\": \"done\"",
            "\"literal\": literal",
            "\"literal_length\"",
            "\"reason\"",
        ] {
            assert!(HOST_SCRIPT.contains(field), "{field}");
        }
        assert!(HOST_SCRIPT.contains(&format!("const MAX_LITERAL := {MAX_LITERAL_CHARS}")));
    }
}
