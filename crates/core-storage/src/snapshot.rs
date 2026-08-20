use crate::columns::ColumnStore;
use crate::edge_props::EdgeProps;
use crate::idmap::IdMap;
use crate::interner::Interner;
use crate::topology::Topology;
use crate::types::{GraphError, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const MAGIC: [u8; 4] = *b"GDB1";
pub const VERSION: u16 = 4;

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
}

pub fn encode(state: &SnapshotState) -> Vec<u8> {
    let payload = bincode::serialize(state).expect("snapshot serialize cannot fail");
    let mut out = Vec::with_capacity(10 + payload.len());
    out.extend(MAGIC);
    out.extend(VERSION.to_le_bytes());
    out.extend(crc32fast::hash(&payload).to_le_bytes());
    out.extend(payload);
    out
}

pub fn decode(bytes: &[u8]) -> Result<Option<SnapshotState>> {
    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.len() < 10 || bytes[0..4] != MAGIC {
        return Err(GraphError::Corrupt {
            detail: "snapshot: bad magic".into(),
        });
    }
    let version = u16::from_le_bytes(bytes[4..6].try_into().unwrap());
    if version != VERSION {
        let hint = if version == 3 {
            " — V3 snapshot is no longer supported; re-snapshot with a V4 binary"
        } else {
            ""
        };
        return Err(GraphError::Corrupt {
            detail: format!("snapshot: unsupported version {version}{hint}"),
        });
    }
    let crc = u32::from_le_bytes(bytes[6..10].try_into().unwrap());
    let payload = &bytes[10..];
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
