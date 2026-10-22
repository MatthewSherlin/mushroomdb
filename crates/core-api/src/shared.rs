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
    BatchOp, DiskMark, DiskState, FsyncPolicy, Precondition, WriteAuthz, LOCK_POLL_INTERVAL,
    WRITE_LOCK_WAIT,
};
use crate::durable_wal::{DurableWal, WalPosition};
use crate::reader::ReaderSnapshot;
use crate::transaction::Transaction;
use crate::GraphDb;
use core_storage::sync_wal_at;
use core_storage::truncate_wal_at;
use core_storage::GraphError;
use core_storage::RealFs;
use core_storage::Result;
use std::ops::{Deref, DerefMut};
use std::path::Path;
#[cfg(test)]
use std::sync::atomic::AtomicU64;
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
    /// Apply on the compare-and-set path even when `preconds` is empty, so the
    /// submission is applied alone and its commit number can be read back.
    /// Set by [`SharedDb::submit_batch_cas_with_seq`].
    own_commit: bool,
    /// Role name for role-scoped write authz.  `Some` only for
    /// `submit_batch_authz` calls; `None` for full-authority submissions.
    /// The drain thread resolves mask + scope under `inner.write()` (§5 lock
    /// discipline: authz check and mutation share one guard lifetime).
    authz_role: Option<String>,
    done: std::sync::mpsc::SyncSender<Result<Applied>>,
}

/// What the drain thread reports back for one submission.
struct Applied {
    /// `(nodes_inserted, edges_inserted)`.
    inserted: (usize, usize),
    /// The store's `commit_seq` read under the same write guard, straight
    /// after this submission applied. Recorded for CAS submissions only —
    /// non-CAS submissions are coalesced, and one call applies several, so
    /// there is no per-submission number to read — and `None` elsewhere.
    commit_seq: Option<u64>,
}

impl Applied {
    fn uncounted(inserted: (usize, usize)) -> Self {
        Self {
            inserted,
            commit_seq: None,
        }
    }
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

/// How long a published version no reader has asked for waits for the
/// background builder: it builds the latest such version once per delay.
#[cfg(not(test))]
const LAZY_BUILD_DELAY: Duration = Duration::from_millis(2);
/// Longer under the unit tests, whose debug commits take milliseconds: what
/// they pin is commits arriving faster than the tick.
#[cfg(test)]
const LAZY_BUILD_DELAY: Duration = Duration::from_millis(100);

// ── Published snapshot ────────────────────────────────────────────────────────

/// The reader snapshot of the last state a writer released.
///
/// [`SharedDb::reader`] hands this out when the RwLock is held or awaited by
/// a writer, so capturing a reader never waits behind a `db.write()` holder —
/// a Cypher write, `/ingest`, `create_rule`, a group apply — however long it
/// holds the lock. Every write guard publishes as it is released, still under
/// the lock (see [`PublishingGuard`]), so a read issued after a write returned
/// sees that write: either it takes the read lock and reads live state, or a
/// writer holds the lock and this slot already holds the released state.
///
/// It is also what a capture reads first, without taking the RwLock at all:
/// the slot carries the [`DiskMark`] its snapshot was taken at, and while the
/// files on disk still match that mark, no write has landed since — in this
/// process every write publishes before it releases the lock, and another
/// process's write moves the WAL — so the published snapshot *is* the live
/// state. Readers then never hold a read guard a writer must wait out, and
/// never contend with it beyond one `Arc` clone under the slot's mutex.
pub(crate) struct Published {
    slot: Mutex<Option<Arc<PublishedState>>>,
    /// Looks at the store's files for the lock-free staleness check. `None`
    /// if it could not be made; every capture then takes the locked path.
    probe: Option<DiskProbe>,
    /// Set while a write guard holds the RwLock, from its acquisition until
    /// it has published; see [`Self::current`].
    writing: AtomicBool,
    /// Set by every capture; cleared by the next publish, which hands its
    /// version to the builder at once only if it was set. A hint, not a
    /// guarantee: a reader having read since the last publish makes one
    /// reading this version likely, nothing more, and the flag is read and
    /// written without ordering. Getting it wrong changes only when the
    /// builder builds, never what a capture is served.
    demand: AtomicBool,
    builder: Arc<Builder>,
}

/// The store's files, looked at by one capture at a time on behalf of all.
///
/// A capture must see every commit that returned before it began, so the
/// look it acts on must have *started* after it began — and any such look
/// will do, whichever capture made it. Captures arriving while one runs wait
/// for the next and share it: at most one runs at a time, instead of one per
/// concurrent reader. Those `stat`s are what a writer's WAL append contends
/// with in the kernel; eight readers making them back to back cost a
/// single-node commit more than everything else a capture does.
struct DiskProbe {
    /// Its own handle on the directory, so a look needs nothing behind the
    /// lock; it never takes the store's `LOCK`.
    fs: RealFs,
    looks: Mutex<Looks>,
    done: Condvar,
}

/// Looks are numbered as they start; they run one at a time, so they finish
/// in order.
#[derive(Default)]
struct Looks {
    started: u64,
    finished: u64,
    running: bool,
    /// What the last finished look saw; `None` if it failed.
    seen: Option<DiskState>,
}

impl DiskProbe {
    fn new(dir: &Path) -> Option<Self> {
        Some(Self {
            fs: RealFs::new(dir).ok()?,
            looks: Mutex::new(Looks::default()),
            done: Condvar::new(),
        })
    }

    /// The store's files as a look that started after this call did saw
    /// them, or `None` if that look failed.
    fn observe(&self) -> Option<DiskState> {
        let mut looks = self.looks.lock().unwrap_or_else(|e| e.into_inner());
        // Every look numbered above this one starts after this call did.
        let after = looks.started;
        loop {
            if looks.finished > after {
                return looks.seen;
            }
            if !looks.running {
                looks.running = true;
                looks.started += 1;
                let number = looks.started;
                drop(looks);
                let seen = DiskState::read(&self.fs).ok();
                let mut looks = self.looks.lock().unwrap_or_else(|e| e.into_inner());
                looks.running = false;
                looks.finished = number;
                looks.seen = seen;
                drop(looks);
                self.done.notify_all();
                return seen;
            }
            looks = self.done.wait(looks).unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// One published state: the snapshot, and the disk mark it is current for
/// (`None` when it must not be served without the lock — see
/// [`Published::set`]).
struct PublishedState {
    snap: ReaderSnapshot,
    mark: Option<DiskMark>,
}

impl Published {
    fn new(db: &GraphDb<RealFs>, probe: Option<DiskProbe>, builder: Arc<Builder>) -> Self {
        let state = db.try_reader().map(|snap| {
            snap.prepare();
            Arc::new(PublishedState {
                snap,
                mark: Some(db.disk_mark()),
            })
        });
        Self {
            slot: Mutex::new(state),
            probe,
            writing: AtomicBool::new(false),
            demand: AtomicBool::new(false),
            builder,
        }
    }

    /// Note that a reader captured, so the next publish is handed to the
    /// builder at once. Writes the flag only when it is clear, so readers
    /// capturing back to back do not keep taking its cache line from each
    /// other.
    fn note_reader(&self) {
        if !self.demand.load(Ordering::Relaxed) {
            self.demand.store(true, Ordering::Relaxed);
        }
    }

    /// The published state, holding the slot's mutex only to clone the `Arc`.
    fn state(&self) -> Option<Arc<PublishedState>> {
        self.slot.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The published snapshot, when the store on disk is still exactly the
    /// state it was published at. Takes no RwLock.
    ///
    /// While a write guard holds the RwLock, the published snapshot is served
    /// without looking at the disk at all — exactly what a capture that found
    /// the RwLock held has always been served, and for the same reason: that
    /// writer refreshes as it opens its write scope and publishes as it
    /// releases. Looking then would only find its own WAL append in flight,
    /// and would contend with that append in the kernel.
    fn current(&self) -> Option<ReaderSnapshot> {
        let probe = self.probe.as_ref()?;
        let state = self.state()?;
        if self.writing.load(Ordering::Acquire) {
            return Some(state.snap.share());
        }
        let mark = state.mark?;
        let disk = probe.observe()?;
        (!mark.is_stale_at(&disk)).then(|| state.snap.share())
    }

    /// Publish `db`'s state, and hand it to the background builder.
    ///
    /// Runs under the write lock, and costs only the capture: the snapshot's
    /// delta-tail layer and query index are built by [`Builder`] off the lock,
    /// into the cells every snapshot of the version shares. A reader arriving
    /// before the build finishes shares those cells: `get_or_init` has it wait
    /// for the build in flight or do it itself, never twice.
    ///
    /// The builder is woken for this version only when a reader has captured
    /// since the last publish (see [`Self::note_reader`]: a hint that one is
    /// waiting). Otherwise the
    /// version waits for the builder's own schedule ([`LAZY_BUILD_DELAY`]):
    /// back-to-back commits nobody reads then cost no wake-up and no build
    /// each, and the builder builds the latest of them once per delay. The
    /// slot itself is always updated here, so what a capture is served —
    /// read-your-write, and never a torn state — does not depend on it.
    ///
    /// The new snapshot replaces the old one before the build, so the old
    /// version's cells are no longer held here and the build extends them in
    /// place instead of copying them.
    ///
    /// A handle with no state to publish (between a reload's reset and its
    /// fold) keeps the old snapshot for captures a writer blocks, as before,
    /// but drops its mark, so no capture takes it for the live state.
    fn set(&self, db: &GraphDb<RealFs>) {
        let Some(snap) = db.try_reader() else {
            let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(old) = slot.as_ref().filter(|s| s.mark.is_some()) {
                *slot = Some(Arc::new(PublishedState {
                    snap: old.snap.share(),
                    mark: None,
                }));
            }
            return;
        };
        let state = Arc::new(PublishedState {
            snap,
            mark: Some(db.disk_mark()),
        });
        let old = self
            .slot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(Arc::clone(&state));
        let eager = self.demand.load(Ordering::Relaxed);
        if eager {
            self.demand.store(false, Ordering::Relaxed);
        }
        self.builder.submit(state, old, eager);
    }

    fn get(&self) -> Option<ReaderSnapshot> {
        self.state().map(|state| state.snap.share())
    }
}

/// The one background thread that builds published snapshots' layers and
/// indexes, off the write lock.
///
/// It holds at most one pending snapshot: a newer publish replaces an older
/// one not yet started, so builds never queue. A superseded version is built,
/// if anyone reads it, by its first reader, as before.
///
/// A publish made while a reader seems to be waiting (see
/// [`Published::note_reader`]) wakes it at once. Any other waits for the
/// thread's next tick: one [`LAZY_BUILD_DELAY`] after each build it builds
/// the latest queued version, and when that tick finds nothing it sleeps
/// until a publish wakes it. So a writer committing faster than the tick
/// with no reader wakes it about once, not once per commit; a slower one
/// costs it the wake-up that builds each commit and one empty tick.
///
/// It also drops the states a publish replaced. Dropping a snapshot releases
/// one reference on each of its `Arc`s — the fold, every delta of the tail —
/// whose counts readers' captures are updating at the same moment, so the
/// writer leaves that to this thread. They are dropped before the next build,
/// so a superseded state never holds a cell the build would rather extend in
/// place than copy.
pub(crate) struct Builder {
    pending: Mutex<BuilderQueue>,
    wake: std::sync::Condvar,
    shutdown: std::sync::atomic::AtomicBool,
    /// Publishes that woke the builder thread.
    #[cfg(test)]
    wakes: AtomicU64,
    /// Times the builder thread returned from a wait, timed or not.
    #[cfg(test)]
    thread_wakes: AtomicU64,
    /// Publishes handed to the builder at once because a reader had read
    /// since the publish before.
    #[cfg(test)]
    eager: AtomicU64,
}

#[derive(Default)]
struct BuilderQueue {
    next: Option<Arc<PublishedState>>,
    retired: Vec<Arc<PublishedState>>,
    /// Build now, without waiting out the delay.
    urgent: bool,
    /// The thread is asleep with no timeout: a publish must wake it.
    sleeping: bool,
}

impl Builder {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            pending: Mutex::new(BuilderQueue::default()),
            wake: std::sync::Condvar::new(),
            shutdown: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            wakes: AtomicU64::new(0),
            #[cfg(test)]
            thread_wakes: AtomicU64::new(0),
            #[cfg(test)]
            eager: AtomicU64::new(0),
        })
    }

    /// Queue `state` for its build, and `old`, the state it replaced, for
    /// dropping. `eager`: build it at once rather than on the thread's own
    /// schedule.
    ///
    /// When the publish is not `eager` — by the hint, no reader asked for
    /// `old` — and the fold is unchanged, `old` (and a queued state `state`
    /// supersedes) is dropped here instead. Should a reader hold it after
    /// all, dropping here releases only this reference; the reader keeps the
    /// state. Its fold and deltas live on in `state`, so that drop is usually
    /// a few reference counts. Queued until the next tick, it
    /// would hold the built layer the next build moves, and a reader building
    /// first would copy that layer instead.
    fn submit(&self, state: Arc<PublishedState>, old: Option<Arc<PublishedState>>, eager: bool) {
        // Dropped here only when it shares `state`'s fold: a state of an
        // older fold may hold the last reference to that fold.
        let unread_same_fold = |s: &Option<Arc<PublishedState>>| {
            !eager
                && s.as_ref()
                    .is_some_and(|s| Arc::ptr_eq(&s.snap.frozen, &state.snap.frozen))
        };
        let old_here = unread_same_fold(&old);
        let mut queue = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let superseded = queue.next.take();
        let superseded_here = old_here && unread_same_fold(&superseded);
        queue.next = Some(state);
        let mut here = (None, None);
        if superseded_here {
            here.0 = superseded;
        } else {
            queue.retired.extend(superseded);
        }
        if old_here {
            here.1 = old;
        } else {
            queue.retired.extend(old);
        }
        // A sleeping thread would never see this publish; one waiting out
        // its delay will, and one already told to hurry needs no second
        // wake.
        let wake = (eager || queue.sleeping) && !queue.urgent;
        if wake {
            queue.urgent = true;
            queue.sleeping = false;
        }
        drop(queue);
        #[cfg(test)]
        {
            self.wakes.fetch_add(u64::from(wake), Ordering::Relaxed);
            self.eager.fetch_add(u64::from(eager), Ordering::Relaxed);
        }
        if wake {
            self.wake.notify_one();
        }
        drop(here);
    }

    fn run(self: Arc<Self>) {
        let mut queue = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            // Wait for a reason to build: a wake-up, or the tick after the
            // last build finding something to build. A tick that finds
            // nothing sleeps until a publish wakes it.
            let tick_end = Instant::now() + LAZY_BUILD_DELAY;
            loop {
                if self.shutdown.load(Ordering::Acquire) {
                    // No reader is left to want a build; drop what is queued
                    // off the lock.
                    let queued = (queue.next.take(), std::mem::take(&mut queue.retired));
                    drop(queue);
                    drop(queued);
                    return;
                }
                if queue.urgent {
                    break;
                }
                let work = queue.next.is_some() || !queue.retired.is_empty();
                let now = Instant::now();
                if now < tick_end {
                    queue = self
                        .wake
                        .wait_timeout(queue, tick_end - now)
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                    #[cfg(test)]
                    self.thread_wakes.fetch_add(1, Ordering::Relaxed);
                } else if work {
                    break;
                } else {
                    queue.sleeping = true;
                    queue = self.wake.wait(queue).unwrap_or_else(|e| e.into_inner());
                    #[cfg(test)]
                    self.thread_wakes.fetch_add(1, Ordering::Relaxed);
                }
            }
            queue.urgent = false;
            queue.sleeping = false;
            let next = queue.next.take();
            let retired = std::mem::take(&mut queue.retired);
            drop(queue);
            drop(retired);
            if let Some(state) = next {
                if state.snap.prepare() {
                    state.snap.mark_built_in_background();
                }
            }
            queue = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// Stops the builder thread and joins it when the last `SharedDb` drops.
struct BuilderHandle {
    builder: Arc<Builder>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Drop for BuilderHandle {
    fn drop(&mut self) {
        self.builder
            .shutdown
            .store(true, std::sync::atomic::Ordering::Release);
        // Take the lock so the store cannot slip between the thread's check
        // and its wait.
        drop(
            self.builder
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        );
        self.builder.wake.notify_all();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// The RwLock write guard, publishing the state it leaves behind to
/// [`Published`] as it is released.
pub(crate) struct PublishingGuard<'a> {
    guard: std::sync::RwLockWriteGuard<'a, GraphDb<RealFs>>,
    published: &'a Published,
}

impl Drop for PublishingGuard<'_> {
    fn drop(&mut self) {
        // A panic mid-write may have left the handle half-updated; the slot
        // keeps the last state a writer finished with.
        if !std::thread::panicking() {
            self.published.set(&self.guard);
        }
        self.published.writing.store(false, Ordering::Release);
    }
}

impl Deref for PublishingGuard<'_> {
    type Target = GraphDb<RealFs>;
    fn deref(&self) -> &GraphDb<RealFs> {
        &self.guard
    }
}

impl DerefMut for PublishingGuard<'_> {
    fn deref_mut(&mut self) -> &mut GraphDb<RealFs> {
        &mut self.guard
    }
}

/// Take `inner`'s write lock; the state is published when the guard drops.
fn write_publishing<'a>(
    inner: &'a RwLock<GraphDb<RealFs>>,
    published: &'a Published,
) -> PublishingGuard<'a> {
    let guard = inner.write().unwrap_or_else(|e| e.into_inner());
    published.writing.store(true, Ordering::Release);
    PublishingGuard { guard, published }
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
    inner: PublishingGuard<'a>,
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
    /// The last released state, for captures a writer would otherwise block.
    published: Arc<Published>,
    /// Builds published snapshots' layers and indexes off the write lock.
    _builder: Arc<BuilderHandle>,
    queue: Arc<WriteQueue>,
    /// Keeps the drain thread alive; signals + joins on last drop.
    _drain: Arc<DrainHandle>,
    /// WAL I/O mutex.  Serialises all WAL appends, fsyncs, and truncations
    /// across the drain thread and direct writers.  See module-level doc for
    /// the required acquisition order.
    wal_mu: Arc<Mutex<()>>,
    /// The handle's durable-WAL cell, read without any of the locks above.
    durable_wal: DurableWal,
    /// Where this handle's WAL lives, and how it is fsynced: for vouching
    /// for WAL bytes it applied but did not write (see
    /// [`durable_wal_position`](SharedDb::durable_wal_position)).
    vouch: Arc<(std::path::PathBuf, SyncWalFn)>,
    /// Captures that went past the published snapshot to the RwLock.
    #[cfg(test)]
    locked_captures: Arc<std::sync::atomic::AtomicU64>,
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
        let durable_wal = db.durable_wal();
        let vouch = Arc::new((dir.clone(), Arc::clone(&sync_fn)));
        let builder = Builder::new();
        let builder_thread = {
            let builder = Arc::clone(&builder);
            thread::Builder::new()
                .name("reader-layer-builder".into())
                .spawn(move || builder.run())
                .expect("failed to spawn reader layer builder thread")
        };
        let probe = DiskProbe::new(&dir);
        let published = Arc::new(Published::new(&db, probe, Arc::clone(&builder)));
        let inner = Arc::new(RwLock::new(db));
        let queue = WriteQueue::new();
        let wal_mu = Arc::new(Mutex::new(()));
        let dir_arc = Arc::new(dir);

        let drain_inner = Arc::clone(&inner);
        let drain_published = Arc::clone(&published);
        let drain_queue = Arc::clone(&queue);
        let drain_dir = Arc::clone(&dir_arc);
        let drain_wal_mu = Arc::clone(&wal_mu);
        let drain_sync_fn = Arc::clone(&sync_fn);
        let drain_durable = durable_wal.clone();

        let handle = thread::Builder::new()
            .name("groupcommit-drain".into())
            .spawn(move || {
                drain_loop(
                    drain_inner,
                    drain_published,
                    drain_queue,
                    drain_dir,
                    drain_wal_mu,
                    drain_sync_fn,
                    drain_durable,
                )
            })
            .expect("failed to spawn group-commit drain thread");

        SharedDb {
            inner,
            published,
            _builder: Arc::new(BuilderHandle {
                builder,
                handle: Some(builder_thread),
            }),
            queue: Arc::clone(&queue),
            _drain: Arc::new(DrainHandle {
                queue,
                handle: Some(handle),
            }),
            wal_mu,
            durable_wal,
            vouch,
            #[cfg(test)]
            locked_captures: Arc::default(),
        }
    }

    /// How much of the live WAL is known to be on stable storage: only bytes
    /// an fsync has covered, ending on a frame boundary, with a generation
    /// that changes whenever `wal.bin` is replaced. See [`WalPosition`].
    ///
    /// Takes none of the handle's locks — not the RwLock, not the WAL mutex —
    /// so polling it never delays a reader or a writer. This is what a backup
    /// reads instead of the file's length, which counts bytes still in the
    /// page cache.
    ///
    /// WAL bytes this handle applied without writing them itself — what its
    /// open replayed, or what a refresh absorbed from another process — are
    /// fsynced once here, on the caller's thread and holding none of the
    /// handle's locks, before they count as durable. An open that nobody asks
    /// about this pays no fsync.
    pub fn durable_wal_position(&self) -> WalPosition {
        if let Some((generation, len, frames)) = self.durable_wal.unvouched() {
            // A failed fsync leaves the position where it is; the next call
            // tries again, and this handle's next write's fsync covers it.
            let (dir, sync_fn) = &*self.vouch;
            if sync_fn(dir).is_ok() {
                self.durable_wal.vouch(generation, len, frames);
            }
        }
        self.durable_wal.get()
    }

    /// Catch up with another process's commits or snapshot, if there are any.
    ///
    /// The same metadata-only check every read makes. For a reader that does
    /// not otherwise read — the backup worker, when the disk shows a snapshot
    /// this handle has not seen.
    pub fn refresh_if_stale_now(&self) {
        self.refresh_if_stale();
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
            let mut db = write_publishing(&self.inner, &self.published);
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
        let (acquired, failure) = match self.poll_cross_process_lock(WRITE_LOCK_WAIT) {
            Ok(acquired) => (acquired, None),
            Err(e) => (false, Some(e)),
        };
        // The outcome is latched on the db itself, so it surfaces on the first
        // mutation: a denied lock as `Busy` (or `Unwritable` when the
        // filesystem refused permission rather than a peer holding the lock),
        // and a refresh failure under a taken lock as the degraded error
        // (`refresh` marks the handle).
        let (mut guard, _) = self.enter_write_scope(_wal, acquired);
        if let Some(e) = &failure {
            guard.note_lock_failure(e);
        }
        guard
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
        let mut inner = write_publishing(&self.inner, &self.published);
        let entered = inner.enter_write_scope(acquired);
        (WriteGuard { inner, _wal: wal }, entered)
    }

    /// Capture a lock-free [`ReaderSnapshot`] of the current db state.
    ///
    /// Never waits behind a writer in this process. Usually it takes no lock
    /// at all (below); when it does try the read lock and finds it free, it
    /// holds it only long enough to clone a handful of `Arc` handles.
    /// When a writer holds or awaits it, the snapshot is the state the last
    /// writer released — which includes every write this process has
    /// returned, so a read issued after such a write still sees it.
    ///
    /// Another process's commits are followed as [`Self::read`] follows them:
    /// when the handle is stale, this applies the peer's WAL tail before
    /// capturing, under the write lock, waiting for the read guards already
    /// held (no new one is granted while it waits). A peer commit that has
    /// returned is therefore in the next capture, whoever holds a read guard.
    /// The wait happens only on a capture that finds the handle stale — one
    /// per peer commit, not per read — and is bounded by the longest read
    /// guard then held, as it was before 0.10. A stale handle found while a
    /// writer holds the lock needs no wait: that writer refreshes when it
    /// opens its write scope and publishes the result as it releases.
    ///
    /// Subsequent reads on the returned snapshot are lock-free.
    ///
    /// # No RwLock in the common case
    ///
    /// The capture itself first reads the published snapshot with no RwLock:
    /// when the files on disk still match the mark it was published at, it is
    /// the live state (see [`Published`]), and the capture costs one `Arc`
    /// clone under the slot's mutex plus the same metadata-only check a read
    /// always made. The RwLock is tried only when that check sees a commit —
    /// another process's, or one in flight here — so concurrent readers do
    /// not hold read guards a writer has to wait for.
    pub fn reader(&self) -> ReaderSnapshot {
        use std::sync::TryLockError;
        self.published.note_reader();
        if let Some(snap) = self.published.current() {
            return snap;
        }
        #[cfg(test)]
        self.locked_captures
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        match self.inner.try_read() {
            Ok(db) => {
                if !db.is_stale().unwrap_or(false) {
                    return db.reader();
                }
                drop(db);
                // Another process committed. Serving the published state here
                // would hide its commit for as long as any read guard is held,
                // so apply it, waiting for those guards, then capture.
                self.refresh_if_stale();
                self.read().reader()
            }
            Err(TryLockError::Poisoned(p)) => p.into_inner().reader(),
            Err(TryLockError::WouldBlock) => match self.published.get() {
                Some(snap) => snap,
                None => self.read().reader(),
            },
        }
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
            own_commit: false,
            authz_role: None,
            done: tx,
        });
        receive(rx).map(|a| a.inserted)
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
        self.submit_batch_cas_with_seq(preconds, ops)
            .map(|(inserted, _)| inserted)
    }

    /// [`submit_batch_cas`](Self::submit_batch_cas), also returning the commit
    /// number this batch landed at.
    ///
    /// The number is read by the drain thread under the same write guard that
    /// applied the batch, so it is this batch's and no one else's — reading
    /// [`reader`](Self::reader)`().version` afterwards would race every other
    /// writer. An all-noop batch writes no frame and returns the number of the
    /// commit it found in place.
    pub(crate) fn submit_batch_cas_with_seq(
        &self,
        preconds: Vec<Precondition>,
        ops: Vec<BatchOp>,
    ) -> Result<((usize, usize), u64)> {
        if let Some(msg) = self.queue.degraded_message() {
            return Err(GraphError::Io(std::io::Error::other(msg)));
        }
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.queue.enqueue(Submission {
            ops,
            preconds,
            own_commit: true,
            authz_role: None,
            done: tx,
        });
        let applied = receive(rx)?;
        let seq = applied.commit_seq.ok_or_else(|| {
            GraphError::Io(std::io::Error::other(
                "group-commit drain thread reported no commit number for a CAS submission",
            ))
        })?;
        Ok((applied.inserted, seq))
    }

    /// Open an interactive transaction over this store.
    ///
    /// Takes no lock beyond the instant it needs to freeze a
    /// [`ReaderSnapshot`]; see [`Transaction`] for the isolation model.
    pub fn begin_transaction(&self) -> Transaction {
        let (base_snap, base_store) = {
            let db = self.read();
            (db.reader(), db.store_id())
        };
        Transaction::new(self.clone(), base_snap, base_store)
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
            own_commit: false,
            authz_role: Some(role),
            done: tx,
        });
        receive(rx).map(|a| a.inserted)
    }
}

/// Wait for the drain thread's answer to one submission.
fn receive(rx: std::sync::mpsc::Receiver<Result<Applied>>) -> Result<Applied> {
    rx.recv().unwrap_or_else(|_| {
        Err(GraphError::Io(std::io::Error::other(
            "group-commit drain thread terminated unexpectedly",
        )))
    })
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

/// A submission's `(preconds, own_commit, ops, authz_role)`, taken out of the
/// group so the group itself can still be signalled afterwards.
type Extracted = (Vec<Precondition>, bool, Vec<BatchOp>, Option<String>);

/// Results of a coalesced apply, which carry no per-submission commit number.
fn uncounted(results: Vec<Result<(usize, usize)>>) -> impl Iterator<Item = Result<Applied>> {
    results.into_iter().map(|r| r.map(Applied::uncounted))
}

fn drain_loop(
    inner: Arc<RwLock<GraphDb<RealFs>>>,
    published: Arc<Published>,
    queue: Arc<WriteQueue>,
    dir: Arc<std::path::PathBuf>,
    wal_mu: Arc<Mutex<()>>,
    sync_fn: SyncWalFn,
    durable_wal: DurableWal,
) {
    loop {
        // Wait for work (or shutdown with empty queue).
        let mut group = queue.wait_and_drain();
        if group.is_empty() {
            return; // shutdown + nothing pending
        }

        // Extract ops, preconditions, and authz_role together; keep alignment
        // with group index.
        let submissions: Vec<Extracted> = group
            .iter_mut()
            .map(|s| {
                (
                    std::mem::take(&mut s.preconds),
                    s.own_commit,
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
            let mut db = write_publishing(&inner, &published);
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
                    let mut db = write_publishing(&inner, &published);
                    db.end_write_lock();
                }
                drop(wal_guard);
                for sub in group {
                    let _ = sub.done.send(Err(GraphError::Busy { holder: None }));
                }
                continue;
            }
            Err(GraphError::Unwritable {
                path,
                detail,
                read_only_filesystem,
            }) => {
                // The filesystem refused permission to take the lock at all:
                // a deployment fault, not a peer. Say so to every submitter.
                // The handle is not degraded - fixing the directory's
                // ownership lets the next group proceed.
                {
                    let mut db = inner.write().unwrap_or_else(|e| e.into_inner());
                    db.end_write_lock();
                }
                drop(wal_guard);
                for sub in group {
                    let _ = sub.done.send(Err(GraphError::Unwritable {
                        path: path.clone(),
                        detail: detail.clone(),
                        read_only_filesystem,
                    }));
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
                    let mut db = write_publishing(&inner, &published);
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
        // Where the group ends: the fsync below covers exactly this much. Set
        // under the write guard, with wal_mu held, so nothing can append
        // between there and the fsync.
        let post_group: (u64, u64);
        let (results, should_sync): (Vec<Result<Applied>>, bool) = {
            let mut db = write_publishing(&inner, &published);
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
            let mut r: Vec<Result<Applied>> = Vec::with_capacity(submissions.len());
            let mut pending_non_cas: Vec<Vec<BatchOp>> = Vec::new();

            for (preconds, own_commit, ops, authz_role) in submissions {
                if let Some(role) = authz_role {
                    // Authz submission: process individually (breaks coalescing).
                    // Flush accumulated non-CAS batch first so authz evaluation
                    // sees their writes already applied.
                    if !pending_non_cas.is_empty() {
                        let batch = std::mem::take(&mut pending_non_cas);
                        r.extend(uncounted(db.commit_group_nosync(batch)));
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
                    r.push(result.map(Applied::uncounted));
                } else if preconds.is_empty() && !own_commit {
                    // Non-CAS: accumulate for a batched commit_group_nosync call.
                    pending_non_cas.push(ops);
                } else {
                    // CAS: flush accumulated non-CAS batch first so that precond
                    // evaluation sees their writes already applied.
                    if !pending_non_cas.is_empty() {
                        let batch = std::mem::take(&mut pending_non_cas);
                        r.extend(uncounted(db.commit_group_nosync(batch)));
                    }
                    let result = match db.check_preconditions(&preconds) {
                        Ok(()) => db
                            .commit_group_nosync(vec![ops])
                            .into_iter()
                            .next()
                            .unwrap_or(Ok((0, 0))),
                        Err(e) => Err(e),
                    };
                    // Read under the guard that applied it: this batch's number.
                    r.push(result.map(|inserted| Applied {
                        inserted,
                        commit_seq: Some(db.commit_seq()),
                    }));
                }
            }
            // Flush any remaining non-CAS submissions.
            if !pending_non_cas.is_empty() {
                r.extend(uncounted(db.commit_group_nosync(pending_non_cas)));
            }
            post_group = (db.wal_consumed(), db.wal_frames_written());
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
                let mut db = write_publishing(&inner, &published);
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

        // The group's frames are on stable storage now.
        if should_sync && results.iter().any(|r| r.is_ok()) {
            durable_wal.synced(post_group.0, post_group.1);
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
            let mut db = write_publishing(&inner, &published);
            db.flush_deferred_events();
            db.set_deferred_events_mode(false);
            // inner.write() released here (RAII) before wal_mu below.
        }

        // ── Step 5b: Release the cross-process lock ──────────────────────────
        //
        // The group's bytes are now fsynced, so another process may safely
        // snapshot or truncate around them.
        {
            let mut db = write_publishing(&inner, &published);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Value;
    use std::sync::atomic::Ordering;

    fn tmp(name: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "mdb-shared-unit-{name}-{}-{nanos}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn value_of(snap: &ReaderSnapshot, key: &str) -> Option<Value> {
        snap.node_info(key)?.props.get("v").cloned()
    }

    fn locked_captures(db: &SharedDb) -> u64 {
        db.locked_captures.load(Ordering::Relaxed)
    }

    /// A capture made after a write returned reads that write from the
    /// published snapshot, without the RwLock, through both write paths and
    /// across folds.
    #[test]
    fn a_capture_after_a_returned_write_reads_it_without_the_lock() {
        let dir = tmp("no-lock");
        let db = SharedDb::open(&dir).unwrap();
        db.write().insert_node("N", "a", vec![]).unwrap();
        let commits = 2 * crate::FOLD_EVERY_K as i64 + 3;
        for i in 0..commits {
            if i % 2 == 0 {
                db.write().set_prop("a", "v", Value::Int(i)).unwrap();
            } else {
                db.submit_batch(vec![BatchOp::SetProp {
                    key: "a".into(),
                    field: "v".into(),
                    value: Value::Int(i),
                }])
                .unwrap();
            }
            for _ in 0..3 {
                assert_eq!(value_of(&db.reader(), "a"), Some(Value::Int(i)));
            }
        }
        assert_eq!(
            locked_captures(&db),
            0,
            "a capture with no commit in flight took the RwLock"
        );
        assert_eq!(
            db.reader().version,
            db.read().reader().version,
            "the published snapshot is not the live state"
        );
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A commit the published snapshot does not include — here another
    /// handle's, as another process's would be — is seen by the next
    /// capture, which takes the locked path once and publishes the result.
    #[test]
    fn a_commit_outside_the_published_snapshot_is_read_by_the_next_capture() {
        let dir = tmp("peer");
        let db = SharedDb::open(&dir).unwrap();
        db.write().insert_node("N", "a", vec![]).unwrap();
        assert_eq!(value_of(&db.reader(), "b"), None);
        assert_eq!(locked_captures(&db), 0);
        {
            let mut peer = GraphDb::open(&dir).unwrap();
            peer.insert_node("N", "b", vec![("v".into(), Value::Int(7))])
                .unwrap();
        }
        assert_eq!(value_of(&db.reader(), "b"), Some(Value::Int(7)));
        assert_eq!(locked_captures(&db), 1);
        // Refreshed and republished: the next capture is lock-free again.
        assert_eq!(value_of(&db.reader(), "b"), Some(Value::Int(7)));
        assert_eq!(locked_captures(&db), 1);
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn builder_counts(db: &SharedDb) -> (u64, u64) {
        let b = &db.published.builder;
        (
            b.wakes.load(Ordering::Relaxed),
            b.eager.load(Ordering::Relaxed),
        )
    }

    /// TX-5c: a commit no reader is waiting for neither wakes the background
    /// builder nor hands it the new version at once. The builder still builds
    /// the latest version on its own schedule, and a commit after a read
    /// hands its version over at once, as before.
    #[test]
    fn a_commit_with_no_reader_since_the_last_publish_does_not_wake_the_builder() {
        let dir = tmp("lazy");
        let db = SharedDb::open(&dir).unwrap();
        db.write().insert_node("N", "a", vec![]).unwrap();
        // Let the builder finish and fall asleep: the first commit wakes it.
        std::thread::sleep(LAZY_BUILD_DELAY * 4);
        let (wakes0, eager0) = builder_counts(&db);
        let commits = 200;
        for i in 0..commits {
            db.write().set_prop("a", "v", Value::Int(i)).unwrap();
        }
        let (wakes1, eager1) = builder_counts(&db);
        assert_eq!(
            eager1 - eager0,
            0,
            "a commit no reader asked for was handed over at once"
        );
        assert!(
            wakes1 - wakes0 < commits as u64 / 4,
            "{} of {commits} commits with no reader woke the builder",
            wakes1 - wakes0
        );
        // Still built in the background: the live version, captured under a
        // read guard (which keeps writers out, so it stays the latest).
        {
            let g = db.read();
            let snap = g.reader();
            assert!(!snap.deltas.is_empty());
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !snap.built_in_background() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the builder never built the version no reader asked for"
                );
                std::thread::sleep(std::time::Duration::from_micros(200));
            }
        }
        // A read, then a commit: that commit's version is handed over at once.
        assert_eq!(value_of(&db.reader(), "a"), Some(Value::Int(commits - 1)));
        db.write().set_prop("a", "v", Value::Int(-1)).unwrap();
        let (_, eager2) = builder_counts(&db);
        assert_eq!(
            eager2 - eager1,
            1,
            "a commit after a read was not handed over at once"
        );
        assert_eq!(value_of(&db.reader(), "a"), Some(Value::Int(-1)));
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// TX-5c: a version replaced before any reader asked for it is dropped as
    /// it is replaced, not kept until the builder's next tick. Kept, it holds
    /// the built layer the next build would move, so the first reader after a
    /// quiet spell — after a bulk load, say — copied a bulk-sized layer while
    /// every other reader waited on it.
    #[test]
    fn a_version_replaced_before_anyone_read_it_is_not_kept_alive() {
        let dir = tmp("retire");
        let db = SharedDb::open(&dir).unwrap();
        db.write().insert_node("N", "a", vec![]).unwrap();
        db.write().set_prop("a", "v", Value::Int(1)).unwrap();
        let replaced = std::sync::Arc::downgrade(&db.published.state().unwrap());
        {
            let g = db.read();
            let snap = g.reader();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !snap.built_in_background() {
                assert!(std::time::Instant::now() < deadline, "never built");
                std::thread::sleep(std::time::Duration::from_micros(200));
            }
        }
        // Wait until only the slot holds the version: the builder has let
        // go of what it built.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while replaced.strong_count() > 1 {
            assert!(
                std::time::Instant::now() < deadline,
                "the builder kept the version it built"
            );
            std::thread::sleep(std::time::Duration::from_micros(200));
        }
        db.write().set_prop("a", "v", Value::Int(2)).unwrap();
        assert!(
            replaced.upgrade().is_none(),
            "the replaced version is still held after the commit that replaced it"
        );
        assert_eq!(value_of(&db.reader(), "a"), Some(Value::Int(2)));
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// TX-5c fix round 1: with no reader, a writer slower than the tick does
    /// not keep the builder ticking. Each commit costs it the wake-up that
    /// builds it and one empty tick, then it sleeps; between commits it does
    /// not wake at all.
    #[test]
    fn the_builder_sleeps_between_commits_no_reader_waits_for() {
        let dir = tmp("idle");
        let db = SharedDb::open(&dir).unwrap();
        db.write().insert_node("N", "a", vec![]).unwrap();
        let thread_wakes = || db.published.builder.thread_wakes.load(Ordering::Relaxed);
        let gap = LAZY_BUILD_DELAY * 4;
        std::thread::sleep(gap);
        let before = thread_wakes();
        let commits = 5;
        for i in 0..commits {
            db.write().set_prop("a", "v", Value::Int(i)).unwrap();
            std::thread::sleep(gap);
        }
        let woke = thread_wakes() - before;
        assert!(
            woke <= 2 * commits as u64 + 1,
            "the builder woke {woke} times for {commits} commits {gap:?} apart with no reader"
        );
        // Idle: no commit, no wake.
        let idle = thread_wakes();
        std::thread::sleep(gap);
        assert_eq!(thread_wakes(), idle, "the builder woke with nothing to do");
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
