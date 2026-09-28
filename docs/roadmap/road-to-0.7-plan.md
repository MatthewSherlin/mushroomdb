# Road to 0.7 — a survey of `main` at `0aee864`, and what to do next

> **Status, 2026-09-28.** Items 1, 2, 3, 4, 5, 10a and 10b are **FIXED** in the
> working tree, uncommitted, with tests that fail without each fix. Item 9 gained
> a fix to `check-claims.sh` (it walked gitignored trees and failed locally for
> anyone who had done SDD work, while CI saw a clean checkout and passed).
> Item 6 is **FIXED** too, by route (a): the memory surface now advertises
> nineteen tools, `upsert_entity`, `ingest_json` and `create_rule` having joined
> it after the readers.
>
> Verified: 2,495 workspace tests + 13 multiprocess tests pass, `clippy -D
> warnings` clean on both feature sets, `fmt` clean, and `check-claims`,
> `check-defect-ledger`, `check-pyi` and `render-plugin --check` all OK. A real
> stdio handshake against an empty store returns all nineteen.

**Written 2026-09-28.** Every `file:line` below was read at `0aee864`. Where a claim
rests on a probe rather than on source, it says so.

v0.6.11 is merged and unreleased. **It should not be tagged as it stands** — its
headline feature, date-addressable history, resolves dates into the wrong index
space and answers with an older graph rather than an error. Fixing that is also
the one thing standing between this project and the first pass of its own
pre-registered gate.

| | |
|---|---|
| workspace version | 0.6.11 |
| latest tag | v0.6.10 |
| unreleased commits | 23 |
| defect ledger | 35 closed, row 36 open |
| association gate | FAILED on 2 legs (correctness) |
| cost leg | −41.2%, interval entirely below zero — **passing for the first time** |

---

## P0 — before v0.6.11 is tagged

### 1. A date resolves into the wrong index space — BLOCKER, CONFIRMED

**Reproduced 2026-09-28 with the shipped `mushroomdb 0.6.11` release binary on a
store created seconds earlier. No benchmark, no harness, under a minute:**

```
$ mushroomdb demo /tmp/demo
$ mushroomdb stats /tmp/demo
nodes: 60 live · edges: 334 · rules: 7

$ mushroomdb asof /tmp/demo --at 2026-09-28 --query "MATCH (a)-[r]->(b) RETURN count(r)"
as-of commit 9 of 16
  COUNT(r)=220                     <-- today's date, 220 of 334 edges

$ mushroomdb query /tmp/demo "MATCH (a)-[r]->(b) RETURN count(r)"
  COUNT(r)=334
```

A store written moments ago, asked for **today**, returns **34% fewer edges than
it holds**. 16 WAL frames, 10 recorded stamps — the newest six frames are
unreachable by any date, so a date query can never see the current graph. Put this
in the bug report; it needs nothing but the demo command.

The same failure on the association world, via the shipped CLI: `--at 2026-07-14`
prints `as-of commit 167 of 588` and returns 7 keys; `--commit 317` (that day's
real frame) returns all 8. That is `assoc-timetravel-4` exactly, reproduced with
no harness involved.


`stamp_commit_time` records `frame_index = seq - 1` (`crates/core-api/src/db.rs:4952`),
which is a WAL frame index only if one commit is exactly one frame. It is not:
`db.rs:5295` increments `commit_seq` once and stamps the time, then `db.rs:5341`
appends a **second frame** — the HISTORY-MARKER carrying rule attribution — for
that same commit. Every rule-deriving write produces two frames and one stamp.

So `resolve_date` returns an index counted in commits, while `edges_at`,
`was_linked`, both history readouts, `asof --at`, HTTP `at_commit`, the Python
binding and the MCP tools all consume it as a WAL frame index. The gap grows with
history, so a date drifts further into the past the later you ask. **It fails
silently and conservatively** — a plausible older graph, never an error.

Measured on the association benchmark world: **588 WAL frames against 311 recorded
stamps** (≈1.89 frames per commit — exactly one marker per deriving commit).
`2026-07-14` resolves to 167; that day's state is at frame 317, 23 simulated days
later.

```
WAL frames   [0 data][1 marker][2 data][3 marker][4 data][5 marker]
commit          A                 B                 C
stamped as      0  ok             1  wrong (is 2)   2  wrong (is 4)
```

Commit A stamps correctly by coincidence; every commit after it is recorded one
frame short per marker already written.

**Only stores with linking rules are affected** — which is the configuration
mushroomdb exists for, and is why the 18 `commit_times` unit tests pass: they
exercise stores whose commits fire nothing.

**Do:**
1. Stamp the true frame index instead of deriving one from `commit_seq` at
   `db.rs:4952`. The primary frame's index is knowable at `db.rs:5241` (the count
   of frames appended so far).
2. Existing sidecars hold wrong values and cannot be repaired in place. Bump
   `COMMIT_TIMES_VERSION` — the format already carries one — and treat older files
   as unusable rather than reinterpreting them. A store that silently answers dates
   from a v1 sidecar is the bug shipping twice.
3. Correct `docs/site/timetravel.md:83`, which documents the two as one space.
4. Add the test the subsystem never had: on a store whose commits derive edges,
   assert `resolve_date(t)` equals the frame `edges_at` needs — and assert a date
   of *now* reaches the newest commit.
5. Rewrite `CHANGELOG.md:26-31`, which attributes the gate failure to a generator
   /engine modelling disagreement and states "the resolved commit is the correct
   one for the date". Both halves are false.

### 2. A v0.6.11 tag would publish a Python wheel labelled 0.6.10 — BLOCKER

`bindings/python/pyproject.toml:7` is still `0.6.10`, and maturin reads it. The npm
launcher and TypeScript client rewrite their version from the git tag;
**pyproject does not self-heal**. `.well-known/mcp/server-card.json:5` is also
0.6.10 — only the card's tool array is contract-tested, so its version drifts
unguarded.

**Do:** bump both; add the version to the server-card contract test.

### 3. The changelog states a measurement no source supports

`CHANGELOG.md:94` reports the full-text open as **476 ms → 227 ms**. The spec says
**456 ms** in three places (`docs/roadmap/v0.6.11-time-and-vectors-spec.md:440`,
`:458`, `:473`), as does the implementing commit.

### 4. The front page cites a benchmark run two weeks stale

`README.md:445-448` and `docs/site/association-bench.md:140-170` still report the
11 Sep run — 0.967 at $0.0923, "the gate failed on the cost interval". The newest
committed run is `benchmarks/agent-tasks/results/20260925T200950Z/`: **0.994 at
$0.0357**, cost interval now passing, failing on correctness instead. The README
sells a weaker result than the one in the repo and attributes the failure to the
wrong leg.

### 5. Row 36 of the defect ledger is stale

Its body still reads "deferred to 0.6.11 — bisection is the first task if this is
picked up", but 0.6.11 did the measurement and half the fix. The row's own closing
note is the accurate one: 227 ms is halved, not solved, and the real fix is
persisting the full-text index as a snapshot section — a format change
deliberately excluded. Nothing currently tracks the residual.

---

## P1 — v0.6.12: the server patch, and the first minute

The v0.6.11 spec (`:224-234`) already named 0.6.12 and its scope, deliberately
keeping a security-shaped patch out of a release that moved the vector format. It
is the only named release with no plan document. One thing belongs in it that the
spec does not yet name.

### 6. The skill's first instruction names tools the agent cannot call — FIRST-RUN

`crates/cli/skills/mushroom/SKILL.md:14` — the first thing `/mushroom` says to do
with an empty store — offers `ingest_json` for a batch and `upsert_entity` for
one. Neither is in `ASSOCIATION_TOOLS` (`crates/server/src/mcp.rs:1194-1215`), the
sixteen a memory store advertises, and `install` never writes `--all-tools`. The
server serves them; the host builds its tool set from `tools/list`, so the model
never sees them. The learn pass (`SKILL.md:67`) writes concepts with `ingest_json`
too, and the advanced block proposes `create_rule` — same problem.

This is the wall a user hits *after* a clean install, on the emptiest store, which
is every new user's first store. Nothing in CI catches it: the handshake asserts
the sixteen are listed, never that the skill's names are among them.

**Do:** decide whether those three belong on the memory surface or whether the
skill should reach them another way; then add the test — every tool name the skill
mentions must appear in the listing that skill ships with.

> **Fixed 2026-09-28, by route (a).** `upsert_entity`, `ingest_json` and
> `create_rule` joined `ASSOCIATION_TOOLS`, after the readers and before
> `stats` — listing order is ranking, and the questions stay the point. The
> count moved from sixteen to nineteen across the README, `mcp.md`,
> `quickstart.md`, `skill.md`, `index.html`, `llms.txt`, the server card, the
> skill and its Cursor variant, `ci.yml`'s `--expect-tools`, the doctor
> handshake assertion, and every test pinning the listing; `llms-full.txt` and
> the plugin were regenerated.
>
> The durable part is the new binding test: **every tool name the shipped
> `SKILL.md` mentions must appear in the listing a memory store actually
> returns.** It reads a real `tools/list` rather than a constant the test
> declares — the first version of it compared against the test file's own copy
> of the list and passed with the bug reintroduced, which is exactly the way
> this class of test fails silently. Reverting the three additions now makes it
> fail, naming all three.

### 7. The HTTP server has no timeout, no limits, and contradicts itself

- Unauthenticated `GET /health` returns live node and edge counts
  (`crates/server/src/http.rs:471`) while `/metrics` refuses a role-bound token on
  the stated grounds that counters leak graph size (`http.rs:1060`).
- `tower-http` is pulled with the `fs` feature only — no request timeout, rate
  limit or concurrency limit anywhere. The only defence is a 64 MiB body cap.
- SIGTERM calls `serve.abort()` (`crates/cli/src/main.rs:734`), killing in-flight
  requests and live subscribers before the shutdown snapshot.
- No `tracing`, `log`, access log or request id in the tree.

**Do:** write the v0.6.12 plan and spec first. This is a security-shaped patch,
and the project's convention is that those get a reviewed design.

---

## P2 — what 0.7 needs before it can ship at all

### 8. Every inter-crate pin breaks at 0.7.0 — BLOCKER FOR 0.7

All twelve workspace path dependencies carry `version = "0.6.10"`. The caret range
still matches 0.6.11, so nothing is broken today — but `^0.6.10` does not match
`0.7.0`, and every `cargo publish` fails until all twelve are hand-bumped,
mid-release, across eight crates published in dependency order with thirty-second
waits between them.

**Do:** write the bump script that does not exist. Version currently lives
hand-edited in at least eight files; one script run before the tag retires items
2 and 8 permanently.

### 9. Three things ship without a CI job

- `clients/typescript` is **published on every release and tested by no
  workflow**; its own integration suite skips rather than fails when cargo is
  missing (`tests/global-setup.ts:11`).
- `packaging/tests/run.sh` — the only test of `install.sh` and the npm launcher,
  the two paths most users arrive through — runs in no workflow.
- `scripts/check-defect-ledger.sh` — the script that enforces every ledger row
  being closed or deferred, *and* that every row has a matching detail section —
  runs in **no workflow**. It is invoked by hand as "Task 12" of a release plan.
  (`check-pyi.sh` does run, transitively: `check-claims.sh:100` calls it and CI
  runs that at `ci.yml:393`.)
- The Miri job named in `.github/workflows/ci.yml:26-30` as the mitigation for
  five `rkyv::access_unchecked` sites on mmap'd sections is a TODO blocked on
  Miri's mmap support. The only real defence is an on-demand `mushroomdb verify`.
  The safety comment on those sites says plainly that a bit-flip on a relative
  pointer resolves out of bounds and is *genuine UB, not a panic*. There are also
  four raw-pointer RAII guards in `db.rs` that take `&mut self.field` as a pointer
  then call `&mut self` methods while it is live — exactly what Miri would catch.

### 10. Two more defects in the commit-times sidecar — both confirmed in source

An audit of `CommitTimes::push` (`crates/core-storage/src/commit_times.rs:117-126`,
which silently returns when `commit <= newest_commit()`) found that guard
**unreachable** — every candidate path is closed, because `commit_seq` is reseeded
monotonically from `last_change` on open and `reset()` zeroes the counter and the
map together. But tracing it surfaced two defects that are reachable.

**10a — multi-process writes clobber the sidecar. Silent and permanent.**
`refresh()`'s incremental branch (`db.rs:~2773-2815`) applies a peer's WAL tail and
raises `commit_seq` but **never reloads `commit_times`** — verified: zero mentions
of `commit_times` anywhere in that branch. Only the full-reload branch re-reads it.
So a process opened before a peer's first commit holds an empty in-memory map;
when it writes, `let first = self.commit_times.is_empty()` (`db.rs:4965`) is true,
so it takes `fs.write_atomic(FileId::CommitTimes, …)` (`:4968-4972`) and
**overwrites the entire sidecar with its one entry**, destroying every entry the
peer wrote. Nothing reports it. The non-empty variant is milder and still wrong:
the append is byte-correct while the in-memory map has a hole, so `resolve_instant`
on that handle answers an older commit than the truth.

This contradicts a README headline directly — *"a running `serve`, an editor hook,
a git hook and a CLI command can share one store."* They can share the store; they
cannot currently share its dates.

**10b — a truncating snapshot desyncs the map by N.** `snapshot()` with
`keep_wal: false` writes a baseline WAL (`db.rs:13758`, `:13893`) and
`wal_total_commits()` returns to 0. But `truncate_below` has **exactly one call
site** — `db.rs:13824`, inside the archive-retention prune branch — so a plain
truncating snapshot never calls it. The sidecar keeps entries `0..N-1` for frames
that no longer exist while the new WAL renumbers from 0, and the next write stamps
`frame_index = N` for what the history surface now calls frame 0.
`resolve_instant`'s floor check cannot catch it: `wal_horizon_floor` is only
assigned at open and in that same prune branch, so it is still 0.

**Coverage:** `crates/core-api/tests/commit_times.rs` has 18 tests and none cover
write-after-truncating-snapshot, retention pruning, or multi-process. Items 1, 10a
and 10b are all the same subsystem shipping ahead of its test surface — fix them as
one piece of work, not three.

### 11. Loose ends that make the repo contradict itself

- Licence inconsistent four ways: repo is MIT OR Apache-2.0, the plugin manifest
  says MIT, and the Python binding, sim-harness and core-bench say Apache-2.0.
- `CONTRIBUTING.md:139` still lists v0.4.1–v0.5.2 as the published tags.
- Two places still call the TypeScript client unpublished; it has shipped for
  months.
- Both LangChain and LlamaIndex integrations sit at v0.1.0, a month stale, in no
  CI job and in no publish workflow.
- Homebrew requires a hand-copied formula PR per tag and is absent from the
  README's install options.
- Seven roadmap plans for shipped releases are untracked in git.

---

## P3 — the levers that change what this project can claim

### 12. Pass the gate — it is one defect away

The pre-registered gate has never passed. On the 25 Sep run the cost leg passes
for the first time: **−41.2%, 95% interval [−0.0353, −0.0124], entirely below
zero**. The only failing legs are correctness, by **−0.006** — one key, on one
task, reproducibly across three reps. That task is `assoc-timetravel-4`, and that
key is item 1.

Measured directly at the day's real frame, the engine returns the full eight-key
truth set: the derivation is right and the date resolution is wrong. The other
three time-travel tasks scored 1.000 while also being answered 8–23 simulated days
early — their answer sets happen not to change across the offset.

**Fix item 1, re-run, and every leg plausibly passes.**

**Confirmed end-to-end** with the shipped 0.6.11 binary: `asof --at 2026-07-14`
resolves to commit 167 of 588 and returns 7 keys; `--commit 317` returns all 8.
See item 1.

Note also that `CHANGELOG.md:26-31` attributes the failure to "a disagreement
between the generator's model and the engine's incremental derivation", and says
"the resolved commit is the correct one for the date". If item 1 holds, that
sentence is wrong and needs rewriting.

### 12b. Two cheap guards that would have caught item 1 before it shipped

Worth doing whatever else happens, because this is the process gap rather than the
bug. The committed summary `results/20260925T200950Z/summary.md` was added by
commit `b93f176` (28 Sep) — the *same commit* that changed date resolution in
`db.rs`. The run itself executed at 16:09 EDT on 25 Sep, an hour **before**
`703e3a8` landed. So the release's headline numbers came from a binary two commits
behind what shipped, and the changelog's causal explanation for the one failing key
was written three days later from the summary, without anyone re-opening the store.

1. **Record the binary's commit SHA in every run summary, and fail the gate when it
   is not the release SHA.** The harness already records `subject HEAD` for the
   code suite — the same idea, pointed at the engine under test.
2. **Fill in the classification column before quoting a run.** It is blank for all
   three sub-1.0 cells in the 25 Sep summary. The previous run's
   `classification.md` is exactly the artifact that forces someone to open the
   store — and opening the store is how `as-of commit 167 of 588` becomes visible.

`check-claims.sh` is a good gate that checks the wrong axis here: it verifies no
file makes a retired *kind* of claim, and nothing verifies that a quoted number
matches a committed run or that a cited run is the newest one. Items 3 and 4 of
this plan both pass CI today.

### 13. Re-run the gate at the scale the plan specified

Task 9 of `docs/roadmap/v0.6.11-time-and-vectors-plan.md:923-937` specified
`--scale 30000`. The committed run carries the 2,000-entity digest, so it did not
happen, and `README.md:447` correctly qualifies the result as "on a 2,000-entity
world" — the honest caveat, and also the one a sceptical reader stops at.

Cost note: the association store is never snapshotted, and a single open of its
174 MB WAL was measured at **298 seconds**, paid per graph-arm cell. That cost
appears in no summary and no gate. Snapshot the world before scaling the run.

### 14. Finish LongMemEval — the only external yardstick in reach

481 lines already exist, untracked, in `benchmarks/longmemeval/`: a loader for the
official schema and a turn-store with Cypher retrieval and an optional hybrid leg.
Never committed, referenced by no doc, and its docstring targets the 0.4.4
binding — four minor versions stale. It began as a stretch task that was never
dispatched.

Every benchmark this project owns is one it wrote itself. LongMemEval is the
standard the agent-memory field actually cites, and a published number on it would
be worth more than any in-house suite.

**Needs:** the real 500-question dataset; an answer generator and judge (today
only retrieval is exercised — nothing scores an answer); baseline arms mirroring
the association suite's files and SQLite forms; a pre-registered pass condition
written before the run; a `run.py` emitting a committed `summary.md`; a port to
the current binding so the keyword leg is real.

### 15. The scale story is the largest unbacked claim in the repo

The stated design target is **10M nodes**. There is no measurement of any kind
above 50k vectors or 100k nodes. What has been measured is red, by wide margins:

| Gate | Measured | Ceiling |
|---|---|---|
| HNSW build growth 2k→10k | 16.19× | 8× |
| HNSW build at 50k | 1,017 s | 300 s |
| Re-embed growth 2k→50k | 5.85× | 3× |

Two of those three had never actually executed before 0.6.11 — six assertions
shared one test and the first panic ended it.

**The wider version of that bug is still live.** There are 39 `#[ignore]` markers
across 28 distinct tests and **CI runs 8 of them**. The twenty that never run
include `memory_per_node_is_within_the_ceiling`, `streaming_peak_transient_bound`,
`build_50k_is_under_300s`, `update_50k_is_under_25ms`, `remove_is_not_a_full_scan`
and `mvcc_concurrent_reads_during_writes` — that is **every memory-ceiling and
scale-growth assertion in the repo, unenforced**. A gate nothing runs is
indistinguishable from a gate that passes, which is precisely the failure 0.6.11
corrected in the small.

The escape routes are closing: SQ8 quantisation was spiked and cut at 0.92×
(slower), hand-written SIMD is ruled out at the NEON ceiling, and parallel HNSW
build is refused because deterministic expansion order is what keeps the graph a
function of the WAL. A 30k × 1536-D index build already takes 10.4 minutes, and
per-insert cost at 30k has already reached the 50k figure — the curve is steeper
between those points than the endpoints suggest.

**Do:** either pick the ceiling apart deliberately as its own release, or restate
the design target to what is measured. Shipping a 10M-node claim with a red 50k
gate is the one place this project's evidence discipline is not applied to itself.

### 16. The bindings expose a fraction of the engine

Python is the strongest binding and exposes about fifty methods against 180 public
functions in `db.rs` alone. Absent entirely: **all four graph algorithms**
(pagerank, communities, connected components, degree centrality), **all of
views**, **all of full-text**, **all of subscriptions**, and the whole rule
lifecycle (`delete_rule`, `rebuild_rule`, `rules`, `suggest_rules`). Also missing:
batch CAS and authz, `backup_to`, `apply_schema`, fsync policy, slow-query
capture.

The TypeScript client covers twelve of roughly twenty-eight server routes: no time
travel, no vector search, no roles or masks, no backup, no node or property
deletion. Both gaps sit directly under the features the README leads with.

---

## Sequence

Ordered by what unblocks what, not by size.

| # | Work | Why here |
|---|---|---|
| ~~1~~ | ~~Confirm item 1~~ | **Done 2026-09-28** — reproduced on a fresh demo store with the shipped binary |
| 2 | Fix the index space **and** items 10a/10b together; version or rebuild the sidecar; correct `timetravel.md`; add the tests the subsystem never had | One subsystem, three silent-wrong-answer defects; splitting them means touching the sidecar format three times |
| 3 | Re-run the association gate | Cost leg already passes; first realistic chance at a full pass |
| 4 | Write the bump script; fix pyproject, server-card, the 476 ms figure, the stale README result | One script retires a recurring class of release bug |
| 5 | Tag v0.6.11 | Now the wheel, the card and the claims all say what is true |
| 6 | v0.6.12: spec, then the server patch and the first-run fix | Already scoped by the 0.6.11 spec; the skill/listing mismatch belongs with it |
| 7 | CI for the TypeScript client and the packaging tests | Both currently ship untested; cheap, and 0.7 moves both |
| 8 | 0.7: bump all twelve pins, remove the code-graph door | The pins stop the publish otherwise |
| 9 | LongMemEval, or the scale ceiling — one of the two, properly | Both are release-sized; doing either halfway is worse than not starting |

---

## The pattern worth naming in the retro

Three of the findings in this plan are the same failure shape, not three unrelated
bugs: **a real defect present for one or more releases, invisible because nothing
was positioned to report it.**

- The HNSW scale gates: six assertions shared one `#[test]`, the first failure
  ended it, and **two of three reds had never executed**. The doc had been counting
  reds by reading a printed table. 0.6.11's own note: *"nothing regressed and
  nothing was fixed — the third failure was always there and nothing could report
  it."*
- Item 1: `commit_times` has 18 tests, none on a store whose commits fire rules —
  the only configuration where the bug exists.
- Items 10a/10b: no test covers multi-process or a truncating snapshot, the two
  operations the sidecar is wrong about.
- And the wider version: 20 of 28 `#[ignore]`d tests never run in CI, including
  every memory-ceiling and scale-growth assertion.

A gate nothing runs is indistinguishable from a gate that passes. The cheapest
durable fix is not more tests — it is making unrun assertions *visible*: one
assertion per test so a failure cannot mask its successors, and a CI step that
reports how many `#[ignore]`d tests exist against how many were executed.

## Not covered here, deliberately

- The 42 individually deferred items catalogued across the 0.6.x specs (vector
  scale, view-fed rule chaining, query-defined scopes, namespace write tokens,
  `MERGE` namespace naming, and the rest). Real, none urgent.
- `crates/core-api/src/db.rs` at 15,451 lines — 56% of its crate's production
  source, holding the write path, batch engine, scoped reads, history,
  multiplicity, migration and HNSW glue. Also real, also not urgent.
- Two large modules with no tests of their own: `core-api/src/shared.rs` (1,017
  production lines — group commit, the three-way lock order, the fsync-failure
  contract — 0 inline tests) and `core-storage/src/v8/encode.rs` (1,276 lines, the
  snapshot *writer*, reached only through round-trip and fuzz).
- `docs/design.md` has drifted from the code: §3/§4.2 still describe storage as
  HashMap topology plus zstd bincode snapshots. It also promises a cross-binding
  conformance corpus (`:234`) that does not exist.
