# Association suite — truth vs the engine

`truth.py --build <dir> --seed 20260910 --scale 2000`: 200 `explain` probes, 20 `was_linked` probes and 20 `node_history` probes against the brute-force truth.

**0 unexplained disagreement(s) — every task's truth stands.**

## Filed: `node-history-deleted-props` (2 probes)

`node_history` on a deleted node reports its insert and its delete but none of its `prop_set` records: `db.rs`'s SetPropId branch resolves the id with `key_of`, which returns None for a tombstoned id, while the insert/delete branches match on the key string. `edge_history` and `was_linked` use `key_of_historical` instead.

- `known-gap[node-history-deleted-props]: node_history talent-000049: changelog sets 1 props, engine reports 0`
- `known-gap[node-history-deleted-props]: node_history talent-000557: changelog sets 1 props, engine reports 0`
