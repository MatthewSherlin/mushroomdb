//! Range indexes (0.10 TY-8).
//!
//! A range index is a named declaration on `(label, field)` over the same
//! sorted postings the equality index keeps, so it answers ranges and string
//! prefixes as well as exact matches. Its declaration is one WAL record; the
//! postings are rebuilt from the stored values at every open. These tests hold
//! the postings to the same answer live, reopened, snapshotted (truncating and
//! `keep_wal`), batched or not, and as of a past commit.
//!
//! `EnableRangeIndex` / `DisableRangeIndex` are format-V11 WAL records, which a
//! 0.9.x reader cannot decode. Until the V11 format lands (TY-12) the write
//! path refuses them like a V11 value, so a default build cannot declare a
//! range index at all — the first test below holds that. Everything else runs
//! on a handle the `test-hooks` hook unlocks, which is the path TY-12 opens
//! for real: `cargo test -p mushroomdb --features test-hooks --test range_index`.

use mushroomdb::{GraphError, RealFs, Value};
use std::path::{Path, PathBuf};

type GraphDb = mushroomdb::GraphDb<RealFs>;

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "graphdb-range-index-{}-{}-{}",
        name,
        std::process::id(),
        nanos,
    ))
}

fn wal_bytes(dir: &Path) -> Vec<u8> {
    std::fs::read(dir.join("wal.bin")).unwrap_or_default()
}

fn val(i: i64) -> Vec<(String, Value)> {
    vec![("val".into(), Value::Int(i))]
}

const RANGE_REFUSAL: &str = "not supported in mushroomdb 0.10 yet: range indexes cannot be \
                             declared until the V11 format lands";

#[test]
fn enable_range_index_is_refused_until_the_v11_format_lands() {
    let dir = tmp("locked");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Score", "s1", val(1)).unwrap();
    let before = wal_bytes(&dir);
    match db.enable_range_index("idx", "Score", "val") {
        Err(GraphError::QueryError { detail }) => assert_eq!(detail, RANGE_REFUSAL),
        other => panic!("expected the V11 record refusal, got {other:?}"),
    }
    assert_eq!(
        wal_bytes(&dir),
        before,
        "a refused declaration was appended"
    );
    assert!(db.range_indexes().is_empty());
    assert!(!db.is_index_enabled("Score", "val"));
    match db.disable_range_index("idx") {
        Err(GraphError::QueryError { detail }) => assert_eq!(detail, "no range index named 'idx'"),
        other => panic!("expected an unknown-name refusal, got {other:?}"),
    }
    drop(db);
    let reopened = GraphDb::open(&dir).unwrap();
    assert!(reopened.range_indexes().is_empty());
    assert!(!reopened.is_index_enabled("Score", "val"));
}

/// Ledger row 125: an equality index answered `= 2` with the `Int` 2 only,
/// where a scan also finds the `Float` 2.0 — so declaring an index changed a
/// query's answer. The index now answers as the scan does.
#[test]
fn an_equality_index_finds_int_and_float_alike() {
    let dir = tmp("row-125");
    let mut db = GraphDb::open(&dir).unwrap();
    for (k, v) in [
        ("i2", Value::Int(2)),
        ("f2", Value::Float(2.0)),
        ("f25", Value::Float(2.5)),
        ("z", Value::Int(0)),
        ("nz", Value::Float(-0.0)),
    ] {
        db.insert_node("U", k, vec![("v".into(), v.clone())])
            .unwrap();
        db.insert_node("Plain", &format!("p-{k}"), vec![("v".into(), v)])
            .unwrap();
    }
    db.enable_index("U", "v").unwrap();
    let keys = |db: &GraphDb, label: &str, x: Value| {
        let mut params = std::collections::BTreeMap::new();
        params.insert("x".to_string(), x);
        let rs = db
            .query(
                &format!("MATCH (n:{label} {{v: $x}}) RETURN key(n) AS k"),
                &params,
            )
            .unwrap();
        let mut ks: Vec<String> = (0..rs.len())
            .filter_map(|i| match rs.get(i, "k") {
                Some(Value::Str(k)) => Some(k.trim_start_matches("p-").to_string()),
                _ => None,
            })
            .collect();
        ks.sort();
        ks
    };
    for x in [
        Value::Int(2),
        Value::Float(2.0),
        Value::Int(0),
        Value::Float(0.0),
    ] {
        let scanned = keys(&db, "Plain", x.clone());
        assert_eq!(keys(&db, "U", x.clone()), scanned, "= {x:?}");
    }
    assert_eq!(keys(&db, "U", Value::Int(2)), vec!["f2", "i2"]);
    drop(db);
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(keys(&db, "U", Value::Float(0.0)), vec!["nz", "z"]);
}

// ── Ledger row 123: an equality index over a view property (TY-5b) ─────────

fn user_deg_view() -> mushroomdb::ViewDef {
    mushroomdb::ViewDef {
        name: "deg".into(),
        label: "User".into(),
        view_prop: "deg".into(),
        source: mushroomdb::ViewSource::Degree {
            edge_type: "KNOWS".into(),
            direction: mushroomdb::Direction::Out,
        },
    }
}

/// Keys of the Users the query `WHERE n.deg = 1` returns.
fn users_with_deg_1(db: &GraphDb) -> Vec<String> {
    let rs = db
        .query(
            "MATCH (n:User) WHERE n.deg = 1 RETURN key(n) AS k",
            &std::collections::BTreeMap::new(),
        )
        .unwrap();
    let mut ks: Vec<String> = (0..rs.len())
        .filter_map(|i| match rs.get(i, "k") {
            Some(Value::Str(k)) => Some(k.clone()),
            _ => None,
        })
        .collect();
    ks.sort();
    ks
}

/// A view writes its property without passing the property index, so an
/// equality index over it answered from stale postings until a reopen. The two
/// declarations are now refused in either order, with the WAL untouched.
#[test]
fn an_equality_index_cannot_cover_a_view_property() {
    let dir = tmp("row-123-view-then-index");
    let mut db = GraphDb::open(&dir).unwrap();
    db.create_view(user_deg_view()).unwrap();
    let before = wal_bytes(&dir);
    match db.enable_index("User", "deg") {
        Err(GraphError::QueryError { detail }) => assert_eq!(
            detail,
            "property 'deg' on label 'User' is owned by view 'deg'; an equality index cannot \
             cover it"
        ),
        other => panic!("expected a view refusal, got {other:?}"),
    }
    assert_eq!(wal_bytes(&dir), before);
    assert!(!db.is_index_enabled("User", "deg"));
    // Another label's `deg` is not the view's.
    db.enable_index("Admin", "deg").unwrap();

    let dir = tmp("row-123-index-then-view");
    let mut db = GraphDb::open(&dir).unwrap();
    db.enable_index("User", "deg").unwrap();
    let before = wal_bytes(&dir);
    match db.create_view(user_deg_view()) {
        Err(GraphError::QueryError { detail }) => assert_eq!(
            detail,
            "property 'deg' on label 'User' is covered by an equality index; a view cannot own it"
        ),
        other => panic!("expected an index refusal, got {other:?}"),
    }
    assert_eq!(wal_bytes(&dir), before);
    assert!(db.views().is_empty());
}

/// The review's reproduction: a store that already holds an equality index and
/// a view over one property — written before the refusal existed — opens, and
/// answers the same live as after a reopen, whatever the view does next.
#[test]
fn a_store_with_an_index_over_a_view_property_opens_and_answers_correctly() {
    let dir = tmp("row-123-existing");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.create_view(user_deg_view()).unwrap();
        db.insert_node("User", "a", vec![]).unwrap();
        db.insert_node("User", "b", vec![]).unwrap();
    }
    // The index record an older release would have logged after the view.
    let frame = core_storage::encode_record(&core_storage::WalRecord::EnableIndex {
        label: "User".into(),
        field: "deg".into(),
    });
    let mut wal = wal_bytes(&dir);
    wal.extend(frame);
    std::fs::write(dir.join("wal.bin"), wal).unwrap();

    let mut db = GraphDb::open(&dir).unwrap();
    // The drop is reported, and the declaration is gone.
    assert_eq!(
        db.dropped_view_indexes(),
        [("User".to_string(), "deg".to_string())]
    );
    assert!(!db.is_index_enabled("User", "deg"));
    assert!(users_with_deg_1(&db).is_empty());
    db.insert_edge("KNOWS", "a", "b").unwrap();
    assert_eq!(users_with_deg_1(&db), vec!["a"], "live");
    drop(db);
    let mut db = GraphDb::open(&dir).unwrap();
    assert_eq!(users_with_deg_1(&db), vec!["a"], "after a reopen");
    db.insert_edge("KNOWS", "b", "a").unwrap();
    assert_eq!(users_with_deg_1(&db), vec!["a", "b"], "live, again");
    db.snapshot().unwrap();
    drop(db);
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(users_with_deg_1(&db), vec!["a", "b"], "after a snapshot");
    // The snapshot no longer carries the declaration: nothing to drop now.
    assert!(db.dropped_view_indexes().is_empty());
    assert!(!db.is_index_enabled("User", "deg"));
}

#[cfg(feature = "test-hooks")]
mod unlocked {
    use super::*;
    use mushroomdb::{BatchBuilder, Direction, SnapshotOptions, ViewDef, ViewSource};
    use std::collections::BTreeMap;

    type Batch<'a> = BatchBuilder<'a, RealFs>;

    fn open_temp(name: &str) -> (PathBuf, GraphDb) {
        let dir = tmp(name);
        let mut db = GraphDb::open(&dir).unwrap();
        db.unlock_v11_format_for_test();
        (dir, db)
    }

    fn reopen(dir: &Path, db: GraphDb) -> GraphDb {
        drop(db);
        let mut db = GraphDb::open(dir).unwrap();
        db.unlock_v11_format_for_test();
        db
    }

    fn int(i: i64) -> Value {
        Value::Int(i)
    }

    /// `[lo, hi]` on `(Score, val)`, as node keys in index order.
    fn between(db: &GraphDb, lo: i64, hi: i64) -> Vec<String> {
        db.range_lookup_keys_for_test(
            "Score",
            "val",
            Some((&int(lo), true)),
            Some((&int(hi), true)),
        )
    }

    fn everything(db: &GraphDb) -> Vec<String> {
        db.range_lookup_keys_for_test("Score", "val", None, None)
    }

    fn keys(ks: &[&str]) -> Vec<String> {
        ks.iter().map(|k| k.to_string()).collect()
    }

    fn decl() -> Vec<(String, String, String)> {
        vec![("idx".into(), "Score".into(), "val".into())]
    }

    // ── The brief's case ────────────────────────────────────────────────────

    #[test]
    fn test_range_index_rebuild_at_open() {
        let (dir, mut db) = open_temp("rebuild");
        db.enable_range_index("idx", "Score", "val").unwrap();
        db.write_batch(|b: &mut Batch<'_>| {
            for i in 0..10i64 {
                b.insert_node("Score", &format!("s{i}"), val(i));
            }
        })
        .unwrap();
        db.snapshot().unwrap();
        drop(db);
        // Reopen — the index is rebuilt from the declaration.
        let db2 = GraphDb::open(&dir).unwrap();
        let results = db2
            .query(
                "MATCH (n:Score) WHERE n.val >= 5 RETURN key(n)",
                &BTreeMap::new(),
            )
            .unwrap();
        assert_eq!(results.len(), 5);
        assert_eq!(db2.range_indexes(), decl());
        assert_eq!(
            db2.range_lookup_keys_for_test("Score", "val", Some((&int(5), true)), None),
            keys(&["s5", "s6", "s7", "s8", "s9"])
        );
    }

    // ── Maintenance ─────────────────────────────────────────────────────────

    #[test]
    fn declaring_indexes_existing_nodes_and_every_write_after() {
        let (dir, mut db) = open_temp("maintain");
        db.insert_node("Score", "a", val(1)).unwrap();
        db.insert_node("Score", "b", val(5)).unwrap();
        db.insert_node("Other", "x", val(3)).unwrap();
        db.enable_range_index("idx", "Score", "val").unwrap();
        assert!(db.is_index_enabled("Score", "val"));
        assert_eq!(everything(&db), keys(&["a", "b"]), "backfilled, label only");

        db.insert_node("Score", "c", val(3)).unwrap();
        db.set_prop("a", "val", int(9)).unwrap();
        db.insert_node("Score", "d", val(4)).unwrap();
        db.delete_node("d").unwrap();
        db.rename_node("b", "bee").unwrap();
        db.insert_node("Score", "e", vec![("val".into(), Value::Str("5".into()))])
            .unwrap();
        db.insert_node("Score", "f", val(7)).unwrap();
        db.remove_prop("f", "val").unwrap();
        let want = keys(&["c", "bee", "a"]);
        assert_eq!(between(&db, i64::MIN, i64::MAX), want);
        assert_eq!(between(&db, 3, 5), keys(&["c", "bee"]));
        // A string value is in the index, but only a string bound finds it.
        assert_eq!(
            db.range_lookup_keys_for_test(
                "Score",
                "val",
                None,
                Some((&Value::Str("9".into()), true))
            ),
            keys(&["e"])
        );

        let db = reopen(&dir, db);
        assert_eq!(between(&db, i64::MIN, i64::MAX), want, "reopened");
    }

    #[test]
    fn a_prefix_lookup_reads_the_same_postings() {
        let (dir, mut db) = open_temp("prefix");
        db.enable_range_index("by_name", "P", "name").unwrap();
        for (k, n) in [("p1", "apple"), ("p2", "apricot"), ("p3", "banana")] {
            db.insert_node("P", k, vec![("name".into(), Value::Str(n.into()))])
                .unwrap();
        }
        let db = reopen(&dir, db);
        let ap = db.range_lookup_keys_for_test(
            "P",
            "name",
            Some((&Value::Str("ap".into()), true)),
            Some((&Value::Str("aq".into()), false)),
        );
        assert_eq!(ap, keys(&["p1", "p2"]));
    }

    // ── The differential probe ──────────────────────────────────────────────

    /// Separate commits vs one batch × live / reopened / truncating snapshot /
    /// `keep_wal` snapshot, then a write after each: the declaration and the
    /// postings are the same on all eight.
    #[test]
    fn a_range_index_agrees_across_axes() {
        #[derive(Debug, Clone, Copy)]
        enum Axis {
            Live,
            Reopened,
            Snapshotted,
            SnapshotKeepWal,
        }
        let mut seen = Vec::new();
        for axis in [
            Axis::Live,
            Axis::Reopened,
            Axis::Snapshotted,
            Axis::SnapshotKeepWal,
        ] {
            for batched in [false, true] {
                let (dir, mut db) = open_temp("axes");
                db.insert_node("Score", "early", val(2)).unwrap();
                db.enable_range_index("idx", "Score", "val").unwrap();
                if batched {
                    db.write_batch(|b: &mut Batch<'_>| {
                        b.insert_node("Score", "s4", val(4));
                        b.insert_node("Score", "s6", val(6));
                        b.insert_node("Score", "gone", val(5));
                        b.insert_node("Admin", "root", val(5));
                        b.set_prop("early", "val", int(8));
                        b.delete_node("gone");
                    })
                    .unwrap();
                } else {
                    db.insert_node("Score", "s4", val(4)).unwrap();
                    db.insert_node("Score", "s6", val(6)).unwrap();
                    db.insert_node("Score", "gone", val(5)).unwrap();
                    db.insert_node("Admin", "root", val(5)).unwrap();
                    db.set_prop("early", "val", int(8)).unwrap();
                    db.delete_node("gone").unwrap();
                }
                match axis {
                    Axis::Live => {}
                    Axis::Reopened => db = reopen(&dir, db),
                    Axis::Snapshotted => {
                        db.snapshot().unwrap();
                        db = reopen(&dir, db);
                    }
                    Axis::SnapshotKeepWal => {
                        db.snapshot_with(SnapshotOptions {
                            keep_wal: true,
                            ..Default::default()
                        })
                        .unwrap();
                        db = reopen(&dir, db);
                    }
                }
                let name = format!("{axis:?} batched={batched}");
                assert_eq!(db.range_indexes(), decl(), "{name}");
                db.insert_node("Score", "s5", val(5)).unwrap();
                let after_write = (everything(&db), between(&db, 5, 8));
                let db = reopen(&dir, db);
                assert_eq!(
                    (everything(&db), between(&db, 5, 8)),
                    after_write,
                    "{name}: reopened after the write"
                );
                seen.push((name, after_write));
            }
        }
        let first = &seen[0].1;
        assert_eq!(
            first,
            &(
                keys(&["s4", "s5", "s6", "early"]),
                keys(&["s5", "s6", "early"])
            )
        );
        for (name, state) in &seen {
            assert_eq!(state, first, "{name} disagrees with {}", seen[0].0);
        }
    }

    #[test]
    fn open_at_sees_the_declaration_only_from_its_commit() {
        let (dir, mut db) = open_temp("open-at");
        db.insert_node("Score", "a", val(1)).unwrap(); // commit 0
        db.enable_range_index("idx", "Score", "val").unwrap(); // commit 1
        db.insert_node("Score", "b", val(2)).unwrap(); // commit 2
        db.disable_range_index("idx").unwrap(); // commit 3
        drop(db);
        let before = GraphDb::open_at(&dir, 0).unwrap();
        assert!(before.range_indexes().is_empty(), "declared at commit 1");
        assert!(!before.is_index_enabled("Score", "val"));
        let declared = GraphDb::open_at(&dir, 1).unwrap();
        assert_eq!(declared.range_indexes(), decl());
        assert_eq!(everything(&declared), keys(&["a"]));
        let written = GraphDb::open_at(&dir, 2).unwrap();
        assert_eq!(everything(&written), keys(&["a", "b"]));
        let dropped = GraphDb::open_at(&dir, 3).unwrap();
        assert!(dropped.range_indexes().is_empty());
        assert!(!dropped.is_index_enabled("Score", "val"));
        let mut as_of = GraphDb::open_at(&dir, 2).unwrap();
        assert!(matches!(
            as_of.enable_range_index("other", "Score", "val"),
            Err(GraphError::ReadOnly)
        ));
        assert!(matches!(
            as_of.disable_range_index("idx"),
            Err(GraphError::ReadOnly)
        ));
    }

    // ── Withdrawal ──────────────────────────────────────────────────────────

    #[test]
    fn a_withdrawn_range_index_stays_withdrawn_across_reopen_and_snapshot() {
        let (dir, mut db) = open_temp("withdraw");
        db.insert_node("Score", "a", val(1)).unwrap();
        db.enable_range_index("idx", "Score", "val").unwrap();
        db.disable_range_index("idx").unwrap();
        assert!(db.range_indexes().is_empty());
        assert!(!db.is_index_enabled("Score", "val"));
        assert!(everything(&db).is_empty());
        let mut db = reopen(&dir, db);
        assert!(db.range_indexes().is_empty());
        db.snapshot().unwrap();
        let mut db = reopen(&dir, db);
        assert!(db.range_indexes().is_empty());
        assert!(!db.is_index_enabled("Score", "val"));
        // The name is free again.
        db.enable_range_index("idx", "Score", "val").unwrap();
        assert_eq!(everything(&db), keys(&["a"]));
    }

    /// A truncating snapshot re-emits a range-only pair as its range index,
    /// not as an equality index too, so dropping the range index after the
    /// snapshot leaves the pair unindexed exactly as it does before one.
    #[test]
    fn a_range_only_pair_is_gone_after_its_drop_across_a_snapshot() {
        for keep_wal in [false, true] {
            let (dir, mut db) = open_temp("drop-after-snapshot");
            db.insert_node("Score", "a", val(1)).unwrap();
            db.enable_range_index("idx", "Score", "val").unwrap();
            db.snapshot_with(SnapshotOptions {
                keep_wal,
                ..Default::default()
            })
            .unwrap();
            let mut db = reopen(&dir, db);
            assert_eq!(db.range_indexes(), decl(), "keep_wal={keep_wal}");
            db.disable_range_index("idx").unwrap();
            assert!(!db.is_index_enabled("Score", "val"), "keep_wal={keep_wal}");
            let db = reopen(&dir, db);
            assert!(db.range_indexes().is_empty(), "keep_wal={keep_wal}");
            assert!(!db.is_index_enabled("Score", "val"), "keep_wal={keep_wal}");
        }
    }

    /// Dropping a range index over a pair that also has an equality index —
    /// declared directly or by a unique constraint — keeps that index, live
    /// and after a truncating snapshot.
    #[test]
    fn dropping_a_range_index_keeps_an_equality_index_on_the_pair() {
        for snapshot in [false, true] {
            let (dir, mut db) = open_temp("keep-eq");
            db.insert_node("Score", "a", val(1)).unwrap();
            db.enable_index("Score", "val").unwrap();
            db.enable_range_index("idx", "Score", "val").unwrap();
            if snapshot {
                db.snapshot().unwrap();
                db = reopen(&dir, db);
            }
            db.disable_range_index("idx").unwrap();
            assert!(db.is_index_enabled("Score", "val"), "snapshot={snapshot}");
            let db = reopen(&dir, db);
            assert!(db.is_index_enabled("Score", "val"), "snapshot={snapshot}");
            assert!(db.range_indexes().is_empty());
            assert_eq!(
                everything(&db),
                keys(&["a"]),
                "the equality postings remain"
            );
        }
    }

    #[test]
    fn a_constraint_keeps_its_index_when_the_range_index_goes() {
        let (dir, mut db) = open_temp("keep-constraint");
        db.insert_node("Score", "a", val(1)).unwrap();
        db.enable_range_index("idx", "Score", "val").unwrap();
        db.create_constraint("uniq_val", "Score", "val").unwrap();
        db.disable_range_index("idx").unwrap();
        assert!(db.is_index_enabled("Score", "val"));
        assert!(matches!(
            db.insert_node("Score", "b", val(1)),
            Err(GraphError::ConstraintViolation { .. })
        ));
        let mut db = reopen(&dir, db);
        assert!(db.is_index_enabled("Score", "val"));
        assert!(matches!(
            db.insert_node("Score", "b", val(1)),
            Err(GraphError::ConstraintViolation { .. })
        ));
    }

    #[test]
    fn disable_index_on_a_range_only_pair_is_refused() {
        let (dir, mut db) = open_temp("disable-eq");
        db.enable_range_index("idx", "Score", "val").unwrap();
        let before = wal_bytes(&dir);
        match db.disable_index("Score", "val") {
            Err(GraphError::QueryError { detail }) => assert_eq!(
                detail,
                "the index on (Score, val) is range index 'idx'; drop it with disable_range_index"
            ),
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert_eq!(wal_bytes(&dir), before);
        assert_eq!(db.range_indexes(), decl());
    }

    #[test]
    fn enable_index_on_a_range_indexed_pair_names_the_range_index() {
        let (_dir, mut db) = open_temp("enable-eq");
        db.enable_range_index("idx", "Score", "val").unwrap();
        match db.enable_index("Score", "val") {
            Err(GraphError::RuleInvalid { detail }) => assert_eq!(
                detail,
                "property index for (\"Score\", \"val\") already enabled: range index 'idx' covers it"
            ),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_second_name_or_a_second_range_index_on_a_pair_is_refused() {
        let (dir, mut db) = open_temp("dups");
        db.enable_range_index("idx", "Score", "val").unwrap();
        let before = wal_bytes(&dir);
        match db.enable_range_index("idx", "Score", "other") {
            Err(GraphError::QueryError { detail }) => {
                assert_eq!(detail, "a range index named 'idx' already exists")
            }
            other => panic!("expected a duplicate-name refusal, got {other:?}"),
        }
        match db.enable_range_index("idx2", "Score", "val") {
            Err(GraphError::QueryError { detail }) => assert_eq!(
                detail,
                "property 'val' on label 'Score' already has range index 'idx'"
            ),
            other => panic!("expected a duplicate-pair refusal, got {other:?}"),
        }
        match db.disable_range_index("nope") {
            Err(GraphError::QueryError { detail }) => {
                assert_eq!(detail, "no range index named 'nope'")
            }
            other => panic!("expected an unknown-name refusal, got {other:?}"),
        }
        assert_eq!(wal_bytes(&dir), before);
        assert_eq!(db.range_indexes(), decl());
    }

    // ── Views ───────────────────────────────────────────────────────────────

    fn deg_view() -> ViewDef {
        ViewDef {
            name: "deg".into(),
            label: "User".into(),
            view_prop: "deg".into(),
            source: ViewSource::Degree {
                edge_type: "KNOWS".into(),
                direction: Direction::Out,
            },
        }
    }

    /// A view writes its property without passing the property index, so a
    /// range index over a view property would answer from stale postings until
    /// the next open. Refused both ways, before anything is written.
    #[test]
    fn a_range_index_cannot_cover_a_view_property() {
        let (dir, mut db) = open_temp("view-then-index");
        db.create_view(deg_view()).unwrap();
        let before = wal_bytes(&dir);
        match db.enable_range_index("by_deg", "User", "deg") {
            Err(GraphError::QueryError { detail }) => assert_eq!(
                detail,
                "property 'deg' on label 'User' is owned by view 'deg'; a range index cannot \
                 cover it"
            ),
            other => panic!("expected a view refusal, got {other:?}"),
        }
        assert_eq!(wal_bytes(&dir), before);
        assert!(db.range_indexes().is_empty());
        // Another label's `deg` is not the view's.
        db.enable_range_index("admin_deg", "Admin", "deg").unwrap();

        let (dir, mut db) = open_temp("index-then-view");
        db.enable_range_index("by_deg", "User", "deg").unwrap();
        let before = wal_bytes(&dir);
        match db.create_view(deg_view()) {
            Err(GraphError::QueryError { detail }) => assert_eq!(
                detail,
                "property 'deg' on label 'User' is covered by range index 'by_deg'; a view \
                 cannot own it"
            ),
            other => panic!("expected a range-index refusal, got {other:?}"),
        }
        assert_eq!(wal_bytes(&dir), before);
        assert!(db.views().is_empty());
    }

    // ── Index against scan, through the store ──────────────────────────────

    /// xorshift64*: deterministic, no dependency.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    fn pool() -> Vec<Value> {
        let dt = |us: i64, off: i32| Value::DateTime {
            micros_utc: us,
            offset_seconds: off,
        };
        vec![
            Value::Int(-1),
            Value::Int(0),
            Value::Int(2),
            Value::Int(5),
            Value::Int((1 << 60) + 1),
            Value::Float(-0.0),
            Value::Float(0.0),
            Value::Float(2.0),
            Value::Float(5.5),
            Value::Float(f64::NAN),
            Value::Float((1u64 << 60) as f64),
            Value::Str("a".into()),
            Value::Str("ab".into()),
            Value::Bool(false),
            Value::Bool(true),
            Value::Date(3),
            Value::LocalDateTime(7),
            dt(10, 0),
            dt(10, 3600),
            dt(30, 0),
            Value::Binary(vec![1]),
            Value::Duration {
                months: 0,
                days: 1,
                nanos: 0,
            },
        ]
    }

    /// Every operator and BETWEEN, over every live `S` node: the index (the
    /// range lookup, and Cypher equality, which reads the index) against
    /// `eval_cmp` applied to what the store reads back.
    fn assert_index_matches_scan(
        db: &GraphDb,
        keys: &[String],
        values: &[Value],
        rng: &mut Rng,
        ctx: &str,
    ) {
        use core_query::{eval_cmp, CmpOp};
        let stored: Vec<(String, Value)> = keys
            .iter()
            .filter_map(|k| {
                let n = db.node_info(k)?;
                (n.label == "S").then_some(())?;
                n.props.get("v").cloned().map(|v| (k.clone(), v))
            })
            .collect();
        let scan = |pred: &dyn Fn(&Value) -> bool| {
            let mut ks: Vec<String> = stored
                .iter()
                .filter(|(_, v)| pred(v))
                .map(|(k, _)| k.clone())
                .collect();
            ks.sort();
            ks
        };
        let sorted = |mut ks: Vec<String>| {
            ks.sort();
            ks
        };
        for _ in 0..8 {
            let a = &values[rng.below(values.len())];
            let b = &values[rng.below(values.len())];
            for (op, lo, hi) in [
                (CmpOp::Lt, None, Some((a, false))),
                (CmpOp::Le, None, Some((a, true))),
                (CmpOp::Gt, Some((a, false)), None),
                (CmpOp::Ge, Some((a, true)), None),
            ] {
                let got = sorted(db.range_lookup_keys_for_test("S", "v", lo, hi));
                assert_eq!(got, scan(&|v| eval_cmp(&op, v, a)), "{ctx}: {op:?} {a:?}");
            }
            for (li, hincl) in [(true, true), (true, false), (false, true), (false, false)] {
                let got = sorted(db.range_lookup_keys_for_test(
                    "S",
                    "v",
                    Some((a, li)),
                    Some((b, hincl)),
                ));
                let want = scan(&|v| {
                    eval_cmp(if li { &CmpOp::Ge } else { &CmpOp::Gt }, v, a)
                        && eval_cmp(if hincl { &CmpOp::Le } else { &CmpOp::Lt }, v, b)
                });
                assert_eq!(got, want, "{ctx}: BETWEEN {a:?} ({li}) AND {b:?} ({hincl})");
            }
            let mut params = BTreeMap::new();
            params.insert("x".to_string(), a.clone());
            let rs = db
                .query("MATCH (n:S {v: $x}) RETURN key(n) AS k", &params)
                .unwrap();
            let got = sorted(
                (0..rs.len())
                    .filter_map(|i| match rs.get(i, "k") {
                        Some(Value::Str(k)) => Some(k.clone()),
                        _ => None,
                    })
                    .collect(),
            );
            assert_eq!(got, scan(&|v| eval_cmp(&CmpOp::Eq, v, a)), "{ctx}: = {a:?}");
        }
    }

    /// Random inserts (some without the property, some of another label),
    /// updates, removals, deletes and renames; the index is checked against
    /// the scan as it goes, then reopened, then snapshotted and reopened.
    #[test]
    fn the_index_matches_a_scan_under_random_writes() {
        let values = pool();
        for seed in 1..=6u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let (dir, mut db) = open_temp("differential");
            db.enable_range_index("idx", "S", "v").unwrap();
            let mut keys: Vec<String> = Vec::new();
            for step in 0..120 {
                let pick = |rng: &mut Rng| values[rng.below(values.len())].clone();
                match rng.below(10) {
                    0..=3 => {
                        let key = format!("k{step}");
                        let label = if rng.below(5) == 0 { "T" } else { "S" };
                        let props = if rng.below(6) == 0 {
                            vec![]
                        } else {
                            vec![("v".to_string(), pick(&mut rng))]
                        };
                        db.insert_node(label, &key, props).unwrap();
                        keys.push(key);
                    }
                    4..=5 if !keys.is_empty() => {
                        let key = keys[rng.below(keys.len())].clone();
                        let v = pick(&mut rng);
                        db.set_prop(&key, "v", v).unwrap();
                    }
                    6 if !keys.is_empty() => {
                        let key = keys[rng.below(keys.len())].clone();
                        db.remove_prop(&key, "v").unwrap();
                    }
                    7 if !keys.is_empty() => {
                        let key = keys.swap_remove(rng.below(keys.len()));
                        db.delete_node(&key).unwrap();
                    }
                    8 if !keys.is_empty() => {
                        let at = rng.below(keys.len());
                        let new_key = format!("{}-r{step}", keys[at]);
                        db.rename_node(&keys[at], &new_key).unwrap();
                        keys[at] = new_key;
                    }
                    _ => {}
                }
                if step % 10 == 9 {
                    let ctx = format!("seed {seed} step {step}");
                    assert_index_matches_scan(&db, &keys, &values, &mut rng, &ctx);
                }
            }
            let mut db = reopen(&dir, db);
            assert_index_matches_scan(
                &db,
                &keys,
                &values,
                &mut rng,
                &format!("seed {seed} reopened"),
            );
            db.snapshot().unwrap();
            let db = reopen(&dir, db);
            assert_index_matches_scan(
                &db,
                &keys,
                &values,
                &mut rng,
                &format!("seed {seed} snapshotted"),
            );
        }
    }
}
