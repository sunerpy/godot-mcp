//! Answer projection: bounded pages, honest provenance, and a public shape that
//! cannot carry an internal identifier.
//!
//! Every `core` tool returns the same envelope — `{ project, data, page?,
//! provenance }` — and every collection inside it is a [`Page`]. Two rules are
//! load-bearing:
//!
//! * **A page is deterministic.** Sections are rendered from the Todo 10 graph's
//!   own total order, so `offset`/`limit` name the same items across calls, and a
//!   section longer than its page declares [`Page::truncated`] plus a
//!   continuation instead of looking complete.
//! * **An answer is never stronger than its weakest item.** The envelope's
//!   confidence is the *weakest* confidence appearing anywhere inside it, so a
//!   single inferred call edge cannot be laundered into an exact answer. The
//!   [`Provenance`] constructor from Todo 3 then refuses a dishonest
//!   source/confidence pairing outright.
//!
//! The projection is also the redaction boundary. A [`GraphNodeId`] spelling such
//! as `script:scripts/player.gd` is an *internal* identity: it is never emitted.
//! Callers receive `{ kind, name, file, line }`, which is addressable by a human
//! and by a follow-up tool call, and nothing else.

use std::collections::BTreeMap;

use godot_mcp_analysis::graph::{
    EdgeResolution, GraphEdge, GraphNode, GraphNodeId, GraphSpan, SemanticGraph,
};
use godot_mcp_analysis::index::{Coherence, IndexSnapshot};
use godot_mcp_domain::Confidence;
use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::{
    GodotValue, OutcomeError, ProjectIdentity, Provenance, ProvenanceSource,
};
use serde_json::{Map, Value, json};

/// Detail key carrying an actionable next step on a refusal.
pub const REMEDIATION_DETAIL_KEY: &str = "remediation";

/// Construct a stable, non-retryable outcome error with no structured detail.
#[must_use]
pub fn outcome_error(kind: ErrorKind, message: impl Into<String>) -> OutcomeError {
    OutcomeError::new(kind, message, false, BTreeMap::new())
}

/// Construct a refusal that tells the caller what to do about it.
#[must_use]
pub fn remediation_error(
    kind: ErrorKind,
    message: impl Into<String>,
    remediation: impl Into<String>,
) -> OutcomeError {
    let mut details = BTreeMap::new();
    details.insert(
        REMEDIATION_DETAIL_KEY.to_owned(),
        GodotValue::String(remediation.into()),
    );
    OutcomeError::new(kind, message, false, details)
}

/// The bounded-window description attached to every collection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Page {
    offset: usize,
    limit: usize,
    returned: usize,
    total: usize,
}

impl Page {
    /// Whether items were withheld because the window ended before the section
    /// did.
    #[must_use]
    pub const fn truncated(self) -> bool {
        self.offset + self.returned < self.total
    }

    /// The offset a caller must pass to continue, when the page is truncated.
    #[must_use]
    pub const fn next_offset(self) -> Option<usize> {
        if self.truncated() {
            Some(self.offset + self.returned)
        } else {
            None
        }
    }

    /// Deterministic public rendering.
    #[must_use]
    pub fn to_value(self) -> Value {
        json!({
            "offset": self.offset,
            "limit": self.limit,
            "returned": self.returned,
            "total": self.total,
            "truncated": self.truncated(),
            "next_offset": self.next_offset(),
        })
    }
}

/// A caller-supplied window, already validated against the session's limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageRequest {
    offset: usize,
    limit: usize,
}

impl PageRequest {
    /// Construct a window.
    #[must_use]
    pub const fn new(offset: usize, limit: usize) -> Self {
        Self { offset, limit }
    }

    /// Apply the window to an already-ordered section.
    ///
    /// The input order is the Todo 10 graph's own total order, so the same
    /// `(offset, limit)` names the same items on every call over one snapshot.
    #[must_use]
    pub fn apply<T>(self, items: Vec<T>) -> (Vec<T>, Page) {
        let total = items.len();
        let taken: Vec<T> = items
            .into_iter()
            .skip(self.offset)
            .take(self.limit)
            .collect();
        let page = Page {
            offset: self.offset,
            limit: self.limit,
            returned: taken.len(),
            total,
        };
        (taken, page)
    }
}

/// Honesty accumulator: an answer's confidence is the weakest of its parts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfidenceFloor {
    confidence: Confidence,
    source: ProvenanceSource,
}

impl ConfidenceFloor {
    /// Start from the strongest claim the caller could possibly make.
    #[must_use]
    pub const fn exact(source: ProvenanceSource) -> Self {
        Self {
            confidence: Confidence::Exact,
            source,
        }
    }

    /// Weaken the floor to include `confidence`.
    ///
    /// Weakening also weakens the *evidence class*, because Todo 3 refuses to
    /// label `StaticIndex` evidence as unresolved and refuses to label
    /// `Heuristic` evidence as exact: the two move together or the provenance
    /// cannot be constructed at all.
    pub fn include(&mut self, confidence: Confidence) {
        let weaker = match (self.confidence, confidence) {
            (Confidence::Unresolved, _) | (_, Confidence::Unresolved) => Confidence::Unresolved,
            (Confidence::Inferred, _) | (_, Confidence::Inferred) => Confidence::Inferred,
            (Confidence::Exact, Confidence::Exact) => Confidence::Exact,
        };
        self.confidence = weaker;
        self.source = match weaker {
            Confidence::Exact => self.source,
            Confidence::Inferred => ProvenanceSource::Heuristic,
            Confidence::Unresolved => ProvenanceSource::Unresolved,
        };
    }

    /// Weaken the floor to include an edge resolution.
    pub fn include_resolution(&mut self, resolution: EdgeResolution) {
        self.include(resolution.confidence());
    }

    /// The honest confidence reached.
    #[must_use]
    pub const fn confidence(self) -> Confidence {
        self.confidence
    }

    /// Build the provenance record for this floor.
    ///
    /// # Errors
    /// Returns [`ErrorKind::Internal`] only when the accumulated pairing is
    /// dishonest, which would be a defect in this module rather than a property
    /// of the project.
    pub fn provenance(
        self,
        subject: impl Into<String>,
        generation: u64,
    ) -> Result<Provenance, OutcomeError> {
        Provenance::new(self.source, self.confidence, subject, generation).map_err(|violation| {
            outcome_error(
                ErrorKind::Internal,
                format!("a core answer produced dishonest provenance: {violation}"),
            )
        })
    }
}

/// Evidence class and floor implied by a snapshot's coherence verdict.
///
/// A snapshot that is behind the filesystem can still be *served*, but never as
/// an exact answer: the floor starts at inferred so the caller sees a
/// remembered-but-unproven result for what it is.
#[must_use]
pub fn floor_for(snapshot: &IndexSnapshot) -> ConfidenceFloor {
    let mut floor = ConfidenceFloor::exact(ProvenanceSource::StaticIndex);
    if !snapshot.is_current() {
        floor.include(Confidence::Inferred);
    }
    floor
}

/// Public, identity-free rendering of one graph node.
///
/// The internal [`GraphNodeId`] is deliberately absent. What remains — kind,
/// name, file, line — is enough to address the same construct in a follow-up
/// call and nothing more.
#[must_use]
pub fn node_value(node: &GraphNode) -> Value {
    json!({
        "kind": node.kind().code(),
        "name": node.name(),
        "file": node.file(),
        "line": node.span().map(GraphSpan::start_line),
        "confidence": node.provenance().confidence(),
    })
}

/// Render a node by internal identity, falling back to an honest unresolved
/// placeholder when the graph has no such node.
///
/// The fallback still never echoes the identity spelling: an unmappable target is
/// reported as unresolved rather than by leaking `script:scripts/player.gd`.
#[must_use]
pub fn node_ref_value(graph: &SemanticGraph, id: &GraphNodeId) -> Value {
    match graph.node(id) {
        Some(node) => node_value(node),
        None => json!({
            "kind": "unresolved",
            "name": Value::Null,
            "file": Value::Null,
            "line": Value::Null,
            "confidence": Confidence::Unresolved,
        }),
    }
}

/// Every internal [`GraphNodeId`] kind prefix, longest first so `sub_resource:`
/// is stripped before `resource:` could match part of it.
const INTERNAL_ID_PREFIXES: &[&str] = &[
    "input_action:",
    "sub_resource:",
    "scene_node:",
    "text_resource:",
    "script_class:",
    "resource:",
    "function:",
    "autoload:",
    "project:",
    "script:",
    "signal:",
    "scene:",
    "asset:",
    "class:",
];

/// Strip an internal node-identity prefix from a candidate hint.
///
/// Todo 10 builds some hints out of the internal [`GraphNodeId`] spelling, and
/// that spelling is not part of the public contract: it names the graph's own
/// keying scheme, which callers must never depend on or address. The payload that
/// remains — a project-relative path, or `path::name` — is information the answer
/// already carries elsewhere, so redacting only the prefix keeps the hint useful
/// and checkable by a human without exposing an internal identifier.
#[must_use]
pub fn public_hint(hint: &str) -> String {
    for prefix in INTERNAL_ID_PREFIXES {
        if let Some(payload) = hint.strip_prefix(prefix) {
            return payload.to_owned();
        }
    }
    hint.to_owned()
}

/// Public rendering of one classified relationship.
///
/// Every candidate keeps its own evidence, so an inferred edge with three
/// candidates is presented as three possibilities rather than as a guess promoted
/// to a fact.
#[must_use]
pub fn edge_value(graph: &SemanticGraph, edge: &GraphEdge) -> Value {
    let candidates: Vec<Value> = edge
        .candidates()
        .iter()
        .map(|candidate| {
            json!({
                "rank": candidate.rank(),
                "hint": public_hint(candidate.hint()),
                "target": candidate.target().map(|target| node_ref_value(graph, target)),
                "confidence": candidate.provenance().confidence(),
            })
        })
        .collect();
    json!({
        "relation": edge.kind().code(),
        "from": node_ref_value(graph, edge.source()),
        "resolution": edge.resolution().code(),
        "candidates": candidates,
        "file": edge.file(),
        "line": edge.span().map(GraphSpan::start_line),
        "detail": edge.detail(),
        "confidence": edge.provenance().confidence(),
    })
}

/// Public rendering of the analyzer's disclosed limitations.
///
/// These are statements about what the static layer cannot see. They are always
/// emitted — an omitted limitation would make an incomplete answer look complete.
#[must_use]
pub fn limitations_value(graph: &SemanticGraph) -> Value {
    Value::Array(
        graph
            .limitations()
            .iter()
            .map(|limitation| {
                json!({
                    "code": limitation.kind().code(),
                    "message": limitation.message(),
                    "subjects": limitation.subjects(),
                })
            })
            .collect(),
    )
}

/// Public rendering of a snapshot's coherence verdict.
///
/// The subjects are project-relative paths the verdict already names; no cache
/// or absolute path is included.
#[must_use]
pub fn coherence_value(coherence: &Coherence) -> Value {
    json!({
        "verdict": coherence.code(),
        "reason": coherence
            .stale_reason()
            .map(|reason| reason.code().to_owned())
            .or_else(|| coherence
                .rescan_reason()
                .map(|reason| reason.code().to_owned())),
        "subjects": coherence.subjects(),
    })
}

/// Assemble the public envelope every `core` tool returns.
///
/// # Errors
/// Returns [`ErrorKind::Internal`] when the accumulated provenance would be
/// dishonest.
pub fn answer(
    project: &ProjectIdentity,
    subject: impl Into<String>,
    floor: ConfidenceFloor,
    data: Value,
) -> Result<Value, OutcomeError> {
    let generation = project.generation();
    let provenance = floor.provenance(subject, generation)?;
    let mut envelope = Map::new();
    envelope.insert(
        "project".to_owned(),
        serde_json::to_value(project).map_err(|_| {
            outcome_error(
                ErrorKind::Internal,
                "project identity could not be rendered",
            )
        })?,
    );
    envelope.insert("data".to_owned(), data);
    envelope.insert(
        "provenance".to_owned(),
        serde_json::to_value(&provenance)
            .map_err(|_| outcome_error(ErrorKind::Internal, "provenance could not be rendered"))?,
    );
    Ok(Value::Object(envelope))
}
