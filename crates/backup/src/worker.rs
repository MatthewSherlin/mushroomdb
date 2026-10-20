//! The backup task: ships WAL segments, snapshots, archives and side files, and
//! writes the manifests that name consistent restore points.
//!
//! # What it reads, and what it never touches
//!
//! It reads the store's files through ordinary OS reads on Tokio's blocking
//! pool, and one value from the engine: [`WalSource::durable_wal_position`],
//! which says how much of `wal.bin` is fsynced, in which generation, and which
//! `snapshot.bin` the engine's state is based on. Reading it takes no engine
//! lock, so it cannot slow a write. The one exception: when the files on disk
//! show a snapshot or commits the engine has not seen (another process wrote
//! them), the worker asks the handle to catch up — the same metadata check and
//! refresh every read makes. It never writes to the store directory.
//!
//! # Consistency
//!
//! A restore point is a snapshot plus the WAL that starts beside it. Which
//! snapshot and which WAL that is comes from the files on disk, not from the
//! handle's memory: a restore point is built only when the snapshot on disk is
//! the one the engine's position names, and a new generation starts whenever
//! the snapshot on disk changes or the engine replaces `wal.bin`. A snapshot
//! that kept its WAL carries a mark of the prefix it holds, and the open of a
//! restored store skips that prefix by itself, so shipping the whole WAL from
//! offset 0 beside it is always correct.
//!
//! The pair is checked on both sides of opening `snapshot.bin`, WAL bytes are
//! kept only when neither the engine's generation nor the snapshot on disk
//! changed across the read, and anything else is retried on the next tick.
//!
//! # Objects and ordering
//!
//! Every object a manifest names is immutable (see [`crate::manifest`]); a
//! manifest is written only after every object it names exists, and `LATEST`
//! only after its manifest. Retention deletes only what no retained manifest
//! names. So at any point — a crash between any two requests included — every
//! manifest in the bucket verifies, and `LATEST` names one of them.

use crate::disk::{self, DiskState, Ident};
use crate::manifest::{
    archive_key, crc32_hex, format_crc32, latest_key, manifest_key, object_key, parse_archive_name,
    parse_segment_key, parse_versioned_generation, segment_key, side_key, snapshot_key, FileEntry,
    Manifest, SegmentEntry, MANIFEST_VERSION,
};
use crate::segment::{complete_frames, read_new_segment};
use crate::store::BackupConfig;
use crate::BackupError;
use mushroomdb::{SharedDb, WalPosition};
use object_store::buffered::BufWriter;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, PutPayload};
use std::collections::{BTreeMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// The store's small files that travel with every restore point.
///
/// `snapshot.intent` is deliberately absent: it describes one machine's
/// half-finished snapshot and must never be restored onto another (W4 §4).
pub const SIDE_FILES: [&str; 5] = [
    "commit_times.bin",
    "roles.json",
    "keys.json",
    "wal.genesis",
    "wal.floor",
];

/// Consecutive failed steps after which the worker reports itself unhealthy.
pub const UNHEALTHY_AFTER_FAILURES: u32 = 3;
/// First retry delay after a failure; doubles per failure up to [`MAX_BACKOFF`].
pub const FIRST_BACKOFF: Duration = Duration::from_secs(1);
/// Longest retry delay.
pub const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// The shortest lag that makes the worker unhealthy: durable data unshipped
/// for longer than `max(MIN_UNHEALTHY_LAG, 10 × wal_interval)`.
pub const MIN_UNHEALTHY_LAG: Duration = Duration::from_secs(30);
/// Read size when streaming a snapshot or archive.
const UPLOAD_CHUNK: usize = 8 * 1024 * 1024;

/// Where the worker learns how much of the WAL is durable.
///
/// [`SharedDb`] is the real one. The trait exists so the worker does not care
/// how the handle is held.
pub trait WalSource: Send + Sync + 'static {
    fn durable_wal_position(&self) -> WalPosition;

    /// Absorb whatever another process committed or snapshotted. Called, on a
    /// blocking thread, only when the files on disk are ahead of the position.
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
    /// Unix seconds of the last step that left nothing durable unshipped; 0
    /// before the first.
    pub last_success_ts: Arc<AtomicU64>,
    /// False after [`UNHEALTHY_AFTER_FAILURES`] consecutive failures, or while
    /// durable data has stayed unshipped for longer than
    /// `max(30 s, 10 × wal_interval)`; true again once both clear.
    pub healthy: Arc<AtomicBool>,
    /// `backup_lag_s`: seconds the oldest durable, unshipped WAL byte has
    /// waited; 0 when everything durable is shipped.
    pub lag_s: Arc<AtomicU64>,
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
}

/// The WAL generation being shipped.
#[derive(Debug, Clone)]
struct Generation {
    /// Its number in the bucket (`wal_generation`).
    number: u64,
    /// The engine's generation it ships.
    engine: u64,
    /// The `snapshot.bin` it starts beside.
    snapshot: Option<Ident>,
    /// Bytes of `wal.bin` shipped so far.
    cursor: u64,
    /// Frames in those bytes.
    frames_shipped: u64,
    /// Global frame index of the WAL's first frame, once a position has shown
    /// it (`frames` at a `len` the cursor reached).
    base_frames: Option<u64>,
    segments: Vec<SegmentEntry>,
}

impl Generation {
    fn commit_seq(&self) -> Option<u64> {
        self.base_frames.map(|b| b + self.frames_shipped)
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
    last_success_ts: Arc<AtomicU64>,
    healthy: Arc<AtomicBool>,
    lag_s: Arc<AtomicU64>,
    consecutive_failures: u32,
    backoff: Duration,
    /// Since when durable WAL bytes have been waiting to ship; `None` when
    /// none are.
    unshipped_since: Option<Instant>,
    /// Whether the bucket's previous state has been read.
    resumed: bool,
    next_generation: u64,
    current: Option<Generation>,
    /// The snapshot the current generation starts beside.
    snapshot: Option<FileEntry>,
    /// Archives in the bucket, by number, with the local length they were
    /// uploaded from. An archive is immutable: never uploaded twice.
    archives: BTreeMap<u64, (u64, FileEntry)>,
    /// Archives the current restore point names.
    base_archives: Vec<u64>,
    /// Side files uploaded, by name, with the local identity they came from.
    side: BTreeMap<&'static str, (Ident, FileEntry)>,
    manifest_dirty: bool,
    last_manifest_at: Option<Instant>,
    /// Test hook: requests that change the bucket still allowed before the
    /// worker behaves as if killed. See [`BackupWorker::fail_after_writes`].
    write_budget: Option<u64>,
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
        Ok(Self {
            config,
            store,
            source,
            dir: store_dir,
            prefix,
            error_count: Arc::new(AtomicU64::new(0)),
            last_success_ts: Arc::new(AtomicU64::new(0)),
            healthy: Arc::new(AtomicBool::new(true)),
            lag_s: Arc::new(AtomicU64::new(0)),
            consecutive_failures: 0,
            backoff: FIRST_BACKOFF,
            unshipped_since: None,
            resumed: false,
            next_generation: 0,
            current: None,
            snapshot: None,
            archives: BTreeMap::new(),
            base_archives: Vec::new(),
            side: BTreeMap::new(),
            manifest_dirty: false,
            last_manifest_at: None,
            write_budget: None,
        })
    }

    /// Start the worker as a Tokio task. It runs one step at once — reading
    /// the bucket's previous state and uploading a base restore point — then
    /// one every `wal_interval_ms`, and one immediately on each trigger.
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
        let worker = Self::new(config, store, source, store_dir)?;
        let BackupCounters {
            error_count,
            last_success_ts,
            healthy,
            lag_s,
        } = worker.counters();
        let join = tokio::spawn(worker.run(snapshot_rx));
        Ok(BackupHandle {
            error_count,
            last_success_ts,
            healthy,
            lag_s,
            join,
        })
    }

    /// The counters this worker updates (the same ones its
    /// [`BackupHandle`] carries).
    pub fn counters(&self) -> BackupCounters {
        BackupCounters {
            error_count: Arc::clone(&self.error_count),
            last_success_ts: Arc::clone(&self.last_success_ts),
            healthy: Arc::clone(&self.healthy),
            lag_s: Arc::clone(&self.lag_s),
        }
    }

    /// Test hook: allow `n` more requests that change the bucket (PUTs and
    /// DELETEs), then fail every one as if the process had been killed just
    /// before it. Drop the worker after the first failure to model a kill
    /// between two requests.
    #[doc(hidden)]
    pub fn fail_after_writes(&mut self, n: u64) {
        self.write_budget = Some(n);
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
            // A step blocked on an unresponsive bucket must not hold up a
            // shutdown: the step is dropped — its in-flight request abandoned
            // — the moment one arrives.
            let stepped = {
                let step = self.step();
                tokio::pin!(step);
                loop {
                    tokio::select! {
                        d = &mut step => break Some(d),
                        changed = rx.changed() => {
                            if is_shutdown(changed, &mut rx) {
                                break None;
                            }
                        }
                    }
                }
            };
            match stepped {
                Some(d) => delay = d,
                None => break,
            }
        }
        let grace = Duration::from_millis(self.config.shutdown_grace_ms);
        if tokio::time::timeout(grace, self.flush()).await.is_err() {
            self.error_count.fetch_add(1, Ordering::AcqRel);
            tracing::warn!(
                grace_ms = self.config.shutdown_grace_ms,
                "backup flush did not finish within the shutdown grace; abandoned"
            );
        }
    }

    /// One step, with its outcome recorded in the counters. Returns how long
    /// to wait before the next one: the WAL interval after a success, the
    /// back-off after a failure.
    pub async fn step(&mut self) -> Duration {
        let result = self.run_once().await;
        let delay = match result {
            Ok(()) => {
                self.consecutive_failures = 0;
                self.backoff = FIRST_BACKOFF;
                Duration::from_millis(self.config.wal_interval_ms)
            }
            Err(e) => {
                self.error_count.fetch_add(1, Ordering::AcqRel);
                self.consecutive_failures += 1;
                let delay = self.backoff;
                self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
                tracing::warn!(
                    error = %e,
                    consecutive_failures = self.consecutive_failures,
                    retry_in_s = delay.as_secs(),
                    "backup step failed"
                );
                delay
            }
        };
        self.update_health().await;
        delay
    }

    /// Recompute lag and health from what is durable now and what has
    /// shipped.
    async fn update_health(&mut self) {
        let pos = self.position().await;
        let caught_up = self.current.as_ref().is_some_and(|g| {
            g.engine == pos.generation && g.snapshot == pos.snapshot && g.cursor >= pos.len
        });
        if caught_up {
            self.unshipped_since = None;
            self.last_success_ts.store(unix_now(), Ordering::Release);
        } else if self.unshipped_since.is_none() {
            self.unshipped_since = Some(Instant::now());
        }
        let lag = self.unshipped_since.map_or(Duration::ZERO, |t| t.elapsed());
        self.lag_s.store(lag.as_secs(), Ordering::Release);
        let limit = MIN_UNHEALTHY_LAG.max(Duration::from_millis(self.config.wal_interval_ms) * 10);
        let healthy = self.consecutive_failures < UNHEALTHY_AFTER_FAILURES && lag <= limit;
        self.healthy.store(healthy, Ordering::Release);
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
                    self.update_health().await;
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

    /// One step: read the bucket's previous state if not done yet; upload a
    /// new restore point when the snapshot on disk or the engine's WAL
    /// generation changed; ship the WAL bytes fsynced since the last step;
    /// write a manifest when one is due.
    pub async fn run_once(&mut self) -> Result<(), BackupError> {
        if !self.resumed {
            self.resume().await?;
        }
        let mut disk = self.observe().await?;
        let mut pos = self.position().await;
        // The disk is ahead of the handle: another process snapshotted, or
        // committed. Let the handle catch up before trusting its offsets.
        if pos.snapshot != disk.snapshot || disk.wal_len > pos.len {
            self.catch_up().await?;
            disk = self.observe().await?;
            pos = self.position().await;
        }
        if pos.snapshot_in_progress || pos.snapshot != disk.snapshot {
            // Between states, or still behind; next step.
            return Ok(());
        }
        let current = self.current.as_ref().map(|g| (g.engine, g.snapshot));
        if current != Some((pos.generation, disk.snapshot)) {
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

    /// The engine's durable position, read on the blocking pool: the first
    /// read after the handle applied WAL bytes it did not write fsyncs them.
    async fn position(&self) -> WalPosition {
        let source = Arc::clone(&self.source);
        match tokio::task::spawn_blocking(move || source.durable_wal_position()).await {
            Ok(p) => p,
            // The read itself cannot fail; a panicked blocking task falls back
            // to reading here.
            Err(_) => self.source.durable_wal_position(),
        }
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
    /// names.
    async fn resume(&mut self) -> Result<(), BackupError> {
        let mut highest: Option<u64> = None;
        let wal_dir = ObjectPath::from(object_key(&self.prefix, "wal"));
        for p in self.list(&wal_dir).await?.common_prefixes {
            if let Some(g) = p.filename().and_then(|s| s.parse::<u64>().ok()) {
                highest = highest.max(Some(g));
            }
        }
        for dir in ["snapshots", "archives"] {
            let path = ObjectPath::from(object_key(&self.prefix, dir));
            for o in self.list(&path).await?.objects {
                let g = parse_versioned_generation(&self.prefix, o.location.as_ref());
                highest = highest.max(g);
            }
        }
        if let Some(m) = self.latest_manifest().await? {
            highest = highest.max(Some(m.wal_generation));
            for f in &m.files {
                if let Some(n) = parse_archive_name(&f.name) {
                    self.archives.insert(n, (f.size, f.clone()));
                }
            }
        }
        self.next_generation = highest.map_or(0, |g| g + 1);
        self.resumed = true;
        Ok(())
    }

    async fn latest_manifest(&mut self) -> Result<Option<Manifest>, BackupError> {
        let key = match self.get(&latest_key(&self.prefix)).await? {
            Some(b) => String::from_utf8(b)
                .map_err(|_| BackupError::Manifest("LATEST is not UTF-8".into()))?,
            None => return Ok(None),
        };
        let bytes = self.get(key.trim()).await?.ok_or_else(|| {
            BackupError::Manifest(format!("LATEST names {key}, which does not exist"))
        })?;
        Manifest::from_json(&bytes)
            .map(Some)
            .map_err(BackupError::Manifest)
    }

    // ── Restore points ──────────────────────────────────────────────────────

    /// Start a generation beside the snapshot on disk: upload the snapshot,
    /// new archives and changed side files under keys of their own, ship the
    /// generation's durable WAL bytes from offset 0, then write the manifest.
    async fn upload_base(&mut self, pos: WalPosition, disk: DiskState) -> Result<(), BackupError> {
        if disk.intent_nonempty {
            // A snapshot that started since `pos` was read writes the intent
            // first; that is not a failure, just a step to retry.
            if self.position().await.snapshot_in_progress {
                return Ok(());
            }
            // A snapshot on this machine stopped between writing snapshot.bin
            // and replacing wal.bin. The pair on disk is only correct with the
            // intent, which never leaves the machine, so wait for the next
            // snapshot to settle it.
            return Err(BackupError::Inconsistent(
                "snapshot.intent is not empty: the last snapshot did not finish; \
                 waiting for the next one before uploading a restore point"
                    .into(),
            ));
        }
        let Some(frames_now) = pos.frames else {
            return Ok(());
        };
        let dir = self.dir.clone();
        let opened = blocking(move || disk::open_snapshot(&dir)).await?;
        if opened.as_ref().map(|(_, ident)| *ident) != disk.snapshot {
            return Ok(());
        }
        let after = self.position().await;
        if after.generation != pos.generation
            || after.snapshot_in_progress
            || after.snapshot != disk.snapshot
        {
            // A snapshot ran or started while the file was opened: the file
            // may not go with this WAL. Next step.
            return Ok(());
        }
        // Burn the number now: objects a failed attempt leaves behind belong
        // to a generation no manifest will ever name.
        let number = self.next_generation;
        self.next_generation += 1;
        let snapshot = match opened {
            Some((file, _)) => {
                let key = snapshot_key(&self.prefix, number, frames_now);
                Some(self.put_file("snapshot.bin", &key, file).await?)
            }
            None => None,
        };
        let archives = self.upload_new_archives(number).await?;
        self.sync_side_files().await?;
        self.snapshot = snapshot;
        self.base_archives = archives;
        self.current = Some(Generation {
            number,
            engine: pos.generation,
            snapshot: disk.snapshot,
            cursor: 0,
            frames_shipped: 0,
            base_frames: None,
            segments: Vec::new(),
        });
        self.manifest_dirty = true;
        self.ship_segments().await?;
        self.write_manifest().await
    }

    /// Upload every local archive not yet in the bucket; return the numbers of
    /// all local archives.
    async fn upload_new_archives(&mut self, generation: u64) -> Result<Vec<u64>, BackupError> {
        let dir = self.dir.clone();
        let local = blocking(move || disk::local_archives(&dir)).await?;
        let mut present = Vec::new();
        for (n, len) in local {
            if self.archives.get(&n).is_none_or(|(l, _)| *l != len) {
                let path = self.dir.join(format!("wal.{n}.archive"));
                let file = match blocking(move || std::fs::File::open(path)).await {
                    Ok(f) => f,
                    // Pruned by a snapshot since the listing.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e.into()),
                };
                let name = format!("wal.{n}.archive");
                let key = archive_key(&self.prefix, generation, n);
                let entry = self.put_file(&name, &key, file).await?;
                self.archives.insert(n, (entry.size, entry));
            }
            present.push(n);
        }
        Ok(present)
    }

    /// Upload each side file whose local copy changed since it was last
    /// uploaded, under a key addressed by its content; forget one that no
    /// longer exists locally.
    async fn sync_side_files(&mut self) -> Result<(), BackupError> {
        for name in SIDE_FILES {
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
            let key = side_key(&self.prefix, name, &crc32, size);
            if self.side.get(name).is_none_or(|(_, e)| e.key != key) {
                self.put(&key, bytes).await?;
                self.manifest_dirty = true;
            }
            let entry = FileEntry {
                name: name.to_string(),
                key,
                size,
                crc32,
            };
            self.side.insert(name, (ident, entry));
        }
        Ok(())
    }

    // ── WAL segments ────────────────────────────────────────────────────────

    /// Ship every fsynced WAL byte of the current generation not shipped yet.
    async fn ship_segments(&mut self) -> Result<(), BackupError> {
        loop {
            let Some(g) = self.current.as_ref() else {
                return Ok(());
            };
            let (engine, snap, cursor, number) = (g.engine, g.snapshot, g.cursor, g.number);
            let pos = self.position().await;
            if pos.generation != engine || pos.snapshot != snap {
                return Ok(());
            }
            if pos.len <= cursor {
                if pos.len == cursor {
                    self.note_base_frames(pos);
                }
                return Ok(());
            }
            let wal = self.dir.join("wal.bin");
            let len = pos.len;
            let read = blocking(move || read_new_segment(&wal, cursor, len)).await?;
            // wal.bin may have been replaced during the read — by this
            // handle's snapshot (its generation moves) or another process's
            // (the snapshot on disk moves). The bytes may then be the new
            // file's: drop them, and let the next step start the new
            // generation.
            let disk = self.observe().await?;
            if self.position().await.generation != pos.generation || disk.snapshot != snap {
                return Ok(());
            }
            let Some((start, end, bytes)) = read else {
                return Ok(());
            };
            let (_, frames) = complete_frames(&bytes);
            let key = segment_key(&self.prefix, number, start, end);
            let crc32 = crc32_hex(&bytes);
            self.put(&key, bytes).await?;
            if let Some(g) = self.current.as_mut() {
                g.cursor = end;
                g.frames_shipped += frames;
                g.segments.push(SegmentEntry { key, crc32 });
            }
            if end == pos.len {
                self.note_base_frames(pos);
            }
            self.manifest_dirty = true;
        }
    }

    /// The cursor stands at `pos.len`: learn the generation's first frame
    /// index from it.
    fn note_base_frames(&mut self, pos: WalPosition) {
        if let (Some(g), Some(frames)) = (self.current.as_mut(), pos.frames) {
            if g.base_frames.is_none() {
                g.base_frames = frames.checked_sub(g.frames_shipped);
            }
        }
    }

    // ── Manifests and retention ─────────────────────────────────────────────

    /// Write a manifest for the current restore point and point `LATEST` at
    /// it, then apply retention. Nothing is written until the generation's
    /// first frame index is known, so `commit_seq` always matches what the
    /// files and segments hold.
    async fn write_manifest(&mut self) -> Result<(), BackupError> {
        self.sync_side_files().await?;
        let Some(generation) = self.current.as_ref() else {
            return Ok(());
        };
        let Some(commit_seq) = generation.commit_seq() else {
            return Ok(());
        };
        let mut files = Vec::new();
        files.extend(self.snapshot.clone());
        for n in &self.base_archives {
            if let Some((_, e)) = self.archives.get(n) {
                files.push(e.clone());
            }
        }
        files.extend(self.side.values().map(|(_, e)| e.clone()));
        let dir = self.dir.clone();
        let oldest_commit_seq = blocking(move || Ok(disk::floor(&dir))).await?;
        let manifest = Manifest {
            version: MANIFEST_VERSION,
            created_at: now_rfc3339(),
            commit_seq,
            oldest_commit_seq,
            files,
            wal_generation: generation.number,
            segments: generation.segments.clone(),
        };
        let key = manifest_key(&self.prefix, manifest.commit_seq);
        self.put(&key, manifest.to_json()).await?;
        self.put(&latest_key(&self.prefix), key.into_bytes())
            .await?;
        self.manifest_dirty = false;
        self.last_manifest_at = Some(Instant::now());
        if self.config.retain_n > 0 {
            self.apply_retention().await?;
        }
        Ok(())
    }

    /// Keep the newest `retain_n` manifests, then delete every snapshot,
    /// archive, side file and segment no kept manifest names — except the
    /// current generation's segments, which the next manifest will. `LATEST`
    /// and anything outside those four directories is never touched.
    ///
    /// Conservative on doubt: when a kept manifest cannot be read, nothing but
    /// the surplus manifests is deleted.
    async fn apply_retention(&mut self) -> Result<(), BackupError> {
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
        if keys.len() <= self.config.retain_n {
            return Ok(());
        }
        let surplus = keys.len() - self.config.retain_n;
        for k in &keys[..surplus] {
            self.delete(k).await?;
        }
        let mut referenced: HashSet<String> = HashSet::new();
        for k in &keys[surplus..] {
            let Some(bytes) = self.get(k).await? else {
                return Ok(());
            };
            let Ok(m) = Manifest::from_json(&bytes) else {
                return Ok(());
            };
            referenced.extend(m.files.into_iter().map(|f| f.key));
            referenced.extend(m.segments.into_iter().map(|s| s.key));
        }
        let current_number = self.current.as_ref().map(|g| g.number);
        let mut doomed = Vec::new();
        for top in ["snapshots", "archives", "side", "wal"] {
            for k in self.list_all(&object_key(&self.prefix, top)).await? {
                if referenced.contains(&k) {
                    continue;
                }
                if let Some((g, _, _)) = parse_segment_key(&self.prefix, &k) {
                    if Some(g) == current_number || current_number.is_some_and(|c| g > c) {
                        continue;
                    }
                }
                doomed.push(k);
            }
        }
        for k in doomed {
            self.delete(&k).await?;
        }
        let keep: HashSet<&String> = referenced.iter().collect();
        self.archives.retain(|_, (_, e)| keep.contains(&e.key));
        Ok(())
    }

    /// Every object key under `prefix`, recursively.
    async fn list_all(&self, prefix: &str) -> Result<Vec<String>, BackupError> {
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

    async fn put(&mut self, key: &str, bytes: Vec<u8>) -> Result<(), BackupError> {
        self.spend_write()?;
        let path = ObjectPath::from(key);
        let store = Arc::clone(&self.store);
        self.deadline(|| format!("PUT {key}"), async move {
            store.put(&path, PutPayload::from(bytes)).await?;
            Ok(())
        })
        .await
    }

    /// Stream `file` to `key`, multipart when large; return its entry. Each
    /// chunk, and the final commit, is under the operation deadline.
    async fn put_file(
        &mut self,
        name: &str,
        key: &str,
        file: std::fs::File,
    ) -> Result<FileEntry, BackupError> {
        self.spend_write()?;
        let mut file = tokio::fs::File::from_std(file);
        let mut writer = BufWriter::new(Arc::clone(&self.store), ObjectPath::from(key));
        let mut hasher = crc32fast::Hasher::new();
        let mut size = 0u64;
        let mut buf = vec![0u8; UPLOAD_CHUNK];
        let timeout = self.op_timeout();
        let streamed: Result<(), BackupError> = async {
            loop {
                let n = file.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                size += n as u64;
                tokio::time::timeout(timeout, writer.write_all(&buf[..n]))
                    .await
                    .map_err(|_| BackupError::Timeout(format!("upload of {key}")))??;
            }
            tokio::time::timeout(timeout, writer.shutdown())
                .await
                .map_err(|_| BackupError::Timeout(format!("upload of {key}")))??;
            Ok(())
        }
        .await;
        if let Err(e) = streamed {
            let _ = tokio::time::timeout(timeout, writer.abort()).await;
            return Err(e);
        }
        Ok(FileEntry {
            name: name.to_string(),
            key: key.to_string(),
            size,
            crc32: format_crc32(hasher.finalize()),
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

    async fn delete(&mut self, key: &str) -> Result<(), BackupError> {
        self.spend_write()?;
        let path = ObjectPath::from(key);
        let store = Arc::clone(&self.store);
        self.deadline(|| format!("DELETE {key}"), async move {
            match store.delete(&path).await {
                Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
                Err(e) => Err(e.into()),
            }
        })
        .await
    }

    async fn list(&self, dir: &ObjectPath) -> Result<object_store::ListResult, BackupError> {
        let store = Arc::clone(&self.store);
        let d = dir.clone();
        self.deadline(|| format!("LIST {dir}"), async move {
            Ok(store.list_with_delimiter(Some(&d)).await?)
        })
        .await
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
