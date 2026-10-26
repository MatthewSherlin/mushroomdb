# Arrow, pandas and polars

`GraphDb.to_arrow_tables()` returns the whole graph as two [Apache Arrow](https://arrow.apache.org/) tables, so a data tool can read it without a Cypher round trip.

```python
from mushroomdb import GraphDb

db = GraphDb.open("./store")
tables = db.to_arrow_tables()      # {"nodes": pyarrow.Table, "edges": pyarrow.Table}

nodes_df = tables["nodes"].to_pandas()             # pandas

import polars as pl
edges_df = pl.from_arrow(tables["edges"])          # polars
```

`pyarrow` is optional. Install it with `pip install pyarrow`; without it the call raises `ImportError` naming that command. pandas and polars are yours to install as well.

## Columns

The columns are fixed, so a script written against one store reads any other.

| Table | Column | Type | Meaning |
|---|---|---|---|
| `nodes` | `key` | string | The node key. Rows are sorted by it. |
| `nodes` | `label` | string | The node's label. |
| `nodes` | `properties` | string | A JSON object of the node's properties, keys sorted, always valid JSON. Each value is written as the HTTP API writes it; see below. |
| `edges` | `source` | string | Key of the node the edge leaves. |
| `edges` | `type` | string | The edge type. |
| `edges` | `target` | string | Key of the node the edge enters. |
| `edges` | `derived` | bool | `true` for an edge a rule derived, `false` for one you wrote. |
| `edges` | `rule` | string | The deriving rule's name; null for a manual edge. |
| `edges` | `weight` | float64 | The rule's `weight_prop` value when it is numeric; otherwise null. |

Edges are sorted by `(type, source, target)`. An empty store gives empty tables with the same columns.

Properties are one JSON column because different labels carry different properties. To read them into columns, use `pandas.json_normalize(nodes_df["properties"].map(json.loads))`, or polars `str.json_decode(dtype)` with a `pl.Struct` naming the properties you want.

### Property value forms

The forms match what the HTTP API returns for the same value, so the two surfaces agree.

| Engine value | JSON in `properties` |
|---|---|
| Integer | A JSON number, exact up to 64 bits. Parse it as a 64-bit integer: a double-based parser loses digits above 2^53. |
| Float | A JSON number. NaN, Infinity and -Infinity are `null`, never `NaN`. |
| Boolean, string | `true` / `false`, a JSON string. |
| List, map | A JSON array, a JSON object, with these same forms inside. |
| Null inside a list or map | `null` |
| Date | `{"$date": "2024-01-15"}` |
| Local date-time | `{"$ldt": "2024-01-15T12:34:56.789012"}` |
| Date-time with offset | `{"$dt": "2024-01-15T12:34:56.789012+05:30"}` |
| Duration | `{"$dur": "P1Y2M3DT4S"}`, ISO 8601 |
| Binary | `{"$binary": "AP8="}`, base64 |

A date outside Python's `datetime` range (year above 9999) still exports, as its text form.

### Namespaces and scope

The namespace a node lives in is a property: it appears in `properties` as `ns`. An unscoped handle exports every namespace, the same as an unscoped `db.query`, and ignores roles and masks. A handle from `scoped()` refuses the call.

## Limits

- It materialises every node and edge in memory at once. Use it on a graph that fits in memory, or query a slice with `db.query` instead.
- It is a read of the whole store, so a handle from `scoped()` refuses it with `ValueError`; call it on the handle `scoped()` was called on.
- It reads the live store and takes no `at=` argument. For a past commit, use `query_at`.

For a complete script that builds a graph and retrieves from it, see `examples/graph-retrieval/` in the repository.
