use core_api::{GraphDb, OpenOptions, Value};

fn store_from_fixture(name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-migrate-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("snapshot.bin"), bytes).unwrap();
    std::fs::write(d.join("wal.bin"), b"").unwrap();
    d
}

/// A V5 store opens, migrates to the current VERSION, leaves a .bak of the
/// original bytes; a second clean open finds VERSION and deletes the .bak.
#[test]
fn v5_store_auto_migrates_on_open_with_bak() {
    let dir = store_from_fixture("v5", include_bytes!("fixtures/golden_v5.bin"));
    {
        let db = GraphDb::open(&dir).unwrap();
        assert_eq!(db.node_count(), 2);
    }
    // On-disk snapshot is now current VERSION; .bak holds the old bytes.
    assert_eq!(
        core_api::snapshot_version_at(&dir).unwrap(),
        Some(core_storage::snapshot::VERSION),
        "snapshot must be rewritten to current VERSION after migration"
    );
    let bak = std::fs::read(dir.join("snapshot.bin.bak")).unwrap();
    assert_eq!(
        u16::from_le_bytes([bak[4], bak[5]]),
        5,
        ".bak must contain the original V5 bytes"
    );
    // Data survives the migration.
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(
        db.get_prop("a", "v"),
        Some(&Value::Int(42)),
        "property v=42 on node 'a' must survive migration"
    );
    // Second clean open at current version must remove .bak.
    assert!(
        !dir.join("snapshot.bin.bak").exists(),
        "second clean open must delete the .bak"
    );
}

/// Opening with auto_migrate=false must not touch any on-disk file.
#[test]
fn auto_migrate_false_leaves_disk_untouched() {
    let dir = store_from_fixture("v6-noauto", include_bytes!("fixtures/golden_v6.bin"));
    let before = std::fs::read(dir.join("snapshot.bin")).unwrap();
    let _db = GraphDb::open_with_options(
        &dir,
        OpenOptions {
            auto_migrate: false,
        },
    )
    .unwrap();
    assert_eq!(
        std::fs::read(dir.join("snapshot.bin")).unwrap(),
        before,
        "snapshot.bin must be byte-identical after auto_migrate=false open"
    );
    assert!(
        !dir.join("snapshot.bin.bak").exists(),
        "no .bak should be created when auto_migrate=false"
    );
}

/// Opening a store that is already at the current version is a no-op:
/// no .bak is created.
#[test]
fn current_version_open_is_a_no_op_migration() {
    let dir = std::env::temp_dir().join(format!(
        "graphdb-migrate-current-noop-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "x", vec![]).unwrap();
        db.snapshot().unwrap(); // writes current VERSION
    }
    // Reopen: no migration needed.
    let _db = GraphDb::open(&dir).unwrap();
    assert!(
        !dir.join("snapshot.bin.bak").exists(),
        "no .bak should appear when snapshot is already at current VERSION"
    );
}

/// Auto-migrate uses keep_wal=true: WAL frames written before the migration
/// are preserved and the migrated node is visible after reopen.
#[test]
fn wal_preserved_by_auto_migrate() {
    // Phase 1: set up a V6 store with a non-empty WAL tail.
    let dir = store_from_fixture("v6-keepwal", include_bytes!("fixtures/golden_v6.bin"));
    {
        // Open with auto_migrate=false so we can write to the WAL without
        // triggering migration yet.
        let mut db = GraphDb::open_with_options(
            &dir,
            OpenOptions {
                auto_migrate: false,
            },
        )
        .unwrap();
        db.insert_node("N", "extra", vec![]).unwrap();
        // db drops here — WAL is fsynced on each mutation, so "extra" is durable.
    }
    // Verify there is at least one WAL commit before migration.
    let wal_count_before = core_api::wal_commit_count_at(&dir).unwrap();
    assert!(
        wal_count_before > 0,
        "WAL must have at least one commit before migration"
    );

    // Phase 2: reopen with auto_migrate=true (default). Migration uses keep_wal=true.
    let db = GraphDb::open(&dir).unwrap();

    // The node inserted via WAL must be visible (WAL was replayed).
    assert!(
        db.has_node("extra"),
        "WAL-inserted node 'extra' must survive migration"
    );

    // WAL commit count must not have dropped to zero (keep_wal preserved WAL).
    let wal_count_after = core_api::wal_commit_count_at(&dir).unwrap();
    assert!(
        wal_count_after > 0,
        "WAL commit count must remain > 0 after auto-migrate (keep_wal)"
    );
}
