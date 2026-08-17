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
        db.insert_node(
            "A",
            "a",
            vec![("tags".into(), Value::List(vec![Value::Str("x".into())]))],
        )
        .unwrap();
        db.create_rule(RuleDef {
            name: "rel".into(),
            src_label: "A".into(),
            dst_label: "A".into(),
            predicate: Predicate::Overlap {
                field: "tags".into(),
                min: 0.5,
            },
            edge_type: "REL".into(),
            weight_prop: Some("score".into()),
            max_edges: None,
        })
        .unwrap();
        db.insert_node(
            "A",
            "b",
            vec![("tags".into(), Value::List(vec![Value::Str("x".into())]))],
        )
        .unwrap();
        db.snapshot().unwrap();
        // post-snapshot wal-tail write
        db.insert_node(
            "A",
            "c",
            vec![("tags".into(), Value::List(vec![Value::Str("x".into())]))],
        )
        .unwrap();
    }
    let mut db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.rules().len(), 1);
    assert_eq!(db.edge_count(), 6); // a,b,c pairwise, both directions
                                    // derived edges still owned after recovery (guard works)
    assert!(matches!(
        db.insert_edge("REL", "a", "b"),
        Err(GraphError::RuleOwned { .. })
    ));
    // and incremental firing still works after reopen (indexes were rebuilt)
    db.insert_node(
        "A",
        "d",
        vec![("tags".into(), Value::List(vec![Value::Str("x".into())]))],
    )
    .unwrap();
    assert_eq!(db.edge_count(), 12);
}

#[test]
fn version_2_snapshot_is_rejected() {
    let dir = tmp("snap-v2");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "a", vec![]).unwrap();
        db.snapshot().unwrap();
    }
    let path = dir.join("snapshot.bin");
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[4] = 2;
    bytes[5] = 0; // stamp VERSION=2
    std::fs::write(&path, bytes).unwrap();
    match GraphDb::open(&dir) {
        Err(GraphError::Corrupt { detail }) => assert!(detail.contains("version 2"), "{detail}"),
        Ok(_) => panic!("expected Corrupt, got Ok"),
        Err(e) => panic!("expected Corrupt, got other error: {e:?}"),
    }
}

#[test]
fn crash_between_snapshot_and_wal_truncation_recovers() {
    // === Phase 1: CreateRule idempotency ===
    // Snapshot captures rule; crash leaves pre-snapshot WAL (with CreateRule) intact.
    // Reopen must not fail with RuleInvalid on the duplicate CreateRule replay.
    let dir = tmp("crash-create-rule");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node(
            "A",
            "a",
            vec![("tags".into(), Value::List(vec![Value::Str("x".into())]))],
        )
        .unwrap();
        db.create_rule(RuleDef {
            name: "rel".into(),
            src_label: "A".into(),
            dst_label: "A".into(),
            predicate: Predicate::Overlap {
                field: "tags".into(),
                min: 0.5,
            },
            edge_type: "REL".into(),
            weight_prop: None,
            max_edges: None,
        })
        .unwrap();
        db.insert_node(
            "A",
            "b",
            vec![("tags".into(), Value::List(vec![Value::Str("x".into())]))],
        )
        .unwrap();
        // save pre-snapshot WAL (has InsertNode a, CreateRule rel, InsertNode b)
        let pre_snap_wal = std::fs::read(dir.join("wal.bin")).unwrap();
        db.snapshot().unwrap();
        // simulate crash: restore pre-snapshot WAL before truncation took effect
        std::fs::write(dir.join("wal.bin"), &pre_snap_wal).unwrap();
    }
    let mut db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.rules().len(), 1);
    assert_eq!(db.edge_count(), 2); // a↔b both directions
                                    // incremental firing still works after recovery
    db.insert_node(
        "A",
        "c",
        vec![("tags".into(), Value::List(vec![Value::Str("x".into())]))],
    )
    .unwrap();
    assert_eq!(db.edge_count(), 6); // a,b,c pairwise

    // === Phase 2: DeleteRule idempotency ===
    // Snapshot captures state without a deleted rule; crash leaves WAL (with DeleteRule) intact.
    // Reopen must not fail with RuleNotFound on the replay of a delete for an absent rule.
    let dir2 = tmp("crash-delete-rule");
    {
        let mut db = GraphDb::open(&dir2).unwrap();
        db.insert_node("A", "x", vec![]).unwrap();
        db.create_rule(RuleDef {
            name: "gone".into(),
            src_label: "A".into(),
            dst_label: "A".into(),
            predicate: Predicate::Overlap {
                field: "tags".into(),
                min: 0.5,
            },
            edge_type: "GONE".into(),
            weight_prop: None,
            max_edges: None,
        })
        .unwrap();
        // first snapshot: rule "gone" in engine
        db.snapshot().unwrap();
        // delete the rule; WAL now contains [DeleteRule "gone"]
        db.delete_rule("gone").unwrap();
        let pre_snap_wal = std::fs::read(dir2.join("wal.bin")).unwrap();
        // second snapshot: captures state WITHOUT "gone"; truncates WAL
        db.snapshot().unwrap();
        // simulate crash before WAL truncation
        std::fs::write(dir2.join("wal.bin"), &pre_snap_wal).unwrap();
    }
    // reopen: snapshot has no "gone", WAL replays DeleteRule "gone" for missing rule → must not error
    let db = GraphDb::open(&dir2).unwrap();
    assert_eq!(db.rules().len(), 0); // "gone" is still gone
}
