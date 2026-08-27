//! rkyv-archived layout types for the V8 snapshot format.
//!
//! All primitive fields archive as little-endian via rend (rkyv's LE-primitive
//! crate), which is the default for rkyv 0.8 on all targets.  The format is
//! therefore architecture-portable and LE-pinned.

use rkyv::{Archive, Deserialize, Serialize};

// ---------------------------------------------------------------------------
// CSR topology
// ---------------------------------------------------------------------------

/// One vertex's sorted adjacency list within an edge type and direction.
#[derive(Archive, Serialize, Deserialize, Clone, Debug)]
pub struct CsrRow {
    pub vertex: u32,
    /// Sorted, unique neighbor ids.
    pub neighbors: Vec<u32>,
}

/// All adjacency rows for one direction (out or in) of one edge type.
/// Rows are sorted by `vertex` ascending so lookups can binary-search.
#[derive(Archive, Serialize, Deserialize, Clone, Debug)]
pub struct CsrAdjMap {
    pub rows: Vec<CsrRow>,
}

/// Full typed adjacency (out + in) for a single edge type.
/// `etype` is the intern id of the edge-type label.
#[derive(Archive, Serialize, Deserialize, Clone, Debug)]
pub struct CsrEtype {
    pub etype: u32,
    pub out_adj: CsrAdjMap,
    pub in_adj: CsrAdjMap,
}

/// The full archived CSR topology: all edge types, sorted ascending by etype.
#[derive(Archive, Serialize, Deserialize, Clone, Debug)]
pub struct CsrData {
    /// Sorted by `etype` so lookups can binary-search.
    pub etypes: Vec<CsrEtype>,
    pub edge_count: u64,
}

// ---------------------------------------------------------------------------
// Columns
// ---------------------------------------------------------------------------

/// Typed column payload.  Mixed/list columns fall back to a bincode blob.
/// For Task 1, vectors are stored via the Mixed path; Task 2 adds a dedicated
/// raw-f64 column type.
#[derive(Archive, Serialize, Deserialize, Clone, Debug)]
pub enum ColumnData {
    /// Dense i64 column.  `present` is the presence bitmap as u64 words (LE).
    Int { data: Vec<i64>, present: Vec<u64> },
    /// Dense f64 column.
    Float { data: Vec<f64>, present: Vec<u64> },
    /// Dense bool column stored as u8 (0/1).
    Bool { data: Vec<u8>, present: Vec<u64> },
    /// String column: ids index into `strings`; `present` is the presence bitmap.
    Str {
        ids: Vec<u32>,
        present: Vec<u64>,
        strings: Vec<String>,
    },
    /// Mixed/list column: bincode-encoded `HashMap<u32, Value>`.
    Mixed(Vec<u8>),
}

/// One field entry: name + typed column.
#[derive(Archive, Serialize, Deserialize, Clone, Debug)]
pub struct FieldEntry {
    pub name: String,
    pub col: ColumnData,
}

/// The full archived column store.  Fields are sorted by name for determinism.
#[derive(Archive, Serialize, Deserialize, Clone, Debug)]
pub struct ColumnsData {
    pub fields: Vec<FieldEntry>,
}

// ---------------------------------------------------------------------------
// IdMap
// ---------------------------------------------------------------------------

/// Archived id map: dense-allocated key → id table plus tombstones.
#[derive(Archive, Serialize, Deserialize, Clone, Debug)]
pub struct IdMapData {
    /// Keys in dense id order (to_key[id] = key).
    pub to_key: Vec<String>,
    /// Permanently retired ids, sorted ascending.
    pub tombstones: Vec<u32>,
}

// ---------------------------------------------------------------------------
// Interner
// ---------------------------------------------------------------------------

/// Archived symbol interner: symbol ids map to their string names.
#[derive(Archive, Serialize, Deserialize, Clone, Debug)]
pub struct InternerData {
    /// to_str[sym] = name.
    pub to_str: Vec<String>,
}

// ---------------------------------------------------------------------------
// Type aliases matching the binding interface
// ---------------------------------------------------------------------------

pub type ArchivedCsr = ArchivedCsrData;
pub type ArchivedColumns = ArchivedColumnsData;
pub type ArchivedIdMap = ArchivedIdMapData;
pub type ArchivedInterner = ArchivedInternerData;
