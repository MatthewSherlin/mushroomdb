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
//! # WAL I/O lock order (load-bearing)
//!
//! A WAL mutex (`SharedDb::wal_mu`) serialises all WAL I/O — appends, fsyncs,
//! and truncations — across the drain thread and direct writers.
//!
//! **Required acquisition order (must be consistent in all code paths):**
//!
//! 1. `wal_mu`  — acquired first
//! 2. `inner` (RwLock write guard) — acquired second, while holding `wal_mu`
//!
//! [`SharedDb::write`] enforces this by acquiring `wal_mu` before the RwLock.
//! The drain thread acquires `wal_mu` before `inner.write()`.
//! Readers NEVER acquire `wal_mu`, nor the cross-process lock — neither a
//! concurrent fsync nor a peer process holding the store lock lengthens a read.
//! What a reader can wait on is another thread's RwLock write guard: a direct
//! writer's mutation, a group apply, or its own periodic refresh.
//!
//! Holding `wal_mu` from before [append group frames] through [fsync OR
//! truncation resolution] closes the truncation race: no concurrent direct
//! write can insert WAL frames between the group's append and its fsync
//! outcome, so `truncate_wal_at(pre_len)` is always a safe tail-trim.
//!
//! # Cross-process write lock
//!
//! The two locks above serialise writers inside this process. Across processes
//! the store carries an advisory exclusive lock on its `LOCK` file, and it is
//! taken **between** them, not last. The full order is:
//!
//! 1. `wal_mu`
//! 2. cross-process `LOCK` — polled here, with no RwLock guard held
//! 3. `inner` (RwLock write guard)
//!
//! Waiting for the cross-process lock is the slow step: a busy peer can hold it
//! for the whole [`WRITE_LOCK_WAIT`](crate::WRITE_LOCK_WAIT) budget. Polling it
//! before the RwLock is what keeps that wait off the read path — each attempt
//! takes the RwLock in *read* mode for one `try_lock` syscall and releases it,
//! so readers are never blocked by another process. Taking `inner.write()`
//! first would stall every reader in this process behind a peer in another one.
//!
//! Both WAL-appending paths take it: [`SharedDb::write`] for the whole guard
//! scope, and the drain thread for the whole of each group, from before the
//! first append until after the group's fsync resolves. Releasing it only after
//! the fsync is what stops another process's snapshot from truncating bytes
//! this one has written but not yet made durable. `snapshot_with` refuses to
//! run without it, for the same reason.
//!
//! Entering the write scope also refreshes, so a write always applies on top of
//! every other process's commits. A writer that cannot get the lock within its
//! wait budget fails with [`GraphError::Busy`], having written nothing.
//!
//! Readers take neither `wal_mu` nor the cross-process lock. They check for
//! other processes' commits on every read — a metadata-only staleness check —
//! and apply the WAL tail under the in-process write lock, which is the same
//! lock the drain thread holds while it appends and applies — so a refresh
//! never observes a half-committed group.
//!
//! # Fsync-failure contract
//!
//! If the group fsync fails the drain thread immediately:
//! 1. **Truncates** the WAL file back to the pre-group offset — this prevents
//!    a later successful fsync from silently making the failed group durable
//!    by flushing the full inode page cache.  The truncation is safe because
//!    `wal_mu` prevents any concurrent write from appending after `pre_len`.
//! 2. **Marks** the database degraded via [`GraphDb::set_degraded`] — all
//!    subsequent [`submit_batch`] and `db.write()` mutation attempts return
//!    an IO error until the database is reopened.
//! 3. **Discards** buffered event notifications (no subscriber sees un-durable
//!    data).
//! 4. **Signals** all submitters in the failed group with an IO error.
//! 5. **Exits** the drain loop.
//!
//! Readers may have already observed the failed group's data (between the
//! write-lock release and the truncation); that window is equivalent to the
//! `Relaxed` durability contract.
//!
//! # Event ordering (Strict policy, R2)
//!
//! Under `FsyncPolicy::Strict` or `Batched`, subscription events are deferred
//! until after the group fsync.  The drain thread then reacquires the write lock
//! (while still holding `wal_mu`) to call [`GraphDb::flush_deferred_events`],
//! releases the write lock, releases `wal_mu`, and finally signals submitters.
//! Flushing events while `wal_mu` is held prevents a concurrent direct writer
//! from slipping in between the group fsync and the event flush and delivering
//! its event before the group's events — preserving a global monotone event
//! order across both the drain path and the direct write path.
//! Under `Relaxed`, events fire immediately (no fsync to wait for).
//!
//! # Event delivery and crashes (R2)
//!
//! Subscription events are best-effort post-durability notifications.  A crash
//! between a successful group fsync and the `flush_deferred_events` call drops
//! those events — a strictly narrower loss window than pre-4b (where events
//! could fire before any fsync).
//!
//! # Shutdown
//!
//! `SharedDb` clones the drain handle via an `Arc`; the last clone to drop
//! triggers `DrainHandle::drop`, which signals shutdown + joins the thread.
//! The drain thread can never exit while any `SharedDb` clone exists (the
//! `Arc<Inner>` is held by every clone); no submission enqueued before the
//! last clone is dropped can be silently lost.

use crate::db::{
    BatchOp, FsyncPolicy, Precondition, WriteAuthz, LOCK_POLL_INTERVAL, WRITE_LOCK_WAIT,
};
use crate::reader::ReaderSnapshot;
use crate::GraphDb;
use core_storage::sync_wal_at;
use core_storage::truncate_wal_at;
use core_storage::GraphError;
use core_storage::RealFs;
use core_storage::Result;
use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

// ── Group-commit constants ────────────────────────────────────────────────────

/// Maximum submissions coalesced into one group.  Caps write-lock hold time
/// under extreme write bursts.
const MAX_GROUP_SIZE: usize = 256;

// ── WAL sync function type ────────────────────────────────────────────────────

/// A callable that syncs the WAL at a given directory path.
///
/// In production this is always `sync_wal_at`.  Tests may inject a failing
/// implementation via [`SharedDb::open_with_test_sync`] to exercise the
/// fsync-failure contract through the live drain thread.
type SyncWalFn = Arc<dyn Fn(&Path) -> std::io::Result<()> + Send + Sync>;

// ── Submission type ───────────────────────────────────────────────────────────

struct Submission {
    ops: Vec<BatchOp>,
    /// Compare-and-set preconditions.  Empty for plain `submit_batch` calls;
    /// non-empty for `submit_batch_cas` calls.  The drain thread checks these
    /// under the same write guard as the batch apply (no TOCTOU).
    preconds: Vec<Precondition>,
    /// Role name for role-scoped write authz.  `Some` only for
    /// `submit_batch_authz` calls; `None` for full-authority submissions.
    /// The drain thread resolves mask + scope under `inner.write()` (§5 lock
    /// discipline: authz check and mutation share one guard lifetime).
    authz_role: Option<String>,
    done: std::sync::mpsc::SyncSender<Result<(usize, usize)>>,
}

// ── WriteQueue ────────────────────────────────────────────────────────────────

struct WriteQueue {
    pending: Mutex<Vec<Submission>>,
    notify: Condvar,
    shutdown: AtomicBool,
    /// Set by the drain thread on a group fsync failure.  Non-None means the
    /// drain thread has exited; future `submit_batch` calls return Err immediately
    /// rather than blocking forever on a dead drain thread.
    degraded_msg: Mutex<Option<String>>,
}

impl WriteQueue {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            pending: Mutex::new(Vec::new()),
            notify: Condvar::new(),
            shutdown: AtomicBool::new(false),
            degraded_msg: Mutex::new(None),
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
        // Hold the queue mutex while flagging + notifying: the drain thread
        // checks `shutdown` only under this mutex, so the notify can no longer
        // fire between its predicate check and Condvar::wait (missed wakeup).
        let _lock = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        self.shutdown.store(true, Ordering::Release);
        self.notify.notify_all();
    }

    fn set_degraded(&self, msg: String) {
        *self.degraded_msg.lock().unwrap_or_else(|e| e.into_inner()) = Some(msg);
    }

    fn degraded_message(&self) -> Option<String> {
        self.degraded_msg
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
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

// ── WriteGuard ────────────────────────────────────────────────────────────────

/// Compound write guard returned by [`SharedDb::write`].
///
/// Holds both the WAL mutex and the RwLock write guard.  Fields are declared
/// in drop order — `inner` (RwLock) releases first, then `_wal` (WAL mutex)
/// — preserving the lock-release ordering required by the WAL I/O discipline.
///
/// # Lock order
///
/// Acquisition: `wal_mu` → cross-process `LOCK` → `inner` (RwLock write). The
/// cross-process lock is polled with no RwLock write guard held, so a peer
/// process holding it never stalls this process's readers.
/// Release: cross-process `LOCK` (in `Drop`), then `inner`, then `wal_mu`
/// (RAII, struct field declaration order).
///
/// # Cross-process lock
///
/// Constructing the guard also takes the store's advisory cross-process write
/// lock and refreshes, so mutations made through it land on top of every other
/// process's commits. If the lock could not be taken within the caller's wait
/// budget, the guard is still returned but every mutation on it fails with
/// [`GraphError::Busy`] — use [`SharedDb::write_with_wait`] to see the failure
/// up front instead.
pub struct WriteGuard<'a> {
    /// RwLock write guard — dropped first (field declared first).
    inner: std::sync::RwLockWriteGuard<'a, GraphDb<RealFs>>,
    /// WAL mutex guard — dropped second (field declared second).
    _wal: std::sync::MutexGuard<'a, ()>,
}

impl<'a> Drop for WriteGuard<'a> {
    fn drop(&mut self) {
        // Release the cross-process lock before either in-process lock: another
        // process may be polling for it, and there is nothing left to serialise.
        self.inner.end_write_lock();
    }
}

impl<'a> Deref for WriteGuard<'a> {
    type Target = GraphDb<RealFs>;
    fn deref(&self) -> &GraphDb<RealFs> {
        &self.inner
    }
}

impl<'a> DerefMut for WriteGuard<'a> {
    fn deref_mut(&mut self) -> &mut GraphDb<RealFs> {
        &mut self.inner
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
/// need complex multi-step mutations (e.g. Cypher write queries).  It acquires
/// the WAL mutex first, then the RwLock write guard, satisfying the lock-order
/// discipline described in the module doc.
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
    /// WAL I/O mutex.  Serialises all WAL appends, fsyncs, and truncations
    /// across the drain thread and direct writers.  See module-level doc for
    /// the required acquisition order.
    wal_mu: Arc<Mutex<()>>,
}

const _: () = {
    fn assert_send_sync<T: Send + Sync>() {}
    let _ = assert_send_sync::<SharedDb>;
};

impl SharedDb {
    /// Open the store at `dir` as a shared, multi-reader handle.
    ///
    /// Unlike a plain [`GraphDb`], this does not hold the store's cross-process
    /// write lock for the handle's lifetime — a server would otherwise lock out
    /// every other process for as long as it runs. The lock is taken per write
    /// instead, and reads follow other processes' commits automatically.
    pub fn open(dir: &Path) -> Result<Self> {
        let db = GraphDb::open_unlocked(dir)?;
        Ok(Self::from_db_and_dir_with_sync(
            db,
            dir.to_path_buf(),
            Arc::new(sync_wal_at),
        ))
    }

    /// Open with an injectable WAL sync function.
    ///
    /// Allows tests to inject fsync failures through the live drain thread
    /// without requiring real filesystem manipulation.  Not intended for
    /// production use; the `test_sync` name signals its purpose.
    pub fn open_with_test_sync(
        dir: &Path,
        sync: impl Fn(&Path) -> std::io::Result<()> + Send + Sync + 'static,
    ) -> Result<Self> {
        let db = GraphDb::open_unlocked(dir)?;
        Ok(Self::from_db_and_dir_with_sync(
            db,
            dir.to_path_buf(),
            Arc::new(sync),
        ))
    }

    fn from_db_and_dir_with_sync(
        db: GraphDb<RealFs>,
        dir: std::path::PathBuf,
        sync_fn: SyncWalFn,
    ) -> Self {
        let inner = Arc::new(RwLock::new(db));
        let queue = WriteQueue::new();
        let wal_mu = Arc::new(Mutex::new(()));
        let dir_arc = Arc::new(dir);

        let drain_inner = Arc::clone(&inner);
        let drain_queue = Arc::clone(&queue);
        let drain_dir = Arc::clone(&dir_arc);
        let drain_wal_mu = Arc::clone(&wal_mu);
        let drain_sync_fn = Arc::clone(&sync_fn);

        let handle = thread::Builder::new()
            .name("groupcommit-drain".into())
            .spawn(move || {
                drain_loop(
                    drain_inner,
                    drain_queue,
                    drain_dir,
                    drain_wal_mu,
                    drain_sync_fn,
                )
            })
            .expect("failed to spawn group-commit drain thread");

        SharedDb {
            inner,
            queue: Arc::clone(&queue),
            _drain: Arc::new(DrainHandle {
                queue,
                handle: Some(handle),
            }),
            wal_mu,
        }
    }

    /// Check whether another process has committed, and if so absorb its
    /// commits.
    ///
    /// Runs on every read. The check is metadata-only — one `stat` of the WAL,
    /// and a second of the snapshot when the WAL length is unchanged — which
    /// is the price of a handle that follows its peers.
    ///
    /// It is deliberately not rate-limited by a timer. A wall-clock window
    /// makes visibility depend on how fast the peer happened to be: a read
    /// landing inside the window returns a view that predates a commit which
    /// had already completed, while the same read on a slower machine would
    /// have seen it. The child process in `multiprocess.rs` test 1 writes its
    /// 100 nodes in ~11 ms on Linux — well inside the 50 ms window this check
    /// used to keep, and outside it on macOS, where spawning the child costs
    /// more than the window itself.
    ///
    /// Readers never touch the cross-process lock and never block on it: the
    /// worst a reader does is take the in-process write lock briefly to apply
    /// the WAL tail. Applying the tail under that lock is what makes the check
    /// safe against a concurrent group commit — the drain thread appends,
    /// applies, and advances the cursor under the same lock, so a refresh never
    /// sees a half-committed group and never replays one twice.
    ///
    /// A staleness check that fails (an unreadable store directory, say) is
    /// treated as "not stale": a read must not fail because the store might
    /// have moved on.
    fn refresh_if_stale(&self) {
        let stale = self
            .inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_stale()
            .unwrap_or(false);
        if stale {
            let mut db = self.inner.write().unwrap_or_else(|e| e.into_inner());
            if let Err(e) = db.refresh() {
                // `refresh` degrades the handle on failure, so mutations
                // already refuse. Tell the write queue too, so a submitter gets
                // the error immediately instead of blocking on a drain thread
                // whose next group will fail anyway.
                self.queue.set_degraded(format!(
                    "refresh failed while following another process's commits: {e}; \
                     reopen required"
                ));
            }
        }
    }

    /// Shared read access. Many readers may hold this concurrently.
    ///
    /// Readers never acquire the WAL mutex and never wait on the cross-process
    /// write lock, so neither a concurrent fsync nor a peer process writing the
    /// store lengthens a read. A reader can still wait on another thread's
    /// RwLock write guard, including the refresh below.
    ///
    /// # Following other processes
    ///
    /// Every read checks whether another process has committed and, if so,
    /// applies its commits before handing out the guard. A handle therefore
    /// stays current without reopening, and the guarantee is not a timing
    /// accident: a read started after a peer's commit completed sees that
    /// commit. The check is metadata-only — one or two `stat` calls, no file
    /// contents — so a read loop pays a syscall, not a reload.
    ///
    /// # Deadlock warning
    ///
    /// Do not hold a returned guard while calling any method on the same
    /// [`SharedDb`]; the [`RwLock`] is not re-entrant; doing so deadlocks.
    pub fn read(&self) -> impl Deref<Target = GraphDb<RealFs>> + '_ {
        self.refresh_if_stale();
        self.inner.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Exclusive write access, waiting up to [`WRITE_LOCK_WAIT`] for the
    /// store's cross-process write lock.
    ///
    /// Acquires the WAL mutex first, then the cross-process lock, then the
    /// RwLock write guard — the order required by the fsync-failure contract
    /// (see module doc). Waiting for the cross-process lock happens with no
    /// RwLock write guard held, which is what keeps a busy peer process off
    /// this process's read path. The returned [`WriteGuard`] releases the
    /// cross-process lock, then the RwLock, then the WAL mutex on drop.
    ///
    /// # When another process holds the lock
    ///
    /// The guard is still returned, but every mutation through it fails with
    /// [`GraphError::Busy`] and writes nothing. Call
    /// [`write_with_wait`](SharedDb::write_with_wait) when you would rather see
    /// that up front, or choose your own wait budget.
    ///
    /// # Deadlock warning
    ///
    /// Do not hold a returned guard while calling any method on the same
    /// [`SharedDb`]; the [`RwLock`] is not re-entrant; doing so deadlocks.
    pub fn write(&self) -> WriteGuard<'_> {
        let _wal = self.wal_mu.lock().unwrap_or_else(|e| e.into_inner());
        let acquired = self
            .poll_cross_process_lock(WRITE_LOCK_WAIT)
            .unwrap_or(false);
        // The outcome is latched on the db itself, so it surfaces on the first
        // mutation: a denied lock as `Busy`, and a refresh failure under a
        // taken lock as the degraded error (`refresh` marks the handle).
        self.enter_write_scope(_wal, acquired).0
    }

    /// Like [`write`](SharedDb::write) but with an explicit wait budget, and
    /// [`GraphError::Busy`] returned up front when the cross-process write lock
    /// is not free within it.
    ///
    /// A zero wait makes exactly one attempt. Nothing is written on failure, so
    /// retrying later is always safe.
    pub fn write_with_wait(&self, wait: Duration) -> Result<WriteGuard<'_>> {
        let _wal = self.wal_mu.lock().unwrap_or_else(|e| e.into_inner());
        if !self.poll_cross_process_lock(wait)? {
            return Err(GraphError::Busy { holder: None });
        }
        let (guard, entered) = self.enter_write_scope(_wal, true);
        entered?;
        Ok(guard)
    }

    /// Poll for the store's cross-process write lock, holding **no** in-process
    /// guard while waiting.
    ///
    /// Lock order is `wal_mu` → cross-process `LOCK` → `inner` (RwLock write).
    /// The caller already holds `wal_mu`, which is what keeps two writers in
    /// this process from polling at once. Each attempt takes the RwLock in
    /// *read* mode for the length of one `try_lock` syscall and releases it, so
    /// concurrent readers are never blocked by a peer process: waiting for a
    /// busy peer costs readers nothing.
    fn poll_cross_process_lock(&self, wait: Duration) -> Result<bool> {
        let deadline = Instant::now() + wait;
        loop {
            {
                let db = self.inner.read().unwrap_or_else(|e| e.into_inner());
                if db.try_cross_process_lock()? {
                    return Ok(true);
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(false);
            }
            std::thread::sleep(LOCK_POLL_INTERVAL.min(deadline.saturating_duration_since(now)));
        }
    }

    /// Take the RwLock write guard, wrap both in-process guards in a
    /// [`WriteGuard`], and open the write scope with the lock outcome the
    /// caller already obtained.
    ///
    /// The guard is returned even when the scope failed to open, so its `Drop`
    /// always releases the cross-process lock; the failure rides on the second
    /// tuple element and is latched on the handle for `write()`'s benefit.
    fn enter_write_scope<'a>(
        &'a self,
        wal: std::sync::MutexGuard<'a, ()>,
        acquired: bool,
    ) -> (WriteGuard<'a>, Result<()>) {
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let entered = inner.enter_write_scope(acquired);
        (WriteGuard { inner, _wal: wal }, entered)
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
    /// - **Fsync failure**: the drain thread truncates the WAL back to the
    ///   pre-group offset and marks the database degraded.  All submitters in
    ///   the failed group and all subsequent callers receive `Err`.  Data that
    ///   was already in readers' snapshots (observed between write-lock release
    ///   and truncation) is not rolled back — equivalent to the `Relaxed`
    ///   window for in-flight readers.  Reopen the database to recover.
    /// - A crash between group fsyncs loses the entire unfsynced group, but
    ///   never tears an individual submission (CRC-protected frame boundaries).
    ///
    /// Under `Relaxed` policy (set via `db.write().set_fsync_policy`):
    /// - WAL frames are appended but NOT synced; caller unblocks after apply.
    ///
    /// # Event ordering
    ///
    /// Under `Strict` / `Batched` policy, subscription events fire AFTER the
    /// group fsync (durability before notification).  Under `Relaxed`, events
    /// fire immediately after apply.
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
        // Fast-path rejection: if the drain thread already exited due to a
        // fsync failure, return Err immediately rather than blocking forever.
        if let Some(msg) = self.queue.degraded_message() {
            return Err(GraphError::Io(std::io::Error::other(msg)));
        }
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.queue.enqueue(Submission {
            ops,
            preconds: Vec::new(),
            authz_role: None,
            done: tx,
        });
        rx.recv().unwrap_or_else(|_| {
            Err(GraphError::Io(std::io::Error::other(
                "group-commit drain thread terminated unexpectedly",
            )))
        })
    }

    /// Like [`submit_batch`] but with compare-and-set preconditions.
    ///
    /// The preconditions are evaluated by the drain thread under the **same**
    /// write guard as the batch apply — there is no TOCTOU window.  If any
    /// precondition fails, the entire batch is rejected with
    /// [`core_storage::GraphError::CasConflict`] and no WAL frame is written.
    ///
    /// See [`crate::Precondition`] for the full semantics.
    pub fn submit_batch_cas(
        &self,
        preconds: Vec<Precondition>,
        ops: Vec<BatchOp>,
    ) -> Result<(usize, usize)> {
        if let Some(msg) = self.queue.degraded_message() {
            return Err(GraphError::Io(std::io::Error::other(msg)));
        }
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.queue.enqueue(Submission {
            ops,
            preconds,
            authz_role: None,
            done: tx,
        });
        rx.recv().unwrap_or_else(|_| {
            Err(GraphError::Io(std::io::Error::other(
                "group-commit drain thread terminated unexpectedly",
            )))
        })
    }

    /// Like [`submit_batch`] but with role-scoped write authorization.
    ///
    /// The drain thread resolves `mask_for_role` + scope under the same write
    /// guard as the mutation (§5 lock discipline: authz BEFORE any CAS
    /// preconditions, BEFORE the WAL write).
    ///
    /// - Role with `write: None` → `GraphError::RoleWriteDenied` (endpoint not
    ///   permitted) — maps to HTTP 403.
    /// - Scope / visibility violations inside the batch → `GraphError::RoleWriteDenied`
    ///   with the appropriate §4.3 reason string.
    ///
    /// All-or-nothing semantics: a single denied op rejects the entire batch
    /// with no WAL frame written.
    pub fn submit_batch_authz(&self, role: String, ops: Vec<BatchOp>) -> Result<(usize, usize)> {
        if let Some(msg) = self.queue.degraded_message() {
            return Err(GraphError::Io(std::io::Error::other(msg)));
        }
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.queue.enqueue(Submission {
            ops,
            preconds: Vec::new(),
            authz_role: Some(role),
            done: tx,
        });
        rx.recv().unwrap_or_else(|_| {
            Err(GraphError::Io(std::io::Error::other(
                "group-commit drain thread terminated unexpectedly",
            )))
        })
    }
}

// ── Drain thread ──────────────────────────────────────────────────────────────

/// Poll for the store's cross-process write lock without holding the RwLock
/// write guard, so a busy peer never stalls this process's readers.
///
/// The free-function twin of [`SharedDb::poll_cross_process_lock`]; the drain
/// thread has the `Arc<RwLock<..>>` but no `SharedDb`. The caller must already
/// hold `wal_mu`.
fn poll_cross_process_lock(inner: &Arc<RwLock<GraphDb<RealFs>>>, wait: Duration) -> Result<bool> {
    let deadline = Instant::now() + wait;
    loop {
        {
            let db = inner.read().unwrap_or_else(|e| e.into_inner());
            if db.try_cross_process_lock()? {
                return Ok(true);
            }
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(false);
        }
        std::thread::sleep(LOCK_POLL_INTERVAL.min(deadline.saturating_duration_since(now)));
    }
}

fn drain_loop(
    inner: Arc<RwLock<GraphDb<RealFs>>>,
    queue: Arc<WriteQueue>,
    dir: Arc<std::path::PathBuf>,
    wal_mu: Arc<Mutex<()>>,
    sync_fn: SyncWalFn,
) {
    loop {
        // Wait for work (or shutdown with empty queue).
        let mut group = queue.wait_and_drain();
        if group.is_empty() {
            return; // shutdown + nothing pending
        }

        // Extract ops, preconditions, and authz_role together; keep alignment
        // with group index.
        let submissions: Vec<(Vec<Precondition>, Vec<BatchOp>, Option<String>)> = group
            .iter_mut()
            .map(|s| {
                (
                    std::mem::take(&mut s.preconds),
                    std::mem::take(&mut s.ops),
                    s.authz_role.take(),
                )
            })
            .collect();

        // ── Step 1: Acquire WAL mutex BEFORE the write lock ─────────────────
        //
        // Lock order: wal_mu → RwLock write guard.
        //
        // Holding wal_mu from here through [fsync OR truncation resolution]
        // closes the truncation race: no concurrent db.write() caller can
        // append WAL frames between the group's own appends and its fsync
        // outcome.  truncate_wal_at(pre_len) is therefore always a safe
        // tail-trim with no risk of wiping acknowledged direct writes.
        let wal_guard = wal_mu.lock().unwrap_or_else(|e| e.into_inner());

        // ── Step 1b: Take the cross-process write lock for this group ────────
        //
        // The drain thread appends to the WAL without going through
        // `SharedDb::write`, so it must serialise against other processes
        // itself.  Polling happens with NO RwLock guard held (lock order:
        // wal_mu → cross-process LOCK → inner.write), so a busy peer cannot
        // stall this process's readers for the duration of the wait.
        // `enter_write_scope` then refreshes under the lock, so the group
        // applies on top of every commit another process has made.  The lock is
        // released only after this group's fsync resolves: until then the tail
        // contains bytes we have not made durable, and another process's
        // snapshot must not truncate them away.
        let lock_outcome = poll_cross_process_lock(&inner, WRITE_LOCK_WAIT).and_then(|acquired| {
            let mut db = inner.write().unwrap_or_else(|e| e.into_inner());
            db.enter_write_scope(acquired).map(|()| acquired)
        });
        match lock_outcome {
            Ok(true) => {}
            Ok(false) => {
                // Another process is writing.  Every submitter in this group is
                // told so; the drain loop stays alive and the next group tries
                // again — a busy peer is a transient condition, not a failure
                // of this database handle.
                {
                    let mut db = inner.write().unwrap_or_else(|e| e.into_inner());
                    db.end_write_lock();
                }
                drop(wal_guard);
                for sub in group {
                    let _ = sub.done.send(Err(GraphError::Busy { holder: None }));
                }
                continue;
            }
            Err(e) => {
                // The lock was taken but the refresh under it failed, which
                // leaves the handle degraded (see `GraphDb::refresh`).  This is
                // the same class of failure as a group fsync failure: the
                // database must be reopened, so the drain loop exits rather
                // than failing every future group identically.
                let reason = format!("refresh under the write lock failed: {e}; reopen required");
                {
                    let mut db = inner.write().unwrap_or_else(|e| e.into_inner());
                    db.discard_deferred_events();
                    db.set_deferred_events_mode(false);
                    db.set_degraded();
                    db.end_write_lock();
                }
                queue.set_degraded(reason.clone());
                drop(wal_guard);
                for sub in group {
                    let _ = sub
                        .done
                        .send(Err(GraphError::Io(std::io::Error::other(reason.clone()))));
                }
                return;
            }
        }

        // Snapshot WAL size while holding wal_mu — no concurrent WAL append
        // is possible, so this offset is a stable pre-group boundary.
        let pre_group_wal_len = std::fs::metadata(dir.join("wal.bin"))
            .map(|m| m.len())
            .unwrap_or(0);

        // ── Step 2: Apply all submissions under the write lock, NO fsync ────
        //
        // For Strict / Batched policy we enable deferred event mode so that
        // subscription notifications only fire after the group fsync (R2:
        // durability before notification).  For Relaxed policy events fire
        // immediately (no fsync to wait for).
        let (results, should_sync): (Vec<Result<(usize, usize)>>, bool) = {
            let mut db = inner.write().unwrap_or_else(|e| e.into_inner());
            let sync_needed = db.fsync_policy() != FsyncPolicy::Relaxed;
            if sync_needed {
                db.set_deferred_events_mode(true);
            }
            // Apply submissions in FIFO order.
            //
            // Non-CAS submissions are coalesced into a single commit_group_nosync
            // call (preserving the T4b group-write batching intent — one write
            // boundary per drain group, not per submission).  CAS submissions
            // break the coalescing because their preconditions must be evaluated
            // AFTER all prior submissions in the group have been applied (no
            // TOCTOU); they are processed individually between coalesced non-CAS
            // runs.
            let mut r: Vec<Result<(usize, usize)>> = Vec::with_capacity(submissions.len());
            let mut pending_non_cas: Vec<Vec<BatchOp>> = Vec::new();

            for (preconds, ops, authz_role) in submissions {
                if let Some(role) = authz_role {
                    // Authz submission: process individually (breaks coalescing).
                    // Flush accumulated non-CAS batch first so authz evaluation
                    // sees their writes already applied.
                    if !pending_non_cas.is_empty() {
                        let batch = std::mem::take(&mut pending_non_cas);
                        r.extend(db.commit_group_nosync(batch));
                    }
                    // Resolve WriteAuthz under the write guard (§5 lock discipline:
                    // scope + mask resolved in the same guard as the mutation;
                    // authz check fires BEFORE any WAL write).
                    let result = (|| -> Result<(usize, usize)> {
                        let scope = {
                            // Temporary scope so the borrow on db.roles ends
                            // before write_batch_authz_nosync borrows db mutably.
                            let roles_vec = db.roles();
                            let def = roles_vec
                                .iter()
                                .find(|r| r.name == role)
                                .ok_or_else(|| GraphError::KeyNotFound {
                                    key: format!("role:{role}"),
                                })?
                                .clone();
                            // write:None → byte-identical v1 blanket-403 body.
                            def.write.ok_or_else(|| GraphError::RoleWriteDenied {
                                reason: "role-bound token: writes are not permitted".into(),
                            })?
                        };
                        let mask = db.mask_for_role(&role)?;
                        let authz = WriteAuthz { role, scope, mask };
                        db.write_batch_authz_nosync(Some(&authz), ops)
                    })();
                    r.push(result);
                } else if preconds.is_empty() {
                    // Non-CAS: accumulate for a batched commit_group_nosync call.
                    pending_non_cas.push(ops);
                } else {
                    // CAS: flush accumulated non-CAS batch first so that precond
                    // evaluation sees their writes already applied.
                    if !pending_non_cas.is_empty() {
                        let batch = std::mem::take(&mut pending_non_cas);
                        r.extend(db.commit_group_nosync(batch));
                    }
                    let result = match db.check_preconditions(&preconds) {
                        Ok(()) => db
                            .commit_group_nosync(vec![ops])
                            .into_iter()
                            .next()
                            .unwrap_or(Ok((0, 0))),
                        Err(e) => Err(e),
                    };
                    r.push(result);
                }
            }
            // Flush any remaining non-CAS submissions.
            if !pending_non_cas.is_empty() {
                r.extend(db.commit_group_nosync(pending_non_cas));
            }
            (r, sync_needed)
            // ← RwLock write guard released here; wal_mu still held
        };

        // ── Step 3: ONE fsync for the group, OUTSIDE the write lock ─────────
        //
        // Readers may see committed-but-unfsynced data between here and the
        // fsync below (same contract as Relaxed).  Submitters unblock only
        // after the fsync, guaranteeing durability from their perspective.
        // wal_mu remains held so no concurrent writer can extend the WAL tail.
        let sync_result: Result<()> = if should_sync && results.iter().any(|r| r.is_ok()) {
            sync_fn(&dir).map_err(GraphError::Io)
        } else {
            Ok(()) // Relaxed policy or all submissions failed validation
        };

        // ── Step 4: Handle fsync failure ─────────────────────────────────────
        if let Err(ref io_err) = sync_result {
            if results.iter().any(|r| r.is_ok()) {
                // Truncate WAL to the pre-group boundary.  Safe because wal_mu
                // is held — no other writer can have appended since pre_len was
                // measured, so this always removes exactly the failed group's
                // frames and nothing else.
                let _ = truncate_wal_at(&dir, pre_group_wal_len);
            }
            // Acquire write lock to update in-memory degraded state.
            // Lock order maintained: wal_mu (held) → RwLock write.
            {
                let mut db = inner.write().unwrap_or_else(|e| e.into_inner());
                db.discard_deferred_events();
                db.set_deferred_events_mode(false);
                // Mark degraded so db.write().insert_node(...) etc. also fail.
                db.set_degraded();
                // The truncation shortened the WAL: bring the frame cursor back
                // to the file's real length so a later staleness check does not
                // read our own in-memory state as a peer's rollback.
                db.set_wal_consumed(pre_group_wal_len);
                db.end_write_lock();
            }
            // Propagate failure to queue BEFORE releasing wal_mu so any
            // direct writer waiting for wal_mu sees the degraded flag when
            // it wakes (and log_then_apply_with will return Err(degraded)).
            queue.set_degraded(io_err.to_string());
            // Release WAL mutex — no more WAL I/O will happen.
            drop(wal_guard);
            // Signal each submitter with an IO error.
            for sub in group {
                let _ = sub.done.send(Err(GraphError::Io(std::io::Error::other(
                    "group-commit fsync failed; database is degraded, reopen required",
                ))));
            }
            return; // drain loop exits; no further groups accepted
        }

        // ── Step 5: Flush deferred events AFTER successful fsync (R2) ────────
        //
        // Flush events while wal_mu is still held so no direct writer can slip
        // between this group's fsync and its event delivery.  Lock order is
        // wal_mu (held) → inner.write() — the same order enforced everywhere
        // else; no deadlock: the drain thread never holds inner while waiting
        // for wal_mu.  Event delivery is pure in-memory (subscriber callbacks
        // only); no WAL I/O occurs in flush_deferred_events.
        if should_sync {
            let mut db = inner.write().unwrap_or_else(|e| e.into_inner());
            db.flush_deferred_events();
            db.set_deferred_events_mode(false);
            // inner.write() released here (RAII) before wal_mu below.
        }

        // ── Step 5b: Release the cross-process lock ──────────────────────────
        //
        // The group's bytes are now fsynced, so another process may safely
        // snapshot or truncate around them.
        {
            let mut db = inner.write().unwrap_or_else(|e| e.into_inner());
            db.end_write_lock();
        }

        // Release WAL mutex after events are flushed.  Unblocks any direct
        // writer that was waiting for wal_mu; they will observe the correct
        // event order when they subsequently deliver their own events.
        drop(wal_guard);

        // ── Step 6: Signal each submitter ────────────────────────────────────
        for (sub, result) in group.into_iter().zip(results) {
            let _ = sub.done.send(result);
        }
    }
}
