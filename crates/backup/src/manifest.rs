//! The manifest: one consistent restore point, and the object keys it names.
//!
//! Schema per W4 §3.1, with one deliberate change: each object carries a
//! CRC-32 (`crc32`, eight lowercase hex digits) instead of a SHA-256. The
//! checksum is there to catch corruption, not tampering, and S3 verifies every
//! upload with its own SHA-256 checksum besides.
//!
//! Every object a manifest names has an immutable key: the snapshot and the
//! archives carry the generation they were uploaded for, a side file its own
//! checksum and size, a segment its generation and byte range. Nothing a
//! manifest names is ever overwritten, so every manifest still in the bucket
//! verifies — not only the newest. Each entry also says which file of the
//! store it restores to (`name`), since the key no longer does.

use serde::{Deserialize, Serialize};

/// The only manifest version this release writes or reads.
pub const MANIFEST_VERSION: u32 = 1;

/// One consistent restore point: the snapshot, the archives and side files
/// that go with it, and the ordered WAL segments of one generation.
///
/// Restoring it means: download `files`, concatenate `segments` (all of
/// `wal_generation`, in offset order, starting at offset 0) into `wal.bin`,
/// and open the store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// [`MANIFEST_VERSION`].
    pub version: u32,
    /// When the manifest was written, RFC 3339 in UTC.
    pub created_at: String,
    /// Global frame index one past the last frame this restore point holds —
    /// the index space `open_at` and `--at <commit>` use: what a store opened
    /// from `files` plus `segments` reports as `wal_total_commits`. Also the
    /// manifest's key, so manifests sort by it.
    pub commit_seq: u64,
    /// The first global frame index still reachable through the archives
    /// (the store's `wal.floor`; 0 when nothing was ever pruned). A point in
    /// time before it cannot be restored.
    pub oldest_commit_seq: u64,
    /// Snapshot, archives and side files, each with its size and checksum.
    pub files: Vec<FileEntry>,
    /// The WAL generation `segments` belong to. A new generation starts each
    /// time the store's `wal.bin` is replaced, so offsets never collide.
    pub wal_generation: u64,
    /// Segments of `wal_generation`, in offset order, contiguous from 0.
    pub segments: Vec<SegmentEntry>,
}

/// One whole object named by a manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// The store file it restores to: `snapshot.bin`, `wal.N.archive`,
    /// `commit_times.bin`, ….
    pub name: String,
    pub key: String,
    pub size: u64,
    /// CRC-32 of the object's bytes, as eight lowercase hex digits.
    pub crc32: String,
}

/// One WAL segment named by a manifest. Its byte range is in its key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentEntry {
    pub key: String,
    /// CRC-32 of the segment's bytes, as eight lowercase hex digits.
    pub crc32: String,
}

impl Manifest {
    pub fn to_json(&self) -> Vec<u8> {
        serde_json::to_vec_pretty(self).expect("a manifest always serialises")
    }

    /// Parse a manifest, refusing any version this release does not know.
    pub fn from_json(bytes: &[u8]) -> Result<Self, String> {
        let m: Manifest =
            serde_json::from_slice(bytes).map_err(|e| format!("manifest is not valid: {e}"))?;
        if m.version != MANIFEST_VERSION {
            return Err(format!(
                "manifest version {} is not supported by this release (expected {MANIFEST_VERSION})",
                m.version
            ));
        }
        Ok(m)
    }
}

/// CRC-32 of `bytes` in the manifest's format.
pub fn crc32_hex(bytes: &[u8]) -> String {
    format_crc32(crc32fast::hash(bytes))
}

/// A CRC-32 value in the manifest's format.
pub fn format_crc32(crc: u32) -> String {
    format!("{crc:08x}")
}

/// `{prefix}/{name}`, or `name` alone when the prefix is empty.
pub fn object_key(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

/// `{prefix}/wal/{generation}/{start:020}-{end:020}`.
pub fn segment_key(prefix: &str, generation: u64, start: u64, end: u64) -> String {
    object_key(prefix, &format!("wal/{generation}/{start:020}-{end:020}"))
}

/// The `(generation, start, end)` a segment key names, or `None` when `key`
/// is not a segment key under `prefix`.
pub fn parse_segment_key(prefix: &str, key: &str) -> Option<(u64, u64, u64)> {
    let rest = key.strip_prefix(&object_key(prefix, "wal/"))?;
    let (generation, range) = rest.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    Some((
        generation.parse().ok()?,
        start.parse().ok()?,
        end.parse().ok()?,
    ))
}

/// `{prefix}/manifest/{commit_seq:020}.json`.
pub fn manifest_key(prefix: &str, commit_seq: u64) -> String {
    object_key(prefix, &format!("manifest/{commit_seq:020}.json"))
}

/// `{prefix}/LATEST`: holds the newest manifest's key as plain UTF-8.
pub fn latest_key(prefix: &str) -> String {
    object_key(prefix, "LATEST")
}

/// `{prefix}/snapshots/{generation}-{commit_seq}.bin`: the snapshot uploaded
/// for `generation`, when the store's durable position was `commit_seq`.
pub fn snapshot_key(prefix: &str, generation: u64, commit_seq: u64) -> String {
    object_key(prefix, &format!("snapshots/{generation}-{commit_seq}.bin"))
}

/// `{prefix}/archives/{generation}-wal.{n}.archive`: archive `n`, uploaded
/// while shipping `generation`.
pub fn archive_key(prefix: &str, generation: u64, n: u64) -> String {
    object_key(prefix, &format!("archives/{generation}-wal.{n}.archive"))
}

/// `{prefix}/side/{name}/{crc32}-{size}`: a side file, addressed by content.
pub fn side_key(prefix: &str, name: &str, crc32: &str, size: u64) -> String {
    object_key(prefix, &format!("side/{name}/{crc32}-{size}"))
}

/// The generation a snapshot or archive key was uploaded for, or `None` for
/// any other key.
pub fn parse_versioned_generation(prefix: &str, key: &str) -> Option<u64> {
    let rest = key
        .strip_prefix(&object_key(prefix, "snapshots/"))
        .or_else(|| key.strip_prefix(&object_key(prefix, "archives/")))?;
    rest.split_once('-')?.0.parse().ok()
}

/// The archive number in a store file name `wal.{n}.archive`.
pub fn parse_archive_name(name: &str) -> Option<u64> {
    name.strip_prefix("wal.")?
        .strip_suffix(".archive")?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_zero_padded_and_parse_back() {
        let k = segment_key("p", 3, 0, 65536);
        assert_eq!(k, "p/wal/3/00000000000000000000-00000000000000065536");
        assert_eq!(parse_segment_key("p", &k), Some((3, 0, 65536)));
        assert_eq!(parse_segment_key("q", &k), None);
        assert_eq!(
            segment_key("", 0, 1, 2),
            "wal/0/00000000000000000001-00000000000000000002"
        );
        assert_eq!(
            manifest_key("p", 42),
            "p/manifest/00000000000000000042.json"
        );
        assert_eq!(latest_key(""), "LATEST");
        assert_eq!(archive_key("a/b", 3, 7), "a/b/archives/3-wal.7.archive");
        assert_eq!(snapshot_key("p", 4, 99), "p/snapshots/4-99.bin");
        assert_eq!(
            side_key("p", "wal.floor", "0000abcd", 8),
            "p/side/wal.floor/0000abcd-8"
        );
        assert_eq!(
            parse_versioned_generation("p", "p/snapshots/4-99.bin"),
            Some(4)
        );
        assert_eq!(
            parse_versioned_generation("a/b", "a/b/archives/3-wal.7.archive"),
            Some(3)
        );
        assert_eq!(parse_versioned_generation("p", "p/side/x/1-2"), None);
        assert_eq!(parse_archive_name("wal.7.archive"), Some(7));
        assert_eq!(parse_archive_name("wal.floor"), None);
    }

    #[test]
    fn an_unknown_version_is_refused() {
        let mut m = Manifest {
            version: 2,
            created_at: "2026-10-20T00:00:00Z".into(),
            commit_seq: 0,
            oldest_commit_seq: 0,
            files: vec![],
            wal_generation: 0,
            segments: vec![],
        };
        assert!(Manifest::from_json(&m.to_json()).is_err());
        m.version = MANIFEST_VERSION;
        assert!(Manifest::from_json(&m.to_json()).is_ok());
    }

    #[test]
    fn crc_is_eight_hex_digits() {
        assert_eq!(crc32_hex(b""), "00000000");
        assert_eq!(crc32_hex(b"123456789"), "cbf43926");
    }
}
