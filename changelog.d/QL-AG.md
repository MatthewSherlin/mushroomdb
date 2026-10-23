### Fixed
- **`sum`, `avg`, `min` and `max` over a bare variable returned null.** `UNWIND [1,2] AS x RETURN max(x)` (and the same through `WITH x AS y`) gave null instead of 2; the same four over a property were right. Present in released 0.9.x (ledger row 157).
- **`sum` of integers returned a float.** `UNWIND [1,2,3] AS x RETURN sum(x)` gave `6.0`; it now gives `6`, a float only when an input is a float, and an integer sum past 64 bits is an error (`sum(): integer overflow`) instead of a rounded float. `avg` stays a float. Present in released 0.9.x (ledger row 158).

### Changed
- **`min` and `max` order strings, and `sum`/`avg`/`min`/`max` refuse a node or relationship.** `min`/`max` returned null over strings (and skipped lists, maps and booleans); they now return the smallest or largest non-null value, numbers before strings. `sum`, `avg`, `min` or `max` of a node, relationship or path variable is now the error `<function>(): expected a value, got a graph element`. `sum` of integers now returns an integer instead of a float (row 158).
