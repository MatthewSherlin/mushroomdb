//! V8 snapshot format: mmap-able zero-copy snapshot.
//!
//! Wire layout (all integers LE):
//! ```text
//! [0..4]   MAGIC "GDB1"
//! [4..6]   VERSION = 8 (u16 LE)
//! [6..8]   section_count (u16 LE)
//! [8..8+16*N] SectionEntry * N  -- {id:u8, _pad:[u8;3], offset:u32, len:u32, crc32:u32}
//! [8+16*N..8+16*N+4]  whole-header CRC32
//! [..4096]  zero-pad
//! sections start at 8-byte aligned offsets (from file start, after the header page)
//! ```

pub mod encode;
pub mod layout;
pub mod seam;

use crate::types::{GraphError, Result};
use crate::v8::layout::{ArchivedColumns, ArchivedCsr, ArchivedIdMap, ArchivedInterner};
use memmap2::MmapOptions;
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};

/// Storage backing for a `MappedBase`.
///
/// `Mapped` uses a read-only `mmap` backed by the file on disk.
/// `Owned` holds the raw bytes in a `Vec` (used when the Fs abstraction
/// returns bytes rather than a file path, e.g. in-memory Fs in tests).
enum Backing {
    Mapped(memmap2::Mmap),
    Owned(Vec<u8>),
}

impl std::ops::Deref for Backing {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            Backing::Mapped(m) => m.as_ref(),
            Backing::Owned(v) => v.as_slice(),
        }
    }
}

/// Size of the header page in bytes.
pub const HEADER_SIZE: usize = 4096;

/// Section id constants.
pub const SECTION_TOPOLOGY: u8 = 0;
pub const SECTION_COLUMNS: u8 = 1;
pub const SECTION_IDS: u8 = 2;
pub const SECTION_SYMS: u8 = 3;
pub const SECTION_META: u8 = 4;

/// Total number of canonical section slots (used for atomic check_state array).
pub const V8_MAGIC_SECTION_COUNT: usize = 5;

/// Atomic check state values.
const STATE_UNCHECKED: u8 = 0;
const STATE_OK: u8 = 1;
const STATE_BAD: u8 = 2;

#[derive(Clone, Copy)]
struct SectionEntry {
    id: u8,
    offset: u32,
    len: u32,
    crc32: u32,
}

/// A read-only V8 snapshot backed by an mmap or owned bytes.
///
/// Sections are accessed zero-copy via `rkyv::access_unchecked` after a
/// lazy per-section CRC32 validation.  Validates the whole-header CRC on
/// construction and validates each section's CRC on first access.
pub struct MappedBase {
    backing: Backing,
    dir: Vec<SectionEntry>,
    /// Per-section lazy check state: 0=unchecked, 1=ok, 2=bad.
    check_state: [AtomicU8; V8_MAGIC_SECTION_COUNT],
}

impl MappedBase {
    /// Open and mmap a V8 snapshot file at `path`.
    ///
    /// Validates the 4KB header (magic, version, section directory,
    /// whole-header CRC32). Per-section CRC validation is deferred until
    /// first access.
    pub fn map(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path).map_err(GraphError::Io)?;
        // SAFETY: we hold the file open for the lifetime of the Mmap.
        let mmap = unsafe { MmapOptions::new().map(&file) }.map_err(GraphError::Io)?;
        let dir = parse_header(&mmap)?;
        Ok(Self {
            backing: Backing::Mapped(mmap),
            dir,
            check_state: std::array::from_fn(|_| AtomicU8::new(STATE_UNCHECKED)),
        })
    }

    /// Construct a `MappedBase` from an owned byte buffer (no file required).
    ///
    /// Used when the `Fs` implementation returns bytes directly (e.g. the
    /// in-memory `MemFs` used in unit tests or the generic `open_with(fs)`
    /// path that only exposes `Fs::read`).
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        let dir = parse_header(&bytes)?;
        Ok(Self {
            backing: Backing::Owned(bytes),
            dir,
            check_state: std::array::from_fn(|_| AtomicU8::new(STATE_UNCHECKED)),
        })
    }

    /// Return the raw bytes for `section_id`, validating its CRC32 lazily.
    fn section_bytes(&self, section_id: u8) -> Result<&[u8]> {
        let entry = self
            .dir
            .iter()
            .find(|e| e.id == section_id)
            .ok_or_else(|| GraphError::Corrupt {
                detail: format!("v8: section {section_id} not found in directory"),
            })?;
        let start = entry.offset as usize;
        let end = start
            .checked_add(entry.len as usize)
            .ok_or_else(|| GraphError::Corrupt {
                detail: format!("v8: section {section_id} length overflow"),
            })?;
        let bytes = self
            .backing
            .get(start..end)
            .ok_or_else(|| GraphError::Corrupt {
                detail: format!("v8: section {section_id} extends beyond file"),
            })?;
        // Lazy CRC validation.
        let idx = section_id as usize;
        if idx < V8_MAGIC_SECTION_COUNT {
            match self.check_state[idx].load(Ordering::Acquire) {
                STATE_OK => {} // already verified
                STATE_BAD => {
                    return Err(GraphError::Corrupt {
                        detail: format!("v8: section {section_id} CRC mismatch (cached)"),
                    });
                }
                _ => {
                    let computed = crc32fast::hash(bytes);
                    if computed != entry.crc32 {
                        self.check_state[idx].store(STATE_BAD, Ordering::Release);
                        return Err(GraphError::Corrupt {
                            detail: format!(
                                "v8: section {section_id} CRC mismatch \
                                 (expected {:08x}, computed {:08x})",
                                entry.crc32, computed
                            ),
                        });
                    }
                    self.check_state[idx].store(STATE_OK, Ordering::Release);
                }
            }
        }
        Ok(bytes)
    }

    /// Zero-copy access to the archived topology (CSR).
    ///
    /// The section CRC has already been validated by `section_bytes`.
    /// `rkyv::access` performs a lightweight structural check (root pointer
    /// in bounds, correct alignment) and is otherwise zero-copy.
    pub fn topology(&self) -> Result<&ArchivedCsr> {
        let bytes = self.section_bytes(SECTION_TOPOLOGY)?;
        rkyv::access::<crate::v8::layout::ArchivedCsrData, rkyv::rancor::Error>(bytes).map_err(
            |e| GraphError::Corrupt {
                detail: format!("v8: topology rkyv access: {e}"),
            },
        )
    }

    /// Zero-copy access to the archived column store.
    pub fn columns(&self) -> Result<&ArchivedColumns> {
        let bytes = self.section_bytes(SECTION_COLUMNS)?;
        rkyv::access::<crate::v8::layout::ArchivedColumnsData, rkyv::rancor::Error>(bytes).map_err(
            |e| GraphError::Corrupt {
                detail: format!("v8: columns rkyv access: {e}"),
            },
        )
    }

    /// Zero-copy access to the archived id map.
    pub fn ids(&self) -> Result<&ArchivedIdMap> {
        let bytes = self.section_bytes(SECTION_IDS)?;
        rkyv::access::<crate::v8::layout::ArchivedIdMapData, rkyv::rancor::Error>(bytes).map_err(
            |e| GraphError::Corrupt {
                detail: format!("v8: ids rkyv access: {e}"),
            },
        )
    }

    /// Zero-copy access to the archived symbol interner.
    pub fn syms(&self) -> Result<&ArchivedInterner> {
        let bytes = self.section_bytes(SECTION_SYMS)?;
        rkyv::access::<crate::v8::layout::ArchivedInternerData, rkyv::rancor::Error>(bytes).map_err(
            |e| GraphError::Corrupt {
                detail: format!("v8: syms rkyv access: {e}"),
            },
        )
    }

    /// Raw bytes for the bincode meta section.
    pub fn meta_bytes(&self) -> Result<&[u8]> {
        self.section_bytes(SECTION_META)
    }
}

/// Parse and validate the V8 header page.
///
/// Validates magic, version, directory bounds, and the whole-header CRC32.
/// Returns the section directory on success.
fn parse_header(mmap: &[u8]) -> Result<Vec<SectionEntry>> {
    if mmap.len() < HEADER_SIZE {
        return Err(GraphError::Corrupt {
            detail: format!(
                "v8: file is {} bytes; minimum for header is {HEADER_SIZE}",
                mmap.len()
            ),
        });
    }
    if &mmap[0..4] != b"GDB1" {
        return Err(GraphError::Corrupt {
            detail: "v8: bad magic (expected GDB1)".into(),
        });
    }
    let version = u16::from_le_bytes(mmap[4..6].try_into().unwrap());
    if version != 8 {
        return Err(GraphError::Corrupt {
            detail: format!("v8: expected version 8, got {version}"),
        });
    }
    let section_count = u16::from_le_bytes(mmap[6..8].try_into().unwrap()) as usize;
    let dir_end = 8usize
        .checked_add(section_count.saturating_mul(16))
        .ok_or_else(|| GraphError::Corrupt {
            detail: "v8: directory length overflow".into(),
        })?;
    if dir_end + 4 > HEADER_SIZE {
        return Err(GraphError::Corrupt {
            detail: format!(
                "v8: {section_count} sections require dir_end={dir_end} which overflows the header"
            ),
        });
    }
    // Whole-header CRC32 covers bytes [0..dir_end].
    let stored_crc = u32::from_le_bytes(mmap[dir_end..dir_end + 4].try_into().unwrap());
    let computed_crc = crc32fast::hash(&mmap[0..dir_end]);
    if stored_crc != computed_crc {
        return Err(GraphError::Corrupt {
            detail: format!(
                "v8: header CRC mismatch (expected {:08x}, computed {:08x})",
                stored_crc, computed_crc
            ),
        });
    }
    // Parse directory entries: {id:u8, _pad:[u8;3], offset:u32, len:u32, crc32:u32}.
    let mut dir = Vec::with_capacity(section_count);
    for i in 0..section_count {
        let base = 8 + i * 16;
        let id = mmap[base];
        let offset = u32::from_le_bytes(mmap[base + 4..base + 8].try_into().unwrap());
        let len = u32::from_le_bytes(mmap[base + 8..base + 12].try_into().unwrap());
        let crc32 = u32::from_le_bytes(mmap[base + 12..base + 16].try_into().unwrap());
        dir.push(SectionEntry {
            id,
            offset,
            len,
            crc32,
        });
    }
    Ok(dir)
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::columns::ColumnStore;
    use crate::idmap::IdMap;
    use crate::interner::Interner;
    use crate::topology::Topology;
    use crate::types::Value;
    use crate::v8::encode::{encode_v8, V8Meta};
    use std::collections::BTreeMap;

    fn tiny_v8_meta() -> V8Meta {
        V8Meta {
            labels: vec![0, 0],
            edge_props: crate::edge_props::EdgeProps::new(),
            rule_defs: vec![],
            provenance: BTreeMap::new(),
            rule_tripped: BTreeMap::new(),
            rule_fires: BTreeMap::new(),
            ivf_state: BTreeMap::new(),
            view_defs: vec![],
            wal_truncated: false,
            hnsw: BTreeMap::new(),
        }
    }

    fn encode_tiny() -> Vec<u8> {
        let mut ids = IdMap::new();
        ids.get_or_insert("a");
        ids.get_or_insert("b");
        let mut syms = Interner::new();
        let e = syms.intern("E");
        let mut topo = Topology::new();
        topo.add_edge(e, 0, 1);
        let mut props = ColumnStore::new();
        props.set(0, "v", Value::Int(42));
        let meta = tiny_v8_meta();
        let mut out = Vec::new();
        encode_v8(None, &topo, &props, &ids, &syms, &meta, &mut out).expect("encode_v8");
        out
    }

    fn tmp_path(suffix: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(format!(
            "/tmp/mushroom_v8_{}_{}.bin",
            std::process::id(),
            suffix
        ))
    }

    #[test]
    fn v8_encode_and_map_sections_valid() {
        let bytes = encode_tiny();
        let path = tmp_path("valid");
        std::fs::write(&path, &bytes).unwrap();
        let _cleanup = defer_remove(&path);
        let base = MappedBase::map(&path).expect("map");
        // Topology section: 1 edge
        let topo = base.topology().expect("topology()");
        assert_eq!(u64::from(topo.edge_count), 1);
        // IDs section: 2 keys
        let ids = base.ids().expect("ids()");
        assert_eq!(ids.to_key.len(), 2);
        // Syms section: 1 symbol
        let syms = base.syms().expect("syms()");
        assert_eq!(syms.to_str.len(), 1);
        assert_eq!(syms.to_str[0].as_str(), "E");
    }

    #[test]
    fn v8_corrupt_section_crc_returns_corrupt_error() {
        let mut bytes = encode_tiny();
        // Section 0 entry: at offset 8 in file.
        // Entry layout: {id:u8, _pad:3, offset:u32, len:u32, crc32:u32}
        // offset field is at bytes[8+4..8+8].
        let entry0_section_offset = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        // Flip a byte inside the section payload (skip the rkyv root pointer
        // at the end by targeting the start of the payload).
        if entry0_section_offset < bytes.len() {
            bytes[entry0_section_offset] ^= 0xff;
        }
        let path = tmp_path("corrupt");
        std::fs::write(&path, &bytes).unwrap();
        let _cleanup = defer_remove(&path);
        match MappedBase::map(&path) {
            Ok(base) => {
                // Map succeeded (header ok). Section access must fail.
                let result = base.topology();
                match result {
                    Err(GraphError::Corrupt { .. }) => {}
                    Err(e) => panic!("expected Corrupt, got {e:?}"),
                    Ok(_) => panic!("expected Corrupt error but topology() succeeded"),
                }
            }
            Err(GraphError::Corrupt { .. }) => {
                // Corruption may have hit the header — also acceptable.
            }
            Err(e) => panic!("unexpected error: {e:?}"),
        }
    }

    /// RAII guard that removes the file on drop.
    struct DeferRemove(std::path::PathBuf);
    impl Drop for DeferRemove {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    fn defer_remove(p: &std::path::Path) -> DeferRemove {
        DeferRemove(p.to_path_buf())
    }
}
