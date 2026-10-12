//! The trusted-Unix weak-boundary path: a private durable **locator**, then a
//! blocked minimal supervisor, then a gated child.
//!
//! An empty POSIX process group cannot be pre-created, because a process group
//! requires a member process. This path therefore never claims to pre-create one.
//! What it pre-creates is an inert private *boundary locator*, before any process
//! of any kind exists:
//!
//! 1. a pre-bound, descriptor-relative private control endpoint (a Unix-domain
//!    socket pair, so the endpoint never appears in the filesystem namespace and
//!    cannot be connected to by anything that did not inherit it);
//! 2. a fresh 256-bit nonce;
//! 3. the planned transaction ID.
//!
//! The supervisor's end's socket identity (inode on Linux, kernel socket handle on
//! macOS) plus that nonce are the locator's durable identity, fsynced as the
//! `intent_fsynced` payload while only inert state exists. After a crash a restarted
//! process asks the OS which live process holds *that exact* socket **and** carries
//! *that exact* nonce — authenticated peer credentials — not scanning process groups.
//!
//! A bare PGID is never an identity or a proof here. The supervisor's process
//! group is recorded only as part of the (PID, start time, PGID, nonce, endpoint)
//! tuple, and every one of those must match exactly before a live process is
//! trusted as the recorded supervisor.
//!
//! This mode explicitly cannot contain a deliberate detach by the supervised
//! process, and retains supervisor-`SIGKILL` residual risk after release: if the
//! supervisor is itself `SIGKILL`ed after release, its process group offers no
//! further guarantee. See [`WEAK_RESIDUAL_RISKS`](super::boundary::WEAK_RESIDUAL_RISKS).

use std::path::PathBuf;

use super::boundary::{
    BoundaryError, BoundaryIdentity, BoundaryKind, BoundaryProvider, ContainmentBoundary,
};
use super::procfs;

/// Environment variable carrying the control nonce to the supervisor. Its
/// presence in a live process's environment is half of the authenticated peer
/// credential; holding the recorded socket inode is the other half.
pub const NONCE_ENVIRONMENT_VARIABLE: &str = "GODOT_MCP_LAUNCH_NONCE";

/// Bytes of the control nonce. 256 bits, as the protocol requires.
pub const NONCE_BYTES: usize = 32;

/// A boundary provider implementing the trusted-Unix locator protocol.
#[derive(Debug, Clone)]
pub struct UnixLocatorProvider {
    state_dir: PathBuf,
    supervisor_binary: PathBuf,
}

impl UnixLocatorProvider {
    /// Bind a provider to one private state directory and the bundled supervisor.
    #[must_use]
    pub const fn new(state_dir: PathBuf, supervisor_binary: PathBuf) -> Self {
        Self {
            state_dir,
            supervisor_binary,
        }
    }

    #[cfg(unix)]
    fn locator_dir(&self, transaction_id: &str) -> PathBuf {
        let token: String = transaction_id
            .chars()
            .filter(|character| character.is_ascii_alphanumeric())
            .collect();
        self.state_dir.join(format!("locator-{token}"))
    }
}

impl BoundaryProvider for UnixLocatorProvider {
    fn kind(&self) -> BoundaryKind {
        BoundaryKind::UnixLocator
    }

    fn available(&self) -> Result<(), BoundaryError> {
        if !cfg!(unix) {
            return Err(BoundaryError::Unavailable(
                "the Unix locator protocol exists only on Unix hosts".to_owned(),
            ));
        }
        if !procfs::observations_available() {
            return Err(BoundaryError::Unavailable(
                "this platform cannot make the live process observations the locator protocol \
                 needs to prove or disprove a peer"
                    .to_owned(),
            ));
        }
        // The reason is public, so it names the executable and never where this
        // server is installed.
        if !self.supervisor_binary.is_file() {
            return Err(BoundaryError::Unavailable(format!(
                "the bundled `{}` executable is not beside this server",
                self.supervisor_binary.file_name().map_or_else(
                    || "godot-mcp-supervisor".into(),
                    |name| name.to_string_lossy()
                )
            )));
        }
        std::fs::create_dir_all(&self.state_dir)
            .map_err(|error| BoundaryError::Unavailable(error.to_string()))
    }

    fn create(&self, transaction_id: &str) -> Result<Box<dyn ContainmentBoundary>, BoundaryError> {
        self.available()?;
        imp::create(self, transaction_id)
    }

    fn reopen(
        &self,
        identity: &BoundaryIdentity,
    ) -> Result<Option<Box<dyn ContainmentBoundary>>, BoundaryError> {
        if identity.kind != BoundaryKind::UnixLocator {
            return Ok(None);
        }
        imp::reopen(identity)
    }
}

/// A fresh 256-bit nonce, lowercase hex.
#[cfg(unix)]
fn fresh_nonce() -> String {
    // The nonce only needs to be unpredictable to another *local* process that did
    // not inherit the endpoint, and it is never a secret at rest, so it is derived
    // from the OS random source when available and from a high-resolution,
    // per-process-unique mix otherwise.
    let mut bytes = [0_u8; NONCE_BYTES];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut file| std::io::Read::read_exact(&mut file, &mut bytes))
        .is_err()
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let mix = super::journal::hex_sha256(
            format!(
                "{}:{}:{:?}",
                std::process::id(),
                now.as_nanos(),
                std::thread::current().id()
            )
            .as_bytes(),
        );
        for (slot, byte) in bytes.iter_mut().zip(mix.as_bytes()) {
            *slot = *byte;
        }
    }
    let mut hex = String::with_capacity(NONCE_BYTES * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    hex
}

#[cfg(unix)]
mod imp {
    use std::io::{BufRead, BufReader, Write};
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use super::super::boundary::{
        Anchor, AnchorLink, BoundaryLimits, ContainmentState, GatedChild, MemberObservation,
        SpawnPlan, WaitOutcome,
    };
    use super::super::gate;
    use super::{
        BoundaryError, BoundaryIdentity, BoundaryKind, ContainmentBoundary,
        NONCE_ENVIRONMENT_VARIABLE, UnixLocatorProvider, fresh_nonce, procfs,
    };

    /// One pre-bound control endpoint: the parent's end plus the supervisor's end,
    /// waiting to be inherited. The endpoint's durable *identity* (its inode) lives
    /// on the [`BoundaryIdentity`], because that is what survives a restart.
    struct PreBoundEndpoint {
        ours: UnixStream,
        /// The supervisor's end, handed over exactly once. The parent must give up
        /// its own copy as the supervisor inherits it: while the parent still holds a
        /// descriptor onto that end, the socket has a live local peer, so a dead
        /// supervisor would never produce end of file and the parent could not tell a
        /// lost supervisor from a slow one.
        theirs: std::cell::RefCell<Option<UnixStream>>,
    }

    /// Pre-create the inert locator: no process of any kind exists afterwards.
    pub(super) fn create(
        provider: &UnixLocatorProvider,
        transaction_id: &str,
    ) -> Result<Box<dyn ContainmentBoundary>, BoundaryError> {
        let directory = provider.locator_dir(transaction_id);
        std::fs::create_dir_all(&directory)
            .map_err(|error| BoundaryError::Create(error.to_string()))?;
        let (ours, theirs) =
            UnixStream::pair().map_err(|error| BoundaryError::Create(error.to_string()))?;
        let inode = procfs::socket_identity(std::os::fd::AsFd::as_fd(&theirs))
            .map_err(|error| BoundaryError::Create(error.to_string()))?;
        let nonce = fresh_nonce();
        // The locator file is inert reclaimable state: a reservation with no process
        // behind it. A crash before `intent_fsynced` leaves at most this.
        let record = format!("transaction={transaction_id}\nnonce={nonce}\ninode={inode}\n");
        std::fs::write(directory.join("locator"), record)
            .map_err(|error| BoundaryError::Create(error.to_string()))?;
        Ok(Box::new(Locator {
            identity: BoundaryIdentity {
                kind: BoundaryKind::UnixLocator,
                identity: directory.display().to_string(),
                control_endpoint: Some(format!("socketpair:inode={inode}")),
                control_nonce: Some(nonce),
                control_socket_inode: Some(inode),
            },
            endpoint: Some(PreBoundEndpoint {
                ours,
                theirs: std::cell::RefCell::new(Some(theirs)),
            }),
        }))
    }

    /// Reopen a locator from its durable identity alone, as a restarted process
    /// must. Such a locator can inspect and clean up, but never spawn: the
    /// pre-bound endpoint it would have handed over died with the previous process.
    ///
    /// The directory is the locator's record, not its boundary: the boundary is
    /// the supervisor holding the recorded socket. A directory removed by
    /// someone else (a project state directory deleted under a running server)
    /// therefore means the boundary is gone only when no authenticated peer is
    /// left; otherwise the locator is reopened so the launch can still be ended.
    pub(super) fn reopen(
        identity: &BoundaryIdentity,
    ) -> Result<Option<Box<dyn ContainmentBoundary>>, BoundaryError> {
        let locator = Locator {
            identity: identity.clone(),
            endpoint: None,
        };
        if !PathBuf::from(&identity.identity).is_dir() && locator.authenticated_peer()?.is_none() {
            return Ok(None);
        }
        Ok(Some(Box::new(locator)))
    }

    struct Locator {
        identity: BoundaryIdentity,
        endpoint: Option<PreBoundEndpoint>,
    }

    impl Locator {
        fn nonce(&self) -> Result<&str, BoundaryError> {
            self.identity.control_nonce.as_deref().ok_or_else(|| {
                BoundaryError::Enumerate("the locator record carries no control nonce".to_owned())
            })
        }

        fn socket_inode(&self) -> Result<u64, BoundaryError> {
            self.identity.control_socket_inode.ok_or_else(|| {
                BoundaryError::Enumerate(
                    "the locator record carries no control socket identity".to_owned(),
                )
            })
        }

        /// The live process holding an authenticated peer connection to this exact
        /// pre-bound endpoint, if any.
        ///
        /// Authentication is the conjunction of two independent OS observations: the
        /// process holds a descriptor onto the recorded socket inode, and its
        /// environment carries the recorded nonce. Neither alone is accepted.
        fn authenticated_peer(&self) -> Result<Option<procfs::ProcessIdentity>, BoundaryError> {
            let inode = self.socket_inode()?;
            let nonce = self.nonce()?;
            let pids =
                procfs::live_pids().map_err(|error| BoundaryError::Enumerate(error.to_string()))?;
            for pid in pids {
                let holds = procfs::holds_socket_inode(pid, inode)
                    .map_err(|error| BoundaryError::Enumerate(error.to_string()))?;
                if !holds {
                    continue;
                }
                let carries = procfs::environment_carries(pid, NONCE_ENVIRONMENT_VARIABLE, nonce)
                    .map_err(|error| BoundaryError::Enumerate(error.to_string()))?;
                if !carries {
                    continue;
                }
                if let Some(identity) = procfs::process_identity(pid)
                    .map_err(|error| BoundaryError::Enumerate(error.to_string()))?
                {
                    return Ok(Some(identity));
                }
            }
            Ok(None)
        }

        fn group_members(&self, pgid: u32) -> Result<Vec<MemberObservation>, BoundaryError> {
            let pids =
                procfs::live_pids().map_err(|error| BoundaryError::Enumerate(error.to_string()))?;
            let mut observed = Vec::new();
            for pid in pids {
                if let Some(identity) = procfs::process_identity(pid)
                    .map_err(|error| BoundaryError::Enumerate(error.to_string()))?
                    && identity.process_group == Some(pgid)
                {
                    observed.push(MemberObservation {
                        pid,
                        gated: identity.is_gated_barrier(),
                        start_time: identity.start_time,
                    });
                }
            }
            Ok(observed)
        }
    }

    impl ContainmentBoundary for Locator {
        fn identity(&self) -> &BoundaryIdentity {
            &self.identity
        }

        fn limits(&self) -> Result<BoundaryLimits, BoundaryError> {
            // Honest by construction: the weak path enforces neither no-breakaway nor
            // kill-on-close, which is exactly why an untrusted launch can never use it.
            Ok(BoundaryLimits {
                raw: "no-breakaway=false kill-on-close=false residual-risk=deliberate_detach,\
                      post_release_supervisor_sigkill"
                    .to_owned(),
                ..BoundaryLimits::default()
            })
        }

        fn members(&self) -> Result<Vec<MemberObservation>, BoundaryError> {
            let Some(peer) = self.authenticated_peer()? else {
                return Ok(Vec::new());
            };
            match peer.process_group {
                Some(pgid) => self.group_members(pgid),
                None => Ok(vec![MemberObservation {
                    pid: peer.pid,
                    gated: peer.is_gated_barrier(),
                    start_time: peer.start_time,
                }]),
            }
        }

        fn state(&self) -> Result<ContainmentState, BoundaryError> {
            match self.members() {
                Ok(members) if members.is_empty() => Ok(ContainmentState::Empty),
                Ok(members) => Ok(ContainmentState::Contained(members)),
                Err(error) => Ok(ContainmentState::Unprovable(error.to_string())),
            }
        }

        fn adopt_recorded_member(
            &self,
            pid: u32,
            start_time: Option<&str>,
            process_group: Option<u32>,
        ) -> Result<Option<MemberObservation>, BoundaryError> {
            let Some(peer) = self.authenticated_peer()? else {
                return Ok(None);
            };
            if process_group.is_some() && peer.process_group != process_group {
                return Err(BoundaryError::NotProvenEmpty(format!(
                    "the authenticated supervisor's process group does not match the durable record \
                     (observed pid {} start {:?} pgid {:?}, recorded pgid {process_group:?})",
                    peer.pid, peer.start_time, peer.process_group
                )));
            }
            let observed = if peer.pid == pid {
                Some(peer)
            } else {
                procfs::process_identity(pid)
                    .map_err(|error| BoundaryError::Enumerate(error.to_string()))?
            };
            let Some(observed) = observed else {
                return Ok(None);
            };
            if !observed.matches_recorded(pid, start_time, process_group) {
                return Err(BoundaryError::NotProvenEmpty(format!(
                    "the authenticated supervisor's process group contains pid {pid}, but its \
                     PID/start-time/PGID tuple does not match the durable record (observed start \
                     {:?} pgid {:?}, recorded start {start_time:?} pgid {process_group:?})",
                    observed.start_time, observed.process_group
                )));
            }
            Ok(Some(MemberObservation {
                pid: observed.pid,
                gated: observed.is_gated_barrier(),
                start_time: observed.start_time,
            }))
        }

        fn terminate_all(&self) -> Result<(), BoundaryError> {
            if let Some(peer) = self.authenticated_peer()? {
                let pgid = peer.process_group.unwrap_or(peer.pid);
                procfs::kill_process_group(pgid)
                    .map_err(|error| BoundaryError::NotProvenEmpty(error.to_string()))?;
                procfs::kill_pid(peer.pid)
                    .map_err(|error| BoundaryError::NotProvenEmpty(error.to_string()))?;
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match self.state()? {
                    ContainmentState::Empty => return Ok(()),
                    ContainmentState::Unprovable(reason) => {
                        return Err(BoundaryError::NotProvenEmpty(reason));
                    }
                    ContainmentState::Contained(members) => {
                        if Instant::now() >= deadline {
                            return Err(BoundaryError::NotProvenEmpty(format!(
                                "the supervisor's process group still has {} live member(s)",
                                members.len()
                            )));
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }
            }
        }

        fn terminate_recorded_group(&self, process_group: u32) -> Result<(), BoundaryError> {
            procfs::kill_process_group(process_group)
                .map_err(|error| BoundaryError::NotProvenEmpty(error.to_string()))?;
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let remaining = self.group_members(process_group)?;
                if remaining.is_empty() {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err(BoundaryError::NotProvenEmpty(format!(
                        "process group {process_group} still has {} live member(s)",
                        remaining.len()
                    )));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        fn close(&self) -> Result<(), BoundaryError> {
            let directory = PathBuf::from(&self.identity.identity);
            if !directory.exists() {
                return Ok(());
            }
            if self.state()? != ContainmentState::Empty {
                return Err(BoundaryError::NotProvenEmpty(
                    "refusing to reclaim a locator whose endpoint still has a live peer".to_owned(),
                ));
            }
            std::fs::remove_dir_all(&directory)
                .map_err(|error| BoundaryError::NotProvenEmpty(error.to_string()))
        }

        /// Spawn the minimal bundled supervisor into a brand-new process group, with
        /// the pre-bound control endpoint inherited as its standard input, and block
        /// until it announces itself.
        ///
        /// No supervised process exists when this returns: the supervisor is blocked
        /// on its inherited end of the control channel before it is permitted to
        /// create anything.
        fn create_anchor(&self, plan: &SpawnPlan) -> Result<Anchor, BoundaryError> {
            let nonce = self.nonce()?.to_owned();
            let endpoint = self.endpoint.as_ref().ok_or_else(|| {
                BoundaryError::Control(
                    "this locator was reopened from a durable record and cannot spawn a new \
                     supervisor"
                        .to_owned(),
                )
            })?;
            let inherited = endpoint.theirs.borrow_mut().take().ok_or_else(|| {
                BoundaryError::Control(
                    "the pre-bound control endpoint was already consumed".to_owned(),
                )
            })?;
            gate::create(&plan.gate_path)?;

            let mut arguments = vec![
                "supervise".to_owned(),
                "--gate".to_owned(),
                plan.gate_path.display().to_string(),
                "--child-stdout".to_owned(),
                plan.stdout_path.display().to_string(),
                "--child-stderr".to_owned(),
                plan.stderr_path.display().to_string(),
                "--".to_owned(),
            ];
            arguments.extend(gate::barrier_argv(plan));

            let child = Command::new(&plan.barrier_binary)
                .args(&arguments)
                .env_clear()
                .envs(plan.environment.iter().cloned())
                .env(NONCE_ENVIRONMENT_VARIABLE, &nonce)
                .current_dir(&plan.work_dir)
                // The control endpoint is inherited as fd 0. The protocol's own stdout
                // is never inherited: the supervisor's own stdout/stderr go to capture
                // files, and the supervised child gets its own.
                .stdin(Stdio::from(OwnedFd::from(inherited)))
                .stdout(Stdio::from(gate::capture_file(
                    &plan.stdout_path.with_extension("supervisor"),
                )?))
                .stderr(Stdio::from(gate::capture_file(
                    &plan.stderr_path.with_extension("supervisor"),
                )?))
                .spawn()
                .map_err(|error| BoundaryError::Spawn(error.to_string()))?;

            let stream = endpoint
                .ours
                .try_clone()
                .map_err(|error| BoundaryError::Control(error.to_string()))?;
            let mut link = SupervisorLink {
                reader: BufReader::new(stream),
                nonce,
                supervisor_pid: child.id(),
                process_group: None,
            };
            let announced = link.read_line(Duration::from_secs(10))?;
            let (pid, pgid) = parse_announce(&announced, &link.nonce)?;
            if pid != child.id() {
                return Err(BoundaryError::Control(format!(
                    "the supervisor announced PID {pid} but was spawned as {}",
                    child.id()
                )));
            }
            link.process_group = Some(pgid);
            let start_time = procfs::process_identity(pid)
                .map_err(|error| BoundaryError::Enumerate(error.to_string()))?
                .and_then(|identity| identity.start_time);
            Ok(Anchor {
                pid,
                start_time,
                process_group: Some(pgid),
                gate_path: plan.gate_path.clone(),
                child: Some(child),
                link: Some(Box::new(link)),
                gated: None,
            })
        }

        fn create_gated_child(&self, anchor: &mut Anchor) -> Result<GatedChild, BoundaryError> {
            let link = anchor.link.as_mut().ok_or_else(|| {
                BoundaryError::Control("the locator anchor has no control channel".to_owned())
            })?;
            let gated = link.create_gated_child()?;
            anchor.gated = Some(gated.clone());
            Ok(gated)
        }

        fn confirm_membership(&self, pid: u32) -> Result<bool, BoundaryError> {
            Ok(self.members()?.iter().any(|member| member.pid == pid))
        }

        fn release(&self, anchor: &mut Anchor) -> Result<(), BoundaryError> {
            anchor
                .link
                .as_mut()
                .ok_or_else(|| {
                    BoundaryError::Control("the locator anchor has no control channel".to_owned())
                })?
                .release()
        }

        fn wait(
            &self,
            anchor: &mut Anchor,
            deadline: Duration,
        ) -> Result<WaitOutcome, BoundaryError> {
            let outcome = anchor
                .link
                .as_mut()
                .ok_or_else(|| {
                    BoundaryError::Control("the locator anchor has no control channel".to_owned())
                })?
                .wait(deadline);
            if let Ok(outcome) = &outcome
                && outcome.timed_out
            {
                let _ = self.terminate_all();
            }
            outcome
        }
    }

    fn parse_announce(line: &str, nonce: &str) -> Result<(u32, u32), BoundaryError> {
        let mut pid = None;
        let mut pgid = None;
        let mut announced_nonce = None;
        for field in line.split_whitespace() {
            match field.split_once('=') {
                Some(("pid", value)) => pid = value.parse::<u32>().ok(),
                Some(("pgid", value)) => pgid = value.parse::<u32>().ok(),
                Some(("nonce", value)) => announced_nonce = Some(value.to_owned()),
                _ => {}
            }
        }
        if announced_nonce.as_deref() != Some(nonce) {
            return Err(BoundaryError::Control(
                "the supervisor did not present the expected control nonce".to_owned(),
            ));
        }
        match (pid, pgid) {
            (Some(pid), Some(pgid)) => Ok((pid, pgid)),
            _ => Err(BoundaryError::Control(format!(
                "the supervisor's announcement is unusable: {}",
                line.trim()
            ))),
        }
    }

    /// Whether a socket option was refused because the peer already shut the
    /// connection down: macOS answers EINVAL then, where Linux accepts the option.
    fn peer_has_shut_down(error: &std::io::Error) -> bool {
        cfg!(target_os = "macos")
            && error.raw_os_error() == Some(rustix::io::Errno::INVAL.raw_os_error())
    }

    struct SupervisorLink {
        reader: BufReader<UnixStream>,
        nonce: String,
        supervisor_pid: u32,
        process_group: Option<u32>,
    }

    impl SupervisorLink {
        fn read_line(&mut self, deadline: Duration) -> Result<String, BoundaryError> {
            if let Err(error) = self.reader.get_ref().set_read_timeout(Some(deadline)) {
                // macOS refuses a receive timeout with EINVAL once the supervisor has
                // closed its end. Nothing can block then: the read below returns what
                // the supervisor wrote before it exited, or end of file.
                if !peer_has_shut_down(&error) {
                    return Err(BoundaryError::Control(error.to_string()));
                }
            }
            let mut line = String::new();
            match self.reader.read_line(&mut line) {
                Ok(0) => Err(BoundaryError::SupervisorLost(
                    "the control channel reached end of file".to_owned(),
                )),
                Ok(_) => Ok(line),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Err(
                    BoundaryError::Wait("the control channel timed out".to_owned()),
                ),
                Err(error) => Err(BoundaryError::SupervisorLost(error.to_string())),
            }
        }

        fn send(&mut self, message: &str) -> Result<(), BoundaryError> {
            let mut stream = self.reader.get_ref();
            stream
                .write_all(message.as_bytes())
                .and_then(|()| stream.flush())
                .map_err(|error| BoundaryError::Control(error.to_string()))
        }
    }

    impl AnchorLink for SupervisorLink {
        fn create_gated_child(&mut self) -> Result<GatedChild, BoundaryError> {
            self.send("create\n")?;
            let line = self.read_line(Duration::from_secs(10))?;
            let pid = line
                .split_whitespace()
                .find_map(|field| field.strip_prefix("pid="))
                .and_then(|value| value.parse::<u32>().ok())
                .ok_or_else(|| {
                    BoundaryError::Spawn(format!(
                        "the supervisor did not report a created child: {}",
                        line.trim()
                    ))
                })?;
            let identity = gate::wait_for_barrier(pid, Duration::from_secs(10))
                .map_err(|error| BoundaryError::Spawn(error.to_string()))?
                .ok_or_else(|| {
                    BoundaryError::Spawn(
                        "the gated child never reached its barrier before it disappeared"
                            .to_owned(),
                    )
                })?;
            Ok(GatedChild {
                pid,
                start_time: identity.start_time,
            })
        }

        fn release(&mut self) -> Result<(), BoundaryError> {
            self.send(&format!("release {}\n", self.nonce))
        }

        fn wait(&mut self, deadline: Duration) -> Result<WaitOutcome, BoundaryError> {
            match self.read_line(deadline.max(Duration::from_millis(1))) {
                Ok(line) => parse_exit(&line),
                Err(BoundaryError::Wait(_)) => Ok(WaitOutcome {
                    exit_code: None,
                    signal: None,
                    timed_out: true,
                }),
                Err(error) => Err(error),
            }
        }

        fn terminate(&mut self) {
            if let Some(pgid) = self.process_group {
                let _ = procfs::kill_process_group(pgid);
            }
            let _ = procfs::kill_pid(self.supervisor_pid);
        }

        fn sigkill_supervisor(&mut self) {
            let _ = procfs::kill_pid(self.supervisor_pid);
        }
    }

    fn parse_exit(line: &str) -> Result<WaitOutcome, BoundaryError> {
        let mut exit_code = None;
        let mut signal = None;
        for field in line.split_whitespace() {
            match field.split_once('=') {
                Some(("code", value)) => exit_code = value.parse::<i32>().ok(),
                Some(("signal", value)) => {
                    signal = Some(gate::describe_signal(
                        value.parse::<i32>().unwrap_or_default(),
                    ));
                }
                _ => {}
            }
        }
        if exit_code.is_none() && signal.is_none() {
            return Err(BoundaryError::SupervisorLost(format!(
                "the supervisor's completion report is unusable: {}",
                line.trim()
            )));
        }
        Ok(WaitOutcome {
            exit_code,
            signal,
            timed_out: false,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// A supervisor that reports its child's exit and closes its end before
        /// the server reads is still heard: on macOS the receive timeout is then
        /// refused, and the report was failed as a broken control channel.
        #[test]
        fn a_report_sent_before_the_supervisor_closed_its_end_is_still_read() {
            let (ours, mut theirs) = UnixStream::pair().expect("a socket pair");
            theirs.write_all(b"exited code=0\n").expect("the report");
            drop(theirs);
            let mut link = SupervisorLink {
                reader: BufReader::new(ours),
                nonce: String::new(),
                supervisor_pid: 0,
                process_group: None,
            };
            let outcome = link
                .wait(Duration::from_secs(5))
                .expect("the report written before the close is read");
            assert_eq!(outcome.exit_code, Some(0));
            assert!(!outcome.timed_out);
            assert!(matches!(
                link.read_line(Duration::from_secs(5)),
                Err(BoundaryError::SupervisorLost(_))
            ));
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use super::{BoundaryError, BoundaryIdentity, ContainmentBoundary, UnixLocatorProvider};

    fn off_platform() -> BoundaryError {
        BoundaryError::Unavailable("the Unix locator protocol exists only on Unix hosts".to_owned())
    }

    pub(super) fn create(
        _provider: &UnixLocatorProvider,
        _transaction_id: &str,
    ) -> Result<Box<dyn ContainmentBoundary>, BoundaryError> {
        Err(off_platform())
    }

    pub(super) fn reopen(
        _identity: &BoundaryIdentity,
    ) -> Result<Option<Box<dyn ContainmentBoundary>>, BoundaryError> {
        Err(off_platform())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn the_nonce_is_two_hundred_and_fifty_six_bits_of_hex() {
        let nonce = fresh_nonce();
        assert_eq!(nonce.len(), NONCE_BYTES * 2);
        assert!(nonce.chars().all(|character| character.is_ascii_hexdigit()));
        assert_ne!(nonce, fresh_nonce(), "each launch gets a fresh nonce");
    }

    #[test]
    fn a_missing_supervisor_binary_is_never_available() {
        let provider = UnixLocatorProvider::new(
            std::env::temp_dir().join("godot-mcp-locator-test"),
            PathBuf::from("/nonexistent/godot-mcp-supervisor"),
        );
        assert_eq!(
            provider.strength(),
            super::super::boundary::BoundaryStrength::Weak
        );
        assert!(matches!(
            provider.available(),
            Err(BoundaryError::Unavailable(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_pre_created_locator_has_no_process_behind_it() {
        use super::super::boundary::{BoundaryStrength, ContainmentState};

        let dir = tempfile::tempdir().expect("tempdir");
        let provider = UnixLocatorProvider::new(
            dir.path().join("state"),
            std::env::current_exe().expect("test binary"),
        );
        let locator = provider
            .create("00000000-0000-4000-8000-000000000141")
            .expect("the locator is pre-created before any process exists");
        assert_eq!(locator.strength(), BoundaryStrength::Weak);
        let identity = locator.identity().clone();
        assert!(identity.control_nonce.is_some());
        assert!(identity.control_socket_inode.is_some());
        assert_eq!(
            locator.state().expect("state"),
            ContainmentState::Empty,
            "a freshly pre-created locator provably has no peer"
        );
        assert!(!locator.limits().expect("limits").contains_untrusted_code());
    }
}
