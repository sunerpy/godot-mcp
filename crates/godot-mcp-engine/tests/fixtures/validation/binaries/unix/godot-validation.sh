#!/bin/sh
# A fake Godot 4.4+ editor build, for validation / import / test execution.
#
# It is NOT a real Godot build and never pretends to be one. It reproduces
# exactly the observable command-line contract Todo 15 depends on, as verified
# against the official engine sources:
#
#   --check-only --script <path>
#       Parses one script and quits. Exit 0 when the script is valid, exit 1
#       when it is not (main/main.cpp: `return script_res->is_valid() ?
#       EXIT_SUCCESS : EXIT_FAILURE`). A parse error is reported on *stderr* in
#       the engine's exact two-line form:
#           SCRIPT ERROR: Parse Error: <message>
#              at: GDScript::reload (<file>:<line>)
#       A *clean* script prints nothing at all — which is precisely why a zero
#       exit on its own can never be read as "the check ran".
#
#       Whether a script is "valid" is decided from the script's own bytes: a
#       script containing the token godot_mcp_calibration_syntax_error fails, and the
#       reported line is the line that token appears on. That makes the fake
#       behave like a parser for any script handed to it, including a canary the
#       caller writes outside the project.
#
#   --import
#       Implies --editor and --quit (main.cpp: `editor = true; ...
#       quit_after = 1`). Introduced in 4.2. Writes the engine's import cache
#       under <project>/.godot/ and quits. On a broken resource it reports the
#       failure and exits 1.
#
#   -s <script> [framework args]
#       Runs a script. Used by both GUT and gdUnit4 CLI runners. The fake replays
#       a committed output document and exits with a committed status, so the
#       framework summary this suite parses is real captured framework text
#       rather than something invented in Rust.
#
#   Unknown flags are ignored without a diagnostic, exactly as the official
#   command-line reference documents ("unknown command line arguments have no
#   effect whatsoever").
#
# Per-project behaviour comes from <project>/fake-engine.conf:
#   check_failure_exit=<integer, the status a parse error exits with; default
#     1, and 0 reproduces Godot's macOS builds, which report the error and exit 0>
#   import=ok | import=fail:<res:// path>
#   test_output=<project-relative file replayed to stdout>
#   test_exit=<integer>
#   test_sleep_ms=<integer, makes the run outlive a short deadline>

project=""
script=""
check_only=0
do_import=0
run_script=""

while [ $# -gt 0 ]; do
  case "$1" in
    --path)
      project="$2"
      shift 2
      ;;
    --script)
      script="$2"
      shift 2
      ;;
    -s)
      run_script="$2"
      shift 2
      ;;
    --check-only)
      check_only=1
      shift
      ;;
    --import)
      do_import=1
      shift
      ;;
    --version)
      printf '%s\n' '4.7.1.stable.official.b09f793f5'
      exit 0
      ;;
    *)
      # Unknown or irrelevant option: ignored without a diagnostic.
      shift
      ;;
  esac
done

if [ -z "$project" ]; then
  project="$PWD"
fi

conf="$project/fake-engine.conf"

conf_value() {
  [ -f "$conf" ] || return 0
  sed -n "s/^$1=//p" "$conf" | head -1
}

# Resolve a script argument the way Godot does: a res:// path is project
# relative, anything else is taken as a filesystem path.
resolve() {
  case "$1" in
    res://*)
      printf '%s/%s\n' "$project" "$(printf '%s' "$1" | sed 's|^res://||')"
      ;;
    *)
      printf '%s\n' "$1"
      ;;
  esac
}

# ---- --check-only --script -------------------------------------------------
if [ "$check_only" -eq 1 ] && [ -n "$script" ]; then
  resolved=$(resolve "$script")
  if [ ! -f "$resolved" ]; then
    printf 'ERROR: Can'\''t load script: %s\n' "$script" >&2
    printf '   at: main (main/main.cpp:3927)\n' >&2
    exit 1
  fi
  line=$(grep -n 'godot_mcp_calibration_syntax_error' "$resolved" | head -1 | cut -d: -f1)
  if [ -n "$line" ]; then
    printf 'SCRIPT ERROR: Parse Error: Expected statement, found "Identifier" instead.\n' >&2
    printf '   at: GDScript::reload (%s:%s)\n' "$script" "$line" >&2
    failure_exit=$(conf_value check_failure_exit)
    exit "${failure_exit:-1}"
  fi
  # A valid script: the engine prints nothing whatsoever.
  exit 0
fi

# ---- --import --------------------------------------------------------------
if [ "$do_import" -eq 1 ]; then
  mode=$(conf_value import)
  [ -n "$mode" ] || mode="ok"
  case "$mode" in
    fail:*)
      resource=$(printf '%s' "$mode" | sed 's/^fail://')
      printf 'ERROR: Failed to import resource: %s\n' "$resource" >&2
      printf '   at: _reimport_file (editor/editor_file_system.cpp:2371)\n' >&2
      exit 1
      ;;
    *)
      mkdir -p "$project/.godot" || exit 70
      printf 'godot-mcp-fake uid cache\n' >"$project/.godot/uid_cache.bin" || exit 70
      printf 'Godot Engine v4.7.1.stable.official - https://godotengine.org\n'
      printf 'Import: begin\n'
      printf 'Import: end\n'
      exit 0
      ;;
  esac
fi

# ---- -s <framework runner> -------------------------------------------------
if [ -n "$run_script" ]; then
  sleep_ms=$(conf_value test_sleep_ms)
  [ -n "$sleep_ms" ] || sleep_ms=0
  if [ "$sleep_ms" -gt 0 ]; then
    # Outlive a short deadline deliberately, so the supervisor's wall-clock
    # budget decides the outcome rather than this script.
    sleep $((sleep_ms / 1000 + 1))
  fi
  output=$(conf_value test_output)
  if [ -n "$output" ] && [ -f "$project/$output" ]; then
    cat "$project/$output"
  fi
  status=$(conf_value test_exit)
  [ -n "$status" ] || status=0
  exit "$status"
fi

# Nothing recognisable was requested: behave like an engine handed only flags it
# ignores.
exit 0
