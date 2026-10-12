//! Text scene (`.tscn`) and text resource (`.tres`) parsing.
//!
//! ## Scope
//!
//! This module is a *pure text* layer plus one thin read entry point:
//! [`read_document`] asks Todo 8's [`ResolvedProject`] for the bytes (so path
//! authority, symlink refusal, engine-cache refusal and retarget detection stay
//! in exactly one place) and then hands them to [`parse_document`], which never
//! touches the filesystem, never spawns a process and never consults a Godot
//! install. Nothing here writes: parsing is strictly read-only.
//!
//! ## Attribution
//!
//! The parsed vocabulary — the `[ext_resource type/path/id]` identifier table,
//! `ExtResource("id")` / `SubResource("id")` handles, the
//! `[node name/type/parent]` tree with `script =` bindings, `groups = [...]`
//! membership and `instance=` sub-scene instantiation, the
//! `[connection signal/from/to/method]` wiring with `.` as the scene-root
//! marker, and the balanced-paren attribute scan that keeps
//! `instance=ExtResource("2")` whole — is adapted from the MIT-licensed
//! `codegraph-rust` project, files
//! `crates/codegraph-resolve/src/frameworks/godot_scene.rs` and
//! `godot_resource.rs`. Copyright (c) 2026 sunerpy, MIT. See
//! `THIRD-PARTY-NOTICES.md` at the workspace root. Only behavior is adapted;
//! there is no build-time or runtime dependency on that checkout.
//!
//! ## Honesty rules enforced here
//!
//! * Every malformed header, unknown section, unterminated value and unknown
//!   identifier becomes a *classified* [`SceneDiagnostic`] carrying its exact
//!   1-based span. Nothing is silently dropped and nothing is lumped into one
//!   generic bucket.
//! * A record whose span lies inside a damaged region is downgraded to
//!   `Heuristic`/`Inferred`; a document with any diagnostic may not claim
//!   `Exact`.
//! * An `ExtResource`/`SubResource` handle with no matching declaration keeps
//!   `target = None` and `Unresolved` provenance instead of a guessed target.
//! * `res://` classification reuses Todo 8's frozen rule verbatim, so a path
//!   escaping the project root is [`ResourceTarget::Unsupported`] and a
//!   `uid://` handle stays [`ResourceTarget::Uid`] until the (Todo 11) index
//!   exists.
//! * The binary `.scn` / `.res` formats are refused as `unsupported`, both by
//!   extension and by leading magic, so a renamed binary file is never parsed
//!   into a fabricated model.

use std::collections::HashMap;
use std::path::Path;

use godot_mcp_domain::contracts::{ContractViolation, Provenance, ProvenanceSource};
use godot_mcp_domain::{Confidence, ErrorKind};

use super::model::{
    Connection, DocumentKind, ExtResourceDecl, GroupMembership, Property, PropertyValue,
    ResourceBody, ResourceRef, ResourceRefKind, SceneDiagnostic, SceneDiagnosticKind,
    SceneDocument, SceneNode, SceneSpan, SubResourceDecl, UnknownSection,
};
use crate::project::{ProjectError, ResolvedProject, ResourceTarget, resource_target};

/// Extension of a Godot text scene.
pub const TEXT_SCENE_EXTENSION: &str = "tscn";
/// Extension of a Godot text resource.
pub const TEXT_RESOURCE_EXTENSION: &str = "tres";
/// Extension of a Godot binary scene. Never parsed.
pub const BINARY_SCENE_EXTENSION: &str = "scn";
/// Extension of a Godot binary resource. Never parsed.
pub const BINARY_RESOURCE_EXTENSION: &str = "res";

/// The scene-root marker Godot writes for "this scene's own root node".
const ROOT_MARKER: &str = ".";

/// A failure to read or model a text scene or resource.
#[derive(Debug, thiserror::Error)]
pub enum SceneError {
    /// A binary Godot format was named. This layer parses text formats only and
    /// will not guess at binary contents.
    #[error("the binary Godot {format} format is not supported: {path}")]
    UnsupportedBinaryFormat {
        /// The offending format, as an extension or a detected magic.
        format: String,
        /// The path that named it.
        path: String,
    },
    /// The path names neither a text scene nor a text resource.
    #[error("{path} is neither a .{TEXT_SCENE_EXTENSION} nor a .{TEXT_RESOURCE_EXTENSION} file")]
    UnsupportedExtension {
        /// The rejected path.
        path: String,
    },
    /// The bytes are not valid UTF-8 and will not be lossily reread.
    #[error("{path} is not valid UTF-8")]
    NotUtf8 {
        /// The rejected path.
        path: String,
        /// Byte offset of the first invalid sequence.
        offset: usize,
    },
    /// The read was refused, or failed, by the project layer.
    #[error(transparent)]
    Project(#[from] ProjectError),
    /// The parser could not produce an honest record.
    #[error("{path} could not be modelled: {message}")]
    Parse {
        /// The document being modelled.
        path: String,
        /// Stable outcome category of the violated invariant.
        kind: ErrorKind,
        /// Explanation of the violated invariant.
        message: String,
    },
}

impl SceneError {
    /// Stable outcome category for this failure.
    #[must_use]
    pub fn error_kind(&self) -> ErrorKind {
        match self {
            Self::UnsupportedBinaryFormat { .. } | Self::UnsupportedExtension { .. } => {
                ErrorKind::Unsupported
            }
            Self::NotUtf8 { .. } => ErrorKind::InvalidInput,
            Self::Project(error) => error.error_kind(),
            Self::Parse { kind, .. } => *kind,
        }
    }
}

/// Read and model one in-project text scene or resource.
///
/// The read itself is delegated to `project`, so the held lease re-verifies both
/// stable object identities before any byte is read and `..`, absolute paths,
/// in-root symlinks and the `.godot/` engine cache are refused there rather than
/// re-implemented here. Nothing is ever written.
///
/// # Errors
/// Returns [`SceneError::Project`] when the path is refused or the read fails
/// (`permission_denied` for an escaping, symlinked or engine-cache path),
/// [`SceneError::UnsupportedBinaryFormat`] for `.scn`/`.res`,
/// [`SceneError::UnsupportedExtension`] for any other extension, or
/// [`SceneError::NotUtf8`] when the bytes do not decode.
pub fn read_document(
    project: &ResolvedProject,
    relative: &Path,
    generation: u64,
) -> Result<SceneDocument, SceneError> {
    let display = relative.to_string_lossy().replace('\\', "/");
    // Path authority first: an out-of-root, symlinked or engine-cache path is
    // refused before this layer inspects the name for a supported format.
    let bytes = project.read_source_file(relative)?;
    // Then the format gate, so a binary document is refused rather than decoded.
    classify_extension(&display)?;
    let text = String::from_utf8(bytes).map_err(|error| SceneError::NotUtf8 {
        path: display.clone(),
        offset: error.utf8_error().valid_up_to(),
    })?;
    parse_document(&display, &text, generation)
}

/// Model one already-read text scene or resource.
///
/// `source_path` is used for attribution and to choose the grammar; it is never
/// resolved against the filesystem.
///
/// # Errors
/// Returns [`SceneError::UnsupportedBinaryFormat`] for a binary extension or a
/// binary magic, [`SceneError::UnsupportedExtension`] for an unrelated
/// extension, or [`SceneError::Parse`] when a provenance value the parser wants
/// to emit would be dishonest (a parser bug rather than a property of the
/// input).
pub fn parse_document(
    source_path: &str,
    text: &str,
    generation: u64,
) -> Result<SceneDocument, SceneError> {
    let kind = classify_extension(source_path)?;
    if let Some(format) = binary_magic(text) {
        return Err(SceneError::UnsupportedBinaryFormat {
            format,
            path: source_path.to_owned(),
        });
    }
    Parser::new(source_path, kind, generation)
        .run(text)
        .map_err(|violation| SceneError::Parse {
            path: source_path.to_owned(),
            kind: violation.kind(),
            message: violation.to_string(),
        })
}

/// Which grammar a path names, refusing binary and unrelated formats.
fn classify_extension(source_path: &str) -> Result<DocumentKind, SceneError> {
    let base = source_path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(source_path);
    let extension = base.rsplit_once('.').map(|(_, ext)| ext).unwrap_or("");
    match extension {
        TEXT_SCENE_EXTENSION => Ok(DocumentKind::Scene),
        TEXT_RESOURCE_EXTENSION => Ok(DocumentKind::Resource),
        BINARY_SCENE_EXTENSION | BINARY_RESOURCE_EXTENSION => {
            Err(SceneError::UnsupportedBinaryFormat {
                format: extension.to_owned(),
                path: source_path.to_owned(),
            })
        }
        _ => Err(SceneError::UnsupportedExtension {
            path: source_path.to_owned(),
        }),
    }
}

/// The binary-container magic a text document must never carry. Godot's binary
/// resource containers begin with `RSRC` (uncompressed) or `RSCC` (compressed),
/// and no text document contains a NUL byte.
fn binary_magic(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    if bytes.starts_with(b"RSRC") {
        return Some("resource container (RSRC magic)".to_owned());
    }
    if bytes.starts_with(b"RSCC") {
        return Some("compressed resource container (RSCC magic)".to_owned());
    }
    if bytes.contains(&0) {
        return Some("resource container (NUL byte in a text document)".to_owned());
    }
    None
}

// ---------------------------------------------------------------------------
// Lexing
// ---------------------------------------------------------------------------

/// One `key = value` line, with any brace/bracket/paren continuation folded in.
struct RawLine {
    key: String,
    raw_value: String,
    span: SceneSpan,
    unterminated: bool,
}

/// One well-formed `[name attr=value ...]` section plus the lines beneath it.
struct RawSection {
    name: String,
    attributes: Vec<(String, String)>,
    span: SceneSpan,
    lines: Vec<RawLine>,
}

fn clamp_line(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn is_comment(line: &str) -> bool {
    line.starts_with(';') || line.starts_with('#')
}

/// A whole-line span, 1-based, columns covering the trimmed text's line extent.
fn line_span(start_line: u32, end_line: u32, end_column: u32) -> SceneSpan {
    SceneSpan::new(start_line, 1, end_line, end_column)
}

/// `[name a="b" c=Expr("d")]` → `Some((name, attrs))`. Requires the trimmed line
/// to start `[` and end `]` with a non-empty name.
fn parse_section_header(line: &str) -> Option<(&str, Vec<(String, String)>)> {
    let inner = line.strip_prefix('[')?.strip_suffix(']')?;
    let inner = inner.trim();
    if inner.is_empty() {
        return None;
    }
    let mut parts = inner.splitn(2, char::is_whitespace);
    let name = parts.next()?.trim();
    if name.is_empty() {
        return None;
    }
    Some((name, parts.next().map(parse_attributes).unwrap_or_default()))
}

/// Parse a header's attribute tail into `(key, value)` pairs. Values may be
/// double-quoted (`name="Player"`) or bare expressions
/// (`instance=ExtResource("2")`, `binds= [3]`); a balanced-delimiter scan keeps a
/// bare value whole. Quoted values are stored without their surrounding quotes.
fn parse_attributes(tail: &str) -> Vec<(String, String)> {
    let bytes = tail.as_bytes();
    let len = bytes.len();
    let mut attributes = Vec::new();
    let mut index = 0usize;
    while index < len {
        while index < len && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= len {
            break;
        }
        let key_start = index;
        while index < len && bytes[index] != b'=' && !bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        let key = tail[key_start..index].trim();
        while index < len && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= len || bytes[index] != b'=' {
            // A bare token with no `=`: stay tolerant and skip it.
            continue;
        }
        index += 1;
        while index < len && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= len {
            break;
        }
        let value = if bytes[index] == b'"' {
            let start = index + 1;
            let mut end = start;
            while end < len && bytes[end] != b'"' {
                end += 1;
            }
            let captured = &tail[start..end.min(len)];
            index = if end < len { end + 1 } else { len };
            captured.to_owned()
        } else {
            let start = index;
            let mut depth = 0i32;
            while index < len {
                match bytes[index] {
                    b'(' | b'[' | b'{' => depth += 1,
                    b')' | b']' | b'}' => depth -= 1,
                    byte if byte.is_ascii_whitespace() && depth <= 0 => break,
                    _ => {}
                }
                index += 1;
            }
            tail[start..index].trim().to_owned()
        };
        if !key.is_empty() {
            attributes.push((key.to_owned(), value));
        }
    }
    attributes
}

/// Net delimiter depth of one line, string-aware so a bracket inside a quoted
/// value never opens a continuation.
fn delimiter_delta(line: &str) -> i32 {
    let mut delta = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for byte in line.bytes() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'(' | b'[' | b'{' => delta += 1,
            b')' | b']' | b'}' => delta -= 1,
            _ => {}
        }
    }
    delta
}

/// Split `key = value` at the FIRST `=`. `None` when there is no `=` or the key
/// is empty.
fn split_key_value(line: &str) -> Option<(&str, &str)> {
    let equals = line.find('=')?;
    let key = line[..equals].trim();
    if key.is_empty() {
        return None;
    }
    Some((key, line[equals + 1..].trim()))
}

/// Remove one matching pair of surrounding double quotes, if present.
fn strip_quotes(value: &str) -> &str {
    let trimmed = value.trim();
    trimmed
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(trimmed)
}

/// Every double-quoted string in `value`, in source order.
fn quoted_strings(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = value;
    while let Some(open) = rest.find('"') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('"') else {
            break;
        };
        out.push(after[..close].to_owned());
        rest = &after[close + 1..];
    }
    out
}

/// `Name("id")` → `Some("id")`, tolerating an unquoted legacy id.
fn call_argument<'a>(value: &'a str, function: &str) -> Option<&'a str> {
    let trimmed = value.trim();
    let inner = trimmed.strip_prefix(function)?.trim_start();
    let inner = inner.strip_prefix('(')?.strip_suffix(')')?;
    let inner = inner.trim();
    if inner.is_empty() {
        return None;
    }
    Some(strip_quotes(inner))
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

struct Parser<'a> {
    source_path: &'a str,
    kind: DocumentKind,
    generation: u64,
    diagnostics: Vec<SceneDiagnostic>,
}

impl<'a> Parser<'a> {
    fn new(source_path: &'a str, kind: DocumentKind, generation: u64) -> Self {
        Self {
            source_path,
            kind,
            generation,
            diagnostics: Vec::new(),
        }
    }

    fn diagnose(&mut self, kind: SceneDiagnosticKind, span: SceneSpan, message: impl Into<String>) {
        self.diagnostics.push(SceneDiagnostic {
            kind,
            span,
            message: message.into(),
        });
    }

    fn provenance(
        &self,
        source: ProvenanceSource,
        confidence: Confidence,
        subject: String,
    ) -> Result<Provenance, ContractViolation> {
        Provenance::new(source, confidence, subject, self.generation)
    }

    fn exact(&self, subject: String) -> Result<Provenance, ContractViolation> {
        self.provenance(
            ProvenanceSource::ProjectMetadata,
            Confidence::Exact,
            subject,
        )
    }

    fn resolved(&self, subject: String) -> Result<Provenance, ContractViolation> {
        self.provenance(ProvenanceSource::StaticIndex, Confidence::Exact, subject)
    }

    fn unresolved(&self, subject: String) -> Result<Provenance, ContractViolation> {
        self.provenance(
            ProvenanceSource::Unresolved,
            Confidence::Unresolved,
            subject,
        )
    }

    fn inferred(&self, subject: String) -> Result<Provenance, ContractViolation> {
        self.provenance(ProvenanceSource::Heuristic, Confidence::Inferred, subject)
    }

    /// Evidence for a value that names a project resource, reusing Todo 8's
    /// frozen classification: only a proven project-relative path may claim
    /// exactness.
    fn for_target(
        &self,
        target: &ResourceTarget,
        subject: String,
    ) -> Result<Provenance, ContractViolation> {
        match target {
            ResourceTarget::ProjectRelative { .. } => self.exact(subject),
            ResourceTarget::Uid(_) | ResourceTarget::Unsupported(_) => self.unresolved(subject),
        }
    }

    fn subject(&self, suffix: &str) -> String {
        format!("{}#{suffix}", self.source_path)
    }

    // -- lexing ------------------------------------------------------------

    /// Split the document into well-formed sections, reporting every malformed
    /// header, empty key, missing assignment and unterminated value.
    fn lex(&mut self, text: &str) -> Vec<RawSection> {
        let lines: Vec<&str> = text.lines().collect();
        let mut sections: Vec<RawSection> = Vec::new();
        let mut index = 0usize;

        while index < lines.len() {
            let raw = lines[index];
            let trimmed = raw.trim();
            let line_no = clamp_line(index + 1);
            let end_column = clamp_line(trimmed.chars().count() + 1);

            if trimmed.is_empty() || is_comment(trimmed) {
                index += 1;
                continue;
            }
            if trimmed.starts_with('[') {
                if let Some((name, attributes)) = parse_section_header(trimmed) {
                    sections.push(RawSection {
                        name: name.to_owned(),
                        attributes,
                        span: line_span(line_no, line_no, end_column),
                        lines: Vec::new(),
                    });
                } else {
                    self.diagnose(
                        SceneDiagnosticKind::MalformedSectionHeader,
                        line_span(line_no, line_no, end_column),
                        format!("`{trimmed}` is not a well-formed [section] header"),
                    );
                }
                index += 1;
                continue;
            }

            let Some((key, first_value)) = split_key_value(trimmed) else {
                let (kind, message) = if trimmed.contains('=') {
                    (
                        SceneDiagnosticKind::EmptyKey,
                        "assignment with an empty key".to_owned(),
                    )
                } else {
                    (
                        SceneDiagnosticKind::MissingAssignment,
                        "line is neither a comment, a [section] header nor a key = value \
                         assignment"
                            .to_owned(),
                    )
                };
                self.diagnose(kind, line_span(line_no, line_no, end_column), message);
                index += 1;
                continue;
            };

            // A delimiter-opened value may span several lines. Recovery stops at
            // the next well-formed header so an unterminated value can never
            // swallow the rest of the document.
            let mut value = first_value.to_owned();
            let mut depth = delimiter_delta(first_value);
            let mut consumed = 1usize;
            let mut unterminated = false;
            while depth > 0 {
                let Some(next) = lines.get(index + consumed) else {
                    unterminated = true;
                    break;
                };
                let next_trimmed = next.trim();
                if next_trimmed.starts_with('[') && parse_section_header(next_trimmed).is_some() {
                    unterminated = true;
                    break;
                }
                value.push('\n');
                value.push_str(next);
                depth += delimiter_delta(next);
                consumed += 1;
            }
            let last_line = clamp_line(index + consumed);
            let last_column = lines.get(index + consumed - 1).map_or(end_column, |line| {
                clamp_line(line.trim().chars().count() + 1)
            });
            let span = line_span(line_no, last_line, last_column);
            if unterminated {
                self.diagnose(
                    SceneDiagnosticKind::UnterminatedValue,
                    span,
                    format!("the value of `{key}` opens a delimiter that is never closed"),
                );
            }
            index += consumed;

            let line_record = RawLine {
                key: key.to_owned(),
                raw_value: value,
                span,
                unterminated,
            };
            if let Some(section) = sections.last_mut() {
                section.lines.push(line_record);
            } else {
                // A `key = value` before any section header belongs to no
                // section; report it rather than dropping it silently.
                self.diagnose(
                    SceneDiagnosticKind::MissingAssignment,
                    span,
                    format!("`{}` appears before any [section] header", line_record.key),
                );
            }
        }
        sections
    }

    // -- interpretation ----------------------------------------------------

    #[allow(clippy::too_many_lines)]
    fn run(mut self, text: &str) -> Result<SceneDocument, ContractViolation> {
        let sections = self.lex(text);

        let mut format_version = None;
        let mut self_uid = None;
        let mut root_type = None;
        let mut descriptor_attributes: Vec<(String, String)> = Vec::new();
        let mut ext_resources: Vec<ExtResourceDecl> = Vec::new();
        let mut sub_resources: Vec<SubResourceDecl> = Vec::new();
        let mut nodes: Vec<SceneNode> = Vec::new();
        let mut connections: Vec<Connection> = Vec::new();
        let mut resource_body: Option<ResourceBody> = None;
        let mut unknown_sections: Vec<UnknownSection> = Vec::new();

        // --- pass 1: the descriptor and the identifier tables -------------
        //
        // The tables are built before any use is interpreted, so a handle is
        // resolved by declaration rather than by source order. Duplicate
        // identifiers are first-write-wins, matching the engine, and the later
        // declaration is reported.
        let mut ext_ids: HashMap<String, ResourceTarget> = HashMap::new();
        let mut sub_ids: HashMap<String, ()> = HashMap::new();
        let mut descriptor_seen = false;
        for section in &sections {
            match section.name.as_str() {
                "gd_scene" | "gd_resource" if !descriptor_seen => {
                    descriptor_seen = true;
                    for (key, value) in &section.attributes {
                        match key.as_str() {
                            "format" => format_version = value.trim().parse::<u32>().ok(),
                            "uid" => self_uid = Some(value.clone()),
                            "type" if section.name == "gd_resource" => {
                                root_type = Some(value.clone());
                            }
                            _ => descriptor_attributes.push((key.clone(), value.clone())),
                        }
                    }
                }
                "ext_resource" => {
                    let id = attribute(&section.attributes, "id");
                    let path = attribute(&section.attributes, "path");
                    if id.is_none() {
                        self.diagnose(
                            SceneDiagnosticKind::MissingRequiredAttribute,
                            section.span,
                            "an [ext_resource] declaration has no `id`, so no handle can name it",
                        );
                    }
                    if path.is_none() {
                        self.diagnose(
                            SceneDiagnosticKind::MissingRequiredAttribute,
                            section.span,
                            "an [ext_resource] declaration has no `path`",
                        );
                    }
                    let target = resource_target(path.unwrap_or_default());
                    if let Some(id) = id {
                        if ext_ids.contains_key(id) {
                            self.diagnose(
                                SceneDiagnosticKind::DuplicateResourceId,
                                section.span,
                                format!(
                                    "external resource id `{id}` is already declared earlier; the \
                                     first declaration wins"
                                ),
                            );
                        } else {
                            ext_ids.insert(id.to_owned(), target.clone());
                        }
                    }
                    let provenance = self.for_target(
                        &target,
                        self.subject(&format!("ext_resource:{}", section.span.start_line())),
                    )?;
                    ext_resources.push(ExtResourceDecl {
                        id: id.map(str::to_owned),
                        type_name: attribute(&section.attributes, "type").map(str::to_owned),
                        uid: attribute(&section.attributes, "uid").map(str::to_owned),
                        target,
                        span: section.span,
                        provenance,
                    });
                }
                "sub_resource" => {
                    let id = attribute(&section.attributes, "id");
                    let type_name = attribute(&section.attributes, "type");
                    if id.is_none() {
                        self.diagnose(
                            SceneDiagnosticKind::MissingRequiredAttribute,
                            section.span,
                            "a [sub_resource] declaration has no `id`, so no handle can name it",
                        );
                    }
                    if type_name.is_none() {
                        self.diagnose(
                            SceneDiagnosticKind::MissingRequiredAttribute,
                            section.span,
                            "a [sub_resource] declaration has no `type`",
                        );
                    }
                    if let Some(id) = id {
                        if sub_ids.contains_key(id) {
                            self.diagnose(
                                SceneDiagnosticKind::DuplicateResourceId,
                                section.span,
                                format!(
                                    "internal resource id `{id}` is already declared earlier; the \
                                     first declaration wins"
                                ),
                            );
                        } else {
                            sub_ids.insert(id.to_owned(), ());
                        }
                    }
                }
                _ => {}
            }
        }

        // --- pass 2: node identity, so parents can be checked -------------
        let mut declared_paths: Vec<String> = Vec::new();
        let mut has_root = false;
        for section in &sections {
            if section.name != "node" {
                continue;
            }
            let name = attribute(&section.attributes, "name");
            let parent = attribute(&section.attributes, "parent");
            if parent.is_none() {
                has_root = true;
            }
            if let Some(path) = node_path(name, parent) {
                declared_paths.push(path);
            }
        }

        // --- pass 3: every record -----------------------------------------
        let mut seen_paths: Vec<String> = Vec::new();
        for section in &sections {
            match section.name.as_str() {
                "gd_scene" | "gd_resource" | "ext_resource" => {}
                "sub_resource" => {
                    let properties =
                        self.properties(&section.lines, &ext_ids, &sub_ids, "sub_resource")?;
                    let provenance = self.exact(
                        self.subject(&format!("sub_resource:{}", section.span.start_line())),
                    )?;
                    sub_resources.push(SubResourceDecl {
                        id: attribute(&section.attributes, "id").map(str::to_owned),
                        type_name: attribute(&section.attributes, "type").map(str::to_owned),
                        properties,
                        span: section.span,
                        provenance,
                    });
                }
                "node" => {
                    let node = self.node(section, &ext_ids, &sub_ids, &declared_paths)?;
                    if let Some(path) = node.node_path.clone() {
                        if seen_paths.contains(&path) {
                            self.diagnose(
                                SceneDiagnosticKind::DuplicateNodePath,
                                section.span,
                                format!(
                                    "another node already resolves to the in-scene path `{path}`; \
                                     the first declaration wins"
                                ),
                            );
                        } else {
                            seen_paths.push(path);
                        }
                    }
                    if node.parent_path.is_none() && nodes.iter().any(SceneNode::is_root) {
                        self.diagnose(
                            SceneDiagnosticKind::MultipleSceneRoots,
                            section.span,
                            "a scene must declare exactly one root node; the first root wins",
                        );
                    }
                    nodes.push(node);
                }
                "connection" => {
                    connections.push(self.connection(section, &declared_paths, has_root)?);
                }
                "resource" => {
                    let mut properties =
                        self.properties(&section.lines, &ext_ids, &sub_ids, "resource")?;
                    let script = take_script(&mut properties);
                    let provenance = self.exact(self.subject("resource"))?;
                    if resource_body.is_none() {
                        resource_body = Some(ResourceBody {
                            script,
                            properties,
                            span: section.span,
                            provenance,
                        });
                    }
                }
                other => {
                    self.diagnose(
                        SceneDiagnosticKind::UnknownSection,
                        section.span,
                        format!(
                            "`{other}` is not a section this workspace models; its text is \
                             retained verbatim"
                        ),
                    );
                    let properties =
                        self.properties(&section.lines, &ext_ids, &sub_ids, "unknown_section")?;
                    let provenance = self.exact(
                        self.subject(&format!("unknown_section:{}", section.span.start_line())),
                    )?;
                    unknown_sections.push(UnknownSection {
                        name: other.to_owned(),
                        attributes: section.attributes.clone(),
                        properties,
                        span: section.span,
                        provenance,
                    });
                }
            }
        }

        // A scene without a root is a defect the engine itself refuses to load.
        if self.kind == DocumentKind::Scene && !has_root {
            self.diagnose(
                SceneDiagnosticKind::MissingSceneRoot,
                SceneSpan::new(1, 1, 1, 1),
                "the scene declares no root node (no [node] without a `parent`)",
            );
        }

        // --- honesty pass: downgrade anything inside a damaged region ------
        //
        // A record whose lines overlap a reported defect may not claim
        // exactness: the surrounding text is not fully understood, so the
        // record is heuristic evidence. An already-`Unresolved` record is left
        // alone — it is the strictly more honest label.
        let damaged: Vec<u32> = self
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.span.start_line())
            .collect();
        for node in &mut nodes {
            if overlaps(&damaged, node.span.start_line(), node.block_end_line) {
                node.provenance =
                    self.inferred(self.subject(&format!("node:{}", node.span.start_line())))?;
            }
        }
        for record in &mut ext_resources {
            if record.provenance.source() != ProvenanceSource::Unresolved
                && overlaps(&damaged, record.span.start_line(), record.span.end_line())
            {
                record.provenance = self.inferred(
                    self.subject(&format!("ext_resource:{}", record.span.start_line())),
                )?;
            }
        }
        for record in &mut sub_resources {
            if overlaps(&damaged, record.span.start_line(), record.span.end_line()) {
                record.provenance = self.inferred(
                    self.subject(&format!("sub_resource:{}", record.span.start_line())),
                )?;
            }
        }
        for record in &mut connections {
            if record.provenance.source() != ProvenanceSource::Heuristic
                && overlaps(&damaged, record.span.start_line(), record.span.end_line())
            {
                record.provenance = self
                    .inferred(self.subject(&format!("connection:{}", record.span.start_line())))?;
            }
        }

        let provenance = if self.diagnostics.is_empty() {
            self.exact(self.subject("document"))?
        } else {
            self.inferred(self.subject("document"))?
        };

        Ok(SceneDocument {
            kind: self.kind,
            source_path: self.source_path.to_owned(),
            format_version,
            self_uid,
            root_type,
            descriptor_attributes,
            ext_resources,
            sub_resources,
            nodes,
            connections,
            resource_body,
            unknown_sections,
            diagnostics: self.diagnostics,
            provenance,
        })
    }

    /// Interpret one section's `key = value` lines.
    fn properties(
        &mut self,
        lines: &[RawLine],
        ext_ids: &HashMap<String, ResourceTarget>,
        sub_ids: &HashMap<String, ()>,
        owner: &str,
    ) -> Result<Vec<Property>, ContractViolation> {
        let mut out = Vec::new();
        for line in lines {
            let value = self.property_value(line, ext_ids, sub_ids)?;
            let subject = self.subject(&format!("{owner}:{}:{}", line.span.start_line(), line.key));
            let provenance = if line.unterminated {
                self.inferred(subject)?
            } else {
                self.exact(subject)?
            };
            out.push(Property {
                key: line.key.clone(),
                raw_value: line.raw_value.clone(),
                value,
                span: line.span,
                provenance,
            });
        }
        Ok(out)
    }

    /// Classify one value: a resource handle, a `NodePath`, or exactly-known text
    /// this workspace assigns no structural meaning to.
    fn property_value(
        &mut self,
        line: &RawLine,
        ext_ids: &HashMap<String, ResourceTarget>,
        sub_ids: &HashMap<String, ()>,
    ) -> Result<PropertyValue, ContractViolation> {
        if let Some(reference) = self.resource_ref(&line.raw_value, line.span, ext_ids, sub_ids)? {
            return Ok(PropertyValue::Resource(reference));
        }
        if let Some(path) = call_argument(&line.raw_value, "NodePath") {
            return Ok(PropertyValue::NodePath(path.to_owned()));
        }
        Ok(PropertyValue::RetainedUnknown)
    }

    /// Model an `ExtResource("id")` / `SubResource("id")` handle. An identifier
    /// with no declaration keeps `target = None` and `Unresolved` provenance and
    /// is reported, never mapped to a guess.
    fn resource_ref(
        &mut self,
        value: &str,
        span: SceneSpan,
        ext_ids: &HashMap<String, ResourceTarget>,
        sub_ids: &HashMap<String, ()>,
    ) -> Result<Option<ResourceRef>, ContractViolation> {
        let (kind, id) = if let Some(id) = call_argument(value, "ExtResource") {
            (ResourceRefKind::ExtResource, id)
        } else if let Some(id) = call_argument(value, "SubResource") {
            (ResourceRefKind::SubResource, id)
        } else {
            return Ok(None);
        };
        let subject = self.subject(&format!("{}:{}:{id}", kind.code(), span.start_line()));
        let (target, known) = match kind {
            ResourceRefKind::ExtResource => (ext_ids.get(id).cloned(), ext_ids.contains_key(id)),
            ResourceRefKind::SubResource => (None, sub_ids.contains_key(id)),
        };
        if !known {
            self.diagnose(
                SceneDiagnosticKind::UnknownResourceId,
                span,
                format!(
                    "`{}(\"{id}\")` names an identifier no declaration in this document defines",
                    match kind {
                        ResourceRefKind::ExtResource => "ExtResource",
                        ResourceRefKind::SubResource => "SubResource",
                    }
                ),
            );
        }
        let provenance = if known {
            match &target {
                Some(ResourceTarget::Uid(_) | ResourceTarget::Unsupported(_)) => {
                    self.unresolved(subject)?
                }
                Some(ResourceTarget::ProjectRelative { .. }) | None => self.resolved(subject)?,
            }
        } else {
            self.unresolved(subject)?
        };
        Ok(Some(ResourceRef {
            kind,
            id: id.to_owned(),
            target,
            span,
            provenance,
        }))
    }

    /// Model one `[node ...]` section: its place in the tree, its script, its
    /// groups, its instance relationship and every retained property.
    fn node(
        &mut self,
        section: &RawSection,
        ext_ids: &HashMap<String, ResourceTarget>,
        sub_ids: &HashMap<String, ()>,
        declared_paths: &[String],
    ) -> Result<SceneNode, ContractViolation> {
        let name = attribute(&section.attributes, "name");
        if name.is_none() {
            self.diagnose(
                SceneDiagnosticKind::MissingRequiredAttribute,
                section.span,
                "a [node] header has no `name`, so the node has no addressable path",
            );
        }
        let parent = attribute(&section.attributes, "parent");
        // `.` is the scene-root marker and is always structurally valid; a
        // missing root is reported once, at document level, instead of being
        // charged to every child.
        if let Some(parent_path) = parent
            && parent_path != ROOT_MARKER
            && !declared_paths.iter().any(|known| known == parent_path)
        {
            self.diagnose(
                SceneDiagnosticKind::MissingParentNode,
                section.span,
                format!("`parent=\"{parent_path}\"` names no node declared in this scene"),
            );
        }

        let mut extra_attributes = Vec::new();
        let mut instance = None;
        let mut instance_placeholder = None;
        let mut header_groups = Vec::new();
        for (key, value) in &section.attributes {
            match key.as_str() {
                "name" | "type" | "parent" | "owner" | "index" | "unique_id" | "node_paths" => {}
                // Godot 4 writes persistent groups as a header attribute.
                "groups" => {
                    for name in quoted_strings(value) {
                        let name = name.trim().to_owned();
                        if name.is_empty() {
                            continue;
                        }
                        let provenance = self.exact(
                            self.subject(&format!("group:{}:{name}", section.span.start_line())),
                        )?;
                        header_groups.push(GroupMembership {
                            name,
                            span: section.span,
                            provenance,
                        });
                    }
                }
                "instance" => {
                    instance = self.resource_ref(value, section.span, ext_ids, sub_ids)?;
                    if instance.is_none() {
                        extra_attributes.push((key.clone(), value.clone()));
                    }
                }
                "instance_placeholder" => {
                    instance_placeholder = Some(resource_target(value));
                }
                _ => extra_attributes.push((key.clone(), value.clone())),
            }
        }

        let mut properties = self.properties(&section.lines, ext_ids, sub_ids, "node")?;
        let script = take_script(&mut properties);
        let mut groups = header_groups;
        groups.extend(self.groups(&mut properties)?);
        let block_end_line = section
            .lines
            .last()
            .map_or(section.span.end_line(), |line| line.span.end_line());
        let provenance =
            self.exact(self.subject(&format!("node:{}", section.span.start_line())))?;

        Ok(SceneNode {
            name: name.map(str::to_owned),
            type_name: attribute(&section.attributes, "type").map(str::to_owned),
            parent_path: parent.map(str::to_owned),
            node_path: node_path(name, parent),
            owner_path: attribute(&section.attributes, "owner").map(str::to_owned),
            index: attribute(&section.attributes, "index").map(str::to_owned),
            unique_id: attribute(&section.attributes, "unique_id").map(str::to_owned),
            node_paths: attribute(&section.attributes, "node_paths").map(str::to_owned),
            instance,
            instance_placeholder,
            script,
            groups,
            properties,
            extra_attributes,
            span: section.span,
            block_end_line,
            provenance,
        })
    }

    /// Take the `groups = [...]` line out of `properties` and model each group.
    /// A damaged (unterminated) value yields no membership claim: the raw text
    /// stays queryable as a property and the defect is already reported.
    fn groups(
        &mut self,
        properties: &mut Vec<Property>,
    ) -> Result<Vec<GroupMembership>, ContractViolation> {
        let Some(position) = properties.iter().position(|p| p.key == "groups") else {
            return Ok(Vec::new());
        };
        if properties[position]
            .provenance
            .confidence()
            .eq(&Confidence::Inferred)
        {
            return Ok(Vec::new());
        }
        let property = properties.remove(position);
        let mut out = Vec::new();
        for name in quoted_strings(&property.raw_value) {
            let name = name.trim().to_owned();
            if name.is_empty() {
                continue;
            }
            let provenance =
                self.exact(self.subject(&format!("group:{}:{name}", property.span.start_line())))?;
            out.push(GroupMembership {
                name,
                span: property.span,
                provenance,
            });
        }
        Ok(out)
    }

    /// Model one `[connection ...]` section, resolving each endpoint against the
    /// declared node paths (`.` is the scene root).
    fn connection(
        &mut self,
        section: &RawSection,
        declared_paths: &[String],
        has_root: bool,
    ) -> Result<Connection, ContractViolation> {
        let signal = attribute(&section.attributes, "signal");
        let from = attribute(&section.attributes, "from");
        let to = attribute(&section.attributes, "to");
        let method = attribute(&section.attributes, "method");
        for (label, present) in [
            ("signal", signal.is_some()),
            ("from", from.is_some()),
            ("to", to.is_some()),
            ("method", method.is_some()),
        ] {
            if !present {
                self.diagnose(
                    SceneDiagnosticKind::MissingRequiredAttribute,
                    section.span,
                    format!("a [connection] header has no `{label}`"),
                );
            }
        }
        let resolve = |path: Option<&str>| match path {
            Some(ROOT_MARKER) => has_root,
            Some(path) => declared_paths.iter().any(|known| known == path),
            None => false,
        };
        let from_resolved = resolve(from);
        let to_resolved = resolve(to);
        for (label, path, resolved) in [("from", from, from_resolved), ("to", to, to_resolved)] {
            if let Some(path) = path
                && !resolved
            {
                self.diagnose(
                    SceneDiagnosticKind::DanglingConnectionEndpoint,
                    section.span,
                    format!("the connection's `{label}=\"{path}\"` names no node in this scene"),
                );
            }
        }
        let extra_attributes = section
            .attributes
            .iter()
            .filter(|(key, _)| !matches!(key.as_str(), "signal" | "from" | "to" | "method"))
            .cloned()
            .collect();
        let subject = self.subject(&format!("connection:{}", section.span.start_line()));
        let complete = signal.is_some() && from.is_some() && to.is_some() && method.is_some();
        let provenance = if complete && from_resolved && to_resolved {
            self.exact(subject)?
        } else {
            self.inferred(subject)?
        };
        Ok(Connection {
            signal: signal.map(str::to_owned),
            from_path: from.map(str::to_owned),
            to_path: to.map(str::to_owned),
            method: method.map(str::to_owned),
            from_resolved,
            to_resolved,
            extra_attributes,
            span: section.span,
            provenance,
        })
    }
}

/// Look up a header attribute by key (first match).
fn attribute<'a>(attributes: &'a [(String, String)], key: &str) -> Option<&'a str> {
    attributes
        .iter()
        .find(|(candidate, _)| candidate == key)
        .map(|(_, value)| value.as_str())
}

/// The absolute in-scene path of a node: `.` for the root, `Name` for a direct
/// child of the root, `Parent/Name` otherwise. `None` when the node has no name
/// and is therefore unaddressable.
fn node_path(name: Option<&str>, parent: Option<&str>) -> Option<String> {
    match parent {
        None => Some(ROOT_MARKER.to_owned()),
        Some(ROOT_MARKER) => name.map(str::to_owned),
        Some(parent) => name.map(|name| format!("{parent}/{name}")),
    }
}

/// Take the `script = ...` line out of `properties`, returning its handle when
/// the value was one.
fn take_script(properties: &mut Vec<Property>) -> Option<ResourceRef> {
    let position = properties.iter().position(|property| {
        property.key == "script" && matches!(property.value, PropertyValue::Resource(_))
    })?;
    let property = properties.remove(position);
    match property.value {
        PropertyValue::Resource(reference) => Some(reference),
        PropertyValue::NodePath(_) | PropertyValue::RetainedUnknown => None,
    }
}

/// Whether any damaged line falls inside the inclusive line range.
fn overlaps(damaged: &[u32], start_line: u32, end_line: u32) -> bool {
    damaged
        .iter()
        .any(|line| start_line <= *line && *line <= end_line)
}
