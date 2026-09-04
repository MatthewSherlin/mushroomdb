# mushroom — Claude Code plugin

A live code graph of your repository, wired into Claude Code as an MCP server, a `/mushroom:mushroom` skill, and two hooks. Everything here is rendered by `scripts/render-plugin.sh` from `crates/cli/skills/mushroom/SKILL.md` and the templates in `scripts/plugin-templates/` — do not hand-edit the files under this directory or `.claude-plugin/marketplace.json`; re-run the script instead.

## Install

```
claude marketplace add MatthewSherlin/mushroomdb
claude plugin install mushroom@mushroomdb
```

Then open a repository and type `/mushroom:mushroom`. On the first turn the skill builds the graph if it does not exist yet — one `ingest-git` pass over the git history and the working tree — and prints `map`: the repository's file clusters, most-depended-on files, owners, recently-hot files, and three questions worth asking next. About 2.5 s on a 431-file repository.

After that it is task-first: `impact` before an edit, `context` on a file or symbol, `owners`, `why` with the commits that prove a link, `recall` and `remember` for durable notes. What the graph guarantees and what it does not: [`docs/site/code-graph.md`](../../docs/site/code-graph.md).

(`mushroomdb install` — the npx path, not this plugin — writes the same skill into a project's or user's own `.claude/skills/mushroom/`, where Claude Code invokes it bare as `/mushroom`. The plugin and the npx install are two separate ways to get the same skill; a plugin-provided skill is always namespaced by Claude Code as `/<plugin-name>:<skill-name>`, which for this plugin is `/mushroom:mushroom`.)

## What it wires up

- **MCP server** (`.mcp.json`) — runs `npx -y mushroomdb@<version> mcp --auto`, one process per project, talking to the graph over stdio.
- **Skill** (`skills/mushroom/SKILL.md`, invoked as `/mushroom:mushroom`).
- **`UserPromptSubmit` hook** — runs `npx -y mushroomdb@<version> recall --auto` (5 s timeout) before each turn, printing a recall digest of related graph facts as context.
- **`PostToolUse` hook** (matcher `Edit|Write|MultiEdit`) — runs `npx -y mushroomdb@<version> touch --auto` (30 s timeout, async) after an edit, so the graph re-extracts the changed file without blocking the turn.

The plugin writes **no git hooks** — a plugin has no business editing `.git/hooks`. If you want a commit, checkout or merge to sync the graph, run `mushroomdb install --project` alongside it, or add the hooks yourself.

## `--auto` store location

`--auto` resolves the database as `$CLAUDE_PROJECT_DIR/mushroom-memory` (the environment variable Claude Code sets for plugin MCP servers and hook processes), falling back to `./mushroom-memory` when it is unset. Nothing is written outside the project directory, and the store directory is added to the repository's `.gitignore` on first `ingest-git`.

Several processes can share that store safely: the MCP server, both hooks and any `mushroomdb` command coordinate through one advisory `LOCK` file, and a writer that cannot get it retries on the next event rather than failing the turn. See [`docs/site/concurrency.md`](../../docs/site/concurrency.md).

## Troubleshooting

`mushroomdb doctor` is **not** the tool for a plugin-only install. It reads the files `mushroomdb install` writes — `.mcp.json`, `.claude/settings.json`, `.git/hooks` — and Claude Code holds the plugin's configuration itself, so `doctor` reports `fail config` and exits 1 even when the plugin is working. Use `claude plugin list` for the plugin, and check the store directly:

```
npx -y mushroomdb@<version> stats ./mushroom-memory   # node, edge and rule counts
npx -y mushroomdb@<version> map ./mushroom-memory     # the digest the skill prints
```

`doctor` becomes useful once you also run `mushroomdb install --project`, which is the same command that adds the git hooks.

## Local development

```
bash scripts/render-plugin.sh          # re-render after editing a template or bumping the version
bash scripts/render-plugin.sh --check  # fail if the committed files are stale
claude plugin validate packaging/plugin --strict
```
