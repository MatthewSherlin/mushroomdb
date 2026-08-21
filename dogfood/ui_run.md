# mushroomdb UI at Scale — Dogfood Run

## Machine / Run Header

| Field | Value |
|---|---|
| Date | 2026-08-19 |
| Host | mac.lan |
| OS | macOS 15.7.3 arm64 (Apple M4 Pro, 12 cores) |
| RAM | 24 GiB |
| Server binary | `target/release/mushroomdb` |
| UI | `ui/dist` (production build) |
| Driver | playwright/chromium headless (swiftshader WebGL) via `ui/node_modules` |

## Database Split

| DB | Path | Scale | Rules live |
|---|---|---|---|
| 100k | `dogfood/results/scale-100000-db` | 100,500 nodes, 10,500 edges | FK-only: `auto_fk_talent_user_id` (500 edges), `auto_fk_job_company_id` (10,000 edges) |
| 2k | `dogfood/results/scale-2000-db` | 2,500 nodes, 1,228,422 edges | All 12 rules: FK + 4 matcher types + semantic |

**Caveat (inherited from T3):** Matcher rules (industry_alignment, specialty_match, location_fit, similar_size, matches_design_style) and semantic_match were NOT declared at 100k scale — the 5k×400 cartesian probe extrapolated wall ≈ 489 min and Δrss ≈ 331 GiB (both exceed budget). The 100k derived-edge mesh is FK-only. Protocol legs that require matcher/semantic why-panels ran against the 2k db instead; this split is documented per leg below.

---

## Timings Table

| Leg | Database | Metric | Value | Verdict |
|---|---|---|---|---|
| 1a | 100k | Hub depth-1 expand (API round-trips, in-browser) | **2.1 ms** | Smooth |
| 1b | 100k | Hub depth-2 expand (API round-trips, in-browser) | **1.6 ms** | Smooth |
| 3a | 100k | Console MATCH (t:Talent) LIMIT 100 | **21 ms** | Smooth |
| 3b | 100k | Add to canvas: 100 query results → 200 rendered nodes | **1,138 ms** | Acceptable |
| 3c | 100k | Console MATCH (t:Talent) LIMIT 500 | **35 ms** | Smooth |
| 3d | 100k | Add to canvas: 500 query results → 1,000 rendered nodes | **1,965 ms** | Marginal |
| 4 | 100k | Live ingest 50 nodes (POST /ingest) | **75 ms** | Smooth |
| 4 (WS) | 100k | Ticker update after ingest (WS event) | confirmed | Smooth |
| 5a | 100k | Zoom interaction at ~200 visible nodes (3 clicks + rAF) | **308 ms** ¹ | Acceptable |
| 5b | 100k | Zoom interaction at ~1,000 visible nodes (3 clicks + rAF) | **589 ms** ¹ | Degraded |
| 2a | 2k | Why panel: INDUSTRY_ALIGNMENT | rendered correctly | Smooth |
| 2b | 2k | Why panel: SPECIALTY_MATCH | rendered correctly | Smooth |
| 2c | 2k | Why panel: LOCATION_FIT | rendered correctly | Smooth |
| 2d | 2k | Why panel: SEMANTIC_MATCH | rendered correctly | Smooth |

¹ swiftshader (software WebGL, headless Chromium) — worst-case lower bound; real-device GPU performance will be faster.

---

## Leg 1 — Hub Expansion (100k DB, FK-only mesh)

**Hub node:** `talent-000000`  
**Graph structure at 100k:** each Talent node has at most 1 derived edge (to its corresponding User node, if user_id resolves within the 500 seeded User nodes). Job nodes have 1 COMPANY edge. No inter-company or inter-talent matcher/semantic edges exist.

The depth-1 expand protocol (2 parallel neighborhood calls + /edges + neighbor node-info fetch) completes in **2.1 ms** in-browser. Depth-2 expand (single neighborhood?depth=2 + hop-1 sub-expansions) completes in **1.6 ms** — the sparse FK graph means depth-2 yields the same single hop-1 node as depth-1 with no further expansion.

**Finding:** API latency at 100k for FK-only neighborhood traversal is excellent (sub-3 ms). These numbers do not represent matcher/semantic hub density — a high-fanout hub with hundreds of INDUSTRY_ALIGNMENT edges would have significantly more I/O. The 2k db expansion is covered under Leg 2 indirectly (company nodes with 400+ incoming edges caused the "Add to canvas" button to be busy for > 5 seconds — see UI concern below).

---

## Leg 2 — Why Panels (2k DB, all rules live)

All four why-panel types rendered correctly with representative matching arithmetic:

| Rule | Edge Type | Weight | Why Line |
|---|---|---|---|
| `industry_alignment_tc` | INDUSTRY_ALIGNMENT | 1.0 | `field_equal(industry): architecture = architecture` |
| `specialty_match_tc` | SPECIALTY_MATCH | 1.0 | `overlap(specialties) = |{single-family}| / |{single-family}| = 1` |
| `location_fit_tc` | LOCATION_FIT | 0.831 | `geo_radius(location) = 27.2 km ≤ 160.9 km` |
| `semantic_match_tc` | SEMANTIC_MATCH | 0.998 | `vector_similar(embedding) = cos ≈ 0.998 ≥ 0.85` |

The 2k db has 12 rules, 1,228,422 derived edges. The rules panel loads all 12 entries in ~1.5 s from /stats. Clicking a rule opens the why panel for the first matching edge on canvas in < 1 s. Arithmetic display is correct for all predicate types (field_equal, overlap, geo_radius, vector_similar).

**Screenshots:** `t4-why-industry.png`, `t4-why-specialty.png`, `t4-why-location.png`, `t4-why-semantic.png`

The semantic why panel (`t4-why-semantic.png`) shows a Talent→Company pairing (`talent-000000 → company-000013`); this is expected — `semantic_match_tc` is a Talent-to-Company rule (TC suffix) that fires when cosine similarity of the two nodes' embedding vectors meets the 0.85 threshold.

---

## Leg 3 — Console Query + Add to Canvas (100k DB)

| Step | Time | Notes |
|---|---|---|
| `MATCH (t:Talent) RETURN t LIMIT 100` | 21 ms | JSON response fast |
| `MATCH (t:Talent) RETURN t LIMIT 500` | 35 ms | Slight overhead from larger result set |
| Add 100 query results to canvas | 1,138 ms | Actual 200 nodes rendered: `addHarvestedToCanvas` calls `expandNode(depth=1)` per result key, adding user-neighbor nodes |
| Add 500 query results to canvas | 1,965 ms | Actual 1,000 nodes rendered (same expansion behavior) |

**Design note:** `addHarvestedToCanvas` in `query-result.ts` always expands each returned node at depth 1 (fetching its neighborhood and edges). For the 100k FK-only graph, this doubles node count on canvas (100 Talent + 100 User). This is by design but creates a non-obvious UX where "add 100 nodes" results in 200 visible. At high-fanout scale with dense matcher edges, this could add O(N × degree) nodes.

---

## Leg 4 — Live Ingest (100k DB)

- POST /ingest with 50 Talent nodes (novel IDs): **75 ms**, 200 OK
- Server response: `{"inserted":50,"edges_inserted":0,"skipped_fk_fields":[{"field":"user_id","reason":"no matching target keys"}]}`
- WS ticker updated: `"ingested Talent 50"` (received within 1 s)
- Stats after: `nodes_live = 100,550`
- `edges_inserted = 0` because the ingested nodes' `user_id` values ("user-ud-{i}") do not match any seeded User nodes — FK rule fires but finds no targets. This is correct behavior.
- **Glow behavior:** the ingested nodes were not present on the canvas at ingest time (5 pre-existing Talent nodes were visible; the 50 new nodes had novel IDs not on canvas). The WS event was received and the ticker updated ("ingested Talent 50"), but no glow animation fired — glow only schedules on *born* derived edges for nodes *already on canvas* (`bornEdgeIds` diff). With no canvas overlap, glow is not expected. The glow path would require ingesting a node whose neighbor is already visible.

**Verdict:** Live ingest pipeline is smooth. WS event delivery and ticker update work at 100k base-graph size. Glow is correctly suppressed when ingested nodes have no canvas presence.

---

## Leg 5 — Canvas Scale (100k DB)

| Node count (rendered) | Add latency | Zoom interaction (3 clicks + rAF) | Frame feel |
|---|---|---|---|
| 200 (from LIMIT 100 + expansion) | 1,138 ms | 308 ms | Acceptable |
| 1,000 (from LIMIT 500 + expansion) | 1,965 ms | 589 ms | Degraded |

At 200 rendered nodes, the cosmos.gl canvas is interactive with slight lag. At 1,000 rendered nodes, zoom round-trips are ~590 ms (3 actions measured via rAF fences). The 2× latency increase from 200→1000 nodes tracks the 5× node count increase sub-linearly, but the absolute 590 ms per-action is noticeable.

These measurements use swiftshader (software WebGL in headless Chromium). Real-device GPU performance will be faster; swiftshader is a worst-case lower bound.

**Degradation point:** interaction latency is still "acceptable but sluggish" at 1,000 nodes under swiftshader. Smooth GPU threshold is likely around 200–400 visible nodes (estimated from the sub-linearity). No hard freeze was observed at either scale.

**Screenshots:** `t4-100k-canvas-100nodes.png` (200 rendered), `t4-100k-canvas-500nodes.png` (1,000 rendered)

---

## UI Findings / Concerns

| # | Finding | Severity | Notes |
|---|---|---|---|
| F1 | Rules panel overlaps console: clicking "Run" in console fails if Rules panel is open | Medium | The `.rules` aside panel uses `pointer-events` intercepting `.console-btn[Run]`. UI layout bug — both panels stack in the same z-layer. Confirmed in playwright driver (30 s timeout before workaround). Repro: open both Console and Rules simultaneously. **Visual evidence:** `t4-2k-error.png` captures this state — the Rules panel is open on the left (all 12 rules visible), the console is visible at the bottom with a partial query (`...90002'}) RETURN c`), and the Run button is inaccessible because the rules panel's pointer-event region covers it. |
| F2 | Add-to-canvas for dense company nodes blocks UI for > 5 s | Medium | `addHarvestedToCanvas` calls `expandNode(depth=1)` for each added node. A company node with 400+ incoming INDUSTRY_ALIGNMENT edges triggers 400+ parallel neighborhood fetches. In the 2k db, adding company-000000 (400+ talent neighbors) left the "Add to canvas" button disabled for > 5 s. Not tested at 100k density. |
| F3 | Ticker shows "no events" until first WS event after page load | Low | Expected behavior but visible gap between page-load "connected" state and first ticker line. |
| F4 | label-chip count is 2× query LIMIT at 100k (FK-only graph) | Low / Informational | `addHarvestedToCanvas` depth-1 expansion doubles visible nodes. Expected by design; potentially surprising with dense matcher nodes where each talent-→company expansion could add hundreds of nodes. |
| F5 | Zoom interaction degrades ~2× from 200→1000 rendered nodes (swiftshader) | Medium | 308 ms at 200 nodes, 589 ms at 1,000. Interaction remains functional but noticeable under software renderer. GPU performance expected to be significantly better. |

---

## Smoothness Verdicts

| Scale / Scenario | Verdict |
|---|---|
| Hub depth-1 expand (FK-only, 1 neighbor) | **Smooth** |
| Hub depth-2 expand (FK-only, sparse) | **Smooth** |
| Console query (LIMIT 100–500) | **Smooth** |
| Canvas with 200 rendered nodes (zoom interaction) | **Acceptable** |
| Canvas with 1,000 rendered nodes (zoom interaction) | **Degraded** (590 ms / action, swiftshader) |
| Live ingest 50 nodes + WS ticker | **Smooth** |
| Why panels — all 4 rule types (2k db) | **Smooth + correct arithmetic** |
| Concurrent Rules + Console panel usage | **Broken** (pointer-event z-index conflict, F1) |
| Dense hub expansion (company with 400+ edges, 2k db) | **Blocked** (UI busy > 5 s, F2) |

---

## Screenshots (13 total, captured during the run; not committed)

| File | Description |
|---|---|
| `t4-100k-initial.png` | 100k db — initial empty-canvas state |
| `t4-100k-canvas-100nodes.png` | 100k db — 200 nodes rendered (LIMIT 100 + depth-1 expand) |
| `t4-100k-canvas-500nodes.png` | 100k db — 1,000 nodes rendered (LIMIT 500 + depth-1 expand) |
| `t4-100k-live-ingest.png` | 100k db — after live ingest of 50 nodes, ticker shows "ingested Talent 50" |
| `t4-2k-initial.png` | 2k db — initial empty-canvas state |
| `t4-2k-canvas-prep.png` | 2k db — canvas with talent-000000 + 3 company neighbors (429 chips after full depth-1 expansion) |
| `t4-2k-error.png` | 2k db — **F1 evidence**: Rules panel open left, console open bottom, Run button intercepted by rules panel pointer-events region |
| `t4-2k-rules-panel.png` | 2k db — Rules panel listing all 12 rules (first driver run) |
| `t4-2k-rules-panel-2.png` | 2k db — Rules panel listing all 12 rules (leg-2 focused run) |
| `t4-why-industry.png` | 2k db — why panel: industry_alignment_tc (field_equal) |
| `t4-why-specialty.png` | 2k db — why panel: specialty_match_tc (overlap) |
| `t4-why-location.png` | 2k db — why panel: location_fit_tc (geo_radius) |
| `t4-why-semantic.png` | 2k db — why panel: semantic_match_tc (vector_similar, T→C pairing) |
