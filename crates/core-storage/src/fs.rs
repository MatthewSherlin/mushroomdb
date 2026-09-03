use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileId {
    Wal,
    Snapshot,
    /// Backup of the previous snapshot, kept until the next clean open at
    /// the current format version. Written before any migration to preserve
    /// the original bytes if the migration step fails.
    SnapshotBak,
    /// RBAC role definitions sidecar. Written atomically by `apply_schema`
    /// when roles change; loaded at open. Never part of WAL/snapshot format.
    Roles,
}

impl FileId {
    fn name(self) -> &'static str {
        match self {
            FileId::Wal => "wal.bin",
            FileId::Snapshot => "snapshot.bin",
            FileId::SnapshotBak => "snapshot.bin.bak",
            FileId::Roles => "roles.json",
        }
    }
}

pub trait Fs {
    fn append(&mut self, file: FileId, data: &[u8]) -> std::io::Result<()>;
    fn sync(&mut self, file: FileId) -> std::io::Result<()>;
    fn read(&self, file: FileId) -> std::io::Result<Vec<u8>>;
    fn write_atomic(&mut self, file: FileId, data: &[u8]) -> std::io::Result<()>;
    /// Return the on-disk path of the snapshot file, if any.
    ///
    /// `Some` for `RealFs` (used by `MappedBase::map` for true file mmap).
    /// `None` for `SimFs` and other in-memory implementations (falls back to
    /// `MappedBase::from_bytes`).
    fn snapshot_path(&self) -> Option<std::path::PathBuf> {
        None
    }

    /// Return the on-disk path of the WAL file, if any.
    ///
    /// `Some` for `RealFs`. `None` for in-memory implementations.
    /// Used by [`GraphDb::wal_size_bytes`] to read WAL file metadata.
    fn wal_path(&self) -> Option<std::path::PathBuf> {
        None
    }
    /// Read at most `n` bytes from the beginning of `file` without loading
    /// the full contents.
    ///
    /// Used by the open path to sniff the 6-byte magic+version header before
    /// deciding whether to mmap (V8) or full-read (legacy V5-V7).
    ///
    /// The default implementation calls `read()` and truncates; override in
    /// `RealFs` for a true partial read.
    fn read_prefix(&self, file: FileId, n: usize) -> std::io::Result<Vec<u8>> {
        let mut bytes = self.read(file)?;
        bytes.truncate(n);
        Ok(bytes)
    }

    // ── Cross-process write lock ──────────────────────────────────────────────

    /// Try to take the store's advisory exclusive write lock without blocking.
    ///
    /// Returns `Ok(true)` when the lock is now held by this handle, `Ok(false)`
    /// when another handle holds it.  Taking a lock this handle already holds
    /// is a successful no-op, so callers may re-acquire freely.
    ///
    /// The lock is advisory: it coordinates cooperating mushroomdb processes
    /// and does not stop an unrelated program from writing the files.
    ///
    /// Takes `&self`, not `&mut self`, on purpose: a writer must be able to
    /// poll for the lock without holding the in-process write guard, or a busy
    /// peer in another process would stall every reader in this one.
    ///
    /// Default: `Ok(true)` — an in-memory store has no other process to
    /// coordinate with.
    fn try_lock_exclusive(&self) -> std::io::Result<bool> {
        Ok(true)
    }

    /// Release the advisory write lock.  No-op when it is not held.
    ///
    /// Default: no-op.
    fn unlock(&self) -> std::io::Result<()> {
        Ok(())
    }

    // ── WAL tailing (refresh) ─────────────────────────────────────────────────

    /// Current length of the WAL in bytes.
    ///
    /// Must not read file contents: this is the hot half of a staleness check
    /// and runs on the read path.  The default implementation reads the WAL
    /// because a generic `Fs` has no cheaper option; `RealFs` overrides it with
    /// a metadata-only stat.
    fn wal_len(&self) -> std::io::Result<u64> {
        Ok(self.read(FileId::Wal)?.len() as u64)
    }

    /// Read `file` from byte offset `from` to the end.
    ///
    /// Returns an empty `Vec` when `from` is at or past the end of the file.
    /// Used to decode the WAL tail another process appended since this handle
    /// last consumed it.
    ///
    /// The default implementation reads the whole file and slices; `RealFs`
    /// overrides it with a seek + read of the tail only.
    fn read_range(&self, file: FileId, from: u64) -> std::io::Result<Vec<u8>> {
        let bytes = self.read(file)?;
        let from = from.min(bytes.len() as u64) as usize;
        Ok(bytes[from..].to_vec())
    }

    /// Identity of the current snapshot file as `(len, mtime_nanos)`, or `None`
    /// when no snapshot exists.
    ///
    /// A change in this pair means some process replaced the snapshot, so the
    /// WAL this handle was tailing no longer continues its in-memory state and
    /// a full reload is required.  Like [`wal_len`](Fs::wal_len) this must not
    /// read file contents.
    ///
    /// Default: length-only identity derived from the snapshot bytes (in-memory
    /// stores have no mtime); `RealFs` overrides it with a metadata-only stat.
    fn snapshot_ident(&self) -> std::io::Result<Option<(u64, u64)>> {
        let len = self.read(FileId::Snapshot)?.len() as u64;
        Ok(if len == 0 { None } else { Some((len, 0)) })
    }

    // ── WAL archive methods (Task 4: history-preserving snapshots) ─────────────

    /// List all WAL archive identifiers, sorted ascending (oldest first).
    ///
    /// Each archive created by [`archive_wal`] with commit-seq `N` appears as `N`.
    /// Returns an empty list when no archives exist.
    ///
    /// Default: no archive support — returns empty.
    fn list_archives(&self) -> std::io::Result<Vec<u64>> {
        Ok(vec![])
    }

    /// Read the byte contents of WAL archive `n`.
    ///
    /// Returns an empty `Vec` when the archive does not exist.
    ///
    /// Default: no archive support — returns empty.
    fn read_archive(&self, _n: u64) -> std::io::Result<Vec<u8>> {
        Ok(vec![])
    }

    /// Atomically rename the current WAL to `wal.<n>.archive` (same directory,
    /// same filesystem — the rename is guaranteed atomic at the OS level).
    ///
    /// The caller ensures the snapshot has been durably written before calling
    /// this method.  After a successful rename the old WAL no longer exists as
    /// `wal.bin`; a subsequent [`write_atomic`] on `FileId::Wal` creates a new
    /// empty WAL.
    ///
    /// Returns `Err` if the operation is not supported or fails.
    fn archive_wal(&mut self, _n: u64) -> std::io::Result<()> {
        Err(std::io::Error::other(
            "archive_wal not supported by this Fs implementation",
        ))
    }

    /// Delete archive `n`.  No-op if it does not exist.
    ///
    /// Retention pruning (inside `snapshot_with`) is the only call site.
    ///
    /// Default: no-op.
    fn delete_archive(&mut self, _n: u64) -> std::io::Result<()> {
        Ok(())
    }

    /// Return the persisted horizon floor — the global frame index of the
    /// first commit that is still reachable through surviving archives.
    ///
    /// Defaults to `0` (all history reachable / no pruning ever performed).
    fn read_horizon_floor(&self) -> std::io::Result<u64> {
        Ok(0)
    }

    /// Atomically persist `floor` so that a subsequent [`read_horizon_floor`]
    /// after reopen returns the same value.
    ///
    /// Default: no-op (in-memory only; override in durable implementations).
    fn write_horizon_floor(&mut self, _floor: u64) -> std::io::Result<()> {
        Ok(())
    }

    /// Return `true` when the `wal.genesis` marker file is present.
    ///
    /// The marker signals that the surviving archive chain forms a complete,
    /// uninterrupted WAL history starting from the store's first-ever commit
    /// (the genesis chain).  When absent, archive-resident commits are not
    /// safe to replay from empty state and `open_at` must refuse them.
    ///
    /// Default: `false` (no genesis chain / no archive support).
    fn has_genesis_marker(&self) -> bool {
        false
    }

    /// Durably create the `wal.genesis` marker file.
    ///
    /// Written exactly once, when the first WAL archive is taken from a store
    /// that has never undergone a WAL-truncating snapshot.
    ///
    /// Default: no-op.
    fn write_genesis_marker(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    /// Remove the `wal.genesis` marker file.
    ///
    /// Called when the genesis chain is broken: either by archive pruning
    /// (floor advances past 0) or by a WAL-truncating snapshot taken after
    /// archives already exist.  No-op if the marker is absent.
    ///
    /// Default: no-op.
    fn delete_genesis_marker(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub trait FsIntrospect {
    fn total_appended(&self) -> usize;
    fn sync_count(&self) -> usize {
        0
    }
}

/// Name of the advisory cross-process write lock file.
///
/// Always empty: the file exists only to carry the OS lock. It is created on
/// the first lock attempt and never removed, so the same inode backs the lock
/// for every process that opens the store.
pub const LOCK_FILE: &str = "LOCK";

/// This handle's one open description of the store's `LOCK` file, plus whether
/// it currently owns the lock.
///
/// Exactly one per `RealFs` on purpose: the OS lock is held per open file
/// description, so two descriptions of `LOCK` inside one process would contend
/// with each other. Opened lazily — a `RealFs` created for a one-shot file
/// operation never touches the lock file at all.
#[derive(Debug, Default)]
struct LockState {
    file: Option<File>,
    held: bool,
}

#[derive(Debug)]
pub struct RealFs {
    dir: PathBuf,
    /// Behind a `Mutex` so the lock can be taken and released through `&self`.
    /// A writer polls for the cross-process lock *before* it takes the
    /// in-process write guard, so that a peer holding the lock cannot stall
    /// this process's readers.
    lock: std::sync::Mutex<LockState>,
}

impl RealFs {
    pub fn new(dir: &std::path::Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            lock: std::sync::Mutex::new(LockState::default()),
        })
    }

    /// The database directory this filesystem is rooted at.
    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    fn path(&self, file: FileId) -> PathBuf {
        self.dir.join(file.name())
    }
}

impl Fs for RealFs {
    fn append(&mut self, file: FileId, data: &[u8]) -> std::io::Result<()> {
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path(file))?;
        f.write_all(data)
    }

    fn sync(&mut self, file: FileId) -> std::io::Result<()> {
        let f = File::open(self.path(file))?;
        full_sync(&f)
    }

    fn read(&self, file: FileId) -> std::io::Result<Vec<u8>> {
        match File::open(self.path(file)) {
            Ok(mut f) => {
                let mut buf = Vec::new();
                f.read_to_end(&mut buf)?;
                Ok(buf)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    fn write_atomic(&mut self, file: FileId, data: &[u8]) -> std::io::Result<()> {
        let tmp = self.dir.join(format!("{}.tmp", file.name()));
        {
            let mut f = File::create(&tmp)?;
            f.write_all(data)?;
            full_sync(&f)?;
        }
        std::fs::rename(&tmp, self.path(file))?;
        sync_dir(&self.dir)
    }

    fn snapshot_path(&self) -> Option<std::path::PathBuf> {
        Some(self.path(FileId::Snapshot))
    }

    fn wal_path(&self) -> Option<std::path::PathBuf> {
        Some(self.path(FileId::Wal))
    }

    fn read_prefix(&self, file: FileId, n: usize) -> std::io::Result<Vec<u8>> {
        use std::io::Read as _;
        match File::open(self.path(file)) {
            Ok(mut f) => {
                let mut buf = vec![0u8; n];
                let read = f.read(&mut buf)?;
                buf.truncate(read);
                Ok(buf)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    fn try_lock_exclusive(&self) -> std::io::Result<bool> {
        let mut state = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        if state.held {
            return Ok(true);
        }
        if state.file.is_none() {
            state.file = Some(
                OpenOptions::new()
                    .create(true)
                    .read(true)
                    .write(true)
                    .truncate(false)
                    .open(self.dir.join(LOCK_FILE))?,
            );
        }
        let f = state.file.as_ref().expect("lock file just opened");
        match f.try_lock() {
            Ok(()) => {
                state.held = true;
                Ok(true)
            }
            Err(std::fs::TryLockError::WouldBlock) => Ok(false),
            Err(std::fs::TryLockError::Error(e)) => Err(e),
        }
    }

    fn unlock(&self) -> std::io::Result<()> {
        let mut state = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        if !state.held {
            return Ok(());
        }
        // Clear the flag first: a failed unlock must not leave the handle
        // believing it still owns a lock it may have lost.
        state.held = false;
        match state.file.as_ref() {
            Some(f) => f.unlock(),
            None => Ok(()),
        }
    }

    fn wal_len(&self) -> std::io::Result<u64> {
        match std::fs::metadata(self.path(FileId::Wal)) {
            Ok(m) => Ok(m.len()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(e),
        }
    }

    fn read_range(&self, file: FileId, from: u64) -> std::io::Result<Vec<u8>> {
        use std::io::{Read as _, Seek as _, SeekFrom};
        let mut f = match File::open(self.path(file)) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let len = f.metadata()?.len();
        if from >= len {
            return Ok(Vec::new());
        }
        f.seek(SeekFrom::Start(from))?;
        let mut buf = Vec::with_capacity((len - from) as usize);
        f.read_to_end(&mut buf)?;
        Ok(buf)
    }

    fn snapshot_ident(&self) -> std::io::Result<Option<(u64, u64)>> {
        let m = match std::fs::metadata(self.path(FileId::Snapshot)) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        // mtime is a change hint, not a clock: an unreadable or pre-epoch
        // timestamp degrades to 0, leaving length alone to detect the change.
        let mtime_nanos = m
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        Ok(Some((m.len(), mtime_nanos)))
    }

    fn list_archives(&self) -> std::io::Result<Vec<u64>> {
        let mut ns = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let s = name.to_string_lossy();
            if let Some(mid) = s
                .strip_prefix("wal.")
                .and_then(|r| r.strip_suffix(".archive"))
            {
                if let Ok(n) = mid.parse::<u64>() {
                    ns.push(n);
                }
            }
        }
        ns.sort_unstable();
        Ok(ns)
    }

    fn read_archive(&self, n: u64) -> std::io::Result<Vec<u8>> {
        let path = self.dir.join(format!("wal.{n}.archive"));
        match std::fs::read(&path) {
            Ok(b) => Ok(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
            Err(e) => Err(e),
        }
    }

    fn archive_wal(&mut self, n: u64) -> std::io::Result<()> {
        let wal_path = self.path(FileId::Wal);
        let archive_path = self.dir.join(format!("wal.{n}.archive"));
        std::fs::rename(&wal_path, &archive_path)?;
        sync_dir(&self.dir)
    }

    fn delete_archive(&mut self, n: u64) -> std::io::Result<()> {
        let path = self.dir.join(format!("wal.{n}.archive"));
        match std::fs::remove_file(&path) {
            Ok(()) => sync_dir(&self.dir),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn read_horizon_floor(&self) -> std::io::Result<u64> {
        let path = self.dir.join("wal.floor");
        match std::fs::read(&path) {
            Ok(b) if b.len() >= 8 => Ok(u64::from_le_bytes([
                b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
            ])),
            Ok(_) => Ok(0),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(e),
        }
    }

    fn write_horizon_floor(&mut self, floor: u64) -> std::io::Result<()> {
        let tmp = self.dir.join("wal.floor.tmp");
        {
            let mut f = File::create(&tmp)?;
            f.write_all(&floor.to_le_bytes())?;
            full_sync(&f)?;
        }
        std::fs::rename(&tmp, self.dir.join("wal.floor"))?;
        sync_dir(&self.dir)
    }

    fn has_genesis_marker(&self) -> bool {
        self.dir.join("wal.genesis").exists()
    }

    fn write_genesis_marker(&mut self) -> std::io::Result<()> {
        let path = self.dir.join("wal.genesis");
        {
            let mut f = File::create(&path)?;
            f.write_all(b"")?;
            full_sync(&f)?;
        }
        sync_dir(&self.dir)
    }

    fn delete_genesis_marker(&mut self) -> std::io::Result<()> {
        match std::fs::remove_file(self.dir.join("wal.genesis")) {
            Ok(()) => sync_dir(&self.dir),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

fn full_sync(file: &File) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::io::AsRawFd;
        let fd = file.as_raw_fd();
        let rc = unsafe { libc::fcntl(fd, libc::F_FULLFSYNC) };
        if rc == -1 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        file.sync_all()
    }
}

/// Sync the WAL file at `dir/wal.bin` to persistent storage without
/// requiring a `&mut Fs`.  Used by the group-commit drain thread to fsync
/// outside the exclusive write-lock window (reducing reader-visible latency).
///
/// On macOS, uses `F_FULLFSYNC` for true durability.  On other platforms,
/// falls back to `fdatasync` / `fsync`.  Returns `Ok(())` if the WAL file
/// does not exist (nothing to sync).
pub fn sync_wal_at(dir: &std::path::Path) -> std::io::Result<()> {
    let path = dir.join(FileId::Wal.name());
    let f = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    full_sync(&f)
}

/// Truncate the WAL file at `dir/wal.bin` to exactly `len` bytes and fsync
/// the truncation to persistent storage.
///
/// Used by the group-commit drain thread when a group fsync fails: truncating
/// the WAL back to the last known-good synced offset removes the unsynced
/// frames, ensuring a crash-then-replay cannot silently make the failed group
/// durable via a later successful fsync flushing the whole inode.
///
/// Returns `Ok(())` if the file does not exist (nothing to truncate).
pub fn truncate_wal_at(dir: &std::path::Path, len: u64) -> std::io::Result<()> {
    let path = dir.join(FileId::Wal.name());
    let f = match OpenOptions::new().write(true).open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    f.set_len(len)?;
    f.sync_all() // plain sync_all is sufficient for a truncation barrier
}

fn sync_dir(dir: &std::path::Path) -> std::io::Result<()> {
    let d = File::open(dir)?;
    d.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("graphdb-fs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn append_read_and_atomic_write() {
        let mut fs = RealFs::new(&tmp()).unwrap();
        assert_eq!(fs.read(FileId::Wal).unwrap(), Vec::<u8>::new()); // absent = empty
        fs.append(FileId::Wal, b"ab").unwrap();
        fs.append(FileId::Wal, b"cd").unwrap();
        fs.sync(FileId::Wal).unwrap();
        assert_eq!(fs.read(FileId::Wal).unwrap(), b"abcd");
        fs.write_atomic(FileId::Snapshot, b"snap1").unwrap();
        fs.write_atomic(FileId::Snapshot, b"snap2").unwrap(); // replaces
        assert_eq!(fs.read(FileId::Snapshot).unwrap(), b"snap2");
        fs.write_atomic(FileId::Wal, b"").unwrap(); // truncation path
        assert_eq!(fs.read(FileId::Wal).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn write_atomic_replaces_and_still_readable() {
        // existing append_read_and_atomic_write already covers replace;
        // keep it; dir-sync is best-effort observable only via crash tests.
        // Do not fake F_FULLFSYNC in SimFs.
        let d = std::env::temp_dir().join(format!("graphdb-fs-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let mut fs = RealFs::new(&d).unwrap();
        fs.write_atomic(FileId::Snapshot, b"snap1").unwrap();
        fs.write_atomic(FileId::Snapshot, b"snap2").unwrap();
        assert_eq!(fs.read(FileId::Snapshot).unwrap(), b"snap2");
    }
}
