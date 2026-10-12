#!/bin/sh
# install.sh — install a published godot-mcp release on Linux or macOS.
#
#   curl -fsSL https://raw.githubusercontent.com/sunerpy/godot-mcp/main/scripts/install.sh | sh
#
# It downloads this platform's archive and SHA256SUMS.txt from the GitHub
# release, refuses an archive whose digest is not the published one, extracts
# the whole package, runs the package's own `self-check`, and writes a
# `godot-mcp` launcher on PATH. The package stays together because the server
# finds its supervisor and the project assets it installs beside its own
# executable; the launcher therefore execs the packaged binary rather than
# linking to it.
#
# Environment (prefixed GODOTMCP_, not GODOT_MCP_: the server treats every
# GODOT_MCP_ variable as its own configuration and refuses one it does not know):
#   GODOTMCP_VERSION        release to install, for example v0.2.0 (default: the latest)
#   GODOTMCP_INSTALL_ROOT   where packages are extracted (default: ~/.local/share/godot-mcp)
#   GODOTMCP_BIN_DIR        where the launcher goes (default: ~/.local/bin)
#   GODOTMCP_DOWNLOAD_BASE  base URL of release downloads, for a mirror
#                            (default: https://github.com/sunerpy/godot-mcp/releases/download)

set -eu

REPO="sunerpy/godot-mcp"
CHECKSUM_FILE="SHA256SUMS.txt"

err() {
	printf 'godot-mcp install: %s\n' "$1" >&2
	exit 1
}

info() {
	printf '%s\n' "$1" >&2
}

if command -v curl >/dev/null 2>&1; then
	download() { curl -fsSL "$1" -o "$2"; }
	fetch() { curl -fsSL "$1"; }
elif command -v wget >/dev/null 2>&1; then
	download() { wget -qO "$2" "$1"; }
	fetch() { wget -qO - "$1"; }
else
	err "curl or wget is required"
fi

command -v tar >/dev/null 2>&1 || err "tar is required"

platform="$(uname -s)/$(uname -m)"
case "$platform" in
Linux/x86_64 | Linux/amd64) target="x86_64-unknown-linux-musl" ;;
Darwin/arm64 | Darwin/aarch64) target="aarch64-apple-darwin" ;;
Darwin/x86_64) err "no Intel macOS archive is published; build from source (README, Build from source)" ;;
Linux/aarch64 | Linux/arm64) err "no Linux arm64 archive is published; build from source (README, Build from source)" ;;
*) err "unsupported platform: $platform" ;;
esac

if [ -n "${GODOTMCP_VERSION:-}" ]; then
	version=$(printf '%s' "$GODOTMCP_VERSION" | sed 's/^v//')
else
	info "Resolving the latest release..."
	tag=$(fetch "https://api.github.com/repos/${REPO}/releases/latest" |
		sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)
	[ -n "$tag" ] || err "found no published release (there is none before the first one); build from source (README, Build from source) or set GODOTMCP_VERSION"
	version=$(printf '%s' "$tag" | sed 's/^v//')
fi
case "$version" in
*[!0-9A-Za-z.+-]* | "") err "not a release version: $version" ;;
esac

package="godot-mcp-${version}-${target}"
asset="${package}.tar.gz"
base_url="${GODOTMCP_DOWNLOAD_BASE:-https://github.com/${REPO}/releases/download}/v${version}"
install_root="${GODOTMCP_INSTALL_ROOT:-$HOME/.local/share/godot-mcp}"
bin_dir="${GODOTMCP_BIN_DIR:-$HOME/.local/bin}"
# The launcher quotes the install path; refuse the few characters that would
# end or expand that quoting rather than escaping them.
case "$install_root" in
*'"'* | *\\* | *'$'* | *'`'*) err "GODOTMCP_INSTALL_ROOT must not contain a quote, a backslash, a dollar sign or a backtick" ;;
esac

tmp=$(mktemp -d 2>/dev/null || mktemp -d -t godot-mcp)
trap 'rm -rf "$tmp"' EXIT INT TERM

info "Downloading ${asset}"
download "${base_url}/${asset}" "$tmp/$asset"
download "${base_url}/${CHECKSUM_FILE}" "$tmp/$CHECKSUM_FILE"

expected=$(awk -v name="$asset" '{
	file = $2
	sub(/^\*/, "", file)
	if (file == name) { print $1; exit }
}' "$tmp/$CHECKSUM_FILE")
[ -n "$expected" ] || err "${asset} is missing from ${CHECKSUM_FILE}"

if command -v sha256sum >/dev/null 2>&1; then
	actual=$(sha256sum "$tmp/$asset" | awk '{ print $1 }')
elif command -v shasum >/dev/null 2>&1; then
	actual=$(shasum -a 256 "$tmp/$asset" | awk '{ print $1 }')
else
	err "sha256sum or shasum is required"
fi
[ "$actual" = "$expected" ] || err "checksum mismatch for ${asset}"

mkdir -p "$tmp/extract"
tar -xzf "$tmp/$asset" -C "$tmp/extract"
[ -x "$tmp/extract/$package/bin/godot-mcp" ] || err "${asset} does not contain ${package}/bin/godot-mcp"
"$tmp/extract/$package/bin/godot-mcp" self-check --package "$tmp/extract/$package" >/dev/null ||
	err "the extracted package failed its own self-check"

mkdir -p "$install_root"
rm -rf "${install_root:?}/$package"
mv "$tmp/extract/$package" "$install_root/$package"

mkdir -p "$bin_dir"
launcher="$bin_dir/godot-mcp"
printf '#!/bin/sh\nexec "%s" "$@"\n' "$install_root/$package/bin/godot-mcp" >"$launcher.tmp"
chmod 0755 "$launcher.tmp"
mv "$launcher.tmp" "$launcher"

info "Installed ${package} to ${install_root}/${package}"
info "Launcher: ${launcher}"
case ":$PATH:" in
*":$bin_dir:"*) ;;
*) info "Add ${bin_dir} to PATH to run godot-mcp by name" ;;
esac
