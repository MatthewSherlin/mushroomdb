//! The backup task: ships WAL segments, snapshots, archives and side files, and
//! writes the manifests that name consistent restore points.
//!
//! # What it reads, and what it never touches
//!
//! It reads the store's files through ordinary OS reads on Tokio's blocking
//! pool, and one value from the engine: [`WalSource::durable_wal_position`],
//! which says how much of `wal.bin` is fsynced, in which generation, and which
//! `snapshot.bin` and `wal.bin` files the engine's state is on. Reading it
//! takes no engine lock, so it cannot slow a write. The one exception: when
//! the files on disk show a snapshot or commits the engine has not seen
//! (another process wrote them), the worker asks the handle to catch up — the
//! same metadata check and refresh every read makes. It writes one file into
//! the store directory, once: `backup.lineage`, on the first backup of a store
//! (see *Lineage*).
//!
//! # Consistency
//!
//! A restore point is a snapshot plus the WAL that starts beside it, and which
//! ones those are comes from the files on disk:
//!
//! - Nothing is read while `snapshot.intent` holds anything — a snapshot, in
//!   this process or another, is between writing `snapshot.bin` and replacing
//!   `wal.bin`. The step is deferred, not failed.
//! - Every read is bracketed by two looks at the directory, and kept only if
//!   neither the snapshot, the WAL file, nor the intent changed and the WAL
//!   did not shrink between them ([`DiskState::unchanged_until`]).
//! - A restore point is built only when the engine's position names the
//!   snapshot and WAL file on disk; a new generation starts whenever either
//!   changes. A snapshot that kept its WAL carries a mark of the prefix it
//!   holds, and the open of a restored store skips that prefix by itself, so
//!   shipping the whole WAL from offset 0 beside it is always correct.
//! - A manifest's `commit_seq` is computed from the objects it lists — the
//!   `wal.floor` it carries plus the frames of each archive and segment — so
//!   it is what the restored store reports, by construction.
//!
//! # Objects and ordering
//!
//! Every object a manifest names is immutable (see [`crate::manifest`]); a
//! manifest is written only after every object it names exists, and `LATEST`
//! only after its manifest. Retention deletes only what no retained manifest
//! names. So at any point — a crash between any two requests included — every
//! manifest in the bucket verifies, and `LATEST` names one of them.
//!
//! # Lineage
//!
//! A prefix holds the backups of one store lineage. The first backup of a
//! store gives it a random lineage id (`backup.lineage`, a side file, so a
//! store restored from the prefix carries it on). Every manifest and every
//! object entry names its lineage, and resume checks it before using anything
//! in the prefix. The worker refuses the prefix — uploads nothing, deletes
//! nothing, reports unhealthy with the reason — when its newest restore point
//! is another lineage's, when this store has no lineage but the prefix has
//! restore points, or when this store holds fewer commits than that restore
//! point (it was restored from an older backup, or rewound). Archives found
//! in the prefix are reused only when their number, length and CRC all match
//! the local file. Retention reads only this lineage's manifests and deletes
//! only objects of this lineage's generations, never the manifest it just
//! wrote or the one `LATEST` names.
//!
//! # Writes the bucket can refuse
//!
//! Every immutable object (segment, side file) is written with
//! `If-None-Match: *`; an object already there is accepted only with the same
//! content. Snapshots and archives (multipart) are checked absent first.
//! Manifests and `LATEST` are written with `If-Match` against the version
//! this worker last wrote or read. A refusal means another writer is active:
//! the worker drops its lease and starts over through the lease. A client
//! without conditional PUT (`MUSHROOMDB_BACKUP_LEASE_CONDITIONAL=off`) falls
//! back to plain writes.
//!
//! # The bucket lease
//!
//! A spawned worker uploads nothing until it holds the prefix's
//! [`BucketLease`]. While another instance holds a fresh one the worker
//! uploads nothing, reports itself unhealthy naming the holder, and tries
//! again every third of a TTL — so a successor waiting on a holder that died,
//! or on one shutting down in a rolling deploy, takes over within a TTL. A
//! separate task renews the lease every third of a TTL; every request that
//! changes the bucket runs only while [`BucketLease::may_write`] holds and is
//! cancelled when it stops holding. A lease found lost (taken over) is
//! dropped, and a lease taken again later starts from the bucket's state
//! afresh: its previous run's view of the bucket is stale. On shutdown, after
//! the flush, the lease is deleted. The lease is advisory; see
//! [`crate::lease`] for what it does and does not guarantee.
//!
//! [`BackupWorker::run_once`] and [`BackupWorker::step`] take no lease: they
//! are for tests and callers that hold one themselves.
//!
//! # Health
//!
//! A supervisor beside the worker re-evaluates health every `wal_interval_ms`,
//! whatever the worker is doing, and reports a worker that exits or panics as
//! unhealthy with the reason. A dead backup never looks healthy, and neither
//! does one kept out of the bucket by its lease.

use crate::disk::{self, DiskState, Ident, LINEAGE_FILE};
use crate::lease::{new_instance_id, BucketLease, Exclusion, LeaseError, LeaseWatch};
use crate::manifest::{
    archive_key, crc32_hex, format_crc32, latest_key, manifest_key, object_key, parse_archive_name,
    parse_segment_key, parse_versioned_generation, segment_key, side_key, snapshot_key, FileEntry,
    Manifest, SegmentEntry, MANIFEST_VERSION,
};
use crate::segment::{complete_frames, read_new_segment, FrameCounter};
use crate::store::BackupConfig;
use crate::BackupError;
use mushroomdb::{SharedDb, WalPosition};
use object_store::buffered::BufWriter;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, PutMode, PutPayload, PutResult, UpdateVersion};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// The store's small files that travel with every restore point.
///
/// `snapshot.intent` is deliberately absent: it describes one machine's
/// half-finished snapshot and must never be restored onto another (W4 §4).
pub const SIDE_FILES: [&str; 7] = [
    LINEAGE_FILE,
    "commit_times.bin",
    "roles.json",
    "keys.json",
    "wal.genesis",
    // A V11 store's genesis marker, a name 0.9.x does not read (TY-12).
    "wal.genesis.v11",
    "wal.floor",
];

/// The side files that describe the archive chain. They change only at a
/// snapshot, and they belong to the generation they were read with: captured
/// once, when the generation starts, never refreshed under it.
const CHAIN_FILES: [&str; 3] = ["wal.genesis", "wal.genesis.v11", "wal.floor"];

/// Consecutive failed steps after which the worker reports itself unhealthy.
pub const UNHEALTHY_AFTER_FAILURES: u32 = 3;
/// First retry delay after a failure; doubles per failure up to [`MAX_BACKOFF`].
pub const FIRST_BACKOFF: Duration = Duration::from_secs(1);
/// Longest retry delay.
pub const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// The shortest lag that makes the worker unhealthy: durable data unshipped
/// for longer than `max(MIN_UNHEALTHY_LAG, 10 × wal_interval)`.
pub const MIN_UNHEALTHY_LAG: Duration = Duration::from_secs(30);
/// Retry delay after a failed lease renewal (capped at the renewal interval).
pub const RENEW_RETRY: Duration = Duration::from_secs(1);
/// Read size when streaming a snapshot or archive.
const UPLOAD_CHUNK: usize = 8 * 1024 * 1024;

/// Where the worker learns how much of the WAL is durable.
///
/// [`SharedDb`] is the real one. The trait exists so the worker does not care
/// how the handle is held.
pub trait WalSource: Send + Sync + 'static {
    fn durable_wal_position(&self) -> WalPosition;

    /// Absorb whatever another process committed or snapshotted. Called, on a
    /// blocking thread, only when the files on disk differ from the position
    /// and no snapshot is in progress.
    fn catch_up(&self) {}
}

impl WalSource for SharedDb {
    fn durable_wal_position(&self) -> WalPosition {
        SharedDb::durable_wal_position(self)
    }

    fn catch_up(&self) {
        self.refresh_if_stale_now();
    }
}

/// What the serve loop tells the worker, over a `watch` channel.
///
/// A `watch` keeps only the latest value, so a `Snapshot` followed quickly by
/// a `Shutdown` is seen as `Shutdown` alone. That is safe: the worker finds a
/// new snapshot by itself (the files on disk change), and its shutdown flush
/// uploads it. `Snapshot` only makes that happen now instead of at the next
/// tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackupTrigger {
    /// The channel's initial value; means nothing.
    #[default]
    Idle,
    /// A snapshot finished: upload it now.
    Snapshot,
    /// Flush what is pending, write a final manifest, and stop — within
    /// `shutdown_grace_ms`, abandoning an upload still in flight then.
    Shutdown,
}

/// The running worker, as the serve loop holds it.
///
/// There is one way to stop it: send [`BackupTrigger::Shutdown`] (or drop the
/// sender), then await `join`. It finishes within `shutdown_grace_ms`, however
/// unresponsive the bucket is.
#[derive(Debug)]
pub struct BackupHandle {
    /// Failed steps since start (`mushroomdb_backup_upload_errors_total`).
    pub error_count: Arc<AtomicU64>,
    /// Unix seconds of the last moment everything durable was shipped; 0
    /// before the first.
    pub last_success_ts: Arc<AtomicU64>,
    /// False after [`UNHEALTHY_AFTER_FAILURES`] consecutive failures, while
    /// durable data has stayed unshipped for longer than
    /// `max(30 s, 10 × wal_interval)`, or for good once the worker has
    /// stopped; true otherwise.
    pub healthy: Arc<AtomicBool>,
    /// `backup_lag_s`: seconds the oldest durable, unshipped WAL byte has
    /// waited; 0 when everything durable is shipped.
    pub lag_s: Arc<AtomicU64>,
    /// Why the worker is unhealthy, while it is; once it has stopped, why it
    /// stopped (`"stopped"` after a shutdown, the panic message after a panic).
    pub reason: Arc<Mutex<Option<String>>>,
    /// Finishes when the worker has flushed and stopped.
    pub join: JoinHandle<()>,
}

/// The worker's counters, shared with whoever reports them.
#[derive(Debug, Clone)]
pub struct BackupCounters {
    pub error_count: Arc<AtomicU64>,
    pub last_success_ts: Arc<AtomicU64>,
    pub healthy: Arc<AtomicBool>,
    pub lag_s: Arc<AtomicU64>,
    pub reason: Arc<Mutex<Option<String>>>,
}

/// Which WAL file a generation ships, as the engine's position names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WalKey {
    engine: u64,
    snapshot: Option<Ident>,
    wal: Option<u64>,
}

impl WalKey {
    fn of(pos: &WalPosition) -> Self {
        Self {
            engine: pos.generation,
            snapshot: pos.snapshot,
            wal: pos.wal,
        }
    }
}

/// The WAL generation being shipped.
#[derive(Debug, Clone)]
struct Generation {
    /// Its number in the bucket (`wal_generation`).
    number: u64,
    key: WalKey,
    /// Bytes of `wal.bin` shipped so far.
    cursor: u64,
    /// The store's horizon floor when the generation started.
    floor: u64,
    /// The chain side files, as they were when the generation started.
    chain: Vec<FileEntry>,
    segments: Vec<SegmentEntry>,
}

/// What the health evaluation reads, shared by the worker and its supervisor.
#[derive(Debug, Default)]
struct HealthState {
    /// What has been shipped: which WAL, and up to where.
    shipped: Option<(WalKey, u64)>,
    consecutive_failures: u32,
    /// Why the last step did nothing, when it deferred.
    deferral: Option<String>,
    /// Durable lengths seen and when first seen, for the WAL being shipped.
    seen: VecDeque<(u64, Instant)>,
    seen_key: Option<WalKey>,
    /// Since when a new restore point has been due and not made.
    base_due_since: Option<Instant>,
    /// Why the bucket lease keeps this worker from uploading, while it does.
    lease: Option<String>,
    /// Why this worker refused the prefix (another lineage, or a rewound
    /// store). Permanent for the life of the worker.
    lineage: Option<String>,
    stopped: Option<String>,
}

/// Lag and health, computed from the engine's position and what has shipped.
#[derive(Clone)]
struct Health {
    state: Arc<Mutex<HealthState>>,
    healthy: Arc<AtomicBool>,
    lag_s: Arc<AtomicU64>,
    last_success_ts: Arc<AtomicU64>,
    reason: Arc<Mutex<Option<String>>>,
    limit: Duration,
}

impl Health {
    fn lock(&self) -> std::sync::MutexGuard<'_, HealthState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Re-evaluate at `now` against the durable position `pos`.
    ///
    /// The lag is measured from the first moment a durable length beyond what
    /// has shipped was seen — evaluations run every cadence, so that is at
    /// most one cadence after the fsync — or, when a new restore point is
    /// due, from when that was first seen.
    fn observe(&self, pos: &WalPosition, now: Instant) {
        let mut s = self.lock();
        let key = WalKey::of(pos);
        let lag = match s.shipped {
            Some((k, cursor)) if k == key => {
                s.base_due_since = None;
                if s.seen_key != Some(key) {
                    s.seen.clear();
                    s.seen_key = Some(key);
                }
                if pos.len > s.seen.back().map_or(cursor, |(l, _)| *l) {
                    s.seen.push_back((pos.len, now));
                }
                while s.seen.front().is_some_and(|(l, _)| *l <= cursor) {
                    s.seen.pop_front();
                }
                s.seen
                    .front()
                    .map(|(_, t)| now.saturating_duration_since(*t))
            }
            _ => {
                let since = *s.base_due_since.get_or_insert(now);
                Some(now.saturating_duration_since(since))
            }
        };
        let lag = lag.unwrap_or(Duration::ZERO);
        if lag.is_zero() {
            self.last_success_ts.store(unix_now(), Ordering::Release);
        }
        self.lag_s.store(lag.as_secs(), Ordering::Release);
        let reason = if let Some(r) = &s.stopped {
            Some(r.clone())
        } else if let Some(r) = &s.lineage {
            Some(r.clone())
        } else if let Some(r) = &s.lease {
            Some(r.clone())
        } else if s.consecutive_failures >= UNHEALTHY_AFTER_FAILURES {
            Some(format!(
                "{} consecutive failed backup steps",
                s.consecutive_failures
            ))
        } else if lag > self.limit {
            Some(match &s.deferral {
                Some(d) => format!("durable data unshipped for {}s: {d}", lag.as_secs()),
                None => format!("durable data unshipped for {}s", lag.as_secs()),
            })
        } else {
            None
        };
        self.healthy.store(reason.is_none(), Ordering::Release);
        *self.reason.lock().unwrap_or_else(|e| e.into_inner()) = reason;
    }

    /// The worker is gone, for `why`. Unhealthy from now on.
    fn stop(&self, why: String) {
        self.lock().stopped = Some(why.clone());
        self.healthy.store(false, Ordering::Release);
        *self.reason.lock().unwrap_or_else(|e| e.into_inner()) = Some(why);
    }
}

/// The backup worker. [`BackupWorker::spawn`] runs it as a task; tests drive
/// it a step at a time with [`run_once`](BackupWorker::run_once).
pub struct BackupWorker {
    config: BackupConfig,
    store: Arc<dyn ObjectStore>,
    source: Arc<dyn WalSource>,
    dir: PathBuf,
    prefix: String,
    error_count: Arc<AtomicU64>,
    health: Health,
    backoff: Duration,
    /// Whether the bucket's previous state has been read.
    resumed: bool,
    next_generation: u64,
    /// Every generation number with objects in the bucket that retention has
    /// not retired yet.
    generations: BTreeSet<u64>,
    current: Option<Generation>,
    /// The snapshot the current generation starts beside.
    snapshot: Option<FileEntry>,
    /// Archives in the bucket, by number, with the local length they were
    /// uploaded from. An archive is immutable: never uploaded twice. Entries
    /// seeded from the bucket at resume are reused only once the local
    /// file's CRC matches.
    archives: BTreeMap<u64, Archive>,
    /// Archives the current restore point names.
    base_archives: Vec<u64>,
    /// Side files refreshed at every manifest, by name, with the local
    /// identity they came from.
    side: BTreeMap<&'static str, (Ident, FileEntry)>,
    manifest_dirty: bool,
    last_manifest_at: Option<Instant>,
    /// Test hook: requests that change the bucket still allowed before the
    /// worker behaves as if killed. See [`BackupWorker::fail_after_writes`].
    write_budget: Option<u64>,
    /// Test hook: panic at the start of the next step.
    panic_next: bool,
    /// LIST requests made, for the retention-cost test.
    lists: u64,
    /// The bucket lease, while this worker holds it (spawned workers only).
    lease: Option<Arc<BucketLease>>,
    /// The task renewing `lease`.
    renewer: Option<JoinHandle<()>>,
    /// Leases taken so far; a second one starts from the bucket afresh.
    tenures: u64,
    /// The foreign holder last logged, so a wait is logged once.
    foreign: Option<String>,
    /// This worker's instance id, the same across every lease attempt.
    instance_id: String,
    /// What lease attempts have seen of a suspect foreign lease.
    watch: LeaseWatch,
    /// The store's lineage, once resume has established it.
    lineage: Option<String>,
    /// Set when resume refused the prefix: the worker uploads nothing more.
    refused: bool,
    /// The version of `LATEST` last written or read, for `If-Match`; `None`
    /// when it does not exist.
    latest_version: Option<UpdateVersion>,
    /// The manifest `LATEST` names.
    latest_target: Option<String>,
    /// Versions of the manifests this tenure wrote, for `If-Match`.
    manifest_versions: HashMap<String, UpdateVersion>,
    /// Generation numbers this tenure allocated.
    allocated: BTreeSet<u64>,
}

/// An archive known to be in the bucket.
#[derive(Debug, Clone)]
struct Archive {
    /// Length of the local file it was uploaded from.
    len: u64,
    entry: FileEntry,
    /// Whether the local file is known to have these bytes: uploaded by this
    /// tenure, or CRC-checked against the entry.
    verified: bool,
}

impl BackupWorker {
    /// A worker for the store in `store_dir`, uploading to `store` under the
    /// prefix `config.s3_url` names.
    pub fn new(
        config: BackupConfig,
        store: Arc<dyn ObjectStore>,
        source: Arc<dyn WalSource>,
        store_dir: PathBuf,
    ) -> Result<Self, BackupError> {
        let prefix = config.location()?.prefix;
        let limit = MIN_UNHEALTHY_LAG.max(Duration::from_millis(config.wal_interval_ms) * 10);
        Ok(Self {
            config,
            store,
            source,
            dir: store_dir,
            prefix,
            error_count: Arc::new(AtomicU64::new(0)),
            health: Health {
                state: Arc::default(),
                healthy: Arc::new(AtomicBool::new(true)),
                lag_s: Arc::new(AtomicU64::new(0)),
                last_success_ts: Arc::new(AtomicU64::new(0)),
                reason: Arc::default(),
                limit,
            },
            backoff: FIRST_BACKOFF,
            resumed: false,
            next_generation: 0,
            generations: BTreeSet::new(),
            current: None,
            snapshot: None,
            archives: BTreeMap::new(),
            base_archives: Vec::new(),
            side: BTreeMap::new(),
            manifest_dirty: false,
            last_manifest_at: None,
            write_budget: None,
            panic_next: false,
            lists: 0,
            lease: None,
            renewer: None,
            tenures: 0,
            foreign: None,
            instance_id: new_instance_id(),
            watch: LeaseWatch::default(),
            lineage: None,
            refused: false,
            latest_version: None,
            latest_target: None,
            manifest_versions: HashMap::new(),
            allocated: BTreeSet::new(),
        })
    }

    /// Start the worker as a Tokio task. It takes the bucket lease, then runs
    /// one step at once — reading the bucket's previous state and uploading a
    /// base restore point — then one every `wal_interval_ms`, and one
    /// immediately on each trigger. While another instance holds the lease it
    /// uploads nothing and is unhealthy (see [the module docs](self)).
    ///
    /// # Errors
    ///
    /// Only a malformed `config.s3_url`.
    pub fn spawn(
        config: BackupConfig,
        store: Arc<dyn ObjectStore>,
        source: Arc<dyn WalSource>,
        store_dir: PathBuf,
        snapshot_rx: watch::Receiver<BackupTrigger>,
    ) -> Result<BackupHandle, BackupError> {
        Ok(Self::new(config, store, source, store_dir)?.start(snapshot_rx))
    }

    /// Run this worker as a Tokio task, beside a supervisor that evaluates
    /// health every `wal_interval_ms` and reports the worker's end — a
    /// shutdown, or a panic — as unhealthy with the reason.
    pub fn start(self, snapshot_rx: watch::Receiver<BackupTrigger>) -> BackupHandle {
        let BackupCounters {
            error_count,
            last_success_ts,
            healthy,
            lag_s,
            reason,
        } = self.counters();
        let health = self.health.clone();
        let source = Arc::clone(&self.source);
        let cadence = Duration::from_millis(self.config.wal_interval_ms);
        let worker = tokio::spawn(self.run(snapshot_rx));
        let join = tokio::spawn(async move {
            let mut tick = tokio::time::interval(cadence);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tokio::pin!(worker);
            let ended = loop {
                tokio::select! {
                    ended = &mut worker => break ended,
                    _ = tick.tick() => {
                        let pos = position_of(&source).await;
                        health.observe(&pos, Instant::now());
                    }
                }
            };
            let why = match ended {
                Ok(()) => "stopped".to_string(),
                Err(e) if e.is_panic() => {
                    let p = e.into_panic();
                    let msg = p
                        .downcast_ref::<&str>()
                        .map(|s| s.to_string())
                        .or_else(|| p.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "a non-string panic".into());
                    tracing::error!(panic = %msg, "backup worker panicked; backup has stopped");
                    format!("backup worker panicked: {msg}")
                }
                Err(e) => format!("backup worker ended: {e}"),
            };
            health.stop(why);
        });
        BackupHandle {
            error_count,
            last_success_ts,
            healthy,
            lag_s,
            reason,
            join,
        }
    }

    /// The counters this worker updates (the same ones its
    /// [`BackupHandle`] carries).
    pub fn counters(&self) -> BackupCounters {
        BackupCounters {
            error_count: Arc::clone(&self.error_count),
            last_success_ts: Arc::clone(&self.health.last_success_ts),
            healthy: Arc::clone(&self.health.healthy),
            lag_s: Arc::clone(&self.health.lag_s),
            reason: Arc::clone(&self.health.reason),
        }
    }

    /// The instance id this worker takes the bucket lease under, the same
    /// for every attempt.
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// Test hook: allow `n` more requests that change the bucket (PUTs and
    /// DELETEs), then fail every one as if the process had been killed just
    /// before it. Drop the worker after the first failure to model a kill
    /// between two requests.
    #[doc(hidden)]
    pub fn fail_after_writes(&mut self, n: u64) {
        self.write_budget = Some(n);
    }

    /// Test hook: panic at the start of the next step.
    #[doc(hidden)]
    pub fn panic_on_next_step(&mut self) {
        self.panic_next = true;
    }

    /// Test hook: LIST requests made so far.
    #[doc(hidden)]
    pub fn list_requests(&self) -> u64 {
        self.lists
    }

    async fn run(mut self, mut rx: watch::Receiver<BackupTrigger>) {
        let mut delay = Duration::ZERO;
        loop {
            let shutdown = tokio::select! {
                _ = tokio::time::sleep(delay) => false,
                changed = rx.changed() => is_shutdown(changed, &mut rx),
            };
            if shutdown {
                break;
            }
            // Nothing is uploaded without the bucket lease.
            if !self.holds_lease() {
                match until_shutdown(&mut rx, self.take_lease()).await {
                    None => break,
                    Some(Some(wait)) => {
                        delay = wait;
                        continue;
                    }
                    Some(None) => {}
                }
            }
            // A step blocked on an unresponsive bucket must not hold up a
            // shutdown: the step is dropped — its in-flight request abandoned
            // — the moment one arrives.
            match until_shutdown(&mut rx, self.step()).await {
                Some(d) => delay = d,
                None => break,
            }
            if self.refused {
                break;
            }
        }
        if self.refused {
            // The prefix is not ours: give the lease back so its owner is not
            // kept out, and idle, unhealthy, until shutdown.
            self.release_lease().await;
            until_shutdown(&mut rx, std::future::pending::<()>()).await;
            return;
        }
        if self.holds_lease() {
            let grace = Duration::from_millis(self.config.shutdown_grace_ms);
            if tokio::time::timeout(grace, self.flush()).await.is_err() {
                self.error_count.fetch_add(1, Ordering::AcqRel);
                tracing::warn!(
                    grace_ms = self.config.shutdown_grace_ms,
                    "backup flush did not finish within the shutdown grace; abandoned"
                );
            }
        }
        self.release_lease().await;
    }

    // ── The bucket lease ────────────────────────────────────────────────────

    fn holds_lease(&self) -> bool {
        self.lease.as_ref().is_some_and(|l| !l.is_lost())
    }

    /// Try to take the bucket lease. `None` once it is held; otherwise how
    /// long to wait before trying again.
    async fn take_lease(&mut self) -> Option<Duration> {
        if self.lease.as_ref().is_some_and(|l| l.is_lost()) {
            if let Some(r) = self.renewer.take() {
                r.abort();
            }
            self.lease = None;
        }
        let ttl_s = self.config.lease_ttl_s;
        // A few requests: a read, a conditional write, the two probes.
        let limit = self.op_timeout() * 4;
        let acquire = BucketLease::acquire_as(
            Arc::clone(&self.store),
            &self.prefix,
            ttl_s,
            &self.instance_id,
            &mut self.watch,
        );
        let wait = match tokio::time::timeout(limit, acquire).await {
            Ok(Ok(lease)) => {
                self.hold(lease);
                None
            }
            Ok(Err(LeaseError::ForeignActive { instance_id })) => {
                if self.foreign.as_deref() != Some(instance_id.as_str()) {
                    tracing::warn!(
                        holder = %instance_id,
                        "the backup bucket's lease is held by another instance; \
                         this one uploads nothing while that lease is fresh"
                    );
                    self.foreign = Some(instance_id.clone());
                }
                self.health.lock().lease = Some(format!(
                    "the backup bucket's lease is held by another instance ({instance_id}); \
                     this instance uploads nothing while it is"
                ));
                Some(poll_interval(ttl_s))
            }
            Ok(Err(e @ LeaseError::PreconditionsIgnored { .. })) => {
                tracing::error!(error = %e, "backup is not started");
                self.health.lock().lease = Some(e.to_string());
                Some(MAX_BACKOFF)
            }
            Ok(Err(LeaseError::Store(e))) if self.config.lease_conditional => {
                let why = format!(
                    "taking the bucket lease: {} (if this service rejects conditional PUT \
                     headers, set MUSHROOMDB_BACKUP_LEASE_CONDITIONAL=off)",
                    crate::redact(&e.to_string())
                );
                Some(self.failed(&BackupError::Lease(why)))
            }
            Ok(Err(e)) => Some(self.failed(&BackupError::Lease(e.to_string()))),
            Err(_) => Some(self.failed(&BackupError::Timeout("taking the bucket lease".into()))),
        };
        let pos = self.position().await;
        self.health.observe(&pos, Instant::now());
        wait
    }

    /// Hold `lease`: start renewing it, and forget a previous tenure's view
    /// of the bucket.
    fn hold(&mut self, lease: BucketLease) {
        if self.tenures > 0 {
            self.reset_tenure();
        }
        self.tenures += 1;
        self.foreign = None;
        self.health.lock().lease = None;
        match lease.exclusion() {
            Exclusion::Conditional => tracing::info!(
                instance_id = lease.instance_id(),
                "took the backup bucket lease (conditional writes: one writer at a time)"
            ),
            Exclusion::BestEffort => tracing::warn!(
                instance_id = lease.instance_id(),
                "took the backup bucket lease BEST-EFFORT: conditional writes are off, so two \
                 instances started together may both upload and overwrite each other's objects"
            ),
        }
        let lease = Arc::new(lease);
        self.renewer = Some(tokio::spawn(renew_loop(
            Arc::clone(&lease),
            self.op_timeout(),
            self.health.clone(),
            Arc::clone(&self.error_count),
        )));
        self.lease = Some(lease);
    }

    /// Forget everything known about the bucket: another writer may have
    /// changed it since this worker last held the lease.
    fn reset_tenure(&mut self) {
        self.resumed = false;
        self.lineage = None;
        self.latest_version = None;
        self.latest_target = None;
        self.manifest_versions.clear();
        self.allocated.clear();
        self.next_generation = 0;
        self.generations.clear();
        self.current = None;
        self.snapshot = None;
        self.archives.clear();
        self.base_archives.clear();
        self.side.clear();
        self.manifest_dirty = false;
        self.last_manifest_at = None;
        self.health.lock().shipped = None;
    }

    /// Stop renewing and delete the lease, if this worker still holds it.
    async fn release_lease(&mut self) {
        let Some(lease) = self.lease.take() else {
            return;
        };
        if let Some(r) = self.renewer.take() {
            r.abort();
            let _ = r.await;
        }
        let Ok(lease) = Arc::try_unwrap(lease) else {
            return;
        };
        match tokio::time::timeout(self.op_timeout(), lease.release()).await {
            Ok(Ok(())) => tracing::info!("released the backup bucket lease"),
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "could not release the backup bucket lease; it expires by itself")
            }
            Err(_) => {
                tracing::warn!("releasing the backup bucket lease timed out; it expires by itself")
            }
        }
    }

    /// One step, with its outcome recorded in the counters. Returns how long
    /// to wait before the next one: the WAL interval after a success or a
    /// deferral, the back-off after a failure.
    pub async fn step(&mut self) -> Duration {
        let result = self.run_once().await;
        let delay = match result {
            Ok(()) => {
                self.health.lock().consecutive_failures = 0;
                self.backoff = FIRST_BACKOFF;
                Duration::from_millis(self.config.wal_interval_ms)
            }
            Err(e) => self.failed(&e),
        };
        let pos = self.position().await;
        self.health.observe(&pos, Instant::now());
        delay
    }

    /// Count a failed step or lease attempt; return the back-off to wait.
    fn failed(&mut self, e: &BackupError) -> Duration {
        self.error_count.fetch_add(1, Ordering::AcqRel);
        let failures = {
            let mut s = self.health.lock();
            s.consecutive_failures += 1;
            s.consecutive_failures
        };
        let delay = self.backoff;
        self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
        tracing::warn!(
            error = %e,
            consecutive_failures = failures,
            retry_in_s = delay.as_secs(),
            "backup step failed"
        );
        delay
    }

    /// Ship pending WAL bytes and write a final manifest, retrying a failure
    /// twice. Used on shutdown, under the shutdown grace.
    pub async fn flush(&mut self) {
        for attempt in 0..3u32 {
            let done = match self.run_once().await {
                Ok(()) => self.write_manifest().await,
                other => other,
            };
            match done {
                Ok(()) => {
                    let pos = self.position().await;
                    self.health.observe(&pos, Instant::now());
                    return;
                }
                Err(e) => {
                    self.error_count.fetch_add(1, Ordering::AcqRel);
                    tracing::warn!(error = %e, attempt, "backup flush failed");
                    if attempt < 2 {
                        tokio::time::sleep(FIRST_BACKOFF * (1 << attempt)).await;
                    }
                }
            }
        }
    }

    /// Record why this step did nothing, and do nothing. Not a failure.
    fn defer(&mut self, why: &str) -> Result<(), BackupError> {
        self.health.lock().deferral = Some(why.to_string());
        Ok(())
    }

    /// One step: read the bucket's previous state if not done yet; upload a
    /// new restore point when the snapshot or WAL file on disk, or the
    /// engine's WAL generation, changed; ship the WAL bytes fsynced since the
    /// last step; write a manifest when one is due.
    pub async fn run_once(&mut self) -> Result<(), BackupError> {
        if std::mem::take(&mut self.panic_next) {
            panic!("test hook: backup worker panics");
        }
        if !self.resumed && !self.resume().await? {
            return self.defer("the store's commit count is not known yet");
        }
        let mut disk = self.observe().await?;
        if disk.intent_nonempty {
            return self.defer("a snapshot is in progress (snapshot.intent is not empty)");
        }
        let mut pos = self.position().await;
        // The disk differs from the handle: another process snapshotted,
        // replaced or extended the WAL. Let the handle catch up — the intent
        // was empty just now — and keep the result only if the disk held
        // still across it.
        if pos.snapshot != disk.snapshot || pos.wal != disk.wal || disk.wal_len != pos.len {
            self.catch_up().await?;
            let after = self.observe().await?;
            if !disk.unchanged_until(&after) {
                return self.defer("the store's files changed while catching up");
            }
            disk = after;
            pos = self.position().await;
        }
        if pos.snapshot_in_progress {
            return self.defer("this handle is taking a snapshot");
        }
        if pos.snapshot != disk.snapshot || pos.wal != disk.wal || disk.wal_len < pos.len {
            return self.defer("the handle has not caught up with the files on disk");
        }
        self.health.lock().deferral = None;
        if self.current.as_ref().map(|g| g.key) != Some(WalKey::of(&pos)) {
            return self.upload_base(pos, disk).await;
        }
        self.ship_segments().await?;
        if self.manifest_due() {
            self.write_manifest().await?;
        }
        Ok(())
    }

    fn manifest_due(&self) -> bool {
        let interval = Duration::from_secs(self.config.manifest_interval_s);
        self.manifest_dirty
            && self
                .last_manifest_at
                .is_none_or(|t| t.elapsed() >= interval)
    }

    async fn observe(&self) -> Result<DiskState, BackupError> {
        let dir = self.dir.clone();
        Ok(blocking(move || disk::observe(&dir)).await?)
    }

    async fn position(&self) -> WalPosition {
        position_of(&self.source).await
    }

    async fn catch_up(&self) -> Result<(), BackupError> {
        let source = Arc::clone(&self.source);
        blocking(move || {
            source.catch_up();
            Ok(())
        })
        .await?;
        Ok(())
    }

    // ── Resume ──────────────────────────────────────────────────────────────

    /// Read what a previous run left in the bucket, so this run never reuses a
    /// generation number and never re-uploads an archive the latest manifest
    /// names; and check that the prefix is this store's lineage. `Ok(false)`:
    /// not yet decidable, try again.
    ///
    /// # Errors
    ///
    /// [`BackupError::Lineage`] when the prefix belongs to another lineage or
    /// this store was rewound: the worker then refuses the prefix for good.
    async fn resume(&mut self) -> Result<bool, BackupError> {
        let latest = self.latest_manifest().await?;
        let dir = self.dir.clone();
        let local = blocking(move || disk::read_lineage(&dir)).await?;
        let lineage = match (local, &latest) {
            (Some(ours), Some(m)) if ours != m.lineage => {
                return Err(self.refuse(format!(
                    "this backup prefix holds the backups of another store (lineage {}), \
                     not this one (lineage {ours}); nothing is uploaded or deleted. Point \
                     MUSHROOMDB_BACKUP_S3_URL at a new prefix, or restore this store from \
                     that prefix first",
                    m.lineage
                )))
            }
            (None, Some(m)) => {
                return Err(self.refuse(format!(
                    "this backup prefix holds the backups of another store (lineage {}), and \
                     this store has never been backed up; nothing is uploaded or deleted. \
                     Point MUSHROOMDB_BACKUP_S3_URL at a new prefix, or restore this store \
                     from that prefix first",
                    m.lineage
                )))
            }
            (Some(ours), Some(m)) => match self.position().await.frames {
                None => return Ok(false),
                Some(have) if have < m.commit_seq => {
                    return Err(self.refuse(format!(
                        "this store holds {have} commits, fewer than the {} of the newest \
                         restore point in the backup prefix: it was restored from an older \
                         backup, or rewound. Uploading would bury the newer history, so \
                         nothing is uploaded or deleted. Point MUSHROOMDB_BACKUP_S3_URL at \
                         a new prefix",
                        m.commit_seq
                    )))
                }
                Some(_) => ours,
            },
            (Some(ours), None) => ours,
            (None, None) => {
                let id = new_instance_id();
                let (dir, new) = (self.dir.clone(), id.clone());
                blocking(move || disk::create_lineage(&dir, &new)).await?;
                tracing::info!(lineage = %id, "this store's first backup: a new backup lineage");
                id
            }
        };
        let wal_dir = ObjectPath::from(object_key(&self.prefix, "wal"));
        for p in self.list(&wal_dir).await?.common_prefixes {
            if let Some(g) = p.filename().and_then(|s| s.parse::<u64>().ok()) {
                self.generations.insert(g);
            }
        }
        for dir in ["snapshots", "archives"] {
            let path = ObjectPath::from(object_key(&self.prefix, dir));
            for o in self.list(&path).await?.objects {
                if let Some(g) = parse_versioned_generation(&self.prefix, o.location.as_ref()) {
                    self.generations.insert(g);
                }
            }
        }
        let mut highest = self.generations.last().copied();
        if let Some(m) = latest {
            highest = highest.max(Some(m.wal_generation));
            for f in &m.files {
                if let Some(n) = parse_archive_name(&f.name) {
                    self.archives.insert(
                        n,
                        Archive {
                            len: f.size,
                            entry: f.clone(),
                            verified: false,
                        },
                    );
                }
            }
        }
        self.next_generation = highest.map_or(0, |g| g + 1);
        self.lineage = Some(lineage);
        self.resumed = true;
        Ok(true)
    }

    /// Refuse the prefix for the life of this worker, for `why`.
    fn refuse(&mut self, why: String) -> BackupError {
        tracing::error!(reason = %why, "backup refuses this prefix");
        self.health.lock().lineage = Some(why.clone());
        self.refused = true;
        BackupError::Lineage(why)
    }

    /// The lineage resume established; empty before it.
    fn lineage(&self) -> String {
        self.lineage.clone().unwrap_or_default()
    }

    /// The manifest `LATEST` names, remembering `LATEST`'s version and
    /// target for the next conditional write of it.
    async fn latest_manifest(&mut self) -> Result<Option<Manifest>, BackupError> {
        let key = match self.get_versioned(&latest_key(&self.prefix)).await? {
            Some((b, v)) => {
                self.latest_version = Some(v);
                String::from_utf8(b)
                    .map_err(|_| BackupError::Manifest("LATEST is not UTF-8".into()))?
            }
            None => {
                self.latest_version = None;
                return Ok(None);
            }
        };
        self.latest_target = Some(key.trim().to_string());
        let bytes = self.get(key.trim()).await?.ok_or_else(|| {
            BackupError::Manifest(format!("LATEST names {key}, which does not exist"))
        })?;
        Manifest::from_json(&bytes)
            .map(Some)
            .map_err(BackupError::Manifest)
    }

    // ── Restore points ──────────────────────────────────────────────────────

    /// Start a generation beside the snapshot on disk: open the snapshot and
    /// the archives and read the chain files while the disk holds still, then
    /// upload them under keys of their own, ship the generation's durable WAL
    /// bytes from offset 0, and write the manifest.
    async fn upload_base(&mut self, pos: WalPosition, disk: DiskState) -> Result<(), BackupError> {
        let dir = self.dir.clone();
        let read = blocking(move || {
            let snapshot = disk::open_snapshot(&dir)?;
            let mut archives = Vec::new();
            for (n, len) in disk::local_archives(&dir)? {
                if let Ok(f) = std::fs::File::open(dir.join(format!("wal.{n}.archive"))) {
                    archives.push((n, len, f));
                }
            }
            let mut chain = Vec::new();
            for name in CHAIN_FILES {
                if let Some((_, Some(bytes))) = disk::read_side(&dir, name, None)? {
                    chain.push((name, bytes));
                }
            }
            Ok((snapshot, archives, chain))
        })
        .await?;
        let (snapshot, archives, chain) = read;
        let after = self.observe().await?;
        let pos_after = self.position().await;
        let opened_ident = snapshot.as_ref().map(|(_, i)| *i);
        if !disk.unchanged_until(&after)
            || opened_ident != disk.snapshot
            || WalKey::of(&pos_after) != WalKey::of(&pos)
            || pos_after.snapshot_in_progress
        {
            return self.defer("the store's files changed while a restore point was read");
        }
        // Burn the number now: objects a failed attempt leaves behind belong
        // to a generation no manifest will ever name.
        let number = self.next_generation;
        self.next_generation += 1;
        self.generations.insert(number);
        self.allocated.insert(number);

        let mut present = Vec::new();
        for (n, len, mut file) in archives {
            let reusable = match self.archives.get(&n) {
                Some(a) if a.len == len && a.verified => true,
                Some(a) if a.len == len => {
                    // Seeded from the bucket: the same number and length are
                    // not the same bytes until the CRC says so.
                    let want = a.entry.crc32.clone();
                    let (crc, f) =
                        blocking(move || disk::crc32_of(&mut file).map(|c| (c, file))).await?;
                    file = f;
                    crc == want
                }
                _ => false,
            };
            if reusable {
                if let Some(a) = self.archives.get_mut(&n) {
                    a.verified = true;
                }
            } else {
                let name = format!("wal.{n}.archive");
                let key = archive_key(&self.prefix, number, n);
                let entry = self.put_file(&name, &key, file, true).await?;
                self.archives.insert(
                    n,
                    Archive {
                        len: entry.size,
                        entry,
                        verified: true,
                    },
                );
            }
            present.push(n);
        }
        let mut floor = 0;
        let mut chain_entries = Vec::new();
        for (name, bytes) in chain {
            if name == "wal.floor" {
                floor = bytes
                    .get(..8)
                    .map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap()));
            }
            chain_entries.push(self.put_side(name, bytes).await?);
        }
        let archive_frames: u64 = present
            .iter()
            .filter_map(|n| self.archives.get(n))
            .map(|a| a.entry.frames)
            .sum();
        let snapshot = match snapshot {
            Some((file, _)) => {
                let key = snapshot_key(&self.prefix, number, floor + archive_frames);
                Some(self.put_file("snapshot.bin", &key, file, false).await?)
            }
            None => None,
        };
        self.sync_side_files().await?;
        self.snapshot = snapshot;
        self.base_archives = present;
        self.current = Some(Generation {
            number,
            key: WalKey::of(&pos),
            cursor: 0,
            floor,
            chain: chain_entries,
            segments: Vec::new(),
        });
        self.record_shipped();
        self.manifest_dirty = true;
        self.ship_segments().await?;
        self.write_manifest().await
    }

    /// Upload a side file under its content-addressed key.
    async fn put_side(&mut self, name: &str, bytes: Vec<u8>) -> Result<FileEntry, BackupError> {
        let crc32 = crc32_hex(&bytes);
        let size = bytes.len() as u64;
        let key = side_key(&self.prefix, name, &crc32, size);
        self.put_immutable(&key, bytes, &crc32).await?;
        Ok(FileEntry {
            name: name.to_string(),
            key,
            size,
            crc32,
            frames: 0,
            lineage: self.lineage(),
        })
    }

    /// Upload each per-manifest side file whose local copy changed since it
    /// was last uploaded; forget one that no longer exists locally. The chain
    /// files are the generation's, not refreshed here.
    async fn sync_side_files(&mut self) -> Result<(), BackupError> {
        for name in SIDE_FILES {
            if CHAIN_FILES.contains(&name) {
                continue;
            }
            let known = self.side.get(name).map(|(i, _)| *i);
            let dir = self.dir.clone();
            let read = blocking(move || disk::read_side(&dir, name, known)).await?;
            let Some((ident, bytes)) = read else {
                if self.side.remove(name).is_some() {
                    self.manifest_dirty = true;
                }
                continue;
            };
            let Some(bytes) = bytes else {
                continue;
            };
            let crc32 = crc32_hex(&bytes);
            let size = bytes.len() as u64;
            let unchanged = self
                .side
                .get(name)
                .is_some_and(|(_, e)| e.key == side_key(&self.prefix, name, &crc32, size));
            let entry = if unchanged {
                self.side[name].1.clone()
            } else {
                self.manifest_dirty = true;
                self.put_side(name, bytes).await?
            };
            self.side.insert(name, (ident, entry));
        }
        Ok(())
    }

    // ── WAL segments ────────────────────────────────────────────────────────

    /// Ship every fsynced WAL byte of the current generation not shipped yet.
    /// Each read is bracketed by two looks at the directory and dropped if
    /// anything that pairs it with the snapshot moved.
    async fn ship_segments(&mut self) -> Result<(), BackupError> {
        loop {
            let Some(g) = self.current.as_ref() else {
                return Ok(());
            };
            let (key, cursor, number) = (g.key, g.cursor, g.number);
            let before = self.observe().await?;
            let pos = self.position().await;
            if before.intent_nonempty
                || before.snapshot != key.snapshot
                || before.wal != key.wal
                || WalKey::of(&pos) != key
            {
                return Ok(());
            }
            if pos.len <= cursor {
                return Ok(());
            }
            let wal = self.dir.join("wal.bin");
            let len = pos.len;
            let read = blocking(move || read_new_segment(&wal, cursor, len)).await?;
            let after = self.observe().await?;
            if !before.unchanged_until(&after) || WalKey::of(&self.position().await) != key {
                return Ok(());
            }
            let Some((start, end, bytes)) = read else {
                return Ok(());
            };
            let (_, frames) = complete_frames(&bytes);
            let seg_key = segment_key(&self.prefix, number, start, end);
            let crc32 = crc32_hex(&bytes);
            self.put_immutable(&seg_key, bytes, &crc32).await?;
            let lineage = self.lineage();
            if let Some(g) = self.current.as_mut() {
                g.cursor = end;
                g.segments.push(SegmentEntry {
                    key: seg_key,
                    crc32,
                    frames,
                    lineage,
                });
            }
            self.record_shipped();
            self.manifest_dirty = true;
        }
    }

    fn record_shipped(&self) {
        if let Some(g) = &self.current {
            self.health.lock().shipped = Some((g.key, g.cursor));
        }
    }

    // ── Manifests and retention ─────────────────────────────────────────────

    /// Write a manifest for the current restore point and point `LATEST` at
    /// it, then apply retention.
    async fn write_manifest(&mut self) -> Result<(), BackupError> {
        self.sync_side_files().await?;
        let Some(generation) = self.current.as_ref() else {
            return Ok(());
        };
        let mut files = Vec::new();
        files.extend(self.snapshot.clone());
        for n in &self.base_archives {
            if let Some(a) = self.archives.get(n) {
                files.push(a.entry.clone());
            }
        }
        files.extend(generation.chain.iter().cloned());
        files.extend(self.side.values().map(|(_, e)| e.clone()));
        // What the restored store will count: its floor, plus every frame of
        // every archive and segment listed here.
        let commit_seq = generation.floor
            + files.iter().map(|f| f.frames).sum::<u64>()
            + generation.segments.iter().map(|s| s.frames).sum::<u64>();
        let manifest = Manifest {
            version: MANIFEST_VERSION,
            lineage: self.lineage(),
            created_at: now_rfc3339(),
            commit_seq,
            oldest_commit_seq: generation.floor,
            files,
            wal_generation: generation.number,
            segments: generation.segments.clone(),
        };
        // Refuse rather than write a restore point that would not verify.
        manifest.verify_lineage().map_err(BackupError::Manifest)?;
        let key = manifest_key(&self.prefix, manifest.commit_seq);
        self.put_manifest(&key, manifest.to_json()).await?;
        self.put_latest(&key).await?;
        self.manifest_dirty = false;
        self.last_manifest_at = Some(Instant::now());
        if self.config.retain_n > 0 {
            self.apply_retention(&key).await?;
        }
        Ok(())
    }

    /// Keep the newest `retain_n` manifests of this lineage, then delete
    /// what only the dropped ones named.
    ///
    /// - Only manifests that parse and name this lineage are counted or
    ///   deleted; any other manifest under the prefix is left alone.
    ///   `just_written` and `LATEST`'s target are always kept.
    /// - Snapshots and archives are deleted when no kept manifest names them
    ///   and their generation is this lineage's (named by one of its
    ///   manifests, or allocated by this tenure). Side files are deleted when
    ///   a dropped manifest named them and no kept one does.
    /// - Segments: a generation of this lineage older than every kept
    ///   manifest's (other than the current one) has its prefix listed, once,
    ///   and its unnamed segments deleted. `wal/` is never listed whole.
    ///
    /// `LATEST` and anything outside those directories is never touched.
    /// Conservative on doubt: when a kept manifest cannot be read, nothing but
    /// the surplus manifests is deleted.
    async fn apply_retention(&mut self, just_written: &str) -> Result<(), BackupError> {
        let dir = ObjectPath::from(object_key(&self.prefix, "manifest"));
        let mut keys: Vec<String> = self
            .list(&dir)
            .await?
            .objects
            .into_iter()
            .map(|o| o.location.to_string())
            .filter(|k| k.ends_with(".json"))
            .collect();
        keys.sort();
        let ours = self.lineage();
        let mut mine: Vec<(String, Manifest)> = Vec::new();
        for k in keys {
            let Some(bytes) = self.get(&k).await? else {
                continue;
            };
            match Manifest::from_json(&bytes) {
                Ok(m) if m.lineage == ours => mine.push((k, m)),
                _ => {}
            }
        }
        if mine.len() <= self.config.retain_n {
            return Ok(());
        }
        let newest_start = mine.len() - self.config.retain_n;
        let protected = |k: &str| k == just_written || self.latest_target.as_deref() == Some(k);
        let (mut kept, mut dropped) = (Vec::new(), Vec::new());
        for (i, (k, m)) in mine.into_iter().enumerate() {
            if i >= newest_start || protected(&k) {
                kept.push((k, m));
            } else {
                dropped.push((k, m));
            }
        }
        if dropped.is_empty() {
            return Ok(());
        }
        let mut owned: BTreeSet<u64> = self.allocated.clone();
        owned.extend(kept.iter().chain(&dropped).map(|(_, m)| m.wal_generation));
        for (k, _) in &dropped {
            self.delete(k).await?;
        }
        let mut referenced: HashSet<String> = HashSet::new();
        let mut oldest_kept = u64::MAX;
        for (_, m) in &kept {
            oldest_kept = oldest_kept.min(m.wal_generation);
            referenced.extend(m.files.iter().map(|f| f.key.clone()));
            referenced.extend(m.segments.iter().map(|s| s.key.clone()));
        }
        let mut doomed = Vec::new();
        for top in ["snapshots", "archives"] {
            for k in self.list_all(&object_key(&self.prefix, top)).await? {
                let ours = parse_versioned_generation(&self.prefix, &k)
                    .is_some_and(|g| owned.contains(&g));
                if ours && !referenced.contains(&k) {
                    doomed.push(k);
                }
            }
        }
        let side_prefix = object_key(&self.prefix, "side/");
        let side: BTreeSet<String> = dropped
            .iter()
            .flat_map(|(_, m)| m.files.iter().map(|f| f.key.clone()))
            .filter(|k| k.starts_with(&side_prefix) && !referenced.contains(k))
            .collect();
        doomed.extend(side);
        let current = self.current.as_ref().map(|g| g.number);
        let retiring: Vec<u64> = owned
            .iter()
            .copied()
            .filter(|g| *g < oldest_kept && Some(*g) != current)
            .collect();
        for g in &retiring {
            let prefix = object_key(&self.prefix, &format!("wal/{g}"));
            for k in self.list_all(&prefix).await? {
                if parse_segment_key(&self.prefix, &k).is_some_and(|(sg, _, _)| sg == *g)
                    && !referenced.contains(&k)
                {
                    doomed.push(k);
                }
            }
        }
        for k in doomed {
            self.delete(&k).await?;
        }
        for g in retiring {
            self.generations.remove(&g);
        }
        let keep: HashSet<&String> = referenced.iter().collect();
        self.archives.retain(|_, a| keep.contains(&a.entry.key));
        Ok(())
    }

    /// Every object key under `prefix`, recursively.
    async fn list_all(&mut self, prefix: &str) -> Result<Vec<String>, BackupError> {
        let mut out = Vec::new();
        let mut dirs = vec![ObjectPath::from(prefix)];
        while let Some(d) = dirs.pop() {
            let r = self.list(&d).await?;
            out.extend(r.objects.into_iter().map(|o| o.location.to_string()));
            dirs.extend(r.common_prefixes);
        }
        Ok(out)
    }

    // ── Object store calls, each under the operation deadline ───────────────

    fn op_timeout(&self) -> Duration {
        Duration::from_millis(self.config.op_timeout_ms)
    }

    async fn deadline<T>(
        &self,
        what: impl FnOnce() -> String,
        fut: impl Future<Output = Result<T, BackupError>>,
    ) -> Result<T, BackupError> {
        match tokio::time::timeout(self.op_timeout(), fut).await {
            Ok(r) => r,
            Err(_) => Err(BackupError::Timeout(what())),
        }
    }

    /// Spend one request of the test hook's budget, if one is set.
    fn spend_write(&mut self) -> Result<(), BackupError> {
        match self.write_budget.as_mut() {
            Some(0) => Err(BackupError::Inconsistent(
                "test hook: the process is gone".into(),
            )),
            Some(n) => {
                *n -= 1;
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// A request that changes the bucket: under the operation deadline and,
    /// when the worker holds a lease, the lease's fence.
    async fn fenced<T>(
        &self,
        what: impl FnOnce() -> String,
        fut: impl Future<Output = Result<T, BackupError>>,
    ) -> Result<T, BackupError> {
        fence(self.lease.as_deref(), self.deadline(what, fut)).await
    }

    /// One PUT in `mode`, under the deadline and the fence. The outer error
    /// is the worker's (deadline, fence, test hook); the inner one is the
    /// service's answer, for the caller to interpret.
    async fn put_mode(
        &mut self,
        key: &str,
        payload: PutPayload,
        mode: PutMode,
    ) -> Result<Result<PutResult, object_store::Error>, BackupError> {
        self.spend_write()?;
        let path = ObjectPath::from(key);
        let store = Arc::clone(&self.store);
        self.fenced(|| format!("PUT {key}"), async move {
            Ok(store.put_opts(&path, payload, mode.into()).await)
        })
        .await
    }

    /// Write an immutable object with `If-None-Match: *`. One already there
    /// is accepted only when its CRC is `crc32` (a retried upload that had
    /// landed); any other content means another writer.
    async fn put_immutable(
        &mut self,
        key: &str,
        bytes: Vec<u8>,
        crc32: &str,
    ) -> Result<(), BackupError> {
        let payload = PutPayload::from(bytes);
        match self.put_mode(key, payload.clone(), PutMode::Create).await? {
            Ok(_) => Ok(()),
            Err(object_store::Error::NotImplemented) => {
                self.put_mode(key, payload, PutMode::Overwrite).await??;
                Ok(())
            }
            Err(object_store::Error::AlreadyExists { .. }) => match self.get(key).await? {
                Some(b) if crc32_hex(&b) == crc32 => Ok(()),
                _ => Err(self.conflict(format!("{key} already exists with other content"))),
            },
            Err(e) => Err(e.into()),
        }
    }

    /// Write a manifest: `If-Match` against the version this tenure last
    /// wrote at `key`; otherwise `If-None-Match: *`, and over an existing one
    /// only when it is this lineage's (a previous run's restore point at the
    /// same commit).
    async fn put_manifest(&mut self, key: &str, bytes: Vec<u8>) -> Result<(), BackupError> {
        let payload = PutPayload::from(bytes);
        let mode = match self.manifest_versions.get(key) {
            Some(v) => PutMode::Update(v.clone()),
            None => PutMode::Create,
        };
        let written = match self.put_mode(key, payload.clone(), mode).await? {
            Err(object_store::Error::AlreadyExists { .. }) => {
                let existing = self.get_versioned(key).await?;
                let ours = existing.as_ref().is_some_and(|(b, _)| {
                    Manifest::from_json(b).is_ok_and(|m| m.lineage == self.lineage())
                });
                match existing {
                    Some((_, v)) if ours => {
                        self.put_mode(key, payload.clone(), PutMode::Update(v))
                            .await?
                    }
                    _ => return Err(self.conflict(format!("{key} is another lineage's manifest"))),
                }
            }
            other => other,
        };
        let r = match written {
            Ok(r) => r,
            Err(object_store::Error::NotImplemented) => {
                self.put_mode(key, payload, PutMode::Overwrite).await??
            }
            Err(object_store::Error::Precondition { .. })
            | Err(object_store::Error::AlreadyExists { .. }) => {
                return Err(self.conflict(format!("{key} changed since this worker wrote it")))
            }
            Err(e) => return Err(e.into()),
        };
        self.manifest_versions
            .insert(key.to_string(), version_of(r));
        Ok(())
    }

    /// Point `LATEST` at `target`, with `If-Match` against the version last
    /// written or read (`If-None-Match: *` when there was none). A refusal
    /// means another writer moved it.
    async fn put_latest(&mut self, target: &str) -> Result<(), BackupError> {
        let key = latest_key(&self.prefix);
        let payload = PutPayload::from(target.as_bytes().to_vec());
        let mode = match &self.latest_version {
            Some(v) => PutMode::Update(v.clone()),
            None => PutMode::Create,
        };
        let r = match self.put_mode(&key, payload.clone(), mode).await? {
            Ok(r) => r,
            Err(object_store::Error::NotImplemented) => {
                self.put_mode(&key, payload, PutMode::Overwrite).await??
            }
            Err(object_store::Error::Precondition { .. })
            | Err(object_store::Error::AlreadyExists { .. })
            | Err(object_store::Error::NotFound { .. }) => {
                return Err(self.conflict("LATEST changed since this worker last wrote it".into()))
            }
            Err(e) => return Err(e.into()),
        };
        self.latest_version = Some(version_of(r));
        self.latest_target = Some(target.to_string());
        Ok(())
    }

    /// The bucket refused a write because another writer got there first.
    /// Drop the lease: the worker starts over through it, and resume reads
    /// the bucket afresh.
    fn conflict(&mut self, what: String) -> BackupError {
        let why = format!("another writer is using this backup prefix: {what}");
        tracing::warn!(reason = %why, "backup write refused by the bucket");
        if let Some(l) = &self.lease {
            l.mark_lost();
            self.health.lock().lease = Some(why.clone());
        }
        BackupError::Conflict(why)
    }

    /// Stream `file` to `key`, multipart when large; return its entry, with
    /// its frame count when `count_frames`. Each chunk, and the final commit,
    /// is under the operation deadline and the lease's fence.
    ///
    /// A failure while chunks are being written aborts the upload once. A
    /// failure of the final commit drops it instead: the writer has already
    /// been shut down, and an abort then is not allowed (it panics). An
    /// unfinished multipart upload left that way is the bucket's to expire.
    async fn put_file(
        &mut self,
        name: &str,
        key: &str,
        file: std::fs::File,
        count_frames: bool,
    ) -> Result<FileEntry, BackupError> {
        // A multipart upload cannot be conditional here: check the key is
        // free first. Every snapshot and archive key carries a generation
        // this worker allocated, so one already there is another writer's.
        if self.head(key).await? {
            return Err(self.conflict(format!("{key} already exists")));
        }
        self.spend_write()?;
        let mut file = tokio::fs::File::from_std(file);
        let mut writer = BufWriter::new(Arc::clone(&self.store), ObjectPath::from(key));
        let mut hasher = crc32fast::Hasher::new();
        let mut counter = FrameCounter::default();
        let mut size = 0u64;
        let mut buf = vec![0u8; UPLOAD_CHUNK];
        let timeout = self.op_timeout();
        let lease = self.lease.clone();
        let written: Result<(), BackupError> = async {
            loop {
                let n = file.read(&mut buf).await?;
                if n == 0 {
                    return Ok(());
                }
                hasher.update(&buf[..n]);
                if count_frames {
                    counter.feed(&buf[..n]);
                }
                size += n as u64;
                fence(lease.as_deref(), async {
                    tokio::time::timeout(timeout, writer.write_all(&buf[..n]))
                        .await
                        .map_err(|_| BackupError::Timeout(format!("upload of {key}")))??;
                    Ok(())
                })
                .await?;
            }
        }
        .await;
        if let Err(e) = written {
            let _ = tokio::time::timeout(timeout, writer.abort()).await;
            return Err(e);
        }
        fence(lease.as_deref(), async {
            tokio::time::timeout(timeout, writer.shutdown())
                .await
                .map_err(|_| BackupError::Timeout(format!("upload of {key}")))??;
            Ok(())
        })
        .await?;
        Ok(FileEntry {
            name: name.to_string(),
            key: key.to_string(),
            size,
            crc32: format_crc32(hasher.finalize()),
            frames: counter.frames(),
            lineage: self.lineage(),
        })
    }

    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, BackupError> {
        let path = ObjectPath::from(key);
        let store = Arc::clone(&self.store);
        self.deadline(|| format!("GET {key}"), async move {
            match store.get(&path).await {
                Ok(r) => Ok(Some(r.bytes().await?.to_vec())),
                Err(object_store::Error::NotFound { .. }) => Ok(None),
                Err(e) => Err(e.into()),
            }
        })
        .await
    }

    async fn get_versioned(
        &self,
        key: &str,
    ) -> Result<Option<(Vec<u8>, UpdateVersion)>, BackupError> {
        let path = ObjectPath::from(key);
        let store = Arc::clone(&self.store);
        self.deadline(|| format!("GET {key}"), async move {
            match store.get(&path).await {
                Ok(r) => {
                    let v = UpdateVersion {
                        e_tag: r.meta.e_tag.clone(),
                        version: r.meta.version.clone(),
                    };
                    Ok(Some((r.bytes().await?.to_vec(), v)))
                }
                Err(object_store::Error::NotFound { .. }) => Ok(None),
                Err(e) => Err(e.into()),
            }
        })
        .await
    }

    async fn head(&self, key: &str) -> Result<bool, BackupError> {
        let path = ObjectPath::from(key);
        let store = Arc::clone(&self.store);
        self.deadline(|| format!("HEAD {key}"), async move {
            match store.head(&path).await {
                Ok(_) => Ok(true),
                Err(object_store::Error::NotFound { .. }) => Ok(false),
                Err(e) => Err(e.into()),
            }
        })
        .await
    }

    async fn delete(&mut self, key: &str) -> Result<(), BackupError> {
        self.spend_write()?;
        let path = ObjectPath::from(key);
        let store = Arc::clone(&self.store);
        self.fenced(|| format!("DELETE {key}"), async move {
            match store.delete(&path).await {
                Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
                Err(e) => Err(e.into()),
            }
        })
        .await
    }

    async fn list(&mut self, dir: &ObjectPath) -> Result<object_store::ListResult, BackupError> {
        self.lists += 1;
        let store = Arc::clone(&self.store);
        let d = dir.clone();
        self.deadline(|| format!("LIST {dir}"), async move {
            Ok(store.list_with_delimiter(Some(&d)).await?)
        })
        .await
    }
}

/// The engine's durable position, read on the blocking pool: the first read
/// after the handle applied WAL bytes it did not write fsyncs them.
async fn position_of(source: &Arc<dyn WalSource>) -> WalPosition {
    let s = Arc::clone(source);
    match tokio::task::spawn_blocking(move || s.durable_wal_position()).await {
        Ok(p) => p,
        // The read itself cannot fail; a panicked blocking task falls back to
        // reading here.
        Err(_) => source.durable_wal_position(),
    }
}

/// Run `fut` unless a shutdown arrives first; `None` if it did.
async fn until_shutdown<F: Future>(
    rx: &mut watch::Receiver<BackupTrigger>,
    fut: F,
) -> Option<F::Output> {
    tokio::pin!(fut);
    loop {
        tokio::select! {
            out = &mut fut => return Some(out),
            changed = rx.changed() => {
                if is_shutdown(changed, rx) {
                    return None;
                }
            }
        }
    }
}

/// How often a worker kept out by a foreign lease looks again.
fn poll_interval(ttl_s: u64) -> Duration {
    Duration::from_millis(ttl_s.max(1) * 1000 / 3)
}

/// Run a write while `lease` allows writes, and cancel it the moment it
/// stops: lost, or not renewed in time. The lease is checked before every
/// poll of the write, so a process resumed after a freeze is fenced before
/// the write makes progress, not after. No lease: run it as it is.
async fn fence<T>(
    lease: Option<&BucketLease>,
    fut: impl Future<Output = Result<T, BackupError>>,
) -> Result<T, BackupError> {
    let Some(lease) = lease else {
        return fut.await;
    };
    let fenced = || {
        Err(BackupError::Lease(if lease.is_lost() {
            "lost to another instance; this one no longer writes to the bucket".into()
        } else {
            "not renewed in time; writes are stopped until it is".into()
        }))
    };
    tokio::pin!(fut);
    let mut wake: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;
    std::future::poll_fn(|cx| {
        let Some(deadline) = lease.write_deadline() else {
            return std::task::Poll::Ready(fenced());
        };
        if let std::task::Poll::Ready(out) = fut.as_mut().poll(cx) {
            return std::task::Poll::Ready(out);
        }
        // Look again at the deadline, and at least every second: the wall
        // clock can jump.
        let at = deadline.min(Instant::now() + Duration::from_secs(1));
        let sleep = wake.get_or_insert_with(|| Box::pin(tokio::time::sleep_until(at)));
        if sleep.deadline() != at {
            sleep.as_mut().reset(at);
        }
        if sleep.as_mut().poll(cx).is_ready() {
            sleep
                .as_mut()
                .reset(Instant::now() + Duration::from_millis(1));
            cx.waker().wake_by_ref();
        }
        std::task::Poll::Pending
    })
    .await
}

/// Renew `lease` every third of its TTL, and a second after a failure. Stops
/// when the lease is found lost; health says why the worker is kept out.
async fn renew_loop(
    lease: Arc<BucketLease>,
    op_timeout: Duration,
    health: Health,
    errors: Arc<AtomicU64>,
) {
    let every = lease.renew_interval().max(Duration::from_millis(1));
    let mut wait = every;
    loop {
        tokio::time::sleep(wait).await;
        let why = match tokio::time::timeout(op_timeout, lease.renew()).await {
            Ok(Ok(())) => {
                health.lock().lease = None;
                wait = every;
                continue;
            }
            Ok(Err(e @ LeaseError::Lost { .. })) => {
                tracing::warn!(error = %e, "lost the backup bucket lease; uploads stop");
                health.lock().lease = Some(format!(
                    "{e}; this instance uploads nothing while another holds it"
                ));
                return;
            }
            Ok(Err(e)) => e.to_string(),
            Err(_) => "the renewal timed out".to_string(),
        };
        errors.fetch_add(1, Ordering::AcqRel);
        tracing::warn!(error = %why, "backup bucket lease renewal failed; retrying");
        if !lease.may_write() {
            health.lock().lease = Some(format!(
                "the bucket lease could not be renewed ({why}); uploads are stopped until it is"
            ));
        }
        wait = RENEW_RETRY.min(every);
    }
}

fn version_of(r: PutResult) -> UpdateVersion {
    UpdateVersion {
        e_tag: r.e_tag,
        version: r.version,
    }
}

fn is_shutdown(
    changed: Result<(), watch::error::RecvError>,
    rx: &mut watch::Receiver<BackupTrigger>,
) -> bool {
    match changed {
        // The sender is gone: nobody can ask for a shutdown any more, so this
        // is it.
        Err(_) => true,
        Ok(()) => *rx.borrow_and_update() == BackupTrigger::Shutdown,
    }
}

/// Run a file-system call on Tokio's blocking pool.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> std::io::Result<T> + Send + 'static,
) -> std::io::Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .unwrap_or_else(|e| Err(std::io::Error::other(format!("blocking read failed: {e}"))))
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}
