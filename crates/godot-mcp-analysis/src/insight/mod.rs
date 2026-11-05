//! Project-wide static views built on the parsers and the semantic graph.
//!
//! Every other module of this crate answers a question about *one* document or
//! about the semantic graph as a whole. This one answers the questions an agent
//! asks about a whole project at once: which files nothing uses, which
//! references form a cycle, how signals are wired, how complex the code is,
//! where a string occurs, what a `uid://` names, which nodes match a query and
//! what a scene exports.
//!
//! ## Inputs
//!
//! The views read a [`ProjectSources`]: every inventoried project-relative path
//! with the bytes a caller read for it through the held project lease. This
//! module performs no I/O of its own, so the caller decides which files to read
//! and keeps sole path authority; a file the caller did not (or could not) read
//! is carried as [`FileContent::Unread`] and every view that needed it says so.
//!
//! A file Godot's editor file system ignores — a hidden one, or one under a
//! hidden directory or a directory holding a `.gdignore` ([`GodotFileView`]) —
//! is carried as excluded: it exists, so a reference to it resolves, but Godot
//! never scans it or registers its `class_name`, and no view lists it.
//!
//! [`ProjectModel`] parses those bytes once per request with the same GDScript,
//! scene and resource parsers the semantic graph is built from. Nothing here
//! re-implements a grammar or the `res://` classification rule.
//!
//! ## Honesty
//!
//! * A conclusion that depends on what the static layer cannot see — a load
//!   whose path is computed at run time, a damaged region, a binary container,
//!   a C# script, an engine class's members — is reported with that dependency
//!   named, and its confidence is lowered rather than rounded up.
//! * Absence is claimed only over what was read: a file the caller did not
//!   supply content for makes the view's coverage partial.
//! * Every list a view returns is complete over what was analysed and ordered
//!   deterministically, so a caller can page it.

pub mod glob;
pub mod metrics;
mod model;
pub mod presets;
pub mod references;
pub mod scenes;
pub mod script_facts;
pub mod search;
pub mod signals;
pub mod uid;

pub use model::{
    Coverage, CoverageGap, Exclusion, FileContent, FileKind, GodotFileView, IgnoredFiles,
    MAX_TEXT_FILE_BYTES, NodeOrigin, ProjectModel, ProjectSources, ResolvedNode, ScriptChain,
    ScriptModel, ScriptOrigin, SourceFile, Target, is_binary_by_extension,
};
