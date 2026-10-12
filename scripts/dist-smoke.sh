#!/bin/sh
# dist-smoke.sh — published as `smoke.sh` beside the release archives AND inside
# every archive payload. This is the artifact the frozen
# `isolation-docker-clean-room` gate executes:
#
#   docker run --rm --network=none --read-only --cap-drop=ALL \
#     --security-opt=no-new-privileges \
#     --mount type=bind,source=$PWD/target/release-dist,target=/dist,readonly \
#     $RELEASE_GATES_ISOLATION_IMAGE /dist/smoke.sh
#
# ---------------------------------------------------------------------------
# WHAT THE CLEAN ROOM ACTUALLY PROVIDES — measured, not assumed
# ---------------------------------------------------------------------------
# Against the digest-pinned image CI uses (alpine 3.22, busybox
# 1.37) under exactly the flags above:
#
#   * `/bin/sh` is busybox ash. There is no bash. This script is POSIX shell.
#   * `/tmp` is NOT writable: `--read-only` covers the whole root filesystem.
#     The ONLY writable mount is `/dev/shm`, a 64 MiB tmpfs mounted `noexec`.
#     This script therefore needs no scratch space at all — every check streams.
#   * There is no `/lib64/ld-linux-x86-64.so.2`; the image is musl-based.
#
# Two independent consequences follow, and they decide this script's scope:
#
#   THE PACKAGED BINARY IS NOT EXECUTED HERE. The Linux release is a statically
#   linked `x86_64-unknown-linux-musl` PIE, but the archive is compressed and the
#   clean room has nowhere to extract it that is both writable and executable.
#   So `--version`, `self-check` and `serve`-opens-no-listener are NOT checked
#   here and this script does not pretend otherwise. Those are established by the
#   `packaged-smoke-*` gates, which extract and run the binary on a real runner of
#   each platform, and by the archive round-trip tests in
#   crates/godot-mcp-cli/tests/package_and_repair.rs.
#
#   WHAT IS PROVABLE HERE IS THE ARTIFACT ITSELF, VERIFIED WITHOUT TRUSTING IT.
#   Everything below is done with reads only, offline, with no capabilities, and
#   without executing one byte of the payload — which is a stronger position from
#   which to audit a release than asking the release to audit itself.
#
# ---------------------------------------------------------------------------
# THE CHECKS, AND EXACTLY WHAT EACH ONE PROVES
# ---------------------------------------------------------------------------
# Clean-room assertions (dist mode only; each FAILS if its docker flag is absent,
# so this gate cannot be passed by relaxing the container):
#   C1 CapEff is all zeros                 -> `--cap-drop=ALL` is in force.
#   C2 NoNewPrivs is 1                     -> `--security-opt=no-new-privileges`.
#   C3 writing to `/` is refused           -> `--read-only` is in force.
#   C4 writing into the dist dir is refused-> the bind mount really is readonly.
#   C5 only `lo` exists, no route, no open
#      TCP socket                          -> `--network=none` is in force.
#   These prove the ENVIRONMENT, not the artifact. They do not prove the binary
#   would behave correctly if it had a network.
#
# Dist-level checks:
#   D1 `sha256sum -c SHA256SUMS.txt`       -> every archive the release claims is
#      present and its bytes hash to the published digest. Does not prove the
#      digest is the one a maintainer signed; nothing here is signed.
#   D2 provenance.json subjects match the
#      SHA256SUMS set, name for name and
#      digest for digest                   -> the attestation is not stale and
#      describes exactly the archives on disk. Does not prove the attestation is
#      authentic: `provenance.json` is deliberately unsigned.
#   D3 the payload manifest records
#      profile "release"                   -> a QA-lane or debug archive cannot be
#      published as a release. Proves what the packager recorded, not the flags
#      cargo really used.
#   D4 the executed /dist/smoke.sh is byte-
#      identical to the archive's recorded
#      smoke.sh payload entry              -> the clean room ran exactly the
#      digest-recorded, reviewed script, not an unversioned copy beside it.
#
# Per-archive payload checks (dist mode streams them out of the archive; payload
# mode reads them from an extracted tree):
#   P1 every file MANIFEST.json records is
#      present and its bytes hash to the
#      recorded SHA-256, at the recorded
#      size                                -> the payload has not drifted since
#      packaging. This is `self-check`'s core claim, recomputed independently.
#   P2 the archive carries NOTHING beyond
#      the recorded set plus MANIFEST.json  -> no file was smuggled into the
#      archive without a manifest entry.
#   P3 MANIFEST.json is not itself recorded -> the manifest cannot claim its own
#      digest, which would be unverifiable.
#   P4 every REQUIRED_PAYLOAD_PATHS entry
#      is recorded                          -> no release ships without its
#      notices, SBOM, schemas, bridge codec, example config or this script.
#   P5 `bin/` holds exactly the declared
#      executable and supervisor            -> structurally forbids a test-hooks
#      or qualification-tools binary riding along.
#   P6 no payload file outside those two and
#      smoke.sh carries any execute bit or
#      a setuid/setgid bit                  -> forbids a stowaway executable
#      anywhere else in the tree.
#   P7 the archive's stored mode equals the
#      manifest's recorded mode (tar only;
#      busybox `unzip -l` cannot report
#      stored modes)                        -> smoke.sh really is 0755 in the
#      published output and no payload file was re-moded after packaging.
#   P8 each declared binary's first bytes are
#      the right container for the declared
#      target: ELF64/LE/x86-64, PE `MZ`, or
#      Mach-O 64/arm64                      -> the archive's executable is a real
#      binary for the triple it claims, not a stub or a wrong-arch file. Proves
#      the container format only; it does not prove the binary runs.
#   P9 the shipped examples/godot-mcp.toml
#      selects the stdio transport and
#      assigns no listener, HTTP/auth
#      section or credential key            -> the configuration a first-run
#      operator copies opens no socket. Proves the SHIPPED EXAMPLE's content, NOT
#      the binary's runtime listener behaviour, which needs execution.
#   All eleven fail closed. Any check whose input cannot be read is a failure, not
#   a skip.
#
# NOT CHECKED HERE, deliberately:
#   * credential-marker scanning. The packager refuses to stage a payload that
#     contains a marker and `godot-mcp self-check` rescans at runtime; a shell
#     copy of that list would have to embed the markers literally, which would
#     make THIS FILE fail the packager's own credential scan.
#   * SBOM accuracy, reproducibility, signature verification, and anything that
#     needs the binary to run.
#
# ---------------------------------------------------------------------------
# TWO MODES, auto-detected from the script's own directory
# ---------------------------------------------------------------------------
#   dist    — the directory holds SHA256SUMS.txt: verify the published release
#             set. Asserts the clean room, because that is what this copy exists
#             for.
#   payload — the directory holds MANIFEST.json and bin/: verify one extracted
#             package offline, with nothing but a POSIX shell and coreutils. No
#             clean-room assertion; an operator is not in a container.
#
# Exit status: 0 only when every check performed passed. Any problem, and any
# input that could not be read, exits 1.

set -eu
# pipefail is not POSIX: set it only where the shell accepts it (busybox ash does).
# shellcheck disable=SC3040
(set -o pipefail) 2>/dev/null && set -o pipefail || true

# --- BEGIN REQUIRED_PAYLOAD_PATHS ------------------------------------------
# Mirrors godot_mcp_cli::package::REQUIRED_PAYLOAD_PATHS. The two lists are
# asserted identical by `the_shell_smoke_requires_exactly_the_rust_payload_paths`
# in crates/godot-mcp-cli/tests/release_package.rs, so neither can drift.
REQUIRED_PAYLOAD_PATHS='bridge/bridge_codec.gd
examples/godot-mcp.toml
licenses/LICENSE-MIT
licenses/THIRD-PARTY-NOTICES.md
sbom/godot-mcp.cdx.json
schemas/bridge/protocol-v1.json
smoke.sh'
# --- END REQUIRED_PAYLOAD_PATHS --------------------------------------------

PROBLEMS=0
MODE=''
ARCHIVE=''
PAYLOAD_ROOT=''
ROOT_DIR=''
LABEL=''
MANIFEST_TARGET=''
MANIFEST_VERSION=''
MANIFEST_PROFILE=''
PAYLOAD_SMOKE_SHA=''

say() { echo "smoke: $*"; }
ok() { echo "smoke: ok   $*"; }
fail() {
    echo "smoke: FAIL $*" >&2
    PROBLEMS=$((PROBLEMS + 1))
}
die() {
    echo "smoke: FAIL $*" >&2
    exit 1
}

[ $# -eq 0 ] || die "this script takes no arguments; the isolation gate invokes it bare (got: $*)"

SELF_NAME=$(basename -- "$0")
DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd) || die "cannot resolve the directory of $0"
SELF="$DIR/$SELF_NAME"

# Count matching lines without letting grep's early exit break the pipeline under
# `set -o pipefail`: `grep -c` consumes all of its input, and the `|| true` keeps
# a zero count from aborting under `set -e`.
matches() {
    printf '%s\n' "$1" | { grep -Ec -- "$2" || true; }
}

read_member() {
    case "$MODE" in
    tar) tar -xzOf "$ARCHIVE" "$PAYLOAD_ROOT/$1" ;;
    zip) unzip -p "$ARCHIVE" "$PAYLOAD_ROOT/$1" ;;
    dir) cat "$ROOT_DIR/$1" ;;
    *) return 1 ;;
    esac
}

# Emit `path|size|mode` for every member. `mode` is `-` where the container
# format's listing cannot report it. The packager stages a flat list of regular
# files under exactly one payload root and writes no directory, symlink, hardlink
# or device entry, so any entry that is not a regular file under that root — the
# shape a hostile or hand-edited archive would need — is emitted with a `!` tag and
# reported rather than silently dropped.
list_entries() {
    case "$MODE" in
    tar)
        tar -tzvf "$ARCHIVE" | awk -v root="$PAYLOAD_ROOT/" '
            function oct(p) {
                o = 0
                if (substr(p, 2, 1) == "r") o += 400
                if (substr(p, 3, 1) == "w") o += 200
                if (substr(p, 4, 1) == "x") o += 100
                if (substr(p, 5, 1) == "r") o += 40
                if (substr(p, 6, 1) == "w") o += 20
                if (substr(p, 7, 1) == "x") o += 10
                if (substr(p, 8, 1) == "r") o += 4
                if (substr(p, 9, 1) == "w") o += 2
                if (substr(p, 10, 1) == "x") o += 1
                return o
            }
            NF >= 6 {
                name = $NF
                if (substr($1, 1, 1) != "-") { print "!entry-type-" substr($1, 1, 1) " " name "|0|-"; next }
                if (substr(name, 1, length(root)) != root) { print "!outside-payload-root " name "|0|-"; next }
                print substr(name, length(root) + 1) "|" $3 "|" oct($1)
            }'
        ;;
    zip)
        unzip -l "$ARCHIVE" | awk -v root="$PAYLOAD_ROOT/" '
            $1 ~ /^[0-9]+$/ && NF == 4 {
                name = $4
                if (substr(name, 1, length(root)) != root) { print "!outside-payload-root " name "|0|-"; next }
                print substr(name, length(root) + 1) "|" $1 "|-"
            }'
        ;;
    dir)
        (
            cd "$ROOT_DIR" || exit 1
            find . -type f -print | sed 's|^\./||' | sort | while IFS= read -r found; do
                printf '%s|%s|-\n' "$found" "$(wc -c <"$found" | tr -d ' ')"
            done
        )
        ;;
    *) return 1 ;;
    esac
}

# Flatten MANIFEST.json into tagged lines. serde_json writes sorted keys and one
# scalar per line, so a line-oriented reader is exact here; the parse is then
# cross-checked against its own `"path":` count so a silent mis-parse cannot pass.
parse_manifest() {
    awk '
        function value(text) {
            sub(/^[^:]*:[ \t]*/, "", text)
            sub(/,[ \t]*$/, "", text)
            gsub(/^"|"$/, "", text)
            return text
        }
        function flush() {
            if (seen == 4) {
                print "FILE|" path "|" sha "|" size "|" mode
                seen = 0
                files++
            }
        }
        /^[ \t]*"executable"[ \t]*:/ { print "EXECUTABLE|" value($0); next }
        /^[ \t]*"supervisor"[ \t]*:/ { print "SUPERVISOR|" value($0); next }
        /^[ \t]*"target"[ \t]*:/     { print "TARGET|" value($0); next }
        /^[ \t]*"version"[ \t]*:/    { print "VERSION|" value($0); next }
        /^[ \t]*"profile"[ \t]*:/    { print "PROFILE|" value($0); next }
        /^[ \t]*"mode"[ \t]*:/   { mode = value($0); seen++; flush(); next }
        /^[ \t]*"path"[ \t]*:/   { path = value($0); seen++; paths++; flush(); next }
        /^[ \t]*"sha256"[ \t]*:/ { sha = value($0); seen++; flush(); next }
        /^[ \t]*"size"[ \t]*:/   { size = value($0); seen++; flush(); next }
        END { print "COUNTS|" files + 0 "|" paths + 0 }
    '
}

tagged() {
    printf '%s\n' "$1" | awk -F'|' -v key="$2" '$1 == key { print $2; exit }'
}

digest_of_member() {
    read_member "$1" | sha256sum
}

# First 32 bytes as lower-case hex, space separated. The trailing `cat` drains the
# rest of the stream so the producer never takes SIGPIPE, which `pipefail` would
# otherwise turn into a spurious failure.
member_head_hex() {
    read_member "$1" | { od -An -tx1 -N 32; cat >/dev/null; } | tr -s ' \n' ' ' |
        sed 's/^ //; s/ $//'
}

check_binary_shape() {
    shape_path="$1"
    shape_target="$2"
    shape_before=$PROBLEMS
    if ! shape_hex=$(member_head_hex "$shape_path"); then
        fail "$LABEL: $shape_path could not be read to check its container format"
        return 0
    fi
    case "$shape_target" in
    *-linux-gnu | *-linux-musl)
        case "$shape_hex" in
        '7f 45 4c 46 02 01 01 '*) ;;
        *) fail "$LABEL: $shape_path is not a 64-bit little-endian ELF (head: $shape_hex)" ;;
        esac
        shape_machine=$(printf '%s' "$shape_hex" | cut -d' ' -f19,20)
        [ "$shape_machine" = '3e 00' ] ||
            fail "$LABEL: $shape_path declares e_machine $shape_machine, not x86-64 (3e 00)"
        shape_expected='a 64-bit little-endian x86-64 ELF image'
        ;;
    *-windows-msvc)
        case "$shape_hex" in
        '4d 5a '*) ;;
        *) fail "$LABEL: $shape_path is not a PE image (no MZ; head: $shape_hex)" ;;
        esac
        shape_expected='a PE image'
        ;;
    *-apple-darwin)
        case "$shape_hex" in
        'cf fa ed fe '*) ;;
        *) fail "$LABEL: $shape_path is not a 64-bit Mach-O image (head: $shape_hex)" ;;
        esac
        shape_cpu=$(printf '%s' "$shape_hex" | cut -d' ' -f5,6,7,8)
        [ "$shape_cpu" = '0c 00 00 01' ] ||
            fail "$LABEL: $shape_path declares Mach-O cputype $shape_cpu, not arm64 (0c 00 00 01)"
        shape_expected='a 64-bit arm64 Mach-O image'
        ;;
    *)
        fail "$LABEL: no container-format expectation is defined for target $shape_target"
        shape_expected=''
        ;;
    esac
    if [ "$shape_before" -eq "$PROBLEMS" ] && [ -n "$shape_expected" ]; then
        ok "$LABEL: $shape_path is $shape_expected"
    fi
}

check_example_config() {
    if ! config_text=$(read_member 'examples/godot-mcp.toml'); then
        fail "$LABEL: examples/godot-mcp.toml could not be read"
        return 0
    fi
    config_before=$PROBLEMS
    [ "$(matches "$config_text" '^[[:space:]]*transport[[:space:]]*=[[:space:]]*"stdio"')" -gt 0 ] ||
        fail "$LABEL: the shipped example config does not select the stdio transport"
    [ "$(matches "$config_text" '^[[:space:]]*(bind|bind_address|listen|listen_address|address|host|port)[[:space:]]*=')" -eq 0 ] ||
        fail "$LABEL: the shipped example config assigns a network listener key"
    [ "$(matches "$config_text" '^[[:space:]]*\[(http|auth|oidc|tls)')" -eq 0 ] ||
        fail "$LABEL: the shipped example config enables an http/auth/oidc/tls section"
    [ "$(matches "$config_text" '^[[:space:]]*(token|secret|client_secret|password|api_key)[[:space:]]*=')" -eq 0 ] ||
        fail "$LABEL: the shipped example config assigns a credential key"
    if [ "$config_before" -eq "$PROBLEMS" ]; then
        ok "$LABEL: the shipped example config selects stdio and assigns no listener or credential key"
    fi
}

# P1..P9 over whatever `read_member`/`list_entries` are currently bound to.
verify_payload() {
    LABEL="$1"

    if ! manifest_text=$(read_member 'MANIFEST.json'); then
        fail "$LABEL: MANIFEST.json could not be read; the payload cannot be verified"
        return 0
    fi
    [ -n "$manifest_text" ] || {
        fail "$LABEL: MANIFEST.json is empty"
        return 0
    }

    parsed=$(printf '%s\n' "$manifest_text" | parse_manifest)
    table=$(printf '%s\n' "$parsed" | { grep '^FILE|' || true; } | cut -d'|' -f2-)
    counts=$(tagged "$parsed" 'COUNTS')
    parsed_files=${counts%%|*}
    parsed_paths=${counts#*|}
    if [ "${parsed_files:-0}" -eq 0 ]; then
        fail "$LABEL: MANIFEST.json records no payload file; refusing to treat that as a pass"
        return 0
    fi
    if [ "$parsed_files" != "$parsed_paths" ]; then
        fail "$LABEL: the manifest parse is inconsistent ($parsed_files records for $parsed_paths paths)"
        return 0
    fi

    MANIFEST_TARGET=$(tagged "$parsed" 'TARGET')
    MANIFEST_VERSION=$(tagged "$parsed" 'VERSION')
    MANIFEST_PROFILE=$(tagged "$parsed" 'PROFILE')
    declared_executable=$(tagged "$parsed" 'EXECUTABLE')
    declared_supervisor=$(tagged "$parsed" 'SUPERVISOR')
    say "$LABEL: manifest declares version $MANIFEST_VERSION target $MANIFEST_TARGET profile $MANIFEST_PROFILE, $parsed_files payload files"
    [ -n "$declared_executable" ] || fail "$LABEL: MANIFEST.json declares no executable"
    [ -n "$declared_supervisor" ] || fail "$LABEL: MANIFEST.json declares no supervisor"
    [ -n "$MANIFEST_TARGET" ] || fail "$LABEL: MANIFEST.json declares no target"

    if ! entries=$(list_entries); then
        fail "$LABEL: the payload could not be listed"
        return 0
    fi
    rejected=$(printf '%s\n' "$entries" | { grep '^!' || true; })
    if [ -n "$rejected" ]; then
        printf '%s\n' "$rejected" | while IFS= read -r bad; do
            echo "smoke: FAIL $LABEL: rejected member: $bad" >&2
        done
        fail "$LABEL: the payload holds members outside its single root, or entries that are not regular files"
    fi

    # P1 + P7: digest, size and stored mode of every recorded file.
    verified=0
    old_ifs=$IFS
    set -f
    IFS='
'
    for row in $table; do
        IFS=$old_ifs
        set +f
        row_path=${row%%|*}
        row_rest=${row#*|}
        row_sha=${row_rest%%|*}
        row_rest=${row_rest#*|}
        row_size=${row_rest%%|*}
        row_mode=${row_rest#*|}

        listed=$(printf '%s\n' "$entries" |
            awk -F'|' -v want="$row_path" '$1 == want && !hit { print $2 "|" $3; hit = 1 }')
        if [ -z "$listed" ]; then
            fail "$LABEL: $row_path is recorded in MANIFEST.json but absent from the payload"
        else
            listed_size=${listed%%|*}
            listed_mode=${listed#*|}
            [ "$listed_size" = "$row_size" ] ||
                fail "$LABEL: $row_path is stored at $listed_size bytes, manifest records $row_size"
            if [ "$listed_mode" != '-' ] && [ "$listed_mode" != "$row_mode" ]; then
                fail "$LABEL: $row_path is stored with mode $listed_mode, manifest records $row_mode"
            fi
        fi

        if computed=$(digest_of_member "$row_path"); then
            computed=${computed%% *}
            if [ "$computed" = "$row_sha" ]; then
                verified=$((verified + 1))
            else
                fail "$LABEL: $row_path hashes to $computed, manifest records $row_sha"
            fi
        else
            fail "$LABEL: $row_path could not be read to verify its digest"
        fi

        set -f
        IFS='
'
    done
    IFS=$old_ifs
    set +f
    if [ "$verified" -eq "$parsed_files" ]; then
        ok "$LABEL: all $verified recorded payload files match their SHA-256, size and stored mode"
    else
        fail "$LABEL: only $verified of $parsed_files recorded payload files verified"
    fi

    # P2 + P3: the member set is exactly the recorded set plus MANIFEST.json.
    recorded=$(printf '%s\n' "$table" | awk -F'|' '{ print $1 }' | sort)
    present=$(printf '%s\n' "$entries" | awk -F'|' '{ print $1 }' | sort)
    expected=$(printf '%s\nMANIFEST.json\n' "$recorded" | sort)
    if [ "$expected" = "$present" ]; then
        ok "$LABEL: the payload carries nothing beyond its recorded files and MANIFEST.json"
    else
        difference=$(
            {
                printf '%s\n' "$expected" | sed 's/^/E /'
                printf '%s\n' "$present" | sed 's/^/P /'
            } | awk '
                { seen[$2] = seen[$2] $1 }
                END {
                    for (key in seen) {
                        if (seen[key] == "E") print "recorded-but-absent " key
                        else if (seen[key] == "P") print "present-but-unrecorded " key
                    }
                }' | sort
        )
        printf '%s\n' "$difference" | while IFS= read -r line; do
            [ -n "$line" ] || continue
            echo "smoke: FAIL $LABEL: $line" >&2
        done
        fail "$LABEL: the payload member set does not equal its manifest"
    fi
    if [ "$(matches "$recorded" '^MANIFEST\.json$')" -eq 0 ]; then
        ok "$LABEL: MANIFEST.json does not record its own digest"
    else
        fail "$LABEL: MANIFEST.json records itself, which no reader could verify"
    fi

    # P4: every required payload path.
    missing_required=''
    old_ifs=$IFS
    set -f
    IFS='
'
    for required in $REQUIRED_PAYLOAD_PATHS; do
        IFS=$old_ifs
        set +f
        if [ "$(matches "$recorded" "^$(printf '%s' "$required" | sed 's/[.[\*^$]/\\&/g')\$")" -eq 0 ]; then
            missing_required="$missing_required $required"
        fi
        set -f
        IFS='
'
    done
    IFS=$old_ifs
    set +f
    if [ -z "$missing_required" ]; then
        ok "$LABEL: every required payload path is recorded"
    else
        fail "$LABEL: required payload path(s) missing:$missing_required"
    fi

    # P5: bin/ holds exactly the declared executable and supervisor.
    stored_binaries=$(printf '%s\n' "$recorded" | { grep '^bin/' || true; } | sort | tr '\n' ' ')
    wanted_binaries=$(printf '%s\n%s\n' "$declared_executable" "$declared_supervisor" | sort | tr '\n' ' ')
    if [ "$stored_binaries" = "$wanted_binaries" ]; then
        ok "$LABEL: bin/ holds exactly $wanted_binaries"
    else
        fail "$LABEL: bin/ holds [$stored_binaries], the manifest declares [$wanted_binaries]"
    fi

    # P6: no other payload file is executable or setuid/setgid.
    stowaways=$(printf '%s\n' "$table" | awk -F'|' \
        -v exe="$declared_executable" -v sup="$declared_supervisor" '
        {
            m = $4 + 0
            owner = int(m / 100) % 10
            group = int(m / 10) % 10
            other = m % 10
            if (m >= 1000 || owner % 2 == 1 || group % 2 == 1 || other % 2 == 1) {
                if ($1 != exe && $1 != sup && $1 != "smoke.sh") print $1 " (mode " $4 ")"
            }
        }')
    if [ -z "$stowaways" ]; then
        ok "$LABEL: no payload file outside bin/ and smoke.sh carries an execute or setuid bit"
    else
        printf '%s\n' "$stowaways" | while IFS= read -r line; do
            echo "smoke: FAIL $LABEL: unexpected executable payload file: $line" >&2
        done
        fail "$LABEL: the payload carries an executable file it does not declare"
    fi
    if [ "$(matches "$table" '^smoke\.sh\|[0-9a-f]+\|[0-9]+\|755$')" -gt 0 ]; then
        ok "$LABEL: smoke.sh is recorded with mode 755"
    else
        fail "$LABEL: smoke.sh is not recorded with mode 755; the clean room execs it directly"
    fi

    # P8: container format of each declared binary.
    if [ -n "$declared_executable" ] && [ -n "$MANIFEST_TARGET" ]; then
        check_binary_shape "$declared_executable" "$MANIFEST_TARGET"
    fi
    if [ -n "$declared_supervisor" ] && [ -n "$MANIFEST_TARGET" ]; then
        check_binary_shape "$declared_supervisor" "$MANIFEST_TARGET"
    fi

    # P9: the shipped example configuration.
    check_example_config

    PAYLOAD_SMOKE_SHA=$(printf '%s\n' "$table" |
        awk -F'|' '$1 == "smoke.sh" && !hit { print $2; hit = 1 }')
}

assert_clean_room() {
    caps=$(awk '$1 == "CapEff:" { print $2 }' /proc/self/status || true)
    if [ -z "$caps" ]; then
        fail "clean room: /proc/self/status reports no CapEff; capability state is unknown"
    else
        case "$caps" in
        *[!0]*) fail "clean room: effective capabilities are $caps; --cap-drop=ALL is not in force" ;;
        *) ok "clean room: no effective capability (CapEff $caps)" ;;
        esac
    fi

    nnp=$(awk '$1 == "NoNewPrivs:" { print $2 }' /proc/self/status || true)
    if [ "$nnp" = '1' ]; then
        ok 'clean room: NoNewPrivs is set'
    else
        fail "clean room: NoNewPrivs is ${nnp:-absent}; --security-opt=no-new-privileges is not in force"
    fi

    if (: >/.godot-mcp-smoke-write-probe) 2>/dev/null; then
        rm -f /.godot-mcp-smoke-write-probe 2>/dev/null || true
        fail 'clean room: the root filesystem accepted a write; --read-only is not in force'
    else
        ok 'clean room: the root filesystem refused a write'
    fi

    if (: >"$DIR/.godot-mcp-smoke-write-probe") 2>/dev/null; then
        rm -f "$DIR/.godot-mcp-smoke-write-probe" 2>/dev/null || true
        fail "clean room: $DIR accepted a write; the dist mount is not readonly"
    else
        ok "clean room: $DIR refused a write"
    fi

    interfaces=$(awk 'NR > 2 { sub(":", "", $1); print $1 }' /proc/net/dev | sort | tr '\n' ' ')
    if [ "$interfaces" = 'lo ' ]; then
        ok 'clean room: the only network interface is lo'
    else
        fail "clean room: network interfaces are [$interfaces]; --network=none is not in force"
    fi
    routes=$(awk 'NR > 1' /proc/net/route | wc -l | tr -d ' ')
    if [ "$routes" = '0' ]; then
        ok 'clean room: the routing table is empty'
    else
        fail "clean room: the routing table holds $routes route(s); egress is reachable"
    fi
    sockets=0
    for table_file in /proc/net/tcp /proc/net/tcp6; do
        [ -r "$table_file" ] || continue
        sockets=$((sockets + $(awk 'NR > 1' "$table_file" | wc -l | tr -d ' ')))
    done
    if [ "$sockets" -eq 0 ]; then
        ok 'clean room: no TCP socket is open in this namespace'
    else
        fail "clean room: $sockets TCP socket(s) are open in this namespace"
    fi
}

verify_dist() {
    cd "$DIR" || die "cannot enter $DIR"
    [ -f SHA256SUMS.txt ] || die "$DIR holds no SHA256SUMS.txt"

    malformed=$(awk 'NF != 2 { print NR }' SHA256SUMS.txt | tr '\n' ' ')
    [ -z "$malformed" ] || die "SHA256SUMS.txt line(s) $malformed are not '<digest>  <name>'"
    archives=$(awk '{ print $2 }' SHA256SUMS.txt)
    [ -n "$archives" ] || die 'SHA256SUMS.txt records no archive'

    # D1
    if sums_report=$(sha256sum -c SHA256SUMS.txt 2>&1); then
        ok "SHA256SUMS.txt verifies against the published bytes"
        printf '%s\n' "$sums_report" | sed 's/^/smoke:      /'
    else
        printf '%s\n' "$sums_report" | sed 's/^/smoke:      /' >&2
        fail 'SHA256SUMS.txt does not verify against the published bytes'
    fi

    # Anything in this directory that SHA256SUMS.txt does not name is not part of
    # the release: a release dist is assembled from one runner per platform, so a
    # leftover or foreign file is reported and left unaudited rather than treated
    # as release content.
    unrecorded=$(
        {
            find . -maxdepth 1 -type f -name '*.tar.gz' -print
            find . -maxdepth 1 -type f -name '*.zip' -print
        } | sed 's|^\./||' | sort | while IFS= read -r found; do
            if [ "$(matches "$archives" "^$(printf '%s' "$found" | sed 's/[.[\*^$]/\\&/g')\$")" -eq 0 ]; then
                printf '%s\n' "$found"
            fi
        done
    )
    if [ -n "$unrecorded" ]; then
        printf '%s\n' "$unrecorded" | while IFS= read -r line; do
            say "note: $line is present but not named by SHA256SUMS.txt; it is not part of this release and was not audited"
        done
    fi

    # D2
    if [ -f provenance.json ]; then
        provenance_pairs=$(awk '
            /^[ \t]*"sha256"[ \t]*:/ {
                value = $0
                sub(/^[^:]*:[ \t]*"/, "", value)
                sub(/".*$/, "", value)
                pending = value
                next
            }
            /^[ \t]*"name"[ \t]*:/ {
                value = $0
                sub(/^[^:]*:[ \t]*"/, "", value)
                sub(/".*$/, "", value)
                if (pending != "") { print value "|" pending; pending = "" }
                next
            }' provenance.json | sort)
        sums_pairs=$(awk '{ print $2 "|" $1 }' SHA256SUMS.txt | sort)
        if [ "$provenance_pairs" = "$sums_pairs" ]; then
            ok 'provenance.json attests exactly the archives SHA256SUMS.txt records, digest for digest'
        else
            say "provenance subjects: $(printf '%s' "$provenance_pairs" | tr '\n' ' ')"
            say "checksum entries:    $(printf '%s' "$sums_pairs" | tr '\n' ' ')"
            fail 'provenance.json does not attest exactly the recorded archive set'
        fi
    else
        fail 'provenance.json is absent from the published dist'
    fi

    self_sha=$(sha256sum <"$SELF")
    self_sha=${self_sha%% *}

    old_ifs=$IFS
    set -f
    IFS='
'
    for archive in $archives; do
        IFS=$old_ifs
        set +f
        ARCHIVE="$archive"
        case "$archive" in
        *.tar.gz)
            MODE=tar
            PAYLOAD_ROOT=${archive%.tar.gz}
            ;;
        *.zip)
            MODE=zip
            PAYLOAD_ROOT=${archive%.zip}
            ;;
        *)
            fail "$archive has no recognised archive extension"
            set -f
            IFS='
'
            continue
            ;;
        esac
        if [ ! -f "$archive" ]; then
            fail "$archive is recorded but absent from $DIR"
            set -f
            IFS='
'
            continue
        fi
        say "auditing $archive (payload root $PAYLOAD_ROOT)"
        MANIFEST_TARGET=''
        MANIFEST_VERSION=''
        MANIFEST_PROFILE=''
        PAYLOAD_SMOKE_SHA=''
        verify_payload "$archive"

        # D3
        if [ "$MANIFEST_PROFILE" = 'release' ]; then
            ok "$archive: the payload manifest records the release profile"
        else
            fail "$archive: the payload manifest records profile '$MANIFEST_PROFILE', not 'release'"
        fi

        # D3b: the archive name must agree with the target it declares.
        if [ "$PAYLOAD_ROOT" = "godot-mcp-$MANIFEST_VERSION-$MANIFEST_TARGET" ]; then
            ok "$archive: its name agrees with the declared version and target"
        else
            fail "$archive: payload root $PAYLOAD_ROOT does not match godot-mcp-$MANIFEST_VERSION-$MANIFEST_TARGET"
        fi

        # D4
        if [ -z "$PAYLOAD_SMOKE_SHA" ]; then
            fail "$archive: no smoke.sh payload entry to compare the executed script against"
        elif [ "$PAYLOAD_SMOKE_SHA" = "$self_sha" ]; then
            ok "$archive: the executed $SELF is byte-identical to its recorded smoke.sh payload ($self_sha)"
        else
            fail "$archive: the executed $SELF hashes to $self_sha but its recorded smoke.sh payload is $PAYLOAD_SMOKE_SHA"
        fi

        set -f
        IFS='
'
    done
    IFS=$old_ifs
    set +f
}

verify_extracted_payload() {
    MODE=dir
    ROOT_DIR="$DIR"
    say "auditing the extracted package at $DIR"
    verify_payload "$DIR"
}

if [ -f "$DIR/SHA256SUMS.txt" ]; then
    say "mode dist: verifying the published release set in $DIR"
    say "runner $(uname -s) $(uname -m)"
    assert_clean_room
    verify_dist
    say 'not verified here: binary execution (--version, self-check, serve listeners),'
    say '                   credential-marker scanning, SBOM accuracy, reproducibility,'
    say '                   and any signature. See this file header for why.'
elif [ -f "$DIR/MANIFEST.json" ] && [ -d "$DIR/bin" ]; then
    say "mode payload: verifying one extracted package in $DIR"
    verify_extracted_payload
    say 'not verified here: the clean-room environment (this mode is not a container),'
    say '                   binary execution, credential-marker scanning, SBOM accuracy.'
else
    die "$DIR is neither a release dist (no SHA256SUMS.txt) nor an extracted package (no MANIFEST.json + bin/)"
fi

if [ "$PROBLEMS" -eq 0 ]; then
    say 'PASS: every check performed above passed'
    exit 0
fi
echo "smoke: FAIL: $PROBLEMS check(s) failed" >&2
exit 1
