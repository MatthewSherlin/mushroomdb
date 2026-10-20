//! How much of the live WAL is known to be on stable storage.
//!
//! A reader outside the engine — the backup worker — must ship only bytes that
//! a local power loss cannot take back. The file's length is not that: it
//! counts bytes still in the page cache. This module is the one read-only
//! window onto what the engine has actually fsynced.
//!
//! The handle updates the position at the points where it already knows the
//! answer — right after a successful WAL fsync, when a snapshot replaces
//! `wal.bin`, and when it loads or absorbs a WAL it did not write — so the
//! write path pays one uncontended mutex per fsync and no extra I/O.

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
    /// Global frame index one past the frame that ends at `len` — the index
    /// space [`GraphDb::open_at`](crate::GraphDb::open_at) and
    /// [`GraphDb::wal_total_commits`](crate::GraphDb::wal_total_commits) use.
    /// `None` while unknown: between a WAL replacement and the end of the
    /// snapshot that made it.
    pub frames: Option<u64>,
    /// Identity, as `(len, mtime_nanos)`, of the `snapshot.bin` this handle's
    /// state is based on; `None` for a store with no snapshot. A different
    /// value on disk means another process snapshotted and this handle has not
    /// caught up yet: its WAL offsets describe a file that is gone. Computed
    /// exactly as [`disk_snapshot_ident`] computes it.
    pub snapshot: Option<(u64, u64)>,
    /// Identity of the `wal.bin` file `len` is an offset into (the inode
    /// number on Unix; `None` elsewhere or when there is no WAL). A different
    /// file on disk means this handle has not caught up with a WAL another
    /// process replaced.
    pub wal: Option<u64>,
    /// True while a snapshot is running. `snapshot.bin` and `wal.bin` may then
    /// be between states on disk (the new snapshot renamed in, the old WAL not
    /// yet replaced), so a reader pairing the two must wait.
    pub snapshot_in_progress: bool,
}

/// The identity of `dir/snapshot.bin` in the form [`WalPosition::snapshot`]
/// uses, read from the file system.
pub fn disk_snapshot_ident(dir: &std::path::Path) -> std::io::Result<Option<(u64, u64)>> {
    use core_storage::fs::Fs as _;
    core_storage::fs::RealFs::new(dir)?.snapshot_ident()
}

/// The identity of `dir/wal.bin` in the form [`WalPosition::wal`] uses.
pub fn disk_wal_ident(dir: &std::path::Path) -> std::io::Result<Option<u64>> {
    use core_storage::fs::Fs as _;
    core_storage::fs::RealFs::new(dir)?.wal_ident()
}

#[derive(Debug, Default)]
struct State {
    pos: WalPosition,
    /// `(len, frames)` of WAL bytes this handle applied but did not write and
    /// fsync itself — what an open replayed, or what a refresh absorbed from
    /// another process. A reader of the position fsyncs them once and then
    /// counts them durable ([`DurableWal::vouch`]).
    unvouched: Option<(u64, u64)>,
}

/// Shared cell behind [`WalPosition`]. Cheap to clone; every clone sees the
/// same position.
#[derive(Debug, Clone, Default)]
pub(crate) struct DurableWal(Arc<Mutex<State>>);

impl DurableWal {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn get(&self) -> WalPosition {
        self.lock().pos
    }

    /// Everything up to `len` bytes (`frames` frames) of the current WAL is
    /// now fsynced.
    pub(crate) fn synced(&self, len: u64, frames: u64) {
        let mut s = self.lock();
        s.pos.len = len;
        s.pos.frames = Some(frames);
        if s.unvouched.is_some_and(|(l, _)| l <= len) {
            s.unvouched = None;
        }
    }

    /// `wal.bin` was replaced: offsets start again in a new generation, and
    /// nothing of the new file is known durable yet.
    pub(crate) fn replaced(&self) {
        let mut s = self.lock();
        s.pos.generation += 1;
        s.pos.len = 0;
        s.pos.frames = None;
        s.unvouched = None;
    }

    /// The handle loaded `snapshot` and a WAL of `len` bytes holding the
    /// frames from `base_frames` to `frames`. None of it is vouched for yet.
    pub(crate) fn loaded(
        &self,
        snapshot: Option<(u64, u64)>,
        wal: Option<u64>,
        base_frames: u64,
        len: u64,
        frames: u64,
    ) {
        let mut s = self.lock();
        s.pos.snapshot = snapshot;
        s.pos.wal = wal;
        s.pos.len = 0;
        s.pos.frames = Some(base_frames);
        s.unvouched = (len > 0).then_some((len, frames));
    }

    /// The handle absorbed another process's frames, up to `len` bytes and
    /// `frames` frames; that process may or may not have fsynced them.
    pub(crate) fn absorbed(&self, len: u64, frames: u64) {
        let mut s = self.lock();
        if len > s.pos.len {
            s.unvouched = Some((len, frames));
        }
    }

    /// The handle's state is now based on the snapshot identified `ident`,
    /// with its cursor in the WAL file identified `wal`.
    pub(crate) fn snapshot_is(&self, ident: Option<(u64, u64)>, wal: Option<u64>) {
        let mut s = self.lock();
        s.pos.snapshot = ident;
        s.pos.wal = wal;
    }

    /// `(generation, len, frames)` of bytes waiting to be vouched for.
    pub(crate) fn unvouched(&self) -> Option<(u64, u64, u64)> {
        let s = self.lock();
        s.unvouched.map(|(l, f)| (s.pos.generation, l, f))
    }

    /// An fsync covered the unvouched bytes `(generation, len, frames)`: count
    /// them durable, unless the WAL was replaced or went further meanwhile.
    pub(crate) fn vouch(&self, generation: u64, len: u64, frames: u64) {
        let mut s = self.lock();
        if s.pos.generation != generation {
            return;
        }
        if s.pos.len < len {
            s.pos.len = len;
            s.pos.frames = Some(frames);
        }
        if s.unvouched.is_some_and(|(l, _)| l <= len) {
            s.unvouched = None;
        }
    }

    /// Mark a snapshot as running until the returned guard drops.
    pub(crate) fn begin_snapshot(&self) -> SnapshotMark {
        self.lock().pos.snapshot_in_progress = true;
        SnapshotMark(self.clone())
    }
}

/// Clears [`WalPosition::snapshot_in_progress`] on drop, so every exit from a
/// snapshot — success, error or panic — clears it.
pub(crate) struct SnapshotMark(DurableWal);

impl Drop for SnapshotMark {
    fn drop(&mut self) {
        self.0.lock().pos.snapshot_in_progress = false;
    }
}
