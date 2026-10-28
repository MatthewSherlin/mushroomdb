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
    /// Which file `wal.bin` is ([`mushroomdb::WalPosition::wal`]'s form).
    pub wal: Option<u64>,
    /// Whether `snapshot.intent` holds anything: a snapshot is running, or
    /// one stopped half-way.
    pub intent_nonempty: bool,
}

pub fn observe(dir: &Path) -> io::Result<DiskState> {
    let snapshot =
        not_found_is_none(std::fs::metadata(dir.join("snapshot.bin")))?.map(|m| ident_of(&m));
    let wal_meta = not_found_is_none(std::fs::metadata(dir.join("wal.bin")))?;
    let wal_len = wal_meta.as_ref().map_or(0, |m| m.len());
    let wal = wal_meta.as_ref().and_then(file_ident);
    let intent_nonempty = not_found_is_none(std::fs::metadata(dir.join("snapshot.intent")))?
        .is_some_and(|m| m.len() > 0);
    Ok(DiskState {
        snapshot,
        wal_len,
        wal,
        intent_nonempty,
    })
}

impl DiskState {
    /// Nothing that pairs a snapshot with a WAL changed between `self` and a
    /// later look `after`: no snapshot was running at either, the same
    /// snapshot and the same WAL file are there, and the WAL only grew.
    pub fn unchanged_until(&self, after: &DiskState) -> bool {
        !self.intent_nonempty
            && !after.intent_nonempty
            && self.snapshot == after.snapshot
            && self.wal == after.wal
            && after.wal_len >= self.wal_len
    }
}

/// The inode number on Unix; `None` elsewhere. The same identity the engine
/// records for `wal.bin`.
fn file_ident(meta: &std::fs::Metadata) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        Some(meta.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
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

/// The store file naming the backup lineage it belongs to.
pub const LINEAGE_FILE: &str = "backup.lineage";

/// The store's backup lineage, or `None` when it has never been backed up.
pub fn read_lineage(dir: &Path) -> io::Result<Option<String>> {
    Ok(
        not_found_is_none(std::fs::read_to_string(dir.join(LINEAGE_FILE)))?
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
    )
}

/// Give the store the backup lineage `id`: written to a temporary file,
/// fsynced, renamed into place, and the directory fsynced. The one file the
/// backup worker ever writes into a store directory.
pub fn create_lineage(dir: &Path, id: &str) -> io::Result<()> {
    use std::io::Write as _;
    let tmp = dir.join(format!("{LINEAGE_FILE}.tmp"));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(id.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, dir.join(LINEAGE_FILE))?;
    #[cfg(unix)]
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

/// The store file a restore-on-boot writes: which lineage and restore point
/// the store was restored from. Never uploaded (it is not a side file).
pub const PARENT_FILE: &str = "backup.parent";

/// What [`PARENT_FILE`] holds.
///
/// A store restored at boot has a new lineage of its own, like every restore.
/// This record is what lets its backup continue under the prefix it was
/// restored from: the worker adopts that prefix only while `LATEST` still
/// names exactly this manifest of exactly this lineage, and from then on
/// writes only objects of its own lineage beside the parent's, which it never
/// overwrites or deletes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ParentRecord {
    /// The lineage of the backup the store was restored from.
    pub lineage: String,
    /// The key of the manifest it was restored from.
    pub manifest: String,
    /// The commits the restored store held.
    pub commit_seq: u64,
}

/// The store's parent record, or `None` when it has none (or one that does
/// not parse: it then adopts nothing).
pub fn read_parent(dir: &Path) -> io::Result<Option<ParentRecord>> {
    Ok(not_found_is_none(std::fs::read(dir.join(PARENT_FILE)))?
        .and_then(|b| serde_json::from_slice(&b).ok()))
}

/// Write [`PARENT_FILE`] as [`create_lineage`] writes the lineage: a
/// temporary file, fsynced, renamed into place, the directory fsynced.
pub fn create_parent(dir: &Path, record: &ParentRecord) -> io::Result<()> {
    use std::io::Write as _;
    let bytes = serde_json::to_vec(record).map_err(io::Error::other)?;
    let tmp = dir.join(format!("{PARENT_FILE}.tmp"));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, dir.join(PARENT_FILE))?;
    #[cfg(unix)]
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

/// CRC-32 of a whole file, read from its start; the file is left at its
/// start again.
pub fn crc32_of(file: &mut std::fs::File) -> io::Result<String> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    file.seek(SeekFrom::Start(0))?;
    let mut h = crc32fast::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    file.seek(SeekFrom::Start(0))?;
    Ok(crate::manifest::format_crc32(h.finalize()))
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
