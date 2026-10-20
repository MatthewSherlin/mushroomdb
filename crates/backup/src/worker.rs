//! The backup task: ships WAL segments, snapshots, archives and side files, and
//! writes the manifests that name consistent restore points.
//!
//! # What it reads, and what it never touches
//!
//! It reads the store's files through ordinary OS reads, and one value from the
//! engine: [`WalSource::durable_wal_position`], which says how much of
//! `wal.bin` is fsynced and which generation of it that is. It takes no engine
//! lock — not the RwLock, not the WAL mutex — so it cannot slow a write; it
//! never writes to the store directory.
//!
//! # Consistency
//!
//! A restore point is a snapshot plus the WAL generation that starts at it.
//! The engine bumps the generation each time it replaces `wal.bin`, and flags
//! the whole of a snapshot as in progress. The worker pairs `snapshot.bin`
//! with a generation only when the position reads the same, and not in
//! progress, on both sides of opening the file; it ships WAL bytes only when
//! the generation is unchanged across the read. Anything else is retried on
//! the next tick.

use crate::manifest::{
    archive_key, crc32_hex, format_crc32, latest_key, manifest_key, object_key, parse_archive_key,
    parse_segment_key, segment_key, FileEntry, Manifest, SegmentEntry, MANIFEST_VERSION,
};
use crate::segment::read_new_segment;
use crate::store::BackupConfig;
use crate::BackupError;
use mushroomdb::{SharedDb, WalPosition};
use object_store::buffered::BufWriter;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, PutPayload};
use std::collections::{BTreeMap, HashSet};
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
/// Read size when streaming a snapshot or archive.
const UPLOAD_CHUNK: usize = 8 * 1024 * 1024;

/// Where the worker learns how much of the WAL is durable.
///
/// [`SharedDb`] is the real one. The trait exists so the worker does not care
/// how the handle is held.
pub trait WalSource: Send + Sync + 'static {
    fn durable_wal_position(&self) -> WalPosition;
}

impl WalSource for SharedDb {
    fn durable_wal_position(&self) -> WalPosition {
        SharedDb::durable_wal_position(self)
    }
}

/// What the serve loop tells the worker, over a `watch` channel.
///
/// A `watch` keeps only the latest value, so a `Snapshot` followed quickly by
/// a `Shutdown` is seen as `Shutdown` alone. That is safe: the worker finds a
/// new snapshot by itself (the WAL generation changes), and its shutdown flush
/// uploads it. `Snapshot` only makes that happen now instead of at the next
/// tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackupTrigger {
    /// The channel's initial value; means nothing.
    #[default]
    Idle,
    /// A snapshot finished: upload it now.
    Snapshot,
    /// Flush what is pending, write a final manifest, and stop.
    Shutdown,
}

/// The running worker, as the serve loop holds it.
///
/// There is one way to stop it: send [`BackupTrigger::Shutdown`] (or drop the
/// sender), then await `join`.
#[derive(Debug)]
pub struct BackupHandle {
    /// Failed steps since start (`mushroomdb_backup_upload_errors_total`).
    pub error_count: Arc<AtomicU64>,
    /// Unix seconds of the last step that left nothing durable unshipped; 0
    /// before the first.
    pub last_success_ts: Arc<AtomicU64>,
    /// False after [`UNHEALTHY_AFTER_FAILURES`] consecutive failures, true
    /// again after the next success.
    pub healthy: Arc<AtomicBool>,
    /// Finishes when the worker has flushed and stopped.
    pub join: JoinHandle<()>,
}

/// The worker's counters, shared with whoever reports them.
#[derive(Debug, Clone)]
pub struct BackupCounters {
    pub error_count: Arc<AtomicU64>,
    pub last_success_ts: Arc<AtomicU64>,
    pub healthy: Arc<AtomicBool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdent {
    len: u64,
    modified: Option<SystemTime>,
}

impl FileIdent {
    fn of(meta: &std::fs::Metadata) -> Self {
        Self {
            len: meta.len(),
            modified: meta.modified().ok(),
        }
    }
}

/// The WAL generation being shipped.
#[derive(Debug, Clone)]
struct Generation {
    /// The engine's generation this one ships.
    engine: u64,
    /// Its number in the bucket (`wal_generation`).
    number: u64,
    /// Bytes of `wal.bin` shipped so far.
    cursor: u64,
    segments: Vec<SegmentEntry>,
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
    consecutive_failures: u32,
    backoff: Duration,
    /// Whether the bucket's previous state has been read.
    resumed: bool,
    next_generation: u64,
    current: Option<Generation>,
    /// Snapshot of the current restore point, with the local file identity it
    /// was uploaded from.
    snapshot: Option<(FileIdent, FileEntry)>,
    /// Archives already in the bucket. Archives are immutable: never re-uploaded.
    archives: BTreeMap<u64, FileEntry>,
    /// Archives the current restore point names.
    base_archives: Vec<u64>,
    /// Side files uploaded, by name.
    side: BTreeMap<&'static str, (FileIdent, FileEntry)>,
    commit_seq: u64,
    manifest_dirty: bool,
    last_manifest_at: Option<Instant>,
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
            consecutive_failures: 0,
            backoff: FIRST_BACKOFF,
            resumed: false,
            next_generation: 0,
            current: None,
            snapshot: None,
            archives: BTreeMap::new(),
            base_archives: Vec::new(),
            side: BTreeMap::new(),
            commit_seq: 0,
            manifest_dirty: false,
            last_manifest_at: None,
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
        } = worker.counters();
        let join = tokio::spawn(worker.run(snapshot_rx));
        Ok(BackupHandle {
            error_count,
            last_success_ts,
            healthy,
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
        }
    }

    async fn run(mut self, mut rx: watch::Receiver<BackupTrigger>) {
        let mut delay = Duration::ZERO;
        loop {
            let shutdown = tokio::select! {
                _ = tokio::time::sleep(delay) => false,
                changed = rx.changed() => match changed {
                    // The sender is gone: nobody can ask for a shutdown any
                    // more, so this is it.
                    Err(_) => true,
                    Ok(()) => *rx.borrow_and_update() == BackupTrigger::Shutdown,
                },
            };
            if shutdown {
                self.flush().await;
                return;
            }
            delay = self.step().await;
        }
    }

    /// One step, with its outcome recorded in the counters. Returns how long
    /// to wait before the next one: the WAL interval after a success, the
    /// back-off after a failure.
    pub async fn step(&mut self) -> Duration {
        match self.run_once().await {
            Ok(()) => {
                self.consecutive_failures = 0;
                self.backoff = FIRST_BACKOFF;
                self.healthy.store(true, Ordering::Release);
                self.last_success_ts.store(unix_now(), Ordering::Release);
                Duration::from_millis(self.config.wal_interval_ms)
            }
            Err(e) => {
                self.error_count.fetch_add(1, Ordering::AcqRel);
                self.consecutive_failures += 1;
                if self.consecutive_failures >= UNHEALTHY_AFTER_FAILURES {
                    self.healthy.store(false, Ordering::Release);
                }
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
        }
    }

    /// Ship pending WAL bytes and write a final manifest, retrying a failure
    /// twice. Used on shutdown.
    pub async fn flush(&mut self) {
        for attempt in 0..3 {
            let done = match self.run_once().await {
                Ok(()) if self.current.is_some() => self.write_manifest().await,
                other => other,
            };
            match done {
                Ok(()) => {
                    self.last_success_ts.store(unix_now(), Ordering::Release);
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
    /// new restore point when the snapshot or the WAL generation changed; ship
    /// the WAL bytes fsynced since the last step; write a manifest when one is
    /// due.
    pub async fn run_once(&mut self) -> Result<(), BackupError> {
        if !self.resumed {
            self.resume().await?;
        }
        let pos = self.source.durable_wal_position();
        if pos.snapshot_in_progress {
            return Ok(());
        }
        let new_generation = self.current.as_ref().map(|g| g.engine) != Some(pos.generation);
        let snapshot_changed = self.local_snapshot_ident()? != self.snapshot.as_ref().map(|s| s.0);
        if new_generation || snapshot_changed {
            // A restore point was written, or the files were between states
            // and nothing changed; either way this step is done.
            self.upload_base(pos, new_generation).await?;
            return Ok(());
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

    // ── Resume ──────────────────────────────────────────────────────────────

    /// Read what a previous run left in the bucket, so this run never reuses a
    /// generation number and never re-uploads an archive.
    async fn resume(&mut self) -> Result<(), BackupError> {
        let mut highest: Option<u64> = None;
        let wal_dir = ObjectPath::from(object_key(&self.prefix, "wal"));
        for p in self
            .store
            .list_with_delimiter(Some(&wal_dir))
            .await?
            .common_prefixes
        {
            if let Some(g) = p.filename().and_then(|s| s.parse::<u64>().ok()) {
                highest = highest.max(Some(g));
            }
        }
        if let Some(m) = self.latest_manifest().await? {
            highest = highest.max(Some(m.wal_generation));
            self.commit_seq = m.commit_seq;
            for f in &m.files {
                if let Some(n) = parse_archive_key(&self.prefix, &f.key) {
                    self.archives.insert(n, f.clone());
                }
            }
        }
        self.next_generation = highest.map_or(0, |g| g + 1);
        self.resumed = true;
        Ok(())
    }

    async fn latest_manifest(&self) -> Result<Option<Manifest>, BackupError> {
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

    fn local_snapshot_ident(&self) -> Result<Option<FileIdent>, BackupError> {
        match std::fs::metadata(self.dir.join("snapshot.bin")) {
            Ok(m) => Ok(Some(FileIdent::of(&m))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Upload a restore point: the snapshot (when changed), new archives, the
    /// side files, and — after a WAL replacement — a new generation whose
    /// segments start at 0. Ships the new generation's durable bytes, then
    /// writes the manifest.
    async fn upload_base(
        &mut self,
        pos: WalPosition,
        new_generation: bool,
    ) -> Result<(), BackupError> {
        let intent = self.dir.join("snapshot.intent");
        if std::fs::metadata(&intent).is_ok_and(|m| m.len() > 0) {
            // A snapshot that started since `pos` was read writes the intent
            // first; that is not a failure, just a step to retry.
            if self.source.durable_wal_position().snapshot_in_progress {
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
        let snapshot_file = match std::fs::File::open(self.dir.join("snapshot.bin")) {
            Ok(f) => Some(f),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let after = self.source.durable_wal_position();
        if after.generation != pos.generation || after.snapshot_in_progress {
            // A snapshot ran or started while the file was opened: the file
            // may not go with this generation. Next step.
            return Ok(());
        }
        if let Some(file) = snapshot_file {
            let ident = FileIdent::of(&file.metadata()?);
            let key = object_key(&self.prefix, "snapshot.bin");
            let unchanged = self.snapshot.as_ref().is_some_and(|(i, _)| *i == ident);
            if !unchanged {
                let entry = self.put_file(&key, file).await?;
                self.snapshot = Some((ident, entry));
            }
        } else {
            self.snapshot = None;
        }
        self.base_archives = self.upload_new_archives().await?;
        self.sync_side_files().await?;
        if new_generation {
            self.current = Some(Generation {
                engine: pos.generation,
                number: self.next_generation,
                cursor: 0,
                segments: Vec::new(),
            });
            self.next_generation += 1;
        }
        self.manifest_dirty = true;
        self.ship_segments().await?;
        self.write_manifest().await
    }

    /// Upload every local archive not yet in the bucket; return the numbers of
    /// all local archives.
    async fn upload_new_archives(&mut self) -> Result<Vec<u64>, BackupError> {
        let mut local = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let name = entry?.file_name();
            let name = name.to_string_lossy();
            if let Some(n) = name
                .strip_prefix("wal.")
                .and_then(|r| r.strip_suffix(".archive"))
                .and_then(|n| n.parse::<u64>().ok())
            {
                local.push(n);
            }
        }
        local.sort_unstable();
        let mut present = Vec::new();
        for n in local {
            if !self.archives.contains_key(&n) {
                let file = match std::fs::File::open(self.dir.join(format!("wal.{n}.archive"))) {
                    Ok(f) => f,
                    // Pruned by a snapshot since the listing.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e.into()),
                };
                let entry = self.put_file(&archive_key(&self.prefix, n), file).await?;
                self.archives.insert(n, entry);
            }
            present.push(n);
        }
        Ok(present)
    }

    /// Upload each side file whose local copy changed since it was last
    /// uploaded; forget one that no longer exists locally.
    async fn sync_side_files(&mut self) -> Result<(), BackupError> {
        for name in SIDE_FILES {
            let path = self.dir.join(name);
            let meta = match std::fs::metadata(&path) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    if self.side.remove(name).is_some() {
                        self.manifest_dirty = true;
                    }
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            let ident = FileIdent::of(&meta);
            if self.side.get(name).is_some_and(|(i, _)| *i == ident) {
                continue;
            }
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            let key = object_key(&self.prefix, name);
            let entry = FileEntry {
                key: key.clone(),
                size: bytes.len() as u64,
                crc32: crc32_hex(&bytes),
            };
            self.put(&key, bytes).await?;
            self.side.insert(name, (ident, entry));
            self.manifest_dirty = true;
        }
        Ok(())
    }

    // ── WAL segments ────────────────────────────────────────────────────────

    /// Ship every fsynced WAL byte of the current generation not shipped yet.
    async fn ship_segments(&mut self) -> Result<(), BackupError> {
        let wal = self.dir.join("wal.bin");
        loop {
            let Some(engine_gen) = self.current.as_ref().map(|g| g.engine) else {
                return Ok(());
            };
            let pos = self.source.durable_wal_position();
            if pos.generation != engine_gen {
                return Ok(());
            }
            let cursor = self.current.as_ref().map_or(0, |g| g.cursor);
            if pos.len <= cursor {
                if pos.len == cursor {
                    self.commit_seq = self.commit_seq.max(pos.frames);
                }
                return Ok(());
            }
            let read = read_new_segment(&wal, cursor, pos.len)?;
            if self.source.durable_wal_position().generation != pos.generation {
                // wal.bin was replaced during the read: the bytes may be the
                // new file's. Drop them; the next step starts the new
                // generation.
                return Ok(());
            }
            let Some((start, end, bytes)) = read else {
                return Ok(());
            };
            let number = self.current.as_ref().map_or(0, |g| g.number);
            let key = segment_key(&self.prefix, number, start, end);
            let crc32 = crc32_hex(&bytes);
            self.put(&key, bytes).await?;
            if let Some(g) = self.current.as_mut() {
                g.cursor = end;
                g.segments.push(SegmentEntry { key, crc32 });
            }
            if end == pos.len {
                self.commit_seq = self.commit_seq.max(pos.frames);
            }
            self.manifest_dirty = true;
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
        if let Some((_, e)) = &self.snapshot {
            files.push(e.clone());
        }
        for n in &self.base_archives {
            if let Some(e) = self.archives.get(n) {
                files.push(e.clone());
            }
        }
        files.extend(self.side.values().map(|(_, e)| e.clone()));
        let manifest = Manifest {
            version: MANIFEST_VERSION,
            created_at: now_rfc3339(),
            commit_seq: self.commit_seq,
            oldest_commit_seq: self.local_floor(),
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

    /// The store's horizon floor (`wal.floor`), or 0.
    fn local_floor(&self) -> u64 {
        std::fs::read(self.dir.join("wal.floor"))
            .ok()
            .and_then(|b| {
                b.get(..8)
                    .map(|s| u64::from_le_bytes(s.try_into().unwrap()))
            })
            .unwrap_or(0)
    }

    /// Keep the newest `retain_n` manifests. Then delete every segment of a
    /// generation older than any kept manifest's, and every archive no kept
    /// manifest names. `snapshot.bin`, the side files and `LATEST` are never
    /// deleted.
    ///
    /// Conservative on doubt: when a kept manifest cannot be read, nothing but
    /// the surplus manifests is deleted.
    async fn apply_retention(&mut self) -> Result<(), BackupError> {
        let dir = ObjectPath::from(object_key(&self.prefix, "manifest"));
        let mut keys: Vec<String> = self
            .store
            .list_with_delimiter(Some(&dir))
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
        let mut min_generation = u64::MAX;
        let mut referenced: HashSet<String> = HashSet::new();
        for k in &keys[surplus..] {
            let Some(bytes) = self.get(k).await? else {
                return Ok(());
            };
            let Ok(m) = Manifest::from_json(&bytes) else {
                return Ok(());
            };
            min_generation = min_generation.min(m.wal_generation);
            referenced.extend(m.files.into_iter().map(|f| f.key));
        }
        let wal_dir = ObjectPath::from(object_key(&self.prefix, "wal"));
        for gen_dir in self
            .store
            .list_with_delimiter(Some(&wal_dir))
            .await?
            .common_prefixes
        {
            let Some(g) = gen_dir.filename().and_then(|s| s.parse::<u64>().ok()) else {
                continue;
            };
            if g >= min_generation {
                continue;
            }
            for o in self
                .store
                .list_with_delimiter(Some(&gen_dir))
                .await?
                .objects
            {
                let k = o.location.to_string();
                if parse_segment_key(&self.prefix, &k).is_some_and(|(sg, _, _)| sg == g) {
                    self.delete(&k).await?;
                }
            }
        }
        let top = (!self.prefix.is_empty()).then(|| ObjectPath::from(self.prefix.as_str()));
        for o in self.store.list_with_delimiter(top.as_ref()).await?.objects {
            let k = o.location.to_string();
            if let Some(n) = parse_archive_key(&self.prefix, &k) {
                if !referenced.contains(&k) {
                    self.delete(&k).await?;
                    self.archives.remove(&n);
                }
            }
        }
        Ok(())
    }

    // ── Object store calls ──────────────────────────────────────────────────

    async fn put(&self, key: &str, bytes: Vec<u8>) -> Result<(), BackupError> {
        self.store
            .put(&ObjectPath::from(key), PutPayload::from(bytes))
            .await?;
        Ok(())
    }

    /// Stream `file` to `key`, multipart when large; return its entry.
    async fn put_file(&self, key: &str, file: std::fs::File) -> Result<FileEntry, BackupError> {
        let mut file = tokio::fs::File::from_std(file);
        let mut writer = BufWriter::new(Arc::clone(&self.store), ObjectPath::from(key));
        let mut hasher = crc32fast::Hasher::new();
        let mut size = 0u64;
        let mut buf = vec![0u8; UPLOAD_CHUNK];
        let streamed: std::io::Result<()> = async {
            loop {
                let n = file.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                size += n as u64;
                writer.write_all(&buf[..n]).await?;
            }
            writer.shutdown().await
        }
        .await;
        if let Err(e) = streamed {
            let _ = writer.abort().await;
            return Err(e.into());
        }
        Ok(FileEntry {
            key: key.to_string(),
            size,
            crc32: format_crc32(hasher.finalize()),
        })
    }

    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, BackupError> {
        match self.store.get(&ObjectPath::from(key)).await {
            Ok(r) => Ok(Some(r.bytes().await?.to_vec())),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn delete(&self, key: &str) -> Result<(), BackupError> {
        match self.store.delete(&ObjectPath::from(key)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
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
