# Decision records

Decisions that shaped this release, with the reasoning that produced them. Each record
is written so that a future reader can tell whether the reasoning still holds, rather
than only what was decided.

| Record | Subject |
| --- | --- |
| [0001](0001-gdext-reference-only.md) | The native Godot bridge is reference and test only |
| [0002](0002-loopback-only-http.md) | HTTP binds loopback only, and TLS lives in a proxy |
| [0003](0003-no-arbitrary-script-execution.md) | There is no arbitrary-script-execution tool |
| [0004](0004-editor-plane-cancelled.md) | The editor plane was cancelled for the first release |
| [0005](0005-exact-godot-versions.md) | Only exact Godot patch versions, from one lock |
| [0006](0006-three-state-transactions.md) | A durable mutation has exactly three terminal states |
| [0007](0007-rustsec-2023-0071-resource-server.md) | RUSTSEC-2023-0071 is not exploitable in the resource-server path |
| [0008](0008-one-strong-isolation-release-gate.md) | One strong Docker isolation check satisfies the release plan |
| [0009](0009-aws-lc-rs-jwt-backend.md) | JWT uses aws-lc-rs and removes the vulnerable rsa dependency |
| [0010](0010-complete-development-bridge-strategy.md) | Complete development restores editor/runtime with a default GDScript bridge and optional gdext backend |
