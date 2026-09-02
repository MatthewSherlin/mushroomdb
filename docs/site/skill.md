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

## What gets written

### Claude Code — project scope (`--project`)

| File | Purpose |
|------|---------|
| `.claude/skills/mushroom/SKILL.md` | The `/mushroom` skill, with `{{DB_PATH}}` substituted for your db path. |
| `.mcp.json` | `mcpServers.mushroomdb` entry: `{"command":"mushroomdb","args":["mcp","<db>"]}`. Created if absent; merged if present. |
| `.claude/skills/mushroom/.install-manifest.json` | Manifest of everything written — consumed by `uninstall`. |

### Claude Code — user scope (no `--project`)

Same as above but paths are:

| File | Location |
|------|---------|
| Skill | `~/.claude/skills/mushroom/SKILL.md` |
| MCP config | `~/.claude/settings.json` (`mcpServers` key — same format as project `.mcp.json`) |
| Manifest | `~/.mushroomdb/install-manifest.json` |

**Verified 2026-09-02:** `claude mcp add --help` confirms user-level MCP
servers are stored in `~/.claude/settings.json` under `mcpServers`.

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

If `.mcp.json` (or `~/.claude/settings.json`) already has a `mushroomdb`
entry pointing to a **different** database path, `install` exits non-zero,
prints manual-merge instructions, and makes **no changes**.

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

**MCP server not connecting** — verify `mushroomdb` is on your `PATH`:
`which mushroomdb`. If installed via npm: `npx mushroomdb mcp <db>` works
too; edit `.mcp.json` to use `"command":"npx","args":["mushroomdb","mcp","<db>"]`.
