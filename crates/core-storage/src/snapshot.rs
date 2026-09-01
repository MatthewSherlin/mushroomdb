use crate::columns::ColumnStore;
use crate::edge_props::EdgeProps;
use crate::idmap::IdMap;
use crate::interner::Interner;
use crate::pack::{push_u32, read_u32};
use crate::topology::Topology;
use crate::types::{GraphError, Result};
use crate::v8::encode::V8Meta;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};

pub const MAGIC: [u8; 4] = *b"GDB1";
/// V5: uncompressed bincode payload with CRC32 header.
pub const VERSION_5: u16 = 5;
/// V6: zstd-compressed V5 payload in a container.
pub const VERSION_6: u16 = 6;
/// V7: zstd(crc + packed CSR + packed columns + bincode leftover).
pub const VERSION_7: u16 = 7;
/// V8: 4KB header page + rkyv sections (mmap-able zero-copy).
pub const VERSION_8: u16 = 8;
/// Current default encoding version.
pub const VERSION: u16 = VERSION_8;

/// IVF state for one side (src or dst) of a single approximate rule.
/// Persisted in V4 snapshots so `open()` can restore cluster assignments
/// without re-fitting k-means.
#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct SideIvfState {
    /// Fitted k-means centroids (empty = not yet fitted).
    pub centroids: Vec<Vec<f64>>,
    /// Per-node cluster assignment (node_id → centroid index).
    pub clusters: BTreeMap<u32, usize>,
    /// Drift counter at snapshot time.
    pub drift: u64,
}

/// IVF state for both sides of one approximate rule.
#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct PerRuleIvfState {
    pub src: SideIvfState,
    pub dst: SideIvfState,
}

#[derive(Serialize, Deserialize)]
pub struct SnapshotState {
    pub ids: IdMap,
    pub syms: Interner,
    pub topo: Topology,
    pub props: ColumnStore,
    pub labels: Vec<u32>,
    pub edge_props: EdgeProps,
    /// Bincoded `RuleDef` bytes — one entry per rule.  Raw bytes keep
    /// core-storage independent of core-rules.
    pub rule_defs: Vec<Vec<u8>>,
    pub provenance: BTreeMap<String, BTreeSet<(u32, u32, u32)>>,
    /// Per-rule budget-trip flags.
    pub rule_tripped: BTreeMap<String, bool>,
    /// Per-rule fire counters.
    pub rule_fires: BTreeMap<String, u64>,
    /// Per-approximate-rule IVF state (centroids + assignments + drift).
    /// Empty for exact rules and rules with no fitted clusters.
    /// Added in VERSION 4.
    pub ivf_state: BTreeMap<String, PerRuleIvfState>,
    /// Bincoded `ViewDef` bytes — one entry per materialized view.  Raw bytes
    /// keep core-storage independent of core-rules.  Values are NOT stored
    /// here; they are recomputed after open from the persisted topo + props.
    /// Added in VERSION 5.
    pub view_defs: Vec<Vec<u8>>,
    /// True when the snapshot write truncated the WAL (`keep_wal: false`),
    /// i.e. the on-disk WAL head coincides with this snapshot's state and
    /// `open_at` must load the snapshot as its base before replaying frames.
    /// Serialized in the V7 meta section only; V5/V6 payloads never carried
    /// it, so it is skipped here to keep their bincode wire shape frozen
    /// (decode of old formats leaves the default `false` = WAL-only as-of).
    #[serde(skip)]
    pub wal_truncated: bool,
    /// Per-approximate-rule HNSW graph blobs: rule name → `(src_blob, dst_blob)`.
    /// Each blob is an opaque bincoded `HnswIndex`.  Serialized in the V7 meta
    /// section only; V5/V6 payloads never carried it — skipped here so their
    /// bincode wire shape is frozen (missing → default empty map).
    #[serde(skip)]
    pub hnsw_state: BTreeMap<String, (Vec<u8>, Vec<u8>)>,
}

#[derive(Serialize, Deserialize)]
struct V7Meta {
    ids: IdMap,
    syms: Interner,
    labels: Vec<u32>,
    edge_props: EdgeProps,
    rule_defs: Vec<Vec<u8>>,
    provenance: BTreeMap<String, BTreeSet<(u32, u32, u32)>>,
    rule_tripped: BTreeMap<String, bool>,
    rule_fires: BTreeMap<String, u64>,
    ivf_state: BTreeMap<String, PerRuleIvfState>,
    view_defs: Vec<Vec<u8>>,
    /// See [`SnapshotState::wal_truncated`]. V7-only field.
    wal_truncated: bool,
    /// Per-approximate-rule HNSW graph blobs: rule name → `(src_blob, dst_blob)`.
    /// Added last so the field is easily skipped on old V7 blobs by setting a
    /// default.  V7 is unreleased; the fixture is regenerated after this change.
    #[serde(default)]
    hnsw: BTreeMap<String, (Vec<u8>, Vec<u8>)>,
}

fn wrap_zstd(version: u16, inner: Vec<u8>) -> Vec<u8> {
    let compressed =
        zstd::encode_all(inner.as_slice(), 3).expect("zstd compress cannot fail on in-memory buf");
    let mut out = Vec::with_capacity(6 + compressed.len());
    out.extend(MAGIC);
    out.extend(version.to_le_bytes());
    out.extend(compressed);
    out
}

fn crc_inner(payload: &[u8]) -> Vec<u8> {
    let crc = crc32fast::hash(payload);
    let mut inner = Vec::with_capacity(4 + payload.len());
    inner.extend(crc.to_le_bytes());
    inner.extend(payload);
    inner
}

/// Encode a snapshot as a V8 container (mmap-able zero-copy).
///
/// V8 is the default format.  V7/V6/V5 are still decoded on open.
///
/// # Errors
/// [`GraphError::Corrupt`] if any section exceeds 4 GiB.
pub fn encode(state: &SnapshotState) -> Result<Vec<u8>> {
    encode_v8_from_state(state)
}

fn encode_v8_from_state(state: &SnapshotState) -> Result<Vec<u8>> {
    let ivf_bytes = if state.ivf_state.is_empty() {
        Vec::new()
    } else {
        bincode::serialize(&state.ivf_state).expect("IVF state serialize cannot fail")
    };
    let meta = V8Meta {
        labels: state.labels.clone(),
        edge_props: state.edge_props.clone(),
        rule_defs: state.rule_defs.clone(),
        provenance: state.provenance.clone(),
        rule_tripped: state.rule_tripped.clone(),
        rule_fires: state.rule_fires.clone(),
        ivf_bytes,
        view_defs: state.view_defs.clone(),
        wal_truncated: state.wal_truncated,
        hnsw: state.hnsw_state.clone(),
        last_change: HashMap::new(),
    };
    let mut out = Vec::new();
    crate::v8::encode::encode_v8(
        None,
        None,
        None,
        None,
        &state.topo,
        &state.props,
        &state.ids,
        &state.syms,
        &meta,
        &mut out,
    )?;
    Ok(out)
}

/// V6 encode kept so tests can pin `decode(v6_bytes)` after VERSION=7.
pub fn encode_v6(state: &SnapshotState) -> Vec<u8> {
    let payload = bincode::serialize(state).expect("snapshot serialize cannot fail");
    wrap_zstd(VERSION_6, crc_inner(&payload))
}

fn section_len(name: &str, len: usize) -> Result<u32> {
    u32::try_from(len).map_err(|_| GraphError::Corrupt {
        detail: format!(
            "snapshot: packed {name} section is {len} bytes, exceeds u32 length prefix"
        ),
    })
}

pub fn encode_v7(state: &SnapshotState) -> Result<Vec<u8>> {
    let mut topo = Vec::new();
    state.topo.pack(&mut topo);
    let mut props = Vec::new();
    state.props.pack(&mut props);
    let meta = V7Meta {
        ids: state.ids.clone(),
        syms: state.syms.clone(),
        labels: state.labels.clone(),
        edge_props: state.edge_props.clone(),
        rule_defs: state.rule_defs.clone(),
        provenance: state.provenance.clone(),
        rule_tripped: state.rule_tripped.clone(),
        rule_fires: state.rule_fires.clone(),
        ivf_state: state.ivf_state.clone(),
        view_defs: state.view_defs.clone(),
        wal_truncated: state.wal_truncated,
        hnsw: state.hnsw_state.clone(),
    };
    let meta_bytes = bincode::serialize(&meta).expect("snapshot meta serialize cannot fail");
    let mut payload = Vec::with_capacity(8 + topo.len() + props.len() + meta_bytes.len());
    push_u32(&mut payload, section_len("topology", topo.len())?);
    payload.extend_from_slice(&topo);
    push_u32(&mut payload, section_len("columns", props.len())?);
    payload.extend_from_slice(&props);
    payload.extend_from_slice(&meta_bytes);
    Ok(wrap_zstd(VERSION_7, crc_inner(&payload)))
}

/// Peek at the on-disk snapshot format version without a full decode.
///
/// Reads only the 6-byte header (4 B magic + 2 B version LE).  Returns
/// `None` if `bytes` is empty (absent snapshot), or an error if the magic
/// is wrong or the header is truncated.  Does not validate the payload.
pub fn peek_version(bytes: &[u8]) -> Result<Option<u16>> {
    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.len() < 6 || bytes[0..4] != MAGIC {
        return Err(crate::types::GraphError::Corrupt {
            detail: "snapshot: bad magic or truncated header".into(),
        });
    }
    // Infallible: `bytes.len() >= 6` checked above; `bytes[4..6]` is exactly 2 bytes.
    Ok(Some(u16::from_le_bytes(bytes[4..6].try_into().unwrap())))
}

pub fn decode(bytes: &[u8]) -> Result<Option<SnapshotState>> {
    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.len() < 6 || bytes[0..4] != MAGIC {
        return Err(GraphError::Corrupt {
            detail: "snapshot: bad magic".into(),
        });
    }
    // Infallible: `bytes.len() >= 6` checked above; `bytes[4..6]` is exactly 2 bytes.
    let version = u16::from_le_bytes(bytes[4..6].try_into().unwrap());
    match version {
        VERSION_5 => decode_v5(&bytes[6..]),
        VERSION_6 => decode_v6(&bytes[6..]),
        VERSION_7 => decode_v7(&bytes[6..]),
        VERSION_8 => {
            // V8 is a file-based format; decode via MappedBase from owned bytes.
            let mapped = crate::v8::MappedBase::from_bytes(bytes.to_vec())?;
            decode_v8_from_mapped(&mapped)
        }
        other => {
            let hint = if other == 3 {
                " — V3 snapshot is no longer supported; re-snapshot with a V8 binary"
            } else if other == 4 {
                " — V4 snapshot is no longer supported; re-snapshot with a V8 binary"
            } else {
                ""
            };
            Err(GraphError::Corrupt {
                detail: format!("snapshot: unsupported version {other}{hint}"),
            })
        }
    }
}

/// Reconstruct a full `SnapshotState` from a `MappedBase`.
///
/// Used by `decode()` for the fuzz-safe migration/integrity path.  The
/// hot production path in `db.rs` does NOT call this — it uses zero-copy
/// seam views backed by `MappedBase::topology()` (unchecked) directly.
///
/// This function uses `rkyv::access` (validated) for all large sections so
/// that corrupt bytes return `GraphError::Corrupt` rather than UB.  Small
/// sections (IDS, SYMS, RULES_META, VIEWS) already CRC-check on first touch.
pub fn decode_v8_from_mapped(mapped: &crate::v8::MappedBase) -> Result<Option<SnapshotState>> {
    use crate::v8::encode::{
        archived_edge_props_to_owned, archived_hnsw_to_owned, archived_provenance_to_owned,
        archived_rules_meta_to_owned, archived_to_columnstore, archived_to_idmap,
        archived_to_interner, archived_views_to_owned, csr_to_topology, decode_ivf_bytes,
        decode_meta,
    };
    use crate::v8::{
        SECTION_COLUMNS, SECTION_EDGE_PROPS, SECTION_HNSW, SECTION_PROVENANCE, SECTION_TOPOLOGY,
    };

    // Large sections: use validated rkyv::access so corrupt bytes return
    // GraphError::Corrupt instead of UB (required by the fuzz invariant).
    // The production seam path (db.rs topo_view/props_view) uses the
    // unchecked MappedBase::topology()/columns()/edge_props_section() accessors.
    let archived_topo = rkyv::access::<crate::v8::layout::ArchivedCsrData, rkyv::rancor::Error>(
        mapped.section_bytes(SECTION_TOPOLOGY)?,
    )
    .map_err(|e| GraphError::Corrupt {
        detail: format!("v8: topology rkyv access: {e}"),
    })?;
    let topo = csr_to_topology(archived_topo);

    let archived_cols =
        rkyv::access::<crate::v8::layout::ArchivedColumnsData, rkyv::rancor::Error>(
            mapped.section_bytes(SECTION_COLUMNS)?,
        )
        .map_err(|e| GraphError::Corrupt {
            detail: format!("v8: columns rkyv access: {e}"),
        })?;
    let props = archived_to_columnstore(archived_cols);

    let archived_ids = mapped.ids()?;
    let ids = archived_to_idmap(archived_ids);

    let archived_syms = mapped.syms()?;
    let syms = archived_to_interner(archived_syms);

    // V8Meta now carries only labels and wal_truncated in the bincode section.
    // IVF state is read from section 10 directly.
    let meta_bytes = mapped.meta_bytes()?;
    let meta = decode_meta(meta_bytes)?;

    let archived_ep =
        rkyv::access::<crate::v8::layout::ArchivedEdgePropsData, rkyv::rancor::Error>(
            mapped.section_bytes(SECTION_EDGE_PROPS)?,
        )
        .map_err(|e| GraphError::Corrupt {
            detail: format!("v8: edge_props rkyv access: {e}"),
        })?;
    let edge_props = archived_edge_props_to_owned(archived_ep);

    let archived_hnsw = rkyv::access::<
        crate::v8::layout::ArchivedHnswSectionData,
        rkyv::rancor::Error,
    >(mapped.section_bytes(SECTION_HNSW)?)
    .map_err(|e| GraphError::Corrupt {
        detail: format!("v8: hnsw rkyv access: {e}"),
    })?;
    let hnsw_state = archived_hnsw_to_owned(archived_hnsw);

    let archived_prov = rkyv::access::<
        crate::v8::layout::ArchivedProvenanceSectionData,
        rkyv::rancor::Error,
    >(mapped.section_bytes(SECTION_PROVENANCE)?)
    .map_err(|e| GraphError::Corrupt {
        detail: format!("v8: provenance rkyv access: {e}"),
    })?;
    let provenance = archived_provenance_to_owned(archived_prov);

    let (rule_defs, rule_tripped, rule_fires) =
        archived_rules_meta_to_owned(mapped.rules_meta_section()?);
    let view_defs = archived_views_to_owned(mapped.views_section()?);
    let ivf_state = decode_ivf_bytes(mapped.ivf_bytes()?);

    Ok(Some(SnapshotState {
        ids,
        syms,
        topo,
        props,
        labels: meta.labels,
        edge_props,
        rule_defs,
        provenance,
        rule_tripped,
        rule_fires,
        ivf_state,
        view_defs,
        wal_truncated: meta.wal_truncated,
        hnsw_state,
    }))
}

/// Decode a V5 (uncompressed) payload.  The 4-byte header has already been
/// stripped; `body` starts at the CRC32 field.
fn decode_v5(body: &[u8]) -> Result<Option<SnapshotState>> {
    let payload = strip_crc(body)?;
    bincode::deserialize(payload)
        .map(Some)
        .map_err(|e| GraphError::Corrupt {
            detail: format!("snapshot: {e}"),
        })
}

/// Decode a V6 (zstd-compressed) payload.  The 4-byte header has already been
/// stripped; `body` starts at the compressed bytes.
fn decode_v6(body: &[u8]) -> Result<Option<SnapshotState>> {
    let inner = zstd::decode_all(body).map_err(|e| GraphError::Corrupt {
        detail: format!("snapshot: zstd decompress failed: {e}"),
    })?;
    decode_v5(&inner)
}

fn decode_v7(body: &[u8]) -> Result<Option<SnapshotState>> {
    let inner = zstd::decode_all(body).map_err(|e| GraphError::Corrupt {
        detail: format!("snapshot: zstd decompress failed: {e}"),
    })?;
    let payload = strip_crc(&inner)?;
    let mut pos = 0usize;
    let topo_len = read_u32(payload, &mut pos)? as usize;
    let topo_bytes = payload
        .get(pos..pos + topo_len)
        .ok_or_else(|| GraphError::Corrupt {
            detail: "snapshot: truncated packed topology".into(),
        })?;
    pos += topo_len;
    let (topo, topo_consumed) = Topology::unpack(topo_bytes)?;
    if topo_consumed != topo_len {
        return Err(GraphError::Corrupt {
            detail: format!("snapshot: topology pack consumed {topo_consumed} of {topo_len}"),
        });
    }
    let props_len = read_u32(payload, &mut pos)? as usize;
    let props_bytes = payload
        .get(pos..pos + props_len)
        .ok_or_else(|| GraphError::Corrupt {
            detail: "snapshot: truncated packed columns".into(),
        })?;
    pos += props_len;
    let (props, props_consumed) = ColumnStore::unpack(props_bytes)?;
    if props_consumed != props_len {
        return Err(GraphError::Corrupt {
            detail: format!("snapshot: columns pack consumed {props_consumed} of {props_len}"),
        });
    }
    let meta: V7Meta = bincode::deserialize(&payload[pos..]).map_err(|e| GraphError::Corrupt {
        detail: format!("snapshot: {e}"),
    })?;
    Ok(Some(SnapshotState {
        ids: meta.ids,
        syms: meta.syms,
        topo,
        props,
        labels: meta.labels,
        edge_props: meta.edge_props,
        rule_defs: meta.rule_defs,
        provenance: meta.provenance,
        rule_tripped: meta.rule_tripped,
        rule_fires: meta.rule_fires,
        ivf_state: meta.ivf_state,
        view_defs: meta.view_defs,
        wal_truncated: meta.wal_truncated,
        hnsw_state: meta.hnsw,
    }))
}

fn strip_crc(body: &[u8]) -> Result<&[u8]> {
    if body.len() < 4 {
        return Err(GraphError::Corrupt {
            detail: "snapshot: truncated CRC header".into(),
        });
    }
    // Infallible: `body.len() >= 4` checked above; `body[0..4]` is exactly 4 bytes.
    let crc = u32::from_le_bytes(body[0..4].try_into().unwrap());
    let payload = &body[4..];
    if crc32fast::hash(payload) != crc {
        return Err(GraphError::Corrupt {
            detail: "snapshot: crc mismatch".into(),
        });
    }
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Value;

    fn tiny_state() -> SnapshotState {
        let mut ids = IdMap::new();
        ids.get_or_insert("a");
        ids.get_or_insert("b");
        let mut syms = Interner::new();
        let n = syms.intern("N");
        let e = syms.intern("E");
        let mut topo = Topology::new();
        topo.add_edge(e, 0, 1);
        let mut props = ColumnStore::new();
        props.set(0, "v", Value::Int(42));
        props.set(1, "name", Value::Str("bob".into()));
        SnapshotState {
            ids,
            syms,
            topo,
            props,
            labels: vec![n, n],
            edge_props: EdgeProps::new(),
            rule_defs: vec![],
            provenance: BTreeMap::new(),
            rule_tripped: BTreeMap::new(),
            rule_fires: BTreeMap::new(),
            ivf_state: BTreeMap::new(),
            view_defs: vec![],
            wal_truncated: true,
            hnsw_state: BTreeMap::new(),
        }
    }

    #[test]
    fn decode_v6_bytes_still_works_after_version_8_default_encode() {
        let state = tiny_state();
        let v6 = encode_v6(&state);
        assert_eq!(&v6[0..4], MAGIC);
        assert_eq!(u16::from_le_bytes([v6[4], v6[5]]), VERSION_6);
        let v8 = encode(&state).unwrap();
        assert_eq!(u16::from_le_bytes([v8[4], v8[5]]), VERSION_8);
        assert_eq!(VERSION, VERSION_8);

        let back6 = decode(&v6).unwrap().unwrap();
        assert!(
            !back6.wal_truncated,
            "V6 payload never carried wal_truncated; decode must default false"
        );
        assert_eq!(back6.ids.get("a"), Some(0));
        assert_eq!(back6.props.get(0, "v"), Some(&Value::Int(42)));
        assert_eq!(back6.topo.edge_count(), 1);
        assert_eq!(
            back6
                .topo
                .neighbors(
                    back6.syms.get("E").unwrap(),
                    crate::topology::Direction::Out,
                    0
                )
                .as_ref(),
            &[1]
        );

        let back8 = decode(&v8).unwrap().unwrap();
        assert!(
            back8.wal_truncated,
            "V8 meta must round-trip wal_truncated=true"
        );
        assert_eq!(back8.ids.get("b"), Some(1));
        assert_eq!(back8.props.get(1, "name"), Some(&Value::Str("bob".into())));
        assert_eq!(back8.topo.edge_count(), 1);
        assert_eq!(back8.labels, vec![back8.syms.get("N").unwrap(); 2]);
    }
}
