//! Restore a store from a backup prefix: `mushroomdb restore --from s3://…`.
//!
//! A restore reads one restore point (W4 §3.1):
//!
//! 1. `{prefix}/LATEST` names the newest manifest. A point in time
//!    (`--at`) uses the earliest manifest of the same lineage that holds the
//!    target commit instead; a timestamp is first resolved to a commit
//!    through the newest manifest's `commit_times.bin`.
//! 2. Every file the manifest names is downloaded and its size and CRC-32
//!    checked; its segments are checked the same way, must be of the
//!    manifest's generation and contiguous from offset 0, and are
//!    concatenated into `wal.bin`. A restore of the newest manifest also
//!    takes the segments of its generation shipped after it, up to the last
//!    contiguous one, each checked frame by frame, so a restore loses at
//!    most the segment cadence. The manifest, its entries and the store's
//!    `backup.lineage` file must all name one lineage.
//! 3. With `--at`, the copy is cut back to the state right after the target
//!    commit ([`mushroomdb::restore::cut_at_commit`]): what `open_at` shows
//!    there, as an ordinary store.
//! 4. The copy gets a **new backup lineage**. A restored store is a new
//!    history: its next backup can never resume into the prefix it came from
//!    or delete anything there (defect-ledger row 138). Back it up to a new
//!    prefix.
//! 5. The copy is opened, read-only, and must hold the commits the restore
//!    point says it does. Only then is it moved into place.
//!
//! Everything up to the last step happens in a staging directory beside the
//! target. A restore that fails at any point leaves the target exactly as it
//! was — absent, or an empty directory — and removes the staging directory.
//! A target that holds anything is refused before anything is downloaded.

use crate::disk;
use crate::lease::new_instance_id;
use crate::manifest::{crc32_hex, latest_key, object_key, parse_segment_key, Manifest};
use crate::segment::complete_frames;
use crate::store::{build_store_with, BackupConfig};
use crate::{redact, BackupError};
use object_store::path::Path as ObjectPath;
use object_store::ObjectStore;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::io::AsyncWriteExt as _;

/// The point in time to restore (`--at`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreTarget {
    /// The store as it was right after this commit: the 0-based commit index
    /// `asof --commit` and `open_at` take. The restored store holds this
    /// many commits plus one.
    CommitSeq(u64),
    /// The store as it was at this instant: the last commit recorded at or
    /// before it, resolved through `commit_times.bin`.
    Timestamp(OffsetDateTime),
}

impl RestoreTarget {
    /// A commit index (digits only) or an RFC 3339 timestamp
    /// (`2026-10-01T12:00:00Z`).
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
            return s
                .parse()
                .map(RestoreTarget::CommitSeq)
                .map_err(|e| format!("--at {s:?}: {e}"));
        }
        OffsetDateTime::parse(s, &Rfc3339)
            .map(RestoreTarget::Timestamp)
            .map_err(|_| {
                format!(
                    "--at {s:?} is neither a commit number nor an RFC 3339 timestamp \
                     (for example 2026-10-01T12:00:00Z)"
                )
            })
    }
}

impl std::fmt::Display for RestoreTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RestoreTarget::CommitSeq(n) => write!(f, "commit {n}"),
            RestoreTarget::Timestamp(t) => write!(f, "{}", rfc3339(*t)),
        }
    }
}

/// What a restore did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreS3Outcome {
    /// The directory the store was restored into.
    pub into: PathBuf,
    /// The key of the manifest restored from.
    pub manifest: String,
    /// Commits the restored store holds (its `wal_total_commits`).
    pub commits: u64,
    /// The lineage of the backup restored from.
    pub source_lineage: String,
    /// The restored store's own, new lineage.
    pub lineage: String,
    /// The store files written, by name.
    pub files: Vec<String>,
    /// WAL segments concatenated into `wal.bin`.
    pub segments: usize,
    /// Bytes downloaded.
    pub bytes: u64,
}

/// Why a restore failed. The target is untouched in every case.
#[derive(Debug)]
pub enum RestoreS3Error {
    /// The `--from` URL or the `AWS_*` environment cannot be used.
    Config(String),
    /// The object store refused or failed a request.
    Store(object_store::Error),
    /// Reading or writing local files failed.
    Io(std::io::Error),
    /// The prefix holds no backup: there is no `LATEST`.
    NoBackup { from: String },
    /// A manifest is missing or does not parse.
    Manifest(String),
    /// An object does not match its manifest entry (size, CRC-32, lineage,
    /// generation or offsets), or the restored copy does not hold what the
    /// manifest says.
    Verify(String),
    /// The target directory already holds something.
    IntoNotEmpty { into: PathBuf },
    /// `--at` names a commit below the oldest one the backup still holds.
    BeforeOldestArchive { target: u64, oldest: u64 },
    /// `--at` names a commit past the newest one in the backup, which holds
    /// commits `0..commits`.
    AfterLatest { target: u64, commits: u64 },
    /// `--at` is a timestamp, and the backup holds no `commit_times.bin` (or
    /// one that records no commit) to resolve it with.
    CommitTimesAbsent,
    /// The target cannot be rebuilt from what the backup holds.
    Unreachable(String),
}

impl std::fmt::Display for RestoreS3Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RestoreS3Error::Config(m) => write!(f, "{m}"),
            RestoreS3Error::Store(e) => write!(f, "object store: {}", redact(&e.to_string())),
            RestoreS3Error::Io(e) => write!(f, "{e}"),
            RestoreS3Error::NoBackup { from } => {
                write!(f, "{from} holds no backup: there is no LATEST under it")
            }
            RestoreS3Error::Manifest(m) => write!(f, "manifest: {m}"),
            RestoreS3Error::Verify(m) => write!(f, "the restore point does not verify: {m}"),
            RestoreS3Error::IntoNotEmpty { into } => write!(
                f,
                "{} is not empty; restore into a new or empty directory",
                into.display()
            ),
            RestoreS3Error::BeforeOldestArchive { target, oldest } => write!(
                f,
                "commit {target} is before the oldest commit the backup holds ({oldest}); \
                 restore commit {oldest} or later"
            ),
            RestoreS3Error::AfterLatest { target, commits } => match commits.checked_sub(1) {
                Some(newest) => write!(
                    f,
                    "commit {target} is past the newest commit in the backup ({newest})"
                ),
                None => write!(f, "commit {target}: the backup holds no commits"),
            },
            RestoreS3Error::CommitTimesAbsent => write!(
                f,
                "the backup records no commit times (no commit_times.bin), so a timestamp \
                 cannot be resolved; restore by commit number instead"
            ),
            RestoreS3Error::Unreachable(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for RestoreS3Error {}

impl From<object_store::Error> for RestoreS3Error {
    fn from(e: object_store::Error) -> Self {
        RestoreS3Error::Store(e)
    }
}

impl From<std::io::Error> for RestoreS3Error {
    fn from(e: std::io::Error) -> Self {
        RestoreS3Error::Io(e)
    }
}

impl From<BackupError> for RestoreS3Error {
    fn from(e: BackupError) -> Self {
        match e {
            BackupError::Store(e) => RestoreS3Error::Store(e),
            BackupError::Io(e) => RestoreS3Error::Io(e),
            BackupError::Manifest(m) => RestoreS3Error::Manifest(m),
            other => RestoreS3Error::Config(other.to_string()),
        }
    }
}

/// Restore the backup at `from` (`s3://bucket/prefix`, `r2://…` or
/// `minio://…`) into `into`, at `at` or at the newest restore point.
///
/// Credentials and endpoint come from the `AWS_*` variables, as for backup
/// ([`crate::build_store`]). See the module docs for what is verified and in
/// which order.
pub async fn restore_from_s3(
    from: &str,
    at: Option<RestoreTarget>,
    into: &Path,
) -> Result<RestoreS3Outcome, RestoreS3Error> {
    restore_from_s3_with(from, at, into, |k| std::env::var(k).ok()).await
}

/// [`restore_from_s3`] over any variable lookup.
pub async fn restore_from_s3_with(
    from: &str,
    at: Option<RestoreTarget>,
    into: &Path,
    get: impl Fn(&str) -> Option<String>,
) -> Result<RestoreS3Outcome, RestoreS3Error> {
    let config = BackupConfig::new(from.trim());
    let location = config
        .location()
        .map_err(|e| RestoreS3Error::Config(config_message(e, from)))?;
    check_into(into)?;
    let store = build_store_with(&config, get)
        .map_err(|e| RestoreS3Error::Config(config_message(e, from)))?;
    restore_from_store(store, &location.prefix, at, into).await
}

fn config_message(e: BackupError, from: &str) -> String {
    match e {
        BackupError::Config(m) => m.replace("MUSHROOMDB_BACKUP_S3_URL", "--from"),
        other => format!("{from}: {other}"),
    }
}

/// Restore from `prefix` in `store` into `into`: the object-store half of
/// [`restore_from_s3`], for any [`ObjectStore`].
pub async fn restore_from_store(
    store: Arc<dyn ObjectStore>,
    prefix: &str,
    at: Option<RestoreTarget>,
    into: &Path,
) -> Result<RestoreS3Outcome, RestoreS3Error> {
    check_into(into)?;
    let bucket = Bucket { store, prefix };
    let Some(latest_target) = bucket.get_text(&latest_key(prefix)).await? else {
        return Err(RestoreS3Error::NoBackup {
            from: if prefix.is_empty() {
                "the bucket".into()
            } else {
                prefix.to_string()
            },
        });
    };
    let latest = bucket.manifest(&latest_target).await?;
    let tail = bucket.tail_segments(&latest).await?;
    let available = latest.commit_seq + tail.iter().map(|s| s.frames).sum::<u64>();

    let target = match at {
        None => None,
        Some(RestoreTarget::CommitSeq(n)) => Some(n),
        Some(RestoreTarget::Timestamp(t)) => Some(bucket.resolve_time(&latest, t).await?),
    };
    let (key, manifest, tail, expect) = match target {
        None => (latest_target, latest, tail, available),
        Some(n) if n >= available => {
            return Err(RestoreS3Error::AfterLatest {
                target: n,
                commits: available,
            })
        }
        Some(n) if n >= latest.commit_seq => (latest_target, latest, tail, n + 1),
        Some(n) => {
            let (key, m) = bucket.manifest_holding(n, &latest_target, &latest).await?;
            if n < m.oldest_commit_seq {
                return Err(RestoreS3Error::BeforeOldestArchive {
                    target: n,
                    oldest: m.oldest_commit_seq,
                });
            }
            (key, m, Vec::new(), n + 1)
        }
    };

    let staging = Staging::beside(into)?;
    let mut out = RestoreS3Outcome {
        into: into.to_path_buf(),
        manifest: key,
        commits: expect,
        source_lineage: manifest.lineage.clone(),
        lineage: new_instance_id(),
        files: Vec::new(),
        segments: 0,
        bytes: 0,
    };
    bucket
        .download(&manifest, &tail, staging.path(), &mut out)
        .await?;

    let dir = staging.path().to_path_buf();
    let lineage = out.lineage.clone();
    let cut = target;
    let manifest_key = out.manifest.clone();
    blocking(move || -> Result<(), RestoreS3Error> {
        if let Some(n) = cut {
            mushroomdb::restore::cut_at_commit(&dir, n).map_err(|e| match e {
                mushroomdb::GraphError::CommitOutOfRange { floor, .. } if n < floor => {
                    RestoreS3Error::BeforeOldestArchive {
                        target: n,
                        oldest: floor,
                    }
                }
                mushroomdb::GraphError::CommitOutOfRange { .. } => {
                    RestoreS3Error::Unreachable(format!(
                        "commit {n} cannot be rebuilt from the restore point {manifest_key}: \
                         the history it needs before that restore point's snapshot is not \
                         whole in the backup (a truncating snapshot or archive retention \
                         removed some of it)"
                    ))
                }
                other => RestoreS3Error::Io(std::io::Error::other(other.to_string())),
            })?;
        }
        disk::create_lineage(&dir, &lineage)?;
        let opened = mushroomdb::GraphDb::open_with_options(
            &dir,
            mushroomdb::OpenOptions {
                auto_migrate: false,
                repair_wal: false,
                read_only: true,
            },
        )
        .map_err(|e| RestoreS3Error::Verify(format!("the restored copy does not open: {e}")))?;
        let commits = opened
            .wal_total_commits()
            .map_err(|e| RestoreS3Error::Verify(e.to_string()))?;
        if commits != expect {
            return Err(RestoreS3Error::Verify(format!(
                "the restored copy holds {commits} commits; {manifest_key} says {expect}"
            )));
        }
        Ok(())
    })
    .await?;
    if !out.files.iter().any(|f| f == disk::LINEAGE_FILE) {
        out.files.push(disk::LINEAGE_FILE.to_string());
    }
    let dir = staging.path().to_path_buf();
    let target_dir = into.to_path_buf();
    let names = out.files.clone();
    blocking(move || install(&dir, &target_dir, &names)).await?;
    staging.installed();
    Ok(out)
}

// ── The bucket ──────────────────────────────────────────────────────────────

/// Bytes per ranged GET when downloading a file: the most a restore holds in
/// memory at once, whatever the size of the snapshot.
const DOWNLOAD_CHUNK: usize = 8 * 1024 * 1024;

struct Bucket<'a> {
    store: Arc<dyn ObjectStore>,
    prefix: &'a str,
}

/// A segment of the newest manifest's generation shipped after it.
struct TailSegment {
    key: String,
    bytes: Vec<u8>,
    frames: u64,
}

impl Bucket<'_> {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, RestoreS3Error> {
        match self.store.get(&ObjectPath::from(key)).await {
            Ok(r) => Ok(Some(r.bytes().await?.to_vec())),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn get_text(&self, key: &str) -> Result<Option<String>, RestoreS3Error> {
        match self.get(key).await? {
            None => Ok(None),
            Some(b) => String::from_utf8(b)
                .map(|s| Some(s.trim().to_string()))
                .map_err(|_| RestoreS3Error::Manifest(format!("{key} is not UTF-8"))),
        }
    }

    async fn manifest(&self, key: &str) -> Result<Manifest, RestoreS3Error> {
        let bytes = self.get(key).await?.ok_or_else(|| {
            RestoreS3Error::Manifest(format!("{key} is named but does not exist"))
        })?;
        Manifest::from_json(&bytes).map_err(|e| RestoreS3Error::Manifest(format!("{key}: {e}")))
    }

    /// The earliest manifest of `latest`'s lineage whose restore point holds
    /// commit `n`: the first one past it in commit order. Manifests of any
    /// other lineage, and ones that do not parse, are passed over. `latest`
    /// itself holds `n`, so there always is one.
    async fn manifest_holding(
        &self,
        n: u64,
        latest_key: &str,
        latest: &Manifest,
    ) -> Result<(String, Manifest), RestoreS3Error> {
        let dir = ObjectPath::from(object_key(self.prefix, "manifest"));
        let mut keyed: Vec<(u64, String)> = self
            .store
            .list_with_delimiter(Some(&dir))
            .await?
            .objects
            .into_iter()
            .filter_map(|o| {
                let key = o.location.to_string();
                let seq = key
                    .rsplit('/')
                    .next()?
                    .strip_suffix(".json")?
                    .parse::<u64>()
                    .ok()?;
                (seq > n).then_some((seq, key))
            })
            .collect();
        keyed.sort();
        for (_, key) in keyed {
            match self.manifest(&key).await {
                Ok(m) if m.lineage == latest.lineage && m.commit_seq > n => return Ok((key, m)),
                Ok(_) | Err(RestoreS3Error::Manifest(_)) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok((latest_key.to_string(), latest.clone()))
    }

    /// The segments of `latest`'s generation after its last one, up to the
    /// last contiguous one whose bytes are whole, valid WAL frames.
    async fn tail_segments(&self, latest: &Manifest) -> Result<Vec<TailSegment>, RestoreS3Error> {
        let mut end = match latest.segments.last() {
            Some(s) => {
                parse_segment_key(self.prefix, &s.key)
                    .ok_or_else(|| {
                        RestoreS3Error::Verify(format!("{} is not a segment key", s.key))
                    })?
                    .2
            }
            None => 0,
        };
        let dir = ObjectPath::from(object_key(
            self.prefix,
            &format!("wal/{}", latest.wal_generation),
        ));
        let mut later: Vec<(u64, u64, String)> = self
            .store
            .list_with_delimiter(Some(&dir))
            .await?
            .objects
            .into_iter()
            .filter_map(|o| {
                let key = o.location.to_string();
                let (g, start, stop) = parse_segment_key(self.prefix, &key)?;
                (g == latest.wal_generation && start >= end).then_some((start, stop, key))
            })
            .collect();
        later.sort();
        let mut tail = Vec::new();
        for (start, stop, key) in later {
            if start != end {
                break;
            }
            let Some(bytes) = self.get(&key).await? else {
                break;
            };
            let (whole, frames) = complete_frames(&bytes);
            if bytes.len() as u64 != stop - start || whole != bytes.len() {
                break;
            }
            end = stop;
            tail.push(TailSegment { key, bytes, frames });
        }
        Ok(tail)
    }

    /// The commit `t` resolves to through `latest`'s `commit_times.bin`.
    async fn resolve_time(
        &self,
        latest: &Manifest,
        t: OffsetDateTime,
    ) -> Result<u64, RestoreS3Error> {
        let entry = latest
            .files
            .iter()
            .find(|f| f.name == "commit_times.bin")
            .ok_or(RestoreS3Error::CommitTimesAbsent)?;
        let bytes = self
            .get(&entry.key)
            .await?
            .ok_or_else(|| RestoreS3Error::Verify(format!("{} does not exist", entry.key)))?;
        if bytes.len() as u64 != entry.size || crc32_hex(&bytes) != entry.crc32 {
            return Err(RestoreS3Error::Verify(format!(
                "{} does not match its manifest entry",
                entry.key
            )));
        }
        let ms = (t.unix_timestamp_nanos().div_euclid(1_000_000)) as i64;
        mushroomdb::restore::commit_at_instant(&bytes, latest.oldest_commit_seq, ms).map_err(|e| {
            match e {
                mushroomdb::GraphError::NoRecordedTime => RestoreS3Error::CommitTimesAbsent,
                mushroomdb::GraphError::TimeBeforeFloor {
                    floor_ms,
                    floor_commit,
                } => RestoreS3Error::Unreachable(format!(
                    "{} is before the oldest recorded commit time ({}, commit {floor_commit})",
                    rfc3339(t),
                    OffsetDateTime::from_unix_timestamp_nanos(floor_ms as i128 * 1_000_000)
                        .map(rfc3339)
                        .unwrap_or_else(|_| format!("{floor_ms} ms"))
                )),
                mushroomdb::GraphError::CommitOutOfRange { commit, floor, .. } => {
                    RestoreS3Error::BeforeOldestArchive {
                        target: commit,
                        oldest: floor,
                    }
                }
                other => RestoreS3Error::Verify(format!("commit_times.bin: {other}")),
            }
        })
    }

    /// Download and verify every object of `m` (and `tail`) into `dir`.
    async fn download(
        &self,
        m: &Manifest,
        tail: &[TailSegment],
        dir: &Path,
        out: &mut RestoreS3Outcome,
    ) -> Result<(), RestoreS3Error> {
        for f in &m.files {
            if !is_store_file_name(&f.name) {
                return Err(RestoreS3Error::Verify(format!(
                    "{} restores to {:?}, which is not a store file",
                    f.key, f.name
                )));
            }
            if out.files.contains(&f.name) {
                return Err(RestoreS3Error::Verify(format!(
                    "the manifest names {} twice",
                    f.name
                )));
            }
            let crc = self
                .download_file(&f.key, f.size, &dir.join(&f.name))
                .await?;
            if crc != f.crc32 {
                return Err(RestoreS3Error::Verify(format!(
                    "{} has CRC-32 {crc}; the manifest says {}",
                    f.key, f.crc32
                )));
            }
            if f.name == disk::LINEAGE_FILE {
                let named = tokio::fs::read_to_string(dir.join(&f.name)).await?;
                if named.trim() != m.lineage {
                    return Err(RestoreS3Error::Verify(format!(
                        "{} names lineage {}, its manifest lineage {}",
                        f.key,
                        named.trim(),
                        m.lineage
                    )));
                }
            }
            out.bytes += f.size;
            out.files.push(f.name.clone());
        }

        let mut wal = tokio::fs::File::create(dir.join("wal.bin")).await?;
        let mut offset = 0u64;
        for s in &m.segments {
            let (g, start, end) = parse_segment_key(self.prefix, &s.key)
                .ok_or_else(|| RestoreS3Error::Verify(format!("{} is not a segment key", s.key)))?;
            if g != m.wal_generation {
                return Err(RestoreS3Error::Verify(format!(
                    "{} is of generation {g}, the manifest's is {}",
                    s.key, m.wal_generation
                )));
            }
            if start != offset {
                return Err(RestoreS3Error::Verify(format!(
                    "{} starts at {start}; the segments before it end at {offset}",
                    s.key
                )));
            }
            let bytes = self.get(&s.key).await?.ok_or_else(|| {
                RestoreS3Error::Verify(format!("{} is named but does not exist", s.key))
            })?;
            let (whole, frames) = complete_frames(&bytes);
            if bytes.len() as u64 != end - start
                || crc32_hex(&bytes) != s.crc32
                || whole != bytes.len()
                || frames != s.frames
            {
                return Err(RestoreS3Error::Verify(format!(
                    "{} does not match its manifest entry (size, CRC-32 or frames)",
                    s.key
                )));
            }
            wal.write_all(&bytes).await?;
            offset = end;
            out.bytes += bytes.len() as u64;
            out.segments += 1;
        }
        for s in tail {
            wal.write_all(&s.bytes).await?;
            out.bytes += s.bytes.len() as u64;
            out.segments += 1;
            tracing::debug!(key = %s.key, "segment after the newest manifest");
        }
        wal.sync_all().await?;
        out.files.push("wal.bin".into());
        Ok(())
    }

    /// Download `key` (`size` bytes) to `path` by ranged GETs, returning its
    /// CRC-32.
    async fn download_file(
        &self,
        key: &str,
        size: u64,
        path: &Path,
    ) -> Result<String, RestoreS3Error> {
        let location = ObjectPath::from(key);
        let meta = match self.store.head(&location).await {
            Ok(m) => m,
            Err(object_store::Error::NotFound { .. }) => {
                return Err(RestoreS3Error::Verify(format!(
                    "{key} is named but does not exist"
                )))
            }
            Err(e) => return Err(e.into()),
        };
        if meta.size as u64 != size {
            return Err(RestoreS3Error::Verify(format!(
                "{key} is {} bytes; the manifest says {size}",
                meta.size
            )));
        }
        let mut file = tokio::fs::File::create(path).await?;
        let mut crc = crc32fast::Hasher::new();
        let mut at = 0usize;
        let size = size as usize;
        while at < size {
            let stop = (at + DOWNLOAD_CHUNK).min(size);
            let chunk = self.store.get_range(&location, at..stop).await?;
            if chunk.len() != stop - at {
                return Err(RestoreS3Error::Verify(format!(
                    "{key} changed while it was read"
                )));
            }
            crc.update(&chunk);
            file.write_all(&chunk).await?;
            at = stop;
        }
        file.sync_all().await?;
        Ok(crate::manifest::format_crc32(crc.finalize()))
    }
}

/// A name a manifest may restore a file to: one plain file name, and none
/// the restore writes itself or must never restore.
fn is_store_file_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\'])
        && !matches!(name, "wal.bin" | "snapshot.intent" | "LOCK")
}

// ── The target ──────────────────────────────────────────────────────────────

/// Refuse a target that holds anything, or is not a directory.
fn check_into(into: &Path) -> Result<(), RestoreS3Error> {
    match std::fs::read_dir(into) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                Err(RestoreS3Error::IntoNotEmpty {
                    into: into.to_path_buf(),
                })
            } else {
                Ok(())
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(RestoreS3Error::Io(std::io::Error::new(
            e.kind(),
            format!("{}: {e}", into.display()),
        ))),
    }
}

/// The staging directory beside the target. Dropped without
/// [`installed`](Self::installed), it is removed, and so are the directories
/// above it that the restore created.
struct Staging {
    path: PathBuf,
    /// Ancestors of the target this restore created, deepest first.
    created: Vec<PathBuf>,
}

impl Staging {
    fn beside(into: &Path) -> Result<Self, RestoreS3Error> {
        let parent = match into.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        let created: Vec<PathBuf> = parent
            .ancestors()
            .take_while(|a| !a.as_os_str().is_empty() && !a.exists())
            .map(Path::to_path_buf)
            .collect();
        std::fs::create_dir_all(&parent)?;
        let name = into
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "restored".into());
        let path = parent.join(format!(
            ".{name}.restore-{}-{}",
            std::process::id(),
            &new_instance_id()[..8]
        ));
        let staging = Self { path, created };
        std::fs::create_dir(&staging.path)?;
        Ok(staging)
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// The restore is in place: the staging directory goes, the directories
    /// above the target stay.
    fn installed(mut self) {
        self.created.clear();
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
        for dir in &self.created {
            // Only an empty one: anything another process put there stays.
            let _ = std::fs::remove_dir(dir);
        }
    }
}

/// Move the restored files from `staging` into `into`. A missing target is
/// created; an empty one is filled in place (it may be a mount point). A
/// failed move undoes the moves made, so the target is left as it was.
fn install(staging: &Path, into: &Path, names: &[String]) -> Result<(), RestoreS3Error> {
    // The target may have been filled since the start; it is checked again.
    check_into(into)?;
    let created = !into.exists();
    std::fs::create_dir_all(into)?;
    let mut moved: Vec<&String> = Vec::new();
    for name in names {
        if let Err(e) = std::fs::rename(staging.join(name), into.join(name)) {
            for done in &moved {
                let _ = std::fs::rename(into.join(done), staging.join(done));
            }
            if created {
                let _ = std::fs::remove_dir(into);
            }
            return Err(RestoreS3Error::Io(std::io::Error::new(
                e.kind(),
                format!("installing {name} into {}: {e}", into.display()),
            )));
        }
        moved.push(name);
    }
    #[cfg(unix)]
    std::fs::File::open(into)?.sync_all()?;
    Ok(())
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, RestoreS3Error> + Send + 'static,
) -> Result<T, RestoreS3Error> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| RestoreS3Error::Io(std::io::Error::other(e.to_string())))?
}

fn rfc3339(t: OffsetDateTime) -> String {
    t.format(&Rfc3339).unwrap_or_else(|_| t.to_string())
}
