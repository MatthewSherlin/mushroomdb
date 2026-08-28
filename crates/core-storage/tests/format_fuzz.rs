//! On-disk format robustness: `wal::decode_all` and `snapshot::decode`
//! never panic, and mutated valid WAL streams decode as a prefix of the
//! pristine record list.
//!
//! Four generators, 256 cases each (1024 total):
//!   (a) arbitrary byte vectors (len 0..4096) → `wal::decode_all`
//!   (b) arbitrary byte vectors (len 0..4096) → `snapshot::decode`
//!   (c) bit-flip / truncate / splice mutations of a valid WAL stream
//!       (several records including a `Batch` and a `RebuildRule`) and a
//!       valid encoded `SnapshotState`
//!   (d) mutate the raw v3 snapshot *payload*, then reattach a fresh valid
//!       header (magic + version + CRC of the mutated payload) so
//!       `bincode::deserialize` is actually reached

use core_storage::snapshot::{self, SnapshotState};
use core_storage::wal::{decode_all, encode_record, WalRecord};
use core_storage::{ColumnStore, EdgeProps, IdMap, Interner, Topology, Value};
use proptest::prelude::*;
use std::collections::{BTreeMap, BTreeSet};
use std::panic::{catch_unwind, AssertUnwindSafe};

fn hex_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "Box<dyn Any>".into()
    }
}

/// Valid multi-record WAL: several variants, including one `Batch` (no nested
/// batch) and one `RebuildRule`.
fn valid_wal_records() -> Vec<WalRecord> {
    vec![
        WalRecord::InsertNode {
            label: "Person".into(),
            key: "alice".into(),
            props: vec![("age".into(), Value::Int(30))],
        },
        WalRecord::InsertEdge {
            edge_type: "KNOWS".into(),
            src_key: "alice".into(),
            dst_key: "bob".into(),
        },
        WalRecord::SetProp {
            key: "alice".into(),
            field: "name".into(),
            value: Value::Str("Alice".into()),
        },
        WalRecord::CreateRule {
            def_bytes: b"rule-def".to_vec(),
        },
        WalRecord::RemoveProp {
            key: "alice".into(),
            field: "age".into(),
        },
        WalRecord::Batch(vec![
            WalRecord::InsertNode {
                label: "Org".into(),
                key: "acme".into(),
                props: vec![],
            },
            WalRecord::DeleteNode { key: "tmp".into() },
        ]),
        WalRecord::RebuildRule { name: "eq".into() },
        WalRecord::DeleteEdge {
            edge_type: "KNOWS".into(),
            src_key: "alice".into(),
            dst_key: "bob".into(),
        },
        WalRecord::DeleteNode {
            key: "alice".into(),
        },
        WalRecord::DeleteRule { name: "eq".into() },
        WalRecord::CreateView {
            def_bytes: b"view-def".to_vec(),
        },
        WalRecord::DeleteView {
            name: "my_view".into(),
        },
    ]
}

fn encode_wal(recs: &[WalRecord]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for r in recs {
        bytes.extend(encode_record(r));
    }
    bytes
}

fn valid_snapshot_bytes() -> Vec<u8> {
    let mut ids = IdMap::new();
    ids.get_or_insert("alice");
    ids.get_or_insert("bob");
    ids.delete("bob");

    let mut syms = Interner::new();
    let person = syms.intern("Person");

    let mut props = ColumnStore::new();
    props.set(0, "age", Value::Int(30));

    let mut provenance = BTreeMap::new();
    let mut edges = BTreeSet::new();
    edges.insert((0, 0, 1));
    provenance.insert("eq".into(), edges);

    let mut rule_tripped = BTreeMap::new();
    rule_tripped.insert("eq".into(), false);

    let mut rule_fires = BTreeMap::new();
    rule_fires.insert("eq".into(), 3);

    let state = SnapshotState {
        ids,
        syms,
        topo: Topology::new(),
        props,
        labels: vec![person, u32::MAX],
        edge_props: EdgeProps::new(),
        rule_defs: vec![b"rule-bytes".to_vec()],
        provenance,
        rule_tripped,
        rule_fires,
        ivf_state: Default::default(),
        hnsw_state: Default::default(),
        view_defs: vec![],
        wal_truncated: true,
    };
    snapshot::encode(&state).expect("fixture state fits u32 section lengths")
}

fn bit_flip(bytes: &[u8], entropy: &[u8]) -> Vec<u8> {
    if bytes.is_empty() {
        return Vec::new();
    }
    let mut out = bytes.to_vec();
    if entropy.is_empty() {
        out[0] ^= 1;
        return out;
    }
    for chunk in entropy.chunks(2) {
        let idx = chunk[0] as usize % out.len();
        let bit = if chunk.len() > 1 { chunk[1] % 8 } else { 0 };
        out[idx] ^= 1 << bit;
    }
    out
}

fn truncate_bytes(bytes: &[u8], entropy: &[u8]) -> Vec<u8> {
    if bytes.is_empty() {
        return Vec::new();
    }
    let n = match entropy {
        [a, b, ..] => u16::from_le_bytes([*a, *b]) as usize,
        [a] => *a as usize,
        [] => 0,
    };
    bytes[..n % (bytes.len() + 1)].to_vec()
}

/// Insert a run of entropy bytes at a position derived from entropy.
fn splice_bytes(bytes: &[u8], entropy: &[u8]) -> Vec<u8> {
    let pos = match entropy {
        [a, b, ..] => u16::from_le_bytes([*a, *b]) as usize % (bytes.len() + 1),
        [a] => (*a as usize) % (bytes.len() + 1),
        [] => 0,
    };
    let insert: &[u8] = if entropy.len() > 2 {
        &entropy[2..]
    } else {
        &[0xaa, 0xbb]
    };
    let mut out = Vec::with_capacity(bytes.len() + insert.len());
    out.extend_from_slice(&bytes[..pos]);
    out.extend_from_slice(insert);
    out.extend_from_slice(&bytes[pos..]);
    out
}

/// Frame `payload` as a v3 snapshot: GDB1 + version 3 + crc32 of payload.
fn wrap_snapshot_payload(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(10 + payload.len());
    out.extend(snapshot::MAGIC);
    out.extend(snapshot::VERSION.to_le_bytes());
    out.extend(crc32fast::hash(payload).to_le_bytes());
    out.extend(payload);
    out
}

fn mutate(bytes: &[u8], kind: u8, entropy: &[u8]) -> Vec<u8> {
    match kind % 4 {
        0 => bit_flip(bytes, entropy),
        1 => truncate_bytes(bytes, entropy),
        2 => splice_bytes(bytes, entropy),
        _ => {
            let flipped = bit_flip(bytes, entropy);
            let spliced = splice_bytes(&flipped, entropy);
            truncate_bytes(&spliced, entropy)
        }
    }
}

fn check_wal_decode(bytes: &[u8], pristine: Option<&[WalRecord]>) -> Result<(), TestCaseError> {
    let outcome = catch_unwind(AssertUnwindSafe(|| decode_all(bytes)));
    match outcome {
        Ok((recs, valid_len)) => {
            prop_assert!(
                valid_len <= bytes.len(),
                "valid_len {valid_len} > input len {} ; input: {}",
                bytes.len(),
                hex_bytes(bytes)
            );
            if let Some(pristine) = pristine {
                let n = recs.len();
                prop_assert!(
                    n <= pristine.len() && recs == pristine[..n],
                    "decoded records are not a prefix of the pristine stream\n  decoded ({n}): {recs:?}\n  pristine ({}): {pristine:?}\n  input: {}",
                    pristine.len(),
                    hex_bytes(bytes)
                );
            }
            Ok(())
        }
        Err(panic) => Err(TestCaseError::fail(format!(
            "wal::decode_all panicked: {} ; input ({} bytes): {}",
            panic_message(panic),
            bytes.len(),
            hex_bytes(bytes)
        ))),
    }
}

fn check_snap_decode(bytes: &[u8]) -> Result<(), TestCaseError> {
    let outcome = catch_unwind(AssertUnwindSafe(|| snapshot::decode(bytes)));
    match outcome {
        Ok(_) => Ok(()),
        Err(panic) => Err(TestCaseError::fail(format!(
            "snapshot::decode panicked: {} ; input ({} bytes): {}",
            panic_message(panic),
            bytes.len(),
            hex_bytes(bytes)
        ))),
    }
}

#[test]
fn valid_fixtures_roundtrip() {
    let recs = valid_wal_records();
    assert!(
        recs.iter().any(|r| matches!(r, WalRecord::Batch(_))),
        "fixture must include a Batch"
    );
    assert!(
        recs.iter()
            .any(|r| matches!(r, WalRecord::RebuildRule { .. })),
        "fixture must include a RebuildRule"
    );
    let bytes = encode_wal(&recs);
    let (decoded, consumed) = decode_all(&bytes);
    assert_eq!(decoded, recs);
    assert_eq!(consumed, bytes.len());

    let snap = valid_snapshot_bytes();
    let state = snapshot::decode(&snap).expect("valid snapshot must decode");
    assert!(state.is_some(), "non-empty snapshot decodes as Some");
}

// Block (a): arbitrary bytes → wal::decode_all never panics.
proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]
    #[test]
    fn decode_all_never_panics_on_arbitrary_bytes(
        bytes in proptest::collection::vec(any::<u8>(), 0..4096)
    ) {
        check_wal_decode(&bytes, None)?;
    }
}

// Block (b): arbitrary bytes → snapshot::decode never panics.
proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]
    #[test]
    fn snapshot_decode_never_panics_on_arbitrary_bytes(
        bytes in proptest::collection::vec(any::<u8>(), 0..4096)
    ) {
        check_snap_decode(&bytes)?;
    }
}

// Coverage gap: production `access_unchecked` seams
// ---------------------------------------------------------------------------
// The blocks below exercise `snapshot::decode`, which uses validated
// `rkyv::access` on every section. The production hot path (MappedBase::topology,
// MappedBase::columns, MappedBase::edge_props_section) uses `rkyv::access_unchecked`
// and has NO hostile-bytes proptest coverage here. All fuzz routes go through the
// validated `decode_v8_from_mapped` path.
//
// `catch_unwind` cannot defend a UB path: if a corrupt relative-pointer field
// causes `ArchivedVec::as_slice` to resolve an out-of-bounds address, Rust's
// unsafety guarantee is violated before any panic handler can fire.
// The appropriate defence is Miri or ASAN (see .github/workflows/ci.yml TODO).
// ---------------------------------------------------------------------------

// Block (c): mutations of valid encodings — no panic, WAL is a prefix.
proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]
    #[test]
    fn mutated_valid_encodings_never_panic_and_wal_is_prefix(
        kind in any::<u8>(),
        entropy in proptest::collection::vec(any::<u8>(), 0..64)
    ) {
        let pristine = valid_wal_records();
        let wal = encode_wal(&pristine);
        let mutated_wal = mutate(&wal, kind, &entropy);
        check_wal_decode(&mutated_wal, Some(&pristine))?;

        let snap = valid_snapshot_bytes();
        let mutated_snap = mutate(&snap, kind.wrapping_add(17), &entropy);
        check_snap_decode(&mutated_snap)?;
    }
}

// Block (d): CRC-valid mutated payload → snapshot::decode reaches bincode.
proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]
    #[test]
    fn snapshot_decode_never_panics_on_crc_valid_mutated_payload(
        kind in any::<u8>(),
        entropy in proptest::collection::vec(any::<u8>(), 0..64)
    ) {
        let snap = valid_snapshot_bytes();
        prop_assert!(
            snap.len() >= 10,
            "fixture snapshot must have a 10-byte header; got {}",
            snap.len()
        );
        let mutated = mutate(&snap[10..], kind, &entropy);
        let framed = wrap_snapshot_payload(&mutated);
        check_snap_decode(&framed)?;
    }
}
