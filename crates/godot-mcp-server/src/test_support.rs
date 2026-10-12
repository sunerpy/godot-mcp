//! Deterministic harness primitives compiled only with `test-hooks`.

use std::io::{self, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tracing::Dispatch;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

use crate::observability::{SafeField, SafeFieldError};

mod audit_limits_harness;
pub mod fake_editor_plugin;
mod journal_harness;
mod policy_harness;
mod router_harness;
mod scheduler_harness;
mod task34_fixture;

pub use audit_limits_harness::{
    AttemptRefusal, AttemptReport, AttemptSpec, AuditLimitsConfig, AuditLimitsHarness,
    KernelAuditCallReport, LimitGate, NegativeCallReport,
};
pub use journal_harness::{
    EffectBoundary, FaultInjection, HarnessError, JournalFoundationHarness, JournalPhase,
    MockFileEffect, MockFileOperation, ObservedTransaction, RecoveryOperation, TerminalDisposition,
};
pub use policy_harness::{
    BoundaryTouches, PolicyDispatchReport, PolicyHarness, PolicyMatrixRow, PolicyOutcome,
    policy_matrix,
};
pub use router_harness::OutcomeRouterHarness;
pub use scheduler_harness::{
    MutationReport, ReadOverlap, RefusalReport, RequestSpec, SchedulerIdempotencyHarness,
};
pub use task34_fixture::{
    FIXTURE_OPENED_AT_UNIX_MS, Task34EffectKind, Task34FileOperation, Task34FileState,
    Task34FixtureJournal, Task34Held, Task34Proof, Task34RecordSpec, Task34Release,
};

/// Monotonic deterministic clock local to one fixture; no process-global state.
#[derive(Debug)]
pub struct DeterministicClock {
    next_millis: AtomicU64,
}

impl DeterministicClock {
    /// Return the next deterministic timestamp.
    #[must_use]
    pub fn now_millis(&self) -> u64 {
        self.next_millis.fetch_add(1, Ordering::Relaxed)
    }
}

/// A reserved loopback port. Its listener prevents ambient parallel allocation.
#[derive(Debug)]
pub struct ReservedPort {
    port: u16,
    listener: TcpListener,
}

impl ReservedPort {
    /// Reserved TCP port number.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Consume the reservation when a test is ready to own the listener.
    #[must_use]
    pub fn into_listener(self) -> TcpListener {
        self.listener
    }
}

impl PartialEq<u16> for ReservedPort {
    fn eq(&self, other: &u16) -> bool {
        self.port == *other
    }
}

/// Deterministic allocator that reserves each selected port before returning it.
///
/// Determinism here is fixture-local *ordering*, never an exact port number. The
/// cursor starts at the fixture's `first_port` and every allocation moves it
/// strictly forward, so one fixture hands out monotonically increasing,
/// non-repeating ports and holds each one for its own lifetime. The loopback
/// range is shared with the rest of the host, though: an unrelated process can
/// already own a port inside the fixture's range, so
/// [`DeterministicPorts::allocate`] steps over it instead of failing.
#[derive(Debug)]
pub struct DeterministicPorts {
    next: Mutex<u16>,
    held: Mutex<Vec<ReservedPort>>,
}

impl DeterministicPorts {
    /// Consecutive ports one [`DeterministicPorts::allocate`] call may probe.
    ///
    /// The scan is bounded so a host with no free port in the range says so
    /// instead of walking the whole port space.
    pub const SCAN_WINDOW: u16 = 64;

    /// Reserve the first *available* loopback port at or after the cursor.
    ///
    /// Ports another process already holds are skipped — that is
    /// [`io::ErrorKind::AddrInUse`], which also covers a port whose previous
    /// socket is still in `TIME_WAIT`. The returned port stays bound for the
    /// fixture lifetime, so a sibling test cannot take it, and each call returns
    /// a strictly greater port than the previous one.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::AddrNotAvailable`] when every port in the
    /// [`Self::SCAN_WINDOW`] probed from the cursor is taken, and propagates any
    /// bind failure that is not a port conflict.
    pub fn allocate(&self) -> io::Result<u16> {
        let mut cursor = self
            .next
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let start = *cursor;
        for offset in 0..Self::SCAN_WINDOW {
            let Some(candidate) = start.checked_add(offset) else {
                break;
            };
            let address = SocketAddrV4::new(Ipv4Addr::LOCALHOST, candidate);
            let listener = match TcpListener::bind(address) {
                Ok(listener) => listener,
                // Somebody else owns this port; a fixture must not inherit an
                // unrelated process's choice of ports as a failure.
                Err(error) if error.kind() == io::ErrorKind::AddrInUse => continue,
                Err(error) => return Err(error),
            };
            self.held
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(ReservedPort {
                    port: candidate,
                    listener,
                });
            // Saturating keeps the cursor inside `u16` at the top of the range;
            // the port just reserved can never be handed out twice, because this
            // fixture holds its listener until it drops.
            *cursor = candidate.saturating_add(1);
            return Ok(candidate);
        }
        Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            format!(
                "no bindable loopback port in {start}..{end} ({window} probed)",
                end = start.saturating_add(Self::SCAN_WINDOW),
                window = Self::SCAN_WINDOW
            ),
        ))
    }
}

/// Fixture-local deterministic clock, port reservations, and temp-root namespace.
#[derive(Debug)]
pub struct DeterministicFixture {
    root: PathBuf,
    fixture_id: SafeField,
    seed: u64,
    next_root: AtomicU64,
    clock: DeterministicClock,
    ports: DeterministicPorts,
}

impl DeterministicFixture {
    /// Create an isolated fixture without mutating process environment or globals.
    ///
    /// `first_port` seeds the port cursor. Allocation begins at or after it; an
    /// exact port number is not part of the contract, see [`DeterministicPorts`].
    pub fn new(
        parent: &Path,
        fixture_id: &str,
        seed: u64,
        first_port: u16,
    ) -> Result<Self, FixtureError> {
        let fixture_id = SafeField::new(fixture_id)?;
        let root = parent.join(format!("{}-{seed}", fixture_id.as_str()));
        std::fs::create_dir_all(&root).map_err(FixtureError::Io)?;
        Ok(Self {
            root,
            fixture_id,
            seed,
            next_root: AtomicU64::new(0),
            clock: DeterministicClock {
                next_millis: AtomicU64::new(seed),
            },
            ports: DeterministicPorts {
                next: Mutex::new(first_port),
                held: Mutex::new(Vec::new()),
            },
        })
    }

    /// Fixture-local deterministic clock.
    #[must_use]
    pub fn clock(&self) -> &DeterministicClock {
        &self.clock
    }

    /// Fixture-local port allocator with live reservations.
    #[must_use]
    pub fn ports(&self) -> &DeterministicPorts {
        &self.ports
    }

    /// Create the next deterministic, collision-free root below the fixture.
    pub fn temp_root(&self, label: &str) -> Result<PathBuf, FixtureError> {
        let label = SafeField::new(label)?;
        let index = self.next_root.fetch_add(1, Ordering::Relaxed);
        let path = self.root.join(format!(
            "{}-{}-{}-{index:04}",
            self.fixture_id.as_str(),
            self.seed,
            label.as_str()
        ));
        std::fs::create_dir(&path).map_err(FixtureError::Io)?;
        Ok(path)
    }
}

/// Deterministic fixture setup failure.
#[derive(Debug, thiserror::Error)]
pub enum FixtureError {
    /// A fixture identifier could escape or forge its namespace.
    #[error("unsafe fixture identifier")]
    UnsafeIdentifier(#[from] SafeFieldError),
    /// Fixture filesystem setup failed.
    #[error("fixture setup failed")]
    Io(#[source] io::Error),
}

#[derive(Clone, Default)]
struct CapturedWriter(Arc<Mutex<Vec<u8>>>);

impl CapturedWriter {
    fn bytes(&self) -> Vec<u8> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

struct CapturedGuard(Arc<Mutex<Vec<u8>>>);

impl Write for CapturedGuard {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'writer> MakeWriter<'writer> for CapturedWriter {
    type Writer = CapturedGuard;

    fn make_writer(&'writer self) -> Self::Writer {
        CapturedGuard(Arc::clone(&self.0))
    }
}

/// In-memory protocol/diagnostic capture with separate stdout and stderr sinks.
pub struct CapturedProtocolHarness {
    stdout: CapturedWriter,
    stderr: CapturedWriter,
    dispatch: Dispatch,
}

impl Default for CapturedProtocolHarness {
    fn default() -> Self {
        Self::new()
    }
}

impl CapturedProtocolHarness {
    /// Create a harness whose tracing subscriber can write only to stderr capture.
    #[must_use]
    pub fn new() -> Self {
        let stdout = CapturedWriter::default();
        let stderr = CapturedWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_ansi(false)
            .with_env_filter(EnvFilter::new("trace"))
            .with_writer(stderr.clone())
            .finish();
        Self {
            stdout,
            stderr,
            dispatch: Dispatch::new(subscriber),
        }
    }

    /// Run code with this fixture's local subscriber, avoiding global races.
    pub fn run<Output>(&self, operation: impl FnOnce() -> Output) -> Output {
        tracing::dispatcher::with_default(&self.dispatch, operation)
    }

    /// Write one already-framed protocol line to the protocol-only stdout sink.
    pub fn protocol_frame(&self, frame: &str) {
        let mut guard = self.stdout.make_writer();
        guard
            .write_all(frame.as_bytes())
            .and_then(|()| guard.write_all(b"\n"))
            .expect("in-memory protocol capture cannot fail");
    }

    /// Captured protocol bytes.
    #[must_use]
    pub fn stdout(&self) -> Vec<u8> {
        self.stdout.bytes()
    }

    /// Captured structured diagnostic bytes.
    #[must_use]
    pub fn stderr(&self) -> Vec<u8> {
        self.stderr.bytes()
    }
}

#[cfg(test)]
mod harness_audit {
    //! The harness must expose behaviour and observations, never authority. A
    //! harness that could hand a test a permit or a repair proof would let the
    //! acceptance suite "authorize" work without going through the production
    //! gates, which would make those gates decorative.

    const SCHEDULER_HARNESS: &str = include_str!("test_support/scheduler_harness.rs");

    fn production_source(source: &str) -> &str {
        source
            .find("#[cfg(test)]")
            .map_or(source, |index| &source[..index])
    }

    #[test]
    fn the_scheduler_harness_never_returns_a_permit() {
        let source = production_source(SCHEDULER_HARNESS);
        for banned in [
            "-> EffectPermit",
            "-> PolicyPermit",
            "-> Result<EffectPermit",
            "-> Result<PolicyPermit",
            "-> Option<EffectPermit",
            "-> Option<PolicyPermit",
        ] {
            assert!(
                !source.contains(banned),
                "the harness must never hand a test a permit value ({banned})"
            );
        }
    }

    #[test]
    fn the_scheduler_harness_mints_a_repair_proof_only_from_the_recovery_path() {
        let source = production_source(SCHEDULER_HARNESS);
        assert_eq!(
            source.matches("RepairProof::sealed").count(),
            1,
            "exactly one repair-proof mint site may exist in the harness"
        );
        assert!(
            source.contains("OperatorRecoveryAuthority::new"),
            "the proof must come from the real recovery authority"
        );
        assert!(
            source.contains("apply_repair"),
            "the proof must be minted from a re-derived typed proof, never fabricated"
        );
    }

    #[test]
    fn the_scheduler_harness_never_reports_which_element_mismatched() {
        let source = production_source(SCHEDULER_HARNESS);
        for banned in [
            "mismatched_dimension",
            "which_dimension",
            "-> TupleDimension",
        ] {
            assert!(
                !source.contains(banned),
                "the harness must not surface a per-dimension cause ({banned})"
            );
        }
    }
}

#[cfg(test)]
mod deterministic_ports_contract {
    //! A fixture is deterministic in the order it hands out ports, not in which
    //! numbers it gets. The loopback range belongs to the whole host, so a port
    //! inside the fixture's range can already be owned by an unrelated process;
    //! that is ambient state, not a test failure, and the allocator has to absorb
    //! it while still keeping its reservations and its monotonic order.

    use std::io;
    use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};

    use super::{DeterministicFixture, DeterministicPorts};

    fn bind(port: u16) -> io::Result<TcpListener> {
        TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
    }

    fn is_reserved(port: u16) -> bool {
        bind(port).is_err()
    }

    #[test]
    fn allocate_steps_over_a_port_another_process_already_holds() {
        let intruder = bind(0).expect("an ephemeral loopback port must be bindable");
        let occupied = intruder
            .local_addr()
            .expect("a bound listener has a local address")
            .port();

        let sandbox = tempfile::tempdir().expect("sandbox");
        let fixture = DeterministicFixture::new(sandbox.path(), "ports-occupied", 1, occupied)
            .expect("fixture");

        let allocated = fixture
            .ports()
            .allocate()
            .expect("a first port owned by another process must not fail the fixture");

        assert!(
            allocated > occupied,
            "allocate must step over the occupied port {occupied}, got {allocated}"
        );
        assert!(
            is_reserved(allocated),
            "allocated port {allocated} must stay reserved for the fixture lifetime"
        );
        assert_eq!(
            intruder
                .local_addr()
                .expect("a bound listener has a local address")
                .port(),
            occupied,
            "the intruder must still hold {occupied} while the assertions run"
        );
    }

    #[test]
    fn allocated_ports_increase_monotonically_and_stay_reserved() {
        const FIRST: u16 = 41_700;

        let sandbox = tempfile::tempdir().expect("sandbox");
        let fixture = DeterministicFixture::new(sandbox.path(), "ports-monotonic", 3, FIRST)
            .expect("fixture");

        let mut previous: Option<u16> = None;
        for _ in 0..3 {
            let port = fixture.ports().allocate().expect("allocate");
            assert!(
                port >= FIRST,
                "allocation must not go below the seeded cursor {FIRST}, got {port}"
            );
            if let Some(previous) = previous {
                assert!(
                    port > previous,
                    "ports must increase strictly, got {port} after {previous}"
                );
            }
            assert!(
                is_reserved(port),
                "allocated port {port} must stay reserved for the fixture lifetime"
            );
            previous = Some(port);
        }
    }

    #[test]
    fn allocate_reports_an_exhausted_window_instead_of_scanning_on() {
        const FIRST: u16 = 41_800;

        let mut occupied = Vec::new();
        for offset in 0..DeterministicPorts::SCAN_WINDOW {
            let port = FIRST + offset;
            match bind(port) {
                Ok(listener) => occupied.push(listener),
                // A port we cannot take is already owned by somebody else, which
                // serves this test's premise just as well as one we hold.
                Err(error) if error.kind() == io::ErrorKind::AddrInUse => {}
                Err(error) => panic!("unexpected bind failure on {port}: {error}"),
            }
        }

        let sandbox = tempfile::tempdir().expect("sandbox");
        let fixture = DeterministicFixture::new(sandbox.path(), "ports-exhausted", 5, FIRST)
            .expect("fixture");

        let error = fixture
            .ports()
            .allocate()
            .expect_err("a fully occupied window must be reported, not scanned past");

        assert_eq!(
            error.kind(),
            io::ErrorKind::AddrNotAvailable,
            "exhaustion must be an addressing failure, got {error}"
        );
        let message = error.to_string();
        for expected in [
            FIRST.to_string(),
            (FIRST + DeterministicPorts::SCAN_WINDOW).to_string(),
        ] {
            assert!(
                message.contains(&expected),
                "the error must name the window it probed ({expected}), got {message}"
            );
        }
        assert!(
            !occupied.is_empty(),
            "the window must be held by live listeners while the assertion runs"
        );
    }
}
