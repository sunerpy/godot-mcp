<!-- docs-generated: error-taxonomy -->
<!-- Regenerate with: cargo run -p godot-mcp-cli --features test-hooks --bin docs-check -- --write -->
# Outcome taxonomy

This page is generated from the `schemars` schemas of `godot_mcp_domain`'s own enums, so a variant
added or removed in code shows up here and nowhere else. Editing it by hand fails the docs
check; change the canonical source and regenerate instead.

## Error codes

Every failed operation carries exactly one of these codes. `uncertain` is
not a synonym for `internal`: it is the honest answer when a static analysis
cannot prove a result, and it is reported instead of a confident guess.

* `cancelled`
* `conflict`
* `internal`
* `invalid_input`
* `not_found`
* `permission_denied`
* `timeout`
* `unavailable`
* `uncertain`
* `unsupported`

## Operation planes

The plane an operation acts on, expressed by outcome rather than by
implementation.

* `editor`
* `project`
* `runtime`
* `static`

## Capabilities

The capability an operation consumes. There is no capability variant for
arbitrary script execution.

* `editor_mutate`
* `export_destructive`
* `project_write`
* `read`
* `runtime_execute`

## Static confidence

Static answers are never silently presented as certain. Each carries
provenance and one of these levels.

* `exact`
* `inferred`
* `unresolved`

## Profiles

The selectable capability profiles.

* `core`
* `editor`
* `full`
* `runtime`
