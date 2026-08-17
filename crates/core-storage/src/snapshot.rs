use crate::columns::ColumnStore;
use crate::edge_props::EdgeProps;
use crate::idmap::IdMap;
use crate::interner::Interner;
use crate::topology::Topology;
use crate::types::{GraphError, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const MAGIC: [u8; 4] = *b"GDB1";
pub const VERSION: u16 = 3;

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
    /// Per-rule budget-trip flags. Appended this plan; VERSION stays 3
    /// (`v3` = this plan's final snapshot shape; nothing shipped between).
    pub rule_tripped: BTreeMap<String, bool>,
    /// Per-rule fire counters. Same VERSION-3 append as `rule_tripped`.
    pub rule_fires: BTreeMap<String, u64>,
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
        return Err(GraphError::Corrupt {
            detail: format!("snapshot: unsupported version {version}"),
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
