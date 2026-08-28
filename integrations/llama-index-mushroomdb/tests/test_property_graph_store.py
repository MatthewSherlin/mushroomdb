"""pytest suite for MushroomDBPropertyGraphStore.

Run from the package root:
    pytest tests/ -v

All tests use a temporary directory so they are fully isolated.
"""

from __future__ import annotations

import math
import tempfile
from typing import List

import pytest
from llama_index.core.graph_stores.types import (
    ChunkNode,
    EntityNode,
    Relation,
)
from llama_index.core.vector_stores.types import VectorStoreQuery

from llama_index.graph_stores.mushroomdb import MushroomDBPropertyGraphStore


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture()
def store(tmp_path):
    """Fresh store backed by a temp directory."""
    s = MushroomDBPropertyGraphStore(str(tmp_path))
    yield s
    try:
        s._db.close()
    except Exception:
        pass


def _make_entity(name: str, label: str = "entity", **props) -> EntityNode:
    return EntityNode(name=name, label=label, properties=props)


def _make_chunk(text: str, id_: str | None = None, **props) -> ChunkNode:
    return ChunkNode(text=text, id_=id_, properties=props)


def _make_rel(src: str, label: str, dst: str) -> Relation:
    return Relation(label=label, source_id=src, target_id=dst)


# ---------------------------------------------------------------------------
# 1. Instantiation
# ---------------------------------------------------------------------------


def test_instantiate(tmp_path):
    """Store can be opened and the client is non-None."""
    store = MushroomDBPropertyGraphStore(str(tmp_path))
    assert store.client is not None
    store._db.close()


def test_class_flags():
    """supports_structured_queries and supports_vector_queries are True."""
    assert MushroomDBPropertyGraphStore.supports_structured_queries is True
    assert MushroomDBPropertyGraphStore.supports_vector_queries is True


# ---------------------------------------------------------------------------
# 2. upsert_nodes
# ---------------------------------------------------------------------------


def test_upsert_entity_nodes(store):
    alice = _make_entity("Alice", label="Person", age=30)
    bob = _make_entity("Bob", label="Person", age=25)
    store.upsert_nodes([alice, bob])

    retrieved = store.get(ids=[alice.id, bob.id])
    ids_found = {n.id for n in retrieved}
    assert alice.id in ids_found
    assert bob.id in ids_found


def test_upsert_chunk_nodes(store):
    chunk = _make_chunk("The quick brown fox", id_="doc_1_chunk_0")
    store.upsert_nodes([chunk])

    retrieved = store.get(ids=["doc_1_chunk_0"])
    assert len(retrieved) == 1
    assert isinstance(retrieved[0], ChunkNode)
    assert retrieved[0].text == "The quick brown fox"


def test_upsert_entity_is_idempotent(store):
    """Upserting the same node twice updates properties without error."""
    node = _make_entity("Alice", score=1)
    store.upsert_nodes([node])

    node2 = _make_entity("Alice", score=99)
    store.upsert_nodes([node2])

    retrieved = store.get(ids=[node.id])
    assert len(retrieved) == 1
    assert retrieved[0].properties.get("score") == 99


def test_upsert_node_with_embedding(store):
    embedding = [0.1, 0.2, 0.3, 0.4]
    node = EntityNode(name="EmbeddedAlice", embedding=embedding)
    store.upsert_nodes([node])

    retrieved = store.get(ids=[node.id])
    assert len(retrieved) == 1
    assert retrieved[0].embedding == pytest.approx(embedding)


# ---------------------------------------------------------------------------
# 3. upsert_relations
# ---------------------------------------------------------------------------


def test_upsert_relations(store):
    alice = _make_entity("Alice")
    bob = _make_entity("Bob")
    store.upsert_nodes([alice, bob])
    rel = _make_rel(alice.id, "KNOWS", bob.id)
    store.upsert_relations([rel])

    triplets = store.get_triplets(entity_names=["Alice"])
    assert len(triplets) == 1
    src, r, dst = triplets[0]
    assert r.label == "KNOWS"
    assert dst.id == bob.id


def test_upsert_relations_creates_placeholder_nodes(store):
    """upsert_relations must not crash when endpoints are not yet in the store."""
    rel = _make_rel("ghost_src", "HAUNTS", "ghost_dst")
    store.upsert_relations([rel])

    # Both placeholder nodes should now exist
    nodes = store.get(ids=["ghost_src", "ghost_dst"])
    assert len(nodes) == 2


def test_label_update_relations_then_nodes(store):
    """upsert_relations first (placeholders) then upsert_nodes with real label.

    LlamaIndex does not guarantee insertion order, so nodes may arrive after
    the relations that reference them.  The placeholder has label "entity";
    the real EntityNode may carry label "Person".  The store must update the
    label and preserve the incident edges.
    """
    # 1. Relations arrive first — creates placeholder nodes with label "entity"
    rel = _make_rel("Alice", "KNOWS", "Bob")
    store.upsert_relations([rel])

    placeholders = store.get(ids=["Alice", "Bob"])
    assert all(n.label == "entity" for n in placeholders)

    # 2. Real nodes arrive later with correct label
    alice = EntityNode(name="Alice", label="Person", properties={"role": "admin"})
    bob = EntityNode(name="Bob", label="Person")
    store.upsert_nodes([alice, bob])

    # Label must now reflect the real label
    updated = store.get(ids=["Alice", "Bob"])
    assert all(n.label == "Person" for n in updated), [n.label for n in updated]

    # Edge must still be present after the label update
    triplets = store.get_triplets(entity_names=["Alice"])
    assert len(triplets) == 1
    _, r, dst = triplets[0]
    assert r.label == "KNOWS"
    assert dst.id == "Bob"


def test_label_update_preserves_properties(store):
    """Label update via delete+reinsert must not lose existing user properties."""
    # Placeholder created by upsert_relations
    rel = _make_rel("Alice", "LIKES", "Carol")
    store.upsert_relations([rel])

    # Real node with same id but new label and added properties
    alice = EntityNode(name="Alice", label="Person", properties={"age": 30})
    store.upsert_nodes([alice])

    retrieved = store.get(ids=["Alice"])
    assert len(retrieved) == 1
    node = retrieved[0]
    assert node.label == "Person"
    assert node.properties.get("age") == 30


def test_upsert_relation_idempotent(store):
    """Upserting the same relation twice produces exactly one edge."""
    alice = _make_entity("Alice")
    bob = _make_entity("Bob")
    store.upsert_nodes([alice, bob])
    rel = _make_rel(alice.id, "KNOWS", bob.id)
    store.upsert_relations([rel])
    store.upsert_relations([rel])  # duplicate

    triplets = store.get_triplets(entity_names=["Alice"])
    assert len(triplets) == 1


def test_relation_properties_are_dropped(store):
    """Relation.properties are silently dropped — binding gap, not a design choice.

    The mushroomdb 0.1.2 binding has no edge-property API (insert_edge takes
    only type/src/dst; SET r.field is rejected by the Cypher planner).
    This test pins the current behavior so that when the binding gains edge-
    property support this test is the obvious place to flip.
    """
    alice = _make_entity("Alice")
    bob = _make_entity("Bob")
    store.upsert_nodes([alice, bob])

    rel = Relation(
        label="KNOWS",
        source_id=alice.id,
        target_id=bob.id,
        properties={"weight": 0.9, "since": "2024"},
    )
    store.upsert_relations([rel])

    triplets = store.get_triplets(entity_names=["Alice"])
    assert len(triplets) == 1
    _, r, _ = triplets[0]
    # Properties are NOT round-tripped — binding gap.
    assert r.properties == {}


# ---------------------------------------------------------------------------
# 4. get — by id and by property
# ---------------------------------------------------------------------------


def test_get_by_id(store):
    store.upsert_nodes([_make_entity("Alice"), _make_entity("Bob")])
    alice = _make_entity("Alice")
    results = store.get(ids=[alice.id])
    assert len(results) == 1
    assert results[0].id == alice.id


def test_get_by_property(store):
    store.upsert_nodes([
        _make_entity("Alice", role="admin"),
        _make_entity("Bob", role="user"),
    ])
    results = store.get(properties={"role": "admin"})
    assert len(results) == 1
    assert results[0].properties.get("role") == "admin"


def test_get_unknown_id_returns_empty(store):
    results = store.get(ids=["does_not_exist"])
    assert results == []


# ---------------------------------------------------------------------------
# 5. get_triplets
# ---------------------------------------------------------------------------


def test_get_triplets_by_entity_name(store):
    alice = _make_entity("Alice")
    bob = _make_entity("Bob")
    carol = _make_entity("Carol")
    store.upsert_nodes([alice, bob, carol])
    store.upsert_relations([
        _make_rel(alice.id, "KNOWS", bob.id),
        _make_rel(carol.id, "LIKES", bob.id),
    ])

    triplets = store.get_triplets(entity_names=["Alice"])
    assert len(triplets) == 1
    _, r, _ = triplets[0]
    assert r.label == "KNOWS"


def test_get_triplets_by_relation_name(store):
    alice = _make_entity("Alice")
    bob = _make_entity("Bob")
    store.upsert_nodes([alice, bob])
    store.upsert_relations([
        _make_rel(alice.id, "KNOWS", bob.id),
        _make_rel(alice.id, "LIKES", bob.id),
    ])

    knows_triplets = store.get_triplets(relation_names=["KNOWS"])
    assert all(r.label == "KNOWS" for _, r, _ in knows_triplets)


def test_get_triplets_by_id(store):
    alice = _make_entity("Alice")
    bob = _make_entity("Bob")
    store.upsert_nodes([alice, bob])
    store.upsert_relations([_make_rel(alice.id, "KNOWS", bob.id)])

    triplets = store.get_triplets(ids=[alice.id])
    assert len(triplets) == 1


def test_get_triplets_no_filter_returns_all(store):
    alice = _make_entity("Alice")
    bob = _make_entity("Bob")
    carol = _make_entity("Carol")
    store.upsert_nodes([alice, bob, carol])
    store.upsert_relations([
        _make_rel(alice.id, "KNOWS", bob.id),
        _make_rel(carol.id, "LIKES", bob.id),
    ])

    triplets = store.get_triplets()
    assert len(triplets) == 2


# ---------------------------------------------------------------------------
# 6. get_rel_map
# ---------------------------------------------------------------------------


def test_get_rel_map_depth_1(store):
    alice = _make_entity("Alice")
    bob = _make_entity("Bob")
    carol = _make_entity("Carol")
    store.upsert_nodes([alice, bob, carol])
    store.upsert_relations([
        _make_rel(alice.id, "KNOWS", bob.id),
        _make_rel(bob.id, "KNOWS", carol.id),
    ])

    # depth=1 should only see Alice->Bob, not Bob->Carol
    triplets = store.get_rel_map([alice], depth=1)
    assert len(triplets) == 1
    src, r, dst = triplets[0]
    assert src.id == alice.id
    assert dst.id == bob.id


def test_get_rel_map_depth_2(store):
    alice = _make_entity("Alice")
    bob = _make_entity("Bob")
    carol = _make_entity("Carol")
    store.upsert_nodes([alice, bob, carol])
    store.upsert_relations([
        _make_rel(alice.id, "KNOWS", bob.id),
        _make_rel(bob.id, "KNOWS", carol.id),
    ])

    triplets = store.get_rel_map([alice], depth=2)
    assert len(triplets) == 2


def test_get_rel_map_ignore_rels(store):
    alice = _make_entity("Alice")
    bob = _make_entity("Bob")
    store.upsert_nodes([alice, bob])
    store.upsert_relations([
        _make_rel(alice.id, "KNOWS", bob.id),
        _make_rel(alice.id, "HATES", bob.id),
    ])

    triplets = store.get_rel_map([alice], depth=1, ignore_rels=["HATES"])
    rel_labels = {r.label for _, r, _ in triplets}
    assert "HATES" not in rel_labels
    assert "KNOWS" in rel_labels


# ---------------------------------------------------------------------------
# 7. delete
# ---------------------------------------------------------------------------


def test_delete_by_id(store):
    alice = _make_entity("Alice")
    store.upsert_nodes([alice])
    assert len(store.get(ids=[alice.id])) == 1

    store.delete(ids=[alice.id])
    assert store.get(ids=[alice.id]) == []


def test_delete_by_entity_name(store):
    alice = _make_entity("Alice")
    store.upsert_nodes([alice])
    store.delete(entity_names=["Alice"])
    assert store.get(ids=[alice.id]) == []


def test_delete_relation_by_name(store):
    alice = _make_entity("Alice")
    bob = _make_entity("Bob")
    store.upsert_nodes([alice, bob])
    store.upsert_relations([
        _make_rel(alice.id, "KNOWS", bob.id),
        _make_rel(alice.id, "LIKES", bob.id),
    ])

    store.delete(relation_names=["KNOWS"])

    triplets = store.get_triplets()
    rel_labels = {r.label for _, r, _ in triplets}
    assert "KNOWS" not in rel_labels
    assert "LIKES" in rel_labels


def test_delete_cascades_edges(store):
    """Deleting a node via DETACH DELETE removes its incident edges."""
    alice = _make_entity("Alice")
    bob = _make_entity("Bob")
    store.upsert_nodes([alice, bob])
    store.upsert_relations([_make_rel(alice.id, "KNOWS", bob.id)])

    store.delete(ids=[alice.id])

    # Edge should be gone even though bob still exists
    triplets = store.get_triplets()
    assert triplets == []
    assert len(store.get(ids=[bob.id])) == 1


# ---------------------------------------------------------------------------
# 8. vector_query
# ---------------------------------------------------------------------------


def _norm(v: List[float]) -> List[float]:
    mag = math.sqrt(sum(x * x for x in v))
    return [x / mag for x in v]


def test_vector_query_returns_closest(store):
    e1 = EntityNode(name="Doc1", embedding=_norm([1.0, 0.0, 0.0]))
    e2 = EntityNode(name="Doc2", embedding=_norm([0.0, 1.0, 0.0]))
    e3 = EntityNode(name="Doc3", embedding=_norm([0.0, 0.0, 1.0]))
    store.upsert_nodes([e1, e2, e3])

    query = VectorStoreQuery(
        query_embedding=_norm([0.9, 0.1, 0.0]),
        similarity_top_k=1,
    )
    nodes, scores = store.vector_query(query)

    assert len(nodes) == 1
    assert nodes[0].id == e1.id
    assert scores[0] > 0.9


def test_vector_query_top_k(store):
    embs = [_norm([float(i), 0.0, 0.0]) for i in range(1, 6)]
    nodes = [EntityNode(name=f"Node{i}", embedding=embs[i - 1]) for i in range(1, 6)]
    store.upsert_nodes(nodes)

    query = VectorStoreQuery(
        query_embedding=_norm([1.0, 0.0, 0.0]),
        similarity_top_k=3,
    )
    result_nodes, scores = store.vector_query(query)

    assert len(result_nodes) == 3
    assert all(s >= 0.0 for s in scores)


def test_vector_query_no_embedding_returns_empty(store):
    store.upsert_nodes([_make_entity("NoEmbedding")])
    query = VectorStoreQuery(query_embedding=[1.0, 0.0], similarity_top_k=1)
    nodes, scores = store.vector_query(query)
    assert nodes == []
    assert scores == []


def test_vector_query_no_query_embedding(store):
    store.upsert_nodes([EntityNode(name="X", embedding=[0.1, 0.2])])
    query = VectorStoreQuery(query_embedding=None, similarity_top_k=1)
    nodes, scores = store.vector_query(query)
    assert nodes == []
    assert scores == []


# ---------------------------------------------------------------------------
# 9. structured_query (Cypher passthrough)
# ---------------------------------------------------------------------------


def test_structured_query_read(store):
    alice = _make_entity("Alice", color="blue")
    store.upsert_nodes([alice])

    rows = store.structured_query(
        "MATCH (n:entity) WHERE n.__name__ = 'Alice' RETURN n.__id__, n.color LIMIT 1"
    )
    assert len(rows) == 1
    assert rows[0].get("n.color") == "blue"


def test_structured_query_write(store):
    alice = _make_entity("Alice")
    store.upsert_nodes([alice])

    store.structured_query(
        "MATCH (n) WHERE n.__id__ = 'Alice' SET n.updated = 1"
    )
    retrieved = store.get(ids=[alice.id])
    assert retrieved[0].properties.get("updated") == 1


# ---------------------------------------------------------------------------
# 10. persist
# ---------------------------------------------------------------------------


def test_persist_does_not_raise(store, tmp_path):
    store.upsert_nodes([_make_entity("Alice")])
    # persist must not raise; path arg is accepted for API compat but not used
    store.persist(str(tmp_path / "snapshot"))


# ---------------------------------------------------------------------------
# 11. End-to-end: PropertyGraphIndex smoke test
# ---------------------------------------------------------------------------


def test_property_graph_index_smoke(tmp_path):
    """Insert documents with a fake embedding model, then retrieve via index.

    Uses MockLLM (from llama-index-core itself) so no external API key is
    required.  kg_extractors=[] means no path extraction is attempted, so
    documents are ingested only as ChunkNodes.  That is sufficient to verify
    the round-trip: documents in → nodes stored in mushroomdb → vector_query
    finds them back.
    """
    from llama_index.core import PropertyGraphIndex
    from llama_index.core.llms.mock import MockLLM
    from llama_index.core.schema import Document
    from llama_index.core.base.embeddings.base import BaseEmbedding

    class _FakeEmbed(BaseEmbedding):
        """Deterministic fake embedding that returns a fixed-length vector."""

        model_name: str = "fake-embed"
        embed_batch_size: int = 10

        def _get_query_embedding(self, query: str) -> List[float]:
            return [0.1, 0.2, 0.3, 0.4]

        def _get_text_embedding(self, text: str) -> List[float]:
            return [0.1, 0.2, 0.3, 0.4]

        async def _aget_query_embedding(self, query: str) -> List[float]:
            return self._get_query_embedding(query)

    pg_store = MushroomDBPropertyGraphStore(str(tmp_path))

    docs = [
        Document(text="Alice knows Bob.", id_="doc1"),
        Document(text="Bob likes Carol.", id_="doc2"),
    ]

    index = PropertyGraphIndex.from_documents(
        docs,
        llm=MockLLM(),
        property_graph_store=pg_store,
        embed_model=_FakeEmbed(),
        embed_kg_nodes=True,
        kg_extractors=[],
        use_async=False,
    )

    # The index must have ingested at least the two document chunks.
    stats = pg_store.client.stats()
    assert stats["nodes_live"] > 0

    # Vector query should find nodes (embeddings were stored alongside chunks).
    query = VectorStoreQuery(
        query_embedding=[0.1, 0.2, 0.3, 0.4],
        similarity_top_k=2,
    )
    nodes, scores = pg_store.vector_query(query)
    assert len(nodes) > 0
    assert all(isinstance(s, float) for s in scores)

    pg_store._db.close()
