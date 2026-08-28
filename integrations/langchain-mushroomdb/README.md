# langchain-mushroomdb

LangChain `GraphStore` backed by **mushroomdb** — an embedded Rust graph database with a Python binding via PyO3.

Built against **langchain-community 0.4.2** / **langchain-core 1.6.1**.

## Installation

```bash
# 1. Build the mushroomdb Python binding from source
cd path/to/graph-db/bindings/python
VIRTUAL_ENV=$VIRTUAL_ENV maturin develop

# 2. Install this package
pip install -e path/to/graph-db/integrations/langchain-mushroomdb
```

## Quick start (no API key required)

The example below uses only standard library constructs and mushroomdb — no LLM or external API key needed.

```python
import tempfile

from langchain_community.graphs.graph_document import GraphDocument, Node, Relationship
from langchain_core.documents import Document

from langchain_mushroomdb import MushroomDBGraphStore

with tempfile.TemporaryDirectory() as db_path:
    store = MushroomDBGraphStore(db_path)

    # Build a small GraphDocument manually
    alice = Node(id="alice", type="Person", properties={"name": "Alice", "age": 30})
    inception = Node(id="inception", type="Movie", properties={"title": "Inception"})
    watched = Relationship(source=alice, target=inception, type="WATCHED")

    source_doc = Document(
        page_content="Alice watched Inception last night.",
        metadata={"source": "article-001"},
    )

    gd = GraphDocument(nodes=[alice, inception], relationships=[watched], source=source_doc)

    # Load with include_source=True to persist the source Document node
    store.add_graph_documents([gd], include_source=True)

    # Cypher query — direct passthrough to mushroomdb
    rows = store.query(
        "MATCH (p:Person)-[r:WATCHED]->(m:Movie) RETURN p.__id__, m.__id__ LIMIT 10"
    )
    print("Rows:", rows)
    # [{'p.__id__': 'alice', 'm.__id__': 'inception'}]

    # Refresh and inspect the schema
    store.refresh_schema()
    print("\nSchema string:\n", store.get_schema)
    print("\nStructured schema:\n", store.get_structured_schema)

    store._db.close()
```

## Manual graph operations

```python
from langchain_community.graphs.graph_document import Node, Relationship, GraphDocument
from langchain_mushroomdb import MushroomDBGraphStore

store = MushroomDBGraphStore("/tmp/mydb")

# Insert nodes and edges
gd = GraphDocument(
    nodes=[
        Node(id="bob",   type="Person", properties={"name": "Bob"}),
        Node(id="carol", type="Person", properties={"name": "Carol"}),
    ],
    relationships=[
        Relationship(
            source=Node(id="bob",   type="Person"),
            target=Node(id="carol", type="Person"),
            type="KNOWS",
        )
    ],
    source=None,
)
store.add_graph_documents([gd])

# Cypher passthrough (read)
rows = store.query("MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.__id__, b.__id__")
print(rows)  # [{'a.__id__': 'bob', 'b.__id__': 'carol'}]

# Refresh schema
store.refresh_schema()
print(store.get_schema)
print(store.get_structured_schema)

store._db.close()
```

## Reserved internal property name

| Property | Holds |
|---|---|
| `__id__` | Node's string key as stored in mushroomdb |

Do not use `__id__` in your node `properties` dicts — it is overwritten on every upsert.

## `include_source` behaviour

When `add_graph_documents(..., include_source=True)` is called, the `source` `Document` is stored as a `Document`-labelled node keyed by `metadata["source"]` (falling back to `metadata["id"]`, then a hash of `page_content`). Each entity node in `gd.nodes` gains a `MENTIONS` edge from the Document to the entity.

## Supported features

| Feature | Supported | Notes |
|---|---|---|
| `add_graph_documents` | Yes | Nodes and edges; idempotent upsert; `include_source` creates Document node + MENTIONS edges |
| `query` | Yes | Cypher passthrough; write-intent detected up front (no silent exception-swallowing) |
| `refresh_schema` | Yes | Scans all nodes and edges; updates `get_schema` and `get_structured_schema` |
| `get_schema` | Yes | Human-readable string listing labels and relationship patterns |
| `get_structured_schema` | Yes | Dict with `node_props`, `rel_props`, `relationships` |

## Closing the store

Call `store.close()` explicitly when you are done, or wrap usage in a `try/finally` block.  `__del__` closes the handle as a backstop but the interpreter does not guarantee when (or whether) `__del__` runs.  Calling `close()` more than once is safe.

```python
store = MushroomDBGraphStore("/tmp/mydb")
try:
    store.add_graph_documents([...])
finally:
    store.close()
```

## Upstream package status

This package is built against `langchain-community 0.4.2`.  The `langchain-community` package is officially deprecated — see [langchain-ai/langchain-community#674](https://github.com/langchain-ai/langchain-community/issues/674) for the migration roadmap.  You may see a `DeprecationWarning` on import; this is expected.  Standalone integration packages like `langchain-mushroomdb` are the successor pattern the migration guide recommends.

## Known limitations

- **Relation properties not persisted**: mushroomdb 0.1.2 has no edge-property API (`insert_edge` accepts only type/src/dst). `Relationship.properties` are silently dropped. This is a binding gap, not a design choice; it is the flip-point for future edge-property support.
- **Cypher subset**: mushroomdb implements a subset of openCypher. Not supported: `MERGE`, `id()` function, double-quoted string literals, `id` as a parameter name (reserved by the parser). `DELETE r` on a relationship silently no-ops — use `delete_edge` directly.
- **Write-intent routing**: `query()` inspects the query string for write keywords (`CREATE`, `SET`, `DELETE`, `MERGE`, `REMOVE`) and routes to `query_write` accordingly. A malformed read query raises rather than silently no-oping.
- **Schema built by scan**: `refresh_schema()` scans all nodes and edges (up to 100 000 each). Suitable for knowledge-graph sizes; not for billion-node datasets.
- **Single-process**: mushroomdb is embedded — open at most one store per database path per process.
