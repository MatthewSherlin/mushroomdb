# mushroom — Claude Code plugin

Graph memory for agents, wired into Claude Code as an MCP server, a `/mushroom:mushroom` skill, and three hooks: `SessionStart` (the session brief), `UserPromptSubmit` (recall) and `PostToolUse` (re-extract what you just edited). On an entity store the session gets the fifteen-tool association surface — query, explain, time travel, visibility — and a brief that prints the store's own schema with one worked call per question kind. Every file under this directory *except this README* is rendered by `scripts/render-plugin.sh` from `crates/cli/skills/mushroom/SKILL.md` and the templates in `scripts/plugin-templates/` — do not hand-edit those, or `.claude-plugin/marketplace.json`; re-run the script instead.

## Install

```
claude marketplace add MatthewSherlin/mushroomdb
claude plugin install mushroom@mushroomdb
```

Then open a repository and type `/mushroom:mushroom`. On the first turn the skill builds the store if it does not exist yet and prints a brief: the store's own schema, with one worked call per question kind — why are two entities related, what did the graph look like at a past commit, what would a change do, who may see it.

After that it is task-first: one named call per question kind — `explain_association`, `node_edges`, `edges_at`, `what_if`, `query` with a `role`, `remember` and `recall` — copied from the worked calls the brief printed, against the store's own keys. What the engine guarantees and what it does not: [`docs/site/mcp.md`](../../docs/site/mcp.md).

(`mushroomdb install` — the npx path, not this plugin — writes the same skill into a project's or user's own `.claude/skills/mushroom/`, where Claude Code invokes it bare as `/mushroom`. The plugin and the npx install are two separate ways to get the same skill; a plugin-provided skill is always namespaced by Claude Code as `/<plugin-name>:<skill-name>`, which for this plugin is `/mushroom:mushroom`.)

> **Deprecated in 0.6.4:** the code-graph door — the `explore`, `map`, `context`, `impact`, `owners`, `why` and `sync` tools, the three grep/edit hooks, and the plugin's coding-assistant positioning. It still works and is still tested; it is **removed in 0.7**. See [Deprecations](../../README.md#deprecations). The plugin itself is not going away.

## What it wires up

- **MCP server** (`.mcp.json`) — runs `npx -y mushroomdb@<version> mcp --auto`, one process per project, talking to the graph over stdio.
- **Skill** (`skills/mushroom/SKILL.md`, invoked as `/mushroom:mushroom`).
- **`SessionStart` hook** — runs `${CLAUDE_PLUGIN_ROOT}/hooks/run.sh brief --auto` (5 s timeout) as a session opens. On an entity store it prints the store's own schema in one block — the labels, the edge types and the rules that derive them — with one worked call per question kind against the store's own keys. On a store built by `ingest-git` it prints the repository summary instead (deprecated, removed in 0.7). Byte-stable for a given store, so Claude Code caches it for the whole session.
- **`UserPromptSubmit` hook** — runs `${CLAUDE_PLUGIN_ROOT}/hooks/run.sh recall --auto` (5 s timeout) before each turn, printing a recall digest of related graph facts as context.
- **`PostToolUse` hook** (matcher `Edit|Write|MultiEdit`) — runs `${CLAUDE_PLUGIN_ROOT}/hooks/run.sh touch --auto` (30 s timeout, async) after an edit, so the graph re-extracts the changed file without blocking the turn.

The plugin writes **no git hooks** — a plugin has no business editing `.git/hooks`. If you want a commit, checkout or merge to sync the graph, run `mushroomdb install --project` alongside it, or add the hooks yourself.

## `hooks/run.sh` — why the hooks do not call `npx`

`npx -y mushroomdb@<version> …` costs about half a second before it does any work: a cache check, a version resolve, and a Node process of its own. The MCP server pays that once per session, which is fine, and so does the `SessionStart` hook. The other two fire on **every prompt and every file edit**, which is not.

So the hooks call `run.sh` instead. On its first run it asks the package where its native binary is — `npx -y mushroomdb@<version> --print-binary` — writes that one line to

```
${CLAUDE_PLUGIN_DATA:-~/.mushroomdb}/binary-<version>
```

and then `exec`s it. Every later hook reads the cached line and skips straight to the `exec`. Measured warm, `--version` end to end:

| | |
|---|---:|
| `npx -y mushroomdb@<version>` | 514 ms |
| `node <launcher>` | 118 ms |
| the native binary | 7 ms |

The launcher is only a shim that starts a Node runtime and then spawns that same binary, which is why the binary is asked for first. `--print-launcher` is the second rung, cached separately as `launcher-<version>`, for a package whose binary was never fetched; `npx -y mushroomdb@<version> "$@"` is the last, slower and always correct.

The caches are keyed by version, so a plugin upgrade resolves afresh rather than running the old copy; the previous version's files are single lines and are simply left behind. Every cached path is re-checked before use, since npm's cache can be pruned out from under it.

The cache lives under `$CLAUDE_PLUGIN_DATA` or `$HOME/.mushroomdb` and nowhere else. A line in it is a path the script `exec`s with the hook's arguments and the prompt payload on stdin, so where it may be read from is a security question rather than a convenience one: both of those directories are already the user's own, whereas a world-writable fallback such as `/tmp` would let any local user drop in a path and have the next prompt run it. With neither variable set there is simply no cache, and the script resolves every time.

`run.sh` is rendered from `scripts/plugin-templates/run.sh.tmpl` (it embeds the version) and must stay executable; `scripts/render-plugin.sh --check` fails if it is stale or loses its executable bit.

## `--auto` store location

`--auto` resolves the database as `$CLAUDE_PROJECT_DIR/mushroom-memory` (the environment variable Claude Code sets for plugin MCP servers and hook processes), falling back to `mushroom-memory` at the root of the working tree the command ran in, and to `~/.mushroomdb/memory` outside a checkout. This plugin is Claude Code's, so the first step always answers; `mushroomdb install --platform cursor` or `--platform codex` writes the path instead, because those hosts set no such variable. That fallback finds a *working tree*, not the `.git` directory worktrees share, so each `git worktree add` gets its own graph rather than reading the checkout next door's. Nothing is written outside the project directory, and the store directory is added to the repository's `.gitignore` on first `ingest-git`.

Several processes can share that store safely: the MCP server, all three hooks and any `mushroomdb` command coordinate through one advisory `LOCK` file, and a writer that cannot get it retries on the next event rather than failing the turn. See [`docs/site/concurrency.md`](../../docs/site/concurrency.md).

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
