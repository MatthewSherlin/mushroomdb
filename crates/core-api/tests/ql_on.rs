//! QL-ON: a write on a node that `OPTIONAL MATCH` did not find is skipped for
//! that row (openCypher: "Ignore null when setting / removing / deleting"), in
//! auto-commit and inside a transaction. A `CREATE` or `MERGE` of a
//! relationship from a null node is an error, and nothing is written.

use mushroomdb::{GraphError, RealFs, SharedDb, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;

type GraphDb = mushroomdb::GraphDb<RealFs>;
type Rows = Vec<Vec<Option<Value>>>;

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "graphdb-qlon-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn s(v: &str) -> Value {
    Value::Str(v.into())
}

fn none() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

fn rows(rs: &mushroomdb::ResultSet) -> Rows {
    (0..rs.len()).map(|i| rs.row(i).to_vec()).collect()
}

fn count(rs: &mushroomdb::ResultSet, col: &str) -> i64 {
    match rs.get(0, col) {
        Some(Value::Int(n)) => *n,
        other => panic!("column {col}: {other:?}"),
    }
}

/// Every node (key, label, properties) and every edge, sorted.
const NODES: &str = "MATCH (n) RETURN n.key AS k, labels(n) AS l, properties(n) AS p ORDER BY k";
const EDGES: &str =
    "MATCH (a)-[r]->(b) RETURN a.key AS a, type(r) AS t, b.key AS b ORDER BY a, t, b";

/// `a` -K-> `b`, and `c` with no outgoing K.
const SETUP: [&str; 4] = [
    "CREATE (:Person {id: 'a', v: 1})",
    "CREATE (:Person {id: 'b', v: 2})",
    "CREATE (:Person {id: 'c', v: 3})",
    "MATCH (a {id: 'a'}), (b {id: 'b'}) CREATE (a)-[:K]->(b)",
];

/// The statements that must each skip a null node: the write and the counter
/// it must leave at zero.
const NULL_WRITES: [(&str, &str); 9] = [
    ("OPTIONAL MATCH (n:Nobody) SET n.x = 1", "properties_set"),
    (
        "OPTIONAL MATCH (n:Nobody) SET n += {x: 1}",
        "properties_set",
    ),
    ("OPTIONAL MATCH (n:Nobody) SET n = {x: 1}", "properties_set"),
    ("OPTIONAL MATCH (n:Nobody) REMOVE n.v", "properties_set"),
    ("OPTIONAL MATCH (n:Nobody) DELETE n", "deleted"),
    ("OPTIONAL MATCH (n:Nobody) DETACH DELETE n", "deleted"),
    (
        "MATCH (p:Person {id: 'c'}) OPTIONAL MATCH (p)-[:K]->(q) SET q.x = 1",
        "properties_set",
    ),
    (
        "MATCH (p:Person {id: 'c'}) OPTIONAL MATCH (p)-[:K]->(q) DETACH DELETE q",
        "deleted",
    ),
    (
        "MATCH (p:Person {id: 'c'}) OPTIONAL MATCH (p)-[r:K]->(q) DELETE r",
        "deleted",
    ),
];

fn auto(name: &str) -> GraphDb {
    let mut db = GraphDb::open(&tmp(name)).unwrap();
    for q in SETUP {
        db.query_write(q, &none()).unwrap();
    }
    db
}

fn shared(name: &str) -> SharedDb {
    let db = SharedDb::open(&tmp(name)).unwrap();
    for q in SETUP {
        db.write().query_write(q, &none()).unwrap();
    }
    db
}

fn auto_state(db: &GraphDb) -> (Rows, Rows) {
    (
        rows(&db.query(NODES, &none()).unwrap()),
        rows(&db.query(EDGES, &none()).unwrap()),
    )
}

fn shared_state(db: &SharedDb) -> (Rows, Rows) {
    let r = db.reader();
    (
        rows(&r.query(NODES, &none()).unwrap()),
        rows(&r.query(EDGES, &none()).unwrap()),
    )
}

fn detail(e: GraphError) -> String {
    match e {
        GraphError::QueryError { detail } => detail,
        other => panic!("expected QueryError, got {other:?}"),
    }
}

#[test]
fn a_write_on_a_null_node_is_skipped_in_auto_commit() {
    let mut db = auto("auto-skip");
    let before = auto_state(&db);
    for (q, counter) in NULL_WRITES {
        let rs = db
            .query_write(q, &none())
            .unwrap_or_else(|e| panic!("{q}: {e}"));
        assert_eq!(count(&rs, counter), 0, "{q}");
        assert_eq!(auto_state(&db), before, "{q}");
    }
}

#[test]
fn a_write_on_a_null_node_is_skipped_inside_a_transaction() {
    let db = shared("tx-skip");
    let before = shared_state(&db);
    for (q, counter) in NULL_WRITES {
        let mut tx = db.begin_transaction();
        let rs = tx
            .execute(q, &none())
            .unwrap_or_else(|e| panic!("{q}: {e}"));
        assert_eq!(count(&rs, counter), 0, "{q}");
        tx.commit().unwrap();
        assert_eq!(shared_state(&db), before, "{q}");
    }
}

#[test]
fn only_the_rows_with_a_node_are_written() {
    let mut db = auto("mixed");
    let rs = db
        .query_write(
            "MATCH (p:Person) OPTIONAL MATCH (p)-[:K]->(q) SET q.seen = true",
            &none(),
        )
        .unwrap();
    assert_eq!(count(&rs, "properties_set"), 1, "only b is a K target");
    let got = rows(
        &db.query(
            "MATCH (n) RETURN n.id AS id, n.seen AS seen ORDER BY id",
            &none(),
        )
        .unwrap(),
    );
    assert_eq!(
        got,
        vec![
            vec![Some(s("a")), None],
            vec![Some(s("b")), Some(Value::Bool(true))],
            vec![Some(s("c")), None],
        ]
    );
    // REMOVE and DELETE skip the same way, per row.
    let rs = db
        .query_write(
            "MATCH (p:Person) OPTIONAL MATCH (p)-[:K]->(q) REMOVE q.seen",
            &none(),
        )
        .unwrap();
    assert_eq!(count(&rs, "properties_set"), 1);
    let rs = db
        .query_write(
            "MATCH (p:Person) OPTIONAL MATCH (p)-[:K]->(q) DETACH DELETE q",
            &none(),
        )
        .unwrap();
    // The node and the relationship it took with it, as DETACH DELETE counts.
    assert_eq!(count(&rs, "deleted"), 2);
    assert!(!db.has_node("b") && db.has_node("a") && db.has_node("c"));
}

#[test]
fn a_return_after_the_skipped_write_gives_the_null() {
    let mut db = auto("return");
    let rs = db
        .query_write("OPTIONAL MATCH (a:DoesNotExist) DELETE a RETURN a", &none())
        .unwrap();
    assert_eq!(rows(&rs), vec![vec![None]]);
    let rs = db
        .query_write(
            "OPTIONAL MATCH (a:DoesNotExist) SET a.x = 1 RETURN a.x AS x",
            &none(),
        )
        .unwrap();
    assert_eq!(rows(&rs), vec![vec![None]]);
}

#[test]
fn a_leading_optional_match_writes_when_it_finds_the_node() {
    let mut db = auto("found");
    let rs = db
        .query_write("OPTIONAL MATCH (n:Person {id: 'c'}) SET n.x = 1", &none())
        .unwrap();
    assert_eq!(count(&rs, "properties_set"), 1);
    assert_eq!(db.get_prop("c", "x"), Some(Value::Int(1)));
}

#[test]
fn a_relationship_created_from_a_null_node_is_an_error_and_writes_nothing() {
    let mut db = auto("create-null");
    let before = auto_state(&db);
    for q in [
        "OPTIONAL MATCH (n:Nobody) CREATE (n)-[:R]->(:T {id: 'z'})",
        "MATCH (p:Person {id: 'c'}) OPTIONAL MATCH (p)-[:K]->(q) CREATE (q)-[:R]->(p)",
        "OPTIONAL MATCH (n:Nobody) MERGE (n)-[:R]->(:T {id: 'z'})",
    ] {
        let e = db.query_write(q, &none()).unwrap_err();
        let d = detail(e);
        assert!(d.contains("null"), "{q}: {d}");
        assert_eq!(auto_state(&db), before, "{q}");
    }
}

#[test]
fn a_rollback_leaves_the_store_as_it_was() {
    let db = shared("rollback");
    let before = shared_state(&db);
    let mut tx = db.begin_transaction();
    tx.execute(
        "MATCH (p:Person) OPTIONAL MATCH (p)-[:K]->(q) SET q.seen = true",
        &none(),
    )
    .unwrap();
    tx.execute("OPTIONAL MATCH (n:Nobody) DELETE n", &none())
        .unwrap();
    tx.rollback();
    assert_eq!(shared_state(&db), before);
}

#[test]
fn a_read_only_store_still_refuses_the_statement() {
    let dir = tmp("read-only");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.query_write("CREATE (:Person {id: 'a'})", &none())
            .unwrap();
    }
    let db = mushroomdb::SharedDb::open(&dir).unwrap();
    // The read path never takes a write statement.
    let e = db
        .reader()
        .query("OPTIONAL MATCH (n:Nobody) SET n.x = 1", &none())
        .unwrap_err();
    assert!(matches!(e, GraphError::QueryError { .. }), "{e:?}");
}

#[test]
fn deleting_a_relationship_that_is_there_is_refused_not_skipped() {
    let mut db = auto("rel-there");
    let before = auto_state(&db);
    let e = db
        .query_write(
            "MATCH (p:Person {id: 'a'}) OPTIONAL MATCH (p)-[r:K]->(q) DELETE r",
            &none(),
        )
        .unwrap_err();
    assert!(detail(e).contains("DELETE of the relationship"));
    assert_eq!(auto_state(&db), before);
}
