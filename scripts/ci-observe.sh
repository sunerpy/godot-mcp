#!/bin/sh
# ci-observe.sh — run one required release-gate check and record what happened.
#
# The command is NEVER written here or in a workflow file. It is looked up by
# name from the release-gate catalogue (`make release-gate-specs`), which derives
# every Godot version, URL and digest from
# tests/fixtures/godot-releases.lock.json. That is why a workflow step is just
# `scripts/ci-observe.sh <check-name>`: the binding between a check and its exact
# command lives in one place and the harness re-verifies it.
#
# The observation records the captured exit code, the runner identity, the
# candidate commit/tree and, for archive-bound checks, the tested archive digest.
# It is written to ci/observations/<check>.json.
#
# The script's own exit code mirrors the check's, so a failing check fails its
# job. An observation that could not be produced at all leaves no file behind,
# which the harness reports as a `missing` — and therefore failed — entry.
#
# Usage:
#   scripts/ci-observe.sh <check-name>
#   scripts/ci-observe.sh <check-name> --unavailable "<reason>"

set -eu

die() {
    echo "ci-observe: FAIL: $*" >&2
    exit 2
}

CHECK="${1:-}"
[ -n "$CHECK" ] || die "usage: ci-observe.sh <check-name> [--unavailable <reason>]"
shift

STATE="executed"
REASON=""
while [ $# -gt 0 ]; do
    case "$1" in
        --unavailable) STATE="unavailable"; REASON="${2:-unspecified}"; shift 2 ;;
        --skipped) STATE="skipped"; REASON="${2:-unspecified}"; shift 2 ;;
        *) die "unknown argument '$1'" ;;
    esac
done

SPECS="${RELEASE_GATE_SPECS:-}"
if [ -z "$SPECS" ]; then
    SPECS=$(mktemp)
    trap 'rm -f "$SPECS"' EXIT INT TERM
    make release-gate-specs > "$SPECS" || die "the release-gate catalogue could not be produced"
fi

spec_field() {
    CHECK="$CHECK" FIELD="$1" python3 - "$SPECS" <<'PY'
import json, os, sys
name = os.environ["CHECK"]
field = os.environ["FIELD"]
specs = json.load(open(sys.argv[1], encoding="utf-8"))["checks"]
for spec in specs:
    if spec["name"] == name:
        print(spec.get(field) or "")
        break
else:
    sys.exit(3)
PY
}

COMMAND=$(spec_field command) || die "check '$CHECK' is not in the release-gate catalogue"
ARCHIVE_NAME=$(spec_field archive_file_name) \
    || die "check '$CHECK' is not in the release-gate catalogue"

SOURCE_COMMIT="${SOURCE_COMMIT:-${GITHUB_SHA:-}}"
[ -n "$SOURCE_COMMIT" ] || die "SOURCE_COMMIT (or GITHUB_SHA) must identify the candidate commit"
SOURCE_TREE="${SOURCE_TREE:-}"
if [ -z "$SOURCE_TREE" ]; then
    SOURCE_TREE=$(git rev-parse "${SOURCE_COMMIT}^{tree}") \
        || die "SOURCE_TREE must be supplied when it cannot be derived from the commit"
fi

OS_NAME=$(uname -s)
OS_VERSION=$(uname -r)
ARCH=$(uname -m)
IMAGE_DIGEST="${RELEASE_GATES_RUNNER_IMAGE_DIGEST:-}"

OBSERVATION_DIR="ci/observations"
OBSERVATION_PATH="$OBSERVATION_DIR/$CHECK.json"
mkdir -p "$OBSERVATION_DIR"
rm -f "$OBSERVATION_PATH"

sha256_file() {
    python3 - "$1" <<'PY'
import hashlib, pathlib, sys
print(hashlib.sha256(pathlib.Path(sys.argv[1]).read_bytes()).hexdigest())
PY
}

ARCHIVE_SHA256=""
ARCHIVE_PATH=""
if [ "$STATE" = "executed" ] && [ -n "$ARCHIVE_NAME" ]; then
    ARCHIVES_DIR="${RELEASE_GATES_ARCHIVES_DIR:-target/release-dist}"
    ARCHIVE_PATH="$ARCHIVES_DIR/$ARCHIVE_NAME"
    [ -f "$ARCHIVE_PATH" ] || die "archive-bound check '$CHECK' cannot read $ARCHIVE_PATH"
    ARCHIVE_SHA256_BEFORE=$(sha256_file "$ARCHIVE_PATH") \
        || die "archive-bound check '$CHECK' could not hash $ARCHIVE_PATH before execution"
fi

EXIT_CODE=0
if [ "$STATE" = "executed" ]; then
    echo "ci-observe: $CHECK: $COMMAND"
    set +e
    sh -c "$COMMAND"
    EXIT_CODE=$?
    set -e
else
    # An unavailable or skipped environment is reported as exactly that. The
    # harness turns it into a failed entry with a distinct nonzero sentinel; it is
    # never a pass and never a silent non-failure.
    echo "ci-observe: $CHECK: $STATE: $REASON" >&2
    EXIT_CODE=1
fi

if [ "$STATE" = "executed" ] && [ -n "$ARCHIVE_NAME" ]; then
    ARCHIVE_SHA256_AFTER=$(sha256_file "$ARCHIVE_PATH") \
        || die "archive-bound check '$CHECK' could not hash $ARCHIVE_PATH after execution"
    [ "$ARCHIVE_SHA256_BEFORE" = "$ARCHIVE_SHA256_AFTER" ] \
        || die "archive-bound check '$CHECK' saw $ARCHIVE_PATH change during execution"
    ARCHIVE_SHA256="$ARCHIVE_SHA256_AFTER"
fi

CHECK="$CHECK" COMMAND="$COMMAND" STATE="$STATE" EXIT_CODE="$EXIT_CODE" \
OS_NAME="$OS_NAME" OS_VERSION="$OS_VERSION" ARCH="$ARCH" \
IMAGE_DIGEST="$IMAGE_DIGEST" SOURCE_COMMIT="$SOURCE_COMMIT" SOURCE_TREE="$SOURCE_TREE" \
ARCHIVE_SHA256="$ARCHIVE_SHA256" python3 - > "$OBSERVATION_PATH" <<'PY'
import json, os
runner = {
    "os": os.environ["OS_NAME"],
    "os_version": os.environ["OS_VERSION"],
    "arch": os.environ["ARCH"],
}
digest = os.environ.get("IMAGE_DIGEST", "").strip()
if digest:
    runner["image_digest"] = digest
record = {
    "name": os.environ["CHECK"],
    "command": os.environ["COMMAND"],
    "observation": os.environ["STATE"],
    "runner_identity": runner,
    "source_commit": os.environ["SOURCE_COMMIT"],
    "source_tree": os.environ["SOURCE_TREE"],
}
if record["observation"] == "executed":
    record["exit_code"] = int(os.environ["EXIT_CODE"])
archive_sha256 = os.environ.get("ARCHIVE_SHA256", "").strip()
if archive_sha256:
    record["archive_sha256"] = archive_sha256
print(json.dumps(record, indent=2, sort_keys=True))
PY

exit "$EXIT_CODE"
