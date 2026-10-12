//! The `core` profile's outcome tools: static Godot intelligence, headless.
//!
//! Fifteen canonical tools, one shared [`CoreSession`], and one registration
//! entry point ([`register_core_tools`]). Every handler is reached through Todo
//! 5's [`ServiceKernel`](crate::kernel::ServiceKernel), which is the single
//! authorization and dispatch boundary: nothing here builds its own dispatch
//! table, and a name absent from the canonical `core` manifest cannot be
//! registered at all.
//!
//! ## What this layer is, and is not
//!
//! It is a *projection*. Symbols, references, callers, impact, scene and resource
//! trees, dependencies, the dangling audit and diagnostics are all read off the
//! Todo 10 semantic graph that Todo 11 persisted, and rendered into a bounded
//! public shape. No grammar, no relationship rule and no graph traversal is
//! re-implemented here.
//!
//! It is also headless. `core` never starts, connects to, or waits on a Godot
//! process. `project.validate` and `script.validate` are therefore *static*
//! validators: they report what the static layers can prove and disclose, as a
//! limitation, that engine-level validation was not performed. Claiming otherwise
//! would be the one dishonesty this profile cannot afford, because "the engine
//! said it compiles" is exactly what a caller would read into a bare `valid`.
//!
//! The single exception is a *lookup*, not an execution: `health.get` stats the
//! engine plane's executable-discovery candidates so the `godot` component can
//! report whether a launch is possible, and it deliberately stops short of the
//! capability probe, which would run the build.
//!
//! ## Invariants the acceptance suite pins
//!
//! * Every one of the 15 canonical tools has a real handler; none reports
//!   `unavailable`.
//! * A binary `.scn` / `.res` request is [`ErrorKind::Unsupported`] with
//!   actionable remediation — never an empty success.
//! * Every collection is a deterministic page that declares its own truncation.
//! * Cancellation is observed on the project-wide paths.
//! * An answer's confidence is the weakest confidence inside it.
//! * No internal graph identity, SQL fragment or local filesystem path can appear
//!   in a response.

mod answer;
mod handlers;
mod session;

pub use answer::{
    ConfidenceFloor, Page, PageRequest, REMEDIATION_DETAIL_KEY, outcome_error, remediation_error,
};
pub(crate) use handlers::input_schema;
pub use handlers::register_core_tools;
pub use session::{CoreLimits, CoreSession, DocumentExpectation, PinnedSession};
