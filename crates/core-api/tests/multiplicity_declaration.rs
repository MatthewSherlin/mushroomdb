//! The multiplicity opt-in is a *format* declaration, and that makes losing it
//! different from losing an index declaration.
//!
//! `EnableFulltext` and `EnableIndex` describe something the store can rebuild:
//! lose one and an operator calls the enable again. `MULTIPLICITY_ENABLED`
//! describes what the store's bytes *are* — a WAL that carries discriminant 23
//! and a snapshot stamped V10 so an older binary refuses the open instead of
//! truncating at it. Lose that one and the store keeps the bytes and drops the
//! stamp: the next snapshot writes V9 while the archives still carry
//! discriminant 23, and a v0.6.9 archive scan reads them as a truncated prefix.
//!
//! Three properties are tested here, one per defect the fifth review pass found:
//!
//! * **#22** — a failure anywhere in `snapshot_with(archive_wal)`'s sequence
//!   between renaming the live WAL away and writing its replacement must not
//!   opt the store back out.
//! * **#23** — opting in must not cost the store its archive genesis chain.
//! * **#24** — a duplicate edge whose endpoints are created in the *same* frame
//!   must be counted; that shape is the primary ingest shape, not a corner.
//!
//! The sixth pass added three more, and they point the other way: the opt-in is
//! **not atomic**, and the tests below assert what the code does rather than
//! what its docstring promised.
//!
//! * **#32** — a failed `enable_multiplicity()` beside an existing archive comes
//!   back opted *in* on the next open.
//! * **#33** — the recovery's predicate, and the one invariant it must keep: it
//!   never opts in a store whose snapshot is not V10.
//! * **#34** — the rollback was never reliable: append precedes fsync, so a
//!   failed barrier leaves the declaration durable with no archive in sight.

use core_storage::fs::{FileId, Fs};
use core_storage::{decode_all, WalRecord};
use mushroomdb::{AlgoDir, GraphDb, SnapshotOptions};
use sim_harness::SimFs;
use std::cell::Cell;

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "graphdb-mult-decl-{}-{}-{}",
        name,
        std::process::id(),
        nanos
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn opts_archive() -> SnapshotOptions {
    SnapshotOptions {
        archive_wal: true,
        ..SnapshotOptions::default()
    }
}

// ── #22: a transient failure mid-archive must not opt the store back out ─────

/// Which call in the archive sequence fails once.
///
/// Both sit *after* `archive_wal` has renamed the live WAL away and *before*
/// the replacement baseline is on disk — the window in which the store holds
/// no live declaration at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FailAt {
    /// The retention sweep's `list_archives`, once the WAL has been renamed
    /// away — one of the fallible calls that used to sit between the rename
    /// and the baseline write. It was `write_genesis_marker` until row 98
    /// moved that write to before the snapshot; the store keeps a retention
    /// bound in these tests so that the sweep runs.
    AfterRename,
    /// The baseline WAL write itself, the last call in the sequence.
    BaselineWal,
    /// The `append` of `MULTIPLICITY_ENABLED` itself, inside
    /// `enable_multiplicity`'s `log_then_apply` — i.e. after the V10 snapshot
    /// is already on disk and before any record is. Used by #32.
    DeclarationAppend,
    /// The fsync that follows a **successful** append of that record. The frame
    /// is already in `wal.bin`; only the durability barrier failed. Used by #34.
    DeclarationSync,
}

/// A filesystem that fails one named call exactly once and then works normally.
///
/// Deliberately *not* a crash: `SimFs::with_crash_after_ops` latches, so every
/// later call fails too and the failure is indistinguishable from a power cut.
/// The defect is that a single `Err` — a transient `EIO`, an `ENOSPC` that
/// clears — is enough, so the injected failure has to be a single `Err`.
#[derive(Debug)]
struct FailOnce {
    inner: SimFs,
    at: FailAt,
    fired: Cell<bool>,
    /// Set once the live WAL has been renamed away, so the baseline-write
    /// target does not fire on an unrelated earlier `write_atomic`.
    archived: Cell<bool>,
    /// Set once a frame carrying `MULTIPLICITY_ENABLED` has actually landed, so
    /// the `DeclarationSync` target fires on that record's barrier and not on an
    /// unrelated earlier one.
    declared: Cell<bool>,
}

impl FailOnce {
    fn new(at: FailAt) -> Self {
        Self {
            inner: SimFs::new(),
            at,
            fired: Cell::new(false),
            archived: Cell::new(false),
            declared: Cell::new(false),
        }
    }

    /// `true` exactly once, when `at` matches.
    fn should_fail(&self, at: FailAt) -> bool {
        if self.at != at || self.fired.get() {
            return false;
        }
        self.fired.set(true);
        true
    }

    fn err() -> std::io::Error {
        std::io::Error::other("injected transient failure")
    }
}

impl Fs for FailOnce {
    fn append(&mut self, file: FileId, data: &[u8]) -> std::io::Result<()> {
        let declares = file == FileId::Wal && frame_declares_multiplicity(data);
        if declares && self.should_fail(FailAt::DeclarationAppend) {
            return Err(Self::err());
        }
        self.inner.append(file, data)?;
        if declares {
            self.declared.set(true);
        }
        Ok(())
    }
    fn sync(&mut self, file: FileId) -> std::io::Result<()> {
        if file == FileId::Wal && self.declared.get() && self.should_fail(FailAt::DeclarationSync) {
            return Err(Self::err());
        }
        self.inner.sync(file)
    }
    fn read(&self, file: FileId) -> std::io::Result<Vec<u8>> {
        self.inner.read(file)
    }
    fn write_atomic(&mut self, file: FileId, data: &[u8]) -> std::io::Result<()> {
        if file == FileId::Wal && self.archived.get() && self.should_fail(FailAt::BaselineWal) {
            return Err(Self::err());
        }
        self.inner.write_atomic(file, data)
    }
    fn list_archives(&self) -> std::io::Result<Vec<u64>> {
        if self.archived.get() && self.should_fail(FailAt::AfterRename) {
            return Err(Self::err());
        }
        self.inner.list_archives()
    }
    fn read_archive(&self, n: u64) -> std::io::Result<Vec<u8>> {
        self.inner.read_archive(n)
    }
    fn archive_wal(&mut self, n: u64) -> std::io::Result<()> {
        self.inner.archive_wal(n)?;
        self.archived.set(true);
        Ok(())
    }
    fn delete_archive(&mut self, n: u64) -> std::io::Result<()> {
        self.inner.delete_archive(n)
    }
    fn read_horizon_floor(&self) -> std::io::Result<u64> {
        self.inner.read_horizon_floor()
    }
    fn write_horizon_floor(&mut self, floor: u64) -> std::io::Result<()> {
        self.inner.write_horizon_floor(floor)
    }
    fn has_genesis_marker(&self) -> bool {
        self.inner.has_genesis_marker()
    }
    fn write_genesis_marker(&mut self) -> std::io::Result<()> {
        self.inner.write_genesis_marker()
    }
    fn delete_genesis_marker(&mut self) -> std::io::Result<()> {
        self.inner.delete_genesis_marker()
    }
}

/// Whether a framed WAL byte string carries the opt-in **declaration**
/// specifically, as opposed to a count for a real pair. Both are discriminant
/// 23; only this one turns the store's format over.
fn frame_declares_multiplicity(bytes: &[u8]) -> bool {
    let (frames, _) = decode_all(bytes);
    frames.iter().any(|f| match f {
        WalRecord::Batch(inner) => inner.iter().any(|r| r.is_multiplicity_decl()),
        r => r.is_multiplicity_decl(),
    })
}

/// The version stamped on a survivor's `snapshot.bin`, or `None` when there is
/// no snapshot at all.
fn snapshot_version<F: Fs>(fs: &F) -> Option<u16> {
    core_storage::snapshot::peek_version(&fs.read(FileId::Snapshot).unwrap_or_default())
        .unwrap_or(None)
}

/// Whether a WAL byte string carries any discriminant-23 frame — the record a
/// v0.6.9 decoder stops at.
fn has_discriminant_23(bytes: &[u8]) -> bool {
    let (frames, _) = decode_all(bytes);
    frames.iter().any(|f| match f {
        WalRecord::SetEdgeCount { .. } => true,
        WalRecord::Batch(inner) => inner
            .iter()
            .any(|r| matches!(r, WalRecord::SetEdgeCount { .. })),
        _ => false,
    })
}

/// **#22.** `archive_wal` renames the live WAL away and the baseline that
/// re-emits `MULTIPLICITY_ENABLED` is written six fallible calls later. A single
/// `Err` in between leaves the store with no live declaration — and reopening it
/// would report "not opted in", stop counting, and write a **V9** snapshot over
/// archives that still carry discriminant 23.
///
/// Two things answer it, and they answer different halves.
///
/// The **baseline write now comes first**, with no fallible call between it and
/// the rename, so an ordinary `Err` anywhere later — `FailAt::AfterRename` and
/// the others like it — cannot lose any declaration, including the two
/// (`EnableFulltext`, `EnableIndex`) that have no stamp to fall back on.
///
/// The **V10 stamp** closes what ordering cannot: the crash between the rename
/// and the write itself, which `FailAt::BaselineWal` stands in for. It is
/// written by the same `snapshot_with` call, before the rename, and an archive
/// exists only because a snapshot took one — so a V10 snapshot standing next to
/// an archive is proof the store was opted in when that archive was made,
/// whatever happened to the live WAL afterwards.
#[test]
fn a_failed_archive_cannot_opt_the_store_back_out() {
    // At least one target must reach the state where the live WAL really has
    // lost the declaration, or this test would pass on ordering alone and say
    // nothing about the stamp.
    let mut saw_lost_declaration = 0usize;
    for at in [FailAt::AfterRename, FailAt::BaselineWal] {
        let mut db = GraphDb::open_with(FailOnce::new(at)).unwrap();
        db.set_wal_archive_retention(Some(8));
        db.insert_node("N", "a", vec![]).unwrap();
        db.insert_node("N", "b", vec![]).unwrap();
        db.enable_multiplicity().unwrap();
        assert!(db.insert_edge("E", "a", "b").unwrap());
        assert!(!db.insert_edge("E", "a", "b").unwrap());
        assert_eq!(
            db.degree_multiplicity("a", Some("E"), AlgoDir::Out)
                .unwrap(),
            2
        );

        let err = db.snapshot_with(opts_archive()).unwrap_err();
        assert!(
            err.to_string().contains("injected transient failure"),
            "fixture: the archive sequence must fail at {at:?}; got {err}"
        );

        let survivor = db.into_fs().inner.surviving_state();

        // The state the defect is about: the rename happened, so the archive
        // carries discriminant 23 and there is no going back.
        let archives = survivor.list_archives().unwrap();
        assert_eq!(archives.len(), 1, "{at:?}: the rename happened");
        assert!(
            has_discriminant_23(&survivor.read_archive(archives[0]).unwrap()),
            "{at:?}: fixture — the archived WAL carries the record"
        );
        if !has_discriminant_23(&survivor.read(FileId::Wal).unwrap()) {
            saw_lost_declaration += 1;
        }

        let mut reopened = GraphDb::open_with(survivor).unwrap();
        assert!(
            reopened.is_multiplicity_enabled(),
            "{at:?}: a failed archive must not opt the store back out — the \
             archives already carry discriminant 23"
        );

        // And the guard the opt-in bought stays bought: the next snapshot is
        // still V10, not a V9 an older binary would walk straight past.
        reopened.insert_node("N", "c", vec![]).unwrap();
        reopened.snapshot().unwrap();
        let fs = reopened.into_fs();
        assert_eq!(
            core_storage::snapshot::peek_version(&fs.read(FileId::Snapshot).unwrap()).unwrap(),
            Some(10),
            "{at:?}: the store must keep writing V10"
        );
    }
    assert_eq!(
        saw_lost_declaration, 1,
        "exactly one target must reach the lost-declaration state: \
         AfterRename is saved by the reordered baseline write, BaselineWal is \
         not and is what exercises the V10 stamp"
    );
}

/// The ordering half, stated on its own: a failure *after* the replacement WAL
/// is written must leave every declaration on disk — including the two that have
/// no snapshot stamp behind them and that no `open` can recover.
#[test]
fn a_failure_after_the_rename_keeps_the_index_declarations_too() {
    let mut db = GraphDb::open_with(FailOnce::new(FailAt::AfterRename)).unwrap();
    db.set_wal_archive_retention(Some(8));
    db.insert_node("N", "a", vec![]).unwrap();
    db.enable_fulltext("N", "body").unwrap();
    db.enable_index("N", "tag").unwrap();

    db.snapshot_with(opts_archive()).unwrap_err();
    let survivor = db.into_fs().inner.surviving_state();
    assert_eq!(survivor.list_archives().unwrap().len(), 1);

    let reopened = GraphDb::open_with(survivor).unwrap();
    assert!(
        reopened.is_fulltext_enabled("N", "body"),
        "the full-text declaration must survive a failure after the rename"
    );
    assert!(
        reopened.is_index_enabled("N", "tag"),
        "and so must the property-index declaration"
    );
}

/// The store that never opted in must not be swept up by the recovery above: a
/// V9 snapshot next to an archive says nothing, and neither does an opted-out
/// store with no archives at all.
#[test]
fn the_recovery_does_not_opt_a_store_in_by_itself() {
    let mut db = GraphDb::open_with(SimFs::new()).unwrap();
    db.insert_node("N", "a", vec![]).unwrap();
    db.insert_node("N", "b", vec![]).unwrap();
    assert!(db.insert_edge("E", "a", "b").unwrap());
    db.snapshot_with(opts_archive()).unwrap();
    let fs = db.into_fs();
    assert_eq!(
        core_storage::snapshot::peek_version(&fs.read(FileId::Snapshot).unwrap()).unwrap(),
        Some(9),
        "fixture: a store that never opted in archives at V9"
    );
    assert_eq!(fs.list_archives().unwrap().len(), 1, "fixture: it archived");

    let reopened = GraphDb::open_with(fs).unwrap();
    assert!(
        !reopened.is_multiplicity_enabled(),
        "an archive alone must not opt a store in"
    );
}

// ── #32/#33/#34: the opt-in is not atomic, and the docs now say so ───────────

/// **#32.** `enable_multiplicity()` is **not atomic**, and a store that already
/// archived is the sharpest case: the call returns `Err`, reports the store
/// opted out, and the *next open* opts it in anyway.
///
/// The sequence is ordinary. Archive once while opted out — V9 snapshot, one
/// archive, no declaration anywhere. Then call `enable_multiplicity()`: it
/// stamps the V10 snapshot first (that ordering is the guard and must stay), and
/// then fails before the declaration reaches the WAL. Nothing on disk says
/// "opted in" — but the recovery reads V10-beside-an-archive as if it did.
///
/// This test asserts what the code **does**, not what the docstring used to
/// promise. The promise ("left opted *out*", "the next snapshot rewrites at V9")
/// is what is wrong here; the behaviour is safe, because the V10 stamp reached
/// disk before anything a v0.6.9 reader would truncate at.
#[test]
fn a_failed_opt_in_beside_an_archive_comes_back_opted_in() {
    let mut db = GraphDb::open_with(FailOnce::new(FailAt::DeclarationAppend)).unwrap();
    db.insert_node("N", "a", vec![]).unwrap();
    db.insert_node("N", "b", vec![]).unwrap();

    // Archive while still opted out: this archive was taken by a store that was
    // *not* counting, which is what makes #33's inference unsound.
    db.snapshot_with(opts_archive()).unwrap();

    let err = db.enable_multiplicity().unwrap_err();
    assert!(
        err.to_string().contains("injected transient failure"),
        "fixture: the declaration append must fail; got {err}"
    );
    assert!(
        !db.is_multiplicity_enabled(),
        "the failed call reports the store opted out, on this handle"
    );

    let survivor = db.into_fs().inner.surviving_state();
    assert_eq!(
        snapshot_version(&survivor),
        Some(10),
        "fixture: the V10 stamp went first and landed — the safe direction"
    );
    let archives = survivor.list_archives().unwrap();
    assert_eq!(archives.len(), 1, "fixture: one archive, taken at V9");
    assert!(
        !has_discriminant_23(&survivor.read_archive(archives[0]).unwrap()),
        "fixture: the archive predates the opt-in, so it carries no record — \
         the counterexample to 'V10 beside an archive proves the archive was \
         made while opted in'"
    );
    assert!(
        !frame_declares_multiplicity(&survivor.read(FileId::Wal).unwrap()),
        "fixture: no declaration reached the live WAL either"
    );

    let reopened = GraphDb::open_with(survivor).unwrap();
    assert!(
        reopened.is_multiplicity_enabled(),
        "the recovery completes an opt-in that returned Err: the call is not \
         atomic, and `enable_multiplicity`'s docs must say so"
    );
}

/// **#34.** The rollback was unreliable before the recovery existed, and still
/// is without any archive in sight. `log_then_apply` appends, *then* fsyncs: a
/// failed barrier leaves `MULTIPLICITY_ENABLED` already in `wal.bin` while the
/// call sets `self.multiplicity = false` and returns `Err`. The next open
/// replays the record and the store is opted in.
///
/// No archive, so the recovery clause plays no part — this is the append/sync
/// ordering alone.
#[test]
fn a_failed_opt_in_whose_record_reached_the_wal_comes_back_opted_in() {
    let mut db = GraphDb::open_with(FailOnce::new(FailAt::DeclarationSync)).unwrap();
    db.insert_node("N", "a", vec![]).unwrap();

    let err = db.enable_multiplicity().unwrap_err();
    assert!(
        err.to_string().contains("injected transient failure"),
        "fixture: the declaration's fsync must fail; got {err}"
    );
    assert!(
        !db.is_multiplicity_enabled(),
        "the failed call reports the store opted out, on this handle"
    );

    let survivor = db.into_fs().inner.surviving_state();
    assert!(
        survivor.list_archives().unwrap().is_empty(),
        "fixture: no archive — the recovery clause cannot be what opts this in"
    );
    assert!(
        frame_declares_multiplicity(&survivor.read(FileId::Wal).unwrap()),
        "fixture: the append succeeded, so the record is already on disk"
    );
    assert_eq!(
        snapshot_version(&survivor),
        Some(10),
        "and the stamp still precedes it, which is why this is safe"
    );

    let reopened = GraphDb::open_with(survivor).unwrap();
    assert!(
        reopened.is_multiplicity_enabled(),
        "plain WAL replay opts the store in: the rollback was never reliable"
    );
}

/// **#33, the invariant the predicate must preserve.** Whatever else the
/// recovery does, it must never opt in a store whose snapshot is not V10 — a
/// store stamped V9 has made no promise to an older reader, so opting it in
/// would start writing discriminant 23 behind a stamp that does not guard it.
///
/// This is the mutation killer for the version clause: drop
/// `snapshot_version == Some(VERSION_10)`, or widen it to `.is_some()`, and
/// every reopen below comes back opted in.
#[test]
fn the_recovery_never_opts_in_a_store_whose_snapshot_is_not_v10() {
    let mut fs = {
        let mut db = GraphDb::open_with(SimFs::new()).unwrap();
        db.insert_node("N", "a", vec![]).unwrap();
        db.insert_node("N", "b", vec![]).unwrap();
        db.into_fs()
    };

    // Three successive archives, all taken by a store that never opted in. Each
    // reopen must stay opted out no matter how many archives stand beside the
    // V9 stamp.
    for round in 0..3 {
        let mut db = GraphDb::open_with(fs).unwrap();
        assert!(
            !db.is_multiplicity_enabled(),
            "round {round}: a V9 snapshot beside {round} archives must not opt in"
        );
        db.insert_node("N", &format!("n{round}"), vec![]).unwrap();
        db.snapshot_with(opts_archive()).unwrap();
        fs = db.into_fs();
        assert_eq!(
            snapshot_version(&fs),
            Some(9),
            "round {round}: fixture — an opted-out store archives at V9"
        );
        assert_eq!(
            fs.list_archives().unwrap().len(),
            round + 1,
            "round {round}: fixture — the archives accumulate"
        );
    }

    let reopened = GraphDb::open_with(fs).unwrap();
    assert!(
        !reopened.is_multiplicity_enabled(),
        "the V10 stamp is what carries the conclusion; without it the archives \
         say nothing"
    );
}

// ── #23: opting in must not forfeit the archive genesis chain ────────────────

/// **#23.** `enable_multiplicity` writes `snapshot.bin`, and the archive path
/// refuses the genesis marker whenever a snapshot already exists — a rule meant
/// to catch a *truncating* snapshot taken in an earlier session. A
/// history-preserving snapshot is not that, so opting in must not cost the store
/// `open_at` through its archives, permanently, in exchange for a count.
#[test]
fn opting_in_keeps_the_archive_genesis_chain() {
    let dir = tmp("genesis-after-opt-in");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "a", vec![]).unwrap(); // frame 0
        db.enable_multiplicity().unwrap(); // frame 1, and writes snapshot.bin
        db.insert_node("N", "b", vec![]).unwrap(); // frame 2
        db.snapshot_with(opts_archive()).unwrap(); // first archive
    }
    assert!(
        dir.join("wal.genesis").exists(),
        "a keep_wal snapshot leaves the WAL whole, so the archive chain really \
         does start at genesis"
    );

    // The marker is only worth having if `open_at` honours it: frame 0 lives in
    // the archive, and replaying it from empty state is exactly what the genesis
    // chain licenses.
    let old = GraphDb::open_at(&dir, 0).unwrap();
    assert!(old.has_node("a"));
    assert!(!old.has_node("b"), "frame 2 is after the commit asked for");
}

/// The conservative half stays conservative: a snapshot from an *earlier
/// session* still refuses genesis, because this binary cannot tell a
/// history-preserving one from a truncating one once the handle that took it is
/// gone.
#[test]
fn a_prior_session_snapshot_still_refuses_genesis() {
    let dir = tmp("genesis-prior-session");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "a", vec![]).unwrap();
        db.enable_multiplicity().unwrap();
    }
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "b", vec![]).unwrap();
        db.snapshot_with(opts_archive()).unwrap();
    }
    assert!(
        !dir.join("wal.genesis").exists(),
        "snapshot.bin predates this session: the chain cannot be proven whole"
    );
}

/// A truncating snapshot taken in *this* session still refuses genesis — the
/// case the rule exists for, and the one the fix must not relax.
#[test]
fn a_truncating_snapshot_in_this_session_still_refuses_genesis() {
    let dir = tmp("genesis-truncated");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "a", vec![]).unwrap();
        db.snapshot().unwrap(); // keep_wal = false: history is gone
        db.insert_node("N", "b", vec![]).unwrap();
        db.snapshot_with(opts_archive()).unwrap();
    }
    assert!(
        !dir.join("wal.genesis").exists(),
        "a truncating snapshot broke the chain before the archive was taken"
    );
}

// ── #24: the primary ingest shape must count ─────────────────────────────────

/// **#24.** A mirror rebuild writes nodes and their edges in one frame. Before
/// this fix the count for such a pair was silently dropped, because the
/// endpoints have no dense id until the frame is rewritten — so the feature did
/// not count on the very shape that motivated it.
#[test]
fn a_duplicate_counts_when_its_endpoints_are_created_in_the_same_frame() {
    let dir = tmp("same-frame-endpoints");
    let mut db = GraphDb::open(&dir).unwrap();
    db.enable_multiplicity().unwrap();
    db.batch()
        .insert_node("N", "a", vec![])
        .insert_node("N", "b", vec![])
        .insert_edge("E", "a", "b")
        .insert_edge("E", "a", "b")
        .insert_edge("E", "a", "b")
        .commit()
        .unwrap();

    assert_eq!(
        db.degree("a", Some("E"), AlgoDir::Out).unwrap(),
        1,
        "adjacency is still a set"
    );
    assert_eq!(
        db.degree_multiplicity("a", Some("E"), AlgoDir::Out)
            .unwrap(),
        3,
        "all three inserts count, even though nothing in the frame existed \
         before the frame"
    );

    drop(db);
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(
        db.degree_multiplicity("a", Some("E"), AlgoDir::Out)
            .unwrap(),
        3,
        "and the count is durable"
    );
}

/// A deferred count still sits where the duplicate did. If the counts were
/// simply appended to the end of the frame instead of spliced back into
/// position, a later op in that same frame — here a delete of the pair — would
/// be overwritten, and the store would keep a count for an edge it does not
/// have.
#[test]
fn a_deferred_count_stays_in_front_of_a_later_delete_in_the_same_frame() {
    let dir = tmp("same-frame-delete");
    let mut db = GraphDb::open(&dir).unwrap();
    db.enable_multiplicity().unwrap();
    db.batch()
        .insert_node("N", "a", vec![])
        .insert_node("N", "b", vec![])
        .insert_edge("E", "a", "b")
        .insert_edge("E", "a", "b")
        .delete_edge("E", "a", "b")
        .commit()
        .unwrap();

    assert_eq!(db.degree("a", Some("E"), AlgoDir::Out).unwrap(), 0);
    assert_eq!(
        db.get_edge_prop("E", "a", "b", "count"),
        None,
        "the delete came after the count and must win"
    );
}

/// The edge *type* is interned in the same frame too. A count that resolved
/// endpoints but not the type would still drop this one.
#[test]
fn a_same_frame_duplicate_counts_under_a_brand_new_edge_type() {
    let dir = tmp("same-frame-etype");
    let mut db = GraphDb::open(&dir).unwrap();
    db.enable_multiplicity().unwrap();
    db.batch()
        .insert_node("N", "a", vec![])
        .insert_node("N", "b", vec![])
        .insert_edge("BRAND_NEW", "a", "b")
        .insert_edge("BRAND_NEW", "a", "b")
        .commit()
        .unwrap();
    assert_eq!(
        db.degree_multiplicity("a", Some("BRAND_NEW"), AlgoDir::Out)
            .unwrap(),
        2
    );
}

/// The opt-in still gates it. A store that never asked keeps writing nothing —
/// the line the whole feature rests on, checked on the path this fix added.
#[test]
fn a_same_frame_duplicate_writes_no_record_without_the_opt_in() {
    let dir = tmp("same-frame-off");
    let mut db = GraphDb::open(&dir).unwrap();
    db.batch()
        .insert_node("N", "a", vec![])
        .insert_node("N", "b", vec![])
        .insert_edge("E", "a", "b")
        .insert_edge("E", "a", "b")
        .commit()
        .unwrap();
    drop(db);
    let wal = std::fs::read(dir.join("wal.bin")).unwrap();
    assert!(
        !has_discriminant_23(&wal),
        "a store that never opted in must write no discriminant-23 record"
    );
}
