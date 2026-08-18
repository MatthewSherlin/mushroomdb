# Launch GIF click-path

Record against `graphdb serve --ui` (the built dist), not `vite` dev.
Verified 2026-08-18 on `http://127.0.0.1:8080/` after the three-command
flow in the README.

## Setup

```text
graphdb demo ./db
cd ui && npm ci && npm run build
graphdb serve ./db --ui ui/dist --addr 127.0.0.1:8080
```

Open `http://127.0.0.1:8080/`. Wordmark status dot goes `--signal`
(connected) after the `/watch` ack. Watch URL is
`ws://127.0.0.1:8080/watch` (same origin — no Vite proxy).

Empty canvas: "Open a node to start" + "Load demo neighborhood".
Activity ticker: "no events".

## Do not start on the empty-state button

"Load demo neighborhood" runs `MATCH (n) RETURN n LIMIT 1`. That key
is **org-01**, not person-01. The org-01 root chip stays unlabeled
(neighborhood rows omit the root); neighbors are 3 Person + 2 Project.

Query `person-01` explicitly.

## Click-path

1. **Confirm live.** Status dot connected. Leave Explore selected.

2. **Open person-01 via the console.** Rail → Console. Replace the
   starter (`MATCH (n) RETURN n LIMIT 25`) with the demo FIT query:

   ```
   MATCH (p:Person {id: 'person-01'})-[r:FIT]->(proj:Project)
   RETURN p, proj, r.score AS score
   ORDER BY score DESC, proj
   ```

   Click **Run** (or Cmd/Ctrl-Enter). Table (verified):

   | p | proj | score |
   |---|---|---|
   | person-01 | proj-01 | 1 |
   | person-01 | proj-02 | 0.5 |
   | person-01 | proj-20 | 0.5 |

3. **Add to canvas.** Click **Add to canvas**. Gold derived edges
   appear (FIT + auto-FK). Label chips show type (`Person` / `Project`
   / `Org`), not keys — hover a node for the mono hover card
   (`person-01`, `Person`, `N props`).

4. **Watch scores, then click a gold edge.** Click the gold link
   between person-01 and proj-01 (not a structure-gray user edge —
   the demo neighborhood is all derived). Why panel (verified):

   ```
   skill_fit
   FIT · 1
   person-01 → proj-01
   overlap(skills) = |{s01, s02, s03}| / |{s01, s02, s03}| = 1
   ```

   Shared tokens `s01 s02 s03` are gold-highlighted on both ends.

   Optional second click — gold person-01 → proj-02 (score 0.5):

   ```
   overlap(skills) = |{s02, s03}| / |{s01, s02, s03, s04}| = 0.5
   ```

   Optional key-match — gold person-01 → org-01 (or Rules →
   `auto_fk_person_org_id`):

   ```
   auto_fk_person_org_id
   ORG
   person-01 → org-01
   person.org_id = "org-01" → org-01
   ```

   Rules → `skill_fit` opens the same why card on the first visible
   FIT (person-01 → proj-01) if GPU pick is awkward.

5. **Live insert from a second terminal** (keep the canvas on
   person-01 / proj-01):

   ```bash
   curl -sS -X POST http://127.0.0.1:8080/ingest \
     -H 'content-type: application/json' \
     -d '{"label":"Person","rows":[{"id":"person-96","name":"Person 96","org_id":"org-01","project_id":"proj-01","skills":["s01","s02","s03"]}]}'
   ```

   Verified on the served origin:

   - ticker: `node inserted person-96` then `ingested Person 1`
   - status dot gold-flashes (~280ms) then connected
   - canvas resyncs; chips grew 14 → 41 (Person 22 / Org 7 / Project 12)
   - new gold derived edges (FIT + auto-FK onto person-96) pulse
     wider for ~600ms (`prefers-reduced-motion` disables the pulse)

Hold the last frame on the gold glow, then cut.

## Timing notes

- Wait for the status dot before any click (~1s after load).
- After **Add to canvas**, wait for the neighborhood + explain burst
  (~1s) before clicking an edge.
- After the curl, the glow is 600ms — do not cut away early.
- Ingest frames are `node_inserted` + `ingested` only (no per-rule
  `edge_inserted`); the glow is the resync derived-edge diff.
