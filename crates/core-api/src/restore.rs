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

use crate::{GraphDb, GraphError, Result};
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
