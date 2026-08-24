use crate::columns::ColumnStore;
use crate::edge_props::EdgeProps;
use crate::idmap::IdMap;
use crate::interner::Interner;
use crate::topology::Topology;
use crate::types::{GraphError, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const MAGIC: [u8; 4] = *b"GDB1";
/// V5: uncompressed bincode payload with CRC32 header.
pub const VERSION_5: u16 = 5;
/// V6: zstd-compressed V5 payload in a container.
/// Header (magic + version) is uncompressed; the rest is zstd.
pub const VERSION: u16 = 6;

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

/// Encode a snapshot as a V6 container (zstd-compressed).
///
/// Wire format:
///   [4B magic][2B version=6][zstd-compressed([4B crc32][bincode payload])]
///
/// The header (magic + version) is deliberately left uncompressed so
/// `decode()` can identify the container type before decompressing.
pub fn encode(state: &SnapshotState) -> Vec<u8> {
    let payload = bincode::serialize(state).expect("snapshot serialize cannot fail");
    let crc = crc32fast::hash(&payload);
    // Build the inner bytes: CRC32 + payload (mirrors V5 layout pre-compression).
    let mut inner = Vec::with_capacity(4 + payload.len());
    inner.extend(crc.to_le_bytes());
    inner.extend(payload);
    let compressed =
        zstd::encode_all(inner.as_slice(), 3).expect("zstd compress cannot fail on in-memory buf");
    let mut out = Vec::with_capacity(6 + compressed.len());
    out.extend(MAGIC);
    out.extend(VERSION.to_le_bytes());
    out.extend(compressed);
    out
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
        VERSION => decode_v6(&bytes[6..]),
        other => {
            let hint = if other == 3 {
                " — V3 snapshot is no longer supported; re-snapshot with a V6 binary"
            } else if other == 4 {
                " — V4 snapshot is no longer supported; re-snapshot with a V6 binary"
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
    if body.len() < 4 {
        return Err(GraphError::Corrupt {
            detail: "snapshot: truncated V5 header".into(),
        });
    }
    let crc = u32::from_le_bytes(body[0..4].try_into().unwrap());
    let payload = &body[4..];
    if crc32fast::hash(payload) != crc {
        return Err(GraphError::Corrupt {
            detail: "snapshot: crc mismatch".into(),
        });
    }
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
    // After decompression the inner layout is identical to V5: [4B crc32][payload].
    decode_v5(&inner)
}
