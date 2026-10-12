#!/bin/sh
# godot-artifact.sh — fetch one exact locked Godot artifact into a hash-keyed
# cache, or fail.
#
# The cache key IS the expected digest, so a cache hit is by definition a
# hash-verified artifact and a tampered or truncated download can never be
# promoted into the cache. Nothing here knows a version, a URL or a digest: all
# three are supplied by the caller, which got them from
# tests/fixtures/godot-releases.lock.json via `make ci-matrix`.
#
# Usage:
#   scripts/godot-artifact.sh --url <URL> --sha256 <64-hex> [--name <ASSET>] \
#                            [--cache <DIR>]
#
# Prints the absolute path of the verified artifact on stdout. Any problem —
# missing argument, absent download tool, network failure, digest mismatch — is a
# nonzero exit. There is no "could not check, assume fine" path.

set -eu

URL=""
EXPECTED=""
NAME=""
CACHE="${GODOT_ARTIFACT_CACHE:-$HOME/.cache/godot-mcp/godot-artifacts}"

die() {
    echo "godot-artifact: FAIL: $*" >&2
    exit 1
}

while [ $# -gt 0 ]; do
    case "$1" in
        --url) URL="${2:-}"; shift 2 ;;
        --sha256) EXPECTED="${2:-}"; shift 2 ;;
        --name) NAME="${2:-}"; shift 2 ;;
        --cache) CACHE="${2:-}"; shift 2 ;;
        *) die "unknown argument '$1'" ;;
    esac
done

[ -n "$URL" ] || die "--url is required"
[ -n "$EXPECTED" ] || die "--sha256 is required"

case "$EXPECTED" in
    *[!0-9a-f]* | "") die "--sha256 must be lowercase hex" ;;
esac
[ "${#EXPECTED}" -eq 64 ] || die "--sha256 must be exactly 64 characters"

if [ -z "$NAME" ]; then
    NAME=$(basename "$URL")
fi

digest_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d' ' -f1
    else
        die "no sha256sum and no shasum on this runner; a download cannot be verified"
    fi
}

TARGET_DIR="$CACHE/$EXPECTED"
TARGET="$TARGET_DIR/$NAME"

if [ -f "$TARGET" ]; then
    ACTUAL=$(digest_of "$TARGET")
    if [ "$ACTUAL" = "$EXPECTED" ]; then
        echo "$TARGET"
        exit 0
    fi
    echo "godot-artifact: cached $TARGET no longer matches its key; discarding" >&2
    rm -f "$TARGET"
fi

mkdir -p "$TARGET_DIR"
STAGING="$TARGET_DIR/.staging-$NAME"
rm -f "$STAGING"

if command -v curl >/dev/null 2>&1; then
    curl --fail --location --silent --show-error --output "$STAGING" "$URL" \
        || die "download failed: $URL"
elif command -v wget >/dev/null 2>&1; then
    wget --quiet --output-document "$STAGING" "$URL" || die "download failed: $URL"
else
    die "neither curl nor wget is available; the locked artifact cannot be fetched"
fi

ACTUAL=$(digest_of "$STAGING")
if [ "$ACTUAL" != "$EXPECTED" ]; then
    rm -f "$STAGING"
    die "digest mismatch for $URL: the download does not match the locked digest"
fi

mv "$STAGING" "$TARGET"
echo "$TARGET"
