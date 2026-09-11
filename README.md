<img src="docs/assets/mark-animated.svg" width="110" align="right" alt="" />

# mushroomdb

[![stars](https://img.shields.io/github/stars/MatthewSherlin/mushroomdb?style=flat&logo=github)](https://github.com/MatthewSherlin/mushroomdb/stargazers)
[![crates.io](https://img.shields.io/crates/v/mushroomdb-cli?logo=rust&label=crates.io)](https://crates.io/crates/mushroomdb-cli)
[![npm](https://img.shields.io/npm/v/mushroomdb?logo=npm&label=npm)](https://www.npmjs.com/package/mushroomdb)
[![PyPI](https://img.shields.io/pypi/v/mushroomdb?logo=python&logoColor=white&label=PyPI)](https://pypi.org/project/mushroomdb/)
[![CI](https://img.shields.io/github/actions/workflow/status/MatthewSherlin/mushroomdb/ci.yml?branch=main&label=CI)](https://github.com/MatthewSherlin/mushroomdb/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)

**The graph that stays true — and knows who's allowed to see it.**

mushroomdb is the data layer for agents that reason over entities. It is an embedded Rust graph
database in which a relationship is a schema declaration: write a rule once, and every write
derives, maintains and **retracts** the matching edges, each one carrying the rule, the score and
the values that produced it. An agent reaches it over MCP — fifteen tools on an entity store — or
you embed it as a Rust library, a Python module, or a sidecar beside your own service. Four
questions are what it exists for: **why are these two related** (`explain_association`, answered
with the evidence rather than an assertion), **what did that look like then** (`edges_at`, the edges
a node had at any past commit), **what would this change do** (`what_if`, computed without writing
anything), and **who may see it** (`query` with a `role`, so one graph answers differently per
caller). Local-first: a directory on disk, no account, no endpoint, no model call in the write path
unless you enable embeddings.

`ingest-git` stays supported as a **data source**: commits, pull requests, files and authors become
entities with rule-derived relationships, which is what makes a ticket↔commit link a rule rather
than a script.

> **Deprecated in 0.6.4:** the code-graph door — the `explore`, `map`, `context`, `impact`,
> `owners`, `why` and `sync` tools, the three grep/edit hooks, and the plugin's coding-assistant
> positioning. It still works and is still tested; it is **removed in 0.7**. See
> [Deprecations](#deprecations).

*Pre-1.0 alpha — APIs and formats may change between minor versions.*

[Docs](docs/site/index.md) · [Changelog](CHANGELOG.md) · [Issues](https://github.com/MatthewSherlin/mushroomdb/issues)

![A single SET changes one property; the rule fires, new scored edges are derived and the stale ones retracted, inside the same write](docs/assets/ingest-git-cascade.gif)

## Quick start

```sh
npx mushroomdb install --db ./memory
```

One command writes the `/mushroom` skill, an MCP server listing the fifteen-tool association
surface, and the session hooks. Then a worked flow, four tool calls:

```text
upsert_entity  →  create_rule  →  find_similar  →  explain_association
  (store)           (link)           (recall)          (explain)
```

Full tool reference: [`docs/site/mcp.md`](docs/site/mcp.md).

- **Live, not a snapshot.** One `SET` on a property re-derives the matching edges — added, scored
  and retracted — before the write closes. The `SET` in the GIF above is that one write.
- **Retracts instead of going stale.** A property drifting out of a rule's predicate doesn't leave
  a stale edge behind; the engine retracts it in the same write that caused the drift.
- **Explains any link.** `explain_association` names the rule, the score, and the values the two
  entities actually share, so *"why are these two related?"* has an answer your assistant can quote
  instead of a guess.
- **Knows who's allowed to see it.** Pass a `role` or a `mask` with a query and the same graph
  answers differently per caller; write statements are rejected on masked queries.
- **Answers what it said last week.** `edges_at` returns the edges a node had at a past commit;
  `mushroomdb asof ./db --commit 5 --query "…"` replays the WAL to that commit, derived edges
  included.

## Where it fits

**What it is**

- An embedded, single-binary graph database with a rule engine that maintains edges for you.
- A 27-tool MCP server — fifteen listed on an entity store, three on a store built by
  `ingest-git` — plus a `/mushroom` skill and a Claude Code plugin.
- Safe for several processes at once: one writer at a time behind an advisory `LOCK` file, any
  number of readers, and every handle picks up a peer's commits by `refresh()` rather than
  reopening — so a running `serve`, an editor hook, a git hook and a CLI command can share one
  store. [`docs/site/concurrency.md`](docs/site/concurrency.md)
- Local-first: your data stays on disk, no cloud service, no model call in the write path unless
  you enable embeddings.

**What it isn't**

- Not a hosted memory service — there is no account, no endpoint, nothing to sign up for.
- Not a vector database. Vector predicates and HNSW are built in; bring your own embeddings.
- Not a Postgres replacement. Single writer, no interactive transactions, memory-first storage.

---

## Deprecations

**What "deprecated" means here.** It still works in 0.6.4, it is still tested on every release, and
nothing is removed. It is no longer promoted — not on this page, not in the skill's task rules, not
in the plugin's description — its documentation page opens with a notice, and an `install` that
turns one of the hooks on prints a deprecation line. It is **removed in 0.7**. The migration is
**nothing to do**, unless you relied on the specific thing named below.

| Deprecated | If you relied on it |
|---|---|
| The tools `explore`, `map`, `context`, `impact`, `owners`, `why`, `sync` | Pin `mushroomdb@0.6.x`. Nothing on the entity surface replaces them: they answer from a repository graph, which 0.7 stops shipping tools for. `ingest-git` and `query` keep answering the same facts as Cypher. |
| The hooks `--intercept-grep`, `--impact-before-edit`, `--enrich-grep` | Re-run `install` without the flag; the hook comes out the way any other manifest entry does. Nothing replaces them. |
| The plugin's coding-assistant positioning | The plugin is not going away. Its description and skill now lead with entity memory. |
| Code-suite benchmark runs | `python3 benchmarks/agent-tasks/run.py --suite code` still runs. No further runs are committed; the committed summaries stay as the record. |

**Why, measured.** Across 240 cells — arms stock / installed / invoked / cli × 3 reps × 20 tasks over
two repositories — the invoked graph arm scored 0.924 against stock's 0.927 (paired difference
`-0.0035 [-0.0112, 0.0017]`) and cost $0.2713 against $0.2267 (`+0.04463 [+0.01312, +0.07471]`,
about +20%), and the two installed-but-not-invoked arms made **0 graph calls in 120 sessions**:
[`results/20260910T000418Z`](benchmarks/agent-tasks/results/20260910T000418Z/summary.md). An agent
holding `grep` neither needs a graph for those questions nor chooses one. What the engine is for is
measured separately, on the association suite, and reported under
[Benchmarks](#benchmarks).

---

## The differentiator

Most graph databases require you to create edges manually or run a batch similarity script after
each load. mushroomdb makes edge creation a schema declaration. A rule like "connect every Person
to every Org whose `skills` list overlaps theirs by at least 50%" is written once:

```rust
db.create_rule(RuleDef {
    name: "skill_fit".into(),
    src_label: "Person".into(),
    dst_label: "Org".into(),
    predicate: Predicate::Overlap { field: "skills".into(), min: 0.5 },
    edge_type: "FIT".into(),
    weight_prop: Some("score".into()),
    max_edges: Some(5), // keep the 5 best-matching Orgs per Person (top-k per source)
}).expect("rule");
```

After that, every `insert_node` and `set_prop` evaluates the rule incrementally. The engine writes
the edge, stores the Jaccard score, and retracts the edge if the properties later diverge — without
any manual work.

Watch it live — a Cypher `SET` changes one property, the `founded_within` rule fires, and new
scored edges appear in the bundled explorer:

![A SET statement deriving new scored edges live](docs/assets/demo-set-derives.gif)

Open the Rules panel, and the Why slide-over shows the exact predicate arithmetic behind every
derived edge:

![Rules panel and Why slide-over showing overlap arithmetic](docs/assets/rules-why.gif)

### Predicates

Six predicate kinds ship today. They compose via `All(...)` (AND, score = min) and `Any(...)`
(OR, score = max), nested up to depth 4.

| Predicate | What it tests |
|---|---|
| `KeyMatch` | FK equality — source field matches destination key |
| `FieldEqual` | Exact match on a named scalar field (string, int, float, bool) |
| `Overlap` | Jaccard on list-valued fields, min threshold |
| `NumericWithin` | Absolute numeric difference within a tolerance; score = `1 - |Δ|/tolerance` |
| `GeoRadius` | Haversine distance on `[lat, lon]` fields within km; score = `1 - dist/radius` |
| `VectorSimilar` | Cosine similarity on float arrays, min threshold |

Auto-FK: fields ending in `_id` whose values match existing node keys get `KeyMatch` rules created
automatically at ingest time. `VectorSimilar` accepts `approximate: true` to switch candidate
selection to in-tree HNSW (per-query recall min 0.90, mean 0.998 at 5k nodes / dim 1536,
fixed-seed probe). Full reference: [`docs/site/rules.md`](docs/site/rules.md).

### Built on the same engine

- **Live subscriptions.** `subscribe_rule` (Rust) and `GET /subscribe` (WebSocket) stream
  `EdgeFired` / `EdgeRetracted` the moment they hit the WAL — not polled, not batched. Bounded
  65,536-event queue; slow consumers get a `Lagged { missed: N }` marker instead of a disconnect.
  [`docs/site/subscriptions.md`](docs/site/subscriptions.md)
- **Rule attribution across time.** Every derived edge writes a HISTORY-MARKER WAL record carrying
  the rule name, so `edge_history`, `node_history`, and `was_linked` answer *which* rule created a
  link and *at which commit*. `GraphDb::open_at(&dir, 5)` replays to a past commit, derived edges
  included; out-of-range commits return `CommitOutOfRange`, never wrong data.
  [`docs/site/timetravel.md`](docs/site/timetravel.md)
- **Materialized views.** Degree counts and neighbor aggregates (sum/avg/min/max) maintained
  incrementally on every edge change — no cron, no triggers, no stale caches.
  [`docs/site/views.md`](docs/site/views.md)
- **Rule suggestions.** `db.suggest_rules()` (or `mushroomdb suggest ./db`) profiles your data and
  ranks candidate rules with estimated edge counts and rationale. Seeded sampling, so the same
  database always returns the same suggestions. No rule is ever applied automatically.
  [`docs/site/suggest.md`](docs/site/suggest.md)

---

## Agent memory

Graph structure captures the shape of real knowledge — entities, associations, similarity, and
lineage — and rule-derived edges keep those associations fresh as new facts arrive.

- Entities map to nodes (`Person`, `Document`, `Project`, `Concept`, …).
- Associations are edges derived from data: cosine similarity on embeddings, shared field values,
  FK relationships, geographic proximity. Declare a rule once; every write maintains the matching
  edges without agent-side bookkeeping.
- Recall has three modes: `find_similar` by query vector (HNSW when available, brute force
  otherwise); `find_similar` by key (neighbors along a rule-derived edge type); `query` for
  structured Cypher recall. `hybrid_search` fuses fulltext and vector results via Reciprocal Rank
  Fusion.
- Explanations are built in: `explain_association` shows which rules and scores produced each
  link, so an agent can cite evidence instead of asserting a conclusion.
- Node masks are the ACL primitive: pass `mask: [key1, key2, …]` to `query` to restrict the visible
  node set. Write statements are rejected on masked queries.
  [`docs/site/masks.md`](docs/site/masks.md)
- Schema-as-code: `mushroomdb schema apply <dir> <schema.json>` idempotently applies rules, views,
  and fulltext indexes, printing a created/updated/unchanged diff.

**Minimal workflow** (four tool calls):

```text
upsert_entity  →  create_rule  →  find_similar  →  explain_association
  (store)           (link)           (recall)          (explain)
```

**Fourteen task tools** answer a question in prose in one call. Seven answer on any store; seven
are the deprecated code door. They are what the skill reaches for, and what `tools/list` shows
first:

| Tool | Purpose |
|---|---|
| `explore` | **Deprecated (0.7).** One tool to find: `context`, `impact`, `history` or `all` for one target in one reply, capped by a token `budget` |
| `map` | **Deprecated (0.7).** The repository in one screen: size, last sync, clusters, key files, owners, hot files |
| `context` | **Deprecated (0.7).** One file or symbol from every side: where it is as `path:start-end`, signature, callers, callees, importers, co-change partners, commits, notes. `full` adds the body |
| `impact` | **Deprecated (0.7).** What changing these files reaches: partners with scores, importers, symbols other files call, owner. Defaults to the working tree's diff |
| `owners` | **Deprecated (0.7).** Top author and share, who else knows the file, last touch, the split by quarter |
| `why` | **Deprecated (0.7).** Every rule edge between two nodes with its evidence, or the shortest path when there is none |
| `explain_association` | Why two entities are associated: every rule-derived edge between them, with the rule, the score, the predicate it matched, and the values the two actually share |
| `node_edges` | Every edge on one node, grouped by edge type, with the rule and score behind each. `all_of: [types]` answers with the partners linked by every one of them, as keys; `edge_type`, `label`, `direction` and `limit` narrow it further |
| `neighborhood` | At depth 1 the same grouped listing; above 1 the breadth-first `(key, label, depth)` table |
| `edges_at` | The edges a node had at one 0-based WAL commit — the graph as it was, not as it is — with the same `all_of` / `edge_type` / `label` / `direction` filters |
| `what_if` | The derived edges a property change would lose and gain, computed without writing anything. `edge_type` prints both sides as partner keys |
| `recall` | One pointer per hit — `path:line symbol — first doc line` — for the identifiers in a topic |
| `remember` | Write a note into the graph and return its key |
| `sync` | **Deprecated (0.7).** Bring the store up to date: commits since the last sync, then the dirty working tree |

Each of the fourteen also takes `json: true`, which answers with the raw report instead of
the rendered digest.

**The thirteen graph tools** reach the store directly. Their descriptions are prefixed `Advanced:`
in `tools/list`, so an assistant knows which surface is the front door. The default listing follows
the store: a store built by `ingest-git` lists three tools in all — `explore`, `query` and `stats` —
and any other store lists fifteen, the association surface: `query` (with an optional `role`),
`explain_association`, `neighborhood`, `node_info`, `node_edges`, `was_linked`, `edges_at`,
`what_if`, `node_history`, `edge_history`, `find_similar`, `hybrid_search`, `remember`, `recall`
and `stats`. All 27 stay served either way — the listing decides what a session can call, not what
the server answers — and `mushroomdb mcp <db> --all-tools` lists the whole set:

| Tool | Purpose |
|---|---|
| `upsert_entity` | Insert or update a node by key (no existence check needed) |
| `ingest_json` | Batch-ingest nodes of one label from a JSON array |
| `create_rule` | Declare a derivation rule; backfills existing nodes immediately |
| `find_similar` | Find similar nodes by query vector (HNSW) or by derived edge traversal |
| `hybrid_search` | RRF over fulltext + vector results |
| `explain` | The rules and scores that link two nodes, as JSON — `explain_association` above answers the same question in prose |
| `query` | Cypher query (read or write); pass `mask` for an ACL-scoped read, or `role` to answer as one role from the store's `roles.json` |
| `node_info` | Return a node's key, label, and properties |
| `stats` | Live node, edge, and rule counts |
| `node_history` | WAL change history for a node (archives included; a `snapshot --truncate` ends the reach) |
| `edge_history` | Add/retract lifecycle for edges between two nodes, with rule attribution |
| `was_linked` | Point-in-time edge check: was an edge active at a given commit? |
| `rename_node` | Rename a node's key; old_key, new_key |

Full walkthrough, tool reference, and Claude Desktop setup: [`docs/site/mcp.md`](docs/site/mcp.md).
Skill, plugin, and hook details: [`docs/site/skill.md`](docs/site/skill.md).

---

## Install options

```sh
claude plugin install mushroom@mushroomdb   # after `claude marketplace add MatthewSherlin/mushroomdb`
npx mushroomdb install            # skill + MCP server + hooks, no toolchain needed
cargo install mushroomdb-cli      # `mushroomdb` binary from crates.io (no embedded UI)
cargo add mushroomdb              # embedded Rust library
pip install mushroomdb            # Python bindings
```

`install` writes an MCP entry that runs `npx -y mushroomdb@<version>`, so the assistant needs
nothing installed globally and nothing is copied into your home directory. Point it at a local
build with `--command <path>`. `mushroomdb doctor` verifies the result end to end — config entry,
store, lock, hooks, git hooks, and a real stdio handshake with the configured command.

To see the bundled explorer, write a demo graph and serve it:

```sh
mushroomdb demo ./db
mushroomdb serve ./db
```

Open `http://127.0.0.1:8080/`. The demo graph has 10 Orgs, 20 Projects, 30 People, and 334
edges — 304 of them derived by seven rule sets. When a token is configured, open
`http://host:8080/?token=…`. Building the binary with the UI embedded, Docker, and the
`install.sh` script are covered in [CONTRIBUTING.md](CONTRIBUTING.md).

**Role-bound tokens** limit a caller to a named subset of nodes. Define roles in `schema.json`
under the `roles` key (each role has a `label` selector list), then pass `--role-token TOKEN:ROLE`
(repeatable) when starting the server, or set `MUSHROOMDB_ROLE_TOKENS="tok1:role1,tok2:role2"`.
A role token receives only the nodes matching its label selectors — read endpoints return rows
filtered to the visible set; write, subscription, and analytics endpoints return 403. Unknown token
or role name: 401. The never-widen invariant is enforced in the server: a client-supplied mask is
always intersected with the role mask. The MCP interface (`mushroomdb mcp`) is a stdio JSON-RPC
server for local agent use and is not subject to bearer-token or role enforcement.

---

## CLI reference

| Command | What it does |
|---|---|
| `mushroomdb install [--platform claude-code\|cursor\|codex\|all] [--project\|--user] [--db <path>] [--command <path>] [--delivery cli\|mcp\|both] [--no-git-hooks] [--intercept-grep] [--impact-before-edit] [--enrich-grep] [--always-load\|--no-always-load] [--no-prewarm]` | Write the `/mushroom` skill + MCP server entry + the `SessionStart`, `UserPromptSubmit` and `PostToolUse` hooks + git hooks. Auto-detects platform and scope. `--delivery cli` writes no server entry: the skill teaches the binary instead. The three experimental hooks — grep redirect, blast radius before an edit, symbol facts after a search — are off by default; `alwaysLoad` on the server entry is on by default for an install that pins a store with `--db` |
| `mushroomdb uninstall [--platform …] [--project] [--db <path>]` | Remove exactly what `install` wrote (manifest-driven; leaves user files) |
| `mushroomdb disable [--platform …] [--project\|--user]` | Turn an install off without removing it: strips the MCP entry, the hooks and the git hook blocks. The skill, the store and `.gitignore` stay |
| `mushroomdb enable [--platform …] [--project\|--user]` | Turn a disabled install back on, re-resolving the command instead of replaying what `disable` removed |
| `mushroomdb doctor [--project\|--user] [--platform …]` | Verify an install: config entry, npx reachability, store, lock, hooks, git hooks, a real stdio handshake, and duplicate-scope servers. Exit 1 on any `fail` |
| `mushroomdb ingest-git <dir> <repo> [--exclude <pattern>]... [--prs] [--no-structure] [--no-docs] [--ensure-gitignore]` | Graph a git repository: `Author`, `Commit`, `File`, `Symbol` nodes plus `CO_CHANGED`, `KNOWS`, `IMPORTS`, `CALLS` and `MENTIONS` rules. Re-run to sync. See [`docs/site/ingest-git.md`](docs/site/ingest-git.md) |
| `mushroomdb brief <dir>\|--auto` | The repository's shape from the graph alone — counts, last sync, most central files, most called symbols — capped at 4,000 bytes and byte-stable between runs. Hook body for `SessionStart` |
| `mushroomdb explore <dir> <target> [--depth context\|impact\|history\|all] [--full]` | **Deprecated (0.7).** One tool to find: `context`, `impact` and `owners` composed behind one depth |
| `mushroomdb map <dir> [--json]` | **Deprecated (0.7).** The repository in one screen: clusters, key files, owners, hot files, and three questions worth asking |
| `mushroomdb context <dir> <target> [--full]` | **Deprecated (0.7).** One file or symbol from every side. `<target>` is a path, a symbol key, or a bare symbol name. The body is quoted only with `--full` |
| `mushroomdb impact <dir> <file>...` | **Deprecated (0.7).** What changing these files reaches: co-change partners, importers, and the symbols other files call |
| `mushroomdb owners <dir> <path>` | **Deprecated (0.7).** Top author and share, who else knows it, last touch, the last four quarters |
| `mushroomdb why <dir> <a> <b>` | **Deprecated (0.7).** Every rule edge between two nodes with its evidence, or the shortest path between them |
| `mushroomdb sync <dir>\|--auto [--json]` | **Deprecated (0.7).** Re-sync the repository the store was built from: new commits, then the working tree where it differs from `HEAD`. Takes no repo argument — reads it off the graph. `--json` prints the counts as one object. The git hooks `install` writes use `--auto`, so each worktree syncs its own store |
| `mushroomdb touch <dir>\|--auto [<file>...]` | Re-extract just these files. With no `<file>` reads them from a `PostToolUse` payload on stdin (hook body) |
| `mushroomdb recall <dir>\|--auto` | Hook body for the `/mushroom` skill's `UserPromptSubmit` recall hook: reads a prompt payload on stdin, prints one pointer per hit for the identifiers the prompt names, and nothing when it names none. Wired automatically by `install` |
| `mushroomdb intercept <dir>\|--auto` | Hook body for the optional `PreToolUse` grep redirect (`install --intercept-grep`): reads a `Grep` payload on stdin and exits 2 with a pointer at `explore` when the pattern is a symbol the graph holds |
| `mushroomdb mcp <dir>\|--auto` | Start a stdio MCP JSON-RPC server for agent tools |
| `mushroomdb demo <dir>` | Write a deterministic demo graph (10 Orgs, 20 Projects, 30 People) |
| `mushroomdb serve <dir>` | Start the HTTP server + optional UI (default `127.0.0.1:8080`; `--token` on non-loopback; `--role-token TOKEN:ROLE`) |
| `mushroomdb query <dir> <cypher>` | Run a Cypher read or write (`--query` also accepted) |
| `mushroomdb asof <dir> --commit N` | Read-only view at a WAL commit |
| `mushroomdb stats <dir>` | Print node/edge/rule counts |
| `mushroomdb suggest <dir>` | Rank candidate linking rules (scored top-k 32, KeyMatch 512) |
| `mushroomdb schema apply <dir> <schema.json>` | Idempotently apply a schema file (rules, views, fulltext indexes); prints a diff |
| `mushroomdb snapshot <dir> [--keep-wal\|--truncate] [--retention N]` | Write `snapshot.bin` and archive the WAL as `wal.<N>.archive`, so history reads still reach it. `--truncate` discards it; `--keep-wal` leaves `wal.bin` whole |
| `mushroomdb verify <dir>` | Audit snapshot integrity: CRC32 all 12 sections, exit 2 on any mismatch |
| `mushroomdb migrate <dir>` | Migrate an older store format in place |
| `mushroomdb backup <dir> <dest>` | Copy store files to `<dest>` and CRC-verify the copy. WARNING: unsafe against a running `serve` — use `POST /backup` for live-served stores |
| `mushroomdb export <dir> <dest> [--format jsonl\|parquet\|graphml]` | Export nodes, edges, and rules. JSONL is byte-identical across runs; Parquet is not across library versions. GraphML exports nodes and edges only, as a single `.graphml` file, for import into generic graph viewers and analysis tools |
| `mushroomdb algo pagerank\|wcc\|degree <dir> [--top N]` | PageRank, weakly-connected components, or degree centrality over manual + derived edges. `--weight-prop`/`--min-weight` weight or filter the edge set |
| `mushroomdb algo communities <dir> [--edge-type T]... [--weight-prop P] [--min-weight X] [--top N]` | Louvain communities with per-community cohesion and overall modularity |
| `mushroomdb --version` | Print the CLI's version and exit |

**Concurrency:** every CLI write command, the hooks, and a running `mushroomdb serve` coordinate
through one advisory `LOCK` file in the store directory, so they are safe to run against the same
store at the same time. A writer that cannot get the lock within two seconds exits 3 with
`another mushroomdb process is writing; retry`, having written nothing. Readers never take the
lock and never wait; `recall` opens read-only (`read_only: true`) so an unattended hook can never
delay a writer or fail because one is running. What the lock does *not* give you: cross-process
transactions, and subscription events for a peer's writes — a commit absorbed by `refresh()` is
visible on the next read but notifies nobody. Full model:
[`docs/site/concurrency.md`](docs/site/concurrency.md).

Full HTTP endpoint reference: [`docs/site/api.md`](docs/site/api.md).

---

## Known limitations

| Limitation | Detail |
|---|---|
| Memory-first | The in-memory store is RAM-bound. Design target is 10M nodes (~5–15 GB with properties). mmap-backed storage is deferred. |
| Single writer, no interactive transactions | One writer at a time, many readers — within a process via `RwLock`, across processes via the advisory `LOCK` file. `write_batch` commits all ops in one WAL frame (all-or-nothing on crash replay) but is **not isolated**: readers may observe intermediate states while a committed batch is applied in memory. Multi-statement `BEGIN`/`COMMIT` is not supported, and there are no cross-process transactions. |
| Peer writes do not notify subscribers | Commits another process made are picked up by `refresh()` and are there on the next read, but they fire no `EdgeFired`/`EdgeRetracted` event, so `/watch` and `/subscribe` see only writes made through this process. Poll if you need to react to a hook's writes. |
| Cold start without a snapshot re-fires all rules | Snapshots persist derived edges, ANN state, and view definitions. At 100k nodes / ~10M derived edges: **0.02 s** from a V8 snapshot vs **8.16 min** WAL-only (ANN re-fit dominates). Call `snapshot()` before close. See [`dogfood/results/scale-100k.md`](dogfood/results/scale-100k.md). |
| Two-hop Cypher joins at scale | Dense patterns producing >1,000,000 intermediate rows error without `LIMIT`. Add `LIMIT n` — the pull-based executor stops early and never materializes the full binding table. |
| Cypher write subset | CREATE, MATCH…SET, MATCH…DELETE, MATCH…DETACH DELETE, and MERGE (single-key, with `ON CREATE SET` / `ON MATCH SET`) are supported. Derived edges cannot be deleted manually. Variable-length paths are hard-capped at 10 hops; unbounded `*min..` is rejected at parse time. Full coverage table: [`docs/site/query.md`](docs/site/query.md). |
| Approximate vector mode is opt-in | `approximate: true` enables HNSW candidate selection. Per-query recall min 0.90, mean 0.998 at 5k / dim 1536 (fixed-seed probe). Review the trade-off before using it in completeness-critical workloads. |
| Demo refuses existing directories | `mushroomdb demo` exits 1 if the target directory is non-empty, including hidden files (`.DS_Store` counts). Use a fresh path. |
| Python bindings return dicts | pandas/polars zero-copy is not wired yet. HTTP `POST /query` defaults to Arrow IPC; JSON via `?format=json`. |

---

## Benchmarks

10,000-node graph (Apple M4 Pro, macOS 15.7.3, arm64), mushroomdb v0.1.1 release build, 2026-08-24.
Full methodology and honesty notes:
[`benchmarks/results/head-to-head-10k-v2.md`](benchmarks/results/head-to-head-10k-v2.md).

| Workload | mushroomdb | Neo4j | KùzuDB | Memgraph |
|---|---|---|---|---|
| Bulk ingest | 784 ms | 13.2 s | 1.21 min | 12.5 s |
| Neighborhood depth-1 (p50) | 0.4 µs | 1.22 ms | 99.6 µs | 1.34 ms |
| Neighborhood depth-1 (p95) | 2.2 µs | 1.46 ms | 519 µs | 2.14 ms |
| Neighborhood depth-2 (p50) | 0.2 µs | 7.18 ms | 1.08 ms | 9.22 ms |
| Cypher scan-filter-project (1.4k rows) | 1.22 ms | 93.7 ms | 3.95 ms | 83.7 ms |
| Cypher two-hop join (200 rows) | 261.6 µs ★ | 3.99 ms ★ | 1.59 ms ★ | 1.96 ms ★ |
| Cold-start: V8 snapshot open | 0.02 s ▽ | — | — | — |
| Cold-start: WAL-only open | 8.16 min ▽ | — | — | — |
| Server boot-to-ready | n/a (embedded) | 6.6 s | n/a (embedded) | 4.3 s |

**Honesty notes:**

- mushroomdb numbers are **embedded** — no network round-trip, no serialization overhead. KùzuDB
  is also embedded, so its numbers are directly comparable. Neo4j and Memgraph go over
  bolt/localhost (~0.1–1 ms round-trip per query).
- ★ Two-hop join: same dataset, same warmup policy, all four engines on **5,810,000
  INDUSTRY_ALIGNMENT edges**. Fresh process → ingest + preload → 3 discarded warmups → median of 10
  runs. mushroomdb derives the edges via `create_rule`; competitors were pre-loaded via UNWIND MERGE
  or COPY FROM CSV. All engines return 200 rows.
- ★ Earlier v2.1 two-hop values were **retracted** for cross-engine contamination; the v2
  mushroomdb 307 µs figure was **retired** (measured on a smaller 1M-edge graph). Both are
  documented in the methodology file rather than quietly dropped.
- ▽ 100k cold-start measured 2026-08-28, warm file cache, cold process, `/usr/bin/time -l`:
  V8 snapshot open 0.02 s at 31–41 MiB RSS; snapshot size 1.8 GiB; snapshot write ~35 s. Cold-cache
  was not measured. See [`dogfood/results/scale-100k.md`](dogfood/results/scale-100k.md).
- Rule engine vs hand-rolled maintenance (10k nodes, 1,000 specialty updates, drift = 0 for all
  three): per-op expert-written **64.93 min**, batched expert-written **24.98 s**, rule engine
  **17.58 s**. Both hand-rolled variants were written by the engine team with full knowledge of
  retraction semantics — drift = 0 is a property of that, not of hand-rolling in general.
  [`benchmarks/results/handrolled-vs-rules.md`](benchmarks/results/handrolled-vs-rules.md)

**Agent benchmarks** measure the entity engine directly. `benchmarks/agent-tasks/` runs real
`claude -p` sessions against executable truth. The association suite (`--suite association`) asks
twenty relationship questions of one generated world written three ways — as JSON files, as a
single relational file, and as a mushroomdb store. The graph arm moved from 0.795 at $0.5089
([`20260911T005749Z`](benchmarks/agent-tasks/results/20260911T005749Z/summary.md)) to 0.967 at
$0.0923 ([`20260911T065400Z`](benchmarks/agent-tasks/results/20260911T065400Z/summary.md)) against
the relational baseline's 0.974 at $0.1158 — a correctness tie at a lower mean cost, and still
behind files + grep (1.000 at $0.0949) on a 2,000-entity world; the gate failed on the cost
interval. The code suite (`--suite code`) is retired; its committed summaries stay as the record.
[`docs/site/association-bench.md`](docs/site/association-bench.md) describes the suite and how to
rebuild the world.

---

## Architecture

```text
graph-db/
├── crates/
│   ├── core-storage      # Packed adjacency topology + columnar property store + WAL + snapshots
│   ├── core-rules        # linking rules, per-rule indexes, incremental maintenance
│   ├── core-query        # pull-based interpreter; traversal ops + Cypher subset
│   ├── core-api          # the one public Rust interface; typed error enums
│   ├── code-extract      # tree-sitter symbol/import/call extraction; bytes in, facts out
│   ├── arrow-bridge      # results ↔ Arrow buffers
│   ├── server            # axum HTTP + WebSocket; serves UI
│   ├── cli               # mushroomdb binary
│   └── sim-harness       # DST: virtual clock, fault-injecting IO, seeded runner
├── ui/                   # TypeScript + Vite graph explorer
├── bindings/python/      # PyO3 / maturin
└── clients/typescript/   # HTTP + WebSocket client
```

Dependency rule (inward only):
`bindings/server/cli → core-api → {core-query, core-rules} → core-storage`

Storage uses a dense-id WAL with per-commit fsync (configurable via `FsyncPolicy`), plus mmap-able
V8 rkyv snapshots (12 sections: CSR topology, columnar properties, HNSW blobs, provenance, IVF
state, per-node last-change index, and more — zero-copy, no heap allocation on open). V5/V6/V7
stores are auto-migrated to V8 on `GraphDb::open`. Derived edges are not WAL-logged; they are
restored directly from the mmap'd sections. See [`docs/format-stability.md`](docs/format-stability.md)
for the format evolution contract.

---

## Roadmap

Phases 1–4 and Plan 18 all landed. What remains:

| Priority | Item |
|---|---|
| Medium | mmap snapshots; lock-free epoch readers |
| Medium | v1.0 format stability (snapshot + WAL semver guarantee) |
| Low | `CASE` in a write-statement `RETURN`; subqueries; napi-rs; WASM |
| Low | Multi-statement `BEGIN/COMMIT` interactive transactions |

---

## Docs

- [Quickstart](docs/site/quickstart.md) · [Rules](docs/site/rules.md) · [Cypher reference](docs/site/query.md) · [HTTP + MCP API](docs/site/api.md)
- [The live code graph](docs/site/code-graph.md) · [Concurrency](docs/site/concurrency.md) · [Codebase graph](docs/site/ingest-git.md)
- [Install, plugin and hooks](docs/site/skill.md) · [MCP tools](docs/site/mcp.md) · [Association benchmark](docs/site/association-bench.md)
- [Time travel](docs/site/timetravel.md) · [Subscriptions](docs/site/subscriptions.md) · [Views](docs/site/views.md) · [Rule suggestions](docs/site/suggest.md)
- [Masks and access control](docs/site/masks.md) · [Full-text search](docs/site/fulltext.md) · [Property indexes](docs/site/indexes.md) · [Graph algorithms](docs/site/algorithms.md)
- [Durability and recovery](docs/site/durability.md) · [Panic policy](docs/site/panic-policy.md) · [Testing](docs/site/testing.md) · [Format stability](docs/format-stability.md)
- [Design spec](docs/design.md) · [Moat roadmap](docs/site/roadmap-moat.md) · [Case study](docs/dogfood-report.md)

Building from source, Docker, packaging, and the test gates are in
[CONTRIBUTING.md](CONTRIBUTING.md).

---

## License

Copyright 2026 Matthew Sherlin.

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
