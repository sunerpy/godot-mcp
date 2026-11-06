# docs/site

The pages of the godot-mcp website, published at <https://firlab.app/godot-mcp/>. This file is
for people who edit them; it is not itself a page of the site.

The words live here, next to the code they describe, so a change that alters what a user sees
updates the page in the same pull request. The site itself — its theme, components, build and
deployment — lives in [sunerpy/firlab](https://github.com/sunerpy/firlab) under `godot-mcp/`.
Its sync script copies these pages and the references they link into the site; do not edit the
copies there.

These pages are also checked by `docs-check` with the rest of `docs/`: tool names, configuration
keys, links, example hosts and Godot versions follow the same rules as every other page
([Documentation rules](../contributor/documentation.md)).

## What is published

| Source | Published at |
| --- | --- |
| `docs/site/<path>.md` | `/godot-mcp/<path>` |
| `docs/site/zh/<path>.md` | `/godot-mcp/zh/<path>` |
| `docs/operator/configuration.md` | `/godot-mcp/reference/configuration`, English only |
| `docs/protocol/tool-inventory.md` | `/godot-mcp/reference/tools`, English only |
| `docs/protocol/error-taxonomy.md` | `/godot-mcp/reference/outcomes`, English only |
| `docs/operator/version-matrix.md` | `/godot-mcp/reference/godot-versions`, English only |
| `docs/operator/managed-sessions.md`, `exports.md`, `editor-bridge.md`, `artifacts.md`, `process-boundaries.md`, `recovery.md`, `recipes.md`, `http-trusted-local.md`, `http-proxied-oidc.md`, `http-open.md` | `/godot-mcp/reference/<name>`, English only |
| `docs/security/threat-model.md` | `/godot-mcp/reference/threat-model`, English only |
| `docs/operator/usage-and-verification.md` | `/godot-mcp/zh/reference/usage`, Chinese only |
| `docs/site/public/godot-mcp-logo.svg` | The site's logo and favicon |
| `docs/site/public/screens/<name>.webp` | `/godot-mcp/screens/<name>.webp`, the screenshots the pages show |

A reference in one language only gets a short page in the other language that links to it. The
sync script in FirLab (`godot-mcp/scripts/sync-godot-mcp-docs.sh`) holds the exact mapping;
publishing another reference means adding it there and to the sidebars in the same FirLab
change.

## Rules the sync checks

The sync stops before it writes anything when:

- a page exists in one language only: every `docs/site/<path>.md` needs
  `docs/site/zh/<path>.md`, and the reverse;
- a page sits where a synced reference goes, such as `docs/site/reference/tools.md`;
- a page uses a component the theme does not register (see below);
- a file in `public/screens/` is not named `<name>.webp` in lower-case letters, digits and hyphens,
  or a page refers to a `/screens/` file that does not exist;
- a page uses a colloquial word (`just`, `simply`, `stuff`; `搞定`, `折腾`, `试试`, `看看` and the
  like) or names the source tree (`crates/godot-mcp-…/`, `src/….rs`) or a plan's task number;
  `developers.md` may name the source tree.

## Writing

- Write for someone who uses godot-mcp with a Godot project, not for someone who works on it. Say
  what to do and what happens; leave the internals to the references and `developers.md`.
- Tool names, arguments, commands, configuration keys, outcome codes and environment variables
  go in code spans.
- Every number, version and answer on a page comes from the code, a release or a recorded run.
  Godot versions are exact patch versions (4.7.1), never a major-minor alias.
- Leave out how a page was checked: no "validated with", "recorded on" or test notes. That
  evidence belongs in the pull request.
- Do not name private paths or hosts. Examples use a project under `/path/to/my-game`, loopback
  addresses, names under `example.invalid` and, for another machine of a network, documentation
  addresses such as `192.0.2.10` (RFC 5737).
- Chinese pages are written in Chinese, not translated word for word, and keep the English names
  of tools, fields and products. Use standard written Chinese (「无法读取」「未设置」), not
  colloquial words.
- Every page but the two home pages opens with frontmatter holding one `description:` line, a
  sentence that says what the page helps with. VitePress writes it into the page's
  `<meta name="description">`, so search engines and link previews show it. Quote it when it
  contains `: `.
- When a release changes what a user sees, the page describes the new behaviour and then says in
  one sentence how earlier versions behave, naming the last version that behaves the old way:
  "godot-mcp 0.3.0 and earlier close a session after five minutes".
- A troubleshooting entry is headed by the symptom as the user sees it, and says what causes it
  and what to change.

## Links

Link other files by their real path in this repository, the way GitHub resolves it. A page at the
top of `docs/site/` writes `[Configuration](../operator/configuration.md)` and
`[Install](guide/install.md)`; a page in a subdirectory adds one `../` per level, and a page under
`zh/` one more. The sync turns a link to a published reference into a link to its page on the
site, and any other link that leaves the published files into a GitHub link.

Do not write site paths such as `/guide/install` in Markdown links: GitHub cannot follow them. The
home page's frontmatter is the exception, because it is data for the theme, not Markdown.

## Components

Pages are GitHub Markdown, plus the components of the site's theme. They render on the site only;
GitHub drops a tag it does not know, so a page shows nothing in its place there.

| Component | Where |
| --- | --- |
| `HomeBuilt`, `HomeCapabilities`, `HomeShowcases`, `HomeClients`, `SplitBlock`, `HomeEngine`, `HomePrivacy`, `HomeFaq`, `HomeCloser` | The two home pages, in this order. `SplitBlock proof="profiles"` holds the page's own Markdown beside the profile table; `HomeEngine` holds the install commands. |
| `ScreenFigure` | Any page: one screenshot from `public/screens/`, with `src`, an optional `dark` for the dark theme, `width`, `height`, `alt` and an optional `caption`. |
| `HomeIndex`, `HomeSteps`, `HomePlatforms`, `HomeScope`, `StatusTag`, VitePress's `Badge` | The previous home page layout; still registered so an older checkout builds. |

## The home page

`index.md` and `zh/index.md` carry their content in the `home:` frontmatter, which the theme lays
out, and set `markdownStyles: false`. The build fails when a field is missing, misspelt or of the
wrong shape (`theme/data/home-schema.ts` in FirLab).

| Field | Holds |
| --- | --- |
| `facts` | Two to four `term` and `text` rows under the hero's buttons, each one short line: in a 1280 × 800 window three one-line facts start the demo under the hero at 558 px, where three of two or three lines started it at 674 px. |
| `stages` | Under the hero: two `steps` (the titles of the two halves), an `exchange` (a `label` for screen readers and the `transcript` lines: `call`, `argument` for a call's further arguments, `answer`), the `screen` the exchange answered and a `caption`. |
| `built` | How godot-mcp is built: the `client`, the `server` with its `parts`, the `targets` it reaches (each with `name`, `detail`, `status` and an optional `link`) and a `note`. |
| `capabilities` | At least six `items`, each with an `icon`, `title`, `status` (`no-engine` or `engine`), `body`, one piece of `evidence`, a `link` and an optional `wide: true` for a cell two columns wide. Evidence is `code` (a `text`, one line of an answer per line), or `chips` (`items`, tool names). The icons are those of the theme's `data/icons.ts`. |
| `showcases` | At least two, each with `title`, `intro`, a `screen` and its optional `caption`, two to four `points`, and optionally an `exchange` (as in `stages`) and a `guide` line. |
| `clients` | `items` with `name`, `link`, an optional `mark` (`cursor`, `codex`, `opencode` or `pi`) and either the `command` that registers godot-mcp or, for a client without one, a `note` on where its configuration goes; then a `more` line for the clients the band does not list, and an optional `guide` line. |
| `profiles` | The table beside the profiles split: `columns`, `rows` of `cells`, `code` (the indexes of the columns shown as code) and an optional `caption`. |
| `engine` | The locked Godot builds as `versions` (`version`, `detail`), the `platforms` (`name`, `archive`, `detail`), an `install` line under the commands and a `note`. |
| `privacy` | Where each kind of traffic goes: `sendsLabel` and `modes` with `name`, `sends` and `detail`. |
| `faq` | At least four `items`, each a question `q` and its answer `a`. |
| `closer` | The last band: `title`, `body` and `actions` (`text`, `link`, `theme`: `brand` or `alt`, and an optional `icon`). |

A `screen` is `src`, an optional `dark`, `width`, `height` and `alt`; each path is
`/screens/<name>.webp`, a file in `public/screens/`.

Prose fields may mark code with backticks, strong words with double asterisks, and a link to an
`https://` address in Markdown's own form. A link to a page of the site goes in a `link` or a
`guide` field (`text` and `link`), where the build can check it. Quote a value that starts with a
backtick or contains `: `. In a YAML flow list such as `cells: [a, b, c]` a comma separates cells, so quote a
cell that contains one. A code cell after the first column wraps between its words, so a list of
tool names is written with spaces between them and no commas.

The exchanges are a real session against the demo project in `examples/site-demo/`: each `call`
line is a tool call a client sent, and each `answer` line shows fields of the structured answer
godot-mcp returned, with every value copied unchanged. The capture script below records them.

## Screenshots

The pictures in `public/screens/` are taken from the demo project by
`scripts/site/capture_screens.py`, through the server built from this tree:

| Picture | Shows |
| --- | --- |
| `editor-en-light`, `editor-en-dark`, `editor-zh-light`, `editor-zh-dark` | The editor with the demo project, in English and Chinese, with the Light and the Default theme, and the Godot MCP panel on its Activity page |
| `game` | The game after `input.sequence.send` walked the player to two coins |
| `game-scenario` | The screenshot the last step of a `test.scenario.run` scenario kept |

It needs a virtual display, the MCP Python SDK version the script names and the newest Godot
editor in `tests/fixtures/godot-releases.lock.json`; it checks all three before it starts:

```sh
cargo build --release --locked -p godot-mcp-cli --bin godot-mcp
cargo build --release --locked -p godot-mcp-engine --bin godot-mcp-supervisor
xvfb-run -a -s "-screen 0 1600x1000x24" python3 -I -B scripts/site/capture_screens.py \
  --binary target/release/godot-mcp --godot <the Godot editor binary> \
  --out docs/site/public/screens
```

Every answer is checked against the shape the server documents and every picture against its
answer; a failed check stops the run and names the picture. The answers go to
`target/site-captures/report.json`, where the home page's exchanges are copied from. Look at every
picture afterwards, one at a time, before you commit it. The demo's art and how it was made are in
`examples/site-demo/art/README.md`.

## Preview

With a FirLab checkout next to this repository:

```sh
cd ../firlab/godot-mcp
pnpm install --frozen-lockfile
./scripts/sync-godot-mcp-docs.sh ../../godot-mcp
pnpm dev    # http://localhost:5173/godot-mcp/
```

`pnpm build` followed by `./scripts/check-dist.sh dist` runs the same checks as the deployment.

## Publishing

Two workflows connect this repository to the site:

- `.github/workflows/docs-site.yml` runs on pull requests that touch the pages or the published
  references. It syncs them into a checkout of FirLab's `main`, builds the site and runs
  `check-dist.sh`, without a secret. It is advisory and not part of `CI Success`.
- `.github/workflows/publish-site.yml` runs after a merge to `main` that touches them. It runs the
  same sync and, when the site changes, commits the result as
  `docs(godot-mcp): sync from godot-mcp@<sha>` to the branch `godot-mcp-docs/sync` in FirLab and
  opens a pull request from it, or updates the one already open. Once FirLab's checks pass, it
  squash-merges the pull request at the commit that was checked, and FirLab's deploy publishes the
  site. A failed check leaves the pull request open; the next run replaces its commit.

`publish-site.yml` needs the repository secret `FIRLAB_DOCS_TOKEN` only when the site changes: a
fine-grained personal access token for `sunerpy/firlab` only, with Contents and Pull requests read
and write and nothing else. GitHub has no API that creates one, so it is made by hand and stored
with:

```sh
gh secret set FIRLAB_DOCS_TOKEN --repo sunerpy/godot-mcp
```

Without it, a run whose sync changes the site fails and says so, and nothing reaches FirLab until
someone opens a sync pull request there by hand. A run whose sync changes nothing passes without
it.
