//! A single ingest that both creates a rule (auto-FK) and inserts user edges of a
//! not-yet-interned type must produce a WAL frame that replays identically.
use core_api::{Direction, GraphDb, IngestOptions, Value};
use std::collections::BTreeMap;

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "ingest-edges-replay-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn row(pairs: &[(&str, &str)]) -> BTreeMap<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), Value::Str((*v).to_string())))
        .collect()
}

fn seed(db: &mut GraphDb<core_storage::fs::RealFs>) {
    let o = IngestOptions::default();
    db.ingest_with_edges(
        "Author",
        vec![row(&[("id", "a@x"), ("name", "a")])],
        &o,
        &[],
    )
    .unwrap();
    db.ingest_with_edges(
        "File",
        vec![row(&[("id", "f1"), ("top_author_id", "a@x")])],
        &o,
        &[],
    )
    .unwrap();
    // rule (auto-FK Commit.author_id → AUTHOR) + user edge TOUCHED in ONE call
    db.ingest_with_edges(
        "Commit",
        vec![row(&[("id", "c1"), ("author_id", "a@x")])],
        &o,
        &[("TOUCHED".into(), "c1".into(), "f1".into())],
    )
    .unwrap();
}

fn assert_shape(db: &GraphDb<core_storage::fs::RealFs>) {
    assert_eq!(
        db.neighbors("c1", "AUTHOR", Direction::Out).unwrap(),
        vec!["a@x".to_string()]
    );
    assert_eq!(
        db.neighbors("c1", "TOUCHED", Direction::Out).unwrap(),
        vec!["f1".to_string()]
    );
    assert_eq!(
        db.neighbors("f1", "TOP_AUTHOR", Direction::Out).unwrap(),
        vec!["a@x".to_string()]
    );
}

#[test]
fn rule_plus_user_edge_in_one_ingest_replays_from_wal() {
    let dir = tmp("wal");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        seed(&mut db);
        assert_shape(&db);
    }
    let db = GraphDb::open(&dir).expect("reopen must replay the frame");
    assert_shape(&db);
}

#[test]
fn rule_plus_user_edge_in_one_ingest_survives_snapshot_then_more_wal() {
    let dir = tmp("snap");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        seed(&mut db);
        db.snapshot().unwrap();
        // a second frame of the same shape after the snapshot
        let o = IngestOptions::default();
        db.ingest_with_edges(
            "Commit",
            vec![row(&[("id", "c2"), ("author_id", "a@x")])],
            &o,
            &[("TOUCHED".into(), "c2".into(), "f1".into())],
        )
        .unwrap();
    }
    let db = GraphDb::open(&dir).unwrap();
    assert_shape(&db);
    assert_eq!(
        db.neighbors("c2", "TOUCHED", Direction::Out).unwrap(),
        vec!["f1".to_string()]
    );
}

#[test]
fn two_new_edge_types_and_two_new_rules_in_one_ingest_replay() {
    let dir = tmp("multi");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        let o = IngestOptions::default();
        db.ingest_with_edges("Author", vec![row(&[("id", "a@x")])], &o, &[])
            .unwrap();
        db.ingest_with_edges("Repo", vec![row(&[("id", "r1")])], &o, &[])
            .unwrap();
        db.ingest_with_edges(
            "File",
            vec![row(&[
                ("id", "f1"),
                ("top_author_id", "a@x"),
                ("repo_id", "r1"),
            ])],
            &o,
            &[],
        )
        .unwrap();
        db.ingest_with_edges(
            "Commit",
            vec![row(&[
                ("id", "c1"),
                ("author_id", "a@x"),
                ("repo_id", "r1"),
            ])],
            &o,
            &[
                ("TOUCHED".into(), "c1".into(), "f1".into()),
                ("IN_REPO".into(), "c1".into(), "r1".into()),
            ],
        )
        .unwrap();
    }
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(
        db.neighbors("c1", "AUTHOR", Direction::Out).unwrap(),
        vec!["a@x".to_string()]
    );
    assert_eq!(
        db.neighbors("c1", "REPO", Direction::Out).unwrap(),
        vec!["r1".to_string()]
    );
    assert_eq!(
        db.neighbors("c1", "TOUCHED", Direction::Out).unwrap(),
        vec!["f1".to_string()]
    );
    assert_eq!(
        db.neighbors("c1", "IN_REPO", Direction::Out).unwrap(),
        vec!["r1".to_string()]
    );
}
