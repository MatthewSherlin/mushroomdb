//! V11: the opt-in, sticky snapshot format behind the 0.10 value types, unique
//! constraints and range indexes (0.10 TY-12, contract C9).
//!
//! A 0.9.x reader cannot decode a WAL frame that carries a `Date` or a
//! `CreateConstraint`; it treats the frame as a corrupt tail and truncates the
//! WAL there. So a store moves to V11 *before* its first such record is
//! appended — one V11 snapshot, keeping the WAL — and a 0.9.x reader stops at
//! `snapshot: unsupported version 11` with the WAL untouched. The properties
//! pinned here:
//!
//!   1. Every write path upgrades at its first V11-only record, and only then.
//!   2. A store that never writes one keeps writing V9 (or V10), with nothing
//!      V11 in its snapshot.
//!   3. V11 is sticky: dropping the last constraint or range index, reopening,
//!      or snapshotting again never moves the store back.
//!   4. The upgrade is ordered: the stamp is on disk before the record.
//!   5. Section 18 carries the constraints and the multiplicity opt-in, so a
//!      WAL archived away before its baseline was written loses neither.
//!   6. Section 17 (derived bits) and section 18 coexist in one V11 snapshot.

use core_storage::{decode_all, WalRecord};
use mushroomdb::{
    GraphError, IngestOptions, OpenOptions, Predicate, RealFs, RuleDef, SnapshotOptions,
    SnapshotStop, Value,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

type GraphDb = mushroomdb::GraphDb<RealFs>;

/// The snapshot versions v0.9.2's `snapshot::decode` accepts: V5–V7 by their
/// own arms and V8–V10 through `MMAP_CONTAINER_VERSIONS`. Everything else lands
/// in its `other` arm and is refused with `snapshot: unsupported version {N}`.
const V0_9_2_ACCEPTS: [u16; 6] = [5, 6, 7, 8, 9, 10];

/// Section 17 (TX-6's derived bits) and section 18 (V11's declarations).
const SECTION_DERIVED_BITS: u8 = 17;
const SECTION_DECLARATIONS: u8 = 18;

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "graphdb-snapshot-v11-{}-{}-{}",
        name,
        std::process::id(),
        nanos
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn peek_snapshot_version(dir: &Path) -> Option<u16> {
    mushroomdb::snapshot_version_at(dir).expect("peek snapshot version")
}

fn wal_bytes(dir: &Path) -> Vec<u8> {
    std::fs::read(dir.join("wal.bin")).unwrap_or_default()
}

/// The section ids in `snapshot.bin`'s directory.
fn section_ids(dir: &Path) -> Vec<u8> {
    let bytes = std::fs::read(dir.join("snapshot.bin")).expect("snapshot.bin");
    let n = u16::from_le_bytes([bytes[6], bytes[7]]) as usize;
    (0..n).map(|i| bytes[8 + 16 * i]).collect()
}

fn date(days: i32) -> Value {
    Value::Date(days)
}

fn props(pairs: &[(&str, Value)]) -> Vec<(String, Value)> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), v.clone()))
        .collect()
}

/// One of each 0.10 value type.
fn typed_values() -> Vec<(&'static str, Value)> {
    vec![
        ("d", Value::Date(19_737)),
        ("ldt", Value::LocalDateTime(1_705_322_096_789_012)),
        (
            "dt",
            Value::DateTime {
                micros_utc: 1_705_302_296_789_012,
                offset_seconds: 19_800,
            },
        ),
        (
            "dur",
            Value::Duration {
                months: 14,
                days: 3,
                nanos: 1,
            },
        ),
        ("bin", Value::Binary(vec![0xDE, 0xAD, 0xBE, 0xEF])),
        ("listed", Value::List(vec![Value::Int(1), Value::Date(0)])),
    ]
}

fn email_rule() -> RuleDef {
    RuleDef {
        name: "same_day".into(),
        src_label: "E".into(),
        dst_label: "E".into(),
        predicate: Predicate::FieldEqual { field: "on".into() },
        edge_type: "SAME_DAY".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
        namespace: None,
    }
}

// ── 1. every write path upgrades at its first V11-only record ──────────────

#[test]
fn test_v11_write_batch_triggers_upgrade() {
    let dir = tmp("write-batch");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("E", "e0", props(&[("n", Value::Int(1))]))
        .unwrap();
    let before_version = peek_snapshot_version(&dir);
    assert!(before_version.is_none_or(|v| v <= 10));
    assert!(!db.is_v11());
    let wal_before = wal_bytes(&dir);
    db.write_batch(|b| {
        b.insert_node("E", "e1", props(&[("on", date(19_737))]));
    })
    .unwrap();
    assert_eq!(peek_snapshot_version(&dir), Some(11));
    assert!(db.is_v11());
    // The upgrade kept the WAL: what was there is still its prefix, so
    // `open_at` reaches the history from before the upgrade.
    let wal_after = wal_bytes(&dir);
    assert!(wal_after.starts_with(&wal_before));
    assert!(wal_after.len() > wal_before.len());
    drop(db);
    let reopened = GraphDb::open(&dir).unwrap();
    assert!(reopened.is_v11());
    assert_eq!(reopened.get_prop("e1", "on"), Some(date(19_737)));
}

#[test]
fn test_v11_ingest_batch_triggers_upgrade() {
    let dir = tmp("ingest");
    let mut db = GraphDb::open(&dir).unwrap();
    let row = BTreeMap::from([
        ("id".to_string(), Value::Str("e1".into())),
        ("on".to_string(), date(19_737)),
    ]);
    db.ingest("E", vec![row], &IngestOptions::default())
        .unwrap();
    assert_eq!(peek_snapshot_version(&dir), Some(11));
    let json = r#"[{"id": "e2", "on": {"$date": "2024-01-15"}}]"#;
    db.ingest_json("E", json, &IngestOptions::default())
        .unwrap();
    drop(db);
    let reopened = GraphDb::open(&dir).unwrap();
    assert_eq!(reopened.get_prop("e1", "on"), Some(date(19_737)));
    assert_eq!(reopened.get_prop("e2", "on"), Some(date(19_737)));
}

#[test]
fn test_v11_cypher_write_triggers_upgrade() {
    let dir = tmp("cypher");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("E", "e1", vec![]).unwrap();
    db.query_write(
        "MATCH (n:E) SET n.on = date('2024-01-15')",
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(peek_snapshot_version(&dir), Some(11));
    assert_eq!(db.get_prop("e1", "on"), Some(date(19_737)));
    drop(db);
    assert_eq!(
        GraphDb::open(&dir).unwrap().get_prop("e1", "on"),
        Some(date(19_737))
    );
}

/// A write whose rule fires: the derived edges come from the Date values, and
/// the store is V11 before either frame — the write's or the derived-edge
/// history marker — is appended.
#[test]
fn test_v11_rule_derived_write_triggers_upgrade() {
    let dir = tmp("rule");
    let mut db = GraphDb::open(&dir).unwrap();
    db.create_rule(email_rule()).unwrap();
    assert_ne!(peek_snapshot_version(&dir), Some(11));
    db.insert_node("E", "e1", props(&[("on", date(19_737))]))
        .unwrap();
    assert_eq!(peek_snapshot_version(&dir), Some(11));
    db.insert_node("E", "e2", props(&[("on", date(19_737))]))
        .unwrap();
    db.insert_node("E", "e3", props(&[("on", date(1))]))
        .unwrap();
    let live_edges = db.edge_count();
    assert_eq!(live_edges, 2, "e1 and e2 share a day, both directions");
    drop(db);
    let reopened = GraphDb::open(&dir).unwrap();
    assert_eq!(reopened.edge_count(), live_edges);
}

#[test]
fn every_single_mutation_path_upgrades() {
    for (field, value) in typed_values() {
        // insert_node
        let dir = tmp("insert-node");
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "a", props(&[(field, value.clone())]))
            .unwrap();
        assert_eq!(peek_snapshot_version(&dir), Some(11), "{field}");
        drop(db);
        assert_eq!(
            GraphDb::open(&dir).unwrap().get_prop("a", field),
            Some(value.clone())
        );
        // set_prop
        let dir = tmp("set-prop");
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "a", vec![]).unwrap();
        db.set_prop("a", field, value.clone()).unwrap();
        assert_eq!(peek_snapshot_version(&dir), Some(11), "{field}");
        drop(db);
        assert_eq!(
            GraphDb::open(&dir).unwrap().get_prop("a", field),
            Some(value.clone())
        );
        // set_props
        let dir = tmp("set-props");
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "a", vec![]).unwrap();
        db.set_props("a", props(&[("x", Value::Int(1)), (field, value.clone())]))
            .unwrap();
        assert_eq!(peek_snapshot_version(&dir), Some(11), "{field}");
        drop(db);
        assert_eq!(
            GraphDb::open(&dir).unwrap().get_prop("a", field),
            Some(value)
        );
    }
}

#[test]
fn a_constraint_declaration_upgrades() {
    let dir = tmp("constraint");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("U", "a", props(&[("email", Value::Str("a@x".into()))]))
        .unwrap();
    db.create_constraint("uniq_email", "U", "email").unwrap();
    assert_eq!(peek_snapshot_version(&dir), Some(11));
    assert!(matches!(
        db.insert_node("U", "b", props(&[("email", Value::Str("a@x".into()))])),
        Err(GraphError::ConstraintViolation { .. })
    ));
}

#[test]
fn a_range_index_declaration_upgrades() {
    let dir = tmp("range-index");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("U", "a", props(&[("age", Value::Int(3))]))
        .unwrap();
    db.enable_range_index("by_age", "U", "age").unwrap();
    assert_eq!(peek_snapshot_version(&dir), Some(11));
}

/// A declaration refused by the data does no upgrade: the conflict scan runs
/// before the record is built.
#[test]
fn a_refused_constraint_does_not_upgrade() {
    let dir = tmp("refused-constraint");
    let mut db = GraphDb::open(&dir).unwrap();
    for k in ["a", "b"] {
        db.insert_node("U", k, props(&[("email", Value::Str("a@x".into()))]))
            .unwrap();
    }
    let wal_before = wal_bytes(&dir);
    assert!(matches!(
        db.create_constraint("uniq_email", "U", "email"),
        Err(GraphError::ConstraintCreationConflict { .. })
    ));
    assert!(!db.is_v11());
    assert_eq!(peek_snapshot_version(&dir), None);
    assert_eq!(wal_bytes(&dir), wal_before);
}

// ── 2. a store that never writes one stays 0.9.x-shaped ────────────────────

#[test]
fn test_v9_store_stays_v9_without_v11_features() {
    let dir = tmp("stays-v9");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("X", "x1", props(&[("val", Value::Int(1))]))
        .unwrap();
    db.snapshot().unwrap();
    db.insert_node("X", "x2", props(&[("val", Value::Str("hello".into()))]))
        .unwrap();
    db.enable_index("X", "val").unwrap();
    db.snapshot_with(SnapshotOptions {
        keep_wal: true,
        ..SnapshotOptions::default()
    })
    .unwrap();
    assert_eq!(peek_snapshot_version(&dir), Some(9));
    assert!(!section_ids(&dir).contains(&SECTION_DECLARATIONS));
    assert!(!db.is_v11());
    drop(db);
    let reopened = GraphDb::open(&dir).unwrap();
    assert!(!reopened.is_v11());
}

#[test]
fn a_multiplicity_store_without_v11_features_stays_v10() {
    let dir = tmp("stays-v10");
    let mut db = GraphDb::open(&dir).unwrap();
    db.enable_multiplicity().unwrap();
    db.insert_node("X", "a", vec![]).unwrap();
    db.snapshot().unwrap();
    assert_eq!(peek_snapshot_version(&dir), Some(10));
    assert!(!section_ids(&dir).contains(&SECTION_DECLARATIONS));
}

/// A read does not upgrade: a query can take and return a typed parameter on
/// a store that never stores one.
#[test]
fn a_read_with_a_typed_parameter_does_not_upgrade() {
    let dir = tmp("read");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("N", "a", vec![]).unwrap();
    for (_, value) in typed_values() {
        let params = BTreeMap::from([("d".to_string(), value.clone())]);
        let rs = db.query("MATCH (n:N) RETURN $d AS d", &params).unwrap();
        assert_eq!(rs.row(0)[0].as_ref(), Some(&value));
    }
    assert!(!db.is_v11());
    assert_eq!(peek_snapshot_version(&dir), None);
}

// ── 3. V11 is sticky ───────────────────────────────────────────────────────

#[test]
fn dropping_the_last_constraint_keeps_v11() {
    let dir = tmp("drop-constraint");
    let mut db = GraphDb::open(&dir).unwrap();
    db.create_constraint("c", "U", "email").unwrap();
    db.drop_constraint("c").unwrap();
    assert!(db.constraints().is_empty());
    db.snapshot().unwrap();
    assert_eq!(peek_snapshot_version(&dir), Some(11));
    drop(db);
    // Reopened from a truncated WAL that holds no V11 record any more: the
    // snapshot's own version is what keeps the store V11.
    let mut reopened = GraphDb::open(&dir).unwrap();
    assert!(reopened.is_v11());
    reopened.insert_node("U", "a", vec![]).unwrap();
    reopened.snapshot().unwrap();
    assert_eq!(peek_snapshot_version(&dir), Some(11));
}

#[test]
fn dropping_the_last_range_index_keeps_v11() {
    let dir = tmp("drop-range");
    let mut db = GraphDb::open(&dir).unwrap();
    db.enable_range_index("r", "U", "age").unwrap();
    db.disable_range_index("r").unwrap();
    db.snapshot().unwrap();
    assert_eq!(peek_snapshot_version(&dir), Some(11));
}

#[test]
fn overwriting_the_last_typed_value_keeps_v11() {
    let dir = tmp("overwrite");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("N", "a", props(&[("on", date(1))])).unwrap();
    db.set_prop("a", "on", Value::Int(1)).unwrap();
    db.snapshot().unwrap();
    assert_eq!(peek_snapshot_version(&dir), Some(11));
    db.snapshot().unwrap();
    assert_eq!(peek_snapshot_version(&dir), Some(11));
}

/// A WAL replay that meets a V11-only record makes the handle V11 even when
/// the snapshot beside it is not V11 — the shape a restored backup or a
/// replaced snapshot leaves.
#[test]
fn replaying_a_v11_record_over_a_v9_snapshot_makes_the_store_v11() {
    let dir = tmp("replay-detect");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("N", "a", vec![]).unwrap();
    db.snapshot().unwrap();
    assert_eq!(peek_snapshot_version(&dir), Some(9));
    let v9_snapshot = std::fs::read(dir.join("snapshot.bin")).unwrap();
    db.set_prop("a", "on", date(5)).unwrap();
    drop(db);
    // Put the V9 snapshot back: the WAL now replays the Date over it.
    std::fs::write(dir.join("snapshot.bin"), &v9_snapshot).unwrap();
    let mut reopened = GraphDb::open(&dir).unwrap();
    assert!(reopened.is_v11());
    assert_eq!(reopened.get_prop("a", "on"), Some(date(5)));
    reopened.snapshot().unwrap();
    assert_eq!(peek_snapshot_version(&dir), Some(11));
}

#[test]
fn open_at_sees_the_upgrade_where_it_happened() {
    let dir = tmp("open-at");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("N", "a", props(&[("n", Value::Int(1))]))
        .unwrap();
    db.set_prop("a", "on", date(7)).unwrap();
    let total = db.wal_total_commits().unwrap();
    drop(db);
    // Beside its V11 snapshot, every as-of handle is V11: the stamp is the
    // store's word, whatever the replayed prefix holds.
    let before = GraphDb::open_at(&dir, 0).unwrap();
    assert!(before.is_v11());
    assert_eq!(before.get_prop("a", "on"), None);
    drop(before);
    // From the WAL alone (the upgrade kept it), the replay is what says it.
    std::fs::remove_file(dir.join("snapshot.bin")).unwrap();
    let before = GraphDb::open_at(&dir, 0).unwrap();
    assert!(!before.is_v11(), "commit 0 holds no V11 record");
    let after = GraphDb::open_at(&dir, total - 1).unwrap();
    assert!(after.is_v11());
    assert_eq!(after.get_prop("a", "on"), Some(date(7)));
}

// ── 4. the upgrade is ordered ──────────────────────────────────────────────

/// The V11 stamp is on disk before the record: a snapshot that fails leaves
/// the WAL as it was and the handle not V11, and the write can be retried.
#[test]
fn a_failed_upgrade_appends_nothing() {
    for stop in [
        SnapshotStop::BeforeSnapshotWrite,
        SnapshotStop::AfterSnapshotWrite,
    ] {
        let dir = tmp("failed-upgrade");
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "a", vec![]).unwrap();
        let wal_before = wal_bytes(&dir);
        db.stop_next_snapshot(stop);
        assert!(db.set_prop("a", "on", date(1)).is_err(), "{stop:?}");
        assert_eq!(wal_bytes(&dir), wal_before, "{stop:?}");
        // A stamp that reached disk keeps the handle V11 (C9); one that did
        // not leaves it as it was.
        assert_eq!(
            db.is_v11(),
            stop == SnapshotStop::AfterSnapshotWrite,
            "{stop:?}"
        );
        assert_eq!(db.get_prop("a", "on"), None);
        // Whatever the snapshot left, the WAL holds no V11 record, so an
        // older reader that gets past the snapshot loses nothing.
        let (frames, _) = decode_all(&wal_bytes(&dir));
        assert!(frames.iter().all(|f| !matches!(
            f,
            WalRecord::SetPropId {
                value: Value::Date(_),
                ..
            }
        )));
        db.set_prop("a", "on", date(1)).unwrap();
        assert_eq!(peek_snapshot_version(&dir), Some(11));
        drop(db);
        assert_eq!(
            GraphDb::open(&dir).unwrap().get_prop("a", "on"),
            Some(date(1))
        );
    }
}

/// An older reader stops at the version and leaves the WAL byte-identical.
///
/// 0.9.2 accepts V5–V10 and refuses every other version through the same
/// `other` arm this binary uses for V12, with the text `snapshot: unsupported
/// version {N}` — so V12 here stands in for what 0.9.2 does with V11 (TY-13's
/// format-compat job runs the released binary itself).
#[test]
fn a_reader_that_does_not_know_v11_refuses_by_name_and_touches_nothing() {
    assert!(!V0_9_2_ACCEPTS.contains(&11));
    let dir = tmp("refusal");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("N", "a", props(&[("on", date(1))])).unwrap();
    db.create_constraint("c", "N", "on").unwrap();
    drop(db);
    assert_eq!(peek_snapshot_version(&dir), Some(11));
    let wal_before = wal_bytes(&dir);
    let mut snap = std::fs::read(dir.join("snapshot.bin")).unwrap();
    core_storage::snapshot::stamp_container_version(&mut snap, 12).unwrap();
    std::fs::write(dir.join("snapshot.bin"), &snap).unwrap();
    let msg = match GraphDb::open_with_options(&dir, OpenOptions::default()) {
        Ok(_) => panic!("an unknown snapshot version must refuse the open"),
        Err(e) => e.to_string(),
    };
    assert!(
        msg.contains("snapshot: unsupported version 12"),
        "the refusal must name the version; got: {msg}"
    );
    assert!(msg.contains("upgrade your binary"), "{msg}");
    assert_eq!(wal_bytes(&dir), wal_before);
}

// ── 5. section 18 carries what an archived-away WAL would lose ─────────────

/// The state a crash between an archive's rename and its baseline WAL leaves
/// (window B in `snapshot_with`): the snapshot, the archive, and no live WAL.
fn archive_then_lose_the_baseline(db: &mut GraphDb, dir: &Path) {
    db.snapshot_with(SnapshotOptions {
        archive_wal: true,
        ..SnapshotOptions::default()
    })
    .unwrap();
    std::fs::remove_file(dir.join("wal.bin")).unwrap();
}

#[test]
fn a_constraint_survives_a_lost_baseline_wal() {
    let dir = tmp("lost-baseline-constraint");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("U", "a", props(&[("email", Value::Str("a@x".into()))]))
        .unwrap();
    db.create_constraint("uniq_email", "U", "email").unwrap();
    archive_then_lose_the_baseline(&mut db, &dir);
    assert!(section_ids(&dir).contains(&SECTION_DECLARATIONS));
    drop(db);
    let mut reopened = GraphDb::open(&dir).unwrap();
    assert_eq!(reopened.constraints().len(), 1);
    assert!(matches!(
        reopened.insert_node("U", "b", props(&[("email", Value::Str("a@x".into()))])),
        Err(GraphError::ConstraintViolation { .. })
    ));
}

#[test]
fn multiplicity_on_a_v11_store_survives_a_lost_baseline_wal() {
    let dir = tmp("lost-baseline-multiplicity");
    let mut db = GraphDb::open(&dir).unwrap();
    db.enable_multiplicity().unwrap();
    db.insert_node("N", "a", props(&[("on", date(1))])).unwrap();
    assert_eq!(peek_snapshot_version(&dir), Some(11));
    archive_then_lose_the_baseline(&mut db, &dir);
    drop(db);
    let reopened = GraphDb::open(&dir).unwrap();
    assert!(reopened.is_multiplicity_enabled());
    assert!(reopened.is_v11());
}

/// A constraint dropped after a keep-WAL snapshot that carried it stays
/// dropped: the section is applied before the WAL, whose `DropConstraint`
/// then removes it.
#[test]
fn a_dropped_constraint_does_not_come_back_from_the_section() {
    let dir = tmp("dropped-after-section");
    let mut db = GraphDb::open(&dir).unwrap();
    db.create_constraint("c", "U", "email").unwrap();
    db.snapshot_with(SnapshotOptions {
        keep_wal: true,
        ..SnapshotOptions::default()
    })
    .unwrap();
    db.drop_constraint("c").unwrap();
    drop(db);
    let reopened = GraphDb::open(&dir).unwrap();
    assert!(reopened.constraints().is_empty());
}

/// A damaged section 18 refuses the open: silently losing a constraint would
/// let duplicates in.
#[test]
fn a_damaged_declarations_section_refuses_the_open() {
    let dir = tmp("damaged-section");
    let mut db = GraphDb::open(&dir).unwrap();
    db.create_constraint("c", "U", "email").unwrap();
    db.snapshot().unwrap();
    drop(db);
    let mut bytes = std::fs::read(dir.join("snapshot.bin")).unwrap();
    let n = u16::from_le_bytes([bytes[6], bytes[7]]) as usize;
    let entry = (0..n)
        .map(|i| &bytes[8 + 16 * i..8 + 16 * (i + 1)])
        .find(|e| e[0] == SECTION_DECLARATIONS)
        .expect("section 18");
    let offset = u32::from_le_bytes(entry[4..8].try_into().unwrap()) as usize;
    let len = u32::from_le_bytes(entry[8..12].try_into().unwrap()) as usize;
    bytes[offset + len - 1] ^= 0xFF;
    std::fs::write(dir.join("snapshot.bin"), &bytes).unwrap();
    let err = match GraphDb::open(&dir) {
        Ok(_) => panic!("a damaged section 18 must refuse the open"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("declarations"), "{err}");
}

// ── 6. section 17 and section 18 in one snapshot ───────────────────────────

#[test]
fn derived_bits_and_declarations_coexist_in_a_v11_snapshot() {
    let dir = tmp("derived-bits");
    let mut db = GraphDb::open(&dir).unwrap();
    db.create_rule(email_rule()).unwrap();
    for k in ["e1", "e2", "e3"] {
        db.insert_node("E", k, props(&[("on", date(19_737))]))
            .unwrap();
    }
    db.create_constraint("c", "E", "id").unwrap();
    db.snapshot().unwrap();
    let ids = section_ids(&dir);
    assert!(ids.contains(&SECTION_DERIVED_BITS), "{ids:?}");
    assert!(ids.contains(&SECTION_DECLARATIONS), "{ids:?}");
    assert_eq!(peek_snapshot_version(&dir), Some(11));
    let edges = db.edge_count();
    drop(db);
    let mut reopened = GraphDb::open(&dir).unwrap();
    assert_eq!(reopened.edge_count(), edges);
    // The derived edges read back as rule-owned from the bits.
    assert!(matches!(
        reopened.insert_edge("SAME_DAY", "e1", "e2"),
        Err(GraphError::RuleOwned { .. })
    ));
}

// ── values: refusals that stay, and round-trips over every axis ────────────

/// A DateTime offset beyond ±18:00 is refused at the write, whatever built
/// it; exactly ±18:00 is stored.
#[test]
fn a_datetime_offset_beyond_18_hours_is_refused() {
    let dir = tmp("offset");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("N", "a", vec![]).unwrap();
    let wal_before = wal_bytes(&dir);
    for bad in [18 * 3600 + 1, -(18 * 3600 + 1), i32::MAX, i32::MIN] {
        let v = Value::DateTime {
            micros_utc: 0,
            offset_seconds: bad,
        };
        match db.set_prop("a", "at", v.clone()) {
            Err(GraphError::QueryError { detail }) => {
                assert!(detail.contains("outside ±18:00"), "{detail}")
            }
            other => panic!("offset {bad}: expected a refusal, got {other:?}"),
        }
        assert!(db
            .set_prop("a", "at", Value::List(vec![v.clone()]))
            .is_err());
    }
    assert_eq!(wal_bytes(&dir), wal_before);
    assert!(!db.is_v11(), "a refused value does not upgrade");
    for edge in [18 * 3600, -18 * 3600] {
        let v = Value::DateTime {
            micros_utc: 0,
            offset_seconds: edge,
        };
        db.set_prop("a", "at", v.clone()).unwrap();
        assert_eq!(db.get_prop("a", "at"), Some(v));
    }
}

/// `Value::Null` is never stored (QL-17), on a V11 store as on any other.
#[test]
fn a_v11_store_still_refuses_null() {
    let dir = tmp("null");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("N", "a", props(&[("on", date(1))])).unwrap();
    assert!(db.is_v11());
    let wal_before = wal_bytes(&dir);
    assert!(db.set_prop("a", "x", Value::Null).is_err());
    assert!(db
        .insert_node("N", "b", props(&[("x", Value::List(vec![Value::Null]))]))
        .is_err());
    assert_eq!(wal_bytes(&dir), wal_before);
}

/// Every typed value reads the same live, after a snapshot, after a second
/// snapshot over a V11 base (the merge path), after a reopen, and through
/// `open_at` — written one batch at a time or all at once.
#[test]
fn typed_values_round_trip_over_every_axis() {
    let values = typed_values();
    let expect = |db: &GraphDb, what: &str| {
        for (i, (field, v)) in values.iter().enumerate() {
            let key = format!("n{i}");
            assert_eq!(db.get_prop(&key, field), Some(v.clone()), "{what}: {field}");
            // A second node so each column has a gap and a neighbour.
            assert_eq!(
                db.get_prop("other", field),
                Some(Value::Int(i as i64)),
                "{what}: other.{field}"
            );
        }
    };
    for one_batch in [false, true] {
        let dir = tmp("axes");
        let mut db = GraphDb::open(&dir).unwrap();
        let other: Vec<(String, Value)> = values
            .iter()
            .enumerate()
            .map(|(i, (f, _))| ((*f).to_string(), Value::Int(i as i64)))
            .collect();
        if one_batch {
            db.write_batch(|b| {
                b.insert_node("N", "other", other.clone());
                for (i, (field, v)) in values.iter().enumerate() {
                    b.insert_node("N", &format!("n{i}"), props(&[(field, v.clone())]));
                }
            })
            .unwrap();
        } else {
            db.insert_node("N", "other", other.clone()).unwrap();
            for (i, (field, v)) in values.iter().enumerate() {
                db.insert_node("N", &format!("n{i}"), props(&[(field, v.clone())]))
                    .unwrap();
            }
        }
        expect(&db, "live");
        let total = db.wal_total_commits().unwrap();
        let as_of = GraphDb::open_at(&dir, total - 1).unwrap();
        expect(&as_of, "open_at");
        drop(as_of);
        db.snapshot().unwrap();
        expect(&db, "after snapshot");
        db.insert_node("N", "late", props(&[("d", date(3))]))
            .unwrap();
        db.snapshot().unwrap();
        expect(&db, "after a merge snapshot");
        drop(db);
        let reopened = GraphDb::open(&dir).unwrap();
        expect(&reopened, "reopened");
        assert_eq!(reopened.get_prop("late", "d"), Some(date(3)));
        assert_eq!(peek_snapshot_version(&dir), Some(11));
    }
}

// ── fix round 1: the upgrade and group commit (I1) ─────────────────────────

fn copy_files(from: &Path, to: &Path) {
    let _ = std::fs::remove_dir_all(to);
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        if e.file_type().unwrap().is_file() && e.file_name() != "LOCK" {
            std::fs::copy(e.path(), to.join(e.file_name())).unwrap();
        }
    }
}

/// A group commit holding a plain submission (`b`) and then the store's first
/// `Date` (`c`). The upgrade must run ahead of the group, so its snapshot holds
/// only `a`, which an earlier group made durable — never `b`, which the group
/// fsync below fails and truncates.
///
/// Before the fix the upgrade ran mid-group, at `c`'s rewrite: the snapshot
/// held `b` before its fsync, the failed fsync truncated `b`'s frame and told
/// its submitter `Err`, and the reopened store had `b` anyway.
///
/// The same moment is also taken as a crash image — the files as a kill
/// between the upgrade and the group fsync leaves them — with the group's
/// unsynced tail kept and with it lost. Each reopens to exactly a durable
/// state: `a` alone, or `a` with the whole group.
#[test]
fn a_group_commit_upgrades_ahead_of_its_first_append() {
    use mushroomdb::{BatchOp, SharedDb};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc, Mutex};

    let dir = tmp("group-upgrade");
    let kept = tmp("group-upgrade-crash-kept");
    let lost = tmp("group-upgrade-crash-lost");
    let calls = Arc::new(AtomicUsize::new(0));
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = Mutex::new(release_rx);
    let pre_group_len = Arc::new(Mutex::new(0u64));
    let (c2, p2, d2, k2, l2) = (
        Arc::clone(&calls),
        Arc::clone(&pre_group_len),
        dir.clone(),
        kept.clone(),
        lost.clone(),
    );
    let db = SharedDb::open_with_test_sync(&dir, move |path| {
        match c2.fetch_add(1, Ordering::SeqCst) {
            // The first group (`a`): held until `b` and `c` are queued, so
            // they drain together as the next group.
            0 => {
                let r = core_storage::sync_wal_at(path);
                *p2.lock().unwrap() = std::fs::metadata(d2.join("wal.bin")).unwrap().len();
                release_rx.lock().unwrap().recv().unwrap();
                r
            }
            // The second group (`b`, `c`): take the crash images, then fail.
            1 => {
                copy_files(&d2, &k2);
                copy_files(&d2, &l2);
                let pre = *p2.lock().unwrap();
                core_storage::truncate_wal_at(&l2, pre).unwrap();
                Err(std::io::Error::other("injected fsync failure"))
            }
            _ => core_storage::sync_wal_at(path),
        }
    })
    .unwrap();
    let node = |key: &str, props: Vec<(String, Value)>| BatchOp::InsertNode {
        label: "N".into(),
        key: key.into(),
        props,
    };
    let a = {
        let db = db.clone();
        let op = node("a", vec![("n".into(), Value::Int(1))]);
        std::thread::spawn(move || db.submit_batch(vec![op]))
    };
    // Let `a` reach its (held) fsync, then queue `b` before `c`.
    while calls.load(Ordering::SeqCst) == 0 {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let b = {
        let db = db.clone();
        let op = node("b", vec![("n".into(), Value::Int(2))]);
        std::thread::spawn(move || db.submit_batch(vec![op]))
    };
    std::thread::sleep(std::time::Duration::from_millis(150));
    let c = {
        let db = db.clone();
        let op = node("c", vec![("on".into(), date(1))]);
        std::thread::spawn(move || db.submit_batch(vec![op]))
    };
    std::thread::sleep(std::time::Duration::from_millis(150));
    release_tx.send(()).unwrap();
    assert!(a.join().unwrap().is_ok());
    assert!(b.join().unwrap().is_err(), "b's group fsync failed");
    assert!(c.join().unwrap().is_err(), "c's group fsync failed");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "b and c drained as one group"
    );
    drop(db);

    assert_eq!(peek_snapshot_version(&dir), Some(11), "the upgrade ran");
    let reopened = GraphDb::open(&dir).unwrap();
    assert!(reopened.node_info("a").is_some());
    assert!(
        reopened.node_info("b").is_none(),
        "a submission told Err must not be durable"
    );
    assert!(reopened.node_info("c").is_none());
    assert!(reopened.is_v11());
    drop(reopened);

    let lost_db = GraphDb::open(&lost).unwrap();
    assert!(lost_db.node_info("a").is_some());
    assert!(
        lost_db.node_info("b").is_none(),
        "the lost tail took b with it"
    );
    assert!(lost_db.node_info("c").is_none());
    let kept_db = GraphDb::open(&kept).unwrap();
    assert!(kept_db.node_info("a").is_some());
    assert!(kept_db.node_info("b").is_some());
    assert_eq!(kept_db.get_prop("c", "on"), Some(date(1)));
}

// ── fix round 1: section 18 replays at the snapshot's own point (I2) ───────

/// What `is_index_enabled`, the constraints and the range indexes say, for
/// comparing a reopened store against the never-closed one.
fn declared(db: &GraphDb, pairs: &[(&str, &str)]) -> String {
    let indexes: Vec<bool> = pairs
        .iter()
        .map(|(l, f)| db.is_index_enabled(l, f))
        .collect();
    format!(
        "indexes {indexes:?} constraints {:?} ranges {:?}",
        db.constraints(),
        db.range_indexes()
    )
}

/// The review's reproduction: a constraint replaced under the same name, then
/// a keep-WAL snapshot. The section says `c1` covers `b`; the held WAL prefix
/// says `a` was indexed first. The reopened store must keep both indexes, as
/// the never-closed one does — and so must the store after a later truncating
/// snapshot, where the loss would become permanent.
#[test]
fn a_replaced_constraint_reopens_as_the_never_closed_store() {
    let pairs = [("L", "a"), ("L", "b")];
    for keep_then_truncate in [false, true] {
        let dir = tmp("i2-constraint");
        let mut db = GraphDb::open(&dir).unwrap();
        db.create_constraint("c1", "L", "a").unwrap();
        db.drop_constraint("c1").unwrap();
        db.create_constraint("c1", "L", "b").unwrap();
        db.snapshot_with(SnapshotOptions {
            keep_wal: true,
            ..SnapshotOptions::default()
        })
        .unwrap();
        let live = declared(&db, &pairs);
        assert!(db.is_index_enabled("L", "a") && db.is_index_enabled("L", "b"));
        drop(db);
        let mut reopened = GraphDb::open(&dir).unwrap();
        assert_eq!(declared(&reopened, &pairs), live, "reopened");
        if keep_then_truncate {
            reopened.snapshot().unwrap();
            drop(reopened);
            let again = GraphDb::open(&dir).unwrap();
            assert_eq!(
                declared(&again, &pairs),
                live,
                "after a truncating snapshot"
            );
        }
    }
}

#[test]
fn a_replaced_index_reopens_as_the_never_closed_store() {
    let pairs = [("L", "a"), ("L", "b")];
    let dir = tmp("i2-index");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("L", "x", props(&[("on", date(1))])).unwrap(); // V11
    db.enable_index("L", "a").unwrap();
    db.disable_index("L", "a").unwrap();
    db.enable_index("L", "b").unwrap();
    db.snapshot_with(SnapshotOptions {
        keep_wal: true,
        ..SnapshotOptions::default()
    })
    .unwrap();
    let live = declared(&db, &pairs);
    drop(db);
    assert_eq!(declared(&GraphDb::open(&dir).unwrap(), &pairs), live);
}

#[test]
fn a_replaced_range_index_reopens_as_the_never_closed_store() {
    let pairs = [("L", "a"), ("L", "b")];
    let dir = tmp("i2-range");
    let mut db = GraphDb::open(&dir).unwrap();
    db.enable_range_index("r", "L", "a").unwrap();
    db.disable_range_index("r").unwrap();
    db.enable_range_index("r", "L", "b").unwrap();
    db.snapshot_with(SnapshotOptions {
        keep_wal: true,
        ..SnapshotOptions::default()
    })
    .unwrap();
    let live = declared(&db, &pairs);
    drop(db);
    assert_eq!(declared(&GraphDb::open(&dir).unwrap(), &pairs), live);
}

/// Section 18 now carries equality and range indexes too, so a V11 store's
/// lost baseline WAL (window B) keeps them.
#[test]
fn indexes_survive_a_lost_baseline_wal() {
    let dir = tmp("lost-baseline-indexes");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node(
        "L",
        "x",
        props(&[("a", Value::Int(1)), ("b", Value::Int(2))]),
    )
    .unwrap();
    db.enable_index("L", "a").unwrap();
    db.enable_range_index("r", "L", "b").unwrap();
    archive_then_lose_the_baseline(&mut db, &dir);
    drop(db);
    let reopened = GraphDb::open(&dir).unwrap();
    assert!(reopened.is_index_enabled("L", "a"));
    assert_eq!(
        reopened.range_indexes(),
        vec![("r".to_string(), "L".to_string(), "b".to_string())]
    );
}

// ── fix round 1: as-of handles and failed upgrades (M1, M2) ───────────────

#[test]
fn an_as_of_handle_over_a_truncated_v11_store_is_v11() {
    let dir = tmp("asof-v11");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("N", "a", props(&[("on", date(1))])).unwrap();
    db.snapshot().unwrap();
    db.insert_node("N", "b", vec![]).unwrap();
    let total = db.wal_total_commits().unwrap();
    drop(db);
    assert!(GraphDb::open_at(&dir, total - 1).unwrap().is_v11());
}

/// A V11 stamp that reached disk keeps the handle V11 even when the upgrade
/// failed after it: a later snapshot must not write V9 over it (C9).
#[test]
fn a_v11_stamp_on_disk_survives_a_failed_upgrade() {
    let dir = tmp("failed-after-stamp");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("N", "a", vec![]).unwrap();
    db.stop_next_snapshot(SnapshotStop::AfterSnapshotWrite);
    assert!(db.set_prop("a", "on", date(1)).is_err());
    assert_eq!(peek_snapshot_version(&dir), Some(11));
    assert!(db.is_v11());
    db.snapshot().unwrap();
    assert_eq!(peek_snapshot_version(&dir), Some(11));
}

// ── fix round 1: the genesis marker a 0.9.x `open_at` reads (I4) ──────────

/// A V11 store keeps its genesis-chain marker as `wal.genesis.v11`, so a
/// 0.9.x `open_at` — which replays archives from empty on the strength of
/// `wal.genesis` alone — refuses archive-resident commits instead of answering
/// from archives it cannot fully decode. 0.10 still reaches every one.
#[test]
fn a_v11_store_keeps_its_genesis_marker_under_the_v11_name() {
    let dir = tmp("genesis-v11");
    let arch = SnapshotOptions {
        archive_wal: true,
        ..SnapshotOptions::default()
    };
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("N", "a", vec![]).unwrap();
    db.snapshot_with(arch.clone()).unwrap();
    assert!(dir.join("wal.genesis").exists(), "a pre-V11 store's marker");
    db.insert_node("N", "c", props(&[("on", date(1))])).unwrap();
    assert!(!dir.join("wal.genesis").exists(), "renamed at the upgrade");
    assert!(dir.join("wal.genesis.v11").exists());
    db.insert_node("N", "d", vec![]).unwrap();
    db.snapshot_with(arch).unwrap();
    db.insert_node("N", "e", vec![]).unwrap();
    assert!(!dir.join("wal.genesis").exists());
    let total = db.wal_total_commits().unwrap();
    drop(db);
    // Every commit, archive-resident or not, is reachable by this binary.
    for commit in 0..total {
        GraphDb::open_at(&dir, commit).unwrap_or_else(|e| panic!("commit {commit}: {e}"));
    }
    let at_c = GraphDb::open_at(&dir, 1).unwrap();
    assert_eq!(at_c.get_prop("c", "on"), Some(date(1)));
    // A copy made by `backup_to` keeps the chain.
    let copy = tmp("genesis-v11-copy");
    GraphDb::open(&dir).unwrap().backup_to(&copy).unwrap();
    assert!(copy.join("wal.genesis.v11").exists());
    assert_eq!(
        GraphDb::open_at(&copy, 1).unwrap().get_prop("c", "on"),
        Some(date(1))
    );
}

/// A store that never opts in keeps `wal.genesis`, as 0.9.x wrote it.
#[test]
fn a_non_v11_store_keeps_wal_genesis() {
    let dir = tmp("genesis-v9");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("N", "a", vec![]).unwrap();
    db.snapshot_with(SnapshotOptions {
        archive_wal: true,
        ..SnapshotOptions::default()
    })
    .unwrap();
    db.insert_node("N", "b", vec![]).unwrap();
    db.snapshot().unwrap();
    assert!(!dir.join("wal.genesis.v11").exists());
}
