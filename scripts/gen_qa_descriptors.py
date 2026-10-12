#!/usr/bin/env python3
"""One-shot generator for qa/tasks/1.json .. 37.json.

Seeds every descriptor verbatim from each Todo's own prose QA scenarios in the
approved plan. Run once by Todo 1; thereafter each descriptor is owned by its
Todo. This generator is committed for provenance/reproducibility only; the
descriptors themselves are the committed contract.
"""
import io
import json
import os

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
QA = os.path.join(ROOT, "qa", "tasks")
os.makedirs(QA, exist_ok=True)

EV = "<attemptDir>/task-{n}-godot-mcp-server-architecture.{ext}"

# (test-crate, test-file, exact-case, evidence-ext) for the simple two-command
# Todos (happy + failure), transcribed verbatim from each Todo's QA line.
SIMPLE = {
    2:  ("godot-mcp-domain", "lease_ownership", "same_object_alias_converges_to_one_lease", "second_owner_symlink_and_retarget_reject", "txt"),
    3:  ("godot-mcp-domain", "contract_snapshots", "profile_algebra_and_dto_roundtrip", "provenance_invariant_rejects_false_exact", "json"),
    4:  ("godot-mcp-server", "evidence_redaction", "stdio_stderr_separation_smoke", "secret_and_source_logging_is_redacted", "txt"),
    5:  ("godot-mcp-server", "kernel_rmcp_parity", "in_memory_client_initialize_list_call_read_cancel", "malformed_params_returns_invalid_params", "json"),
    6:  ("godot-mcp-server", "journal_foundation", "journal_foundation_happy_path", "journal_foundation_failure_paths", "txt"),
    7:  ("godot-mcp-analysis", "gdscript_parser", "gdscript_parser_happy_path", "gdscript_parser_failure_paths", "json"),
    8:  ("godot-mcp-analysis", "project_identity", "project_identity_happy_path", "project_identity_failure_paths", "json"),
    9:  ("godot-mcp-analysis", "scene_resource_model", "scene_resource_model_happy_path", "scene_resource_model_failure_paths", "json"),
    10: ("godot-mcp-analysis", "semantic_graph", "semantic_graph_happy_path", "semantic_graph_failure_paths", "json"),
    11: ("godot-mcp-analysis", "index_persistence", "index_persistence_happy_path", "index_persistence_failure_paths", "json"),
    12: ("godot-mcp-server", "core_outcomes", "core_outcomes_happy_path", "core_outcomes_failure_paths", "json"),
    13: ("godot-mcp-engine", "godot_discovery", "godot_discovery_happy_path", "godot_discovery_failure_paths", "json"),
    14: ("godot-mcp-engine", "process_launch_protocol", "process_launch_protocol_happy_path", "process_launch_protocol_failure_paths", "txt"),
    15: ("godot-mcp-engine", "validation_import_test", "validation_import_test_happy_path", "validation_import_test_failure_paths", "json"),
    16: ("godot-mcp-engine", "disposable_execution", "disposable_execution_happy_path", "disposable_execution_failure_paths", "txt"),
    17: ("godot-mcp-engine", "canonical_writes", "canonical_writes_happy_path", "canonical_writes_failure_paths", "json"),
    18: ("godot-mcp-engine", "lsp_dap_adapters", "lsp_dap_adapters_happy_path", "lsp_dap_adapters_failure_paths", "json"),
    19: ("godot-mcp-bridge", "bridge_protocol", "bridge_protocol_happy_path", "bridge_protocol_failure_paths", "json"),
    20: ("godot-mcp-bridge", "editor_plugin_harness", "editor_plugin_harness_happy_path", "editor_plugin_harness_failure_paths", "txt"),
    21: ("godot-mcp-bridge", "type_catalog", "type_catalog_happy_path", "type_catalog_failure_paths", "json"),
    22: ("godot-mcp-bridge", "editor_inspection", "editor_inspection_happy_path", "editor_inspection_failure_paths", "json"),
    23: ("godot-mcp-bridge", "undoable_mutations", "undoable_mutations_happy_path", "undoable_mutations_failure_paths", "json"),
    24: ("godot-mcp-bridge", "screenshots_recording", "screenshots_recording_happy_path", "screenshots_recording_failure_paths", "json"),
    25: ("godot-mcp-bridge", "runtime_inspection", "runtime_inspection_happy_path", "runtime_inspection_failure_paths", "json"),
    26: ("godot-mcp-server", "capability_policy", "capability_policy_happy_path", "capability_policy_failure_paths", "json"),
    27: ("godot-mcp-server", "scheduler_idempotency", "scheduler_idempotency_happy_path", "scheduler_idempotency_failure_paths", "json"),
    28: ("godot-mcp-server", "http_transport", "http_transport_happy_path", "http_transport_failure_paths", "json"),
    29: ("godot-mcp-server", "oidc_resource_server", "oidc_resource_server_happy_path", "oidc_resource_server_failure_paths", "json"),
    30: ("godot-mcp-server", "http_session_binding", "http_session_binding_happy_path", "http_session_binding_failure_paths", "json"),
    31: ("godot-mcp-server", "audit_limits", "audit_limits_happy_path", "audit_limits_failure_paths", "json"),
    32: ("godot-mcp-server", "outcome_router", "outcome_router_happy_path", "outcome_router_failure_paths", "json"),
    33: ("godot-mcp-server", "health_diagnostics", "health_diagnostics_happy_path", "health_diagnostics_failure_paths", "json"),
    36: ("godot-mcp-cli", "docs_link_check", "docs_link_check_happy_path", "docs_link_check_failure_paths", "txt"),
    37: ("godot-mcp-server", "e2e_qualification", "e2e_qualification_happy_path", "e2e_qualification_failure_paths", "json"),
}

# Assertion text per simple Todo (transcribed from the plan's QA line), keyed by
# (happy, failure). Kept concise but specific; the runner checks the exit code.
ASSERT = {
    2: ("one process holds the object-identity lease and performs a descriptor-relative in-root read/CAS write through a lexical/case or junction/bind-mount alias",
        "the production code path returns conflict/permission_denied for second alias owner, symlink swap, same-object symlink component, root/project replacement, junction/bind retarget, outside path, stale preimage and direct non-loopback config, each failing before cache/socket/file effects"),
    3: ("serializes/deserializes every DTO and validates full = core union editor union runtime minus unsafe_script",
        "the production invariant check rejects a fixture marking an inferred dynamic reference as exact"),
    4: ("captured stdout contains only protocol frames while stderr carries structured logs",
        "a deliberate token/source logging fixture is replaced by [REDACTED], failing if raw bytes occur"),
    5: ("drives an in-memory rmcp client through initialize/list/call/read/cancel",
        "malformed schema input returns deterministic MCP invalid_input error data without a panic"),
    6: ("assert_cmd drives a core request and a test permit commits/reopens each mock effect through the durable store",
        "the production code path rejects fault injection before/after every phase fsync, partial staged bytes, direct effect invocation, illegal transition, quarantine bypass and release use of test hooks, without an unjournaled effect or guessed outcome"),
    7: ("parses classes/functions/signals/preload/calls",
        "a syntactically damaged fixture preserves recoverable symbols and emits parser diagnostics"),
    8: ("a nested script reached through a symlink-free lexical/case or supported same-object junction/bind alias resolves one fixture project identity",
        "even a same-object path containing a symlink component, a retargeted junction/bind, or an outside-root path fails with permission_denied before lease/file read"),
    9: ("a nested scene with ext/sub resources and signal connections yields the expected graph",
        "a malformed/unknown resource remains queryable with uncertainty and exact location"),
    10: ("traces scene->script->autoload/signal/resource path",
         "an ambiguous dynamic target returns all candidates plus uncertainty and never invents a single call path"),
    11: ("editing one script changes only the affected graph slice/hash",
         "watcher overflow/schema mismatch forces a safe rescan rather than stale success"),
    12: ("asking impact of a script retrieves a bounded provenance-rich graph",
         "requesting an unsupported binary resource returns unsupported with remediation, not empty success"),
    13: ("a fake 4.7.1 editor advertises the expected commands",
         "with a template that silently ignores the editor flag, the probe detects the missing capability and returns unsupported"),
    14: ("a strong-contained helper remains non-executing until its complete identity receipt is durable, then exits with matching hashes",
         "error or SIGKILL at every launch phase leaves no unreceipted executing child, failed release terminates/closes the boundary, and restart cleans or blocks on every survivor before mutation resumes"),
    15: ("a headless fixture test reports exact counts",
         "a script syntax error and a fake misleading 'started' output cannot be classified as success"),
    16: ("validation on a copied fixture leaves the source hash unchanged",
         "a symlink/secret-copy attempt is rejected and no outside marker appears"),
    17: ("creates scene/resource via a journaled engine transaction (with its linked process_launch transaction independently showing clean exit 0/no signal/empty boundary) and reopens it",
         ".scn mutation, duplicate/conflicting UID, interrupted non-atomic replacement, and matching file post-hash with linked proof missing/mismatched/live/signaled/nonzero/non-empty-boundary yield no unproven success and block the next mutation pending repair"),
    18: ("diagnostics and DAP stack come from the exact owned fixture-editor receipt",
         "a spoofed/stale loopback endpoint, >4 MiB/bad header/9th connection is rejected without effect dispatch or server failure"),
    19: ("a Rust fake server and Godot codec use the rendezvous secret then exchange hello/ping/read/cancel with identical golden JSON",
         "missing/wrong/replayed secret, stale project identity, major-version mismatch, 16 MiB frame and duplicate mutation ID fail with stable errors"),
    20: ("the plugin connects, reports capabilities, processes a read and cleanly disables",
         "the Rust endpoint disappears/reappears and the plugin reconnects without replaying queued committed/unresolved writes"),
    21: ("sets and reads representative 2D/3D/physics/material properties using tagged values",
         "invalid Vector3, unknown enum bit, stale object handle and cyclic/deep container are rejected without editor mutation"),
    22: ("inspects a scene with grouped nodes and signal connections",
         "requesting an outside-root resource, a deleted node handle or excessive tree depth is bounded and leaves editor state unchanged"),
    23: ("create/reparent/rename/configure/connect/save then undo/redo round-trip through the early gateway",
         "invalid type, name conflict, open-resource conflict, cancellation-before-start and phase interruption cause zero unjournaled mutation, with quarantine blocking subsequent writes where state cannot be proven"),
    24: ("captures editor/game PNG, records then reads valid ranges through opaque resources over stdio, producing parity vectors Todo 30/37 will later replay over authenticated HTTP",
         "a guessed ID, stale generation, other principal/session, traversal, concurrent recording, invalid range/FPS/resolution, no renderer, timeout, crash and byte cap produce stable denial/cleanup with public evidence containing metadata/hash only"),
    25: ("configures a representative 3D scene, runs it, inspects properties/signals/UI, simulates key/mouse/action/sequence and captures the result",
         "server SIGKILL at every transaction boundary, heartbeat loss, stopped/paused/crashed game, invalid input, autoload conflict, replay and unavailable renderer release input autonomously and, on restart, restore proven original settings or quarantine with bounded diagnostics"),
    26: ("an expected profile operation receives a private production permit and executes against an existing SafeRoot handle",
         "forged/missing/stale permit, core-profile mutation, changed project generation, quarantine and unresolved survivor return permission_denied/conflict/unavailable before any side effect"),
    27: ("concurrent reads overlap, one journaled write executes exclusively through the existing Todo 6 store, and its exact same-context retry receives the recorded result without replay",
         "queue starvation, cancellation race, PID survivor and idempotency reuse across every tuple dimension yield no duplicate effect, guessed outcome or cross-context disclosure, while only a proof-producing operator repair unblocks writes"),
    28: ("an ephemeral trusted_local client reads the ACL-protected launch secret and initialize/list/call/stream/cancel match stdio",
         "missing/stale/wrong secret, direct non-loopback bind, proxied dispatch before Todo 29, forwarded headers, HTTP external URL, invalid Origin/Host, oversize body and query token are rejected before kernel dispatch"),
    29: ("an authenticated same-host proxy hop carries the configured principal's token with exact resource/scopes and calls allowed tools before/after overlapping-key rotation using an allowlisted algorithm and unambiguous kid",
         "loopback peer without hop credential, second subject, another audience, HTTP issuer, mixed/private answer, redirect to metadata IP, environment proxy, public-to-private lookup/connect rebinding, alg:none, HMAC substitution, non-allowlisted algorithm, key/algorithm confusion, duplicate/missing kid, token-controlled jku/x5u/x5c/jwk, and an unsupported crit header entry are rejected with no SSRF or kernel dispatch"),
    30: ("an authenticated subject resumes its own SSE/session/task, reads its opaque artifact range, and its authenticated HTTP screenshot/recording read matches the stdio vector",
         "missing proxy-hop identity or second subject reuses an ID and receives 404/403 per the non-enumerating contract with zero task/artifact disclosure or dispatch"),
    31: ("an allowed mutation creates a redacted policy/effect/outcome event",
         "raw token/source/media bytes never appear, rate/concurrency limit blocks before dispatch, full registry/schema enumeration and a negative MCP call prove arbitrary execution is absent"),
    32: ("one client workflow analyzes, validates, edits via UndoRedo, runs, inspects and captures using the correct planes",
         "an unsaved editor conflict refuses engine/text overwrite and returns an actionable conflict instead of fallback"),
    33: ("a core server reports ready with the editor absent and a remediation hint",
         "an unresolved survivor, quarantined transaction, pending autoload restoration, wrong Godot/plugin protocol and JWKS outage report the precise blocked/degraded capability with no sensitive data or raw artifact path"),
    36: ("a scripted clean temp environment follows docs to install/configure/stdio smoke without interpretation",
         "the docs checker detects a stale tool/config/protocol name and the security lint rejects 'sandboxed'/unauthenticated-public examples"),
    37: ("invokes the exact producer command against a frozen fixture candidate, asserting final-gate-first staging, the five qualification outputs in order, complete evidence/parity, producer provenance, and all six binaries' documented success",
         "injecting failure before/within each producer stage and each publication rename, precreating each F1-F5 output, mutating each identity/review/tool/runner input, and supplying non-approving/reused receipts each exits nonzero with all seven final producer output paths absent and never synthesizes or modifies a review receipt"),
}


def cargo_test(crate, test, case):
    return f"cargo test -p {crate} --test {test} -- --exact {case}"


def write_descriptor(n, obj):
    path = os.path.join(QA, f"{n}.json")
    with io.open(path, "w", encoding="utf-8") as f:
        json.dump(obj, f, indent=2, ensure_ascii=False)
        f.write("\n")


# ---- Task 1 ----------------------------------------------------------------
task1 = {
    "task": 1,
    "wrapper": "simple",
    "happy": [
        {"command": "cargo +1.96.0 metadata --format-version 1", "expected_exit": 0,
         "assertions": ["dependency graph is parseable and acyclic",
                        "lists exactly the intended workspace members",
                        "lists all eight qualification binaries including produce-qualification"]},
        {"command": cargo_test("godot-mcp-domain", "architecture_boundaries", "no_transport_ui_imports"), "expected_exit": 0,
         "assertions": ["domain/analysis/engine crates import no transport/UI layer"]},
        {"command": cargo_test("godot-mcp-cli", "tooling_lock", "tooling_lock_populated"), "expected_exit": 0,
         "assertions": ["tooling.lock.json records exact toolchain/cargo-deny/cargo-cyclonedx version/URL/checksum/executable-SHA"]},
        {"command": cargo_test("godot-mcp-cli", "release_graph_audit", "no_shipping_gdext_artifact"), "expected_exit": 0,
         "assertions": ["no shipping crate godot / .gdextension in the manifest graph"]},
        {"command": cargo_test("godot-mcp-cli", "qa_descriptors", "all_37_descriptors_present_and_valid"), "expected_exit": 0,
         "assertions": ["every qa/tasks/1.json through qa/tasks/37.json exists, schema-validates and has non-empty command/assertion fields",
                        "qa/tasks/34.json contains its fixed IDs and full commands with no angle-bracket transaction token or shorthand"]},
        {"command": "cargo +1.96.0 run --locked -p godot-mcp-cli --features test-hooks --bin qa-audit -- --fixture ordinary-dependency-library", "expected_exit": 0,
         "assertions": ["an ordinary native .so/.dylib/.dll outside the shipping addon/bridge tree is accepted (native-bridge detection is scoped and produces no false positive)"]},
    ],
    "failure": [
        {"command": "cargo +1.96.0 run --locked -p godot-mcp-cli --features test-hooks --bin qa-audit -- --fixture domain-imports-server", "expected_exit": 1,
         "assertions": ["a fixture adding godot-mcp-server as a domain dependency fails no_transport_ui_imports"]},
        {"command": "cargo +1.96.0 run --locked -p godot-mcp-cli --features test-hooks --bin qa-audit -- --fixture shipping-godot", "expected_exit": 1,
         "assertions": ["a fixture pinning production godot = 0.5.4 fails no_shipping_gdext_artifact with the exact forbidden edge named"]},
        {"command": "cargo +1.96.0 run --locked -p godot-mcp-cli --features test-hooks --bin qa-audit -- --fixture shipping-gdext-bridge", "expected_exit": 1,
         "assertions": ["a fixture declaring a shipping gdext-bridge dependency fails no_shipping_gdext_artifact by name"]},
        {"command": "cargo +1.96.0 run --locked -p godot-mcp-cli --features test-hooks --bin qa-audit -- --fixture shipping-gdextension", "expected_exit": 1,
         "assertions": ["a fixture shipping a .gdextension descriptor fails no_shipping_gdext_artifact by name"]},
        {"command": "cargo +1.96.0 run --locked -p godot-mcp-cli --features test-hooks --bin qa-audit -- --fixture shipping-native-bridge-lib", "expected_exit": 1,
         "assertions": ["a fixture shipping a native bridge library in the addon tree fails no_shipping_gdext_artifact by name at its addon location"]},
        {"command": "cargo +1.96.0 run --locked -p godot-mcp-cli --features test-hooks --bin qa-audit -- --fixture unpopulated-tooling-lock", "expected_exit": 1,
         "assertions": ["a fixture with an unpopulated or checksum-mismatched tooling.lock.json fails tooling_lock_populated"]},
        {"command": "cargo +1.96.0 run --locked -p godot-mcp-cli --features test-hooks --bin qa-audit -- --fixture missing-descriptor", "expected_exit": 1,
         "assertions": ["a fixture with a deleted/emptied qa/tasks/17.json fails all_37_descriptors_present_and_valid by name"]},
        {"command": "cargo +1.96.0 run --locked -p godot-mcp-cli --features test-hooks --bin qa-audit -- --fixture task34-angle-bracket", "expected_exit": 1,
         "assertions": ["a fixture with a Todo 34 angle-bracket transaction token fails all_37_descriptors_present_and_valid by name"]},
    ],
    "evidence": EV.format(n=1, ext="txt"),
}
write_descriptor(1, task1)

# ---- simple Todos ----------------------------------------------------------
for n, (crate, test, hcase, fcase, ext) in SIMPLE.items():
    ha, fa = ASSERT[n]
    obj = {
        "task": n,
        "wrapper": "simple",
        "happy": [
            {"command": cargo_test(crate, test, hcase), "expected_exit": 0, "assertions": [ha]},
        ],
        "failure": [
            {"command": cargo_test(crate, test, fcase), "expected_exit": 0, "assertions": [fa]},
        ],
        "evidence": EV.format(n=n, ext=ext),
    }
    # Task 35 happy additionally invokes the release-gates harness (non-qualifying).
    if n == 35:
        pass
    write_descriptor(n, obj)

# ---- Task 35 (extra harness invocation in happy, plus fixture-drift failures) ----
harness_cmd = ("RUSTUP_TOOLCHAIN=1.96.0 make release-gates-harness "
               "CANDIDATE_WORKTREE=tests/fixtures/release-candidate "
               "SOURCE_COMMIT=1111111111111111111111111111111111111111 "
               "SOURCE_TREE=2222222222222222222222222222222222222222 "
               "ARCHIVES_DIR=tests/fixtures/release-archives "
               "OUTPUT=target/qa-task-35/release-gates.json "
               "OUTPUT_SHA=target/qa-task-35/release-gates.sha256 QUALIFYING=0")
task35 = {
    "task": 35,
    "wrapper": "simple",
    "happy": [
        {"command": cargo_test("godot-mcp-cli", "ci_matrix_lock", "ci_matrix_lock_happy_path"), "expected_exit": 0,
         "assertions": ["exact lock-driven jobs plus a clean release-graph audit"]},
        {"command": harness_cmd, "expected_exit": 0,
         "assertions": ["produces schema-valid non-qualifying fixture release-gates output only"]},
    ],
    "failure": [
        {"command": cargo_test("godot-mcp-cli", "ci_matrix_lock", "ci_matrix_lock_failure_paths"), "expected_exit": 0,
         "assertions": ["alias/URL/hash/platform/gdext drift, missing/skipped/unavailable/nonzero required checks, and direct QUALIFYING=1 invocation outside Todo 37's producer context are all rejected with no canonical final gate output"]},
    ],
    "evidence": EV.format(n=35, ext="txt"),
}
write_descriptor(35, task35)

# ---- Task 34 (fixture-lifecycle, packaged repair CLI, fixed IDs verbatim) ----
SETUP = ("cargo +1.96.0 run --locked -p godot-mcp-cli --features test-hooks "
         "--bin qa-fixture-34 -- setup --scenario task34")
CLEANUP = ("cargo +1.96.0 run --locked -p godot-mcp-cli --features test-hooks "
           "--bin qa-fixture-34 -- cleanup --scenario task34")


# Task 34's repair commands NEVER invoke an ambient-PATH `godot-mcp`. Every
# repair command resolves the frozen host-package manifest, whose recorded
# executable SHA-256 the `qa-host-invoke` resolver verifies before invoking the
# exact packaged binary. The `godot-mcp transaction ...` arguments are forwarded
# verbatim after `--`.
HOST_PACKAGE_MANIFEST = "target/qa-task-34/host-package/manifest.json"
RESOLVER = ("cargo +1.96.0 run --locked -p godot-mcp-cli --features test-hooks "
            "--bin qa-host-invoke -- --manifest " + HOST_PACKAGE_MANIFEST + " --")


def repair(txid, op, proof):
    return (f"{RESOLVER} transaction repair --project tests/fixtures/quarantined "
            f"--transaction-id {txid} --operation {op} "
            f"--proof-file tests/fixtures/repair/{proof}")


task34 = {
    "task": 34,
    "wrapper": "fixture-lifecycle",
    "fixture_lifecycle": {"setup": SETUP, "cleanup": CLEANUP},
    "happy": [
        {"command": cargo_test("godot-mcp-cli", "package_and_repair", "package_and_repair_happy_path"), "expected_exit": 0,
         "assertions": ["addon installation plus stdio smoke from the extracted archive and a clean release-graph audit"]},
        {"command": repair("34000000-0000-4000-8000-000000000101", "compensate", "file-pre-state-match.json"), "expected_exit": 0,
         "assertions": ["observes compensated"]},
        {"command": repair("34000000-0000-4000-8000-000000000102", "acknowledge-safe-state", "file-post-state-match.json"), "expected_exit": 0,
         "assertions": ["observes committed"]},
        {"command": repair("34000000-0000-4000-8000-000000000201", "terminate", "process-pre-state-match.json"), "expected_exit": 0,
         "assertions": ["kills the proven still-contained survivor and observes compensated"]},
        {"command": repair("34000000-0000-4000-8000-000000000202", "acknowledge-safe-state", "process-post-state-match.json"), "expected_exit": 0,
         "assertions": ["observes committed"]},
        {"command": repair("34000000-0000-4000-8000-000000000301", "compensate", "editor-pre-state-match.json"), "expected_exit": 0,
         "assertions": ["observes compensated"]},
        {"command": repair("34000000-0000-4000-8000-000000000302", "acknowledge-safe-state", "editor-post-state-match.json"), "expected_exit": 0,
         "assertions": ["observes committed"]},
    ],
    "failure": [
        {"command": cargo_test("godot-mcp-cli", "package_and_repair", "package_and_repair_failure_paths"), "expected_exit": 0,
         "assertions": ["production rejection for preexisting conflicting addon/public HTTP config with no partial change, forbidden godot/.gdextension release-graph content, altered file proof, dead process with unprovable release history, and unreachable editor bridge"]},
        {"command": repair("34000000-0000-4000-8000-000000001001", "acknowledge-safe-state", "composite-linked-proof-missing.json"), "expected_exit": 1,
         "assertions": ["leaves transaction 34000000-0000-4000-8000-000000001001 quarantined because the linked process proof is missing"]},
        {"command": repair("34000000-0000-4000-8000-000000001002", "acknowledge-safe-state", "composite-linked-proof-wrong-transaction.json"), "expected_exit": 1,
         "assertions": ["leaves transaction 34000000-0000-4000-8000-000000001002 quarantined because the linked process proof names another transaction"]},
        {"command": repair("34000000-0000-4000-8000-000000001003", "acknowledge-safe-state", "composite-linked-process-live.json"), "expected_exit": 1,
         "assertions": ["leaves transaction 34000000-0000-4000-8000-000000001003 quarantined because the linked process is still live"]},
        {"command": repair("34000000-0000-4000-8000-000000001004", "acknowledge-safe-state", "composite-linked-process-signaled.json"), "expected_exit": 1,
         "assertions": ["leaves transaction 34000000-0000-4000-8000-000000001004 quarantined because the linked process is signaled"]},
        {"command": repair("34000000-0000-4000-8000-000000001005", "acknowledge-safe-state", "composite-linked-process-nonzero-exit.json"), "expected_exit": 1,
         "assertions": ["leaves transaction 34000000-0000-4000-8000-000000001005 quarantined because the linked process exits nonzero"]},
        {"command": repair("34000000-0000-4000-8000-000000001006", "acknowledge-safe-state", "composite-linked-boundary-nonempty.json"), "expected_exit": 1,
         "assertions": ["leaves transaction 34000000-0000-4000-8000-000000001006 quarantined because the linked process reports a non-empty descendant/boundary set"]},
    ],
    "evidence": EV.format(n=34, ext="txt"),
}
write_descriptor(34, task34)

print("wrote 37 descriptors to", QA)
