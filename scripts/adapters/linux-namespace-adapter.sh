#!/bin/sh
# The reference container adapter for Linux: a strong boundary built from a
# cgroup v2 directory and bubblewrap's namespaces.
#
# Each boundary is a child cgroup of the adapter's root, a cgroup v2 directory
# delegated to the server's user that holds no process of its own (its own
# process lives in a leaf beside the boundaries), so its sub-tree can use the
# pids, memory and cpu controllers. `run` joins the boundary's cgroup and then
# starts the program under bubblewrap in new user, PID, mount, IPC, UTS and
# cgroup namespaces, no nested user namespaces, a private /proc that shows only
# the launch's own processes, and an empty tmpfs over /sys/fs/cgroup. A member
# therefore never sees, and can never write, a cgroup outside its boundary, which
# is what no-breakaway means here; the files, the display and loopback are the
# host's, so Godot runs as it would outside.
#
# Verbs, as the server's adapter protocol defines them:
#   create    <root> <id>               make the boundary's cgroup, print its identity
#   exists    <root> <id>               exit 0 iff the boundary exists
#   run       <root> <id> -- <argv...>  join the boundary, start argv in its namespaces
#   members   <root> <id>               print every member PID, one per line
#   terminate <root> <id>               kill every member through cgroup.kill
#   close     <root> <id>               remove a provably empty boundary
#   limits    <root> <id>               print the enforced limits
#
# Configure it as process.adapter.program, with process.adapter.root naming the
# delegated cgroup directory (see docs/operator/process-boundaries.md).

set -u

# The limits every boundary enforces: what a windowed Godot game needs with room
# to spare, and no more (the same bounds the delegated cgroup boundary uses).
MAX_TASKS=4096
MAX_MEMORY_BYTES=2147483648
CPU_QUOTA_US=200000
CPU_PERIOD_US=100000

fail() {
  printf 'linux-namespace-adapter: %s\n' "$1" >&2
  exit "${2:-70}"
}

verb="${1:-}"
root="${2:-}"
id="${3:-}"
if [ -z "$verb" ] || [ -z "$root" ] || [ -z "$id" ]; then
  fail 'usage: <verb> <root> <id> [-- argv...]' 64
fi
shift 3
case "$id" in
  */* | . | ..) fail 'a boundary identity never holds a path separator' 64 ;;
esac
dir="$root/$id"

# Every member PID of the boundary, its nested cgroups included.
members() {
  [ -d "$dir" ] || return 0
  find "$dir" -name cgroup.procs -type f -exec cat {} + 2>/dev/null
}

case "$verb" in
  create)
    [ -f "$root/cgroup.procs" ] || fail 'the adapter root is not a cgroup v2 directory' 65
    if [ -n "$(cat "$root/cgroup.procs")" ]; then
      fail 'the adapter root holds processes of its own; keep them in a leaf beside the boundaries' 65
    fi
    for controller in pids memory cpu; do
      if ! grep -qw "$controller" "$root/cgroup.subtree_control"; then
        printf '+%s\n' "$controller" >"$root/cgroup.subtree_control" 2>/dev/null ||
          fail "the adapter root cannot give its boundaries the $controller controller" 65
      fi
    done
    mkdir "$dir" 2>/dev/null || fail "boundary $id already exists or cannot be made" 65
    if ! printf '%s\n' "$MAX_TASKS" >"$dir/pids.max" ||
      ! printf '%s\n' "$MAX_MEMORY_BYTES" >"$dir/memory.max" ||
      ! printf '%s %s\n' "$CPU_QUOTA_US" "$CPU_PERIOD_US" >"$dir/cpu.max" ||
      [ ! -f "$dir/cgroup.kill" ]; then
      rmdir "$dir" 2>/dev/null
      fail "boundary $id cannot enforce its limits and kill-on-close" 65
    fi
    printf 'container:%s\n' "$id"
    ;;
  exists)
    [ -d "$dir" ] || exit 1
    ;;
  run)
    [ -d "$dir" ] || fail "boundary $id does not exist" 65
    if [ "${1:-}" = "--" ]; then
      shift
    fi
    [ $# -gt 0 ] || fail 'run needs an argument array' 64
    # The launch runs with the environment the server allows it, which need not
    # name a PATH, so bubblewrap is named by its path.
    bwrap=/usr/bin/bwrap
    [ -x "$bwrap" ] || fail 'bubblewrap (/usr/bin/bwrap) is not installed' 69
    # Joining first: the processes bubblewrap forks below, and everything they
    # start, are inside the boundary from their first instruction.
    printf '%s\n' "$$" >"$dir/cgroup.procs" || fail "could not join boundary $id"
    exec "$bwrap" \
      --unshare-user --disable-userns \
      --unshare-pid --unshare-ipc --unshare-uts --unshare-cgroup \
      --new-session \
      --dev-bind / / \
      --tmpfs /sys/fs/cgroup \
      --proc /proc \
      -- "$@"
    ;;
  members)
    [ -d "$dir" ] || fail "boundary $id does not exist" 65
    members
    ;;
  terminate)
    [ -d "$dir" ] || fail "boundary $id does not exist" 65
    printf '1\n' >"$dir/cgroup.kill" || fail "could not terminate boundary $id"
    ;;
  close)
    [ -d "$dir" ] || exit 0
    if [ -n "$(members)" ]; then
      fail "refusing to close boundary $id while a member remains" 66
    fi
    # Nested cgroups first, the boundary itself last.
    find "$dir" -depth -type d -exec rmdir {} + 2>/dev/null
    [ ! -d "$dir" ] || fail "boundary $id could not be removed"
    ;;
  limits)
    [ -d "$dir" ] || fail "boundary $id does not exist" 65
    tasks=$(cat "$dir/pids.max")
    memory=$(cat "$dir/memory.max")
    read -r quota period <"$dir/cpu.max"
    case "$quota" in
      max) cpu=max ;;
      *) cpu=$((quota * 100 / period)) ;;
    esac
    kill_on_close=false
    [ -f "$dir/cgroup.kill" ] && kill_on_close=true
    printf 'pids=%s memory=%s cpu=%s no-breakaway=true kill-on-close=%s\n' \
      "$tasks" "$memory" "$cpu" "$kill_on_close"
    ;;
  *)
    fail "unknown verb $verb" 64
    ;;
esac
exit 0
