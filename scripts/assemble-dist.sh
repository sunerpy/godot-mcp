#!/usr/bin/env bash
# assemble-dist.sh — merge the per-platform `make dist` outputs into one set.
#
# `make dist` builds only the host platform's archive, so each platform leg of CI
# and of the release workflow uploads its own target/release-dist: one archive,
# a SHA256SUMS.txt naming it and the host-scoped provenance.json, which a release
# leg renames to provenance-<target>.json before uploading. This script copies
# every archive into OUT, writes one SHA256SUMS.txt over all of them (sorted by
# name, in the format `sha256sum -c` expects) and copies every
# provenance-<target>.json beside them.
#
# Every digest is re-verified against the archive bytes before it is written: a
# manifest that does not describe its archive stops the assembly.
#
# Usage:
#   scripts/assemble-dist.sh [--allow-incomplete] OUT DIST_DIR...
#
# --allow-incomplete skips (with a warning) a DIST_DIR that has no
# SHA256SUMS.txt, so CI can still hand the release-gates harness the archives
# that were built and let it name the missing one. The release workflow never
# passes it.

set -euo pipefail

die() {
	echo "assemble-dist: FAIL: $*" >&2
	exit 1
}

allow_incomplete=0
if [ "${1:-}" = "--allow-incomplete" ]; then
	allow_incomplete=1
	shift
fi
[ $# -ge 2 ] || die "usage: assemble-dist.sh [--allow-incomplete] OUT DIST_DIR..."
out=$1
shift

sha256_of() {
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum "$1" | awk '{ print $1 }'
	else
		shasum -a 256 "$1" | awk '{ print $1 }'
	fi
}

mkdir -p "$out"
manifest="$out/SHA256SUMS.txt"
lines=$(mktemp)
trap 'rm -f "$lines"' EXIT
count=0

for dist in "$@"; do
	dist=${dist%/}
	if [ ! -f "$dist/SHA256SUMS.txt" ]; then
		if [ "$allow_incomplete" -eq 1 ]; then
			echo "::warning::assemble-dist: $dist has no SHA256SUMS.txt; skipped"
			continue
		fi
		die "$dist has no SHA256SUMS.txt"
	fi
	while read -r digest name; do
		[ -n "$digest" ] || continue
		name=${name#\*}
		case "$name" in
		*/* | "") die "$dist/SHA256SUMS.txt names an invalid archive '$name'" ;;
		esac
		[ -f "$dist/$name" ] || die "$dist/SHA256SUMS.txt names $name, which is not there"
		[ "$(sha256_of "$dist/$name")" = "$digest" ] || die "$dist/$name does not hash to its recorded digest"
		[ ! -e "$out/$name" ] || die "$name appears in more than one dist"
		cp "$dist/$name" "$out/$name"
		printf '%s  %s\n' "$digest" "$name" >>"$lines"
		count=$((count + 1))
	done <"$dist/SHA256SUMS.txt"
	for provenance in "$dist"/provenance-*.json; do
		[ -f "$provenance" ] || continue
		[ ! -e "$out/${provenance##*/}" ] || die "${provenance##*/} appears in more than one dist"
		cp "$provenance" "$out/"
	done
done

[ "$count" -gt 0 ] || die "no archive was assembled"
LC_ALL=C sort -k2 "$lines" >"$manifest"
echo "assemble-dist: $count archive(s) into $out"
cat "$manifest"
