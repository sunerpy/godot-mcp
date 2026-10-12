//! The Linux delegated cgroup v2 boundary: weak containment.
//!
//! A cgroup is a stable, named object that exists before any process joins it,
//! and `cgroup.procs` makes membership enumerable from the cgroup path alone —
//! including by a restarted process that did not create it. That is exactly what
//! `intent_fsynced` needs as a recovery anchor.
//!
//! Kill-on-close is provided by `cgroup.kill`, which terminates every member
//! (including descendants) in one write, and the launch cgroup's limits bound
//! what its members use. It does **not** prevent breakaway: a member runs as the
//! user the sub-tree is delegated to, owns every `cgroup.procs` of that
//! sub-tree, and can move itself into another cgroup of it. The boundary is
//! therefore weak and never contains untrusted code; the limits line says so.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::boundary::{
    Anchor, BoundaryError, BoundaryIdentity, BoundaryKind, BoundaryLimits, BoundaryProvider,
    ContainmentBoundary, ContainmentState, GatedChild, MemberObservation, SpawnPlan, WaitOutcome,
};
use super::gate;
use super::procfs;

/// Maximum tasks a launch cgroup permits. The pids controller counts every
/// thread, not only every process, and one windowed Godot game already runs
/// about a hundred threads on a 32-CPU host (a worker pool and a software
/// rasterizer sized to the CPU count), so the bound is set to stop a runaway
/// fork, not to fit one engine's threads.
pub const DEFAULT_MAX_TASKS: u64 = 4096;

/// Maximum committed memory a launch cgroup permits, in bytes.
pub const DEFAULT_MAX_MEMORY_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// CPU rate cap a launch cgroup permits, as a percentage of one CPU-second.
pub const DEFAULT_CPU_PERCENT: u32 = 200;

/// A boundary provider backed by a cgroup v2 sub-tree the operator delegated.
#[derive(Debug, Clone)]
pub struct CgroupV2Provider {
    root: PathBuf,
}

impl CgroupV2Provider {
    /// Bind a provider to one delegated cgroup v2 directory.
    #[must_use]
    pub const fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn cgroup_dir(&self, transaction_id: &str) -> PathBuf {
        let token: String = transaction_id
            .chars()
            .filter(|character| character.is_ascii_alphanumeric())
            .collect();
        self.root.join(format!("launch-{token}"))
    }
}

impl BoundaryProvider for CgroupV2Provider {
    fn kind(&self) -> BoundaryKind {
        BoundaryKind::CgroupV2
    }

    fn available(&self) -> Result<(), BoundaryError> {
        if !cfg!(target_os = "linux") {
            return Err(BoundaryError::Unavailable(
                "cgroup v2 containment exists only on Linux".to_owned(),
            ));
        }
        if !procfs::observations_available() {
            return Err(BoundaryError::Unavailable(
                "this platform cannot make the live process observations containment proof needs"
                    .to_owned(),
            ));
        }
        // The launch root is a boundary locator, so no reason below names it.
        if !self.root.join("cgroup.controllers").is_file() {
            return Err(BoundaryError::Unavailable(
                "the launch root is not a delegated cgroup v2 directory".to_owned(),
            ));
        }
        // Delegation is proven by actually creating and removing a probe cgroup:
        // a readable directory that refuses `mkdir` is not a usable boundary.
        let probe = self.root.join("godot-mcp-delegation-probe");
        let _ = std::fs::remove_dir(&probe);
        std::fs::create_dir(&probe).map_err(|error| {
            BoundaryError::Unavailable(format!(
                "the launch root's cgroup v2 directory is not delegated to this user: {error}"
            ))
        })?;
        let _ = std::fs::remove_dir(&probe);
        Ok(())
    }

    fn create(&self, transaction_id: &str) -> Result<Box<dyn ContainmentBoundary>, BoundaryError> {
        self.available()?;
        let dir = self.cgroup_dir(transaction_id);
        std::fs::create_dir(&dir).map_err(|error| BoundaryError::Create(error.to_string()))?;
        write_limit(&dir, "pids.max", &DEFAULT_MAX_TASKS.to_string())?;
        write_limit(&dir, "memory.max", &DEFAULT_MAX_MEMORY_BYTES.to_string())?;
        write_limit(
            &dir,
            "cpu.max",
            &format!("{} 100000", u64::from(DEFAULT_CPU_PERCENT) * 1000),
        )?;
        Ok(Box::new(CgroupBoundary {
            identity: BoundaryIdentity {
                kind: BoundaryKind::CgroupV2,
                identity: dir.display().to_string(),
                control_endpoint: None,
                control_nonce: None,
                control_socket_inode: None,
            },
            dir,
        }))
    }

    fn reopen(
        &self,
        identity: &BoundaryIdentity,
    ) -> Result<Option<Box<dyn ContainmentBoundary>>, BoundaryError> {
        if identity.kind != BoundaryKind::CgroupV2 {
            return Ok(None);
        }
        let dir = PathBuf::from(&identity.identity);
        if !dir.join("cgroup.procs").is_file() {
            return Ok(None);
        }
        Ok(Some(Box::new(CgroupBoundary {
            identity: identity.clone(),
            dir,
        })))
    }
}

fn write_limit(dir: &Path, file: &str, value: &str) -> Result<(), BoundaryError> {
    // A controller the parent has not enabled is not an error the launch can fix,
    // but it must not be reported as an enforced limit either, so the limit line
    // reflects only what was actually written.
    std::fs::write(dir.join(file), value)
        .map_err(|error| BoundaryError::Create(format!("{file}: {error}")))
}

struct CgroupBoundary {
    identity: BoundaryIdentity,
    dir: PathBuf,
}

impl CgroupBoundary {
    fn member_pids(&self) -> Result<Vec<u32>, BoundaryError> {
        let text = match std::fs::read_to_string(self.dir.join("cgroup.procs")) {
            Ok(text) => text,
            // The kernel removes a cgroup only once it has no member, so one
            // that has gone — its delegated parent was cleaned up after the
            // last member died — is proven empty.
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && matches!(self.dir.try_exists(), Ok(false)) =>
            {
                return Ok(Vec::new());
            }
            Err(error) => return Err(BoundaryError::Enumerate(error.to_string())),
        };
        Ok(text
            .lines()
            .filter_map(|line| line.trim().parse::<u32>().ok())
            .collect())
    }
}

impl ContainmentBoundary for CgroupBoundary {
    fn identity(&self) -> &BoundaryIdentity {
        &self.identity
    }

    fn limits(&self) -> Result<BoundaryLimits, BoundaryError> {
        let read = |file: &str| {
            std::fs::read_to_string(self.dir.join(file))
                .ok()
                .map(|value| value.trim().to_owned())
        };
        let pids = read("pids.max").and_then(|value| value.parse::<u64>().ok());
        let memory = read("memory.max").and_then(|value| value.parse::<u64>().ok());
        let cpu = read("cpu.max")
            .and_then(|value| {
                value
                    .split_whitespace()
                    .next()
                    .and_then(|quota| quota.parse::<u64>().ok())
            })
            .map(|quota| u32::try_from(quota / 1000).unwrap_or(u32::MAX));
        // `cgroup.kill` is what makes kill-on-close real; without it the boundary
        // cannot claim to terminate descendants, so it is not reported as enforced.
        let kill_on_close = self.dir.join("cgroup.kill").is_file();
        Ok(BoundaryLimits {
            raw: format!(
                "pids={} memory={} cpu={} no-breakaway=false kill-on-close={kill_on_close}",
                pids.map_or_else(|| "max".to_owned(), |value| value.to_string()),
                memory.map_or_else(|| "max".to_owned(), |value| value.to_string()),
                cpu.map_or_else(|| "max".to_owned(), |value| value.to_string()),
            ),
            no_breakaway: false,
            kill_on_close,
            max_processes: pids,
            max_memory_bytes: memory,
            cpu_percent: cpu,
        })
    }

    fn members(&self) -> Result<Vec<MemberObservation>, BoundaryError> {
        let mut observed = Vec::new();
        for pid in self.member_pids()? {
            if let Some(identity) = procfs::process_identity(pid)
                .map_err(|error| BoundaryError::Enumerate(error.to_string()))?
            {
                observed.push(MemberObservation {
                    pid,
                    start_time: identity.start_time.clone(),
                    gated: identity.is_gated_barrier(),
                });
            }
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
        let kill = self.dir.join("cgroup.kill");
        if kill.is_file() {
            std::fs::write(&kill, "1")
                .map_err(|error| BoundaryError::NotProvenEmpty(error.to_string()))?;
        } else {
            for pid in self.member_pids()? {
                procfs::kill_pid(pid)
                    .map_err(|error| BoundaryError::NotProvenEmpty(error.to_string()))?;
            }
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
                            "cgroup {} still contains {} member(s) after termination",
                            self.dir.display(),
                            members.len()
                        )));
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }

    fn close(&self) -> Result<(), BoundaryError> {
        if !self.dir.exists() {
            return Ok(());
        }
        if self.state()? != ContainmentState::Empty {
            return Err(BoundaryError::NotProvenEmpty(format!(
                "refusing to close cgroup {} while a member remains",
                self.dir.display()
            )));
        }
        std::fs::remove_dir(&self.dir)
            .map_err(|error| BoundaryError::NotProvenEmpty(error.to_string()))
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
                    "the gated child never reached its barrier before it disappeared".to_owned(),
                )
            })?;
        // The barrier joins its cgroup before it blocks, so its exec is visible
        // a moment before its membership is. Its gate is still closed, so
        // waiting for the membership too lets nothing run outside the cgroup.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.member_pids()?.contains(&anchor.pid) {
            let present = procfs::process_identity(anchor.pid)
                .map_err(|error| BoundaryError::Spawn(error.to_string()))?
                .is_some();
            if !present {
                return Err(BoundaryError::Spawn(
                    "the gated child exited before it joined its cgroup".to_owned(),
                ));
            }
            if Instant::now() >= deadline {
                return Err(BoundaryError::Spawn(format!(
                    "the gated child never joined cgroup {}",
                    self.dir.display()
                )));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        anchor.start_time = identity.start_time.clone();
        let gated = GatedChild {
            pid: anchor.pid,
            start_time: identity.start_time,
        };
        anchor.gated = Some(gated.clone());
        Ok(gated)
    }

    fn confirm_membership(&self, pid: u32) -> Result<bool, BoundaryError> {
        Ok(self.member_pids()?.contains(&pid))
    }

    fn release(&self, anchor: &mut Anchor) -> Result<(), BoundaryError> {
        gate::open(&anchor.gate_path)
    }

    fn wait(&self, anchor: &mut Anchor, deadline: Duration) -> Result<WaitOutcome, BoundaryError> {
        let Some(child) = anchor.child.as_mut() else {
            return Err(BoundaryError::Wait(
                "the cgroup boundary has no waitable anchor".to_owned(),
            ));
        };
        gate::wait_for_child(child, deadline, || {
            let _ = self.terminate_all();
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_undelegated_cgroup_root_is_never_an_available_boundary() {
        let provider = CgroupV2Provider::new(PathBuf::from("/sys/fs/cgroup/godot-mcp-absent"));
        assert_eq!(
            provider.strength(),
            super::super::boundary::BoundaryStrength::Weak
        );
        assert!(matches!(
            provider.available(),
            Err(BoundaryError::Unavailable(_))
        ));
    }

    #[test]
    fn the_cgroup_path_is_the_whole_identity() {
        let provider = CgroupV2Provider::new(PathBuf::from("/sys/fs/cgroup/godot"));
        assert_eq!(
            provider.cgroup_dir("00000000-0000-4000-8000-000000000141"),
            PathBuf::from("/sys/fs/cgroup/godot/launch-00000000000040008000000000000141")
        );
    }
}
