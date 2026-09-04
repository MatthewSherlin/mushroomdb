# mushroom — Claude Code plugin

A live code graph of your repository, wired into Claude Code as an MCP server, a `/mushroom` skill, and two hooks. Everything here is rendered by `scripts/render-plugin.sh` from `crates/cli/skills/mushroom/SKILL.md` and the templates in `scripts/plugin-templates/` — do not hand-edit the files under this directory or `.claude-plugin/marketplace.json`; re-run the script instead.

## Install

```
claude marketplace add MatthewSherlin/mushroomdb
claude plugin install mushroom@mushroomdb
```

Then open a repository and type `/mushroom` (or `/mushroom:mushroom`). The skill builds the graph on first use if it does not exist yet.

## What it wires up

- **MCP server** (`.mcp.json`) — runs `npx -y mushroomdb@<version> mcp --auto`, one process per project, talking to the graph over stdio.
- **Skill** (`skills/mushroom/SKILL.md`, invoked as `/mushroom:mushroom`) and a **command shim** (`commands/mushroom.md`, invoked as `/mushroom`) that just calls the skill — Claude Code does not yet register a plugin skill as a user-facing slash command on its own, so the shim exists to make `/mushroom` work.
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
