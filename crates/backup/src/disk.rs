//! Everything the worker reads from the store directory, as blocking calls.
//!
//! The worker runs each of these on Tokio's blocking pool
//! ([`tokio::task::spawn_blocking`]), never on a runtime thread: a segment
//! read can be 64 MiB, and a side file grows with the store's history.

use std::io;
use std::path::Path;
use std::time::UNIX_EPOCH;

/// A file's identity as `(len, mtime_nanos)` — the form the engine uses for
/// `snapshot.bin` ([`mushroomdb::WalPosition::snapshot`]).
pub type Ident = (u64, u64);

/// [`Ident`] of a file's metadata, computed as the engine computes it.
pub fn ident_of(meta: &std::fs::Metadata) -> Ident {
    let mtime_nanos = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    (meta.len(), mtime_nanos)
}

fn not_found_is_none<T>(r: io::Result<T>) -> io::Result<Option<T>> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// What one look at the store directory says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskState {
    /// `snapshot.bin`'s identity; `None` when there is none.
    pub snapshot: Option<Ident>,
    /// `wal.bin`'s length in bytes (0 when absent).
    pub wal_len: u64,
    /// Whether `snapshot.intent` holds anything: a snapshot is running, or
    /// one stopped half-way.
    pub intent_nonempty: bool,
}

pub fn observe(dir: &Path) -> io::Result<DiskState> {
    let snapshot =
        not_found_is_none(std::fs::metadata(dir.join("snapshot.bin")))?.map(|m| ident_of(&m));
    let wal_len = not_found_is_none(std::fs::metadata(dir.join("wal.bin")))?.map_or(0, |m| m.len());
    let intent_nonempty = not_found_is_none(std::fs::metadata(dir.join("snapshot.intent")))?
        .is_some_and(|m| m.len() > 0);
    Ok(DiskState {
        snapshot,
        wal_len,
        intent_nonempty,
    })
}

/// `snapshot.bin` opened, with the identity of the file actually opened.
pub fn open_snapshot(dir: &Path) -> io::Result<Option<(std::fs::File, Ident)>> {
    let Some(f) = not_found_is_none(std::fs::File::open(dir.join("snapshot.bin")))? else {
        return Ok(None);
    };
    let ident = ident_of(&f.metadata()?);
    Ok(Some((f, ident)))
}

/// Every `wal.N.archive` in the directory, as `(n, len)`, sorted by `n`.
pub fn local_archives(dir: &Path) -> io::Result<Vec<(u64, u64)>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if let Some(n) = crate::manifest::parse_archive_name(&name.to_string_lossy()) {
            if let Some(m) = not_found_is_none(entry.metadata())? {
                out.push((n, m.len()));
            }
        }
    }
    out.sort_unstable();
    Ok(out)
}

/// A side file's identity, and its bytes when the identity differs from
/// `known`. `Ok(None)` when the file does not exist.
pub fn read_side(
    dir: &Path,
    name: &str,
    known: Option<Ident>,
) -> io::Result<Option<(Ident, Option<Vec<u8>>)>> {
    let path = dir.join(name);
    let Some(meta) = not_found_is_none(std::fs::metadata(&path))? else {
        return Ok(None);
    };
    let ident = ident_of(&meta);
    if known == Some(ident) {
        return Ok(Some((ident, None)));
    }
    match not_found_is_none(std::fs::read(&path))? {
        Some(bytes) => Ok(Some((ident, Some(bytes)))),
        None => Ok(None),
    }
}

/// The store's horizon floor (`wal.floor`), or 0.
pub fn floor(dir: &Path) -> u64 {
    std::fs::read(dir.join("wal.floor"))
        .ok()
        .and_then(|b| {
            b.get(..8)
                .map(|s| u64::from_le_bytes(s.try_into().unwrap()))
        })
        .unwrap_or(0)
}
