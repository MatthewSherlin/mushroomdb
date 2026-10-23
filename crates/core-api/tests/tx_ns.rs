//! TX-NS (defect-ledger rows 148 and 149), through the public API:
//!
//! - every query entry point, run inside `with_entity_detail`, returns the
//!   node, relationship or path behind each bare-variable cell, read from the
//!   state the statement saw (a role's mask, a transaction's own writes, a
//!   write's committed result); outside it nothing changes, so `RETURN n` is
//!   still the key string for HTTP and Python;
//! - every write statement reports exact `WriteStats`, auto-commit and inside
//!   a transaction alike, while the one-row write result keeps its columns.

use mushroomdb::{
    schema::Schema, with_entity_detail, Entity, EntityKind, NodeEntity, RealFs, ResultSet, RoleDef,
    SharedDb, Value, WriteStats,
};
use std::collections::BTreeMap;
use std::path::PathBuf;

type GraphDb = mushroomdb::GraphDb<RealFs>;

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "graphdb-txns-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn s(v: &str) -> Value {
    Value::Str(v.into())
}

fn i(v: i64) -> Value {
    Value::Int(v)
}

fn none() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

fn write(db: &mut GraphDb, q: &str) -> ResultSet {
    db.query_write(q, &none())
        .unwrap_or_else(|e| panic!("{q}: {e}"))
}

/// ada -KNOWS-> bob, ada -OWNS-> sec (a Secret).
fn graph(name: &str) -> GraphDb {
    let mut db = GraphDb::open(&tmp(name)).unwrap();
    for q in [
        "CREATE (:Person {id: 'ada', name: 'Ada'})",
        "CREATE (:Person {id: 'bob', name: 'Bob'})",
        "CREATE (:Secret {id: 'sec', code: 'x'})",
        "MATCH (a {id: 'ada'}), (b {id: 'bob'}) CREATE (a)-[:KNOWS]->(b)",
        "MATCH (a {id: 'ada'}), (b {id: 'sec'}) CREATE (a)-[:OWNS]->(b)",
    ] {
        write(&mut db, q);
    }
    db
}

fn props(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

fn node_at(rs: &ResultSet, row: usize, col: usize) -> &NodeEntity {
    match rs.entity(row, col) {
        Some(Entity::Node(n)) => n,
        other => panic!("cell ({row}, {col}): {other:?}"),
    }
}

#[test]
fn return_n_is_unchanged_without_detail_and_a_node_with_it() {
    let db = graph("plain");
    let q = "MATCH (n:Person {id: 'ada'}) RETURN n";
    let plain = db.query(q, &none()).unwrap();
    assert_eq!(plain.row(0), &[Some(s("ada"))]);
    assert!(!plain.has_entities());
    assert_eq!(plain.column_kind(0), Some(EntityKind::Node));
    let rs = with_entity_detail(|| db.query(q, &none())).unwrap();
    assert_eq!(rs, plain);
    let n = node_at(&rs, 0, 0);
    assert_eq!(n.label, "Person");
    assert_eq!(n.properties, props(&[("id", s("ada")), ("name", s("Ada"))]));
}

#[test]
fn a_role_sees_only_its_nodes_and_no_relationship_to_a_hidden_one() {
    let mut db = graph("role");
    db.apply_schema(&Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![],
        roles: vec![RoleDef {
            name: "people".into(),
            keys: vec![],
            labels: vec!["Person".into()],
            visible_where: None,
            namespaces: None,
            write: None,
        }],
    })
    .unwrap();
    let mask = db.mask_for_role("people").unwrap();
    let q = "MATCH (a {id: 'ada'})-[r]->(b) RETURN r";
    let rs = with_entity_detail(|| db.query_masked(q, &none(), &mask)).unwrap();
    assert_eq!(
        rs.len(),
        1,
        "the OWNS relationship to the hidden Secret is absent"
    );
    let Some(Entity::Relationship(r)) = rs.entity(0, 0) else {
        panic!("{:?}", rs.entity(0, 0))
    };
    assert_eq!((r.rel_type.as_str(), r.end.as_str()), ("KNOWS", "bob"));
    // The snapshot reader gives the same answer.
    let snap = db.reader();
    let mask = snap.mask_for_role("people").unwrap();
    let rs = with_entity_detail(|| snap.query_masked(q, &none(), &mask)).unwrap();
    assert_eq!(rs.len(), 1);
    assert!(matches!(rs.entity(0, 0), Some(Entity::Relationship(_))));
}

#[test]
fn a_transaction_returns_its_own_buffered_node_and_a_path() {
    let shared = SharedDb::open(&tmp("tx")).unwrap();
    shared
        .write()
        .query_write("CREATE (:P {id: 'a', v: 1})", &none())
        .unwrap();
    let mut tx = shared.begin_transaction();
    with_entity_detail(|| {
        let made = tx
            .execute("CREATE (n:P {id: 'b', v: 2}) RETURN n", &none())
            .unwrap();
        assert_eq!(node_at(&made, 0, 0).properties.get("v"), Some(&i(2)));
        assert_eq!(made.stats().unwrap().nodes_created, 1);
        tx.execute(
            "MATCH (a {id: 'a'}), (b {id: 'b'}) CREATE (a)-[:K]->(b)",
            &none(),
        )
        .unwrap();
        let read = tx
            .execute("MATCH p = (a {id: 'a'})-[:K]->(b) RETURN p", &none())
            .unwrap();
        let Some(Entity::Path(p)) = read.entity(0, 0) else {
            panic!("{:?}", read.entity(0, 0))
        };
        assert_eq!(p.nodes[1].key, "b");
        assert_eq!(p.relationships[0].rel_type, "K");
    });
    tx.rollback();
}

#[test]
fn a_write_return_carries_the_written_state() {
    let mut db = graph("write-return");
    let set = with_entity_detail(|| {
        db.query_write("MATCH (n {id: 'ada'}) SET n.age = 36 RETURN n", &none())
    })
    .unwrap();
    assert_eq!(set.column_kind(0), Some(EntityKind::Node));
    assert_eq!(node_at(&set, 0, 0).properties.get("age"), Some(&i(36)));
    assert_eq!(set.stats().unwrap().properties_set, 1);
    let merged = with_entity_detail(|| {
        db.query_write(
            "MATCH (a {id: 'ada'}) MERGE (a)-[r:LIKES]->(c:Person {id: 'cam'}) RETURN r, c",
            &none(),
        )
    })
    .unwrap();
    let Some(Entity::Relationship(r)) = merged.entity(0, 0) else {
        panic!("{:?}", merged.entity(0, 0))
    };
    assert_eq!((r.start.as_str(), r.end.as_str()), ("ada", "cam"));
    assert_eq!(node_at(&merged, 0, 1).label, "Person");
}

fn stats(db: &mut GraphDb, q: &str) -> WriteStats {
    *write(db, q)
        .stats()
        .unwrap_or_else(|| panic!("{q}: no stats"))
}

#[test]
fn every_write_statement_reports_exact_stats() {
    let mut db = GraphDb::open(&tmp("stats")).unwrap();
    let created = write(
        &mut db,
        "CREATE (:P {id: 'a', name: 'A'})-[:K]->(:P {id: 'b'})",
    );
    assert_eq!(
        created.stats(),
        Some(&WriteStats {
            nodes_created: 2,
            labels_added: 2,
            properties_set: 3,
            relationships_created: 1,
            ..WriteStats::default()
        })
    );
    // The write row keeps its documented columns and meaning.
    assert_eq!(
        created.row(0),
        &[Some(i(2)), Some(i(0)), Some(i(0)), Some(i(1))]
    );
    assert_eq!(
        stats(&mut db, "MATCH (n:P) SET n.x = 1"),
        WriteStats {
            properties_set: 2,
            ..WriteStats::default()
        }
    );
    assert_eq!(
        stats(&mut db, "MERGE (n:P {id: 'c'}) ON CREATE SET n.y = 1"),
        WriteStats {
            nodes_created: 1,
            labels_added: 1,
            properties_set: 2,
            ..WriteStats::default()
        }
    );
    assert_eq!(
        stats(&mut db, "MERGE (n:P {id: 'c'}) ON MATCH SET n.y = 2"),
        WriteStats {
            properties_set: 1,
            ..WriteStats::default()
        }
    );
    assert_eq!(
        stats(&mut db, "UNWIND ['d', 'e'] AS k CREATE (:Q {id: k})"),
        WriteStats {
            nodes_created: 2,
            labels_added: 2,
            properties_set: 2,
            ..WriteStats::default()
        }
    );
    assert_eq!(
        stats(
            &mut db,
            "MATCH (a:P {id: 'a'})-[r:K]->(b:P {id: 'b'}) DELETE r"
        ),
        WriteStats {
            relationships_deleted: 1,
            ..WriteStats::default()
        }
    );
    write(
        &mut db,
        "MATCH (a {id: 'a'}), (b {id: 'b'}) CREATE (a)-[:K]->(b)",
    );
    let detached = write(&mut db, "MATCH (n:P {id: 'a'}) DETACH DELETE n");
    assert_eq!(
        detached.stats(),
        Some(&WriteStats {
            nodes_deleted: 1,
            relationships_deleted: 1,
            ..WriteStats::default()
        })
    );
    assert_eq!(detached.get(0, "deleted"), Some(&i(2)), "row unchanged");
    assert_eq!(
        stats(&mut db, "UNWIND [1] AS x MATCH (n:Q) DETACH DELETE n"),
        WriteStats {
            nodes_deleted: 2,
            ..WriteStats::default()
        }
    );
    assert_eq!(
        stats(&mut db, "CREATE INDEX p_name FOR (n:P) ON (n.name)"),
        WriteStats {
            indexes_added: 1,
            ..WriteStats::default()
        }
    );
    assert!(!stats(
        &mut db,
        "CREATE INDEX p_name IF NOT EXISTS FOR (n:P) ON (n.name)"
    )
    .contains_updates());
    assert_eq!(stats(&mut db, "DROP INDEX p_name").indexes_removed, 1);
    assert_eq!(
        stats(
            &mut db,
            "CREATE CONSTRAINT p_id FOR (n:P) REQUIRE n.code IS UNIQUE"
        )
        .constraints_added,
        1
    );
    assert_eq!(
        stats(&mut db, "DROP CONSTRAINT p_id").constraints_removed,
        1
    );
}

#[test]
fn a_transaction_reports_the_same_stats_as_auto_commit() {
    let statements = [
        "CREATE (:P {id: 'a', name: 'A'})-[:K]->(:P {id: 'b'})",
        "MATCH (n:P) SET n.x = 1",
        "MERGE (n:P {id: 'c'}) ON CREATE SET n.y = 1",
        "MATCH (a {id: 'b'}) MERGE (a)-[:L]->(:P {id: 'd'})",
        "MATCH (a:P {id: 'a'})-[r:K]->(b:P {id: 'b'}) DELETE r",
        "MATCH (n:P {id: 'b'}) DETACH DELETE n",
    ];
    let mut auto = GraphDb::open(&tmp("stats-auto")).unwrap();
    let expected: Vec<WriteStats> = statements.iter().map(|q| stats(&mut auto, q)).collect();
    let shared = SharedDb::open(&tmp("stats-tx")).unwrap();
    let mut tx = shared.begin_transaction();
    for (q, want) in statements.iter().zip(&expected) {
        let rs = tx
            .execute(q, &none())
            .unwrap_or_else(|e| panic!("{q}: {e}"));
        assert_eq!(rs.stats(), Some(want), "{q}");
    }
    tx.commit().unwrap();
}

/// Ledger row 150: the write row's `properties_set` counts a `MERGE`'s
/// `ON CREATE SET` / `ON MATCH SET` items, node and relationship form, on
/// every path. (Released 0.9.x reported 0.)
#[test]
fn row_150_merge_set_items_are_counted_in_properties_set() {
    let statements = [
        ("MERGE (n:P {id: 'a'}) ON CREATE SET n.x = 1, n.y = 2", 2),
        (
            "MERGE (n:P {id: 'a'}) ON CREATE SET n.x = 1 ON MATCH SET n.z = 3",
            1,
        ),
        ("MERGE (n:P {id: 'a'})", 0),
        (
            "MERGE (a:P {id: 'a'})-[:K]->(b:P {id: 'b'}) ON CREATE SET b.v = 1, a.w = 2",
            2,
        ),
        (
            "MERGE (a:P {id: 'a'})-[:K]->(b:P {id: 'b'}) ON MATCH SET b.v = 5",
            1,
        ),
        (
            "UNWIND ['c'] AS k MERGE (n:P {id: k}) ON CREATE SET n.x = 1",
            1,
        ),
    ];
    let mut auto = GraphDb::open(&tmp("row150-auto")).unwrap();
    let shared = SharedDb::open(&tmp("row150-tx")).unwrap();
    let mut tx = shared.begin_transaction();
    for (q, want) in statements {
        let rs = write(&mut auto, q);
        assert_eq!(
            rs.get(0, "properties_set"),
            Some(&i(want)),
            "auto-commit: {q}"
        );
        let rs = tx
            .execute(q, &none())
            .unwrap_or_else(|e| panic!("{q}: {e}"));
        assert_eq!(
            rs.get(0, "properties_set"),
            Some(&i(want)),
            "in a transaction: {q}"
        );
    }
    tx.commit().unwrap();
}
