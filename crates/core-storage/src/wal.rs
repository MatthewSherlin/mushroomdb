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
    CreateRule {
        def_bytes: Vec<u8>,
    },
    DeleteRule {
        name: String,
    },
    // ── Mutation variants (appended last — bincode is positional) ─────────────
    RemoveProp {
        key: String,
        field: String,
    },
    DeleteEdge {
        edge_type: String,
        src_key: String,
        dst_key: String,
    },
    DeleteNode {
        key: String,
    },
    /// One WAL frame = one atomic batch. Nested `Batch` inside a `Batch` is
    /// invalid: `encode_record` debug-asserts against it, and `decode_all`
    /// treats a frame whose payload deserialises to a nested `Batch` as corrupt
    /// (stops cleanly before that frame, returning the valid prefix).
    Batch(Vec<WalRecord>),
    /// Recompute one rule from scratch (un-trip / repair). Appended after
    /// `Batch`; bincode discriminant is 9. `Batch` stays at 8.
    RebuildRule {
        name: String,
    },
}

/// Encode a single WAL record as a framed byte sequence: `[len u32][crc u32][payload]`.
///
/// # Panics (debug builds)
/// Panics if `rec` is a `Batch` that contains a nested `Batch` — nested batches
/// are semantically invalid.
pub fn encode_record(rec: &WalRecord) -> Vec<u8> {
    if let WalRecord::Batch(inner) = rec {
        debug_assert!(
            !inner.iter().any(|r| matches!(r, WalRecord::Batch(_))),
            "nested Batch is invalid: a Batch may not contain another Batch"
        );
    }
    let payload = bincode::serialize(rec).expect("walrecord serialize cannot fail");
    let crc = crc32fast::hash(&payload);
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend((payload.len() as u32).to_le_bytes());
    out.extend(crc.to_le_bytes());
    out.extend(payload);
    out
}

/// Decode as many complete, valid WAL frames as possible from `bytes`.
///
/// Returns `(records, valid_len)` where `valid_len` is the byte offset of the
/// first frame that was torn, corrupt, or undeserializable — callers can
/// truncate the WAL file to `valid_len` to discard the invalid tail.
///
/// A `Batch` frame whose inner record list contains a nested `Batch` is treated
/// as corrupt: decoding stops before that frame (the frame itself is not pushed).
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
            Ok(WalRecord::Batch(inner)) => {
                // Nested Batch inside a Batch is invalid; treat as corrupt frame.
                if inner.iter().any(|r| matches!(r, WalRecord::Batch(_))) {
                    return (recs, pos);
                }
                recs.push(WalRecord::Batch(inner));
            }
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

    // ── Task 2: new variant roundtrips ────────────────────────────────────────

    #[test]
    fn roundtrip_remove_prop() {
        let r = WalRecord::RemoveProp {
            key: "n1".into(),
            field: "age".into(),
        };
        let bytes = encode_record(&r);
        let (recs, _) = decode_all(&bytes);
        assert_eq!(recs, vec![r]);
    }

    #[test]
    fn roundtrip_delete_edge() {
        let r = WalRecord::DeleteEdge {
            edge_type: "KNOWS".into(),
            src_key: "a".into(),
            dst_key: "b".into(),
        };
        let bytes = encode_record(&r);
        let (recs, _) = decode_all(&bytes);
        assert_eq!(recs, vec![r]);
    }

    #[test]
    fn roundtrip_delete_node() {
        let r = WalRecord::DeleteNode { key: "x".into() };
        let bytes = encode_record(&r);
        let (recs, _) = decode_all(&bytes);
        assert_eq!(recs, vec![r]);
    }

    #[test]
    fn roundtrip_rebuild_rule() {
        let r = WalRecord::RebuildRule { name: "eq".into() };
        let bytes = encode_record(&r);
        let (recs, _) = decode_all(&bytes);
        assert_eq!(recs, vec![r]);
    }

    #[test]
    fn batch_of_three_is_one_frame() {
        let inner = vec![
            WalRecord::DeleteNode { key: "a".into() },
            WalRecord::DeleteNode { key: "b".into() },
            WalRecord::DeleteNode { key: "c".into() },
        ];
        let batch = WalRecord::Batch(inner.clone());
        let frame = encode_record(&batch);

        // Exactly ONE frame: one (u32 len + u32 crc) header at offset 0.
        // Verify by calling decode_all on the raw bytes.
        let (recs, consumed) = decode_all(&frame);
        assert_eq!(consumed, frame.len(), "should consume the whole frame");
        assert_eq!(recs.len(), 1, "one decoded record (the Batch)");
        assert_eq!(recs[0], WalRecord::Batch(inner));
    }

    #[test]
    fn torn_mid_batch_frame_drops_whole_batch() {
        // Two plain records before the batch, then a batch frame that is torn.
        let pre = sample();
        let batch = WalRecord::Batch(vec![
            WalRecord::DeleteNode { key: "a".into() },
            WalRecord::DeleteNode { key: "b".into() },
        ]);
        let mut bytes = Vec::new();
        for r in &pre {
            bytes.extend(encode_record(r));
        }
        let batch_start = bytes.len();
        bytes.extend(encode_record(&batch));

        // Truncate 3 bytes inside the batch frame.
        bytes.truncate(bytes.len() - 3);

        let (recs, consumed) = decode_all(&bytes);
        assert_eq!(recs, pre, "only pre-batch records survive");
        assert_eq!(
            consumed, batch_start,
            "valid_len stops at batch frame start"
        );
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "nested Batch")]
    fn nested_batch_encode_panics_in_debug() {
        let inner_batch = WalRecord::Batch(vec![WalRecord::DeleteNode { key: "z".into() }]);
        let outer = WalRecord::Batch(vec![inner_batch]);
        encode_record(&outer); // must debug_assert-panic
    }

    /// Pin the exact on-disk wire format for two variants: one pre-existing
    /// (discriminant 0) and the first new variant added in Plan 4 (discriminant 5).
    ///
    /// **If this test fails you have broken every existing database file.**
    /// WAL variants must ONLY be appended — never reordered or inserted.
    /// The frame layout is `[len: u32 LE][crc32: u32 LE][bincode payload]`.
    /// The discriminant is the first 4 bytes of the payload (u32 LE).
    /// `Batch` stays at discriminant 8; `RebuildRule` is 9.
    #[test]
    fn golden_bytes_pin_wire_format() {
        // ── Variant 0: InsertNode { label: "L", key: "k", props: [] } ──────────
        let insert_node = WalRecord::InsertNode {
            label: "L".into(),
            key: "k".into(),
            props: vec![],
        };
        #[rustfmt::skip]
        let expected_insert_node: &[u8] = &[
            // header: len=30 LE, crc32 LE
            30, 0, 0, 0, 114, 69, 253, 24,
            // payload: discriminant=0 (InsertNode)
            0, 0, 0, 0,
            // label "L": len=1, b'L'
            1, 0, 0, 0, 0, 0, 0, 0, 76,
            // key "k": len=1, b'k'
            1, 0, 0, 0, 0, 0, 0, 0, 107,
            // props: len=0
            0, 0, 0, 0, 0, 0, 0, 0,
        ];
        assert_eq!(
            encode_record(&insert_node),
            expected_insert_node,
            "InsertNode wire format changed — this breaks all existing WAL files"
        );

        // ── Variant 5: RemoveProp { key: "n1", field: "age" } ─────────────────
        // This is the first Plan-4 mutation variant; pins the append boundary.
        let remove_prop = WalRecord::RemoveProp {
            key: "n1".into(),
            field: "age".into(),
        };
        #[rustfmt::skip]
        let expected_remove_prop: &[u8] = &[
            // header: len=25 LE, crc32 LE
            25, 0, 0, 0, 35, 214, 55, 239,
            // payload: discriminant=5 (RemoveProp)
            5, 0, 0, 0,
            // key "n1": len=2, b'n', b'1'
            2, 0, 0, 0, 0, 0, 0, 0, 110, 49,
            // field "age": len=3, b'a', b'g', b'e'
            3, 0, 0, 0, 0, 0, 0, 0, 97, 103, 101,
        ];
        assert_eq!(
            encode_record(&remove_prop),
            expected_remove_prop,
            "RemoveProp wire format changed — this breaks all existing WAL files"
        );

        // ── Variant 9: RebuildRule { name: "eq" } ─────────────────────────────
        // Batch remains discriminant 8; this variant is appended after it.
        let rebuild = WalRecord::RebuildRule { name: "eq".into() };
        #[rustfmt::skip]
        let expected_rebuild: &[u8] = &[
            // header: len=14 LE, crc32 LE
            14, 0, 0, 0, 242, 136, 144, 68,
            // payload: discriminant=9 (RebuildRule)
            9, 0, 0, 0,
            // name "eq": len=2, b'e', b'q'
            2, 0, 0, 0, 0, 0, 0, 0, 101, 113,
        ];
        assert_eq!(
            encode_record(&rebuild),
            expected_rebuild,
            "RebuildRule wire format changed — append-only WAL variants"
        );
    }

    #[test]
    fn nested_batch_decode_is_treated_as_corrupt() {
        // Manually encode a Batch whose payload contains a nested Batch by
        // serializing the raw bincode bytes, bypassing encode_record's assert.
        let inner_batch = WalRecord::Batch(vec![WalRecord::DeleteNode { key: "z".into() }]);
        let outer = WalRecord::Batch(vec![inner_batch]);

        // Encode the payload without the debug assert by calling bincode directly.
        let payload = bincode::serialize(&outer).unwrap();
        let crc = crc32fast::hash(&payload);
        let mut frame = Vec::with_capacity(8 + payload.len());
        frame.extend((payload.len() as u32).to_le_bytes());
        frame.extend(crc.to_le_bytes());
        frame.extend(&payload);

        // Prepend a valid record so we can verify the stop position.
        let good = encode_record(&WalRecord::DeleteNode { key: "good".into() });
        let good_len = good.len();
        let mut bytes = good;
        bytes.extend(&frame);

        let (recs, consumed) = decode_all(&bytes);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0], WalRecord::DeleteNode { key: "good".into() });
        assert_eq!(
            consumed, good_len,
            "stops cleanly before the nested-batch frame"
        );
    }
}
