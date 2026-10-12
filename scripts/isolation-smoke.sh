#!/bin/sh
# isolation-smoke.sh — prove the packaged binary behaves under strong isolation.
#
# One mode, selected by RELEASE_GATES_ISOLATION_MODE:
#
#   docker — run the published archive's smoke inside a container with no
#            network, a read-only root filesystem, all capabilities dropped and
#            no new privileges.
#
# This mode requires a real runtime this repository's development host may not
# have. If the runtime is absent, this script exits nonzero. An unavailable
# isolation runtime is never a pass and never a silent skip.
#
# Usage:
#   RELEASE_GATES_ISOLATION_MODE=docker scripts/isolation-smoke.sh

set -eu

MODE="${RELEASE_GATES_ISOLATION_MODE:-}"
ARCHIVES_DIR="${RELEASE_GATES_ARCHIVES_DIR:-target/release-dist}"

die() {
    echo "isolation-smoke: FAIL: $*" >&2
    exit 1
}

while [ $# -gt 0 ]; do
    case "$1" in
        --mode) MODE="${2:-}"; shift 2 ;;
        --archives-dir) ARCHIVES_DIR="${2:-}"; shift 2 ;;
        *) die "unknown argument '$1'" ;;
    esac
done

[ -n "$MODE" ] || die "RELEASE_GATES_ISOLATION_MODE (or --mode) must be 'docker'"
[ -d "$ARCHIVES_DIR" ] || die "archives directory $ARCHIVES_DIR is absent"

case "$MODE" in
    docker)
        command -v docker >/dev/null 2>&1 \
            || die "no container runtime on this runner; the isolation check cannot be performed"
        IMAGE="${RELEASE_GATES_ISOLATION_IMAGE:-}"
        [ -n "$IMAGE" ] \
            || die "RELEASE_GATES_ISOLATION_IMAGE must pin the isolation image by digest"
        case "$IMAGE" in
            *@sha256:*) : ;;
            *) die "RELEASE_GATES_ISOLATION_IMAGE must be pinned by digest, not by tag" ;;
        esac
        docker run --rm --network=none --read-only --cap-drop=ALL \
            --security-opt=no-new-privileges \
            --mount "type=bind,source=$(pwd)/$ARCHIVES_DIR,target=/dist,readonly" \
            "$IMAGE" /dist/smoke.sh \
            || die "the containerised smoke failed under strong isolation"
        ;;
    *) die "unknown isolation mode '$MODE'" ;;
esac

echo "isolation-smoke: PASS: mode $MODE"
