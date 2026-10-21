#### BREAKING

### Changed
- **ORDER BY … DESC now puts nulls first, per openCypher.** Null is the largest value in ordering, so ascending puts nulls last (as before) and descending puts them first (they used to be last in both directions). Equality and comparison operators are unchanged.
- **`ORDER BY … LIMIT k` is a bounded top-k selection.** With `LIMIT k` (and optional `SKIP s`) only the first `s + k` rows are selected and sorted instead of sorting every row; ties keep encounter order, so results are identical to a full sort. `ORDER BY n.score DESC LIMIT 10` over 1M nodes went from about 570 ms to 330 ms.

### Added
- **`ORDER BY` any expression in scope** — `MATCH (n:Person) RETURN n.name ORDER BY n.score DESC LIMIT 3` now works: a sort key no longer has to be a `RETURN` column. Keys can be properties, arithmetic, function calls, subscripts or `CASE` over the variables in scope, mixed with projected aliases (`ORDER BY s * -1`), and `WITH … ORDER BY` may name a variable the `WITH` does not carry. `RETURN DISTINCT` and aggregating `RETURN` still sort only what they project, and refuse anything else with a named error.
