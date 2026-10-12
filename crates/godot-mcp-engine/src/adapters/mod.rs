//! Bounded LSP/DAP adapters and the engine outcome tools.
//!
//! # The one rule this module exists to enforce
//!
//! > **A loopback listener is not an endpoint. A server-owned launch receipt is.**
//!
//! Godot's GDScript language server binds `127.0.0.1:6005` and its debug adapter
//! binds `127.0.0.1:6006` (hardcoded to loopback, no host setting). Both are
//! `EditorPlugin`s, so they exist only inside a running editor — an export template
//! has no language server compiled in at all. That makes "something is listening on
//! 6005" worthless as identity: it could be another editor, another project,
//! another user's session, or an unrelated process that squatted the port after the
//! editor this server launched exited.
//!
//! So an endpoint is usable here only when it comes out of a launch **this** server
//! performed through Todo 14's protocol, and only while the whole tuple still holds:
//! PID, process start time, binary hash, project root URI, endpoint, and project
//! generation. [`receipt`] is that check, and it is re-run after every reconnect,
//! because a reconnect to the same port is exactly the moment PID reuse becomes
//! indistinguishable from continuity.
//!
//! | module | responsibility |
//! | --- | --- |
//! | [`codec`] | the raw-TCP `Content-Length` frame codec, 4 MiB ceiling, fragmentation/coalescing |
//! | [`channel`] | the bounded framed byte channel, over a deterministic pair or a loopback socket |
//! | [`receipt`] | what makes an endpoint owned, and every way ownership fails |
//! | [`admission`] | the 8-client ceiling, refused without dispatching an effect |
//! | [`lsp`] | the language-server adapter: initialize, Godot notifications, diagnostics, symbols |
//! | [`dap`] | the debug-adapter surface, and honest `unavailable` for what Godot omits |
//! | [`connect`] | the one guarded path: ownership, then admission, then permit, then dial |
//! | [`tools`] | validate/test/import/run/stop/UID, every one through the gateway port |
//! | [`error`] | the closed failure set, mapped to Todo 3's error kinds |
//! | [`uri`] | `file://` URIs and project paths as Godot spells and reads them |
//!
//! # What this module never does
//!
//! * never treats either protocol as WebSocket — Godot moved off WebSocket in
//!   `24b27043f` (2020-02-01) and all of 4.x is raw TCP;
//! * never trusts a listener it did not launch;
//! * never exceeds 4 MiB per frame or 8 concurrent clients;
//! * never reaches an effect except through an [`EffectGate`](tools::EffectGate)
//!   permit, which the server layer backs with Todo 6's `EffectGateway`;
//! * never claims `@tool` breakpoints — Godot's own documentation states the
//!   debugger and breakpoints "cannot be used directly with tool scripts", and
//!   structurally cannot: every DAP handler drives the *remote* debugger session to
//!   a spawned game, while `@tool` code runs in the editor process that *is* the
//!   debugger;
//! * never reports an unimplemented method as a success — Godot's DAP dispatcher
//!   silently drops an unknown command with no response at all, so a caller waiting
//!   for one would hang; [`dap`] refuses those up front as `unsupported`.

pub mod admission;
pub mod channel;
pub mod codec;
pub mod connect;
pub mod dap;
pub mod error;
pub mod lsp;
pub mod receipt;
pub mod tools;
pub mod uri;

pub use admission::{AdmissionError, ClientAdmission, ClientTicket, MAX_CLIENTS};
pub use channel::{DuplexChannel, FramedChannel, TcpChannel};
pub use codec::{
    CodecError, FrameHeader, FrameReader, MAX_HEADER_BYTES, MAX_PAYLOAD_BYTES, encode_frame,
    parse_header, write_frame,
};
pub use connect::{ConnectedEndpoint, EndpointRegistry, exchange_request, exchange_tool_name};
pub use dap::{DapAdapter, DapCapability, DapSession, StackFrameSummary, VariableSummary};
pub use error::AdapterError;
pub use lsp::{LspAdapter, LspCapability, LspSession, PublishedDiagnostic, SymbolSummary};
pub use receipt::{
    EditorEndpoint, EndpointKind, EndpointReceipt, ObservedEndpoint, OwnedEndpoint,
    OwnershipFailure,
};
pub use tools::{
    EffectBoundary, EffectGate, EffectRequest, EngineTool, EngineToolOutcome, EngineTools,
    StopRequest, SupervisedResult, ToolPermit,
};

#[cfg(test)]
mod tests {
    const MODULES: [&str; 10] = [
        "mod.rs",
        "admission.rs",
        "channel.rs",
        "codec.rs",
        "connect.rs",
        "dap.rs",
        "error.rs",
        "lsp.rs",
        "receipt.rs",
        "tools.rs",
    ];

    fn source(name: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/adapters")
            .join(name);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
    }

    /// A module's shipped code: everything before its `cfg(test)` module. What a
    /// test names itself is not what the crate compiles into a release build.
    fn production_source(name: &str) -> String {
        let text = source(name);
        let marker = format!("#[cfg({})]", "test");
        match text.find(&marker) {
            Some(offset) => text[..offset].to_owned(),
            None => text,
        }
    }

    /// Forbidden spellings, assembled at runtime so this test's own source text
    /// cannot satisfy the search it performs.
    fn forbidden_process_spellings() -> [String; 2] {
        let command = format!("Comm{}", "and");
        [
            format!("{command}::new"),
            format!("std::process::{command}"),
        ]
    }

    #[test]
    fn no_module_in_this_plane_spawns_a_process_itself() {
        // Every Godot process comes from Todo 14's launch protocol via Todo 15's
        // runner. A process built here would live outside the containment boundary
        // and outside the launch journal.
        for name in MODULES {
            let text = source(name);
            for spelling in forbidden_process_spellings() {
                assert!(
                    !text.contains(&spelling),
                    "src/adapters/{name} must not build a process itself ({spelling})"
                );
            }
        }
    }

    #[test]
    fn no_module_treats_either_protocol_as_websocket() {
        // Godot 3.1/3.2.0 hosted the language server over WebSocket; commit
        // 24b27043f replaced it with raw TCP and all of 4.x is raw TCP. A WebSocket
        // handshake at these ports would never parse.
        let upper = format!("{}{}", "Web", "Socket");
        let lower = upper.to_ascii_lowercase();
        let forbidden = [
            upper.clone(),
            lower,
            format!("Sec-{upper}-Key"),
            format!("ws{}//", ":"),
        ];
        for name in MODULES {
            let text = production_source(name);
            for spelling in &forbidden {
                // The prose in this module names the migration it refuses, so the
                // spelling is allowed only inside a comment line.
                for (number, line) in text.lines().enumerate() {
                    let trimmed = line.trim_start();
                    let is_comment = trimmed.starts_with("//");
                    assert!(
                        !line.contains(spelling.as_str()) || is_comment,
                        "src/adapters/{name}:{} must not implement {spelling}",
                        number + 1
                    );
                }
            }
        }
    }

    #[test]
    fn the_engine_limits_are_the_engines_own_numbers() {
        assert_eq!(
            super::MAX_PAYLOAD_BYTES,
            4_194_304,
            "LSP_MAX_BUFFER_SIZE / DAP_MAX_BUFFER_SIZE are both 4194304"
        );
        assert_eq!(
            super::MAX_CLIENTS,
            8,
            "LSP_MAX_CLIENTS / DAP_MAX_CLIENTS are both 8"
        );
    }
}
