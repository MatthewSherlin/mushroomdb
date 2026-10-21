//! `MATCH … SET n = {…}` and `SET n += {…}` (QL-06): the map forms go through
//! the same write path as `set_prop` / `remove_prop`, so rules, the WAL, the
//! namespace guard and the view-owned refusal all see them.

use core_storage::wal::{decode_all, WalRecord};
use mushroomdb::{
    schema::Schema, BatchOp, Direction, GraphError, Predicate, RealFs, RoleDef, RuleDef, SharedDb,
    Value, ViewDef, ViewSource, WriteScope,
};
use std::collections::BTreeMap;
use std::path::PathBuf;

type GraphDb = mushroomdb::GraphDb<RealFs>;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "graphdb-cypher-set-map-{name}-{}",
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

fn query_err(db: &mut GraphDb, q: &str) -> GraphError {
    db.query_write(q, &p()).unwrap_err()
}

/// Every property of a node, as the engine stores it.
fn props(db: &GraphDb, key: &str) -> BTreeMap<String, Value> {
    db.node_info(key).expect("node exists").props
}

/// Every node's props, sorted, as text: the state the probes compare.
fn dump(db: &GraphDb) -> Vec<String> {
    ["alice", "bob", "carol"]
        .iter()
        .map(|k| format!("{k} {:?}", props(db, k)))
        .collect()
}

fn wal_ops(dir: &std::path::Path) -> Vec<String> {
    let wal = std::fs::read(dir.join("wal.bin")).unwrap();
    let (records, _) = decode_all(&wal);
    records
        .iter()
        .flat_map(|r| match r {
            WalRecord::Batch(inner) => inner.iter().collect::<Vec<_>>(),
            other => vec![other],
        })
        .filter_map(|r| match r {
            WalRecord::RemoveProp { key, field } => Some(format!("remove {key}.{field}")),
            WalRecord::SetProp { key, field, .. } => Some(format!("set {key}.{field}")),
            _ => None,
        })
        .collect()
}

// ── += (merge) ──────────────────────────────────────────────────────────────

#[test]
fn merge_sets_the_listed_keys_and_leaves_the_rest() {
    let mut db = GraphDb::open(&tmp("merge")).unwrap();
    seed(&mut db);
    let rs = run(
        &mut db,
        "MATCH (n:P) WHERE n.id = 'alice' SET n += {a: 2.5, d: 'new'}",
    );
    assert_eq!(rs.get(0, "properties_set"), Some(&Value::Int(2)));
    assert_eq!(db.get_prop("alice", "a"), Some(Value::Float(2.5)));
    assert_eq!(db.get_prop("alice", "d"), Some(Value::Str("new".into())));
    assert_eq!(db.get_prop("alice", "b"), Some(Value::Int(10)));
    assert_eq!(db.get_prop("alice", "c"), Some(Value::Int(1)));
    assert_eq!(db.get_prop("bob", "a"), Some(Value::Int(2)), "other node");
}

#[test]
fn merge_with_an_empty_map_is_a_no_op_and_logs_nothing() {
    let mut db = GraphDb::open(&tmp("merge-empty")).unwrap();
    seed(&mut db);
    let before = db.commit_seq();
    let rs = run(&mut db, "MATCH (n:P) SET n += {}");
    assert_eq!(rs.get(0, "properties_set"), Some(&Value::Int(0)));
    assert_eq!(db.commit_seq(), before);
}

#[test]
fn merge_with_a_null_value_removes_that_property() {
    let mut db = GraphDb::open(&tmp("merge-null")).unwrap();
    seed(&mut db);
    let rs = run(
        &mut db,
        "MATCH (n:P) WHERE n.id = 'alice' SET n += {a: null, zzz: null, b: 11}",
    );
    // a removed, zzz absent (no-op), b set.
    assert_eq!(rs.get(0, "properties_set"), Some(&Value::Int(2)));
    assert_eq!(db.get_prop("alice", "a"), None);
    assert_eq!(db.get_prop("alice", "zzz"), None);
    assert_eq!(db.get_prop("alice", "b"), Some(Value::Int(11)));
    assert_eq!(db.get_prop("alice", "c"), Some(Value::Int(1)));
}

#[test]
fn merge_values_are_expressions_evaluated_per_matched_row() {
    let mut db = GraphDb::open(&tmp("merge-expr")).unwrap();
    seed(&mut db);
    let mut params = p();
    params.insert("bump".into(), Value::Int(100));
    db.query_write("MATCH (n:P) SET n += {a: n.a * 2, b: n.b + $bump}", &params)
        .unwrap();
    assert_eq!(db.get_prop("alice", "a"), Some(Value::Int(2)));
    assert_eq!(db.get_prop("carol", "a"), Some(Value::Int(6)));
    assert_eq!(db.get_prop("bob", "b"), Some(Value::Int(120)));
}

// ── = (replace) ─────────────────────────────────────────────────────────────

#[test]
fn replace_keeps_exactly_the_map_and_removes_every_other_user_property() {
    let dir = tmp("replace");
    let mut db = GraphDb::open(&dir).unwrap();
    seed(&mut db);
    let rs = run(&mut db, "MATCH (n:P) WHERE n.id = 'alice' SET n = {a: 2.5}");
    // a set; b and c removed.
    assert_eq!(rs.get(0, "properties_set"), Some(&Value::Int(3)));
    let alice = props(&db, "alice");
    assert_eq!(alice.get("a"), Some(&Value::Float(2.5)));
    assert_eq!(alice.get("b"), None);
    assert_eq!(alice.get("c"), None);
    assert_eq!(
        alice.get("id"),
        Some(&Value::Str("alice".into())),
        "the key property survives a replace"
    );
    assert_eq!(db.get_prop("bob", "b"), Some(Value::Int(20)), "other node");
    let ops = wal_ops(&dir);
    let removed: Vec<&String> = ops.iter().filter(|o| o.starts_with("remove ")).collect();
    assert_eq!(
        removed,
        vec!["remove alice.b", "remove alice.c"],
        "RemoveProp is emitted for every unlisted property, and only those: {ops:?}"
    );
}

#[test]
fn replace_with_an_empty_map_removes_every_user_property_but_not_the_node() {
    let mut db = GraphDb::open(&tmp("replace-empty")).unwrap();
    seed(&mut db);
    run(&mut db, "MATCH (n:P) WHERE n.id = 'alice' SET n = {}");
    let alice = props(&db, "alice");
    assert_eq!(alice.get("a"), None);
    assert_eq!(alice.get("b"), None);
    assert_eq!(alice.get("c"), None);
    assert_eq!(alice.get("id"), Some(&Value::Str("alice".into())));
    assert_eq!(db.node_info("alice").unwrap().label, "P");
}

#[test]
fn replace_treats_a_null_entry_as_an_absent_key() {
    let mut db = GraphDb::open(&tmp("replace-null")).unwrap();
    seed(&mut db);
    run(
        &mut db,
        "MATCH (n:P) WHERE n.id = 'alice' SET n = {a: null, b: 5}",
    );
    let alice = props(&db, "alice");
    assert_eq!(
        alice.get("a"),
        None,
        "a null entry sets nothing and removes"
    );
    assert_eq!(alice.get("b"), Some(&Value::Int(5)));
    assert_eq!(alice.get("c"), None);
}

#[test]
fn replace_over_many_nodes_with_a_row_dependent_map() {
    let mut db = GraphDb::open(&tmp("replace-many")).unwrap();
    seed(&mut db);
    run(&mut db, "MATCH (n:P) WHERE n.a >= 2 SET n = {a: n.a + 100}");
    assert_eq!(db.get_prop("alice", "b"), Some(Value::Int(10)), "unmatched");
    assert_eq!(db.get_prop("bob", "a"), Some(Value::Int(102)));
    assert_eq!(db.get_prop("bob", "b"), None);
    assert_eq!(db.get_prop("carol", "a"), Some(Value::Int(103)));
}

#[test]
fn replace_with_a_node_bound_by_several_rows_ends_in_the_last_rows_map() {
    let mut db = GraphDb::open(&tmp("replace-rows")).unwrap();
    seed(&mut db);
    db.insert_edge("KNOWS", "alice", "carol").unwrap();
    db.insert_edge("KNOWS", "bob", "carol").unwrap();
    // carol is bound by two rows; the statement must not fail or double-write.
    run(&mut db, "MATCH (x:P)-[:KNOWS]->(y:P) SET y = {a: x.a}");
    let carol = props(&db, "carol");
    assert_eq!(carol.len(), 2, "id and exactly one map key: {carol:?}");
    assert!(matches!(carol.get("a"), Some(Value::Int(1 | 2))));
}

// ── RETURN ──────────────────────────────────────────────────────────────────

#[test]
fn return_projects_the_post_write_state() {
    let mut db = GraphDb::open(&tmp("return")).unwrap();
    seed(&mut db);
    let rs = run(
        &mut db,
        "MATCH (n:P) WHERE n.id = 'alice' SET n = {score: 1} RETURN n.score AS score, n.a AS a",
    );
    assert_eq!(rs.len(), 1);
    assert_eq!(rs.get(0, "score"), Some(&Value::Int(1)));
    assert_eq!(rs.get(0, "a"), None);
    let rs = run(
        &mut db,
        "MATCH (n:P) WHERE n.id = 'bob' SET n += {score: 7} RETURN n.score AS score, n.b AS b",
    );
    assert_eq!(rs.get(0, "score"), Some(&Value::Int(7)));
    assert_eq!(rs.get(0, "b"), Some(&Value::Int(20)));
}

// ── Rules ───────────────────────────────────────────────────────────────────

fn two_in_one_region(db: &mut GraphDb) {
    db.create_rule(same_region_rule()).unwrap();
    run(db, "CREATE (n:P {id: 'a', region: 'eu', other: 1})");
    run(db, "CREATE (n:P {id: 'b', region: 'eu', other: 2})");
    assert!(
        !db.explain("a", "b").unwrap().is_empty(),
        "edge derived: not vacuous"
    );
}

#[test]
fn replace_retracts_the_edge_a_rule_derived_from_a_removed_property() {
    let mut db = GraphDb::open(&tmp("rule-replace")).unwrap();
    two_in_one_region(&mut db);
    run(&mut db, "MATCH (n:P) WHERE n.id = 'b' SET n = {other: 2}");
    assert!(db.explain("a", "b").unwrap().is_empty(), "edge retracted");
    assert_eq!(db.edge_count(), 0);
    run(
        &mut db,
        "MATCH (n:P) WHERE n.id = 'b' SET n += {region: 'eu'}",
    );
    assert!(!db.explain("a", "b").unwrap().is_empty(), "and it returns");
}

#[test]
fn merge_null_retracts_the_edge_a_rule_derived_from_the_property() {
    let mut db = GraphDb::open(&tmp("rule-merge")).unwrap();
    two_in_one_region(&mut db);
    run(
        &mut db,
        "MATCH (n:P) WHERE n.id = 'b' SET n += {region: null}",
    );
    assert!(db.explain("a", "b").unwrap().is_empty());
    assert_eq!(db.edge_count(), 0);
}

#[test]
fn replace_that_changes_the_ruled_property_rederives_for_the_new_value() {
    let mut db = GraphDb::open(&tmp("rule-change")).unwrap();
    two_in_one_region(&mut db);
    run(&mut db, "CREATE (n:P {id: 'c', region: 'us'})");
    run(
        &mut db,
        "MATCH (n:P) WHERE n.id = 'b' SET n = {region: 'us'}",
    );
    assert!(db.explain("a", "b").unwrap().is_empty());
    assert!(!db.explain("b", "c").unwrap().is_empty());
}

// ── The key, the namespace, views ───────────────────────────────────────────

#[test]
fn the_key_property_is_never_removed_and_never_changed_by_a_map() {
    let mut db = GraphDb::open(&tmp("id")).unwrap();
    seed(&mut db);
    // Naming the node's own key is a no-op.
    run(
        &mut db,
        "MATCH (n:P) WHERE n.id = 'alice' SET n = {id: 'alice', a: 9}",
    );
    assert_eq!(
        props(&db, "alice").get("id"),
        Some(&Value::Str("alice".into()))
    );
    assert_eq!(db.get_prop("alice", "a"), Some(Value::Int(9)));
    for q in [
        "MATCH (n:P) WHERE n.id = 'alice' SET n = {id: 'mallory', a: 1}",
        "MATCH (n:P) WHERE n.id = 'alice' SET n += {id: 'mallory'}",
        "MATCH (n:P) WHERE n.id = 'alice' SET n += {id: null}",
        "MATCH (n:P) WHERE n.id = 'alice' SET n = {id: 7}",
    ] {
        let before = db.commit_seq();
        match query_err(&mut db, q) {
            GraphError::QueryError { detail } => {
                assert!(
                    detail.contains("`id`") && detail.contains("rename_node"),
                    "{detail}"
                )
            }
            other => panic!("{q}: expected QueryError, got {other:?}"),
        }
        assert_eq!(db.commit_seq(), before, "{q}: nothing written");
    }
    assert_eq!(db.get_prop("alice", "a"), Some(Value::Int(9)));
    assert!(db.node_info("alice").is_some());
}

fn namespaced(name: &str) -> GraphDb {
    let mut db = GraphDb::open(&tmp(name)).unwrap();
    db.insert_node(
        "P",
        "n1",
        vec![
            ("ns".into(), Value::Str("tenant".into())),
            ("a".into(), Value::Int(1)),
            ("b".into(), Value::Int(2)),
        ],
    )
    .unwrap();
    db
}

#[test]
fn replace_keeps_the_namespace_when_the_map_omits_it() {
    let mut db = namespaced("ns-keep");
    run(&mut db, "MATCH (n:P) WHERE n.id = 'n1' SET n = {a: 5}");
    assert_eq!(db.namespace_of("n1").as_deref(), Some("tenant"));
    assert_eq!(db.get_prop("n1", "b"), None);
    assert_eq!(db.get_prop("n1", "a"), Some(Value::Int(5)));
}

#[test]
fn a_map_naming_the_same_namespace_is_accepted() {
    let mut db = namespaced("ns-same");
    run(
        &mut db,
        "MATCH (n:P) WHERE n.id = 'n1' SET n = {ns: 'tenant', a: 5}",
    );
    assert_eq!(db.namespace_of("n1").as_deref(), Some("tenant"));
    assert_eq!(db.get_prop("n1", "b"), None);
}

#[test]
fn a_map_that_moves_the_namespace_is_refused_and_applies_nothing() {
    let mut db = namespaced("ns-move");
    for q in [
        "MATCH (n:P) WHERE n.id = 'n1' SET n = {ns: 'other', a: 5}",
        "MATCH (n:P) WHERE n.id = 'n1' SET n += {ns: 'other', a: 5}",
        "MATCH (n:P) WHERE n.id = 'n1' SET n += {ns: null, a: 5}",
        "MATCH (n:P) WHERE n.id = 'n1' SET n = {ns: null, a: 5}",
    ] {
        let err = query_err(&mut db, q);
        assert!(
            matches!(err, GraphError::NamespaceImmutable { .. }),
            "{q}: {err:?}"
        );
        assert_eq!(db.namespace_of("n1").as_deref(), Some("tenant"), "{q}");
        assert_eq!(
            db.get_prop("n1", "a"),
            Some(Value::Int(1)),
            "{q}: unapplied"
        );
        assert_eq!(
            db.get_prop("n1", "b"),
            Some(Value::Int(2)),
            "{q}: unapplied"
        );
    }
}

fn degree_view_store(name: &str) -> GraphDb {
    let mut db = GraphDb::open(&tmp(name)).unwrap();
    db.insert_node("City", "c1", vec![("name".into(), Value::Str("x".into()))])
        .unwrap();
    db.insert_node("Person", "p1", vec![]).unwrap();
    db.create_view(ViewDef {
        name: "city_pop".into(),
        label: "City".into(),
        view_prop: "pop".into(),
        source: ViewSource::Degree {
            edge_type: "LIVES_IN".into(),
            direction: Direction::In,
        },
    })
    .unwrap();
    db.insert_edge("LIVES_IN", "p1", "c1").unwrap();
    assert_eq!(db.get_prop("c1", "pop"), Some(Value::Int(1)));
    db
}

#[test]
fn replace_leaves_a_view_owned_property_in_place() {
    let mut db = degree_view_store("view-keep");
    run(&mut db, "MATCH (n:City) SET n = {name: 'y'}");
    assert_eq!(db.get_prop("c1", "pop"), Some(Value::Int(1)));
    assert_eq!(db.get_prop("c1", "name"), Some(Value::Str("y".into())));
    run(&mut db, "MATCH (n:City) SET n = {}");
    assert_eq!(db.get_prop("c1", "pop"), Some(Value::Int(1)));
    assert_eq!(db.get_prop("c1", "name"), None);
}

#[test]
fn a_map_that_names_a_view_owned_property_is_refused() {
    let mut db = degree_view_store("view-refuse");
    for q in [
        "MATCH (n:City) SET n = {pop: 99}",
        "MATCH (n:City) SET n += {pop: 99, name: 'z'}",
        "MATCH (n:City) SET n += {pop: null}",
        "MATCH (n:City) SET n = {pop: null}",
    ] {
        let err = query_err(&mut db, q);
        assert!(
            matches!(err, GraphError::ViewPropReadOnly { .. }),
            "{q}: {err:?}"
        );
        assert_eq!(db.get_prop("c1", "pop"), Some(Value::Int(1)), "{q}");
        assert_eq!(
            db.get_prop("c1", "name"),
            Some(Value::Str("x".into())),
            "{q}"
        );
    }
}

#[test]
fn remove_of_a_view_owned_property_is_view_prop_read_only() {
    let mut db = degree_view_store("view-remove");
    let err = query_err(&mut db, "MATCH (n:City) REMOVE n.pop");
    assert!(
        matches!(err, GraphError::ViewPropReadOnly { .. }),
        "{err:?}"
    );
    assert_eq!(db.get_prop("c1", "pop"), Some(Value::Int(1)));
}

// ── Refusals and atomicity ──────────────────────────────────────────────────

#[test]
fn a_value_the_store_cannot_hold_fails_the_whole_statement() {
    let mut db = GraphDb::open(&tmp("atomic")).unwrap();
    seed(&mut db);
    let before = db.commit_seq();
    let err = query_err(
        &mut db,
        "MATCH (n:P) WHERE n.id = 'alice' SET n = {a: 1, b: [1, null]}",
    );
    assert!(matches!(err, GraphError::QueryError { .. }), "{err:?}");
    assert_eq!(db.commit_seq(), before);
    assert_eq!(
        db.get_prop("alice", "c"),
        Some(Value::Int(1)),
        "no removals either"
    );
    assert_eq!(db.get_prop("alice", "b"), Some(Value::Int(10)));
}

#[test]
fn malformed_and_unsupported_forms_are_refused_by_name() {
    let mut db = GraphDb::open(&tmp("refuse")).unwrap();
    seed(&mut db);
    db.insert_edge("KNOWS", "alice", "bob").unwrap();
    for (q, needle) in [
        ("MATCH (n:P) SET n = m", "neither a map literal"),
        ("MATCH (n:P) SET n += 5", "neither a map literal"),
        ("MATCH (n:P) SET n = 5", "neither a map literal"),
        ("MATCH (n:P) SET n = {a: 1}, n.b = 2", "another SET item"),
        ("MATCH (n:P) SET m = {a: 1}", "not a node variable"),
        (
            "MATCH (x:P)-[r:KNOWS]->(y:P) SET r = {w: 1}",
            "relationship",
        ),
        ("MATCH (n:P) SET n = {a: 1, a: 2}", "duplicate map key"),
        ("MATCH (n:P) SET n = {a: 1", "map literal"),
    ] {
        match db.query_write(q, &p()) {
            Err(GraphError::QueryError { detail }) => {
                assert!(detail.contains(needle), "{q}: {detail}")
            }
            other => panic!("{q}: expected a refusal, got {other:?}"),
        }
    }
    assert_eq!(db.get_prop("alice", "a"), Some(Value::Int(1)));
}

#[test]
fn single_property_set_is_unchanged() {
    let mut db = GraphDb::open(&tmp("plain-set")).unwrap();
    seed(&mut db);
    run(
        &mut db,
        "MATCH (n:P) WHERE n.id = 'alice' SET n.a = 5, n.b = 6",
    );
    assert_eq!(db.get_prop("alice", "a"), Some(Value::Int(5)));
    assert_eq!(db.get_prop("alice", "b"), Some(Value::Int(6)));
}

// ── Differential probes ─────────────────────────────────────────────────────

#[test]
fn map_forms_are_identical_one_statement_or_many_live_replayed_snapshotted_and_at() {
    // One statement per form.
    let mut one = GraphDb::open(&tmp("diff-one")).unwrap();
    seed(&mut one);
    run(&mut one, "MATCH (n:P) SET n = {a: n.a + 1, d: 'x'}");
    run(&mut one, "MATCH (n:P) SET n += {a: null, e: 1}");
    // Separate commits per node.
    let dir_many = tmp("diff-many");
    let mut many = GraphDb::open(&dir_many).unwrap();
    seed(&mut many);
    for id in ["alice", "bob", "carol"] {
        run(
            &mut many,
            &format!("MATCH (n:P) WHERE n.id = '{id}' SET n = {{a: n.a + 1, d: 'x'}}"),
        );
    }
    for id in ["alice", "bob", "carol"] {
        run(
            &mut many,
            &format!("MATCH (n:P) WHERE n.id = '{id}' SET n += {{a: null, e: 1}}"),
        );
    }
    let live = dump(&many);
    assert_eq!(dump(&one), live, "one batch vs separate commits");
    assert!(
        live.iter()
            .all(|r| r.contains("\"e\": Int(1)") && !r.contains("\"a\"")),
        "{live:?}"
    );
    assert!(live.iter().all(|r| !r.contains("\"b\"")), "{live:?}");

    let last = many.commit_seq();
    let at = GraphDb::open_at(&dir_many, last - 1).unwrap();
    assert_eq!(dump(&at), live, "open_at");
    drop(at);
    drop(many);
    let mut reopened = GraphDb::open(&dir_many).unwrap();
    assert_eq!(dump(&reopened), live, "WAL replay");
    reopened.snapshot().unwrap();
    drop(reopened);
    let mut after = GraphDb::open(&dir_many).unwrap();
    assert_eq!(dump(&after), live, "after a snapshot");
    // A replace after a snapshot sees the snapshot's properties.
    run(&mut after, "MATCH (n:P) WHERE n.id = 'bob' SET n = {z: 1}");
    drop(after);
    let db = GraphDb::open(&dir_many).unwrap();
    let bob = props(&db, "bob");
    assert_eq!(bob.len(), 2, "id and z only: {bob:?}");
    assert_eq!(bob.get("z"), Some(&Value::Int(1)));
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
fn map_forms_inside_a_transaction_are_refused_by_name_and_buffer_nothing() {
    let dir = tmp("tx");
    let db: SharedDb = SharedDb::open(&dir).unwrap();
    db.submit_batch(vec![node("x")]).unwrap();
    let mut tx = db.begin_transaction();
    for q in [
        "MATCH (n:P) WHERE n.id = 'x' SET n = {a: 9}",
        "MATCH (n:P) WHERE n.id = 'x' SET n += {a: 9}",
        "MATCH (n:P) WHERE n.id = 'x' SET n += {a: 9} RETURN n.a AS a",
    ] {
        match tx.execute(q, &p()).unwrap_err() {
            GraphError::QueryError { detail } => assert_eq!(
                detail,
                "not supported in mushroomdb 0.10 inside a transaction: \
                 MATCH … SET n = {…} / SET n += {…}",
                "{q}"
            ),
            other => panic!("{q}: expected QueryError, got {other:?}"),
        }
    }
    // The transaction is still usable and nothing was buffered.
    let inside = tx
        .execute("MATCH (n:P) WHERE n.id = 'x' RETURN n.a AS a", &p())
        .unwrap();
    assert_eq!(inside.get(0, "a"), Some(&Value::Int(1)));
    tx.commit().unwrap();
    let info = db.reader().node_info("x").unwrap();
    assert_eq!(info.props.get("a"), Some(&Value::Int(1)));
    assert_eq!(info.props.get("b"), Some(&Value::Int(2)));
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
            }),
        }],
    })
    .unwrap();
    db.insert_node(
        "Mine",
        "m",
        vec![("a".into(), Value::Int(1)), ("b".into(), Value::Int(1))],
    )
    .unwrap();
    db.insert_node(
        "Secret",
        "s",
        vec![("a".into(), Value::Int(1)), ("b".into(), Value::Int(1))],
    )
    .unwrap();
    db
}

#[test]
fn scoped_map_forms_reach_visible_nodes_and_never_hidden_ones() {
    let mut db = writer_store("scoped");
    db.query_write_authz("writer", "MATCH (n) SET n = {a: 5}", &p())
        .unwrap();
    assert_eq!(db.get_prop("m", "a"), Some(Value::Int(5)));
    assert_eq!(db.get_prop("m", "b"), None, "visible node: replaced");
    assert_eq!(
        db.get_prop("s", "a"),
        Some(Value::Int(1)),
        "hidden: untouched"
    );
    assert_eq!(
        db.get_prop("s", "b"),
        Some(Value::Int(1)),
        "hidden: untouched"
    );
    db.query_write_authz("writer", "MATCH (n) SET n += {b: null, c: 2}", &p())
        .unwrap();
    assert_eq!(db.get_prop("m", "c"), Some(Value::Int(2)));
    assert_eq!(db.get_prop("s", "c"), None);
    assert_eq!(db.get_prop("s", "b"), Some(Value::Int(1)));
}

// ── Parameter maps ──────────────────────────────────────────────────────────

fn map_param(entries: Vec<(&str, Value)>) -> BTreeMap<String, Value> {
    let map: BTreeMap<String, Value> = entries
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    let mut params = p();
    params.insert("props".into(), Value::Map(map));
    params
}

#[test]
fn a_parameter_map_merges_like_the_literal_including_null_removal() {
    let mut db = GraphDb::open(&tmp("param-merge")).unwrap();
    seed(&mut db);
    let params = map_param(vec![
        ("a", Value::Float(2.5)),
        ("b", Value::Null),
        ("d", Value::Str("new".into())),
    ]);
    let rs = db
        .query_write(
            "MATCH (n:P) WHERE n.id = 'alice' SET n += $props RETURN n.a AS a, n.b AS b",
            &params,
        )
        .unwrap();
    assert_eq!(rs.get(0, "a"), Some(&Value::Float(2.5)));
    assert_eq!(rs.get(0, "b"), None);
    assert_eq!(db.get_prop("alice", "b"), None, "null removed it");
    assert_eq!(db.get_prop("alice", "c"), Some(Value::Int(1)), "rest kept");
    assert_eq!(db.get_prop("alice", "d"), Some(Value::Str("new".into())));
    assert_eq!(db.get_prop("bob", "b"), Some(Value::Int(20)), "other node");
}

#[test]
fn a_parameter_map_replaces_like_the_literal() {
    let mut db = GraphDb::open(&tmp("param-replace")).unwrap();
    seed(&mut db);
    let params = map_param(vec![("a", Value::Int(9)), ("b", Value::Null)]);
    db.query_write("MATCH (n:P) WHERE n.id = 'alice' SET n = $props", &params)
        .unwrap();
    let alice = props(&db, "alice");
    assert_eq!(alice.get("a"), Some(&Value::Int(9)));
    assert_eq!(alice.get("b"), None);
    assert_eq!(alice.get("c"), None);
    assert_eq!(alice.get("id"), Some(&Value::Str("alice".into())));
    // Empty parameter map: replace clears, merge is a no-op.
    let empty = map_param(vec![]);
    let before = db.commit_seq();
    db.query_write("MATCH (n:P) WHERE n.id = 'bob' SET n += $props", &empty)
        .unwrap();
    assert_eq!(db.commit_seq(), before);
    db.query_write("MATCH (n:P) WHERE n.id = 'bob' SET n = $props", &empty)
        .unwrap();
    assert_eq!(props(&db, "bob").len(), 1, "only id remains");
}

#[test]
fn a_parameter_map_meets_the_same_protections_as_a_literal() {
    let mut db = degree_view_store("param-protect");
    for (name, entries) in [
        ("view", vec![("pop", Value::Int(5))]),
        ("view-null", vec![("pop", Value::Null)]),
    ] {
        let params = map_param(entries);
        let before = db.commit_seq();
        let err = db
            .query_write("MATCH (n:City) SET n += $props", &params)
            .unwrap_err();
        assert!(
            matches!(
                err,
                GraphError::ViewPropReadOnly { .. } | GraphError::QueryError { .. }
            ),
            "{name}: {err:?}"
        );
        assert_eq!(db.commit_seq(), before, "{name}");
    }
    let mut id_db = GraphDb::open(&tmp("param-id")).unwrap();
    seed(&mut id_db);
    let err = id_db
        .query_write(
            "MATCH (n:P) WHERE n.id = 'alice' SET n += $props",
            &map_param(vec![("id", Value::Str("other".into()))]),
        )
        .unwrap_err();
    assert!(matches!(err, GraphError::QueryError { .. }), "{err:?}");
    assert_eq!(
        id_db.get_prop("alice", "id"),
        Some(Value::Str("alice".into()))
    );
    let mut ns_db = namespaced("param-ns");
    let err = ns_db
        .query_write(
            "MATCH (n:P) WHERE n.id = 'n1' SET n = $props",
            &map_param(vec![("ns", Value::Str("other".into()))]),
        )
        .unwrap_err();
    assert!(
        matches!(err, GraphError::NamespaceImmutable { .. }),
        "{err:?}"
    );
    assert_eq!(ns_db.get_prop("n1", "a"), Some(Value::Int(1)));
}

#[test]
fn a_parameter_that_is_not_a_map_is_a_named_error_and_writes_nothing() {
    let mut db = GraphDb::open(&tmp("param-bad")).unwrap();
    seed(&mut db);
    let before = db.commit_seq();
    for (what, value) in [
        ("string", Some(Value::Str("x".into()))),
        ("list", Some(Value::List(vec![Value::Int(1)]))),
        ("null", Some(Value::Null)),
        ("missing", None),
    ] {
        let mut params = p();
        if let Some(v) = value {
            params.insert("props".into(), v);
        }
        for q in ["MATCH (n:P) SET n = $props", "MATCH (n:P) SET n += $props"] {
            let err = db.query_write(q, &params).unwrap_err();
            let GraphError::QueryError { detail } = err else {
                panic!("{what} {q}: expected QueryError");
            };
            if what != "missing" {
                assert!(detail.contains("must be a map"), "{what} {q}: {detail}");
            }
        }
    }
    assert_eq!(db.commit_seq(), before);
    assert_eq!(db.get_prop("alice", "a"), Some(Value::Int(1)));
}

// ── The key property in the single-property forms ───────────────────────────

#[test]
fn set_and_remove_of_the_key_property_are_refused_like_the_map_form() {
    let mut db = GraphDb::open(&tmp("id-single")).unwrap();
    seed(&mut db);
    let before = db.commit_seq();
    for q in [
        "MATCH (n:P) WHERE n.id = 'alice' SET n.id = 'mallory'",
        "MATCH (n:P) WHERE n.id = 'alice' SET n.a = 5, n.id = 7",
        "MATCH (n:P) WHERE n.id = 'alice' REMOVE n.id",
        "MATCH (n:P) WHERE n.id = 'alice' REMOVE n.a, n.id",
        "MERGE (n:P {id: 'alice'}) ON MATCH SET n.id = 'mallory'",
        "MERGE (n:P {id: 'zed'}) ON CREATE SET n.id = 'mallory'",
    ] {
        match query_err(&mut db, q) {
            GraphError::QueryError { detail } => assert!(
                detail.contains("`id`") && detail.contains("rename_node"),
                "{q}: {detail}"
            ),
            other => panic!("{q}: expected QueryError, got {other:?}"),
        }
        assert_eq!(db.commit_seq(), before, "{q}: nothing written");
    }
    assert_eq!(db.get_prop("alice", "a"), Some(Value::Int(1)));
    assert!(db.node_info("zed").is_none());
    // Restating the key is a no-op, not an error.
    run(
        &mut db,
        "MATCH (n:P) WHERE n.id = 'alice' SET n.id = 'alice', n.a = 6",
    );
    assert_eq!(db.get_prop("alice", "a"), Some(Value::Int(6)));
    // Reading `n.id` is unchanged.
    let rs = db
        .query("MATCH (n:P) WHERE n.id = 'alice' RETURN n.id AS k", &p())
        .unwrap();
    assert_eq!(rs.get(0, "k"), Some(&Value::Str("alice".into())));
}

#[test]
fn key_property_refusals_hold_inside_a_transaction_too() {
    let dir = tmp("id-tx");
    let db: SharedDb = SharedDb::open(&dir).unwrap();
    db.submit_batch(vec![BatchOp::InsertNode {
        label: "P".into(),
        key: "x".into(),
        props: vec![("id".into(), Value::Str("x".into()))],
    }])
    .unwrap();
    let mut tx = db.begin_transaction();
    for q in [
        "MATCH (n:P) WHERE n.id = 'x' SET n.id = 'y'",
        "MATCH (n:P) WHERE n.id = 'x' REMOVE n.id",
        "MERGE (n:P {id: 'x'}) ON MATCH SET n.id = 'y'",
    ] {
        assert!(
            matches!(tx.execute(q, &p()), Err(GraphError::QueryError { .. })),
            "{q}"
        );
    }
    tx.commit().unwrap();
    assert!(db.reader().node_info("x").is_some());
}

// ── MERGE with a map ────────────────────────────────────────────────────────

#[test]
fn merge_set_with_a_map_is_refused_by_name() {
    let mut db = GraphDb::open(&tmp("merge-map")).unwrap();
    for q in [
        "MERGE (n:P {id: 'm'}) ON CREATE SET n = {a: 1}",
        "MERGE (n:P {id: 'm'}) ON MATCH SET n += {a: 1}",
        "MERGE (n:P {id: 'm'}) ON CREATE SET n.b = 1, n = {a: 1}",
    ] {
        match query_err(&mut db, q) {
            GraphError::QueryError { detail } => assert!(
                detail.contains("not supported in mushroomdb 0.10: MERGE … SET with a map"),
                "{q}: {detail}"
            ),
            other => panic!("{q}: expected QueryError, got {other:?}"),
        }
    }
    assert!(db.node_info("m").is_none());
}

// ── A node that does not carry `id == key` is not covered ───────────────────

#[test]
fn a_node_keyed_on_another_field_treats_id_as_an_ordinary_property() {
    let mut db = GraphDb::open(&tmp("sku")).unwrap();
    // Imported with key_field = sku; `id` is just a column here.
    run(&mut db, "CREATE (n:Item {id: 'seed'})");
    db.insert_node(
        "Item",
        "sku-1",
        vec![("id".into(), Value::Int(1001)), ("n".into(), Value::Int(1))],
    )
    .unwrap();
    db.insert_node("Item", "sku-2", vec![("n".into(), Value::Int(2))])
        .unwrap();
    run(
        &mut db,
        "MATCH (n:Item) WHERE key(n) = 'sku-1' SET n.id = 2002",
    );
    assert_eq!(db.get_prop("sku-1", "id"), Some(Value::Int(2002)));
    run(
        &mut db,
        "MATCH (n:Item) WHERE key(n) = 'sku-1' SET n += {id: 'x'}",
    );
    assert_eq!(db.get_prop("sku-1", "id"), Some(Value::Str("x".into())));
    run(
        &mut db,
        "MATCH (n:Item) WHERE key(n) = 'sku-2' SET n.id = 'free'",
    );
    assert_eq!(db.get_prop("sku-2", "id"), Some(Value::Str("free".into())));
    run(&mut db, "MATCH (n:Item) WHERE key(n) = 'sku-1' REMOVE n.id");
    assert_eq!(db.get_prop("sku-1", "id"), None);
    // `=` replaces it like any property.
    run(
        &mut db,
        "MATCH (n:Item) WHERE key(n) = 'sku-2' SET n = {n: 5}",
    );
    assert_eq!(db.get_prop("sku-2", "id"), None);
    // The convention node beside them is still protected.
    for q in [
        "MATCH (n:Item) WHERE key(n) = 'seed' SET n.id = 'x'",
        "MATCH (n:Item) WHERE key(n) = 'seed' REMOVE n.id",
    ] {
        assert!(
            matches!(query_err(&mut db, q), GraphError::QueryError { .. }),
            "{q}"
        );
    }
    assert_eq!(db.get_prop("seed", "id"), Some(Value::Str("seed".into())));
}

#[test]
fn set_id_on_an_id_equals_key_node_is_refused_and_remove_too() {
    let mut db = GraphDb::open(&tmp("convention")).unwrap();
    seed(&mut db);
    for q in [
        "MATCH (n:P) WHERE n.id = 'alice' SET n.id = 'mallory'",
        "MATCH (n:P) WHERE n.id = 'alice' REMOVE n.id",
    ] {
        match query_err(&mut db, q) {
            GraphError::QueryError { detail } => {
                assert!(
                    detail.contains("key property `id`") && detail.contains("rename_node"),
                    "{detail}"
                )
            }
            other => panic!("{q}: {other:?}"),
        }
    }
    assert_eq!(db.get_prop("alice", "id"), Some(Value::Str("alice".into())));
}
