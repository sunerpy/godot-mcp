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
| `docs/operator/managed-sessions.md`, `exports.md`, `editor-bridge.md`, `artifacts.md`, `process-boundaries.md`, `recovery.md`, `http-trusted-local.md`, `http-proxied-oidc.md` | `/godot-mcp/reference/<name>`, English only |
| `docs/security/threat-model.md` | `/godot-mcp/reference/threat-model`, English only |
| `docs/operator/usage-and-verification.md` | `/godot-mcp/zh/reference/usage`, Chinese only |
| `docs/site/public/godot-mcp-logo.svg` | The site's logo and favicon |

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
  addresses and names under `example.invalid`.
- Chinese pages are written in Chinese, not translated word for word, and keep the English names
  of tools, fields and products.

## Links

Link other files by their real path in this repository, the way GitHub resolves it. A page at the
top of `docs/site/` writes `[Configuration](../operator/configuration.md)` and
`[Install](guide/install.md)`; a page in a subdirectory adds one `../` per level, and a page under
`zh/` one more. The sync turns a link to a published reference into a link to its page on the
site, and any other link that leaves the published files into a GitHub link.

Do not write site paths such as `/guide/install` in Markdown links: GitHub cannot follow them. The
home page's frontmatter is the exception, because it is data for the theme, not Markdown.

## Components

Pages are GitHub Markdown, plus these components of the site's theme: `HomeIndex`, `HomeSteps`,
`SplitBlock`, `HomePlatforms`, `HomePrivacy`, `HomeScope`, `StatusTag` and VitePress's `Badge`.
They render on the site only; on GitHub a page shows them as plain tags, so keep them to the two
home pages.

## The home page

`index.md` and `zh/index.md` carry their content in the `home:` frontmatter, which the theme lays
out. The build fails when a field is missing, misspelt or of the wrong shape
(`theme/data/home-schema.ts` in FirLab).

| Field | Holds |
| --- | --- |
| `facts` | Two to four `term` and `text` rows under the tagline. |
| `visual` | The exchange in the hero: a `label` for screen readers, the `transcript` lines (`call`, `argument` for a call's further arguments, `answer`) and an optional `caption`. |
| `index` | Groups of capabilities, each with `title`, `body`, `status` (`no-engine` or `engine`) and an optional `link`. |
| `steps` | Four steps, each with `title`, `body` and an optional `command`. |
| `profiles`, `sessions`, `versions` | The tables beside the three splits: `columns`, `rows` of `cells`, `code` (the indexes of the columns shown as code) and an optional `caption`. |
| `platforms` | The platform table: `columns`, and `rows` with `name` and one cell per remaining column. |
| `privacy` | Where each kind of traffic goes: `sendsLabel` and `modes` with `name`, `sends` and `detail`. |
| `scope` | What godot-mcp does not do, as `items`. |

In a YAML flow list such as `cells: [a, b, c]` a comma separates cells, so quote a cell that
contains one. A code cell after the first column wraps between its words, so a list of tool names
is written with spaces between them and no commas.

The exchange is a real session against a small project: each `call` line is a tool call a client
sent, and each `answer` line shows fields of the structured answer godot-mcp returned, with every
value copied unchanged. To record a new one, run the calls against the project the page names and
copy the values.

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
