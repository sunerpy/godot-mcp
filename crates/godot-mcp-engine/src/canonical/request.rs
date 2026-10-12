//! What a canonical write asks for, and which targets are writable at all.
//!
//! Path classification happens **here**, before anything is journaled and before
//! any process exists, because three of this todo's refusals are decidable from
//! the request alone:
//!
//! | refusal | why it is decidable up front |
//! | --- | --- |
//! | `.scn` / `.res` | binary resources are never mutated and never rewritten as text |
//! | anything but `.tscn` / `.tres` | this module only asks the engine for canonical *text* resources |
//! | absolute / `..` / helper-owned paths | a target inside the helper's own versioned area would make the staged artifact its own preimage |
//!
//! The request's canonical bytes are the hash input recorded on the transaction.
//! They are assembled from a sorted map, so the same request always produces the
//! same `canonical_request_hash` and the same helper request document.
//!
//! Nothing in this module synthesises resource text and nothing derives a UID.
//! Both belong to the engine: `ResourceSaver.save()` owns canonical `.tscn`/
//! `.tres` bytes and `ResourceUID` owns the `uid://` value and its spelling.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::canonical::error::CanonicalError;
use crate::canonical::helper::HelperScript;
use crate::supervision::journal::hex_sha256;
use crate::validation::{is_valid_project_relative_path, project_relative_path};

/// The canonical text-resource kinds this module can ask the engine to produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TextResourceKind {
    /// A packed scene, saved by `ResourceSaver` as canonical TSCN text.
    Scene,
    /// A resource, saved by `ResourceSaver` as canonical TRES text.
    Resource,
}

impl TextResourceKind {
    /// The lowercase extension, without a dot.
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Scene => "tscn",
            Self::Resource => "tres",
        }
    }
}

/// Extensions that are binary resources. Never mutated; never rewritten as text.
pub const BINARY_EXTENSIONS: [&str; 2] = ["scn", "res"];

/// Classify a project-relative target path.
///
/// # Errors
/// Returns [`CanonicalError::BinaryResource`] for `.scn`/`.res`,
/// [`CanonicalError::NotATextResource`] for any other extension, and
/// [`CanonicalError::UnusablePath`] for an absolute path, a `..` component, a
/// backslash-or-empty component, or a path inside the helper's own area.
pub fn classify_target(path: &str) -> Result<TextResourceKind, CanonicalError> {
    if path.starts_with("res://") || !is_valid_project_relative_path(path) {
        return Err(CanonicalError::UnusablePath {
            path: path.to_owned(),
            reason: "the target must be a portable project-relative path without a res:// prefix"
                .to_owned(),
        });
    }
    let normalized = path.to_owned();
    if normalized.starts_with('/') || normalized.contains(':') {
        return Err(CanonicalError::UnusablePath {
            path: path.to_owned(),
            reason: "the target must be a project-relative path, not an absolute or scheme-qualified one"
                .to_owned(),
        });
    }
    if normalized
        .split('/')
        .any(|component| component == ".." || component.is_empty() || component == ".")
    {
        return Err(CanonicalError::UnusablePath {
            path: path.to_owned(),
            reason: "the target must not contain `.`, `..` or empty path components".to_owned(),
        });
    }
    if HelperScript::is_helper_owned(&normalized) {
        return Err(CanonicalError::UnusablePath {
            path: path.to_owned(),
            reason: "the bundled helper's own versioned area is never a write target".to_owned(),
        });
    }
    let extension = normalized
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    if BINARY_EXTENSIONS.contains(&extension.as_str()) {
        return Err(CanonicalError::BinaryResource { path: normalized });
    }
    match extension.as_str() {
        "tscn" => Ok(TextResourceKind::Scene),
        "tres" => Ok(TextResourceKind::Resource),
        _ => Err(CanonicalError::NotATextResource { path: normalized }),
    }
}

/// The four things a canonical-write transaction can ask the engine to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CanonicalOperation {
    /// Create a text scene or resource that does not exist yet.
    CreateResource,
    /// Re-save an existing text scene or resource through the engine.
    UpdateResource,
    /// Create/register a `ResourceUID` for an existing resource.
    RegisterUid,
    /// Query a `ResourceUID` or a path's UID. Read-only: never publishes.
    QueryUid,
    /// Apply a batch of typed operations to an existing text scene or
    /// resource, through the engine.
    ApplyBatch,
}

impl CanonicalOperation {
    /// Every operation. There are five.
    pub const ALL: [Self; 5] = [
        Self::CreateResource,
        Self::UpdateResource,
        Self::RegisterUid,
        Self::QueryUid,
        Self::ApplyBatch,
    ];

    /// The helper's own operation spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CreateResource => "create_resource",
            Self::UpdateResource => "update_resource",
            Self::RegisterUid => "register_uid",
            Self::QueryUid => "query_uid",
            Self::ApplyBatch => "apply_batch",
        }
    }

    /// Whether this operation publishes bytes onto a target path.
    #[must_use]
    pub const fn mutates(self) -> bool {
        !matches!(self, Self::QueryUid)
    }

    /// Whether the target must already exist for this operation to make sense.
    #[must_use]
    pub const fn requires_existing_target(self) -> bool {
        matches!(
            self,
            Self::UpdateResource | Self::RegisterUid | Self::ApplyBatch
        )
    }
}

/// One canonical-write request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalRequest {
    /// What to do.
    pub operation: CanonicalOperation,
    /// The project-relative target, for every operation but a UID query.
    pub target: String,
    /// `resource_name` to set on the saved resource, when the caller wants one.
    pub resource_name: Option<String>,
    /// Root node class for a created scene. The engine instantiates it; this crate
    /// never builds a node tree.
    pub root_type: Option<String>,
    /// Root node name for a created scene.
    pub root_name: Option<String>,
    /// A `uid://…` or project-relative path to resolve, for a UID query.
    pub query: Option<String>,
    /// The engine class of a created resource (`.tres`); `Resource` when absent.
    pub resource_class: Option<String>,
    /// The compact JSON array of typed operations the engine applies, for a
    /// create or a batch. Validated by the caller; the engine re-validates
    /// every operation against its own class database.
    pub batch: Option<String>,
    /// For a batch: the compact JSON object mapping each node handle the
    /// operations use to the path of its node in the target as it is now. The
    /// engine binds each handle to that node before the first operation runs.
    pub handles: Option<String>,
    /// Resources currently open in a live editor session. A target in this set is
    /// refused: writing it from outside would race an unsaved buffer.
    pub open_resources: Vec<String>,
    /// When set, nothing is ever published. Structurally enforced: the dry-run
    /// entry point has no publisher to hand to.
    pub dry_run: bool,
}

impl CanonicalRequest {
    /// A minimal create request.
    #[must_use]
    pub fn create(target: &str) -> Self {
        Self {
            operation: CanonicalOperation::CreateResource,
            target: target.to_owned(),
            resource_name: None,
            root_type: None,
            root_name: None,
            query: None,
            resource_class: None,
            batch: None,
            handles: None,
            open_resources: Vec::new(),
            dry_run: false,
        }
    }

    /// A minimal batch request.
    #[must_use]
    pub fn apply_batch(target: &str, batch: String) -> Self {
        Self {
            operation: CanonicalOperation::ApplyBatch,
            batch: Some(batch),
            ..Self::create(target)
        }
    }

    /// A minimal update request.
    #[must_use]
    pub fn update(target: &str) -> Self {
        Self {
            operation: CanonicalOperation::UpdateResource,
            target: target.to_owned(),
            ..Self::create(target)
        }
    }

    /// A minimal UID registration request.
    #[must_use]
    pub fn register_uid(target: &str) -> Self {
        Self {
            operation: CanonicalOperation::RegisterUid,
            ..Self::create(target)
        }
    }

    /// The request's canonical facts, sorted, as the hash input and the helper's
    /// request document body.
    #[must_use]
    pub fn canonical_facts(&self) -> BTreeMap<String, String> {
        let mut facts = BTreeMap::new();
        facts.insert("operation".to_owned(), self.operation.as_str().to_owned());
        facts.insert("target".to_owned(), self.target.clone());
        if let Some(name) = &self.resource_name {
            facts.insert("resource_name".to_owned(), name.clone());
        }
        if let Some(root_type) = &self.root_type {
            facts.insert("root_type".to_owned(), root_type.clone());
        }
        if let Some(root_name) = &self.root_name {
            facts.insert("root_name".to_owned(), root_name.clone());
        }
        if let Some(query) = &self.query {
            facts.insert("query".to_owned(), query.clone());
        }
        if let Some(resource_class) = &self.resource_class {
            facts.insert("resource_class".to_owned(), resource_class.clone());
        }
        if let Some(batch) = &self.batch {
            facts.insert("batch".to_owned(), batch.clone());
        }
        if let Some(handles) = &self.handles {
            facts.insert("handles".to_owned(), handles.clone());
        }
        facts
    }

    /// The canonical request value, including the facts that never reach the
    /// helper (dry-run, the open-resource set) but do bind the transaction.
    #[must_use]
    pub fn canonical_value(&self) -> Value {
        let mut open = self.open_resources.clone();
        open.sort();
        open.dedup();
        json!({
            "dry_run": self.dry_run,
            "facts": self
                .canonical_facts()
                .into_iter()
                .collect::<BTreeMap<String, String>>(),
            "open_resources": open,
        })
    }

    /// The canonical request bytes.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(&self.canonical_value()).unwrap_or_default()
    }

    /// SHA-256 over the complete canonical request.
    #[must_use]
    pub fn canonical_request_hash(&self) -> String {
        hex_sha256(&self.canonical_bytes())
    }

    /// Deterministic 16-hex staging ID for this request, derived from its own
    /// canonical bytes so an identical request always stages identically.
    #[must_use]
    pub fn staging_id(&self) -> String {
        hex_sha256(&self.canonical_bytes())
            .chars()
            .take(16)
            .collect()
    }

    /// Validate the request against everything decidable without touching disk.
    ///
    /// # Errors
    /// Returns the classification refusal for the target path, or
    /// [`CanonicalError::ResourceOpen`] when the target is open in an editor
    /// session, or [`CanonicalError::UnusablePath`] when a UID query carries no
    /// query string.
    pub fn validate(&self) -> Result<TextResourceKind, CanonicalError> {
        let kind = classify_target(&self.target)?;
        if self.operation == CanonicalOperation::ApplyBatch && self.batch.is_none() {
            return Err(CanonicalError::UnusablePath {
                path: self.target.clone(),
                reason: "a batch request needs its operations".to_owned(),
            });
        }
        if [&self.batch, &self.handles].into_iter().any(|fact| {
            fact.as_deref()
                .is_some_and(|text| text.contains('\n') || text.contains('\r'))
        }) {
            return Err(CanonicalError::UnusablePath {
                path: self.target.clone(),
                reason: "a batch and its handles must be compact single-line JSON".to_owned(),
            });
        }
        if self.operation == CanonicalOperation::QueryUid
            && self
                .query
                .as_ref()
                .is_none_or(|query| query.trim().is_empty())
        {
            return Err(CanonicalError::UnusablePath {
                path: self.target.clone(),
                reason: "a UID query needs a `uid://…` value or a project-relative path".to_owned(),
            });
        }
        let normalized_target = self.target.replace('\\', "/");
        if self.operation.mutates()
            && self.open_resources.iter().any(|open| {
                let open = open.replace('\\', "/");
                project_relative_path(&open) == normalized_target
            })
        {
            return Err(CanonicalError::ResourceOpen {
                path: normalized_target,
            });
        }
        Ok(kind)
    }

    /// The helper request document's pairs, including the staging artifact path
    /// the helper is permitted to write.
    #[must_use]
    pub fn helper_pairs(
        &self,
        staging_id: &str,
        kind: TextResourceKind,
    ) -> BTreeMap<String, String> {
        let mut pairs = self.canonical_facts();
        pairs.insert(
            "artifact".to_owned(),
            HelperScript::artifact_path(staging_id, kind.extension()),
        );
        pairs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_binary_resource_is_refused_from_the_request_alone() {
        for path in ["scenes/main.scn", "art/tiles.RES", "a/b/c.Scn"] {
            let error = classify_target(path).expect_err("binary resources are never written");
            assert!(
                matches!(error, CanonicalError::BinaryResource { .. }),
                "{path} is refused as a binary resource, got {error}"
            );
        }
    }

    #[test]
    fn only_tscn_and_tres_are_writable_text_resources() {
        assert_eq!(
            classify_target("scenes/main.tscn").expect("a scene"),
            TextResourceKind::Scene
        );
        assert_eq!(
            classify_target("data/config.tres").expect("a resource"),
            TextResourceKind::Resource
        );
        for path in ["scripts/player.gd", "project.godot", "art/sprite.png"] {
            assert!(matches!(
                classify_target(path),
                Err(CanonicalError::NotATextResource { .. })
            ));
        }
    }

    #[test]
    fn an_escaping_or_helper_owned_target_is_refused() {
        for path in [
            "/absolute/main.tscn",
            "../outside.tscn",
            "a/../../b.tscn",
            "C:/win.tscn",
            "scenes\\main.tscn",
            "https://example.test/main.tscn",
            "scenes/a\u{0085}b.tscn",
            "godot_mcp_helpers/2/staging/x.tscn",
            "",
        ] {
            assert!(
                matches!(
                    classify_target(path),
                    Err(CanonicalError::UnusablePath { .. })
                ),
                "{path} must be refused as an unusable target"
            );
        }
    }

    #[test]
    fn an_open_resource_is_never_a_mutation_target() {
        let mut request = CanonicalRequest::update("scenes/main.tscn");
        request.open_resources = vec!["res://scenes/main.tscn".to_owned()];
        assert!(matches!(
            request.validate(),
            Err(CanonicalError::ResourceOpen { .. })
        ));

        let mut query = CanonicalRequest::create("scenes/main.tscn");
        query.operation = CanonicalOperation::QueryUid;
        query.query = Some("uid://b1234".to_owned());
        query.open_resources = vec!["res://scenes/main.tscn".to_owned()];
        assert!(
            query.validate().is_ok(),
            "a read-only UID query does not publish, so an open buffer does not conflict"
        );
    }

    #[test]
    fn the_canonical_request_hash_is_stable_and_covers_every_fact() {
        let base = CanonicalRequest::create("scenes/main.tscn");
        assert_eq!(base.canonical_request_hash(), base.canonical_request_hash());
        assert_eq!(base.canonical_request_hash().len(), 64);
        assert_eq!(base.staging_id().len(), 16);

        let mut renamed = base.clone();
        renamed.resource_name = Some("Main".to_owned());
        assert_ne!(
            base.canonical_request_hash(),
            renamed.canonical_request_hash(),
            "every request fact is bound into the hash"
        );

        let mut dry = base.clone();
        dry.dry_run = true;
        assert_ne!(
            base.canonical_request_hash(),
            dry.canonical_request_hash(),
            "a dry run is a different request from a publishing one"
        );
    }

    #[test]
    fn node_handles_reach_the_helper_as_one_bound_fact() {
        let mut request = CanonicalRequest::apply_batch("scenes/main.tscn", "[]".to_owned());
        let plain = request.canonical_request_hash();
        request.handles = Some("{\"nh1:0\":\"Player\"}".to_owned());
        let pairs = request.helper_pairs(&request.staging_id(), TextResourceKind::Scene);
        assert_eq!(
            pairs.get("handles").map(String::as_str),
            Some("{\"nh1:0\":\"Player\"}")
        );
        assert_ne!(request.canonical_request_hash(), plain);
        assert!(request.validate().is_ok());
        request.handles = Some("{\n}".to_owned());
        assert!(request.validate().is_err(), "handles must be one line");
    }

    #[test]
    fn the_helper_request_names_a_staging_artifact_and_never_the_target() {
        let request = CanonicalRequest::create("scenes/main.tscn");
        let staging_id = request.staging_id();
        let pairs = request.helper_pairs(&staging_id, TextResourceKind::Scene);
        let artifact = pairs.get("artifact").expect("an artifact path");
        assert!(artifact.starts_with("godot_mcp_helpers/2/staging/"));
        assert!(artifact.ends_with(".tscn"));
        assert_eq!(
            pairs.get("target").map(String::as_str),
            Some("scenes/main.tscn")
        );
        assert_ne!(artifact, "scenes/main.tscn");
    }

    #[test]
    fn only_a_uid_query_is_non_mutating() {
        assert_eq!(CanonicalOperation::ALL.len(), 5);
        let mutating: Vec<&str> = CanonicalOperation::ALL
            .iter()
            .filter(|operation| operation.mutates())
            .map(|operation| operation.as_str())
            .collect();
        assert_eq!(
            mutating,
            vec![
                "create_resource",
                "update_resource",
                "register_uid",
                "apply_batch"
            ]
        );
        assert!(CanonicalOperation::ApplyBatch.requires_existing_target());
        assert!(CanonicalOperation::UpdateResource.requires_existing_target());
        assert!(CanonicalOperation::RegisterUid.requires_existing_target());
        assert!(!CanonicalOperation::CreateResource.requires_existing_target());
    }
}
