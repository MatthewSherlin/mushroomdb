//! Tests for `GraphDb::backup_to` (Task 4: consistent online backup).

use core_api::{
    BackupReport, Direction, GraphDb, Predicate, RoleDef, RuleDef, Schema, SnapshotOptions, Value,
};
use std::path::PathBuf;

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "graphdb-backup-{}-{}-{}",
        name,
        std::process::id(),
        nanos
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// Build a store that has: snapshot.bin + wal.bin (tail) + one WAL archive +
/// wal.floor + wal.genesis + roles.json.
///
/// Returns the db dir and the source node count for query comparison.
fn build_store_with_archives(dir: &std::path::Path) -> usize {
    let mut db = GraphDb::open(dir).unwrap();
    db.insert_node(
        "Person",
        "p1",
        vec![("name".into(), Value::Str("Alice".into()))],
    )
    .unwrap();
    db.insert_node(
        "Person",
        "p2",
        vec![("name".into(), Value::Str("Bob".into()))],
    )
    .unwrap();
    db.insert_edge("KNOWS", "p1", "p2").unwrap();

    // Rule so there are derived edges to test provenance copy.
    db.create_rule(RuleDef {
        name: "knows_back".into(),
        src_label: "Person".into(),
        dst_label: "Person".into(),
        predicate: Predicate::FieldEqual {
            field: "name".into(),
        },
        edge_type: "KNOWS_SAME".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    })
    .unwrap();

    // Archive snapshot: snapshot.bin + wal.<N>.archive + wal.floor + wal.genesis.
    db.set_wal_archive_retention(None);
    db.snapshot_with(SnapshotOptions {
        archive_wal: true,
        keep_wal: false,
    })
    .unwrap();

    // Write a tail WAL entry so wal.bin is non-empty after the archive.
    db.insert_node(
        "Person",
        "p3",
        vec![("name".into(), Value::Str("Carol".into()))],
    )
    .unwrap();

    // Apply schema with a role so roles.json is written.
    let schema = Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![],
        roles: vec![RoleDef {
            name: "reader".into(),
            keys: vec![],
            labels: vec!["Person".into()],
            write: None,
        }],
    };
    db.apply_schema(&schema).unwrap();

    db.node_count()
}

// ── test 1: all expected files are copied ────────────────────────────────────

#[test]
fn backup_copies_all_expected_files() {
    let src = tmp("src1");
    let dst = tmp("dst1");
    let _n = build_store_with_archives(&src);

    let db = GraphDb::open(&src).unwrap();
    let report: BackupReport = db.backup_to(&dst).unwrap();

    // snapshot.bin must be present.
    assert!(
        report.files.contains(&"snapshot.bin".to_string()),
        "expected snapshot.bin in files: {:?}",
        report.files
    );
    // At least one archive must have been copied.
    let has_archive = report.files.iter().any(|f| f.ends_with(".archive"));
    assert!(
        has_archive,
        "expected at least one .archive in files: {:?}",
        report.files
    );
    // wal.floor is written only when archives are pruned; assert only if it exists in source.
    let src_floor = src.join("wal.floor");
    if src_floor.exists() {
        assert!(
            report.files.contains(&"wal.floor".to_string()),
            "expected wal.floor in files: {:?}",
            report.files
        );
    }
    // wal.genesis must be copied (written on first archive_wal).
    assert!(
        report.files.contains(&"wal.genesis".to_string()),
        "expected wal.genesis in files: {:?}",
        report.files
    );
    // roles.json must be copied.
    assert!(
        report.files.contains(&"roles.json".to_string()),
        "expected roles.json in files: {:?}",
        report.files
    );
    // Physical files must actually exist in the dest dir.
    for f in &report.files {
        let p = dst.join(f);
        assert!(p.exists(), "file not on disk: {}", p.display());
    }
    assert!(report.bytes > 0, "should have copied non-zero bytes");
}

// ── test 2: backup opens clean and query-equals source ───────────────────────

#[test]
fn backup_dest_opens_clean_and_queries_equal() {
    let src = tmp("src2");
    let dst = tmp("dst2");
    let src_count = build_store_with_archives(&src);

    let db = GraphDb::open(&src).unwrap();
    let report = db.backup_to(&dst).unwrap();
    assert!(report.verified, "backup verification must succeed");
    drop(db);

    let backup_db = GraphDb::open(&dst).unwrap();
    assert_eq!(
        backup_db.node_count(),
        src_count,
        "backup node count must match source"
    );
    // Manual edge must survive.
    let nbrs = backup_db.neighbors("p1", "KNOWS", Direction::Out).unwrap();
    assert_eq!(nbrs, vec!["p2"], "manual edge p1→p2 must be in backup");
}

// ── test 3: verified flag is true for valid backup ───────────────────────────

#[test]
fn backup_verified_is_true() {
    let src = tmp("src3");
    let dst = tmp("dst3");
    build_store_with_archives(&src);
    let db = GraphDb::open(&src).unwrap();
    let report = db.backup_to(&dst).unwrap();
    assert!(report.verified, "verified must be true for a clean backup");
}

// ── test 4: simple store (no archive) also backs up correctly ─────────────────

#[test]
fn backup_simple_store_no_archive() {
    let src = tmp("src4");
    let dst = tmp("dst4");
    {
        let mut db = GraphDb::open(&src).unwrap();
        db.insert_node("A", "x", vec![]).unwrap();
        db.insert_node("A", "y", vec![]).unwrap();
        db.insert_edge("E", "x", "y").unwrap();
        db.snapshot().unwrap();
        // WAL tail
        db.insert_node("A", "z", vec![]).unwrap();
    }

    let db = GraphDb::open(&src).unwrap();
    let report = db.backup_to(&dst).unwrap();
    assert!(report.verified, "verified must be true");

    let bdb = GraphDb::open(&dst).unwrap();
    assert_eq!(bdb.node_count(), 3);
}

// ── test 5: backup while concurrent reader sees the same data ─────────────────

#[test]
fn backup_is_consistent_snapshot() {
    let src = tmp("src5");
    let dst = tmp("dst5");

    {
        let mut db = GraphDb::open(&src).unwrap();
        for i in 0..10 {
            db.insert_node("N", &format!("n{i}"), vec![]).unwrap();
        }
        db.snapshot().unwrap();
    }

    let db = GraphDb::open(&src).unwrap();
    let count_before = db.node_count();
    let report = db.backup_to(&dst).unwrap();
    assert!(report.verified);

    let bdb = GraphDb::open(&dst).unwrap();
    assert_eq!(bdb.node_count(), count_before);
}

// ── test 6: export all_nodes_for_export is sorted and complete ───────────────

#[test]
fn all_nodes_for_export_sorted_and_complete() {
    let dir = tmp("src6");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("A", "z", vec![("v".into(), Value::Int(3))])
        .unwrap();
    db.insert_node("A", "a", vec![("v".into(), Value::Int(1))])
        .unwrap();
    db.insert_node("B", "m", vec![]).unwrap();

    let nodes = db.all_nodes_for_export();
    assert_eq!(nodes.len(), 3);
    // Sorted by key ascending.
    let keys: Vec<&str> = nodes.iter().map(|n| n.key.as_str()).collect();
    assert_eq!(keys, vec!["a", "m", "z"], "nodes must be sorted by key");
    // Labels preserved.
    let a = nodes.iter().find(|n| n.key == "a").unwrap();
    assert_eq!(a.label, "A");
    assert_eq!(a.props.get("v"), Some(&Value::Int(1)));
}

// ── test 7: all_nodes_for_export is deterministic across two calls ────────────

#[test]
fn all_nodes_for_export_deterministic() {
    let dir = tmp("src7");
    let mut db = GraphDb::open(&dir).unwrap();
    for i in 0..20 {
        db.insert_node("N", &format!("n{i:02}"), vec![]).unwrap();
    }
    let first = db.all_nodes_for_export();
    let second = db.all_nodes_for_export();
    assert_eq!(first, second, "all_nodes_for_export must be deterministic");
}

// ── test 8: all_edges_for_export includes derived edges with rule name ─────────

#[test]
fn all_edges_for_export_includes_derived_with_rule() {
    let dir = tmp("src8");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("T", "a", vec![("tag".into(), Value::Str("x".into()))])
        .unwrap();
    db.insert_node("T", "b", vec![("tag".into(), Value::Str("x".into()))])
        .unwrap();
    db.create_rule(RuleDef {
        name: "same_tag".into(),
        src_label: "T".into(),
        dst_label: "T".into(),
        predicate: Predicate::FieldEqual {
            field: "tag".into(),
        },
        edge_type: "SAME".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    })
    .unwrap();

    let edges = db.all_edges_for_export();
    // There should be derived edges a→b and b→a.
    let derived_edges: Vec<_> = edges.iter().filter(|e| e.derived).collect();
    assert!(
        !derived_edges.is_empty(),
        "expected derived edges, got: {:?}",
        edges
    );
    for e in &derived_edges {
        assert_eq!(
            e.rule.as_deref(),
            Some("same_tag"),
            "derived edge should carry rule name"
        );
    }
}

// ── test 9: all_edges_for_export sorted by (edge_type, src, dst) ─────────────

#[test]
fn all_edges_for_export_sorted() {
    let dir = tmp("src9");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("N", "c", vec![]).unwrap();
    db.insert_node("N", "a", vec![]).unwrap();
    db.insert_node("N", "b", vec![]).unwrap();
    db.insert_edge("Z", "c", "a").unwrap();
    db.insert_edge("A", "b", "c").unwrap();
    db.insert_edge("A", "a", "b").unwrap();

    let edges = db.all_edges_for_export();
    // Check sorted order: A/a→b, A/b→c, Z/c→a.
    assert_eq!(edges[0].edge_type, "A");
    assert_eq!(edges[0].src, "a");
    assert_eq!(edges[0].dst, "b");
    assert_eq!(edges[1].edge_type, "A");
    assert_eq!(edges[1].src, "b");
    assert_eq!(edges[1].dst, "c");
    assert_eq!(edges[2].edge_type, "Z");
}
