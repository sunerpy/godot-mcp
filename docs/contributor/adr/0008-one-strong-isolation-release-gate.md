# 0008 — One strong Docker isolation check satisfies the release plan

**Status:** accepted.

## Decision

Keep `isolation-docker-clean-room` as the single required strong-isolation
release check and remove `isolation-vm-egress-denied` from the required-check
catalogue. This is a correction of an implementation-specific overconstraint,
not an exemption from the frozen release plan.

Remove the `vm` branch from `scripts/isolation-smoke.sh` and its
`RELEASE_GATES_VM_RUNNER` workflow wiring as well. Once the catalogue entry is
gone, that interface has no production caller, does not verify virtualization or
network state itself, and would preserve a misleading dead path. The Docker-only
operator helper remains fail-closed when its real container runtime or
digest-pinned image is unavailable.

## Plan requirement

The frozen plan uses `strong Docker/VM isolation checks` at lines 77 and 437 and
the singular compound `strong-isolation` at line 501. It does not name
`isolation-vm-egress-denied`, `isolation-docker-clean-room`, `--deny-egress`,
`network=none`, `RELEASE_GATES_VM_RUNNER`, nested virtualization, QEMU or KVM.
Nothing in that text requires one Docker check and a second VM check to pass
together.

The two concrete check names and the VM-runner interface were introduced by the
Todo-35 harness implementation. Treating both members of that implementation's
array as `required: true` turned one required property into two required
mechanisms. Removing the unsupported mechanism restores the catalogue to the
plan's required property.

## Why the Docker check is the stronger proof

The retained command is unchanged:

```text
docker run --rm --network=none --read-only --cap-drop=ALL \
  --security-opt=no-new-privileges \
  --mount type=bind,source=$PWD/target/release-dist,target=/dist,readonly \
  $RELEASE_GATES_ISOLATION_IMAGE /dist/smoke.sh
```

It executes the archive's own `/dist/smoke.sh` with four independently useful
restrictions: an isolated kernel network namespace, a read-only root filesystem,
a read-only archive mount, and neither Linux capabilities nor privilege
escalation. In particular, `--network=none` establishes the no-network condition
at the container boundary rather than accepting a runner's assertion about its
environment.

The removed VM branch only checked that an externally supplied command existed,
then invoked it with `--deny-egress --archives <dir>` and trusted exit code zero.
It neither established that the command launched a virtual machine nor probed
whether egress was actually denied. Its claimed isolation therefore depended
entirely on an unspecified external runner being honest, while the Docker check
directly requests the relevant kernel restrictions.

## Ecosystem evidence

A review of eight mature Rust, Godot and MCP projects found zero required VM
isolation or egress-denied archive-validation release gates in eight repositories:

* `godot-rust/gdext`
* `godot-rust/gdnative`
* `BurntSushi/ripgrep`
* `sharkdp/bat`
* `bytecodealliance/wasmtime`
* `denoland/deno`
* `modelcontextprotocol/rust-sdk`
* `modelcontextprotocol/servers`

The `godot-rust/gdext` `.github/workflows/full-ci.yml` workflow has zero matches
for the seven surveyed isolation terms. More significantly, Wasmtime records the
following at `bytecodealliance/wasmtime/.github/workflows/main.yml:997-999`:

> ideally we would test in a system-mode QEMU VM

Wasmtime therefore documents system-mode VM testing as an unrealized ideal, not
as a release gate, despite isolation being central to that project.
This survey does not prove that VM testing has no value. It shows that making an
externally injected nested-VM runner mandatory for this archive is not an
established release prerequisite that should override the plan's mechanism-neutral
`strong-isolation` requirement.

## CI feasibility

The development host has neither `/dev/kvm` nor `qemu-system-x86_64`. The
containerized CI environment likewise provides no nested-virtualization
facility. The former workflow did not provision one; it only read the repository
variable `RELEASE_GATES_VM_RUNNER` and expected that variable to name an already
installed external command.

Consequently the removed check was structurally unsatisfiable on the available
CI runners. Writing a command that merely returned success would not repair that
defect: it would fabricate the very isolation property the release process is
supposed to prove.

## Consequences

* The catalogue retains every non-VM required check: workspace checks,
  exact-Godot matrix checks, the GPU/display check, one strong Docker isolation
  check, packaged archive smokes and the local OIDC/JWKS deployment smoke.
* `isolation-docker-clean-room` remains required, with its name, command,
  `CheckKind::Isolation` classification and fail-closed observation semantics
  unchanged.
* The compatibility workflow has one Docker isolation job and no
  `RELEASE_GATES_VM_RUNNER` reference. The committed passing fixture likewise has
  one isolation observation.
* Historical CI observations remain historical facts and are not relabeled or
  edited. Removing a catalogue requirement does not rewrite an observation's
  recorded exit code.
* The `advisory` check and `deny.toml` are unaffected. This decision provides no
  mechanism for weakening, skipping or reclassifying any other required check.

## Invalidation condition

Reopen this decision if the release plan is changed to explicitly require both a
container check and a VM check, or if the Docker check stops enforcing the
network, filesystem, capability or no-new-privileges restrictions above. A future
VM test must provision a real VM boundary and independently verify the claimed
network property; restoring the former trust-only runner interface is not
sufficient.
