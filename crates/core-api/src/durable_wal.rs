//! How much of the live WAL is known to be on stable storage.
//!
//! A reader outside the engine — the backup worker — must ship only bytes that
//! a local power loss cannot take back. The file's length is not that: it
//! counts bytes still in the page cache. This module is the one read-only
//! window onto what the engine has actually fsynced.
//!
//! The handle updates the position at the points where it already knows the
//! answer — right after a successful WAL fsync, and when a snapshot replaces
//! `wal.bin` — so the write path pays one uncontended mutex per fsync and no
//! extra I/O.

use std::sync::{Arc, Mutex};

/// A point in the live WAL that is on stable storage.
///
/// `len` always ends on a frame boundary: it is only ever set to the handle's
/// own frame cursor, which advances by whole frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WalPosition {
    /// Bumped every time this handle's `wal.bin` stops continuing the file it
    /// was before — a snapshot that truncates or archives it, or a reload
    /// after another process did. Offsets are only comparable within one
    /// generation. Starts at 0 when the handle opens; it is not persisted.
    pub generation: u64,
    /// Bytes of the current `wal.bin` known to be fsynced.
    pub len: u64,
    /// Global frame index one past the last durable frame — the index space
    /// [`GraphDb::open_at`](crate::GraphDb::open_at) and
    /// [`GraphDb::wal_total_commits`](crate::GraphDb::wal_total_commits) use.
    /// Meaningful only when `len > 0` or right after a completed snapshot.
    pub frames: u64,
    /// True while a snapshot is running. `snapshot.bin` and `wal.bin` may then
    /// be between states on disk (the new snapshot renamed in, the old WAL not
    /// yet replaced), so a reader pairing the two must wait.
    pub snapshot_in_progress: bool,
}

/// Shared cell behind [`WalPosition`]. Cheap to clone; every clone sees the
/// same position.
#[derive(Debug, Clone, Default)]
pub(crate) struct DurableWal(Arc<Mutex<WalPosition>>);

impl DurableWal {
    pub(crate) fn get(&self) -> WalPosition {
        *self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Everything up to `len` bytes (`frames` frames) of the current WAL is
    /// now fsynced.
    pub(crate) fn synced(&self, len: u64, frames: u64) {
        let mut p = self.0.lock().unwrap_or_else(|e| e.into_inner());
        p.len = len;
        p.frames = frames;
    }

    /// Like [`synced`](Self::synced), but only when still in `generation`
    /// and only forward: a later write's fsync may already have gone further,
    /// and a replaced WAL makes the offsets meaningless.
    pub(crate) fn synced_at_least(&self, generation: u64, len: u64, frames: u64) {
        let mut p = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if p.generation == generation && p.len < len {
            p.len = len;
            p.frames = frames;
        }
    }

    /// `wal.bin` was replaced: offsets start again in a new generation, and
    /// nothing of the new file is known durable yet.
    pub(crate) fn replaced(&self) {
        let mut p = self.0.lock().unwrap_or_else(|e| e.into_inner());
        p.generation += 1;
        p.len = 0;
        p.frames = 0;
    }

    /// Mark a snapshot as running until the returned guard drops.
    pub(crate) fn begin_snapshot(&self) -> SnapshotMark {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .snapshot_in_progress = true;
        SnapshotMark(self.clone())
    }
}

/// Clears [`WalPosition::snapshot_in_progress`] on drop, so every exit from a
/// snapshot — success, error or panic — clears it.
pub(crate) struct SnapshotMark(DurableWal);

impl Drop for SnapshotMark {
    fn drop(&mut self) {
        self.0
             .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .snapshot_in_progress = false;
    }
}
