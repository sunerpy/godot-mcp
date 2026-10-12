//! Typed GDScript syntax nodes and parser diagnostics.
//!
//! Every type here is owned by this workspace. Nothing in this module references
//! a CodeGraph store, graph, node-id or edge type: the syntax layer hands back
//! plain typed data plus [`Provenance`], and the semantic graph (Todo 10) and the
//! index (Todo 11) build on top without the analysis crate knowing about them.
//!
//! ## Attribution
//!
//! The GDScript node-kind and field vocabulary reproduced here (the
//! `function_definition` / `constructor_definition` / `class_definition` /
//! `enum_definition` / `call` kinds, the `name` / `parameters` / `return_type` /
//! `body` / `class_body` / `enumerator_list` fields, the `_init` constructor-name
//! token and the `static_keyword` static flag) is adapted from the MIT-licensed
//! `codegraph-rust` project, file
//! `crates/codegraph-extract/src/lang/gdscript.rs` (`GdscriptSpec`).
//! Copyright (c) 2026 sunerpy, MIT. See `THIRD-PARTY-NOTICES.md` at the
//! workspace root for the full notice. Only behavior is adapted; there is no
//! build-time or runtime dependency on that checkout.

use godot_mcp_domain::Confidence;
use godot_mcp_domain::contracts::Provenance;
use serde::{Deserialize, Serialize};

/// 1-based, inclusive line/column span of a syntax construct.
///
/// Lines and columns are 1-based so spans can be reported to a human or an agent
/// without an off-by-one translation step. Byte offsets are retained for exact
/// slicing of the original source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct GdSpan {
    start_line: u32,
    start_column: u32,
    end_line: u32,
    end_column: u32,
    start_byte: u32,
    end_byte: u32,
}

impl GdSpan {
    /// Construct a span from already-1-based line/column pairs and byte offsets.
    #[must_use]
    pub const fn new(
        start_line: u32,
        start_column: u32,
        end_line: u32,
        end_column: u32,
        start_byte: u32,
        end_byte: u32,
    ) -> Self {
        Self {
            start_line,
            start_column,
            end_line,
            end_column,
            start_byte,
            end_byte,
        }
    }

    /// First line of the construct (1-based).
    #[must_use]
    pub const fn start_line(&self) -> u32 {
        self.start_line
    }

    /// First column of the construct (1-based).
    #[must_use]
    pub const fn start_column(&self) -> u32 {
        self.start_column
    }

    /// Last line of the construct (1-based, inclusive).
    #[must_use]
    pub const fn end_line(&self) -> u32 {
        self.end_line
    }

    /// Last column of the construct (1-based, exclusive of the final byte).
    #[must_use]
    pub const fn end_column(&self) -> u32 {
        self.end_column
    }

    /// Byte offset of the first byte of the construct.
    #[must_use]
    pub const fn start_byte(&self) -> u32 {
        self.start_byte
    }

    /// Byte offset one past the last byte of the construct.
    #[must_use]
    pub const fn end_byte(&self) -> u32 {
        self.end_byte
    }

    /// Whether `line` falls inside this span.
    #[must_use]
    pub const fn contains_line(&self, line: u32) -> bool {
        self.start_line <= line && line <= self.end_line
    }

    /// Whether `other` is fully contained in this span, byte-wise.
    #[must_use]
    pub const fn contains_span(&self, other: &Self) -> bool {
        self.start_byte <= other.start_byte && other.end_byte <= self.end_byte
    }
}

/// Class of parser problem surfaced for a GDScript file.
///
/// Tree-sitter recovers from damage instead of failing, so an `ERROR` or
/// `MISSING` node is the only signal that the tree no longer matches the source.
/// Every such node becomes one of these diagnostics; none is ever discarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GdDiagnosticKind {
    /// A tree-sitter `ERROR` node: the source in this span could not be parsed.
    SyntaxError,
    /// A tree-sitter `MISSING` node: a token the grammar requires was absent and
    /// was synthesized by error recovery.
    MissingToken,
    /// A construct was recognized but its name could not be recovered, so it
    /// cannot be reported as a symbol.
    UnnamedDeclaration,
}

impl GdDiagnosticKind {
    /// Stable wire code for this diagnostic kind.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::SyntaxError => "syntax_error",
            Self::MissingToken => "missing_token",
            Self::UnnamedDeclaration => "unnamed_declaration",
        }
    }
}

/// One structured parser problem, tied to the exact span that caused it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GdDiagnostic {
    kind: GdDiagnosticKind,
    span: GdSpan,
    message: String,
}

impl GdDiagnostic {
    /// Construct a diagnostic.
    #[must_use]
    pub fn new(kind: GdDiagnosticKind, span: GdSpan, message: impl Into<String>) -> Self {
        Self {
            kind,
            span,
            message: message.into(),
        }
    }

    /// Problem class.
    #[must_use]
    pub const fn kind(&self) -> GdDiagnosticKind {
        self.kind
    }

    /// Source span the problem was found at.
    #[must_use]
    pub const fn span(&self) -> &GdSpan {
        &self.span
    }

    /// Human-readable explanation.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// One declared parameter of a function, constructor, lambda or signal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GdParameter {
    name: String,
    type_hint: Option<String>,
    has_default: bool,
    span: GdSpan,
}

impl GdParameter {
    /// Construct a parameter record.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        type_hint: Option<String>,
        has_default: bool,
        span: GdSpan,
    ) -> Self {
        Self {
            name: name.into(),
            type_hint,
            has_default,
            span,
        }
    }

    /// Declared identifier.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Declared static type, when the author annotated one.
    #[must_use]
    pub fn type_hint(&self) -> Option<&str> {
        self.type_hint.as_deref()
    }

    /// Whether the parameter declares a default value.
    #[must_use]
    pub const fn has_default(&self) -> bool {
        self.has_default
    }

    /// Source span of the parameter.
    #[must_use]
    pub const fn span(&self) -> &GdSpan {
        &self.span
    }
}

/// A `func` or `_init` declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GdFunction {
    name: String,
    owner: Option<String>,
    is_static: bool,
    is_constructor: bool,
    signature: Option<String>,
    return_type: Option<String>,
    parameters: Vec<GdParameter>,
    span: GdSpan,
    provenance: Provenance,
}

impl GdFunction {
    /// Construct a function record.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: impl Into<String>,
        owner: Option<String>,
        is_static: bool,
        is_constructor: bool,
        signature: Option<String>,
        return_type: Option<String>,
        parameters: Vec<GdParameter>,
        span: GdSpan,
        provenance: Provenance,
    ) -> Self {
        Self {
            name: name.into(),
            owner,
            is_static,
            is_constructor,
            signature,
            return_type,
            parameters,
            span,
            provenance,
        }
    }

    /// Declared name. Constructors report the literal `_init` token, matching the
    /// MIT reference behavior.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Name of the enclosing inner `class`, when the function is nested in one.
    #[must_use]
    pub fn owner(&self) -> Option<&str> {
        self.owner.as_deref()
    }

    /// Whether the declaration carries the `static` keyword.
    #[must_use]
    pub const fn is_static(&self) -> bool {
        self.is_static
    }

    /// Whether this is a `_init` constructor declaration.
    #[must_use]
    pub const fn is_constructor(&self) -> bool {
        self.is_constructor
    }

    /// Verbatim parameter-list text, including the surrounding parentheses.
    #[must_use]
    pub fn signature(&self) -> Option<&str> {
        self.signature.as_deref()
    }

    /// Declared return type, without the `->`.
    #[must_use]
    pub fn return_type(&self) -> Option<&str> {
        self.return_type.as_deref()
    }

    /// Structured parameters.
    #[must_use]
    pub fn parameters(&self) -> &[GdParameter] {
        &self.parameters
    }

    /// Source span of the whole declaration.
    #[must_use]
    pub const fn span(&self) -> &GdSpan {
        &self.span
    }

    /// Evidence backing this record. Downgraded to
    /// [`Confidence::Inferred`](godot_mcp_domain::Confidence::Inferred) when the
    /// declaration overlaps a parser diagnostic.
    #[must_use]
    pub const fn provenance(&self) -> &Provenance {
        &self.provenance
    }
}

/// An inner `class` declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GdClass {
    name: String,
    owner: Option<String>,
    extends: Option<String>,
    span: GdSpan,
    provenance: Provenance,
}

impl GdClass {
    /// Construct an inner-class record.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        owner: Option<String>,
        extends: Option<String>,
        span: GdSpan,
        provenance: Provenance,
    ) -> Self {
        Self {
            name: name.into(),
            owner,
            extends,
            span,
            provenance,
        }
    }

    /// Declared class name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Enclosing inner class, for nested declarations.
    #[must_use]
    pub fn owner(&self) -> Option<&str> {
        self.owner.as_deref()
    }

    /// Declared base type of the inner class.
    #[must_use]
    pub fn extends(&self) -> Option<&str> {
        self.extends.as_deref()
    }

    /// Source span of the declaration.
    #[must_use]
    pub const fn span(&self) -> &GdSpan {
        &self.span
    }

    /// Evidence backing this record.
    #[must_use]
    pub const fn provenance(&self) -> &Provenance {
        &self.provenance
    }
}

/// One `enum` member.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GdEnumMember {
    name: String,
    value: Option<String>,
    span: GdSpan,
}

impl GdEnumMember {
    /// Construct an enum-member record.
    #[must_use]
    pub fn new(name: impl Into<String>, value: Option<String>, span: GdSpan) -> Self {
        Self {
            name: name.into(),
            value,
            span,
        }
    }

    /// Member identifier.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Verbatim explicit value expression, when one was written.
    #[must_use]
    pub fn value(&self) -> Option<&str> {
        self.value.as_deref()
    }

    /// Source span of the member.
    #[must_use]
    pub const fn span(&self) -> &GdSpan {
        &self.span
    }
}

/// An `enum` declaration and its members.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GdEnum {
    name: String,
    owner: Option<String>,
    members: Vec<GdEnumMember>,
    span: GdSpan,
    provenance: Provenance,
}

impl GdEnum {
    /// Construct an enum record.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        owner: Option<String>,
        members: Vec<GdEnumMember>,
        span: GdSpan,
        provenance: Provenance,
    ) -> Self {
        Self {
            name: name.into(),
            owner,
            members,
            span,
            provenance,
        }
    }

    /// Declared enum name. Anonymous enums are reported as a diagnostic instead.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Enclosing inner class, when nested.
    #[must_use]
    pub fn owner(&self) -> Option<&str> {
        self.owner.as_deref()
    }

    /// Declared members, in source order.
    #[must_use]
    pub fn members(&self) -> &[GdEnumMember] {
        &self.members
    }

    /// Source span of the declaration.
    #[must_use]
    pub const fn span(&self) -> &GdSpan {
        &self.span
    }

    /// Evidence backing this record.
    #[must_use]
    pub const fn provenance(&self) -> &Provenance {
        &self.provenance
    }
}

/// A `signal` declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GdSignal {
    name: String,
    owner: Option<String>,
    parameters: Vec<GdParameter>,
    span: GdSpan,
    provenance: Provenance,
}

impl GdSignal {
    /// Construct a signal record.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        owner: Option<String>,
        parameters: Vec<GdParameter>,
        span: GdSpan,
        provenance: Provenance,
    ) -> Self {
        Self {
            name: name.into(),
            owner,
            parameters,
            span,
            provenance,
        }
    }

    /// Declared signal name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Enclosing inner class, when nested.
    #[must_use]
    pub fn owner(&self) -> Option<&str> {
        self.owner.as_deref()
    }

    /// Declared signal parameters.
    #[must_use]
    pub fn parameters(&self) -> &[GdParameter] {
        &self.parameters
    }

    /// Source span of the declaration.
    #[must_use]
    pub const fn span(&self) -> &GdSpan {
        &self.span
    }

    /// Evidence backing this record.
    #[must_use]
    pub const fn provenance(&self) -> &Provenance {
        &self.provenance
    }
}

/// Storage class of a member declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GdVariableKind {
    /// `var` member.
    Var,
    /// `const` member.
    Const,
    /// `@export var` (Godot 4) or `export(...) var` (Godot 3) member.
    Export,
    /// `@onready var` (Godot 4) or `onready var` (Godot 3) member.
    OnReady,
}

impl GdVariableKind {
    /// Stable wire code for this storage class.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Var => "var",
            Self::Const => "const",
            Self::Export => "export",
            Self::OnReady => "onready",
        }
    }
}

/// A `var` / `const` member declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GdVariable {
    name: String,
    owner: Option<String>,
    kind: GdVariableKind,
    type_hint: Option<String>,
    is_static: bool,
    default_value: Option<String>,
    span: GdSpan,
    provenance: Provenance,
}

impl GdVariable {
    /// Construct a member-variable record.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: impl Into<String>,
        owner: Option<String>,
        kind: GdVariableKind,
        type_hint: Option<String>,
        is_static: bool,
        default_value: Option<String>,
        span: GdSpan,
        provenance: Provenance,
    ) -> Self {
        Self {
            name: name.into(),
            owner,
            kind,
            type_hint,
            is_static,
            default_value,
            span,
            provenance,
        }
    }

    /// Declared member name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Enclosing inner class, when nested.
    #[must_use]
    pub fn owner(&self) -> Option<&str> {
        self.owner.as_deref()
    }

    /// Storage class.
    #[must_use]
    pub const fn kind(&self) -> GdVariableKind {
        self.kind
    }

    /// Declared static type, when annotated. `:=` inference yields `None`
    /// because the syntax layer cannot prove the inferred type.
    #[must_use]
    pub fn type_hint(&self) -> Option<&str> {
        self.type_hint.as_deref()
    }

    /// Whether the declaration carries the `static` keyword.
    #[must_use]
    pub const fn is_static(&self) -> bool {
        self.is_static
    }

    /// Verbatim initializer expression, when one was written.
    #[must_use]
    pub fn default_value(&self) -> Option<&str> {
        self.default_value.as_deref()
    }

    /// Source span of the declaration.
    #[must_use]
    pub const fn span(&self) -> &GdSpan {
        &self.span
    }

    /// Evidence backing this record.
    #[must_use]
    pub const fn provenance(&self) -> &Provenance {
        &self.provenance
    }
}

/// Dispatch shape of a call site, as far as syntax alone can establish it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GdCallKind {
    /// `foo(...)` — an unqualified call in the current scope.
    Local,
    /// `receiver.foo(...)` — the receiver's runtime type is not known statically.
    Qualified,
    /// `super.foo(...)` / `.foo(...)` — dispatched to the base script.
    Base,
}

impl GdCallKind {
    /// Stable wire code for this dispatch shape.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Qualified => "qualified",
            Self::Base => "base",
        }
    }
}

/// A call site.
///
/// The syntax layer never claims to have resolved the callee: a call record
/// always carries [`Confidence::Inferred`], because proving the target needs the
/// project-wide index built by later todos.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GdCall {
    callee: String,
    receiver: Option<String>,
    kind: GdCallKind,
    enclosing_function: Option<String>,
    argument_count: u32,
    span: GdSpan,
    provenance: Provenance,
}

impl GdCall {
    /// Construct a call-site record.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        callee: impl Into<String>,
        receiver: Option<String>,
        kind: GdCallKind,
        enclosing_function: Option<String>,
        argument_count: u32,
        span: GdSpan,
        provenance: Provenance,
    ) -> Self {
        Self {
            callee: callee.into(),
            receiver,
            kind,
            enclosing_function,
            argument_count,
            span,
            provenance,
        }
    }

    /// Called identifier, without the receiver.
    #[must_use]
    pub fn callee(&self) -> &str {
        &self.callee
    }

    /// Verbatim receiver expression for a qualified call.
    #[must_use]
    pub fn receiver(&self) -> Option<&str> {
        self.receiver.as_deref()
    }

    /// Dispatch shape.
    #[must_use]
    pub const fn kind(&self) -> GdCallKind {
        self.kind
    }

    /// Name of the function the call appears in, when it is inside one.
    #[must_use]
    pub fn enclosing_function(&self) -> Option<&str> {
        self.enclosing_function.as_deref()
    }

    /// Number of positional arguments written at the call site.
    #[must_use]
    pub const fn argument_count(&self) -> u32 {
        self.argument_count
    }

    /// Source span of the call.
    #[must_use]
    pub const fn span(&self) -> &GdSpan {
        &self.span
    }

    /// Evidence backing this record; never [`Confidence::Exact`].
    #[must_use]
    pub const fn provenance(&self) -> &Provenance {
        &self.provenance
    }
}

/// A `preload("res://…")` or `load("res://…")` resource reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GdResourceRef {
    path: String,
    is_preload: bool,
    enclosing_function: Option<String>,
    span: GdSpan,
    provenance: Provenance,
}

impl GdResourceRef {
    /// Construct a resource-reference record.
    #[must_use]
    pub fn new(
        path: impl Into<String>,
        is_preload: bool,
        enclosing_function: Option<String>,
        span: GdSpan,
        provenance: Provenance,
    ) -> Self {
        Self {
            path: path.into(),
            is_preload,
            enclosing_function,
            span,
            provenance,
        }
    }

    /// Referenced resource path, with the surrounding quotes removed.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Whether the reference used `preload` (compile-time) rather than `load`.
    #[must_use]
    pub const fn is_preload(&self) -> bool {
        self.is_preload
    }

    /// Name of the function the reference appears in, when inside one.
    #[must_use]
    pub fn enclosing_function(&self) -> Option<&str> {
        self.enclosing_function.as_deref()
    }

    /// Source span of the reference.
    #[must_use]
    pub const fn span(&self) -> &GdSpan {
        &self.span
    }

    /// Evidence backing this record.
    #[must_use]
    pub const fn provenance(&self) -> &Provenance {
        &self.provenance
    }
}

/// Everything the syntax layer recovered from one `.gd` file.
///
/// A file report is always returned when the grammar loaded and the source
/// decoded: damage becomes [`diagnostics`](Self::diagnostics) plus a downgraded
/// [`provenance`](Self::provenance), never a dropped result and never a
/// fabricated exact answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GdScriptFile {
    source_path: String,
    class_name_declaration: Option<String>,
    icon_path: Option<String>,
    extends: Option<String>,
    functions: Vec<GdFunction>,
    classes: Vec<GdClass>,
    enums: Vec<GdEnum>,
    signals: Vec<GdSignal>,
    variables: Vec<GdVariable>,
    calls: Vec<GdCall>,
    preloads: Vec<GdResourceRef>,
    diagnostics: Vec<GdDiagnostic>,
    provenance: Provenance,
}

impl GdScriptFile {
    /// Construct a file report. Callers are expected to be inside this crate;
    /// the constructor is public so downstream tests can build fixtures.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        source_path: impl Into<String>,
        class_name_declaration: Option<String>,
        icon_path: Option<String>,
        extends: Option<String>,
        functions: Vec<GdFunction>,
        classes: Vec<GdClass>,
        enums: Vec<GdEnum>,
        signals: Vec<GdSignal>,
        variables: Vec<GdVariable>,
        calls: Vec<GdCall>,
        preloads: Vec<GdResourceRef>,
        diagnostics: Vec<GdDiagnostic>,
        provenance: Provenance,
    ) -> Self {
        Self {
            source_path: source_path.into(),
            class_name_declaration,
            icon_path,
            extends,
            functions,
            classes,
            enums,
            signals,
            variables,
            calls,
            preloads,
            diagnostics,
            provenance,
        }
    }

    /// Path the source was read from, as supplied by the caller.
    #[must_use]
    pub fn source_path(&self) -> &str {
        &self.source_path
    }

    /// Global script class registered by `class_name`.
    #[must_use]
    pub fn class_name_declaration(&self) -> Option<&str> {
        self.class_name_declaration.as_deref()
    }

    /// Editor icon path declared alongside `class_name`.
    #[must_use]
    pub fn icon_path(&self) -> Option<&str> {
        self.icon_path.as_deref()
    }

    /// Base type declared by the file-level `extends`.
    #[must_use]
    pub fn extends(&self) -> Option<&str> {
        self.extends.as_deref()
    }

    /// Functions and constructors, in source order.
    #[must_use]
    pub fn functions(&self) -> &[GdFunction] {
        &self.functions
    }

    /// Inner classes, in source order.
    #[must_use]
    pub fn classes(&self) -> &[GdClass] {
        &self.classes
    }

    /// Enums, in source order.
    #[must_use]
    pub fn enums(&self) -> &[GdEnum] {
        &self.enums
    }

    /// Signals, in source order.
    #[must_use]
    pub fn signals(&self) -> &[GdSignal] {
        &self.signals
    }

    /// Member `var` / `const` declarations, in source order.
    #[must_use]
    pub fn variables(&self) -> &[GdVariable] {
        &self.variables
    }

    /// Call sites, in source order.
    #[must_use]
    pub fn calls(&self) -> &[GdCall] {
        &self.calls
    }

    /// `preload` / `load` resource references, in source order.
    #[must_use]
    pub fn preloads(&self) -> &[GdResourceRef] {
        &self.preloads
    }

    /// Structured parser problems. Empty exactly when the tree matched the
    /// source with no recovery.
    #[must_use]
    pub fn diagnostics(&self) -> &[GdDiagnostic] {
        &self.diagnostics
    }

    /// Whether the report is partial, i.e. at least one diagnostic was emitted.
    #[must_use]
    pub fn is_partial(&self) -> bool {
        !self.diagnostics.is_empty()
    }

    /// File-level evidence. `Exact` only for a clean parse.
    #[must_use]
    pub const fn provenance(&self) -> &Provenance {
        &self.provenance
    }

    /// File-level confidence, mirroring [`Self::provenance`].
    #[must_use]
    pub const fn confidence(&self) -> Confidence {
        self.provenance.confidence()
    }
}
