"""The Python graph quickstart, using the actual public binding API."""
from mushroomdb import GraphDb


def test_graph_quickstart_returns_exact_people_and_relationship(tmp_path):
    with GraphDb.open(tmp_path / "graph") as db:
        db.insert_node("Person", "alice", {"name": "Alice"})
        db.insert_node("Person", "bob", {"name": "Bob"})
        assert db.insert_edge("KNOWS", "alice", "bob") is True
        rows = db.query("MATCH (a:Person)-[:KNOWS]->(b) RETURN a.name, b.name")
        assert rows == [{"a.name": "Alice", "b.name": "Bob"}]
        assert db.stats()["nodes_live"] == 2
        assert db.stats()["edges"] == 1
        assert db.node_info("alice")["label"] == "Person"
        assert db.node_info("alice")["props"]["name"] == "Alice"
        assert db.node_info("bob")["props"]["name"] == "Bob"
        assert db.node_edges("alice") == [
            {"edge_type": "KNOWS", "src_key": "alice", "dst_key": "bob", "derived": False}
        ]
