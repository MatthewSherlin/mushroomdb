use core_api::{Direction, GraphDb, GraphError, Predicate, RuleDef, Value};

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

#[test]
fn snapshot_plus_wal_tail_recovers_everything() {
    let dir = tmp("snap");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "a", vec![("v".into(), Value::Int(1))])
            .unwrap();
        db.insert_node("N", "b", vec![]).unwrap();
        db.insert_edge("E", "a", "b").unwrap();
        db.snapshot().unwrap();
        // post-snapshot writes live only in the wal tail
        db.insert_node("N", "c", vec![]).unwrap();
        db.insert_edge("E", "b", "c").unwrap();
    }
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.node_count(), 3);
    assert_eq!(db.edge_count(), 2);
    assert_eq!(db.get_prop("a", "v"), Some(&Value::Int(1)));
    assert_eq!(db.neighbors("b", "E", Direction::Out).unwrap(), vec!["c"]);
}

#[test]
fn corrupt_snapshot_fails_loudly() {
    let dir = tmp("snapcorrupt");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "a", vec![]).unwrap();
        db.snapshot().unwrap();
    }
    let path = dir.join("snapshot.bin");
    let mut bytes = std::fs::read(&path).unwrap();
    let n = bytes.len();
    bytes[n - 1] ^= 0xFF;
    std::fs::write(&path, bytes).unwrap();
    assert!(GraphDb::open(&dir).is_err()); // spec §8: refuse loudly, never guess
}

#[test]
fn snapshot_preserves_rules_provenance_and_scores() {
    let dir = tmp("snap-rules");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("A", "a", vec![("tags".into(), Value::List(vec![Value::Str("x".into())]))]).unwrap();
        db.create_rule(RuleDef {
            name: "rel".into(), src_label: "A".into(), dst_label: "A".into(),
            predicate: Predicate::Overlap { field: "tags".into(), min: 0.5 },
            edge_type: "REL".into(), weight_prop: Some("score".into()),
        }).unwrap();
        db.insert_node("A", "b", vec![("tags".into(), Value::List(vec![Value::Str("x".into())]))]).unwrap();
        db.snapshot().unwrap();
        // post-snapshot wal-tail write
        db.insert_node("A", "c", vec![("tags".into(), Value::List(vec![Value::Str("x".into())]))]).unwrap();
    }
    let mut db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.rules().len(), 1);
    assert_eq!(db.edge_count(), 6); // a,b,c pairwise, both directions
    // derived edges still owned after recovery (guard works)
    assert!(matches!(db.insert_edge("REL", "a", "b"), Err(GraphError::RuleOwned { .. })));
    // and incremental firing still works after reopen (indexes were rebuilt)
    db.insert_node("A", "d", vec![("tags".into(), Value::List(vec![Value::Str("x".into())]))]).unwrap();
    assert_eq!(db.edge_count(), 12);
}

#[test]
fn version_1_snapshot_is_rejected() {
    let dir = tmp("snap-v1");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "a", vec![]).unwrap();
        db.snapshot().unwrap();
    }
    let path = dir.join("snapshot.bin");
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[4] = 1; bytes[5] = 0; // stamp VERSION=1
    std::fs::write(&path, bytes).unwrap();
    match GraphDb::open(&dir) {
        Err(GraphError::Corrupt { detail }) => assert!(detail.contains("version 1"), "{detail}"),
        Ok(_) => panic!("expected Corrupt, got Ok"),
        Err(e) => panic!("expected Corrupt, got other error: {e:?}"),
    }
}
