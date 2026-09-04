# mushroom — Claude Code plugin

A live code graph of your repository, wired into Claude Code as an MCP server, a `/mushroom:mushroom` skill, and two hooks. Everything here is rendered by `scripts/render-plugin.sh` from `crates/cli/skills/mushroom/SKILL.md` and the templates in `scripts/plugin-templates/` — do not hand-edit the files under this directory or `.claude-plugin/marketplace.json`; re-run the script instead.

## Install

```
claude marketplace add MatthewSherlin/mushroomdb
claude plugin install mushroom@mushroomdb
```

Then open a repository and type `/mushroom:mushroom`. The skill builds the graph on first use if it does not exist yet.

(`mushroomdb install` — the npx path, not this plugin — writes the same skill into a project's or user's own `.claude/skills/mushroom/`, where Claude Code invokes it bare as `/mushroom`. The plugin and the npx install are two separate ways to get the same skill; a plugin-provided skill is always namespaced by Claude Code as `/<plugin-name>:<skill-name>`, which for this plugin is `/mushroom:mushroom`.)

## What it wires up

- **MCP server** (`.mcp.json`) — runs `npx -y mushroomdb@<version> mcp --auto`, one process per project, talking to the graph over stdio.
- **Skill** (`skills/mushroom/SKILL.md`, invoked as `/mushroom:mushroom`).
- **`UserPromptSubmit` hook** — runs `npx -y mushroomdb@<version> recall --auto` (5 s timeout) before each turn, printing a recall digest of related graph facts as context.
- **`PostToolUse` hook** (matcher `Edit|Write|MultiEdit`) — runs `npx -y mushroomdb@<version> touch --auto` (30 s timeout, async) after an edit, so the graph re-extracts the changed file without blocking the turn.

## `--auto` store location

`--auto` resolves the database as `$CLAUDE_PROJECT_DIR/mushroom-memory` (the environment variable Claude Code sets for plugin MCP servers and hook processes), falling back to `./mushroom-memory` when it is unset. Nothing is written outside the project directory, and the store directory is added to the repository's `.gitignore` on first `ingest-git`.

## Local development

```
bash scripts/render-plugin.sh          # re-render after editing a template or bumping the version
bash scripts/render-plugin.sh --check  # fail if the committed files are stale
claude plugin validate packaging/plugin --strict
```
