---
layout: home
markdownStyles: false
title: "godot-mcp: an MCP server for Godot projects"
titleTemplate: false
description: godot-mcp is one static Rust binary that gives an MCP client a Godot project — analysis without the engine, journaled edits, and the games and editors it runs, drives, captures and debugs.

hero:
  name: godot-mcp
  text: An MCP server for Godot projects
  tagline: One static binary beside your project. Your agent reads GDScript, scenes and resources without the engine, changes them through journaled transactions, and runs, drives, captures and debugs the game and the editor.
  actions:
    - theme: brand
      text: Install
      link: /guide/install
    - theme: alt
      text: Quick start
      link: /guide/quick-start
    - theme: alt
      text: GitHub
      link: https://github.com/sunerpy/godot-mcp

home:
  facts:
    - term: Runs as
      text: One local process, stdio by default
    - term: Godot
      text: 4.4.0 or later, the build you point it at
    - term: Built as
      text: A Rust server with an optional editor plugin

  stages:
    steps:
      - What an agent sent the running game
      - The screenshot godot-mcp answered
    exchange:
      label: An agent plays the demo game through godot-mcp. input.sequence.send presses ui_right for 2000 ms and answers that both of its steps ran. runtime.wait waits until score is 2 and answers satisfied. runtime.screenshot.capture answers a 960 by 540 artifact of 529325 bytes.
      transcript:
        - kind: call
          text: input.sequence.send
        - kind: argument
          text: "action ui_right  press  holdMs 2000"
        - kind: argument
          text: "wait  ms 2000"
        - kind: answer
          text: "steps_run  2"
        - kind: call
          text: runtime.wait
        - kind: argument
          text: "property score  ==  2"
        - kind: answer
          text: "satisfied  true"
        - kind: call
          text: runtime.screenshot.capture
        - kind: answer
          text: "artifact  960 × 540  529325 bytes"
    screen:
      src: /screens/game.webp
      width: 960
      height: 540
      alt: The demo game. A round teal robot stands on a grassy ledge against a wooden crate, one coin floats above the crate and one at the far left, and the counter in the top-right corner shows 2.
    caption: The demo project in examples/site-demo, running in Godot 4.7.1. The picture is the PNG of this answer, converted to WebP for the page.

  built:
    title: A Rust server, not only an editor plugin
    intro: Your client talks to one Rust binary. GDScript runs inside Godot only where a task needs the engine, so analysis and file edits work with no editor open and no Godot installed.
    client:
      name: Your MCP client
      detail: Starts godot-mcp as a process and talks to it over stdio, or over HTTP when you turn that on, on this machine or across your development network.
    server:
      name: godot-mcp
      detail: One static Rust binary beside the project, with no language runtime to install.
      parts:
        - Speaks MCP
        - Reads GDScript, scenes and resources without the engine
        - Journals every change before it runs
        - Starts and contains every Godot process it uses
    targets:
      - name: The project's files
        detail: GDScript, scenes, resources and settings, read and changed on a machine with no Godot installed.
        status: no-engine
        link: /guide/analysis
      - name: The headless helper
        detail: A GDScript file pinned by its SHA-256, with which Godot writes every scene and resource the server changes.
        status: engine
        link: /guide/editing#scenes-and-resources
      - name: The runtime probe
        detail: GDScript passed on the command line to each game the server starts. No autoload is installed and no setting changes.
        status: engine
        link: /guide/sessions#run-the-game
      - name: The godot_mcp add-on
        detail: An optional editor plugin for the editor you have open. Each change it makes is one undoable editor action.
        status: engine
        link: /guide/editor
    note: A tool that is only an editor plugin works only while the editor runs, and only with what the editor holds. godot-mcp works beside the project, and its add-on brings in your editor when you want it.

  capabilities:
    title: What your agent can do with it
    intro: Each of these is a set of tools the agent calls. The ones marked No engine answer from the project's files on a machine without Godot; the others run the Godot build you configure.
    items:
      - icon: scan-search
        title: Read the project
        wide: true
        status: no-engine
        body: Scene trees, GDScript symbols, references, callers and impact, and the dependency graph, read from the files. Every relation says how sure it is.
        evidence:
          kind: code
          text: |-
            Player       CharacterBody2D  player.gd
            Coins/CoinA.collected → ._on_coin_collected  exact
        link: /guide/analysis
      - icon: file-pen-line
        title: Change it safely
        status: no-engine
        body: Every write names the SHA-256 of what it replaces and is journaled before it runs. A stale edit is refused, and a crash part-way is settled before the next request.
        evidence:
          kind: code
          text: "file.patch player.gd  →  conflict"
        link: /guide/editing
      - icon: cpu
        title: Check, import and test
        status: engine
        body: Scripts checked by Godot itself, the import pipeline, and the project's own GUT or gdUnit4 suite with exact counts, in the build you configure.
        evidence:
          kind: code
          text: "script.check player.gd  →  passed"
        link: /guide/engine
      - icon: package
        title: Export
        status: engine
        body: A preset exported with exactly the templates of your Godot build, as a managed export you can follow and cancel. The output goes to the server's storage, never into the project.
        evidence:
          kind: chips
          items: [export.presets.list, project.export, project.export.status, project.export.cancel]
        link: /guide/engine#export
      - icon: gamepad-2
        title: Run the game
        status: engine
        body: The main scene or any scene, in a window or headless, with a probe the server passes on the command line, so nothing in the project changes.
        evidence:
          kind: code
          text: "project.run.start  →  running, probe connected"
        link: /guide/sessions#run-the-game
      - icon: mouse-pointer-click
        title: Play it and check it
        status: engine
        body: Keys, mouse and actions that are always released afterwards, then waits and assertions in one scenario that reports every step.
        evidence:
          kind: code
          text: "test.scenario.run  →  passed, 5 of 5 steps"
        link: /guide/sessions#test-the-game
      - icon: app-window
        title: Work in your editor
        status: engine
        body: With the `godot_mcp` add-on, edit and save the scenes your editor holds, paint tiles, key animations, capture the window and read the log. Each edit is one undoable action.
        evidence:
          kind: chips
          items: [editor.scene.edit, editor.undo, editor.scene.save, editor.screenshot.capture]
        link: /guide/editor
      - icon: bug
        title: Debug
        status: engine
        body: A headless editor session hosts Godot's language server and debug adapter, for breakpoints, the stack, scopes and variables, and a script's diagnostics.
        evidence:
          kind: chips
          items: [editor.session.start, debug.command, debug.session.get]
        link: /guide/sessions#debug

  showcases:
    - title: Edits you can undo, in your editor
      intro: The `godot_mcp` add-on connects the Godot editor you have open. An agent's change to an open scene goes into the editor's own undo history, and the add-on's panel lists the requests the editor answered.
      screen:
        src: /screens/editor-en-light.webp
        dark: /screens/editor-en-dark.webp
        width: 1600
        height: 1000
        alt: The Godot 4.7.1 editor with the demo project's main scene open in the 2D view. The Player node is selected, and the Inspector shows its script's Speed and Jump Speed. The godot_mcp dock reads godot-mcp connected, and the Godot MCP panel at the bottom lists 13 requests, newest first, from editor.scene.save down to editor.scene.open.
      caption: Godot 4.7.1 with the godot_mcp add-on, on the demo project.
      points:
        - title: One undoable action
          body: "`editor.scene.edit` applies a batch of changes as one entry in the scene's undo history, and `editor.undo` takes it back."
        - title: Nothing saved until you ask
          body: An edit stays in the editor until `editor.scene.save`, and a headless write to a file the editor holds is refused with `conflict`.
        - title: Requests on the panel
          body: The panel lists the newest 500 requests the editor answered, each with its method, what it was about and its result, but never its arguments.
        - title: A switch for you
          body: While **Pause agent edits** is on, the editor refuses every change the server asks it to make; reads still work.
      guide:
        text: The editor add-on
        link: /guide/editor
    - title: A game it plays and checks
      intro: The server starts the game with a probe of its own, sends it input and reads its state back. `test.scenario.run` runs input, waits and assertions in one call and answers a report.
      screen:
        src: /screens/game-scenario.webp
        width: 960
        height: 540
        alt: The demo game after the scenario. The robot is back at the left edge, the coin that was there is gone, one coin still floats above the crate, and the counter shows 3.
      caption: The screenshot the scenario's last step took, converted to WebP for the page.
      points:
        - title: Input that is always released
          body: Every key, button and action is released when its hold ends, when the sequence ends or when the session stops.
        - title: Waits on the game's own state
          body: A wait holds until a node exists or is gone, a property compares, a signal fires or a control shows a text.
        - title: A report, not a log
          body: Each step passes, fails with what was expected and what was found, or says why it could not run.
      exchange:
        label: The report test.scenario.run answered for five steps. An assert_property step, an input step that walks left, a wait until score is 3, an assert_ui_text step on HUD/Count and a screenshot all passed, and the verdict is passed with 5 of 5 steps passed.
        transcript:
          - kind: call
            text: test.scenario.run
          - kind: argument
            text: "steps: assert_property, input, wait,"
          - kind: argument
            text: "       assert_ui_text, screenshot"
          - kind: answer
            text: "two-coins   assert_property  passed"
          - kind: answer
            text: "walk-back   input            passed"
          - kind: answer
            text: "third-coin  wait             passed"
          - kind: answer
            text: "hud         assert_ui_text   passed"
          - kind: answer
            text: "after       screenshot       passed"
          - kind: answer
            text: "verdict  passed   passed 5  total 5"
      guide:
        text: Test the game
        link: /guide/sessions#test-the-game

  clients:
    title: One command in any MCP client
    intro: godot-mcp is a stdio MCP server. Every client that starts a process runs the same command, and most register it in one line.
    items:
      - name: Claude Code
        command: claude mcp add godot-mcp -- godot-mcp serve --project /path/to/my-game --profile full
        link: /guide/clients#claude-code
      - name: Codex
        mark: codex
        command: codex mcp add godot-mcp -- godot-mcp serve --project /path/to/my-game --profile full
        link: /guide/clients#codex
      - name: OpenCode
        mark: opencode
        note: An `mcp` entry in `opencode.json`, in the project or in its global configuration.
        link: /guide/clients#opencode
      - name: Pi Agent
        mark: pi
        command: pi mcp add godot-mcp -- godot-mcp serve --project /path/to/my-game --profile full
        link: /guide/clients#pi-agent
      - name: Cursor
        mark: cursor
        note: The same command as an entry of the `mcpServers` JSON block.
        link: /guide/clients#clients-configured-with-json
    more: Claude Desktop and any other client that starts a process over stdio run the same command, most of them from a JSON block.
    guide:
      text: MCP clients
      link: /guide/clients

  profiles:
    columns: [Profile, Adds, Tools such as]
    rows:
      - cells: [core, Reads; changes nothing, project.info.get scene.tree.get gdscript.references.get]
      - cells: [editor, Writes and the editor add-on, file.patch scene.batch.apply editor.addon.install]
      - cells: [runtime, Runs and watches the game, project.run.start input.sequence.send runtime.screenshot.capture]
      - cells: [full, All three, core editor runtime]
    code: [0, 2]
    caption: A tool outside the profile is not listed. A listed tool whose engine, game or editor is not there answers unavailable and says which one.

  engine:
    title: The engine, exactly
    intro: Every Godot build the project supports is an exact patch version in one lock, with its official download and digest. Each release has an archive for every platform below, smoke-tested there against the newest build.
    versions:
      - version: 4.7.2
        detail: Editor check, export templates, display check and the packaged smoke, on Linux, macOS and Windows
      - version: 4.7.1
        detail: Editor check and export templates, on Linux
      - version: 4.6.1
        detail: Editor check and export templates, on Linux
      - version: 4.5.1
        detail: Editor check and export templates, on Linux
      - version: 4.4.1
        detail: Editor check and export templates, on Linux
    platforms:
      - name: Linux x86_64
        archive: x86_64-unknown-linux-musl
        detail: Statically linked. Games and editors run in a window under X11 or Wayland, or headless, contained by a process group, a delegated cgroup or a container adapter.
      - name: macOS Apple silicon
        archive: aarch64-apple-darwin
        detail: Games and editors run in a window in a logged-in user's session, or headless, contained by a process group or a container adapter.
      - name: Windows x86_64
        archive: x86_64-pc-windows-msvc
        detail: Statically linked C runtime. Games and editors run in a window in a logged-on desktop session, or headless, contained by a Job Object.
    install: The installer checks the archive it downloads against the release's SHA256SUMS.txt.
    guide:
      text: The install guide
      link: /guide/install
    note: The minimum supported version is 4.4.0. Intel macOS, Linux arm64 and Windows ARM64 have no archive; build from source there.

  privacy:
    title: Where your project goes
    intro: godot-mcp works on your machine. It makes no network request of its own, apart from fetching your identity provider's signing keys when you configure OIDC.
    sendsLabel: Goes to
    modes:
      - name: Tool calls
        sends: Your MCP client
        detail: Over stdio the client and the server talk through two pipes. An answer carries the project data the call asked for, and nothing goes anywhere else.
      - name: HTTP, when you turn it on
        sends: The clients that reach the listener
        detail: "`trusted_local` listens on loopback and accepts only requests that carry the per-launch secret, and `proxied_oidc` only an authenticated proxy hop carrying one user's token. `open` listens on the address you give it, for a development network whose machines are yours, with no credential or one token."
      - name: The engine
        sends: Your machine
        detail: Godot runs locally from the build you configure, inside a process boundary the server owns. godot-mcp never downloads an engine or export templates.
      - name: Logs and captures
        sends: Local files
        detail: Diagnostics go to stderr or a rolling file with secrets and source redacted. Screenshots, recordings and exports stay on disk as artifacts until their retention ends.

  faq:
    title: Questions
    items:
      - q: Do I need the Godot editor open?
        a: No. Analysis and file edits work from the project's files with no Godot installed. Checks, imports, tests, exports and games need a Godot executable, which the server starts itself. Only the editor tools need your editor, through the `godot_mcp` add-on.
        guide:
          text: How it is organised
          link: /guide/introduction#how-it-is-organised
      - q: Which parts of a Godot project can it work on?
        a: Animation, AnimationTree, physics, 3D, particles, navigation, audio, shaders, themes, TileMap and the running game, each through tools the agent calls. `scene.recipe.apply` adds a common setup, such as a physics body or smoke particles, to a scene by name, and every recipe is also an MCP prompt the agent can find.
        guide:
          text: What it covers
          link: /guide/capabilities
      - q: Is it an editor plugin?
        a: It includes one, the optional `godot_mcp` add-on, which brings the editor you have open into the work. The server itself is one Rust binary beside the project, so with no editor open it still reads, edits, runs, tests and exports the project.
        guide:
          text: A Rust server, not only an editor plugin
          link: /guide/introduction#a-rust-server-not-only-an-editor-plugin
      - q: Which Godot versions does it support?
        a: Godot 4.4.0 or later. CI checks the exact builds 4.4.1, 4.5.1, 4.6.1, 4.7.1 and 4.7.2, and a tool that needs a newer build says so, such as `editor.log.get`, which needs 4.5.1.
        guide:
          text: Godot versions
          link: /reference/godot-versions
      - q: Can it break my project?
        a: Every write names the SHA-256 of what it replaces and is journaled before it runs, so a stale edit is refused with `conflict` and a crash part-way is settled before the next request. It does not edit binary `.scn` and `.res` files; save them as text first.
        guide:
          text: Editing the project
          link: /guide/editing
      - q: Does it run GDScript an agent writes?
        a: Only after you start the server with `--allow-scripts`, and its typed tools call no engine method outside their own fixed set. Loading a project runs that project's own code, so the server contains what it starts without making an untrusted project safe to open.
        guide:
          text: Threat model
          link: /reference/threat-model
      - q: Can an agent on another machine use it?
        a: "Yes, on a development network whose machines are yours: `godot-mcp serve --http 0.0.0.0:8765` serves MCP over HTTP to the other machines, with no credential or with one token. Beyond such a network, keep HTTP on loopback or put an authenticated reverse proxy in front."
        guide:
          text: HTTP
          link: /operate/http
      - q: Does it send my project anywhere?
        a: No. It makes no network request of its own, apart from fetching your identity provider's signing keys when you configure OIDC, and it never downloads Godot or export templates. One server serves one project for one user; it is not a shared service.
        guide:
          text: HTTP
          link: /operate/http

  closer:
    title: Install godot-mcp
    body: One binary for Linux, macOS and Windows, under the MIT License. Bug reports and feature requests go to [GitHub Issues](https://github.com/sunerpy/godot-mcp/issues).
    actions:
      - text: Install
        link: /guide/install
        theme: brand
        icon: download
      - text: Quick start
        link: /guide/quick-start
        theme: alt
        icon: arrow-right
---

<HomeBuilt />

<HomeCapabilities />

<HomeShowcases />

<HomeClients />

<SplitBlock proof="profiles">

## Choose what it may do

A profile is the ceiling for everything the server may do to your project. `core` reads and
changes nothing. `editor` adds the writes, each one journaled and checked against what it
replaces. `runtime` adds running the project and watching it.

Running an agent's own GDScript is off in every profile until you start the server with
`--allow-scripts`.

[Profiles and answers](guide/profiles.md) · [Tools and resources](../protocol/tool-inventory.md)

</SplitBlock>

<HomeEngine>

::: code-group

```sh [Linux and macOS]
curl -fsSL https://raw.githubusercontent.com/sunerpy/godot-mcp/main/scripts/install.sh | sh
```

```powershell [Windows]
irm https://raw.githubusercontent.com/sunerpy/godot-mcp/main/scripts/install.ps1 | iex
```

```sh [Source]
git clone https://github.com/sunerpy/godot-mcp.git
cd godot-mcp && make dist
```

:::

</HomeEngine>

<HomePrivacy />

<HomeFaq />

<HomeCloser />
