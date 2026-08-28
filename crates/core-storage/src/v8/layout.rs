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
/// Tag 5 (`Vector`) stores raw f64 runs for all-float list properties,
/// enabling zero-copy `&[f64]` access without boxing through `Value`.
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
    /// Raw f64 vector column (B2).  `dim` floats per node, stored contiguously.
    ///
    /// Layout: `data[id * dim .. (id + 1) * dim]` = the vector for node `id`.
    /// `present[word]` bit `bit` is set iff `id = word*64 + bit` has a value.
    /// Zero-copy `&[f64]` access via `ColumnsView::vector(id, field)`.
    ///
    /// **Overlay/archive asymmetry:** vectors written after the last snapshot
    /// live in the owned overlay as `Value::List([Value::Float, ...])` and are
    /// not accessible through `vector()`.  Callers must fall back to
    /// `ColumnsView::get()` which returns `ValueRef::Owned(Value::List(...))`.
    Vector {
        dim: u32,
        data: Vec<f64>,
        present: Vec<u64>,
    },
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
// Edge props (section 5)
// ---------------------------------------------------------------------------

/// One edge's property blob: (etype, src, dst) + bincode(BTreeMap<String, Value>).
/// Entries in `EdgePropsData` are sorted by (etype, src, dst) for binary search.
#[derive(Archive, Serialize, Deserialize, Clone, Debug)]
pub struct EdgePropEntry {
    pub etype: u32,
    pub src: u32,
    pub dst: u32,
    /// bincode-encoded `BTreeMap<String, Value>`.
    pub props_blob: Vec<u8>,
}

/// All archived edge properties, sorted by (etype, src, dst).
#[derive(Archive, Serialize, Deserialize, Clone, Debug)]
pub struct EdgePropsData {
    pub entries: Vec<EdgePropEntry>,
}

// ---------------------------------------------------------------------------
// HNSW (section 6)
// ---------------------------------------------------------------------------

/// Blob-per-rule HNSW storage.  Each entry holds the bincoded `HnswIndex`
/// for src and dst sides of one rule.  Stored as opaque bytes so core-storage
/// remains independent of core-rules; zero-copy blob slicing avoids loading
/// unused rules.
#[derive(Archive, Serialize, Deserialize, Clone, Debug)]
pub struct HnswRuleEntry {
    pub name: String,
    /// bincoded `HnswIndex` for the source side.
    pub src_blob: Vec<u8>,
    /// bincoded `HnswIndex` for the destination side.
    pub dst_blob: Vec<u8>,
}

/// All per-rule HNSW graph blobs, sorted by rule name.
#[derive(Archive, Serialize, Deserialize, Clone, Debug)]
pub struct HnswSectionData {
    pub rules: Vec<HnswRuleEntry>,
}

// ---------------------------------------------------------------------------
// Provenance (section 7)
// ---------------------------------------------------------------------------

/// A single provenance triple (etype, src, dst) serialized as three u32s.
#[derive(Archive, Serialize, Deserialize, Clone, Debug)]
pub struct Triple {
    pub etype: u32,
    pub src: u32,
    pub dst: u32,
}

/// Provenance triples for one rule, sorted by (etype, src, dst) ascending.
/// Binary search replaces BTreeSet iteration on the read path.
#[derive(Archive, Serialize, Deserialize, Clone, Debug)]
pub struct ProvenanceEntry {
    pub rule: String,
    pub triples: Vec<Triple>,
}

/// All per-rule provenance entries, sorted by rule name.
#[derive(Archive, Serialize, Deserialize, Clone, Debug)]
pub struct ProvenanceSectionData {
    pub entries: Vec<ProvenanceEntry>,
}

// ---------------------------------------------------------------------------
// Rules meta (section 8)
// ---------------------------------------------------------------------------

/// Tripped (budget-trip) flag for one rule.
#[derive(Archive, Serialize, Deserialize, Clone, Debug)]
pub struct RuleTripEntry {
    pub rule: String,
    pub tripped: bool,
}

/// Fire counter for one rule.
#[derive(Archive, Serialize, Deserialize, Clone, Debug)]
pub struct RuleFireEntry {
    pub rule: String,
    pub fires: u64,
}

/// All rule definitions, trip flags, and fire counters.
/// Entries in each sub-list are sorted by rule name.
#[derive(Archive, Serialize, Deserialize, Clone, Debug)]
pub struct RulesMetaData {
    /// One bincoded `RuleDef` per rule, in rule-name order.
    pub rule_defs: Vec<Vec<u8>>,
    pub tripped: Vec<RuleTripEntry>,
    pub fires: Vec<RuleFireEntry>,
}

// ---------------------------------------------------------------------------
// Views (section 9)
// ---------------------------------------------------------------------------

/// All materialized view definitions (one bincoded `ViewDef` per entry).
#[derive(Archive, Serialize, Deserialize, Clone, Debug)]
pub struct ViewsSectionData {
    pub view_defs: Vec<Vec<u8>>,
}

// ---------------------------------------------------------------------------
// Type aliases matching the binding interface
// ---------------------------------------------------------------------------

pub type ArchivedCsr = ArchivedCsrData;
pub type ArchivedColumns = ArchivedColumnsData;
pub type ArchivedIdMap = ArchivedIdMapData;
pub type ArchivedInterner = ArchivedInternerData;
pub type ArchivedEdgeProps = ArchivedEdgePropsData;
pub type ArchivedHnsw = ArchivedHnswSectionData;
pub type ArchivedProvenance = ArchivedProvenanceSectionData;
pub type ArchivedRulesMeta = ArchivedRulesMetaData;
pub type ArchivedViews = ArchivedViewsSectionData;
