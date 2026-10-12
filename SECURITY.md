# Security Policy

## Supported versions

Security fixes go to the `main` branch and the newest published release. Older
releases do not receive guaranteed backports.

## Report a vulnerability privately

Do **not** open a public issue for a vulnerability, and keep exploit details out
of public discussions. Use GitHub Private Vulnerability Reporting:

<https://github.com/sunerpy/godot-mcp/security/advisories/new>

Include, when available:

- the affected version or commit, platform and installation method;
- a minimal reproduction with the expected and observed behaviour;
- the impact and what an attacker needs to be able to do;
- which boundary it crosses: the project root, a process boundary, the launch
  secret or OIDC authentication of the HTTP transport, the editor bridge, or an
  archive and its checksums;
- logs with secrets and personal paths removed.

You should receive an acknowledgement within seven days. Disclosure is
coordinated once the report is reproduced and a fixed release exists.

## Security boundaries

godot-mcp is a local, single-user server. Over stdio it serves the one client
that started it. Over HTTP it accepts only `trusted_local` (a per-launch secret,
loopback only) or `proxied_oidc` (an authenticated proxy hop plus one end-user
principal). Project files are reached through held directory handles, so a path
that leaves the project, crosses a symlink or a retargeted junction fails before
any effect.

The server starts Godot only for projects you opened. A Godot project can run
arbitrary code when the engine loads it, and the server's process boundaries
contain and stop what it starts; they are not a sandbox for untrusted projects.
The [threat model](docs/security/threat-model.md) states the trust boundaries and
the residual risks.

For ordinary bugs and hardening ideas with nothing confidential, use the public
issue templates.
