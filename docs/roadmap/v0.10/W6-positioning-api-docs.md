# W6 — Positioning, Public API, Docs, Integrations, Ledger Triage

> Part of the v0.10 spec — see [`../v0.10-spec.md`](../v0.10-spec.md). Where this document and the master spec disagree, the master spec wins; its §4 lists every override.

**Unit id prefix: PS-**

---

## 1. Outcome

After 0.10 a general graph developer can:

- Find mushroomdb by searching "embedded graph database Cypher Rust" — not just agent-memory terms — and land on a README and docs site that lead with graph operations.
- Open the engine in Rust, Python, TypeScript or via Bolt and write their first graph query in 60 seconds from any language, following a dedicated quickstart page per surface.
- Install LangChain and LlamaIndex adapters from PyPI (`pip install langchain-mushroomdb`, `pip install llama-index-mushroomdb`) and wire mushroomdb as a graph store without copying from the repo.
- Use memory-specific APIs (`db.memory.remember(...)`, `db.memory.recall(...)` in Python; `mushroomdb::memory::remember(...)` in Rust) without those names polluting the plain graph surface; a plain `GraphDb.open()` imposes no entity rules, alias caps, or identifier gate on ordinary nodes.
- Navigate a docs site organised graph-first (Get started → Data model → Cypher reference → Drivers/Bolt → … → Agent memory & MCP) rather than agent-first.

---

## 2. Current State

### Public API surface

- **Rust:** `crates/core-api/src/lib.rs:9` exports `pub mod memory` — the module boundary already exists. `GraphDb` itself (`crates/core-api/src/db.rs`) exposes `remember`, `recall`, `upsert_entity`, `forget` as inherent methods, but these are not re-exported from `lib.rs` under any `memory::` path; callers reach them via `db.remember(...)`.
- **Python:** `bindings/python/src/lib.rs:2094, 2161, 2204, 2314` — `remember`, `recall`, `upsert_entity`, `forget` are top-level `#[pymethods]` on `GraphDb`, indistinguishable from `insert_node`, `query`, `find_similar`. No sub-namespace.
- **TypeScript client:** `clients/typescript/src/client.ts` — no `remember`/`recall`; those reach the server only via MCP. No reorganisation needed.
- **CLI:** `mushroomdb --help` command ordering — not audited yet; assumed memory-first (install, mcp, demo appear before graph utilities).
- **Memory-only guardrails:** the 32-alias cap and `upsert_entity` label guard live in `crates/core-api/src/memory/remember.rs`. Plain `insert_node`, `set_prop`, `upsert_node` paths never invoke them. The identifier gate (`is_keymatch_rooted` in `crates/core-rules/src/def.rs:240`) is an internal rule-engine optimization — not user-facing.

### Package metadata

- `Cargo.toml` workspace keywords: `["graph", "embedded", "database", "mcp", "agent-memory"]` — "agent-memory" is identity-defining in discovery.
- `crates/core-api/Cargo.toml:10` description: `"Embedded graph database with Cypher queries, rule triggers, and Arrow export"` — graph-neutral; good.
- README tagline: `"The graph that stays true — and knows who's allowed to see it."` — agent-focused.
- `docs/site/index.md:1` opens with `"mushroomdb is the data layer for agents that reason over entities."` — must change.

### Docs site

- `docs/site/` has ~34 pages in a flat structure; `book.toml` drives the mdBook site.
- `docs/site/quickstart.md` leads with `mushroomdb install --delivery mcp` and the four MCP questions — agent-first.
- No language-specific 60-second pages for Rust, Python, TypeScript, Docker, or Bolt.
- `docs/site/rust.md` exists but is reference-style with usage examples, not a 60-second tutorial.
- `docs/site/python.md` covers binding usage; not positioned as a quickstart.
- Voice/tone PR (docs-overhaul PR 4) never shipped (`docs/roadmap/docs-overhaul-and-08-direction.md` memory note).
- `llms.txt` and `llms-full.txt` exist at repo root; content mirrors the agent-first site structure.

### Integrations

- `integrations/langchain-mushroomdb/` — LangChain `GraphStore` adapter exists with `pyproject.toml` and tests; not published to PyPI.
- `integrations/llama-index-mushroomdb/` — LlamaIndex adapter exists with `pyproject.toml` and tests; not published to PyPI.
- No GraphRAG-style example.
- Arrow bridge: `crates/arrow-bridge` exists; not surfaced in the Python binding beyond `ingest_json`.

### Release mechanics

- `scripts/bump-version.sh` manages 28 version sites across all `Cargo.toml`, `pyproject.toml`, npm, MCP manifests.
- Release checklist house style established: `docs/roadmap/v0.9.1-release-checklist.md`.
- "No competitor names" rule: global CLAUDE.md memory; current repo content uses category labels throughout.

---

## 3. Design

### 3.1 Rust public API (`mushroomdb` crate)

No module restructuring. `pub mod memory` already exists in `lib.rs:9`. The Rust public surface is already naturally separated: callers do `use mushroomdb::memory::{remember, ...}` or `use mushroomdb::GraphDb` and call `db.remember(...)`. Nothing to change for Rust callers — the module boundary already expresses the separation.

What does change:
- `lib.rs` doc comment on `pub mod memory`: add explicit note — "All functions in this module are memory-API. A plain `GraphDb` with no memory schema applied imposes none of these behaviours on ordinary nodes."
- Ensure no memory-module types (`RememberArgs`, `RecallResult`, etc.) appear in `GraphDb`'s own doc as primary surface; they should be listed under `memory::`.

### 3.2 Python API (`db.memory.*`)

Add a `MemoryAccessor` struct (`bindings/python/src/memory_accessor.rs`) — a thin wrapper holding a `Py<GraphDb>` reference:

```python
# 0.10: new path
db.memory.remember("...")
db.memory.recall("topic")
db.memory.upsert_entity("key", {...}, label="Person")
db.memory.forget(key="key")
db.memory.schema_report(budget_ms=200)

# deprecated aliases on GraphDb — emit DeprecationWarning, kept for 0.10
db.remember(...)       # → db.memory.remember(...)
db.recall(...)         # → db.memory.recall(...)
db.upsert_entity(...)  # → db.memory.upsert_entity(...)
db.forget(...)         # → db.memory.forget(...)
db.schema_report(...)  # → db.memory.schema_report(...)
```

`GraphDb.memory` is a `@property` returning a cached `MemoryAccessor`. The deprecated methods emit `DeprecationWarning` via Python's `warnings.warn` (called from Rust `PyErr` before delegation). Keeping the aliases for one release means `0.10` is the last version they work; `0.11` removes them.

**What a plain `GraphDb.open()` does not impose:** no alias cap on ordinary nodes, no `upsert_entity` label check, no identity-link creation. The identifier gate (`is_keymatch_rooted`) is internal rule engine logic and never visible to any caller.

### 3.3 CLI command ordering

Reorder `mushroomdb --help` output groups:
1. Graph operations: `query`, `asof`, `stats`, `verify`, `snapshot`, `migrate`, `serve`
2. Data input: `ingest-json`, `ingest-git`, `demo`
3. Agent memory setup: `install`, `mcp`, `doctor`

This is cosmetic (clap display order), not a flag/subcommand removal.

Add `--stdin` and `--file <path>` to `query` and `asof --query` (row 70 fix).

### 3.4 Package metadata

- **Workspace keywords** (`Cargo.toml`): change to `["graph", "embedded", "database", "cypher", "graph-database"]`. Remove `"mcp"` and `"agent-memory"` from the primary keywords (they belong in the CLI crate's own metadata, not the library).
- **CLI crate keywords**: `["graph-database", "cypher", "mcp", "agent-memory", "cli"]` — keeps agent-memory discoverable for that audience via `mushroomdb-cli`.
- **PyPI description** (`bindings/python/pyproject.toml`): update to `"Embedded graph database with Cypher queries, rule-derived edges, time travel, and agent-memory tools."`
- **npm** (`packaging/npm/package.json`): update description similarly; add `"cypher"`, `"bolt"` (once W3 lands) to keywords.
- **README tagline**: change to: `"An embedded graph database — Cypher queries, self-maintaining rule-derived edges, time travel, and a memory layer for AI agents."` Secondary line: `"Single file. Zero server. Opens in 20 ms."`
- **docs/site/index.md** first sentence: `"mushroomdb is an embedded graph database for any app that needs a graph — and a memory layer for AI agents."` Restructure the body to lead with the graph feature set; move the agent-memory / MCP install path to the bottom of the page.
- **MCP registry card / server.json**: keep agent-focused. The MCP registry audience wants agent-memory tooling. No change to the card's framing.
- **SKILL.md** (`crates/cli/skills/mushroom/SKILL.md`): keep agent-focused. Update version references. Add one line noting that graph queries (`query`, `asof`) can be used directly alongside the memory tools.
- **Homebrew**: verify `packaging/homebrew/` formula is up to date and file a tap submission if not already listed (`brew search mushroomdb`). This is a human-only step; add it to the 0.10 release checklist.

### 3.5 First-run experience — 60-second paths

Create `docs/site/get-started/` directory with:
- `rust.md` — 60-second Rust quickstart (add node, create rule, query, explain)
- `python.md` — 60-second Python quickstart (pip install, open, insert, query)
- `typescript.md` — 60-second TypeScript quickstart (npx serve, MushroomClient, query)
- `docker.md` — 60-second Docker quickstart (docker pull, run, curl first query)
- `bolt.md` — 60-second "existing app via Bolt" quickstart (W3 dependency: Bolt server must be running; skip or mark `[requires 0.10]` if W3 is late)
- `index.md` — language picker landing

The existing `docs/site/quickstart.md` becomes an "agent memory quickstart" that redirects general developers to `get-started/`.

Each language quickstart must:
1. Be a program that compiles and runs (Rust: added to `crates/core-api/examples/` and compiled in CI; Python: added to `bindings/python/tests/`; TypeScript: added to `clients/typescript/tests/`).
2. Lead with plain graph operations (no mention of `remember`/`recall` until the "What's next" section).
3. End with three links: full language reference, agent-memory guide, deployment.

### 3.6 Docs site information architecture (IA)

Replace the current flat 34-page list with a two-level hierarchy in `book.toml`. New chapter order:

```
Get started
  Rust
  Python
  TypeScript
  Docker
  Existing Cypher app (Bolt)  [W3 dependency]

Data model
  Nodes, edges, properties
  Labels and keys
  Namespaces

Cypher reference
  Query language
  Conformance table  [W1 dependency — generated]

Drivers & Bolt  [W3 dependency]

Transactions  [W2 dependency]

Indexes & constraints
  Full-text search
  Vector search
  Property indexes
  Unique constraints  [W1 dependency]
  Range indexes  [W1 dependency]

Import / export
  ingest-json
  ingest-git
  Arrow / pandas / polars

Deploy & backup
  Running as a service
  Deployment targets  [W3 dependency for templates]
  Durability and crash recovery
  Backup and restore

Performance
  Benchmarks
  Known limits

Data safety
  Concurrency and multiprocess
  Panic policy
  Testing guide

"Superpowers"
  Rules — derived edges
  Time travel
  Visibility and access control
  Full-text and vector search
  Rule suggestion
  Subscriptions
  Algorithms
  Views

Agent memory & MCP
  MCP quickstart (current docs/site/quickstart.md)
  MCP tools reference
  Install and plugin
  Skill and hooks
  Association benchmark

Reference
  CLI
  HTTP API
  Python API  [generated]
  TypeScript client
  Rust API
  MCP tools  [generated]
```

Pages that already exist are moved, not rewritten, unless their opening paragraphs need agent-framing removed. New pages: get-started/*, upgrade-from-0.9.md.

**Generated pages — extend CI pattern:**
- Existing: CLI reference, MCP tools, Python API, HTTP page (checked by a test against the router).
- Add in 0.10: Cypher function conformance table generated from W1's TCK results and checked in CI (if the table drifts from the test results, CI fails).

**Voice and tone (deferred PR 4 from docs-overhaul):**
- Replace "agents" as the subject in every non-agent-section page opener with the actual subject (graph, user, developer).
- Use present-tense technical statements. No adjectives asserting quality ("powerful", "blazing").
- The agent memory section keeps its own more personal voice — it IS the memory product.
- Apply this as a single pass in PS-5 after the IA skeleton lands.

**llms.txt / llms-full.txt:**
- Update the summary in `llms.txt` to reflect the new framing ("embedded graph database with self-maintaining derived edges").
- Re-run `scripts/gen-llms-full.sh` after the IA restructure; the generator must handle the new `get-started/` subdirectory. Update the generator's page list and the CI check.

### 3.7 Integrations

**In-repo packages (publish to PyPI in 0.10):**
- `integrations/langchain-mushroomdb/` → publish as `langchain-mushroomdb` on PyPI; add to `release.yml`.
- `integrations/llama-index-mushroomdb/` → publish as `llama-index-mushroomdb` on PyPI; add to `release.yml`.
- Both packages have `pyproject.toml`; add their versions to `scripts/bump-version.sh`.
- Add CI job: run each integration's test suite on every PR that touches `bindings/python/` or `integrations/`.

**GraphRAG-style example:**
- Add `integrations/graphrag-mushroomdb/` — a Python notebook (no new package; no PyPI publish) demonstrating entity extraction → mushroomdb storage → hybrid graph+vector retrieval as a GraphRAG pattern. Depends on `langchain-mushroomdb`.

**Arrow / pandas / polars:**
- `crates/arrow-bridge` exists; expose `GraphDb.to_arrow_tables()` in the Python binding (returns `dict[str, pyarrow.Table]` for nodes and edges). No new dependencies: `pyarrow` is already a dev dependency for the wheel build.
- Add a docs page under "Import / export".

**What stays out of this workstream and must go to Decisions:**
- OGM/ORM layer — owner must decide.
- JavaScript/TypeScript LangChain or LlamaIndex adapters — owner must decide.
- Publishing the integration packages as a monorepo vs. separate repos — builder's call (recommendation: keep in-repo for 0.10, extract if they grow independently).

---

## 4. Interaction with Differentiators

- **Rules engine / derived edges:** the docs restructure puts rules under "Superpowers" — not a demotion, a reframing. The rules page content does not change.
- **Time travel / as-of:** kept in "Superpowers"; the Cypher reference will link to it for the `AS OF` clause (once W1 adds it).
- **Scoped visibility:** moved from early agent-memory framing into "Superpowers / Visibility and access control."
- **Snapshots / WAL replay:** `db.memory.*` namespace is cosmetic; no change to snapshot semantics.
- **Full-text / vector indexes:** appear in "Superpowers" but also in "Indexes & constraints" as a cross-reference; no engine change.
- **MCP tools:** first-class, unchanged. The plugin's `SKILL.md` and `server.json` are not touched beyond version bumps.
- **Memory schema auto-application:** `schema apply --memory-defaults` is unchanged; the docs page for it moves to "Agent memory & MCP / Install and plugin."

---

## 5. Compatibility and Migration

### API breaks in 0.10

| Surface | What changes | Migration |
|---|---|---|
| Python `GraphDb.remember(...)` | Deprecated; still works with `DeprecationWarning` | Change to `db.memory.remember(...)` |
| Python `GraphDb.recall(...)` | Deprecated | `db.memory.recall(...)` |
| Python `GraphDb.upsert_entity(...)` | Deprecated | `db.memory.upsert_entity(...)` |
| Python `GraphDb.forget(...)` | Deprecated | `db.memory.forget(...)` |
| Python `GraphDb.schema_report(...)` | Deprecated | `db.memory.schema_report(...)` |
| Rust — none | `pub mod memory` already the boundary | No change for Rust callers |
| TypeScript — none | No `remember`/`recall` in TS client | No change |
| crate keywords | "agent-memory", "mcp" removed from library keywords | Not a code break; affects crates.io search only |
| CLI command group ordering | Visual only | No script breakage |

**0.10 is a minor bump (0.x); breaks are allowed per the brief.** The deprecated Python aliases are removed in 0.11.

### Format compatibility

W6 makes no engine or storage changes. No format version bump from this workstream. Compatibility analysis is W1 and W2's responsibility.

### Downgrade

Docs changes and Python namespace additions do not affect the on-disk format. A user who downgrades the library still opens their store normally.

---

## 6. Verification

### Tests

- **PS-2 (Python namespace):** unit tests that `db.memory.remember(...)` succeeds; that `db.remember(...)` emits `DeprecationWarning`; that the deprecated form still returns the correct result. Added to `bindings/python/tests/test_memory_namespace.py`.
- **PS-3 (CLI stdin/file query, row 70):** integration test: write a Cypher statement to a temp file, pipe it to `mushroomdb query --file`; verify correct result. Added to `crates/cli/tests/query_stdin.rs`.
- **PS-4 (get-started/ examples):** each quickstart Rust example added to `crates/core-api/examples/` and compiled in CI; Python example added to `bindings/python/tests/test_quickstart.py`; TypeScript example to `clients/typescript/tests/`.
- **PS-6 (conformance table CI):** fail CI if the generated Cypher function table diverges from W1's TCK results.
- **PS-7 (integration packages):** run `integrations/langchain-mushroomdb/tests/` and `integrations/llama-index-mushroomdb/tests/` in CI on every PR touching `bindings/python/` or `integrations/`.
- **PS-10 (ledger rows):** tests per row per the ledger's own pinning requirements (see section 7 notes).

### Performance gates

W6 introduces no engine changes. No performance gates needed for this workstream. Any doc-site build time regressions are informational only.

### Docs checks

- Re-run `scripts/check-links.py` after IA restructure; all internal links must resolve.
- Re-run `scripts/gen-llms-full.sh --check`; no missing or extra pages.
- Re-run `scripts/check-claims.sh`; no count or claim drift from moved pages.
- The generated Python API reference, MCP tools reference, and HTTP page CI checks continue unchanged.
- Add: Cypher conformance table check (W1 dependency).
- Add: integration test jobs for LangChain and LlamaIndex packages.

---

## 7. Build Units

| Unit | Description | Size | Files / crates touched | Depends on | Conflict hotspots |
|---|---|---|---|---|---|
| **PS-1** | Package metadata: keywords, descriptions, README tagline, docs/site/index.md framing | S | `Cargo.toml`, `bindings/python/pyproject.toml`, `packaging/npm/package.json`, `README.md`, `docs/site/index.md` | none | README (any PR can touch it) |
| **PS-2** | Python memory namespace: `MemoryAccessor`, `db.memory.*`, deprecated aliases with `DeprecationWarning` | M | `bindings/python/src/lib.rs`, new `bindings/python/src/memory_accessor.rs`, `bindings/python/tests/test_memory_namespace.py` | none | `lib.rs` (high-conflict; coordinate with any other Python binding PR) |
| **PS-3** | CLI: reorder help groups; add `--stdin`/`--file` to `query` and `asof --query` (row 70) | S | `crates/cli/src/lib.rs`, `crates/cli/tests/query_stdin.rs` | none | `crates/cli/src/lib.rs` |
| **PS-4** | Docs IA skeleton: add `get-started/` subdirectory, update `book.toml` chapter order, move existing pages (no rewrites yet), add redirect stubs for moved URLs | M | `docs/site/get-started/*.md`, `book.toml`, `docs/site/quickstart.md` | none | `book.toml` |
| **PS-5** | 60-second language quickstarts: Rust, Python, TypeScript, Docker, Bolt stub; examples compiled in CI; tone pass on moved pages | M | `docs/site/get-started/*.md`, `crates/core-api/examples/`, `bindings/python/tests/test_quickstart.py`, `clients/typescript/tests/` | PS-4, W3 (Bolt page can be skeleton) | examples dir |
| **PS-6** | Conformance table CI generation (generated Cypher function table checked against TCK results) | M | `scripts/gen-conformance-table.sh`, `docs/site/get-started/` cypher reference page, `.github/workflows/ci.yml` | W1 (TCK results must exist) | `ci.yml` |
| **PS-7** | LangChain + LlamaIndex PyPI publish: add to `release.yml`, `publish.yml`, `bump-version.sh`; CI test jobs | S | `integrations/langchain-mushroomdb/`, `integrations/llama-index-mushroomdb/`, `scripts/bump-version.sh`, `release.yml`, `publish.yml`, `.github/workflows/ci.yml` | none | `release.yml`, `publish.yml` |
| **PS-8** | GraphRAG example notebook; Arrow/pandas Python binding exposure + docs page | M | `integrations/graphrag-mushroomdb/`, `bindings/python/src/lib.rs` (arrow exposure), `docs/site/` import-export page | PS-7, PS-4 | `lib.rs` (coordinate with PS-2) |
| **PS-9** | `llms.txt` / `llms-full.txt` update; `gen-llms-full.sh` updated for new IA | S | `llms.txt`, `llms-full.txt`, `scripts/gen-llms-full.sh` | PS-4 | `llms-full.txt` (gen script; coordinate) |
| **PS-10** | Ledger rows 44, 54, 63 (skill shadowing), 64 (label disagreement report), 69 (user-scope brief), 88 (MCP arg coercion) — bundle of small fixes | M | `crates/cli/src/lib.rs` (44), `ci.yml` (54), `crates/cli/src/install.rs` (63, 69), `crates/core-api/src/memory/remember.rs` (64), `crates/server/src/mcp.rs` (88) | none | `crates/cli/src/lib.rs`, `mcp.rs` |
| **PS-11** | Ledger row 51: `same_as_pairs` propagates read errors (public signature change, `Vec` → `Result`) | S | `crates/core-api/src/memory/identity.rs`, callers in `remember.rs` | none | `memory/identity.rs` |
| **PS-12** | 0.10.0 release checklist + changelog structure + upgrade guide (0.9.x → 0.10) | S | `docs/roadmap/v0.10.0-release-checklist.md`, `docs/site/upgrade-from-0.9.md`, `CHANGELOG.md` template | PS-1 through PS-11 complete | `CHANGELOG.md` |

---

## 8. Decisions

### (a) Builder's call

**PS-2 deprecation scope:** recommend deprecating `remember`, `recall`, `upsert_entity`, `forget`, `schema_report` on the `GraphDb` class. Do NOT deprecate memory-adjacent utilities (`set_props_many`, `ingest_json`, `find_similar`) since these are general-purpose graph operations that happen to be useful for memory workflows too. A builder may settle this.

**GraphRAG example format:** Jupyter notebook vs. plain Python script. Recommend plain Python script (no new dependency; CI can run it as a test); a notebook can be added alongside for users who prefer it.

**LangChain/LlamaIndex in the same `release.yml`:** recommend yes — version them in lockstep with the main library so a `0.10.1` fix to the binding is reflected in the adapters. Downside: PyPI publishes happen for all three packages on every release. Low cost given they're small packages.

**`integrations/` stays in-repo for 0.10:** recommend yes. Extracting to separate repos adds CI complexity without benefit until the adapters have independent release cadences.

### (b) Owner must decide

**Row 66 — raw writes bypass alias maintenance:** `set_prop`, `upsert_node`, `set_props_many`, Cypher `SET`, `ingest_json` can write `name`, `aliases`, `alias_keys` and `id` on an entity node without the alias maintenance `upsert_entity` and `remember` do. Options:
1. **Refuse those fields on entity nodes from raw paths** — clean, breaks any caller writing them directly.
2. **Keep current behaviour** — documented on `set_props_many` and pinned by test; next describing write heals it.
3. **Move to `db.memory.*` namespace semantically** — document that the memory API guarantees alias integrity; raw paths explicitly do not.
Recommendation: option 3 — document the contract, do not add a refusal that would break callers.

**OGM/ORM layer:** an ORM layer wrapping `GraphDb` (Python model classes, schema validation, typed accessors) is frequently expected by developers coming from relational ORMs. Options:
1. **In-scope for 0.10 in `integrations/python-ogm/`** — adds one M-sized unit; requires owner approval of the API design.
2. **Deferred to 0.11** — keeps scope tight; can ship a thin `mushroomdb-orm` package independently.
3. **Never** — Cypher + the Python binding is already the idiomatic interface.
Recommendation: option 2. The adapters (LangChain/LlamaIndex) are the higher-leverage integrations for 0.10 discoverability.

**JavaScript LangChain / LlamaIndex adapters:** the Python adapters are in-repo and ready; JS adapters would need writing from scratch. Options:
1. **Add JS adapters to `integrations/` in 0.10** — L-sized; new TypeScript packages.
2. **Deferred to 0.11** — Bolt (W3) makes this easier since official JavaScript Bolt drivers would work with mushroomdb directly.
Recommendation: option 2 — defer; Bolt's W3 landing changes the landscape for JS integrations.

---

## 9. New Dependencies

| Package | In | Reason | No-new-dep alternative |
|---|---|---|---|
| `pyarrow` (dev/test only) | `bindings/python` test extras | Arrow export tests | Already present as dev dependency per build system; not a new addition |
| `langchain-community`, `langchain-core` | `integrations/langchain-mushroomdb/` | LangChain adapter runtime deps | These are the integration's whole purpose; no alternative |
| `llama-index-core` | `integrations/llama-index-mushroomdb/` | LlamaIndex adapter runtime dep | Same |

No new Rust crates. No new npm packages (beyond the integration packages themselves). Both integration packages are published as separate PyPI packages, so their dependencies do not become dependencies of the main `mushroomdb` Python wheel.

---

## 10. Out of Scope / Deferred

- **OGM/ORM Python layer** — owner decision; deferred to 0.11.
- **JavaScript LangChain / LlamaIndex adapters** — deferred to 0.11 pending Bolt (W3).
- **WASM / browser embedding** — not in 0.10.
- **CDC / streaming ingestion** — not in 0.10.
- **ISO GQL compliance** — marketing "GQL-path" credible as a direction; full conformance is 2027+.
- **Homebrew formula publication** — human-only step; on the release checklist but not a build unit.
- **LongMemEval published benchmark** — the harness exists (`benchmarks/longmemeval/`); running and publishing the number is W4's benchmark workstream, not W6.
- **Custom graph explorer UI** — Bolt (W3) unlocks third-party tools; no custom UI in 0.10.

---

## Ledger Triage

17 DEFERRED rows between `DEFECT-INDEX:BEGIN` and `DEFECT-INDEX:END` in `docs/roadmap/v0.6.10-defects.md`:

| Row | Severity | Fix in 0.10? | Assignment | Notes |
|---|---|---|---|---|
| 44 | Low | **Fix in 0.10** | PS-10 | One release of grace for retired hook bodies is done; delete `Command::RetiredHook` and the five bodies |
| 51 | Low | **Fix in 0.10** | PS-11 | Propagating error changes `same_as_pairs` from `Vec` to `Result` — handle during 0.10 API cleanup; two callers run post-commit, which means the error must be logged and swallowed there (no crash) |
| 54 | Low CI | **Fix in 0.10** | PS-10 | Enable `cargo clippy --all-features -D warnings` in CI; the `too_many_arguments` lint on `http.rs:484` must be fixed first (refactor the function or add a config struct) |
| 62 | Low | **Fix in 0.10** | W5 PF-07 | Measure the four caps at 1M (master §4.5) |
| 63 | Low | **Fix in 0.10** | PS-10 | Skill shadowing detection — install should warn when a user-scope skill shadows the project one |
| 64 | Low | **Fix in 0.10** | PS-10 | `remember` label disagreement: update the report to say the label went unused; not a silent wrong write |
| 65 | Low | **Fix in 0.10** | W3 (with TX-2) | GIL release requires atomising `upsert_node`'s read-then-write (master §4.5) |
| 66 | Low | **Close by documenting the contract** | W6 | Master §4.5: raw writes do not maintain aliases; the memory API does. No refusal |
| 67 | Low | **Fix in 0.10** | W1 | `prepare_delete_edge` correctness — engine fix, W1's scope |
| 69 | Low | **Fix in 0.10** | PS-10 | `delivery_for_store` path fix for user-scope installs |
| 70 | Low | **Fix in 0.10** | PS-3 | `query --stdin` / `--file`; PS-3 build unit covers it |
| 85 | Low | **Fix in 0.10** | W1 | `what_if_set_prop` double-count; engine fix, W1's scope |
| 88 | Low | **Fix in 0.10** | PS-10 | MCP arg coercion; fix in `crates/server/src/mcp.rs` |
| 90 | Low | **Split across workstreams** | W1 + PS-10 | Batch preview `label_of` and Cypher `count(true)` / bool bugs → W1; TypeScript `algo` config → PS-10 (small client-side fix); invisible-char refusal → W1 |
| 101 | Low | **Keep deferred** | — | Snapshot refusals after interrupted snapshot are safe (never wrong answers, only refusals); complex to fix; defer to 0.11 |
| 102 | Medium | **Fix in 0.10** | W5 PF-01 | Key-anchored label scan at 1M nodes (281 ms) (master §4.5) |
| 114 | Low | **Fix in 0.10** | W3 TX-6 | `reader().node_edges` reports `derived: false` always (master §4.5) |

### Snapshot --truncate unreported defect

The project memory note (`docs-overhaul-and-08-direction.md`) references `"snapshot --truncate misreports history"` as a known unreported engine defect. Searching the repo confirms this is **ledger row 80** (`docs/roadmap/v0.6.10-defects.md:95`), which is marked **CLOSED** — the floor was moved to the correct position before the WAL is replaced, and the fix is pinned by `crates/core-api/tests/snapshot.rs` and CLI tests. **No new ledger row needed.** The project memory note predates the fix.

---

## 0.10 Release Checklist Shape

Follow `docs/roadmap/v0.9.1-release-checklist.md` house style (human-steps only; CI does the rest). Key additions for 0.10 as a major-feature release:

**Pre-merge gates (new in 0.10):**
- Upgrade guide `docs/site/upgrade-from-0.9.md` is published and linked from `CHANGELOG.md`.
- Conformance score is published on the docs site and in the changelog (W1 dependency).
- LangChain and LlamaIndex packages smoke-tested in fresh virtualenv at the release version.
- Benchmark run at 1M+ nodes is published and linked (W4 dependency).
- All deprecated Python aliases emit `DeprecationWarning` (not errors) on the release version.
- Homebrew formula updated and tap PR open (human step; can lag 24 h).

**CHANGELOG structure for 0.10.0:**
```markdown
## 0.10.0 — YYYY-MM-DD

### Migration from 0.9.x
[upgrade guide link + three-bullet summary of breaks]

### What's new
[four parts: Cypher, Bolt, Deploy, Docs/Integrations]

### Bug fixes
[ledger rows closed in 0.10, by row number]

### Deprecations
[db.remember → db.memory.remember; removal in 0.11]
```

**Version bump mechanics:** add `integrations/langchain-mushroomdb/pyproject.toml` and `integrations/llama-index-mushroomdb/pyproject.toml` to `scripts/bump-version.sh`'s site list. The `--check` mode must verify them alongside all existing sites.

**"No competitor names" rule:** all copy produced by this workstream uses category labels only ("the incumbent server graph DB", "embedded graph DBs"). This applies to the upgrade guide, README, and integration READMEs. The rule is enforced by `scripts/check-claims.sh` scan; add integration paths to that scan.
