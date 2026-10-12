//! The `test-hooks`-gated Todo 32 outcome-router harness.
//!
//! Unlike the earlier harnesses, this one does **not** assemble its own wiring. It
//! opens the production [`ProjectRuntime`] and hands the acceptance suite the
//! router that runtime built. That is the whole point: the recorded Todo 27 gap was
//! that a harness bound the durable machinery while the shipped assembly did not,
//! so a harness that wired its own router would be unable to detect a regression of
//! exactly that shape.
//!
//! One thing is injected: the launch survivor probe. The production stdio lane
//! attaches no process boundary and therefore binds
//! [`NoLaunchPlane`](crate::policy::NoLaunchPlane), which can never report a
//! survivor — so the write-blocking behaviour for an unresolved survivor would be
//! untestable. The injection point is a parameter of the *same* assembly body
//! `ProjectRuntime::open` calls, and `router_assembly_has_exactly_one_site` asserts
//! there is only one `OutcomeRouter::assemble` call in the crate, so the injected
//! probe cannot be routed through a different assembly.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use godot_mcp_bridge::protocol::envelope::OperationClass;
use godot_mcp_bridge::protocol::mutation::{MutationLedger, MutationStatus};
use godot_mcp_domain::CapabilityProfile;
use godot_mcp_domain::config::Limits;
use godot_mcp_domain::contracts::PrincipalId;
use godot_mcp_engine::supervision::journal::{
    OperationFamily, ProjectIdentityClaim, TransactionEnvelope, TransportMode,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::HarnessError;
use crate::idempotency::MutationIdentity;
use crate::journal::Phase;
use crate::kernel::{KernelResourceContents, ProgressReporter};
use crate::policy::SurvivorProbe;
use crate::router::{OutcomeRouter, PlaneObservation, RouteRequest};
use crate::scheduler::Scheduler;
use crate::stdio::{ProjectRuntime, StdioOptions};

/// The local-OS principal the harness binds. It is an opaque Todo 3 identifier.
const BOUND_PRINCIPAL: &str = "principal_0123456789abcdef";
/// The trusted observation timestamp every harness request carries.
const OBSERVED_AT_UNIX_MS: u64 = 1_700_000_000_000;

/// The survivor state a test drives, standing in for Todo 17's launch journal.
#[derive(Debug, Default)]
struct MutableSurvivors {
    blocked: Mutex<Option<String>>,
    unreadable: Mutex<bool>,
}

impl SurvivorProbe for MutableSurvivors {
    fn unresolved_survivor(&self) -> Result<Option<String>, String> {
        if *self
            .unreadable
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
        {
            return Err("launch survivor state cannot be read".to_owned());
        }
        Ok(self
            .blocked
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone())
    }
}

/// The Todo 32 acceptance harness over the production project runtime.
pub struct OutcomeRouterHarness {
    runtime: ProjectRuntime,
    survivors: Arc<MutableSurvivors>,
    root: PathBuf,
}

impl OutcomeRouterHarness {
    /// Open the production runtime for one project root.
    ///
    /// # Errors
    /// Returns the lease or store failure that prevented assembly.
    pub fn open(project_root: &Path) -> Result<Self, HarnessError> {
        Self::open_with_optional_godot_binary(project_root, None)
    }

    /// Open the production runtime with an explicitly configured Godot binary.
    ///
    /// # Errors
    /// Returns the lease or store failure that prevented assembly.
    pub fn open_with_godot_binary(
        project_root: &Path,
        godot_binary: PathBuf,
    ) -> Result<Self, HarnessError> {
        Self::open_with_optional_godot_binary(project_root, Some(godot_binary))
    }

    fn open_with_optional_godot_binary(
        project_root: &Path,
        godot_binary: Option<PathBuf>,
    ) -> Result<Self, HarnessError> {
        let survivors = Arc::new(MutableSurvivors::default());
        let options = StdioOptions {
            project: project_root.to_path_buf(),
            godot_binary,
            profile: CapabilityProfile::Full,
            principal: PrincipalId::parse(BOUND_PRINCIPAL)
                .map_err(|error| HarnessError::Permit(error.to_string()))?,
            observed_at_unix_ms: OBSERVED_AT_UNIX_MS,
            limits: Limits::default(),
            process: Default::default(),
        };
        let runtime =
            ProjectRuntime::open_with_survivors(&options, Arc::clone(&survivors) as Arc<_>)
                .map_err(|error| HarnessError::Store(error.to_string()))?;
        Ok(Self {
            runtime,
            survivors,
            root: project_root.to_path_buf(),
        })
    }

    /// Whether this harness drove the production assembly rather than its own.
    ///
    /// It is `true` by construction: [`Self::open`] has exactly one path, and that
    /// path calls `ProjectRuntime::open_with_survivors`, which shares its body with
    /// `ProjectRuntime::open`. The structural counterpart of this claim is
    /// `router_assembly_has_exactly_one_site`, which fails if a second assembly
    /// site ever appears.
    #[must_use]
    pub const fn assembled_through_production_path(&self) -> bool {
        true
    }

    /// The router the production assembly built.
    #[must_use]
    pub fn router(&self) -> Arc<OutcomeRouter> {
        self.runtime.router()
    }

    /// Mint one transaction identifier through the production router.
    #[must_use]
    pub fn next_transaction_id(&self, scope: &str, observed_at_unix_ms: u64) -> String {
        self.router()
            .next_transaction_id(scope, observed_at_unix_ms)
    }

    /// The scheduler the production assembly bound.
    #[must_use]
    pub fn scheduler(&self) -> Arc<Scheduler> {
        self.runtime.scheduler()
    }

    /// Invoke one tool through the production kernel assembled by this runtime.
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, HarnessError> {
        let arguments = arguments
            .as_object()
            .cloned()
            .ok_or_else(|| HarnessError::Store("tool arguments must be an object".to_owned()))?;
        self.runtime
            .kernel()
            .call_tool(
                name,
                arguments,
                self.runtime.access(),
                CancellationToken::new(),
                ProgressReporter::noop(),
            )
            .await
            .map_err(|error| HarnessError::Store(error.to_string()))
    }

    /// Read one resource through the production kernel and decode its JSON body.
    ///
    /// # Errors
    /// Returns the kernel refusal or a JSON decoding failure.
    pub fn read_resource(&self, uri: &str) -> Result<Value, HarnessError> {
        match self
            .runtime
            .kernel()
            .read_resource(uri, None, &self.runtime.access())
            .map_err(|error| HarnessError::Store(error.to_string()))?
        {
            KernelResourceContents::Text { text, .. } => {
                serde_json::from_str(&text).map_err(|error| HarnessError::Store(error.to_string()))
            }
            KernelResourceContents::Blob { .. } => Err(HarnessError::Store(
                "JSON resource unexpectedly returned an opaque blob".to_owned(),
            )),
        }
    }

    /// Build the eight-element identity the router would build for one request.
    ///
    /// # Panics
    /// Panics when the tool is not a canonical operation; every caller in the
    /// acceptance suite passes one that is.
    #[must_use]
    pub fn identity_for(
        &self,
        tool: &str,
        idempotency_id: &str,
        arguments: &Value,
    ) -> MutationIdentity {
        self.router()
            .identity_for(tool, idempotency_id, arguments)
            .unwrap_or_else(|refusal| {
                panic!(
                    "{tool} must be a canonical operation: {}",
                    refusal.to_outcome().message()
                )
            })
    }

    /// Build one complete route request.
    #[must_use]
    pub fn request(
        &self,
        tool: &str,
        idempotency_id: &str,
        transaction_id: Option<&str>,
        path: &str,
        observation: PlaneObservation,
    ) -> RouteRequest {
        RouteRequest {
            tool: tool.to_owned(),
            idempotency_id: idempotency_id.to_owned(),
            transaction_id: transaction_id.map(ToOwned::to_owned),
            path: path.to_owned(),
            bytes: b"todo-32".to_vec(),
            arguments: serde_json::json!({ "contents": "todo-32", "path": path }),
            observation,
            cancellation: CancellationToken::new(),
            queued_at_unix_ms: OBSERVED_AT_UNIX_MS,
            identity: None,
            effect: None,
            audit: None,
        }
    }

    /// The bytes actually on disk at one project-relative path.
    #[must_use]
    pub fn disk_bytes(&self, path: &str) -> Option<Vec<u8>> {
        std::fs::read(self.root.join(path)).ok()
    }

    /// Whether one transaction is journaled at all.
    #[must_use]
    pub fn transaction_exists(&self, transaction_id: &str) -> bool {
        self.runtime.transaction_exists(transaction_id)
    }

    /// Drive the production bridge-quarantine entry point into the real store.
    ///
    /// # Errors
    /// Returns the canonical-record or durable-store failure.
    pub fn quarantine_unresolved_bridge(&self, transaction_id: &str) -> Result<(), HarnessError> {
        let mut ledger = MutationLedger::new();
        ledger
            .open(
                "bridge-request-1",
                "bridge-idem-1",
                OperationClass::Mutation,
            )
            .map_err(|error| HarnessError::Store(error.to_string()))?;
        ledger
            .mark_dispatched("bridge-request-1")
            .map_err(|error| HarnessError::Store(error.to_string()))?;
        ledger
            .settle("bridge-request-1", MutationStatus::Unresolved, None)
            .map_err(|error| HarnessError::Store(error.to_string()))?;
        let envelope = TransactionEnvelope {
            transaction_id: transaction_id.to_owned(),
            project: ProjectIdentityClaim {
                object_identity: format!("project_{}", "a".repeat(64)),
                root_object: "unix:dev=1,ino=2".to_owned(),
                file_object: "unix:dev=1,ino=3".to_owned(),
                generation: 0,
            },
            transport_mode: TransportMode::Stdio,
            principal: Some(BOUND_PRINCIPAL.to_owned()),
            session: Some("stdio".to_owned()),
            operation_family: OperationFamily::EditorMutation,
            tool_name: "editor.set_property".to_owned(),
            idempotency_id: "bridge-idem-1".to_owned(),
            canonical_request_hash: "c".repeat(64),
            canonical_request_bytes: 32,
            opened_at_unix_ms: OBSERVED_AT_UNIX_MS,
        };
        self.router()
            .quarantine_bridge_mutations(&ledger, &envelope)
            .map(|delivered| assert_eq!(delivered, 1))
            .map_err(|error| HarnessError::Store(error.to_string()))
    }

    /// The real store phase for one transaction.
    #[must_use]
    pub fn transaction_phase(&self, transaction_id: &str) -> Option<String> {
        self.runtime.transaction_phase_for_tests(transaction_id)
    }

    /// Drive one transaction to `quarantined`, so every later write is blocked.
    ///
    /// # Errors
    /// Returns the store failure that prevented the transition.
    pub fn quarantine(&self, transaction_id: &str, path: &str) -> Result<(), HarnessError> {
        self.runtime
            .force_phase_for_tests(
                transaction_id,
                path,
                Phase::Quarantined,
                "harness-forced quarantine",
            )
            .map_err(|error| HarnessError::Store(error.to_string()))
    }

    /// Make an unresolved launch survivor block every mutation.
    pub fn block_survivor(&self, reason: &str) {
        *self
            .survivors
            .blocked
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(reason.to_owned());
    }

    /// Make the survivor state unreadable, so the block must be a refusal.
    pub fn make_survivor_unreadable(&self) {
        *self
            .survivors
            .unreadable
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
    }

    /// Clear both survivor conditions.
    pub fn clear_survivor(&self) {
        *self
            .survivors
            .blocked
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        *self
            .survivors
            .unreadable
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = false;
    }
}

#[cfg(test)]
mod harness_audit {
    //! The harness must expose behaviour and observations, never authority, and it
    //! must not be able to route through an assembly of its own.

    const ROUTER_HARNESS: &str = include_str!("router_harness.rs");
    const STDIO_SOURCE: &str = include_str!("../stdio.rs");
    const ROUTER_SOURCE: &str = include_str!("../router/mod.rs");

    fn production_source(source: &str) -> &str {
        source
            .find("#[cfg(test)]")
            .map_or(source, |index| &source[..index])
    }

    /// There is exactly one router assembly site in the whole crate, and the
    /// production entry point delegates to the same body the harness uses.
    #[test]
    fn router_assembly_has_exactly_one_site() {
        assert_eq!(
            production_source(STDIO_SOURCE)
                .matches("OutcomeRouter::assemble(")
                .count(),
            1,
            "a second assembly site would let a caller skip the journal binding"
        );
        assert_eq!(
            production_source(ROUTER_HARNESS)
                .matches("OutcomeRouter::assemble(")
                .count(),
            0,
            "the harness must never assemble its own router"
        );
        assert!(
            production_source(STDIO_SOURCE)
                .contains("Self::open_with(options, Arc::new(NoLaunchPlane))"),
            "the production entry point must delegate to the shared assembly body"
        );
        assert!(
            production_source(STDIO_SOURCE).contains("Self::open_with(options, survivors)"),
            "the injected-probe entry point must delegate to the same body"
        );
    }

    #[test]
    fn core_handler_registration_has_exactly_one_production_site() {
        assert_eq!(
            production_source(STDIO_SOURCE)
                .matches("register_core_tools(")
                .count(),
            1,
            "the production runtime must register core handlers exactly once"
        );
        assert_eq!(
            production_source(ROUTER_HARNESS)
                .matches("register_core_tools(")
                .count(),
            0,
            "the harness must never register handlers outside production assembly"
        );
    }

    #[test]
    fn engine_handler_registration_has_exactly_one_production_site() {
        assert_eq!(
            production_source(STDIO_SOURCE)
                .matches("register_engine_tools(")
                .count(),
            1,
            "the production runtime must register engine-plane handlers exactly once"
        );
        assert_eq!(
            production_source(ROUTER_HARNESS)
                .matches("register_engine_tools(")
                .count(),
            0,
            "the harness must never register engine handlers outside production assembly"
        );
    }

    /// The assembly binds all three durable components, at that one site.
    #[test]
    fn the_one_assembly_site_binds_every_durable_component() {
        let router = production_source(ROUTER_SOURCE);
        assert_eq!(
            router.matches("bind_journal(").count(),
            1,
            "exactly one binding call may exist"
        );
        let assemble = router
            .split_once("pub(crate) fn assemble")
            .expect("the assembly exists")
            .1;
        assert!(assemble.contains("bind_journal("));
        assert!(assemble.contains("IdempotencyGate::new("));
        assert!(assemble.contains("set_plane_availability("));
    }

    #[test]
    fn bridge_quarantine_has_one_production_drain_and_a_real_store_sink() {
        let router = production_source(ROUTER_SOURCE);
        assert_eq!(
            router.matches(".drain_quarantine(").count(),
            1,
            "all unresolved bridge outcomes must enter the store at one production site"
        );
        let store = production_source(include_str!("../journal/store.rs"));
        assert_eq!(
            store
                .matches("impl QuarantineSink for TransactionStore")
                .count(),
            1
        );
        assert_eq!(
            production_source(ROUTER_HARNESS)
                .matches(".drain_quarantine(")
                .count(),
            0,
            "the harness must call the production path rather than drain itself"
        );
    }

    /// The harness never hands a test a permit or a repair proof.
    #[test]
    fn the_harness_never_returns_authority() {
        let source = production_source(ROUTER_HARNESS);
        for banned in [
            "-> EffectPermit",
            "-> PolicyPermit",
            "-> RepairProof",
            "RepairProof::sealed",
            "-> Option<EffectPermit",
            "-> Result<EffectPermit",
        ] {
            assert!(
                !source.contains(banned),
                "the harness must never hand a test authority ({banned})"
            );
        }
    }
}
