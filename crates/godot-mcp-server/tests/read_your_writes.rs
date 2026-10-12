//! M1 acceptance: a committed mutation is readable in the revision it returns.
//!
//! The read-your-writes barrier (plan section 5.4) is exercised through the
//! production assembly: a file effect routed through the outcome router commits
//! durably, the workspace coordinator observes the exact touched path, a new
//! coherent revision is published, and the route report carries that revision
//! before it returns — without waiting for any filesystem event.

mod support;

use godot_mcp_server::router::{
    EnginePresence, PlaneObservation, ProtocolFidelity, RuntimePresence, WorkspaceFreshness,
};
use godot_mcp_server::test_support::OutcomeRouterHarness;
use serde_json::{Value, json};
use support::ProjectTree;

fn shipped(harness: &OutcomeRouterHarness) -> PlaneObservation {
    PlaneObservation::new(
        harness.router().plane_availability(),
        EnginePresence::HeadlessOnly,
        RuntimePresence::NoInstance,
        ProtocolFidelity::Full,
        WorkspaceFreshness::Fresh,
    )
}

#[tokio::test]
async fn a_committed_write_is_in_the_revision_its_route_returns() {
    let tree = ProjectTree::new("m1-ryw");
    std::fs::create_dir_all(tree.root.join("notes")).expect("notes directory");
    let harness = OutcomeRouterHarness::open(&tree.root).expect("production runtime");
    let before: Value = harness
        .call_tool("project.info.get", json!({}))
        .await
        .expect("project.info.get");
    let before_revision = before["workspace"]["revision"]
        .as_u64()
        .expect("answers carry the workspace revision");

    let transaction = harness.next_transaction_id("ryw", 1_700_000_000_000);
    let report = harness
        .router()
        .route(&harness.request(
            "resource.update",
            "idem-ryw-1",
            Some(&transaction),
            "notes/written.txt",
            shipped(&harness),
        ))
        .await
        .expect("a file effect through the production router commits");
    let workspace = report
        .workspace()
        .expect("a committed mutation must carry the revision that contains it");
    let revision = workspace["revision"].as_u64().expect("revision");
    assert!(
        revision > before_revision,
        "the barrier publishes a new revision: {workspace}"
    );
    assert_eq!(workspace["coherent"], json!(true), "{workspace}");

    // The very next read sees the committed file at that revision or later,
    // with no filesystem-event timing involved.
    let discovered: Value = harness
        .call_tool("project.discover", json!({}))
        .await
        .expect("project.discover");
    assert!(
        discovered.to_string().contains("notes/written.txt"),
        "the committed file must be immediately readable: {discovered}"
    );
    assert!(
        discovered["workspace"]["revision"]
            .as_u64()
            .expect("revision")
            >= revision
    );
}
