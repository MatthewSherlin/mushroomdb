//! A snapshot of a store that has never committed (ledger row 119).
//!
//! `snapshot_with(archive_wal)` renamed `wal.bin` to an archive even when
//! there was no `wal.bin`, so the first snapshot of a fresh store failed with
//! "No such file" after it had written `snapshot.intent`. The same call is
//! what `mushroomdb snapshot <fresh-dir>` and a header-only CSV import make.
use mushroomdb::{GraphDb, SnapshotOptions, Value};
use std::path::{Path, PathBuf};

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("mdb-snap-empty-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn intent_is_clear(dir: &Path) -> bool {
    std::fs::read(dir.join("snapshot.intent")).map_or(true, |b| b.is_empty())
}

fn archives(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.ends_with(".archive"))
        .collect()
}

fn archive() -> SnapshotOptions {
    SnapshotOptions {
        keep_wal: false,
        archive_wal: true,
    }
}

fn modes() -> [(&'static str, SnapshotOptions); 3] {
    [
        (
            "archive",
            SnapshotOptions {
                keep_wal: false,
                archive_wal: true,
            },
        ),
        (
            "truncate",
            SnapshotOptions {
                keep_wal: false,
                archive_wal: false,
            },
        ),
        (
            "keep",
            SnapshotOptions {
                keep_wal: true,
                archive_wal: false,
            },
        ),
    ]
}

#[test]
fn a_snapshot_of_a_store_with_no_commits_succeeds_and_leaves_no_intent() {
    for (name, opts) in modes() {
        let dir = tmp(name);
        let mut db = GraphDb::open(&dir).unwrap();
        db.snapshot_with(opts.clone())
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(intent_is_clear(&dir), "{name}: snapshot.intent left behind");
        assert!(
            archives(&dir).is_empty(),
            "{name}: an empty WAL is not archived"
        );
        drop(db);

        // It opens, is empty, and takes writes and a second snapshot.
        let mut db = GraphDb::open(&dir).unwrap();
        assert_eq!(db.node_count(), 0);
        db.insert_node("P", "a", vec![("g".into(), Value::Int(1))])
            .unwrap();
        db.snapshot_with(opts)
            .unwrap_or_else(|e| panic!("{name} 2nd: {e}"));
        assert!(
            intent_is_clear(&dir),
            "{name}: intent after second snapshot"
        );
        drop(db);
        let db = GraphDb::open(&dir).unwrap();
        assert!(db.has_node("a"), "{name}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn a_snapshot_of_a_store_with_only_a_declaration_keeps_the_declaration() {
    let dir = tmp("declaration");
    let mut db = GraphDb::open(&dir).unwrap();
    db.enable_fulltext("P", "name").unwrap();
    db.snapshot_with(archive()).unwrap();
    assert!(intent_is_clear(&dir));
    drop(db);
    let db = GraphDb::open(&dir).unwrap();
    assert!(db.is_fulltext_enabled("P", "name"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_first_archive_after_an_empty_snapshot_still_holds_the_history() {
    let dir = tmp("then-archive");
    let mut db = GraphDb::open(&dir).unwrap();
    db.snapshot_with(archive()).unwrap();
    db.insert_node("P", "a", vec![]).unwrap();
    db.snapshot_with(archive()).unwrap();
    assert_eq!(archives(&dir).len(), 1, "the write is archived");
    drop(db);
    let db = GraphDb::open_at(&dir, 0).unwrap();
    assert!(db.has_node("a"), "history reachable through the archive");
    let _ = std::fs::remove_dir_all(&dir);
}
