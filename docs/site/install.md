# Install

Pick the row for what you want to do. The paths are not interchangeable: the last column is what
each one leaves out.

| You want to… | Run | You get | You do not get |
|---|---|---|---|
| Give Claude Code a memory | `claude plugin marketplace add MatthewSherlin/mushroomdb` then `claude plugin install mushroom@mushroomdb` | The `/mushroom:mushroom` skill, the MCP server and its 23 tools, the session and prompt hooks | A store path of your choosing: it uses `$CLAUDE_PROJECT_DIR/mushroom-memory`, the project Claude Code is open in |
| The same in Cursor or Codex, or with a store you name | `npx mushroomdb install --db ./memory` | The `/mushroom` skill, the MCP entry, the hooks; `--platform claude-code\|cursor\|codex\|all` | Anything installed globally: the entry runs `npx` |
| See the graph in a browser | `npx mushroomdb demo ./db && npx mushroomdb serve ./db`, or `docker run --rm -p 8080:8080 -e MUSHROOMDB_TOKEN=changeme ghcr.io/matthewsherlin/mushroomdb` | The explorer UI and the HTTP API at `:8080` | An assistant wired to it: that is one of the two rows above |
| Use it from a shell only, with no MCP server | `npx mushroomdb install --delivery cli` | The skill, the hooks, and three commands: `why` (why two keys are related), `asof --at <date>` (a Cypher read at a past date), `query` (any Cypher; `--role <name>` answers as one role) | Three task tools as one call — no `edges_at`, `what_if` or `node_edges` — and no `remember`: a fact is a `CREATE` |
| Embed it in a Rust program | `cargo add mushroomdb` | The engine as a library | The CLI, the MCP server, the UI |
| Embed it in a Python program | `pip install mushroomdb` | The engine as a module: rules, Cypher, vectors, history, scoped handles, the graph algorithms, full-text, and the memory calls (`remember`, `recall`, `upsert_entity`, `forget`) as data | The MCP server and the UI: it is a library, not a tool surface |
| Build the CLI from crates.io | `cargo install mushroomdb-cli` | The `mushroomdb` binary | **The explorer UI.** This build has none, and `serve` answers the API only without saying so. Use the `npx` or Docker row to see the graph |

Docker details, `install.sh`, and building the binary with the UI embedded are in
[CONTRIBUTING.md](https://github.com/MatthewSherlin/mushroomdb/blob/main/CONTRIBUTING.md).

## Platforms

A release carries a native binary for each of these, and `npx mushroomdb` fetches the one for the
machine it runs on:

| System | Built for |
|---|---|
| macOS | Apple Silicon, Intel |
| Linux | x86_64 with glibc, x86_64 with musl (Alpine), arm64 with glibc |
| Windows | x86_64 |

The Intel macOS, musl and Windows builds are new after 0.7.2. Each is built on every pull
request and run through a demo store, a query, three snapshots and a backup; the test suite
itself runs on Linux and macOS only. `install.sh` covers macOS and Linux. The Homebrew formula
is Apple Silicon and Linux. `pip install mushroomdb` has wheels for Apple Silicon and the two
glibc Linux targets; elsewhere it builds from source and needs a Rust toolchain. On anything
else, `cargo install mushroomdb-cli`.

**On Windows the binary is what is supported.** `mushroomdb.exe` and `npx mushroomdb` are run
on every pull request through a store's commands and one MCP exchange; `serve` is built and
not run there. `mushroomdb install`, the step below that writes an
assistant's MCP entry and hooks, has not been run there: the hooks are shell scripts and need a
POSIX shell, and the search for `node` and `npx` does not look for `.exe` or `.cmd`. Write the
MCP entry by hand (`npx -y mushroomdb mcp <store>`; some Windows clients need `cmd /c` in
front of `npx`), or use WSL, where the Linux build applies.
Keep the store on a local NTFS disk: a snapshot replaces a file the process has mapped, which
NTFS allows and exFAT, FAT and some network and container mounts do not.

`install` writes an MCP entry that runs `npx -y mushroomdb@<version>`, so the assistant needs
nothing installed globally and nothing is copied into your home directory. Point it at a local
build with `--command <path>`. `mushroomdb doctor` verifies the result end to end — config entry,
store, lock, hooks, and a real stdio handshake with the configured command.

To see the bundled explorer, write a demo graph and serve it. These lines assume a `mushroomdb`
binary on your `PATH` that embeds the UI — a release binary, or the one `install.sh` fetches. The
plugin and `npx` rows put nothing on `PATH`: there, prefix each line with `npx`.

```sh
mushroomdb demo ./db
mushroomdb serve ./db
```

Open `http://127.0.0.1:8080/`. The demo graph has 10 Orgs, 20 Projects, 30 People, and 334
edges — all 334 derived by seven rule sets. When a token is configured, open
`http://host:8080/?token=…`.
