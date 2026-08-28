# llama-index-graph-stores-mushroomdb

LlamaIndex `PropertyGraphStore` backed by **mushroomdb** — an embedded Rust graph database with a Python binding via PyO3.

Built against **llama-index-core 0.14.24**.

## Installation

```bash
# 1. Build the mushroomdb Python binding from source
cd path/to/graph-db/bindings/python
VIRTUAL_ENV=$VIRTUAL_ENV maturin develop

# 2. Install this package
pip install -e path/to/graph-db/integrations/llama-index-mushroomdb
```

## Quick start (no API key required)

The example below uses `MockLLM` (from `llama-index-core`) and an inline fake
embedding model so no external API key is needed.

```python
import tempfile
from typing import List

from llama_index.core import PropertyGraphIndex
from llama_index.core.llms.mock import MockLLM
from llama_index.core.schema import Document
from llama_index.core.base.embeddings.base import BaseEmbedding
from llama_index.graph_stores.mushroomdb import MushroomDBPropertyGraphStore


class FakeEmbed(BaseEmbedding):
    model_name: str = "fake"
    embed_batch_size: int = 10

    def _get_query_embedding(self, query: str) -> List[float]:
        return [0.1, 0.2, 0.3, 0.4]

    def _get_text_embedding(self, text: str) -> List[float]:
        return [0.1, 0.2, 0.3, 0.4]

    async def _aget_query_embedding(self, query: str) -> List[float]:
        return self._get_query_embedding(query)


with tempfile.TemporaryDirectory() as db_path:
    store = MushroomDBPropertyGraphStore(db_path)

    docs = [
        Document(text="Alice knows Bob.", id_="doc1"),
        Document(text="Bob works at Acme Corp.", id_="doc2"),
    ]

    index = PropertyGraphIndex.from_documents(
        docs,
        llm=MockLLM(),
        property_graph_store=store,
        embed_model=FakeEmbed(),
        embed_kg_nodes=True,
        kg_extractors=[],   # skip LLM-based path extraction
        use_async=False,
    )

    # Nodes are now in the store.
    print(store.client.stats())   # {"nodes_live": 2, ...}
```

## Quick start with OpenAI (optional)

If you have an OpenAI API key and `llama-index-llms-openai` installed, drop
the `llm=MockLLM()` and `kg_extractors=[]` arguments to enable automatic
knowledge-graph extraction from the documents.

```python
# pip install llama-index-llms-openai llama-index-embeddings-openai
import os, tempfile
from llama_index.core import PropertyGraphIndex
from llama_index.core.schema import Document
from llama_index.graph_stores.mushroomdb import MushroomDBPropertyGraphStore

os.environ["OPENAI_API_KEY"] = "sk-..."

with tempfile.TemporaryDirectory() as db_path:
    store = MushroomDBPropertyGraphStore(db_path)
    index = PropertyGraphIndex.from_documents(
        [Document(text="Alice knows Bob.")],
        property_graph_store=store,
        use_async=False,
    )
    retriever = index.as_retriever(include_text=False)
    for node in retriever.retrieve("Who does Alice know?"):
        print(node.get_content())
```

## Manual graph operations

```python
from llama_index.core.graph_stores.types import EntityNode, Relation
from llama_index.graph_stores.mushroomdb import MushroomDBPropertyGraphStore

store = MushroomDBPropertyGraphStore("/tmp/mydb")

# Upsert entities
alice = EntityNode(name="Alice", label="Person", properties={"age": 30})
bob   = EntityNode(name="Bob",   label="Person", properties={"age": 25})
store.upsert_nodes([alice, bob])

# Upsert a relation
store.upsert_relations([Relation(label="KNOWS", source_id=alice.id, target_id=bob.id)])

# Traverse
triplets = store.get_triplets(entity_names=["Alice"])
print(triplets)  # [(EntityNode(Alice), Relation(KNOWS), EntityNode(Bob))]

# Cypher passthrough
rows = store.structured_query(
    "MATCH (n:Person) RETURN n.__name__ LIMIT 10"
)

# Flush to snapshot
store.persist("/ignored/path")
store.close()
```

## Reserved internal property names

The following property names are used internally and will be **silently
overwritten** if present in user-supplied `EntityNode.properties` or
`ChunkNode.properties`:

| Property | Holds |
|---|---|
| `__id__` | Primary key (`LabelledNode.id`) |
| `__type__` | Node kind: `"entity"` or `"chunk"` |
| `__name__` | `EntityNode.name` |
| `__text__` | `ChunkNode.text` |
| `__embedding__` | Embedding vector (list of floats) |

## Supported features

| Feature | Supported | Notes |
|---|---|---|
| `upsert_nodes` | Yes | EntityNode and ChunkNode; idempotent; label updates via delete+reinsert |
| `upsert_relations` | Yes | Creates placeholder nodes for unknown endpoints; **Relation.properties dropped** (binding gap) |
| `get` | Yes | By id or by property filter |
| `get_triplets` | Yes | Filter by entity name, relation type, id, or property |
| `get_rel_map` | Yes | BFS traversal to configurable depth |
| `delete` | Yes | Nodes (DETACH DELETE) and/or edges by relation type |
| `structured_query` | Yes | Cypher passthrough; read and write auto-detected |
| `vector_query` | Yes | Python cosine similarity over stored embeddings |
| `persist` | Yes | Delegates to mushroomdb `snapshot()` |
| Async methods | Yes | Thin sync wrappers (no event-loop blocking for embedded DB) |

## Known limitations

- **Relation properties not persisted**: the mushroomdb 0.1.2 binding has no edge-property API (`insert_edge` accepts only type/src/dst; `SET r.field` is rejected by the Cypher planner). `Relation.properties` passed to `upsert_relations` are silently dropped; `get_triplets` and `get_rel_map` always return relations with `properties={}`. This is a binding gap queued alongside ANN.
- **No native ANN / `find_similar`**: `vector_query` scans all embeddings in Python. Suitable for knowledge-graph sizes (thousands of nodes); not for million-scale vector workloads.
- **Cypher subset**: mushroomdb implements a subset of openCypher. `MERGE`, `id()`, double-quoted string literals, and `id` as a parameter name are not available.
- **Single-process**: mushroomdb is an embedded store — one process per database path at a time.
- **Label updates**: when a node's label changes (e.g. placeholder `"entity"` → `"Person"`), the store deletes and reinserts the node, restoring user-owned edges. Rule-derived edges are re-derived by the engine automatically.
