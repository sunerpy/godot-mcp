//! Validate/test/import/run/stop/UID engine tools, and the effect-permit port.
//!
//! # Every tool goes through a permit
//!
//! [`EngineTools`] holds an [`EffectGate`]. Nothing in this module — and nothing in
//! the two adapters — reaches an effect without first calling
//! [`EffectGate::authorize`] and receiving a [`ToolPermit`]. The server layer backs
//! that port with Todo 6's `EffectGateway`, which validates the permit seal, epoch,
//! project object identity/generation, capability, operation family and boundary,
//! re-checks quarantine at execution time and journals the four fsynced phases.
//!
//! The port exists because this crate must not depend on `godot-mcp-server`: the
//! gateway is crate-private there by design, so the engine plane names the
//! *obligation* and the server supplies the implementation.
//!
//! # This is not Todo 27's scheduler
//!
//! There is no queue, no fairness policy and no backpressure here. A tool asks for a
//! permit, gets one or is refused, and runs. Scheduling arrives later and on top of
//! this, not inside it.
//!
//! # Six tools, three effect boundaries
//!
//! | tool | boundary | why |
//! | --- | --- | --- |
//! | `validate` | `process` | runs the engine's `--check-only --script` through Todo 15 |
//! | `test` | `process` | runs the project's committed framework through Todo 15 |
//! | `import` | `process` | runs `--import`, which writes `.godot/` |
//! | `run` | `process` | launches a supervised game instance |
//! | `stop` | `process` | terminates a launch this server owns, by its recorded identity |
//! | `uid` | `file` | engine-minted `ResourceUID` work through Todo 17 |
//!
//! `stop` is the one that would be easiest to get wrong. It never takes a PID from a
//! caller: it takes a launch transaction ID, reads that launch's own recorded
//! boundary identity and PID/start-time tuple, and terminates *that*. A caller
//! cannot ask this server to kill an arbitrary process.

use std::time::Duration;

use crate::adapters::error::AdapterError;
use crate::supervision::journal::OperationFamily;
use crate::validation::outcome::{Classification, Verdict};

/// The effect boundary a tool or exchange touches. Mirrors Todo 6's boundary kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EffectBoundary {
    /// A supervised OS process.
    Process,
    /// A project file mutation.
    File,
    /// A language-server / debug-adapter exchange.
    LspDap,
}

impl EffectBoundary {
    /// Todo 6's wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Process => "process",
            Self::File => "file",
            Self::LspDap => "lsp_dap",
        }
    }
}

/// The six engine tools this todo exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EngineTool {
    /// Parse-check a script.
    Validate,
    /// Run the project's committed test framework.
    Test,
    /// Import the project's resources.
    Import,
    /// Launch a supervised game instance.
    Run,
    /// Terminate a launch this server owns.
    Stop,
    /// Engine-minted `ResourceUID` work.
    Uid,
}

impl EngineTool {
    /// Every tool. There are six.
    pub const ALL: [Self; 6] = [
        Self::Validate,
        Self::Test,
        Self::Import,
        Self::Run,
        Self::Stop,
        Self::Uid,
    ];

    /// The canonical tool name this operation is journaled under.
    #[must_use]
    pub const fn tool_name(self) -> &'static str {
        match self {
            Self::Validate => "engine.validate",
            Self::Test => "engine.test",
            Self::Import => "engine.import",
            Self::Run => "engine.run",
            Self::Stop => "engine.stop",
            Self::Uid => "engine.uid",
        }
    }

    /// The effect boundary this tool touches.
    #[must_use]
    pub const fn boundary(self) -> EffectBoundary {
        match self {
            Self::Validate | Self::Test | Self::Import | Self::Run | Self::Stop => {
                EffectBoundary::Process
            }
            Self::Uid => EffectBoundary::File,
        }
    }

    /// The operation family the transaction belongs to.
    #[must_use]
    pub const fn operation_family(self) -> OperationFamily {
        match self {
            // A parse check reads; it never writes the project.
            Self::Validate => OperationFamily::DiagnosticProbe,
            // A test run and a game run execute project code under supervision.
            Self::Test | Self::Run | Self::Stop => OperationFamily::RuntimeExecution,
            // An import writes `.godot/`; a UID write touches a resource.
            Self::Import | Self::Uid => OperationFamily::ProjectMutation,
        }
    }
}

/// One request for authority to perform an effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectRequest {
    /// The canonical tool name.
    pub tool_name: String,
    /// The boundary being touched.
    pub boundary: EffectBoundary,
    /// The operation family.
    pub operation_family: OperationFamily,
    /// The held project's opaque object identity.
    pub project_object_identity: String,
    /// The held project's generation, so a permit minted against an older
    /// generation is refused as stale.
    pub project_generation: u64,
    /// A short description of the concrete operation, for audit.
    pub detail: String,
}

/// Authority to perform exactly one effect.
///
/// Produced only by an [`EffectGate`] implementation. Every effect-performing entry
/// point in this module and in the two adapters takes one by reference, so an effect
/// with no permit is not expressible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPermit {
    transaction_id: String,
    tool_name: String,
    boundary: EffectBoundary,
    operation_family: OperationFamily,
    project_generation: u64,
}

impl ToolPermit {
    /// Issue a permit. Called by the gateway implementation only — the engine plane
    /// never mints its own authority.
    #[must_use]
    pub fn issue(transaction_id: &str, request: &EffectRequest) -> Self {
        Self {
            transaction_id: transaction_id.to_owned(),
            tool_name: request.tool_name.clone(),
            boundary: request.boundary,
            operation_family: request.operation_family,
            project_generation: request.project_generation,
        }
    }

    /// The transaction this permit authorises.
    #[must_use]
    pub fn transaction_id(&self) -> &str {
        &self.transaction_id
    }

    /// The tool this permit authorises.
    #[must_use]
    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    /// The boundary this permit authorises.
    #[must_use]
    pub const fn boundary(&self) -> EffectBoundary {
        self.boundary
    }

    /// The operation family this permit authorises.
    #[must_use]
    pub const fn operation_family(&self) -> OperationFamily {
        self.operation_family
    }

    /// The project generation this permit was minted against.
    #[must_use]
    pub const fn project_generation(&self) -> u64 {
        self.project_generation
    }

    /// Whether this permit authorises `request`.
    ///
    /// Checked at the point of use as well as at issue, so a permit cannot be
    /// carried sideways into a different boundary or a newer generation.
    #[must_use]
    pub fn authorises(&self, request: &EffectRequest) -> bool {
        self.tool_name == request.tool_name
            && self.boundary == request.boundary
            && self.operation_family == request.operation_family
            && self.project_generation == request.project_generation
    }
}

/// The port onto Todo 6's `EffectGateway`.
pub trait EffectGate {
    /// Authorize one effect, or refuse it.
    ///
    /// # Errors
    /// Returns the refusal reason. The engine plane maps it to
    /// [`AdapterError::PermitRefused`] and performs no effect.
    fn authorize(&self, request: &EffectRequest) -> Result<ToolPermit, String>;
}

/// Ask the gate for authority, mapping a refusal to the adapter error taxonomy.
///
/// # Errors
/// Returns [`AdapterError::PermitRefused`] when the gate refuses.
pub fn permit_for(
    gate: &dyn EffectGate,
    request: &EffectRequest,
) -> Result<ToolPermit, AdapterError> {
    let permit = gate
        .authorize(request)
        .map_err(|detail| AdapterError::PermitRefused { detail })?;
    if permit.authorises(request) {
        Ok(permit)
    } else {
        Err(AdapterError::PermitRefused {
            detail: format!(
                "the issued permit authorises {}/{} at generation {}, not {}/{} at generation {}",
                permit.tool_name(),
                permit.boundary().as_str(),
                permit.project_generation(),
                request.tool_name,
                request.boundary.as_str(),
                request.project_generation,
            ),
        })
    }
}

/// One supervised invocation's result: the verdict it reached and the exact argument
/// array that produced it.
pub type SupervisedResult = Result<(Verdict, Vec<String>), AdapterError>;

/// What one engine tool produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineToolOutcome {
    /// Which tool ran.
    pub tool: EngineTool,
    /// The transaction the effect was journaled under.
    pub transaction_id: String,
    /// The verdict, from Todo 15's single classification entry point where the tool
    /// runs the engine.
    pub verdict: Verdict,
    /// The exact argument array issued, when a process ran.
    pub argv: Vec<String>,
    /// A short, quotable description of what was observed.
    pub detail: String,
}

impl EngineToolOutcome {
    /// Whether the tool provably completed its work.
    #[must_use]
    pub const fn is_success(&self) -> bool {
        self.verdict.classification.is_success()
    }
}

/// What a `stop` request names. Never a raw PID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopRequest {
    /// The launch transaction to terminate. The recorded boundary identity and
    /// PID/start-time tuple come from that launch's own durable record, so a caller
    /// cannot ask this server to kill an arbitrary process.
    pub launch_transaction_id: String,
    /// How long to wait for the boundary to be proven empty.
    pub deadline: Duration,
}

/// The six engine tools, bound to one effect gate.
pub struct EngineTools<'g> {
    gate: &'g dyn EffectGate,
    project_object_identity: String,
    project_generation: u64,
}

impl<'g> EngineTools<'g> {
    /// Bind the tools to one gate and one held project identity.
    #[must_use]
    pub fn new(
        gate: &'g dyn EffectGate,
        project_object_identity: &str,
        project_generation: u64,
    ) -> Self {
        Self {
            gate,
            project_object_identity: project_object_identity.to_owned(),
            project_generation,
        }
    }

    /// The effect request `tool` would need authority for.
    #[must_use]
    pub fn request_for(&self, tool: EngineTool, detail: &str) -> EffectRequest {
        EffectRequest {
            tool_name: tool.tool_name().to_owned(),
            boundary: tool.boundary(),
            operation_family: tool.operation_family(),
            project_object_identity: self.project_object_identity.clone(),
            project_generation: self.project_generation,
            detail: detail.to_owned(),
        }
    }

    /// Obtain authority for `tool`.
    ///
    /// # Errors
    /// Returns [`AdapterError::PermitRefused`] when the gate refuses or issues a
    /// permit that does not match the request.
    pub fn permit(&self, tool: EngineTool, detail: &str) -> Result<ToolPermit, AdapterError> {
        permit_for(self.gate, &self.request_for(tool, detail))
    }

    /// Run one tool whose work is a supervised engine invocation.
    ///
    /// `invoke` is the Todo 15 operation (`check_script`, `run_tests`,
    /// `import_project`, …) reduced to the verdict and argv it produced. It is called
    /// **only after** a permit has been issued, so a refused permit performs no
    /// effect at all.
    ///
    /// # Errors
    /// Returns [`AdapterError::PermitRefused`] before `invoke` is called when the gate
    /// refuses, or whatever `invoke` itself returns.
    pub fn run_supervised(
        &self,
        tool: EngineTool,
        detail: &str,
        invoke: &mut dyn FnMut(&ToolPermit) -> SupervisedResult,
    ) -> Result<EngineToolOutcome, AdapterError> {
        let permit = self.permit(tool, detail)?;
        let (verdict, argv) = invoke(&permit)?;
        let detail = verdict_detail(&verdict);
        Ok(EngineToolOutcome {
            tool,
            transaction_id: permit.transaction_id().to_owned(),
            verdict,
            argv,
            detail,
        })
    }

    /// Terminate a launch this server owns.
    ///
    /// `terminate` receives the permit and the launch transaction ID and is expected
    /// to drive Todo 14's own boundary termination, which proves the boundary empty.
    /// This function never accepts a PID.
    ///
    /// # Errors
    /// Returns [`AdapterError::PermitRefused`] before `terminate` is called when the
    /// gate refuses, or whatever `terminate` itself returns.
    pub fn stop(
        &self,
        request: &StopRequest,
        terminate: &mut dyn FnMut(&ToolPermit, &str, Duration) -> Result<Verdict, AdapterError>,
    ) -> Result<EngineToolOutcome, AdapterError> {
        let detail = format!("terminate launch {}", request.launch_transaction_id);
        let permit = self.permit(EngineTool::Stop, &detail)?;
        let verdict = terminate(&permit, &request.launch_transaction_id, request.deadline)?;
        let detail = verdict_detail(&verdict);
        Ok(EngineToolOutcome {
            tool: EngineTool::Stop,
            transaction_id: permit.transaction_id().to_owned(),
            verdict,
            argv: Vec::new(),
            detail,
        })
    }
}

/// A quotable one-line description of a verdict.
fn verdict_detail(verdict: &Verdict) -> String {
    format!("{}: {}", verdict.classification.as_str(), verdict.reason)
}

/// The verdict an operation reports when the build cannot do the requested work.
///
/// Kept here so a tool that refuses before launching produces the same shape as one
/// that ran, rather than a bespoke error.
#[must_use]
pub fn unsupported_verdict(detection: &str) -> Verdict {
    Verdict {
        classification: Classification::Unsupported,
        reason: format!("the requested capability is not available: {detection}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A gate that records every authorization it is asked for, so a test can assert
    /// that a refused operation dispatched **no** effect.
    struct RecordingGate {
        seen: RefCell<Vec<EffectRequest>>,
        refuse: Option<String>,
    }

    impl RecordingGate {
        fn allowing() -> Self {
            Self {
                seen: RefCell::new(Vec::new()),
                refuse: None,
            }
        }

        fn refusing(reason: &str) -> Self {
            Self {
                seen: RefCell::new(Vec::new()),
                refuse: Some(reason.to_owned()),
            }
        }

        fn calls(&self) -> usize {
            self.seen.borrow().len()
        }
    }

    impl EffectGate for RecordingGate {
        fn authorize(&self, request: &EffectRequest) -> Result<ToolPermit, String> {
            self.seen.borrow_mut().push(request.clone());
            match &self.refuse {
                Some(reason) => Err(reason.clone()),
                None => Ok(ToolPermit::issue(
                    "00000000-0000-4000-8000-000000001899",
                    request,
                )),
            }
        }
    }

    fn passed() -> Verdict {
        Verdict {
            classification: Classification::Passed,
            reason: "exit status 0 matched and the parsed evidence is clean".to_owned(),
        }
    }

    #[test]
    fn every_tool_names_its_boundary_and_family() {
        for tool in EngineTool::ALL {
            assert!(tool.tool_name().starts_with("engine."));
            match tool {
                EngineTool::Uid => assert_eq!(tool.boundary(), EffectBoundary::File),
                _ => assert_eq!(tool.boundary(), EffectBoundary::Process),
            }
        }
        assert_eq!(
            EngineTool::Validate.operation_family(),
            OperationFamily::DiagnosticProbe,
            "a parse check reads and never writes the project"
        );
        assert_eq!(
            EngineTool::Import.operation_family(),
            OperationFamily::ProjectMutation,
            "--import writes .godot/"
        );
    }

    #[test]
    fn a_refused_permit_performs_no_effect() {
        let gate = RecordingGate::refusing("the authority epoch is stale");
        let tools = EngineTools::new(&gate, "project_abc", 17);
        let mut invoked = 0_u32;
        let error = tools
            .run_supervised(EngineTool::Import, "import the project", &mut |_permit| {
                invoked += 1;
                Ok((passed(), Vec::new()))
            })
            .expect_err("refused");
        assert!(
            matches!(error, AdapterError::PermitRefused { .. }),
            "{error}"
        );
        assert_eq!(
            invoked, 0,
            "the effect body never ran, so nothing was dispatched"
        );
        assert_eq!(gate.calls(), 1, "the gate was consulted exactly once");
    }

    #[test]
    fn a_permit_for_the_wrong_boundary_is_rejected_at_the_point_of_use() {
        struct CrossBoundaryGate;
        impl EffectGate for CrossBoundaryGate {
            fn authorize(&self, request: &EffectRequest) -> Result<ToolPermit, String> {
                // Mint authority for a *different* boundary than was asked for.
                let mut forged = request.clone();
                forged.boundary = EffectBoundary::LspDap;
                Ok(ToolPermit::issue("tx", &forged))
            }
        }
        let gate = CrossBoundaryGate;
        let tools = EngineTools::new(&gate, "project_abc", 17);
        let error = tools
            .permit(EngineTool::Import, "import")
            .expect_err("mismatched permit");
        assert!(
            matches!(error, AdapterError::PermitRefused { .. }),
            "{error}"
        );
    }

    #[test]
    fn a_permit_from_an_older_generation_does_not_authorise_a_newer_request() {
        let gate = RecordingGate::allowing();
        let tools = EngineTools::new(&gate, "project_abc", 17);
        let permit = tools.permit(EngineTool::Validate, "check").expect("issued");
        let newer = EffectRequest {
            project_generation: 18,
            ..tools.request_for(EngineTool::Validate, "check")
        };
        assert!(
            !permit.authorises(&newer),
            "a generation change invalidates the permit"
        );
    }

    #[test]
    fn a_permitted_tool_reports_the_transaction_it_ran_under() {
        let gate = RecordingGate::allowing();
        let tools = EngineTools::new(&gate, "project_abc", 17);
        let outcome = tools
            .run_supervised(EngineTool::Validate, "check res://a.gd", &mut |permit| {
                assert_eq!(permit.boundary(), EffectBoundary::Process);
                Ok((passed(), vec!["--check-only".to_owned()]))
            })
            .expect("ran");
        assert!(outcome.is_success());
        assert_eq!(
            outcome.transaction_id,
            "00000000-0000-4000-8000-000000001899"
        );
        assert_eq!(outcome.argv, vec!["--check-only".to_owned()]);
        assert!(outcome.detail.starts_with("passed: "));
    }

    #[test]
    fn stop_names_a_launch_transaction_and_never_a_pid() {
        let gate = RecordingGate::allowing();
        let tools = EngineTools::new(&gate, "project_abc", 17);
        let mut seen: Option<String> = None;
        let outcome = tools
            .stop(
                &StopRequest {
                    launch_transaction_id: "00000000-0000-4000-8000-000000001700".to_owned(),
                    deadline: Duration::from_secs(5),
                },
                &mut |_permit, transaction_id, _deadline| {
                    seen = Some(transaction_id.to_owned());
                    Ok(passed())
                },
            )
            .expect("stopped");
        assert_eq!(
            seen.as_deref(),
            Some("00000000-0000-4000-8000-000000001700")
        );
        assert_eq!(outcome.tool, EngineTool::Stop);
        assert!(outcome.argv.is_empty());
    }

    #[test]
    fn an_unsupported_build_produces_a_verdict_not_a_bespoke_error() {
        let verdict = unsupported_verdict("this build does not advertise --import");
        assert_eq!(verdict.classification, Classification::Unsupported);
        assert!(!verdict.classification.is_success());
    }
}
