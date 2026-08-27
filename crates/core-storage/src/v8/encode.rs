//! `encode_v8` — write a V8 mmap-able snapshot, and helpers to reconstruct
//! owned types from the archived sections.
//!
//! Wire format (all integers LE):
//!
//! ```text
//! [0..4]       MAGIC "GDB1"
//! [4..6]       VERSION = 8 (u16 LE)
//! [6..8]       section_count (u16 LE)
//! [8..8+16*N]  directory: {id:u8, _pad:[u8;3], offset:u32, len:u32, crc32:u32} * N
//! [8+16*N..+4] whole-header crc32 (over bytes 0..8+16*N)
//! [..4096]     zero padding to complete the header page
//! sections at 8-byte aligned offsets from file start (>= 4096)
//! ```
//!
//! Section ids:
//!   0 = CSR topology (rkyv)
//!   1 = columns (rkyv)
//!   2 = id map (rkyv)
//!   3 = interner (rkyv)
//!   4 = meta (bincode V8Meta)

use crate::columns::ColumnStore;
use crate::edge_props::EdgeProps;
use crate::idmap::IdMap;
use crate::interner::Interner;
use crate::snapshot::PerRuleIvfState;
use crate::topology::Topology;
use crate::types::{GraphError, Result, Value};
use crate::v8::layout::{
    ColumnData, ColumnsData, CsrAdjMap, CsrData, CsrEtype, CsrRow, EdgePropEntry, EdgePropsData,
    FieldEntry, HnswRuleEntry, HnswSectionData, IdMapData, InternerData, ProvenanceEntry,
    ProvenanceSectionData, RuleFireEntry, RuleTripEntry, RulesMetaData, Triple, ViewsSectionData,
};
use crate::v8::{
    HEADER_SIZE, SECTION_COLUMNS, SECTION_EDGE_PROPS, SECTION_HNSW, SECTION_IDS, SECTION_META,
    SECTION_PROVENANCE, SECTION_RULES_META, SECTION_SYMS, SECTION_TOPOLOGY, SECTION_VIEWS,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write;

// ---------------------------------------------------------------------------
// V8Meta — bincode-serialized section 4
// ---------------------------------------------------------------------------

/// Metadata that does not fit in the rkyv sections; serialized as bincode.
#[derive(Serialize, Deserialize, Default)]
pub struct V8Meta {
    pub labels: Vec<u32>,
    pub edge_props: EdgeProps,
    pub rule_defs: Vec<Vec<u8>>,
    pub provenance: BTreeMap<String, BTreeSet<(u32, u32, u32)>>,
    pub rule_tripped: BTreeMap<String, bool>,
    pub rule_fires: BTreeMap<String, u64>,
    pub ivf_state: BTreeMap<String, PerRuleIvfState>,
    pub view_defs: Vec<Vec<u8>>,
    pub wal_truncated: bool,
    pub hnsw: BTreeMap<String, (Vec<u8>, Vec<u8>)>,
}

// ---------------------------------------------------------------------------
// encode_v8
// ---------------------------------------------------------------------------

/// Encode an in-memory graph state as a V8 snapshot, writing to `out`.
///
/// `base` is the existing mapped snapshot (for Task-3 merge path).  For the
/// initial encode (Task 1), pass `None`.  Only the overlay fields are used.
pub fn encode_v8<W: Write>(
    base: Option<&crate::v8::MappedBase>,
    overlay_topo: &Topology,
    overlay_props: &ColumnStore,
    overlay_ids: &IdMap,
    overlay_syms: &Interner,
    meta: &V8Meta,
    out: &mut W,
) -> Result<()> {
    // Base is reserved for the Task-3 merge path; full encode from overlay for now.
    let _ = base;

    // 1. Serialise each section to bytes.
    let topo_bytes = rkyv_encode(&topology_to_csr(overlay_topo))?;
    let cols_bytes = rkyv_encode(&columnstore_to_data(overlay_props)?)?;
    let ids_bytes = rkyv_encode(&idmap_to_data(overlay_ids))?;
    let syms_bytes = rkyv_encode(&interner_to_data(overlay_syms))?;
    let meta_bytes = bincode::serialize(meta).map_err(|e| GraphError::Corrupt {
        detail: format!("v8: meta bincode serialize: {e}"),
    })?;
    // New Task-2 sections (5-9):
    let edge_props_bytes = rkyv_encode(&edge_props_to_data(&meta.edge_props))?;
    let hnsw_bytes = rkyv_encode(&hnsw_to_data(&meta.hnsw))?;
    let prov_bytes = rkyv_encode(&provenance_to_data(&meta.provenance))?;
    let rules_meta_bytes = rkyv_encode(&rules_meta_to_data(
        &meta.rule_defs,
        &meta.rule_tripped,
        &meta.rule_fires,
    ))?;
    let views_bytes = rkyv_encode(&ViewsSectionData {
        view_defs: meta.view_defs.clone(),
    })?;

    let sections: &[(u8, &[u8])] = &[
        (SECTION_TOPOLOGY, &topo_bytes),
        (SECTION_COLUMNS, &cols_bytes),
        (SECTION_IDS, &ids_bytes),
        (SECTION_SYMS, &syms_bytes),
        (SECTION_META, &meta_bytes),
        (SECTION_EDGE_PROPS, &edge_props_bytes),
        (SECTION_HNSW, &hnsw_bytes),
        (SECTION_PROVENANCE, &prov_bytes),
        (SECTION_RULES_META, &rules_meta_bytes),
        (SECTION_VIEWS, &views_bytes),
    ];
    let n = sections.len();

    // 2. Compute section offsets (sections start after the 4 KB header page).
    let mut offsets = Vec::with_capacity(n);
    let mut cur: u64 = HEADER_SIZE as u64;
    for (_, bytes) in sections {
        offsets.push(cur);
        cur = align8(cur + bytes.len() as u64);
    }

    // Verify that all offsets and lengths fit in u32.
    for (i, ((_, bytes), &offset)) in sections.iter().zip(offsets.iter()).enumerate() {
        if offset > u32::MAX as u64 {
            return Err(GraphError::Corrupt {
                detail: format!("v8: section {i} offset {offset} exceeds u32"),
            });
        }
        if bytes.len() > u32::MAX as usize {
            return Err(GraphError::Corrupt {
                detail: format!("v8: section {i} length {} exceeds u32", bytes.len()),
            });
        }
    }

    // 3. Build the 4 KB header page.
    let mut header = vec![0u8; HEADER_SIZE];
    header[0..4].copy_from_slice(b"GDB1");
    header[4..6].copy_from_slice(&8u16.to_le_bytes());
    header[6..8].copy_from_slice(&(n as u16).to_le_bytes());

    let mut pos = 8usize;
    for ((section_id, bytes), &offset) in sections.iter().zip(offsets.iter()) {
        let crc32 = crc32fast::hash(bytes);
        header[pos] = *section_id;
        header[pos + 1] = 0;
        header[pos + 2] = 0;
        header[pos + 3] = 0;
        header[pos + 4..pos + 8].copy_from_slice(&(offset as u32).to_le_bytes());
        header[pos + 8..pos + 12].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
        header[pos + 12..pos + 16].copy_from_slice(&crc32.to_le_bytes());
        pos += 16;
    }
    // Whole-header CRC over magic..last directory entry.
    let header_crc = crc32fast::hash(&header[0..pos]);
    header[pos..pos + 4].copy_from_slice(&header_crc.to_le_bytes());
    // Remaining bytes are already zero.

    out.write_all(&header).map_err(GraphError::Io)?;

    // 4. Write sections with 8-byte alignment padding between sections.
    // Exactly what is CRC-covered:
    //   • The 4 KB header page bytes [0 .. dir_end] are covered by the
    //     whole-header CRC32 stored at header[dir_end..dir_end+4].
    //   • Each section payload [offset .. offset+len] is covered by the
    //     per-section CRC32 in its directory entry.
    //   • The zero-pad bytes written between sections for 8-byte alignment
    //     are NOT part of any section's [offset+len] range and are therefore
    //     NOT covered by any CRC; they are always zero by construction.
    //   • The last section is NOT padded: the file ends exactly at the last
    //     section's final byte so there are no unchecked trailing bytes.
    let zero_pad = [0u8; 8];
    let last_idx = sections.len().saturating_sub(1);
    for (i, ((_, bytes), &offset)) in sections.iter().zip(offsets.iter()).enumerate() {
        out.write_all(bytes).map_err(GraphError::Io)?;
        if i < last_idx {
            let end = offset + bytes.len() as u64;
            let next = align8(end);
            let pad = (next - end) as usize;
            if pad > 0 {
                out.write_all(&zero_pad[..pad]).map_err(GraphError::Io)?;
            }
        }
    }

    Ok(())
}

fn align8(n: u64) -> u64 {
    (n + 7) & !7
}

// ---------------------------------------------------------------------------
// rkyv serialization helper
// ---------------------------------------------------------------------------

fn rkyv_encode<T>(value: &T) -> Result<Vec<u8>>
where
    T: for<'a> rkyv::Serialize<
        rkyv::api::high::HighSerializer<
            rkyv::util::AlignedVec,
            rkyv::ser::allocator::ArenaHandle<'a>,
            rkyv::rancor::Error,
        >,
    >,
{
    rkyv::api::high::to_bytes::<rkyv::rancor::Error>(value)
        .map(|av| av.to_vec())
        .map_err(|e| GraphError::Corrupt {
            detail: format!("v8: rkyv encode: {e}"),
        })
}

// ---------------------------------------------------------------------------
// Build rkyv types from owned graph types
// ---------------------------------------------------------------------------

fn topology_to_csr(topo: &Topology) -> CsrData {
    let mut buf = Vec::new();
    topo.pack(&mut buf);
    topology_from_pack(&buf, topo.edge_count())
}

fn topology_from_pack(buf: &[u8], edge_count: u64) -> CsrData {
    use crate::pack::{read_u32, read_u64};
    let mut pos = 0usize;
    let n_etypes = match read_u32(buf, &mut pos) {
        Ok(n) => n as usize,
        Err(_) => {
            return CsrData {
                etypes: vec![],
                edge_count: 0,
            }
        }
    };
    let mut etypes = Vec::with_capacity(n_etypes);
    for _ in 0..n_etypes {
        let et = match read_u32(buf, &mut pos) {
            Ok(v) => v,
            Err(_) => break,
        };
        let out_adj = unpack_adj_rows(buf, &mut pos);
        let in_adj = unpack_adj_rows(buf, &mut pos);
        etypes.push(CsrEtype {
            etype: et,
            out_adj,
            in_adj,
        });
    }
    let _ = read_u64(buf, &mut pos); // skip pack's own edge_count
    CsrData { etypes, edge_count }
}

fn unpack_adj_rows(buf: &[u8], pos: &mut usize) -> CsrAdjMap {
    use crate::pack::{read_u32, read_u32s};
    let n = match read_u32(buf, pos) {
        Ok(v) => v as usize,
        Err(_) => return CsrAdjMap { rows: vec![] },
    };
    let mut rows = Vec::with_capacity(n);
    for _ in 0..n {
        let v = match read_u32(buf, pos) {
            Ok(v) => v,
            Err(_) => break,
        };
        let neighbors = match read_u32s(buf, pos) {
            Ok(v) => v,
            Err(_) => break,
        };
        rows.push(CsrRow {
            vertex: v,
            neighbors,
        });
    }
    CsrAdjMap { rows }
}

fn columnstore_to_data(store: &ColumnStore) -> Result<ColumnsData> {
    let mut buf = Vec::new();
    store.pack(&mut buf);
    let mut data = decode_all_columns(&buf)?;
    // Post-process: promote Mixed columns that are pure all-float lists of equal
    // dimension to ColumnData::Vector for zero-copy raw-f64 access (B2).
    for field in data.fields.iter_mut() {
        if let ColumnData::Mixed(ref blob) = field.col {
            let map: HashMap<u32, Value> =
                bincode::deserialize(blob.as_slice()).unwrap_or_default();
            if let Some(vec_col) = try_promote_to_vector(&map) {
                field.col = vec_col;
            }
        }
    }
    Ok(data)
}

/// Try to promote a Mixed column map to `ColumnData::Vector` if all values are
/// `Value::List([Value::Float, ...])` with the same non-zero dimension.
///
/// Returns `None` if the column cannot be promoted (empty map, mixed types,
/// non-float list elements, or inconsistent dimension).
fn try_promote_to_vector(map: &HashMap<u32, Value>) -> Option<ColumnData> {
    if map.is_empty() {
        return None;
    }
    // Determine dimension from the first entry.
    let dim = map.values().next().and_then(|v| match v {
        Value::List(items) => {
            if items.iter().all(|i| matches!(i, Value::Float(_))) {
                Some(items.len())
            } else {
                None
            }
        }
        _ => None,
    })?;
    if dim == 0 {
        return None;
    }
    // Validate all entries: must be float lists of the same dimension.
    let max_id = *map.keys().max().unwrap_or(&0) as usize;
    for v in map.values() {
        match v {
            Value::List(items)
                if items.len() == dim && items.iter().all(|i| matches!(i, Value::Float(_))) => {}
            _ => return None,
        }
    }
    // Build the dense vector array.
    let n_nodes = max_id + 1;
    let mut data = vec![0.0f64; n_nodes * dim];
    let mut present_words = vec![0u64; n_nodes.div_ceil(64)];
    for (&id, v) in map {
        let items = match v {
            Value::List(items) => items,
            _ => unreachable!(),
        };
        let start = id as usize * dim;
        for (i, item) in items.iter().enumerate() {
            if let Value::Float(f) = item {
                data[start + i] = *f;
            }
        }
        let word = id as usize / 64;
        let bit = id as usize % 64;
        present_words[word] |= 1u64 << bit;
    }
    Some(ColumnData::Vector {
        dim: dim as u32,
        data,
        present: present_words,
    })
}

fn decode_all_columns(buf: &[u8]) -> Result<ColumnsData> {
    use crate::pack::{read_exact, read_f64s, read_i64s, read_str, read_u32, read_u32s};
    let mut pos = 0usize;

    // StrIntern table: string count, then each string.
    let n_intern = read_u32(buf, &mut pos).map_err(|_| GraphError::Corrupt {
        detail: "v8: columns pack: truncated intern-string count".into(),
    })? as usize;
    let mut intern_strings: Vec<String> = Vec::with_capacity(n_intern);
    for i in 0..n_intern {
        intern_strings.push(read_str(buf, &mut pos).map_err(|_| GraphError::Corrupt {
            detail: format!("v8: columns pack: truncated intern-string[{i}]"),
        })?);
    }

    let n_fields = read_u32(buf, &mut pos).map_err(|_| GraphError::Corrupt {
        detail: "v8: columns pack: truncated field count".into(),
    })? as usize;
    let mut fields = Vec::with_capacity(n_fields);

    for field_idx in 0..n_fields {
        let fname = read_str(buf, &mut pos).map_err(|_| GraphError::Corrupt {
            detail: format!("v8: columns pack: truncated field name at index {field_idx}"),
        })?;
        let tag = read_exact(buf, &mut pos, 1).map_err(|_| GraphError::Corrupt {
            detail: format!("v8: column '{fname}' pack decode: truncated tag byte"),
        })?[0];
        let col = match tag {
            0 => {
                let data = read_i64s(buf, &mut pos).map_err(|_| GraphError::Corrupt {
                    detail: format!("v8: column '{fname}' pack decode: truncated Int data"),
                })?;
                let present = unpack_bitmap(buf, &mut pos).ok_or_else(|| GraphError::Corrupt {
                    detail: format!("v8: column '{fname}' pack decode: truncated Int bitmap"),
                })?;
                ColumnData::Int { data, present }
            }
            1 => {
                let data = read_f64s(buf, &mut pos).map_err(|_| GraphError::Corrupt {
                    detail: format!("v8: column '{fname}' pack decode: truncated Float data"),
                })?;
                let present = unpack_bitmap(buf, &mut pos).ok_or_else(|| GraphError::Corrupt {
                    detail: format!("v8: column '{fname}' pack decode: truncated Float bitmap"),
                })?;
                ColumnData::Float { data, present }
            }
            2 => {
                let n = read_u32(buf, &mut pos).map_err(|_| GraphError::Corrupt {
                    detail: format!("v8: column '{fname}' pack decode: truncated Bool length"),
                })? as usize;
                let raw = read_exact(buf, &mut pos, n)
                    .map_err(|_| GraphError::Corrupt {
                        detail: format!("v8: column '{fname}' pack decode: truncated Bool data"),
                    })?
                    .to_vec();
                let present = unpack_bitmap(buf, &mut pos).ok_or_else(|| GraphError::Corrupt {
                    detail: format!("v8: column '{fname}' pack decode: truncated Bool bitmap"),
                })?;
                ColumnData::Bool { data: raw, present }
            }
            3 => {
                let ids = read_u32s(buf, &mut pos).map_err(|_| GraphError::Corrupt {
                    detail: format!("v8: column '{fname}' pack decode: truncated Str ids"),
                })?;
                let present = unpack_bitmap(buf, &mut pos).ok_or_else(|| GraphError::Corrupt {
                    detail: format!("v8: column '{fname}' pack decode: truncated Str bitmap"),
                })?;
                ColumnData::Str {
                    ids,
                    present,
                    strings: intern_strings.clone(),
                }
            }
            4 => {
                let blob_len = read_u32(buf, &mut pos).map_err(|_| GraphError::Corrupt {
                    detail: format!("v8: column '{fname}' pack decode: truncated Mixed length"),
                })? as usize;
                let blob = read_exact(buf, &mut pos, blob_len)
                    .map_err(|_| GraphError::Corrupt {
                        detail: format!("v8: column '{fname}' pack decode: truncated Mixed data"),
                    })?
                    .to_vec();
                ColumnData::Mixed(blob)
            }
            other => {
                return Err(GraphError::Corrupt {
                    detail: format!(
                        "v8: column '{fname}' pack decode: unknown tag byte {other} at field \
                         index {field_idx}"
                    ),
                });
            }
        };
        fields.push(FieldEntry { name: fname, col });
    }

    fields.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(ColumnsData { fields })
}

fn unpack_bitmap(buf: &[u8], pos: &mut usize) -> Option<Vec<u64>> {
    use crate::pack::{read_exact, read_u32};
    let n = read_u32(buf, pos).ok()? as usize;
    let bytes = read_exact(buf, pos, n.saturating_mul(8)).ok()?;
    Some(
        bytes
            .chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
            .collect(),
    )
}

fn idmap_to_data(ids: &IdMap) -> IdMapData {
    let to_key: Vec<String> = ids.all_keys().to_vec();
    let tombstones: Vec<u32> = ids
        .all_keys()
        .iter()
        .enumerate()
        .filter_map(|(i, _)| {
            let id = i as u32;
            if ids.is_tombstoned(id) {
                Some(id)
            } else {
                None
            }
        })
        .collect();
    IdMapData { to_key, tombstones }
}

fn interner_to_data(syms: &Interner) -> InternerData {
    let n = syms.len();
    let mut to_str = Vec::with_capacity(n);
    for i in 0u32..n as u32 {
        to_str.push(syms.resolve(i).unwrap_or("").to_string());
    }
    InternerData { to_str }
}

// ---------------------------------------------------------------------------
// Reconstruct owned types from archived sections
// ---------------------------------------------------------------------------

/// Reconstruct a `Topology` from an archived CSR section.
pub fn csr_to_topology(archived: &crate::v8::layout::ArchivedCsr) -> Topology {
    let mut topo = Topology::new();
    for et_entry in archived.etypes.iter() {
        let et = u32::from(et_entry.etype);
        for row in et_entry.out_adj.rows.iter() {
            let src = u32::from(row.vertex);
            for &nbr in row.neighbors.iter() {
                let dst = u32::from(nbr);
                topo.add_edge(et, src, dst);
            }
        }
    }
    topo
}

/// Reconstruct a `ColumnStore` from archived columns.
pub fn archived_to_columnstore(archived: &crate::v8::layout::ArchivedColumns) -> ColumnStore {
    let mut store = ColumnStore::new();
    for field in archived.fields.iter() {
        let name: &str = field.name.as_str();
        match &field.col {
            crate::v8::layout::ArchivedColumnData::Int { data, present } => {
                bitmap_for_each(present.as_slice(), |node| {
                    let idx = node as usize;
                    if idx < data.len() {
                        store.set(node, name, Value::Int(i64::from(data[idx])));
                    }
                });
            }
            crate::v8::layout::ArchivedColumnData::Float { data, present } => {
                bitmap_for_each(present.as_slice(), |node| {
                    let idx = node as usize;
                    if idx < data.len() {
                        store.set(node, name, Value::Float(f64::from(data[idx])));
                    }
                });
            }
            crate::v8::layout::ArchivedColumnData::Bool { data, present } => {
                bitmap_for_each(present.as_slice(), |node| {
                    let idx = node as usize;
                    if idx < data.len() {
                        store.set(node, name, Value::Bool(data[idx] != 0));
                    }
                });
            }
            crate::v8::layout::ArchivedColumnData::Str {
                ids,
                present,
                strings,
            } => {
                let strings_vec: Vec<String> =
                    strings.iter().map(|s| s.as_str().to_string()).collect();
                bitmap_for_each(present.as_slice(), |node| {
                    let idx = node as usize;
                    if idx < ids.len() {
                        let sid = u32::from(ids[idx]) as usize;
                        if sid < strings_vec.len() {
                            store.set(node, name, Value::Str(strings_vec[sid].clone()));
                        }
                    }
                });
            }
            crate::v8::layout::ArchivedColumnData::Mixed(blob) => {
                let map: HashMap<u32, Value> =
                    bincode::deserialize(blob.as_slice()).unwrap_or_default();
                for (node, v) in map {
                    store.set(node, name, v);
                }
            }
            crate::v8::layout::ArchivedColumnData::Vector { dim, data, present } => {
                let dim_val = u32::from(*dim) as usize;
                bitmap_for_each(present.as_slice(), |node| {
                    let start = node as usize * dim_val;
                    let end = start + dim_val;
                    if end <= data.len() {
                        let floats: Vec<Value> = data[start..end]
                            .iter()
                            .map(|f| Value::Float(f64::from(*f)))
                            .collect();
                        store.set(node, name, Value::List(floats));
                    }
                });
            }
        }
    }
    store
}

fn bitmap_for_each<F: FnMut(u32)>(words: &[rkyv::Archived<u64>], mut f: F) {
    for (wi, word) in words.iter().enumerate() {
        let mut w: u64 = u64::from(*word);
        while w != 0 {
            let bit = w.trailing_zeros();
            f(wi as u32 * 64 + bit);
            w &= w - 1;
        }
    }
}

/// Reconstruct an `IdMap` from archived data.
pub fn archived_to_idmap(archived: &crate::v8::layout::ArchivedIdMap) -> IdMap {
    let mut ids = IdMap::new();
    let tombstoned: BTreeSet<u32> = archived.tombstones.iter().map(|t| u32::from(*t)).collect();
    for (i, key) in archived.to_key.iter().enumerate() {
        let s = key.as_str();
        if tombstoned.contains(&(i as u32)) {
            ids.get_or_insert(s);
            ids.delete(s);
        } else if !s.is_empty() {
            ids.get_or_insert(s);
        }
    }
    ids
}

/// Reconstruct an `Interner` from archived data.
pub fn archived_to_interner(archived: &crate::v8::layout::ArchivedInterner) -> Interner {
    let mut syms = Interner::new();
    for s in archived.to_str.iter() {
        syms.intern(s.as_str());
    }
    syms
}

/// Decode the `V8Meta` from the bincode meta section bytes.
pub fn decode_meta(bytes: &[u8]) -> Result<V8Meta> {
    bincode::deserialize(bytes).map_err(|e| GraphError::Corrupt {
        detail: format!("v8: meta bincode deserialize: {e}"),
    })
}

// ---------------------------------------------------------------------------
// Task-2 section encoders
// ---------------------------------------------------------------------------

/// Build `EdgePropsData` from owned `EdgeProps`.
/// Entries are sorted by (etype, src, dst) for binary search on read.
fn edge_props_to_data(ep: &EdgeProps) -> EdgePropsData {
    let mut entries: Vec<EdgePropEntry> = ep
        .sorted_entries()
        .into_iter()
        .map(|(etype, src, dst, props)| {
            let props_blob = bincode::serialize(&props).unwrap_or_default();
            EdgePropEntry {
                etype,
                src,
                dst,
                props_blob,
            }
        })
        .collect();
    entries.sort_by_key(|e| (e.etype, e.src, e.dst));
    EdgePropsData { entries }
}

/// Build `HnswSectionData` from the blobs map in V8Meta.
/// Rules are sorted by name.
fn hnsw_to_data(hnsw: &BTreeMap<String, (Vec<u8>, Vec<u8>)>) -> HnswSectionData {
    let mut rules: Vec<HnswRuleEntry> = hnsw
        .iter()
        .map(|(name, (src, dst))| HnswRuleEntry {
            name: name.clone(),
            src_blob: src.clone(),
            dst_blob: dst.clone(),
        })
        .collect();
    rules.sort_by(|a, b| a.name.cmp(&b.name));
    HnswSectionData { rules }
}

/// Build `ProvenanceSectionData` from owned provenance map.
/// Triples within each rule are sorted by (etype, src, dst).
fn provenance_to_data(prov: &BTreeMap<String, BTreeSet<(u32, u32, u32)>>) -> ProvenanceSectionData {
    let mut entries: Vec<ProvenanceEntry> = prov
        .iter()
        .map(|(rule, triples)| {
            let mut sorted: Vec<Triple> = triples
                .iter()
                .map(|&(etype, src, dst)| Triple { etype, src, dst })
                .collect();
            // BTreeSet is already sorted, but make explicit for clarity.
            sorted.sort_by_key(|t| (t.etype, t.src, t.dst));
            ProvenanceEntry {
                rule: rule.clone(),
                triples: sorted,
            }
        })
        .collect();
    entries.sort_by(|a, b| a.rule.cmp(&b.rule));
    ProvenanceSectionData { entries }
}

/// Build `RulesMetaData` from owned rule metadata.
/// Entries sorted by rule name within each sub-list.
fn rules_meta_to_data(
    rule_defs: &[Vec<u8>],
    tripped: &BTreeMap<String, bool>,
    fires: &BTreeMap<String, u64>,
) -> RulesMetaData {
    let tripped_vec: Vec<RuleTripEntry> = tripped
        .iter()
        .map(|(rule, &t)| RuleTripEntry {
            rule: rule.clone(),
            tripped: t,
        })
        .collect();
    let fires_vec: Vec<RuleFireEntry> = fires
        .iter()
        .map(|(rule, &f)| RuleFireEntry {
            rule: rule.clone(),
            fires: f,
        })
        .collect();
    RulesMetaData {
        rule_defs: rule_defs.to_vec(),
        tripped: tripped_vec,
        fires: fires_vec,
    }
}

// ---------------------------------------------------------------------------
// Decode helpers for Task-2 sections
// ---------------------------------------------------------------------------

/// Decode `EdgePropsData` back into `EdgeProps`.
pub fn archived_edge_props_to_owned(archived: &crate::v8::layout::ArchivedEdgeProps) -> EdgeProps {
    let mut ep = EdgeProps::new();
    for entry in archived.entries.iter() {
        let etype = u32::from(entry.etype);
        let src = u32::from(entry.src);
        let dst = u32::from(entry.dst);
        let props: std::collections::BTreeMap<String, Value> =
            bincode::deserialize(entry.props_blob.as_slice()).unwrap_or_default();
        for (field, value) in props {
            ep.set(etype, src, dst, &field, value);
        }
    }
    ep
}

/// Decode `ProvenanceSectionData` back into a `BTreeMap<String, BTreeSet<(u32,u32,u32)>>`.
pub fn archived_provenance_to_owned(
    archived: &crate::v8::layout::ArchivedProvenance,
) -> BTreeMap<String, BTreeSet<(u32, u32, u32)>> {
    let mut map = BTreeMap::new();
    for entry in archived.entries.iter() {
        let rule = entry.rule.as_str().to_string();
        let triples: BTreeSet<(u32, u32, u32)> = entry
            .triples
            .iter()
            .map(|t| (u32::from(t.etype), u32::from(t.src), u32::from(t.dst)))
            .collect();
        map.insert(rule, triples);
    }
    map
}

/// Return type for `archived_rules_meta_to_owned`.
pub type RulesMetaOwned = (Vec<Vec<u8>>, BTreeMap<String, bool>, BTreeMap<String, u64>);

/// Decode `RulesMetaData` back into separate collections.
pub fn archived_rules_meta_to_owned(
    archived: &crate::v8::layout::ArchivedRulesMeta,
) -> RulesMetaOwned {
    let rule_defs: Vec<Vec<u8>> = archived
        .rule_defs
        .iter()
        .map(|b| b.as_slice().to_vec())
        .collect();
    let tripped: BTreeMap<String, bool> = archived
        .tripped
        .iter()
        .map(|e| (e.rule.as_str().to_string(), e.tripped))
        .collect();
    let fires: BTreeMap<String, u64> = archived
        .fires
        .iter()
        .map(|e| (e.rule.as_str().to_string(), u64::from(e.fires)))
        .collect();
    (rule_defs, tripped, fires)
}

/// Decode `ViewsSectionData` back into a `Vec<Vec<u8>>`.
pub fn archived_views_to_owned(archived: &crate::v8::layout::ArchivedViews) -> Vec<Vec<u8>> {
    archived
        .view_defs
        .iter()
        .map(|b| b.as_slice().to_vec())
        .collect()
}

/// Binary-search the archived provenance section for a specific triple.
///
/// Triples within each rule's entry are stored sorted by `(etype, src, dst)`,
/// so containment is O(log n) without materialising a `BTreeSet`.
///
/// Returns `true` if `(etype, src, dst)` is recorded for `rule`.
pub fn archived_provenance_contains(
    archived: &crate::v8::layout::ArchivedProvenance,
    rule: &str,
    etype: u32,
    src: u32,
    dst: u32,
) -> bool {
    let entry = archived.entries.iter().find(|e| e.rule.as_str() == rule);
    let Some(entry) = entry else {
        return false;
    };
    // Triples are sorted; use binary search.
    entry
        .triples
        .binary_search_by(|t| {
            let te = u32::from(t.etype);
            let ts = u32::from(t.src);
            let td = u32::from(t.dst);
            (te, ts, td).cmp(&(etype, src, dst))
        })
        .is_ok()
}

/// Decode `HnswSectionData` back into the `BTreeMap<String, (Vec<u8>, Vec<u8>)>` format.
pub fn archived_hnsw_to_owned(
    archived: &crate::v8::layout::ArchivedHnsw,
) -> BTreeMap<String, (Vec<u8>, Vec<u8>)> {
    archived
        .rules
        .iter()
        .map(|e| {
            (
                e.name.as_str().to_string(),
                (
                    e.src_blob.as_slice().to_vec(),
                    e.dst_blob.as_slice().to_vec(),
                ),
            )
        })
        .collect()
}
