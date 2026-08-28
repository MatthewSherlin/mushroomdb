//! Concurrent access to a [`GraphDb`] via a process-wide reader-writer lock,
//! plus a group-commit write queue that batches concurrent submissions behind
//! a single WAL fsync per group.
//!
//! # Group-commit design
//!
//! [`SharedDb::submit_batch`] enqueues a `Vec<BatchOp>` and blocks until the
//! containing **group** is durably committed.  A background drain thread owned
//! by `SharedDb` wakes on new submissions, drains up to `MAX_GROUP_SIZE`
//! pending submissions, acquires the write lock, applies each submission as a
//! separate WAL `Batch` frame (no per-submission fsync), **releases the write
//! lock**, then performs **one** fsync on the WAL file outside the lock.  Only
//! after the fsync completes does the drain thread signal each submitter.
//!
//! # Fsync-outside-guard
//!
//! Moving the WAL fsync outside the exclusive write-lock window means
//! concurrent readers can acquire the read lock while the fsync is in flight,
//! reducing p95 read latency under write bursts.  Readers may transiently
//! observe committed-but-not-yet-synced data during that window — the same
//! contract as `FsyncPolicy::Relaxed` — but submitters only receive `Ok`
//! after the fsync, so durability is fully guaranteed from their perspective.
//!
//! # Shutdown
//!
//! When the last `SharedDb` clone is dropped, `DrainHandle::drop` signals the
//! drain thread (via the `shutdown` flag + condvar wake) and joins it.  Any
//! submissions still queued at shutdown time receive an explicit IO error.

use crate::db::BatchOp;
use crate::reader::ReaderSnapshot;
use crate::GraphDb;
use core_storage::sync_wal_at;
use core_storage::GraphError;
use core_storage::RealFs;
use core_storage::Result;
use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread;

// ── Group-commit constants ────────────────────────────────────────────────────

/// Maximum submissions coalesced into one group.  Caps write-lock hold time
/// under extreme write bursts.
const MAX_GROUP_SIZE: usize = 256;

// ── Submission type ───────────────────────────────────────────────────────────

struct Submission {
    ops: Vec<BatchOp>,
    done: std::sync::mpsc::SyncSender<Result<(usize, usize)>>,
}

// ── WriteQueue ────────────────────────────────────────────────────────────────

struct WriteQueue {
    pending: Mutex<Vec<Submission>>,
    notify: Condvar,
    shutdown: AtomicBool,
}

impl WriteQueue {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            pending: Mutex::new(Vec::new()),
            notify: Condvar::new(),
            shutdown: AtomicBool::new(false),
        })
    }

    fn enqueue(&self, sub: Submission) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(sub);
        self.notify.notify_one();
    }

    fn signal_shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        self.notify.notify_all();
    }

    /// Block until work is available or shutdown; return at most `MAX_GROUP_SIZE`
    /// submissions.  Returns an empty `Vec` only when shutdown is set AND the
    /// queue is empty.
    fn wait_and_drain(&self) -> Vec<Submission> {
        let mut lock = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if !lock.is_empty() {
                let n = lock.len().min(MAX_GROUP_SIZE);
                return lock.drain(..n).collect();
            }
            if self.shutdown.load(Ordering::Acquire) {
                return vec![];
            }
            lock = self.notify.wait(lock).unwrap_or_else(|e| e.into_inner());
        }
    }
}

// ── DrainHandle ───────────────────────────────────────────────────────────────

/// Signals the drain thread and joins it when dropped.  Owned inside an
/// `Arc` so the last `SharedDb` clone triggers the join.
struct DrainHandle {
    queue: Arc<WriteQueue>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Drop for DrainHandle {
    fn drop(&mut self) {
        self.queue.signal_shutdown();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

// ── SharedDb ──────────────────────────────────────────────────────────────────

/// Shared handle to an on-disk [`GraphDb`]. [`Clone`] is cheap and shares state.
///
/// # Group-commit write path
///
/// [`SharedDb::submit_batch`] routes mutations through a group-commit queue.
/// A background drain thread batches concurrent submissions under a single WAL
/// fsync, yielding throughput that scales with concurrency.
///
/// # Direct write path
///
/// [`SharedDb::write`] gives exclusive `&mut GraphDb` access for callers that
/// already hold the lock or need complex multi-step mutations (e.g. Cypher
/// write queries).  The `&mut self` API on `GraphDb` is unchanged and is the
/// fast path for embedded / single-writer use.
///
/// # Event-sink deadlock
///
/// [`GraphDb::set_event_sink`] runs inside `log_then_apply` while the write
/// guard is held.  A sink must never call [`SharedDb::read`] or
/// [`SharedDb::write`] on the same handle.
#[derive(Clone)]
pub struct SharedDb {
    inner: Arc<RwLock<GraphDb<RealFs>>>,
    queue: Arc<WriteQueue>,
    /// Keeps the drain thread alive; signals + joins on last drop.
    _drain: Arc<DrainHandle>,
}

const _: () = {
    fn assert_send_sync<T: Send + Sync>() {}
    let _ = assert_send_sync::<SharedDb>;
};

impl SharedDb {
    pub fn open(dir: &Path) -> Result<Self> {
        let db = GraphDb::open(dir)?;
        Ok(Self::from_db_and_dir(db, dir.to_path_buf()))
    }

    fn from_db_and_dir(db: GraphDb<RealFs>, dir: std::path::PathBuf) -> Self {
        let inner = Arc::new(RwLock::new(db));
        let queue = WriteQueue::new();
        let dir_arc = Arc::new(dir);

        let drain_inner = Arc::clone(&inner);
        let drain_queue = Arc::clone(&queue);
        let drain_dir = Arc::clone(&dir_arc);

        let handle = thread::Builder::new()
            .name("groupcommit-drain".into())
            .spawn(move || drain_loop(drain_inner, drain_queue, drain_dir))
            .expect("failed to spawn group-commit drain thread");

        SharedDb {
            inner,
            queue: Arc::clone(&queue),
            _drain: Arc::new(DrainHandle {
                queue,
                handle: Some(handle),
            }),
        }
    }

    /// Shared read access. Many readers may hold this concurrently.
    ///
    /// # Deadlock warning
    ///
    /// Do not hold a returned guard while calling any method on the same
    /// [`SharedDb`]; the [`RwLock`] is not re-entrant; doing so deadlocks.
    pub fn read(&self) -> impl Deref<Target = GraphDb<RealFs>> + '_ {
        self.inner.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Exclusive write access. Blocks until no other readers or writers hold
    /// the lock.
    ///
    /// # Deadlock warning
    ///
    /// Do not hold a returned guard while calling any method on the same
    /// [`SharedDb`]; the [`RwLock`] is not re-entrant; doing so deadlocks.
    pub fn write(&self) -> impl DerefMut<Target = GraphDb<RealFs>> + '_ {
        self.inner.write().unwrap_or_else(|e| e.into_inner())
    }

    /// Capture a lock-free [`ReaderSnapshot`] of the current db state.
    ///
    /// Acquires the read lock only long enough to clone a handful of `Arc`
    /// handles.  Subsequent reads on the returned snapshot are lock-free.
    pub fn reader(&self) -> ReaderSnapshot {
        self.read().reader()
    }

    /// Enqueue a mutation batch for the group-committing writer.
    ///
    /// Blocks until the **containing group** is durably committed (one WAL
    /// fsync per group under `Strict` policy).  Submissions from concurrent
    /// callers are coalesced into groups of up to 256 items.
    ///
    /// # Durability semantics
    ///
    /// Under `Strict` policy (the default):
    /// - Each submission becomes a separate WAL `Batch` frame.
    /// - All frames in a group share one fsync — the caller unblocks only
    ///   after that fsync.
    /// - A crash between group fsyncs loses the entire unfsynced group, but
    ///   never tears an individual submission (CRC-protected frame boundaries).
    ///
    /// Under `Relaxed` policy (set via `db.write().set_fsync_policy`):
    /// - WAL frames are appended but NOT synced; caller unblocks after apply.
    ///
    /// # FIFO ordering
    ///
    /// Submissions from the same caller arrive FIFO at the queue.  Across
    /// concurrent callers, drain order within a group is arbitrary, but each
    /// submission's commit sequence is monotonically increasing.
    ///
    /// # Returns
    ///
    /// `(nodes_inserted, edges_inserted)` on success.  An all-noop batch
    /// returns `(0, 0)`.
    pub fn submit_batch(&self, ops: Vec<BatchOp>) -> Result<(usize, usize)> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.queue.enqueue(Submission { ops, done: tx });
        rx.recv().unwrap_or_else(|_| {
            Err(GraphError::Io(std::io::Error::other(
                "group-commit drain thread terminated unexpectedly",
            )))
        })
    }
}

// ── Drain thread ──────────────────────────────────────────────────────────────

fn drain_loop(
    inner: Arc<RwLock<GraphDb<RealFs>>>,
    queue: Arc<WriteQueue>,
    dir: Arc<std::path::PathBuf>,
) {
    loop {
        // Wait for work (or shutdown with empty queue).
        let mut group = queue.wait_and_drain();
        if group.is_empty() {
            return; // shutdown + nothing pending
        }

        // ── Apply all submissions under the write lock, NO fsync ─────────────
        //
        // Each submission's ops are moved out to avoid a clone.
        let ops_batches: Vec<Vec<BatchOp>> = group
            .iter_mut()
            .map(|s| std::mem::take(&mut s.ops))
            .collect();
        let results: Vec<Result<(usize, usize)>> = {
            let mut db = inner.write().unwrap_or_else(|e| e.into_inner());
            db.commit_group_nosync(ops_batches)
            // ← write lock released here; readers unblock before fsync
        };

        // ── ONE fsync for the group, OUTSIDE the write lock ──────────────────
        //
        // Readers may see committed-but-unfsynced data between here and the
        // fsync below (same contract as Relaxed).  Submitters unblock only
        // after the fsync, guaranteeing durability from their perspective.
        let sync_result: Result<()> = if results.iter().any(|r| r.is_ok()) {
            sync_wal_at(&dir).map_err(GraphError::Io)
        } else {
            Ok(()) // all submissions failed validation — nothing synced
        };

        // ── Signal each submitter ────────────────────────────────────────────
        for (sub, result) in group.into_iter().zip(results) {
            let final_result = match &sync_result {
                Ok(()) => result,
                Err(io_err) if result.is_ok() => {
                    // Committed to WAL but fsync failed.  Data is in kernel
                    // buffer but durability not guaranteed.  Report IO error.
                    Err(GraphError::Io(std::io::Error::other(io_err.to_string())))
                }
                Err(_) => result, // validation error stands
            };
            let _ = sub.done.send(final_result);
        }
    }
}
