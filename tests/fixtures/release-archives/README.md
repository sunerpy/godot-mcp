Release-archive fixture for the Todo-35 release-gates harness.

The three files here stand in for the published release archive set the packaged
Linux/macOS/Windows smokes are bound to, plus the `SHA256SUMS.txt` manifest the
harness verifies them against.

They are opaque byte stand-ins, not real archives. The harness's archive binding
is a digest check — read the bytes, hash them, compare against the manifest line —
so a stand-in exercises exactly the same code path a real archive would, without
committing tens of megabytes or claiming a macOS/Windows build that this
development host cannot produce (it has no macOS SDK and no MSVC toolchain).

Their names come from `godot_mcp_cli::package::payload_root` plus each target's
archive extension, which is where the real `make dist` output gets its names, so
a change to that naming shows up here as a failing archive binding rather than as
a silently ignored mismatch.

If a digest here ever needs to change, regenerate the manifest from the bytes:

    ( cd tests/fixtures/release-archives \
      && for n in *.tar.gz *.zip; do \
           printf '%s  %s\n' "$(sha256sum "$n" | cut -d' ' -f1)" "$n"; \
         done > SHA256SUMS.txt )
