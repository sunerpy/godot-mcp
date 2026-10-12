//! `process_launch`-kind re-derivation.
//!
//! A content hash is never proof here. What is re-derived is exactly Todo 14's
//! typed record, cross-checked against **live** OS process and boundary
//! inspection:
//!
//! * PID **plus** platform-native start time, so a reused PID is a different
//!   process and never the recorded one;
//! * boundary membership, re-derived from the boundary's own locator rather than
//!   trusted from the proof file; and
//! * post-exit boundary/descendant emptiness, re-derived by asking whether any
//!   recorded member of the boundary is still live.
//!
//! Release status, exit code and terminating signal are read from the durable
//! record, because they are facts about the release protocol and about an
//! already-reaped child that no later live observation can recover. That is Todo
//! 14's own division: the durable marks say what the protocol did, and live
//! inspection says what is true now. Neither alone is a proof.
//!
//! # Safety is never inferred from death
//!
//! A dead process whose release history cannot be independently determined proves
//! nothing and stays `quarantined`. Only two live states prove anything — a
//! still-suspended survivor proves the recorded pre-state, and a resumed,
//! still-contained survivor proves the recorded post-state — plus one terminal
//! state: an observed clean exit with a live-proven-empty boundary.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use godot_mcp_engine::supervision::boundary::BoundaryKind;
use godot_mcp_engine::supervision::procfs;
use godot_mcp_server::operator::{ObservedProof, ObservedRelease, ProvenDisposition, RepairTarget};

use super::supplied::{SuppliedHeader, SuppliedProcess};
use super::{LiveSurvivor, Proven, RepairError, Verdict};

/// How long a proven kill waits for the process to actually disappear.
pub const TERMINATION_DEADLINE: Duration = Duration::from_secs(10);

/// How often a signalled process is re-observed.
const TERMINATION_POLL: Duration = Duration::from_millis(20);

/// One boundary member, as the boundary's own locator records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundaryMember {
    /// The member's PID.
    pub pid: u32,
    /// The member's recorded platform-native start time, when one was recorded.
    pub start_time: Option<String>,
}

/// What live inspection of one boundary reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundaryObservation {
    /// Every member the locator records.
    pub members: Vec<BoundaryMember>,
    /// The members that are still live, matched on PID **and** start time.
    pub live_members: Vec<BoundaryMember>,
}

impl BoundaryObservation {
    /// Whether the boundary currently holds no live member at all.
    #[must_use]
    pub fn is_empty_now(&self) -> bool {
        self.live_members.is_empty()
    }

    /// Whether the exact recorded identity is a recorded member of the boundary.
    #[must_use]
    pub fn contains(&self, pid: u32, start_time: &str) -> bool {
        self.members
            .iter()
            .any(|member| member.pid == pid && member.start_time.as_deref() == Some(start_time))
    }
}

/// What live inspection of one recorded process identity reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessObservation {
    /// The recorded PID.
    pub pid: u32,
    /// The recorded start time.
    pub start_time: String,
    /// Whether that exact identity is still live.
    pub live: bool,
    /// The boundary the record names, re-derived.
    pub boundary: BoundaryObservation,
    /// Whether the recorded identity is a confirmed member of that boundary.
    pub membership_confirmed: bool,
}

/// Re-derive and compare a `process_launch`-kind repair.
///
/// # Errors
/// Returns [`RepairError::Observation`] when this platform cannot make the live
/// observations at all, so the transaction stays `quarantined` rather than being
/// settled from a guess.
pub fn derive(
    target: &RepairTarget,
    header: &SuppliedHeader,
    supplied: &SuppliedProcess,
) -> Result<Verdict, RepairError> {
    let Some(ObservedProof::ProcessLaunch {
        boundary_identity,
        pid: recorded_pid,
        start_time: recorded_start_time,
        boundary_membership_confirmed: recorded_membership,
        release_status,
        exit_code,
        signal,
        boundary_empty_after_exit,
    }) = target.recorded_proof.clone()
    else {
        return Ok(Verdict::Refused(format!(
            "transaction {} records no observed process_launch proof, so there is no identity to \
             re-derive",
            target.transaction_id
        )));
    };
    let (Some(recorded_pid), Some(recorded_start_time)) = (recorded_pid, recorded_start_time)
    else {
        return Ok(Verdict::Refused(format!(
            "transaction {} records no PID/start-time pair, so no live process can ever be matched \
             to it",
            target.transaction_id
        )));
    };

    let observation = observe(&boundary_identity, recorded_pid, &recorded_start_time)?;
    if !recorded_membership {
        return Ok(Verdict::Refused(format!(
            "transaction {} recorded unconfirmed boundary membership, so an observed process can \
             never be trusted to belong to its boundary",
            target.transaction_id
        )));
    }
    if !observation.membership_confirmed {
        return Ok(Verdict::Refused(format!(
            "pid {recorded_pid} (start time {recorded_start_time}) is not a recorded member of \
             boundary {boundary_identity}"
        )));
    }

    let disposition = match (release_status, observation.live) {
        (ObservedRelease::Suspended, true) => ProvenDisposition::PreState,
        (ObservedRelease::ResumedContained, true) => ProvenDisposition::PostState,
        (ObservedRelease::ExitedClean, false)
            if exit_code == Some(0) && signal.is_none() && observation.boundary.is_empty_now() =>
        {
            ProvenDisposition::PostState
        }
        (status, live) => {
            return Ok(Verdict::Refused(format!(
                "release status {} with a {} process and a {} boundary proves neither recorded \
                 state; safety is never inferred from death",
                status.as_str(),
                if live { "live" } else { "dead" },
                if observation.boundary.is_empty_now() {
                    "provably empty"
                } else {
                    "non-empty"
                }
            )));
        }
    };
    if header.claimed_disposition != disposition {
        return Ok(Verdict::Refused(format!(
            "the proof file claims a {} state and the re-derivation proves a {} state",
            header.claimed_disposition.as_str(),
            disposition.as_str()
        )));
    }

    if supplied.observed_pid != recorded_pid {
        return Ok(Verdict::Refused(format!(
            "the proof file claims pid {} and the record names pid {recorded_pid}",
            supplied.observed_pid
        )));
    }
    if supplied.observed_start_time != recorded_start_time {
        return Ok(Verdict::Refused(format!(
            "the proof file claims start time {} and the re-derived identity is {recorded_start_time}",
            supplied.observed_start_time
        )));
    }
    if supplied.observed_boundary_membership_confirmed != observation.membership_confirmed {
        return Ok(Verdict::Refused(
            "the proof file's boundary-membership claim disagrees with the re-derived membership"
                .to_owned(),
        ));
    }
    if supplied.observed_release_status != release_status {
        return Ok(Verdict::Refused(format!(
            "the proof file claims release status {} and the record holds {}",
            supplied.observed_release_status.as_str(),
            release_status.as_str()
        )));
    }
    if supplied.observed_exit_code != exit_code {
        return Ok(Verdict::Refused(format!(
            "the proof file claims exit code {:?} and the record holds {exit_code:?}",
            supplied.observed_exit_code
        )));
    }
    if supplied.observed_boundary_empty != boundary_empty_after_exit {
        return Ok(Verdict::Refused(format!(
            "the proof file claims post-exit boundary emptiness {:?} and the record holds \
             {boundary_empty_after_exit:?}",
            supplied.observed_boundary_empty
        )));
    }

    Ok(Verdict::Proven(Proven {
        disposition,
        observed: ObservedProof::ProcessLaunch {
            boundary_identity: boundary_identity.clone(),
            pid: Some(recorded_pid),
            start_time: Some(recorded_start_time.clone()),
            boundary_membership_confirmed: observation.membership_confirmed,
            release_status,
            exit_code,
            signal,
            boundary_empty_after_exit,
        },
        detail: format!(
            "pid {recorded_pid} (start time {recorded_start_time}) is a confirmed member of \
             boundary {boundary_identity}, is {}, and its recorded release status is {}",
            if observation.live { "live" } else { "gone" },
            release_status.as_str()
        ),
        live_survivor: observation.live.then_some(LiveSurvivor {
            pid: recorded_pid,
            start_time: recorded_start_time,
        }),
    }))
}

/// Observe one recorded identity and its boundary, live.
///
/// # Errors
/// Returns [`RepairError::Observation`] when the platform cannot report process
/// identity at all, or when the boundary's own locator cannot be re-derived.
pub fn observe(
    boundary_identity: &str,
    pid: u32,
    start_time: &str,
) -> Result<ProcessObservation, RepairError> {
    if !procfs::observations_available() {
        return Err(RepairError::Observation(
            "this platform cannot observe live process identity, so a process_launch proof can \
             never be re-derived here"
                .to_owned(),
        ));
    }
    let boundary = observe_boundary(boundary_identity)?;
    let live = is_live(pid, start_time)?;
    Ok(ProcessObservation {
        pid,
        start_time: start_time.to_owned(),
        live,
        membership_confirmed: boundary.contains(pid, start_time),
        boundary,
    })
}

/// Whether the exact recorded identity still designates a live process.
///
/// # Errors
/// Returns [`RepairError::Observation`] when the platform cannot answer.
pub fn is_live(pid: u32, start_time: &str) -> Result<bool, RepairError> {
    match procfs::process_identity(pid) {
        Ok(Some(identity)) => Ok(identity.matches_recorded(pid, Some(start_time), None)),
        Ok(None) => Ok(false),
        Err(error) => Err(RepairError::Observation(format!(
            "pid {pid} could not be observed: {error}"
        ))),
    }
}

/// Re-derive one containment boundary's membership from its own locator.
///
/// The recorded identity is spelled `<kind>:<identity>` using Todo 14's canonical
/// [`BoundaryKind`] names. Only the `unix_locator` mechanism records a locator
/// directory this command can independently enumerate; every other mechanism is
/// refused rather than approximated, so an unverifiable boundary can never be
/// reported as empty.
///
/// # Errors
/// Returns [`RepairError::Observation`] when the boundary cannot be re-derived.
pub fn observe_boundary(boundary_identity: &str) -> Result<BoundaryObservation, RepairError> {
    let (kind, identity) = boundary_identity.split_once(':').ok_or_else(|| {
        RepairError::Observation(format!(
            "boundary identity {boundary_identity:?} is not spelled <kind>:<identity>"
        ))
    })?;
    let kind = BoundaryKind::parse(kind).ok_or_else(|| {
        RepairError::Observation(format!(
            "boundary identity {boundary_identity:?} names no known containment mechanism"
        ))
    })?;
    if kind != BoundaryKind::UnixLocator {
        return Err(RepairError::Observation(format!(
            "this command cannot independently enumerate a {} boundary, so its membership is \
             refused rather than assumed",
            kind.as_str()
        )));
    }
    let locator = PathBuf::from(identity);
    let recorded = read_locator_file(&locator.join("identity"))?;
    if recorded.trim() != boundary_identity {
        return Err(RepairError::Observation(format!(
            "boundary locator {} identifies itself as {:?}, not {boundary_identity:?}",
            locator.display(),
            recorded.trim()
        )));
    }
    let members = parse_members(&read_locator_file(&locator.join("members"))?);
    let mut live_members = Vec::new();
    for member in &members {
        let Some(start_time) = member.start_time.as_deref() else {
            continue;
        };
        if is_live(member.pid, start_time)? {
            live_members.push(member.clone());
        }
    }
    Ok(BoundaryObservation {
        members,
        live_members,
    })
}

fn read_locator_file(path: &Path) -> Result<String, RepairError> {
    std::fs::read_to_string(path).map_err(|error| {
        RepairError::Observation(format!(
            "the boundary locator file {} could not be read: {error}",
            path.display()
        ))
    })
}

/// Parse a locator's `members` file: one `<pid> <start-time>` pair per line, with
/// `-` standing for an unrecorded start time. A member with no recorded start
/// time can never be matched again after PID reuse, so it is never reported live.
fn parse_members(text: &str) -> Vec<BoundaryMember> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse::<u32>().ok()?;
            let start_time = match fields.next() {
                None | Some("-") => None,
                Some(value) => Some(value.to_owned()),
            };
            Some(BoundaryMember { pid, start_time })
        })
        .collect()
}

/// Terminate the exact proven survivor and prove it is gone.
///
/// Nothing is signalled unless the recorded PID **and** start time still match,
/// so an unrelated process that inherited the PID is never touched. The wait is
/// bounded, so this never hangs.
///
/// # Errors
/// Returns a diagnosable reason when the survivor could not be signalled or is
/// still live after [`TERMINATION_DEADLINE`]. The caller aborts the repair with
/// nothing journaled.
pub fn terminate(survivor: &LiveSurvivor) -> Result<(), String> {
    let live = |()| is_live(survivor.pid, &survivor.start_time).map_err(|error| error.to_string());
    if !live(())? {
        return Err(format!(
            "pid {} (start time {}) is no longer the proven survivor, so nothing was terminated",
            survivor.pid, survivor.start_time
        ));
    }
    procfs::kill_pid(survivor.pid).map_err(|error| {
        format!(
            "the proven survivor (pid {}) could not be signalled: {error}",
            survivor.pid
        )
    })?;
    let deadline = Instant::now() + TERMINATION_DEADLINE;
    loop {
        if !live(())? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "the proven survivor (pid {}) is still live {}s after being signalled",
                survivor.pid,
                TERMINATION_DEADLINE.as_secs()
            ));
        }
        std::thread::sleep(TERMINATION_POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A locator's membership list is the boundary's own record, so its parse must
    /// keep exactly the identities it names and drop nothing silently.
    #[test]
    fn a_members_file_parses_to_exactly_its_identities() {
        let members = parse_members("41 12345\n42 -\nnot-a-pid 1\n\n43 67890\n");
        assert_eq!(
            members,
            vec![
                BoundaryMember {
                    pid: 41,
                    start_time: Some("12345".to_owned())
                },
                BoundaryMember {
                    pid: 42,
                    start_time: None
                },
                BoundaryMember {
                    pid: 43,
                    start_time: Some("67890".to_owned())
                },
            ]
        );
    }

    /// Membership is matched on PID **and** start time together; either alone
    /// would accept a later reuse of the same PID.
    #[test]
    fn membership_requires_both_the_pid_and_the_start_time() {
        let observation = BoundaryObservation {
            members: parse_members("42 12345\n43 -\n"),
            live_members: Vec::new(),
        };
        assert!(observation.contains(42, "12345"));
        assert!(!observation.contains(42, "99999"));
        assert!(!observation.contains(99, "12345"));
        assert!(
            !observation.contains(43, "-"),
            "a member with no recorded start time is never a confirmed identity"
        );
        assert!(observation.is_empty_now());
    }

    /// Only the `unix_locator` mechanism can be independently enumerated here;
    /// every other spelling must be refused rather than approximated.
    #[test]
    fn only_an_enumerable_boundary_is_accepted() {
        for identity in [
            "cgroup_v2:/sys/fs/cgroup/godot-mcp",
            "windows_job_object:godot-mcp-1",
            "container_adapter:podman-1",
        ] {
            let error =
                observe_boundary(identity).expect_err("an unenumerable boundary is refused");
            assert!(
                error.to_string().contains("refused rather than assumed"),
                "{error} must fail closed"
            );
        }
        assert!(observe_boundary("no-colon").is_err());
        assert!(observe_boundary("unknown_kind:/tmp/x").is_err());
    }

    /// A locator that does not identify itself as the recorded boundary is a
    /// different boundary, and must never be enumerated as this one.
    #[test]
    fn a_locator_must_identify_itself_as_the_recorded_boundary() {
        let dir = tempfile::tempdir().expect("temp dir");
        let locator = dir.path().join("boundary");
        std::fs::create_dir_all(&locator).expect("locator dir");
        let recorded = format!("unix_locator:{}", locator.display());
        std::fs::write(locator.join("identity"), "unix_locator:/elsewhere").expect("identity");
        std::fs::write(locator.join("members"), "").expect("members");
        let error = observe_boundary(&recorded).expect_err("a foreign locator is refused");
        assert!(error.to_string().contains("identifies itself as"));

        std::fs::write(locator.join("identity"), &recorded).expect("identity");
        let observation = observe_boundary(&recorded).expect("its own locator is enumerated");
        assert!(observation.members.is_empty());
        assert!(observation.is_empty_now());
    }

    /// This process is live under its own identity and not live under a start time
    /// it never had, which is the whole PID-reuse guard in one assertion.
    #[test]
    fn liveness_is_matched_on_the_exact_recorded_identity() {
        if !procfs::observations_available() {
            return;
        }
        let own = std::process::id();
        let identity = procfs::process_identity(own)
            .expect("this process is observable")
            .expect("this process is live");
        let start_time = identity.start_time.expect("a start time is reported");
        assert!(is_live(own, &start_time).expect("answerable"));
        assert!(!is_live(own, "0").expect("answerable"));
    }
}
