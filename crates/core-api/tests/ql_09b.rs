//! QL-09b: clauses after `UNWIND`: `UNWIND $rows AS r CREATE …`, `… MERGE …`,
//! `… MATCH … SET …`, `… REMOVE …`, `… DELETE …`. The writes run per row and
//! commit as one batch.

use core_storage::wal::decode_all;
use mushroomdb::{schema::Schema, GraphError, RealFs, RoleDef, SharedDb, Value, WriteScope};
use std::collections::BTreeMap;
use std::path::PathBuf;

type GraphDb = mushroomdb::GraphDb<RealFs>;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-ql09b-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn s(v: &str) -> Value {
    Value::Str(v.into())
}

fn map(pairs: &[(&str, Value)]) -> Value {
    Value::Map(
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
    )
}

/// `{rows: [{id: …, name: …}, …]}` for `ids`, each named `name-<id>`.
fn rows_param(ids: &[&str]) -> BTreeMap<String, Value> {
    let rows = ids
        .iter()
        .map(|id| map(&[("id", s(id)), ("name", s(&format!("name-{id}")))]))
        .collect();
    BTreeMap::from([("rows".to_string(), Value::List(rows))])
}

fn none() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

fn run(db: &mut GraphDb, q: &str, params: &BTreeMap<String, Value>) -> mushroomdb::ResultSet {
    db.query_write(q, params).unwrap()
}

fn detail(e: GraphError) -> String {
    match e {
        GraphError::QueryError { detail } => detail,
        other => panic!("expected QueryError, got {other:?}"),
    }
}

fn count(rs: &mushroomdb::ResultSet, col: &str) -> i64 {
    match rs.get(0, col) {
        Some(Value::Int(n)) => *n,
        other => panic!("column {col}: {other:?}"),
    }
}

fn wal_frames(dir: &std::path::Path) -> usize {
    let wal = std::fs::read(dir.join("wal.bin")).unwrap_or_default();
    decode_all(&wal).0.len()
}

fn wal_bytes(dir: &std::path::Path) -> Vec<u8> {
    std::fs::read(dir.join("wal.bin")).unwrap_or_default()
}

fn names(db: &GraphDb, q: &str, params: &BTreeMap<String, Value>) -> Vec<String> {
    let rs = db.query(q, params).unwrap();
    (0..rs.len())
        .map(|i| match rs.get(i, "v") {
            Some(Value::Str(s)) => s.clone(),
            Some(other) => format!("{other:?}"),
            None => "null".into(),
        })
        .collect()
}

const CREATE_PEOPLE: &str = "UNWIND $rows AS r CREATE (:Person {id: r.id, name: r.name})";

// ── CREATE ──────────────────────────────────────────────────────────────────

#[test]
fn ten_rows_become_ten_nodes_with_their_properties_in_one_wal_frame() {
    let dir = tmp("ten");
    let mut db = GraphDb::open(&dir).unwrap();
    let ids: Vec<String> = (0..10).map(|i| format!("p{i}")).collect();
    let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    let rs = run(&mut db, CREATE_PEOPLE, &rows_param(&id_refs));
    assert_eq!(count(&rs, "created"), 10);
    assert_eq!(db.commit_seq(), 1, "one commit");
    assert_eq!(wal_frames(&dir), 1, "one WAL frame for the whole statement");
    let got = names(
        &db,
        "MATCH (n:Person) RETURN n.id AS v ORDER BY n.id",
        &none(),
    );
    let mut want = ids.clone();
    want.sort();
    assert_eq!(got, want);
    for id in &ids {
        assert_eq!(db.get_prop(id, "name"), Some(s(&format!("name-{id}"))));
        assert_eq!(db.get_prop(id, "id"), Some(s(id)), "id mirrors the key");
        assert_eq!(db.node_info(id).unwrap().label, "Person");
    }
}

#[test]
fn a_null_property_value_is_an_absent_property() {
    let mut db = GraphDb::open(&tmp("null-prop")).unwrap();
    let params = BTreeMap::from([(
        "rows".to_string(),
        Value::List(vec![
            map(&[("id", s("a")), ("email", s("a@x"))]),
            map(&[("id", s("b"))]),
        ]),
    )]);
    run(
        &mut db,
        "UNWIND $rows AS r CREATE (:P {id: r.id, email: r.email})",
        &params,
    );
    assert_eq!(db.get_prop("a", "email"), Some(s("a@x")));
    assert_eq!(db.get_prop("b", "email"), None);
    assert!(db.has_node("b"));
}

#[test]
fn create_may_use_expressions_literals_and_a_second_parameter() {
    let mut db = GraphDb::open(&tmp("expr")).unwrap();
    let mut params = rows_param(&["x", "y"]);
    params.insert("tenant".into(), s("acme"));
    run(
        &mut db,
        "UNWIND $rows AS r CREATE (:P {id: r.id, who: toUpper(r.name), tenant: $tenant, n: 1 + 2})",
        &params,
    );
    assert_eq!(db.get_prop("x", "who"), Some(s("NAME-X")));
    assert_eq!(db.get_prop("y", "tenant"), Some(s("acme")));
    assert_eq!(db.get_prop("y", "n"), Some(Value::Int(3)));
}

#[test]
fn a_range_unwind_creates_without_a_parameter() {
    let mut db = GraphDb::open(&tmp("range")).unwrap();
    let rs = run(
        &mut db,
        "UNWIND range(1, 5) AS i CREATE (:N {id: toString(i), i: i})",
        &none(),
    );
    assert_eq!(count(&rs, "created"), 5);
    assert_eq!(db.get_prop("3", "i"), Some(Value::Int(3)));
}

#[test]
fn create_with_an_edge_between_two_new_nodes_per_row() {
    let mut db = GraphDb::open(&tmp("edge-new")).unwrap();
    let params = BTreeMap::from([(
        "rows".to_string(),
        Value::List(vec![
            map(&[("a", s("a1")), ("b", s("b1"))]),
            map(&[("a", s("a2")), ("b", s("b2"))]),
        ]),
    )]);
    let rs = run(
        &mut db,
        "UNWIND $rows AS r CREATE (:A {id: r.a})-[:LINKS]->(:B {id: r.b})",
        &params,
    );
    assert_eq!(count(&rs, "created"), 4);
    assert_eq!(count(&rs, "relationships_created"), 2);
    assert_eq!(db.edge_count(), 2);
    let edges = db.node_edges("a2").unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].dst_key, "b2");
}

#[test]
fn create_an_edge_between_nodes_matched_per_row() {
    let mut db = GraphDb::open(&tmp("edge-matched")).unwrap();
    run(&mut db, CREATE_PEOPLE, &rows_param(&["a", "b", "c"]));
    let params = BTreeMap::from([(
        "edges".to_string(),
        Value::List(vec![
            map(&[("from", s("a")), ("to", s("b"))]),
            map(&[("from", s("b")), ("to", s("c"))]),
            map(&[("from", s("a")), ("to", s("nobody"))]),
        ]),
    )]);
    let rs = run(
        &mut db,
        "UNWIND $edges AS e MATCH (x:Person {id: e.from}), (y:Person {id: e.to}) CREATE (x)-[:KNOWS]->(y)",
        &params,
    );
    assert_eq!(count(&rs, "created"), 0);
    assert_eq!(
        count(&rs, "relationships_created"),
        2,
        "row 143: the edges are counted"
    );
    assert_eq!(
        db.edge_count(),
        2,
        "the row naming a missing node matches nothing"
    );
    assert!(db
        .node_edges("a")
        .unwrap()
        .iter()
        .any(|e| e.edge_type == "KNOWS" && e.dst_key == "b"));
}

#[test]
fn create_returns_the_created_nodes_and_counts() {
    let mut db = GraphDb::open(&tmp("create-return")).unwrap();
    let rs = run(
        &mut db,
        "UNWIND $rows AS r CREATE (n:Person {id: r.id, name: r.name}) RETURN n.name AS v ORDER BY v",
        &rows_param(&["b", "a"]),
    );
    let got: Vec<_> = (0..rs.len()).map(|i| rs.get(i, "v").cloned()).collect();
    assert_eq!(got, vec![Some(s("name-a")), Some(s("name-b"))]);
    let rs = run(
        &mut db,
        "UNWIND $rows AS r CREATE (n:Person {id: toUpper(r.id)}) RETURN count(*) AS c",
        &rows_param(&["b", "a"]),
    );
    assert_eq!(rs.get(0, "c"), Some(&Value::Int(2)));
}

#[test]
fn comma_separated_patterns_create_each_and_may_share_an_edge() {
    let mut db = GraphDb::open(&tmp("comma")).unwrap();
    let rs = run(
        &mut db,
        "UNWIND ['a', 'b'] AS k CREATE (x:N {id: k}), (y:M {id: toUpper(k)}), (x)-[:T]->(y)",
        &none(),
    );
    assert_eq!(count(&rs, "created"), 4);
    assert_eq!(count(&rs, "relationships_created"), 2);
    assert_eq!(db.edge_count(), 2);
    assert!(db.has_node("A") && db.has_node("b"));
}

#[test]
fn set_a_literal_map_with_row_values_per_row() {
    let mut db = GraphDb::open(&tmp("set-literal-map")).unwrap();
    run(&mut db, CREATE_PEOPLE, &rows_param(&["a"]));
    let params = BTreeMap::from([(
        "rows".to_string(),
        Value::List(vec![map(&[("id", s("a")), ("v", Value::Int(4))])]),
    )]);
    run(
        &mut db,
        "UNWIND $rows AS r MATCH (n:Person {id: r.id}) SET n += {v: r.v, w: r.v * 2, gone: null}",
        &params,
    );
    assert_eq!(db.get_prop("a", "v"), Some(Value::Int(4)));
    assert_eq!(db.get_prop("a", "w"), Some(Value::Int(8)));
}

#[test]
fn an_optional_match_that_found_nothing_skips_the_set() {
    let mut db = GraphDb::open(&tmp("optional-set")).unwrap();
    run(&mut db, CREATE_PEOPLE, &rows_param(&["a"]));
    let params = BTreeMap::from([("ids".to_string(), Value::List(vec![s("a"), s("zz")]))]);
    let rs = run(
        &mut db,
        "UNWIND $ids AS i OPTIONAL MATCH (n:Person {id: i}) SET n.hit = true",
        &params,
    );
    assert_eq!(count(&rs, "properties_set"), 1);
    assert_eq!(db.get_prop("a", "hit"), Some(Value::Bool(true)));
}

#[test]
fn a_nested_unwind_over_a_row_field_creates_one_node_per_element() {
    let mut db = GraphDb::open(&tmp("nested")).unwrap();
    let params = BTreeMap::from([(
        "rows".to_string(),
        Value::List(vec![
            map(&[
                ("id", s("a")),
                ("tags", Value::List(vec![s("t1"), s("t2")])),
            ]),
            map(&[("id", s("b")), ("tags", Value::List(vec![s("t2")]))]),
        ]),
    )]);
    let rs = run(
        &mut db,
        "UNWIND $rows AS r UNWIND r.tags AS t MERGE (:Tag {id: t})",
        &params,
    );
    assert_eq!(count(&rs, "created"), 2, "t2 once");
    assert!(db.has_node("t1") && db.has_node("t2"));
}

#[test]
fn an_empty_list_writes_nothing_and_commits_nothing() {
    let dir = tmp("empty");
    let mut db = GraphDb::open(&dir).unwrap();
    let rs = run(&mut db, CREATE_PEOPLE, &rows_param(&[]));
    assert_eq!(count(&rs, "created"), 0);
    assert_eq!(db.commit_seq(), 0);
    assert_eq!(wal_frames(&dir), 0);
}

#[test]
fn a_row_without_a_string_id_is_refused_and_nothing_is_written() {
    let dir = tmp("no-id");
    let mut db = GraphDb::open(&dir).unwrap();
    let params = BTreeMap::from([(
        "rows".to_string(),
        Value::List(vec![map(&[("id", s("ok"))]), map(&[("name", s("anon"))])]),
    )]);
    let e = db
        .query_write(CREATE_PEOPLE, &params)
        .map(|_| ())
        .unwrap_err();
    assert!(
        detail(e).contains("requires a string 'id' property"),
        "named error"
    );
    assert!(!db.has_node("ok"), "row 1 was not written either");
    assert_eq!(wal_frames(&dir), 0);
}

// ── Atomicity ───────────────────────────────────────────────────────────────

#[test]
fn a_duplicate_key_in_the_list_fails_the_whole_statement() {
    let dir = tmp("dup-in-list");
    let mut db = GraphDb::open(&dir).unwrap();
    let before = wal_bytes(&dir);
    let e = db
        .query_write(CREATE_PEOPLE, &rows_param(&["a", "b", "a"]))
        .unwrap_err();
    assert!(matches!(e, GraphError::DuplicateKey { .. }), "{e:?}");
    assert!(!db.has_node("a") && !db.has_node("b"));
    assert_eq!(wal_bytes(&dir), before, "WAL unchanged");
}

#[test]
fn a_key_that_already_exists_fails_the_whole_statement() {
    let dir = tmp("dup-existing");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Person", "c", vec![("id".into(), s("c"))])
        .unwrap();
    let before = wal_bytes(&dir);
    let e = db
        .query_write(CREATE_PEOPLE, &rows_param(&["a", "b", "c", "d"]))
        .unwrap_err();
    assert!(matches!(e, GraphError::DuplicateKey { .. }), "{e:?}");
    assert!(!db.has_node("a") && !db.has_node("d"));
    assert_eq!(wal_bytes(&dir), before, "WAL unchanged");
}

#[test]
fn a_failing_set_on_a_later_row_unwrites_the_earlier_rows() {
    let dir = tmp("late-failure");
    let mut db = GraphDb::open(&dir).unwrap();
    run(&mut db, CREATE_PEOPLE, &rows_param(&["a", "b", "c"]));
    let before = wal_bytes(&dir);
    let params = BTreeMap::from([(
        "rows".to_string(),
        Value::List(vec![
            map(&[("id", s("a")), ("v", Value::Int(1))]),
            map(&[("id", s("b")), ("v", Value::Int(2))]),
            map(&[("id", s("c"))]),
        ]),
    )]);
    let e = db
        .query_write(
            "UNWIND $rows AS r MATCH (n:Person {id: r.id}) SET n.v = r.v",
            &params,
        )
        .unwrap_err();
    assert!(detail(e).contains("evaluated to null"));
    assert_eq!(db.get_prop("a", "v"), None);
    assert_eq!(wal_bytes(&dir), before, "WAL unchanged");
}

// ── MATCH correlated on the alias ───────────────────────────────────────────

#[test]
fn unwind_then_match_looks_each_row_up() {
    let mut db = GraphDb::open(&tmp("match-read")).unwrap();
    run(&mut db, CREATE_PEOPLE, &rows_param(&["a", "b", "c"]));
    let params = BTreeMap::from([(
        "ids".to_string(),
        Value::List(vec![s("c"), s("a"), s("zz")]),
    )]);
    // A plain read, not a write.
    let got = names(
        &db,
        "UNWIND $ids AS id MATCH (n:Person {id: id}) RETURN n.name AS v",
        &params,
    );
    assert_eq!(got, vec!["name-c", "name-a"]);
    let got = names(
        &db,
        "UNWIND $ids AS id OPTIONAL MATCH (n:Person {id: id}) RETURN n.name AS v",
        &params,
    );
    assert_eq!(got, vec!["name-c", "name-a", "null"]);
    let got = names(
        &db,
        "UNWIND $ids AS id WITH id WHERE id <> 'a' MATCH (n:Person {id: id}) RETURN n.name AS v",
        &params,
    );
    assert_eq!(got, vec!["name-c"]);
}

#[test]
fn unwind_match_set_updates_the_matched_nodes() {
    let dir = tmp("match-set");
    let mut db = GraphDb::open(&dir).unwrap();
    run(&mut db, CREATE_PEOPLE, &rows_param(&["a", "b", "c"]));
    let frames = wal_frames(&dir);
    let params = BTreeMap::from([(
        "rows".to_string(),
        Value::List(vec![
            map(&[("id", s("a")), ("score", Value::Int(10))]),
            map(&[("id", s("c")), ("score", Value::Int(30))]),
            map(&[("id", s("nobody")), ("score", Value::Int(99))]),
        ]),
    )]);
    let rs = run(
        &mut db,
        "UNWIND $rows AS r MATCH (n:Person {id: r.id}) SET n.score = r.score, n.seen = true",
        &params,
    );
    assert_eq!(count(&rs, "properties_set"), 4);
    assert_eq!(db.get_prop("a", "score"), Some(Value::Int(10)));
    assert_eq!(db.get_prop("c", "seen"), Some(Value::Bool(true)));
    assert_eq!(db.get_prop("b", "score"), None);
    assert_eq!(wal_frames(&dir), frames + 1, "one frame");
}

#[test]
fn unwind_match_where_filters_the_rows() {
    let mut db = GraphDb::open(&tmp("match-where")).unwrap();
    run(&mut db, CREATE_PEOPLE, &rows_param(&["a", "b", "c"]));
    let params = BTreeMap::from([("ids".to_string(), Value::List(vec![s("a"), s("b"), s("c")]))]);
    run(
        &mut db,
        "UNWIND $ids AS id MATCH (n:Person {id: id}) WHERE n.name <> 'name-b' SET n.picked = 1",
        &params,
    );
    assert_eq!(db.get_prop("a", "picked"), Some(Value::Int(1)));
    assert_eq!(db.get_prop("b", "picked"), None);
    assert_eq!(db.get_prop("c", "picked"), Some(Value::Int(1)));
}

#[test]
fn with_filters_rows_before_the_writes() {
    let mut db = GraphDb::open(&tmp("with")).unwrap();
    let params = BTreeMap::from([(
        "rows".to_string(),
        Value::List(vec![
            map(&[("id", s("keep")), ("ok", Value::Bool(true))]),
            map(&[("id", s("drop")), ("ok", Value::Bool(false))]),
        ]),
    )]);
    run(
        &mut db,
        "UNWIND $rows AS r WITH r WHERE r.ok = true CREATE (:P {id: r.id})",
        &params,
    );
    assert!(db.has_node("keep"));
    assert!(!db.has_node("drop"));
    // WITH may rename and compute.
    run(
        &mut db,
        "UNWIND $rows AS r WITH toUpper(r.id) AS k CREATE (:P {id: k})",
        &params,
    );
    assert!(db.has_node("KEEP") && db.has_node("DROP"));
}

// ── MERGE ───────────────────────────────────────────────────────────────────

#[test]
fn merge_sees_the_rows_before_it_in_the_same_statement() {
    let dir = tmp("merge-dedupe");
    let mut db = GraphDb::open(&dir).unwrap();
    let rs = run(
        &mut db,
        "UNWIND ['x', 'x', 'y', 'x'] AS k MERGE (:N {id: k})",
        &none(),
    );
    assert_eq!(count(&rs, "created"), 2);
    assert_eq!(db.node_count(), 2);
    assert_eq!(wal_frames(&dir), 1);
    // A second run finds both.
    let rs = run(
        &mut db,
        "UNWIND ['x', 'y'] AS k MERGE (:N {id: k})",
        &none(),
    );
    assert_eq!(count(&rs, "created"), 0);
    assert_eq!(db.commit_seq(), 1, "nothing to write, nothing committed");
}

#[test]
fn merge_on_create_and_on_match_set_run_per_row() {
    let mut db = GraphDb::open(&tmp("merge-on")).unwrap();
    let params = BTreeMap::from([(
        "rows".to_string(),
        Value::List(vec![
            map(&[("id", s("a")), ("v", Value::Int(1))]),
            map(&[("id", s("a")), ("v", Value::Int(2))]),
            map(&[("id", s("b")), ("v", Value::Int(3))]),
        ]),
    )]);
    run(
        &mut db,
        "UNWIND $rows AS r MERGE (n:N {id: r.id}) ON CREATE SET n.first = r.v ON MATCH SET n.last = r.v",
        &params,
    );
    assert_eq!(db.get_prop("a", "first"), Some(Value::Int(1)));
    assert_eq!(db.get_prop("a", "last"), Some(Value::Int(2)));
    assert_eq!(db.get_prop("b", "first"), Some(Value::Int(3)));
    assert_eq!(db.get_prop("b", "last"), None);
}

#[test]
fn merge_a_relationship_per_row_creates_each_node_and_edge_once() {
    let dir = tmp("merge-edge");
    let mut db = GraphDb::open(&dir).unwrap();
    let params = BTreeMap::from([(
        "rows".to_string(),
        Value::List(vec![
            map(&[("a", s("a")), ("b", s("b"))]),
            map(&[("a", s("a")), ("b", s("c"))]),
            map(&[("a", s("a")), ("b", s("b"))]),
            map(&[("a", s("c")), ("b", s("b"))]),
        ]),
    )]);
    let q = "UNWIND $rows AS r MERGE (x:P {id: r.a})-[:K]->(y:P {id: r.b}) ON CREATE SET y.hit = 1";
    let rs = run(&mut db, q, &params);
    assert_eq!(count(&rs, "created"), 3, "a, b and c once each");
    assert_eq!(count(&rs, "relationships_created"), 3);
    assert_eq!(db.edge_count(), 3, "a->b, a->c, c->b");
    assert_eq!(wal_frames(&dir), 1);
    // Again: everything exists.
    let rs = run(&mut db, q, &params);
    assert_eq!(count(&rs, "created"), 0);
    assert_eq!(db.edge_count(), 3);
}

#[test]
fn merge_after_a_create_in_the_same_statement_finds_the_created_node() {
    let mut db = GraphDb::open(&tmp("create-then-merge")).unwrap();
    let rs = run(
        &mut db,
        "UNWIND ['a', 'b'] AS k CREATE (:N {id: k}) MERGE (:N {id: k})",
        &none(),
    );
    assert_eq!(
        count(&rs, "created"),
        2,
        "the MERGE found each created node"
    );
    assert_eq!(db.node_count(), 2);
}

#[test]
fn merge_with_a_non_string_key_is_a_named_error() {
    let mut db = GraphDb::open(&tmp("merge-key")).unwrap();
    let e = db
        .query_write("UNWIND [1, 2] AS k MERGE (:N {id: k})", &none())
        .unwrap_err();
    assert!(detail(e).contains("MERGE key value must be a string"));
    assert_eq!(db.node_count(), 0);
}

// ── SET / REMOVE / DELETE ───────────────────────────────────────────────────

#[test]
fn set_plus_equals_a_row_map_adds_its_keys() {
    let mut db = GraphDb::open(&tmp("set-plus")).unwrap();
    run(&mut db, CREATE_PEOPLE, &rows_param(&["a", "b"]));
    db.set_prop("a", "keep", Value::Int(7)).unwrap();
    let params = BTreeMap::from([(
        "rows".to_string(),
        Value::List(vec![
            map(&[("id", s("a")), ("age", Value::Int(30)), ("city", s("Oslo"))]),
            map(&[("id", s("b")), ("age", Value::Int(41))]),
        ]),
    )]);
    let rs = run(
        &mut db,
        "UNWIND $rows AS r MATCH (n:Person {id: r.id}) SET n += r",
        &params,
    );
    assert_eq!(count(&rs, "properties_set"), 5);
    assert_eq!(db.get_prop("a", "age"), Some(Value::Int(30)));
    assert_eq!(db.get_prop("a", "city"), Some(s("Oslo")));
    assert_eq!(
        db.get_prop("a", "keep"),
        Some(Value::Int(7)),
        "+= keeps the rest"
    );
    assert_eq!(db.get_prop("a", "name"), Some(s("name-a")));
    assert_eq!(db.get_prop("b", "age"), Some(Value::Int(41)));
}

#[test]
fn set_equals_a_row_map_replaces_the_properties() {
    let mut db = GraphDb::open(&tmp("set-eq")).unwrap();
    run(&mut db, CREATE_PEOPLE, &rows_param(&["a"]));
    db.set_prop("a", "old", Value::Int(1)).unwrap();
    let params = BTreeMap::from([(
        "rows".to_string(),
        Value::List(vec![map(&[("id", s("a")), ("age", Value::Int(30))])]),
    )]);
    run(
        &mut db,
        "UNWIND $rows AS r MATCH (n:Person {id: r.id}) SET n = r",
        &params,
    );
    assert_eq!(db.get_prop("a", "age"), Some(Value::Int(30)));
    assert_eq!(db.get_prop("a", "old"), None, "= replaces");
    assert_eq!(db.get_prop("a", "name"), None);
    assert_eq!(
        db.get_prop("a", "id"),
        Some(s("a")),
        "the key property stays"
    );
}

#[test]
fn set_a_map_on_a_node_created_in_the_same_statement() {
    let mut db = GraphDb::open(&tmp("set-created")).unwrap();
    let params = BTreeMap::from([(
        "rows".to_string(),
        Value::List(vec![map(&[("id", s("a")), ("x", Value::Int(1))])]),
    )]);
    run(
        &mut db,
        "UNWIND $rows AS r CREATE (n:N {id: r.id}) SET n += r SET n.y = 2",
        &params,
    );
    assert_eq!(db.get_prop("a", "x"), Some(Value::Int(1)));
    assert_eq!(db.get_prop("a", "y"), Some(Value::Int(2)));
}

#[test]
fn set_a_map_that_is_not_a_map_is_a_named_error() {
    let mut db = GraphDb::open(&tmp("set-notmap")).unwrap();
    run(&mut db, CREATE_PEOPLE, &rows_param(&["a"]));
    let params = BTreeMap::from([("ids".to_string(), Value::List(vec![s("a")]))]);
    let e = db
        .query_write(
            "UNWIND $ids AS i MATCH (n:Person {id: i}) SET n += i",
            &params,
        )
        .unwrap_err();
    assert!(detail(e).contains("the value must be a map"));
}

#[test]
fn remove_a_property_per_row() {
    let mut db = GraphDb::open(&tmp("remove")).unwrap();
    run(&mut db, CREATE_PEOPLE, &rows_param(&["a", "b", "c"]));
    let params = BTreeMap::from([("ids".to_string(), Value::List(vec![s("a"), s("c")]))]);
    let rs = run(
        &mut db,
        "UNWIND $ids AS i MATCH (n:Person {id: i}) REMOVE n.name",
        &params,
    );
    assert_eq!(count(&rs, "properties_set"), 2);
    assert_eq!(db.get_prop("a", "name"), None);
    assert_eq!(db.get_prop("b", "name"), Some(s("name-b")));
    assert_eq!(db.get_prop("c", "name"), None);
}

#[test]
fn removing_the_key_property_is_refused_and_writes_nothing() {
    let dir = tmp("remove-id");
    let mut db = GraphDb::open(&dir).unwrap();
    run(&mut db, CREATE_PEOPLE, &rows_param(&["a"]));
    let before = wal_bytes(&dir);
    let params = BTreeMap::from([("ids".to_string(), Value::List(vec![s("a")]))]);
    let e = db
        .query_write(
            "UNWIND $ids AS i MATCH (n:Person {id: i}) REMOVE n.id",
            &params,
        )
        .unwrap_err();
    assert!(e.to_string().contains("key property"), "{e}");
    assert_eq!(wal_bytes(&dir), before);
}

#[test]
fn delete_and_detach_delete_per_row() {
    let mut db = GraphDb::open(&tmp("delete")).unwrap();
    run(&mut db, CREATE_PEOPLE, &rows_param(&["a", "b", "c", "d"]));
    db.insert_edge("K", "a", "b").unwrap();
    let ids = BTreeMap::from([("ids".to_string(), Value::List(vec![s("c"), s("d")]))]);
    let rs = run(
        &mut db,
        "UNWIND $ids AS i MATCH (n:Person {id: i}) DELETE n",
        &ids,
    );
    assert_eq!(count(&rs, "deleted"), 2);
    assert!(!db.has_node("c") && !db.has_node("d"));
    // A node with an edge needs DETACH.
    let ids = BTreeMap::from([("ids".to_string(), Value::List(vec![s("a")]))]);
    let e = db
        .query_write("UNWIND $ids AS i MATCH (n:Person {id: i}) DELETE n", &ids)
        .unwrap_err();
    assert!(detail(e).contains("incident edges"));
    assert!(db.has_node("a"));
    run(
        &mut db,
        "UNWIND $ids AS i MATCH (n:Person {id: i}) DETACH DELETE n",
        &ids,
    );
    assert!(!db.has_node("a"));
    assert_eq!(db.edge_count(), 0);
    assert!(db.has_node("b"));
}

// ── Refusals ────────────────────────────────────────────────────────────────

#[test]
fn a_read_clause_after_a_write_clause_is_refused_by_name() {
    let mut db = GraphDb::open(&tmp("read-after-write")).unwrap();
    let e = db
        .query_write(
            "UNWIND [1] AS i CREATE (:N {id: 'a'}) MATCH (n:N) SET n.x = 1",
            &none(),
        )
        .unwrap_err();
    assert!(
        detail(e).contains("a MATCH, UNWIND or WITH clause after a write clause"),
        "named"
    );
    assert_eq!(db.node_count(), 0);
}

#[test]
fn deleting_a_relationship_after_unwind_is_refused_by_name() {
    let mut db = GraphDb::open(&tmp("del-rel")).unwrap();
    let e = db
        .query_write("UNWIND ['a'] AS i MATCH (x)-[r:K]->(y) DELETE r", &none())
        .unwrap_err();
    assert!(detail(e).contains("DELETE of the relationship"));
}

#[test]
fn an_unwind_read_is_still_not_a_write() {
    let db = GraphDb::open(&tmp("read-stays")).unwrap();
    assert_eq!(
        names(&db, "UNWIND [1, 2] AS v RETURN v", &none()),
        vec!["Int(1)", "Int(2)"]
    );
    assert!(!mushroomdb::is_write_query("UNWIND [1] AS x RETURN x").unwrap());
    assert!(mushroomdb::is_write_query("UNWIND [1] AS x CREATE (:N {id: 'a'})").unwrap());
    assert!(mushroomdb::is_write_query("UNWIND [1] AS x MATCH (n) SET n.a = 1").unwrap());
}

// ── Transactions ────────────────────────────────────────────────────────────

/// Inside a transaction the statement runs too (TX-MW, ledger row 144):
/// buffered until COMMIT, read by the transaction's later statements.
#[test]
fn inside_a_transaction_the_statement_runs_and_commits_with_it() {
    let dir = tmp("tx");
    let db: SharedDb = SharedDb::open(&dir).unwrap();
    let mut tx = db.begin_transaction();
    let rs = tx.execute(CREATE_PEOPLE, &rows_param(&["a", "b"])).unwrap();
    assert_eq!(count(&rs, "created"), 2);
    let rs = tx
        .execute(
            "UNWIND ['a', 'b'] AS k MATCH (p:Person {id: k}) SET p.seen = true",
            &none(),
        )
        .unwrap();
    assert_eq!(count(&rs, "properties_set"), 2);
    assert!(db.reader().node_info("a").is_none(), "not committed yet");
    tx.commit().unwrap();
    let a = db.reader().node_info("a").unwrap();
    assert_eq!(a.props.get("seen"), Some(&Value::Bool(true)));
    assert!(db.reader().node_info("b").is_some());
}

#[test]
fn a_read_with_unwind_and_match_works_inside_a_transaction() {
    let dir = tmp("tx-read");
    let db: SharedDb = SharedDb::open(&dir).unwrap();
    let mut tx = db.begin_transaction();
    tx.execute("CREATE (:P {id: 'x', n: 5})", &none()).unwrap();
    let rs = tx
        .execute(
            "UNWIND ['x', 'y'] AS k MATCH (n:P {id: k}) RETURN n.n AS v",
            &none(),
        )
        .unwrap();
    assert_eq!(rs.len(), 1);
    assert_eq!(rs.get(0, "v"), Some(&Value::Int(5)));
}

// ── Differential: one statement against separate commits ────────────────────

fn dump(db: &GraphDb) -> Vec<String> {
    let mut out = Vec::new();
    for key in ["a", "b", "c", "d"] {
        if db.has_node(key) {
            let mut edges: Vec<_> = db
                .node_edges(key)
                .unwrap()
                .into_iter()
                .map(|e| format!("{}:{}>{}", e.edge_type, e.src_key, e.dst_key))
                .collect();
            edges.sort();
            edges.dedup();
            out.push(format!("{key} {:?} {edges:?}", db.node_info(key).unwrap()));
        }
    }
    out
}

#[test]
fn one_unwind_statement_equals_the_same_rows_written_one_by_one_in_every_state() {
    let params = BTreeMap::from([(
        "rows".to_string(),
        Value::List(vec![
            map(&[("a", s("a")), ("b", s("b")), ("v", Value::Int(1))]),
            map(&[("a", s("b")), ("b", s("c")), ("v", Value::Int(2))]),
            map(&[("a", s("a")), ("b", s("b")), ("v", Value::Int(3))]),
            map(&[("a", s("d")), ("b", s("a")), ("v", Value::Int(4))]),
        ]),
    )]);
    let unwind = "UNWIND $rows AS r MERGE (x:P {id: r.a})-[:K]->(y:P {id: r.b}) \
                  ON CREATE SET x.c = r.v ON MATCH SET y.m = r.v";

    let mut manual = GraphDb::open(&tmp("diff-manual")).unwrap();
    let Some(Value::List(rows)) = params.get("rows") else {
        unreachable!()
    };
    for row in rows {
        let Value::Map(m) = row else { unreachable!() };
        let (Value::Str(a), Value::Str(b), Value::Int(v)) = (&m["a"], &m["b"], &m["v"]) else {
            unreachable!()
        };
        let single = format!(
            "MERGE (x:P {{id: '{a}'}})-[:K]->(y:P {{id: '{b}'}}) \
             ON CREATE SET x.c = {v} ON MATCH SET y.m = {v}"
        );
        manual.query_write(&single, &none()).unwrap();
    }
    let dir = tmp("diff-unwind");
    let mut db = GraphDb::open(&dir).unwrap();
    run(&mut db, unwind, &params);
    let live = dump(&db);
    assert_eq!(live, dump(&manual), "one statement vs four");
    assert_eq!(db.commit_seq(), 1);
    assert_eq!(manual.commit_seq(), 4);

    let at = GraphDb::open_at(&dir, db.commit_seq() - 1).unwrap();
    assert_eq!(dump(&at), live, "open_at");
    drop(at);
    drop(db);
    let mut reopened = GraphDb::open(&dir).unwrap();
    assert_eq!(dump(&reopened), live, "WAL replay");
    reopened.snapshot().unwrap();
    drop(reopened);
    let mut after = GraphDb::open(&dir).unwrap();
    assert_eq!(dump(&after), live, "after a snapshot");
    // The same statement after the snapshot finds everything.
    let rs = run(&mut after, unwind, &params);
    assert_eq!(count(&rs, "created"), 0);
}

#[test]
fn create_set_remove_delete_statements_replay_identically_live_reopened_and_snapshotted() {
    let dir = tmp("diff-mixed");
    let mut db = GraphDb::open(&dir).unwrap();
    let rows = BTreeMap::from([(
        "rows".to_string(),
        Value::List(vec![
            map(&[("id", s("a")), ("x", Value::Int(1))]),
            map(&[("id", s("b")), ("x", Value::Int(2))]),
            map(&[("id", s("c")), ("x", Value::Int(3))]),
            map(&[("id", s("d")), ("x", Value::Int(4))]),
        ]),
    )]);
    let ids = BTreeMap::from([("ids".to_string(), Value::List(vec![s("a"), s("b")]))]);
    let gone = BTreeMap::from([("ids".to_string(), Value::List(vec![s("d")]))]);
    run(
        &mut db,
        "UNWIND $rows AS r CREATE (:P {id: r.id, x: r.x})",
        &rows,
    );
    run(
        &mut db,
        "UNWIND $rows AS r MATCH (n:P {id: r.id}) SET n += {y: r.x * 10}",
        &rows,
    );
    run(
        &mut db,
        "UNWIND $ids AS i MATCH (n:P {id: i}) REMOVE n.x",
        &ids,
    );
    run(
        &mut db,
        "UNWIND ['a'] AS k MERGE (:P {id: k})-[:K]->(:P {id: 'c'})",
        &none(),
    );
    run(
        &mut db,
        "UNWIND $ids AS i MATCH (n:P {id: i}) SET n = {z: 1}",
        &ids,
    );
    run(
        &mut db,
        "UNWIND $ids AS i MATCH (n:P {id: i}) DETACH DELETE n",
        &gone,
    );
    let live = dump(&db);
    assert!(live.iter().any(|l| l.starts_with("c ")), "{live:?}");
    assert!(!db.has_node("d"));
    assert_eq!(db.get_prop("a", "x"), None);
    assert_eq!(db.get_prop("a", "z"), Some(Value::Int(1)));
    let last = db.commit_seq();
    assert_eq!(last, 6, "one commit per statement");
    let at = GraphDb::open_at(&dir, last - 1).unwrap();
    assert_eq!(dump(&at), live, "open_at");
    drop(at);
    drop(db);
    let mut reopened = GraphDb::open(&dir).unwrap();
    assert_eq!(dump(&reopened), live, "WAL replay");
    reopened.snapshot().unwrap();
    drop(reopened);
    let after = GraphDb::open(&dir).unwrap();
    assert_eq!(dump(&after), live, "after a snapshot");
}

// ── Fix round 1: values are read as the statement goes ─────────────────────

fn counter_db(name: &str) -> GraphDb {
    let mut db = GraphDb::open(&tmp(name)).unwrap();
    run(&mut db, "CREATE (:C {id: 'c', count: 0, v: 1})", &none());
    db
}

#[test]
fn a_later_row_reads_what_an_earlier_row_set() {
    let mut db = counter_db("counter");
    let rs = run(
        &mut db,
        "UNWIND [1, 1] AS x MATCH (n:C {id: 'c'}) SET n.count = n.count + 1",
        &none(),
    );
    assert_eq!(db.get_prop("c", "count"), Some(Value::Int(2)));
    assert_eq!(count(&rs, "properties_set"), 2);
    assert_eq!(
        db.commit_seq(),
        2,
        "setup plus one commit for the statement"
    );
}

#[test]
fn a_later_set_clause_reads_what_an_earlier_one_set() {
    let mut db = counter_db("chain");
    run(
        &mut db,
        "UNWIND [1] AS x MATCH (n:C {id: 'c'}) SET n.v = 10 SET n.w = n.v",
        &none(),
    );
    assert_eq!(db.get_prop("c", "w"), Some(Value::Int(10)));
    run(
        &mut db,
        "UNWIND [1] AS x MATCH (n:C {id: 'c'}) SET n.a = 5, n.b = n.a + 1",
        &none(),
    );
    assert_eq!(db.get_prop("c", "b"), Some(Value::Int(6)));
}

#[test]
fn a_plain_match_set_item_reads_what_an_earlier_item_set() {
    let mut db = counter_db("plain-chain");
    run(
        &mut db,
        "MATCH (n:C {id: 'c'}) SET n.v = 10, n.w = n.v + 0",
        &none(),
    );
    assert_eq!(db.get_prop("c", "w"), Some(Value::Int(10)));
}

#[test]
fn a_plain_match_set_reads_the_writes_of_an_earlier_row_for_the_same_node() {
    let mut db = counter_db("plain-rows");
    run(&mut db, "CREATE (:D {id: 'd1'})", &none());
    run(&mut db, "CREATE (:D {id: 'd2'})", &none());
    run(
        &mut db,
        "MATCH (n:C {id: 'c'}), (m:D) SET n.count = n.count + 1",
        &none(),
    );
    assert_eq!(
        db.get_prop("c", "count"),
        Some(Value::Int(2)),
        "one row per D"
    );
    run(
        &mut db,
        "MATCH (n:C {id: 'c'}), (m:D) SET n += {count: n.count + 10}",
        &none(),
    );
    assert_eq!(db.get_prop("c", "count"), Some(Value::Int(22)));
}

#[test]
fn a_plain_match_set_counter_reads_per_row() {
    let mut db = counter_db("plain-counter");
    run(&mut db, "CREATE (:C {id: 'd', count: 5})", &none());
    run(&mut db, "MATCH (n:C) SET n.count = n.count + 1", &none());
    assert_eq!(db.get_prop("c", "count"), Some(Value::Int(1)));
    assert_eq!(db.get_prop("d", "count"), Some(Value::Int(6)));
}

#[test]
fn merge_on_match_counts_each_duplicate_key() {
    let mut db = GraphDb::open(&tmp("merge-counter")).unwrap();
    run(
        &mut db,
        "UNWIND ['a', 'a', 'a', 'b'] AS k MERGE (n:C {id: k}) \
         ON CREATE SET n.count = 1 ON MATCH SET n.count = n.count + 1",
        &none(),
    );
    assert_eq!(db.get_prop("a", "count"), Some(Value::Int(3)));
    assert_eq!(db.get_prop("b", "count"), Some(Value::Int(1)));
    run(
        &mut db,
        "UNWIND ['a', 'a'] AS k MERGE (n:C {id: k}) ON MATCH SET n.count = n.count + 1",
        &none(),
    );
    assert_eq!(
        db.get_prop("a", "count"),
        Some(Value::Int(5)),
        "against the store too"
    );
}

#[test]
fn a_value_may_read_a_property_of_a_node_created_in_the_statement() {
    let mut db = GraphDb::open(&tmp("create-then-read")).unwrap();
    run(
        &mut db,
        "UNWIND [1] AS x CREATE (n:C {id: 'a', v: 3}) SET n.w = n.v + 1",
        &none(),
    );
    assert_eq!(db.get_prop("a", "w"), Some(Value::Int(4)));
}

#[test]
fn a_set_value_that_reads_an_unset_property_is_null_and_refused() {
    let mut db = counter_db("read-missing");
    let e = db
        .query_write(
            "UNWIND [1] AS x MATCH (n:C {id: 'c'}) SET n.w = n.nope + 1",
            &none(),
        )
        .unwrap_err();
    assert!(detail(e).contains("evaluated to null"));
    assert_eq!(db.get_prop("c", "w"), None);
}

#[test]
fn a_value_reads_a_removed_property_as_absent_and_a_function_of_a_node() {
    let mut db = counter_db("read-removed");
    let e = db
        .query_write(
            "UNWIND [1] AS x MATCH (n:C {id: 'c'}) REMOVE n.v SET n.w = n.v + 1",
            &none(),
        )
        .unwrap_err();
    assert!(detail(e).contains("evaluated to null"));
    run(
        &mut db,
        "UNWIND [1] AS x MATCH (n:C {id: 'c'}) SET n.k = key(n), n.l = n.id",
        &none(),
    );
    assert_eq!(db.get_prop("c", "k"), Some(s("c")));
    assert_eq!(db.get_prop("c", "l"), Some(s("c")));
}

#[test]
fn a_map_set_reads_the_statements_own_writes() {
    let mut db = counter_db("map-reads");
    run(
        &mut db,
        "UNWIND [1, 1] AS x MATCH (n:C {id: 'c'}) SET n += {count: n.count + 1}",
        &none(),
    );
    assert_eq!(db.get_prop("c", "count"), Some(Value::Int(2)));
}

#[test]
fn a_failing_dependent_row_still_writes_nothing() {
    let dir = tmp("dependent-fail");
    let mut db = GraphDb::open(&dir).unwrap();
    run(&mut db, "CREATE (:C {id: 'c', count: 0})", &none());
    let before = wal_bytes(&dir);
    let e = db
        .query_write(
            "UNWIND [1, 0] AS x MATCH (n:C {id: 'c'}) SET n.count = n.count + 10 / x",
            &none(),
        )
        .unwrap_err();
    assert!(detail(e).contains("division by zero"));
    assert_eq!(wal_bytes(&dir), before);
    assert_eq!(db.get_prop("c", "count"), Some(Value::Int(0)));
}

#[test]
fn return_after_a_set_of_a_filtered_property_still_returns_the_row() {
    let mut db = GraphDb::open(&tmp("ret-filtered")).unwrap();
    run(&mut db, "CREATE (:C {id: 'a', s: 'old'})", &none());
    let rs = run(
        &mut db,
        "UNWIND [1] AS x MATCH (n:C {id: 'a', s: 'old'}) SET n.s = 'new' RETURN n.s AS v",
        &none(),
    );
    assert_eq!(rs.len(), 1);
    assert_eq!(rs.get(0, "v"), Some(&s("new")), "the post-write value");
    let rs = run(
        &mut db,
        "UNWIND [1] AS x MATCH (n:C) WHERE n.s = 'new' SET n.s = 'newer' RETURN n.s AS v",
        &none(),
    );
    assert_eq!((rs.len(), rs.get(0, "v")), (1, Some(&s("newer"))));
}

#[test]
fn return_after_remove_of_a_filtered_property_still_returns_the_row() {
    let mut db = GraphDb::open(&tmp("ret-remove")).unwrap();
    run(&mut db, "CREATE (:C {id: 'a', f: 1})", &none());
    let rs = run(
        &mut db,
        "UNWIND [1] AS x MATCH (n:C {id: 'a', f: 1}) REMOVE n.f RETURN n.id AS v",
        &none(),
    );
    assert_eq!((rs.len(), rs.get(0, "v")), (1, Some(&s("a"))));
}

#[test]
fn return_count_after_detach_delete_counts_the_deleted_rows() {
    let mut db = GraphDb::open(&tmp("ret-delete")).unwrap();
    run(&mut db, CREATE_PEOPLE, &rows_param(&["a", "b"]));
    let rs = run(
        &mut db,
        "UNWIND ['a', 'b'] AS k MATCH (n:Person {id: k}) DETACH DELETE n RETURN count(*) AS c",
        &none(),
    );
    assert_eq!(count(&rs, "c"), 2);
    assert!(!db.has_node("a") && !db.has_node("b"));
}

#[test]
fn return_after_create_with_a_nondeterministic_key_returns_every_row() {
    let mut db = GraphDb::open(&tmp("ret-rand")).unwrap();
    let rs = run(
        &mut db,
        "UNWIND range(1, 3) AS i CREATE (n:N {id: toString(rand())}) RETURN n.id AS v",
        &none(),
    );
    assert_eq!(rs.len(), 3);
    let mut seen: Vec<String> = (0..3).map(|i| format!("{:?}", rs.get(i, "v"))).collect();
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), 3, "each row returns its own node");
    assert!(seen.iter().all(|v| !v.contains("None")));
}

#[test]
fn return_projects_the_value_variables_and_properties_of_written_nodes() {
    let mut db = GraphDb::open(&tmp("ret-values")).unwrap();
    let rs = run(
        &mut db,
        "UNWIND $rows AS r CREATE (n:P {id: r.id, name: r.name}) \
         RETURN r.id AS rid, n.name AS nm, n.id AS nid ORDER BY rid",
        &rows_param(&["b", "a"]),
    );
    assert_eq!(rs.len(), 2);
    assert_eq!(rs.get(0, "rid"), Some(&s("a")));
    assert_eq!(rs.get(0, "nm"), Some(&s("name-a")));
    assert_eq!(rs.get(1, "nid"), Some(&s("b")));
}

#[test]
fn a_plain_delete_beside_many_created_edges_deletes_each_and_keeps_the_edges() {
    let mut db = GraphDb::open(&tmp("delete-beside-edges")).unwrap();
    run(
        &mut db,
        "UNWIND range(1, 2000) AS i CREATE (:X {id: toString(i)})",
        &none(),
    );
    let rs = run(
        &mut db,
        "UNWIND [1] AS u MATCH (x:X) \
         CREATE (a:N {id: toString(rand())})-[:T]->(b:N {id: toString(rand())}) DELETE x",
        &none(),
    );
    assert_eq!(count(&rs, "created"), 4000);
    assert_eq!(count(&rs, "relationships_created"), 2000);
    assert_eq!(count(&rs, "deleted"), 2000);
    assert_eq!(db.edge_count(), 2000);
}

#[test]
fn a_node_deleted_in_the_statement_no_longer_holds_the_edges_made_to_it() {
    let mut db = GraphDb::open(&tmp("delete-pending-edge")).unwrap();
    run(&mut db, CREATE_PEOPLE, &rows_param(&["a"]));
    // The edge is made, then its end is detached; a plain delete of the other
    // end then finds no edge left in the statement.
    let rs = run(
        &mut db,
        "UNWIND ['a'] AS k \
         MERGE (n:Person {id: k})-[:T]->(m:Person {id: 'm'}) DETACH DELETE n DELETE m",
        &none(),
    );
    // n, the relationship its DETACH DELETE removed (TX-MW: counted as
    // `MATCH … DETACH DELETE` counts it), and m.
    assert_eq!(count(&rs, "deleted"), 3);
    assert_eq!(db.edge_count(), 0);
}

// ── Fix round 2: a relationship variable in RETURN ─────────────────────────

fn edge_db(name: &str) -> GraphDb {
    let mut db = GraphDb::open(&tmp(name)).unwrap();
    run(
        &mut db,
        "CREATE (a:A {id: 'a1'})-[:T]->(b:A {id: 'b1'})",
        &none(),
    );
    run(
        &mut db,
        "CREATE (a:A {id: 'a2'})-[:T]->(b:A {id: 'b2'})",
        &none(),
    );
    db
}

#[test]
fn return_names_a_relationship_matched_before_a_set() {
    let mut db = edge_db("ret-rel-set");
    let rs = run(
        &mut db,
        "UNWIND [1] AS x MATCH (a:A)-[r:T]->(b:A) SET a.x = 1 \
         RETURN type(r) AS t, b.id AS b ORDER BY b",
        &none(),
    );
    assert_eq!(rs.len(), 2);
    assert_eq!(rs.get(0, "t"), Some(&s("T")));
    assert_eq!(rs.get(0, "b"), Some(&s("b1")));
    assert_eq!(rs.get(1, "b"), Some(&s("b2")));
    assert_eq!(db.get_prop("a1", "x"), Some(Value::Int(1)));
}

#[test]
fn return_reads_a_relationships_property_and_count_after_a_set() {
    let mut db = edge_db("ret-rel-props");
    // `count` is the insert count the engine keeps on every edge.
    let rs = run(
        &mut db,
        "UNWIND [1] AS x MATCH (a:A)-[r:T]->(b:A) SET b.y = 2 \
         RETURN r.count AS c, count(*) AS n",
        &none(),
    );
    assert_eq!(rs.len(), 1);
    assert_eq!(rs.get(0, "n"), Some(&Value::Int(2)));
}

#[test]
fn return_names_the_relationship_of_a_merge() {
    let mut db = GraphDb::open(&tmp("ret-rel-merge")).unwrap();
    let rs = run(
        &mut db,
        "UNWIND ['x', 'y'] AS k MERGE (a:A {id: k})-[r:T]->(b:B {id: 'b'}) \
         RETURN type(r) AS t, a.id AS a ORDER BY a",
        &none(),
    );
    assert_eq!(rs.len(), 2);
    assert_eq!(rs.get(0, "t"), Some(&s("T")));
    assert_eq!(rs.get(1, "a"), Some(&s("y")));
    assert_eq!(db.edge_count(), 2);
    // Run again: the edges exist, the relationship is still returned.
    let rs = run(
        &mut db,
        "UNWIND ['x'] AS k MERGE (a:A {id: k})-[r:T]->(b:B {id: 'b'}) RETURN type(r) AS t",
        &none(),
    );
    assert_eq!((rs.len(), rs.get(0, "t")), (1, Some(&s("T"))));
}

#[test]
fn return_names_the_relationship_of_a_merge_after_a_set() {
    let mut db = GraphDb::open(&tmp("ret-rel-merge-set")).unwrap();
    let rs = run(
        &mut db,
        "UNWIND ['x'] AS k MERGE (a:A {id: k})-[r:T]->(b:B {id: 'b'}) \
         ON CREATE SET a.made = true SET b.seen = 1 RETURN type(r) AS t, b.seen AS seen",
        &none(),
    );
    assert_eq!(rs.get(0, "t"), Some(&s("T")));
    assert_eq!(rs.get(0, "seen"), Some(&Value::Int(1)));
}

#[test]
fn return_after_a_create_of_an_edge_between_matched_nodes_names_a_matched_relationship() {
    let mut db = edge_db("ret-rel-create");
    let rs = run(
        &mut db,
        "UNWIND ['a1'] AS k MATCH (a:A {id: k})-[r:T]->(b:A) \
         CREATE (c:A {id: 'c1'})-[:U]->(b) RETURN type(r) AS t, c.id AS c",
        &none(),
    );
    assert_eq!(rs.len(), 1);
    assert_eq!(rs.get(0, "t"), Some(&s("T")));
    assert_eq!(rs.get(0, "c"), Some(&s("c1")));
}

#[test]
fn a_relationship_with_anonymous_ends_and_a_type_alternation_is_returned() {
    let mut db = edge_db("ret-rel-anon");
    run(
        &mut db,
        "CREATE (a:A {id: 'a3'})-[:U]->(b:A {id: 'b3'})",
        &none(),
    );
    let rs = run(
        &mut db,
        "UNWIND [1] AS x MATCH (a:A)-[r:T|U]->() SET a.x = 2 \
         RETURN type(r) AS t, a.id AS a ORDER BY a",
        &none(),
    );
    assert_eq!(rs.len(), 3);
    assert_eq!(rs.get(2, "t"), Some(&s("U")));
    assert_eq!(rs.get(0, "t"), Some(&s("T")));
}

#[test]
fn a_relationship_whose_end_the_statement_deleted_is_null() {
    let mut db = edge_db("ret-rel-deleted");
    let rs = run(
        &mut db,
        "UNWIND ['a1'] AS k MATCH (a:A {id: k})-[r:T]->(b:A) DETACH DELETE a \
         RETURN type(r) AS t, count(*) AS n",
        &none(),
    );
    assert_eq!(rs.len(), 1);
    assert_eq!(rs.get(0, "n"), Some(&Value::Int(1)));
    assert_eq!(rs.get(0, "t"), None, "the relationship went with its end");
}

#[test]
fn a_variable_length_relationship_in_return_is_refused_by_name_and_writes_nothing() {
    let mut db = edge_db("ret-rel-varlen");
    let e = db
        .query_write(
            "UNWIND [1] AS x MATCH (a:A)-[r:T*1..2]->(b:A) SET a.x = 1 RETURN r.length AS n",
            &none(),
        )
        .unwrap_err();
    assert!(detail(e)
        .contains("not supported in mushroomdb 0.10: the variable-length relationship `r`"));
    assert_eq!(db.get_prop("a1", "x"), None);
}

// ── Scale ───────────────────────────────────────────────────────────────────

#[test]
fn ten_thousand_rows_are_one_batch_and_do_not_go_quadratic() {
    let dir = tmp("scale");
    let mut db = GraphDb::open(&dir).unwrap();
    let ids: Vec<String> = (0..10_000).map(|i| format!("n{i}")).collect();
    let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    let started = std::time::Instant::now();
    let rs = run(&mut db, CREATE_PEOPLE, &rows_param(&id_refs));
    let elapsed = started.elapsed();
    assert_eq!(count(&rs, "created"), 10_000);
    assert_eq!(db.node_count(), 10_000);
    assert_eq!(wal_frames(&dir), 1);
    // Debug build, shared machine: a quadratic pass over 10k rows takes minutes.
    assert!(elapsed.as_secs() < 60, "10k rows took {elapsed:?}");
}

// ── Role-scoped writes ──────────────────────────────────────────────────────

fn scoped_store(name: &str, create_labels: &[&str]) -> GraphDb {
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
                create_labels: create_labels.iter().map(|s| s.to_string()).collect(),
                update_labels: vec!["Mine".into()],
                delete_labels: vec![],
                create_edge_types: vec![],
                delete_edge_types: vec![],
            }),
        }],
    })
    .unwrap();
    db.insert_node("Mine", "m1", vec![]).unwrap();
    db.insert_node("Secret", "s", vec![]).unwrap();
    db
}

#[test]
fn a_scoped_role_creates_only_inside_its_scope_and_all_or_nothing() {
    let mut db = scoped_store("scoped-create", &["Mine"]);
    let ok = BTreeMap::from([("ids".to_string(), Value::List(vec![s("n1"), s("n2")]))]);
    db.query_write_authz("writer", "UNWIND $ids AS i CREATE (:Mine {id: i})", &ok)
        .unwrap();
    assert!(db.has_node("n1") && db.has_node("n2"));
    let e = db
        .query_write_authz(
            "writer",
            "UNWIND $ids AS i CREATE (:Secret {id: i})",
            &BTreeMap::from([("ids".to_string(), Value::List(vec![s("z1")]))]),
        )
        .unwrap_err();
    assert!(matches!(e, GraphError::RoleWriteDenied { .. }), "{e:?}");
    assert!(!db.has_node("z1"));
}

#[test]
fn a_scoped_unwind_match_cannot_touch_a_hidden_node() {
    let mut db = scoped_store("scoped-hidden", &["Mine"]);
    let ids = BTreeMap::from([("ids".to_string(), Value::List(vec![s("m1"), s("s")]))]);
    db.query_write_authz(
        "writer",
        "UNWIND $ids AS i MATCH (n {id: i}) SET n.touched = 1",
        &ids,
    )
    .unwrap();
    assert_eq!(db.get_prop("m1", "touched"), Some(Value::Int(1)));
    assert_eq!(db.get_prop("s", "touched"), None, "hidden node untouched");
}

#[test]
fn a_scoped_merge_follows_the_same_scope_rules() {
    let mut db = scoped_store("scoped-merge", &["Mine"]);
    let ids = BTreeMap::from([(
        "ids".to_string(),
        Value::List(vec![s("m1"), s("m9"), s("m9")]),
    )]);
    db.query_write_authz("writer", "UNWIND $ids AS i MERGE (:Mine {id: i})", &ids)
        .unwrap();
    assert!(db.has_node("m9"));
    let hidden = BTreeMap::from([("ids".to_string(), Value::List(vec![s("s")]))]);
    let e = db
        .query_write_authz("writer", "UNWIND $ids AS i MERGE (:Mine {id: i})", &hidden)
        .unwrap_err();
    assert!(matches!(e, GraphError::RoleWriteDenied { .. }), "{e:?}");
}

/// A list comprehension or `reduce` in a per-row SET value reads the node's
/// properties as the row is written, and a local variable that shares a node
/// variable's name stays the local variable.
#[test]
fn a_comprehension_or_reduce_in_a_set_value_reads_per_row_and_keeps_its_own_variables() {
    let mut db = counter_db("comprehension");
    run(
        &mut db,
        "UNWIND [1, 1] AS x MATCH (n:C {id: 'c'}) \
         SET n.count = reduce(acc = n.count, i IN [x] | acc + i)",
        &none(),
    );
    assert_eq!(db.get_prop("c", "count"), Some(Value::Int(2)));
    run(
        &mut db,
        "UNWIND [1] AS x MATCH (n:C {id: 'c'}) \
         SET n.l = [n IN [1, 2] | n * 10], n.r = reduce(n = 0, i IN [1, 2, 3] | n + i), \
         n.p = [n IN [{v: 5}] | n.v]",
        &none(),
    );
    assert_eq!(
        db.get_prop("c", "p"),
        Some(Value::List(vec![Value::Int(5)])),
        "`n.v` inside the comprehension reads the local `n`, not the node's `v` (1)"
    );
    assert_eq!(
        db.get_prop("c", "l"),
        Some(Value::List(vec![Value::Int(10), Value::Int(20)]))
    );
    assert_eq!(db.get_prop("c", "r"), Some(Value::Int(6)));
    run(
        &mut db,
        "UNWIND [1, 1] AS x MATCH (n:C {id: 'c'}) \
         SET n.m = [i IN [n.count] WHERE i > 0 | i + x]",
        &none(),
    );
    assert_eq!(
        db.get_prop("c", "m"),
        Some(Value::List(vec![Value::Int(3)]))
    );
}
