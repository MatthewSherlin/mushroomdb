# Roadmap to v0.5 — "The Memory Release"

> **Purpose:** the durable master plan for mushroomdb's journey from v0.4.0 to v0.5.0.
> Written to survive context windows. If you are a fresh agent or a future maintainer,
> this document + the two it references (`docs/roadmap/v0.4.1-foundations-plan.md`,
> `docs/site/roadmap-moat.md`) contain the entire trajectory and the reasoning behind it.
> Read this top-to-bottom before touching v0.5 work.

**Date:** 2026-09-01 · **Author context:** written immediately after shipping v0.4.0.

---

## 1. Where we are (v0.4.0, shipped 2026-08-31)

v0.4.0 is live on crates.io, npm, PyPI, and ghcr.io. It was an additive, backward-compatible release across five tracks:

- **Property (equality) index** — `MATCH (n:L {field: v})` is an O(matches) lookup instead of an O(N) scan. WAL-persisted, rebuilt on open, planner `IndexScan`. *Caveat: single inline equality only; compound/WHERE-clause filters still scan.*
- **Cypher fluency** — `collect()`, `UNION`/`UNION ALL`, `CASE WHEN`, multi-rel-type `[:A|:B]`, and `contains`/`startsWith`/`endsWith`/`toInteger`/`toFloat`/`toString`. (`toLower`/`toUpper` already existed pre-v0.4.)
- **Temporal `query_at(commit, cypher)`** — read the graph as it existed at a past WAL commit (Rust / `POST /query {"as_of": N}` / Python). *Caveat: rejected with role tokens or client masks — no ACL'd time travel yet.*
- **Trust hardening** — `mushroomdb verify` now runs a structural rkyv-bytecheck pass (closes the `access_unchecked` UB on a malicious snapshot); concurrency torture tests; Python parity (`enable_index`, `node_history`, `was_linked`).
- **Moat groundwork** — temporal shipped; rule chaining + memory-native *designed* in `docs/site/roadmap-moat.md`.

**Bugs fixed en route (real, not cosmetic):** graph algorithms returned zero over snapshot-restored derived edges (HTTP + CLI); Cypher couldn't insert list literals.

---

## 2. Strategic thesis (read this before arguing about features)

**Win a category; do not chase Neo4j.** You will not become "the best graph DB" by out-scaling Neo4j on general workloads. You become GOAT by being the **uncontested default for agent memory + self-maintaining app relationships** — the way SQLite owns *local relational* without beating Postgres. Depth in the wedge beats breadth everywhere.

**mushroomdb's differentiated position (verified against the 2026 competitive landscape):**

The agent-memory category (Zep/Graphiti, Mem0, Letta, Cognee, LangMem) is *entirely* built on **LLM extraction** — a model extracts entities/facts from text into a store. mushroomdb is a structurally different **third camp**: **declarative + deterministic + explainable + embedded + access-controlled.** It already owns primitives the leaders charge for:

| Category feature | Their approach | mushroomdb already has |
|---|---|---|
| Fact invalidation | Zep temporal edge invalidation | **Rule auto-retraction** (edges retract when props diverge; recorded in `edge_history`) |
| Provenance | Zep "episode-level provenance" | **`explain()`** — why any edge exists, with the arithmetic |
| "Millions of small cold graphs" runtime | Zep's tuned service | **V8 mmap** — 0.02s cold open, 31 MiB RSS |
| Hybrid retrieval | vector+graph+text | **`hybrid_search`** (RRF) |
| **Who is allowed to see it (ACL)** | **nobody in the category** | **RBAC masks + write-scopes** |

**The 2026 SOTA question list** (from Mem0's own state-of-memory report): *"what should the agent know now, where did it come from, is it still true, **who is allowed to see it**, and how is it assembled into context."* That ACL question is on everyone's list and **only mushroomdb answers it.** That is the wedge-within-the-wedge: **the agent-memory engine with real access control.**

**Benchmark reality (be honest):** Zep/Graphiti scores **63.8%** on LongMemEval (the category's standard test) with GPT-4o; Mem0 **49.0%**. mushroomdb has **no published number yet.** Zep is backed by Neo4j, funded, has production users. **We are a credible, differentiated, early-stage challenger — not the leader.** Publishing an honest LongMemEval number (even if not #1) is the price of admission to the conversation and is itself the differentiator (nobody else publishes deterministic + ACL'd memory numbers).

---

## 3. Versioning & release discipline (adopted 2026-09-01)

Pre-1.0 semver `0.MINOR.PATCH`:

- **PATCH (0.4.1, 0.4.2, …)** = backward-compatible fixes + hardening. **A patch release NEVER changes the on-disk format.** This lets us promise: *"within a 0.4.x line, upgrade in place, no rebuild."* That is the honest, shippable-today version of the 1.0 format promise, and it directly answers the standing evaluator concern ("format breaks between minors → rebuild on upgrade").
- **MINOR (0.5.0)** = new features, *may* change the format (with auto-migration + a documented note). The memory-release features that touch the data model (namespaces, bi-temporal) are minor precisely because they change the format.

**Cadence:** a series of small format-stable patches makes the engine unshakeable, *then* 0.5.0 lands the moat on a foundation people already trust. Frequent boring releases *are* the trust record.

---

## 4. Foundations-first: the v0.4.x patch series

**Why foundations before features:** the evaluator/adoption blockers are all foundation, not features. Nobody is asking for bi-temporal — they're saying "I can't treat this as source of truth yet." Build the trust, then the moat.

### v0.4.1 — backfill scale + CI trust gates  → **fully specced in `docs/roadmap/v0.4.1-foundations-plan.md`**
The single highest-value patch. Kills the **Cartesian backfill wall** (a real dogfood measurement: a `70k × 20k` rule projected **~685 min / ~331 GiB** — it materializes the cross-product before applying `max_edges`). Fix: apply per-source top-k *before* global accumulation and cap the `max_edges = None` path with a bounded heap. Plus close the **three-release CI blind spots**: un-ignore the HNSW recall gates, re-pin V8/group-commit bench baselines and gate regressions, audit the ~102 prod-path `unwrap`s, add WAL-replay + HTTP-body fuzz targets. **Format-stable — ideal for unattended/overnight work.**

### v0.4.2 — safety + operability hardening (format-stable)
- **TLS on the HTTP server** — add `rustls` behind a feature flag, or at minimum a crisp "terminate TLS at a reverse proxy" doc + config. (Review L3.)
- **Format compatibility matrix in CI** — a test that opens a stored snapshot of *every* historical format version (V5→V8) and asserts it loads. Turns "format stability" from a claim into a defended invariant. This is the concrete first step of the 1.0 format promise.
- Finish the `unwrap` audit and expand fuzzing (WAL frame torture, snapshot-loader) if not fully covered in 0.4.1.
- **Cookie `Secure` flag** conditional on TLS (review M5, deferred in v0.4.0 because it would break the HTTP-only UI).

### v0.4.3 — query completeness + observability (format-stable)
- **Compound / WHERE-clause property index** — extend the equality index from single-inline-equality to (a) `WHERE n.f = x`, and (b) compound `AND` of equalities via index intersection. The KB's real queries are `MATCH (d:Doc {namespace: $ns})` + type/status/visibility filters — today only the first predicate is indexed. Reuse the `PropertyIndex` structure; add an index-intersection plan step.
- **Incremental subscriptions** — `subscribe_query` currently re-executes the full query per commit. Make it incremental (diff the affected rows). Foundational for scale.
- **Observability** — a `/metrics` endpoint (WAL size, RSS, commit rate) and a slow-query log. Operational maturity for "system of record" trust.

*(0.4.x ordering is a guide, not a contract — pull items forward if an adopter is blocked.)*

---

## 5. The v0.5.0 Memory Release — full scope

The release that makes mushroomdb undeniably #1 at the thing it is uniquely built for. Ranked in three tiers. **Design detail for chaining + memory-native lives in `docs/site/roadmap-moat.md`; the additions below come from the competitive analysis and the KB/evaluator feedback and refine it.**

> **Format warning:** namespaces and bi-temporal change the on-disk data model. They are minor-version features and **must be built with the maintainer reviewing** — not unattended. Auto-migration from any 0.4.x store is required, with a changelog note.

### Tier 1 — the wedge (the reasons to choose mushroomdb over Zep/Mem0)

**1. Namespaces (the #1 feature — validated by both the KB agent and the market).**
Per-agent / per-user / per-project partitioning, composed with the existing RBAC mask **at the same enforcement choke point** so the security proofs carry over. This collapses "compute an allow-list per request" into "declare the isolation once." Two shapes to support (the 2026 production pattern is *both*):
- **Hard isolation** — a namespace token sees only its namespace's nodes/edges; rules and vector search run within a namespace by default.
- **Cross-namespace shared edges** — private agent memory + shared org knowledge in one store ("isolated timelines *with* cross-agent edges for shared context").
- *Design:* a reserved namespace column (or `_ns:` label prefix) enforced where masks are enforced. The KB's 5-tier ACL is mostly namespace-shaped (project/project_admin = "which project UUIDs can you see"), so this is a direct fit. Reuse the mask intersection logic already proven in v0.3 RBAC.

**2. Temporal + RBAC composition (promote from "follow-on" to headline).**
Today `query_at` is rejected with role tokens/masks (no ACL'd time travel). The KB agent flagged this as a hard limiter; the competitive analysis shows *why it's a moat*: **temporal memory + per-caller ACL is a combination literally no competitor has.** Make `query_at` mask-aware (open the temporal instance, apply the role's mask to the temporal read). This unblocks exposing time-travel beyond admin-only.

**3. Bi-temporal edges (valid-time) — match Zep's defining feature.**
`query_at` gives *system/ingestion*-time travel. You are missing *valid time* — "who worked at Acme in 2021?" when the fact was recorded in 2026. Zep tracks four timestamps (t_valid, t_invalid, t_created, t_expired). The **pragmatic version is cheap**: edges already carry properties, so support `valid_from`/`valid_to` edge intervals + query predicates over them (and optionally an `AS OF VALID TIME t` sugar). This gives Zep-class temporal reasoning **without their LLM-extraction machinery** — and combined with rule auto-retraction (your existing "fact invalidation"), it's a complete temporal model.

**4. Published LongMemEval number (task #7, still pending — now a competitive necessity, not just honesty infra).**
It's *the* number buyers compare (Zep 63.8%, Mem0 49%). Build the harness, run it, publish the number **with methodology and known limitations** — even if not #1. If the declarative + RBAC approach scores well, that's the differentiation made concrete. The repo already operates this way (measured gates, published misses); this extends it.

### Tier 2 — depth (the moat matured)

**5. Rule chaining** (design in `roadmap-moat.md` §2). Derived edges feed dependent rules → `doc→entity→doc` cascades ("documents connected through the same client/person" — exactly what a KG over entity-docs wants). **Cycle-safe design:** static rule-dependency graph built at `create_rule`; bounded-depth propagation (default 4); per-node "already fired at depth d" set so each rule fires at most once per node per commit (guarantees termination even with cycles); reject un-damped edge→edge cycles at declare time; deterministic on replay. *Touches rule firing semantics — re-verify the cycle-safety when it's code, with the maintainer.*

**6. Decay + decay-weighted hybrid retrieval.** Links fade with age unless reinforced. Store each edge's last-reinforced commit (implied by `edge_history`); expose `decay(base, age, halflife)` as a scalar function + an optional maintained `effective_weight` view. **The novel synthesis nobody has:** combine decay with the existing `hybrid_search` so recent/reinforced memories rank higher automatically — a memory-native retrieval mode, not just a function. (Modest for a docs KB; central for agent working-memory.)

**7. Consolidation (with a hard retention-safety requirement).** Old per-node history compacts into summaries past a retention horizon (implemented at WAL-archive time; the archive machinery exists). **CRITICAL SAFETY FLAG from the KB agent:** consolidation compacts old `PropSet` history into summaries — *if a deployment ever serves document revisions / source-of-truth data from WAL history, consolidation would destroy it.* Requirement: an **explicit, opt-in retention window** that treats retained history (e.g. body revisions) as sacred and never compacts it. State this in the dogfood checklist before shipping.

### Tier 3 — the scale unlock (the "toy → real" gate)

**8. Larger-than-memory.** The #1 foundational ceiling: the write/derive path (provenance, HNSW, IVF, heap-allocated) caps at ~100k–500k nodes; mmap makes *open* larger-than-RAM but not the working set. Disk-backed/paged working set so agent memories grow unbounded. **This is the single biggest "sidecar → system of record" unlock in the review** and is the natural v0.5 (or v0.6) headline for scale. Larger effort than the memory features; may split across 0.5/0.6.

---

## 6. Adoption blockers → resolution map (the honest scorecard)

| Blocker (from evaluators / KB agent / review) | Resolved by |
|---|---|
| Cartesian backfill wall (70k×20k → 685min/331GiB) | **v0.4.1** |
| CI blind to perf + recall regressions (3 releases) | **v0.4.1** |
| ~102 unaudited prod-path unwraps | **v0.4.1 / 0.4.2** |
| Pre-1.0 format breaks between versions | **Patch-format-stability rule (now) + compat matrix (0.4.2) + 1.0 freeze** |
| No PyPI wheel / Docker image | **Shipped in v0.4.0** (crates/npm/PyPI/ghcr) |
| No TLS on HTTP server | **v0.4.2** |
| Compound/WHERE filters still scan | **v0.4.3** |
| ACL'd time travel impossible | **v0.5 (temporal+RBAC composition)** |
| No per-agent isolation | **v0.5 (namespaces)** |
| RAM-bound (~500k node ceiling) | **v0.5/0.6 (larger-than-memory)** |
| No multi-statement transactions / snapshot isolation | **post-0.5 (isolation model)** |
| Single-node, no replication (RDS blocker) | **post-0.5 (continuous WAL backup / PITR)** |
| Subscriptions re-execute | **v0.4.3 (incremental)** |

---

## 7. Definition of done for v0.5.0

- Namespaces (hard isolation + cross-namespace shared edges), composed with masks, with the RBAC adversarial suite extended to cover namespace escapes.
- `query_at` works under role tokens + masks (ACL'd time travel), tested.
- Bi-temporal valid-time edges + query predicates, tested; auto-migration from 0.4.x.
- Rule chaining with the cycle-safety suite green (fixpoint, termination, replay-equivalence).
- Decay function + decay-weighted retrieval; consolidation with the retention-safety guard.
- **A published LongMemEval number** with methodology and a documented limitations list.
- Full workspace green, clippy clean, fmt clean, all new format touched by the compatibility matrix.
- CHANGELOG `## v0.5.0` with a clear "format changed — auto-migrates from 0.4.x; back up first" note.

---

## 8. Cross-references

- `docs/roadmap/v0.4.1-foundations-plan.md` — the detailed, buildable 0.4.1 plan (backfill + CI gates). **Start here for the overnight/next build.**
- `docs/site/roadmap-moat.md` — design detail for rule chaining + memory-native (decay/consolidation/namespaces).
- `docs/site/durability.md` — crash-recovery model + why the deep WAL-derived-replay is deferred.
- `docs/site/indexes.md` — the property index (extend it for the compound index in 0.4.3).

## 9. Marketing note (for after 0.5 ships)

Do not post "best graph DB ever" — it invites attack on the (real) early-stage weaknesses. Post in the **honest, technical, limitations-included** register: "edges that declare, maintain, and explain themselves; per-agent namespaces; access-controlled by default; here's our LongMemEval number and here's what it can't do yet." The honesty *is* the marketing for this audience. **Gate the launch post on: 0.4.1 (wall fixed) + a published LongMemEval number + the format-stability promise.** A low-key build-in-public post is fine anytime.
