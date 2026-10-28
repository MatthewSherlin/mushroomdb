//! QL-VL (defect-ledger row 132): a variable-length hop applies its
//! destination's label and inline properties the same way however the store
//! was built or read: live, one batch or many commits, WAL replay, snapshotted
//! and reopened, and with or without a property index on the destination's
//! `(label, prop)`.
//!
//! On v0.9.2 `MATCH (a {id: 'a'})-[*1..3]->(b:Person {active: true})` over
//! `a -> b(Person, active) -> c(Person, inactive) -> d(Robot, active)`
//! returned `b`, `c` and `d`.

use mushroomdb::{GraphDb, Value};
use std::collections::BTreeMap;

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "graphdb-qlvl-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

type Db = GraphDb<core_storage::fs::RealFs>;

/// `(label, key, active)`; `None` leaves the property off.
const NODES: [(&str, &str, Option<bool>); 7] = [
    ("Person", "a", Some(true)),
    ("Person", "b", Some(true)),
    ("Person", "c", Some(false)),
    ("Robot", "d", Some(true)),
    ("Person", "e", Some(true)),
    ("Person", "f", None),
    ("City", "g", None),
];

const EDGES: [(&str, &str, &str); 6] = [
    ("R", "a", "b"),
    ("R", "b", "c"),
    ("R", "c", "d"),
    ("R", "d", "e"),
    ("R", "a", "f"),
    ("LIVES", "e", "g"),
];

fn props(active: Option<bool>) -> Vec<(String, Value)> {
    active
        .map(|v| vec![("active".to_owned(), Value::Bool(v))])
        .unwrap_or_default()
}

fn build_one_batch(dir: &std::path::Path) -> Db {
    let mut db = GraphDb::open(dir).unwrap();
    let mut batch = db.batch();
    for (label, key, active) in NODES {
        batch.insert_node(label, key, props(active));
    }
    for (etype, src, dst) in EDGES {
        batch.insert_edge(etype, src, dst);
    }
    batch.commit().unwrap();
    db
}

fn build_many_commits(dir: &std::path::Path) -> Db {
    let mut db = GraphDb::open(dir).unwrap();
    for (label, key, active) in NODES {
        db.insert_node(label, key, props(active)).unwrap();
    }
    for (etype, src, dst) in EDGES {
        db.insert_edge(etype, src, dst).unwrap();
    }
    db
}

const QUERIES: [&str; 7] = [
    "MATCH (a {id: 'a'})-[*1..3]->(b:Person {active: true}) RETURN b ORDER BY b",
    "MATCH (a {id: 'a'})-[:R*]->(b:Person {active: true}) RETURN b ORDER BY b",
    "MATCH (a {id: 'a'})-[*1..4]->(b:Person {active: true})-[:LIVES]->(c) RETURN b, c",
    "MATCH (a {id: 'a'}) OPTIONAL MATCH (a)-[*1..2]->(b:Robot) RETURN a, b",
    "MATCH p = (a {id: 'e'})<-[*1..4]-(b:Person {active: false}) RETURN b, p.length AS n",
    "MATCH (a:Person {active: true})-[*1..2]->(b:Person {active: true}) \
     RETURN a, b ORDER BY a, b",
    "MATCH (a {id: 'g'}) OPTIONAL MATCH p = (a)-[*1..2]->(b) RETURN a, b, p",
];

fn answers(db: &Db) -> Vec<Vec<Vec<Option<Value>>>> {
    QUERIES
        .iter()
        .map(|q| {
            let rs = db
                .query(q, &BTreeMap::new())
                .unwrap_or_else(|e| panic!("{q}: {e}"));
            (0..rs.len()).map(|i| rs.row(i).to_vec()).collect()
        })
        .collect()
}

fn s(v: &str) -> Option<Value> {
    Some(Value::Str(v.into()))
}

fn expected() -> Vec<Vec<Vec<Option<Value>>>> {
    vec![
        vec![vec![s("b")]],
        vec![vec![s("b")], vec![s("e")]],
        vec![vec![s("e"), s("g")]],
        vec![vec![s("a"), None]],
        vec![vec![s("c"), Some(Value::Int(2))]],
        vec![vec![s("a"), s("b")]],
        vec![vec![s("g"), None, None]],
    ]
}

#[test]
fn the_destination_filter_holds_live_replayed_snapshotted_and_indexed() {
    let want = expected();
    let dir = tmp("shapes");
    let db = build_one_batch(&dir);
    assert_eq!(answers(&db), want, "live, one batch");
    drop(db);

    let replayed = GraphDb::open(&dir).unwrap();
    assert_eq!(answers(&replayed), want, "WAL replay");
    let mut snapshotting = replayed;
    snapshotting.snapshot().unwrap();
    assert_eq!(answers(&snapshotting), want, "snapshotting");
    drop(snapshotting);
    let mut reopened = GraphDb::open(&dir).unwrap();
    assert_eq!(answers(&reopened), want, "reopened from a snapshot");

    // An index on the destination's `(label, prop)` gives the scan's rows.
    reopened.enable_index("Person", "active").unwrap();
    assert_eq!(answers(&reopened), want, "indexed, live");
    drop(reopened);
    let indexed = GraphDb::open(&dir).unwrap();
    assert_eq!(answers(&indexed), want, "indexed, reopened");

    let many = build_many_commits(&tmp("many"));
    assert_eq!(answers(&many), want, "many commits");
}

#[test]
fn parameters_in_the_destination_map_work_through_the_api() {
    let db = build_one_batch(&tmp("params"));
    let rs = db
        .query_with_params(
            "MATCH (a {id: 'a'})-[*1..4]->(b {id: $id}) RETURN b",
            &[("id", Value::Str("e".into()))],
        )
        .unwrap();
    assert_eq!(rs.len(), 1);
    assert_eq!(rs.row(0).to_vec(), vec![s("e")]);
    let rs = db
        .query_with_params(
            "MATCH (a {id: 'a'})-[*1..4]->(b:Person {active: $on}) RETURN b",
            &[("on", Value::Bool(false))],
        )
        .unwrap();
    assert_eq!(rs.len(), 1);
    assert_eq!(rs.row(0).to_vec(), vec![s("c")]);
}

/// Released 0.9.x drove `MATCH … SET` and `MATCH … DETACH DELETE` from the
/// same unfiltered hop, so the write landed on every reachable node; the
/// DETACH DELETE below would have deleted `b`, `c`, `d` and `f`.
#[test]
fn a_write_lands_only_on_the_filtered_destination() {
    let mut db = build_one_batch(&tmp("write"));
    let none = BTreeMap::new();
    db.query_write(
        "MATCH (a {id: 'a'})-[*1..3]->(b:Robot) SET b.hit = true",
        &none,
    )
    .unwrap();
    let rs = db
        .query("MATCH (n) WHERE n.hit = true RETURN n", &none)
        .unwrap();
    assert_eq!(
        (0..rs.len())
            .map(|i| rs.row(i).to_vec())
            .collect::<Vec<_>>(),
        vec![vec![s("d")]]
    );
    db.query_write(
        "MATCH (a {id: 'a'})-[*1..3]->(b:Person {active: false}) DETACH DELETE b",
        &none,
    )
    .unwrap();
    let rs = db.query("MATCH (n) RETURN n ORDER BY n", &none).unwrap();
    let left: Vec<Vec<Option<Value>>> = (0..rs.len()).map(|i| rs.row(i).to_vec()).collect();
    assert_eq!(
        left,
        ["a", "b", "d", "e", "f", "g"]
            .iter()
            .map(|k| vec![s(k)])
            .collect::<Vec<_>>()
    );
}
