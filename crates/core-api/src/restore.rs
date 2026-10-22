//! Seeding a store directory from a backup.
//!
//! The mirror of [`GraphDb::backup_to`](crate::GraphDb::backup_to), and it
//! lives beside it for the same reason: a restore is a file-level copy of a
//! store directory followed by an open that proves the copy is a store.
//!
//! This was the CLI's `--restore-from` implementation until v0.6.10, when the
//! Python binding needed it too. `cli::restore_if_empty` is now a thin wrapper
//! over [`restore_if_empty`] here, with the CLI's error type; the semantics are
//! unchanged.

use crate::{GraphDb, GraphError, RealFs, Result};
use std::path::{Path, PathBuf};

/// What [`restore_if_empty`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum RestoreOutcome {
    /// `db_dir` already holds a store; nothing was copied.
    AlreadyPresent,
    /// Seeded from `from`.
    Restored {
        from: PathBuf,
        files: Vec<String>,
        bytes: u64,
    },
    /// `from` held no backup: nothing under it looks like a store.
    Empty,
}

/// Every file [`GraphDb::backup_to`](crate::GraphDb::backup_to) copies that is
/// not a WAL archive.
///
/// Kept in the same order, so a restore writes them the way a backup wrote
/// them. Archives are found by name at copy time, since their count varies.
const RESTORE_FILES: [&str; 8] = [
    "snapshot.bin",
    "snapshot.bin.bak",
    "wal.bin",
    "wal.floor",
    "wal.genesis",
    "roles.json",
    "commit_times.bin",
    "snapshot.intent",
];

/// Is there a store in `dir` already?
///
/// A store is "present" when `dir` holds `snapshot.bin` or a non-empty
/// `wal.bin`. An empty `wal.bin` is what a crashed first boot leaves behind,
/// and seeding over it is the whole point of `--restore-from`.
pub fn holds_a_store(dir: &Path) -> bool {
    if dir.join("snapshot.bin").is_file() {
        return true;
    }
    std::fs::metadata(dir.join("wal.bin"))
        .map(|m| m.is_file() && m.len() > 0)
        .unwrap_or(false)
}

/// How recently a backup directory was written: the newest mtime among the
/// files that make it a store.
///
/// `snapshot.bin` alone is not enough to rank by, because a store that has
/// never snapshotted backs up as `wal.bin` and nothing else — which is exactly
/// what `mushroomdb demo` then `mushroomdb backup` produces.
fn backup_mtime(dir: &Path) -> Option<std::time::SystemTime> {
    ["snapshot.bin", "wal.bin"]
        .iter()
        .filter_map(|n| {
            std::fs::metadata(dir.join(n))
                .and_then(|m| m.modified())
                .ok()
        })
        .max()
}

/// Pick the backup under `from`, if there is one.
///
/// `from` is either a backup directory itself (it holds a store) or a
/// directory of them, in which case the immediate subdirectory named `latest`
/// wins outright if it holds one — so a symlink or a rolling copy can name
/// itself — and otherwise the newest by mtime wins.
///
/// "Holds a store" is [`holds_a_store`], the same predicate that decides
/// whether `db_dir` needs seeding. A backup of a never-snapshotted store is
/// `wal.bin` and nothing else, and it carries every commit; ranking on
/// `snapshot.bin` alone would skip it and start empty.
fn choose_backup(from: &Path) -> Option<PathBuf> {
    if holds_a_store(from) {
        return Some(from.to_path_buf());
    }
    let entries = std::fs::read_dir(from).ok()?;
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in entries.flatten() {
        let dir = entry.path();
        if !holds_a_store(&dir) {
            continue;
        }
        if dir.file_name().map(|n| n == "latest").unwrap_or(false) {
            return Some(dir);
        }
        let Some(mtime) = backup_mtime(&dir) else {
            continue;
        };
        // Ties break on the path, so a vault of same-second backups still
        // picks the same one on every boot.
        let better = match &best {
            None => true,
            Some((best_mtime, best_dir)) => (mtime, &dir) > (*best_mtime, best_dir),
        };
        if better {
            best = Some((mtime, dir));
        }
    }
    best.map(|(_, dir)| dir)
}

/// Seed `db_dir` from the newest backup under `from`, if `db_dir` has no store.
///
/// A store is "present" when `db_dir` holds `snapshot.bin` or a non-empty
/// `wal.bin`. `from` is either a backup directory itself (it holds a store by
/// that same test) or a directory of them, in which case the immediate
/// subdirectory named `latest` wins if it holds one, else the newest by mtime.
///
/// The restore is all-or-nothing. The backup is copied into a staging
/// directory **inside** `db_dir` and opened there — the same CRC and replay
/// checks any open runs — and only a copy that opened is moved into place. Any
/// *error* removes the staging directory (or, once files have started moving,
/// undoes the moves already made) and leaves `db_dir` exactly as it was, so
/// the error names the paths, the operator can fix the backup, and the next
/// boot restores rather than reporting [`RestoreOutcome::AlreadyPresent`] over
/// a half-written store. That unwind is process-local: a crash between the
/// two renames that install the staged files (not a returned error, but the
/// process dying) can leave `db_dir` holding one file but not the other. The
/// next boot sees that partial store as already present and reports
/// [`RestoreOutcome::AlreadyPresent`] rather than restoring over it — clear
/// the directory and restore again.
///
/// Staging lives inside `db_dir` on purpose: `db_dir` is typically the mount
/// point, so a sibling directory could land on another filesystem and turn the
/// final moves into cross-device copies.
///
/// # Errors
///
/// Whatever creating `db_dir`, copying a file, opening the staged copy, or
/// moving it into place returned, as [`GraphError::Io`] carrying the message
/// verbatim — the paths are in the message, and a caller that must tell the
/// causes apart has them in the text rather than in the variant.
pub fn restore_if_empty(db_dir: &Path, from: &Path) -> Result<RestoreOutcome> {
    if holds_a_store(db_dir) {
        return Ok(RestoreOutcome::AlreadyPresent);
    }
    let Some(backup) = choose_backup(from) else {
        return Ok(RestoreOutcome::Empty);
    };

    std::fs::create_dir_all(db_dir)
        .map_err(|e| failed(format!("restore into {}: {e}", db_dir.display())))?;

    // Named for this process, so two `serve` processes racing onto one fresh
    // volume stage into separate directories rather than over each other.
    let staging = db_dir.join(format!(".restore-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging); // a previous run that was killed
    std::fs::create_dir_all(&staging)
        .map_err(|e| failed(format!("restore into {}: {e}", staging.display())))?;

    let outcome = stage_and_install(db_dir, &staging, &backup);
    // Whether it worked or not: the staging directory never outlives the call.
    // On success it holds only what opening the copy created (`LOCK`), since
    // the restored files were moved out of it.
    let _ = std::fs::remove_dir_all(&staging);
    outcome
}

/// A restore failure, carrying `msg` and nothing else.
///
/// `std::io::Error::other` displays as its message alone, so the text a caller
/// sees is the text built here — which is what lets the CLI keep printing the
/// messages it always has.
fn failed(msg: String) -> GraphError {
    GraphError::Io(std::io::Error::other(msg))
}

/// Copy `backup` into `staging`, prove it opens, then move it into `db_dir`.
///
/// Split out of [`restore_if_empty`] so every early return runs through one
/// cleanup of `staging` at the call site.
fn stage_and_install(db_dir: &Path, staging: &Path, backup: &Path) -> Result<RestoreOutcome> {
    let mut names: Vec<String> = RESTORE_FILES.iter().map(|n| n.to_string()).collect();
    let mut archives: Vec<String> = std::fs::read_dir(backup)
        .map_err(|e| failed(format!("restore from {}: {e}", backup.display())))?
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with("wal.") && n.ends_with(".archive"))
        .collect();
    archives.sort();
    names.extend(archives);

    let mut files = Vec::new();
    let mut bytes = 0u64;
    for name in names {
        let src = backup.join(&name);
        if !src.is_file() {
            continue;
        }
        let n = std::fs::copy(&src, staging.join(&name))
            .map_err(|e| failed(format!("restore {} from {}: {e}", name, backup.display())))?;
        bytes += n;
        files.push(name);
    }

    // Prove the copy opens before `serve` gets it. A copy that does not open is
    // a hard failure, not a silent empty start — and because it opened in
    // staging, nothing of it reaches `db_dir`.
    GraphDb::<core_storage::fs::RealFs>::open(staging).map_err(|e| {
        failed(format!(
            "restore into {} from {} failed: the copy does not open: {e}",
            db_dir.display(),
            backup.display()
        ))
    })?;

    // The copy is good. Move it in. A rename within one directory tree is the
    // closest thing to atomic the filesystem offers; if one still fails, undo
    // the moves already made so `db_dir` is left as it was found.
    let mut moved: Vec<&String> = Vec::new();
    for name in &files {
        if let Err(e) = std::fs::rename(staging.join(name), db_dir.join(name)) {
            for done in &moved {
                let _ = std::fs::remove_file(db_dir.join(done));
            }
            return Err(failed(format!(
                "restore into {} from {}: installing {name}: {e}",
                db_dir.display(),
                backup.display()
            )));
        }
        moved.push(name);
    }

    Ok(RestoreOutcome::Restored {
        from: backup.to_path_buf(),
        files,
        bytes,
    })
}

// ── Point in time ────────────────────────────────────────────────────────────

/// Cut the closed store in `dir` back to the state right after `commit`, so
/// that an ordinary open of it shows exactly what [`GraphDb::open_at`] at
/// `commit` shows, and it holds `commit + 1` commits.
///
/// `commit` is the 0-based index `open_at` and `asof --commit` take. The cut
/// reaches exactly the commits `open_at` reaches, by the same rules, and
/// refuses the rest with the same [`GraphError::CommitOutOfRange`]:
///
/// - **A commit in the live WAL** keeps the snapshot when it is the WAL's
///   base (a snapshot that replaced its WAL), and the WAL is cut after the
///   commit's frame. A snapshot that kept its WAL is the state at the WAL's
///   end, not its start; it is removed and the WAL prefix replays from
///   nothing, which is possible only while the WAL starts at the store's
///   first commit (no floor, no archives).
/// - **A commit in an archive** is rebuilt from nothing: the archives' frames
///   up to it become the WAL and the snapshot and archives are removed. That
///   needs the archive chain to start at the store's first commit (the
///   genesis marker, no floor).
///
/// The commit-time sidecar loses the entries after `commit`, so the next
/// commit is stamped. Nothing else is touched: roles, keys and the other
/// sidecars are what the store held.
///
/// The store must be closed and must not be in the middle of a snapshot: a
/// non-empty `snapshot.intent` is refused. This is the restore path's tool,
/// run on a staged copy before it is installed.
///
/// # Errors
///
/// [`GraphError::CommitOutOfRange`] for a commit `open_at` cannot reach;
/// [`GraphError::Io`] or [`GraphError::Corrupt`] when a file cannot be read
/// or written. Every check runs before the first write.
pub fn cut_at_commit(dir: &Path, commit: u64) -> Result<()> {
    use core_storage::fs::Fs as _;

    let intent = dir.join("snapshot.intent");
    if std::fs::metadata(&intent).is_ok_and(|m| m.len() > 0) {
        return Err(failed(format!(
            "cut {} at commit {commit}: a snapshot was interrupted half way ({} is not empty); \
             open the store once to settle it first",
            dir.display(),
            intent.display()
        )));
    }
    let fs = RealFs::new(dir)?;
    let floor = fs.read_horizon_floor()?;
    let genesis = fs.has_genesis_marker();
    // Archives at or below a raised floor are orphans the next open removes;
    // they hold no reachable commit.
    let archive_ns: Vec<u64> = fs
        .list_archives()?
        .into_iter()
        .filter(|&n| floor == 0 || n > floor)
        .collect();
    let mut archives = Vec::new();
    for &n in &archive_ns {
        let bytes = fs.read_archive(n)?;
        let ends = frame_ends(&bytes);
        archives.push((n, bytes, ends));
    }
    let archive_frames: u64 = archives.iter().map(|(_, _, e)| e.len() as u64).sum();
    let wal = match std::fs::read(dir.join("wal.bin")) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(GraphError::Io(e)),
    };
    let live_ends = frame_ends(&wal);
    let total = floor + archive_frames + live_ends.len() as u64;
    let out_of_range = GraphError::CommitOutOfRange {
        commit,
        total,
        floor,
    };
    if commit < floor || commit >= total {
        return Err(out_of_range);
    }
    let local = commit - floor;

    if local < archive_frames {
        if floor > 0 || !genesis {
            return Err(out_of_range);
        }
        let mut prefix = Vec::new();
        let mut left = local + 1;
        for (_, bytes, ends) in &archives {
            let take = left.min(ends.len() as u64) as usize;
            if take > 0 {
                prefix.extend_from_slice(&bytes[..ends[take - 1]]);
            }
            left -= take as u64;
            if left == 0 {
                break;
            }
        }
        rewrite_commit_times(dir, commit)?;
        write_file(dir, "wal.bin", &prefix)?;
        remove_if_present(&dir.join("snapshot.bin"))?;
        for (n, _, _) in &archives {
            remove_if_present(&dir.join(format!("wal.{n}.archive")))?;
        }
        return Ok(());
    }

    let keep = (local - archive_frames + 1) as usize;
    let drop_snapshot = match snapshot_replaced_its_wal(dir)? {
        None | Some(true) => false,
        Some(false) if floor > 0 || archive_frames > 0 => return Err(out_of_range),
        Some(false) => true,
    };
    rewrite_commit_times(dir, commit)?;
    write_file(dir, "wal.bin", &wal[..live_ends[keep - 1]])?;
    if drop_snapshot {
        remove_if_present(&dir.join("snapshot.bin"))?;
    }
    Ok(())
}

/// The commit an instant resolves to through a store's `commit_times.bin`
/// (`bytes`), above the store's WAL floor `wal_floor`: the last commit
/// recorded at or before `unix_ms`. The same resolution a date query makes.
///
/// # Errors
///
/// [`GraphError::NoRecordedTime`], [`GraphError::TimeBeforeFloor`] and
/// [`GraphError::CommitOutOfRange`] as a date query returns them;
/// [`GraphError::Corrupt`] when `bytes` does not decode.
pub fn commit_at_instant(bytes: &[u8], wal_floor: u64, unix_ms: i64) -> Result<u64> {
    core_storage::commit_times::decode(bytes)?.resolve_instant(unix_ms, wal_floor)
}

/// The byte offset at which each whole, valid frame of `bytes` ends, as far
/// as the engine's own decoder reads.
fn frame_ends(bytes: &[u8]) -> Vec<usize> {
    let (records, valid) = core_storage::wal::decode_all(bytes);
    let mut ends = Vec::with_capacity(records.len());
    let mut pos = 0usize;
    while pos < valid {
        let len = u32::from_le_bytes(bytes[pos..pos + 4].try_into().expect("4 bytes")) as usize;
        pos += 8 + len;
        ends.push(pos);
    }
    debug_assert_eq!(ends.len(), records.len());
    ends
}

/// Whether `snapshot.bin` replaced its WAL (`Some(true)`: it is the state at
/// the WAL's start), kept it (`Some(false)`: the state at its end), or is
/// absent (`None`). The flag `open_at` reads to decide the same thing.
fn snapshot_replaced_its_wal(dir: &Path) -> Result<Option<bool>> {
    let path = dir.join("snapshot.bin");
    let bytes = match std::fs::read(&path) {
        Ok(b) if b.is_empty() => return Ok(None),
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(GraphError::Io(e)),
    };
    let container = bytes.len() >= 6
        && &bytes[0..4] == b"GDB1"
        && core_storage::snapshot::is_mmap_container(u16::from_le_bytes([bytes[4], bytes[5]]));
    if container {
        drop(bytes);
        let mapped = core_storage::v8::MappedBase::map(&path)?;
        let meta = core_storage::v8::encode::decode_meta(mapped.meta_bytes()?)?;
        return Ok(Some(meta.wal_truncated));
    }
    Ok(core_storage::snapshot::decode(&bytes)?.map(|s| s.wal_truncated))
}

/// Drop the commit-time entries after `commit`. A sidecar that does not
/// decode is left as it is: the open reports it, as it would have.
fn rewrite_commit_times(dir: &Path, commit: u64) -> Result<()> {
    use core_storage::commit_times::{decode, ENTRY_LEN, HEADER_LEN};
    let path = dir.join("commit_times.bin");
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(GraphError::Io(e)),
    };
    if decode(&bytes).is_err() {
        return Ok(());
    }
    // Entries are `(commit u64, unix_ms i64)`, ascending by commit.
    let mut out = bytes[..HEADER_LEN].to_vec();
    for entry in bytes[HEADER_LEN..].chunks_exact(ENTRY_LEN) {
        let c = u64::from_le_bytes(entry[..8].try_into().expect("8 bytes"));
        if c <= commit {
            out.extend_from_slice(entry);
        }
    }
    if out.len() != bytes.len() {
        write_file(dir, "commit_times.bin", &out)?;
    }
    Ok(())
}

/// Write `dir/name` whole: a temporary file, fsynced, renamed into place.
fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let tmp = dir.join(format!("{name}.cut-tmp"));
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, dir.join(name))?;
    Ok(())
}

fn remove_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(GraphError::Io(e)),
    }
}
