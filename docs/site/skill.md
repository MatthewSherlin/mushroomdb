# mushroomdb install — Claude Code, Cursor and Codex setup

`mushroomdb install` is the one-command front door: it writes the `/mushroom`
skill (Claude Code) or rules file (Cursor), registers the MCP server, and wires
the prompt and git hooks that keep the graph current, so your assistant can
query and update a live graph immediately.

> **Alpha.** Local only. No data leaves your machine.

---

## Quick start

```
mushroomdb install --platform claude-code --project
```

Then open Claude Code in the same directory and type `/mushroom`. Inside a
git repository, the skill's bootstrap prefers `ingest-git` over the demo
graph, so the first run seeds the store from the repo's own authors, commits,
and files instead — see [`docs/site/ingest-git.md`](ingest-git.md).

---

## Flags

| Flag | Description |
|------|-------------|
| `--platform claude-code\|cursor\|codex\|all` | Target platform. Default: auto-detect (reads `~/.claude` / `.cursor/` presence). `all` is Claude Code and Cursor; Codex is opt-in, because registering with it runs another program. |
| `--project` / `--user` | Scope. Default: auto — project inside a git checkout, user anywhere else. |
| `--db <path>` | Database directory. Default: `./mushroom-memory` (project) or `~/.mushroomdb/memory` (user). |
| `--command <path>` | Invoke this binary instead of `npx`. Use it for a local build or a pinned install. A relative path is fine to type: it is anchored to the current directory before anything is written, because the assistant spawns the server from a directory of its own. `--db` is anchored the same way. |
| `--no-git-hooks` | Skip the `post-commit` / `post-checkout` / `post-merge` sync hooks. |
| `--no-prewarm` | Skip the one-off `npx -y mushroomdb@<version> --version` fetch. |

---

## How the MCP server is invoked

Your assistant spawns the MCP server by the `command` in the config entry, so
that command must resolve from the assistant's process, not just your shell.
`install` picks the form that will actually work:

| Situation | `command` / `args` written | Why |
|-----------|----------------------------|-----|
| Default | `npx` with `["-y","mushroomdb@<version>","mcp","<db>"]` | Resolves on any machine with Node, needs nothing installed globally, and the version is pinned to the binary that wrote the entry. |
| The `mushroomdb` on `PATH` is this binary (`cargo install`, Homebrew — a symlink to it counts) | `mushroomdb` with `["mcp","<db>"]` | Bare name follows upgrades automatically. |
| `--command <path>` | that path, with `["mcp","<db>"]` | You said which binary; nothing is guessed. |

The bare name is written only when the test passes on identity, not name:
`install` canonicalizes the `mushroomdb` that `PATH` resolves to and the
executable it is running, and uses the bare name only when they are the same
file. Anything else — npm's Node shim, a different build, no hit at all — pins
the published package instead. Nothing is ever copied into your home directory.

The same command is substituted into the skill's bootstrap lines, shell-quoted
where a shell will read it, so `ingest-git` and `serve` can be pasted straight
out of the skill.

If `npx` is on `PATH`, `install` runs `npx -y mushroomdb@<version> --version`
once (up to 180 s) so the assistant's first spawn is not a cold download. It is
best effort: a failure prints a warning and the install still succeeds. Skip it
with `--no-prewarm`.

---

## What gets written

### Claude Code — project scope (`--project`)

| File | Purpose |
|------|---------|
| `.claude/skills/mushroom/SKILL.md` | The `/mushroom` skill, with `{{DB_PATH}}` and `{{BIN}}` substituted for your db path and the resolved command. |
| `.mcp.json` | `mcpServers.mushroomdb` entry (see above). Created if absent; merged if present. |
| `.claude/skills/mushroom/.install-manifest.json` | Manifest of everything written — consumed by `uninstall`. |
| `.claude/settings.json` | Two hook entries. `hooks.UserPromptSubmit` runs `<bin> recall <db>` (5 s timeout) so related facts are injected before each prompt; `hooks.PostToolUse`, matched to `Edit|Write|MultiEdit`, runs `<bin> touch <db>` (30 s, `async`) so an edited file reaches the graph without the tool call waiting. Hooks load at session start: restart Claude Code after install. |
| `.gitignore` | One line for the store directory, when the store is inside the repository. Removed on uninstall. |
| `.git/hooks/post-commit`, `post-checkout`, `post-merge` | A marked block running a backgrounded, silenced `<bin> sync <db>`, so the graph follows commits, branch switches and merges. Your own lines in those files are preserved, and only the marked block is removed on uninstall. Skip with `--no-git-hooks`. |

### Claude Code — user scope (`--user`)

Same as above, minus the two repository-level pieces (no `.gitignore` line and
no git hooks — a user-scope install belongs to no one repository), and the
paths are:

| File | Location |
|------|---------|
| Skill | `~/.claude/skills/mushroom/SKILL.md` |
| MCP config | `~/.claude.json` (top-level `mcpServers` key — same structure as project `.mcp.json`) |
| Manifest | `~/.mushroomdb/install-manifest.json` |
| Hooks | `~/.claude/settings.json` — the same `hooks.UserPromptSubmit` and `hooks.PostToolUse` entries as above. Hooks load at session start: restart Claude Code after install. |

**Verified 2026-09-02 by live inspection:** `~/.claude.json` holds the
top-level `mcpServers` key for Claude Code user-level MCP servers.
`~/.claude/settings.json` holds env/permissions/hooks — `install` merges a
`hooks.UserPromptSubmit` entry into it. Every other key and value is preserved,
but the file is re-serialized: `serde_json` is built without `preserve_order`,
so keys come back alphabetized and indented two spaces. Content is kept,
layout is not. `uninstall` skips the write entirely when there is nothing of
ours to remove, so a file we never touched stays byte-identical.

The prompt hook runs `<bin> recall <db>`, which opens the store without
migration or WAL repair (`auto_migrate: false`, `repair_wal: false`) — it fires
on every prompt and must not write to the store. It prints one of two things:
the topic digest for the prompt, or — when the payload's `cwd` is a checkout
with uncommitted changes — a nudge of at most eight lines naming what those
files reach that is not already in the diff. Both open with a line marking the
content as untrusted graph data, and control characters are stripped from every
rendered value: node keys and names are ingested content, and for an
`ingest-git` store any contributor to the repository controls them.

The `PostToolUse` hook runs `<bin> touch <db>` after an `Edit`, `Write` or
`MultiEdit`, which re-extracts that one file — symbols, imports, mentions and
its hash. It is declared `async` so the tool call does not wait on it, and it
prints nothing and exits 0 whatever it is handed. It is what keeps the prompt
hook's nudge describing the code as it is now rather than as it was at the last
commit.

Cursor gets no hook: its hook contract is undocumented, so the always-apply
rules file remains the only injection mechanism there.

### Cursor — project scope (`--project`)

| File | Purpose |
|------|---------|
| `.cursor/rules/mushroom.mdc` | Always-apply rules file (`alwaysApply: true` frontmatter). |
| `.cursor/mcp.json` | `mcpServers.mushroomdb` entry (same merge logic as Claude Code). |

### Cursor — user scope (`--user`)

| File | Location |
|------|---------|
| Rules | `~/.cursor/rules/mushroom.mdc` |
| MCP config | `~/.cursor/mcp.json` |
| Manifest | `~/.mushroomdb/install-manifest.json` |

### Codex (`--platform codex`)

Codex owns its own configuration, so `install` writes no file for it: it runs

```
codex mcp add mushroomdb -- <command> <args…>
```

and lets Codex record the server. If the `codex` CLI is not on `PATH`,
`install` says so and writes nothing. 0.6.0 ships no Codex skill — the MCP tool
descriptions are what Codex reads.

A Codex-only install writes nothing project-local: no ignore line and no git
hooks, since its manifest lives at
`~/.mushroomdb/install-manifest-codex.json` and removing it must not strip
those out from under a Claude Code or Cursor install sharing the repository.
Pair it with `--platform claude-code` (or run `mushroomdb ingest-git
--ensure-gitignore`) if you want them.

Undoing it needs the platform named — auto-detection never yields Codex:

```
mushroomdb uninstall --platform codex
```

---

## Auto-detection

When `--platform` is omitted, `install` detects which assistant is present:

- `~/.claude` exists **or** `.claude/` in project root → Claude Code
- `.cursor/` in project root **or** `~/.cursor` exists → Cursor
- Both → writes both platforms (`--platform all`)
- Neither → prints guidance and exits non-zero

Codex is never inferred; ask for it with `--platform codex`.

When neither `--project` nor `--user` is given, the scope is inferred too: a
`.git` in the working directory means project scope, anything else means user
scope. The chosen scope is printed in the summary.

An install in the *other* scope is reported, never edited. Two mushroomdb
servers both load, so `install` prints a warning naming the other file and the
`uninstall --user` / `uninstall --project` that removes it.

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

The database an existing entry serves is read as the argument straight after
`mcp`, wherever that falls in `args`, so entries written by any version compare
correctly.

An entry for the **same** database path with a different `command` is not a
conflict: `install` rewrites the command in place and prints `updated mcp
command`. This repairs an entry whose bare `mushroomdb` never resolved,
replaces the absolute path a 0.5.x install wrote, and re-pins an older version.

The two settings hooks are replaced the same way, not added beside the old
ones: any `UserPromptSubmit` or `PostToolUse` hook running `recall` or `touch`
against this same database is removed first, whatever binary it names, and
`install` prints `replaced stale <event> hook`. Without that, a 0.5.x upgrade
would leave the old pair running the copied binary alongside the new pair, and
every prompt would carry two recall digests.

---

## Uninstall

```
mushroomdb uninstall --platform claude-code --project
```

Reads the manifest and removes exactly what `install` wrote: the MCP entry, the
skill or rules file, both settings hooks, the `.gitignore` line, the marked
block in each git hook, and the Codex registration. User files in the same
directories, and user lines in the same files, are left untouched. A
`.gitignore` that exists only because `install` created it is deleted too, but
only when stripping our line leaves it empty — a line you have added since
keeps the file.

Scope is resolved the same way as for `install`, so a bare `uninstall` inside a
git checkout looks for the project manifest first. If there is none, it falls
back to the user manifest before reporting anything, and the summary names the
scope it used. That is the 0.5.x upgrade path: 0.5.x had no scope detection, so
its installs inside checkouts are user-scope.

`--platform codex` is required to undo a Codex install; auto-detection never
yields Codex.

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
resolves: run `<command> --help` in a terminal. If the entry names an absolute
path under `~/.mushroomdb/bin/` it was written by 0.5.x; re-run `install` and
it will re-pin the entry to `npx -y mushroomdb@<version>`. Restart the
assistant afterwards — MCP servers are only spawned at startup.

**`codex was not found on PATH`** — `--platform codex` registers the server by
running the Codex CLI. Install it, or drop the flag.
