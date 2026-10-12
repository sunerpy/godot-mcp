//! Bootstrap: a server launched with a workspace authority instead of a
//! project (plan section 5.2).
//!
//! ```text
//! Bootstrap(workspace authority) -> ProjectBound(project context) -> Closed
//! ```
//!
//! A bootstrap server holds one workspace directory by handle and lists only
//! the bootstrap tools: health, capabilities, project discovery, project
//! creation and one-time binding. `project.bind` assembles the ordinary
//! [`ProjectRuntime`] — lock, journal reconciliation, workspace revision 1 —
//! for exactly the directory it resolved, swaps the transport onto that
//! runtime's kernel and never returns to bootstrap: after the swap the bind
//! tool is no longer listed, so a second project cannot be bound during the
//! process lifetime.
//!
//! The bootstrap authority is separate from project authority. Bootstrap
//! creation writes only a brand-new `project.godot` in an absent or empty
//! directory; it has no journal, so no project transaction can be replayed in
//! this state, and it cannot replace or nest inside an existing project.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock, Weak};

use async_trait::async_trait;
use godot_mcp_domain::catalog::{BootstrapOperation, CoreOperation, HandlerKey};
use godot_mcp_domain::config::Limits;
use godot_mcp_domain::contracts::{OutcomeError, PrincipalId, ProjectIdentity};
use godot_mcp_domain::objid::LeaseKey;
use godot_mcp_domain::workspace_root::{WorkspaceError, WorkspaceRoot};
use godot_mcp_domain::{CapabilityProfile, ErrorKind};
use serde_json::{Map, Value, json};

use crate::bootstrap_schema::MAX_NAME_BYTES;
use crate::kernel::{
    AccessContext, KernelToolCall, KernelToolHandler, PrivatePaths, ServiceKernel,
};
use crate::stdio::{INITIAL_PROJECT_GENERATION, ProjectRuntime, StdioError, StdioOptions};

/// Most entries a project listing observes.
const LIST_MAX_ENTRIES: usize = 4_096;
/// Deepest directory a project listing searches.
const LIST_MAX_DEPTH: usize = 6;

fn outcome_error(kind: ErrorKind, message: impl Into<String>) -> OutcomeError {
    OutcomeError::new(kind, message, false, std::collections::BTreeMap::new())
}

fn workspace_error(error: &WorkspaceError) -> OutcomeError {
    outcome_error(error.error_kind(), error.to_string())
}

/// One resolved bootstrap serve request.
#[derive(Debug, Clone)]
pub struct BootstrapOptions {
    /// The workspace directory the server may create and bind projects under.
    pub workspace: PathBuf,
    /// Explicit Godot executable configured by the operator.
    pub godot_binary: Option<PathBuf>,
    /// Capability profile to expose, before and after binding.
    pub profile: CapabilityProfile,
    /// Local principal identifier for the stdio boundary.
    pub principal: PrincipalId,
    /// Trusted observation timestamp used for retention checks.
    pub observed_at_unix_ms: u64,
    /// Operational limits shared by stdio and optional HTTP.
    pub limits: Limits,
    /// Which boundary contains every Godot launch of the bound project.
    pub process: godot_mcp_domain::config::ProcessConfig,
}

enum SlotState {
    Bootstrap {
        kernel: Arc<ServiceKernel>,
        access: AccessContext,
    },
    Bound {
        runtime: Arc<ProjectRuntime>,
    },
}

/// The server's one state transition, shared by the transport and the bind
/// handler. Every request reads the kernel it is dispatched to from here.
pub struct ServerSlot {
    state: RwLock<SlotState>,
    generation: AtomicU64,
    bind: tokio::sync::Mutex<()>,
}

impl ServerSlot {
    /// Hold `options.workspace` and assemble the bootstrap kernel.
    ///
    /// # Errors
    /// Returns [`StdioError::Lease`]-class failures as [`StdioError::Store`]
    /// when the workspace cannot be held.
    pub fn open_bootstrap(options: BootstrapOptions) -> Result<Arc<Self>, StdioError> {
        let workspace = Arc::new(
            WorkspaceRoot::open(&options.workspace)
                .map_err(|error| StdioError::Store(error.to_string()))?,
        );
        // The bootstrap state has no project; its access context is attributed
        // to the held workspace object, which no project lease can share.
        let identity = workspace.identity();
        let access = AccessContext::new(
            ProjectIdentity::from_lease_key(
                LeaseKey {
                    root: identity,
                    project_file: identity,
                },
                INITIAL_PROJECT_GENERATION,
            ),
            options.principal.clone(),
            None,
            options.observed_at_unix_ms,
        );
        let slot = Arc::new_cyclic(|weak: &Weak<Self>| {
            let mut kernel = ServiceKernel::bootstrap(options.profile)
                .with_private_paths(PrivatePaths::for_workspace(workspace.display_root()));
            let shared = Arc::new(BootstrapShared {
                workspace: Arc::clone(&workspace),
                options: options.clone(),
                slot: weak.clone(),
                create: std::sync::Mutex::new(()),
            });
            for operation in BootstrapAnswer::all() {
                if kernel
                    .register_tool_handler(
                        operation.key(),
                        Arc::new(BootstrapHandler {
                            operation,
                            shared: Arc::clone(&shared),
                        }),
                    )
                    .is_err()
                {
                    // Not listed by this profile (project.create needs a
                    // write-capable profile).
                }
            }
            Self {
                state: RwLock::new(SlotState::Bootstrap {
                    kernel: Arc::new(kernel),
                    access,
                }),
                generation: AtomicU64::new(0),
                bind: tokio::sync::Mutex::new(()),
            }
        });
        let missing = slot.kernel_and_access().0.missing_handlers();
        if !missing.is_empty() {
            return Err(StdioError::Analysis(format!(
                "bootstrap tools have no handler: {}",
                missing.join(", ")
            )));
        }
        Ok(slot)
    }

    /// The kernel and access context the next request is dispatched with.
    #[must_use]
    pub fn kernel_and_access(&self) -> (Arc<ServiceKernel>, AccessContext) {
        match &*self
            .state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
        {
            SlotState::Bootstrap { kernel, access } => (Arc::clone(kernel), access.clone()),
            SlotState::Bound { runtime } => (runtime.kernel(), runtime.access()),
        }
    }

    /// Advances exactly once, when a project is bound.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// The bound project runtime, once one exists.
    #[must_use]
    pub fn runtime(&self) -> Option<Arc<ProjectRuntime>> {
        match &*self
            .state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
        {
            SlotState::Bootstrap { .. } => None,
            SlotState::Bound { runtime } => Some(Arc::clone(runtime)),
        }
    }

    fn is_bound(&self) -> bool {
        self.runtime().is_some()
    }

    fn install(&self, runtime: Arc<ProjectRuntime>) {
        *self
            .state
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = SlotState::Bound { runtime };
        self.generation.fetch_add(1, Ordering::SeqCst);
    }
}

/// What one bootstrap handler answers: the core health operation, which a
/// server holding only a workspace serves from what it holds, or a bootstrap
/// tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BootstrapAnswer {
    Health,
    Tool(BootstrapOperation),
}

impl BootstrapAnswer {
    /// Every operation a bootstrap kernel serves.
    fn all() -> impl Iterator<Item = Self> {
        std::iter::once(Self::Health).chain(BootstrapOperation::ALL.iter().copied().map(Self::Tool))
    }

    /// The catalog operation this handler serves.
    const fn key(self) -> HandlerKey {
        match self {
            Self::Health => CoreOperation::Health.key(),
            Self::Tool(tool) => tool.key(),
        }
    }
}

struct BootstrapShared {
    workspace: Arc<WorkspaceRoot>,
    options: BootstrapOptions,
    slot: Weak<ServerSlot>,
    /// Serializes `project.create`, so one idempotency key's record and the
    /// project it names change together.
    create: std::sync::Mutex<()>,
}

struct BootstrapHandler {
    operation: BootstrapAnswer,
    shared: Arc<BootstrapShared>,
}

fn string_argument<'a>(
    arguments: &'a Map<String, Value>,
    key: &str,
) -> Result<&'a str, OutcomeError> {
    arguments.get(key).and_then(Value::as_str).ok_or_else(|| {
        outcome_error(
            ErrorKind::InvalidInput,
            format!("`{key}` is required and must be a string"),
        )
    })
}

/// A workspace-relative spelling with `\` folded to `/`.
fn relative_path(raw: &str) -> Result<PathBuf, OutcomeError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(outcome_error(
            ErrorKind::InvalidInput,
            "`path` must not be empty",
        ));
    }
    let path = PathBuf::from(trimmed.replace('\\', "/"));
    let private = [
        godot_mcp_domain::lease::PROJECT_STATE_DIR_NAME,
        godot_mcp_domain::workspace_root::BOOTSTRAP_STATE_DIR_NAME,
    ];
    if path.components().any(|component| {
        private
            .iter()
            .any(|name| component.as_os_str() == std::ffi::OsStr::new(name))
    }) {
        return Err(outcome_error(
            ErrorKind::PermissionDenied,
            "the server's private state directory is never a project",
        ));
    }
    Ok(path)
}

fn display_relative(path: &Path) -> String {
    let spelled = path.to_string_lossy().replace('\\', "/");
    if spelled.is_empty() {
        ".".to_owned()
    } else {
        spelled
    }
}

/// Godot's own spelling of a string value.
fn godot_string(text: &str) -> String {
    let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// The `project.godot` a new project starts from.
fn new_manifest(name: &str, renderer: &str) -> String {
    let mut text = String::from(
        "; Engine configuration file.\n\
         ; It's best edited using the editor UI and not directly,\n\
         ; since the parameters that go here are not all obvious.\n\
         ;\n\
         ; Format:\n\
         ;   [section] ; section goes between []\n\
         ;   param=value ; assign values to parameters\n\
         \n\
         config_version=5\n\
         \n\
         [application]\n\
         \n",
    );
    text.push_str(&format!("config/name={}\n", godot_string(name)));
    if renderer != "forward_plus" {
        text.push_str(&format!(
            "\n[rendering]\n\nrenderer/rendering_method={0}\nrenderer/rendering_method.mobile={0}\n",
            godot_string(renderer)
        ));
    }
    text
}

/// The `config/name` of one manifest, when it has one.
fn manifest_name(bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    godot_mcp_analysis::project::ProjectSettings::parse(text, INITIAL_PROJECT_GENERATION)
        .ok()?
        .application()
        .name()
        .map(str::to_owned)
}

impl BootstrapHandler {
    fn health(&self) -> Value {
        json!({
            "status": "ready",
            "state": "bootstrap",
            "server_version": crate::SERVER_VERSION,
            "contract_version": godot_mcp_domain::CONTRACT_VERSION,
            "profile": self.shared.options.profile.manifest_name(),
            "project_bound": false,
            "write_gate": "no project is bound; only project.create writes, and only a new project",
            "next": "call workspace.projects.list, then project.create or project.bind",
        })
    }

    fn list(&self) -> Result<Value, OutcomeError> {
        let workspace = &self.shared.workspace;
        let listing = workspace
            .list_projects(LIST_MAX_ENTRIES, LIST_MAX_DEPTH)
            .map_err(|error| workspace_error(&error))?;
        let projects: Vec<Value> = listing
            .projects()
            .iter()
            .map(|relative| {
                let name = workspace
                    .read_manifest(relative)
                    .ok()
                    .and_then(|bytes| manifest_name(&bytes));
                json!({ "path": display_relative(relative), "name": name })
            })
            .collect();
        Ok(json!({
            "projects": projects,
            "complete": listing.is_complete(),
        }))
    }

    fn create(&self, call: &KernelToolCall) -> Result<Value, OutcomeError> {
        let Some(idempotency_id) = call.idempotency_id.as_deref() else {
            return Err(outcome_error(
                ErrorKind::InvalidInput,
                "project.create requires `idempotencyId`, so a retried call can never apply twice",
            ));
        };
        let relative = relative_path(string_argument(&call.arguments, "path")?)?;
        let name = string_argument(&call.arguments, "name")?;
        if name.trim().is_empty()
            || name.len() > MAX_NAME_BYTES
            || name.chars().any(char::is_control)
        {
            return Err(outcome_error(
                ErrorKind::InvalidInput,
                format!("`name` must be 1-{MAX_NAME_BYTES} bytes without control characters"),
            ));
        }
        let renderer = match call.arguments.get("renderer") {
            None => "forward_plus",
            Some(Value::String(renderer))
                if matches!(
                    renderer.as_str(),
                    "forward_plus" | "mobile" | "gl_compatibility"
                ) =>
            {
                renderer.as_str()
            }
            Some(_) => {
                return Err(outcome_error(
                    ErrorKind::InvalidInput,
                    "`renderer` must be forward_plus, mobile or gl_compatibility",
                ));
            }
        };
        let manifest = new_manifest(name, renderer);
        let workspace = &self.shared.workspace;
        // One record per (principal, idempotency id), bound to this exact
        // request: an intent before the project exists, the answer after.
        let request_sha256 = crate::journal::hex_sha256(
            json!({ "path": display_relative(&relative), "name": name, "renderer": renderer })
                .to_string()
                .as_bytes(),
        );
        let key = crate::journal::hex_sha256(
            format!(
                "{}\0{idempotency_id}",
                call.access.mode_principal().to_journal_string()
            )
            .as_bytes(),
        );
        let record_error =
            |_| outcome_error(ErrorKind::Internal, "a bootstrap record is unreadable");
        let _serialized = self
            .shared
            .create
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let recorded = workspace
            .bootstrap_record(&key)
            .map_err(|error| workspace_error(&error))?;
        let intent = json!({ "request_sha256": request_sha256, "state": "intent" }).to_string();
        if let Some(bytes) = &recorded {
            let record: Value = serde_json::from_slice(bytes).map_err(record_error)?;
            if record["request_sha256"] != json!(request_sha256) {
                return Err(outcome_error(
                    ErrorKind::Conflict,
                    "this `idempotencyId` already named a different project.create request",
                ));
            }
            if record["state"] == json!("created") {
                let mut answer = record["result"].clone();
                answer["replayed"] = json!(true);
                return Ok(answer);
            }
            // An intent without an answer: the attempt that recorded it
            // stopped before answering, so this one finishes it.
        } else if !workspace
            .publish_bootstrap_record(&key, None, intent.as_bytes())
            .map_err(|error| workspace_error(&error))?
        {
            return Err(outcome_error(
                ErrorKind::Conflict,
                "another request recorded this `idempotencyId` first",
            ));
        }
        let (created, replayed) = match workspace.create_project(&relative, manifest.as_bytes()) {
            Ok(created) => (created.relative().to_path_buf(), false),
            // Only this request's own unfinished attempt may have created it.
            Err(WorkspaceError::ProjectExists(_)) if recorded.is_some() => {
                let existing = workspace
                    .read_manifest(&relative)
                    .map_err(|error| workspace_error(&error))?;
                if existing != manifest.as_bytes() {
                    return Err(outcome_error(
                        ErrorKind::Conflict,
                        format!(
                            "{} already holds a different Godot project; bind it instead",
                            display_relative(&relative)
                        ),
                    ));
                }
                let resolved = workspace
                    .resolve_project(&relative)
                    .map_err(|error| workspace_error(&error))?;
                (resolved.relative().to_path_buf(), true)
            }
            Err(WorkspaceError::ProjectExists(_)) => {
                return Err(outcome_error(
                    ErrorKind::Conflict,
                    format!(
                        "{} already holds a Godot project another request created; bind it instead",
                        display_relative(&relative)
                    ),
                ));
            }
            Err(error) => return Err(workspace_error(&error)),
        };
        let answer = json!({
            "path": display_relative(&created),
            "name": name,
            "renderer": renderer,
            "manifest_sha256": crate::journal::hex_sha256(manifest.as_bytes()),
            "replayed": replayed,
            "bound": false,
            "next": "call project.bind with this path",
        });
        let finished = json!({
            "request_sha256": request_sha256,
            "state": "created",
            "result": answer,
        })
        .to_string();
        let expected = recorded.as_deref().unwrap_or(intent.as_bytes());
        if !workspace
            .publish_bootstrap_record(&key, Some(expected), finished.as_bytes())
            .map_err(|error| workspace_error(&error))?
        {
            return Err(outcome_error(
                ErrorKind::Conflict,
                "another request changed this `idempotencyId`'s record",
            ));
        }
        Ok(answer)
    }

    async fn bind(&self, call: &KernelToolCall) -> Result<Value, OutcomeError> {
        let slot =
            self.shared.slot.upgrade().ok_or_else(|| {
                outcome_error(ErrorKind::Unavailable, "the server is shutting down")
            })?;
        let _serialized = slot.bind.lock().await;
        if slot.is_bound() {
            return Err(outcome_error(
                ErrorKind::Conflict,
                "a project is already bound; a server binds exactly one project per lifetime",
            ));
        }
        let relative = relative_path(string_argument(&call.arguments, "path")?)?;
        let resolved = self
            .shared
            .workspace
            .resolve_project(&relative)
            .map_err(|error| workspace_error(&error))?;
        let options = StdioOptions {
            project: resolved.display_root().to_path_buf(),
            godot_binary: self.shared.options.godot_binary.clone(),
            profile: self.shared.options.profile,
            principal: self.shared.options.principal.clone(),
            observed_at_unix_ms: call.access.observed_at_unix_ms(),
            limits: self.shared.options.limits.clone(),
            process: self.shared.options.process.clone(),
        };
        let runtime = tokio::task::spawn_blocking(move || ProjectRuntime::open(&options))
            .await
            .map_err(|_| outcome_error(ErrorKind::Internal, "the bind task did not complete"))?
            .map_err(|error| {
                outcome_error(
                    match error {
                        StdioError::Lease(ref lease) => lease.error_kind(),
                        _ => ErrorKind::Unavailable,
                    },
                    format!("the project could not be bound: {error}"),
                )
            })?;
        if runtime.context().lease().lease_key().root != resolved.root_identity() {
            return Err(outcome_error(
                ErrorKind::Conflict,
                "the project directory changed while it was being bound; nothing was bound",
            ));
        }
        runtime.apply_reconciliation_mode();
        let write_gate = runtime.write_gate_status();
        let revision = runtime
            .kernel()
            .call_tool(
                CoreOperation::ProjectInfo.tool_name(),
                Map::new(),
                runtime.access(),
                call.cancellation.clone(),
                crate::kernel::ProgressReporter::noop(),
            )
            .await
            .ok()
            .and_then(|info| info.get("workspace").cloned());
        let reconciled: Map<String, Value> = runtime
            .reconciliation_counts()
            .into_iter()
            .map(|(conclusion, count)| (conclusion, json!(count)))
            .collect();
        let project = runtime.access().project().object_id().to_string();
        slot.install(Arc::new(runtime));
        Ok(json!({
            "bound": true,
            "path": display_relative(resolved.relative()),
            "project": { "object_id": project, "generation": INITIAL_PROJECT_GENERATION },
            "workspace": revision,
            "reconciled": reconciled,
            "write_gate": match write_gate {
                Ok(()) => json!({ "open": true }),
                Err(reason) => json!({ "open": false, "reason": reason }),
            },
            "tools_changed": true,
        }))
    }
}

#[async_trait]
impl KernelToolHandler for BootstrapHandler {
    async fn call(&self, call: KernelToolCall) -> Result<Value, OutcomeError> {
        match self.operation {
            BootstrapAnswer::Health => Ok(self.health()),
            BootstrapAnswer::Tool(BootstrapOperation::ProjectsList) => self.list(),
            BootstrapAnswer::Tool(BootstrapOperation::ProjectCreate) => self.create(&call),
            BootstrapAnswer::Tool(BootstrapOperation::ProjectBind) => self.bind(&call).await,
        }
    }
}

/// Serve MCP over stdin/stdout from a bootstrap slot until the peer
/// disconnects or shutdown is requested.
///
/// # Errors
/// Returns [`StdioError::Transport`] when the transport fails.
pub async fn serve_bootstrap_stdio(slot: Arc<ServerSlot>) -> Result<(), StdioError> {
    use rmcp::ServiceExt as _;

    let adapter = crate::rmcp_adapter::RmcpAdapter::switching(slot);
    let service = adapter
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|error| StdioError::Transport(error.to_string()))?;
    tracing::info!(
        event = "stdio_serving",
        state = "bootstrap",
        "stdio transport is serving the bootstrap kernel"
    );
    service
        .waiting()
        .await
        .map(|_reason| ())
        .map_err(|error| StdioError::Transport(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_manifest_parses_as_a_godot_4_project() {
        let text = new_manifest("Quoted \"Name\"", "gl_compatibility");
        let settings =
            godot_mcp_analysis::project::ProjectSettings::parse(&text, 1).expect("parses");
        assert!(
            settings.diagnostics().is_empty(),
            "{:?}",
            settings.diagnostics()
        );
        assert_eq!(settings.config_version(), Some(5));
        assert!(settings.application().name().is_some());
        assert_eq!(
            manifest_name(text.as_bytes()).as_deref(),
            settings.application().name()
        );
        assert!(text.contains("renderer/rendering_method=\"gl_compatibility\"\n"));
        assert!(!new_manifest("Plain", "forward_plus").contains("[rendering]"));
    }

    /// A bootstrap kernel serves every bootstrap tool and the health report,
    /// each under its own catalog operation.
    #[test]
    fn a_bootstrap_kernel_serves_each_operation_under_its_own_key() {
        let keys: Vec<HandlerKey> = BootstrapAnswer::all().map(BootstrapAnswer::key).collect();
        assert_eq!(keys.len(), BootstrapOperation::ALL.len() + 1);
        for key in &keys {
            assert!(
                key.descriptor()
                    .in_state(godot_mcp_domain::catalog::ServerState::Bootstrap),
                "{key:?}"
            );
        }
    }
}
