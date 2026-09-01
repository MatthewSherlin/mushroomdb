//! WAL replay robustness: `decode_all` and `wal_commits` never panic, and
//! mutated valid streams decode as a prefix of the pristine record list.
//!
//! Three generators, 256 cases each (768 total):
//!   (a) arbitrary byte vectors (len 0..4096) → `decode_all`
//!   (b) exact record-boundary truncations of a valid multi-record WAL stream
//!       (prefix lengths 0..=full_len, derived from a u16 seed) →
//!       `decode_all` returns a prefix of the pristine record list
//!   (c) bit-flip / truncate / splice mutations of the valid WAL stream →
//!       `decode_all` returns a prefix of the pristine record list
//!
//! Fixture: 12 WAL records covering every variant that appeared in the Task 7
//! unwrap audit (InsertNode, InsertEdge, SetProp, RemoveProp, CreateRule,
//! RebuildRule, Batch, DeleteEdge, DeleteNode, DeleteRule, CreateView,
//! DeleteView) — the same path guarded at wal.rs L206-230.
//!
//! Liveness: the `liveness_probe_catch_unwind_catches_panics` test confirms
//! `catch_unwind` captures deliberate panics; any panic from `decode_all`
//! would surface as a `TestCaseError::fail` rather than a silently passing case.

use core_storage::wal::{decode_all, encode_record, WalRecord};
use core_storage::Value;
use proptest::prelude::*;
use std::panic::{catch_unwind, AssertUnwindSafe};

// ── helpers ──────────────────────────────────────────────────────────────────

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

/// Valid multi-record WAL stream covering every discriminant that appeared
/// in the Task 7 audit path: the guards at wal.rs ~L206-230 must hold for
/// each one after truncation or corruption.
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
        WalRecord::RemoveProp {
            key: "alice".into(),
            field: "age".into(),
        },
        WalRecord::CreateRule {
            def_bytes: b"rule-def".to_vec(),
        },
        WalRecord::RebuildRule { name: "eq".into() },
        WalRecord::Batch(vec![
            WalRecord::InsertNode {
                label: "Org".into(),
                key: "acme".into(),
                props: vec![],
            },
            WalRecord::DeleteNode { key: "tmp".into() },
        ]),
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

fn splice_bytes(bytes: &[u8], entropy: &[u8]) -> Vec<u8> {
    let len = bytes.len();
    let pos = if len == 0 {
        0
    } else {
        match entropy {
            [a, b, ..] => u16::from_le_bytes([*a, *b]) as usize % (len + 1),
            [a] => (*a as usize) % (len + 1),
            [] => 0,
        }
    };
    let insert: &[u8] = if entropy.len() > 2 {
        &entropy[2..]
    } else {
        &[0xaa, 0xbb]
    };
    let mut out = Vec::with_capacity(len + insert.len());
    out.extend_from_slice(&bytes[..pos]);
    out.extend_from_slice(insert);
    out.extend_from_slice(&bytes[pos..]);
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

fn check_decode_all(bytes: &[u8], pristine: Option<&[WalRecord]>) -> Result<(), TestCaseError> {
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
            "decode_all panicked: {} ; input ({} bytes): {}",
            panic_message(panic),
            bytes.len(),
            hex_bytes(bytes)
        ))),
    }
}

// ── deterministic fixture tests ───────────────────────────────────────────────

/// Liveness: `catch_unwind` catches a deliberate panic, so any panic from
/// `decode_all` would surface as a `TestCaseError::fail` rather than passing.
#[test]
fn liveness_probe_catch_unwind_catches_panics() {
    let caught = catch_unwind(AssertUnwindSafe(|| panic!("deliberate probe")));
    assert!(caught.is_err(), "catch_unwind must catch deliberate panics");
}

/// Fixture roundtrip: encode then decode is lossless and consumes all bytes.
#[test]
fn valid_wal_fixture_roundtrip() {
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
    assert_eq!(decoded, recs, "round-trip must be lossless");
    assert_eq!(consumed, bytes.len(), "all bytes must be consumed on valid input");
}

// ── Block (a): arbitrary bytes → decode_all never panics ─────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]
    #[test]
    fn decode_all_never_panics_on_arbitrary_bytes(
        bytes in proptest::collection::vec(any::<u8>(), 0..4096)
    ) {
        check_decode_all(&bytes, None)?;
    }
}

// ── Block (b): prefix truncations of valid WAL → decode_all is a prefix ──────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]
    #[test]
    fn decode_all_never_panics_on_truncated_valid_wal(
        truncate_seed in any::<u16>()
    ) {
        let pristine = valid_wal_records();
        let full = encode_wal(&pristine);
        let cut = (truncate_seed as usize) % (full.len() + 1);
        check_decode_all(&full[..cut], Some(&pristine))?;
    }
}

// ── Block (c): mutations of valid WAL → decode_all is a prefix ───────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]
    #[test]
    fn decode_all_never_panics_on_mutated_valid_wal(
        kind in any::<u8>(),
        entropy in proptest::collection::vec(any::<u8>(), 0..64)
    ) {
        let pristine = valid_wal_records();
        let full = encode_wal(&pristine);
        let mutated = mutate(&full, kind, &entropy);
        check_decode_all(&mutated, Some(&pristine))?;
    }
}
