//! The GDScript language-server adapter.
//!
//! # What Godot's language server actually is
//!
//! `GDScriptLanguageServer` is an `EditorPlugin`, registered inside
//! `#ifdef TOOLS_ENABLED` / `#ifndef GDSCRIPT_NO_LSP` and added with
//! `EditorNode::get_singleton()->add_editor_plugin(lsp_plugin)`. An export template
//! is not `TOOLS_ENABLED`, so it contains no language server at all — not a disabled
//! one, an absent one. It listens on `127.0.0.1:6005` by default and speaks
//! JSON-RPC 2.0 over raw TCP with `Content-Length` framing (see
//! [`crate::adapters::codec`]).
//!
//! # Only some methods exist, and the rest are not silently fine
//!
//! Dispatch is `JSONRPC::process_action`, which looks the method name up in a map
//! built from `ClassDB` bindings plus the scopes registered in the protocol
//! constructor (`textDocument`, `completionItem`, `workspace`). An unbound method
//! with an `id` comes back as `METHOD_NOT_FOUND`; an unbound *notification* is
//! dropped with no reply. Notably absent, and therefore refused here up front:
//!
//! * `shutdown` / `exit` — not bound. Clients disconnect the socket instead, and so
//!   does [`LspSession::close`].
//! * `workspace/symbol` — not bound, and `ServerCapabilities::workspaceSymbolProvider`
//!   is advertised `false`. Project-wide symbol search is
//!   [`crate::adapters::error::AdapterError::Unsupported`] here; the static analysis
//!   plane answers that question instead.
//!
//! `foldingRange` and `colorProvider` are a subtler trap: handlers exist but the
//! advertised capabilities hardcode them to `false`. [`LspCapability`] therefore
//! reports what the *server advertised in its `initialize` result*, not what this
//! adapter hopes is there.
//!
//! # Diagnostics are pushed, not polled
//!
//! There is no `textDocument/diagnostic` request in Godot. `publishDiagnostics` is a
//! server→client notification: `GDScriptWorkspace::publish_diagnostics()` calls
//! `notify_client("textDocument/publishDiagnostics", params)` from `parse_script()`,
//! which is reached through `sync_script_content` from `didOpen`, `didChange` **and**
//! `didSave` alike. So the way to obtain diagnostics for a file is to open or change
//! it and then read notifications until the one for that URI arrives — which is what
//! [`LspSession::diagnostics_for`] does, bounded by a deadline and by a maximum
//! number of intervening notifications.
//!
//! # The `rootUri` check is not cosmetic
//!
//! When the client's `rootPath` does not match the editor's project, `initialize()`
//! sends a `gdscript_client/changeWorkspace` notification rather than failing. An
//! adapter that ignored that would silently be answering questions about a different
//! project. [`LspSession::initialize`] treats that notification as a hard mismatch
//! and refuses the session.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::adapters::channel::FramedChannel;
use crate::adapters::error::AdapterError;
use crate::adapters::receipt::{EndpointKind, OwnedEndpoint};
use crate::adapters::uri;

/// Godot-specific notification announcing the native class tree, sent from
/// `initialized()`.
pub const GODOT_CAPABILITIES_NOTIFICATION: &str = "gdscript/capabilities";

/// Godot-specific notification sent when the client's root does not match the
/// editor's project.
pub const CHANGE_WORKSPACE_NOTIFICATION: &str = "gdscript_client/changeWorkspace";

/// Godot-specific notification sent when `show_native_symbols_in_editor` is on.
pub const SHOW_NATIVE_NOTIFICATION: &str = "gdscript_client/showNative";

/// The push-diagnostics notification.
pub const PUBLISH_DIAGNOSTICS_NOTIFICATION: &str = "textDocument/publishDiagnostics";

/// Methods Godot's language server does **not** bind, with what it does instead.
///
/// Refused before a frame is sent. `shutdown` would come back as `METHOD_NOT_FOUND`
/// and `exit` would be silently dropped, so neither is a usable lifecycle step.
pub const UNBOUND_METHODS: [(&str, &str); 3] = [
    (
        "shutdown",
        "GDScriptLanguageProtocol binds no `shutdown`; the request returns JSON-RPC \
         METHOD_NOT_FOUND. Closing the TCP connection is the only shutdown Godot implements.",
    ),
    (
        "exit",
        "GDScriptLanguageProtocol binds no `exit`; as a notification it is silently dropped. \
         Closing the TCP connection is the only shutdown Godot implements.",
    ),
    (
        "workspace/symbol",
        "GDScriptLanguageProtocol binds no `workspace/symbol` and advertises \
         workspaceSymbolProvider=false; project-wide symbol search is answered by the static \
         analysis plane instead.",
    ),
];

/// The maximum notifications this adapter will read while waiting for one specific
/// message.
///
/// Godot pushes `publishDiagnostics` on every `didChange`, and `initialized` triggers
/// a `gdscript/capabilities` notification carrying the whole native class tree. A
/// bound is needed so a chatty editor cannot hold a request open indefinitely; it is
/// separate from the deadline because a fast editor can be chatty without being slow.
pub const MAX_INTERVENING_NOTIFICATIONS: usize = 256;

/// One capability, as the server itself advertised it in its `initialize` result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspCapability {
    /// The capability's key in `ServerCapabilities`.
    pub name: String,
    /// Whether the server advertised it as usable.
    pub advertised: bool,
}

/// One diagnostic from a `publishDiagnostics` notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedDiagnostic {
    /// The document the diagnostic belongs to.
    pub uri: String,
    /// LSP severity, when the server sent one (1 = error … 4 = hint).
    pub severity: Option<i64>,
    /// Zero-based start line.
    pub line: Option<u64>,
    /// Zero-based start character.
    pub character: Option<u64>,
    /// The message text.
    pub message: String,
}

/// One symbol from a `documentSymbol` result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolSummary {
    /// The symbol's name.
    pub name: String,
    /// LSP `SymbolKind`, when present.
    pub kind: Option<i64>,
    /// The symbol's `detail` string, when present.
    pub detail: Option<String>,
    /// Zero-based start line of the symbol's range.
    pub line: Option<u64>,
    /// Nested children, flattened one level deep with their parent recorded.
    pub container: Option<String>,
}

/// The adapter's entry point: it can only be used with a verified owned endpoint.
pub struct LspAdapter;

impl LspAdapter {
    /// Whether `method` is one Godot's language server does not bind.
    #[must_use]
    pub fn unbound(method: &str) -> Option<&'static str> {
        UNBOUND_METHODS
            .iter()
            .find(|(name, _detail)| *name == method)
            .map(|(_name, detail)| *detail)
    }

    /// Open a session on a verified owned endpoint.
    ///
    /// The endpoint token can only come from [`crate::adapters::receipt::EndpointReceipt::verify`],
    /// so an arbitrary loopback listener cannot reach this function.
    ///
    /// # Errors
    /// Returns [`AdapterError::Unsupported`] when the token is not a language-server
    /// endpoint.
    pub fn open<'c>(
        endpoint: &OwnedEndpoint,
        channel: &'c mut dyn FramedChannel,
        deadline: Duration,
    ) -> Result<LspSession<'c>, AdapterError> {
        if endpoint.kind() != EndpointKind::LanguageServer {
            return Err(AdapterError::unsupported(
                "initialize",
                "this endpoint token names the debug adapter, not the language server",
            ));
        }
        Ok(LspSession {
            channel,
            deadline,
            next_id: 1,
            root_uri: endpoint.project_root_uri().to_owned(),
            launch_transaction_id: endpoint.launch_transaction_id().to_owned(),
            capabilities: Vec::new(),
            pending_diagnostics: Vec::new(),
            native_symbols_announced: false,
            initialized: false,
        })
    }
}

/// One live language-server conversation.
pub struct LspSession<'c> {
    channel: &'c mut dyn FramedChannel,
    deadline: Duration,
    next_id: i64,
    root_uri: String,
    launch_transaction_id: String,
    capabilities: Vec<LspCapability>,
    pending_diagnostics: Vec<PublishedDiagnostic>,
    native_symbols_announced: bool,
    initialized: bool,
}

impl LspSession<'_> {
    /// The launch transaction that owns the editor on the other end.
    #[must_use]
    pub fn launch_transaction_id(&self) -> &str {
        &self.launch_transaction_id
    }

    /// The capabilities the server advertised, once `initialize` has completed.
    #[must_use]
    pub fn capabilities(&self) -> &[LspCapability] {
        &self.capabilities
    }

    /// Whether the server advertised `name` as usable.
    ///
    /// Reports what the server said, not what a handler happens to exist for:
    /// `foldingRange` and `colorProvider` have handlers but are advertised `false`.
    #[must_use]
    pub fn advertises(&self, name: &str) -> bool {
        self.capabilities
            .iter()
            .any(|capability| capability.name == name && capability.advertised)
    }

    /// Whether `gdscript/capabilities` has arrived, which is how the editor signals
    /// the native class tree is available.
    #[must_use]
    pub const fn native_symbols_announced(&self) -> bool {
        self.native_symbols_announced
    }

    /// Perform `initialize` + `initialized`.
    ///
    /// A `gdscript_client/changeWorkspace` notification means the editor's project is
    /// not the root this session claims, and is refused rather than followed.
    ///
    /// # Errors
    /// Returns [`AdapterError::PeerError`] for a JSON-RPC error, `MalformedResponse`
    /// for an unusable result, `Ownership`-style refusal via `PeerError` for a
    /// workspace mismatch, and the codec errors for a framing or transport failure.
    pub fn initialize(&mut self) -> Result<(), AdapterError> {
        let params = json!({
            "processId": Value::Null,
            "rootUri": self.root_uri,
            "rootPath": path_of_uri(&self.root_uri),
            "capabilities": {
                "textDocument": {
                    "publishDiagnostics": { "relatedInformation": false },
                    "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                },
            },
        });
        let result = self.request("initialize", &params)?;
        self.capabilities = read_capabilities(&result);
        self.notify("initialized", &json!({}))?;
        self.initialized = true;
        Ok(())
    }

    /// Tell the server a document is open, which is what makes it publish diagnostics.
    ///
    /// # Errors
    /// Returns the codec errors for a framing or transport failure.
    pub fn did_open(&mut self, uri: &str, text: &str) -> Result<(), AdapterError> {
        self.notify(
            "textDocument/didOpen",
            &json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": "gdscript",
                    "version": 1,
                    "text": text,
                },
            }),
        )
    }

    /// Close a document. Godot's handler is an intentional no-op, kept because some
    /// LSP clients require the notification to be sent.
    ///
    /// # Errors
    /// Returns the codec errors for a framing or transport failure.
    pub fn did_close(&mut self, uri: &str) -> Result<(), AdapterError> {
        self.notify(
            "textDocument/didClose",
            &json!({ "textDocument": { "uri": uri } }),
        )
    }

    /// Read notifications until diagnostics for `uri` arrive.
    ///
    /// Diagnostics are pushed, never requested: there is no `textDocument/diagnostic`
    /// in Godot. Bounded by the session deadline and by
    /// [`MAX_INTERVENING_NOTIFICATIONS`].
    ///
    /// # Errors
    /// Returns [`AdapterError::Timeout`] when the deadline passes first, and the codec
    /// errors for a framing or transport failure.
    pub fn diagnostics_for(&mut self, uri: &str) -> Result<Vec<PublishedDiagnostic>, AdapterError> {
        if let Some(index) = self
            .pending_diagnostics
            .iter()
            .position(|diagnostic| diagnostic.uri == uri)
        {
            return Ok(self.drain_diagnostics_for(uri, index));
        }
        let started = Instant::now();
        for _ in 0..MAX_INTERVENING_NOTIFICATIONS {
            if started.elapsed() > self.deadline {
                return Err(AdapterError::Timeout {
                    method: PUBLISH_DIAGNOSTICS_NOTIFICATION.to_owned(),
                    millis: self.deadline.as_millis().min(u128::from(u64::MAX)) as u64,
                });
            }
            let message = self.read_message()?;
            self.absorb_notification(&message)?;
            if let Some(index) = self
                .pending_diagnostics
                .iter()
                .position(|diagnostic| diagnostic.uri == uri)
            {
                return Ok(self.drain_diagnostics_for(uri, index));
            }
        }
        Err(AdapterError::unavailable(&format!(
            "the editor sent {MAX_INTERVENING_NOTIFICATIONS} notifications without publishing \
             diagnostics for {uri}"
        )))
    }

    /// Request a document's symbols.
    ///
    /// # Errors
    /// Returns [`AdapterError::PeerError`] for a JSON-RPC error and
    /// `MalformedResponse` for a result that is neither an array nor null.
    pub fn document_symbols(&mut self, uri: &str) -> Result<Vec<SymbolSummary>, AdapterError> {
        let result = self.request(
            "textDocument/documentSymbol",
            &json!({ "textDocument": { "uri": uri } }),
        )?;
        read_symbols("textDocument/documentSymbol", &result)
    }

    /// Refuse a project-wide symbol search.
    ///
    /// Godot binds no `workspace/symbol` and advertises `workspaceSymbolProvider=false`,
    /// so this is `unsupported` rather than an empty result — an empty list would be
    /// indistinguishable from "the project has no matching symbols".
    ///
    /// # Errors
    /// Always returns [`AdapterError::Unsupported`].
    pub fn workspace_symbols(&self, _query: &str) -> Result<Vec<SymbolSummary>, AdapterError> {
        Err(AdapterError::unsupported(
            "workspace/symbol",
            LspAdapter::unbound("workspace/symbol").unwrap_or("not bound by Godot"),
        ))
    }

    /// Close the session the only way Godot supports: by dropping the connection.
    ///
    /// `shutdown` and `exit` are not bound, so sending them would produce a
    /// `METHOD_NOT_FOUND` and a silently dropped notification respectively.
    #[must_use]
    pub fn close(self) -> &'static str {
        "GDScriptLanguageProtocol binds neither `shutdown` nor `exit`; the session ends by \
         closing the TCP connection"
    }

    /// Refuse a method Godot does not bind, before any frame is sent.
    ///
    /// # Errors
    /// Returns [`AdapterError::Unsupported`] when `method` is unbound.
    pub fn ensure_bound(method: &str) -> Result<(), AdapterError> {
        match LspAdapter::unbound(method) {
            Some(detail) => Err(AdapterError::unsupported(method, detail)),
            None => Ok(()),
        }
    }

    fn drain_diagnostics_for(&mut self, uri: &str, _index: usize) -> Vec<PublishedDiagnostic> {
        let mut matched = Vec::new();
        let mut kept = Vec::new();
        for diagnostic in std::mem::take(&mut self.pending_diagnostics) {
            if diagnostic.uri == uri {
                matched.push(diagnostic);
            } else {
                kept.push(diagnostic);
            }
        }
        self.pending_diagnostics = kept;
        matched
    }

    /// Send a request and read until its matching response arrives, absorbing any
    /// notifications in between.
    fn request(&mut self, method: &str, params: &Value) -> Result<Value, AdapterError> {
        Self::ensure_bound(method)?;
        let id = self.next_id;
        self.next_id += 1;
        let payload = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .map_err(|error| AdapterError::MalformedResponse {
            method: method.to_owned(),
            detail: format!("the request could not be serialized: {error}"),
        })?;
        self.channel.send(&payload)?;
        let started = Instant::now();
        for _ in 0..MAX_INTERVENING_NOTIFICATIONS {
            if started.elapsed() > self.deadline {
                return Err(AdapterError::Timeout {
                    method: method.to_owned(),
                    millis: self.deadline.as_millis().min(u128::from(u64::MAX)) as u64,
                });
            }
            let message = self.read_message()?;
            if message.get("id").and_then(Value::as_i64) == Some(id) {
                if let Some(error) = message.get("error") {
                    return Err(AdapterError::PeerError {
                        method: method.to_owned(),
                        code: error.get("code").and_then(Value::as_i64),
                        message: error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("the engine sent an error with no message")
                            .to_owned(),
                    });
                }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
            self.absorb_notification(&message)?;
        }
        Err(AdapterError::unavailable(&format!(
            "the editor sent {MAX_INTERVENING_NOTIFICATIONS} messages without answering {method}"
        )))
    }

    fn notify(&mut self, method: &str, params: &Value) -> Result<(), AdapterError> {
        Self::ensure_bound(method)?;
        let payload = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .map_err(|error| AdapterError::MalformedResponse {
            method: method.to_owned(),
            detail: format!("the notification could not be serialized: {error}"),
        })?;
        self.channel.send(&payload)?;
        Ok(())
    }

    fn read_message(&mut self) -> Result<Value, AdapterError> {
        let bytes = self.channel.receive()?;
        serde_json::from_slice(&bytes).map_err(|error| AdapterError::MalformedResponse {
            method: "<inbound>".to_owned(),
            detail: format!("the frame payload is not JSON: {error}"),
        })
    }

    /// Record what a server→client notification carried.
    fn absorb_notification(&mut self, message: &Value) -> Result<(), AdapterError> {
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return Ok(());
        };
        match method {
            PUBLISH_DIAGNOSTICS_NOTIFICATION => {
                self.pending_diagnostics
                    .extend(read_published_diagnostics(message));
                Ok(())
            }
            GODOT_CAPABILITIES_NOTIFICATION => {
                self.native_symbols_announced = true;
                Ok(())
            }
            SHOW_NATIVE_NOTIFICATION => Ok(()),
            CHANGE_WORKSPACE_NOTIFICATION => {
                // Godot answers a rootPath it does not spell exactly as its own
                // workspace root with this notification instead of failing the
                // initialize. Naming the held project only confirms it; following
                // any other would silently move the session onto a project this
                // server does not hold.
                let requested = message
                    .pointer("/params/uri")
                    .and_then(Value::as_str)
                    .or_else(|| message.pointer("/params/path").and_then(Value::as_str))
                    .unwrap_or("<unstated>");
                if same_workspace(requested, &self.root_uri) {
                    return Ok(());
                }
                Err(AdapterError::PeerError {
                    method: CHANGE_WORKSPACE_NOTIFICATION.to_owned(),
                    code: None,
                    message: format!(
                        "the editor is hosting {requested}, not the held project {}; a workspace \
                         change is refused rather than followed",
                        self.root_uri
                    ),
                })
            }
            _ => Ok(()),
        }
    }
}

/// Whether a workspace Godot announced (a path or a `file://` URI) is the
/// held project's root, compared the way Godot itself compares them: decoded,
/// with forward slashes, without a trailing separator and ignoring case.
fn same_workspace(announced: &str, root_uri: &str) -> bool {
    let normalize = |spelling: &str| {
        uri::path_of(spelling)
            .replace('\\', "/")
            .trim_end_matches('/')
            .to_lowercase()
    };
    announced != "<unstated>" && normalize(announced) == normalize(root_uri)
}

/// Read `ServerCapabilities` into the flat advertised/not-advertised form.
///
/// Only booleans and `{ … }` provider objects are interpreted; a provider object is
/// treated as advertised, matching how LSP encodes "supported with options".
fn read_capabilities(result: &Value) -> Vec<LspCapability> {
    let Some(map) = result.pointer("/capabilities").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut capabilities: BTreeMap<String, bool> = BTreeMap::new();
    for (name, value) in map {
        let advertised = match value {
            Value::Bool(flag) => *flag,
            Value::Object(_) => true,
            _ => false,
        };
        capabilities.insert(name.clone(), advertised);
    }
    capabilities
        .into_iter()
        .map(|(name, advertised)| LspCapability { name, advertised })
        .collect()
}

/// Read one `publishDiagnostics` notification.
fn read_published_diagnostics(message: &Value) -> Vec<PublishedDiagnostic> {
    let Some(uri) = message.pointer("/params/uri").and_then(Value::as_str) else {
        return Vec::new();
    };
    let Some(entries) = message
        .pointer("/params/diagnostics")
        .and_then(Value::as_array)
    else {
        // An empty diagnostics array is how the server says "this file is clean", so
        // an absent array is recorded as a clean sentinel rather than dropped.
        return vec![PublishedDiagnostic {
            uri: uri.to_owned(),
            severity: None,
            line: None,
            character: None,
            message: String::new(),
        }];
    };
    if entries.is_empty() {
        return vec![PublishedDiagnostic {
            uri: uri.to_owned(),
            severity: None,
            line: None,
            character: None,
            message: String::new(),
        }];
    }
    entries
        .iter()
        .map(|entry| PublishedDiagnostic {
            uri: uri.to_owned(),
            severity: entry.get("severity").and_then(Value::as_i64),
            line: entry.pointer("/range/start/line").and_then(Value::as_u64),
            character: entry
                .pointer("/range/start/character")
                .and_then(Value::as_u64),
            message: entry
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        })
        .collect()
}

/// Read a `documentSymbol` result, flattening one level of children.
fn read_symbols(method: &str, result: &Value) -> Result<Vec<SymbolSummary>, AdapterError> {
    match result {
        Value::Null => Ok(Vec::new()),
        Value::Array(entries) => {
            let mut symbols = Vec::new();
            for entry in entries {
                let name = entry
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                symbols.push(SymbolSummary {
                    name: name.clone(),
                    kind: entry.get("kind").and_then(Value::as_i64),
                    detail: entry
                        .get("detail")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    line: entry
                        .pointer("/range/start/line")
                        .or_else(|| entry.pointer("/location/range/start/line"))
                        .and_then(Value::as_u64),
                    container: None,
                });
                if let Some(children) = entry.get("children").and_then(Value::as_array) {
                    for child in children {
                        symbols.push(SymbolSummary {
                            name: child
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                            kind: child.get("kind").and_then(Value::as_i64),
                            detail: child
                                .get("detail")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            line: child.pointer("/range/start/line").and_then(Value::as_u64),
                            container: Some(name.clone()),
                        });
                    }
                }
            }
            Ok(symbols)
        }
        other => Err(AdapterError::MalformedResponse {
            method: method.to_owned(),
            detail: format!(
                "the result is {}, but documentSymbol returns an array or null",
                kind_of(other)
            ),
        }),
    }
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// The filesystem path behind a `file://` URI, for the `rootPath` field Godot's
/// `initialize` compares against.
fn path_of_uri(uri: &str) -> String {
    uri::path_of(uri)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_unbound_methods_are_refused_before_a_frame_is_sent() {
        for (method, _detail) in UNBOUND_METHODS {
            let error = LspSession::ensure_bound(method).expect_err("unbound");
            assert!(error.is_unsupported(), "{method}: {error}");
            assert_eq!(
                error.error_kind(),
                godot_mcp_domain::ErrorKind::Unsupported,
                "{method}"
            );
        }
        assert!(LspSession::ensure_bound("textDocument/documentSymbol").is_ok());
    }

    #[test]
    fn advertised_capabilities_come_from_the_server_not_from_hope() {
        let result = json!({
            "capabilities": {
                "documentSymbolProvider": true,
                "workspaceSymbolProvider": false,
                "foldingRangeProvider": false,
                "completionProvider": { "resolveProvider": true },
            }
        });
        let capabilities = read_capabilities(&result);
        let advertised = |name: &str| {
            capabilities
                .iter()
                .find(|capability| capability.name == name)
                .map(|capability| capability.advertised)
        };
        assert_eq!(advertised("documentSymbolProvider"), Some(true));
        assert_eq!(advertised("workspaceSymbolProvider"), Some(false));
        assert_eq!(
            advertised("foldingRangeProvider"),
            Some(false),
            "a handler exists but the capability is hardcoded false, so it is not usable"
        );
        assert_eq!(advertised("completionProvider"), Some(true));
    }

    #[test]
    fn an_empty_diagnostics_array_is_a_clean_answer_not_a_missing_one() {
        let clean = read_published_diagnostics(&json!({
            "method": PUBLISH_DIAGNOSTICS_NOTIFICATION,
            "params": { "uri": "file:///a.gd", "diagnostics": [] }
        }));
        assert_eq!(clean.len(), 1);
        assert_eq!(clean[0].uri, "file:///a.gd");
        assert!(clean[0].message.is_empty());
    }

    #[test]
    fn a_diagnostic_carries_its_location() {
        let parsed = read_published_diagnostics(&json!({
            "method": PUBLISH_DIAGNOSTICS_NOTIFICATION,
            "params": {
                "uri": "file:///broken.gd",
                "diagnostics": [{
                    "severity": 1,
                    "range": { "start": { "line": 7, "character": 3 } },
                    "message": "Parse Error: Expected statement",
                }]
            }
        }));
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].severity, Some(1));
        assert_eq!(parsed[0].line, Some(7));
        assert_eq!(parsed[0].character, Some(3));
        assert!(parsed[0].message.contains("Parse Error"));
    }

    #[test]
    fn nested_symbols_are_flattened_with_their_container() {
        let symbols = read_symbols(
            "textDocument/documentSymbol",
            &json!([{
                "name": "Player",
                "kind": 5,
                "range": { "start": { "line": 0 } },
                "children": [{
                    "name": "_ready",
                    "kind": 6,
                    "range": { "start": { "line": 4 } },
                }],
            }]),
        )
        .expect("array result");
        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0].container, None);
        assert_eq!(symbols[1].name, "_ready");
        assert_eq!(symbols[1].container.as_deref(), Some("Player"));
        assert_eq!(symbols[1].line, Some(4));
    }

    #[test]
    fn a_non_array_symbol_result_is_uncertain_not_empty() {
        let error = read_symbols("textDocument/documentSymbol", &json!({"unexpected": true}))
            .expect_err("malformed");
        assert_eq!(error.error_kind(), godot_mcp_domain::ErrorKind::Uncertain);
    }

    #[test]
    fn a_null_symbol_result_is_an_empty_list() {
        assert!(
            read_symbols("textDocument/documentSymbol", &Value::Null)
                .expect("null is legal")
                .is_empty()
        );
    }

    #[test]
    fn the_root_path_field_is_derived_from_the_root_uri() {
        assert_eq!(path_of_uri("file:///projects/demo"), "/projects/demo");
        assert_eq!(path_of_uri("/already/a/path"), "/already/a/path");
        // Godot compares a Windows rootPath with forward slashes and no
        // leading slash before the drive.
        assert_eq!(
            path_of_uri("file:///C%3A/Users/qa/game"),
            "C:/Users/qa/game"
        );
    }

    #[test]
    fn a_workspace_godot_announces_is_compared_in_every_spelling_it_uses() {
        let root = "file:///C%3A/Users/qa/game";
        for announced in [
            "C:/Users/qa/game",
            "c:/users/qa/game/",
            "file:///C:/Users/qa/game",
            "file:///c%3a/Users/qa/game",
            r"C:\Users\qa\game",
        ] {
            assert!(same_workspace(announced, root), "{announced}");
        }
        assert!(!same_workspace("C:/Users/qa/other", root));
        assert!(same_workspace("/projects/demo", "file:///projects/demo"));
        assert!(!same_workspace("<unstated>", "file:///projects/demo"));
    }
}
