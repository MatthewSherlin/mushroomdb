use core_api::{Direction, GraphDb, GraphError, Predicate, RuleDef, Value};
use core_storage::wal::{decode_all, WalRecord};
use std::collections::BTreeSet;

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn wal_len(dir: &std::path::Path) -> u64 {
    std::fs::metadata(dir.join("wal.bin"))
        .map(|m| m.len())
        .unwrap_or(0)
}

fn tags(xs: &[&str]) -> Value {
    Value::List(xs.iter().map(|s| Value::Str((*s).into())).collect())
}

fn overlap_rule(name: &str, etype: &str) -> RuleDef {
    RuleDef {
        name: name.into(),
        src_label: "A".into(),
        dst_label: "A".into(),
        predicate: Predicate::Overlap {
            field: "tags".into(),
            min: 0.5,
        },
        edge_type: etype.into(),
        weight_prop: Some("score".into()),
    }
}

fn out_pairs(
    db: &GraphDb<core_storage::fs::RealFs>,
    keys: &[&str],
    etype: &str,
) -> BTreeSet<(String, String)> {
    let mut out = BTreeSet::new();
    for k in keys {
        if !db.has_node(k) {
            continue;
        }
        for n in db.neighbors(k, etype, Direction::Out).unwrap() {
            out.insert(((*k).to_string(), n));
        }
    }
    out
}

/// Binding (1): node+node+edge is one WAL frame (exactly one length+crc header).
#[test]
fn node_node_edge_batch_is_one_wal_frame() {
    let dir = tmp("batch-one-frame");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("A", "keep", vec![]).unwrap();
    let before = wal_len(&dir);

    db.batch()
        .insert_node("A", "a", vec![])
        .insert_node("A", "b", vec![])
        .insert_edge("E", "a", "b")
        .commit()
        .unwrap();

    assert!(db.has_node("a"));
    assert!(db.has_node("b"));
    assert_eq!(
        db.neighbors("a", "E", Direction::Out).unwrap(),
        vec!["b".to_string()]
    );

    let wal = std::fs::read(dir.join("wal.bin")).unwrap();
    let suffix = &wal[before as usize..];
    assert!(suffix.len() >= 8, "batch must append a framed record");
    let payload_len = u32::from_le_bytes(suffix[0..4].try_into().unwrap()) as usize;
    assert_eq!(
        suffix.len(),
        8 + payload_len,
        "exactly one length+crc header added (not N single-op frames)"
    );
    let (recs, consumed) = decode_all(suffix);
    assert_eq!(consumed, suffix.len());
    assert_eq!(recs.len(), 1);
    match &recs[0] {
        WalRecord::Batch(inner) => {
            assert_eq!(inner.len(), 3);
            assert!(matches!(&inner[0], WalRecord::InsertNode { key, .. } if key == "a"));
            assert!(matches!(&inner[1], WalRecord::InsertNode { key, .. } if key == "b"));
            assert!(
                matches!(&inner[2], WalRecord::InsertEdge { src_key, dst_key, .. } if src_key == "a" && dst_key == "b")
            );
        }
        other => panic!("expected Batch frame, got {other:?}"),
    }
}

/// Binding (2): invalid op in the middle rejects the whole batch; WAL and state unchanged.
#[test]
fn invalid_middle_op_rejects_batch_wal_and_state_unchanged() {
    let dir = tmp("batch-reject");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node(
        "A",
        "a",
        vec![
            ("name".into(), Value::Str("ada".into())),
            ("age".into(), Value::Int(36)),
        ],
    )
    .unwrap();
    db.insert_node("A", "b", vec![]).unwrap();
    db.insert_edge("KNOWS", "a", "b").unwrap();

    let before_wal = wal_len(&dir);
    let before_nodes = db.node_count();
    let before_edges = db.edge_count();
    let before_name = db.get_prop("a", "name").cloned();
    let before_age = db.get_prop("a", "age").cloned();

    let err = db
        .batch()
        .insert_node("A", "x", vec![])
        .insert_edge("E", "x", "ghost")
        .insert_node("A", "y", vec![])
        .commit()
        .unwrap_err();
    assert!(
        matches!(err, GraphError::KeyNotFound { ref key } if key == "ghost"),
        "expected KeyNotFound ghost, got {err:?}"
    );

    assert_eq!(wal_len(&dir), before_wal);
    assert_eq!(db.node_count(), before_nodes);
    assert_eq!(db.edge_count(), before_edges);
    assert_eq!(db.get_prop("a", "name").cloned(), before_name);
    assert_eq!(db.get_prop("a", "age").cloned(), before_age);
    assert!(!db.has_node("x"));
    assert!(!db.has_node("y"));
    assert!(db.has_node("a"));
    assert!(db.has_node("b"));
}

/// Binding (3): intra-batch edge on new nodes; delete+reinsert is a fresh identity.
#[test]
fn intra_batch_edge_and_delete_reinsert_fresh_identity() {
    let dir = tmp("batch-intra");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("A", "a", vec![("tags".into(), tags(&["x"]))])
        .unwrap();
    db.insert_node("A", "b", vec![("tags".into(), tags(&["x"]))])
        .unwrap();
    db.insert_node("A", "u", vec![]).unwrap();
    db.insert_edge("KNOWS", "a", "u").unwrap();
    db.create_rule(overlap_rule("rel", "REL")).unwrap();
    assert_eq!(db.edge_count(), 3); // a↔b derived + a→u

    db.batch()
        .insert_node("A", "p", vec![])
        .insert_node("A", "q", vec![])
        .insert_edge("LINK", "p", "q")
        .delete_node("a")
        .insert_node("A", "a", vec![("name".into(), Value::Str("fresh".into()))])
        .commit()
        .unwrap();

    assert!(db.has_node("p"));
    assert!(db.has_node("q"));
    assert_eq!(
        db.neighbors("p", "LINK", Direction::Out).unwrap(),
        vec!["q".to_string()]
    );

    assert!(db.has_node("a"));
    assert_eq!(db.get_prop("a", "name"), Some(&Value::Str("fresh".into())));
    assert_eq!(db.get_prop("a", "tags"), None);
    assert!(db.neighbors("a", "REL", Direction::Out).unwrap().is_empty());
    assert!(db
        .neighbors("a", "KNOWS", Direction::Out)
        .unwrap()
        .is_empty());
    assert!(db.neighbors("b", "REL", Direction::Out).unwrap().is_empty());
    assert!(db
        .neighbors("u", "KNOWS", Direction::In)
        .unwrap()
        .is_empty());
}

/// Binding (4): torn mid-frame drops the entire batch on reopen.
#[test]
fn torn_mid_batch_frame_drops_whole_batch_on_reopen() {
    let dir = tmp("batch-torn");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("A", "keep", vec![("v".into(), Value::Int(1))])
            .unwrap();
        db.batch()
            .insert_node("A", "x", vec![])
            .insert_node("A", "y", vec![])
            .insert_edge("E", "x", "y")
            .commit()
            .unwrap();
        assert!(db.has_node("x"));
    }
    let mut bytes = std::fs::read(dir.join("wal.bin")).unwrap();
    assert!(bytes.len() > 3);
    bytes.truncate(bytes.len() - 3);
    std::fs::write(dir.join("wal.bin"), &bytes).unwrap();

    let db = GraphDb::open(&dir).unwrap();
    assert!(db.has_node("keep"));
    assert_eq!(db.get_prop("keep", "v"), Some(&Value::Int(1)));
    assert!(!db.has_node("x"));
    assert!(!db.has_node("y"));
    assert_eq!(db.edge_count(), 0);
}

/// Binding (5): rules fire the same in a batch as in sequential inserts.
#[test]
fn batch_rules_match_sequential_twin_edge_set() {
    let keys = ["a", "b", "c"];

    let dir_batch = tmp("batch-rules-b");
    let mut batched = GraphDb::open(&dir_batch).unwrap();
    batched
        .batch()
        .insert_node("A", "a", vec![("tags".into(), tags(&["x"]))])
        .insert_node("A", "b", vec![("tags".into(), tags(&["x"]))])
        .create_rule(overlap_rule("rel", "REL"))
        .insert_node("A", "c", vec![("tags".into(), tags(&["x"]))])
        .set_prop("a", "tags", tags(&["x", "y"]))
        .commit()
        .unwrap();

    let dir_seq = tmp("batch-rules-s");
    let mut sequential = GraphDb::open(&dir_seq).unwrap();
    sequential
        .insert_node("A", "a", vec![("tags".into(), tags(&["x"]))])
        .unwrap();
    sequential
        .insert_node("A", "b", vec![("tags".into(), tags(&["x"]))])
        .unwrap();
    sequential.create_rule(overlap_rule("rel", "REL")).unwrap();
    sequential
        .insert_node("A", "c", vec![("tags".into(), tags(&["x"]))])
        .unwrap();
    sequential.set_prop("a", "tags", tags(&["x", "y"])).unwrap();

    assert_eq!(
        out_pairs(&batched, &keys, "REL"),
        out_pairs(&sequential, &keys, "REL")
    );
    assert_eq!(batched.edge_count(), sequential.edge_count());
}

/// Binding (6): crash-window replay of a Batch over an already-applied snapshot is a no-op.
#[test]
fn crash_window_replays_batch_idempotently() {
    let dir = tmp("batch-crash-window");
    let expected_edges;
    let expected_name;
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("A", "a", vec![("tags".into(), tags(&["x"]))])
            .unwrap();
        db.insert_node("A", "b", vec![("tags".into(), tags(&["x"]))])
            .unwrap();
        db.insert_node("A", "u", vec![("n".into(), Value::Int(1))])
            .unwrap();
        db.insert_edge("KNOWS", "a", "u").unwrap();

        db.batch()
            .create_rule(overlap_rule("rel", "REL"))
            .remove_prop("u", "n")
            .delete_edge("KNOWS", "a", "u")
            .set_prop("a", "name", Value::Str("ada".into()))
            .insert_node("A", "c", vec![("tags".into(), tags(&["x"]))])
            .commit()
            .unwrap();

        expected_edges = out_pairs(&db, &["a", "b", "c", "u"], "REL");
        expected_name = db.get_prop("a", "name").cloned();
        assert_eq!(db.get_prop("u", "n"), None);
        assert!(db
            .neighbors("a", "KNOWS", Direction::Out)
            .unwrap()
            .is_empty());

        let pre_snap_wal = std::fs::read(dir.join("wal.bin")).unwrap();
        db.snapshot().unwrap();
        std::fs::write(dir.join("wal.bin"), &pre_snap_wal).unwrap();
    }
    let db = GraphDb::open(&dir).unwrap();
    assert!(db.has_node("c"));
    assert_eq!(db.get_prop("a", "name"), expected_name.as_ref());
    assert_eq!(db.get_prop("u", "n"), None);
    assert!(db
        .neighbors("a", "KNOWS", Direction::Out)
        .unwrap()
        .is_empty());
    assert_eq!(out_pairs(&db, &["a", "b", "c", "u"], "REL"), expected_edges);
    assert_eq!(db.rules().len(), 1);
}

/// Binding (7): empty batch writes zero WAL bytes.
#[test]
fn empty_batch_writes_zero_wal_bytes() {
    let dir = tmp("batch-empty");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("A", "a", vec![]).unwrap();
    let before = wal_len(&dir);
    db.batch().commit().unwrap();
    assert_eq!(wal_len(&dir), before);
    assert!(db.has_node("a"));
    assert_eq!(db.node_count(), 1);
}

/// Duplicate key inside the batch is rejected with nothing applied.
#[test]
fn duplicate_key_inside_batch_is_rejected() {
    let dir = tmp("batch-dup");
    let mut db = GraphDb::open(&dir).unwrap();
    let before = wal_len(&dir);
    let err = db
        .batch()
        .insert_node("A", "a", vec![])
        .insert_node("A", "a", vec![])
        .commit()
        .unwrap_err();
    assert!(matches!(err, GraphError::DuplicateKey { ref key } if key == "a"));
    assert_eq!(wal_len(&dir), before);
    assert!(!db.has_node("a"));
}
