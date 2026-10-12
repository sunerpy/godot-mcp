# Quick start

This page registers godot-mcp with an MCP client, makes the first calls and then gives the server
a Godot build. It assumes `godot-mcp` is [installed](install.md) and on your PATH.

## Register it with your client

The client starts the server with the project directory — the one holding `project.godot`, or any
directory below it — and a profile. With Claude Code:

```sh
claude mcp add godot-mcp -- godot-mcp serve --project /path/to/my-game
```

Any other client that starts a process takes the same command and arguments; most accept this
shape ([MCP clients](clients.md) has more):

```json
{
  "mcpServers": {
    "godot-mcp": {
      "command": "godot-mcp",
      "args": ["serve", "--project", "/path/to/my-game", "--profile", "core"]
    }
  }
}
```

Without `--profile` the server starts in `core`, which reads the project and changes nothing.

## Make the first calls

Ask your agent about the project — "what does the main scene contain", "who calls
`take_damage`". A well-behaved agent starts with two calls:

- `capabilities.get` lists the tools and resources of every profile.
- `health.get` names the profile this server runs and says what the machine can serve right now:
  whether a Godot build was found, whether a game or an editor is running, the state of the
  journal and the request queue, each with a remedy when something is missing.

Then come the analysis tools: `project.info.get`, `scene.tree.get`, `gdscript.symbols.get`,
`gdscript.references.get`, `gdscript.impact.get`, `project.dependencies.get`. They answer from the
files, so a machine without Godot gets the same answers. Each answer carries its provenance and
how sure it is, and an analysis that cannot be certain says `inferred` or `uncertain` instead of
guessing.

Send one request at a time and wait for its answer. The server limits concurrent requests per
caller, and a client that sends a burst gets
`operational limit subject_concurrency was exceeded` for the ones over the limit.

## Give it a Godot build

The tools that run Godot need an editor build of Godot 4.4.0 or later. The server looks in the
usual install locations and on `PATH`; to name one, put it in a configuration file:

<!-- docs-check:config expect="accept" -->
```toml
transport = "stdio"
godot_binary = "/opt/godot/godot"
```

and pass the file with `--config`, with the profile that lists the tools you want:

```sh
claude mcp add godot-mcp -- godot-mcp serve --project /path/to/my-game --profile full --config /path/to/godot-mcp.toml
```

`godot-mcp check-config --config /path/to/godot-mcp.toml` runs the same loader and every check the
server runs, and says why a file is refused. `health.get` then reports the Godot build it resolved,
its version and what the server probed it for. The keys are in the
[configuration reference](../../operator/configuration.md).

## Choose a profile

| Profile | Adds | Choose it when |
| --- | --- | --- |
| `core` | Discovery and analysis | You want answers and nothing changed |
| `editor` | Files, scripts, scenes, resources, settings, assets, the editor add-on | You want the project changed |
| `runtime` | Running, testing, exporting and observing the game | You want the project run and watched |
| `full` | All three | You want all of it |

A write in `editor` or `full` is a journaled transaction against the exact bytes it replaces; the
next page, [Profiles and answers](profiles.md), explains what each answer means.
