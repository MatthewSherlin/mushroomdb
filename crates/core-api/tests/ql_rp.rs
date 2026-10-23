//! QL-RP: relationships carry properties written from Cypher.
//!
//! `CREATE` and `MERGE` take a property map on the relationship, and `SET
//! r.p`, `SET r += map`, `SET r = map` and `REMOVE r.p` write them. They are
//! stored as the edge's properties (the map rules already write weights into),
//! carried by two V11 WAL records (`SetEdgePropId`, `RemoveEdgePropId`), so
//! they survive a reopen, a snapshot, `open_at` and a reader snapshot, and run
//! inside a transaction with the same results.
//!
//! One property map per (type, start, end): a second `CREATE` of the pair
//! without a map keeps the multiplicity behaviour, one with a map is refused,
//! and `MERGE` matches the pair and then its map. `count` is reserved, and a
//! rule-owned edge type takes no user property write.

use mushroomdb::{
    schema::Schema, Direction, GraphError, Predicate, RealFs, RoleDef, RuleDef, SharedDb,
    SnapshotOptions, Value, WriteScope,
};
use std::collections::BTreeMap;
use std::path::PathBuf;

type GraphDb = mushroomdb::GraphDb<RealFs>;

const ARCHIVING: SnapshotOptions = SnapshotOptions {
    keep_wal: false,
    archive_wal: true,
};
type Rows = Vec<Vec<Option<Value>>>;

const EXISTS: &str = "a relationship between these nodes with this type already exists; \
                      use MERGE … SET to change its properties";

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "graphdb-qlrp-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn s(v: &str) -> Value {
    Value::Str(v.into())
}

fn p() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

fn params(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

fn map(pairs: &[(&str, Value)]) -> Value {
    Value::Map(
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
    )
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

fn run(db: &mut GraphDb, q: &str) -> mushroomdb::ResultSet {
    db.query_write(q, &p())
        .unwrap_or_else(|e| panic!("{q}: {e}"))
}

fn refused(db: &mut GraphDb, q: &str, pr: &BTreeMap<String, Value>) -> String {
    match db.query_write(q, pr) {
        Err(e) => e.to_string(),
        Ok(rs) => panic!("{q}: expected a refusal, got {rs:?}"),
    }
}

const EDGES: &str = "MATCH (a)-[r]->(b) RETURN key(a) AS a, type(r) AS t, key(b) AS b, \
                     properties(r) AS p ORDER BY a, t, b";

fn edges(db: &GraphDb) -> Rows {
    rows(&db.query(EDGES, &p()).unwrap())
}

fn prop(db: &GraphDb, t: &str, a: &str, b: &str, f: &str) -> Option<Value> {
    db.get_edge_prop(t, a, b, f)
}

fn two(db: &mut GraphDb) {
    run(db, "CREATE (:P {id: 'a'})");
    run(db, "CREATE (:P {id: 'b'})");
}

// ── CREATE ──────────────────────────────────────────────────────────────────

#[test]
fn create_stores_a_literal_map_and_counts_it() {
    let mut db = GraphDb::open(&tmp("create")).unwrap();
    let rs = run(
        &mut db,
        "CREATE (a:P {id: 'a'})-[:T {w: 1, since: 'x'}]->(b:P {id: 'b'})",
    );
    assert_eq!(count(&rs, "created"), 2);
    assert_eq!(count(&rs, "relationships_created"), 1);
    // A created relationship's map is part of its creation, as a created
    // node's properties are: not counted in `properties_set`.
    assert_eq!(count(&rs, "properties_set"), 0);
    assert_eq!(prop(&db, "T", "a", "b", "w"), Some(Value::Int(1)));
    assert_eq!(prop(&db, "T", "a", "b", "since"), Some(s("x")));
    let rs = db
        .query("MATCH (:P {id: 'a'})-[r:T]->() RETURN r.w AS w", &p())
        .unwrap();
    assert_eq!(rs.get(0, "w"), Some(&Value::Int(1)));
    // The other direction is written the same way.
    run(
        &mut db,
        "CREATE (a:P {id: 'c'})<-[r:T {w: 2}]-(b:P {id: 'd'}) RETURN r.w AS w",
    );
    assert_eq!(prop(&db, "T", "d", "c", "w"), Some(Value::Int(2)));
}

#[test]
fn create_takes_parameters_and_expressions() {
    let mut db = GraphDb::open(&tmp("create-params")).unwrap();
    two(&mut db);
    run(&mut db, "CREATE (:P {id: 'c'})");
    db.query_write(
        "MATCH (a:P {id: 'a'}), (b:P {id: 'b'}) CREATE (a)-[:T {w: $w * 2, from: a.id}]->(b)",
        &params(&[("w", Value::Int(21))]),
    )
    .unwrap();
    assert_eq!(prop(&db, "T", "a", "b", "w"), Some(Value::Int(42)));
    assert_eq!(prop(&db, "T", "a", "b", "from"), Some(s("a")));
    let rows_param = Value::List(vec![
        map(&[("a", s("a")), ("b", s("c")), ("w", Value::Float(0.5))]),
        map(&[("a", s("b")), ("b", s("c")), ("w", Value::Null)]),
    ]);
    let rs = db
        .query_write(
            "UNWIND $rows AS row MATCH (a:P {id: row.a}), (b:P {id: row.b}) \
             CREATE (a)-[:K {w: row.w, n: 1}]->(b)",
            &params(&[("rows", rows_param)]),
        )
        .unwrap();
    assert_eq!(count(&rs, "relationships_created"), 2);
    assert_eq!(prop(&db, "K", "a", "c", "w"), Some(Value::Float(0.5)));
    assert_eq!(
        prop(&db, "K", "b", "c", "w"),
        None,
        "a null value is not stored"
    );
    assert_eq!(prop(&db, "K", "b", "c", "n"), Some(Value::Int(1)));
    // `-[:T $props]->`: the whole map is the parameter.
    db.query_write(
        "CREATE (:P {id: 'x'})-[:T $props]->(:P {id: 'y'})",
        &params(&[("props", map(&[("w", Value::Int(7)), ("tag", s("t"))]))]),
    )
    .unwrap();
    assert_eq!(prop(&db, "T", "x", "y", "w"), Some(Value::Int(7)));
    assert_eq!(prop(&db, "T", "x", "y", "tag"), Some(s("t")));
}

#[test]
fn create_with_a_map_on_an_existing_pair_is_refused_and_without_one_is_unchanged() {
    let mut db = GraphDb::open(&tmp("create-existing")).unwrap();
    two(&mut db);
    run(
        &mut db,
        "MATCH (a:P {id: 'a'}), (b:P {id: 'b'}) CREATE (a)-[:T {w: 1}]->(b)",
    );
    let before = db.commit_seq();
    let e = refused(
        &mut db,
        "MATCH (a:P {id: 'a'}), (b:P {id: 'b'}) CREATE (a)-[:T {w: 2}]->(b)",
        &p(),
    );
    assert!(e.contains(EXISTS), "{e}");
    // Inside one statement, too: the second CREATE sees the first.
    let e = refused(
        &mut db,
        "CREATE (x:P {id: 'x'})-[:T {w: 1}]->(y:P {id: 'y'}) CREATE (x)-[:T {w: 2}]->(y)",
        &p(),
    );
    assert!(e.contains(EXISTS), "{e}");
    assert_eq!(db.commit_seq(), before, "nothing written");
    // Without a map: today's behaviour (no second edge, no count on a store
    // that did not opt in), and the properties stay.
    run(
        &mut db,
        "MATCH (a:P {id: 'a'}), (b:P {id: 'b'}) CREATE (a)-[:T]->(b)",
    );
    assert_eq!(db.edge_count(), 1);
    assert_eq!(prop(&db, "T", "a", "b", "w"), Some(Value::Int(1)));
}

#[test]
fn a_duplicate_create_without_a_map_counts_and_keeps_the_properties() {
    let dir = tmp("multiplicity");
    let mut db = GraphDb::open(&dir).unwrap();
    db.enable_multiplicity().unwrap();
    two(&mut db);
    run(
        &mut db,
        "MATCH (a:P {id: 'a'}), (b:P {id: 'b'}) CREATE (a)-[:T {w: 1}]->(b)",
    );
    run(
        &mut db,
        "MATCH (a:P {id: 'a'}), (b:P {id: 'b'}) CREATE (a)-[:T]->(b)",
    );
    assert_eq!(prop(&db, "T", "a", "b", "count"), Some(Value::Int(2)));
    assert_eq!(prop(&db, "T", "a", "b", "w"), Some(Value::Int(1)));
    // `SET r = {…}` replaces the user's map and keeps the count.
    run(&mut db, "MATCH ()-[r:T]->() SET r = {z: 3}");
    assert_eq!(prop(&db, "T", "a", "b", "w"), None);
    assert_eq!(prop(&db, "T", "a", "b", "z"), Some(Value::Int(3)));
    assert_eq!(prop(&db, "T", "a", "b", "count"), Some(Value::Int(2)));
    // And after a snapshot (the count in the base, a write in the overlay).
    db.snapshot().unwrap();
    run(&mut db, "MATCH ()-[r:T]->() SET r.z = 4");
    db.snapshot().unwrap();
    drop(db);
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(prop(&db, "T", "a", "b", "z"), Some(Value::Int(4)));
    assert_eq!(prop(&db, "T", "a", "b", "count"), Some(Value::Int(2)));
}

// ── MERGE ───────────────────────────────────────────────────────────────────

#[test]
fn merge_matches_the_pair_and_then_its_map() {
    let mut db = GraphDb::open(&tmp("merge")).unwrap();
    let q = "MERGE (a:P {id: 'a'})-[r:T {w: 1}]->(b:P {id: 'b'}) \
             ON CREATE SET r.made = true ON MATCH SET r.hits = coalesce(r.hits, 0) + 1";
    let rs = run(&mut db, q);
    assert_eq!(count(&rs, "relationships_created"), 1);
    assert_eq!(prop(&db, "T", "a", "b", "w"), Some(Value::Int(1)));
    assert_eq!(prop(&db, "T", "a", "b", "made"), Some(Value::Bool(true)));
    let rs = run(&mut db, q);
    assert_eq!(count(&rs, "relationships_created"), 0);
    assert_eq!(prop(&db, "T", "a", "b", "hits"), Some(Value::Int(1)));
    run(&mut db, q);
    assert_eq!(prop(&db, "T", "a", "b", "hits"), Some(Value::Int(2)));
    // A MERGE without a map matches the pair whatever it carries.
    run(
        &mut db,
        "MERGE (a:P {id: 'a'})-[r:T]->(b:P {id: 'b'}) ON MATCH SET r.plain = 1",
    );
    assert_eq!(prop(&db, "T", "a", "b", "plain"), Some(Value::Int(1)));
    // A map that differs is refused, not a second relationship.
    let before = db.commit_seq();
    let e = refused(
        &mut db,
        "MERGE (a:P {id: 'a'})-[r:T {w: 2}]->(b:P {id: 'b'})",
        &p(),
    );
    assert!(e.contains(EXISTS), "{e}");
    let e = refused(
        &mut db,
        "MERGE (a:P {id: 'a'})-[r:T {w: 1, other: 5}]->(b:P {id: 'b'})",
        &p(),
    );
    assert!(e.contains(EXISTS), "{e}");
    assert_eq!(db.commit_seq(), before);
    assert_eq!(db.edge_count(), 1);
    // Parameters in the map, and a per-row MERGE.
    db.query_write(
        "UNWIND $ws AS w MERGE (a:P {id: 'c'})-[r:T {w: w}]->(b:P {id: 'd'})",
        &params(&[("ws", Value::List(vec![Value::Int(5), Value::Int(5)]))]),
    )
    .unwrap();
    assert_eq!(prop(&db, "T", "c", "d", "w"), Some(Value::Int(5)));
}

#[test]
fn merge_on_bound_nodes_with_a_map() {
    let mut db = GraphDb::open(&tmp("merge-bound")).unwrap();
    two(&mut db);
    let q = "MATCH (a:P {id: 'a'}), (b:P {id: 'b'}) MERGE (a)-[r:K {since: 2020}]->(b) \
             RETURN r.since AS s";
    let rs = run(&mut db, q);
    assert_eq!(rs.get(0, "s"), Some(&Value::Int(2020)));
    let rs = run(&mut db, q);
    assert_eq!(rs.get(0, "s"), Some(&Value::Int(2020)));
    assert_eq!(db.edge_count(), 1);
}

// ── SET / REMOVE ────────────────────────────────────────────────────────────

fn seeded(name: &str) -> GraphDb {
    let mut db = GraphDb::open(&tmp(name)).unwrap();
    run(
        &mut db,
        "CREATE (a:P {id: 'a'})-[:T {w: 1, keep: 'k'}]->(b:P {id: 'b'})",
    );
    run(
        &mut db,
        "MATCH (b:P {id: 'b'}) CREATE (b)-[:T {w: 10}]->(c:P {id: 'c'})",
    );
    db
}

#[test]
fn set_and_remove_write_relationship_properties() {
    let mut db = seeded("set");
    let rs = run(
        &mut db,
        "MATCH (:P {id: 'a'})-[r:T]->() SET r.w = r.w + 1, r.x = 'y'",
    );
    assert_eq!(count(&rs, "properties_set"), 2);
    assert_eq!(prop(&db, "T", "a", "b", "w"), Some(Value::Int(2)));
    assert_eq!(prop(&db, "T", "a", "b", "x"), Some(s("y")));
    // Every direction finds the same edge.
    run(
        &mut db,
        "MATCH (:P {id: 'b'})<-[r:T]-(:P {id: 'a'}) SET r.left = 1",
    );
    run(
        &mut db,
        "MATCH (:P {id: 'a'})-[r:T]-(:P {id: 'b'}) SET r.undirected = 1",
    );
    assert_eq!(prop(&db, "T", "a", "b", "left"), Some(Value::Int(1)));
    assert_eq!(prop(&db, "T", "a", "b", "undirected"), Some(Value::Int(1)));
    assert_eq!(prop(&db, "T", "b", "a", "undirected"), None);
    // `+=`: set the listed keys, a null removes.
    let rs = db
        .query_write(
            "MATCH (:P {id: 'a'})-[r:T]->() SET r += $m",
            &params(&[("m", map(&[("w", Value::Int(5)), ("x", Value::Null)]))]),
        )
        .unwrap();
    assert_eq!(count(&rs, "properties_set"), 2);
    assert_eq!(prop(&db, "T", "a", "b", "w"), Some(Value::Int(5)));
    assert_eq!(prop(&db, "T", "a", "b", "x"), None);
    assert_eq!(prop(&db, "T", "a", "b", "keep"), Some(s("k")));
    // `=`: exactly the map.
    run(&mut db, "MATCH (:P {id: 'a'})-[r:T]->() SET r = {only: 1}");
    let rs = db
        .query(
            "MATCH (:P {id: 'a'})-[r:T]->() RETURN properties(r) AS p",
            &p(),
        )
        .unwrap();
    assert_eq!(rs.get(0, "p"), Some(&map(&[("only", Value::Int(1))])));
    // REMOVE; an absent property is a no-op and is not counted.
    let rs = run(
        &mut db,
        "MATCH (:P {id: 'a'})-[r:T]->() REMOVE r.only, r.never",
    );
    assert_eq!(count(&rs, "properties_set"), 1);
    assert_eq!(prop(&db, "T", "a", "b", "only"), None);
    // The other edge was never touched.
    assert_eq!(prop(&db, "T", "b", "c", "w"), Some(Value::Int(10)));
    // RETURN reads the written state.
    let rs = run(
        &mut db,
        "MATCH (:P {id: 'b'})-[r:T]->() SET r.w = 11 RETURN r.w AS w",
    );
    assert_eq!(rs.get(0, "w"), Some(&Value::Int(11)));
}

#[test]
fn rows_over_one_relationship_read_each_others_writes() {
    let mut db = seeded("rows");
    run(
        &mut db,
        "UNWIND [1, 2, 3] AS x MATCH (:P {id: 'a'})-[r:T]->(:P {id: 'b'}) SET r.w = r.w + 1",
    );
    assert_eq!(prop(&db, "T", "a", "b", "w"), Some(Value::Int(4)));
    run(
        &mut db,
        "MATCH (:P {id: 'a'})-[r:T]->() SET r.v = 10, r.u = r.v + 1",
    );
    assert_eq!(prop(&db, "T", "a", "b", "u"), Some(Value::Int(11)));
}

#[test]
fn a_null_set_value_is_refused_like_a_node_set() {
    let mut db = seeded("null");
    let e = refused(&mut db, "MATCH ()-[r:T]->() SET r.w = null", &p());
    assert!(e.contains("null"), "{e}");
}

// ── Reserved and owned ──────────────────────────────────────────────────────

#[test]
fn count_is_reserved_on_every_write_form_and_still_reads() {
    let mut db = GraphDb::open(&tmp("count")).unwrap();
    db.enable_multiplicity().unwrap();
    two(&mut db);
    run(
        &mut db,
        "MATCH (a:P {id: 'a'}), (b:P {id: 'b'}) CREATE (a)-[:T]->(b)",
    );
    run(
        &mut db,
        "MATCH (a:P {id: 'a'}), (b:P {id: 'b'}) CREATE (a)-[:T]->(b)",
    );
    let before = db.commit_seq();
    for q in [
        "MATCH ()-[r:T]->() SET r.count = 9",
        "MATCH ()-[r:T]->() SET r += {count: 9}",
        "MATCH ()-[r:T]->() SET r = {count: 9}",
        "MATCH ()-[r:T]->() REMOVE r.count",
        "CREATE (:P {id: 'x'})-[:T {count: 1}]->(:P {id: 'y'})",
        "MERGE (:P {id: 'x'})-[:T {count: 1}]->(:P {id: 'y'})",
        "MERGE (a:P {id: 'a'})-[r:T]->(b:P {id: 'b'}) ON MATCH SET r.count = 1",
    ] {
        let e = refused(&mut db, q, &p());
        assert!(e.contains("count") && e.contains("reserved"), "{q}: {e}");
    }
    assert_eq!(db.commit_seq(), before);
    let rs = db
        .query("MATCH ()-[r:T]->() RETURN r.count AS c", &p())
        .unwrap();
    assert_eq!(rs.get(0, "c"), Some(&Value::Int(2)));
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

#[test]
fn a_rule_owned_edge_type_takes_no_user_property_write() {
    let mut db = GraphDb::open(&tmp("rule")).unwrap();
    db.create_rule(same_region_rule()).unwrap();
    run(&mut db, "CREATE (:P {id: 'a', region: 'eu'})");
    run(&mut db, "CREATE (:P {id: 'b', region: 'eu'})");
    assert!(db.edge_count() > 0);
    let before = db.commit_seq();
    for q in [
        "MATCH ()-[r:SAME]->() SET r.w = 1",
        "MATCH ()-[r:SAME]->() SET r += {w: 1}",
        "MATCH ()-[r:SAME]->() REMOVE r.w",
        "MATCH (a:P {id: 'a'}), (b:P {id: 'b'}) MERGE (a)-[r:SAME]->(b) ON MATCH SET r.w = 1",
        "CREATE (:P {id: 'x'})-[:SAME {w: 1}]->(:P {id: 'y'})",
    ] {
        let e = refused(&mut db, q, &p());
        assert!(e.contains("rule"), "{q}: {e}");
    }
    assert_eq!(db.commit_seq(), before);
}

// ── Durability: live, replayed, snapshotted, at a commit, a reader ───────────

const SCRIPT: [&str; 7] = [
    "CREATE (a:P {id: 'a'})-[:T {w: 1, keep: 'k'}]->(b:P {id: 'b'})",
    "MATCH (b:P {id: 'b'}) CREATE (b)-[:T {w: 10}]->(c:P {id: 'c'})",
    "MATCH (:P {id: 'a'})-[r:T]->() SET r.w = 2",
    "MATCH (:P {id: 'b'})-[r:T]->() SET r += {x: 1, w: null}",
    "MATCH (:P {id: 'a'})-[r:T]->() REMOVE r.keep",
    "MERGE (a:P {id: 'c'})-[r:K {n: 1}]->(b:P {id: 'a'})",
    "MATCH (:P {id: 'c'})-[r:K]->() SET r = {n: 2, m: 3}",
];

#[test]
fn edge_properties_are_identical_live_replayed_snapshotted_and_at() {
    let dir = tmp("axes");
    let mut db = GraphDb::open(&dir).unwrap();
    let mut at: Vec<(u64, Rows)> = Vec::new();
    for (i, q) in SCRIPT.iter().enumerate() {
        run(&mut db, q);
        at.push((db.commit_seq(), edges(&db)));
        // A snapshot in the middle: the base holds the first half, and the
        // archived WAL keeps every commit for `open_at`.
        if i == 3 {
            db.snapshot_with(ARCHIVING).unwrap();
        }
    }
    let live = edges(&db);
    assert!(!live.is_empty());
    drop(db);
    let replayed = GraphDb::open(&dir).unwrap();
    assert_eq!(
        edges(&replayed),
        live,
        "reopened (WAL replay over the snapshot)"
    );
    drop(replayed);
    let mut snap = GraphDb::open(&dir).unwrap();
    snap.snapshot_with(ARCHIVING).unwrap();
    drop(snap);
    let snapped = GraphDb::open(&dir).unwrap();
    assert_eq!(edges(&snapped), live, "snapshotted and reopened");
    drop(snapped);
    for (seq, want) in &at {
        let old = GraphDb::open_at(&dir, seq - 1).unwrap();
        assert_eq!(&edges(&old), want, "open_at {}", seq - 1);
    }
    // One statement per commit vs the whole script in a transaction.
    let shared = SharedDb::open(&tmp("axes-tx")).unwrap();
    let mut tx = shared.begin_transaction();
    for q in SCRIPT {
        tx.execute(q, &p()).unwrap_or_else(|e| panic!("{q}: {e}"));
    }
    tx.commit().unwrap();
    assert_eq!(
        rows(&shared.reader().query(EDGES, &p()).unwrap()),
        live,
        "the same statements in one transaction"
    );
    // A reader snapshot of the live writer sees each commit.
    let shared = SharedDb::open(&tmp("axes-reader")).unwrap();
    for q in SCRIPT {
        shared.write().query_write(q, &p()).unwrap();
        assert_eq!(
            rows(&shared.reader().query(EDGES, &p()).unwrap()),
            rows(&shared.read().query(EDGES, &p()).unwrap()),
            "reader after {q}"
        );
    }
    assert_eq!(rows(&shared.reader().query(EDGES, &p()).unwrap()), live);
}

#[test]
fn a_backup_restore_carries_relationship_properties() {
    let dir = tmp("backup-src");
    let dest = tmp("backup-dst");
    let mut db = GraphDb::open(&dir).unwrap();
    for q in SCRIPT {
        run(&mut db, q);
    }
    let live = edges(&db);
    db.backup_to(&dest).unwrap();
    drop(db);
    assert_eq!(edges(&GraphDb::open(&dest).unwrap()), live);
    let seeded = tmp("backup-seeded");
    mushroomdb::restore::restore_if_empty(&seeded, &dest).unwrap();
    assert_eq!(edges(&GraphDb::open(&seeded).unwrap()), live);
}

#[test]
fn the_first_edge_property_write_moves_the_store_to_v11() {
    let dir = tmp("v11");
    let mut db = GraphDb::open(&dir).unwrap();
    run(&mut db, "CREATE (:P {id: 'a'})-[:T]->(:P {id: 'b'})");
    assert!(
        !db.is_v11(),
        "an edge with no properties writes nothing V11"
    );
    run(&mut db, "MATCH ()-[r:T]->() SET r.w = 1");
    assert!(db.is_v11());
    assert_eq!(mushroomdb::snapshot_version_at(&dir).unwrap(), Some(11));
    drop(db);
    let db = GraphDb::open(&dir).unwrap();
    assert!(db.is_v11());
    assert_eq!(prop(&db, "T", "a", "b", "w"), Some(Value::Int(1)));
}

// ── Transactions ────────────────────────────────────────────────────────────

#[test]
fn a_transaction_reads_its_own_relationship_writes_and_rollback_leaves_none() {
    let db = SharedDb::open(&tmp("tx")).unwrap();
    db.write()
        .query_write("CREATE (a:P {id: 'a'})-[:T {w: 1}]->(b:P {id: 'b'})", &p())
        .unwrap();
    let mut tx = db.begin_transaction();
    tx.execute("MATCH ()-[r:T]->() SET r.w = r.w + 1, r.x = 1", &p())
        .unwrap();
    tx.execute("MATCH ()-[r:T]->() REMOVE r.x", &p()).unwrap();
    let rs = tx
        .execute("MATCH ()-[r:T]->() RETURN r.w AS w, r.x AS x", &p())
        .unwrap();
    assert_eq!(rs.get(0, "w"), Some(&Value::Int(2)));
    assert_eq!(rs.get(0, "x"), None);
    // Not visible outside before COMMIT.
    assert_eq!(
        db.read().get_edge_prop("T", "a", "b", "w"),
        Some(Value::Int(1))
    );
    tx.commit().unwrap();
    assert_eq!(
        db.read().get_edge_prop("T", "a", "b", "w"),
        Some(Value::Int(2))
    );
    let mut tx = db.begin_transaction();
    tx.execute("MATCH ()-[r:T]->() SET r.w = 99", &p()).unwrap();
    tx.rollback();
    assert_eq!(
        db.read().get_edge_prop("T", "a", "b", "w"),
        Some(Value::Int(2))
    );
    // A refusal inside a transaction is the statement's.
    let mut tx = db.begin_transaction();
    let e = tx
        .execute(
            "MATCH (a:P {id: 'a'}), (b:P {id: 'b'}) CREATE (a)-[:T {w: 3}]->(b)",
            &p(),
        )
        .unwrap_err()
        .to_string();
    assert!(e.contains(EXISTS), "{e}");
}

// ── Scopes ──────────────────────────────────────────────────────────────────

fn role(update_edge_types: &[&str]) -> RoleDef {
    RoleDef {
        name: "writer".into(),
        keys: vec![],
        labels: vec!["P".into()],
        visible_where: None,
        namespaces: None,
        write: Some(WriteScope {
            create_labels: vec!["P".into()],
            update_labels: vec!["P".into()],
            delete_labels: vec![],
            create_edge_types: vec!["T".into()],
            delete_edge_types: vec![],
            update_edge_types: update_edge_types.iter().map(|t| t.to_string()).collect(),
        }),
    }
}

fn scoped(name: &str, update_edge_types: &[&str]) -> GraphDb {
    let mut db = GraphDb::open(&tmp(name)).unwrap();
    db.apply_schema(&Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![],
        roles: vec![role(update_edge_types)],
    })
    .unwrap();
    run(
        &mut db,
        "CREATE (a:P {id: 'a'})-[:T {w: 1}]->(b:P {id: 'b'})",
    );
    db
}

#[test]
fn writing_a_relationship_property_needs_update_scope_on_the_edge_type() {
    let mut db = scoped("scope-none", &[]);
    match db.query_write_authz("writer", "MATCH ()-[r:T]->() SET r.w = 2", &p()) {
        Err(GraphError::RoleWriteDenied { reason }) => {
            assert!(reason.contains("update_edge_types"), "{reason}")
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(prop(&db, "T", "a", "b", "w"), Some(Value::Int(1)));
    // Creating an edge with its map needs only create scope, as a node's
    // properties need only create_labels.
    db.query_write_authz(
        "writer",
        "CREATE (:P {id: 'x'})-[:T {w: 5}]->(:P {id: 'y'})",
        &p(),
    )
    .unwrap();
    assert_eq!(prop(&db, "T", "x", "y", "w"), Some(Value::Int(5)));

    let mut db = scoped("scope-ok", &["T"]);
    db.query_write_authz("writer", "MATCH ()-[r:T]->() SET r.w = 2", &p())
        .unwrap();
    db.query_write_authz("writer", "MATCH ()-[r:T]->() REMOVE r.w", &p())
        .unwrap();
    assert_eq!(prop(&db, "T", "a", "b", "w"), None);
}

// ── Row 151: the map forms enforce the id≡key rule as `SET n.id` does ──────

#[test]
fn row_151_map_forms_and_set_id_give_the_same_answer() {
    let mut db = GraphDb::open(&tmp("row151")).unwrap();
    run(&mut db, "CREATE (:P {id: 'k'})");
    run(&mut db, "MERGE (:P {name: 'nk'})");
    let other = params(&[("m", map(&[("id", s("other"))]))]);
    // id-keyed: every form refuses a diverging id.
    for q in [
        "MATCH (n:P) WHERE key(n) = 'k' SET n.id = 'other'",
        "MATCH (n:P) WHERE key(n) = 'k' SET n += $m",
        "MATCH (n:P) WHERE key(n) = 'k' SET n = $m",
        "MERGE (n:P {id: 'k'}) ON MATCH SET n += $m",
        "MERGE (n:P {id: 'new'}) ON CREATE SET n += $m",
        "UNWIND [1] AS u MATCH (n:P) WHERE key(n) = 'k' SET n += $m",
    ] {
        let e = refused(&mut db, q, &other);
        assert!(e.contains("key property `id`"), "{q}: {e}");
    }
    assert_eq!(db.get_prop("k", "id"), Some(s("k")));
    assert!(!db.has_node("new"));
    // Name-keyed (no `id` equal to the key): `id` is an ordinary property for
    // every form alike.
    db.query_write("MATCH (n:P) WHERE key(n) = 'nk' SET n += $m", &other)
        .unwrap();
    assert_eq!(db.get_prop("nk", "id"), Some(s("other")));
    run(
        &mut db,
        "MATCH (n:P) WHERE key(n) = 'nk' SET n.id = 'again'",
    );
    assert_eq!(db.get_prop("nk", "id"), Some(s("again")));

    // The same answers inside a transaction.
    let shared = SharedDb::open(&tmp("row151-tx")).unwrap();
    shared
        .write()
        .query_write("CREATE (:P {id: 'k'})", &p())
        .unwrap();
    for q in [
        "MATCH (n:P) WHERE key(n) = 'k' SET n.id = 'other'",
        "MATCH (n:P) WHERE key(n) = 'k' SET n += $m",
        "MERGE (n:P {id: 'k'}) ON MATCH SET n = $m",
        "MERGE (n:P {id: 'new'}) ON CREATE SET n += $m",
    ] {
        let mut tx = shared.begin_transaction();
        let e = tx.execute(q, &other).unwrap_err().to_string();
        assert!(e.contains("key property `id`"), "{q}: {e}");
    }
    let mut tx = shared.begin_transaction();
    tx.execute("MERGE (n:P {name: 'nk'}) ON CREATE SET n += $m", &other)
        .unwrap();
    tx.commit().unwrap();
    assert_eq!(shared.read().get_prop("nk", "id"), Some(s("other")));
}

/// Review I1: re-`CREATE`ing a pair that already has its relationship binds
/// the existing edge, so a property written on it in the same statement is an
/// update, not part of a creation: a create-only role is refused, on a plain
/// store and on a multiplicity store. A role with update scope may, and a
/// genuinely new edge takes its properties under create scope alone.
#[test]
fn recreating_an_existing_pair_does_not_bypass_update_scope() {
    const RECREATE_SET: &str =
        "MATCH (a:P {id: 'a'}), (b:P {id: 'b'}) CREATE (a)-[r:T]->(b) SET r.w = 99";
    for multiplicity in [false, true] {
        let mut db = scoped(&format!("i1-create-only-{multiplicity}"), &[]);
        if multiplicity {
            db.enable_multiplicity().unwrap();
        }
        let before = db.commit_seq();
        match db.query_write_authz("writer", RECREATE_SET, &p()) {
            Err(GraphError::RoleWriteDenied { reason }) => {
                assert!(reason.contains("update_edge_types"), "{reason}")
            }
            other => panic!("multiplicity={multiplicity}: {other:?}"),
        }
        assert_eq!(db.commit_seq(), before, "nothing written");
        assert_eq!(prop(&db, "T", "a", "b", "w"), Some(Value::Int(1)));
        // A new pair with a SET in the same statement is part of its creation.
        run(&mut db, "CREATE (:P {id: 'c'})");
        db.query_write_authz(
            "writer",
            "MATCH (a:P {id: 'a'}), (c:P {id: 'c'}) CREATE (a)-[r:T]->(c) SET r.w = 5",
            &p(),
        )
        .unwrap();
        assert_eq!(prop(&db, "T", "a", "c", "w"), Some(Value::Int(5)));

        let mut db = scoped(&format!("i1-create-update-{multiplicity}"), &["T"]);
        if multiplicity {
            db.enable_multiplicity().unwrap();
        }
        db.query_write_authz("writer", RECREATE_SET, &p()).unwrap();
        assert_eq!(prop(&db, "T", "a", "b", "w"), Some(Value::Int(99)));
    }
}

fn weighted(name: &str, field: &str, weight: &str) -> RuleDef {
    RuleDef {
        name: name.into(),
        predicate: Predicate::FieldEqual {
            field: field.into(),
        },
        weight_prop: Some(weight.into()),
        ..same_region_rule()
    }
}

/// Ledger row 154: two weighted rules over one edge type. Rule `ra` owns the
/// edge (weight `wa`), a snapshot archives `wa`, then `ra` retracts and `rb`
/// claims the edge (weight `wb`). The retracted rule's archived weight must
/// not reappear live and then vanish at the next snapshot: the read is the
/// same live, after a snapshot and after a reopen.
#[test]
fn row_154_a_retracted_rules_weight_does_not_reappear_across_a_snapshot() {
    let dir = tmp("row154");
    let mut db = GraphDb::open(&dir).unwrap();
    db.create_rule(weighted("ra", "region", "wa")).unwrap();
    run(&mut db, "CREATE (:P {id: 'a', region: 'eu', team: 't'})");
    run(&mut db, "CREATE (:P {id: 'b', region: 'eu', team: 't'})");
    db.create_rule(weighted("rb", "team", "wb")).unwrap();
    let read = |db: &GraphDb| {
        (
            db.neighbors("a", "SAME", Direction::Out)
                .unwrap()
                .contains(&"b".to_string()),
            prop(db, "SAME", "a", "b", "wa"),
            prop(db, "SAME", "a", "b", "wb"),
        )
    };
    assert_eq!(read(&db).1, Some(Value::Float(1.0)), "ra owns the edge");
    db.snapshot().unwrap();
    run(&mut db, "MATCH (n:P {id: 'b'}) SET n.region = 'us'");
    let live = read(&db);
    assert!(live.0, "rb keeps the edge");
    assert_eq!(
        live.1, None,
        "ra's archived weight is gone with its ownership"
    );
    assert_eq!(live.2, Some(Value::Float(1.0)));
    db.snapshot().unwrap();
    assert_eq!(read(&db), live, "after a snapshot");
    drop(db);
    assert_eq!(read(&GraphDb::open(&dir).unwrap()), live, "after a reopen");
}
