# mushroomdb — tighten, then generalize

**Date:** 2026-08-25
**Status:** Locked spec for the 0.2 → 1.0 sequence
**Supersedes:** `docs/design.md` claims that conflict with this document (see § Honesty)
**License:** Apache-2.0

---

## 1. Intent

Become the graph database people actually use — Python, TypeScript, CLI, HTTP, MCP — not a Rust-only kernel with a brochure.

The path is **tighten first**. Completeness (full Cypher, multi-label, disk-beyond-RAM, clustering) waits until the floor is trustworthy and non-Rust surfaces match the Rust API.

Category to own: **the graph that builds and explains itself.** General-purpose graph completeness is a later layer on that category, not a parallel rewrite into Neo4j.

---

## 2. Locked decisions

| Decision | Choice | Why |
|---|---|---|
| Sequencing | Four phases + a wait list. Phase N does not start until its gate is green. | Doing storage, Cypher, and 3-node rules at once will produce a larger untrustworthy surface. |
| Default top-k | New scored rules default `max_edges: Some(32)`. KeyMatch / auto-FK default `Some(1)`. Explicit `None` keeps today's global 1_000_000 first-N-by-id budget. | `None` is the production footgun (dogfood 8% set-coverage). FK is many-to-one. |
| ANN | No new crate in Phase 1. Cosine-normalize existing IVF. In-tree HNSW in Phase 4. | User rule: no surprise dependencies. IVF L2-vs-cosine is the bug to fix first. |
| Python `insert_node` | Flip to `(label, key, props)` matching Rust. Pre-1.0 break. | "Used by everyone" fails if the two embed APIs disagree. |
| Serve bind | Default `127.0.0.1:8080`. Non-loopback bind requires `--token` / `MUSHROOMDB_TOKEN`. Docker may keep `0.0.0.0` only with a token. | Current Docker CMD is unauthenticated R/W on all interfaces. |
| Snapshot | `mushroomdb serve` snapshots on SIGINT/SIGTERM. New `mushroomdb snapshot` CLI. `GraphDb` Drop does **not** snapshot (tests, embedded callers). | WAL-only open is 8 min at 100k. Operators must not have to remember. |
| Planner | Phase 1 adds `ScanKey` + expand-from-bound. No cost-based join reorder beyond that. | Highest usability-per-line. Full optimizer waits for storage physics. |
| Storage rewrite | Phase 3. Not started while HashMap topology still meets 10k–100k with caps. | Sortledton/mmap on a moving API is a year of work; do it once. |
| 3-node rules | Phase 4. Two-node theta-joins stay the v1 model. | Copyable until 3-atom incremental joins exist; don't start until top-k + ANN are real. |
| Completeness wait-list | Multi-label, parallel edges, full openCypher, GQL, clustering, napi-rs, WASM, Aura-style RBAC. | Explicitly **not** 0.2. Revisit after Phase 3 gate. |
| Format | Pre-1.0 may break WAL/snapshot. Phase 3 introduces a versioned schema; 1.0 freezes it. | Positional bincode cannot survive 1.0. |
| Docs vs code | If they disagree, **code wins and docs change in the same PR** — except View `Count`, which is a code bug (docs are correct). | `docs/site/index.md` currently advertises `radius_km` / `dims` fields that do not exist. |
| Native TS | HTTP `mushroomdb-client` is the TypeScript surface through 1.0. napi-rs stays wait-list. | One embed API (Rust/Python) + one network API (HTTP/TS) is enough for "everyone." |

---

## 3. Who "everyone" is

| Audience | Surface they touch | Phase that makes them real |
|---|---|---|
| Python app / matching job | `pip install mushroomdb` → `GraphDb.open` | 1 (API parity) + publish after 1 |
| Node / frontend | `mushroomdb-client` HTTP + WS | 1 (missing methods) + publish after 1 |
| Local human | `npx` / brew / Docker → explorer at `:8080` | 1 (default port, token, snapshot) |
| Agent | `mushroomdb mcp` + write + real recall | 1 (query_write) + 4 (ANN find_similar) |
| Rust embedder | `core_api::GraphDb` | already; must not regress |

Not an audience until the wait-list: DBaaS buyers, Neo4j Browser analysts, multi-tenant SaaS.

---

## 4. Phases and gates

```
Phase 1  Trust + reach     ──►  gate G1
Phase 2  App query surface ──►  gate G2
Phase 3  Storage physics   ──►  gate G3   (do not start before G2)
Phase 4  Linking moat      ──►  gate G4   (may overlap Phase 2 after G1; HNSW after IVF fix)
Wait-list                  ──►  after G3, new spec
```

### Gate G1 (Phase 1 done)

- `cargo test --workspace`, UI/Python/TS gates green.
- Docker refuses to listen on non-loopback without a token.
- `mushroomdb serve ./db` prints `http://127.0.0.1:8080` and snapshots on Ctrl-C.
- `suggest_rules()` / auto-FK never emit `max_edges: None`.
- `MATCH (n {id: $k})` is an `IdMap` point lookup (test pins no full label scan).
- Python `insert_node(label, key, props)` matches Rust.
- TS client has `explain`, `createRule`, `node`, `neighborhood`.
- MCP `query` dispatches writes via `query_write`.
- README/design/site no longer claim mmap, Sortledton, vectorized executor, "JSON nowhere", napi-rs-at-launch, or Neo4j differential testing.

### Gate G2 (Phase 2 done)

- `IN`, `DISTINCT`, `MATCH … SET … RETURN` work with named errors gone.
- HTTP writes run in `spawn_blocking`.
- `FsyncPolicy::{Strict, Batched, Relaxed}` exists; ingest uses Batched.
- `All` candidate spec intersects indexes (not `parts[0]` only).
- `GET /health` returns process liveness + node/edge counts.
- `mushroomdb query` CLI runs read and write Cypher.

### Gate G3 (Phase 3 done)

- Topology is typed CSR (or Sortledton-style blocked adjacency), not `HashMap<u32, Vec<u32>>` as the live structure.
- Columns are `Vec<T>` + null bitmap + interned strings for `Str`.
- Snapshot open at 100k (current dogfood shape) is **< 1 s** (today 8.88 s). Target still 100 ms at 5 GB; 1 s is the G3 bar.
- WAL records dense ids after first intern of a key.
- Snapshot/WAL have an explicit versioned schema (not positional bincode appends).
- `u32` id allocation fails loudly before wrap.

### Gate G4 (Phase 4 done)

- In-tree HNSW behind `approximate: true` for `VectorSimilar`; IVF remains a fallback.
- Per-query recall on the 5k/1536 dogfood probe **min ≥ 0.90** (today min 0.625).
- MCP `find_similar` uses the ANN index, not only pre-derived `SIMILAR` edges.
- At least one 3-node rule form ships (`All` of a 2-node predicate plus a hop).
- `subscribe_query` exists for a documented Cypher subset (not full differential dataflow).

---

## 5. A–E mapping

### A. Make linking unbeatable

| Item | Phase | Notes |
|---|---|---|
| A1 Default `max_edges` | **1** | Scored 32, KeyMatch 1, suggest/auto-FK/demo/HTTP omit → those defaults. Explicit `None` preserved. |
| A2 HNSW + recall SLO + auto-rebuild | **1** auto-rebuild on IVF drift; **4** HNSW | Do not add usearch/hnsw crate in 1. |
| A3 Cost-based `All` | **2** | `CandidateSpec::Intersect`. Leading `VectorSimilar` no longer disables every other index. |
| A4 3-node / path rules | **4** | Wait. Spec in Phase 4 plan, not here. |
| A5 Suggest estimates top-k cardinality | **1** | Preview applies the default cap. Dense FieldEqual still suggested, always capped. |

### B. SQLite-shaped usability

| Item | Phase |
|---|---|
| B6 One install that prints a URL | **1** default `:8080`; publish brew/npm/PyPI/GHCR is a **release** step after G1, not engine work |
| B7 Client/docs parity | **1** |
| B8 `mushroomdb query` + `snapshot` CLI | **1** |
| B9 Snapshot-on-close + cadence | **1** shutdown snapshot; **2** `--snapshot-every <duration>` (not in Phase 1) |

### C. Become a database

| Item | Phase |
|---|---|
| C10 Planner point lookup + expand-from-bound | **1** |
| C11 `IN` / `DISTINCT` / `MATCH SET RETURN` / MERGE ON CREATE | **2** |
| C12 Physical adjacency + real columns | **3** (gated on G2) |
| C13 Group commit + fsync policy + dir fsync + `F_FULLFSYNC` | dir/`F_FULLFSYNC` in **1**; group commit / Batched policy in **2** |
| C14 `spawn_blocking` on writes | **2** (HTTP); identified in 1 docs |

### D. Agent memory

| Item | Phase |
|---|---|
| D15 MCP `query_write` + real `find_similar` | query_write **1**; ANN find_similar **4** |
| D16 Query subscriptions | **4** |

### E. Stop doing

| Item | Phase |
|---|---|
| Honesty pass (README, design.md, site) | **1** first PR |
| Do not GPU-hero the canvas | document 200–400 node viewport in **1**; no cosmos rewrite |
| No parking_lot, no dual-replica on HashMaps | written here; no task |
| No Cypher-completeness race before planner | C11 after C10 |

### Code-quality fixes

| Sev | Fix | Phase |
|---|---|---|
| High | Directory fsync + Darwin `F_FULLFSYNC` | **1** |
| High | Default top-k on suggest / auto-FK | **1** |
| High | Docker non-loopback requires token | **1** |
| High | `MATCH (n {id:$k})` → `IdMap.get` | **1** |
| Med | View `Count` = neighbors that **have** the property | **1** |
| Med | Cosine-normalize before IVF k-means | **1** |
| Med | Auto-rebuild IVF when drift exceeds threshold | **1** |
| Med | Split `exec.rs` when adding `ScanKey` (new file `scan.rs` or `plan_ops.rs` only if the new op does not fit; prefer adding `ScanKey` in place first, extract if the file grows) | **1** extract only if a clean seam appears; **2** if not |
| Med | Versioned snapshot/WAL schema | **3** |
| Med | Python arg order = Rust | **1** |
| Low | Docs: `toLower`/`size` are ASCII/bytes until a Unicode pass | **1** docs; Unicode **wait-list** |
| Low | `textMatches` in unknown-fn error list | **1** |

---

## 6. Honesty overlay on `docs/design.md`

These sentences in `docs/design.md` / README are **false today**. Phase 1 rewrites them to match code. They become true only when the named phase lands.

| Claim | Reality | Becomes true |
|---|---|---|
| Sortledton adjacency | `HashMap` + sorted `Vec` | Phase 3 |
| Columnar Vec + null bitmap | nested `HashMap<String, HashMap<u32, Value>>` | Phase 3 |
| mmap / rkyv zero-copy snapshots | bincode + zstd into heap | Phase 3 |
| Epoch snapshot readers | `std::sync::RwLock` | after Phase 3 |
| Vectorized batches ~1–2k IDs | row-at-a-time Volcano | after Phase 3 (optional) |
| JSON nowhere in the data path | HTTP JSON is the app path; Arrow is default `/query` | keep Arrow default; stop saying JSON nowhere |
| napi-rs at launch | HTTP TS client only | wait-list |
| Differential Cypher vs Neo4j | equivalence is vs traversal API | wait-list |
| Nodes ≥1 label | exactly one label | wait-list |
| Fsync `strict` / `batched` / `relaxed` | strict only | Phase 2 |
| DB open 5 GB < 100 ms | 1.1 GB open 8.88 s | Phase 3 target; G3 bar is 100k < 1 s |

`docs/concurrency-decision.md` already tells the truth. Leave it. Point README at it.

---

## 7. Default top-k semantics (normative)

```text
DEFAULT_SCORED_TOP_K  = 32
DEFAULT_KEYMATCH_TOP_K = 1
DEFAULT_MAX_EDGES (None) = 1_000_000   # unchanged global first-N-by-id
IVF_DRIFT_REBUILD     = 256            # rebuild_rule when dst-side drift exceeds this
```

Resolution order for `RuleDef.max_edges`:

1. Caller set `Some(k)` → use `k`.
2. Caller set `None` explicitly in Rust → global budget (escape hatch).
3. JSON / Python / HTTP omit the field → scored default 32, KeyMatch default 1.
4. `suggest_rules` and auto-FK **always** fill `Some(default)` — they never emit `None`.
5. Demo rules use scored 32 / KeyMatch 1.

`create_rule` does not rewrite an explicit Rust `None`. Docs warn that `None` is "uncapped, first-N by dense id, not by score."

---

## 8. Auth (normative)

- Loopback (`127.0.0.1`, `::1`) : no token required.
- Any other bind: server **exits 1** unless `--token <str>` or env `MUSHROOMDB_TOKEN` is non-empty.
- When a token is set (even on loopback), every HTTP request except `GET /health` must send `Authorization: Bearer <token>` or `?token=`. WS `/watch` and `/subscribe` take the token as a query param (browsers cannot set WS headers portably).
- MCP stdio: no HTTP, no token (local process, existing SECURITY.md threat model).
- No TLS in-process. Reverse proxy remains the TLS story.

Docker: `CMD` may stay `0.0.0.0:8080` **iff** the image documents `MUSHROOMDB_TOKEN` and the binary enforces the rule. Example compose file sets a token.

---

## 9. Planner additions (normative)

### `ScanKey`

New `PlanOp` variant:

```rust
ScanKey {
    var: String,
    key: Operand,           // Lit(Str) or Param(name)
    label: Option<String>,
}
```

Emitted when a MATCH node pattern's property map is **exactly one** equality on field `id` (the IdMap key; ingest default `key_field` is `id`). Mixed maps (`{id: $k, age: 3}`) stay `ScanLabel` + `LookupProps`.

Executor: resolve operand to `Value::Str`, `view.node_id(s)`, optional label check, emit 0 or 1 row. Never walks `labels`.

Test hook: reuse or add a counter analogous to `FUSED_SCAN_FIRES` that increments only in the `ScanKey` arm.

### Expand-from-bound

When compiling a pattern whose **rightmost** node is already bound and the **leftmost** is not, walk the pattern right-to-left and invert relationship direction. The dogfood query:

```cypher
MATCH (t:Talent {id: $tid})
MATCH (c:Company)-[i:INDUSTRY_ALIGNMENT]->(t)
```

must plan as `ScanKey(t)` then `Expand(from=t, dir=In, etype=INDUSTRY_ALIGNMENT, to=c)`, not `ScanLabel(Company)`.

---

## 10. View Count (normative)

`NeighborAgg { agg: Count, prop }` counts neighbors for which `props.get(nbr, prop)` is `Some(_)`. Missing property → not counted. This matches `docs/site/views.md` line 20. DST oracle `scratch_view_value` must use the same definition (today it mirrors the bug).

`Degree` is still `neighbors.len()`.

---

## 11. IVF (normative, Phase 1)

Before `kmeans_fit`, L2-normalize each vector (zero vector skipped, not clustered). Centroids live in cosine-equivalent space. Exact cosine evaluation is unchanged.

On `on_node_changed` / `on_node_removed` for an approximate rule, if `ivf_dst_drift > IVF_DRIFT_REBUILD`, WAL-log `RebuildRule` as a **second commit** after the triggering op (drift is only known post-apply, so a single pre-WAL Batch cannot include it). Rebuild resets drift so it cannot loop. Replay is still a pure function of the WAL. Tests may lower the threshold via a `#[cfg(test)]` override.

---

## 12. Non-goals (wait-list)

Do not schedule implementation until a new spec after G3:

- Multi-label nodes, parallel edges, relationship properties on CREATE
- `CASE`, subqueries, `EXISTS {}`, `UNION`, `collect()`, `COUNT(DISTINCT)`, unbounded variable-length
- Interactive `BEGIN/COMMIT` multi-statement transactions
- Clustering, replication, RBAC roles
- napi-rs, WASM playground
- GQL
- GPU query / cosmos rewrite for 50k on-canvas nodes
- Dual-replica epoch readers on HashMap `GraphDb`
- `parking_lot` swap
- LLM extraction plugins

---

## 13. Success metrics

| Phase | Metric |
|---|---|
| 1 | A Python user can `insert_node` + `create_rule` + `query` with the same argument order as Rust; a TS user can `explain`; a human runs two commands and gets `:8080`; Ctrl-C does not leave an 8-minute reopen. |
| 2 | A developer can paste a 2-hop Cypher snippet with `IN` / `DISTINCT` / `SET … RETURN` and get a result or a named error that is not "unsupported because we didn't." Ingest of 10k nodes via HTTP does not pay 10k fsyncs. |
| 3 | 100k dogfood snapshot open < 1 s; RSS at 100k drops vs HashMap baseline (measure and record). |
| 4 | `find_similar` returns neighbors for a new embedding without a pre-existing `SIMILAR` edge type; 3-node rule dogfood fixture exists. |

"Used by everyone" is G1 + a release that publishes PyPI, npm `mushroomdb-client`, GHCR, and the install.sh tarball. Publishing is a human release step, not a code task in Phase 1 — but Phase 1 must make those packages **correct** so the release is not a lie.

---

## 14. Key Decisions (summary)

1. Tighten before completeness — general-use is client parity + install + planner, not full Cypher.
2. Default top-k is the linking footgun fix; explicit `None` stays as escape hatch.
3. No ANN crate until Phase 4; fix IVF metric first.
4. Non-loopback HTTP is token-gated; loopback stays local-first.
5. Storage rewrite is a gated phase, not a drive-by.
6. 3-node rules are the real moat and wait until two-node linking is safe at default settings.
7. `docs/design.md` is amended to describe the shipped system; aspirational claims move to this spec's later phases.

---

## 15. PR Plan

Ordered, independently reviewable. Implementation plans live under `docs/superpowers/plans/`.

| PR / plan | Title | Depends on |
|---|---|---|
| P1 | Honesty + auth/bind + fsync durability | — |
| P2 | Client/docs parity (Python order, TS methods, MCP writes, site docs) | — (parallel with P1) |
| P3 | Default top-k + suggest preview + View Count | — (parallel with P1) |
| P4 | IVF cosine-normalize + drift auto-rebuild | P3 (uses rebuild_rule) |
| P5 | Serve `:8080`, snapshot CLI, shutdown snapshot | P1 (fsync/dir sync used by snapshot) |
| P6 | Planner `ScanKey` + expand-from-bound | — (parallel) |
| P7 | `mushroomdb query` CLI | P6 (benefits from ScanKey; can land after P2) |
| — | **G1** | P1–P7 |
| P8 | Cypher `IN` / `DISTINCT` / `MATCH SET RETURN` / MERGE ON CREATE | G1 |
| P9 | FsyncPolicy + ingest Batched + HTTP `spawn_blocking` + `/health` | G1 |
| P10 | `All` index intersect | G1 |
| — | **G2** | P8–P10 |
| P11 | Physical columns | G2 |
| P12 | Physical topology (CSR/Sortledton) | P11 |
| P13 | Versioned snapshot/WAL + dense-id WAL + mmap open | P12 |
| — | **G3** | P11–P13 |
| P14 | In-tree HNSW + MCP find_similar | G1 (IVF fix); better after G3 |
| P15 | 3-node rules | G1; better after G3 |
| P16 | `subscribe_query` | G2 |

Do not open P11–P13 before G2. Do not open wait-list work in this sequence.
