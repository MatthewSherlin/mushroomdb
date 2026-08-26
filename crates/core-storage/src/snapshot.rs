use crate::columns::ColumnStore;
use crate::edge_props::EdgeProps;
use crate::idmap::IdMap;
use crate::interner::Interner;
use crate::pack::{push_u32, read_u32};
use crate::topology::Topology;
use crate::types::{GraphError, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const MAGIC: [u8; 4] = *b"GDB1";
/// V5: uncompressed bincode payload with CRC32 header.
pub const VERSION_5: u16 = 5;
/// V6: zstd-compressed V5 payload in a container.
pub const VERSION_6: u16 = 6;
/// V7: zstd(crc + packed CSR + packed columns + bincode leftover).
pub const VERSION: u16 = 7;

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

/// Encode a snapshot as a V7 container (packed CSR/columns + zstd).
///
/// Wire format:
///   [4B magic][2B version=7][zstd-compressed([4B crc32][u32 topo_len][topo][u32 props_len][props][bincode meta])]
pub fn encode(state: &SnapshotState) -> Vec<u8> {
    encode_v7(state)
}

/// V6 encode kept so tests can pin `decode(v6_bytes)` after VERSION=7.
pub fn encode_v6(state: &SnapshotState) -> Vec<u8> {
    let payload = bincode::serialize(state).expect("snapshot serialize cannot fail");
    wrap_zstd(VERSION_6, crc_inner(&payload))
}

fn encode_v7(state: &SnapshotState) -> Vec<u8> {
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
    };
    let meta_bytes = bincode::serialize(&meta).expect("snapshot meta serialize cannot fail");
    let mut payload = Vec::with_capacity(8 + topo.len() + props.len() + meta_bytes.len());
    push_u32(&mut payload, topo.len() as u32);
    payload.extend_from_slice(&topo);
    push_u32(&mut payload, props.len() as u32);
    payload.extend_from_slice(&props);
    payload.extend_from_slice(&meta_bytes);
    wrap_zstd(VERSION, crc_inner(&payload))
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
    let version = u16::from_le_bytes(bytes[4..6].try_into().unwrap());
    match version {
        VERSION_5 => decode_v5(&bytes[6..]),
        VERSION_6 => decode_v6(&bytes[6..]),
        VERSION => decode_v7(&bytes[6..]),
        other => {
            let hint = if other == 3 {
                " — V3 snapshot is no longer supported; re-snapshot with a V7 binary"
            } else if other == 4 {
                " — V4 snapshot is no longer supported; re-snapshot with a V7 binary"
            } else {
                ""
            };
            Err(GraphError::Corrupt {
                detail: format!("snapshot: unsupported version {other}{hint}"),
            })
        }
    }
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
    }))
}

fn strip_crc(body: &[u8]) -> Result<&[u8]> {
    if body.len() < 4 {
        return Err(GraphError::Corrupt {
            detail: "snapshot: truncated CRC header".into(),
        });
    }
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
        }
    }

    #[test]
    fn decode_v6_bytes_still_works_after_version_7_default_encode() {
        let state = tiny_state();
        let v6 = encode_v6(&state);
        assert_eq!(&v6[0..4], MAGIC);
        assert_eq!(u16::from_le_bytes([v6[4], v6[5]]), VERSION_6);
        let v7 = encode(&state);
        assert_eq!(u16::from_le_bytes([v7[4], v7[5]]), VERSION);

        let back6 = decode(&v6).unwrap().unwrap();
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

        let back7 = decode(&v7).unwrap().unwrap();
        assert_eq!(back7.ids.get("b"), Some(1));
        assert_eq!(back7.props.get(1, "name"), Some(&Value::Str("bob".into())));
        assert_eq!(back7.topo.edge_count(), 1);
        assert_eq!(back7.labels, vec![back7.syms.get("N").unwrap(); 2]);
    }
}
