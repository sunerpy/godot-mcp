//! GDScript Tree-sitter parsing front end.
//!
//! ## Attribution
//!
//! The node-kind / field vocabulary and the constructor, static-flag, signature
//! and return-type extraction rules implemented here are adapted from the
//! MIT-licensed `codegraph-rust` project, file
//! `crates/codegraph-extract/src/lang/gdscript.rs` (`GdscriptSpec`).
//! Copyright (c) 2026 sunerpy. Licensed under the MIT License; the full notice
//! is reproduced in `THIRD-PARTY-NOTICES.md` at the workspace root. Only
//! behavior is adapted: nothing links to, or reads from, that checkout.

use godot_mcp_domain::contracts::{Provenance, ProvenanceSource};
use godot_mcp_domain::{Confidence, ErrorKind};
use tree_sitter::{Node, Parser};

use super::nodes::{
    GdCall, GdCallKind, GdClass, GdDiagnostic, GdDiagnosticKind, GdEnum, GdEnumMember, GdFunction,
    GdParameter, GdResourceRef, GdScriptFile, GdSignal, GdSpan, GdVariable, GdVariableKind,
};

/// Deepest syntax nesting the walker descends into before it stops and reports
/// the truncation as a diagnostic. Guards against stack exhaustion on
/// adversarially nested input without ever silently dropping the fact.
const MAX_DEPTH: usize = 512;

/// A hard failure of the syntax layer itself.
///
/// Damaged *source* never produces one of these: it produces a
/// [`GdScriptFile`] carrying diagnostics. This type is reserved for a rejected
/// input (empty path, source too large) or an unusable grammar.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct GdParseError {
    kind: ErrorKind,
    message: String,
}

impl GdParseError {
    fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// Stable outcome category for this failure.
    #[must_use]
    pub const fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// Human-readable explanation.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// A reusable GDScript parser.
///
/// Construction loads the grammar once; [`parse`](Self::parse) may then be
/// called repeatedly. The parser is fully headless and never launches, contacts
/// or requires a Godot editor or runtime.
pub struct GdScriptParser {
    parser: Parser,
}

impl GdScriptParser {
    /// Load the GDScript grammar.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::Unsupported`] when the linked grammar is not
    /// compatible with the linked `tree-sitter` runtime.
    pub fn new() -> Result<Self, GdParseError> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_gdscript::LANGUAGE.into())
            .map_err(|error| {
                GdParseError::new(
                    ErrorKind::Unsupported,
                    format!("GDScript grammar is incompatible with the parser runtime: {error}"),
                )
            })?;
        Ok(Self { parser })
    }

    /// Parse one GDScript source string into typed syntax records.
    ///
    /// `source_path` is echoed back on the report and used to build provenance
    /// subjects; it is never opened. `project_generation` is the caller's
    /// project generation, recorded on every provenance record so a stale answer
    /// is detectable.
    ///
    /// # Errors
    ///
    /// * [`ErrorKind::InvalidInput`] — empty `source_path`, or a source larger
    ///   than `u32::MAX` bytes (spans could not be represented honestly).
    /// * [`ErrorKind::Internal`] — the grammar returned no tree at all.
    pub fn parse(
        &mut self,
        source_path: &str,
        source: &str,
        project_generation: u64,
    ) -> Result<GdScriptFile, GdParseError> {
        if source_path.trim().is_empty() {
            return Err(GdParseError::new(
                ErrorKind::InvalidInput,
                "source path must not be empty",
            ));
        }
        if u32::try_from(source.len()).is_err() {
            return Err(GdParseError::new(
                ErrorKind::InvalidInput,
                "source exceeds the addressable span range of u32 bytes",
            ));
        }

        let tree = self.parser.parse(source, None).ok_or_else(|| {
            GdParseError::new(
                ErrorKind::Internal,
                "GDScript grammar produced no syntax tree",
            )
        })?;

        let mut walker = Walker::new(source);
        walker.walk(tree.root_node(), &Scope::root(), 0);
        walker.finish(source_path, project_generation)
    }
}

// ---------------------------------------------------------------------------
// Walk scope
// ---------------------------------------------------------------------------

/// Lexical context threaded through the walk.
#[derive(Debug, Clone, Default)]
struct Scope {
    owner: Option<String>,
    function: Option<String>,
    top_level: bool,
}

impl Scope {
    fn root() -> Self {
        Self {
            owner: None,
            function: None,
            top_level: true,
        }
    }

    fn nested_in_class(&self, class_name: &str) -> Self {
        Self {
            owner: Some(class_name.to_owned()),
            function: None,
            top_level: false,
        }
    }

    fn nested_in_function(&self, function_name: &str) -> Self {
        Self {
            owner: self.owner.clone(),
            function: Some(function_name.to_owned()),
            top_level: false,
        }
    }

    fn nested(&self) -> Self {
        Self {
            owner: self.owner.clone(),
            function: self.function.clone(),
            top_level: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Raw (pre-provenance) records
// ---------------------------------------------------------------------------

struct RawFunction {
    name: String,
    owner: Option<String>,
    is_static: bool,
    is_constructor: bool,
    signature: Option<String>,
    return_type: Option<String>,
    parameters: Vec<GdParameter>,
    span: GdSpan,
}

struct RawClass {
    name: String,
    owner: Option<String>,
    extends: Option<String>,
    span: GdSpan,
}

struct RawEnum {
    name: String,
    owner: Option<String>,
    members: Vec<GdEnumMember>,
    span: GdSpan,
}

struct RawSignal {
    name: String,
    owner: Option<String>,
    parameters: Vec<GdParameter>,
    span: GdSpan,
}

struct RawVariable {
    name: String,
    owner: Option<String>,
    kind: GdVariableKind,
    type_hint: Option<String>,
    is_static: bool,
    default_value: Option<String>,
    span: GdSpan,
}

struct RawCall {
    callee: String,
    receiver: Option<String>,
    kind: GdCallKind,
    enclosing_function: Option<String>,
    argument_count: u32,
    span: GdSpan,
}

struct RawResourceRef {
    path: String,
    is_preload: bool,
    enclosing_function: Option<String>,
    span: GdSpan,
}

// ---------------------------------------------------------------------------
// Walker
// ---------------------------------------------------------------------------

struct Walker<'src> {
    source: &'src str,
    class_name_declaration: Option<String>,
    icon_path: Option<String>,
    extends: Option<String>,
    functions: Vec<RawFunction>,
    classes: Vec<RawClass>,
    enums: Vec<RawEnum>,
    signals: Vec<RawSignal>,
    variables: Vec<RawVariable>,
    calls: Vec<RawCall>,
    preloads: Vec<RawResourceRef>,
    diagnostics: Vec<GdDiagnostic>,
}

impl<'src> Walker<'src> {
    fn new(source: &'src str) -> Self {
        Self {
            source,
            class_name_declaration: None,
            icon_path: None,
            extends: None,
            functions: Vec::new(),
            classes: Vec::new(),
            enums: Vec::new(),
            signals: Vec::new(),
            variables: Vec::new(),
            calls: Vec::new(),
            preloads: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    // -- helpers ------------------------------------------------------------

    fn text(&self, node: Node<'_>) -> String {
        self.source
            .get(node.byte_range())
            .unwrap_or_default()
            .to_owned()
    }

    fn span(node: Node<'_>) -> GdSpan {
        let start = node.start_position();
        let end = node.end_position();
        GdSpan::new(
            clamp_u32(start.row.saturating_add(1)),
            clamp_u32(start.column.saturating_add(1)),
            clamp_u32(end.row.saturating_add(1)),
            clamp_u32(end.column.saturating_add(1)),
            clamp_u32(node.start_byte()),
            clamp_u32(node.end_byte()),
        )
    }

    fn field_text(&self, node: Node<'_>, field: &str) -> Option<String> {
        node.child_by_field_name(field).map(|n| self.text(n))
    }

    /// Declared type of a `type:`-annotated construct. An `inferred_type`
    /// (`:=`) yields `None`: the syntax layer cannot prove the inferred type.
    fn declared_type(&self, node: Node<'_>) -> Option<String> {
        let field = node.child_by_field_name("type")?;
        if field.kind() == "type" {
            Some(self.text(field))
        } else {
            None
        }
    }

    fn unquote(raw: &str) -> String {
        let trimmed = raw.trim();
        for quote in ['"', '\''] {
            if trimmed.len() >= 2 && trimmed.starts_with(quote) && trimmed.ends_with(quote) {
                return trimmed[1..trimmed.len() - 1].to_owned();
            }
        }
        trimmed.to_owned()
    }

    fn push_diagnostic(&mut self, kind: GdDiagnosticKind, node: Node<'_>, message: &str) {
        self.diagnostics
            .push(GdDiagnostic::new(kind, Self::span(node), message));
    }

    // -- traversal ----------------------------------------------------------

    fn walk(&mut self, node: Node<'src>, scope: &Scope, depth: usize) {
        if depth >= MAX_DEPTH {
            self.push_diagnostic(
                GdDiagnosticKind::SyntaxError,
                node,
                format!("syntax nesting exceeded {MAX_DEPTH} levels; subtree not analyzed")
                    .as_str(),
            );
            return;
        }

        let mut cursor = node.walk();
        let children: Vec<Node<'src>> = node.children(&mut cursor).collect();
        for child in children {
            self.visit(child, scope, depth + 1);
        }
    }

    fn visit(&mut self, node: Node<'src>, scope: &Scope, depth: usize) {
        // Damage is recorded and never descended into, so no symbol can be
        // fabricated out of an unparsed region.
        if node.is_error() {
            self.push_diagnostic(
                GdDiagnosticKind::SyntaxError,
                node,
                "source in this span does not parse as GDScript",
            );
            return;
        }
        if node.is_missing() {
            self.push_diagnostic(
                GdDiagnosticKind::MissingToken,
                node,
                format!(
                    "required token `{}` is absent and was synthesized by error recovery",
                    node.kind()
                )
                .as_str(),
            );
            return;
        }

        match node.kind() {
            "class_name_statement" => self.visit_class_name(node, scope, depth),
            "extends_statement" => self.visit_extends(node, scope),
            "signal_statement" => self.visit_signal(node, scope, depth),
            "enum_definition" => self.visit_enum(node, scope, depth),
            "const_statement" => self.visit_variable(node, scope, GdVariableKind::Const, depth),
            "variable_statement" => {
                let kind = self.annotated_variable_kind(node);
                self.visit_variable(node, scope, kind, depth);
            }
            "export_variable_statement" => {
                self.visit_variable(node, scope, GdVariableKind::Export, depth);
            }
            "onready_variable_statement" => {
                self.visit_variable(node, scope, GdVariableKind::OnReady, depth);
            }
            "function_definition" | "constructor_definition" => {
                self.visit_function(node, scope, depth);
            }
            "class_definition" => self.visit_class(node, scope, depth),
            "call" => self.visit_call(node, scope, depth),
            "base_call" => self.visit_base_call(node, scope, depth),
            "attribute" => self.visit_attribute(node, scope, depth),
            _ => self.walk(node, &scope.nested(), depth),
        }
    }

    fn visit_class_name(&mut self, node: Node<'src>, scope: &Scope, depth: usize) {
        if scope.top_level {
            if let Some(name) = self.field_text(node, "name") {
                self.class_name_declaration.get_or_insert(name);
            } else {
                self.push_diagnostic(
                    GdDiagnosticKind::UnnamedDeclaration,
                    node,
                    "`class_name` declares no recoverable name",
                );
            }
            if let Some(icon) = self.field_text(node, "icon_path") {
                self.icon_path.get_or_insert(Self::unquote(&icon));
            }
            if let Some(extends) = node.child_by_field_name("extends") {
                self.visit_extends(extends, scope);
            }
        }
        self.walk(node, &scope.nested(), depth);
    }

    fn visit_extends(&mut self, node: Node<'src>, scope: &Scope) {
        if !scope.top_level || self.extends.is_some() {
            return;
        }
        let mut cursor = node.walk();
        let base = node
            .named_children(&mut cursor)
            .find(|child| matches!(child.kind(), "type" | "string"))
            .map(|child| {
                if child.kind() == "string" {
                    Self::unquote(&self.text(child))
                } else {
                    self.text(child)
                }
            });
        if let Some(base) = base {
            self.extends = Some(base);
        }
    }

    fn visit_signal(&mut self, node: Node<'src>, scope: &Scope, depth: usize) {
        match self.field_text(node, "name") {
            Some(name) => {
                let parameters = node
                    .child_by_field_name("parameters")
                    .map(|params| self.parameters(params))
                    .unwrap_or_default();
                self.signals.push(RawSignal {
                    name,
                    owner: scope.owner.clone(),
                    parameters,
                    span: Self::span(node),
                });
            }
            None => self.push_diagnostic(
                GdDiagnosticKind::UnnamedDeclaration,
                node,
                "signal declaration has no recoverable name",
            ),
        }
        self.walk(node, &scope.nested(), depth);
    }

    fn visit_enum(&mut self, node: Node<'src>, scope: &Scope, depth: usize) {
        match self.field_text(node, "name") {
            Some(name) => {
                let members = node
                    .child_by_field_name("body")
                    .map(|list| self.enum_members(list))
                    .unwrap_or_default();
                self.enums.push(RawEnum {
                    name,
                    owner: scope.owner.clone(),
                    members,
                    span: Self::span(node),
                });
            }
            None => self.push_diagnostic(
                GdDiagnosticKind::UnnamedDeclaration,
                node,
                "anonymous enum declares no named symbol",
            ),
        }
        self.walk(node, &scope.nested(), depth);
    }

    fn enum_members(&self, list: Node<'src>) -> Vec<GdEnumMember> {
        let mut cursor = list.walk();
        list.named_children(&mut cursor)
            .filter(|child| child.kind() == "enumerator")
            .filter_map(|child| {
                let name = self.field_text(child, "left")?;
                let value = self.field_text(child, "right");
                Some(GdEnumMember::new(name, value, Self::span(child)))
            })
            .collect()
    }

    fn annotated_variable_kind(&self, node: Node<'_>) -> GdVariableKind {
        let mut cursor = node.walk();
        let annotations: Vec<String> = node
            .named_children(&mut cursor)
            .filter(|child| child.kind() == "annotations")
            .map(|child| self.text(child))
            .collect();
        let joined = annotations.join(" ");
        if joined.contains("@export") {
            GdVariableKind::Export
        } else if joined.contains("@onready") {
            GdVariableKind::OnReady
        } else {
            GdVariableKind::Var
        }
    }

    fn visit_variable(
        &mut self,
        node: Node<'src>,
        scope: &Scope,
        kind: GdVariableKind,
        depth: usize,
    ) {
        // Only member declarations are reported. A `var` inside a function body
        // is a local, not part of the script's public shape.
        if scope.function.is_some() {
            self.walk(node, &scope.nested(), depth);
            return;
        }
        match self.field_text(node, "name") {
            Some(name) => {
                self.variables.push(RawVariable {
                    name,
                    owner: scope.owner.clone(),
                    kind,
                    type_hint: self.declared_type(node),
                    is_static: node.child_by_field_name("static").is_some(),
                    default_value: self.field_text(node, "value"),
                    span: Self::span(node),
                });
            }
            None => self.push_diagnostic(
                GdDiagnosticKind::UnnamedDeclaration,
                node,
                "member declaration has no recoverable name",
            ),
        }
        // Descend so an initializer's calls / `preload`s are still collected.
        self.walk(node, &scope.nested(), depth);
    }

    fn visit_function(&mut self, node: Node<'src>, scope: &Scope, depth: usize) {
        let is_constructor = node.kind() == "constructor_definition";
        let name = if is_constructor {
            self.constructor_name(node)
        } else {
            self.field_text(node, "name")
        };

        let Some(name) = name else {
            self.push_diagnostic(
                GdDiagnosticKind::UnnamedDeclaration,
                node,
                "function declaration has no recoverable name",
            );
            self.walk(node, &scope.nested(), depth);
            return;
        };

        let parameters = node
            .child_by_field_name("parameters")
            .map(|params| self.parameters(params))
            .unwrap_or_default();

        self.functions.push(RawFunction {
            name: name.clone(),
            owner: scope.owner.clone(),
            is_static: Self::has_static_keyword(node),
            is_constructor,
            signature: self.field_text(node, "parameters"),
            return_type: self.field_text(node, "return_type"),
            parameters,
            span: Self::span(node),
        });

        self.walk(node, &scope.nested_in_function(&name), depth);
    }

    /// MIT reference behavior: a constructor's name is the literal `_init`
    /// token, which the grammar exposes as an anonymous child rather than a
    /// `name` field.
    fn constructor_name(&self, node: Node<'_>) -> Option<String> {
        let mut cursor = node.walk();
        node.children(&mut cursor)
            .find(|child| child.kind() == "_init")
            .map(|token| self.text(token))
    }

    /// MIT reference behavior: the `static` flag is a `static_keyword` child.
    fn has_static_keyword(node: Node<'_>) -> bool {
        let mut cursor = node.walk();
        node.children(&mut cursor)
            .any(|child| child.kind() == "static_keyword")
    }

    fn visit_class(&mut self, node: Node<'src>, scope: &Scope, depth: usize) {
        let Some(name) = self.field_text(node, "name") else {
            self.push_diagnostic(
                GdDiagnosticKind::UnnamedDeclaration,
                node,
                "inner class declaration has no recoverable name",
            );
            self.walk(node, &scope.nested(), depth);
            return;
        };

        let extends = node
            .child_by_field_name("extends")
            .and_then(|extends_stmt| {
                let mut cursor = extends_stmt.walk();
                extends_stmt
                    .named_children(&mut cursor)
                    .find(|child| matches!(child.kind(), "type" | "string"))
                    .map(|child| Self::unquote(&self.text(child)))
            });

        self.classes.push(RawClass {
            name: name.clone(),
            owner: scope.owner.clone(),
            extends,
            span: Self::span(node),
        });

        self.walk(node, &scope.nested_in_class(&name), depth);
    }

    fn parameters(&self, params: Node<'src>) -> Vec<GdParameter> {
        let mut cursor = params.walk();
        params
            .named_children(&mut cursor)
            .filter_map(|param| match param.kind() {
                "identifier" => Some(GdParameter::new(
                    self.text(param),
                    None,
                    false,
                    Self::span(param),
                )),
                "typed_parameter" => Some(GdParameter::new(
                    self.first_identifier(param)?,
                    self.declared_type(param),
                    false,
                    Self::span(param),
                )),
                "default_parameter" => Some(GdParameter::new(
                    self.first_identifier(param)?,
                    None,
                    true,
                    Self::span(param),
                )),
                "typed_default_parameter" => Some(GdParameter::new(
                    self.first_identifier(param)?,
                    self.declared_type(param),
                    true,
                    Self::span(param),
                )),
                _ => None,
            })
            .collect()
    }

    fn first_identifier(&self, node: Node<'src>) -> Option<String> {
        let mut cursor = node.walk();
        node.named_children(&mut cursor)
            .find(|child| child.kind() == "identifier")
            .map(|child| self.text(child))
    }

    fn argument_count(node: Node<'_>) -> u32 {
        node.child_by_field_name("arguments")
            .map_or(0, |args| clamp_u32(args.named_child_count()))
    }

    fn visit_call(&mut self, node: Node<'src>, scope: &Scope, depth: usize) {
        let callee = {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .find(|child| child.kind() == "identifier")
                .filter(|child| {
                    node.child_by_field_name("arguments")
                        .is_none_or(|args| child.start_byte() < args.start_byte())
                })
                .map(|child| self.text(child))
        };

        if let Some(callee) = callee {
            self.record_resource_ref(node, &callee, scope);
            self.calls.push(RawCall {
                callee,
                receiver: None,
                kind: GdCallKind::Local,
                enclosing_function: scope.function.clone(),
                argument_count: Self::argument_count(node),
                span: Self::span(node),
            });
        }
        self.walk(node, &scope.nested(), depth);
    }

    fn visit_base_call(&mut self, node: Node<'src>, scope: &Scope, depth: usize) {
        if let Some(callee) = self.first_identifier(node) {
            self.calls.push(RawCall {
                callee,
                receiver: None,
                kind: GdCallKind::Base,
                enclosing_function: scope.function.clone(),
                argument_count: Self::argument_count(node),
                span: Self::span(node),
            });
        }
        self.walk(node, &scope.nested(), depth);
    }

    /// `receiver.method(...)` is modeled as an `attribute` whose child is an
    /// `attribute_call`. The receiver text is recovered verbatim; its runtime
    /// type is unknowable from syntax, so the call stays heuristic.
    fn visit_attribute(&mut self, node: Node<'src>, scope: &Scope, depth: usize) {
        let mut cursor = node.walk();
        let calls: Vec<Node<'src>> = node
            .named_children(&mut cursor)
            .filter(|child| child.kind() == "attribute_call")
            .collect();

        for call in calls {
            let Some(callee) = self.first_identifier(call) else {
                continue;
            };
            let receiver = self
                .source
                .get(node.start_byte()..call.start_byte())
                .map(|raw| raw.trim().trim_end_matches('.').trim().to_owned())
                .filter(|raw| !raw.is_empty());
            self.calls.push(RawCall {
                callee,
                receiver,
                kind: GdCallKind::Qualified,
                enclosing_function: scope.function.clone(),
                argument_count: Self::argument_count(call),
                span: Self::span(call),
            });
        }

        self.walk(node, &scope.nested(), depth);
    }

    fn record_resource_ref(&mut self, node: Node<'src>, callee: &str, scope: &Scope) {
        let is_preload = match callee {
            "preload" => true,
            "load" => false,
            _ => return,
        };
        let Some(args) = node.child_by_field_name("arguments") else {
            return;
        };
        let mut cursor = args.walk();
        let Some(literal) = args
            .named_children(&mut cursor)
            .find(|child| child.kind() == "string")
        else {
            // A computed path is a dynamic reference; it is not recorded as a
            // resolvable resource edge by the syntax layer.
            return;
        };
        self.preloads.push(RawResourceRef {
            path: Self::unquote(&self.text(literal)),
            is_preload,
            enclosing_function: scope.function.clone(),
            span: Self::span(node),
        });
    }

    // -- finalization -------------------------------------------------------

    fn damaged(&self, span: &GdSpan) -> bool {
        self.diagnostics
            .iter()
            .any(|diagnostic| span.contains_span(diagnostic.span()))
    }

    fn evidence(
        &self,
        span: &GdSpan,
        subject: String,
        generation: u64,
    ) -> Result<Provenance, GdParseError> {
        let (source, confidence) = if self.damaged(span) {
            (ProvenanceSource::Heuristic, Confidence::Inferred)
        } else {
            (ProvenanceSource::StaticSyntax, Confidence::Exact)
        };
        provenance(source, confidence, subject, generation)
    }

    fn recovered_anything(&self) -> bool {
        self.class_name_declaration.is_some()
            || self.extends.is_some()
            || !self.functions.is_empty()
            || !self.classes.is_empty()
            || !self.enums.is_empty()
            || !self.signals.is_empty()
            || !self.variables.is_empty()
            || !self.calls.is_empty()
            || !self.preloads.is_empty()
    }

    fn finish(self, source_path: &str, generation: u64) -> Result<GdScriptFile, GdParseError> {
        let mut functions = Vec::with_capacity(self.functions.len());
        for raw in &self.functions {
            let subject = subject(source_path, "function", raw.owner.as_deref(), &raw.name);
            functions.push(GdFunction::new(
                raw.name.clone(),
                raw.owner.clone(),
                raw.is_static,
                raw.is_constructor,
                raw.signature.clone(),
                raw.return_type.clone(),
                raw.parameters.clone(),
                raw.span,
                self.evidence(&raw.span, subject, generation)?,
            ));
        }

        let mut classes = Vec::with_capacity(self.classes.len());
        for raw in &self.classes {
            let subject = subject(source_path, "class", raw.owner.as_deref(), &raw.name);
            classes.push(GdClass::new(
                raw.name.clone(),
                raw.owner.clone(),
                raw.extends.clone(),
                raw.span,
                self.evidence(&raw.span, subject, generation)?,
            ));
        }

        let mut enums = Vec::with_capacity(self.enums.len());
        for raw in &self.enums {
            let subject = subject(source_path, "enum", raw.owner.as_deref(), &raw.name);
            enums.push(GdEnum::new(
                raw.name.clone(),
                raw.owner.clone(),
                raw.members.clone(),
                raw.span,
                self.evidence(&raw.span, subject, generation)?,
            ));
        }

        let mut signals = Vec::with_capacity(self.signals.len());
        for raw in &self.signals {
            let subject = subject(source_path, "signal", raw.owner.as_deref(), &raw.name);
            signals.push(GdSignal::new(
                raw.name.clone(),
                raw.owner.clone(),
                raw.parameters.clone(),
                raw.span,
                self.evidence(&raw.span, subject, generation)?,
            ));
        }

        let mut variables = Vec::with_capacity(self.variables.len());
        for raw in &self.variables {
            let subject = subject(
                source_path,
                raw.kind.code(),
                raw.owner.as_deref(),
                &raw.name,
            );
            variables.push(GdVariable::new(
                raw.name.clone(),
                raw.owner.clone(),
                raw.kind,
                raw.type_hint.clone(),
                raw.is_static,
                raw.default_value.clone(),
                raw.span,
                self.evidence(&raw.span, subject, generation)?,
            ));
        }

        // A call target is never proven by syntax alone: the record is always
        // `Inferred`, and a qualified/base receiver is additionally heuristic.
        let mut calls = Vec::with_capacity(self.calls.len());
        for raw in &self.calls {
            let source = if self.damaged(&raw.span) || raw.kind != GdCallKind::Local {
                ProvenanceSource::Heuristic
            } else {
                ProvenanceSource::StaticSyntax
            };
            let subject = format!(
                "{source_path}#call:{}:{}",
                raw.span.start_line(),
                raw.callee
            );
            calls.push(GdCall::new(
                raw.callee.clone(),
                raw.receiver.clone(),
                raw.kind,
                raw.enclosing_function.clone(),
                raw.argument_count,
                raw.span,
                provenance(source, Confidence::Inferred, subject, generation)?,
            ));
        }

        let mut preloads = Vec::with_capacity(self.preloads.len());
        for raw in &self.preloads {
            let subject = format!(
                "{source_path}#resource:{}:{}",
                raw.span.start_line(),
                raw.path
            );
            preloads.push(GdResourceRef::new(
                raw.path.clone(),
                raw.is_preload,
                raw.enclosing_function.clone(),
                raw.span,
                self.evidence(&raw.span, subject, generation)?,
            ));
        }

        let (file_source, file_confidence) = if self.diagnostics.is_empty() {
            (ProvenanceSource::StaticSyntax, Confidence::Exact)
        } else if self.recovered_anything() {
            (ProvenanceSource::Heuristic, Confidence::Inferred)
        } else {
            (ProvenanceSource::Unresolved, Confidence::Unresolved)
        };
        let file_provenance = provenance(
            file_source,
            file_confidence,
            format!("{source_path}#file"),
            generation,
        )?;

        Ok(GdScriptFile::new(
            source_path,
            self.class_name_declaration,
            self.icon_path,
            self.extends,
            functions,
            classes,
            enums,
            signals,
            variables,
            calls,
            preloads,
            self.diagnostics,
            file_provenance,
        ))
    }
}

fn subject(source_path: &str, kind: &str, owner: Option<&str>, name: &str) -> String {
    match owner {
        Some(owner) => format!("{source_path}#{kind}:{owner}.{name}"),
        None => format!("{source_path}#{kind}:{name}"),
    }
}

fn provenance(
    source: ProvenanceSource,
    confidence: Confidence,
    subject: String,
    generation: u64,
) -> Result<Provenance, GdParseError> {
    Provenance::new(source, confidence, subject, generation).map_err(|violation| {
        GdParseError::new(
            violation.kind(),
            format!("parser produced dishonest provenance: {violation}"),
        )
    })
}

fn clamp_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}
