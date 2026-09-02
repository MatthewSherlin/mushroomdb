# mushroomdb install — Claude Code and Cursor setup

`mushroomdb install` is the one-command front door: it writes the `/mushroom`
skill (Claude Code) or rules file (Cursor) and registers the MCP server so
your assistant can query and update a live graph immediately.

> **Alpha.** Local only. No data leaves your machine.

---

## Quick start

```
mushroomdb install --platform claude-code --project
```

Then open Claude Code in the same directory and type `/mushroom`.

---

## Flags

| Flag | Description |
|------|-------------|
| `--platform claude-code\|cursor\|all` | Target platform. Default: auto-detect (reads `~/.claude` / `.cursor/` presence). |
| `--project` | Project scope (writes to `./`). Omit for user scope (`~/`). |
| `--db <path>` | Database directory. Default: `./mushroom-memory` (project) or `~/.mushroomdb/memory` (user). |

---

## How the MCP server is invoked

Your assistant spawns the MCP server by the `command` in the config entry, so
that command must resolve from the assistant's process, not just your shell.
`install` picks the form that will actually work:

| Situation | `command` written | Why |
|-----------|-------------------|-----|
| `mushroomdb` is on `PATH` (`cargo install`, Homebrew, `npm i -g`) | `mushroomdb` | Bare name follows upgrades automatically. |
| Not on `PATH` (`npx mushroomdb install`, a local `target/release` build, a one-off download) | `~/.mushroomdb/bin/mushroomdb` (absolute) | `install` copies the running binary there first, so the path always exists. |

The same command is substituted into the skill's bootstrap lines
(`demo`, `snapshot`, `--help`), so the assistant can seed the store without
a `PATH` lookup either. The copy is tracked in the manifest and removed by
`uninstall`. Re-running `install` from a newer binary refreshes the copy.

---

## What gets written

### Claude Code — project scope (`--project`)

| File | Purpose |
|------|---------|
| `.claude/skills/mushroom/SKILL.md` | The `/mushroom` skill, with `{{DB_PATH}}` and `{{BIN}}` substituted for your db path and the resolved command. |
| `.mcp.json` | `mcpServers.mushroomdb` entry: `{"command":"<see above>","args":["mcp","<db>"]}`. Created if absent; merged if present. |
| `.claude/skills/mushroom/.install-manifest.json` | Manifest of everything written — consumed by `uninstall`. |
| `~/.mushroomdb/bin/mushroomdb` | Only when the binary is not on `PATH`: a copy of the running binary. |

### Claude Code — user scope (no `--project`)

Same as above but paths are:

| File | Location |
|------|---------|
| Skill | `~/.claude/skills/mushroom/SKILL.md` |
| MCP config | `~/.claude.json` (top-level `mcpServers` key — same structure as project `.mcp.json`) |
| Manifest | `~/.mushroomdb/install-manifest.json` |

**Verified 2026-09-02 by live inspection:** `~/.claude.json` holds the
top-level `mcpServers` key for Claude Code user-level MCP servers.
`~/.claude/settings.json` holds env/permissions/hooks but no `mcpServers`.

### Cursor — project scope (`--project`)

| File | Purpose |
|------|---------|
| `.cursor/rules/mushroom.mdc` | Always-apply rules file (`alwaysApply: true` frontmatter). |
| `.cursor/mcp.json` | `mcpServers.mushroomdb` entry (same merge logic as Claude Code). |

### Cursor — user scope (no `--project`)

| File | Location |
|------|---------|
| Rules | `~/.cursor/rules/mushroom.mdc` |
| MCP config | `~/.cursor/mcp.json` |
| Manifest | `~/.mushroomdb/install-manifest.json` |

---

## Auto-detection

When `--platform` is omitted, `install` detects which assistant is present:

- `~/.claude` exists **or** `.claude/` in project root → Claude Code
- `.cursor/` in project root **or** `~/.cursor` exists → Cursor
- Both → writes both platforms (`--platform all`)
- Neither → prints guidance and exits non-zero

---

## Idempotent re-install

Running `mushroomdb install` twice is safe. The second run is a no-op: same
files, same MCP entries, exit 0. Change the `--db` path to a different
location → run `uninstall` first, then re-install.

---

## Conflict handling

If `.mcp.json` (or `~/.claude.json`) already has a `mushroomdb`
entry pointing to a **different** database path, `install` exits non-zero,
prints manual-merge instructions, and makes **no changes**.

An entry for the **same** database path with a different `command` is not a
conflict: `install` rewrites the command in place. This repairs an entry whose
bare `mushroomdb` never resolved, and refreshes a stale absolute path after an
upgrade.

---

## Uninstall

```
mushroomdb uninstall --platform claude-code --project
```

Reads the manifest and removes exactly what `install` wrote. User files in the
same directories are left untouched.

---

## Troubleshooting

**`cannot auto-detect platform`** — no `~/.claude` or `.cursor/` found. Pass
`--platform` explicitly.

**`conflict: .mcp.json already has mcpServers.mushroomdb`** — the entry
already points to a different db path. Run `mushroomdb uninstall` to remove
the old entry, then `install` again with the new path.

**Skill not showing in Claude Code** — restart Claude Code after install, or
run `claude skills reload` (Claude Code ≥ 1.5).

**MCP server not connecting** — check the `command` in the config entry
resolves: run `<command> --help` in a terminal. If the entry still says bare
`mushroomdb` and `which mushroomdb` finds nothing, you installed with a build
older than 0.4.5; re-run `install` and it will copy the binary to
`~/.mushroomdb/bin/` and rewrite the entry. Restart the assistant afterwards —
MCP servers are only spawned at startup.
