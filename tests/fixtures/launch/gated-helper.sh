#!/bin/sh
# A stand-in for the supervised Godot process.
#
# It is deliberately *not* gate-aware: gating is the launch protocol's job, not
# the target program's. The bundled `godot-mcp-supervisor barrier` blocks and then
# execs this script in place, so the fact that none of the side effects below have
# happened yet is itself the proof that no user code ran before the gate opened.
#
# Every observable effect is optional and explicit, so a test can assert exactly
# which of them occurred:
#   --evidence <path> --payload <text>   write payload to path (hash-checkable)
#   --env-dump <path>                    dump the received environment
#   --stdout-bytes <n>                   emit n bytes on stdout
#   --stderr-bytes <n>                   emit n bytes on stderr
#   --exit-code <n>                      exit with n
#   --linger-ms <n>                      stay alive past a short deadline

evidence=""
payload=""
env_dump=""
exit_code=0
stdout_bytes=0
stderr_bytes=0
linger_ms=0

while [ $# -gt 0 ]; do
  case "$1" in
    --evidence)
      evidence="$2"
      shift 2
      ;;
    --payload)
      payload="$2"
      shift 2
      ;;
    --env-dump)
      env_dump="$2"
      shift 2
      ;;
    --exit-code)
      exit_code="$2"
      shift 2
      ;;
    --stdout-bytes)
      stdout_bytes="$2"
      shift 2
      ;;
    --stderr-bytes)
      stderr_bytes="$2"
      shift 2
      ;;
    --linger-ms)
      linger_ms="$2"
      shift 2
      ;;
    *)
      printf 'gated-helper: unexpected argument %s\n' "$1" >&2
      exit 64
      ;;
  esac
done

if [ -n "$evidence" ]; then
  printf '%s' "$payload" >"$evidence"
fi

if [ -n "$env_dump" ]; then
  env >"$env_dump"
fi

emit() {
  count="$1"
  i=0
  while [ "$i" -lt "$count" ]; do
    printf 'x'
    i=$((i + 1))
  done
}

if [ "$stdout_bytes" -gt 0 ]; then
  emit "$stdout_bytes"
fi

if [ "$stderr_bytes" -gt 0 ]; then
  emit "$stderr_bytes" >&2
fi

if [ "$linger_ms" -gt 0 ]; then
  # Deliberately outlive a short deadline so the supervisor's wall-clock budget,
  # not the child, decides when the launch ends.
  sleep $((linger_ms / 1000 + 1))
fi

exit "$exit_code"
