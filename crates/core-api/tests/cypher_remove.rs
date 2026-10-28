//! `MATCH … REMOVE n.prop [, …]` (QL-05): property removal through the same
//! write path as `remove_prop`, so rules, the WAL and the namespace guard all
//! see it.

use core_storage::wal::{decode_all, WalRecord};
use mushroomdb::{
    schema::Schema, BatchOp, GraphError, Predicate, RealFs, RoleDef, RuleDef, SharedDb, Value,
    WriteScope,
};
use std::collections::BTreeMap;
use std::path::PathBuf;

type GraphDb = mushroomdb::GraphDb<RealFs>;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "graphdb-cypher-remove-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn p() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

fn same_region_rule() -> RuleDef {
    RuleDef {
        name: "same-region".into(),
        src_label: "P".into(),
        dst_label: "P".into(),
        predicate: Predicate::FieldEqual {
            field: "region".into(),
        },
        edge_type: "SAME".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
        namespace: None,
    }
}

fn seed(db: &mut GraphDb) {
    for (id, a, b) in [("alice", 1, 10), ("bob", 2, 20), ("carol", 3, 30)] {
        db.query_write(
            &format!("CREATE (n:P {{id: '{id}', a: {a}, b: {b}, c: {a}}})"),
            &p(),
        )
        .unwrap();
    }
}

fn run(db: &mut GraphDb, q: &str) -> mushroomdb::ResultSet {
    db.query_write(q, &p()).unwrap()
}

/// Every node's props, sorted, as text: the state the probes compare.
fn dump(db: &GraphDb) -> Vec<String> {
    let rs = db
        .query(
            "MATCH (n:P) RETURN n.id AS k, n.a AS a, n.b AS b, n.c AS c",
            &p(),
        )
        .unwrap();
    let mut rows: Vec<String> = (0..rs.len()).map(|i| format!("{:?}", rs.row(i))).collect();
    rows.sort();
    rows
}

#[test]
fn remove_one_property_leaves_the_rest() {
    let mut db = GraphDb::open(&tmp("one")).unwrap();
    seed(&mut db);
    let rs = run(&mut db, "MATCH (n:P) WHERE n.id = 'alice' REMOVE n.a");
    assert_eq!(rs.get(0, "properties_set"), Some(&Value::Int(1)));
    assert_eq!(db.get_prop("alice", "a"), None);
    assert_eq!(db.get_prop("alice", "b"), Some(Value::Int(10)));
    assert_eq!(db.get_prop("bob", "a"), Some(Value::Int(2)));
}

#[test]
fn remove_several_properties_in_one_statement() {
    let mut db = GraphDb::open(&tmp("several")).unwrap();
    seed(&mut db);
    let rs = run(&mut db, "MATCH (n:P) WHERE n.id = 'alice' REMOVE n.a, n.b");
    assert_eq!(rs.get(0, "properties_set"), Some(&Value::Int(2)));
    assert_eq!(db.get_prop("alice", "a"), None);
    assert_eq!(db.get_prop("alice", "b"), None);
    assert_eq!(db.get_prop("alice", "c"), Some(Value::Int(1)));
}

#[test]
fn remove_a_missing_property_is_a_no_op() {
    let dir = tmp("missing");
    let mut db = GraphDb::open(&dir).unwrap();
    seed(&mut db);
    let before = db.commit_seq();
    let rs = run(&mut db, "MATCH (n:P) WHERE n.id = 'alice' REMOVE n.missing");
    assert_eq!(rs.get(0, "properties_set"), Some(&Value::Int(0)));
    assert_eq!(db.commit_seq(), before, "nothing to remove: nothing logged");
    assert_eq!(db.get_prop("alice", "a"), Some(Value::Int(1)));
}

#[test]
fn remove_over_many_matched_nodes_and_where_filter() {
    let mut db = GraphDb::open(&tmp("many")).unwrap();
    seed(&mut db);
    run(&mut db, "MATCH (n:P) WHERE n.a >= 2 REMOVE n.b");
    assert_eq!(db.get_prop("alice", "b"), Some(Value::Int(10)));
    assert_eq!(db.get_prop("bob", "b"), None);
    assert_eq!(db.get_prop("carol", "b"), None);
}

#[test]
fn remove_a_property_of_a_node_matched_through_a_relationship() {
    let mut db = GraphDb::open(&tmp("rel")).unwrap();
    seed(&mut db);
    db.insert_edge("KNOWS", "alice", "bob").unwrap();
    run(
        &mut db,
        "MATCH (x:P)-[:KNOWS]->(y:P) WHERE x.id = 'alice' REMOVE y.a",
    );
    assert_eq!(db.get_prop("bob", "a"), None);
    assert_eq!(db.get_prop("alice", "a"), Some(Value::Int(1)));
}

#[test]
fn remove_with_return_projects_the_post_write_state() {
    let mut db = GraphDb::open(&tmp("return")).unwrap();
    seed(&mut db);
    let rs = run(
        &mut db,
        "MATCH (n:P) WHERE n.id = 'alice' REMOVE n.a RETURN n.id AS k, n.a AS a, n.b AS b",
    );
    assert_eq!(rs.len(), 1);
    assert_eq!(rs.get(0, "k"), Some(&Value::Str("alice".into())));
    assert_eq!(rs.get(0, "a"), None, "removed property reads as null");
    assert_eq!(rs.get(0, "b"), Some(&Value::Int(10)));
}

#[test]
fn remove_logs_remove_prop_records_that_replay() {
    let dir = tmp("wal");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        seed(&mut db);
        run(&mut db, "MATCH (n:P) WHERE n.id = 'alice' REMOVE n.a, n.b");
    }
    let wal = std::fs::read(dir.join("wal.bin")).unwrap();
    let (records, _) = decode_all(&wal);
    let removed: Vec<(&str, &str)> = records
        .iter()
        .flat_map(|r| match r {
            WalRecord::Batch(inner) => inner.iter().collect::<Vec<_>>(),
            other => vec![other],
        })
        .filter_map(|r| match r {
            WalRecord::RemoveProp { key, field } => Some((key.as_str(), field.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(removed, vec![("alice", "a"), ("alice", "b")]);
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.get_prop("alice", "a"), None);
    assert_eq!(db.get_prop("alice", "b"), None);
    assert_eq!(db.get_prop("alice", "c"), Some(Value::Int(1)));
}

#[test]
fn remove_retracts_the_edge_a_rule_derived_from_the_property() {
    let mut db = GraphDb::open(&tmp("rule")).unwrap();
    db.create_rule(same_region_rule()).unwrap();
    run(&mut db, "CREATE (n:P {id: 'a', region: 'eu'})");
    run(&mut db, "CREATE (n:P {id: 'b', region: 'eu'})");
    assert!(
        !db.explain("a", "b").unwrap().is_empty(),
        "edge derived: not vacuous"
    );
    run(&mut db, "MATCH (n:P) WHERE n.id = 'b' REMOVE n.region");
    assert!(db.explain("a", "b").unwrap().is_empty(), "edge retracted");
    assert_eq!(db.edge_count(), 0);
    // And it comes back when the property does.
    run(&mut db, "MATCH (n:P) WHERE n.id = 'b' SET n.region = 'eu'");
    assert!(!db.explain("a", "b").unwrap().is_empty());
}

#[test]
fn remove_label_is_refused_by_name() {
    let mut db = GraphDb::open(&tmp("label")).unwrap();
    seed(&mut db);
    let err = db
        .query_write("MATCH (n:P) REMOVE n:Other", &p())
        .unwrap_err();
    match err {
        GraphError::QueryError { detail } => assert!(
            detail.contains("not supported in mushroomdb 0.10: REMOVE n:Label (label removal)"),
            "{detail}"
        ),
        other => panic!("expected QueryError, got {other:?}"),
    }
    assert_eq!(db.get_prop("alice", "a"), Some(Value::Int(1)));
}

#[test]
fn remove_on_an_unbound_variable_is_refused() {
    let mut db = GraphDb::open(&tmp("badvar")).unwrap();
    seed(&mut db);
    let q = "MATCH (x:P) REMOVE z.a";
    assert!(
        matches!(db.query_write(q, &p()), Err(GraphError::QueryError { .. })),
        "{q}"
    );
    // QL-RP: `REMOVE r.w` on a relationship removes its property (a no-op
    // here, where the relationship has none) instead of being refused.
    db.query_write("MATCH (x:P)-[r:KNOWS]->(y:P) REMOVE r.w", &p())
        .unwrap();
}

#[test]
fn remove_namespace_is_refused_like_remove_prop() {
    let mut db = GraphDb::open(&tmp("ns")).unwrap();
    db.insert_node(
        "P",
        "a",
        vec![
            ("ns".into(), Value::Str("x".into())),
            ("a".into(), Value::Int(1)),
        ],
    )
    .unwrap();
    let err = db
        .query_write("MATCH (n:P) WHERE n.id = 'a' REMOVE n.ns", &p())
        .unwrap_err();
    assert!(
        matches!(err, GraphError::NamespaceImmutable { .. }),
        "{err:?}"
    );
    assert_eq!(db.namespace_of("a").as_deref(), Some("x"));
    // The refusal leaves the whole statement unapplied.
    let err = db
        .query_write("MATCH (n:P) WHERE n.id = 'a' REMOVE n.a, n.ns", &p())
        .unwrap_err();
    assert!(
        matches!(err, GraphError::NamespaceImmutable { .. }),
        "{err:?}"
    );
    assert_eq!(db.get_prop("a", "a"), Some(Value::Int(1)));
}

#[test]
fn remove_is_identical_one_statement_or_many_live_replayed_snapshotted_and_at() {
    // One batch.
    let mut one = GraphDb::open(&tmp("diff-one")).unwrap();
    seed(&mut one);
    run(&mut one, "MATCH (n:P) REMOVE n.a, n.b");
    // Separate commits.
    let dir_many = tmp("diff-many");
    let mut many = GraphDb::open(&dir_many).unwrap();
    seed(&mut many);
    run(&mut many, "MATCH (n:P) REMOVE n.a");
    run(&mut many, "MATCH (n:P) REMOVE n.b");
    let live = dump(&many);
    assert_eq!(dump(&one), live, "one batch vs separate commits");
    assert!(
        live.iter().all(|r| r.contains("Int(")),
        "c survives: {live:?}"
    );

    let last = many.commit_seq();
    // Time travel (while the store is still open) to the last commit.
    let at = GraphDb::open_at(&dir_many, last - 1).unwrap();
    assert_eq!(dump(&at), live, "open_at");
    // Never-closed vs reopened (WAL replay).
    drop(many);
    let mut reopened = GraphDb::open(&dir_many).unwrap();
    assert_eq!(dump(&reopened), live, "WAL replay");
    // Snapshotting vs not.
    reopened.snapshot().unwrap();
    drop(reopened);
    let after_snapshot = GraphDb::open(&dir_many).unwrap();
    assert_eq!(dump(&after_snapshot), live, "after a snapshot");
    drop(after_snapshot);
    // And a removal after a snapshot is still replayed.
    let mut db = GraphDb::open(&dir_many).unwrap();
    run(&mut db, "MATCH (n:P) WHERE n.id = 'alice' REMOVE n.c");
    drop(db);
    let db = GraphDb::open(&dir_many).unwrap();
    assert_eq!(db.get_prop("alice", "c"), None);
    assert_eq!(db.get_prop("bob", "c"), Some(Value::Int(2)));
}

// ── Inside an interactive transaction ───────────────────────────────────────

fn node(key: &str) -> BatchOp {
    BatchOp::InsertNode {
        label: "P".into(),
        key: key.into(),
        props: vec![("a".into(), Value::Int(1)), ("b".into(), Value::Int(2))],
    }
}

#[test]
fn remove_inside_a_transaction_buffers_and_commits_atomically() {
    let dir = tmp("tx");
    let db: SharedDb = SharedDb::open(&dir).unwrap();
    db.submit_batch(vec![node("x"), node("y")]).unwrap();
    let mut tx = db.begin_transaction();
    let rs = tx
        .execute("MATCH (n:P) WHERE n.id = 'x' REMOVE n.a, n.zzz", &p())
        .unwrap();
    assert_eq!(rs.get(0, "properties_set"), Some(&Value::Int(1)));
    // Visible inside, not outside, until commit.
    let inside = tx
        .execute(
            "MATCH (n:P) WHERE n.id = 'x' RETURN n.a AS a, n.b AS b",
            &p(),
        )
        .unwrap();
    assert_eq!(inside.get(0, "a"), None);
    assert_eq!(inside.get(0, "b"), Some(&Value::Int(2)));
    assert_eq!(
        db.reader().node_info("x").unwrap().props.get("a"),
        Some(&Value::Int(1))
    );
    tx.commit().unwrap();
    let info = db.reader().node_info("x").unwrap();
    assert_eq!(info.props.get("a"), None);
    assert_eq!(info.props.get("b"), Some(&Value::Int(2)));
    assert_eq!(
        db.reader().node_info("y").unwrap().props.get("a"),
        Some(&Value::Int(1))
    );
    drop(db);
    let reopened = GraphDb::open(&dir).unwrap();
    assert_eq!(reopened.get_prop("x", "a"), None);
}

#[test]
fn remove_with_return_runs_inside_a_transaction() {
    let dir = tmp("tx-return");
    let db: SharedDb = SharedDb::open(&dir).unwrap();
    db.submit_batch(vec![node("x")]).unwrap();
    let mut tx = db.begin_transaction();
    let rs = tx
        .execute(
            "MATCH (n:P) WHERE n.id = 'x' REMOVE n.a RETURN n.a AS a, n.b AS b",
            &p(),
        )
        .unwrap();
    assert_eq!(rs.get(0, "a"), None, "the RETURN reads the removal");
    assert_eq!(rs.get(0, "b"), Some(&Value::Int(2)));
    assert_eq!(
        db.reader().node_info("x").unwrap().props.get("a"),
        Some(&Value::Int(1)),
        "not committed yet"
    );
    tx.commit().unwrap();
    assert_eq!(db.reader().node_info("x").unwrap().props.get("a"), None);
}

// ── Keyword stays a name elsewhere ──────────────────────────────────────────

#[test]
fn remove_is_still_an_ordinary_name_in_reads() {
    let mut db = GraphDb::open(&tmp("name")).unwrap();
    run(&mut db, "CREATE (n:Remove {id: 'r', remove: 1})");
    let rs = db
        .query("MATCH (x:Remove) RETURN x.remove AS remove", &p())
        .unwrap();
    assert_eq!(rs.get(0, "remove"), Some(&Value::Int(1)));
}

// ── Role-scoped writes ──────────────────────────────────────────────────────

fn writer_store(name: &str) -> GraphDb {
    let mut db = GraphDb::open(&tmp(name)).unwrap();
    db.apply_schema(&Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![],
        roles: vec![RoleDef {
            name: "writer".into(),
            keys: vec![],
            labels: vec!["Mine".into()],
            visible_where: None,
            namespaces: None,
            write: Some(WriteScope {
                create_labels: vec![],
                update_labels: vec!["Mine".into()],
                delete_labels: vec![],
                create_edge_types: vec![],
                delete_edge_types: vec![],
                update_edge_types: vec![],
            }),
        }],
    })
    .unwrap();
    db.insert_node("Mine", "m", vec![("a".into(), Value::Int(1))])
        .unwrap();
    db.insert_node("Secret", "s", vec![("a".into(), Value::Int(1))])
        .unwrap();
    db
}

#[test]
fn scoped_remove_reaches_visible_nodes_and_never_hidden_ones() {
    let mut db = writer_store("scoped");
    db.query_write_authz("writer", "MATCH (n) REMOVE n.a", &p())
        .unwrap();
    assert_eq!(db.get_prop("m", "a"), None, "visible node: removed");
    assert_eq!(
        db.get_prop("s", "a"),
        Some(Value::Int(1)),
        "hidden node is absent to the MATCH, so untouched"
    );
}

#[test]
fn scoped_remove_on_a_label_outside_the_role_matches_nothing() {
    let mut db = writer_store("scoped-denied");
    db.insert_node("Visible", "v", vec![("a".into(), Value::Int(1))])
        .unwrap();
    // `Visible` is not in this role's labels, so it is hidden: zero rows, no error.
    db.query_write_authz("writer", "MATCH (n:Visible) REMOVE n.a", &p())
        .unwrap();
    assert_eq!(db.get_prop("v", "a"), Some(Value::Int(1)));
}
