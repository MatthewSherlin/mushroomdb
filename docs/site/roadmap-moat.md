# The moat: rule chaining, temporal queries, memory-native

These are the category-defining features — the reasons to reach for mushroomdb
over a generic embedded graph. One is shipped; two are specified here to be
built with design sign-off, because they touch correctness-critical cores (the
rule engine and the durability model) where a rushed change is worse than none.

## 1. Temporal queries — shipped

`query_at(commit, cypher)` runs a read against the graph as it existed at a past
WAL commit (Rust API, `POST /query {"as_of": N}`, Python `query_at`). It reuses
`open_at`, so the live store is untouched and no new storage state is needed.
This is the agent-replay / versioned-knowledge-graph primitive.

**Next increments (small, additive):** `AS OF COMMIT n` as first-class Cypher
syntax (parser clause → routes to `query_at`); temporal + RBAC-mask composition
(currently rejected); a `commit_at(timestamp)` helper so callers can time-travel
by wall-clock, not just commit index.

## 2. Rule chaining — shipped in v0.5 for derived edges; view-fed rules remain designed

**Today:** a rule-derived edge feeds every via-hop rule that hops over its edge
type, in the same commit, bounded at four levels with each `(rule, source)`
recomputed once per write and every via-edge dependency cycle rejected at
`create_rule`. See [Chaining](rules.md#chaining). What is *not* shipped is the
view half below: a rule cannot read a view, so view values still do not feed
rules, and there is no cycle damping — v0.5 rejects every cycle rather than
looking for a fixpoint.

**Goal:** let the view values a derived edge updates also feed dependent rules,
so `A → B` associations can cascade into `B → C` through aggregates — a
declarative graph computation model no competitor has.

**The hard part — cycles.** Rule R1 updates a neighbor-aggregate view that R2
matches on; R2 creates an edge that changes a view R1 matches on → infinite
loop. The design must be terminating and deterministic.

**Proposed design:**
- Build a static **rule dependency graph** at `create_rule` time: rule R
  *depends on* field F if its predicate reads F; R *produces* field F if it
  writes an edge whose type feeds a view over F. Edges of the dependency graph
  are (produces F) → (depends on F).
- On a write, propagate changes in **dependency order** with a **bounded
  fan-out depth** (default 4, same cap as predicate nesting). Each node carries
  a per-commit "already fired at depth d" set so a rule fires at most once per
  node per commit — this is what guarantees termination even with cycles in the
  dependency graph.
- Reject, at `create_rule`, a dependency cycle that is *not* damped by a view
  aggregate (a pure edge→edge cycle with no fixpoint), with a named error.
- Everything stays deterministic on replay: the propagation order is a function
  of the (static) dependency graph and the committed write, so open/replay
  re-derives identically — preserving the current crash-recovery contract.

**Test plan:** a fixpoint suite (chains converge to the same state regardless of
write interleaving), a termination suite (cyclic-but-damped rules stop), and a
replay-equivalence suite (WAL replay reproduces chained edges exactly).

## 3. Memory-native — designed

Built on the existing `edge_history` / CAS / views foundation. Three
independently shippable pieces:

**Decay.** A derived edge's weight diminishes with age unless reinforced. Store
each edge's last-reinforced commit (already implied by `edge_history`); expose
`decay(base, age_commits, halflife)` as a Cypher scalar function and an optional
maintained `effective_weight` view. Reinforcement = any rule re-fire that
re-asserts the edge resets its age. No background process required for the
query-time function; the maintained view updates incrementally like other views.

**Consolidation.** Old per-node history compacts into a summary once it passes a
retention horizon: instead of N `PropSet` entries, keep a single "field changed
N times between commits X..Y, last value V". This bounds `node_history` growth
for long-lived agent memories while keeping `was_linked` / point-in-time answers
correct for the retained window. Implemented as a WAL-archive-time pass (the
archive machinery already exists), gated by a per-store retention policy.

**Namespaces.** Per-agent isolation so many agents share one store without
seeing each other's memories. Model it as a reserved `_ns` label prefix (or a
first-class namespace column) that composes with the existing RBAC mask: a
namespace token sees only its namespace's nodes/edges, enforced at the same
choke point as masks (so the security proofs carry over). Rules and vector
search run within a namespace by default.

**Sequencing:** decay first (smallest, most self-contained, immediately useful),
then namespaces (reuses the mask enforcement point), then consolidation (needs
the retention-policy surface and archive integration).

## Why these are specified, not rushed

Rule chaining changes the rule engine's firing semantics, and memory-native adds
retention/decay policy to the durability model. Both are the surfaces where
mushroomdb's honesty record is built (deterministic replay, no data loss). The
right way to ship them is with the designs above reviewed and the test plans
green — not as a late, unreviewed change to the correctness core.
