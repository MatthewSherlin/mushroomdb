//! Reader isolation, pinned before the fold is changed.
//!
//! **These must pass against the code as it stands.** Today `fold_now` builds a
//! `FrozenOverlay` by deep-cloning eight owned structures, so isolation is a
//! consequence of the copy: a reader physically cannot see a later write because
//! it holds its own memory. That is the baseline these tests record.
//!
//! Task 14 replaces the copy with `Arc` + `Arc::make_mut`, so isolation stops
//! being a consequence of copying and becomes a consequence of copy-on-write.
//! The failure mode that introduces — a mutation reaching a structure a live
//! reader still points at — is invisible to the rest of the suite, which is why
//! these exist. A green suite would only prove no *existing* test noticed.
//!
//! If any of these fails **before** Task 14, stop: that is a pre-existing
//! isolation bug and a much larger finding than the fold.

use core_api::{SharedDb, FOLD_EVERY_K};
use core_storage::types::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mdb-reader-iso-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

/// Row count is the isolation signal: a snapshot that saw a later write would
/// answer with more rows. `ResultSet` exposes `len()` and keeps its rows private,
/// which is enough — these tests are about *whether* a write leaked, not which.
fn count(rs: &core_api::ResultSet) -> usize {
    rs.len()
}

const ALL: &str = "MATCH (n:N) RETURN key(n)";

/// A snapshot taken before a write never sees it, across every read surface.
#[test]
fn a_snapshot_taken_before_a_write_never_sees_it() {
    let dir = tmp("before-write");
    let db = SharedDb::open(&dir).expect("open");
    db.write().insert_node("N", "a", vec![]).expect("insert");
    db.write().insert_node("N", "b", vec![]).expect("insert");
    db.write().insert_edge("KNOWS", "a", "b").expect("edge");

    let snap = db.reader();
    let before_query = count(&snap.query(ALL, &BTreeMap::new()).expect("query"));
    let before_edges = snap.node_edges("a").expect("edges").len();
    assert_eq!(before_query, 2, "the snapshot starts at two nodes");

    // Write a lot: new nodes, new edges, and a property on an existing node.
    for i in 0..20 {
        db.write()
            .insert_node("N", &format!("later{i}"), vec![])
            .expect("insert");
    }
    db.write().insert_edge("KNOWS", "b", "a").expect("edge");
    db.write()
        .set_prop("a", "score", Value::Int(42))
        .expect("set_prop");

    // The snapshot is unmoved.
    assert_eq!(
        count(&snap.query(ALL, &BTreeMap::new()).expect("query")),
        before_query,
        "the snapshot saw nodes written after it was taken"
    );
    assert_eq!(
        snap.node_edges("a").expect("edges").len(),
        before_edges,
        "the snapshot saw an edge written after it was taken"
    );
    let info = snap.node_info("a").expect("a exists in the snapshot");
    assert!(
        !info.props.iter().any(|(k, _)| k.as_str() == "score"),
        "the snapshot saw a property set after it was taken: {:?}",
        info.props
    );
}

/// A fold triggered *under* a live snapshot must not disturb it.
///
/// This is the one that matters most after Task 14: the fold is exactly where
/// `Arc::make_mut` decides whether to copy, so a fold while a reader is alive is
/// the moment a writer could reach a reader's memory.
#[test]
fn a_snapshot_survives_a_fold_triggered_under_it() {
    let dir = tmp("fold-under");
    let db = SharedDb::open(&dir).expect("open");
    db.write().insert_node("N", "seed", vec![]).expect("insert");

    let snap = db.reader();
    let before = count(&snap.query(ALL, &BTreeMap::new()).expect("query"));
    assert_eq!(before, 1, "the snapshot starts at one node");

    // Drive past the fold threshold so `fold_now` runs while `snap` is alive.
    for i in 0..(FOLD_EVERY_K + 5) {
        db.write()
            .insert_node("N", &format!("n{i}"), vec![])
            .expect("insert");
    }

    assert_eq!(
        count(&snap.query(ALL, &BTreeMap::new()).expect("query")),
        before,
        "a fold under a live snapshot changed what it answers"
    );
}

/// Two snapshots across a write disagree in exactly the expected way, and
/// neither moves after the other is taken.
#[test]
fn two_snapshots_across_a_write_disagree_in_exactly_the_expected_way() {
    let dir = tmp("two-snaps");
    let db = SharedDb::open(&dir).expect("open");
    db.write().insert_node("N", "old", vec![]).expect("insert");

    let a = db.reader();
    db.write().insert_node("N", "new", vec![]).expect("insert");
    let b = db.reader();

    let a_keys = count(&a.query(ALL, &BTreeMap::new()).expect("query"));
    let b_keys = count(&b.query(ALL, &BTreeMap::new()).expect("query"));
    assert_eq!(a_keys, 1, "the older snapshot moved");
    assert_eq!(b_keys, 2, "the newer snapshot is wrong");

    // And taking `b` did not retroactively change `a`.
    assert_eq!(
        count(&a.query(ALL, &BTreeMap::new()).expect("query")),
        a_keys,
        "taking a second snapshot disturbed the first"
    );
}

/// A full-text write under a live snapshot leaves the snapshot's index alone.
///
/// `fulltext` is the structure with the most mutation sites, and unlike `props`
/// and `topo` it is fully materialised rather than an overlay over the mmap — so
/// it is the one `Arc::make_mut` will actually be asked to clone most often.
#[test]
fn a_fulltext_write_under_a_live_snapshot_leaves_its_index_alone() {
    let dir = tmp("fulltext-under");
    let db = SharedDb::open(&dir).expect("open");
    {
        let mut w = db.write();
        w.enable_fulltext("N", "body").expect("enable");
        w.insert_node("N", "d1", vec![("body".into(), Value::Str("alpha".into()))])
            .expect("insert");
    }

    let snap = db.reader();
    let before = count(&snap.query(ALL, &BTreeMap::new()).expect("query"));

    {
        let mut w = db.write();
        w.insert_node("N", "d2", vec![("body".into(), Value::Str("beta".into()))])
            .expect("insert");
        w.set_prop("d1", "body", Value::Str("gamma".into()))
            .expect("set_prop");
    }

    assert_eq!(
        count(&snap.query(ALL, &BTreeMap::new()).expect("query")),
        before,
        "a full-text write reached a live snapshot"
    );
    let info = snap.node_info("d1").expect("d1 in snapshot");
    let body = info
        .props
        .iter()
        .find(|(k, _)| k.as_str() == "body")
        .map(|(_, v)| v.clone());
    assert_eq!(
        body,
        Some(Value::Str("alpha".into())),
        "the snapshot saw a property overwritten after it was taken"
    );
}

/// Many snapshots held at once, each pinned to its own commit.
///
/// After Task 14 every one of these holds an `Arc` to a different generation, so
/// this is what forces `make_mut` to copy repeatedly rather than once.
#[test]
fn a_chain_of_snapshots_each_stay_at_their_own_commit() {
    let dir = tmp("chain");
    let db = SharedDb::open(&dir).expect("open");

    let mut snaps = Vec::new();
    let mut expected = Vec::new();
    for i in 0..12 {
        db.write()
            .insert_node("N", &format!("n{i}"), vec![])
            .expect("insert");
        snaps.push(db.reader());
        expected.push(i + 1);
    }
    // One more burst, past a fold, with all twelve snapshots alive.
    for i in 100..(100 + FOLD_EVERY_K) {
        db.write()
            .insert_node("N", &format!("n{i}"), vec![])
            .expect("insert");
    }

    for (idx, snap) in snaps.iter().enumerate() {
        let got = count(&snap.query(ALL, &BTreeMap::new()).expect("query"));
        assert_eq!(
            got, expected[idx],
            "snapshot {idx} should see {} nodes, saw {got}",
            expected[idx]
        );
    }
}
