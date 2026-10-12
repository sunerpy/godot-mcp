//! The bundled, versioned canonical-write helper script.
//!
//! The helper's bytes are embedded at compile time from the committed file in
//! `crates/godot-mcp-engine/scripts/canonical/` and pinned by SHA-256 in
//! [`HELPER_SHA256`]. A unit test asserts the digest of the embedded bytes, so a
//! substituted, edited or truncated helper is a **test failure** rather than a
//! silent behaviour change. Nothing here is generated at runtime and nothing is
//! fetched from the network.
//!
//! # Why the engine writes the resource and not this crate
//!
//! `ResourceUID.create_id_for_path()` is seeded by the engine with the path *and*
//! the project name, and `ResourceUID.id_to_text()` owns the `uid://` spelling.
//! Reproducing either in Rust would be reimplementing UID derivation, which this
//! project forbids. The canonical `.tscn`/`.tres` serialisation likewise belongs
//! to `ResourceSaver.save()`: hand-written resource text is not canonical bytes.
//! So this module owns only *where the helper is installed* and *how it is
//! invoked* — never what it produces.
//!
//! # The installed layout
//!
//! Everything the helper touches lives under one versioned prefix, so two server
//! versions can coexist in one project without one overwriting the other's
//! helper:
//!
//! ```text
//! res://godot_mcp_helpers/2/canonical_write.gd     the pinned script
//! res://godot_mcp_helpers/2/io/<id>.request        the request document
//! res://godot_mcp_helpers/2/io/<id>.response       the helper's answer
//! res://godot_mcp_helpers/2/staging/<id>.<ext>     the staged artifact
//! ```
//!
//! The helper writes **only** inside `staging/` (plus the response document it
//! was told to write). It never touches the caller's target path: publishing the
//! staged artifact onto the target is done by [`crate::canonical::publish`],
//! under the Todo 2 lease, as a compare-and-swap against the recorded preimage.
//!
//! # Exit codes are not proof
//!
//! [`HelperExit`] spells out the helper's four statuses. `Ok` means "the staged
//! artifact was written" and nothing more. A real Godot build **exits 0 with
//! empty output when handed a flag it does not support** (Todo 13 observed this
//! directly; the official CLI reference states unknown arguments "have no effect
//! whatsoever"), so a zero exit cannot establish that the write happened. Proof
//! requires the conjunction this module's callers enforce: the transaction's own
//! pre/post-hash rules **and** a clean-terminal `process_launch` proof on the
//! linked launch transaction.

use std::path::Path;

use crate::supervision::journal::hex_sha256;

/// The installed helper version. Part of the `res://` path, so an upgrade never
/// overwrites an older server's helper.
pub const HELPER_VERSION: &str = "2";

/// The helper script's bytes, embedded from the committed file.
const HELPER_SOURCE: &str = include_str!("../../scripts/canonical/canonical_write.gd");

/// The pinned SHA-256 of [`HELPER_SOURCE`].
///
/// Asserted by [`tests::the_embedded_helper_matches_its_pinned_digest`]. Editing
/// the script without updating this constant fails that test.
pub const HELPER_SHA256: &str = "8a08359379979ef5d96b2aa3c89fd13597dae79dff2bbb5a98c9fadd8dc0052a";

/// Project-relative directory every helper artifact lives under.
pub const HELPER_ROOT: &str = "godot_mcp_helpers";

/// The four statuses the bundled helper can exit with.
///
/// The numeric values are the helper's own constants, so a change on either side
/// is caught by [`tests::the_helper_declares_exactly_the_exit_codes_this_module_maps`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HelperExit {
    /// The staged artifact was written. **Not** proof the operation completed.
    Ok,
    /// The helper was invoked wrongly (missing/unreadable request document).
    Usage,
    /// The helper refused the request (binary resource, artifact outside
    /// staging, conflicting UID, missing target).
    Refused,
    /// The helper tried and failed (save error, unloadable target).
    Failed,
}

impl HelperExit {
    /// Every status. There are four.
    pub const ALL: [Self; 4] = [Self::Ok, Self::Usage, Self::Refused, Self::Failed];

    /// The helper's own exit status for this outcome.
    #[must_use]
    pub const fn code(self) -> i32 {
        match self {
            Self::Ok => 0,
            Self::Usage => 64,
            Self::Refused => 65,
            Self::Failed => 70,
        }
    }

    /// Stable wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Usage => "usage",
            Self::Refused => "refused",
            Self::Failed => "failed",
        }
    }

    /// Classify an observed exit status. An unknown status is never optimistically
    /// read as success.
    #[must_use]
    pub const fn classify(code: Option<i32>) -> Option<Self> {
        match code {
            Some(0) => Some(Self::Ok),
            Some(64) => Some(Self::Usage),
            Some(65) => Some(Self::Refused),
            Some(70) => Some(Self::Failed),
            _ => None,
        }
    }
}

/// The pinned helper script, ready to install.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HelperScript;

impl HelperScript {
    /// The exact bytes that get installed.
    #[must_use]
    pub const fn source() -> &'static str {
        HELPER_SOURCE
    }

    /// The pinned digest of [`HelperScript::source`].
    #[must_use]
    pub const fn pinned_sha256() -> &'static str {
        HELPER_SHA256
    }

    /// The digest of the embedded bytes as observed right now.
    #[must_use]
    pub fn observed_sha256() -> String {
        hex_sha256(HELPER_SOURCE.as_bytes())
    }

    /// Whether the embedded bytes still match the pinned digest.
    #[must_use]
    pub fn digest_matches() -> bool {
        Self::observed_sha256() == HELPER_SHA256
    }

    /// The project-relative install path, e.g. `godot_mcp_helpers/2/canonical_write.gd`.
    #[must_use]
    pub fn install_path() -> String {
        format!("{HELPER_ROOT}/{HELPER_VERSION}/canonical_write.gd")
    }

    /// The `res://` spelling of the install path, as `--script` takes it.
    #[must_use]
    pub fn script_argument() -> String {
        format!("res://{}", Self::install_path())
    }

    /// Project-relative path of the request document for `staging_id`.
    #[must_use]
    pub fn request_path(staging_id: &str) -> String {
        format!("{HELPER_ROOT}/{HELPER_VERSION}/io/{staging_id}.request")
    }

    /// Project-relative path of the response document for `staging_id`.
    #[must_use]
    pub fn response_path(staging_id: &str) -> String {
        format!("{HELPER_ROOT}/{HELPER_VERSION}/io/{staging_id}.response")
    }

    /// Project-relative path of the staged artifact for `staging_id`.
    #[must_use]
    pub fn artifact_path(staging_id: &str, extension: &str) -> String {
        format!("{HELPER_ROOT}/{HELPER_VERSION}/staging/{staging_id}.{extension}")
    }

    /// Whether `path` is one of the helper's own artifact locations. A caller can
    /// never nominate one of these as a *target*, because publishing into the
    /// helper's staging area would make the staged artifact its own preimage.
    #[must_use]
    pub fn is_helper_owned(path: &str) -> bool {
        let normalized = path.replace('\\', "/");
        normalized == HELPER_ROOT || normalized.starts_with(&format!("{HELPER_ROOT}/"))
    }
}

/// The flags a canonical write requires the configured build to advertise.
///
/// `--script` runs the helper, `--headless` selects the dummy display/audio
/// drivers and `--path` points the engine at the held project root. A build that
/// does not advertise one of these silently ignores it, which would look exactly
/// like a clean success — so the capability is checked before any launch.
pub const REQUIRED_CAPABILITIES: [&str; 3] = ["--headless", "--path", "--script"];

/// The exact argument array for one helper invocation, program path excluded.
///
/// Every element is separate: the project root, the `res://` script spelling and
/// both document paths travel as their own `Vec<String>` entries down through the
/// launch protocol into `execvp`, so no shell can re-interpret a path with a
/// space in it. Everything after the bare `--` reaches the script through
/// `OS.get_cmdline_user_args()`, which is the documented way to pass user
/// arguments past the engine's own flag parser.
#[must_use]
pub fn helper_arguments(project_root: &Path, request_rel: &str, response_rel: &str) -> Vec<String> {
    vec![
        "--headless".to_owned(),
        "--path".to_owned(),
        project_root.display().to_string(),
        "--script".to_owned(),
        HelperScript::script_argument(),
        "--".to_owned(),
        "--request".to_owned(),
        request_rel.to_owned(),
        "--response".to_owned(),
        response_rel.to_owned(),
    ]
}

/// One parsed helper response document.
///
/// The document is the helper's `key=value` form, one pair per line, keys sorted
/// by the helper itself so the bytes are stable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HelperResponse {
    /// `status` — `ok`, `refused` or `failed`.
    pub status: String,
    /// `reason`, when the helper refused or failed.
    pub reason: Option<String>,
    /// `artifact` — the `res://` path of the staged artifact.
    pub artifact: Option<String>,
    /// `target` — the `res://` target the artifact is destined for.
    pub target: Option<String>,
    /// `uid` — the engine-minted `uid://…`. Never derived in Rust.
    pub uid: Option<String>,
    /// `helper_version` — the version that answered.
    pub helper_version: Option<String>,
    /// Every other pair, retained verbatim for diagnostics.
    pub extra: std::collections::BTreeMap<String, String>,
}

impl HelperResponse {
    /// Parse the helper's response document.
    ///
    /// A line without `=` is ignored rather than guessed at; a document with no
    /// `status` parses to an empty status, which no caller treats as success.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut response = Self::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key {
                "status" => response.status = value.to_owned(),
                "reason" => response.reason = Some(value.to_owned()),
                "artifact" => response.artifact = Some(value.to_owned()),
                "target" => response.target = Some(value.to_owned()),
                "uid" => response.uid = Some(value.to_owned()),
                "helper_version" => response.helper_version = Some(value.to_owned()),
                other => {
                    response.extra.insert(other.to_owned(), value.to_owned());
                }
            }
        }
        response
    }

    /// Whether the helper reported `status=ok`. This is one input to a decision,
    /// never the decision itself.
    #[must_use]
    pub fn reports_ok(&self) -> bool {
        self.status == "ok"
    }
}

/// The request document for one helper invocation.
///
/// `key=value` lines, keys sorted, so the bytes are byte-stable for a given
/// request and can be hashed into the transaction's staged write set.
#[must_use]
pub fn request_document(pairs: &std::collections::BTreeMap<String, String>) -> String {
    let mut text = String::new();
    for (key, value) in pairs {
        text.push_str(key);
        text.push('=');
        text.push_str(value);
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_helper_matches_its_pinned_digest() {
        assert_eq!(
            HelperScript::observed_sha256(),
            HELPER_SHA256,
            "the bundled helper script's bytes changed without updating the pinned digest; a \
             substituted helper must be a test failure, never a silent behaviour change"
        );
        assert!(HelperScript::digest_matches());
        assert_eq!(HELPER_SHA256.len(), 64);
        assert!(
            HELPER_SHA256
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        );
    }

    #[test]
    fn the_helper_declares_exactly_the_exit_codes_this_module_maps() {
        // The helper's own constants, read out of the embedded source, so the two
        // sides can never drift apart silently.
        for (constant, expected) in [
            ("const EXIT_OK := 0", HelperExit::Ok),
            ("const EXIT_USAGE := 64", HelperExit::Usage),
            ("const EXIT_REFUSED := 65", HelperExit::Refused),
            ("const EXIT_FAILED := 70", HelperExit::Failed),
        ] {
            assert!(
                HELPER_SOURCE.contains(constant),
                "the embedded helper must declare `{constant}`"
            );
            assert_eq!(
                HelperExit::classify(Some(expected.code())),
                Some(expected),
                "{} maps to {}",
                expected.code(),
                expected.as_str()
            );
        }
        assert_eq!(HelperExit::ALL.len(), 4);
        assert_eq!(
            HelperExit::classify(Some(1)),
            None,
            "an unrecognised status is never read as one of the helper's outcomes"
        );
        assert_eq!(HelperExit::classify(None), None);
    }

    #[test]
    fn the_helper_only_ever_writes_inside_its_versioned_staging_prefix() {
        assert!(
            HELPER_SOURCE
                .contains("const STAGING_PREFIX := \"res://godot_mcp_helpers/2/staging/\""),
            "the embedded helper's staging prefix must be the versioned one this module installs"
        );
        assert!(
            HELPER_SOURCE.contains("artifact_outside_staging"),
            "the helper refuses an artifact outside its staging prefix"
        );
        assert!(
            HELPER_SOURCE.contains("const BINARY_EXTENSIONS := [\"scn\", \"res\"]"),
            "the helper refuses binary resources by extension"
        );
    }

    #[test]
    fn every_installed_path_lives_under_the_versioned_helper_prefix() {
        let paths = [
            HelperScript::install_path(),
            HelperScript::request_path("00ff11ee22dd33cc"),
            HelperScript::response_path("00ff11ee22dd33cc"),
            HelperScript::artifact_path("00ff11ee22dd33cc", "tscn"),
        ];
        for path in &paths {
            assert!(
                path.starts_with("godot_mcp_helpers/2/"),
                "{path} is versioned under the helper root"
            );
            assert!(HelperScript::is_helper_owned(path));
        }
        assert_eq!(
            HelperScript::script_argument(),
            "res://godot_mcp_helpers/2/canonical_write.gd"
        );
        assert!(!HelperScript::is_helper_owned("scenes/main.tscn"));
        assert!(!HelperScript::is_helper_owned("godot_mcp_helpers_other/x"));
    }

    #[test]
    fn the_argument_array_passes_user_arguments_after_a_bare_separator() {
        let argv = helper_arguments(
            Path::new("/tmp/a project"),
            "godot_mcp_helpers/2/io/abc.request",
            "godot_mcp_helpers/2/io/abc.response",
        );
        assert_eq!(
            argv,
            vec![
                "--headless".to_owned(),
                "--path".to_owned(),
                "/tmp/a project".to_owned(),
                "--script".to_owned(),
                "res://godot_mcp_helpers/2/canonical_write.gd".to_owned(),
                "--".to_owned(),
                "--request".to_owned(),
                "godot_mcp_helpers/2/io/abc.request".to_owned(),
                "--response".to_owned(),
                "godot_mcp_helpers/2/io/abc.response".to_owned(),
            ],
            "a path with a space stays one element, and user arguments follow a bare --"
        );
        assert!(
            HELPER_SOURCE.contains("OS.get_cmdline_user_args()"),
            "the helper reads its arguments the documented way"
        );
    }

    #[test]
    fn a_response_document_round_trips_through_its_stable_key_value_form() {
        let mut pairs = std::collections::BTreeMap::new();
        pairs.insert("operation".to_owned(), "create_resource".to_owned());
        pairs.insert(
            "artifact".to_owned(),
            "godot_mcp_helpers/2/staging/a.tscn".to_owned(),
        );
        let document = request_document(&pairs);
        assert_eq!(
            document, "artifact=godot_mcp_helpers/2/staging/a.tscn\noperation=create_resource\n",
            "keys are sorted, so identical requests produce identical bytes"
        );

        let parsed = HelperResponse::parse(
            "artifact=res://godot_mcp_helpers/2/staging/a.tscn\nhelper_version=2\nstatus=ok\n\
             target=res://scenes/main.tscn\nuid=uid://b1234567890ab\nunexpected=kept\n",
        );
        assert!(parsed.reports_ok());
        assert_eq!(parsed.uid.as_deref(), Some("uid://b1234567890ab"));
        assert_eq!(parsed.target.as_deref(), Some("res://scenes/main.tscn"));
        assert_eq!(
            parsed.extra.get("unexpected").map(String::as_str),
            Some("kept")
        );

        let empty = HelperResponse::parse("this line has no separator\n");
        assert!(
            !empty.reports_ok(),
            "an unparsable response is never read as success"
        );
    }

    #[test]
    fn the_required_capabilities_are_the_three_flags_a_helper_run_needs() {
        assert_eq!(
            REQUIRED_CAPABILITIES,
            ["--headless", "--path", "--script"],
            "a build that does not advertise one of these would ignore it silently and exit 0"
        );
    }
}
