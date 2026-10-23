//! QL-03e differential probe: `elementId`, `startNode`, `endNode`, `nodes`,
//! `relationships` and `length` give the same answers live, after a reopen
//! (WAL replay), after a snapshot, and at an earlier commit.

use mushroomdb::{RealFs, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;

type GraphDb = mushroomdb::GraphDb<RealFs>;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-ql03e-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

const QUERIES: [&str; 3] = [
    "MATCH p = (a)-[:K*1..2]->(b) RETURN nodes(p) AS n, relationships(p) AS r, length(p) AS l \
     ORDER BY l, n",
    "MATCH (a)-[r]->(b) RETURN elementId(a) AS a, elementId(r) AS r, startNode(r) AS s, \
     endNode(r) AS e ORDER BY r",
    "MATCH (n) RETURN elementId(n) AS id ORDER BY id",
];

fn answers(db: &GraphDb) -> Vec<Vec<Vec<Option<Value>>>> {
    QUERIES
        .iter()
        .map(|q| {
            let rs = db.query(q, &BTreeMap::new()).unwrap();
            (0..rs.len()).map(|i| rs.row(i).to_vec()).collect()
        })
        .collect()
}

#[test]
fn entity_functions_agree_live_reopened_snapshotted_and_at() {
    let dir = tmp("probe");
    let mut db = GraphDb::open(&dir).unwrap();
    for q in [
        "CREATE (:P {id: 'ada'})",
        "CREATE (:P {id: 'bob'})",
        "CREATE (:P {id: 'cam'})",
        "MATCH (a {id: 'ada'}), (b {id: 'bob'}) CREATE (a)-[:K]->(b)",
        "MATCH (a {id: 'bob'}), (b {id: 'cam'}) CREATE (a)-[:K]->(b)",
        // The last commit only touches a property, so the commit before it
        // holds the same graph shape.
        "MATCH (a {id: 'ada'}) SET a.x = 1",
    ] {
        db.query_write(q, &BTreeMap::new()).unwrap();
    }
    let live = answers(&db);
    assert_eq!(
        live[0].len(),
        3,
        "two one-hop paths and one two-hop: {live:?}"
    );
    assert_eq!(live[2].len(), 3);
    let last = db.commit_seq();
    let at = GraphDb::open_at(&dir, last - 1).unwrap();
    assert_eq!(answers(&at), live, "open_at");
    drop(at);
    drop(db);
    let mut reopened = GraphDb::open(&dir).unwrap();
    assert_eq!(answers(&reopened), live, "WAL replay");
    reopened.snapshot().unwrap();
    drop(reopened);
    let after = GraphDb::open(&dir).unwrap();
    assert_eq!(answers(&after), live, "after a snapshot");
    let _ = std::fs::remove_dir_all(&dir);
}
