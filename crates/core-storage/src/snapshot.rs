use crate::columns::ColumnStore;
use crate::idmap::IdMap;
use crate::interner::Interner;
use crate::topology::Topology;
use crate::types::{GraphError, Result};
use serde::{Deserialize, Serialize};

pub const MAGIC: [u8; 4] = *b"GDB1";
pub const VERSION: u16 = 1;

#[derive(Serialize, Deserialize)]
pub struct SnapshotState {
    pub ids: IdMap,
    pub syms: Interner,
    pub topo: Topology,
    pub props: ColumnStore,
    pub labels: Vec<u32>,
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
