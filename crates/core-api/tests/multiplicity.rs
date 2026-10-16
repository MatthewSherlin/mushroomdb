//! Insert-count multiplicity — opt-in per store (v0.6.10 §5.13, §7 D8).
//!
//! Adjacency stays a set. A second `insert_edge` of the same triple still
//! returns `Ok(false)` and still leaves the unique degree alone; what changes,
//! **and only on a store whose operator asked for it**, is that the triple's
//! insert count is recorded as the reserved edge property `count` and written
//! durably as WAL discriminant 23.
//!
//! The opt-in is the whole design. A reader meeting an unknown WAL discriminant
//! cannot know what the record would have changed, so a store that has written
//! one is no longer readable by a binary that predates it. Gating discriminant
//! 23 behind `enable_multiplicity()` means that step is taken by an operator who
//! asked for the feature, not by everyone who took a patch release —
//! `multiplicity_off_writes_no_new_record` is what holds that line.

use core_storage::{decode_all, WalRecord};
use mushroomdb::{AlgoDir, GraphDb, GraphError, NodeMask, RealFs, Value};
use serde::Deserialize;

type Db = GraphDb<RealFs>;

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "graphdb-multiplicity-{}-{}-{}",
        name,
        std::process::id(),
        nanos
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// `a → b`, inserted once, on a store that has opted in.
fn pair(name: &str) -> (std::path::PathBuf, Db) {
    let dir = tmp(name);
    let mut db = GraphDb::open(&dir).unwrap();
    db.enable_multiplicity().unwrap();
    for k in ["a", "b"] {
        db.insert_node("N", k, vec![]).unwrap();
    }
    assert!(db.insert_edge("E", "a", "b").unwrap());
    (dir, db)
}

fn wal_bytes(dir: &std::path::Path) -> Vec<u8> {
    std::fs::read(dir.join("wal.bin")).unwrap()
}

/// Every record in the WAL, `Batch` frames flattened.
fn wal_records(dir: &std::path::Path) -> Vec<WalRecord> {
    let (frames, valid_len) = decode_all(&wal_bytes(dir));
    assert_eq!(
        valid_len,
        wal_bytes(dir).len(),
        "the WAL must decode to its end"
    );
    let mut out = Vec::new();
    for f in frames {
        match f {
            WalRecord::Batch(inner) => out.extend(inner),
            other => out.push(other),
        }
    }
    out
}

fn count_records(dir: &std::path::Path) -> Vec<WalRecord> {
    wal_records(dir)
        .into_iter()
        .filter(|r| matches!(r, WalRecord::SetEdgeCount { .. }))
        .collect()
}

// ── A decoder that knows only discriminants 0–22 ─────────────────────────────
//
// This mirrors `WalRecord` as v0.6.9 shipped it: the same variants in the same
// order, and nothing after `DisableIndex`. bincode encodes an enum as a
// positional discriminant, so deserialising a v0.6.10 WAL into this type is
// exactly what a v0.6.9 binary does with the same bytes.

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
enum LegacyWalRecord {
    InsertNode {
        label: String,
        key: String,
        props: Vec<(String, Value)>,
    },
    InsertEdge {
        edge_type: String,
        src_key: String,
        dst_key: String,
    },
    SetProp {
        key: String,
        field: String,
        value: Value,
    },
    CreateRule {
        def_bytes: Vec<u8>,
    },
    DeleteRule {
        name: String,
    },
    RemoveProp {
        key: String,
        field: String,
    },
    DeleteEdge {
        edge_type: String,
        src_key: String,
        dst_key: String,
    },
    DeleteNode {
        key: String,
    },
    Batch(Vec<LegacyWalRecord>),
    RebuildRule {
        name: String,
    },
    CreateView {
        def_bytes: Vec<u8>,
    },
    DeleteView {
        name: String,
    },
    EnableFulltext {
        label: String,
        field: String,
    },
    DisableFulltext {
        label: String,
        field: String,
    },
    InsertNodeId {
        label: u32,
        key: String,
        props: Vec<(u32, Value)>,
    },
    SetPropId {
        id: u32,
        field: u32,
        value: Value,
    },
    InsertEdgeId {
        etype: u32,
        src: u32,
        dst: u32,
    },
    Intern {
        id: u32,
        text: String,
    },
    DerivedEdgeAdded {
        rule: String,
        edge_type: String,
        src_key: String,
        dst_key: String,
    },
    DerivedEdgeRetracted {
        rule: String,
        edge_type: String,
        src_key: String,
        dst_key: String,
    },
    RenameNode {
        old_key: String,
        new_key: String,
    },
    EnableIndex {
        label: String,
        field: String,
    },
    DisableIndex {
        label: String,
        field: String,
    },
}

/// Decode framed WAL bytes with the 0–22 decoder, stopping where it stops.
/// Returns `Ok(n_frames)` when every frame decoded, `Err(message)` at the first
/// frame it cannot read.
fn legacy_decode(bytes: &[u8]) -> Result<usize, String> {
    let mut pos = 0usize;
    let mut frames = 0usize;
    while pos + 8 <= bytes.len() {
        let len = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        let start = pos + 8;
        if bytes.len() < start + len {
            break; // torn tail; not what this helper is testing
        }
        match bincode::deserialize::<LegacyWalRecord>(&bytes[start..start + len]) {
            Ok(_) => frames += 1,
            Err(e) => return Err(e.to_string()),
        }
        pos = start + len;
    }
    Ok(frames)
}

// ── The opt-in (§7 D8) ───────────────────────────────────────────────────────

/// **The test this release turns on.** A store that never calls
/// `enable_multiplicity()` must contain nothing a v0.6.9 binary cannot read.
/// If this goes red, the opt-in has leaked and every user's store quietly
/// stopped being readable by the previous release.
#[test]
fn multiplicity_off_writes_no_new_record() {
    let dir = tmp("off-no-record");
    let mut db = GraphDb::open(&dir).unwrap();
    assert!(
        !db.is_multiplicity_enabled(),
        "a fresh store is not opted in"
    );
    for k in ["a", "b"] {
        db.insert_node("N", k, vec![]).unwrap();
    }
    assert!(db.insert_edge("E", "a", "b").unwrap());
    // The duplicate: this is the write that would record a count if the gate
    // were open. It must stay the total no-op it has always been.
    assert!(!db.insert_edge("E", "a", "b").unwrap());
    assert!(!db.insert_edge("E", "a", "b").unwrap());
    drop(db);

    assert!(
        count_records(&dir).is_empty(),
        "a store that never opted in must carry no discriminant-23 record"
    );
    // And the whole WAL replays under a decoder that knows only 0–22.
    let n = legacy_decode(&wal_bytes(&dir)).expect("a 0–22 decoder must read this WAL whole");
    assert!(n > 0, "the fixture writes frames");

    // The readout preference is still answerable: it reports the unique count.
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(
        db.degree_multiplicity("a", Some("E"), AlgoDir::Out)
            .unwrap(),
        1
    );
}

/// `degree_multiplicity` on a store that never opted in is the unique count,
/// not an error: the argument is a readout preference, not a demand the store
/// cannot meet.
#[test]
fn multiplicity_disabled_reports_unique_degree() {
    let dir = tmp("disabled-reports-unique");
    let mut db = GraphDb::open(&dir).unwrap();
    for k in ["a", "b", "c"] {
        db.insert_node("N", k, vec![]).unwrap();
    }
    db.insert_edge("E", "a", "b").unwrap();
    db.insert_edge("E", "a", "c").unwrap();
    assert!(!db.insert_edge("E", "a", "b").unwrap());
    assert_eq!(db.degree("a", Some("E"), AlgoDir::Out).unwrap(), 2);
    assert_eq!(
        db.degree_multiplicity("a", Some("E"), AlgoDir::Out)
            .unwrap(),
        2
    );
}

/// Opting in is a persisted declaration, like `enable_index`: it survives a
/// reopen, so a duplicate written in the next session still counts.
#[test]
fn enable_multiplicity_survives_reopen_and_snapshot() {
    let dir = tmp("enable-survives");
    let mut db = GraphDb::open(&dir).unwrap();
    db.enable_multiplicity().unwrap();
    assert!(db.is_multiplicity_enabled());
    for k in ["a", "b"] {
        db.insert_node("N", k, vec![]).unwrap();
    }
    db.insert_edge("E", "a", "b").unwrap();
    drop(db);

    let mut db = GraphDb::open(&dir).unwrap();
    assert!(
        db.is_multiplicity_enabled(),
        "the declaration is persisted, not per-handle"
    );
    // A truncating snapshot re-emits the declaration into the baseline WAL, the
    // way it re-emits EnableIndex.
    db.snapshot().unwrap();
    drop(db);
    let db = GraphDb::open(&dir).unwrap();
    assert!(
        db.is_multiplicity_enabled(),
        "a snapshot must not silently opt the store back out"
    );
    drop(db);

    let mut db = GraphDb::open(&dir).unwrap();
    assert!(!db.insert_edge("E", "a", "b").unwrap());
    assert_eq!(
        db.degree_multiplicity("a", Some("E"), AlgoDir::Out)
            .unwrap(),
        2
    );
}

/// Enabling twice is a no-op, not an error: an operator should not have to ask
/// whether the store is already opted in before asking for the feature.
#[test]
fn enable_multiplicity_is_idempotent() {
    let dir = tmp("enable-idempotent");
    let mut db = GraphDb::open(&dir).unwrap();
    db.enable_multiplicity().unwrap();
    db.enable_multiplicity().unwrap();
    assert!(db.is_multiplicity_enabled());
    drop(db);
    assert_eq!(
        count_records(&dir).len(),
        1,
        "the second call declares nothing new"
    );
}

// ── The count itself (§5.13) ─────────────────────────────────────────────────

/// A duplicate increments the count and leaves the unique degree alone.
/// `insert_edge` still answers "was this pair new", which it was not.
#[test]
fn duplicate_insert_increments_count_not_unique_degree() {
    let (_dir, mut db) = pair("dup-increments");
    assert_eq!(db.degree("a", Some("E"), AlgoDir::Out).unwrap(), 1);
    assert_eq!(
        db.degree_multiplicity("a", Some("E"), AlgoDir::Out)
            .unwrap(),
        1
    );

    assert!(
        !db.insert_edge("E", "a", "b").unwrap(),
        "the pair is not new, so the answer is still false"
    );
    assert_eq!(
        db.degree("a", Some("E"), AlgoDir::Out).unwrap(),
        1,
        "adjacency is a set"
    );
    assert_eq!(
        db.degree_multiplicity("a", Some("E"), AlgoDir::Out)
            .unwrap(),
        2
    );

    assert!(!db.insert_edge("E", "a", "b").unwrap());
    assert_eq!(db.degree("a", Some("E"), AlgoDir::Out).unwrap(), 1);
    assert_eq!(
        db.degree_multiplicity("a", Some("E"), AlgoDir::Out)
            .unwrap(),
        3
    );

    // The count is the reserved edge property, readable as one.
    assert_eq!(
        db.get_edge_prop("E", "a", "b", "count"),
        Some(Value::Int(3))
    );
    // And the `In` side of the same pair agrees.
    assert_eq!(
        db.degree_multiplicity("b", Some("E"), AlgoDir::In).unwrap(),
        3
    );
}

/// The count is per pair, not per node: a second neighbour keeps its own.
#[test]
fn counts_are_per_pair() {
    let (_dir, mut db) = pair("per-pair");
    db.insert_node("N", "c", vec![]).unwrap();
    db.insert_edge("E", "a", "c").unwrap();
    for _ in 0..3 {
        assert!(!db.insert_edge("E", "a", "b").unwrap());
    }
    assert_eq!(db.degree("a", Some("E"), AlgoDir::Out).unwrap(), 2);
    assert_eq!(
        db.degree_multiplicity("a", Some("E"), AlgoDir::Out)
            .unwrap(),
        5,
        "4 for a→b and 1 for a→c"
    );
    assert_eq!(
        db.get_edge_prop("E", "a", "c", "count"),
        None,
        "absent means 1"
    );
}

/// `degrees` carries the same readout preference, row by row.
#[test]
fn degrees_multiplicity_sums_counts_per_row() {
    let (_dir, mut db) = pair("degrees-multi");
    db.insert_node("N", "c", vec![]).unwrap();
    db.insert_edge("E", "a", "c").unwrap();
    assert!(!db.insert_edge("E", "a", "b").unwrap());
    assert!(!db.insert_edge("E", "a", "c").unwrap());
    assert!(!db.insert_edge("E", "a", "c").unwrap());

    let keys: Vec<String> = vec!["a".into(), "b".into(), "c".into()];
    let unique = db
        .degrees(Some(&keys), None, None, Some("E"), AlgoDir::Out, None)
        .unwrap();
    assert_eq!(
        unique,
        vec![("a".into(), 2), ("b".into(), 0), ("c".into(), 0)]
    );

    let multi = db
        .degrees_multiplicity(Some(&keys), None, None, Some("E"), AlgoDir::Out, None)
        .unwrap();
    assert_eq!(
        multi,
        vec![("a".into(), 5), ("b".into(), 0), ("c".into(), 0)],
        "2 for a→b and 3 for a→c"
    );
}

/// A duplicate submitted through a batch counts the same way a direct
/// `insert_edge` does — `ingest_batch` and Cypher `CREATE` reach the same
/// choke-point, and a count that only one of the two entry points maintains
/// would be worse than no count at all.
#[test]
fn batch_duplicate_insert_increments_count() {
    let (dir, mut db) = pair("batch-dup");
    db.batch()
        .insert_edge("E", "a", "b")
        .insert_edge("E", "a", "b")
        .commit()
        .unwrap();
    assert_eq!(db.degree("a", Some("E"), AlgoDir::Out).unwrap(), 1);
    assert_eq!(
        db.degree_multiplicity("a", Some("E"), AlgoDir::Out)
            .unwrap(),
        3,
        "both duplicates count: the frame carries its own counts forward rather \
         than computing committed+1 twice"
    );

    // And the frame's counts are durable on their own, without a snapshot.
    drop(db);
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(
        db.degree_multiplicity("a", Some("E"), AlgoDir::Out)
            .unwrap(),
        3
    );
}

/// Delete takes the pair and its count. A later re-insert starts at 1, not at
/// what the pair used to carry.
#[test]
fn delete_edge_clears_multiplicity() {
    let (_dir, mut db) = pair("delete-clears");
    assert!(!db.insert_edge("E", "a", "b").unwrap());
    assert!(!db.insert_edge("E", "a", "b").unwrap());
    assert_eq!(
        db.degree_multiplicity("a", Some("E"), AlgoDir::Out)
            .unwrap(),
        3
    );

    assert!(db.delete_edge("E", "a", "b").unwrap());
    assert_eq!(
        db.degree_multiplicity("a", Some("E"), AlgoDir::Out)
            .unwrap(),
        0
    );
    assert_eq!(db.get_edge_prop("E", "a", "b", "count"), None);

    assert!(db.insert_edge("E", "a", "b").unwrap());
    assert_eq!(
        db.degree_multiplicity("a", Some("E"), AlgoDir::Out)
            .unwrap(),
        1,
        "a fresh pair is 1, not 3"
    );
}

// ── Durability (§5.13: the snapshot-only count was rejected) ─────────────────

/// Counts are edge properties, and edge properties are snapshotted.
#[test]
fn multiplicity_survives_a_snapshot() {
    let (dir, mut db) = pair("survives-snapshot");
    for _ in 0..4 {
        assert!(!db.insert_edge("E", "a", "b").unwrap());
    }
    assert_eq!(
        db.degree_multiplicity("a", Some("E"), AlgoDir::Out)
            .unwrap(),
        5
    );
    db.snapshot().unwrap();
    drop(db);

    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(
        db.degree_multiplicity("a", Some("E"), AlgoDir::Out)
            .unwrap(),
        5
    );
    assert_eq!(db.degree("a", Some("E"), AlgoDir::Out).unwrap(), 1);
}

/// **The test the design turns on.** No snapshot: the handle is dropped and the
/// store reopened, so the count has to come back from the WAL alone. A
/// snapshot-only count — the shape v0.6.9 rejected — fails here and passes the
/// test above, which is why both exist.
#[test]
fn multiplicity_survives_wal_replay() {
    let (dir, mut db) = pair("survives-replay");
    for _ in 0..4 {
        assert!(!db.insert_edge("E", "a", "b").unwrap());
    }
    assert_eq!(
        db.degree_multiplicity("a", Some("E"), AlgoDir::Out)
            .unwrap(),
        5
    );
    // Deliberately no snapshot() here.
    drop(db);
    assert!(
        !count_records(&dir).is_empty(),
        "the counts must be in the WAL, not only in memory"
    );

    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(
        db.degree_multiplicity("a", Some("E"), AlgoDir::Out)
            .unwrap(),
        5,
        "the WAL replay path must rebuild the count"
    );
    assert_eq!(db.degree("a", Some("E"), AlgoDir::Out).unwrap(), 1);
}

/// A history-preserving snapshot replays pre-snapshot count records over a base
/// that already folded them in. The record carries an absolute count rather
/// than a delta precisely so that replay lands on the same number.
#[test]
fn replay_over_a_kept_wal_does_not_double_count() {
    let (dir, mut db) = pair("keep-wal-replay");
    for _ in 0..2 {
        assert!(!db.insert_edge("E", "a", "b").unwrap());
    }
    db.snapshot_with(mushroomdb::SnapshotOptions {
        keep_wal: true,
        ..Default::default()
    })
    .unwrap();
    drop(db);

    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(
        db.degree_multiplicity("a", Some("E"), AlgoDir::Out)
            .unwrap(),
        3,
        "absolute counts replay idempotently; a delta would double"
    );
}

// ── The reserved name (§5.13) ────────────────────────────────────────────────

/// `count` is reserved on an edge. Cypher `SET` on it is refused by name rather
/// than by the generic "did not resolve to a node key" the executor would
/// otherwise answer, so the caller learns what is actually wrong.
#[test]
fn cypher_set_on_count_is_refused() {
    let (_dir, mut db) = pair("cypher-set-count");
    let no_params = std::collections::BTreeMap::new();
    match db.query_write("MATCH (a)-[r:E]->(b) SET r.count = 9", &no_params) {
        Err(GraphError::QueryError { detail }) => {
            assert!(
                detail.contains("count") && detail.contains("reserved"),
                "the refusal must name the reserved property; got {detail:?}"
            );
        }
        other => panic!("expected a named QueryError, got {other:?}"),
    }
    assert_eq!(
        db.degree_multiplicity("a", Some("E"), AlgoDir::Out)
            .unwrap(),
        1,
        "the refused write changed nothing"
    );
    // A *node* property called `count` is untouched by the reservation — the
    // reserved name is an edge property.
    db.query_write("MATCH (n:N) SET n.count = 9", &no_params)
        .unwrap();
    assert_eq!(
        db.node_info("a").unwrap().props.get("count"),
        Some(&Value::Int(9))
    );
}

// ── Composition with scoped reads (§5.4) ─────────────────────────────────────

/// A scoped count sums the insert-counts of **visible** pairs only. An unscoped
/// count leaking through a scoped handle discloses a hidden neighbour by
/// arithmetic, which is the leak this whole release exists to prevent — and a
/// multiplicity count leaks more than a unique one, because it says how often.
#[test]
fn scoped_multiplicity_counts_visible_pairs_only() {
    let (_dir, mut db) = pair("scoped-visible-only");
    db.insert_node("N", "c", vec![]).unwrap();
    db.insert_edge("E", "a", "c").unwrap();
    // a→b inserted 3 times, a→c inserted 7 times.
    for _ in 0..2 {
        assert!(!db.insert_edge("E", "a", "b").unwrap());
    }
    for _ in 0..6 {
        assert!(!db.insert_edge("E", "a", "c").unwrap());
    }
    assert_eq!(
        db.degree_multiplicity("a", Some("E"), AlgoDir::Out)
            .unwrap(),
        10
    );

    let mask = NodeMask::from_keys(&db, ["a", "b"]);
    assert_eq!(
        db.degree_scoped_multiplicity("a", Some("E"), AlgoDir::Out, &mask)
            .unwrap(),
        3,
        "`c` is hidden, so its seven inserts are not in the sum"
    );
    // The unique scoped count is still 1: the filter picks pairs, the readout
    // preference picks what each pair contributes.
    assert_eq!(
        db.degree_scoped("a", Some("E"), AlgoDir::Out, &mask)
            .unwrap(),
        1
    );

    // A hidden subject stays KeyNotFound, exactly as the unique reading does.
    let narrow = NodeMask::from_keys(&db, ["a", "b"]);
    match db.degree_scoped_multiplicity("c", Some("E"), AlgoDir::Both, &narrow) {
        Err(GraphError::KeyNotFound { key }) => assert_eq!(key, "c"),
        other => panic!("expected KeyNotFound, got {other:?}"),
    }

    // `degrees` filters both sides the same way.
    let keys: Vec<String> = vec!["a".into(), "b".into(), "c".into()];
    let rows = db
        .degrees_scoped_multiplicity(
            Some(&keys),
            None,
            None,
            Some("E"),
            AlgoDir::Out,
            None,
            &mask,
        )
        .unwrap();
    assert_eq!(rows, vec![("a".into(), 3), ("b".into(), 0)]);
}

// ── Format compatibility (§7 D8) ─────────────────────────────────────────────

/// A decoder that knows only discriminants 0–22 cannot read discriminant 23,
/// and says so: a clean bincode error naming the variant it does not know, not
/// a panic and not a silent skip that would apply the records around it while
/// dropping the one it did not understand.
///
/// What it does *not* do is refuse the open. `decode_all` treats any
/// undeserializable frame as a corrupt tail and returns the valid prefix, so a
/// v0.6.9 binary **truncates** the WAL at the first discriminant-23 frame. That
/// is the real reason the opt-in matters, and it is asserted here rather than
/// assumed.
#[test]
fn a_0_6_9_decoder_refuses_discriminant_23() {
    let (dir, mut db) = pair("legacy-decoder");
    assert!(!db.insert_edge("E", "a", "b").unwrap());
    drop(db);

    let bytes = wal_bytes(&dir);
    let err = legacy_decode(&bytes).expect_err("a 0–22 decoder must not read discriminant 23");
    assert!(
        err.to_lowercase().contains("variant"),
        "the refusal must name the unknown variant; got {err:?}"
    );

    // The store-level consequence, stated rather than assumed: the frame is not
    // skipped past — decoding stops before it, and everything after it is lost.
    let (_recs, valid_len) = decode_all(&bytes);
    assert_eq!(
        valid_len,
        bytes.len(),
        "this decoder does know 23, so it reads the WAL whole"
    );
    let first_23 = {
        let mut pos = 0usize;
        let mut at = None;
        while pos + 8 <= bytes.len() {
            let len = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
            let payload = &bytes[pos + 8..pos + 8 + len];
            if bincode::deserialize::<LegacyWalRecord>(payload).is_err() {
                at = Some(pos);
                break;
            }
            pos += 8 + len;
        }
        at.expect("the fixture writes a discriminant-23 frame")
    };
    assert!(
        first_23 < bytes.len(),
        "a v0.6.9 reader stops here and loses every commit after it"
    );
}

/// The declaration and a count share discriminant 23 and are distinguishable:
/// the declaration is the reserved all-`u32::MAX` tuple with count 0, which no
/// real pair can be.
#[test]
fn the_declaration_is_distinguishable_from_a_count() {
    let (dir, mut db) = pair("decl-vs-count");
    assert!(!db.insert_edge("E", "a", "b").unwrap());
    drop(db);

    let recs = count_records(&dir);
    let decls = recs
        .iter()
        .filter(|r| matches!(r, WalRecord::SetEdgeCount { count: 0, .. }))
        .count();
    assert_eq!(decls, 1, "exactly one declaration");
    let counts: Vec<u64> = recs
        .iter()
        .filter_map(|r| match r {
            WalRecord::SetEdgeCount { count, .. } if *count > 0 => Some(*count),
            _ => None,
        })
        .collect();
    assert_eq!(counts, vec![2], "one count record, carrying an absolute 2");
}

/// The MVCC read path carries the count too.
///
/// `ReaderSnapshot` folds WAL deltas through its own applier, not through
/// `GraphDb::apply`, and it maintains the edge-property overlay that Cypher
/// reads `r.count` from. A count dropped there would make a lock-free reader
/// disagree with the writer about a pair it can otherwise see.
#[test]
fn a_reader_snapshot_carries_the_count() {
    let (_dir, mut db) = pair("reader-count");
    for _ in 0..2 {
        assert!(!db.insert_edge("E", "a", "b").unwrap());
    }
    let snap = db.reader();
    let no_params = std::collections::BTreeMap::new();
    let rs = snap
        .query("MATCH (a)-[r:E]->(b) RETURN r.count AS c", &no_params)
        .unwrap();
    assert_eq!(rs.len(), 1);
    assert_eq!(
        rs.get(0, "c"),
        Some(&Value::Int(3)),
        "the reader's edge-property overlay must carry the count"
    );
}

// ── the reserved name `count` against a rule that claims it (F10) ────────────

/// `EDGE_COUNT_PROP` and a rule's `weight_prop` are stored in the same
/// `edge_props` map, keyed the same way, so a rule declaring `weight_prop:
/// "count"` names the slot the insert count lives in. Nothing refuses such a
/// rule, and `enable_multiplicity` does no pre-flight scan for one — deliberately,
/// because three independent properties keep the two apart. This test is what
/// says so; remove any one of them and it goes red.
///
/// 1. A rule-owned edge cannot be inserted by hand: `insert_edge` answers
///    [`GraphError::RuleOwned`], so a duplicate insert can never reach
///    `edge_count_record` for a pair whose weight the rule wrote.
/// 2. The engine writes a weight only onto an edge the rule *owns* — never onto
///    a pre-existing user edge — so the reverse order cannot collide either.
/// 3. A weight is always `Value::Float`; the count readers take only
///    `Value::Int`, so even a Float in the slot reads as the absent-means-one
///    default rather than as a count.
///
/// If a later release relaxes (1) or (2), this test is the one that notices.
/// It does **not** kill a mutant that widened the count readers to accept a
/// Float — barrier 3 only matters once 1 and 2 have fallen, and the score a
/// `FieldEqual` rule writes is `1.0`, which a Float-accepting reader and the
/// absent-means-one default would both report as 1. Barrier 3 is stated here
/// because it is load-bearing, and pinned only as far as the fixture reaches.
#[test]
fn a_rule_claiming_the_reserved_count_name_cannot_collide_with_it() {
    fn same_rule() -> mushroomdb::RuleDef {
        mushroomdb::RuleDef {
            name: "same".into(),
            src_label: "Person".into(),
            dst_label: "Org".into(),
            predicate: mushroomdb::Predicate::FieldEqual {
                field: "industry".into(),
            },
            edge_type: "SAME".into(),
            // The reserved name, claimed by a rule. Accepted: see above.
            weight_prop: Some("count".into()),
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
            namespace: None,
        }
    }
    fn seed(name: &str) -> (std::path::PathBuf, Db) {
        let dir = tmp(name);
        let mut db = GraphDb::open(&dir).unwrap();
        for (label, key) in [("Org", "o1"), ("Person", "p1")] {
            db.insert_node(
                label,
                key,
                vec![("industry".into(), Value::Str("x".into()))],
            )
            .unwrap();
        }
        (dir, db)
    }
    let weight = |db: &Db| -> Option<Value> {
        db.query(
            "MATCH (p:Person)-[r:SAME]->(o:Org) RETURN r.count",
            &Default::default(),
        )
        .unwrap()
        .get(0, "r.count")
        .cloned()
    };

    // Rule first: the rule owns the edge and writes its weight into `count`.
    let (_a, mut db) = seed("rule-claims-count-a");
    db.create_rule(same_rule())
        .expect("a rule may name the reserved prop; nothing refuses it");
    assert_eq!(
        weight(&db),
        Some(Value::Float(1.0)),
        "fixture: the rule's weight is in the `count` slot, as a Float"
    );
    db.enable_multiplicity()
        .expect("opting in does not pre-flight the rule set, and does not need to");
    for _ in 0..2 {
        assert!(
            matches!(
                db.insert_edge("SAME", "p1", "o1"),
                Err(GraphError::RuleOwned { .. })
            ),
            "barrier 1: a rule-owned edge is not insertable, so no count is ever \
             written over the rule's weight"
        );
    }
    assert_eq!(
        weight(&db),
        Some(Value::Float(1.0)),
        "the rule's weight survives the attempted duplicates"
    );
    assert_eq!(
        db.degree_multiplicity("p1", Some("SAME"), AlgoDir::Out)
            .unwrap(),
        1,
        "the pair contributes the absent-means-one default; no count was ever \
         written for it"
    );

    // User edge first: the rule is created over a pair the user already owns.
    let (_b, mut db) = seed("rule-claims-count-b");
    db.enable_multiplicity().unwrap();
    assert!(db.insert_edge("SAME", "p1", "o1").unwrap());
    assert!(!db.insert_edge("SAME", "p1", "o1").unwrap());
    assert_eq!(weight(&db), Some(Value::Int(2)), "fixture: a real count");
    db.create_rule(same_rule()).unwrap();
    assert_eq!(
        weight(&db),
        Some(Value::Int(2)),
        "barrier 2: the rule does not own the user's edge, so it writes no \
         weight over the count"
    );
    assert_eq!(
        db.explain("p1", "o1")
            .unwrap()
            .first()
            .and_then(|e| e.weight),
        None,
        "and it reports no stored weight for the pair, rather than the count"
    );
    assert!(!db.insert_edge("SAME", "p1", "o1").unwrap());
    assert_eq!(
        weight(&db),
        Some(Value::Int(3)),
        "the count keeps counting: the rule never took the slot"
    );
}
