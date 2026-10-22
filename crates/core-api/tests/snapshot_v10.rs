//! V10: the snapshot version that announces "this store uses WAL discriminant 23".
//!
//! mushroomdb's two on-disk formats fail in opposite directions. An unknown
//! **snapshot** version is refused by name — `snapshot: unsupported version
//! {N}`. An unknown **WAL** discriminant is not: `decode_all` treats a frame it
//! cannot deserialise as a corrupt tail and hands back the valid prefix, and
//! `open_with`'s `repair_wal` (on by default) writes that truncation back to
//! disk. A v0.6.9 binary opening a multiplicity-enabled store would therefore
//! lose every commit after the opt-in, and persist the loss.
//!
//! The snapshot is read *before* the WAL. So the fix is to put the loud failure
//! in front of the silent one: a store that has opted in writes its snapshot at
//! V10, an older reader stops there, and the WAL it would have truncated is
//! never opened.
//!
//! Two properties hold this up, and both are tested here:
//!   1. A store that never opts in keeps writing V9 and stays readable by
//!      v0.6.9 forever (`a_store_that_never_opts_in_still_writes_v9`).
//!   2. `enable_multiplicity()` writes the V10 snapshot *before* the WAL record
//!      it guards, so the guard is never missing when the record is present
//!      (`enable_multiplicity_snapshots_immediately`; the crash-window sweep in
//!      `crates/sim-harness/tests/multiplicity_crash_window.rs` covers the same
//!      ordering at every interruption point).

use core_storage::{decode_all, WalRecord};
use mushroomdb::{AlgoDir, GraphDb, OpenOptions, RealFs, SnapshotOptions};

type Db = GraphDb<RealFs>;

/// The snapshot versions v0.6.9's `snapshot::decode` accepts. Everything else
/// lands in its `other` arm and is refused with `snapshot: unsupported version
/// {N}`. V10 is deliberately outside this set.
const V0_6_9_ACCEPTS: [u16; 5] = [5, 6, 7, 8, 9];

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "graphdb-snapshot-v10-{}-{}-{}",
        name,
        std::process::id(),
        nanos
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn snap_version(dir: &std::path::Path) -> Option<u16> {
    mushroomdb::snapshot_version_at(dir).expect("peek snapshot version")
}

fn wal_bytes(dir: &std::path::Path) -> Vec<u8> {
    std::fs::read(dir.join("wal.bin")).expect("read wal.bin")
}

/// Whether the WAL carries a discriminant-23 (`SetEdgeCount`) frame — the
/// record a v0.6.9 decoder cannot read.
fn wal_has_discriminant_23(dir: &std::path::Path) -> bool {
    let (frames, _) = decode_all(&wal_bytes(dir));
    frames.iter().any(|f| match f {
        WalRecord::SetEdgeCount { .. } => true,
        WalRecord::Batch(inner) => inner
            .iter()
            .any(|r| matches!(r, WalRecord::SetEdgeCount { .. })),
        _ => false,
    })
}

/// CRC-32 (IEEE), the polynomial `wal::encode_record` writes into every frame
/// header. Spelled out here so this test needs no new dependency;
/// `undecodable_frame` checks it against the real encoder before using it.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// A frame with a valid length and CRC whose payload is an enum discriminant no
/// `WalRecord` has. `decode_all` stops at it in its `Err(_)` arm — exactly
/// where v0.6.9 stops at discriminant 23 — so a store carrying one is a store
/// `repair_wal` truncates.
fn undecodable_frame() -> Vec<u8> {
    // The CRC written here has to be the one the reader recomputes, or the
    // frame would stop the decoder as a *corrupt* tail rather than an
    // unreadable record. Pin it against the shipped encoder.
    let real = core_storage::encode_record(&WalRecord::DeleteNode { key: "z".into() });
    assert_eq!(
        u32::from_le_bytes(real[4..8].try_into().unwrap()),
        crc32(&real[8..]),
        "this test's CRC must be the one the WAL encoder writes"
    );

    let payload = 99u32.to_le_bytes(); // bincode enum tag: no such variant
    let mut out = Vec::with_capacity(12);
    out.extend((payload.len() as u32).to_le_bytes());
    out.extend(crc32(&payload).to_le_bytes());
    out.extend(payload);
    out
}

fn append_undecodable_frame(dir: &std::path::Path) {
    let mut bytes = wal_bytes(dir);
    bytes.extend(undecodable_frame());
    std::fs::write(dir.join("wal.bin"), &bytes).expect("write wal.bin");
}

/// Two nodes and an edge, inserted twice.
fn workload(db: &mut Db) {
    for k in ["a", "b"] {
        db.insert_node("N", k, vec![]).unwrap();
    }
    assert!(db.insert_edge("E", "a", "b").unwrap());
    assert!(!db.insert_edge("E", "a", "b").unwrap());
}

// ── Property 1: the store that never asked keeps its downgrade path ──────────

/// **The property everything rests on.** A store that never calls
/// `enable_multiplicity()` must keep writing V9 snapshots — not just once, but
/// on every snapshot it ever takes. V10 exists to refuse an old reader; a store
/// that carries nothing an old reader cannot read must never wear that refusal.
///
/// If this goes red, every user who upgrades to v0.6.10 and takes a snapshot
/// loses the ability to go back to v0.6.9, without ever asking for the feature.
#[test]
fn a_store_that_never_opts_in_still_writes_v9() {
    let dir = tmp("never-opts-in");
    let mut db = GraphDb::open(&dir).unwrap();
    assert!(!db.is_multiplicity_enabled());
    workload(&mut db);
    db.snapshot().unwrap();
    assert_eq!(
        snap_version(&dir),
        Some(9),
        "a store that never opted in writes V9"
    );
    drop(db);

    // Every later snapshot too — including a keep_wal one, and one taken after
    // a reopen that loaded a V9 base and merged an overlay into it.
    let mut db = GraphDb::open(&dir).unwrap();
    assert!(!db.is_multiplicity_enabled());
    db.insert_node("N", "c", vec![]).unwrap();
    db.snapshot_with(SnapshotOptions {
        keep_wal: true,
        ..SnapshotOptions::default()
    })
    .unwrap();
    assert_eq!(
        snap_version(&dir),
        Some(9),
        "the merge-snapshot path must not move the version either"
    );
    db.insert_node("N", "d", vec![]).unwrap();
    db.snapshot().unwrap();
    assert_eq!(snap_version(&dir), Some(9));
    drop(db);

    // V9 is inside the set v0.6.9 reads, and nothing in the WAL is outside 0–22.
    assert!(V0_6_9_ACCEPTS.contains(&9));
    assert!(
        !wal_has_discriminant_23(&dir),
        "a store that never opted in carries no discriminant-23 record"
    );
    // And the binary still advertises 9 as the version it writes by default.
    assert_eq!(Db::format_version(), 9);
    assert_eq!(mushroomdb::SNAPSHOT_VERSION, 9);

    // Reopening reads it back whole.
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.degree("a", Some("E"), AlgoDir::Out).unwrap(), 1);
    assert!(db.has_node("d"));
}

// ── Property 2: opting in moves the store, and does it before the record ─────

/// Opting in moves the store to V10 and keeps it there.
#[test]
fn enable_multiplicity_moves_the_store_to_v10() {
    let dir = tmp("moves-to-v10");
    let mut db = GraphDb::open(&dir).unwrap();
    db.enable_multiplicity().unwrap();
    workload(&mut db);
    db.snapshot().unwrap();
    assert_eq!(snap_version(&dir), Some(10));
    drop(db);

    // The declaration is persisted, so every later snapshot is V10 too.
    let mut db = GraphDb::open(&dir).unwrap();
    assert!(db.is_multiplicity_enabled());
    db.insert_node("N", "c", vec![]).unwrap();
    db.snapshot_with(SnapshotOptions {
        keep_wal: true,
        ..SnapshotOptions::default()
    })
    .unwrap();
    assert_eq!(snap_version(&dir), Some(10));
    drop(db);

    // A V10 snapshot is still fully readable by *this* binary — the version
    // moved, the container did not.
    let db = GraphDb::open(&dir).unwrap();
    assert!(db.is_multiplicity_enabled());
    assert_eq!(db.degree("a", Some("E"), AlgoDir::Out).unwrap(), 1);
    assert_eq!(
        db.degree_multiplicity("a", Some("E"), AlgoDir::Out)
            .unwrap(),
        2,
        "the count survives the V10 snapshot"
    );
    assert!(db.has_node("c"));

    assert!(
        !V0_6_9_ACCEPTS.contains(&10),
        "V10's only job is to be outside the set v0.6.9 reads"
    );
}

/// **The hole this design has to close.** A store that has opted in but has not
/// snapshotted yet would have a WAL containing discriminant 23 and no V10
/// snapshot in front of it — and a v0.6.9 binary would go straight to WAL
/// replay and truncate. So `enable_multiplicity()` takes the snapshot itself,
/// in the same call, before it writes the record.
#[test]
fn enable_multiplicity_snapshots_immediately() {
    let dir = tmp("snapshots-immediately");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("N", "a", vec![]).unwrap();
    assert!(
        !dir.join("snapshot.bin").exists(),
        "fixture: the store has no snapshot before the call"
    );
    let wal_before = wal_bytes(&dir);
    assert!(!wal_before.is_empty(), "fixture: the store has history");

    db.enable_multiplicity().unwrap();

    // No drop, no `snapshot()`, no further write of any kind: the guard is on
    // disk the moment the call returns.
    assert_eq!(
        snap_version(&dir),
        Some(10),
        "enable_multiplicity must leave a V10 snapshot on disk"
    );
    assert!(
        wal_has_discriminant_23(&dir),
        "fixture: the record the snapshot guards is in the WAL"
    );

    // History is kept: the forced snapshot is a `keep_wal` one, so every commit
    // that was reachable by `open_at` before the call still is. Opting in to a
    // count is not a request to compact the store.
    assert_eq!(
        &wal_bytes(&dir)[..wal_before.len()],
        &wal_before[..],
        "the forced snapshot must append to the WAL, not replace it"
    );
    assert!(db.has_node("a"));
    db.insert_node("N", "b", vec![]).unwrap();
    drop(db);
    let db = GraphDb::open(&dir).unwrap();
    assert!(db.is_multiplicity_enabled());
    assert!(db.has_node("a") && db.has_node("b"));
}

// ── Property 3: the refusal, and that it is a refusal ────────────────────────

/// A reader that does not know the store's snapshot version refuses the open by
/// name, and — the whole point — leaves the WAL byte-identical instead of
/// truncating it.
///
/// This binary knows V10, so it cannot play the v0.6.9 reader against a V10
/// file directly. It does not have to: the two runs execute the *same code*.
/// For a version outside its accepted set, `open_dir` takes the `is_v8== false`
/// branch, reads the snapshot, and hits `decode`'s `other` arm — identical
/// between the releases, because V10 is the only thing v0.6.10 added to that
/// match. So a V11 snapshot here and a V10 snapshot under v0.6.9 are the same
/// execution. The store is stamped V11 with a correct header CRC, so the file
/// is as well-formed as the real V10 one it stands in for.
///
/// The control half matters as much as the test half: the same store, opened
/// with a snapshot version the binary *does* know, really does get its WAL
/// truncated. The refusal is what stops that, not the absence of a repair.
#[test]
fn a_0_6_9_reader_refuses_a_v10_snapshot() {
    // The store the scenario is about: opted in, so its snapshot is V10 and its
    // WAL carries discriminant 23.
    let dir = tmp("refuses-v10");
    let mut db = GraphDb::open(&dir).unwrap();
    db.enable_multiplicity().unwrap();
    workload(&mut db);
    db.snapshot_with(SnapshotOptions {
        keep_wal: true,
        ..SnapshotOptions::default()
    })
    .unwrap();
    drop(db);
    assert_eq!(snap_version(&dir), Some(10));
    assert!(wal_has_discriminant_23(&dir));
    assert!(!V0_6_9_ACCEPTS.contains(&10));

    // A WAL whose tail this binary cannot decode — the shape v0.6.9 sees from
    // the first discriminant-23 frame on.
    append_undecodable_frame(&dir);
    let wal_before = wal_bytes(&dir);
    let (_, valid_len) = decode_all(&wal_before);
    assert!(
        valid_len < wal_before.len(),
        "fixture: the WAL must have a tail repair would truncate"
    );

    // ── Control: with a snapshot version the reader knows, repair fires. ──
    let control = tmp("refuses-v10-control");
    copy_store(&dir, &control);
    let mut snap = std::fs::read(control.join("snapshot.bin")).unwrap();
    core_storage::snapshot::stamp_container_version(&mut snap, 9).unwrap();
    std::fs::write(control.join("snapshot.bin"), &snap).unwrap();
    GraphDb::open(&control).expect("a V9 snapshot opens");
    assert_eq!(
        wal_bytes(&control).len(),
        valid_len,
        "control: a reader that gets past the snapshot truncates this WAL"
    );

    // ── Test: an unknown snapshot version refuses, and writes nothing. ──
    // 12, not 11: this binary reads V11 (0.10 TY-12), and the point is a
    // version the reader does not know.
    let mut snap = std::fs::read(dir.join("snapshot.bin")).unwrap();
    core_storage::snapshot::stamp_container_version(&mut snap, 12).unwrap();
    std::fs::write(dir.join("snapshot.bin"), &snap).unwrap();

    let msg = match GraphDb::open_with_options(&dir, OpenOptions::default()) {
        Ok(_) => panic!("an unknown snapshot version must refuse the open"),
        Err(e) => e.to_string(),
    };
    assert!(
        msg.contains("unsupported version 12"),
        "the refusal must name the version; got: {msg}"
    );
    assert_eq!(
        wal_bytes(&dir),
        wal_before,
        "the refusal must leave the WAL byte-identical — this is the whole change"
    );
}

fn copy_store(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_file() {
            std::fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
        }
    }
}
