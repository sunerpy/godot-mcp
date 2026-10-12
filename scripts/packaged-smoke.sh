#!/bin/sh
# packaged-smoke.sh — run the published archive's own binary against one exact
# locked Godot build, on a real network.
#
# This is a network-dependent smoke by design: it downloads the locked Godot
# artifact (hash-verified) and exercises the packaged `godot-mcp` binary end to
# end. If any prerequisite is absent — the archive, the download tooling, the
# network, the extracted binary — this script exits nonzero. It never reports
# success for a check it could not perform, and it never "skips".
#
# Usage:
#   scripts/packaged-smoke.sh --archive <NAME> --godot-url <URL> \
#                             --godot-sha256 <64-hex> [--archives-dir <DIR>]
#
# No version, URL or digest is hard-coded here; the caller supplies all three
# from the lock-derived matrix.

set -eu

ARCHIVE=""
GODOT_URL=""
GODOT_SHA=""
ARCHIVES_DIR="${RELEASE_GATES_ARCHIVES_DIR:-target/release-dist}"

die() {
    echo "packaged-smoke: FAIL: $*" >&2
    exit 1
}

while [ $# -gt 0 ]; do
    case "$1" in
        --archive) ARCHIVE="${2:-}"; shift 2 ;;
        --godot-url) GODOT_URL="${2:-}"; shift 2 ;;
        --godot-sha256) GODOT_SHA="${2:-}"; shift 2 ;;
        --archives-dir) ARCHIVES_DIR="${2:-}"; shift 2 ;;
        *) die "unknown argument '$1'" ;;
    esac
done

[ -n "$ARCHIVE" ] || die "--archive is required"
[ -n "$GODOT_URL" ] || die "--godot-url is required"
[ -n "$GODOT_SHA" ] || die "--godot-sha256 is required"

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)

ARCHIVE_PATH="$ARCHIVES_DIR/$ARCHIVE"
[ -f "$ARCHIVE_PATH" ] || die "archive $ARCHIVE_PATH is absent"

SUMS="$ARCHIVES_DIR/SHA256SUMS.txt"
[ -f "$SUMS" ] || die "checksum manifest $SUMS is absent"
grep -q "  *$ARCHIVE\$" "$SUMS" || die "$SUMS records no line for $ARCHIVE"

# Hash-verified locked Godot artifact. A failure here is a failure of this check.
GODOT_ARTIFACT=$("$SCRIPT_DIR/godot-artifact.sh" --url "$GODOT_URL" --sha256 "$GODOT_SHA") \
    || die "the locked Godot artifact could not be fetched and verified"
[ -f "$GODOT_ARTIFACT" ] || die "the verified Godot artifact is not on disk"

WORK=$(mktemp -d)
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT INT TERM

case "$ARCHIVE" in
    *.tar.gz)
        command -v tar >/dev/null 2>&1 || die "tar is unavailable; the archive cannot be opened"
        tar -xzf "$ARCHIVE_PATH" -C "$WORK" || die "extracting $ARCHIVE failed"
        ;;
    *.zip)
        command -v unzip >/dev/null 2>&1 || die "unzip is unavailable; the archive cannot be opened"
        unzip -q "$ARCHIVE_PATH" -d "$WORK" || die "extracting $ARCHIVE failed"
        ;;
    *) die "unrecognised archive extension for $ARCHIVE" ;;
esac

BINARY=$(find "$WORK" -type f -name 'godot-mcp' -o -type f -name 'godot-mcp.exe' | head -n 1)
[ -n "$BINARY" ] || die "$ARCHIVE carries no godot-mcp executable"
[ -x "$BINARY" ] || chmod +x "$BINARY" || die "$BINARY is not executable"

"$BINARY" --version >/dev/null || die "the packaged binary could not report its version"
"$BINARY" doctor --offline >/dev/null 2>&1 \
    || "$BINARY" --help >/dev/null \
    || die "the packaged binary could not run its self-diagnostic"

echo "packaged-smoke: PASS: $ARCHIVE against $(basename "$GODOT_ARTIFACT")"
