//! The project's `uid://` table, reconstructed from the files that record it.
//!
//! Godot keeps its authoritative UID map in `.godot/uid_cache.bin`, an engine
//! cache this layer never reads. The same facts are recorded in project files,
//! and those are what this table is built from:
//!
//! | Source | Where the UID is written |
//! | --- | --- |
//! | [`UidSource::DocumentHeader`] | the `uid="uid://…"` attribute of a `.tscn` / `.tres` header |
//! | [`UidSource::UidSidecar`] | a `<file>.uid` sidecar beside a script or shader (Godot 4.4 and later) |
//! | [`UidSource::ImportSidecar`] | the `[remap] uid="uid://…"` entry of an asset's `<file>.import` |
//!
//! A file that records no UID in any of these places may still have one in
//! the engine cache, so "no UID recorded" is reported as unknown, never as
//! "has no UID". `[ext_resource uid=… path=…]` declarations in other documents
//! are kept separately as [`UidClaim`]s: they say what a referrer believed,
//! not what the file itself records.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use super::model::ProjectSources;
use crate::scene::SceneDocument;

/// Longest `uid://` handle accepted, in bytes.
pub const MAX_UID_LENGTH: usize = 64;

/// Where a UID record was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UidSource {
    /// The header of a text scene or resource.
    DocumentHeader,
    /// A `.uid` sidecar.
    UidSidecar,
    /// The `[remap]` section of an `.import` sidecar.
    ImportSidecar,
}

impl UidSource {
    /// Stable wire code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::DocumentHeader => "document_header",
            Self::UidSidecar => "uid_sidecar",
            Self::ImportSidecar => "import_sidecar",
        }
    }
}

/// One file recording that it carries one UID.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct UidRecord {
    /// The `uid://` handle.
    pub uid: String,
    /// The file the handle names (for a sidecar, the file beside it).
    pub path: String,
    /// Where the handle was read.
    pub source: UidSource,
    /// The file the handle was read from.
    pub recorded_in: String,
    /// 1-based line it was read from.
    pub line: u32,
    /// Whether [`Self::path`] is an inventoried file. A sidecar whose file is
    /// gone is orphaned.
    pub exists: bool,
}

/// A referrer's `[ext_resource]` naming a file by both UID and path.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct UidClaim {
    /// The `uid://` handle the referrer wrote.
    pub uid: String,
    /// The `res://` path the referrer wrote, project-relative.
    pub path: String,
    /// The referring document.
    pub referrer: String,
    /// 1-based line of the declaration.
    pub line: u32,
}

/// How a lookup came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UidStatus {
    /// Exactly one existing file and one UID agree.
    Resolved,
    /// The records disagree: one UID for several files, or several UIDs for
    /// one file.
    Conflict,
    /// Every record names a file that is no longer in the project.
    Orphaned,
    /// Nothing in the project records it; the engine cache may still.
    Unknown,
}

/// The answer to one lookup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UidLookup {
    /// How the lookup came out.
    pub status: UidStatus,
    /// The file the UID names, or the UID the file carries, when resolved.
    pub value: Option<String>,
    /// Every record the lookup matched, in order.
    pub records: Vec<UidRecord>,
    /// Every referrer claim the lookup matched, in order.
    pub claims: Vec<UidClaim>,
}

/// Every UID the project's files record.
#[derive(Debug, Clone, Default)]
pub struct UidTable {
    records: Vec<UidRecord>,
    claims: Vec<UidClaim>,
}

impl UidTable {
    /// Collect the records from document headers, `.uid` sidecars and
    /// `.import` sidecars, and the claims from `[ext_resource]` declarations.
    pub(crate) fn collect<'d>(
        sources: &ProjectSources,
        documents: impl Iterator<Item = (&'d str, &'d SceneDocument)>,
    ) -> Self {
        let mut records = Vec::new();
        let mut claims = Vec::new();
        for (path, document) in documents {
            if let Some(uid) = document.self_uid() {
                records.push(UidRecord {
                    uid: uid.to_owned(),
                    path: path.to_owned(),
                    source: UidSource::DocumentHeader,
                    recorded_in: path.to_owned(),
                    line: header_line(sources.text(path).unwrap_or_default()),
                    exists: true,
                });
            }
            for declaration in document.ext_resources() {
                if let (Some(uid), Some(target)) = (
                    declaration.uid(),
                    declaration.target().project_relative_path(),
                ) {
                    claims.push(UidClaim {
                        uid: uid.to_owned(),
                        path: target.to_owned(),
                        referrer: path.to_owned(),
                        line: declaration.span().start_line(),
                    });
                }
            }
        }
        for (path, file) in sources.files() {
            let Some(text) = file.text() else {
                continue;
            };
            let found = if let Some(owner) = path.strip_suffix(".uid") {
                sidecar_uid(text).map(|found| (owner, found, UidSource::UidSidecar))
            } else if let Some(owner) = path.strip_suffix(".import") {
                import_uid(text).map(|found| (owner, found, UidSource::ImportSidecar))
            } else {
                None
            };
            if let Some((owner, (uid, line), source)) = found {
                records.push(UidRecord {
                    uid,
                    path: owner.to_owned(),
                    source,
                    recorded_in: path.to_owned(),
                    line,
                    exists: sources.contains(owner),
                });
            }
        }
        records.sort();
        records.dedup();
        claims.sort();
        claims.dedup();
        Self { records, claims }
    }

    /// Every record, ordered by UID, path and source.
    #[must_use]
    pub fn records(&self) -> &[UidRecord] {
        &self.records
    }

    /// Every referrer claim, ordered by UID and path.
    #[must_use]
    pub fn claims(&self) -> &[UidClaim] {
        &self.claims
    }

    /// The existing files recorded as carrying `uid`, distinct and in order.
    #[must_use]
    pub fn paths_of(&self, uid: &str) -> Vec<&str> {
        let paths: BTreeSet<&str> = self
            .records
            .iter()
            .filter(|record| record.uid == uid && record.exists)
            .map(|record| record.path.as_str())
            .collect();
        paths.into_iter().collect()
    }

    /// The UIDs recorded for `path`, distinct and in order.
    #[must_use]
    pub fn uids_of(&self, path: &str) -> Vec<&str> {
        let uids: BTreeSet<&str> = self
            .records
            .iter()
            .filter(|record| record.path == path)
            .map(|record| record.uid.as_str())
            .collect();
        uids.into_iter().collect()
    }

    /// Look a UID up.
    #[must_use]
    pub fn lookup_uid(&self, uid: &str) -> UidLookup {
        let records: Vec<UidRecord> = self
            .records
            .iter()
            .filter(|record| record.uid == uid)
            .cloned()
            .collect();
        let claims: Vec<UidClaim> = self
            .claims
            .iter()
            .filter(|claim| claim.uid == uid)
            .cloned()
            .collect();
        let paths = self.paths_of(uid);
        let (status, value) = match paths.as_slice() {
            [only] => (UidStatus::Resolved, Some((*only).to_owned())),
            [] if records.is_empty() => (UidStatus::Unknown, None),
            [] => (UidStatus::Orphaned, None),
            _ => (UidStatus::Conflict, None),
        };
        UidLookup {
            status,
            value,
            records,
            claims,
        }
    }

    /// Look a project-relative path up.
    #[must_use]
    pub fn lookup_path(&self, path: &str) -> UidLookup {
        let records: Vec<UidRecord> = self
            .records
            .iter()
            .filter(|record| record.path == path)
            .cloned()
            .collect();
        let claims: Vec<UidClaim> = self
            .claims
            .iter()
            .filter(|claim| claim.path == path)
            .cloned()
            .collect();
        let uids = self.uids_of(path);
        let exists = records.iter().any(|record| record.exists);
        let (status, value) = match uids.as_slice() {
            [] => (UidStatus::Unknown, None),
            // Another existing file recording the same UID makes it ambiguous.
            [only] if exists && self.paths_of(only).len() > 1 => (UidStatus::Conflict, None),
            [only] if exists => (UidStatus::Resolved, Some((*only).to_owned())),
            [_] => (UidStatus::Orphaned, None),
            _ => (UidStatus::Conflict, None),
        };
        UidLookup {
            status,
            value,
            records,
            claims,
        }
    }

    /// Every UID that more than one existing file records, and every file
    /// recording more than one UID.
    #[must_use]
    pub fn conflicts(&self) -> BTreeMap<String, Vec<String>> {
        let mut by_uid: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        let mut by_path: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for record in self.records.iter().filter(|record| record.exists) {
            by_uid.entry(&record.uid).or_default().insert(&record.path);
            by_path.entry(&record.path).or_default().insert(&record.uid);
        }
        let mut out = BTreeMap::new();
        for (uid, paths) in by_uid {
            if paths.len() > 1 {
                out.insert(
                    uid.to_owned(),
                    paths.into_iter().map(str::to_owned).collect(),
                );
            }
        }
        for (path, uids) in by_path {
            if uids.len() > 1 {
                out.insert(
                    path.to_owned(),
                    uids.into_iter().map(str::to_owned).collect(),
                );
            }
        }
        out
    }
}

/// Whether `value` is a well-formed `uid://` handle.
#[must_use]
pub fn is_uid(value: &str) -> bool {
    value.len() <= MAX_UID_LENGTH
        && value.strip_prefix("uid://").is_some_and(|body| {
            !body.is_empty()
                && body
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
}

/// The 1-based line of the first section header, where a document's `uid`
/// attribute is written.
fn header_line(text: &str) -> u32 {
    text.lines()
        .position(|line| line.trim_start().starts_with('['))
        .map_or(1, |index| u32::try_from(index + 1).unwrap_or(u32::MAX))
}

/// The UID a `.uid` sidecar records: its first non-empty line.
fn sidecar_uid(text: &str) -> Option<(String, u32)> {
    text.lines().enumerate().find_map(|(index, line)| {
        let value = line.trim();
        is_uid(value).then(|| {
            (
                value.to_owned(),
                u32::try_from(index + 1).unwrap_or(u32::MAX),
            )
        })
    })
}

/// The `uid` an `.import` sidecar records in its `[remap]` section.
fn import_uid(text: &str) -> Option<(String, u32)> {
    let mut in_remap = false;
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_remap = trimmed == "[remap]";
            continue;
        }
        if !in_remap {
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        if key.trim() == "uid" {
            let value = value.trim().trim_matches('"');
            return is_uid(value).then(|| {
                (
                    value.to_owned(),
                    u32::try_from(index + 1).unwrap_or(u32::MAX),
                )
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::parse_document;

    fn table(files: &[(&str, &str)]) -> UidTable {
        let mut sources = ProjectSources::new();
        for (path, text) in files {
            sources.insert_text(*path, text);
        }
        let documents: Vec<(String, SceneDocument)> = files
            .iter()
            .filter(|(path, _)| path.ends_with(".tscn") || path.ends_with(".tres"))
            .map(|(path, text)| {
                (
                    (*path).to_owned(),
                    parse_document(path, text, 1).expect("document"),
                )
            })
            .collect();
        UidTable::collect(
            &sources,
            documents
                .iter()
                .map(|(path, document)| (path.as_str(), document)),
        )
    }

    #[test]
    fn records_come_from_headers_and_both_sidecars() {
        let table = table(&[
            (
                "main.tscn",
                "[gd_scene format=3 uid=\"uid://bmain\"]\n\n[ext_resource type=\"Script\" uid=\"uid://bplayer\" path=\"res://player.gd\" id=\"1\"]\n\n[node name=\"Main\" type=\"Node\"]\n",
            ),
            ("player.gd", "extends Node\n"),
            ("player.gd.uid", "uid://bplayer\n"),
            ("icon.svg", "<svg/>"),
            (
                "icon.svg.import",
                "[remap]\n\nimporter=\"texture\"\nuid=\"uid://bicon\"\npath=\"res://.godot/imported/icon.ctex\"\n",
            ),
            ("gone.gd.uid", "uid://bgone\n"),
        ]);
        let lookup = table.lookup_uid("uid://bplayer");
        assert_eq!(lookup.status, UidStatus::Resolved);
        assert_eq!(lookup.value.as_deref(), Some("player.gd"));
        assert_eq!(lookup.records[0].source, UidSource::UidSidecar);
        assert_eq!(lookup.claims[0].referrer, "main.tscn");
        assert_eq!(
            table.lookup_path("icon.svg").value.as_deref(),
            Some("uid://bicon")
        );
        assert_eq!(table.lookup_path("icon.svg").records[0].line, 4);
        assert_eq!(
            table.lookup_path("main.tscn").records[0].source,
            UidSource::DocumentHeader
        );
        assert_eq!(table.lookup_uid("uid://bgone").status, UidStatus::Orphaned);
        assert_eq!(
            table.lookup_uid("uid://bnothing").status,
            UidStatus::Unknown
        );
        assert_eq!(table.lookup_path("nothing.gd").status, UidStatus::Unknown);
    }

    #[test]
    fn disagreeing_records_are_conflicts() {
        let table = table(&[
            ("a.gd", "extends Node\n"),
            ("a.gd.uid", "uid://bsame\n"),
            ("b.gd", "extends Node\n"),
            ("b.gd.uid", "uid://bsame\n"),
        ]);
        assert_eq!(table.lookup_uid("uid://bsame").status, UidStatus::Conflict);
        // Looked up from either file, the UID is not the file's alone.
        let from_a = table.lookup_path("a.gd");
        assert_eq!(from_a.status, UidStatus::Conflict);
        assert_eq!(from_a.value, None);
        assert_eq!(table.conflicts().len(), 1);
    }

    #[test]
    fn handles_are_validated() {
        assert!(is_uid("uid://b8x2k0qwe3n1"));
        assert!(!is_uid("uid://"));
        assert!(!is_uid("uid://Has-Upper"));
        assert!(!is_uid("res://a.gd"));
    }
}
