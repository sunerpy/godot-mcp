//! The frozen Rust↔GDScript editor-bridge protocol.
//!
//! # The one rule this module exists to enforce
//!
//! > **A loopback port is not an identity. A per-launch secret plus a matching
//! > project is.**
//!
//! Todo 18 established the mirror image of this for the LSP/DAP plane, where Godot
//! listens and this server dials: "something is listening on 127.0.0.1:6005" is
//! compatible with a different editor, a different project, or an unrelated process
//! that took the port. Here the direction is reversed — this server listens and the
//! plugin dials — and so is the risk: *any* local process can dial the bridge port.
//! Loopback narrows the attacker set to "code running on this machine". It does not
//! narrow it to "our plugin".
//!
//! So three facts must be proven before one request is dispatched, and
//! [`handshake`] checks them in that order: knowledge of the 256-bit per-launch
//! secret from a mode-0600 [`rendezvous`] file, agreement on the *same* project by
//! OS object identity and generation, and a compatible protocol MAJOR.
//!
//! | module | responsibility |
//! | --- | --- |
//! | [`ws`] | the minimal RFC 6455 subset: upgrade, text/close/ping/pong, 4 MiB ceiling, masking |
//! | [`digest`] | base64 (RFC 4648) and SHA-1 (RFC 3174), for `Sec-WebSocket-Accept` only |
//! | [`envelope`] | the versioned envelope: ten kinds, one fixed key set, canonical bytes |
//! | [`compat`] | the compatibility matrix and every way a pairing is refused |
//! | [`rendezvous`] | the private file, per-launch secret, platform-safe rotation/refusal, constant-time compare |
//! | [`handshake`] | the authenticated `hello`, nonce replay refusal, classified refusals |
//! | [`listener`] | the loopback-only ephemeral listener and one bounded session |
//! | [`mutation`] | `not_started \| committed \| unresolved`, and the Todo 6 quarantine handoff |
//! | [`harness`] | the pinned headless GDScript codec harness |
//!
//! # What this module never does
//!
//! * never reuses an MCP wire message inside the plugin — the plugin's whole
//!   vocabulary is [`envelope::MessageKind`], and MCP lives in `godot-mcp-server`,
//!   a crate this one cannot import;
//! * never accepts a different project root, `project.godot` object, public project
//!   identity or generation — four separately reported refusals, not one lumped
//!   mismatch;
//! * never binds beyond loopback, and never treats loopback as sufficient on its
//!   own;
//! * never exceeds 4 MiB per frame, and refuses an oversize declaration from the
//!   frame header before allocating the payload;
//! * never assumes a reconnect makes replaying a mutation safe — an `unresolved`
//!   outcome goes to Todo 6's journal and stays there;
//! * never opens a durable store of its own: the quarantine handoff travels through
//!   [`mutation::QuarantineSink`], the same port shape the engine's `LaunchJournal`
//!   uses.
//!
//! # Why WebSocket here, and raw TCP one plane over
//!
//! Godot moved its GDScript language server **off** WebSocket in commit
//! `24b27043f` (2020-02-01), so Todo 18's adapters speak raw TCP `Content-Length`
//! framing. This plane is different: its peer is a GDScript `EditorPlugin` using the
//! engine's built-in RFC 6455 client, where one send call is one message. Speaking
//! WebSocket text frames here means the plugin needs no hand-rolled framing at all.
//! Both planes share the 4 MiB ceiling, taken from the engine's own fixed
//! `4194304`-byte per-peer buffers.

pub mod compat;
pub mod digest;
pub mod envelope;
pub mod handshake;
pub mod harness;
pub mod listener;
pub mod mutation;
pub mod rendezvous;
pub mod ws;

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    fn discover_modules(root: &Path) -> Vec<PathBuf> {
        fn collect(root: &Path, directory: &Path, modules: &mut Vec<PathBuf>) {
            let entries = std::fs::read_dir(directory)
                .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()));
            for entry in entries {
                let entry = entry.unwrap_or_else(|error| {
                    panic!("read an entry under {}: {error}", directory.display())
                });
                let path = entry.path();
                let file_type = entry.file_type().unwrap_or_else(|error| {
                    panic!("read file type for {}: {error}", path.display())
                });
                if file_type.is_dir() {
                    collect(root, &path, modules);
                } else if file_type.is_file()
                    && path.extension() == Some(std::ffi::OsStr::new("rs"))
                {
                    modules.push(
                        path.strip_prefix(root)
                            .unwrap_or_else(|error| {
                                panic!(
                                    "strip protocol root {} from {}: {error}",
                                    root.display(),
                                    path.display()
                                )
                            })
                            .to_path_buf(),
                    );
                }
            }
        }

        let mut modules = Vec::new();
        collect(root, root, &mut modules);
        modules.sort();
        modules
    }

    fn modules() -> Vec<PathBuf> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/protocol");
        let modules = discover_modules(&root);
        assert!(!modules.is_empty(), "src/protocol contains Rust modules");
        modules
    }

    fn source(name: &Path) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/protocol")
            .join(name);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
    }

    /// A module's shipped code: everything before its `cfg(test)` module. What a
    /// test names itself is not what the crate compiles into a release build.
    fn production_source(name: &Path) -> String {
        let text = source(name);
        let marker = format!("#[cfg({})]", "test");
        match text.find(&marker) {
            Some(offset) => text[..offset].to_owned(),
            None => text,
        }
    }

    #[test]
    fn module_discovery_is_recursive_and_non_vacuous() {
        let discovered = modules();
        assert!(
            discovered.len() >= 10,
            "the protocol scanner must cover the complete module plane"
        );
        assert!(discovered.contains(&PathBuf::from("mod.rs")));

        struct TestRoot(PathBuf);
        impl Drop for TestRoot {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        let test_root = TestRoot(std::env::temp_dir().join(format!(
            "godot-mcp-protocol-module-discovery-{}",
            std::process::id()
        )));
        let nested = test_root.0.join("nested");
        std::fs::create_dir_all(&nested).expect("create nested scanner fixture");
        std::fs::write(nested.join("probe.rs"), "//! scanner probe\n")
            .expect("write nested scanner fixture");
        assert_eq!(
            discover_modules(&test_root.0),
            vec![PathBuf::from("nested/probe.rs")],
            "the protocol scanner reaches nested Rust modules"
        );
    }

    /// Forbidden spellings, assembled at runtime so this test's own source text
    /// cannot satisfy the search it performs.
    fn forbidden_mcp_spellings() -> [String; 5] {
        [
            format!("json{}rpc", ""),
            format!("tools/{}call", ""),
            format!("tools/{}list", ""),
            format!("rm{}cp::", ""),
            format!("godot_mcp_{}server", ""),
        ]
    }

    #[test]
    fn no_module_in_this_plane_reuses_an_mcp_wire_message() {
        for name in modules() {
            let text = production_source(&name);
            for spelling in forbidden_mcp_spellings() {
                assert!(
                    !text.contains(&spelling),
                    "src/protocol/{} must not reference the MCP wire type {spelling}",
                    name.display()
                );
            }
        }
    }

    /// The bridge never spawns a process. Every Godot process comes from Todo 14's
    /// launch protocol via Todo 15's runner, inside the containment boundary and
    /// the launch journal.
    #[test]
    fn no_module_in_this_plane_spawns_a_process() {
        let command = format!("Comm{}and", "");
        for name in modules() {
            let text = production_source(&name);
            for spelling in [
                format!("{command}::new"),
                format!("std::process::{command}"),
            ] {
                assert!(
                    !text.contains(&spelling),
                    "src/protocol/{} must not build a process itself ({spelling})",
                    name.display()
                );
            }
        }
    }

    /// Only the listener binds anything, and it binds through the loopback guard.
    #[test]
    fn only_the_listener_binds_a_socket_and_it_checks_loopback() {
        let bind = format!("TcpListener::{}bind", "");
        for name in modules() {
            let text = production_source(&name);
            if name == Path::new("listener.rs") {
                assert!(text.contains(&bind), "the listener is what binds");
                assert!(
                    text.contains("is_loopback"),
                    "the listener checks loopback explicitly"
                );
            } else {
                assert!(
                    !text.contains(&bind),
                    "src/protocol/{} must not bind a listener",
                    name.display()
                );
            }
        }
    }

    /// No module writes its own durable transaction store. The `unresolved`
    /// handoff goes through Todo 6's canonical record and the sink port.
    #[test]
    fn no_module_opens_a_durable_store_of_its_own() {
        let forbidden = [format!("rus{}qlite", ""), format!("Connection::{}open", "")];
        for name in modules() {
            let text = production_source(&name);
            for spelling in &forbidden {
                assert!(
                    !text.contains(spelling),
                    "src/protocol/{} must not open a durable store ({spelling})",
                    name.display()
                );
            }
        }
        assert!(
            production_source(Path::new("mutation.rs")).contains("validate_canonical"),
            "the quarantine handoff validates against Todo 6's canonical schema"
        );
    }

    /// The 4 MiB ceiling is stated once, in the codec; nothing else redefines it.
    #[test]
    fn the_frame_ceiling_is_defined_exactly_once() {
        let declaration = format!("MAX_FRAME_BYTES: usize = 4 * 1024 * {}1024", "");
        let definitions: Vec<PathBuf> = modules()
            .into_iter()
            .filter(|name| production_source(name).contains(&declaration))
            .collect();
        assert_eq!(
            definitions,
            vec![PathBuf::from("ws.rs")],
            "the frame ceiling is defined only by the codec"
        );
    }

    /// Every module in this plane carries a module-level doc header, since the
    /// protocol's reasoning is the deliverable as much as the code is.
    #[test]
    fn every_module_documents_itself() {
        for name in modules() {
            let text = source(&name);
            assert!(
                text.starts_with("//!"),
                "src/protocol/{} opens with a module doc header",
                name.display()
            );
        }
    }
}
