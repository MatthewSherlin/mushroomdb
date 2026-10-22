#### BREAKING
- **Over HTTP, a null inside a map parameter is now passed through, so `SET n += $m` removes that key.** `SET n = $m` likewise replaces every user property with the map's non-null entries (the QL-06 rule). Before, HTTP dropped the null entry and the key stayed.

### Changed
- **A null query parameter binds as `null` over HTTP and in Python, matching Bolt (TX-12).** A JSON `null` (top level, or inside a list or map) and a Python `None` reach the query as a null, so `WHERE $p IS NULL` is true for it. Before, HTTP left a null parameter out and the query failed with `missing parameter`, and Python raised `TypeError`. Writes keep their rules: `SET n.p = $p` with a null `$p` is still refused, and a list holding a null cannot be stored.
