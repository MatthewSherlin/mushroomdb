# mushroomdb Marketplace Dogfood — Findings Report

**Date:** 2026-08-19  
**Machine:** Apple M4 Pro, 24 GiB RAM, macOS 15.7.3 arm64  
**Scope:** Plan 10 end-to-end dogfood against a synthesized marketplace-shaped graph (100k nodes, 1536-dim embeddings). Sources: `dogfood/results/scale-100k.md`, `dogfood/ui_run.md`, `dogfood/test_fixtures.py`, `dogfood/test_scale_small.py`.

> **Caveat (applies throughout):** All numbers are from a local embedded process (`mushroomdb.GraphDb`, not HTTP) on Apple silicon with synthetic hash-chain embeddings. They are **not apples-to-apples** with the marketplace production stack (networked, 14-shard Meilisearch, real OpenAI embeddings). The comparison is structural, not a benchmark replacement.

---

## 1. Semantics — Six Rule Kinds vs Marketplace Matcher Edges

### Result: PASS on all exercised fixture pairs

Ten rule instances (six semantic kinds) were validated against hand-derived expected sets on verbatim marketplace fixtures (5 Talent / 2 Company / 2 Job / 7 User nodes). Every exact-set assertion, score pin, and negative-case check passed (`dogfood/test_fixtures.py`, `test_ingest_rules_derived_edges_and_explain`).

| Rule kind | Predicate | Instances | Fixture pairs derived | Result |
|---|---|---|---|---|
| `field_equal` (industry) | equality → 1.0 | TC + TJ | INDUSTRY_TC: 5, INDUSTRY_TJ: 5 | PASS — exact set |
| `overlap` (specialties) | Jaccard ≥ 0.15 → Jaccard | TC + TJ | SPECIALTY_TC: 4, SPECIALTY_TJ: 4 | PASS — exact set |
| `geo_radius` (location) | km ≤ radius | TC + TJ | LOCATION_TC: 0, LOCATION_TJ: 0 | PASS — correct empty (see gap 2 below) |
| `numeric_within` (size_bucket) | \|Δbucket\| ≤ tol → score | TC (tol=1 and tol=0) | SIMILAR_SIZE_TC: 10; SIMILAR_SIZE_STRICT_TC: 5 present + 5 confirmed absent | PASS — exact set + genuine negatives |
| `matches_design_style` | style overlap | TC | DESIGN_TC: 0 | PASS — correct empty (see gap 2 below) |
| `vector_similar` (semantic) | cosine ≥ 0.85 | TC | ≥ 1 pair; alice→firma pinned to 0.997616453151844 (independent oracle) | PASS — exact set + score pin to 1e-6 |

Score pins verified independently (`_cosine_oracle` in `test_fixtures.py` — no import from `transform.py`):
- `industry_alignment alice→firma`: 1.0  
- `specialty_match alice→firma`: 1.0  
- `similar_size alice→firma (tol=1)`: 0.0 (|3−2|/1 = 1.0, so score = 0.0)  
- `similar_size alice→firmb (tol=1)`: 1.0 (|3−3| = 0)  
- `semantic_match alice→firma`: 0.997616453151844

### Composition gaps (documented, not defects)

**Gap 1 — 'both'-industry 0.8 score:** `field_equal(industry)` in these fixtures fires binary 1.0 only. The 'both' industry value (which the marketplace scorer weights at 0.8 rather than 1.0 for an exact match) is absent from the fixture set (`test_fixtures.py`: `"# field_equal(industry) — binary 1.0. 'both' is absent from these fixtures."`). The 0.8 composition branch is unexercised.

**Gap 2 — GEO and STYLE field coverage:** The verbatim fixtures have no `lat`/`lon` or `design_styles` fields. `location_fit` and `matches_design_style` both fire zero edges on fixture data — correctly so, but these predicate branches (radius arithmetic, style-set overlap) received their first real validation only via the T2/T3 synthesized data where those fields are populated (`test_fixtures.py`: `"GEO and STYLE predicates derive 0 on verbatim fixtures (no lat/lon or design_styles fields) — semantic validation of those two kinds happens in T2/T3 synthesis"`). The 2k smoke run (`test_scale_small.py`) confirms both rules exercise the predicate at synthesized scale.

---

## 2. Scale — Engine Correctness and Two Walls

### 2a. What ran at 100k

| Phase | Status | Wall | Notes |
|---|---|---|---|
| Ingest 100k nodes | ok | **8.92 min** | `insert_node` loop; 1 WAL fsync per node — no Python batch API |
| Non-semantic backfill | extrapolated | (see wall below) | 5k probe × 400 factor; full run not started |
| Semantic backfill | extrapolated | (see wall below) | 5k probe × 400 factor; full run not started |
| Incremental set_prop (n=100) | ok | 381.35 ms | p50=3.48 ms, p95=4.57 ms |
| Big-3 intersection (n=50) | ok | 6.25 ms | p50=5.7 µs, p95=10.1 µs — **intersection=0 (see §2c)** |
| explain (n=100) | ok | 48.95 ms | p50=4.5 µs, p95=5.2 µs |
| Reopen (WAL replay, 1.60 GiB) | ok | **26.87 s** | RSS after = 3.98 GiB; `snapshot()` not in bindings |

Source: `dogfood/results/scale-100k.md`, Phase timings table.

### 2b. Cartesian backfill wall

`create_rule` materializes the **full desired `BTreeMap<(src,dst), score>`** before applying `max_edges`. At 100k (70k Talent × 20k Company = 1.4B pairs), Cartesian predicates (`field_equal`, `overlap`, `numeric_within`, `geo_radius`) enumerate all pairs regardless of how many survive the cap.

5k probe results (`industry_alignment_tc`: 10.5 s, Δrss 847 MiB; `specialty_match_tc`: 14.8 s; `similar_size_tc`: 12.5 s — all hit the 1M-edge budget). Extrapolation factor 400 (pair-count scaling):

| Extrapolation | Value |
|---|---|
| Pair-count factor | 400× (70k/3.5k × 20k/1k) |
| Projected wall (pair-count linear) | 489 min |
| Projected wall (log-factor corrected) | ~685 min (BTreeMap O(log n) at 1.4B pairs vs 3.5M) |
| Projected Δrss | 331 GiB |
| Budget | 30 min / 8 GiB |

The full 100k non-semantic backfill was **not attempted**. The engine design issue: cap must be applied *during* desired-set construction, not after. Source: `dogfood/results/scale-100k.md`, "Backfill (phase 2) — cartesian materialization."

### 2c. Vector wall

5k ScanAll probe: 3.5M pair evaluations, 12.05 min, 111,696 edges, Δrss 92.58 MiB. Extrapolation to 100k:

| Metric | Value |
|---|---|
| Pairs at 100k | 1,400,000,000 |
| Projected wall | **80.3 h** |
| Projected Δrss | ~36 GiB |

Full 100k `semantic_match` was **not attempted** (under-30-min gate failed). Source: `dogfood/results/scale-100k.md`, "Semantic verdict (phase 3)."

### 2d. Correctness verdict

- **1k oracle:** `industry_alignment` exact-set compare: expected=58,100, got=58,100. No engine misbehavior. Source: `dogfood/results/scale-100k.md`, "Oracle."
- **2k smoke:** 11/11 pytest tests passed (`test_scale_small.py`), including FK edge assertion, 50-pair brute-force industry spot-check, reopen survival, and all phase timing checks.

### 2e. Anti-overclaim: the Big-3 µs number is not a 5s-matcher replacement

The Big-3 p50=5.7 µs is three empty `neighbors` calls — no `INDUSTRY_ALIGNMENT`, `SPECIALTY_MATCH`, or `LOCATION_FIT` edges exist at 100k because the backfill was not executed. **This number does not demonstrate that mushroomdb beats the marketplace 5-second matcher at 100k.** The honest claim is: at 2k scale (all matcher rules live, 1,228,422 derived edges), the `node_edges`+`neighbors` API path operates in µs; the backfill to reach that state is where the cost lives, and that cost is blocked by the cartesian wall at 100k. Source: `dogfood/results/scale-100k.md`, "Incremental / Big-3 / explain."

### 2f. Product-surface gaps that shaped the run

- Python bindings expose `insert_node`, `create_rule`, `set_prop`, `query`, `explain`, `neighbors`, `node_edges`, `node_info`. Missing: `ingest_json`, auto-FK, `batch()`, `stats()`, `snapshot()`.
- Cypher has no `COUNT`; intermediate rows cap at 1,000,000.
- Auto-FK was declared as explicit `KeyMatch` rules over a 500-node sparse User set. Source: `dogfood/results/scale-100k.md`, "Product-surface gaps."

---

## 3. UI — Verdict at Scale

### Against the original complaint ("5+ second queries, laggy at 100 nodes")

| Scenario | mushroomdb result | Verdict |
|---|---|---|
| Console MATCH LIMIT 100 | **21 ms** | Fast |
| Console MATCH LIMIT 500 | **35 ms** | Fast |
| Hub depth-1 expand (FK-only, 100k db) | **2.1 ms** | Fast |
| Live ingest 50 nodes + WS ticker | **75 ms** / confirmed | Fast |
| Canvas with 200 rendered nodes (zoom, swiftshader) | **308 ms** per action | Acceptable |
| Canvas with 1,000 rendered nodes (zoom, swiftshader) | **589 ms** per action | Degraded |
| Why panels — 4 rule types (2k db) | rendered correctly | Smooth + correct arithmetic |
| Rules + Console concurrent use | Run button inaccessible | **Broken (F1)** |
| Dense hub (400+ edges) add-to-canvas | UI busy > 5 s | **Blocked (F2)** |

Source: `dogfood/ui_run.md`, Timings Table and Smoothness Verdicts.

### Why panels

All four rule-type why panels rendered correctly against the 2k db (1,228,422 derived edges, all 12 rules live):

| Rule | Edge type | Weight | Why line |
|---|---|---|---|
| `industry_alignment_tc` | INDUSTRY_ALIGNMENT | 1.0 | `field_equal(industry): architecture = architecture` |
| `specialty_match_tc` | SPECIALTY_MATCH | 1.0 | `overlap(specialties) = \|{single-family}\| / \|{single-family}\| = 1` |
| `location_fit_tc` | LOCATION_FIT | 0.831 | `geo_radius(location) = 27.2 km ≤ 160.9 km` |
| `semantic_match_tc` | SEMANTIC_MATCH | 0.998 | `vector_similar(embedding) = cos ≈ 0.998 ≥ 0.85` |

Source: `dogfood/ui_run.md`, "Leg 2 — Why Panels."

### Canvas scale observation

`addHarvestedToCanvas` calls `expandNode(depth=1)` per result node, doubling visible count (LIMIT 100 → 200 nodes rendered; LIMIT 500 → 1,000 nodes). This is by design but creates a non-obvious UX: at marketplace density (a company node with 400+ incoming matcher edges), adding a query result set could add O(N × degree) nodes to the canvas. The 5s block in F2 is the immediate symptom. Source: `dogfood/ui_run.md`, Leg 3 and Finding F2.

Swiftshader qualifier: all zoom-interaction numbers are headless Chromium software WebGL (worst-case lower bound). Real GPU performance is expected to be faster; the 200-node "acceptable" threshold is likely conservative.

---

## 4. Defects and Embarrassments

| ID | Finding | Severity | Proposed owner |
|---|---|---|---|
| F1 | Rules panel overlaps console — clicking "Run" in console is blocked when Rules panel is open (pointer-events z-index conflict). Confirmed with 30-second playwright timeout before workaround. Visual evidence: `t4-2k-error.png`. | **Medium** | UI plan (CSS z-index / pointer-events fix) |
| F2 | `addHarvestedToCanvas` depth-1 expansion blocks UI > 5 s for dense hub nodes (company with 400+ INDUSTRY_ALIGNMENT edges in 2k db). The "Add to canvas" button stays disabled for the duration. Not tested at full 100k matcher density. | **Medium** | UI plan (progressive/lazy expansion, or cap the auto-expand depth) |
| F3 | Ticker shows "no events" gap between page-load "connected" state and first WS event. | **Low** | Accept — expected behavior |
| F4 | Label-chip count is 2× query LIMIT due to depth-1 auto-expansion. Potentially surprising in marketplace context. | **Low / Informational** | Accept / document expected behavior |
| F5 | Zoom interaction degrades ~2× from 200→1,000 rendered nodes under swiftshader (308 ms → 589 ms per action). | **Medium** | Accept with documented GPU target range; swiftshader is worst-case |

Source: `dogfood/ui_run.md`, "UI Findings / Concerns."

---

## 5. Launch-Readiness Call

### Summary

The engine is **semantically correct** at 2k scale (all matcher rule types validated, oracle exact, why panels accurate). The path to v0.1 is blocked by two engine design walls and two UI defects that make the product non-functional in realistic use. The vector-scale problem requires an explicit strategy decision from the owner.

### MUST-FIX before v0.1 tag

| # | Item | Evidence | Action |
|---|---|---|---|
| M1 | **Streaming / capped desired-set construction in `create_rule`** | Cartesian backfill at 100k projected 489–685 min / 331 GiB. `create_rule` currently builds the full `BTreeMap` before applying `max_edges`. Low-selectivity rules (industry, specialty, size) cannot be used at production scale without this change. | Engine plan: apply cap during iteration; emit edges in scoring order and stop at `max_edges` without materializing the full desired set. |
| M2 | **Batch ingest in Python bindings** (and possibly a bulk HTTP path) | 100k ingest: 8.92 min with one WAL fsync per `insert_node` call. No `batch()` or `ingest_json` API exists. This cost dominates any fresh-load or reindex workflow at marketplace scale. | Bindings plan: expose a `batch()` call that groups inserts into a single WAL frame; optionally add a `POST /ingest` bulk endpoint to the HTTP server. |
| M3 | **UI F1 — Rules/Console z-index fix** | Run button is inaccessible whenever the Rules panel is open. This makes the panel combination non-functional in the primary debug workflow. | UI plan: fix `.rules` aside CSS `z-index` / `pointer-events` so it does not intercept the console toolbar. |
| M4 | **UI F2 — Dense hub add-to-canvas block** | Depth-1 auto-expansion of a 400-edge hub blocks UI > 5 s. At marketplace density this would be the default behavior for any company node. | UI plan: cap auto-expand depth at 0 (add the root node only) or paginate the expansion; expose "expand" as an explicit user action for dense nodes. |

### Vector-scale strategy (owner decision required)

The 5k ScanAll probe extrapolates to **80.3 hours at 100k** — `semantic_match` cannot ship at production scale in its current O(n²) ScanAll form. Three options with the available data:

| Option | Mechanism | What it buys | What it costs |
|---|---|---|---|
| **A — Norm-bound pre-reject** | Cache per-node L2 norms at ingest; reject a Talent×Company pair before cosine if `norm(t) * norm(c) * threshold > 1` is provably unreachable (i.e., upper-bound cosine < 0.85) | No external dependency; pure Rust; reduces pair evaluations for non-unit-norm embeddings | Probe embeddings are L2-normalized (norm=1.0 per `test_fixtures.py`); benefit is zero for unit-norm vectors. Not useful for current synthesized data; benefit depends on real-vector distribution. |
| **B — ANN index opt-in (recommended)** | Expose an `approximate_neighbors(embedding, k, threshold)` path backed by an ANN index (e.g., HNSW); `semantic_match` uses it instead of ScanAll | Reduces vector search from O(n²) to O(n log n) typical; well-understood tradeoff (recall vs speed). 80.3 h → O(minutes) at 100k with k~200 and recall~0.95. | Adds an ANN library dependency; index build time at ingest; approximate (not exact) recall. |
| **C — Document dims/size limits as known limitation** | Ship semantic_match with ScanAll; document maximum supported scale in the release notes | Zero engine change | `semantic_match` is unusable at >~5k nodes. The marketplace use case requires it at 100k+. |

**Recommendation:** Option B (ANN opt-in). Option A is ineffective for unit-norm embeddings (which is the current and likely production case). Option C makes `semantic_match` a non-functional feature at the target scale. However, the timing of the ANN work vs v0.1 is the owner's call — if Option C (document and defer) is acceptable to ship a v0.1 without `semantic_match` at 100k, it is the least-risk path for the tag; Option B then lands in the first post-v0.1 engine plan.

**This decision belongs to Matthew.** The data is: 5k ScanAll = 12.05 min, projected 100k = 80.3 h, neither under-30-min nor under-8-GiB. Source: `dogfood/results/scale-100k.md`, "Semantic verdict."

### Known limitations (ship as docs, not blockers)

| Item | Notes |
|---|---|
| Reopen 26.87 s (WAL replay) | Expected for a 1.60 GiB WAL; `snapshot()` not yet in bindings. Document cold-start time. |
| 'both'-industry 0.8 score unvalidated | Composition branch absent from fixtures; no production evidence it fires. Document as unverified. |
| GEO / STYLE rule composition coverage | Validated only via synthesized data; no real lat/lon or design_styles fixtures. |
| Canvas degradation >1,000 nodes | 589 ms per zoom action at 1k nodes under swiftshader. Document recommended viewport node limit (~200–400 nodes for smooth GPU interaction). |
| Cypher lacks COUNT | Use `neighbors` per src key for edge-count queries. Document Cypher surface gaps. |
| Bindings missing `ingest_json` / `auto-FK` / `stats` / `snapshot` | Auto-FK works via explicit `KeyMatch` rules; others absent. Document the exposed surface. |

---

## Addendum — Plan 11 post-run (2026-08-20)

Plan 11 (T1–T4) shipped four targeted engine + bindings changes. The dogfood re-run at the same 100k scale (seed=20260819) confirmed:

| Blocker (pre-Plan-11) | Resolution | Residual |
|---|---|---|
| **M1 Cartesian backfill OOM** — projected 489–685 min / 331 GiB; not attempted | **T1 streaming backfill: 19.742 s, peak 3.61 GiB. Wall fell.** Engine streams desired set directly into store; `max_edges` cap applied during iteration — no full `BTreeMap`. | Uncapped rules remain O(pairs) by definition — the cap is the mechanism. Every high-fanout rule instance must carry explicit `max_edges`. |
| **M2 No batch ingest** — 8.92 min (1 WAL fsync/node) | **T2 `ingest_batch`: 1.35 min (6.6x).** Bindings now expose `ingest_batch`, `stats`, `snapshot`. 10k-node chunks = one WAL frame each. | **Cold-start scales with rule recompute (Roadmap #1, High).** Derived edges are NOT persisted in WAL or snapshot. Every `open()` re-fires all rules from node data. WAL delta was only 120 MiB (node inserts + rule declarations — not edge data). Reopen 7.91 min dominated by IVF-Flat re-derivation (~7.68 min), not WAL I/O. Snapshot reopen also 7.58 min — same bottleneck. Required future work: derived-edge persistence / snapshot-including-derived. |
| **Semantic ScanAll at 100k (80.3 h projected)** | **T3 Cauchy-Schwarz early-exit: 41.7x probe speedup** (5k probe: 17.315 s was 12.05 min). Extrapolated 100k exact: 115.43 min. **T4 `approximate=True` IVF-Flat** adds opt-in approximate path. Per-query ANN recall (uncapped 5k probe): **0.991** — IVF quality is high. | Exact semantic at 100k still over 30-min budget. Set-coverage recall at 100k = 0.080, mechanically bounded by cap/total_positives ≈ 3% — NOT a measure of IVF quality and NOT comparable to the ≥0.90 spec floor (which is per-query recall). |
| **Big-3 unanswered** — no matcher edges at 100k | **Big-3 metro/industry slice: p50=581 µs, mean_matches=500** (500T×500C, all 3 rules fire, no cap needed at 250k pairs. The slice is fully-connected by construction (every talent matches every company under all three rules), so this measures derived-edge traversal latency, not matcher selectivity.). Answers the 5-second complaint in a focused bucket. | Full-graph Big-3 = empty intersection (1M cap at 70k×20k = 0.07% coverage). Awaits derived-edge persistence (Roadmap #1) to remove the cold-start penalty that blocks raising caps. |

### What the numbers do and do not prove

- **Matcher backfill at 100k is viable** with caps. 9-rule 19.742 s is a fair comparison.
- **Big-3 slice (p50=581 µs, mean=500 matches)** answers "can the engine serve a talent match query in reasonable time?" — yes, in a focused metro+industry bucket. Full-graph coverage is a cap/persistence problem, not an engine speed problem.
- **Per-query ANN recall = 0.991** (IVF-Flat, uncapped 5k probe). Set-coverage recall = 0.080 is cap mathematics, not IVF quality. Do not cite 0.080 against the ≥0.90 spec floor.
- **Cold-start is the new top finding.** Every open() re-derives all rules. Adding a second VectorSimilar rule doubles cold-start time. This is the roadmap blocker for prod deployment.
- **Incremental p50 rose** (15.50 ms vs 3.48 ms) proportionally to rules-per-event — expected.

Source: `dogfood/results/scale-100k.md`, "Run 2 (post-Plan-11)."

---

## Addendum — Plan 12 post-run (2026-08-20)

> **Update to the 7.91-min cold-start figure (Plan-11 Addendum, "Cold-start is the new top finding").**
> The Plan-11 Addendum reported a 7.91-min WAL-only reopen and a nearly identical snapshot reopen
> (7.58 min) because snapshot() did not persist derived edges. Plan-12 T4 implemented V4 snapshot
> format: derived edges + IVF centroids are persisted in the snapshot file.
> The old `dogfood/results/scale-100000-db` (pre-Plan-11 bincode) was unreadable; it was deleted
> and rebuilt fresh via `dogfood/scale_run.py` on 2026-08-20 (seed=20260819).

### 100k cold-start: WAL-only vs. V4 snapshot

| Path | Wall | Notes |
|---|---|---|
| WAL-only (no snapshot) | **8.86 min** | Replays WAL; re-fires all 12 rules from node data. Non-semantic rules complete in ~21 s (T1 streaming). IVF-Flat re-derivation dominates (~8.37 min). Similar to Plan-11 7.91 min — IVF-Flat still the bottleneck. |
| Snapshot V4 (T4 Plan-12) | **11.15 s** | snapshot() (36.1 s write, one-time cost) then open() loads derived edges + IVF centroids. No rule re-fire. **47.7× improvement** over WAL-only. |

### Interpretation

- **The 7.91-min figure is superseded.** WAL-only is now 8.86 min (slightly slower: more rules
  declared in this run than Plan-11). Snapshot V4 brings it to **11 seconds**.
- **snapshot() write cost: 36.1 s.** This is a one-time cost at graceful shutdown. Cold restart
  from V4 snapshot then takes 11 s.
- **Non-semantic backfill improved:** 9 rules in 21.2 s (T1 streaming, vs projected 489 min
  pre-Plan-11). Only IVF-Flat re-derivation (8.37 min) now dominates WAL reopen.
- **For the WAL-only path**, the bottleneck is IVF-Flat ANN index rebuild (~8 min at 100k × 1536 dim).
  The V4 snapshot path eliminates this entirely. Production deployment should use `snapshot()` on
  graceful shutdown to avoid the WAL-only penalty.

Source: rebuilt `dogfood/results/scale-100k.md` (2026-08-20); `dogfood/results/scale-100000-db`.
