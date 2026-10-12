//! The Windows Job Object strong boundary.
//!
//! A Job Object is a named kernel object that exists before any process is
//! assigned to it, and `JobObjectBasicProcessIdList` enumerates its members from
//! the job name alone — including from a restarted process that did not create
//! it. That is exactly the recovery anchor `intent_fsynced` records.
//!
//! Every job this module creates is configured with, in one
//! `SetInformationJobObject` call:
//!
//! * [`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`] — closing the last handle kills every
//!   member, so a crashed supervisor cannot leave the job running;
//! * **no** `JOB_OBJECT_LIMIT_BREAKAWAY_OK` and **no**
//!   `JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK` — a child can never leave the job;
//! * `JOB_OBJECT_LIMIT_ACTIVE_PROCESS`, `JOB_OBJECT_LIMIT_JOB_MEMORY` and a hard
//!   CPU rate cap.
//!
//! This module is compiled only on Windows. The Unix-hosted development
//! environment verifies it by cross-compiling (`--target x86_64-pc-windows-msvc`)
//! at check level; a native Windows run is required to exercise it.

#![cfg_attr(
    windows,
    allow(
        unsafe_code,
        reason = "Job Object containment has no safe binding; every call site below is audited and \
                  checks its return value before using any output"
    )
)]

use super::boundary::{
    BoundaryError, BoundaryIdentity, BoundaryKind, BoundaryLimits, BoundaryProvider,
    ContainmentBoundary,
};

/// Maximum member processes a launch job permits.
pub const DEFAULT_ACTIVE_PROCESS_LIMIT: u32 = 64;

/// Maximum committed memory a launch job permits, in bytes.
pub const DEFAULT_JOB_MEMORY_LIMIT: u64 = 2 * 1024 * 1024 * 1024;

/// Hard CPU cap a launch job enforces, in hundredths of a percent of *one*
/// processor: 20000 = 200% = two full cores. Windows takes the cap as a share
/// of the whole machine instead; see [`cpu_rate_for`].
pub const DEFAULT_CPU_RATE: u32 = 20_000;

/// `CpuRate` for [`DEFAULT_CPU_RATE`] on a machine with `processors` logical
/// processors. Windows reads `CpuRate` as cycles per 10,000 of *every*
/// processor's time and refuses a value above 10,000 or of 0, so two cores'
/// worth is 20,000 divided by the processor count, capped at the whole machine
/// and never 0.
#[must_use]
pub fn cpu_rate_for(processors: usize) -> u32 {
    let processors = u32::try_from(processors.max(1)).unwrap_or(u32::MAX);
    (DEFAULT_CPU_RATE / processors).clamp(1, 10_000)
}

/// A boundary provider backed by Windows Job Objects.
#[derive(Debug, Clone)]
pub struct JobObjectProvider {
    name_prefix: String,
}

impl JobObjectProvider {
    /// Bind a provider to one Job Object name prefix.
    #[must_use]
    pub const fn new(name_prefix: String) -> Self {
        Self { name_prefix }
    }

    /// The job name for one transaction. A Job Object name is a kernel object
    /// name, so it must not contain a path separator.
    #[must_use]
    pub fn job_name(&self, transaction_id: &str) -> String {
        let token: String = transaction_id
            .chars()
            .filter(|character| character.is_ascii_alphanumeric())
            .collect();
        format!("{}-launch-{token}", self.name_prefix)
    }
}

impl BoundaryProvider for JobObjectProvider {
    fn kind(&self) -> BoundaryKind {
        BoundaryKind::WindowsJobObject
    }

    fn available(&self) -> Result<(), BoundaryError> {
        imp::available()
    }

    fn create(&self, transaction_id: &str) -> Result<Box<dyn ContainmentBoundary>, BoundaryError> {
        self.available()?;
        imp::create(self.job_name(transaction_id))
    }

    fn reopen(
        &self,
        identity: &BoundaryIdentity,
    ) -> Result<Option<Box<dyn ContainmentBoundary>>, BoundaryError> {
        if identity.kind != BoundaryKind::WindowsJobObject {
            return Ok(None);
        }
        imp::reopen(identity)
    }
}

/// The `job:<name>` identity spelling recorded in the durable payload.
#[must_use]
pub fn identity_of(name: &str) -> BoundaryIdentity {
    BoundaryIdentity {
        kind: BoundaryKind::WindowsJobObject,
        identity: format!("job:{name}"),
        control_endpoint: None,
        control_nonce: None,
        control_socket_inode: None,
    }
}

/// The job name carried by a recorded identity.
#[must_use]
pub fn name_of(identity: &BoundaryIdentity) -> String {
    identity
        .identity
        .strip_prefix("job:")
        .unwrap_or(&identity.identity)
        .to_owned()
}

/// The limits every launch job enforces, as the canonical limit line.
#[must_use]
pub fn enforced_limits() -> BoundaryLimits {
    BoundaryLimits {
        raw: format!(
            "pids={DEFAULT_ACTIVE_PROCESS_LIMIT} memory={DEFAULT_JOB_MEMORY_LIMIT} \
             cpu={} no-breakaway=true kill-on-close=true",
            DEFAULT_CPU_RATE / 100
        ),
        no_breakaway: true,
        kill_on_close: true,
        max_processes: Some(u64::from(DEFAULT_ACTIVE_PROCESS_LIMIT)),
        max_memory_bytes: Some(DEFAULT_JOB_MEMORY_LIMIT),
        cpu_percent: Some(DEFAULT_CPU_RATE / 100),
    }
}

#[cfg(windows)]
mod imp {
    use std::os::windows::io::AsRawHandle as _;
    use std::process::{Command, Stdio};
    use std::ptr::null;
    use std::time::Duration;

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob,
        JOB_OBJECT_CPU_RATE_CONTROL_ENABLE, JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
        JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_JOB_MEMORY,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_BASIC_PROCESS_ID_LIST,
        JOBOBJECT_CPU_RATE_CONTROL_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectBasicProcessIdList, JobObjectCpuRateControlInformation,
        JobObjectExtendedLimitInformation, OpenJobObjectW, QueryInformationJobObject,
        SetInformationJobObject, TerminateJobObject,
    };
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    use super::super::boundary::{
        Anchor, ContainmentState, GatedChild, MemberObservation, SpawnPlan, WaitOutcome,
    };
    use super::super::{gate, procfs};
    use super::{
        BoundaryError, BoundaryIdentity, BoundaryLimits, ContainmentBoundary,
        DEFAULT_ACTIVE_PROCESS_LIMIT, DEFAULT_JOB_MEMORY_LIMIT, cpu_rate_for, enforced_limits,
        identity_of, name_of,
    };

    /// `JOB_OBJECT_ALL_ACCESS`. `windows-sys` does not expose the composite
    /// constant, so it is spelled out from the documented value.
    const JOB_OBJECT_ALL_ACCESS: u32 = 0x001F_001F;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn last_error() -> String {
        std::io::Error::last_os_error().to_string()
    }

    /// An owned Job Object handle. `Drop` closes it, and because every job is
    /// created with kill-on-close, dropping the last handle is what terminates a
    /// job whose launch failed.
    struct OwnedJob(HANDLE);

    impl Drop for OwnedJob {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: `self.0` is a handle this type exclusively owns, obtained
                // from a checked `CreateJobObjectW`/`OpenJobObjectW`, and closed
                // exactly once here.
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
    }

    pub(super) fn available() -> Result<(), BoundaryError> {
        Ok(())
    }

    fn configure(job: HANDLE) -> Result<(), BoundaryError> {
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        // No breakaway flag is ever set, so a member can never leave the job.
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
            | JOB_OBJECT_LIMIT_JOB_MEMORY;
        limits.BasicLimitInformation.ActiveProcessLimit = DEFAULT_ACTIVE_PROCESS_LIMIT;
        limits.JobMemoryLimit = usize::try_from(DEFAULT_JOB_MEMORY_LIMIT).unwrap_or(usize::MAX);
        // SAFETY: `job` is a live Job Object handle and `limits` is a correctly
        // sized, fully initialised `JOBOBJECT_EXTENDED_LIMIT_INFORMATION`.
        let ok = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()).unwrap_or(0),
            )
        };
        if ok == 0 {
            return Err(BoundaryError::Create(format!(
                "SetInformationJobObject(extended limits) failed: {}",
                last_error()
            )));
        }

        // `CpuRate` lives in an anonymous union, so it cannot be set in the
        // struct initializer alongside the named flags.
        let mut cpu = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION {
            ControlFlags: JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
            ..Default::default()
        };
        cpu.Anonymous.CpuRate = cpu_rate_for(
            std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get),
        );
        // SAFETY: `job` is a live Job Object handle and `cpu` is a correctly sized,
        // fully initialised `JOBOBJECT_CPU_RATE_CONTROL_INFORMATION`.
        let ok = unsafe {
            SetInformationJobObject(
                job,
                JobObjectCpuRateControlInformation,
                (&raw const cpu).cast(),
                u32::try_from(size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>()).unwrap_or(0),
            )
        };
        if ok == 0 {
            return Err(BoundaryError::Create(format!(
                "SetInformationJobObject(cpu rate) failed: {}",
                last_error()
            )));
        }
        Ok(())
    }

    pub(super) fn create(name: String) -> Result<Box<dyn ContainmentBoundary>, BoundaryError> {
        let wide_name = wide(&name);
        // SAFETY: `wide_name` is a NUL-terminated UTF-16 buffer that outlives the
        // call; a null security-attributes pointer requests the default descriptor.
        let handle = unsafe { CreateJobObjectW(null(), wide_name.as_ptr()) };
        // SAFETY: `GetLastError` only reads this thread's last-error value, which
        // the call above just set and nothing has overwritten yet.
        let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        if handle.is_null() {
            return Err(BoundaryError::Create(format!(
                "CreateJobObjectW({name}) failed: {}",
                last_error()
            )));
        }
        let job = OwnedJob(handle);
        if existed {
            // `CreateJobObjectW` hands back an existing job of the same name
            // instead of failing. That job is another launch's: joining it would
            // count its members as this launch's and terminate them with it.
            return Err(BoundaryError::Create(
                "a Job Object of this launch's name already exists".to_owned(),
            ));
        }
        configure(job.0)?;
        Ok(Box::new(JobBoundary {
            identity: identity_of(&name),
            job,
        }))
    }

    pub(super) fn reopen(
        identity: &BoundaryIdentity,
    ) -> Result<Option<Box<dyn ContainmentBoundary>>, BoundaryError> {
        let name = name_of(identity);
        let wide_name = wide(&name);
        // SAFETY: `wide_name` is a NUL-terminated UTF-16 buffer that outlives the
        // call. A null return means the named job does not exist.
        let handle = unsafe { OpenJobObjectW(JOB_OBJECT_ALL_ACCESS, 0, wide_name.as_ptr()) };
        if handle.is_null() {
            return Ok(None);
        }
        Ok(Some(Box::new(JobBoundary {
            identity: identity.clone(),
            job: OwnedJob(handle),
        })))
    }

    struct JobBoundary {
        identity: BoundaryIdentity,
        job: OwnedJob,
    }

    impl JobBoundary {
        fn member_pids(&self) -> Result<Vec<u32>, BoundaryError> {
            // The variable-length list is queried into a buffer sized for the
            // configured process limit, so one query always suffices.
            const CAPACITY: usize = DEFAULT_ACTIVE_PROCESS_LIMIT as usize;
            #[repr(C)]
            struct Buffer {
                header: JOBOBJECT_BASIC_PROCESS_ID_LIST,
                rest: [usize; CAPACITY],
            }
            let mut buffer = Buffer {
                header: JOBOBJECT_BASIC_PROCESS_ID_LIST::default(),
                rest: [0; CAPACITY],
            };
            let mut returned = 0_u32;
            // SAFETY: `buffer` is a live, correctly sized allocation whose layout
            // matches the documented variable-length structure, and the length passed
            // is exactly its size.
            let ok = unsafe {
                QueryInformationJobObject(
                    self.job.0,
                    JobObjectBasicProcessIdList,
                    (&raw mut buffer).cast(),
                    u32::try_from(size_of::<Buffer>()).unwrap_or(0),
                    &raw mut returned,
                )
            };
            if ok == 0 {
                return Err(BoundaryError::Enumerate(format!(
                    "QueryInformationJobObject(process id list) failed: {}",
                    last_error()
                )));
            }
            let count = buffer.header.NumberOfProcessIdsInList as usize;
            let mut pids = Vec::with_capacity(count);
            // The first ID lives in the header's one-element array; the remainder
            // follows contiguously in `rest`.
            for index in 0..count.min(CAPACITY + 1) {
                let raw = if index == 0 {
                    buffer.header.ProcessIdList[0]
                } else {
                    buffer.rest[index - 1]
                };
                if let Ok(pid) = u32::try_from(raw) {
                    pids.push(pid);
                }
            }
            Ok(pids)
        }
    }

    impl ContainmentBoundary for JobBoundary {
        fn identity(&self) -> &BoundaryIdentity {
            &self.identity
        }

        fn limits(&self) -> Result<BoundaryLimits, BoundaryError> {
            Ok(enforced_limits())
        }

        fn members(&self) -> Result<Vec<MemberObservation>, BoundaryError> {
            let mut observed = Vec::new();
            for pid in self.member_pids()? {
                // The job can still list a member whose process has just
                // exited; it executes nothing and never will again, so it is
                // not one. A member that cannot be observed stays, fail-closed.
                let start_time = match procfs::process_identity(pid) {
                    Ok(None) => continue,
                    Ok(Some(identity)) => identity.start_time,
                    Err(_) => None,
                };
                observed.push(MemberObservation {
                    pid,
                    start_time,
                    gated: false,
                });
            }
            Ok(observed)
        }

        fn state(&self) -> Result<ContainmentState, BoundaryError> {
            match self.members() {
                Ok(members) if members.is_empty() => Ok(ContainmentState::Empty),
                Ok(members) => Ok(ContainmentState::Contained(members)),
                Err(error) => Ok(ContainmentState::Unprovable(error.to_string())),
            }
        }

        fn terminate_all(&self) -> Result<(), BoundaryError> {
            // SAFETY: `self.job.0` is a live Job Object handle owned by this value.
            let ok = unsafe { TerminateJobObject(self.job.0, 1) };
            if ok == 0 {
                return Err(BoundaryError::NotProvenEmpty(format!(
                    "TerminateJobObject failed: {}",
                    last_error()
                )));
            }
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                match self.state()? {
                    ContainmentState::Empty => return Ok(()),
                    ContainmentState::Unprovable(reason) => {
                        return Err(BoundaryError::NotProvenEmpty(reason));
                    }
                    ContainmentState::Contained(members) => {
                        if std::time::Instant::now() >= deadline {
                            return Err(BoundaryError::NotProvenEmpty(format!(
                                "job still contains {} member(s) after termination",
                                members.len()
                            )));
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }
            }
        }

        fn close(&self) -> Result<(), BoundaryError> {
            if self.state()? != ContainmentState::Empty {
                return Err(BoundaryError::NotProvenEmpty(
                    "refusing to close a job while a member remains".to_owned(),
                ));
            }
            Ok(())
        }

        fn create_anchor(&self, plan: &SpawnPlan) -> Result<Anchor, BoundaryError> {
            gate::create(&plan.gate_path)?;
            let stdout = gate::capture_file(&plan.stdout_path)?;
            let stderr = gate::capture_file(&plan.stderr_path)?;
            let argv = gate::barrier_argv(plan);
            let child = Command::new(&argv[0])
                .args(&argv[1..])
                .env_clear()
                .envs(plan.environment.iter().cloned())
                .current_dir(&plan.work_dir)
                .stdin(Stdio::null())
                .stdout(Stdio::from(stdout))
                .stderr(Stdio::from(stderr))
                .spawn()
                .map_err(|error| BoundaryError::Spawn(error.to_string()))?;
            let handle = child.as_raw_handle() as HANDLE;
            // SAFETY: `handle` is the live process handle `std` owns for `child`, and
            // `self.job.0` is a live Job Object handle. Assignment happens while the
            // child is still blocked at its barrier, so it cannot run user code
            // outside the job.
            let ok = unsafe { AssignProcessToJobObject(self.job.0, handle) };
            if ok == 0 {
                let reason = last_error();
                let mut child = child;
                let _ = child.kill();
                return Err(BoundaryError::Spawn(format!(
                    "AssignProcessToJobObject failed: {reason}"
                )));
            }
            Ok(Anchor {
                pid: child.id(),
                start_time: None,
                process_group: None,
                gate_path: plan.gate_path.clone(),
                child: Some(child),
                link: None,
                gated: None,
            })
        }

        fn create_gated_child(&self, anchor: &mut Anchor) -> Result<GatedChild, BoundaryError> {
            let identity = gate::wait_for_barrier(anchor.pid, Duration::from_secs(10))
                .map_err(|error| BoundaryError::Spawn(error.to_string()))?
                .ok_or_else(|| {
                    BoundaryError::Spawn(
                        "the gated child never reached its barrier before it disappeared"
                            .to_owned(),
                    )
                })?;
            anchor.start_time = identity.start_time.clone();
            let gated = GatedChild {
                pid: anchor.pid,
                start_time: identity.start_time,
            };
            anchor.gated = Some(gated.clone());
            Ok(gated)
        }

        fn confirm_membership(&self, pid: u32) -> Result<bool, BoundaryError> {
            // SAFETY: the PID is passed by value and the returned handle is closed
            // below on every path.
            let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
            if process.is_null() || process == INVALID_HANDLE_VALUE {
                return Ok(false);
            }
            let mut member = 0_i32;
            // SAFETY: `process` and `self.job.0` are live handles, and `member` is a
            // live `BOOL` out-parameter.
            let ok = unsafe { IsProcessInJob(process, self.job.0, &raw mut member) };
            // SAFETY: `process` was obtained from the checked `OpenProcess` above and
            // is closed exactly once here.
            unsafe {
                CloseHandle(process);
            }
            if ok == 0 {
                return Err(BoundaryError::Enumerate(format!(
                    "IsProcessInJob failed: {}",
                    last_error()
                )));
            }
            Ok(member != 0)
        }

        fn release(&self, anchor: &mut Anchor) -> Result<(), BoundaryError> {
            gate::open(&anchor.gate_path)
        }

        fn wait(
            &self,
            anchor: &mut Anchor,
            deadline: Duration,
        ) -> Result<WaitOutcome, BoundaryError> {
            let Some(child) = anchor.child.as_mut() else {
                return Err(BoundaryError::Wait(
                    "the job boundary has no waitable anchor".to_owned(),
                ));
            };
            gate::wait_for_child(child, deadline, || {
                let _ = self.terminate_all();
            })
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::{BoundaryError, BoundaryIdentity, ContainmentBoundary};

    fn off_platform() -> BoundaryError {
        BoundaryError::Unavailable("Job Object containment exists only on Windows".to_owned())
    }

    pub(super) fn available() -> Result<(), BoundaryError> {
        Err(off_platform())
    }

    pub(super) fn create(_name: String) -> Result<Box<dyn ContainmentBoundary>, BoundaryError> {
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

    #[test]
    fn a_job_name_is_a_kernel_object_name_not_a_path() {
        let provider = JobObjectProvider::new("godot-mcp".to_owned());
        let name = provider.job_name("00000000-0000-4000-8000-000000000141");
        assert_eq!(name, "godot-mcp-launch-00000000000040008000000000000141");
        assert!(!name.contains('\\') && !name.contains('/'));
    }

    #[test]
    fn the_identity_round_trips_through_the_job_prefix() {
        let identity = identity_of("godot-mcp-launch-1");
        assert_eq!(identity.identity, "job:godot-mcp-launch-1");
        assert_eq!(name_of(&identity), "godot-mcp-launch-1");
        assert_eq!(identity.kind, BoundaryKind::WindowsJobObject);
    }

    /// `CpuRate` is a share of every processor's cycles, per 10,000, and Windows
    /// refuses anything above 10,000 (100% of the machine) or of 0 with
    /// `ERROR_INVALID_PARAMETER`. Two cores' worth is therefore 20,000 divided
    /// by the processor count, and at most the whole machine.
    #[test]
    fn the_cpu_cap_is_two_cores_as_a_share_of_the_whole_machine() {
        for (processors, rate) in [
            (0, 10_000),
            (1, 10_000),
            (2, 10_000),
            (4, 5_000),
            (8, 2_500),
            (32, 625),
            (64, 312),
        ] {
            assert_eq!(cpu_rate_for(processors), rate, "{processors} processors");
        }
        assert_eq!(
            cpu_rate_for(usize::MAX),
            1,
            "never 0, which Windows refuses"
        );
    }

    #[test]
    fn every_launch_job_enforces_no_breakaway_and_kill_on_close() {
        let limits = enforced_limits();
        assert!(limits.contains_untrusted_code());
        assert_eq!(limits.max_processes, Some(64));
        assert_eq!(limits.cpu_percent, Some(200));
    }

    #[cfg(not(windows))]
    #[test]
    fn job_objects_are_never_claimed_as_available_off_windows() {
        let provider = JobObjectProvider::new("godot-mcp".to_owned());
        assert_eq!(
            provider.strength(),
            super::super::boundary::BoundaryStrength::Strong
        );
        assert!(matches!(
            provider.available(),
            Err(BoundaryError::Unavailable(_))
        ));
    }
}
