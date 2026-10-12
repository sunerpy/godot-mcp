#!/bin/sh
# A fake Godot 4.4+ editor build that runs the bundled canonical-write helper.
#
# It is NOT real Godot and never pretends to be one. It reproduces exactly the
# observable contract Todo 17 depends on:
#
#   --headless --path <project> --script res://<helper> -- --request <rel> --response <rel>
#
# and it behaves like the *engine side* of that contract:
#
#   * everything after the bare `--` is the script's user arguments, exactly what
#     `OS.get_cmdline_user_args()` returns;
#   * the helper writes ONLY inside `res://godot_mcp_helpers/<v>/staging/` plus the
#     response document it was told to write. It never touches the caller's target
#     path. Publishing is the Rust side's compare-and-swap;
#   * `.scn` / `.res` targets are refused with the helper's own exit 65;
#   * exit statuses are the helper's four: 0 ok, 64 usage, 65 refused, 70 failed;
#   * an unknown flag is ignored without a diagnostic, exactly as the official
#     command-line reference documents ("unknown command line arguments have no
#     effect whatsoever").
#
# The UID values it emits stand in for `ResourceUID.create_id_for_path()` /
# `id_to_text()`. They are produced HERE, on the engine side of the boundary — the
# Rust side never derives a UID, and this fixture is what makes that assertable
# without a real Godot installation.
#
# Per-project behaviour comes from <project>/fake-engine.conf:
#   helper=ok                       run the helper normally
#   helper=silent-ignore            ignore --script entirely and exit 0 (the
#                                   silent-no-op an export template performs)
#   helper=no-response              write the staged artifact but no response
#   helper=save-failed              report the helper's save_failed / exit 70
#   helper=uid-conflict:<path>      report a uid_conflict against <path>
#   helper=partial-write            write HALF the artifact, then exit 0 (an
#                                   interrupted non-atomic producer)
#   helper=batch-refused            refuse an apply_batch at its first operation
#   uid=<uid://...>                 the UID the engine "mints" for this project
#   record=<absolute path>          copy each helper request document there

project=""
script=""
request=""
response=""
saw_separator=0

while [ $# -gt 0 ]; do
  if [ "$saw_separator" -eq 1 ]; then
    case "$1" in
      --request)
        request="$2"
        shift 2
        ;;
      --response)
        response="$2"
        shift 2
        ;;
      *)
        shift
        ;;
    esac
    continue
  fi
  case "$1" in
    --)
      saw_separator=1
      shift
      ;;
    --path)
      project="$2"
      shift 2
      ;;
    --script)
      script="$2"
      shift 2
      ;;
    --headless)
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

mode=$(conf_value helper)
[ -n "$mode" ] || mode="ok"
uid=$(conf_value uid)
[ -n "$uid" ] || uid="uid://bfakeengine001"

# ---- the silent no-op an export-template build performs --------------------
# The engine ignores a flag it does not support and exits 0 having done nothing.
# No response document, no staged artifact, no diagnostic.
if [ "$mode" = "silent-ignore" ]; then
  exit 0
fi

# Nothing recognisable was requested.
if [ -z "$script" ]; then
  exit 0
fi

if [ -z "$request" ] || [ -z "$response" ]; then
  printf 'canonical_write: --request and --response are both required\n' >&2
  exit 64
fi

record=$(conf_value record)
if [ -n "$record" ]; then
  cp "$project/$request" "$record"
fi

request_file="$project/$request"
response_file="$project/$response"

if [ ! -f "$request_file" ]; then
  printf 'canonical_write: unreadable request %s\n' "$request" >&2
  exit 64
fi

req_value() {
  sed -n "s/^$1=//p" "$request_file" | head -1
}

operation=$(req_value operation)
target=$(req_value target)
artifact=$(req_value artifact)
resource_name=$(req_value resource_name)
root_type=$(req_value root_type)
root_name=$(req_value root_name)

write_response() {
  # The helper sorts its keys, so the response bytes are stable.
  mkdir -p "$(dirname "$response_file")" || exit 70
  printf '%s' "$1" | sort >"$response_file" || exit 70
}

refuse() {
  write_response "helper_version=2
reason=$1
status=refused
"
  exit 65
}

# ---- the helper's own refusals ---------------------------------------------
case "$artifact" in
  godot_mcp_helpers/*) ;;
  *) refuse artifact_outside_staging ;;
esac
case "$artifact" in
  godot_mcp_helpers/2/staging/*) ;;
  *) refuse artifact_outside_staging ;;
esac

if [ -n "$target" ]; then
  extension=$(printf '%s' "$target" | tr 'A-Z' 'a-z' | sed -n 's/.*\.\([a-z0-9]*\)$/\1/p')
  case "$extension" in
    scn | res)
      write_response "helper_version=2
reason=binary_resource
status=refused
target=res://$target
"
      exit 65
      ;;
    tscn | tres) ;;
    *) refuse not_a_text_resource ;;
  esac
fi

case "$operation" in
  create_resource)
    if [ -f "$project/$target" ]; then
      refuse already_exists
    fi
    ;;
  update_resource | register_uid | apply_batch)
    if [ ! -f "$project/$target" ]; then
      refuse target_missing
    fi
    ;;
  query_uid) ;;
  *) refuse unknown_operation ;;
esac

# ---- configured failure modes ----------------------------------------------
if [ "$operation" = "apply_batch" ] && [ "$mode" = "batch-refused" ]; then
  write_response "detail=Missing
helper_version=2
op_index=0
reason=node_not_found
status=refused
"
  exit 65
fi
case "$mode" in
  save-failed)
    write_response "helper_version=2
reason=save_failed
status=failed
"
    exit 70
    ;;
  uid-conflict:*)
    existing=$(printf '%s' "$mode" | sed 's/^uid-conflict://')
    write_response "existing_path=res://$existing
helper_version=2
reason=uid_conflict
requested_path=res://$target
status=refused
uid=$uid
"
    exit 65
    ;;
esac

# ---- produce the canonical artifact ---------------------------------------
# This is the engine's `ResourceSaver.save()` standing in for itself: the bytes
# are produced on the engine side of the boundary, never in Rust.
artifact_file="$project/$artifact"
mkdir -p "$(dirname "$artifact_file")" || exit 70

if [ "$operation" = "apply_batch" ]; then
  # The engine applied the batch to the loaded target: the fake keeps the
  # target's bytes and appends one node, so the result differs from the input
  # and still parses as the same kind of document.
  {
    cat "$project/$target"
    printf '\n[node name="Batched" type="Node" parent="."]\n'
  } >"$artifact_file" || exit 70
elif [ "$extension" = "tres" ]; then
  {
    printf '[gd_resource type="Resource" format=3 uid="%s"]\n\n' "$uid"
    printf '[resource]\n'
    if [ -n "$resource_name" ]; then
      printf 'resource_name = "%s"\n' "$resource_name"
    fi
  } >"$artifact_file" || exit 70
else
  [ -n "$root_type" ] || root_type="Node"
  [ -n "$root_name" ] || root_name="Root"
  {
    printf '[gd_scene load_steps=1 format=3 uid="%s"]\n\n' "$uid"
    printf '[node name="%s" type="%s"]\n' "$root_name" "$root_type"
  } >"$artifact_file" || exit 70
fi

# An interrupted non-atomic producer: half the bytes are on disk and the process
# still reports success. The Rust side publishes atomically, so the target must
# never end up holding this partial state.
if [ "$mode" = "partial-write" ]; then
  head -c 12 "$artifact_file" >"$artifact_file.partial" 2>/dev/null || exit 70
  mv "$artifact_file.partial" "$artifact_file" || exit 70
fi

if [ "$mode" = "no-response" ]; then
  # The staged artifact exists but the helper never answered, so the run cannot
  # be interpreted.
  exit 0
fi

write_response "artifact=res://$artifact
helper_version=2
status=ok
target=res://$target
uid=$uid
"
exit 0
