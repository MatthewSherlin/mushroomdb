# Rule Suggestion

mushroomdb can profile your data and propose linking rules — **the database that proposes its own schema**.

## Overview

Call `db.suggest_rules()` to receive a list of `RuleSuggestion` objects. Each suggestion includes:

- **`def`** — a complete `RuleDef` ready to pass to `db.create_rule()`.
- **`est_edges`** — estimated number of edges if the rule were applied (extrapolated from a sample of source nodes, labeled as an estimate).
- **`examples`** — up to 3 example `(src_key, dst_key, score)` pairs from the sample evaluation.
- **`rationale`** — a human-readable explanation of why this rule was suggested.

No rule is created automatically. You must accept explicitly:

```rust
let suggestions = db.suggest_rules();
for s in &suggestions {
    println!("{}: est ~{} edges — {}", s.def.name, s.est_edges, s.rationale);
}
// Accept the first one:
if let Some(s) = suggestions.into_iter().next() {
    db.create_rule(s.def)?;
}
```

## Detectors

The profiler runs five detectors over a sample of up to 10,000 nodes per label:

| Detector | Predicate proposed | Trigger |
|---|---|---|
| `_id`-suffix field matching another label's keys | `KeyMatch` | `field` ends with `_id` and ≥1 sampled value matches a dst-label node key |
| Cross-label list-token Jaccard overlap | `Overlap` | Jaccard p50 > 0; `min` set to observed p50 |
| Low-cardinality string equality | `FieldEqual` | ≤20 distinct values in both labels, ≥1 shared value |
| Overlapping numeric ranges | `NumericWithin` | Source and destination ranges overlap; tolerance = spread/4 (min 1.0) |
| Equal-dim float arrays | `VectorSimilar` | Dominant dimension matches across labels; `min=0.8`; `approximate=true` when n>2000 |

Already-existing rules (including auto-FK rules created at ingest time) are never re-suggested.

Suggested `RuleDef.max_edges` is never `None`. Omitted caps fill
`default_max_edges(&predicate)`: **32** for scored predicates (Overlap,
NumericWithin, GeoRadius, VectorSimilar, and `All`/`Any` that are not
KeyMatch-rooted) and **1** for KeyMatch (and KeyMatch-rooted `All`). HTTP
`POST /rules` and MCP `create_rule` apply the same fill when `max_edges` is
absent or JSON-null. Python `create_rule`: missing dict key fills the
default; explicit `None` stays uncapped (`DEFAULT_MAX_EDGES` = 1,000,000).

## API

```rust
// Default seed (0x4d75_7368_726f_6f6d).
let suggestions = db.suggest_rules();

// Custom seed for reproducibility.
let suggestions = db.suggest_rules_seeded(42);

// Custom config (e.g. for tests).
let config = SuggestConfig {
    budget_ms: 50,
    ..SuggestConfig::default()
};
let suggestions = db.suggest_rules_with_config(&config, 42);
```

## Server

```
GET /suggest
```

Returns a JSON array of `RuleSuggestion` objects. Each `def` inside can be POSTed to `/rules` to apply.

## CLI

```
mushroomdb suggest ./my-db
```

Pretty-prints all suggestions with estimated edge counts, example pairs, and rationale.

`mushroomdb demo` prints one suggestion at the end as a teaser.

## Determinism

Sampling is seeded (default seed `0x4d75_7368_726f_6f6d`). The same database content with the same seed produces identical suggestions. Use `suggest_rules_seeded(seed)` to pin the seed in tests or automation.

## Performance

The profiling pass samples at most `max_sample_nodes` (default 10,000) nodes per label. Each candidate rule gets a preview budget of `budget_ms` (default 250 ms) enforced structurally — the function checks elapsed time between source nodes and bails early, retaining partial results. `suggest_rules` never hangs.

## SuggestConfig

| Field | Default | Description |
|---|---|---|
| `max_sample_nodes` | 10,000 | Max nodes sampled per label during profiling |
| `max_sample_sources` | 200 | Max source nodes evaluated during preview |
| `max_examples` | 3 | Max example pairs per suggestion |
| `budget_ms` | 250 | Per-candidate preview time budget (ms) |
