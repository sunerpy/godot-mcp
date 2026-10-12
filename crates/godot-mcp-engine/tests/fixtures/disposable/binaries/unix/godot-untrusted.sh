#!/bin/sh
# A fake Godot 4.4+ editor build for Todo 16's disposable-execution suite.
#
# It is NOT a real Godot build and never pretends to be one. It reproduces the
# observable command-line contract this suite depends on, and then behaves like a
# hostile-but-honest untrusted project: it mutates the tree it was pointed at.
#
#   --version
#       Prints a 4.x version string and exits 0.
#
#   --import
#       In a real engine `--import` implies `--editor` and `--quit`, so project
#       code (a @tool script, a @tool autoload) executes. Here the fake performs
#       exactly the mutations that class of code can perform *through the project
#       path it was handed*:
#         1. writes `.godot/uid_cache.bin` — the artifact an import is asked to
#            produce, so the caller has something observable to nominate;
#         2. appends a line to `project.godot` — an in-place source mutation;
#         3. deletes `main.tscn` — a destructive source mutation;
#         4. writes `../godot-mcp-escape-marker.txt`, i.e. one level ABOVE the
#            project root. Under disposable execution that lands inside the
#            disposable root and nowhere else; run against an original tree it
#            would land beside the user's project;
#         5. writes `godot-mcp-saw-excluded.txt` listing every excluded, secret
#            or engine-generated file it can actually read.
#
#       Every one of those is relative to `--path`. That is precisely the class of
#       mutation a disposable copy contains — and precisely the class it does NOT
#       contain is an absolute-path write, which is why this is mutation
#       containment and not a code sandbox.
#
#   Unknown flags are ignored without a diagnostic, exactly as the official
#   command-line reference documents ("unknown command line arguments have no
#   effect whatsoever").
#
# Per-project behaviour comes from <project>/fake-engine.conf:
#   import_sleep_ms=<integer>   outlive a short deadline deliberately

project=""
do_import=0

while [ $# -gt 0 ]; do
  case "$1" in
    --path)
      project="$2"
      shift 2
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

if [ "$do_import" -ne 1 ]; then
  # Nothing recognisable was requested: behave like an engine handed only flags
  # it ignores.
  exit 0
fi

sleep_ms=$(conf_value import_sleep_ms)
[ -n "$sleep_ms" ] || sleep_ms=0
if [ "$sleep_ms" -gt 0 ]; then
  # Outlive a short deadline deliberately, so the supervisor's wall-clock budget
  # decides the outcome rather than this script.
  sleep $((sleep_ms / 1000 + 1))
fi

printf 'Godot Engine v4.7.1.stable.official - https://godotengine.org\n'
printf 'Import: begin\n'

mkdir -p "$project/.godot" || exit 70
printf 'godot-mcp-fake uid cache\n' >"$project/.godot/uid_cache.bin" || exit 70

# An in-place source mutation.
printf '\n; mutated by the untrusted fixture during import\n' >>"$project/project.godot" || exit 70

# A destructive source mutation.
rm -f "$project/main.tscn"

# An escape attempt one level above the project root.
printf 'godot-mcp fixture: the untrusted run reached this directory\n' \
  >"$project/../godot-mcp-escape-marker.txt" 2>/dev/null

# Everything excluded that the run can nonetheless read.
saw="$project/godot-mcp-saw-excluded.txt"
: >"$saw" || exit 70
for candidate in .env .env.production private.pem credentials.json export_presets.cfg \
  .git/config .git/HEAD .godot/project.godot; do
  if [ -f "$project/$candidate" ]; then
    printf '%s\n' "$candidate" >>"$saw"
  fi
done

printf 'Import: end\n'
exit 0
