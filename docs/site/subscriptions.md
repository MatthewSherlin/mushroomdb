# Live Subscriptions

mushroomdb exposes a post-commit event stream for both rule-derived edge events and raw write mutations. Subscribers receive events **after** the WAL fsync and after the in-memory state is updated, so a subscriber that immediately queries the database upon receiving an event observes the state that produced it.

## v1 Scope

| Feature | v1 (this doc) |
|---|---|
| Rule edge events (`EdgeFired`, `EdgeRetracted`) | yes |
| Write events (`NodeInserted`, `NodeDeleted`, `PropSet`, `PropRemoved`, `EdgeInserted`, `EdgeDeleted`) | yes |
| Incremental query subscriptions (differential dataflow) | roadmap |

## Rust API

```rust
// Subscribe to edge events for one rule.
let sub: Subscription = db.subscribe_rule("skill_fit")?;
// Returns Err(GraphError::RuleNotFound) for unknown rules.

// Subscribe to edge events for all rules.
let sub = db.subscribe_all_rules();

// Subscribe to write events (node/prop mutations).
let sub = db.subscribe_writes();
```

Each call returns a `Subscription` handle. **Dropping it unregisters the subscriber** — the next commit prunes the dead entry; no resources leak.

### Reading events

```rust
// Non-blocking: returns None if the queue is empty.
while let Some(ev) = sub.try_recv() {
    println!("{ev:?}");
}

// Blocking with timeout:
if let Some(ev) = sub.recv_timeout(Duration::from_millis(100)) {
    println!("{ev:?}");
}
```

### Event types

```rust
pub enum DbEvent {
    EdgeFired    { rule, src_key, dst_key, edge_type, weight: Option<f64>, commit_seq },
    EdgeRetracted{ rule, src_key, dst_key, edge_type, commit_seq },
    NodeInserted { label, key, commit_seq },
    NodeDeleted  { key, commit_seq },
    EdgeInserted { edge_type, src, dst, commit_seq },
    EdgeDeleted  { edge_type, src, dst, commit_seq },
    PropSet      { key, field, commit_seq },
    PropRemoved  { key, field, commit_seq },
    Lagged       { missed },
}
```

`commit_seq` is a monotonically increasing counter per `log_then_apply_with` call. All events from a single `write_batch` or Cypher statement share the same `commit_seq` and arrive as a contiguous run in declaration order.

### Overflow — Lagged

Each subscription has a bounded queue (default: 65,536 events). When the queue is full, events are dropped and the internal missed counter is incremented. The next read that finds an empty queue and a non-zero miss count returns `DbEvent::Lagged { missed: N }` before continuing with queued events.

Consumers that require losslessness must read promptly or re-sync by re-reading graph state after observing a `Lagged` marker.

The queue capacity can be tuned per-db-instance via `db.set_sub_capacity(n)` (useful for testing the Lagged path with a small queue).

## WebSocket API (`GET /subscribe`)

The server exposes a WebSocket endpoint at `GET /subscribe`. After upgrade, the client sends a single JSON subscribe message, then receives a stream of JSON event frames.

### Subscribe message

```json
{"rules": ["skill_fit", "geo_match"], "writes": true}
```

All fields are optional:
- `rules`: list of rule names to subscribe to (unknown name → `{"error":"..."}` and close).
- `writes`: if `true`, also receive write events.

### Ack frame

Immediately after a valid subscribe message the server responds:

```json
{"subscribed": true}
```

### Event frames

Events are serialised as internally-tagged JSON:

```json
{"type":"edge_fired","rule":"skill_fit","src_key":"p1","dst_key":"proj-01","edge_type":"FIT","weight":0.87,"commit_seq":42}
{"type":"edge_retracted","rule":"skill_fit","src_key":"p1","dst_key":"proj-01","edge_type":"FIT","commit_seq":45}
{"type":"node_inserted","label":"Person","key":"p1","commit_seq":1}
{"type":"lagged","missed":3}
```

The server disconnects only on a write error; slow consumers receive `{"type":"lagged","missed":N}` and continue.

**Multi-subscription idle latency:** when a single WS connection subscribes to multiple rules, the server bridge thread blocks on the first subscription when idle. Events arriving on secondary subscriptions while the first is quiet may experience up to ~100 ms of additional latency before being forwarded.

## Query subscription skip optimization (v0.4.3)

`subscribe_query` re-executes the plan on every commit. v0.4.3 adds a label-skip fast-path: if the commit can be proven to touch only nodes with a label that does not match the subscription's leading scan, re-execution is skipped entirely — the result set cannot have changed.

### When a subscription is skipped

A subscription for `MATCH (n:Person) …` is skipped when ALL of the following hold:

- The commit contains only node records (`InsertNode`, `InsertNodeId`, `SetProp`, `DeleteNode`) whose resolved labels are all ≠ `Person`.
- The commit contains no edge records (`InsertEdge`, `DeleteEdge`, or their dense-id variants).
- No rule engine produced derived-edge deltas for this commit.

Any record type not in the above set (e.g., unknown future variants) causes re-execution rather than a skip.

### Expand limitation

Subscriptions whose plan contains an `Expand` op (i.e., one-hop MATCH patterns like `MATCH (a:Person)-[r:KNOWS]->(b:Org) RETURN a`) are **never skipped**. Edges can join two label sets together, so a commit touching any label can potentially change the result. This is the conservative v0.4.3 boundary; differential evaluation that handles Expand correctly is roadmap / Phase 5.

### No-scan or unlabeled scan

`MATCH (n) RETURN n` (no label filter) and any subscription whose plan has no recognizable leading scan are also never skipped.

## Invariants

1. **Post-fsync ordering.** Events are distributed inside `log_then_apply_with` after WAL fsync and in-memory apply both complete. A subscriber querying immediately on receipt sees consistent state.
2. **Bounded queue.** Default 65,536 per subscriber. Overflow → `Lagged { missed }`.
3. **Clean unregister on drop.** Dropping `Subscription` releases the `Arc`; the next commit's distribution loop prunes the dead `Weak`.
4. **Replay silence.** `open()` / WAL recovery applies records via `apply`, not `log_then_apply_with`. Pending engine deltas accumulated during replay are drained and discarded before `open_with` returns. Subscriptions installed after open receive only live commits.
5. **commit_seq cohesion.** All events from one `write_batch` share the same `commit_seq` and arrive in a contiguous run.
6. **Skip soundness.** A skipped commit is guaranteed to produce the same result set as the previous execution. Any skip requires: (a) no engine deltas, (b) every record resolves to a label ≠ the scan label, (c) no edge records. Any resolution failure is treated as "must execute."
