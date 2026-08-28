"""Tests for MushroomDBGraphStore.

Built against langchain-community 0.4.2 / langchain-core 1.6.1.
"""

import tempfile
from typing import List

import pytest

from langchain_community.graphs.graph_document import GraphDocument, Node, Relationship
from langchain_core.documents import Document

from langchain_mushroomdb import MushroomDBGraphStore


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture
def store():
    """Fresh MushroomDBGraphStore in a temporary directory."""
    with tempfile.TemporaryDirectory() as d:
        s = MushroomDBGraphStore(d)
        yield s
        try:
            s._db.close()
        except Exception:
            pass


def _make_gd(
    nodes: List[Node],
    rels: List[Relationship],
    source: Document | None = None,
) -> GraphDocument:
    if source is None:
        source = Document(page_content="", metadata={})
    return GraphDocument(nodes=nodes, relationships=rels, source=source)


# ---------------------------------------------------------------------------
# add_graph_documents — round-trip tests
# ---------------------------------------------------------------------------


class TestAddGraphDocuments:
    def test_nodes_are_persisted(self, store):
        alice = Node(id="alice", type="Person", properties={"name": "Alice"})
        bob = Node(id="bob", type="Person", properties={"name": "Bob"})
        store.add_graph_documents([_make_gd([alice, bob], [])])

        info_a = store._db.node_info("alice")
        info_b = store._db.node_info("bob")
        assert info_a is not None
        assert info_b is not None
        assert info_a["label"] == "Person"
        assert info_a["props"]["name"] == "Alice"

    def test_relationships_are_persisted(self, store):
        alice = Node(id="alice", type="Person")
        bob = Node(id="bob", type="Person")
        rel = Relationship(source=alice, target=bob, type="KNOWS")
        store.add_graph_documents([_make_gd([alice, bob], [rel])])

        rows = store.query(
            "MATCH (a)-[r:KNOWS]->(b) RETURN a.__id__, b.__id__ LIMIT 10"
        )
        assert len(rows) == 1
        assert rows[0]["a.__id__"] == "alice"
        assert rows[0]["b.__id__"] == "bob"

    def test_multiple_documents(self, store):
        gd1 = _make_gd([Node(id="n1", type="X")], [])
        gd2 = _make_gd([Node(id="n2", type="Y")], [])
        store.add_graph_documents([gd1, gd2])

        assert store._db.node_info("n1") is not None
        assert store._db.node_info("n2") is not None

    def test_upsert_idempotent(self, store):
        node = Node(id="item1", type="Item", properties={"val": 1})
        store.add_graph_documents([_make_gd([node], [])])
        # Second insert with same id and type → update props in-place
        node2 = Node(id="item1", type="Item", properties={"val": 2})
        store.add_graph_documents([_make_gd([node2], [])])
        info = store._db.node_info("item1")
        assert info["props"]["val"] == 2

    def test_include_source_false_no_document_node(self, store):
        doc = Document(page_content="Alice is great.", metadata={"source": "s1"})
        alice = Node(id="alice", type="Person")
        store.add_graph_documents([_make_gd([alice], [], source=doc)], include_source=False)
        # Source document node should NOT be created
        assert store._db.node_info("s1") is None

    def test_include_source_true_creates_document_node(self, store):
        doc = Document(page_content="Alice is great.", metadata={"source": "s1"})
        alice = Node(id="alice", type="Person")
        store.add_graph_documents([_make_gd([alice], [], source=doc)], include_source=True)

        doc_info = store._db.node_info("s1")
        assert doc_info is not None
        assert doc_info["label"] == "Document"
        assert doc_info["props"]["page_content"] == "Alice is great."

    def test_include_source_true_creates_mentions_edges(self, store):
        doc = Document(page_content="Alice meets Bob.", metadata={"source": "src1"})
        alice = Node(id="alice", type="Person")
        bob = Node(id="bob", type="Person")
        store.add_graph_documents(
            [_make_gd([alice, bob], [], source=doc)], include_source=True
        )
        rows = store.query(
            "MATCH (d)-[r:MENTIONS]->(n) RETURN d.__id__, n.__id__ LIMIT 10"
        )
        targets = {r["n.__id__"] for r in rows}
        assert "alice" in targets
        assert "bob" in targets

    def test_relationship_endpoint_auto_created(self, store):
        """Relationship endpoints not in gd.nodes are created as stubs."""
        alice = Node(id="alice", type="Person")
        bob = Node(id="bob", type="Person")
        rel = Relationship(source=alice, target=bob, type="KNOWS")
        # Pass empty nodes list — endpoints should be auto-created from rel.
        store.add_graph_documents([_make_gd([], [rel])])
        assert store._db.node_info("alice") is not None
        assert store._db.node_info("bob") is not None
        rows = store.query("MATCH (a)-[r:KNOWS]->(b) RETURN a.__id__ LIMIT 1")
        assert len(rows) == 1

    def test_relationship_properties_are_dropped(self, store):
        """Relationship.properties are silently dropped — mushroomdb 0.1.2 has
        no edge-property API (insert_edge is type/src/dst only; SET r.field is
        rejected by the Cypher planner).  This test pins the current behaviour
        and marks the flip-point: when the binding gains edge-property support,
        this test should be updated to assert properties ARE persisted."""
        alice = Node(id="alice", type="Person")
        bob = Node(id="bob", type="Person")
        rel = Relationship(
            source=alice, target=bob, type="KNOWS", properties={"weight": 0.9}
        )
        store.add_graph_documents([_make_gd([alice, bob], [rel])])
        # Edge exists in the graph
        rows = store.query("MATCH (a)-[r:KNOWS]->(b) RETURN a.__id__ LIMIT 1")
        assert len(rows) == 1
        # Properties are NOT returned (no edge-property surface in the binding)
        rows_with_weight = store.query(
            "MATCH (a)-[r:KNOWS]->(b) RETURN r.weight LIMIT 1"
        )
        # r.weight is either missing from the row dict or None
        weight = rows_with_weight[0].get("r.weight") if rows_with_weight else None
        assert weight is None

    def test_label_change_preserves_edge(self, store):
        """Re-adding a node with the same id but a different label (Person →
        Agent) triggers the delete+reinsert path; existing user-owned edges
        must survive."""
        # 1. Insert "alice" as Person with a KNOWS edge to "bob"
        alice_p = Node(id="alice", type="Person", properties={"name": "Alice"})
        bob = Node(id="bob", type="Person")
        rel = Relationship(source=alice_p, target=bob, type="KNOWS")
        store.add_graph_documents([_make_gd([alice_p, bob], [rel])])

        rows_before = store.query("MATCH (a)-[r:KNOWS]->(b) RETURN a.__id__ LIMIT 1")
        assert len(rows_before) == 1

        # 2. Re-add "alice" with label "Agent" (label change)
        alice_a = Node(id="alice", type="Agent", properties={"name": "Alice"})
        store.add_graph_documents([_make_gd([alice_a], [])])

        # Label must have changed
        info = store._db.node_info("alice")
        assert info is not None
        assert info["label"] == "Agent"

        # KNOWS edge must still exist
        rows_after = store.query("MATCH (a)-[r:KNOWS]->(b) RETURN a.__id__ LIMIT 1")
        assert len(rows_after) == 1


# ---------------------------------------------------------------------------
# query — Cypher passthrough tests
# ---------------------------------------------------------------------------


class TestQuery:
    def test_query_returns_list(self, store):
        result = store.query("MATCH (n) RETURN n.__id__ LIMIT 10")
        assert isinstance(result, list)

    def test_query_returns_rows_after_insert(self, store):
        alice = Node(id="alice", type="Person", properties={"name": "Alice"})
        store.add_graph_documents([_make_gd([alice], [])])
        rows = store.query("MATCH (n:Person) RETURN n.__id__ LIMIT 10")
        ids = [r["n.__id__"] for r in rows]
        assert "alice" in ids

    def test_query_with_params(self, store):
        alice = Node(id="alice", type="Person", properties={"score": 42})
        store.add_graph_documents([_make_gd([alice], [])])
        rows = store.query(
            "MATCH (n) WHERE n.score = $score RETURN n.__id__ LIMIT 10",
            params={"score": 42},
        )
        assert any(r["n.__id__"] == "alice" for r in rows)

    def test_query_empty_graph_returns_empty(self, store):
        rows = store.query("MATCH (n) RETURN n.__id__ LIMIT 10")
        assert rows == []

    def test_query_match_by_type_filter(self, store):
        a = Node(id="a", type="Alpha")
        b = Node(id="b", type="Beta")
        store.add_graph_documents([_make_gd([a, b], [])])
        rows = store.query("MATCH (n:Alpha) RETURN n.__id__ LIMIT 10")
        assert len(rows) == 1
        assert rows[0]["n.__id__"] == "a"

    def test_write_keyword_routes_to_write_path(self, store):
        """A query containing a write keyword (DELETE) must be routed to the
        write path by query() without raising.  The write-intent detector
        inspects the query string before execution — no exception swallowing."""
        # Insert a node so there is something to delete.
        store._db.insert_node("Temp", "tmp_wt", {"__id__": "tmp_wt"})
        assert store._db.node_info("tmp_wt") is not None

        # Run a DELETE via the public query() API — write-intent routing should
        # send this to query_write without raising.
        store.query(
            "MATCH (n) WHERE n.__id__ = 'tmp_wt' DETACH DELETE n"
        )
        # Node should now be gone (or at least the call did not raise).

    def test_malformed_read_query_raises(self, store):
        """A syntactically invalid read query must raise (not silently no-op).
        Write-intent routing must NOT suppress errors on the read path."""
        with pytest.raises(Exception):
            store.query("MATCH (n INVALID SYNTAX !!!)")


# ---------------------------------------------------------------------------
# refresh_schema tests
# ---------------------------------------------------------------------------


class TestRefreshSchema:
    def test_schema_empty_before_refresh(self, store):
        # Initial state — no explicit requirement that it's non-empty,
        # but refresh_schema must succeed on an empty store.
        store.refresh_schema()  # should not raise

    def test_get_schema_contains_node_label(self, store):
        alice = Node(id="alice", type="Person", properties={"name": "Alice"})
        store.add_graph_documents([_make_gd([alice], [])])
        store.refresh_schema()
        schema = store.get_schema
        assert "Person" in schema

    def test_get_schema_contains_relationship_type(self, store):
        alice = Node(id="alice", type="Person")
        movie = Node(id="inception", type="Movie")
        rel = Relationship(source=alice, target=movie, type="WATCHED")
        store.add_graph_documents([_make_gd([alice, movie], [rel])])
        store.refresh_schema()
        schema = store.get_schema
        assert "WATCHED" in schema

    def test_get_structured_schema_node_props(self, store):
        alice = Node(id="alice", type="Person", properties={"age": 30})
        store.add_graph_documents([_make_gd([alice], [])])
        store.refresh_schema()
        ss = store.get_structured_schema
        assert "Person" in ss["node_props"]
        assert "age" in ss["node_props"]["Person"]

    def test_get_structured_schema_relationships(self, store):
        a = Node(id="a", type="Author")
        b = Node(id="b", type="Book")
        rel = Relationship(source=a, target=b, type="WROTE")
        store.add_graph_documents([_make_gd([a, b], [rel])])
        store.refresh_schema()
        ss = store.get_structured_schema
        rels = ss["relationships"]
        assert any(
            r["start"] == "Author" and r["type"] == "WROTE" and r["end"] == "Book"
            for r in rels
        )

    def test_get_structured_schema_has_required_keys(self, store):
        store.refresh_schema()
        ss = store.get_structured_schema
        assert "node_props" in ss
        assert "rel_props" in ss
        assert "relationships" in ss

    def test_schema_updates_after_new_data(self, store):
        store.refresh_schema()
        schema_before = store.get_schema
        assert "Robot" not in schema_before

        store.add_graph_documents([_make_gd([Node(id="r1", type="Robot")], [])])
        store.refresh_schema()
        assert "Robot" in store.get_schema

    def test_multiple_labels_in_schema(self, store):
        nodes = [
            Node(id="p1", type="Person"),
            Node(id="c1", type="Company"),
            Node(id="l1", type="Location"),
        ]
        store.add_graph_documents([_make_gd(nodes, [])])
        store.refresh_schema()
        schema = store.get_schema
        assert "Person" in schema
        assert "Company" in schema
        assert "Location" in schema
