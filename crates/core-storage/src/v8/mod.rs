//! V8 snapshot format: mmap-able zero-copy snapshot.
//!
//! Wire layout (all integers LE):
//! ```text
//! [0..4]   MAGIC "GDB1"
//! [4..6]   VERSION = 8 (u16 LE)
//! [6..8]   section_count (u16 LE) — currently 11
//! [8..8+16*N] SectionEntry * N  -- {id:u8, _pad:[u8;3], offset:u32, len:u32, crc32:u32}
//! [8+16*N..8+16*N+4]  whole-header CRC32
//! [..4096]  zero-pad
//! sections start at 8-byte aligned offsets (from file start, after the header page)
//! ```
//!
//! Section ids (T5 layout):
//!   0 = CSR topology (rkyv CsrData)
//!   1 = columns (rkyv ColumnsData)
//!   2 = id map (rkyv IdMapData)
//!   3 = interner (rkyv InternerData)
//!   4 = META (bincode V8Meta — labels + wal_truncated only; large fields moved to own sections)
//!   5 = EDGE_PROPS (rkyv EdgePropsData, sorted by (etype,src,dst))
//!   6 = HNSW (rkyv HnswSectionData, opaque blobs)
//!   7 = PROVENANCE (rkyv ProvenanceSectionData; retained as undecoded bytes at open)
//!   8 = RULES_META (rkyv RulesMetaData)
//!   9 = VIEWS (rkyv ViewsSectionData)
//!  10 = IVF_STATE (bincode BTreeMap<String,PerRuleIvfState>; retained as undecoded bytes at open)

pub mod encode;
pub mod layout;
pub mod seam;

use crate::types::{GraphError, Result};
use crate::v8::layout::{
    ArchivedColumns, ArchivedCsr, ArchivedEdgeProps, ArchivedHnsw, ArchivedIdMap, ArchivedInterner,
    ArchivedRulesMeta, ArchivedViews,
};
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

/// Section id constants (sections 0-4 from Task 1, sections 5-9 from Task 2).
pub const SECTION_TOPOLOGY: u8 = 0;
pub const SECTION_COLUMNS: u8 = 1;
pub const SECTION_IDS: u8 = 2;
pub const SECTION_SYMS: u8 = 3;
pub const SECTION_META: u8 = 4;
/// Edge properties: sorted `EdgePropsData` (etype, src, dst → props blob).
pub const SECTION_EDGE_PROPS: u8 = 5;
/// Per-rule HNSW graph blobs: `HnswSectionData` sorted by rule name.
pub const SECTION_HNSW: u8 = 6;
/// Per-rule provenance sorted triples: `ProvenanceSectionData`.
pub const SECTION_PROVENANCE: u8 = 7;
/// Rule definitions, trip flags, fire counters: `RulesMetaData`.
pub const SECTION_RULES_META: u8 = 8;
/// Materialized view definitions: `ViewsSectionData`.
pub const SECTION_VIEWS: u8 = 9;
/// Per-approximate-rule IVF cluster state: bincode `BTreeMap<String, PerRuleIvfState>`.
/// Retained as undecoded bytes at open; consumed lazily on first mutation or WAL replay.
pub const SECTION_IVF_STATE: u8 = 10;
/// Per-node last-change commit sequence: bincode `HashMap<u32, u64>` (node_id → commit_seq).
/// Small section (8-16 bytes/node); loaded eagerly at open.  Missing in pre-Task-3 snapshots
/// (treated as absent; the live map is rebuilt from WAL replay only).
pub const SECTION_LAST_CHANGE: u8 = 11;

/// Total number of canonical section slots (used for atomic check_state array).
/// Extended from 11 (Task 5: +ivf_state) to 12 (Task 3: +last_change).
pub const V8_MAGIC_SECTION_COUNT: usize = 12;

/// Returns `true` for sections whose content is large enough that a
/// full-section CRC at first touch would cost tens or hundreds of
/// milliseconds.  Integrity for these sections is deferred to the explicit
/// `mushroomdb verify` command.  Bounds are still validated at open time via
/// `validate_section_bounds`.
///
/// Small sections (IDS, SYMS, META, RULES_META, VIEWS) retain eager per-touch
/// CRC because their size is below 3 MiB and the cost is negligible.
fn is_large_section(id: u8) -> bool {
    matches!(
        id,
        SECTION_TOPOLOGY
            | SECTION_COLUMNS
            | SECTION_EDGE_PROPS
            | SECTION_HNSW
            | SECTION_PROVENANCE
            | SECTION_IVF_STATE
    )
}

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
/// Small sections (IDS, SYMS, META, RULES_META, VIEWS) are CRC-checked on
/// first access. Large sections (TOPOLOGY, COLUMNS, EDGE_PROPS, HNSW,
/// PROVENANCE, IVF_STATE) skip automatic CRC; their rkyv accessors use
/// `rkyv::access_unchecked` (O(1) root-pointer lookup, no full-section walk).
/// Full integrity audit is available via `mushroomdb verify`.
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
        // SAFETY: `memmap2::Mmap` is created read-only (MAP_SHARED | PROT_READ).
        // On Linux and macOS the kernel ref-counts the underlying vnode; the fd
        // can be closed after mmap returns and the mapping remains valid for its
        // lifetime.  No mutable aliasing is possible because we never take a
        // `&mut` reference to the mapped bytes through this type.
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

    /// Check that every section listed in the directory fits within the backing
    /// buffer, and that the four large rkyv sections carry enough bytes for
    /// their rkyv root struct.  Pure pointer arithmetic — no bytes are read,
    /// no CRCs are computed, and no page faults are triggered.
    ///
    /// Used by `restore_v8_base` to detect truncated or corrupt snapshots
    /// eagerly at open time before the expensive section content reads are
    /// deferred.
    ///
    /// The minimum-size check closes the gap between `validate_section_bounds`
    /// (which only verifies `(offset, len)` fit in the file) and the
    /// individual accessor checks in `topology()` / `columns()` /
    /// `edge_props_section()` / `hnsw_section()`.  Without this check a
    /// crafted snapshot with `len = 1` for the TOPOLOGY section would pass
    /// bounds validation and then panic inside `topology().expect(...)` on the
    /// first query.  After this check all "bounds validated at open" expects
    /// become true post-validation invariants.
    pub fn validate_section_bounds(&self) -> Result<()> {
        for entry in &self.dir {
            let start = entry.offset as usize;
            let end = start
                .checked_add(entry.len as usize)
                .ok_or_else(|| GraphError::Corrupt {
                    detail: format!(
                        "v8: section {} length overflow (offset={}, len={})",
                        entry.id, entry.offset, entry.len
                    ),
                })?;
            self.backing
                .get(start..end)
                .ok_or_else(|| GraphError::Corrupt {
                    detail: format!(
                        "v8: section {} extends beyond file (end={}, file_len={})",
                        entry.id,
                        end,
                        self.backing.len()
                    ),
                })?;
            // Minimum rkyv root size check for the four large sections.
            // The individual accessors (topology(), columns(), etc.) already
            // guard on this, but only AFTER validate_section_bounds has
            // returned Ok.  Checking here prevents the expect()-on-Err panic
            // that would otherwise fire on the first query after open.
            if let Some(min) = min_rkyv_root_size(entry.id) {
                if (entry.len as usize) < min {
                    return Err(GraphError::Corrupt {
                        detail: format!(
                            "v8: section {} payload too small for rkyv root \
                             (len={}, minimum={})",
                            entry.id, entry.len, min
                        ),
                    });
                }
            }
        }
        Ok(())
    }

    /// Validate the CRC32 of every section and check rkyv-accessible sections
    /// for structural integrity.
    ///
    /// This is the on-demand integrity check exposed by `mushroomdb verify`.
    /// Large sections skip automatic CRC during normal operation; this method
    /// runs it explicitly.
    ///
    /// Returns one entry per directory section:
    /// `(section_id, section_name, bytes_checked, Ok(()) | Err(msg))`.
    pub fn verify_integrity(
        &self,
    ) -> Vec<(u8, &'static str, usize, std::result::Result<(), String>)> {
        let name = |id| match id {
            SECTION_TOPOLOGY => "topology",
            SECTION_COLUMNS => "columns",
            SECTION_IDS => "ids",
            SECTION_SYMS => "syms",
            SECTION_META => "meta",
            SECTION_EDGE_PROPS => "edge_props",
            SECTION_HNSW => "hnsw",
            SECTION_PROVENANCE => "provenance",
            SECTION_RULES_META => "rules_meta",
            SECTION_VIEWS => "views",
            SECTION_IVF_STATE => "ivf_state",
            SECTION_LAST_CHANGE => "last_change",
            _ => "unknown",
        };
        self.dir
            .iter()
            .map(|entry| {
                let id = entry.id;
                let start = entry.offset as usize;
                let end = match start.checked_add(entry.len as usize) {
                    Some(e) => e,
                    None => {
                        return (
                            id,
                            name(id),
                            0,
                            Err(format!("section {id}: length overflow")),
                        )
                    }
                };
                let bytes = match self.backing.get(start..end) {
                    Some(b) => b,
                    None => {
                        return (
                            id,
                            name(id),
                            0,
                            Err(format!("section {id}: extends beyond file")),
                        )
                    }
                };
                let computed = crc32fast::hash(bytes);
                if computed != entry.crc32 {
                    (
                        id,
                        name(id),
                        bytes.len(),
                        Err(format!(
                            "CRC mismatch (expected {:08x}, computed {:08x})",
                            entry.crc32, computed
                        )),
                    )
                } else {
                    (id, name(id), bytes.len(), Ok(()))
                }
            })
            .collect()
    }

    /// Return the raw bytes for `section_id`, validating its CRC32 lazily.
    /// Return the raw bytes for a section by ID.
    ///
    /// Exposed as `pub(crate)` so that `snapshot::decode_v8_from_mapped` can
    /// call `rkyv::access` (validated) for hostile-byte safety while the
    /// production seam path uses the `access_unchecked` accessors above.
    pub(crate) fn section_bytes(&self, section_id: u8) -> Result<&[u8]> {
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
        // Per-section timing when MUSHROOMDB_TRACE_OPEN is set.
        let _trace_t = if std::env::var("MUSHROOMDB_TRACE_OPEN").is_ok() {
            Some((section_id, std::time::Instant::now()))
        } else {
            None
        };

        // Lazy CRC validation — small sections only.
        //
        // Large sections (TOPOLOGY, COLUMNS, EDGE_PROPS, HNSW, PROVENANCE,
        // IVF_STATE) skip the per-touch CRC because a full-section hash at
        // hundreds of MiB costs 50–200 ms and is not necessary for memory
        // safety (bounds are validated at open by `validate_section_bounds`;
        // rkyv access is bounds-checked against the returned slice).
        // Use `mushroomdb verify` for explicit integrity audits.
        if !is_large_section(section_id) {
            let idx = section_id as usize;
            debug_assert!(
                idx < V8_MAGIC_SECTION_COUNT,
                "section_id {section_id} >= V8_MAGIC_SECTION_COUNT ({V8_MAGIC_SECTION_COUNT}); \
                 resize check_state before adding new section ids"
            );
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
        }
        if let Some((id, t)) = _trace_t {
            eprintln!(
                "[MUSHROOMDB_TRACE_OPEN] section_bytes({id}): {:>9.3?}",
                t.elapsed()
            );
        }
        Ok(bytes)
    }

    /// Zero-copy access to the archived topology (CSR).
    ///
    /// Uses `rkyv::access_unchecked` to avoid the O(section-size) pointer
    /// validation walk that `rkyv::access` performs. Section bounds are
    /// verified at open by `validate_section_bounds`; all CSR field accesses
    /// in `seam.rs` go through Rust bounds-checked slice indexing. File
    /// corruption is caught by `mushroomdb verify` (explicit full CRC32).
    pub fn topology(&self) -> Result<&ArchivedCsr> {
        let bytes = self.section_bytes(SECTION_TOPOLOGY)?;
        if bytes.len() < std::mem::size_of::<crate::v8::layout::ArchivedCsrData>() {
            return Err(GraphError::Corrupt {
                detail: "v8: topology section too short for rkyv root".to_string(),
            });
        }
        // SAFETY: (1) Checked bytes.len() >= size_of::<ArchivedCsrData>() above, so
        // root_position cannot underflow. (2) The backing mmap maps the full file with
        // PROT_READ; section bytes are a validated subslice. The encoder writes
        // self-contained sections: all rkyv relative pointers from `encode_v8` are
        // within-section. (3) This is sound for encoder-produced uncorrupted data.
        // However, a bit-flip on a relative-pointer field causes `ArchivedVec::as_slice`
        // to resolve an out-of-bounds address before any length check — genuine UB,
        // not a panic. Mitigated by `mushroomdb verify` (full-section CRC32 on demand)
        // and planned Miri/ASAN CI coverage.
        Ok(unsafe { rkyv::access_unchecked::<crate::v8::layout::ArchivedCsrData>(bytes) })
    }

    /// Zero-copy access to the archived column store.
    ///
    /// Uses `rkyv::access_unchecked`; see `topology()` for the full safety
    /// rationale. Per-field accesses in `ColumnsView` go through
    /// bounds-checked slice indexing and explicit length guards.
    pub fn columns(&self) -> Result<&ArchivedColumns> {
        let bytes = self.section_bytes(SECTION_COLUMNS)?;
        if bytes.len() < std::mem::size_of::<crate::v8::layout::ArchivedColumnsData>() {
            return Err(GraphError::Corrupt {
                detail: "v8: columns section too short for rkyv root".to_string(),
            });
        }
        // SAFETY: Minimum length checked above; encoder writes self-contained sections
        // with all relative pointers within-section. Sound for encoder-produced
        // uncorrupted data. A bit-flip on a relative-pointer field causes
        // `ArchivedVec::as_slice` to resolve an out-of-bounds address before any
        // length check — genuine UB, not a panic. Mitigated by `mushroomdb verify`
        // (full-section CRC32 on demand) and planned Miri/ASAN CI coverage.
        Ok(unsafe { rkyv::access_unchecked::<crate::v8::layout::ArchivedColumnsData>(bytes) })
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

    /// Zero-copy access to the archived edge properties (section 5).
    ///
    /// Uses `rkyv::access_unchecked`; see `topology()` for the full safety
    /// rationale. Per-edge property reads in `EdgePropsView` go through
    /// bounds-checked slice indexing.
    pub fn edge_props_section(&self) -> Result<&ArchivedEdgeProps> {
        let bytes = self.section_bytes(SECTION_EDGE_PROPS)?;
        if bytes.len() < std::mem::size_of::<crate::v8::layout::ArchivedEdgePropsData>() {
            return Err(GraphError::Corrupt {
                detail: "v8: edge_props section too short for rkyv root".to_string(),
            });
        }
        // SAFETY: Minimum length checked above; encoder writes self-contained sections
        // with all relative pointers within-section. Sound for encoder-produced
        // uncorrupted data. A bit-flip on a relative-pointer field causes
        // `ArchivedVec::as_slice` to resolve an out-of-bounds address before any
        // length check — genuine UB, not a panic. Mitigated by `mushroomdb verify`
        // (full-section CRC32 on demand) and planned Miri/ASAN CI coverage.
        Ok(unsafe { rkyv::access_unchecked::<crate::v8::layout::ArchivedEdgePropsData>(bytes) })
    }

    /// Zero-copy access to the archived HNSW section (section 6).
    ///
    /// Uses `rkyv::access_unchecked`; see `topology()` for the full safety
    /// rationale. Called once at first-use to load HNSW state into the engine.
    pub fn hnsw_section(&self) -> Result<&ArchivedHnsw> {
        let bytes = self.section_bytes(SECTION_HNSW)?;
        if bytes.len() < std::mem::size_of::<crate::v8::layout::ArchivedHnswSectionData>() {
            return Err(GraphError::Corrupt {
                detail: "v8: hnsw section too short for rkyv root".to_string(),
            });
        }
        // SAFETY: Minimum length checked above; encoder writes self-contained sections
        // with all relative pointers within-section. Sound for encoder-produced
        // uncorrupted data. A bit-flip on a relative-pointer field causes
        // `ArchivedVec::as_slice` to resolve an out-of-bounds address before any
        // length check — genuine UB, not a panic. Mitigated by `mushroomdb verify`
        // (full-section CRC32 on demand) and planned Miri/ASAN CI coverage.
        // The returned reference is immediately converted to owned data by
        // `archived_hnsw_to_owned`, so no aliasing persists after the call.
        Ok(unsafe { rkyv::access_unchecked::<crate::v8::layout::ArchivedHnswSectionData>(bytes) })
    }

    /// Zero-copy access to the archived rules meta (section 8).
    pub fn rules_meta_section(&self) -> Result<&ArchivedRulesMeta> {
        let bytes = self.section_bytes(SECTION_RULES_META)?;
        rkyv::access::<crate::v8::layout::ArchivedRulesMetaData, rkyv::rancor::Error>(bytes)
            .map_err(|e| GraphError::Corrupt {
                detail: format!("v8: rules_meta rkyv access: {e}"),
            })
    }

    /// Zero-copy access to the archived views (section 9).
    pub fn views_section(&self) -> Result<&ArchivedViews> {
        let bytes = self.section_bytes(SECTION_VIEWS)?;
        rkyv::access::<crate::v8::layout::ArchivedViewsSectionData, rkyv::rancor::Error>(bytes)
            .map_err(|e| GraphError::Corrupt {
                detail: format!("v8: views rkyv access: {e}"),
            })
    }

    /// Raw bytes for the IVF-state section (section 10).
    ///
    /// The caller retains these bytes without decoding until first use.
    /// Returns `Ok(&[])` when the section is absent from the directory
    /// (pre-T5 stores migrated from V5–V7 have no IVF section; treat as empty).
    /// Any other error (truncation, CRC mismatch) is propagated so that torn
    /// writes are detected rather than silently returning an empty map.
    pub fn ivf_bytes(&self) -> Result<&[u8]> {
        if self.dir.iter().all(|e| e.id != SECTION_IVF_STATE) {
            return Ok(&[]);
        }
        self.section_bytes(SECTION_IVF_STATE)
    }

    /// Raw bytes for the last-change section (section 11).
    ///
    /// Returns `Ok(&[])` when the section is absent from the directory
    /// (pre-Task-3 snapshots have no LAST_CHANGE section; treat as empty map).
    /// Any other error (truncation, CRC mismatch) is propagated.
    pub fn last_change_bytes(&self) -> Result<&[u8]> {
        if self.dir.iter().all(|e| e.id != SECTION_LAST_CHANGE) {
            return Ok(&[]);
        }
        self.section_bytes(SECTION_LAST_CHANGE)
    }

    /// Raw bytes for the edge-props section (section 5).
    /// Used for byte-identical passthrough when the overlay has no changes.
    pub fn edge_props_raw_bytes(&self) -> Result<&[u8]> {
        self.section_bytes(SECTION_EDGE_PROPS)
    }

    /// Raw bytes for the provenance section (section 7).
    /// Retained without decoding until first provenance access.
    pub fn provenance_raw_bytes(&self) -> Result<&[u8]> {
        self.section_bytes(SECTION_PROVENANCE)
    }
}

/// Minimum payload size for the four large rkyv-archived sections.
///
/// The rkyv root of an archived type must fit within the section payload;
/// `rkyv::access_unchecked` reads the root pointer at `bytes.len() -
/// size_of::<T::Archived>()`.  A section shorter than the root struct would
/// cause the accessor to attempt an out-of-bounds read.  We catch this at
/// open time in `validate_section_bounds` so the hot-path `expect()` calls
/// never fire on corrupt data.
fn min_rkyv_root_size(section_id: u8) -> Option<usize> {
    use crate::v8::layout::{
        ArchivedColumnsData, ArchivedCsrData, ArchivedEdgePropsData, ArchivedHnswSectionData,
    };
    match section_id {
        SECTION_TOPOLOGY => Some(std::mem::size_of::<ArchivedCsrData>()),
        SECTION_COLUMNS => Some(std::mem::size_of::<ArchivedColumnsData>()),
        SECTION_EDGE_PROPS => Some(std::mem::size_of::<ArchivedEdgePropsData>()),
        SECTION_HNSW => Some(std::mem::size_of::<ArchivedHnswSectionData>()),
        _ => None,
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
    use std::collections::{BTreeMap, HashMap};

    fn tiny_v8_meta() -> V8Meta {
        V8Meta {
            labels: vec![0, 0],
            edge_props: crate::edge_props::EdgeProps::new(),
            rule_defs: vec![],
            provenance: BTreeMap::new(),
            rule_tripped: BTreeMap::new(),
            rule_fires: BTreeMap::new(),
            ivf_bytes: Vec::new(),
            view_defs: vec![],
            wal_truncated: false,
            hnsw: BTreeMap::new(),
            last_change: HashMap::new(),
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
        encode_v8(
            None, None, None, None, &topo, &props, &ids, &syms, &meta, &mut out,
        )
        .expect("encode_v8");
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
        // Corrupt a SMALL section (SECTION_IDS=2) whose CRC is still checked
        // eagerly on access.  Large sections (e.g. TOPOLOGY=0) skip per-touch
        // CRC since v0.2.0; use verify_integrity() to audit them instead.
        let mut bytes = encode_tiny();
        // Directory starts at file offset 8.
        // Each entry is 16 bytes: {id:u8, _pad:[u8;3], offset:u32, len:u32, crc32:u32}
        let section_count = u16::from_le_bytes(bytes[6..8].try_into().unwrap()) as usize;
        let mut target_entry_base = None;
        for i in 0..section_count {
            let base = 8 + i * 16;
            if bytes[base] == SECTION_IDS {
                target_entry_base = Some(base);
                break;
            }
        }
        let entry_base = target_entry_base.expect("SECTION_IDS not found in encode_tiny output");
        // offset field is at entry_base + 4 .. entry_base + 8.
        let section_offset =
            u32::from_le_bytes(bytes[entry_base + 4..entry_base + 8].try_into().unwrap()) as usize;
        // Flip a byte inside the section payload.
        if section_offset < bytes.len() {
            bytes[section_offset] ^= 0xff;
        }
        let path = tmp_path("corrupt");
        std::fs::write(&path, &bytes).unwrap();
        let _cleanup = defer_remove(&path);
        match MappedBase::map(&path) {
            Ok(base) => {
                // Map succeeded (header ok). ids() must fail CRC.
                let result = base.ids();
                match result {
                    Err(GraphError::Corrupt { .. }) => {}
                    Err(e) => panic!("expected Corrupt, got {e:?}"),
                    Ok(_) => panic!("expected Corrupt error but ids() succeeded"),
                }
            }
            Err(GraphError::Corrupt { .. }) => {
                // Corruption may have hit the header — also acceptable.
            }
            Err(e) => panic!("unexpected error: {e:?}"),
        }
    }

    #[test]
    fn v8_verify_integrity_detects_large_section_corruption() {
        // verify_integrity() must catch corruption in large sections (e.g.
        // TOPOLOGY=0) even though section_bytes() skips their CRC.
        let mut bytes = encode_tiny();
        // Find SECTION_TOPOLOGY entry in directory.
        let section_count = u16::from_le_bytes(bytes[6..8].try_into().unwrap()) as usize;
        let mut target_entry_base = None;
        for i in 0..section_count {
            let base = 8 + i * 16;
            if bytes[base] == SECTION_TOPOLOGY {
                target_entry_base = Some(base);
                break;
            }
        }
        let entry_base = target_entry_base.expect("SECTION_TOPOLOGY not found");
        let section_offset =
            u32::from_le_bytes(bytes[entry_base + 4..entry_base + 8].try_into().unwrap()) as usize;
        if section_offset < bytes.len() {
            bytes[section_offset] ^= 0xff;
        }
        let path = tmp_path("corrupt_large");
        std::fs::write(&path, &bytes).unwrap();
        let _cleanup = defer_remove(&path);
        let base = MappedBase::map(&path).expect("map");
        // topology() should succeed (CRC skipped for large sections).
        let _ = base
            .topology()
            .expect("topology() must not CRC-fail large section");
        // verify_integrity() must catch it.
        let results = base.verify_integrity();
        let topo = results
            .iter()
            .find(|(id, _, _, _)| *id == SECTION_TOPOLOGY)
            .expect("topology entry in verify results");
        assert!(
            topo.3.is_err(),
            "verify_integrity must detect TOPOLOGY corruption; got Ok"
        );
    }

    /// A corrupt snapshot where the TOPOLOGY directory entry's `len` is set to 1
    /// (below the rkyv root minimum) must be rejected by `validate_section_bounds`.
    ///
    /// This is the targeted repro for the minimum-size validation gap: before the
    /// fix, `validate_section_bounds` only checked `(offset, len)` fits in the
    /// file, so a `len=1` for the TOPOLOGY section passed — then the first call to
    /// `topology().expect("bounds validated at open")` panicked because
    /// `topology()` returned `Err(Corrupt{too short for rkyv root})`.
    #[test]
    fn validate_section_bounds_rejects_tiny_section_len() {
        let mut bytes = encode_tiny();
        // Locate the SECTION_TOPOLOGY directory entry.
        let section_count = u16::from_le_bytes(bytes[6..8].try_into().unwrap()) as usize;
        let mut topo_entry_base = None;
        for i in 0..section_count {
            let base = 8 + i * 16;
            if bytes[base] == SECTION_TOPOLOGY {
                topo_entry_base = Some(base);
                break;
            }
        }
        let entry_base = topo_entry_base.expect("SECTION_TOPOLOGY in directory");
        // Overwrite len (entry_base+8..entry_base+12) with 1.
        let tiny_len: u32 = 1;
        bytes[entry_base + 8..entry_base + 12].copy_from_slice(&tiny_len.to_le_bytes());
        // Recompute the whole-header CRC so parse_header accepts it.
        let dir_end = 8 + section_count * 16;
        let new_crc = crc32fast::hash(&bytes[0..dir_end]);
        bytes[dir_end..dir_end + 4].copy_from_slice(&new_crc.to_le_bytes());
        // from_bytes validates only the header; validate_section_bounds (called
        // by restore_v8_base) is the step that catches the tiny payload.
        let base = MappedBase::from_bytes(bytes).expect("header CRC is correct after recompute");
        let result = base.validate_section_bounds();
        match result {
            Err(GraphError::Corrupt { detail }) => {
                assert!(
                    detail.contains("too small for rkyv root") || detail.contains("section"),
                    "error should mention tiny section; got: {detail}"
                );
            }
            Err(other) => panic!("expected Corrupt, got {other:?}"),
            Ok(_) => panic!("expected Err(Corrupt) for tiny section len, got Ok"),
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
