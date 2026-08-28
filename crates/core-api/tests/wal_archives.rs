//! Tests for history-preserving WAL archives (Task 4: Temporal & Memory phase).
//!
//! Covers:
//! - Archive created with correct name (wal.<commit_seq>.archive)
//! - History APIs span across archive boundaries
//! - open_at reaches pre-snapshot commits through archives
//! - Retention prunes oldest archives and advances horizon floor
//! - Default path (archive_wal=false) leaves no archive files
//! - Archives not replayed into live state on normal open
//! - was_linked / edge_history at exact archive boundary commits
//! - Pruned-archive horizon honesty: CommitOutOfRange, not silently wrong
//! - Crash-window op-mode sweep: any crash point reopens cleanly

use core_api::{EdgeEvent, GraphDb, GraphError, HistoryChange, SnapshotOptions, Value};
use sim_harness::SimFs;

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "graphdb-wal-archives-{}-{}-{}",
        name,
        std::process::id(),
        nanos
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn opts_archive() -> SnapshotOptions {
    SnapshotOptions {
        archive_wal: true,
        ..SnapshotOptions::default()
    }
}

// ── Test 1: Archive file named by commit_seq ────────────────────────────────

/// After N WAL frames, snapshot_with(archive_wal=true) creates wal.N.archive.
/// The horizon floor stays at 0 (no pruning) and no extra archive files appear.
#[test]
fn archive_file_named_by_commit_seq() {
    let dir = tmp("name");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "a", vec![]).unwrap(); // commit 0 → commit_seq=1
        db.insert_node("N", "b", vec![]).unwrap(); // commit 1 → commit_seq=2
        db.insert_node("N", "c", vec![]).unwrap(); // commit 2 → commit_seq=3
        db.snapshot_with(opts_archive()).unwrap();
        // archive_n = commit_seq = 3
        assert_eq!(db.wal_horizon_floor(), 0, "no pruning yet: floor must be 0");
    }

    // On-disk: wal.3.archive must exist; no other wal.*.archive files.
    let archive_path = dir.join("wal.3.archive");
    assert!(
        archive_path.exists(),
        "expected wal.3.archive to exist after 3 WAL frames"
    );

    let extra_archives: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let n = e.file_name();
            let s = n.to_string_lossy();
            s.starts_with("wal.") && s.ends_with(".archive") && s != "wal.3.archive"
        })
        .collect();
    assert!(
        extra_archives.is_empty(),
        "unexpected extra archive files: {extra_archives:?}"
    );

    // Reopen: horizon floor must still be 0.
    let db2 = GraphDb::open(&dir).unwrap();
    assert_eq!(db2.wal_horizon_floor(), 0);
}

// ── Test 2: node_history spans two archive boundaries ────────────────────────

/// History APIs chain archives oldest-first then the live WAL.
/// A node inserted at commit 0 must appear in node_history even after two
/// archive snapshots that rotate the WAL.
#[test]
fn node_history_spans_two_archive_boundaries() {
    let dir = tmp("history2");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "a", vec![("v".into(), Value::Int(1))])
            .unwrap(); // global commit 0
        db.insert_node("N", "b", vec![]).unwrap(); // global commit 1
        db.snapshot_with(opts_archive()).unwrap(); // wal.2.archive

        db.insert_node("N", "c", vec![]).unwrap(); // global commit 2
        db.insert_node("N", "d", vec![]).unwrap(); // global commit 3
        db.snapshot_with(opts_archive()).unwrap(); // wal.4.archive

        db.insert_node("N", "e", vec![]).unwrap(); // global commit 4
        db.set_prop("a", "v", Value::Int(42)).unwrap(); // global commit 5

        // wal_total_commits: floor=0, 2+2 archive frames + 2 live frames = 6
        assert_eq!(
            db.wal_total_commits().unwrap(),
            6,
            "expected 6 total commits across two archives and live WAL"
        );

        // node_history for "a" must include: NodeInserted (commit 0) + PropSet (commit 5)
        let hist = db.node_history("a").unwrap();
        assert!(
            hist.len() >= 2,
            "expected at least 2 history entries for 'a', got {:?}",
            hist
        );
        let commits: Vec<u64> = hist.iter().map(|e| e.commit).collect();
        assert!(
            commits.contains(&0),
            "NodeInserted at commit 0 missing from history: {commits:?}"
        );
        assert!(
            commits.contains(&5),
            "PropSet at commit 5 missing from history: {commits:?}"
        );
        assert!(
            matches!(&hist[0].change, HistoryChange::NodeInserted { .. }),
            "first entry must be NodeInserted, got {:?}",
            hist[0].change
        );
    }
}

// ── Test 3: open_at reaches pre-snapshot commits through archive ─────────────

/// open_at(commit_in_archive) replays from empty state through archive frames;
/// it must not see nodes inserted after the requested commit.
#[test]
fn open_at_pre_snapshot_commit_through_archive() {
    let dir = tmp("open-at-arc");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "a", vec![]).unwrap(); // commit 0
        db.insert_node("N", "b", vec![]).unwrap(); // commit 1
        db.insert_node("N", "c", vec![]).unwrap(); // commit 2
        db.snapshot_with(opts_archive()).unwrap(); // wal.3.archive (3 archive frames)
        db.insert_node("N", "d", vec![]).unwrap(); // commit 3 (live WAL)
    }

    // open_at(1): replay first 2 archive frames → only a, b visible.
    let db1 = GraphDb::open_at(&dir, 1).unwrap();
    assert!(db1.has_node("a"), "commit 1: 'a' must be visible");
    assert!(db1.has_node("b"), "commit 1: 'b' must be visible");
    assert!(!db1.has_node("c"), "commit 1: 'c' not yet inserted");
    assert!(!db1.has_node("d"), "commit 1: 'd' not yet inserted");

    // open_at(3): commit 3 is the first live WAL frame; load snapshot + replay 1 frame.
    let db3 = GraphDb::open_at(&dir, 3).unwrap();
    assert!(db3.has_node("a"));
    assert!(db3.has_node("b"));
    assert!(db3.has_node("c"));
    assert!(db3.has_node("d"), "commit 3: 'd' must be visible");
}

// ── Test 4: Retention prunes oldest archives ─────────────────────────────────

/// With retention=2, taking a 3rd archive snapshot must delete the oldest archive
/// and advance wal_horizon_floor by the pruned frame count.
#[test]
fn retention_prunes_oldest_archives() {
    let dir = tmp("retention");
    let mut db = GraphDb::open(&dir).unwrap();
    db.set_wal_archive_retention(Some(2));

    // Round 1: 1 insert → archive (1 frame).
    db.insert_node("N", "a", vec![]).unwrap(); // commit 0, commit_seq=1
    db.snapshot_with(opts_archive()).unwrap(); // wal.1.archive; 1 archive total

    // Round 2: 1 insert → archive (1 frame).
    db.insert_node("N", "b", vec![]).unwrap(); // commit 1, commit_seq=2
    db.snapshot_with(opts_archive()).unwrap(); // wal.2.archive; 2 archives, no prune

    assert_eq!(db.wal_horizon_floor(), 0, "no pruning yet after 2 archives");

    // Round 3: 1 insert → archive (1 frame); now 3 > retention=2, prune oldest.
    db.insert_node("N", "c", vec![]).unwrap(); // commit 2, commit_seq=3
    db.snapshot_with(opts_archive()).unwrap(); // wal.3.archive; prune wal.1.archive

    // wal.1.archive (1 frame) pruned → floor advances by 1.
    assert_eq!(
        db.wal_horizon_floor(),
        1,
        "horizon floor must advance by 1 pruned frame"
    );

    // Only wal.2.archive and wal.3.archive remain on disk.
    let archive_files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let n = e.file_name();
            n.to_string_lossy().ends_with(".archive")
        })
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        !archive_files.iter().any(|n| n == "wal.1.archive"),
        "wal.1.archive must be deleted after pruning"
    );
    assert!(
        archive_files.iter().any(|n| n == "wal.2.archive"),
        "wal.2.archive must be retained"
    );
    assert!(
        archive_files.iter().any(|n| n == "wal.3.archive"),
        "wal.3.archive must be retained"
    );

    // wal_total_commits: floor=1, 2 surviving archive frames + 0 live = 3.
    assert_eq!(
        db.wal_total_commits().unwrap(),
        3,
        "total commits = floor + surviving frames"
    );

    // Reopen: floor must be persisted.
    let db2 = GraphDb::open(&dir).unwrap();
    assert_eq!(
        db2.wal_horizon_floor(),
        1,
        "horizon floor must survive reopen"
    );
}

// ── Test 5: Default path unchanged when archive_wal=false ───────────────────

/// snapshot_with(archive_wal=false) must not create any archive files.
/// Behavior is byte-identical to the default snapshot().
#[test]
fn default_path_no_archives_when_archive_wal_false() {
    let dir = tmp("no-archive");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "a", vec![]).unwrap();
        db.insert_node("N", "b", vec![]).unwrap();
        db.snapshot_with(SnapshotOptions {
            archive_wal: false,
            ..SnapshotOptions::default()
        })
        .unwrap();
    }

    // No wal.*.archive files must exist.
    let archive_files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".archive"))
        .collect();
    assert!(
        archive_files.is_empty(),
        "archive_wal=false must not create archive files: {archive_files:?}"
    );

    // No wal.floor sidecar.
    assert!(
        !dir.join("wal.floor").exists(),
        "archive_wal=false must not create wal.floor"
    );

    // Reopen sees the correct node count (snapshot + empty WAL replay).
    let db2 = GraphDb::open(&dir).unwrap();
    assert_eq!(db2.node_count(), 2);
}

// ── Test 6: Archives excluded from live state on normal open ─────────────────

/// After snapshot_with(archive_wal=true), a fresh GraphDb::open() loads
/// the snapshot + replays only the live WAL. Archives are NOT part of
/// the live replay path (they are pre-snapshot by construction).
#[test]
fn archives_not_replayed_into_live_state() {
    let dir = tmp("no-live-replay");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "a", vec![]).unwrap(); // commit 0
        db.insert_node("N", "b", vec![]).unwrap(); // commit 1
        db.insert_node("N", "c", vec![]).unwrap(); // commit 2
        db.snapshot_with(opts_archive()).unwrap(); // wal.3.archive
                                                   // Post-archive live commit:
        db.insert_node("N", "d", vec![]).unwrap(); // commit 3
    }

    // Fresh open: snapshot.bin carries a,b,c; live WAL has d.
    // Archives are on disk but must NOT be replayed into live state.
    let db2 = GraphDb::open(&dir).unwrap();
    assert_eq!(
        db2.node_count(),
        4,
        "all 4 nodes must be present via snapshot+live WAL"
    );
    assert!(db2.has_node("a") && db2.has_node("b") && db2.has_node("c") && db2.has_node("d"));
    // wal_total_commits reflects 3 archive frames + 1 live frame
    assert_eq!(db2.wal_total_commits().unwrap(), 4);
    assert_eq!(db2.wal_horizon_floor(), 0);
}

// ── Test 7: was_linked at archive boundary ───────────────────────────────────

/// An edge inserted at the last commit before an archive snapshot must be
/// visible via was_linked at that commit (which is in the archive) and
/// must also be visible at the first commit of the live WAL.
#[test]
fn was_linked_at_archive_boundary_commit() {
    let dir = tmp("was-linked-boundary");
    let mut db = GraphDb::open(&dir).unwrap();

    db.insert_node("N", "a", vec![]).unwrap(); // commit 0
    db.insert_node("N", "b", vec![]).unwrap(); // commit 1
    db.insert_edge("Knows", "a", "b").unwrap(); // commit 2 ← edge created
    db.snapshot_with(opts_archive()).unwrap(); // wal.3.archive (commits 0,1,2)
    db.set_prop("a", "x", Value::Int(1)).unwrap(); // commit 3 (first live WAL frame)

    // Commit 2 is the last archive frame: edge must be present.
    assert!(
        db.was_linked("a", "b", "Knows", 2).unwrap(),
        "edge must be linked at archive-boundary commit 2"
    );

    // Commit 3 is the first live WAL frame: edge still exists (not deleted).
    assert!(
        db.was_linked("a", "b", "Knows", 3).unwrap(),
        "edge must still be linked at first live commit (commit 3)"
    );

    // Commit 1 (before edge): false.
    assert!(
        !db.was_linked("a", "b", "Knows", 1).unwrap(),
        "edge must not be linked before its insertion commit"
    );
}

// ── Test 8: edge_history spans archive boundary ──────────────────────────────

/// edge_history must return events that cross archive boundaries.
/// An edge inserted in an archive frame and then deleted in the live WAL
/// must appear in edge_history with both Added and Removed events.
#[test]
fn edge_history_spans_archive_boundary() {
    let dir = tmp("edge-hist-boundary");
    let mut db = GraphDb::open(&dir).unwrap();

    db.insert_node("N", "a", vec![]).unwrap(); // commit 0
    db.insert_node("N", "b", vec![]).unwrap(); // commit 1
    db.insert_edge("Knows", "a", "b").unwrap(); // commit 2 ← Added (in archive)
    db.snapshot_with(opts_archive()).unwrap(); // wal.3.archive
    db.delete_edge("Knows", "a", "b").unwrap(); // commit 3 ← Removed (in live WAL)

    let result = db.edge_history("a", "b").unwrap();
    assert_eq!(
        result.items.len(),
        2,
        "expected Added + Removed across archive boundary: {:?}",
        result.items
    );
    assert_eq!(
        result.items[0].event,
        EdgeEvent::Added,
        "first event must be Added (commit 2, in archive)"
    );
    assert_eq!(
        result.items[1].event,
        EdgeEvent::Retracted,
        "second event must be Retracted (commit 3, in live WAL)"
    );
    // Commits must be strictly ordered and span the boundary.
    assert!(
        result.items[0].commit < result.items[1].commit,
        "commits must be strictly increasing"
    );
}

// ── Test 9: open_at first commit of live WAL ─────────────────────────────────

/// The first commit in the live WAL (exactly at total_archive_frames) must be
/// reachable via open_at; it loads the snapshot and replays that single frame.
#[test]
fn open_at_first_live_wal_commit() {
    let dir = tmp("live-boundary");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "a", vec![]).unwrap(); // commit 0
        db.insert_node("N", "b", vec![]).unwrap(); // commit 1
        db.snapshot_with(opts_archive()).unwrap(); // wal.2.archive (2 frames)
        db.insert_node("N", "c", vec![]).unwrap(); // commit 2 ← first live frame
        db.insert_node("N", "d", vec![]).unwrap(); // commit 3
    }

    // commit 2 is the first live WAL commit; open_at should give a,b,c but not d.
    let db2 = GraphDb::open_at(&dir, 2).unwrap();
    assert!(db2.has_node("a"), "commit 2: 'a' from snapshot");
    assert!(db2.has_node("b"), "commit 2: 'b' from snapshot");
    assert!(db2.has_node("c"), "commit 2: 'c' from first live WAL frame");
    assert!(!db2.has_node("d"), "commit 2: 'd' not yet inserted");

    // commit 3: all four nodes.
    let db3 = GraphDb::open_at(&dir, 3).unwrap();
    assert!(db3.has_node("d"), "commit 3: 'd' must be visible");
}

// ── Test 10: Pruned-archive horizon honesty ──────────────────────────────────

/// Pins the full reachability contract:
///
/// (a) Commits below wal_horizon_floor → CommitOutOfRange (pruned; gone).
/// (b) Surviving-archive commits when floor > 0 or genesis chain is broken →
///     open_at returns CommitOutOfRange (prefix needed for reconstruction is
///     absent; refuse rather than return wrong state).
/// (c) Surviving-archive commits → was_linked still works (scan-based, no
///     state reconstruction) and returns an honest answer over surviving data.
/// (d) Live WAL commits → always reachable via open_at (snapshot is their base).
#[test]
fn pruned_archive_horizon_honesty() {
    let dir = tmp("horizon-honesty");
    let mut db = GraphDb::open(&dir).unwrap();
    db.set_wal_archive_retention(Some(1)); // keep only 1 newest archive

    // First archive: 3 frames.  Genesis marker written (no prior truncation).
    db.insert_node("N", "a", vec![]).unwrap(); // commit 0
    db.insert_node("N", "b", vec![]).unwrap(); // commit 1
    db.insert_edge("Knows", "a", "b").unwrap(); // commit 2
    db.snapshot_with(opts_archive()).unwrap(); // wal.3.archive (genesis), floor=0

    // Second archive: 1 frame.  Prunes wal.3.archive → floor=3, genesis=false.
    db.insert_node("N", "c", vec![]).unwrap(); // commit 3
    db.snapshot_with(opts_archive()).unwrap(); // wal.4.archive; prune wal.3

    assert_eq!(
        db.wal_horizon_floor(),
        3,
        "floor must be 3 after pruning 3 frames"
    );

    // One live-WAL commit so we can verify the live-WAL reachable path.
    db.insert_node("N", "d", vec![]).unwrap(); // commit 4 (live WAL)

    // ── (a) Pruned commits: was_linked → CommitOutOfRange ──────────────────
    let total = db.wal_total_commits().unwrap();
    for pruned in [0u64, 1, 2] {
        match db.was_linked("a", "b", "Knows", pruned) {
            Err(GraphError::CommitOutOfRange { commit, .. }) if commit == pruned => {}
            other => panic!(
                "expected CommitOutOfRange for pruned commit {pruned} (total={total}), \
                 got {other:?}"
            ),
        }
    }

    // ── (b) Surviving-archive commit (global 3): open_at → CommitOutOfRange ─
    // floor > 0, genesis chain broken by pruning → open_at must refuse.
    match GraphDb::open_at(&dir, 3).err() {
        Some(GraphError::CommitOutOfRange { commit: 3, .. }) => {}
        other => panic!(
            "expected CommitOutOfRange for open_at(3) (surviving archive, pruned prefix), \
             got {other:?}"
        ),
    }

    // ── (c) Surviving-archive commit: was_linked still works (scan-based) ───
    // wal.4.archive contains only the insert of "c"; the edge Knows(a,b) is in
    // the pruned archive.  was_linked scans surviving data and honestly returns
    // false — it does not error.
    match db.was_linked("a", "b", "Knows", 3) {
        Ok(false) => {} // honest: edge not in surviving records
        Err(GraphError::CommitOutOfRange { .. }) => {
            panic!("was_linked must not return CommitOutOfRange for a surviving commit (commit 3 >= floor 3)")
        }
        other => panic!("unexpected result for was_linked at surviving commit 3: {other:?}"),
    }

    // ── (d) Live WAL commit (global 4): open_at succeeds ────────────────────
    // Live WAL commits are always reachable: snapshot is their reconstruction base.
    let db4 = GraphDb::open_at(&dir, 4).unwrap();
    // Snapshot (taken at 2nd archive) captured a, b, c.  Live WAL adds d.
    assert!(db4.has_node("a") && db4.has_node("b") && db4.has_node("c"));
    assert!(db4.has_node("d"), "live WAL commit 4: 'd' must be visible");

    // Pruned commits via open_at also give CommitOutOfRange.
    for pruned in [0u64, 1, 2] {
        match GraphDb::open_at(&dir, pruned).err() {
            Some(GraphError::CommitOutOfRange { commit, .. }) if commit == pruned => {}
            other => panic!("open_at({pruned}) expected CommitOutOfRange (pruned), got {other:?}"),
        }
    }
}

// ── Test 12: Genesis-chain open_at equivalence ───────────────────────────────

/// For a fresh store with no prior truncating snapshot, `archive_genesis_chain`
/// is set and open_at must return CORRECT states at commits in BOTH archives
/// and in the live WAL.
///
/// Verified by building a reference GraphDb with the same commit history and
/// comparing node membership at each open_at target.
#[test]
fn genesis_chain_open_at_equivalence() {
    let dir = tmp("genesis-equiv");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        // No prior truncating snapshot → genesis chain.
        db.insert_node("N", "r0", vec![("v".into(), Value::Int(0))])
            .unwrap(); // commit 0
        db.insert_node("N", "r1", vec![]).unwrap(); // commit 1
        db.snapshot_with(opts_archive()).unwrap(); // wal.2.archive (commits 0,1)

        db.insert_node("N", "r2", vec![]).unwrap(); // commit 2
        db.snapshot_with(opts_archive()).unwrap(); // wal.3.archive (commit 2)

        db.insert_node("N", "r3", vec![]).unwrap(); // commit 3 (live WAL)
    }

    // Genesis chain must be intact: floor=0, no pruning.
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.wal_horizon_floor(), 0);
    assert_eq!(db.wal_total_commits().unwrap(), 4);

    // Commit 0 (first archive, first frame): only r0.
    let db0 = GraphDb::open_at(&dir, 0).unwrap();
    assert!(db0.has_node("r0"), "commit 0: r0 must be visible");
    assert!(!db0.has_node("r1"), "commit 0: r1 not yet inserted");
    assert!(!db0.has_node("r2"), "commit 0: r2 not yet inserted");
    assert!(!db0.has_node("r3"), "commit 0: r3 not yet inserted");
    assert_eq!(db0.get_prop("r0", "v"), Some(Value::Int(0)));

    // Commit 1 (first archive, second frame): r0 and r1.
    let db1 = GraphDb::open_at(&dir, 1).unwrap();
    assert!(db1.has_node("r0") && db1.has_node("r1"));
    assert!(!db1.has_node("r2") && !db1.has_node("r3"));

    // Commit 2 (second archive): r0, r1, r2.
    let db2 = GraphDb::open_at(&dir, 2).unwrap();
    assert!(db2.has_node("r0") && db2.has_node("r1") && db2.has_node("r2"));
    assert!(!db2.has_node("r3"));

    // Commit 3 (live WAL): all four nodes.
    let db3 = GraphDb::open_at(&dir, 3).unwrap();
    assert!(db3.has_node("r0") && db3.has_node("r1") && db3.has_node("r2") && db3.has_node("r3"));

    // Out of range → CommitOutOfRange.
    assert!(GraphDb::open_at(&dir, 4).is_err());
}

// ── Test 11: Crash-window op-mode sweep ──────────────────────────────────────

/// Sweep all SimFs op crash points through a workload that includes a
/// snapshot_with(archive_wal=true). Every crash survivor must reopen
/// without error.
#[test]
fn crash_window_archive_op_sweep() {
    let total_ops = {
        let mut db = GraphDb::open_with(SimFs::new()).unwrap();
        archive_workload(&mut db).unwrap();
        db.into_fs().total_ops()
    };
    assert!(total_ops > 0, "workload must perform at least one Fs op");

    for crash_op in 0..=total_ops {
        let survivor = match GraphDb::open_with(SimFs::with_crash_after_ops(crash_op)) {
            Ok(mut db) => {
                let _ = archive_workload(&mut db);
                db.into_fs().surviving_state()
            }
            Err(_) => {
                // Crashed during initial reads (before any write); empty state.
                SimFs::new()
            }
        };

        // Invariant: reopen after any crash never panics or errors.
        GraphDb::open_with(survivor).unwrap_or_else(|e| {
            panic!("crash_op={crash_op}: reopen after crash failed: {e}");
        });
    }
}

// ── Test 13: Cross-session truncation detected via wal.truncated ─────────────

/// Session 1 takes a WAL-truncating snapshot (keep_wal=false), which writes
/// the persistent `wal.truncated` marker.  Session 2 reopens, loads the marker
/// (wal_ever_truncated=true), and takes the first archive.  The cross-session
/// truncation must be detected: genesis marker must NOT be written, and
/// archive-resident commits must return CommitOutOfRange.  History scans still work.
#[test]
fn cross_session_truncate_then_archive() {
    let dir = tmp("cross-session");

    // Session 1: write 3 nodes, take WAL-truncating (keep_wal=false) snapshot.
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "a", vec![]).unwrap(); // commit 0
        db.insert_node("N", "b", vec![]).unwrap(); // commit 1
        db.insert_node("N", "c", vec![]).unwrap(); // commit 2
                                                   // Default opts: keep_wal=false, archive_wal=false → truncating snapshot.
        db.snapshot_with(SnapshotOptions::default()).unwrap();
    }

    // wal.truncated must be on disk after session 1.
    assert!(
        dir.join("wal.truncated").exists(),
        "wal.truncated must be written by a keep_wal=false snapshot"
    );

    // Session 2: reopen (wal_ever_truncated loaded from wal.truncated), write 1
    // node, take the first archive.
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "d", vec![]).unwrap(); // commit 0 in new WAL epoch
        db.snapshot_with(opts_archive()).unwrap(); // first archive

        // wal.truncated must still be present (write-once, never deleted).
        assert!(
            dir.join("wal.truncated").exists(),
            "wal.truncated must persist after the archive snapshot"
        );
        // Genesis marker must NOT be present: cross-session truncation detected.
        assert!(
            !dir.join("wal.genesis").exists(),
            "wal.genesis must NOT be written when prior truncation is detected via wal.truncated"
        );
    }

    // Archive-resident commit at global index 0 (floor=0, local=0 < archive frames):
    // without genesis marker, open_at must return CommitOutOfRange, not silently wrong state.
    match GraphDb::open_at(&dir, 0).err() {
        Some(GraphError::CommitOutOfRange { .. }) => {}
        other => panic!(
            "expected CommitOutOfRange for archive-resident commit after cross-session \
             truncation, got {other:?}"
        ),
    }

    // History scan via node_history: "d" was inserted in the archived WAL slice
    // (frame record present) → must appear in node_history despite no genesis chain.
    let db = GraphDb::open(&dir).unwrap();
    let hist = db.node_history("d").unwrap();
    assert!(
        !hist.is_empty(),
        "node 'd' inserted in archived WAL must appear in node_history (scan-based, not state-replay)"
    );
}

// ── Test 14: Legacy-store conservative no-genesis rule ───────────────────────

/// A store with snapshot.bin but no wal.truncated simulates a legacy store
/// (predating the wal.truncated feature) that may or may not have been truncated.
/// The conservative rule: if snapshot.bin exists before the first archive but
/// wal.truncated is absent, do NOT write the genesis marker.  Correctness is
/// preserved; the only cost is that open_at cannot reach archive-resident commits.
#[test]
fn legacy_store_conservative_no_genesis() {
    let dir = tmp("legacy-conservative");

    // Simulate legacy store: take a keep_wal=true snapshot.  This writes
    // snapshot.bin but NOT wal.truncated — identical to what an old code version
    // would leave behind for a store that had any prior snapshot.
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "a", vec![]).unwrap(); // commit 0
        db.insert_node("N", "b", vec![]).unwrap(); // commit 1
        db.snapshot_with(SnapshotOptions {
            keep_wal: true,
            archive_wal: false,
        })
        .unwrap();
    }

    // Verify legacy-store precondition: snapshot.bin present, wal.truncated absent.
    assert!(dir.join("snapshot.bin").exists(), "snapshot.bin must exist");
    assert!(
        !dir.join("wal.truncated").exists(),
        "wal.truncated must NOT exist (simulating legacy store)"
    );

    // Second session: reopen and take the first archive.
    // had_prior_snapshot=true (snapshot.bin exists from session 1),
    // wal_ever_truncated=false (no wal.truncated on disk).
    // Conservative rule: refuse genesis marker.
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "c", vec![]).unwrap(); // commit 2
        db.snapshot_with(opts_archive()).unwrap(); // first archive
    }

    // Genesis marker must NOT be written.
    assert!(
        !dir.join("wal.genesis").exists(),
        "wal.genesis must NOT be written: snapshot.bin existed without wal.truncated \
         (conservative legacy-store rule)"
    );

    // open_at for an archive-resident commit must return CommitOutOfRange.
    // (Archives cover commits 0,1,2; without genesis marker, open_at refuses.)
    match GraphDb::open_at(&dir, 1).err() {
        Some(GraphError::CommitOutOfRange { .. }) => {}
        other => panic!(
            "expected CommitOutOfRange for archive commit without genesis chain \
             (legacy store conservative rule), got {other:?}"
        ),
    }
}

// ── Test 15: Cross-session archive names are monotonic ───────────────────────

/// Verifies that archive names are globally monotonic across sessions, even
/// when commit_seq is under-seeded from last_change on reopen.
///
/// Session 1: insert "a" (last_change[a]=1), insert "b" (last_change[b]=2),
/// enable_fulltext (seq=3, no last_change update), disable_fulltext (seq=4,
/// no last_change update), archive.  Baseline WAL is empty (fulltext disabled).
///
/// Session 2: reopens with commit_seq seeded from max(last_change)=2 plus 0
/// baseline frames → commit_seq=2.  Inserts "c" → commit_seq=3.  Under the OLD
/// naming (archive_n = commit_seq), this produces wal.3.archive, which sorts
/// BEFORE session-1's archive — wrong temporal order.  Under the NEW naming
/// (last_archive_n + live_frame_count), it produces a name strictly greater
/// than session-1's archive.
#[test]
fn cross_session_archive_names_monotonic() {
    let dir = tmp("monotonic");

    // Session 1: 4 WAL frames, only 2 update last_change.
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "a", vec![]).unwrap(); // commit 0, last_change[a]=1
        db.insert_node("N", "b", vec![]).unwrap(); // commit 1, last_change[b]=2
        db.enable_fulltext("N", "tag").unwrap(); // commit 2 — no last_change update
        db.disable_fulltext("N", "tag").unwrap(); // commit 3 — no last_change update
                                                  // 4 live frames; archive_n_new = 0 + 4 = 4.
                                                  // Baseline WAL: empty (fulltext disabled).
        db.snapshot_with(opts_archive()).unwrap();
    }

    let s1_n: u64 = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".archive"))
        .filter_map(|e| {
            let n = e.file_name();
            let s = n.to_string_lossy();
            s.strip_prefix("wal.")
                .and_then(|r| r.strip_suffix(".archive"))
                .and_then(|n| n.parse().ok())
        })
        .next()
        .expect("session 1 must produce 1 archive");

    // Session 2: commit_seq seeded from max(last_change)=2, 0 baseline frames.
    // OLD scheme would produce archive_n = 3 (< s1_n); NEW scheme gives s1_n+1.
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "c", vec![]).unwrap(); // 1 live frame
        db.snapshot_with(opts_archive()).unwrap();
    }

    let mut archive_ns: Vec<u64> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".archive"))
        .filter_map(|e| {
            let n = e.file_name();
            let s = n.to_string_lossy();
            s.strip_prefix("wal.")
                .and_then(|r| r.strip_suffix(".archive"))
                .and_then(|n| n.parse().ok())
        })
        .collect();
    archive_ns.sort_unstable();
    assert_eq!(archive_ns.len(), 2, "expected 2 archives: {archive_ns:?}");
    assert!(
        archive_ns[0] < archive_ns[1],
        "archive names must be strictly increasing across sessions: {archive_ns:?}"
    );
    assert_eq!(archive_ns[0], s1_n, "first archive must be session-1's");

    // History scan: all 3 nodes appear in correct temporal order.
    let db = GraphDb::open(&dir).unwrap();
    let ha = db.node_history("a").unwrap();
    let hb = db.node_history("b").unwrap();
    let hc = db.node_history("c").unwrap();
    assert!(!ha.is_empty() && !hb.is_empty() && !hc.is_empty());
    assert!(
        ha[0].commit < hb[0].commit && hb[0].commit < hc[0].commit,
        "history commits must be in temporal order: a={} b={} c={}",
        ha[0].commit,
        hb[0].commit,
        hc[0].commit
    );
}

// ── Test 16: Crash-window op-mode sweep through prune sequence ────────────────

/// Sweeps all SimFs op crash points through a workload that includes
/// retention-based archive pruning.  Every crash survivor must reopen without
/// error (C1: floor written first before deletes), and the floor/total
/// invariant must hold.  Commits below the floor always yield CommitOutOfRange.
#[test]
fn crash_window_prune_op_sweep() {
    let total_ops = {
        let mut db = GraphDb::open_with(SimFs::new()).unwrap();
        archive_prune_workload(&mut db).unwrap();
        db.into_fs().total_ops()
    };
    assert!(
        total_ops > 0,
        "prune workload must perform at least one Fs op"
    );

    for crash_op in 0..=total_ops {
        let survivor = match GraphDb::open_with(SimFs::with_crash_after_ops(crash_op)) {
            Ok(mut db) => {
                let _ = archive_prune_workload(&mut db);
                db.into_fs().surviving_state()
            }
            Err(_) => SimFs::new(),
        };

        let db = GraphDb::open_with(survivor).unwrap_or_else(|e| {
            panic!("crash_op={crash_op}: reopen after crash failed: {e}");
        });

        // Invariant: floor ≤ total_commits.
        let floor = db.wal_horizon_floor();
        let total = db.wal_total_commits().unwrap_or_else(|e| {
            panic!("crash_op={crash_op}: wal_total_commits failed: {e}");
        });
        assert!(
            floor <= total,
            "crash_op={crash_op}: floor ({floor}) > total_commits ({total})"
        );

        // Invariant: commits below floor must yield CommitOutOfRange (never
        // silently-wrong state).  was_linked checks floor before any node
        // lookup, so this works even for non-existent node pairs.
        for pruned in 0..floor {
            match db.was_linked("x", "y", "E", pruned) {
                Err(GraphError::CommitOutOfRange { .. }) => {}
                other => panic!(
                    "crash_op={crash_op}: was_linked at pruned commit {pruned} \
                     expected CommitOutOfRange, got {other:?}"
                ),
            }
        }
    }
}

/// Minimal workload that exercises the archive_wal snapshot path.
fn archive_workload<F: core_storage::fs::Fs>(db: &mut GraphDb<F>) -> core_api::Result<()> {
    db.insert_node("N", "x", vec![])?;
    db.insert_node("N", "y", vec![])?;
    db.insert_edge("E", "x", "y")?;
    db.snapshot_with(SnapshotOptions {
        archive_wal: true,
        ..SnapshotOptions::default()
    })?;
    db.insert_node("N", "z", vec![])?;
    Ok(())
}

/// Workload that exercises the retention-prune path (two archives with
/// retention=1, causing the first to be pruned when the second is taken).
fn archive_prune_workload<F: core_storage::fs::Fs>(db: &mut GraphDb<F>) -> core_api::Result<()> {
    db.set_wal_archive_retention(Some(1));
    // First archive: 2 frames.
    db.insert_node("N", "x", vec![])?; // commit 0
    db.insert_node("N", "y", vec![])?; // commit 1
    db.snapshot_with(SnapshotOptions {
        archive_wal: true,
        ..SnapshotOptions::default()
    })?;
    // Second archive: 1 frame.  Prunes first archive → floor advances by 2.
    db.insert_node("N", "z", vec![])?; // commit 2
    db.snapshot_with(SnapshotOptions {
        archive_wal: true,
        ..SnapshotOptions::default()
    })?;
    Ok(())
}
