#!/bin/sh
# A fake Godot build that SILENTLY IGNORES the flags it was handed, and prints
# plausible-looking progress output while doing so.
#
# This is not a hypothetical. Todo 13 observed the real behaviour directly: an
# export-template build handed `--editor` exits 0 with empty output, and the
# official command-line reference states outright that "unknown command line
# arguments have no effect whatsoever. The engine will not warn you when using a
# command line argument that doesn't exist with a given build type."
#
# The fake goes one step further than the real failure mode, deliberately: it
# prints a `started` banner, so any classifier that reads "the process started"
# or "the process exited 0" as success will report a pass for work that never
# happened. Nothing here is ever a legitimate pass:
#
#   * `--check-only --script <broken script>` exits 0 and reports no parse error;
#   * `--import` exits 0 and writes no import cache;
#   * `-s <framework runner>` exits 0 and prints no framework summary.

printf 'Godot Engine v4.7.1.stable.official - https://godotengine.org\n'
printf 'godot-mcp-fake: started\n'
printf 'godot-mcp-fake: running\n'

# Every argument is consumed and none of them changes anything.
while [ $# -gt 0 ]; do
  case "$1" in
    --version)
      printf '%s\n' '4.7.1.stable.official.b09f793f5'
      exit 0
      ;;
    *)
      shift
      ;;
  esac
done

printf 'godot-mcp-fake: done\n'
exit 0
