# The association benchmark

The association suite asks the question a graph is supposed to be best at —
*why are these two things related, and what did that look like on some other
day* — and measures the graph against the two things a developer would otherwise
reach for: a directory of JSON files, and a single relational database file.

It lives in `benchmarks/agent-tasks/`, beside the code suite, and shares its
harness: one `claude -p` session per cell, captured as stream-json, so tool
calls are counted and the final `result` event supplies usage, cost, turns and
duration. What differs is where the tasks come from, which arms run, which arm
everything is measured against, and which gate variant applies.

```sh
python3 benchmarks/agent-tasks/run.py --suite association --setup-only
python3 benchmarks/agent-tasks/run.py --suite association --pilot
python3 benchmarks/agent-tasks/run.py --suite association --reps 3
```

---

## One world, three forms

There is exactly one generator and three writers that must not disagree about a
single fact. `association/build.py` synthesizes a world — talents, companies,
jobs — and writes it three ways under the build directory:

| Directory | Arm | What the agent gets |
|---|---|---|
| `files/` | **P** | The entities as `entities/*.json`, the history as `changes.jsonl`, `roles.json`, and a README describing the rules. No server; `Read`, `Grep`, `Glob`, `Bash`. |
| `sqlite/` | **Q** | The same world as one relational file, `world.sqlite`, with its client on `PATH`, the same README and its schema. |
| `graph/` | **R** | The same world as a mushroomdb store, provisioned by `install --delivery mcp --db ./world.mushroomdb` into a directory holding nothing else, so the store is the agent's only data path. |

`equivalent()` loads all three back and asserts they agree. Every arm is allowed
`Read,Grep,Glob,Bash,Edit,Write`; arm R also gets `mcp__mushroomdb`.

**What only the graph form has** is the ten rules loaded as real linking rules —
so the store holds derived edges and their history — and the 1,536-dimension
`embedding` the semantic rule needs. That vector is the one generated field the
flat forms leave out: writing 1,536 floats per entity into a JSON file an agent
is meant to grep would be tens of megabytes of noise. The README names it as the
one field only the store has, and no task may ask about a rule that reads it.

**Time in the store is a WAL commit index**, not a wall clock. `days.json` is the
bridge: for each day it records the commit index of that day's last mutation,
which is what an as-of query wants. The store is never snapshotted — a snapshot
would truncate the very history the suite exists to ask about.

The world is deterministic in its seed and scale (`20260910` / `2000`, a 90-day
window and 300 changes), and every summary records the world digest it was run
against, so a result names the world it measured.

---

## The questions

Twenty tasks, five kinds, four each. Each is graded against executable truth
computed from the world by `association/truth.py` — an answer key, not a rubric —
and most are set checks with an explicit `forbid` list, so naming a near-miss
costs score rather than being ignored.

| Kind | What it asks |
|---|---|
| `why` | Every relationship type the rules derive between two keys, and what the two actually share |
| `multihop` | Which nodes are joined to a set by *all* of several relationship types at once, under a filter |
| `retraction` | Which links a change removed, and when |
| `timetravel` | What the relationships were on a named day |
| `visibility` | What one role may see |

The truth is brute-forced from the generated world independently of the engine,
and then cross-checked *against* the engine — 200 `explain` probes, 20
`was_linked` probes and 20 `node_history` probes, 0 disagreements
(`association/cross-check.md`). A task the baseline arm finishes in fewer than
the pilot floor of turns is *replaced* rather than dropped, so the set stays at
twenty.

---

## Rebuilding the world

```sh
python3 benchmarks/agent-tasks/run.py --suite association --setup-only
```

Builds all three forms once, under the harness scratch directory, and installs
the graph subject. It takes five to six minutes and is skipped when a complete
build is already there — an incomplete one is removed and rebuilt.

```sh
python3 benchmarks/agent-tasks/run.py --suite association --setup-only --reprovision
```

`--reprovision` takes the install off the graph subject first, so the next setup
writes it again with the current binary, skill, brief and `alwaysLoad` setting.
Use it whenever the binary has moved: without it, `install_association_graph` is
a no-op once `.mcp.json` is there, and the run would measure an old skill under
the new version's name. **The world itself is never touched** — the six-minute
build and the digest `tasks.json` was written against both survive, because the
artifacts `--reprovision` removes are the install's, not the store's.

`--force-setup` is the heavier hammer: it discards the world and regenerates it.
That changes the digest, so the task set has to be rebuilt against the new world
before anything measured against the old one can be compared.

---

## The gate

Pre-registered, computed by the harness rather than by hand, with **arm Q as the
baseline** — the graph has to beat one relational file, not a straw man. Arm P is a
second baseline, not a contender: a verdict of "passed, best arm P" would read
as a pass for a run in which files won, so only arm R is under the gate.

An arm passes when all of these hold:

1. correctness at or above arm Q, paired by task (a tie passes),
2. correctness at or above every other arm's paired mean,
3. cost at or below arm Q,
4. the 95% interval of the paired cost difference against arm Q lies
   **entirely below zero**,
5. no max-turns failure on a task arm Q finished.

Correctness carries no interval requirement, because arm Q saturated it in the
pilot — 1.00 on every task — which is what makes **cost the discriminator**.
Adoption is recorded but not gated: in arm R the store is the agent's only data
path, so adoption is 100% by construction and says nothing.

Intervals are a 95% percentile bootstrap over the per-task differences (2,000
resamples, seeded). A task where either side recorded nothing contributes no
difference; a cell that timed out or errored carries no cost, turns or duration
and is excluded from those means rather than counted as zero. Every section of a
summary states how many cells it dropped.

The first run of this suite is committed at
[`benchmarks/agent-tasks/results/20260911T005749Z/summary.md`](../../benchmarks/agent-tasks/results/20260911T005749Z/summary.md),
and it **failed**: the graph arm scored 0.795 against arm Q's 0.987 and cost
$0.5089 against $0.1257. The v0.6.3 CHANGELOG entry reports it in full, and the
association tools in [`mcp.md`](mcp.md) are what that run's failures turned into.

---

## Reading a summary

Each run writes `results/<timestamp>/summary.md` with the same sections in the
same order: the **Gate** verdict and the legs that failed, **Per cell** (one row
per task × arm × rep with its token counts, tool calls, turns, cost, score and
outcome), **Aggregate** (mean per arm), **Deltas vs the baseline arm** (paired,
with intervals), **Correctness by task**, **Sub-1.0 cells** (one line per cell
that did not score 1.0, with a hand-filled classification: `tool`, `model`, or
`grader`), **Tool adoption**, and the **Answers** themselves.

The sub-1.0 table is the one that has to be filled in by hand, by reading each
cell's stream file. It is where a claim about *why* an arm lost is allowed to
come from — not from the aggregate.
