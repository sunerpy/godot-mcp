#!/bin/sh
# The container-adapter fixture, with a `run` that forks: it registers itself,
# starts the argument array as its own registered child and waits for it, as a
# sandbox that creates its namespaces before the program does (bubblewrap, a
# container runtime) must. The gated barrier is then a member of the boundary
# without being the process the server spawned.
#
# The adapter models exactly the properties the launch protocol requires of a
# *strong* boundary, and nothing more:
#
#   * the boundary is a stable, named object that exists before any child does
#     (`create` takes an identity and makes it; no process is involved);
#   * membership is enumerable from the identity alone — after a restart, by a
#     process that did not create the boundary (`members`);
#   * every member can be terminated and the boundary re-proven empty
#     (`terminate` then `members`);
#   * closing is refused while a live member remains (kill-on-close modelled as
#     "close only proceeds when the boundary is provably empty");
#   * no member can leave the boundary (no-breakaway: the member registry *is*
#     the boundary, and `run` registers itself before it execs anything).
#
# `run` registers its own PID, starts the argument array as a child, registers
# that child and waits for it, so the caller can wait on the PID it spawned and
# finds the gated barrier among the members.
#
# Verbs:
#   create    <root> <id>               create the boundary, print its identity
#   exists    <root> <id>               exit 0 iff the boundary exists
#   run       <root> <id> -- <argv...>  register this PID, run argv as a registered child
#   members   <root> <id>               print one live member PID per line
#   terminate <root> <id>               SIGKILL every live member
#   close     <root> <id>               remove a provably empty boundary
#   limits    <root> <id>               print the enforced limits

verb="$1"
root="$2"
id="$3"
if [ -z "$verb" ] || [ -z "$root" ] || [ -z "$id" ]; then
  printf 'container-adapter: usage: <verb> <root> <id> [-- argv...]\n' >&2
  exit 64
fi
shift 3

case "$id" in
  */* | .. | .)
    printf 'container-adapter: refusing a boundary identity with a path separator\n' >&2
    exit 64
    ;;
esac

dir="$root/$id"
members="$dir/members"

# A member counts as live only if it can still execute. A zombie has already
# exited and can never execute again, so it is NOT live -- but `kill -0` succeeds
# on a zombie, so the process state is read directly: from procfs where it
# exists, and from `ps` (macOS) where it does not.
is_live() {
  pid="$1"
  kill -0 "$pid" 2>/dev/null || return 1
  if [ -r "/proc/$pid/stat" ]; then
    state=$(sed -e 's/^.*) //' -e 's/ .*//' "/proc/$pid/stat" 2>/dev/null)
  else
    state=$(ps -o stat= -p "$pid" 2>/dev/null)
  fi
  case "$state" in
    Z*) return 1 ;;
  esac
  return 0
}

live_members() {
  [ -f "$members" ] || return 0
  while IFS= read -r pid; do
    [ -n "$pid" ] || continue
    if is_live "$pid"; then
      printf '%s\n' "$pid"
    fi
  done <"$members"
}

require_boundary() {
  if [ ! -d "$dir" ]; then
    printf 'container-adapter: boundary %s does not exist\n' "$id" >&2
    exit 65
  fi
}

case "$verb" in
  create)
    if [ -d "$dir" ]; then
      printf 'container-adapter: boundary %s already exists\n' "$id" >&2
      exit 65
    fi
    mkdir -p "$dir" || exit 70
    : >"$members" || exit 70
    printf 'pids=64 memory=536870912 cpu=50 no-breakaway=true kill-on-close=true\n' >"$dir/limits"
    printf 'container:%s\n' "$id"
    ;;
  exists)
    [ -d "$dir" ] || exit 1
    ;;
  run)
    require_boundary
    if [ "$1" = "--" ]; then
      shift
    fi
    if [ $# -eq 0 ]; then
      printf 'container-adapter: run needs an argument array\n' >&2
      exit 64
    fi
    # The adapter registers itself first, then its child as soon as it exists;
    # the child is the gated barrier, which runs nothing before its gate opens.
    printf '%s\n' "$$" >>"$members"
    "$@" &
    child=$!
    printf '%s\n' "$child" >>"$members"
    wait "$child"
    exit $?
    ;;
  members)
    require_boundary
    live_members
    ;;
  terminate)
    require_boundary
    live_members | while IFS= read -r pid; do
      kill -9 "$pid" 2>/dev/null
    done
    ;;
  close)
    [ -d "$dir" ] || exit 0
    remaining=$(live_members | wc -l | tr -d ' ')
    if [ "$remaining" != "0" ]; then
      printf 'container-adapter: refusing to close boundary %s with %s live member(s)\n' \
        "$id" "$remaining" >&2
      exit 66
    fi
    rm -rf "$dir"
    ;;
  limits)
    [ -f "$dir/limits" ] || exit 65
    cat "$dir/limits"
    ;;
  *)
    printf 'container-adapter: unknown verb %s\n' "$verb" >&2
    exit 64
    ;;
esac
exit 0
