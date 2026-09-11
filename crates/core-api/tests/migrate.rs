use core_api::{Direction, GraphDb, OpenOptions, Value};

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
        Some(Value::Int(42)),
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
            ..Default::default()
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
                ..Default::default()
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

/// A V7 store opens, migrates to V8, leaves a .bak of the original V7 bytes;
/// a second clean open at V8 deletes the .bak.
///
/// V7 fixture content: 2 nodes ("a", "b"), 1 edge (E: a→b), prop v=42 on "a".
#[test]
fn v7_store_auto_migrates_to_v8_with_bak() {
    let dir = store_from_fixture("v7", include_bytes!("fixtures/golden_v7.bin"));
    {
        let db = GraphDb::open(&dir).unwrap();
        assert_eq!(
            db.node_count(),
            2,
            "V7 store must have 2 nodes after migrate"
        );
        assert_eq!(
            db.edge_count(),
            1,
            "V7 store must have 1 edge after migrate"
        );
    }
    // On-disk snapshot is now V8; .bak holds the original V7 bytes.
    assert_eq!(
        core_api::snapshot_version_at(&dir).unwrap(),
        Some(core_storage::snapshot::VERSION),
        "snapshot must be rewritten to V8 after migration"
    );
    let bak = std::fs::read(dir.join("snapshot.bin.bak")).unwrap();
    assert_eq!(
        u16::from_le_bytes([bak[4], bak[5]]),
        7,
        ".bak must contain the original V7 bytes"
    );
    // Data must survive migration.
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(
        db.get_prop("a", "v"),
        Some(Value::Int(42)),
        "property v=42 on node 'a' must survive V7→V8 migration"
    );
    assert_eq!(
        db.neighbors("a", "E", Direction::Out).unwrap(),
        vec!["b"],
        "edge a→b must survive V7→V8 migration"
    );
    // Second clean open at V8 must remove .bak.
    assert!(
        !dir.join("snapshot.bin.bak").exists(),
        "second clean open at V8 must delete the .bak"
    );
}

/// A V8 store opens, migrates to V9 (one shared string table), leaves a .bak of
/// the original V8 bytes; a second clean open at V9 deletes the .bak.
///
/// V8 fixture content: 2 nodes ("a", "b"), 1 edge (E: a→b), prop v=42 on "a".
#[test]
fn v8_store_auto_migrates_to_v9_with_bak() {
    let dir = store_from_fixture("v8", include_bytes!("fixtures/golden_v8.bin"));
    {
        let db = GraphDb::open(&dir).unwrap();
        assert_eq!(
            db.node_count(),
            2,
            "V8 store must have 2 nodes after migrate"
        );
        assert_eq!(db.edge_count(), 1, "V8 store must have 1 edge after migrate");
    }
    // On-disk snapshot is now V9; .bak holds the original V8 bytes.
    assert_eq!(
        core_api::snapshot_version_at(&dir).unwrap(),
        Some(core_storage::snapshot::VERSION),
        "snapshot must be rewritten to V9 after migration"
    );
    let bak = std::fs::read(dir.join("snapshot.bin.bak")).unwrap();
    assert_eq!(
        u16::from_le_bytes([bak[4], bak[5]]),
        8,
        ".bak must contain the original V8 bytes"
    );
    // Data must survive the migration: the V8 per-column tables are read, and
    // the rewrite collapses them into the one shared section.
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(
        db.get_prop("a", "v"),
        Some(Value::Int(42)),
        "property v=42 on node 'a' must survive V8→V9 migration"
    );
    assert_eq!(
        db.neighbors("a", "E", Direction::Out).unwrap(),
        vec!["b"],
        "edge a→b must survive V8→V9 migration"
    );
    // Second clean open at V9 must remove .bak.
    assert!(
        !dir.join("snapshot.bin.bak").exists(),
        "second clean open at V9 must delete the .bak"
    );
}

/// A V8 store's *string* properties survive the migration and come back through
/// the shared table after the rewrite — the property-preservation half of the
/// V8→V9 auto-migrate, which the tiny golden fixture (v=42, an Int) cannot show.
#[test]
fn v8_string_props_survive_the_migration_to_v9() {
    // Build a V8 store by hand: snapshot with a V8-era encoder is gone, so take
    // the committed V8 fixture's container and write strings through the WAL,
    // then let the migrating open rewrite the whole store at V9.
    let dir = store_from_fixture("v8-strings", include_bytes!("fixtures/golden_v8.bin"));
    let words: Vec<String> = (0..64).map(|i| format!("word-{i:03}")).collect();
    {
        // auto_migrate=false so these land in the WAL over an untouched V8 base.
        let mut db = GraphDb::open_with_options(
            &dir,
            OpenOptions {
                auto_migrate: false,
                ..Default::default()
            },
        )
        .unwrap();
        for (n, w) in words.iter().enumerate() {
            db.insert_node(
                "N",
                &format!("s{n}"),
                (0..4)
                    .map(|f| (format!("f{f}"), Value::Str(w.clone())))
                    .collect(),
            )
            .unwrap();
        }
    }
    assert_eq!(
        core_api::snapshot_version_at(&dir).unwrap(),
        Some(8),
        "the store must still be V8 before the migrating open"
    );
    // Migrating open rewrites the snapshot at V9.
    drop(GraphDb::open(&dir).unwrap());
    assert_eq!(
        core_api::snapshot_version_at(&dir).unwrap(),
        Some(core_storage::snapshot::VERSION),
        "the migrating open must rewrite the snapshot at V9"
    );
    // Reopen and compare every property.
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(
        db.get_prop("a", "v"),
        Some(Value::Int(42)),
        "the V8 base's own property must survive"
    );
    for (n, w) in words.iter().enumerate() {
        for f in 0..4 {
            assert_eq!(
                db.get_prop(&format!("s{n}"), &format!("f{f}")),
                Some(Value::Str(w.clone())),
                "s{n}.f{f} must survive the V8→V9 rewrite"
            );
        }
    }
}
