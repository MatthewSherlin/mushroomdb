use crate::types::Value;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WalRecord {
    InsertNode {
        label: String,
        key: String,
        props: Vec<(String, Value)>,
    },
    InsertEdge {
        edge_type: String,
        src_key: String,
        dst_key: String,
    },
    SetProp {
        key: String,
        field: String,
        value: Value,
    },
}

pub fn encode_record(rec: &WalRecord) -> Vec<u8> {
    let payload = bincode::serialize(rec).expect("walrecord serialize cannot fail");
    let crc = crc32fast::hash(&payload);
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend((payload.len() as u32).to_le_bytes());
    out.extend(crc.to_le_bytes());
    out.extend(payload);
    out
}

pub fn decode_all(bytes: &[u8]) -> (Vec<WalRecord>, usize) {
    let mut recs = Vec::new();
    let mut pos = 0usize;
    loop {
        if bytes.len() < pos + 8 {
            return (recs, pos);
        }
        let len = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        let crc = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap());
        let start = pos + 8;
        if bytes.len() < start + len {
            return (recs, pos); // torn tail
        }
        let payload = &bytes[start..start + len];
        if crc32fast::hash(payload) != crc {
            return (recs, pos); // corrupt tail
        }
        match bincode::deserialize::<WalRecord>(payload) {
            Ok(r) => recs.push(r),
            Err(_) => return (recs, pos),
        }
        pos = start + len;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Value;

    fn sample() -> Vec<WalRecord> {
        vec![
            WalRecord::InsertNode {
                label: "L".into(),
                key: "k1".into(),
                props: vec![("f".into(), Value::Int(1))],
            },
            WalRecord::InsertEdge {
                edge_type: "E".into(),
                src_key: "k1".into(),
                dst_key: "k2".into(),
            },
        ]
    }

    #[test]
    fn roundtrip_multiple_records() {
        let mut bytes = Vec::new();
        for r in sample() {
            bytes.extend(encode_record(&r));
        }
        let (recs, consumed) = decode_all(&bytes);
        assert_eq!(recs, sample());
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn torn_tail_is_dropped_whole() {
        let mut bytes = Vec::new();
        for r in sample() {
            bytes.extend(encode_record(&r));
        }
        let full = bytes.len();
        let first = encode_record(&sample()[0]).len();
        bytes.truncate(full - 3); // tear the second record
        let (recs, consumed) = decode_all(&bytes);
        assert_eq!(recs.len(), 1);
        assert_eq!(consumed, first);
    }

    #[test]
    fn corrupt_crc_stops_replay_at_last_valid() {
        let mut bytes = encode_record(&sample()[0]);
        let n = bytes.len();
        bytes[n - 1] ^= 0xFF; // flip a payload byte
        let (recs, consumed) = decode_all(&bytes);
        assert!(recs.is_empty());
        assert_eq!(consumed, 0);
    }

    #[test]
    fn empty_input_is_fine() {
        let (recs, consumed) = decode_all(&[]);
        assert!(recs.is_empty());
        assert_eq!(consumed, 0);
    }
}
