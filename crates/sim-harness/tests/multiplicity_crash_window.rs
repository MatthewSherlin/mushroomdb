//! The V10 guard has to hold at every interruption point, not just at the end
//! of a successful call.
//!
//! `enable_multiplicity()` writes two things: a V10 snapshot, which tells an
//! older reader to stop, and WAL discriminant 23, which that older reader would
//! otherwise silently truncate the store at. If a crash could leave the record
//! on disk without the snapshot, the store would spend that window in exactly
//! the state the whole change exists to prevent.
//!
//! So the snapshot goes first, and this sweep is what says so: crash on every
//! filesystem call the opt-in makes, and check the surviving store never holds
//! a discriminant-23 record without a V10 snapshot in front of it.

use core_storage::fs::{FileId, Fs};
use core_storage::{decode_all, WalRecord};
use mushroomdb::GraphDb;
use sim_harness::SimFs;

/// Insert a node, opt in, then write a duplicate edge so the WAL carries both
/// the declaration and a real count.
fn workload<F: Fs>(db: &mut GraphDb<F>) -> mushroomdb::Result<()> {
    db.insert_node("N", "a", vec![])?;
    db.insert_node("N", "b", vec![])?;
    db.enable_multiplicity()?;
    db.insert_edge("E", "a", "b")?;
    db.insert_edge("E", "a", "b")?;
    Ok(())
}

fn has_discriminant_23(wal: &[u8]) -> bool {
    let (frames, _) = decode_all(wal);
    frames.iter().any(|f| match f {
        WalRecord::SetEdgeCount { .. } => true,
        WalRecord::Batch(inner) => inner
            .iter()
            .any(|r| matches!(r, WalRecord::SetEdgeCount { .. })),
        _ => false,
    })
}

#[test]
fn no_crash_leaves_discriminant_23_unguarded() {
    let total_ops = {
        let mut db = GraphDb::open_with(SimFs::new()).unwrap();
        workload(&mut db).unwrap();
        db.into_fs().total_ops()
    };
    assert!(total_ops > 0, "the workload must touch the filesystem");

    let mut saw_guarded = 0usize;
    for crash_op in 0..=total_ops {
        // The low end of the sweep spends its whole budget inside `open_with`;
        // those crash points are before the store exists and have nothing to say.
        let Ok(mut db) = GraphDb::open_with(SimFs::with_crash_after_ops(crash_op)) else {
            continue;
        };
        let _ = workload(&mut db); // errors expected once the crash fires
        let survivor = db.into_fs().surviving_state();

        let wal = survivor.read(FileId::Wal).unwrap_or_default();
        let snap = survivor.read(FileId::Snapshot).unwrap_or_default();
        let snap_version = core_storage::snapshot::peek_version(&snap).unwrap_or(None);

        if has_discriminant_23(&wal) {
            assert_eq!(
                snap_version,
                Some(10),
                "crash_op={crash_op}: the WAL declares multiplicity with no V10 \
                 snapshot in front of it — a v0.6.9 reader would truncate here"
            );
            saw_guarded += 1;
        }

        // And the survivor still opens: the guard costs no recoverability.
        let recovered = GraphDb::open_with(survivor).unwrap();
        assert_eq!(
            recovered.is_multiplicity_enabled(),
            has_discriminant_23(&wal)
        );
    }

    assert!(
        saw_guarded > 0,
        "the sweep must actually reach states where the record is present"
    );
}
