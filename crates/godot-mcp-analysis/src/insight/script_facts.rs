//! Per-script facts the project-wide views need and the symbol layer does not
//! keep: path literals, load-like call sites, class-name usages, signal
//! emissions and code connections, per-function complexity, line classes and
//! `@export` declarations.
//!
//! The walk reads the same Tree-sitter GDScript grammar as
//! [`GdScriptParser`](crate::gdscript::GdScriptParser) but keeps different
//! facts, so it owns its own parser instead of widening that layer's records.
//! Symbols (functions, signals, `class_name`, `extends`) still come from
//! [`GdScriptFile`](crate::gdscript::GdScriptFile); nothing here re-derives
//! them.
//!
//! ## Honesty
//!
//! * A fact read out of an `ERROR` region is never invented: damaged subtrees
//!   are not entered, and the file is marked [`ScriptFacts::is_damaged`]. Path
//!   literals are the one exception, and only as a *lower bound*: a damaged
//!   script's raw text is additionally scanned for `res://` / `uid://` strings
//!   ([`ScriptFacts::damaged_text_paths`]) so a file it names is not reported
//!   unused merely because the parser could not recover the call around it.
//! * A load whose path is not a literal (or a same-script string constant) is
//!   recorded as dynamic, with its verbatim argument, never dropped.
//! * Signal names and receivers are recorded as written. Which object a
//!   receiver denotes at run time is not decided here.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use tree_sitter::{Node, Parser};

/// Deepest nesting walked before the subtree is recorded as damaged.
const MAX_DEPTH: usize = 512;

/// Longest verbatim argument or receiver text retained, in characters.
const MAX_SNIPPET_CHARS: usize = 160;

/// Which load-like call a site is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadCallKind {
    /// `preload(path)`: resolved when the script is loaded.
    Preload,
    /// `load(path)`.
    Load,
    /// `<receiver>.load(path)`, e.g. `ResourceLoader.load` or `Image.load`.
    MethodLoad,
    /// `ResourceLoader.load_threaded_request(path)`.
    LoadThreadedRequest,
    /// `SceneTree.change_scene_to_file(path)`.
    ChangeSceneToFile,
    /// `FileAccess.open`, `FileAccess.get_file_as_string` or `get_file_as_bytes`.
    FileRead,
    /// `DirAccess.open`, `DirAccess.get_files_at` or `get_directories_at`: the
    /// whole directory's contents may be used.
    DirectoryList,
}

impl LoadCallKind {
    /// Stable wire code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Preload => "preload",
            Self::Load => "load",
            Self::MethodLoad => "method_load",
            Self::LoadThreadedRequest => "load_threaded_request",
            Self::ChangeSceneToFile => "change_scene_to_file",
            Self::FileRead => "file_read",
            Self::DirectoryList => "directory_list",
        }
    }

    /// Whether the referenced file is loaded when the *script* loads, rather
    /// than when the call runs.
    #[must_use]
    pub const fn is_load_time(self) -> bool {
        matches!(self, Self::Preload)
    }
}

/// How the path argument of a load-like call was established.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PathArgument {
    /// A string literal at the call site.
    Literal,
    /// A bare identifier naming a `const` of this script whose value is a
    /// string literal.
    Constant,
    /// Anything else: the path is built at run time.
    Dynamic,
}

/// One load-like call site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LoadCall {
    /// Which call it is.
    pub kind: LoadCallKind,
    /// The receiver text of a method call (`ResourceLoader`, `get_tree()`).
    pub receiver: Option<String>,
    /// How the path was established.
    pub argument_kind: PathArgument,
    /// The path, when [`Self::argument_kind`] is not dynamic.
    pub path: Option<String>,
    /// The argument exactly as written (truncated to 160 characters).
    pub argument: String,
    /// `res://` / `uid://` string fragments inside a dynamic argument, such as
    /// the `"res://levels/"` of `"res://levels/" + name`.
    pub literal_prefixes: Vec<String>,
    /// Function the call appears in.
    pub enclosing_function: Option<String>,
    /// 1-based line.
    pub line: u32,
    /// 1-based column.
    pub column: u32,
}

/// One `res://` or `uid://` string literal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PathLiteral {
    /// The literal's value, quotes removed.
    pub value: String,
    /// Whether the literal is the path argument of a recorded [`LoadCall`].
    pub load_argument: bool,
    /// Whether the literal is the value of a top-level `const`.
    pub constant: bool,
    /// Whether the literal is the target of `extends "..."`.
    pub extends: bool,
    /// 1-based line.
    pub line: u32,
    /// 1-based column.
    pub column: u32,
}

/// What a signal use does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalUseKind {
    /// `signal_name.emit(...)`.
    Emit,
    /// `emit_signal("signal_name", ...)`.
    EmitSignal,
    /// `signal_name.connect(callable)`.
    ConnectSignal,
    /// `connect("signal_name", callable)`.
    ConnectByName,
}

impl SignalUseKind {
    /// Stable wire code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Emit => "emit",
            Self::EmitSignal => "emit_signal",
            Self::ConnectSignal => "signal_connect",
            Self::ConnectByName => "connect_by_name",
        }
    }

    /// Whether the use emits rather than connects.
    #[must_use]
    pub const fn is_emission(self) -> bool {
        matches!(self, Self::Emit | Self::EmitSignal)
    }
}

/// One signal emission or code connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SignalUse {
    /// What the use does.
    pub kind: SignalUseKind,
    /// The signal name, when written as a literal or an identifier.
    pub signal: Option<String>,
    /// The receiver text, `None` for the script itself (`self` included).
    pub receiver: Option<String>,
    /// The handler method a connection names, when statically recognizable.
    pub handler: Option<String>,
    /// The object the handler is looked up on, `None` for `self`.
    pub handler_target: Option<String>,
    /// The callable or name argument exactly as written (truncated).
    pub argument: Option<String>,
    /// Inner class the use appears in.
    pub owner: Option<String>,
    /// Function the use appears in.
    pub enclosing_function: Option<String>,
    /// 1-based line.
    pub line: u32,
    /// 1-based column.
    pub column: u32,
}

/// Complexity of one function or constructor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FunctionMetrics {
    /// Declared name (`_init` for a constructor).
    pub name: String,
    /// Enclosing inner class.
    pub owner: Option<String>,
    /// First line of the declaration.
    pub line: u32,
    /// Last line of the declaration.
    pub end_line: u32,
    /// Lines the declaration spans.
    pub lines: u32,
    /// McCabe complexity: 1 plus one per `if`, `elif`, `for`, `while`, match
    /// arm, match guard, conditional expression and `and`/`or` operator.
    pub cyclomatic: u32,
    /// Deepest nesting of `if`/`elif`/`else`, `for`, `while`, match-arm and
    /// lambda bodies.
    pub max_nesting: u32,
    /// Whether the function overlaps a damaged region.
    pub damaged: bool,
}

/// How the lines of a script divide.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct LineCounts {
    /// Every line.
    pub total: u32,
    /// Lines with at least one token that is not a comment.
    pub code: u32,
    /// Lines holding only comments (`#region` markers included).
    pub comment: u32,
    /// Lines holding only whitespace.
    pub blank: u32,
}

/// Where an exported variable's type came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TypeSource {
    /// Written as `: Type`.
    Declared,
    /// Inferred with `:=` from a literal initializer.
    InferredLiteral,
    /// Written with `:=`, from an initializer that is not a literal.
    InferredExpression,
    /// Untyped (`var x = value`); the type shown is the initializer's.
    UntypedLiteral,
    /// No type can be read off the declaration.
    Unknown,
}

/// One `@export`ed member variable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExportVariable {
    /// Variable name.
    pub name: String,
    /// Every export annotation exactly as written (`@export_range(0, 10)`).
    pub annotation: String,
    /// Name of the first export annotation (`export_range`).
    pub annotation_name: String,
    /// The type, as declared or as read off a literal initializer.
    pub type_name: Option<String>,
    /// Where [`Self::type_name`] came from.
    pub type_source: TypeSource,
    /// The initializer exactly as written.
    pub default_value: Option<String>,
    /// Active `@export_category`.
    pub category: Option<String>,
    /// Active `@export_group`.
    pub group: Option<String>,
    /// Active `@export_subgroup`.
    pub subgroup: Option<String>,
    /// 1-based line of the variable.
    pub line: u32,
}

/// Everything this walk recovered from one script.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScriptFacts {
    path: String,
    literals: Vec<PathLiteral>,
    load_calls: Vec<LoadCall>,
    identifiers: BTreeSet<String>,
    signal_uses: Vec<SignalUse>,
    functions: Vec<FunctionMetrics>,
    lines: LineCounts,
    exports: Vec<ExportVariable>,
    extends_line: Option<u32>,
    is_tool: bool,
    damaged: bool,
    damaged_text_paths: Vec<PathLiteral>,
}

impl ScriptFacts {
    /// Parse `source` and collect the facts.
    ///
    /// # Errors
    /// Returns a message only when the grammar cannot be loaded or produces no
    /// tree; damaged source still yields facts, marked damaged.
    pub fn parse(path: &str, source: &str) -> Result<Self, String> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_gdscript::LANGUAGE.into())
            .map_err(|error| format!("the GDScript grammar cannot be loaded: {error}"))?;
        let tree = parser
            .parse(source, None)
            .ok_or_else(|| "the GDScript grammar produced no syntax tree".to_owned())?;
        let root = tree.root_node();
        let mut walk = Walk::new(source);
        walk.collect_constants(root);
        walk.visit(root, &Ctx::default(), 0);
        walk.collect_exports(root);
        let lines = count_lines(source, root);
        let damaged = walk.damaged || root.has_error();
        let damaged_text_paths = if damaged {
            scan_text_paths(source)
        } else {
            Vec::new()
        };
        Ok(Self {
            path: path.to_owned(),
            literals: walk.literals,
            load_calls: walk.load_calls,
            identifiers: walk.identifiers,
            signal_uses: walk.signal_uses,
            functions: walk.functions,
            lines,
            exports: walk.exports,
            extends_line: walk.extends_line,
            is_tool: walk.is_tool,
            damaged,
            damaged_text_paths,
        })
    }

    /// The path the facts were collected for.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Every `res://` / `uid://` string literal, in source order.
    #[must_use]
    pub fn literals(&self) -> &[PathLiteral] {
        &self.literals
    }

    /// Every load-like call site, in source order.
    #[must_use]
    pub fn load_calls(&self) -> &[LoadCall] {
        &self.load_calls
    }

    /// Identifier texts used outside member positions (`a.b` keeps `a` only).
    #[must_use]
    pub const fn identifiers(&self) -> &BTreeSet<String> {
        &self.identifiers
    }

    /// Signal emissions and code connections, in source order.
    #[must_use]
    pub fn signal_uses(&self) -> &[SignalUse] {
        &self.signal_uses
    }

    /// Per-function complexity, in source order.
    #[must_use]
    pub fn functions(&self) -> &[FunctionMetrics] {
        &self.functions
    }

    /// How the script's lines divide.
    #[must_use]
    pub const fn lines(&self) -> LineCounts {
        self.lines
    }

    /// Top-level `@export` variables, in source order.
    #[must_use]
    pub fn exports(&self) -> &[ExportVariable] {
        &self.exports
    }

    /// 1-based line of the file-level `extends`, when there is one.
    #[must_use]
    pub const fn extends_line(&self) -> Option<u32> {
        self.extends_line
    }

    /// Whether the script is annotated `@tool`.
    #[must_use]
    pub const fn is_tool(&self) -> bool {
        self.is_tool
    }

    /// Whether any part of the script failed to parse.
    #[must_use]
    pub const fn is_damaged(&self) -> bool {
        self.damaged
    }

    /// `res://` / `uid://` strings found by scanning the raw text of a damaged
    /// script; empty for a clean parse.
    #[must_use]
    pub fn damaged_text_paths(&self) -> &[PathLiteral] {
        &self.damaged_text_paths
    }
}

#[derive(Debug, Clone, Default)]
struct Ctx {
    owner: Option<String>,
    function: Option<usize>,
    function_name: Option<String>,
    nesting: u32,
}

impl Ctx {
    fn nested(&self) -> Self {
        Self {
            nesting: self.nesting + 1,
            ..self.clone()
        }
    }
}

struct Walk<'src> {
    source: &'src str,
    literals: Vec<PathLiteral>,
    load_calls: Vec<LoadCall>,
    identifiers: BTreeSet<String>,
    signal_uses: Vec<SignalUse>,
    functions: Vec<FunctionMetrics>,
    exports: Vec<ExportVariable>,
    constants: BTreeMap<String, String>,
    /// Byte offsets of literals already attributed to a load call or a
    /// constant, so each literal is recorded once with its role.
    consumed: BTreeMap<usize, (bool, bool)>,
    extends_line: Option<u32>,
    is_tool: bool,
    damaged: bool,
}

impl<'src> Walk<'src> {
    fn new(source: &'src str) -> Self {
        Self {
            source,
            literals: Vec::new(),
            load_calls: Vec::new(),
            identifiers: BTreeSet::new(),
            signal_uses: Vec::new(),
            functions: Vec::new(),
            exports: Vec::new(),
            constants: BTreeMap::new(),
            consumed: BTreeMap::new(),
            extends_line: None,
            is_tool: false,
            damaged: false,
        }
    }

    fn text(&self, node: Node<'_>) -> &'src str {
        self.source.get(node.byte_range()).unwrap_or_default()
    }

    fn snippet(&self, node: Node<'_>) -> String {
        truncate_chars(self.text(node).trim(), MAX_SNIPPET_CHARS)
    }

    // -- constants -------------------------------------------------------------

    /// Every `const NAME = "literal"` anywhere in the file, first declaration
    /// winning, so `load(NAME)` can be resolved before the walk reaches it.
    fn collect_constants(&mut self, root: Node<'src>) {
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            if node.is_error() {
                continue;
            }
            if node.kind() == "const_statement"
                && let (Some(name), Some(value)) = (
                    node.child_by_field_name("name"),
                    node.child_by_field_name("value"),
                )
                && is_string_kind(value.kind())
            {
                let literal = literal_value(self.text(value));
                self.constants
                    .entry(self.text(name).to_owned())
                    .or_insert(literal);
                self.consumed.insert(value.start_byte(), (false, true));
            }
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                stack.push(child);
            }
        }
    }

    // -- traversal -------------------------------------------------------------

    fn visit(&mut self, node: Node<'src>, ctx: &Ctx, depth: usize) {
        if depth >= MAX_DEPTH {
            self.damaged = true;
            return;
        }
        if node.is_error() || node.is_missing() {
            self.damaged = true;
            return;
        }
        match node.kind() {
            "annotation" => {
                if ctx.function.is_none() && self.annotation_name(node) == "tool" {
                    self.is_tool = true;
                }
                self.visit_children(node, ctx, depth);
            }
            "function_definition" | "constructor_definition" => {
                self.visit_function(node, ctx, depth);
            }
            "class_definition" => {
                let owner = node
                    .child_by_field_name("name")
                    .map(|name| self.text(name).to_owned());
                let inner = Ctx {
                    owner,
                    function: None,
                    function_name: None,
                    nesting: 0,
                };
                self.visit_children(node, &inner, depth);
            }
            "if_statement" => self.visit_if(node, ctx, depth),
            "for_statement" | "while_statement" => {
                self.count_decision(ctx);
                self.visit_with_nested_body(node, ctx, depth);
            }
            "match_statement" => self.visit_match(node, ctx, depth),
            "conditional_expression" => {
                self.count_decision(ctx);
                self.visit_children(node, ctx, depth);
            }
            "binary_operator" => {
                let op = node
                    .child_by_field_name("op")
                    .map(|op| self.text(op))
                    .unwrap_or_default();
                if matches!(op, "and" | "or" | "&&" | "||") {
                    self.count_decision(ctx);
                }
                self.visit_children(node, ctx, depth);
            }
            "lambda" => self.visit_with_nested_body(node, ctx, depth),
            "call" => self.visit_call(node, ctx, depth),
            "attribute" => self.visit_attribute(node, ctx, depth),
            "string" | "string_name" | "node_path" => self.visit_literal(node),
            "identifier" => {
                self.identifiers.insert(self.text(node).to_owned());
            }
            "extends_statement" => self.visit_extends(node, ctx, depth),
            "parameters" | "pattern_binding" => self.visit_parameters(node, ctx, depth),
            "for_left_placeholder" => {}
            "enumerator" => {
                // `left` is a declared member name, not a usage.
                if let Some(right) = node.child_by_field_name("right") {
                    self.visit(right, ctx, depth + 1);
                }
            }
            _ => self.visit_children(node, ctx, depth),
        }
    }

    fn visit_children(&mut self, node: Node<'src>, ctx: &Ctx, depth: usize) {
        let mut cursor = node.walk();
        let children: Vec<Node<'src>> = node.named_children(&mut cursor).collect();
        for child in children {
            if node.kind() == "for_statement"
                && node
                    .child_by_field_name("left")
                    .is_some_and(|left| left.id() == child.id())
            {
                continue;
            }
            self.visit(child, ctx, depth + 1);
        }
    }

    /// Visit a construct whose `body` field is one nesting level deeper.
    fn visit_with_nested_body(&mut self, node: Node<'src>, ctx: &Ctx, depth: usize) {
        let body = node.child_by_field_name("body").map(|body| body.id());
        let mut cursor = node.walk();
        let children: Vec<Node<'src>> = node.named_children(&mut cursor).collect();
        for child in children {
            if Some(child.id()) == body {
                self.enter_nesting(ctx);
                self.visit(child, &ctx.nested(), depth + 1);
            } else if node.kind() == "for_statement"
                && node
                    .child_by_field_name("left")
                    .is_some_and(|left| left.id() == child.id())
            {
                continue;
            } else {
                self.visit(child, ctx, depth + 1);
            }
        }
    }

    fn visit_if(&mut self, node: Node<'src>, ctx: &Ctx, depth: usize) {
        self.count_decision(ctx);
        let mut cursor = node.walk();
        let children: Vec<Node<'src>> = node.named_children(&mut cursor).collect();
        let body = node.child_by_field_name("body").map(|body| body.id());
        for child in children {
            match child.kind() {
                "elif_clause" => {
                    if child.is_error() || child.is_missing() {
                        self.damaged = true;
                        continue;
                    }
                    self.count_decision(ctx);
                    self.visit_with_nested_body(child, ctx, depth + 1);
                }
                "else_clause" => {
                    if child.is_error() || child.is_missing() {
                        self.damaged = true;
                        continue;
                    }
                    self.visit_with_nested_body(child, ctx, depth + 1);
                }
                _ if Some(child.id()) == body => {
                    self.enter_nesting(ctx);
                    self.visit(child, &ctx.nested(), depth + 1);
                }
                _ => self.visit(child, ctx, depth + 1),
            }
        }
    }

    fn visit_match(&mut self, node: Node<'src>, ctx: &Ctx, depth: usize) {
        let mut cursor = node.walk();
        let children: Vec<Node<'src>> = node.named_children(&mut cursor).collect();
        for child in children {
            if child.kind() != "match_body" {
                self.visit(child, ctx, depth + 1);
                continue;
            }
            let mut arms = child.walk();
            let sections: Vec<Node<'src>> = child.named_children(&mut arms).collect();
            for section in sections {
                if section.is_error() || section.is_missing() {
                    self.damaged = true;
                    continue;
                }
                if section.kind() != "pattern_section" {
                    self.visit(section, ctx, depth + 2);
                    continue;
                }
                self.count_decision(ctx);
                let body = section.child_by_field_name("body").map(|body| body.id());
                let mut parts = section.walk();
                let pieces: Vec<Node<'src>> = section.named_children(&mut parts).collect();
                for piece in pieces {
                    if Some(piece.id()) == body {
                        self.enter_nesting(ctx);
                        self.visit(piece, &ctx.nested(), depth + 3);
                    } else {
                        if piece.kind() == "pattern_guard" {
                            self.count_decision(ctx);
                        }
                        self.visit(piece, ctx, depth + 3);
                    }
                }
            }
        }
    }

    fn visit_function(&mut self, node: Node<'src>, ctx: &Ctx, depth: usize) {
        let name = if node.kind() == "constructor_definition" {
            Some("_init".to_owned())
        } else {
            node.child_by_field_name("name")
                .map(|name| self.text(name).to_owned())
        };
        let Some(name) = name else {
            self.visit_children(node, ctx, depth);
            return;
        };
        let start = node.start_position().row + 1;
        let end = node.end_position().row + 1;
        let index = self.functions.len();
        self.functions.push(FunctionMetrics {
            name: name.clone(),
            owner: ctx.owner.clone(),
            line: clamp_u32(start),
            end_line: clamp_u32(end),
            lines: clamp_u32(end.saturating_sub(start) + 1),
            cyclomatic: 1,
            max_nesting: 0,
            damaged: node.has_error(),
        });
        let inner = Ctx {
            owner: ctx.owner.clone(),
            function: Some(index),
            function_name: Some(name),
            nesting: 0,
        };
        self.visit_children(node, &inner, depth);
    }

    fn count_decision(&mut self, ctx: &Ctx) {
        if let Some(function) = ctx.function.and_then(|index| self.functions.get_mut(index)) {
            function.cyclomatic = function.cyclomatic.saturating_add(1);
        }
    }

    fn enter_nesting(&mut self, ctx: &Ctx) {
        if let Some(function) = ctx.function.and_then(|index| self.functions.get_mut(index)) {
            function.max_nesting = function.max_nesting.max(ctx.nesting + 1);
        }
    }

    fn visit_parameters(&mut self, node: Node<'src>, ctx: &Ctx, depth: usize) {
        // Parameter names are declarations; their types and defaults are uses.
        let mut stack: Vec<Node<'src>> = vec![node];
        while let Some(current) = stack.pop() {
            let mut cursor = current.walk();
            let children: Vec<Node<'src>> = current.named_children(&mut cursor).collect();
            for child in children {
                match child.kind() {
                    "identifier" => {}
                    "type" | "inferred_type" => self.visit(child, ctx, depth + 1),
                    _ if current
                        .child_by_field_name("value")
                        .is_some_and(|value| value.id() == child.id()) =>
                    {
                        self.visit(child, ctx, depth + 1);
                    }
                    _ => stack.push(child),
                }
            }
        }
    }

    fn visit_extends(&mut self, node: Node<'src>, ctx: &Ctx, depth: usize) {
        if ctx.function.is_none() && ctx.owner.is_none() && self.extends_line.is_none() {
            self.extends_line = Some(clamp_u32(node.start_position().row + 1));
        }
        let mut cursor = node.walk();
        let children: Vec<Node<'src>> = node.named_children(&mut cursor).collect();
        for child in children {
            if is_string_kind(child.kind()) {
                let value = literal_value(self.text(child));
                if is_path_literal(&value) {
                    let position = child.start_position();
                    self.literals.push(PathLiteral {
                        value,
                        load_argument: false,
                        constant: false,
                        extends: true,
                        line: clamp_u32(position.row + 1),
                        column: clamp_u32(position.column + 1),
                    });
                }
            } else {
                self.visit(child, ctx, depth + 1);
            }
        }
    }

    fn visit_literal(&mut self, node: Node<'src>) {
        let value = literal_value(self.text(node));
        if !is_path_literal(&value) {
            return;
        }
        let (load_argument, constant) = self
            .consumed
            .get(&node.start_byte())
            .copied()
            .unwrap_or((false, false));
        let position = node.start_position();
        self.literals.push(PathLiteral {
            value,
            load_argument,
            constant,
            extends: false,
            line: clamp_u32(position.row + 1),
            column: clamp_u32(position.column + 1),
        });
    }

    // -- calls -----------------------------------------------------------------

    fn arguments(node: Node<'src>) -> Vec<Node<'src>> {
        node.child_by_field_name("arguments")
            .map(|arguments| {
                let mut cursor = arguments.walk();
                arguments
                    .named_children(&mut cursor)
                    .filter(|child| child.kind() != "comment")
                    .collect()
            })
            .unwrap_or_default()
    }

    fn visit_call(&mut self, node: Node<'src>, ctx: &Ctx, depth: usize) {
        let callee = {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .find(|child| child.kind() == "identifier")
                .filter(|child| {
                    node.child_by_field_name("arguments")
                        .is_none_or(|arguments| child.start_byte() < arguments.start_byte())
                })
        };
        if let Some(callee) = callee {
            let name = self.text(callee);
            let arguments = Self::arguments(node);
            match name {
                "preload" => self.record_load(LoadCallKind::Preload, None, &arguments, node, ctx),
                "load" => self.record_load(LoadCallKind::Load, None, &arguments, node, ctx),
                "emit_signal" => {
                    self.record_name_use(SignalUseKind::EmitSignal, None, &arguments, node, ctx);
                }
                "connect" => self.record_connect(None, None, &arguments, node, ctx),
                _ => {}
            }
        }
        self.visit_children(node, ctx, depth);
    }

    fn visit_attribute(&mut self, node: Node<'src>, ctx: &Ctx, depth: usize) {
        let mut cursor = node.walk();
        let children: Vec<Node<'src>> = node.named_children(&mut cursor).collect();
        for (index, child) in children.iter().copied().enumerate() {
            if index == 0 {
                self.visit(child, ctx, depth + 1);
                continue;
            }
            match child.kind() {
                // A member name after `.` is not a usage of a global name.
                "identifier" => {}
                "attribute_call" => {
                    self.attribute_call(node, &children, index, ctx);
                    for argument in Self::arguments(child) {
                        self.visit(argument, ctx, depth + 2);
                    }
                }
                "attribute_subscript" => {
                    if let Some(arguments) = child.child_by_field_name("arguments") {
                        self.visit(arguments, ctx, depth + 2);
                    }
                }
                _ => self.visit(child, ctx, depth + 1),
            }
        }
    }

    /// The receiver text of the member at `index`: everything before it.
    fn receiver_before(&self, attribute: Node<'_>, member: Node<'_>) -> Option<String> {
        let raw = self
            .source
            .get(attribute.start_byte()..member.start_byte())
            .unwrap_or_default();
        let trimmed = raw.trim().trim_end_matches('.').trim();
        if trimmed.is_empty() || trimmed == "self" {
            None
        } else {
            Some(truncate_chars(trimmed, MAX_SNIPPET_CHARS))
        }
    }

    fn attribute_call(
        &mut self,
        attribute: Node<'src>,
        children: &[Node<'src>],
        index: usize,
        ctx: &Ctx,
    ) {
        let call = children[index];
        let Some(member) = ({
            let mut cursor = call.walk();
            call.named_children(&mut cursor)
                .find(|child| child.kind() == "identifier")
        }) else {
            return;
        };
        let name = self.text(member);
        let receiver = self.receiver_before(attribute, member);
        let arguments = Self::arguments(call);
        let receiver_name = receiver.as_deref().unwrap_or_default();
        match name {
            "load" => {
                self.record_load(LoadCallKind::MethodLoad, receiver, &arguments, call, ctx);
            }
            "load_threaded_request" => self.record_load(
                LoadCallKind::LoadThreadedRequest,
                receiver,
                &arguments,
                call,
                ctx,
            ),
            "change_scene_to_file" => self.record_load(
                LoadCallKind::ChangeSceneToFile,
                receiver,
                &arguments,
                call,
                ctx,
            ),
            "open" if receiver_name == "FileAccess" => {
                self.record_load(LoadCallKind::FileRead, receiver, &arguments, call, ctx);
            }
            "get_file_as_string" | "get_file_as_bytes" if receiver_name == "FileAccess" => {
                self.record_load(LoadCallKind::FileRead, receiver, &arguments, call, ctx);
            }
            "open" | "get_files_at" | "get_directories_at" if receiver_name == "DirAccess" => {
                self.record_load(LoadCallKind::DirectoryList, receiver, &arguments, call, ctx);
            }
            "emit_signal" => {
                self.record_name_use(SignalUseKind::EmitSignal, receiver, &arguments, call, ctx);
            }
            "emit" => {
                // `signal_name.emit(...)`: the member before `.emit` names the
                // signal; anything else is a dynamically obtained Signal.
                let (signal, signal_receiver) = self.signal_before(attribute, children, index);
                let position = call.start_position();
                self.signal_uses.push(SignalUse {
                    kind: SignalUseKind::Emit,
                    signal,
                    receiver: signal_receiver,
                    handler: None,
                    handler_target: None,
                    argument: None,
                    owner: ctx.owner.clone(),
                    enclosing_function: ctx.function_name.clone(),
                    line: clamp_u32(position.row + 1),
                    column: clamp_u32(position.column + 1),
                });
            }
            "connect" => {
                let first_is_name = arguments
                    .first()
                    .is_some_and(|first| is_string_kind(first.kind()));
                if first_is_name || is_object_connect_shape(self, &arguments) {
                    self.record_connect(receiver, None, &arguments, call, ctx);
                } else {
                    let (signal, signal_receiver) = self.signal_before(attribute, children, index);
                    let (handler, handler_target) = arguments
                        .first()
                        .map_or((None, None), |callable| self.handler_of(*callable));
                    let position = call.start_position();
                    self.signal_uses.push(SignalUse {
                        kind: SignalUseKind::ConnectSignal,
                        signal,
                        receiver: signal_receiver,
                        handler,
                        handler_target,
                        argument: arguments.first().map(|callable| self.snippet(*callable)),
                        owner: ctx.owner.clone(),
                        enclosing_function: ctx.function_name.clone(),
                        line: clamp_u32(position.row + 1),
                        column: clamp_u32(position.column + 1),
                    });
                }
            }
            _ => {}
        }
    }

    /// The signal named by the member right before `.emit` / `.connect`, and
    /// the receiver before that member.
    fn signal_before(
        &self,
        attribute: Node<'src>,
        children: &[Node<'src>],
        index: usize,
    ) -> (Option<String>, Option<String>) {
        let Some(previous) = index.checked_sub(1).map(|previous| children[previous]) else {
            return (None, None);
        };
        if previous.kind() != "identifier" {
            return (None, None);
        }
        let signal = self.text(previous).to_owned();
        let receiver = if index == 1 {
            None
        } else {
            self.receiver_before(attribute, previous)
        };
        (Some(signal), receiver)
    }

    fn record_load(
        &mut self,
        kind: LoadCallKind,
        receiver: Option<String>,
        arguments: &[Node<'src>],
        call: Node<'src>,
        ctx: &Ctx,
    ) {
        let Some(first) = arguments.first().copied() else {
            return;
        };
        let (argument_kind, path) = if is_string_kind(first.kind()) {
            self.consumed.insert(first.start_byte(), (true, false));
            (PathArgument::Literal, Some(literal_value(self.text(first))))
        } else if first.kind() == "identifier"
            && let Some(value) = self.constants.get(self.text(first))
        {
            (PathArgument::Constant, Some(value.clone()))
        } else {
            (PathArgument::Dynamic, None)
        };
        let literal_prefixes = if argument_kind == PathArgument::Dynamic {
            self.path_fragments(first)
        } else {
            Vec::new()
        };
        let position = call.start_position();
        self.load_calls.push(LoadCall {
            kind,
            receiver,
            argument_kind,
            path,
            argument: self.snippet(first),
            literal_prefixes,
            enclosing_function: ctx.function_name.clone(),
            line: clamp_u32(position.row + 1),
            column: clamp_u32(position.column + 1),
        });
    }

    fn path_fragments(&self, node: Node<'src>) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![node];
        while let Some(current) = stack.pop() {
            if is_string_kind(current.kind()) {
                let value = literal_value(self.text(current));
                if is_path_literal(&value) {
                    out.push(value);
                }
                continue;
            }
            let mut cursor = current.walk();
            for child in current.named_children(&mut cursor) {
                stack.push(child);
            }
        }
        out.sort();
        out.dedup();
        out
    }

    fn record_name_use(
        &mut self,
        kind: SignalUseKind,
        receiver: Option<String>,
        arguments: &[Node<'src>],
        call: Node<'src>,
        ctx: &Ctx,
    ) {
        let first = arguments.first().copied();
        let signal = first
            .filter(|first| is_string_kind(first.kind()))
            .map(|first| literal_value(self.text(first)));
        let position = call.start_position();
        self.signal_uses.push(SignalUse {
            kind,
            signal,
            receiver,
            handler: None,
            handler_target: None,
            argument: first.map(|first| self.snippet(first)),
            owner: ctx.owner.clone(),
            enclosing_function: ctx.function_name.clone(),
            line: clamp_u32(position.row + 1),
            column: clamp_u32(position.column + 1),
        });
    }

    /// `connect("signal", callable[, flags])` or the Godot 3 form
    /// `connect("signal", target, "method"[, binds, flags])`.
    fn record_connect(
        &mut self,
        receiver: Option<String>,
        _handler_target: Option<String>,
        arguments: &[Node<'src>],
        call: Node<'src>,
        ctx: &Ctx,
    ) {
        let first = arguments.first().copied();
        let signal = first
            .filter(|first| is_string_kind(first.kind()))
            .map(|first| literal_value(self.text(first)));
        let (handler, handler_target, argument) = match (arguments.get(1), arguments.get(2)) {
            (Some(target), Some(method)) if is_string_kind(method.kind()) => (
                Some(literal_value(self.text(*method))),
                self.object_text(*target),
                Some(self.snippet(*method)),
            ),
            (Some(callable), _) => {
                let (handler, target) = self.handler_of(*callable);
                (handler, target, Some(self.snippet(*callable)))
            }
            _ => (None, None, None),
        };
        let position = call.start_position();
        self.signal_uses.push(SignalUse {
            kind: SignalUseKind::ConnectByName,
            signal,
            receiver,
            handler,
            handler_target,
            argument,
            owner: ctx.owner.clone(),
            enclosing_function: ctx.function_name.clone(),
            line: clamp_u32(position.row + 1),
            column: clamp_u32(position.column + 1),
        });
    }

    fn object_text(&self, node: Node<'_>) -> Option<String> {
        let text = self.text(node).trim();
        if text == "self" {
            None
        } else {
            Some(truncate_chars(text, MAX_SNIPPET_CHARS))
        }
    }

    /// The method a callable argument names, and the object it is looked up on.
    fn handler_of(&self, callable: Node<'src>) -> (Option<String>, Option<String>) {
        match callable.kind() {
            "identifier" => (Some(self.text(callable).to_owned()), None),
            "attribute" => {
                let mut cursor = callable.walk();
                let parts: Vec<Node<'src>> = callable.named_children(&mut cursor).collect();
                match parts.as_slice() {
                    // `self.method`
                    [object, member]
                        if member.kind() == "identifier" && self.text(*object) == "self" =>
                    {
                        (Some(self.text(*member).to_owned()), None)
                    }
                    // `method.bind(...)` / `method.unbind(...)`
                    [method, call]
                        if method.kind() == "identifier"
                            && call.kind() == "attribute_call"
                            && self.member_name(*call).is_some_and(|name| {
                                matches!(name, "bind" | "unbind" | "bindv")
                            }) =>
                    {
                        (Some(self.text(*method).to_owned()), None)
                    }
                    // `object.method`
                    [object, member] if member.kind() == "identifier" => (
                        Some(self.text(*member).to_owned()),
                        self.object_text(*object),
                    ),
                    _ => (None, None),
                }
            }
            "call" => {
                // `Callable(object, "method")`
                let callee = {
                    let mut cursor = callable.walk();
                    callable
                        .named_children(&mut cursor)
                        .find(|child| child.kind() == "identifier")
                };
                if callee.is_some_and(|callee| self.text(callee) == "Callable") {
                    let arguments = Self::arguments(callable);
                    if let (Some(object), Some(method)) = (arguments.first(), arguments.get(1))
                        && is_string_kind(method.kind())
                    {
                        return (
                            Some(literal_value(self.text(*method))),
                            self.object_text(*object),
                        );
                    }
                }
                (None, None)
            }
            _ => (None, None),
        }
    }

    fn member_name(&self, call: Node<'src>) -> Option<&'src str> {
        let mut cursor = call.walk();
        call.named_children(&mut cursor)
            .find(|child| child.kind() == "identifier")
            .map(|identifier| self.text(identifier))
    }

    fn annotation_name(&self, annotation: Node<'_>) -> &'src str {
        let mut cursor = annotation.walk();
        annotation
            .named_children(&mut cursor)
            .find(|child| child.kind() == "identifier")
            .map(|identifier| self.text(identifier))
            .unwrap_or_default()
    }

    // -- exports ---------------------------------------------------------------

    /// Top-level `@export` variables, honouring standalone annotation lines
    /// (`@export` on the line before `var`) and the group annotations.
    fn collect_exports(&mut self, root: Node<'src>) {
        let mut pending: Vec<Node<'src>> = Vec::new();
        let mut category: Option<String> = None;
        let mut group: Option<String> = None;
        let mut subgroup: Option<String> = None;
        let mut cursor = root.walk();
        let statements: Vec<Node<'src>> = root.named_children(&mut cursor).collect();
        for statement in statements {
            match statement.kind() {
                "comment" | "region_start" | "region_end" => {}
                "annotation" => {
                    let name = self.annotation_name(statement);
                    let label = || {
                        Self::arguments(statement)
                            .first()
                            .filter(|first| is_string_kind(first.kind()))
                            .map(|first| literal_value(self.text(*first)))
                            .filter(|label| !label.is_empty())
                    };
                    match name {
                        "export_category" => {
                            category = label();
                            group = None;
                            subgroup = None;
                        }
                        "export_group" => {
                            group = label();
                            subgroup = None;
                        }
                        "export_subgroup" => subgroup = label(),
                        _ => pending.push(statement),
                    }
                }
                "variable_statement" => {
                    let mut annotations: Vec<Node<'src>> = std::mem::take(&mut pending);
                    let mut inner = statement.walk();
                    for child in statement.named_children(&mut inner) {
                        if child.kind() == "annotations" {
                            let mut list = child.walk();
                            annotations.extend(
                                child
                                    .named_children(&mut list)
                                    .filter(|annotation| annotation.kind() == "annotation"),
                            );
                        }
                    }
                    let exported: Vec<Node<'src>> = annotations
                        .into_iter()
                        .filter(|annotation| {
                            let name = self.annotation_name(*annotation);
                            name.starts_with("export")
                                && !matches!(
                                    name,
                                    "export_category" | "export_group" | "export_subgroup"
                                )
                        })
                        .collect();
                    if exported.is_empty() || statement.child_by_field_name("static").is_some() {
                        continue;
                    }
                    let annotation = exported
                        .iter()
                        .map(|annotation| self.text(*annotation).trim().to_owned())
                        .collect::<Vec<_>>()
                        .join(" ");
                    let annotation_name = self.annotation_name(exported[0]).to_owned();
                    self.push_export(
                        statement,
                        annotation,
                        annotation_name,
                        (&category, &group, &subgroup),
                    );
                }
                "export_variable_statement" => {
                    pending.clear();
                    let annotation = match statement.child_by_field_name("arguments") {
                        Some(arguments) => format!("export{}", self.text(arguments)),
                        None => "export".to_owned(),
                    };
                    self.push_export(
                        statement,
                        annotation,
                        "export".to_owned(),
                        (&category, &group, &subgroup),
                    );
                }
                _ => pending.clear(),
            }
        }
    }

    fn push_export(
        &mut self,
        statement: Node<'src>,
        annotation: String,
        annotation_name: String,
        (category, group, subgroup): (&Option<String>, &Option<String>, &Option<String>),
    ) {
        let Some(name) = statement.child_by_field_name("name") else {
            return;
        };
        let value = statement.child_by_field_name("value");
        let declared = statement.child_by_field_name("type");
        let (type_name, type_source) = match declared {
            Some(declared) if declared.kind() == "type" => {
                (Some(self.text(declared).to_owned()), TypeSource::Declared)
            }
            Some(_inferred) => match value.and_then(|value| self.literal_type(value)) {
                Some(literal) => (Some(literal), TypeSource::InferredLiteral),
                None => (None, TypeSource::InferredExpression),
            },
            None => match value.and_then(|value| self.literal_type(value)) {
                Some(literal) => (Some(literal), TypeSource::UntypedLiteral),
                None => (None, TypeSource::Unknown),
            },
        };
        self.exports.push(ExportVariable {
            name: self.text(name).to_owned(),
            annotation,
            annotation_name,
            type_name,
            type_source,
            default_value: value.map(|value| self.text(value).trim().to_owned()),
            category: category.clone(),
            group: group.clone(),
            subgroup: subgroup.clone(),
            line: clamp_u32(statement.start_position().row + 1),
        });
    }

    /// The type a literal initializer has, as the inspector would show it.
    fn literal_type(&self, value: Node<'_>) -> Option<String> {
        let named = match value.kind() {
            "integer" => "int",
            "float" => "float",
            "string" => "String",
            "string_name" => "StringName",
            "node_path" => "NodePath",
            "true" | "false" => "bool",
            "array" => "Array",
            "dictionary" => "Dictionary",
            "unary_operator" => {
                let mut cursor = value.walk();
                let operand = value.named_children(&mut cursor).next()?;
                return match operand.kind() {
                    "integer" => Some("int".to_owned()),
                    "float" => Some("float".to_owned()),
                    _ => None,
                };
            }
            "call" => {
                let mut cursor = value.walk();
                let callee = value
                    .named_children(&mut cursor)
                    .find(|child| child.kind() == "identifier")?;
                let callee = self.text(callee);
                return callee
                    .chars()
                    .next()
                    .is_some_and(char::is_uppercase)
                    .then(|| callee.to_owned());
            }
            _ => return None,
        };
        Some(named.to_owned())
    }
}

/// `object.connect(name_variable, callable[, flags])`: two or more arguments
/// whose second is not a connection flag.
fn is_object_connect_shape(walk: &Walk<'_>, arguments: &[Node<'_>]) -> bool {
    match arguments {
        [_, second, ..] => !looks_like_flags(walk, *second),
        _ => false,
    }
}

fn looks_like_flags(walk: &Walk<'_>, node: Node<'_>) -> bool {
    let text = walk.text(node).trim();
    node.kind() == "integer"
        || text.starts_with("CONNECT_")
        || text.contains(".CONNECT_")
        || (node.kind() == "binary_operator" && text.contains("CONNECT_"))
}

const fn is_string_kind(kind: &str) -> bool {
    matches!(kind.as_bytes(), b"string" | b"string_name" | b"node_path")
}

/// Whether a literal value names a project resource.
#[must_use]
pub fn is_path_literal(value: &str) -> bool {
    value.starts_with("res://") || value.starts_with("uid://")
}

/// A GDScript string literal's value: the `&`, `^` or `r` prefix and the
/// surrounding (possibly triple) quotes removed. Escapes are kept verbatim.
#[must_use]
pub fn literal_value(raw: &str) -> String {
    let mut text = raw.trim();
    for prefix in ["&", "^", "r"] {
        if let Some(rest) = text.strip_prefix(prefix)
            && (rest.starts_with('"') || rest.starts_with('\''))
        {
            text = rest;
            break;
        }
    }
    for quote in ["\"\"\"", "'''", "\"", "'"] {
        if text.len() >= quote.len() * 2 && text.starts_with(quote) && text.ends_with(quote) {
            return text[quote.len()..text.len() - quote.len()].to_owned();
        }
    }
    text.to_owned()
}

/// Every quoted `res://…` / `uid://…` string in raw text, with its position.
///
/// This is the fallback reader for text the grammar could not recover and for
/// files this layer does not parse (shaders, JSON, configuration). It only
/// reports quoted strings, so prose mentioning a path in passing is not taken
/// for a reference.
#[must_use]
pub fn scan_text_paths(text: &str) -> Vec<PathLiteral> {
    let mut out = Vec::new();
    for (row, line) in text.lines().enumerate() {
        let bytes = line.as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            let quote = bytes[index];
            if quote != b'"' && quote != b'\'' {
                index += 1;
                continue;
            }
            let rest = &line[index + 1..];
            if !(rest.starts_with("res://") || rest.starts_with("uid://")) {
                index += 1;
                continue;
            }
            let Some(close) = rest.find(quote as char) else {
                break;
            };
            let value = &rest[..close];
            if !value.is_empty() && !value.contains(['\n', '\r']) {
                out.push(PathLiteral {
                    value: value.to_owned(),
                    load_argument: false,
                    constant: false,
                    extends: false,
                    line: clamp_u32(row + 1),
                    column: clamp_u32(line[..index].chars().count() + 1),
                });
            }
            index += close + 2;
        }
    }
    out
}

fn count_lines(source: &str, root: Node<'_>) -> LineCounts {
    let total = source.lines().count();
    let mut code = vec![false; total + 1];
    let mut comment = vec![false; total + 1];
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let start = node.start_position().row;
        let end = node.end_position().row;
        match node.kind() {
            "comment" | "region_start" | "region_end" => {
                if let Some(slot) = comment.get_mut(start) {
                    *slot = true;
                }
                continue;
            }
            "string" | "string_name" | "node_path" => {
                for slot in &mut code[start.min(total)..=end.min(total)] {
                    *slot = true;
                }
                continue;
            }
            _ => {}
        }
        if node.child_count() == 0 {
            if node.start_byte() < node.end_byte() {
                for slot in &mut code[start.min(total)..=end.min(total)] {
                    *slot = true;
                }
            }
            continue;
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    let mut counts = LineCounts {
        total: clamp_u32(total),
        ..LineCounts::default()
    };
    for (row, line) in source.lines().enumerate() {
        if code[row] {
            counts.code += 1;
        } else if comment[row] {
            counts.comment += 1;
        } else if line.trim().is_empty() {
            counts.blank += 1;
        } else {
            // Text no token covers (inside a damaged region): still code.
            counts.code += 1;
        }
    }
    counts
}

fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(limit).collect();
    out.push('…');
    out
}

fn clamp_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(source: &str) -> ScriptFacts {
        ScriptFacts::parse("scripts/sample.gd", source).expect("facts")
    }

    #[test]
    fn load_calls_record_literals_constants_and_dynamic_arguments() {
        let script = facts(
            "extends Node\n\
             const LEVEL = \"res://levels/one.tscn\"\n\
             func _ready():\n\
             \tvar a = preload(\"res://a.tscn\")\n\
             \tvar b = load(LEVEL)\n\
             \tvar c = load(\"res://levels/\" + name + \".tscn\")\n\
             \tvar d = ResourceLoader.load(\"res://d.tres\")\n\
             \tget_tree().change_scene_to_file(\"res://menu.tscn\")\n\
             \tvar f = FileAccess.open(\"res://data.json\", FileAccess.READ)\n",
        );
        let calls = script.load_calls();
        assert_eq!(calls.len(), 6, "{calls:#?}");
        assert_eq!(calls[0].kind, LoadCallKind::Preload);
        assert_eq!(calls[0].path.as_deref(), Some("res://a.tscn"));
        assert_eq!(calls[1].argument_kind, PathArgument::Constant);
        assert_eq!(calls[1].path.as_deref(), Some("res://levels/one.tscn"));
        assert_eq!(calls[2].argument_kind, PathArgument::Dynamic);
        assert_eq!(calls[2].literal_prefixes, vec!["res://levels/".to_owned()]);
        assert_eq!(calls[2].line, 6);
        assert_eq!(calls[3].kind, LoadCallKind::MethodLoad);
        assert_eq!(calls[3].receiver.as_deref(), Some("ResourceLoader"));
        assert_eq!(calls[4].kind, LoadCallKind::ChangeSceneToFile);
        assert_eq!(calls[5].kind, LoadCallKind::FileRead);
        let constant = script
            .literals()
            .iter()
            .find(|literal| literal.value == "res://levels/one.tscn")
            .expect("constant literal");
        assert!(constant.constant && !constant.load_argument);
        let argument = script
            .literals()
            .iter()
            .find(|literal| literal.value == "res://a.tscn")
            .expect("argument literal");
        assert!(argument.load_argument);
    }

    #[test]
    #[allow(clippy::type_complexity)]
    fn signal_uses_cover_every_emission_and_connection_form() {
        let script = facts(
            "extends Node\n\
             signal hit\n\
             func _ready():\n\
             \thit.connect(_on_hit)\n\
             \tconnect(\"died\", Callable(self, \"_on_died\"))\n\
             \t$Timer.timeout.connect(_on_timeout.bind(1))\n\
             \tplayer.connect(\"hit\", self, \"_on_old\")\n\
             \temit_signal(\"hit\")\n\
             \thit.emit()\n\
             \tself.died.emit()\n\
             \tother.score_changed.emit(3)\n\
             \temit_signal(signal_name)\n",
        );
        let uses = script.signal_uses();
        let summary: Vec<(SignalUseKind, Option<&str>, Option<&str>, Option<&str>)> = uses
            .iter()
            .map(|use_| {
                (
                    use_.kind,
                    use_.signal.as_deref(),
                    use_.receiver.as_deref(),
                    use_.handler.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                (
                    SignalUseKind::ConnectSignal,
                    Some("hit"),
                    None,
                    Some("_on_hit")
                ),
                (
                    SignalUseKind::ConnectByName,
                    Some("died"),
                    None,
                    Some("_on_died")
                ),
                (
                    SignalUseKind::ConnectSignal,
                    Some("timeout"),
                    Some("$Timer"),
                    Some("_on_timeout")
                ),
                (
                    SignalUseKind::ConnectByName,
                    Some("hit"),
                    Some("player"),
                    Some("_on_old")
                ),
                (SignalUseKind::EmitSignal, Some("hit"), None, None),
                (SignalUseKind::Emit, Some("hit"), None, None),
                (SignalUseKind::Emit, Some("died"), None, None),
                (
                    SignalUseKind::Emit,
                    Some("score_changed"),
                    Some("other"),
                    None
                ),
                (SignalUseKind::EmitSignal, None, None, None),
            ]
        );
        assert_eq!(uses[0].enclosing_function.as_deref(), Some("_ready"));
    }

    #[test]
    fn complexity_counts_decisions_and_nesting() {
        let script = facts(
            "func simple():\n\
             \treturn 1\n\
             \n\
             func branchy(a, b):\n\
             \tif a and b or a:\n\
             \t\tpass\n\
             \telif b:\n\
             \t\tfor i in range(3):\n\
             \t\t\twhile i < 2:\n\
             \t\t\t\tbreak\n\
             \telse:\n\
             \t\tmatch a:\n\
             \t\t\t1:\n\
             \t\t\t\tpass\n\
             \t\t\t_ when b:\n\
             \t\t\t\tpass\n\
             \tvar z = 1 if a else 2\n",
        );
        let functions = script.functions();
        assert_eq!(functions[0].name, "simple");
        assert_eq!(functions[0].cyclomatic, 1);
        assert_eq!(functions[0].max_nesting, 0);
        assert_eq!(functions[0].lines, 2);
        let branchy = &functions[1];
        // if, and, or, elif, for, while, 2 arms, 1 guard, 1 ternary = 10 decisions
        assert_eq!(branchy.cyclomatic, 11, "{branchy:?}");
        assert_eq!(branchy.max_nesting, 3);
        assert_eq!(branchy.line, 4);
    }

    #[test]
    fn line_counts_separate_code_comments_and_blanks() {
        let script = facts(
            "# header\n\
             extends Node\n\
             \n\
             ## doc\n\
             var a = \"\"\"multi\n\
             # not a comment\n\
             line\"\"\"\n\
             #region Stuff\n\
             func f(): # trailing\n\
             \tpass\n\
             #endregion\n",
        );
        assert_eq!(
            script.lines(),
            LineCounts {
                total: 11,
                code: 6,
                comment: 4,
                blank: 1,
            }
        );
    }

    #[test]
    #[allow(clippy::type_complexity)]
    fn exports_follow_standalone_annotations_and_groups() {
        let script = facts(
            "extends Node\n\
             @export var speed := 10.0\n\
             @export_range(0, 100) var health: int = 100\n\
             @export\n\
             var title: String = \"x\"\n\
             @export_group(\"Stats\")\n\
             @export_enum(\"A\", \"B\") var mode = 0\n\
             @export_group(\"\")\n\
             @onready var label = $Label\n\
             var plain = 3\n\
             export(int) var legacy = 4\n\
             class Inner:\n\
             \t@export var hidden := 1\n",
        );
        let exports: Vec<(
            &str,
            &str,
            Option<&str>,
            TypeSource,
            Option<&str>,
            Option<&str>,
        )> = script
            .exports()
            .iter()
            .map(|export| {
                (
                    export.name.as_str(),
                    export.annotation.as_str(),
                    export.type_name.as_deref(),
                    export.type_source,
                    export.default_value.as_deref(),
                    export.group.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            exports,
            vec![
                (
                    "speed",
                    "@export",
                    Some("float"),
                    TypeSource::InferredLiteral,
                    Some("10.0"),
                    None
                ),
                (
                    "health",
                    "@export_range(0, 100)",
                    Some("int"),
                    TypeSource::Declared,
                    Some("100"),
                    None
                ),
                (
                    "title",
                    "@export",
                    Some("String"),
                    TypeSource::Declared,
                    Some("\"x\""),
                    None
                ),
                (
                    "mode",
                    "@export_enum(\"A\", \"B\")",
                    Some("int"),
                    TypeSource::UntypedLiteral,
                    Some("0"),
                    Some("Stats")
                ),
                (
                    "legacy",
                    "export(int)",
                    Some("int"),
                    TypeSource::UntypedLiteral,
                    Some("4"),
                    None
                ),
            ]
        );
    }

    #[test]
    fn identifiers_skip_member_names_and_declarations() {
        let script = facts(
            "extends Node\n\
             var e: Enemy = Enemy.new()\n\
             func f(param: Weapon, count = Util.MAX):\n\
             \tif e is Boss:\n\
             \t\te.health = Stats.DEFAULT\n\
             \tfor item in items:\n\
             \t\tpass\n",
        );
        let identifiers = script.identifiers();
        for used in ["Enemy", "Weapon", "Util", "Boss", "Stats", "e", "items"] {
            assert!(identifiers.contains(used), "{used} must be recorded");
        }
        for declared in ["param", "count", "health", "new", "MAX", "DEFAULT", "item"] {
            assert!(
                !identifiers.contains(declared),
                "{declared} must not be recorded"
            );
        }
    }

    #[test]
    fn a_damaged_script_keeps_its_quoted_paths_as_a_lower_bound() {
        let script = facts("extends Node\nfunc f(:\n\tvar x = load(\"res://kept.tscn\"\n");
        assert!(script.is_damaged());
        assert!(
            script
                .damaged_text_paths()
                .iter()
                .any(|literal| literal.value == "res://kept.tscn"),
            "{:?}",
            script.damaged_text_paths()
        );
    }

    #[test]
    fn literal_values_strip_prefixes_and_quotes() {
        assert_eq!(literal_value("\"res://a\""), "res://a");
        assert_eq!(literal_value("'res://a'"), "res://a");
        assert_eq!(literal_value("&\"name\""), "name");
        assert_eq!(literal_value("^\"Path\""), "Path");
        assert_eq!(literal_value("r\"raw\""), "raw");
        assert_eq!(literal_value("\"\"\"long\"\"\""), "long");
    }

    #[test]
    fn text_scanning_reports_only_quoted_paths() {
        let found = scan_text_paths(
            "{\"icon\": \"res://art/icon.png\", 'b': 'uid://abc'}\nsee res://not/quoted.png\n",
        );
        let values: Vec<&str> = found.iter().map(|literal| literal.value.as_str()).collect();
        assert_eq!(values, vec!["res://art/icon.png", "uid://abc"]);
        assert_eq!(found[0].line, 1);
    }
}
