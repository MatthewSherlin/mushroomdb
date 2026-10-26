//! QL-NM: a node needs neither an `id` nor a label, and CREATE takes several
//! patterns.
//!
//! A node created without a string `id` gets a generated key (`_:` and 32 hex
//! digits), chosen when the statement runs and carried by the WAL record, so a
//! replay, a snapshot, `open_at` and a backup read the same key back. A node
//! created without a label is stored under the empty label: `MATCH (n)` finds
//! it, `MATCH (n:L)` does not, and `labels(n)` is `[]`. A string `id` is still
//! the key, exactly as before. A MERGE pattern that is not a key MERGE (a
//! label and one string property) matches by label and properties.

use mushroomdb::WriteScope;
use mushroomdb::{schema::Schema, GraphError, RealFs, RoleDef, SharedDb, SnapshotOptions, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

type GraphDb = mushroomdb::GraphDb<RealFs>;
type Rows = Vec<Vec<Option<Value>>>;

const ARCHIVING: SnapshotOptions = SnapshotOptions {
    keep_wal: false,
    archive_wal: true,
};

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "graphdb-qlnm-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn p() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

fn s(v: &str) -> Value {
    Value::Str(v.into())
}

fn rows(rs: &mushroomdb::ResultSet) -> Rows {
    (0..rs.len()).map(|i| rs.row(i).to_vec()).collect()
}

fn run(db: &mut GraphDb, q: &str) -> mushroomdb::ResultSet {
    db.query_write(q, &p())
        .unwrap_or_else(|e| panic!("{q}: {e}"))
}

fn read(db: &GraphDb, q: &str) -> mushroomdb::ResultSet {
    db.query(q, &p()).unwrap_or_else(|e| panic!("{q}: {e}"))
}

fn int(rs: &mushroomdb::ResultSet, col: &str) -> i64 {
    match rs.get(0, col) {
        Some(Value::Int(n)) => *n,
        other => panic!("column {col}: {other:?}"),
    }
}

fn text(v: Option<&Value>) -> String {
    match v {
        Some(Value::Str(s)) => s.clone(),
        other => panic!("expected a string, got {other:?}"),
    }
}

fn stats(rs: &mushroomdb::ResultSet) -> mushroomdb::WriteStats {
    rs.stats().cloned().expect("a write carries its stats")
}

fn is_generated(key: &str) -> bool {
    key.len() == 34
        && key.starts_with("_:")
        && key[2..]
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// Every node: key, labels and properties, by key.
const NODES: &str = "MATCH (n) RETURN key(n) AS k, labels(n) AS l, properties(n) AS p ORDER BY k";
/// The same without the keys, which differ between two runs of one script.
const SHAPE: &str =
    "MATCH (n) RETURN labels(n) AS l, properties(n) AS p ORDER BY toString(p), toString(l)";
const EDGES: &str =
    "MATCH (a)-[r]->(b) RETURN key(a) AS a, type(r) AS t, key(b) AS b ORDER BY a, t, b";

fn nodes(db: &GraphDb) -> Rows {
    rows(&read(db, NODES))
}

fn edges(db: &GraphDb) -> Rows {
    rows(&read(db, EDGES))
}

fn count(db: &GraphDb, q: &str) -> i64 {
    int(&read(db, q), "c")
}

// ── CREATE ──────────────────────────────────────────────────────────────────

#[test]
fn create_takes_a_node_with_no_label_and_no_properties() {
    let mut db = GraphDb::open(&tmp("empty")).unwrap();
    let rs = run(&mut db, "CREATE ()");
    let st = stats(&rs);
    assert_eq!(st.nodes_created, 1);
    assert_eq!(st.labels_added, 0, "a node with no label adds none");
    assert_eq!(count(&db, "MATCH (n) RETURN count(n) AS c"), 1);
    let rs = read(
        &db,
        "MATCH (n) RETURN key(n) AS k, labels(n) AS l, n.label AS nl, properties(n) AS p",
    );
    assert!(is_generated(&text(rs.get(0, "k"))), "{:?}", rs.get(0, "k"));
    assert_eq!(rs.get(0, "l"), Some(&Value::List(vec![])));
    assert_eq!(
        rs.get(0, "nl"),
        None,
        "n.label of an unlabelled node is null"
    );
    assert_eq!(
        rs.get(0, "p"),
        Some(&Value::Map(BTreeMap::new())),
        "no id property"
    );
}

#[test]
fn create_takes_a_label_or_a_map_without_an_id() {
    let mut db = GraphDb::open(&tmp("noid")).unwrap();
    let st = stats(&run(&mut db, "CREATE (:A {name: 'x'})"));
    assert_eq!((st.nodes_created, st.labels_added), (1, 1));
    run(&mut db, "CREATE ({name: 'y'})");
    run(&mut db, "CREATE (:B)");
    let rs = read(
        &db,
        "MATCH (n) RETURN key(n) AS k, labels(n) AS l, n.name AS name ORDER BY name",
    );
    assert_eq!(rs.len(), 3);
    for row in 0..3 {
        assert!(is_generated(&text(rs.get(row, "k"))));
    }
    assert_eq!(rs.get(0, "l"), Some(&Value::List(vec![s("A")])));
    assert_eq!(rs.get(1, "l"), Some(&Value::List(vec![])));
    assert_eq!(rs.get(2, "l"), Some(&Value::List(vec![s("B")])));
    assert_eq!(rs.get(2, "name"), None);
}

#[test]
fn a_string_id_is_still_the_key() {
    let mut db = GraphDb::open(&tmp("id")).unwrap();
    run(&mut db, "CREATE (:P {id: 'alice', age: 3})");
    assert_eq!(db.node_info("alice").unwrap().label, "P");
    match db.query_write("CREATE (:P {id: 'alice'})", &p()) {
        Err(GraphError::DuplicateKey { key }) => assert_eq!(key, "alice"),
        other => panic!("{other:?}"),
    }
    let rs = run(&mut db, "MERGE (n:P {id: 'alice'}) RETURN n.age AS age");
    assert_eq!(rs.get(0, "age"), Some(&Value::Int(3)));
    assert_eq!(
        count(&db, "MATCH (n) RETURN count(n) AS c"),
        1,
        "MERGE upserts by key"
    );
    // A label-less node with a string id: the id is its key.
    run(&mut db, "CREATE ({id: 'bob'})");
    let bob = db.node_info("bob").unwrap();
    assert_eq!(bob.label, "");
    assert_eq!(bob.props.get("id"), Some(&s("bob")));
    // The same in a comma pattern and in the per-row form.
    run(&mut db, "CREATE (:P {id: 'c1'}), (:P {id: 'c2'})");
    run(&mut db, "UNWIND ['u1', 'u2'] AS k CREATE (:P {id: k})");
    for key in ["c1", "c2", "u1", "u2"] {
        assert!(db.node_info(key).is_some(), "{key}");
    }
}

#[test]
fn a_non_string_id_is_an_ordinary_property() {
    let mut db = GraphDb::open(&tmp("intid")).unwrap();
    run(&mut db, "CREATE (:A {id: 1})");
    let rs = read(&db, "MATCH (n:A) RETURN key(n) AS k, n.id AS id");
    assert!(is_generated(&text(rs.get(0, "k"))));
    assert_eq!(rs.get(0, "id"), Some(&Value::Int(1)), "the stored id wins");
    assert_eq!(
        count(&db, "MATCH (n:A {id: 1}) RETURN count(n) AS c"),
        1,
        "a pattern finds it"
    );
    assert_eq!(count(&db, "MATCH (n {id: 1}) RETURN count(n) AS c"), 1);
    // The id≡key rule does not hold it: it may change.
    run(&mut db, "MATCH (n:A) SET n.id = 2");
    assert_eq!(
        read(&db, "MATCH (n:A) RETURN n.id AS id").get(0, "id"),
        Some(&Value::Int(2))
    );
}

#[test]
fn create_takes_several_patterns_and_clauses() {
    let mut db = GraphDb::open(&tmp("comma")).unwrap();
    let st = stats(&run(
        &mut db,
        "CREATE (a:A {n: 1}), (b:B {n: 2}), (a)-[:R]->(b)",
    ));
    assert_eq!((st.nodes_created, st.relationships_created), (2, 1));
    let st = stats(&run(&mut db, "CREATE ()-[:K]->(), (:C)"));
    assert_eq!((st.nodes_created, st.relationships_created), (3, 1));
    let st = stats(&run(
        &mut db,
        "CREATE (x:X) CREATE (y:Y) CREATE (x)-[:S]->(y)",
    ));
    assert_eq!((st.nodes_created, st.relationships_created), (2, 1));
    assert_eq!(count(&db, "MATCH (n) RETURN count(n) AS c"), 7);
    assert_eq!(count(&db, "MATCH ()-[r]->() RETURN count(r) AS c"), 3);
    assert_eq!(
        count(
            &db,
            "MATCH (a:A)-[:R]->(b:B) WHERE a.n = 1 AND b.n = 2 RETURN count(*) AS c"
        ),
        1
    );
    assert_eq!(count(&db, "MATCH (:X)-[:S]->(:Y) RETURN count(*) AS c"), 1);
    // A name bound twice in one CREATE is refused, as before.
    assert!(db.query_write("CREATE (a), (a)", &p()).is_err());
}

#[test]
fn an_unbound_variable_in_a_new_node_is_refused() {
    let mut db = GraphDb::open(&tmp("unbound")).unwrap();
    for q in [
        "CREATE (b {name: missing})",
        "CREATE (), ({name: missing})",
        "MERGE (n {name: missing})",
    ] {
        let e = db.query_write(q, &p()).expect_err(q).to_string();
        assert!(
            e.contains("missing") || e.contains("reads a variable"),
            "{q}: {e}"
        );
    }
    assert_eq!(count(&db, "MATCH (n) RETURN count(n) AS c"), 0);
}

#[test]
fn match_then_create_mixes_bound_and_new_nodes() {
    let mut db = GraphDb::open(&tmp("mixed")).unwrap();
    run(&mut db, "CREATE (:A {id: 'a'}), (:A {id: 'b'})");
    let st = stats(&run(
        &mut db,
        "MATCH (a:A) CREATE (a)-[:R]->(), (:B {from: a.id})",
    ));
    assert_eq!((st.nodes_created, st.relationships_created), (4, 2));
    assert_eq!(
        count(
            &db,
            "MATCH (:A)-[:R]->(m) WHERE labels(m) = [] RETURN count(m) AS c"
        ),
        2
    );
    assert_eq!(count(&db, "MATCH (b:B) RETURN count(b) AS c"), 2);
}

#[test]
fn match_all_finds_unlabelled_nodes_and_a_label_excludes_them() {
    let mut db = GraphDb::open(&tmp("scan")).unwrap();
    run(&mut db, "CREATE (), (:A), ({x: 1})");
    assert_eq!(count(&db, "MATCH (n) RETURN count(n) AS c"), 3);
    assert_eq!(count(&db, "MATCH (n:A) RETURN count(n) AS c"), 1);
    assert_eq!(count(&db, "MATCH (n) WHERE n:A RETURN count(n) AS c"), 1);
    assert_eq!(count(&db, "MATCH (n {x: 1}) RETURN count(n) AS c"), 1);
    let labels = read(&db, "CALL db.labels() YIELD label RETURN label");
    assert_eq!(
        rows(&labels),
        vec![vec![Some(s("A"))]],
        "the empty label is not listed"
    );
    // An empty label name is not a label anyone can write.
    assert!(db.query_write("CREATE (:``)", &p()).is_err());
}

#[test]
fn create_return_projects_the_new_nodes() {
    let mut db = GraphDb::open(&tmp("return")).unwrap();
    let rs = run(&mut db, "CREATE (n {name: 'a'}), (m:M) RETURN n.name AS name, labels(n) AS l, key(n) AS k, labels(m) AS lm");
    assert_eq!(rs.get(0, "name"), Some(&s("a")));
    assert_eq!(rs.get(0, "l"), Some(&Value::List(vec![])));
    assert_eq!(rs.get(0, "lm"), Some(&Value::List(vec![s("M")])));
    let key = text(rs.get(0, "k"));
    assert!(is_generated(&key));
    assert!(db.node_info(&key).is_some());
}

#[test]
fn generated_keys_are_distinct() {
    let mut db = GraphDb::open(&tmp("distinct")).unwrap();
    run(&mut db, "UNWIND range(1, 200) AS i CREATE ({i: i})");
    for _ in 0..50 {
        run(&mut db, "CREATE ()");
    }
    let keys: BTreeSet<String> = nodes(&db).iter().map(|r| text(r[0].as_ref())).collect();
    assert_eq!(keys.len(), 250);
    assert!(keys.iter().all(|k| is_generated(k)));
}

#[test]
fn a_store_with_generated_and_unlabelled_nodes_stays_in_its_format() {
    let dir = tmp("format");
    let mut db = GraphDb::open(&dir).unwrap();
    run(&mut db, "CREATE (), (:A {name: 'x'})-[:R]->({id: 1})");
    assert!(
        !db.is_v11(),
        "a generated key and an empty label are an ordinary key and label"
    );
    db.snapshot_with(ARCHIVING).unwrap();
    assert!(!db.is_v11());
}

// ── MERGE ───────────────────────────────────────────────────────────────────

#[test]
fn merge_matches_by_label_and_properties() {
    let mut db = GraphDb::open(&tmp("merge")).unwrap();
    run(&mut db, "CREATE ({name: 'a'}), (:L {num: 42})");
    let st = stats(&run(&mut db, "MERGE (n {name: 'a'})"));
    assert_eq!(st.nodes_created, 0, "found by its property");
    let st = stats(&run(&mut db, "MERGE (n {name: 'b'})"));
    assert_eq!(st.nodes_created, 1);
    let st = stats(&run(&mut db, "MERGE (n:L {num: 42})"));
    assert_eq!(st.nodes_created, 0, "a non-string property matches");
    let st = stats(&run(&mut db, "MERGE (n:L {num: 43})"));
    assert_eq!(st.nodes_created, 1);
    let st = stats(&run(&mut db, "MERGE (n:Fresh)"));
    assert_eq!(st.nodes_created, 1);
    let st = stats(&run(&mut db, "MERGE (n:Fresh)"));
    assert_eq!(st.nodes_created, 0);
    let st = stats(&run(&mut db, "MERGE ()"));
    assert_eq!(st.nodes_created, 0, "any node matches an empty pattern");
    assert_eq!(count(&db, "MATCH (n) RETURN count(n) AS c"), 5);
    let rs = read(
        &db,
        "MATCH (n {name: 'b'}) RETURN key(n) AS k, labels(n) AS l",
    );
    assert!(is_generated(&text(rs.get(0, "k"))));
    assert_eq!(rs.get(0, "l"), Some(&Value::List(vec![])));
}

#[test]
fn merge_binds_every_match_and_runs_its_set_for_each() {
    let mut db = GraphDb::open(&tmp("merge-many")).unwrap();
    run(&mut db, "CREATE (:X {v: 1}), (:X {v: 1}), (:X {v: 2})");
    let rs = run(
        &mut db,
        "MERGE (n:X {v: 1}) ON MATCH SET n.seen = true RETURN count(*) AS c",
    );
    assert_eq!(int(&rs, "c"), 2);
    assert_eq!(stats(&rs).nodes_created, 0);
    assert_eq!(
        count(&db, "MATCH (n:X) WHERE n.seen RETURN count(n) AS c"),
        2
    );
    let rs = run(
        &mut db,
        "MERGE (n:X {v: 3}) ON CREATE SET n.new = true RETURN n.new AS new",
    );
    assert_eq!(rs.get(0, "new"), Some(&Value::Bool(true)));
}

#[test]
fn merge_sees_the_statements_own_writes() {
    let mut db = GraphDb::open(&tmp("merge-own")).unwrap();
    let st = stats(&run(&mut db, "UNWIND [1, 1, 2] AS x MERGE (n:N {v: x})"));
    assert_eq!(
        st.nodes_created, 2,
        "the second row finds the first row's node"
    );
    let st = stats(&run(&mut db, "CREATE (:M {v: 1}) MERGE (n:M {v: 1})"));
    assert_eq!(
        st.nodes_created, 1,
        "a MERGE after a CREATE finds the created node"
    );
}

#[test]
fn merge_of_a_relationship_with_a_pattern_end_matches_the_whole_path() {
    let mut db = GraphDb::open(&tmp("merge-path")).unwrap();
    run(&mut db, "CREATE (:A {id: 'a'}), (:B {n: 1})");
    // No A-[:T]->B path: a new B is created, though a B exists.
    let st = stats(&run(&mut db, "MATCH (a:A) MERGE (a)-[:T]->(b:B {n: 1})"));
    assert_eq!((st.nodes_created, st.relationships_created), (1, 1));
    // Now the path exists: nothing is created.
    let st = stats(&run(&mut db, "MATCH (a:A) MERGE (a)-[:T]->(b:B {n: 1})"));
    assert_eq!((st.nodes_created, st.relationships_created), (0, 0));
    let st = stats(&run(&mut db, "MERGE (x:X)-[:U]->(y)"));
    assert_eq!((st.nodes_created, st.relationships_created), (2, 1));
    let st = stats(&run(&mut db, "MERGE (x:X)-[:U]->(y)"));
    assert_eq!((st.nodes_created, st.relationships_created), (0, 0));
    assert_eq!(count(&db, "MATCH (n:B) RETURN count(n) AS c"), 2);
    let e = db
        .query_write("MATCH (a:A) MERGE (a)-[:T {w: null}]->(b)", &p())
        .unwrap_err()
        .to_string();
    assert!(e.contains("null"), "{e}");
}

#[test]
fn merge_with_a_string_id_and_more_properties_uses_the_key() {
    let mut db = GraphDb::open(&tmp("merge-id")).unwrap();
    let st = stats(&run(&mut db, "MERGE (n:P {id: 'k', age: 3})"));
    assert_eq!(st.nodes_created, 1);
    assert_eq!(
        db.node_info("k").unwrap().props.get("age"),
        Some(&Value::Int(3))
    );
    let st = stats(&run(&mut db, "MERGE (n:P {id: 'k', age: 3})"));
    assert_eq!(st.nodes_created, 0);
    let st = stats(&run(&mut db, "MERGE (n {id: 'k'})"));
    assert_eq!(
        st.nodes_created, 0,
        "a label-less key MERGE finds the node by key"
    );
}

/// Owner decision (QL-NM round 3): only `id` is a key lookup. A label and
/// any other single property match by label and property; a created node
/// takes the value as its key when it is a string and that key is free.
#[test]
fn merge_on_a_single_property_matches_the_property_not_the_key() {
    let mut db = GraphDb::open(&tmp("merge-prop")).unwrap();
    // Fresh: the node is created, keyed by the value (the 0.9 key).
    let st = stats(&run(&mut db, "MERGE (:P {name: 'nk'})"));
    assert_eq!(st.nodes_created, 1);
    assert_eq!(
        db.node_info("nk").unwrap().props.get("name"),
        Some(&s("nk"))
    );
    assert_eq!(
        stats(&run(&mut db, "MERGE (:P {name: 'nk'})")).nodes_created,
        0
    );
    // A node with that property under another key is matched, not duplicated.
    run(&mut db, "CREATE (:P {id: 'p1', name: 'B'})");
    let st = stats(&run(&mut db, "MERGE (n:P {name: 'B'})"));
    assert_eq!(st.nodes_created, 0, "matched by the property");
    // A generated-key node with the property is matched too.
    run(&mut db, "CREATE (:P {name: 'A'})");
    assert_eq!(
        stats(&run(&mut db, "MERGE (:P {name: 'A'})")).nodes_created,
        0
    );
    // A node keyed by the value whose property differs is not a match.
    run(&mut db, "CREATE (:P {id: 'C', name: 'other'})");
    let rs = run(&mut db, "MERGE (n:P {name: 'C'}) RETURN key(n) AS k");
    let k = text(rs.get(0, "k"));
    assert!(
        is_generated(&k),
        "`C` is taken, so the new node's key is generated: {k}"
    );
    // A different label with the same value is a second node.
    run(&mut db, "CREATE (:Q {id: 'D'})");
    let rs = run(
        &mut db,
        "MERGE (n:P {name: 'D'}) RETURN key(n) AS k, labels(n) AS l",
    );
    assert_eq!(rs.get(0, "l"), Some(&Value::List(vec![s("P")])));
    assert!(is_generated(&text(rs.get(0, "k"))));
    assert_eq!(
        db.node_info("D").unwrap().label,
        "Q",
        "the Q node is untouched"
    );
    // A non-string value: matched by the property, created with a generated key.
    let rs = run(&mut db, "MERGE (n:P {num: 7}) RETURN key(n) AS k");
    assert!(is_generated(&text(rs.get(0, "k"))));
    assert_eq!(
        stats(&run(&mut db, "MERGE (n:P {num: 7})")).nodes_created,
        0
    );
    // Several matches: every one is bound.
    run(&mut db, "CREATE (:R {v: 'x'}), (:R {v: 'x'})");
    let rs = run(&mut db, "MERGE (n:R {v: 'x'}) RETURN count(*) AS c");
    assert_eq!(int(&rs, "c"), 2);
    // `id` is still the key lookup.
    run(&mut db, "CREATE (:P {id: 'k1', name: 'zz'})");
    assert_eq!(
        stats(&run(&mut db, "MERGE (n:P {id: 'k1'})")).nodes_created,
        0
    );
}

/// The same MERGE statements with and without an equality index on the
/// property give the same graph.
#[test]
fn merge_on_a_property_gives_the_same_answer_with_and_without_an_index() {
    let script = [
        "CREATE (:P {id: 'p1', name: 'B'}), (:P {name: 'A'}), (:Q {name: 'A'})",
        "UNWIND ['A', 'B', 'C', 'C', 'A'] AS x MERGE (n:P {name: x}) ON MATCH SET n.hits = coalesce(n.hits, 0) + 1",
        "MERGE (n:P {name: 'B'}) ON MATCH SET n.seen = true",
    ];
    // 0: no index, 1: an equality index, 2: a range index.
    let shape = |index: u8| {
        let mut db = GraphDb::open(&tmp(&format!("idx-{index}"))).unwrap();
        match index {
            1 => db.enable_index("P", "name").unwrap(),
            2 => {
                run(&mut db, "CREATE INDEX FOR (n:P) ON (n.name)");
            }
            _ => {}
        }
        for q in script {
            run(&mut db, q);
        }
        rows(&read(&db, SHAPE))
    };
    let plain = shape(0);
    assert_eq!(plain, shape(1), "equality index");
    assert_eq!(plain, shape(2), "range index");
    assert_eq!(plain.len(), 4, "A, B and C under P, A under Q: {plain:?}");
}

/// A store whose nodes were made by 0.9's `MERGE (:P {name: …})` (key ==
/// value, the property stored) still merges onto them.
#[test]
fn a_store_merged_by_name_in_0_9_still_merges_onto_its_nodes() {
    let mut db = GraphDb::open(&tmp("merge-09")).unwrap();
    for k in ["ada", "bob"] {
        db.insert_node("P", k, vec![("name".into(), s(k))]).unwrap();
    }
    let st = stats(&run(
        &mut db,
        "UNWIND ['ada', 'bob', 'cy'] AS x MERGE (:P {name: x})",
    ));
    assert_eq!(st.nodes_created, 1);
    assert_eq!(db.node_count(), 3);
    assert_eq!(
        db.node_info("cy").unwrap().label,
        "P",
        "a new one keyed as 0.9 would"
    );
}

/// A role-scoped MERGE on a single property: scope first, through the mask;
/// a hidden node with the property is not bound, and a hidden occupant of
/// the value's key is not revealed (the new node falls back to a generated
/// key).
#[test]
fn a_role_scoped_single_property_merge_sees_only_its_view() {
    fn scoped(name: &str) -> GraphDb {
        let mut db = GraphDb::open(&tmp(name)).unwrap();
        db.apply_schema(&Schema {
            fulltext: vec![],
            indexes: vec![],
            rules: vec![],
            views: vec![],
            roles: vec![RoleDef {
                name: "writer".into(),
                keys: vec![],
                labels: vec!["P".into()],
                visible_where: None,
                namespaces: None,
                write: Some(WriteScope {
                    create_labels: vec!["P".into()],
                    update_labels: vec!["P".into()],
                    delete_labels: vec![],
                    create_edge_types: vec![],
                    delete_edge_types: vec![],
                    update_edge_types: vec![],
                }),
            }],
        })
        .unwrap();
        run(&mut db, "CREATE (:Q {id: 'secret', name: 'secret'})");
        db
    }
    let mut db = scoped("role-prop");
    let e = db
        .query_write_authz("writer", "MERGE (n:Q {name: 'secret'})", &p())
        .unwrap_err();
    assert!(matches!(e, GraphError::RoleWriteDenied { .. }), "{e:?}");
    let rs = db
        .query_write_authz(
            "writer",
            "MERGE (n:P {name: 'secret'}) RETURN n.name AS name",
            &p(),
        )
        .unwrap();
    assert_eq!(rs.get(0, "name"), Some(&s("secret")));
    assert_eq!(
        db.node_info("secret").unwrap().label,
        "Q",
        "the hidden node is untouched"
    );
    assert_eq!(
        db.node_count(),
        2,
        "a new P node, keyed apart from the hidden one"
    );
}

// ── Durability: replay, snapshot, open_at, transactions, readers, backup ────

const SCRIPT: &[&str] = &[
    "CREATE ()",
    "CREATE (:A {name: 'a'}), ({name: 'b'})",
    "CREATE (:K {id: 'k1'})-[:R]->(), (:K {id: 1})",
    "MATCH (a:A) CREATE (a)-[:S]->(:B {n: 1})",
    "MERGE (n {name: 'c'})",
    "MATCH (n {name: 'b'}) SET n.x = 1",
    "MATCH (n:B) DETACH DELETE n",
    "UNWIND [1, 2] AS i CREATE ({i: i})",
];

#[test]
fn generated_nodes_are_identical_live_replayed_snapshotted_and_at() {
    let dir = tmp("axes");
    let mut db = GraphDb::open(&dir).unwrap();
    let mut at: Vec<(u64, Rows, Rows)> = Vec::new();
    for (i, q) in SCRIPT.iter().enumerate() {
        run(&mut db, q);
        at.push((db.commit_seq(), nodes(&db), edges(&db)));
        if i == 3 {
            db.snapshot_with(ARCHIVING).unwrap();
        }
    }
    let live = (nodes(&db), edges(&db));
    assert_eq!(live.0.len(), 9);
    drop(db);
    let replayed = GraphDb::open(&dir).unwrap();
    assert_eq!(
        (nodes(&replayed), edges(&replayed)),
        live,
        "reopened (WAL replay)"
    );
    drop(replayed);
    let mut snap = GraphDb::open(&dir).unwrap();
    snap.snapshot_with(ARCHIVING).unwrap();
    drop(snap);
    let snapped = GraphDb::open(&dir).unwrap();
    assert_eq!(
        (nodes(&snapped), edges(&snapped)),
        live,
        "snapshotted and reopened"
    );
    drop(snapped);
    for (seq, n, e) in &at {
        let old = GraphDb::open_at(&dir, seq - 1).unwrap();
        assert_eq!((&nodes(&old), &edges(&old)), (n, e), "open_at {}", seq - 1);
    }
    // The same statements, one commit each, against a never-closed store and
    // in one transaction: the keys differ, the shape does not.
    let shape = {
        let db = GraphDb::open(&dir).unwrap();
        rows(&read(&db, SHAPE))
    };
    let shared = SharedDb::open(&tmp("axes-tx")).unwrap();
    let mut tx = shared.begin_transaction();
    for q in SCRIPT {
        tx.execute(q, &p()).unwrap_or_else(|e| panic!("{q}: {e}"));
    }
    tx.commit().unwrap();
    assert_eq!(
        rows(&shared.reader().query(SHAPE, &p()).unwrap()),
        shape,
        "the same statements in one transaction"
    );
    // A reader snapshot of the live writer sees each commit.
    let shared = SharedDb::open(&tmp("axes-reader")).unwrap();
    for q in SCRIPT {
        shared.write().query_write(q, &p()).unwrap();
        assert_eq!(
            rows(&shared.reader().query(NODES, &p()).unwrap()),
            rows(&shared.read().query(NODES, &p()).unwrap()),
            "reader after {q}"
        );
    }
    assert_eq!(rows(&shared.reader().query(SHAPE, &p()).unwrap()), shape);
}

#[test]
fn a_transaction_reads_its_own_generated_nodes_and_rollback_leaves_none() {
    let db = SharedDb::open(&tmp("tx")).unwrap();
    let mut tx = db.begin_transaction();
    tx.execute("CREATE (), (:A {x: 1})", &p()).unwrap();
    let rs = tx.execute("MATCH (n) RETURN count(n) AS c", &p()).unwrap();
    assert_eq!(rs.get(0, "c"), Some(&Value::Int(2)));
    let rs = tx
        .execute("MERGE (n:A {x: 1}) RETURN key(n) AS k", &p())
        .unwrap();
    let key = text(rs.get(0, "k"));
    assert!(is_generated(&key));
    assert_eq!(
        db.read()
            .query("MATCH (n) RETURN count(n) AS c", &p())
            .unwrap()
            .get(0, "c"),
        Some(&Value::Int(0))
    );
    tx.commit().unwrap();
    assert!(
        db.read().node_info(&key).is_some(),
        "the key the statement saw is the key committed"
    );
    assert_eq!(
        db.read()
            .query("MATCH (n) RETURN count(n) AS c", &p())
            .unwrap()
            .get(0, "c"),
        Some(&Value::Int(2))
    );
    let mut tx = db.begin_transaction();
    tx.execute("CREATE (), ()", &p()).unwrap();
    tx.rollback();
    assert_eq!(
        db.read()
            .query("MATCH (n) RETURN count(n) AS c", &p())
            .unwrap()
            .get(0, "c"),
        Some(&Value::Int(2))
    );
}

#[test]
fn a_backup_restore_carries_generated_keys_and_unlabelled_nodes() {
    let dir = tmp("backup-src");
    let dest = tmp("backup-dst");
    let mut db = GraphDb::open(&dir).unwrap();
    for q in SCRIPT {
        run(&mut db, q);
    }
    let live = (nodes(&db), edges(&db));
    db.backup_to(&dest).unwrap();
    drop(db);
    let restored = GraphDb::open(&dest).unwrap();
    assert_eq!((nodes(&restored), edges(&restored)), live);
    drop(restored);
    let seeded = tmp("backup-seeded");
    mushroomdb::restore::restore_if_empty(&seeded, &dest).unwrap();
    let seeded = GraphDb::open(&seeded).unwrap();
    assert_eq!((nodes(&seeded), edges(&seeded)), live);
}

// ── Roles: default deny for unlabelled nodes ────────────────────────────────

#[test]
fn a_scoped_role_neither_sees_nor_creates_an_unlabelled_node() {
    let mut db = GraphDb::open(&tmp("role")).unwrap();
    db.apply_schema(&Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![],
        roles: vec![RoleDef {
            name: "writer".into(),
            keys: vec![],
            labels: vec!["P".into()],
            visible_where: None,
            namespaces: None,
            write: Some(WriteScope {
                create_labels: vec!["P".into()],
                update_labels: vec!["P".into()],
                delete_labels: vec![],
                create_edge_types: vec![],
                delete_edge_types: vec![],
                update_edge_types: vec![],
            }),
        }],
    })
    .unwrap();
    run(&mut db, "CREATE (), (:P {name: 'p'})");
    let mask = db.mask_for_role("writer").unwrap();
    let rs = db
        .query_masked("MATCH (n) RETURN count(n) AS c", &p(), &mask)
        .unwrap();
    assert_eq!(
        rs.get(0, "c"),
        Some(&Value::Int(1)),
        "the unlabelled node is hidden"
    );
    match db.query_write_authz("writer", "CREATE ()", &p()) {
        Err(GraphError::RoleWriteDenied { .. }) => {}
        other => panic!("{other:?}"),
    }
    db.query_write_authz("writer", "CREATE (:P {name: 'q'})", &p())
        .expect("a labelled node without an id is created under the role's label");
    assert_eq!(count(&db, "MATCH (n) RETURN count(n) AS c"), 3);
}

/// Review C1: a MERGE pattern naming a hidden node's `id` beside other
/// properties answers as it does when the key is absent (or as the key MERGE
/// of that key does), so a role learns nothing about the hidden node.
#[test]
fn a_pattern_merge_never_reveals_a_hidden_node() {
    fn scoped(name: &str, secret: bool) -> GraphDb {
        let mut db = GraphDb::open(&tmp(name)).unwrap();
        db.apply_schema(&Schema {
            fulltext: vec![],
            indexes: vec![],
            rules: vec![],
            views: vec![],
            roles: vec![RoleDef {
                name: "writer".into(),
                keys: vec![],
                labels: vec!["P".into()],
                visible_where: None,
                namespaces: None,
                write: Some(WriteScope {
                    create_labels: vec!["P".into()],
                    update_labels: vec!["P".into()],
                    delete_labels: vec![],
                    create_edge_types: vec![],
                    delete_edge_types: vec![],
                    update_edge_types: vec![],
                }),
            }],
        })
        .unwrap();
        if secret {
            run(&mut db, "CREATE (:Q {id: 'secret', name: 'x'})");
        }
        db
    }
    let answer = |db: &mut GraphDb, q: &str| match db.query_write_authz("writer", q, &p()) {
        Ok(rs) => format!("ok {:?}", rows(&rs)),
        Err(e) => format!("err {e:?}"),
    };
    for (i, (probe, reference)) in [
        (
            "MERGE (n {id: 'secret', name: 'x'}) RETURN n.name",
            "MERGE (n {id: 'secret', name: 'x'}) RETURN n.name",
        ),
        (
            "MERGE (n:Q {id: 'secret', name: 'x'})",
            "MERGE (n:Q {id: 'secret', name: 'x'})",
        ),
        ("MERGE (n {name: 'x'})", "MERGE (n {name: 'x'})"),
    ]
    .into_iter()
    .enumerate()
    {
        let hidden = answer(&mut scoped(&format!("c1-h{i}"), true), probe);
        let absent = answer(&mut scoped(&format!("c1-a{i}"), false), reference);
        assert_eq!(hidden, absent, "{probe}");
        assert!(hidden.contains("RoleWriteDenied"), "{probe}: {hidden}");
    }
    // In the role's own label, the pattern answers as the key MERGE of the
    // same hidden key does: refused, never "exists".
    let mut db = scoped("c1-p", true);
    let pattern = answer(&mut db, "MERGE (n:P {id: 'secret', name: 'zz'})");
    let key = answer(&mut db, "MERGE (n:P {id: 'secret'})");
    assert_eq!(pattern, key);
    assert!(!pattern.contains("exists"), "{pattern}");
    assert_eq!(
        db.node_info("secret").unwrap().label,
        "Q",
        "nothing written"
    );
}

/// In a transaction, a single-property MERGE that creates its node under the
/// value's key guards the commit with that key: two transactions that both
/// create it conflict instead of making two nodes. One that matched guards
/// the node it matched.
#[test]
fn concurrent_single_property_merges_conflict_on_the_value_key() {
    let db = SharedDb::open(&tmp("tx-prop")).unwrap();
    let mut a = db.begin_transaction();
    let mut b = db.begin_transaction();
    a.execute("MERGE (:P {name: 'x'})", &p()).unwrap();
    b.execute("MERGE (:P {name: 'x'})", &p()).unwrap();
    a.commit().unwrap();
    assert!(b.commit().is_err(), "the second create of `x` conflicts");
    assert_eq!(
        db.read()
            .query("MATCH (n:P {name: 'x'}) RETURN count(n) AS c", &p())
            .unwrap()
            .get(0, "c"),
        Some(&Value::Int(1))
    );
    let mut c = db.begin_transaction();
    let mut d = db.begin_transaction();
    c.execute("MERGE (n:P {name: 'x'}) ON MATCH SET n.c = 1", &p())
        .unwrap();
    d.execute("MATCH (n:P {name: 'x'}) SET n.d = 1", &p())
        .unwrap();
    d.commit().unwrap();
    assert!(
        c.commit().is_err(),
        "the matched node changed under the MERGE"
    );
}
