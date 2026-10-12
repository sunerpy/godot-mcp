//! The configured container/VM containment adapter (a **strong** boundary).
//!
//! The adapter is an external program the operator configures. This module owns
//! only the verb protocol, never a shell command string: every invocation is an
//! argument array. The adapter contract is exactly what a strong boundary must
//! provide, and nothing more:
//!
//! | verb | meaning |
//! | --- | --- |
//! | `create <root> <id>` | make the named boundary, with no process involved |
//! | `exists <root> <id>` | exit 0 iff the boundary exists |
//! | `run <root> <id> -- <argv…>` | join the boundary, then exec `argv` |
//! | `members <root> <id>` | print one live member PID per line |
//! | `terminate <root> <id>` | kill every live member |
//! | `close <root> <id>` | remove a provably empty boundary |
//! | `limits <root> <id>` | print the enforced limits |
//!
//! `members` is the load-bearing verb: it makes membership enumerable **from the
//! identity alone**, by a restarted process that did not create the boundary,
//! which is what lets `intent_fsynced` be a sufficient recovery anchor.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::boundary::{
    Anchor, BoundaryError, BoundaryIdentity, BoundaryKind, BoundaryLimits, BoundaryProvider,
    ContainmentBoundary, ContainmentState, GatedChild, MemberObservation, SpawnPlan, WaitOutcome,
};
use super::gate;
use super::procfs;

/// A boundary provider backed by a configured container/VM adapter program.
#[derive(Debug, Clone)]
pub struct ContainerAdapterProvider {
    adapter: PathBuf,
    root: PathBuf,
}

impl ContainerAdapterProvider {
    /// Bind a provider to one adapter program and one boundary root.
    #[must_use]
    pub const fn new(adapter: PathBuf, root: PathBuf) -> Self {
        Self { adapter, root }
    }

    fn run(&self, arguments: &[String]) -> Result<std::process::Output, BoundaryError> {
        Command::new(&self.adapter)
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|error| {
                BoundaryError::Unavailable(format!("{}: {error}", self.adapter.display()))
            })
    }

    fn verb(&self, verb: &str, id: &str) -> Vec<String> {
        vec![
            verb.to_owned(),
            self.root.display().to_string(),
            id.to_owned(),
        ]
    }
}

impl BoundaryProvider for ContainerAdapterProvider {
    fn kind(&self) -> BoundaryKind {
        BoundaryKind::ContainerAdapter
    }

    fn available(&self) -> Result<(), BoundaryError> {
        // The reason is public, so it names the setting rather than the path.
        if !self.adapter.is_file() {
            return Err(BoundaryError::Unavailable(
                "the configured container/VM adapter (`process.adapter.program`) is not an \
                 executable file"
                    .to_owned(),
            ));
        }
        if !procfs::observations_available() {
            return Err(BoundaryError::Unavailable(
                "this platform cannot make the live process observations containment proof needs"
                    .to_owned(),
            ));
        }
        std::fs::create_dir_all(&self.root).map_err(|error| {
            BoundaryError::Unavailable(format!(
                "the adapter's boundary root (`process.adapter.root`) cannot be created: {error}"
            ))
        })
    }

    fn create(&self, transaction_id: &str) -> Result<Box<dyn ContainmentBoundary>, BoundaryError> {
        self.available()?;
        let id = boundary_id(transaction_id);
        let output = self.run(&self.verb("create", &id))?;
        if !output.status.success() {
            return Err(BoundaryError::Create(format!(
                "adapter refused to create boundary {id}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let identity = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if identity.is_empty() {
            return Err(BoundaryError::Create(
                "adapter created a boundary without reporting its identity".to_owned(),
            ));
        }
        let boundary = AdapterBoundary {
            provider: self.clone(),
            id,
            identity: BoundaryIdentity {
                kind: BoundaryKind::ContainerAdapter,
                identity,
                control_endpoint: None,
                control_nonce: None,
                control_socket_inode: None,
            },
        };
        // A boundary that does not actually enforce no-breakaway + kill-on-close
        // is refused here, not silently accepted as strong.
        let limits = boundary.limits()?;
        if !limits.contains_untrusted_code() {
            let _ = boundary.close();
            return Err(BoundaryError::Create(format!(
                "adapter boundary does not enforce no-breakaway + kill-on-close: {}",
                limits.raw
            )));
        }
        Ok(Box::new(boundary))
    }

    fn reopen(
        &self,
        identity: &BoundaryIdentity,
    ) -> Result<Option<Box<dyn ContainmentBoundary>>, BoundaryError> {
        if identity.kind != BoundaryKind::ContainerAdapter {
            return Ok(None);
        }
        let id = identity
            .identity
            .strip_prefix("container:")
            .unwrap_or(&identity.identity)
            .to_owned();
        let output = self.run(&self.verb("exists", &id))?;
        if !output.status.success() {
            return Ok(None);
        }
        Ok(Some(Box::new(AdapterBoundary {
            provider: self.clone(),
            id,
            identity: identity.clone(),
        })))
    }
}

/// The boundary identity spelling: the transaction ID with its dashes stripped,
/// so it is a single path-free token the adapter can name.
fn boundary_id(transaction_id: &str) -> String {
    let token: String = transaction_id
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .collect();
    format!("tx-{token}")
}

struct AdapterBoundary {
    provider: ContainerAdapterProvider,
    id: String,
    identity: BoundaryIdentity,
}

impl AdapterBoundary {
    fn member_pids(&self) -> Result<Vec<u32>, BoundaryError> {
        let output = self
            .provider
            .run(&self.provider.verb("members", &self.id))?;
        if !output.status.success() {
            return Err(BoundaryError::Enumerate(format!(
                "adapter could not enumerate boundary {}: {}",
                self.id,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.trim().parse::<u32>().ok())
            .collect())
    }
}

impl ContainmentBoundary for AdapterBoundary {
    fn identity(&self) -> &BoundaryIdentity {
        &self.identity
    }

    fn limits(&self) -> Result<BoundaryLimits, BoundaryError> {
        let output = self.provider.run(&self.provider.verb("limits", &self.id))?;
        if !output.status.success() {
            return Err(BoundaryError::Enumerate(format!(
                "adapter could not report the limits of boundary {}",
                self.id
            )));
        }
        Ok(BoundaryLimits::parse(&String::from_utf8_lossy(
            &output.stdout,
        )))
    }

    fn members(&self) -> Result<Vec<MemberObservation>, BoundaryError> {
        let mut observed = Vec::new();
        for pid in self.member_pids()? {
            let identity = procfs::process_identity(pid)
                .map_err(|error| BoundaryError::Enumerate(error.to_string()))?;
            if let Some(identity) = identity {
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
        let output = self
            .provider
            .run(&self.provider.verb("terminate", &self.id))?;
        if !output.status.success() {
            return Err(BoundaryError::NotProvenEmpty(format!(
                "adapter could not terminate the members of boundary {}: {}",
                self.id,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        // Re-enumerate: emptiness must be proven, never assumed from a
        // successful kill.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.state()? {
                ContainmentState::Empty => return Ok(()),
                ContainmentState::Contained(members) if Instant::now() >= deadline => {
                    return Err(BoundaryError::NotProvenEmpty(format!(
                        "boundary {} still contains {} member(s) after termination",
                        self.id,
                        members.len()
                    )));
                }
                ContainmentState::Unprovable(reason) => {
                    return Err(BoundaryError::NotProvenEmpty(reason));
                }
                ContainmentState::Contained(_) => {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }

    fn close(&self) -> Result<(), BoundaryError> {
        let output = self.provider.run(&self.provider.verb("close", &self.id))?;
        if !output.status.success() {
            return Err(BoundaryError::NotProvenEmpty(format!(
                "adapter refused to close boundary {}: {}",
                self.id,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(())
    }

    fn create_anchor(&self, plan: &SpawnPlan) -> Result<Anchor, BoundaryError> {
        gate::create(&plan.gate_path)?;
        let stdout = gate::capture_file(&plan.stdout_path)?;
        let stderr = gate::capture_file(&plan.stderr_path)?;
        let mut arguments = self.provider.verb("run", &self.id);
        arguments.push("--".to_owned());
        arguments.extend(gate::barrier_argv(plan));
        let child = Command::new(&self.provider.adapter)
            .args(&arguments)
            .env_clear()
            .envs(plan.environment.iter().cloned())
            .current_dir(&plan.work_dir)
            // The protocol's own stdout is never inherited by a child.
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
        // The adapter's `run` joins the boundary and either execs the bundled
        // barrier in place, so the anchor becomes it, or starts it as a member
        // of its own, as a sandbox that creates its namespaces first must. The
        // gated child is whichever member is the barrier: the PID bound into the
        // receipt is the one that later replaces its image with user code, and
        // waiting for its marker is what makes `staged_fsynced` mean "created
        // but provably not running user code" instead of merely "spawn returned".
        let deadline = Instant::now() + Duration::from_secs(10);
        let gated = loop {
            let barriers: Vec<MemberObservation> = self
                .members()?
                .into_iter()
                .filter(|member| member.gated)
                .collect();
            match barriers.as_slice() {
                [barrier] => {
                    break GatedChild {
                        pid: barrier.pid,
                        start_time: barrier.start_time.clone(),
                    };
                }
                [] => {}
                _ => {
                    return Err(BoundaryError::Spawn(format!(
                        "boundary {} holds {} gated barriers, not one",
                        self.id,
                        barriers.len()
                    )));
                }
            }
            if let Some(child) = anchor.child.as_mut()
                && child
                    .try_wait()
                    .map_err(|error| BoundaryError::Spawn(error.to_string()))?
                    .is_some()
            {
                return Err(BoundaryError::Spawn(
                    "the adapter ended before its gated barrier joined the boundary".to_owned(),
                ));
            }
            if Instant::now() >= deadline {
                return Err(BoundaryError::Spawn(format!(
                    "no gated barrier joined boundary {} in time",
                    self.id
                )));
            }
            std::thread::sleep(gate::GATE_POLL_INTERVAL);
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
                "the adapter boundary has no waitable anchor".to_owned(),
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
    fn a_boundary_identity_is_a_single_path_free_token() {
        let id = boundary_id("00000000-0000-4000-8000-000000000141");
        assert_eq!(id, "tx-00000000000040008000000000000141");
        assert!(!id.contains('/') && !id.contains('.'));
    }

    #[test]
    fn a_missing_adapter_is_never_an_available_strong_boundary() {
        let provider = ContainerAdapterProvider::new(
            PathBuf::from("/nonexistent/godot-mcp-adapter"),
            std::env::temp_dir().join("godot-mcp-adapter-test"),
        );
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
