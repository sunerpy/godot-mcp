# gdext reference lane — the ADR 0010 spike

This directory is the isolated, non-shipping native lane that
[ADR 0001](../docs/contributor/adr/0001-gdext-reference-only.md) reserved and
[ADR 0010](../docs/contributor/adr/0010-complete-development-bridge-strategy.md) asks to
evaluate. Nothing here is built by the repository's workspace, enters its `Cargo.lock`,
or reaches a release archive; the release-graph audit skips this directory, and the
manual `gdext-reference-lane` workflow builds it without feeding any release gate.

## What it is

One bridge read implemented with gdext: `NativeTreeSnapshot.describe_tree`, the runtime
probe's scene-tree read, describing every node exactly as `runtime_probe.gd` does. It is
built with `godot = "=0.5.5"` and its `api-4-4` feature, the lowest API level the
supported matrix needs, with no experimental Godot API. No `.gdextension` descriptor is committed: the
benchmark driver writes one into each scratch project it creates.

## Running it

```sh
CARGO_TARGET_DIR=/tmp/gdext-spike-target \
  cargo +1.96.0 build --release --manifest-path gdext-reference/Cargo.toml
python3 -I -B gdext-reference/run-bench.py \
  --library /tmp/gdext-spike-target/release/libgodot_mcp_gdext_spike.so \
  --godot /path/to/Godot_v4.4.1-stable_linux.x86_64 --godot /path/to/Godot_v4.7.1-stable_linux.x86_64
```

For each build the driver imports a scratch project once, which registers the extension,
then runs `bench/bench.gd` headless: a 10,000-node scene is described ten times by the
shipping GDScript walk, by the walk the probe shipped with before this spike measured it,
and natively, and the three snapshots must be byte-identical before any time is compared.

## Outcome

The measured results and the activation decision are recorded in ADR 0010's
"Spike outcome" section. In short, the native backend stays absent: after profiling, the
GDScript walk is faster than the native one, and the 4.5.1, 4.6.1 and 4.7.1 editors abort
during an import, or when opened and quit, with the extension in the project.
