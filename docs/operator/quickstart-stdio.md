# Quickstart: local stdio

This is the default deployment and the one to start with: one local process, no
socket, no port, no credential. Your MCP client launches `godot-mcp` and speaks
newline-delimited JSON-RPC to it over stdin and stdout.

`stdout` carries protocol frames and nothing else. Every diagnostic goes to
`stderr` or to a rolling file, which is why a log line can never corrupt a frame.

## Before you start

You need a `godot-mcp` release archive for your platform. Get it from the
[project releases page](https://github.com/sunerpy/godot-mcp/releases) and extract
it. Extraction gives you one directory named after the version and target, for
example `godot-mcp-0.1.0-x86_64-unknown-linux-musl`.

Two paths are used throughout this page:

* `$PACKAGE` — the extracted package directory (the one holding
  `MANIFEST.json`).
* `$MCP_BIN` — the executable inside it, `$PACKAGE/bin/godot-mcp`
  (`bin/godot-mcp.exe` on Windows).

Neither name starts with `GODOT_MCP_`, and that is deliberate: the configuration
loader treats every `GODOT_MCP_`-prefixed environment variable as configuration and
rejects one it does not recognize, so a stray helper variable under that prefix
would make `check-config` fail. See [Configuration](configuration.md).

You do **not** need a Godot installation for this quickstart, and you do not need
the Godot editor running. Discovery, analysis and validation of your project's
scripts and scenes are static; only the controlled-engine operations documented
elsewhere ever launch Godot.

## The steps

Each command below is run verbatim by this repository's test suite, in this order,
in a clean temporary environment. If a step needs a path, it is one of the two
variables above and nothing else.

<!-- docs-script:begin name="stdio-smoke" -->
<!-- docs-script:var PACKAGE = the extracted package directory -->
<!-- docs-script:var MCP_BIN = the executable inside that directory -->
<!-- docs-script:var PROJECT = the Godot project root to serve -->

### 1. Verify the package you extracted

Every archive carries a `MANIFEST.json` recording each payload file's size and
SHA-256. `self-check` re-derives them, so a truncated download or a tampered
payload is caught before you run anything against your project.

```sh
"$MCP_BIN" self-check --package "$PACKAGE"
```

### 2. Confirm which build you have

```sh
"$MCP_BIN" version
```

The two lines are the CLI version and the server contract version. The contract
version is what changes when the public tool or resource surface changes.

### 3. Point at a Godot project

Use your own project root here. If you just want to see the server answer a
request, an empty directory with a `project.godot` marker is enough — project
discovery looks for that file at or above the path you pass.

```sh
mkdir -p "$PROJECT" && printf 'config_version=5\n\n[application]\n\nconfig/name="quickstart"\n' > "$PROJECT/project.godot"
```

### 4. Install the project assets the package carries

The package ships the frozen editor-bridge protocol codec and its wire schema.
Installing them writes into one owned subdirectory of your project and refuses any
path that is already occupied by something the installer does not own.

```sh
"$MCP_BIN" assets install --project "$PROJECT" --package "$PACKAGE"
```

Re-running `assets update` later is idempotent: with no drift it changes nothing,
and a file you edited yourself is backed up before it is replaced. `assets remove`
deletes only the files whose contents still match the install manifest, so your
edits are deliberately kept rather than discarded. See
[Editor bridge](editor-bridge.md) for what these assets are and are not.

### 5. Validate a configuration file

The archive ships a valid, credential-free example configuration. `check-config`
runs the production loader and every security invariant, and exits non-zero on a
rejection rather than starting with a configuration it does not like.

```sh
"$MCP_BIN" check-config --config "$PACKAGE/examples/godot-mcp.toml"
```

### 6. Make a real request

This is the whole protocol interaction: send one `initialize` request on stdin,
read the response frame on stdout.

```sh
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"quickstart","version":"0.1.0"}}}' | "$MCP_BIN" serve --stdio --project "$PROJECT" --profile core | grep -q '"serverInfo"'
```

<!-- docs-script:end -->

A successful run means the server acquired the project lease, opened the durable
transaction journal, reconciled anything a previous process left open, and answered
on the shared kernel. Startup writes one reconciliation line per outcome and the
write-gate status to `stderr`.

## What you have now

With the `core` profile the server exposes read-only discovery and analysis: the
project graph, scene and resource trees, GDScript symbols, references, impact and
diagnostics, plus `capabilities.get` and `health.get`. Nothing in `core` can write
to your project or start a process.

Four resources are always available:

* `godot-mcp://resources/capabilities`
* `godot-mcp://resources/health`
* `godot-mcp://resources/project`
* `godot-mcp://resources/scheduler`

The full generated list is in
[Profiles, tools and resources](../protocol/tool-inventory.md).

## Next steps

* Wire it into your client: [MCP clients](mcp-clients.md).
* Understand what an answer means, including when it is honestly uncertain:
  [Profiles and tool outcomes](profiles-and-outcomes.md).
* Tune the loader: [Configuration](configuration.md).
* Only if a second local process genuinely needs to reach the same server:
  [HTTP: `trusted_local`](http-trusted-local.md). HTTP is off by default and is
  not required for normal single-client use.
