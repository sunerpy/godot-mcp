---
layout: home
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
      text: One process your MCP client starts, beside your project, on Linux, macOS or Windows. It speaks stdio by default, with no port and no account.
    - term: Godot
      text: Analysis reads the project's files and needs no engine. Running, testing and exporting use the Godot 4.4.0 or later build you point it at.

  visual:
    label: An MCP exchange with godot-mcp on a small Godot project. scene.tree.get on main.tscn answers its five nodes with their types and scripts, and the exact connection of Mob's body_entered signal to the root. gdscript.references.get for take_damage in player/player.gd answers one inferred call, at main.gd line 6.
    transcript:
      - kind: call
        text: scene.tree.get
      - kind: argument
        text: "scene: main.tscn"
      - kind: answer
        text: ".            Node         main.gd"
      - kind: answer
        text: "Player       Area2D       player/player.gd"
      - kind: answer
        text: "HUD          CanvasLayer  ui/hud.gd"
      - kind: answer
        text: "HUD/Message  Label"
      - kind: answer
        text: "Mob          RigidBody2D"
      - kind: answer
        text: "Mob.body_entered → ._on_mob_body_entered  exact"
      - kind: call
        text: gdscript.references.get
      - kind: argument
        text: "script: player/player.gd"
      - kind: argument
        text: "symbol: take_damage"
      - kind: answer
        text: "main.gd:6  player.take_damage()  calls  inferred"

  index:
    title: What godot-mcp does
    intro: Everything here is a tool your agent calls. The ones marked No engine answer from the project's files on a machine without Godot; the others need a Godot executable, and some of them a game or an editor that godot-mcp starts itself.
    groups:
      - name: Read the project
        items:
          - title: Project and settings
            body: The project root, its name and main scene, every setting and every file, found from any directory inside the project.
            status: no-engine
            link: /guide/analysis#the-project
          - title: GDScript
            body: Symbols, references, callers and impact across scripts, each edge with how sure it is.
            status: no-engine
            link: /guide/analysis#gdscript
          - title: Scenes and resources
            body: The node tree, scripts, instances, groups and signal connections of text .tscn and .tres files.
            status: no-engine
            link: /guide/analysis#scenes-and-resources
          - title: Dependencies
            body: What a scene or resource loads and what loads it, with the references that point at nothing.
            status: no-engine
            link: /guide/analysis#dependencies
      - name: Change it
        items:
          - title: Files and scripts
            body: Create, patch, move and delete, each against the SHA-256 of what it changes, so a stale edit is refused instead of applied.
            status: no-engine
            link: /guide/editing#files-and-scripts
          - title: Scenes and resources
            body: Node, property and connection changes as one batch, journaled as one transaction, through Godot or the open editor.
            status: engine
            link: /guide/editing#scenes-and-resources
          - title: Settings and assets
            body: The main scene, autoloads, input actions, plugins and settings changed through typed operations, and binary assets uploaded in chunks.
            status: no-engine
            link: /guide/editing#settings-and-assets
          - title: The editor
            body: The godot_mcp add-on connects a running editor; a change to an open scene becomes one undoable editor action.
            status: engine
            link: /guide/editor
      - name: Run it
        items:
          - title: Check, import and test
            body: GDScript checked by Godot itself, the import pipeline, and the project's tests with exact counts.
            status: engine
            link: /guide/engine
          - title: Export
            body: A preset exported with the exact templates of your Godot build, as a managed export you can follow and cancel.
            status: engine
            link: /guide/engine#export
          - title: Run the game
            body: The main scene or any scene, in a window or headless, with its logs and status, until you stop it.
            status: engine
            link: /guide/sessions#run-the-game
      - name: Drive and debug
        items:
          - title: Input
            body: Keys, mouse, actions and timed sequences, each released when its hold ends or the session stops.
            status: engine
            link: /guide/sessions#send-input
          - title: Live reads
            body: The running scene tree, properties, signals and UI, read without changing the game.
            status: engine
            link: /guide/sessions#read-the-running-game
          - title: Captures
            body: Screenshots and short recordings, returned as artifacts with their SHA-256.
            status: engine
            link: /guide/sessions#capture
          - title: Debugger
            body: Breakpoints, the stack, scopes and variables through the editor's debug adapter, and its language server's diagnostics.
            status: engine
            link: /guide/sessions#debug

  steps:
    title: From install to the first answer
    items:
      - title: Install
        body: Run the installer for your platform from the Install section below, or download an archive from GitHub Releases; the installer checks it against the release's SHA256SUMS.txt. No release is published yet, so for now build from source.
      - title: Register it with your client
        command: claude mcp add godot-mcp -- godot-mcp serve --project /path/to/my-game
        body: Any MCP client that starts a process takes the same command. It starts in the read-only core profile.
      - title: Ask about the project
        body: Ask your agent about a scene or a script. It calls capabilities.get and health.get to see what it may do here, then the analysis tools.
      - title: Grant more when you need it
        command: --profile full
        body: editor adds writes, runtime adds running and watching the game. Point godot_binary at your Godot build for the engine-backed tools.

  profiles:
    columns: [Profile, Adds, Tools such as]
    rows:
      - cells: [core, Reads; changes nothing, project.info.get scene.tree.get gdscript.references.get]
      - cells: [editor, Writes and the editor add-on, file.patch scene.batch.apply editor.addon.install]
      - cells: [runtime, Runs and watches the game, project.run.start input.sequence.send runtime.screenshot.capture]
      - cells: [full, All three, core editor runtime]
    code: [0, 2]
    caption: A tool outside the profile is not listed. A listed tool whose engine, game or editor is not there answers unavailable and says which one.

  sessions:
    columns: [To, Call, Notes]
    rows:
      - cells: [Run the game, project.run.start project.run.stop, "In a window or headless, until its lifetime ends"]
      - cells: [Press and click, input.key.send input.mouse.send input.action.send input.sequence.send, Everything held is released]
      - cells: [Read it live, runtime.tree.get runtime.properties.get runtime.signals.get runtime.ui.get, Changes nothing in the game]
      - cells: [Capture it, runtime.screenshot.capture runtime.recording.capture, "PNG or animated PNG, as an artifact"]
      - cells: [Debug it, editor.session.start debug.command, "Breakpoints, stack and variables"]
    code: [1]
    caption: At most four sessions run at once. Each belongs to the server that started it, and a server that starts after a crash ends the ones left behind.

  versions:
    columns: [Godot, CI runs, Platforms]
    rows:
      - cells: [4.7.1, "Engine suite, export templates, display check, packaged smoke", "Linux, macOS, Windows"]
      - cells: [4.6.1, "Engine suite, export templates", Linux]
      - cells: [4.5.1, "Engine suite, export templates", Linux]
      - cells: [4.4.1, "Engine suite, export templates", Linux]
    code: [0]
    caption: Exact builds from one lock, each checked against its official SHA-256. The minimum supported version is 4.4.0.

  platforms:
    title: Platforms
    intro: Every release has an archive for each platform below, built and smoke-tested on that platform against the locked Godot build.
    columns: [Platform, Archive, Games and editors]
    rows:
      - name: Linux x86_64
        cells: ["x86_64-unknown-linux-musl, statically linked", "A window under X11 or Wayland, or headless; contained by a process group, a delegated cgroup or a container adapter"]
      - name: macOS Apple silicon
        cells: [aarch64-apple-darwin, "A window in a logged-in user's session, or headless; contained by a process group or a container adapter"]
      - name: Windows x86_64
        cells: ["x86_64-pc-windows-msvc, crt-static", "A window in a logged-on desktop session, or headless; contained by a Job Object"]
    note: Intel macOS, Linux arm64 and Windows ARM64 have no archive; build from source there.

  privacy:
    title: Where your project goes
    intro: godot-mcp works on your machine. It makes no network request of its own, apart from fetching your identity provider's signing keys when you configure OIDC.
    sendsLabel: Goes to
    modes:
      - name: Tool calls
        sends: Your MCP client
        detail: Over stdio the client and the server talk through two pipes. An answer carries the project data the call asked for, and nothing goes anywhere else.
      - name: HTTP, when you turn it on
        sends: A loopback listener
        detail: trusted_local accepts only requests that carry the per-launch secret; proxied_oidc only an authenticated proxy hop carrying one user's token.
      - name: The engine
        sends: Your machine
        detail: Godot runs locally from the build you configure, inside a process boundary the server owns. godot-mcp never downloads an engine or export templates.
      - name: Logs and captures
        sends: Local files
        detail: Diagnostics go to stderr or a rolling file with secrets and source redacted. Screenshots, recordings and exports stay on disk as artifacts until their retention ends.

  scope:
    title: What it does not do
    items:
      - It does not run arbitrary GDScript or call arbitrary engine methods. No profile has such a tool.
      - It does not download Godot or export templates. It uses the build and templates you installed.
      - It does not edit binary .scn and .res files. It asks for them to be saved as text instead.
      - It does not make an untrusted project safe to open. Loading a project runs its code.
      - It is not a shared service. One server serves one project for one user.
---

<HomeIndex />

<HomeSteps />

<SplitBlock proof="profiles">

## Choose what it may do

A profile is the ceiling for everything the server may do to your project. `core` reads and
changes nothing. `editor` adds the writes, each one journaled and checked against what it
replaces. `runtime` adds running the project and watching it. There is no tool that executes
arbitrary code, in any profile.

[Profiles and answers](guide/profiles.md) · [Tools and resources](../protocol/tool-inventory.md)

</SplitBlock>

<SplitBlock proof="sessions" flip>

## A game it can play and watch

godot-mcp starts the game itself, with a probe of its own that changes nothing in the project.
Your agent presses keys, reads the live scene tree and takes screenshots; an editor session adds
breakpoints and the call stack. When the session ends, every input it held is released.

[Games and editors](guide/sessions.md) · [Managed sessions](../operator/managed-sessions.md)

</SplitBlock>

<SplitBlock proof="versions">

## The engine, exactly

Every Godot build the project supports is an exact patch version in one lock, with its official
download and digest. CI runs the engine suite against each of them and smoke-tests every release
archive against the newest, on its own platform.

[Running the engine](guide/engine.md) · [Godot versions](../operator/version-matrix.md)

</SplitBlock>

<HomePlatforms />

<HomePrivacy />

## Install

No release is published yet, so the installers have nothing to download; until the first release,
use the Source tab.

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

The [install guide](guide/install.md) covers the archives, verifying a download and building from
source.

<HomeScope />

## Feedback

Bug reports and feature requests go to [GitHub Issues](https://github.com/sunerpy/godot-mcp/issues).
godot-mcp is MIT licensed and is not affiliated with the Godot Engine project.
