//! On-disk format robustness: `wal::decode_all` and `snapshot::decode`
//! never panic, and mutated valid WAL streams decode as a prefix of the
//! pristine record list.
//!
//! Five generators, 256 cases each (1280 total):
//!   (a) arbitrary byte vectors (len 0..4096) → `wal::decode_all`
//!   (b) arbitrary byte vectors (len 0..4096) → `snapshot::decode`
//!   (c) bit-flip / truncate / splice mutations of a valid WAL stream
//!       (several records including a `Batch` and a `RebuildRule`) and a
//!       valid encoded `SnapshotState`
//!   (d) mutate the raw v3 snapshot *payload*, then reattach a fresh valid
//!       header (magic + version + CRC of the mutated payload) so
//!       `bincode::deserialize` is actually reached
//!   (e) V8 section-directory mutations: set a section `len` to u32::MAX
//!       (out-of-bounds), 1 (below rkyv root minimum), or an arbitrary u32,
//!       then recompute the header CRC so `parse_header` succeeds and
//!       `validate_section_bounds` is actually exercised.  Both
//!       `MappedBase::validate_section_bounds` and `snapshot::decode` are
//!       called; neither may panic.  Random seed (proptest default);
//!       deterministic hardcoded mutations (u32::MAX / 1 / arbitrary) provide
//!       the coverage floor regardless of seed.
//!   (f) bit-flips inside the V9 shared string table (section 12), with both
//!       the section CRC and the header CRC recomputed so the mutation is
//!       reached rather than rejected as a checksum failure.  `snapshot::decode`
//!       must return `Ok` or `Err(Corrupt)` — never panic, never resolve a
//!       corrupted relative pointer out of bounds.  A deterministic companion
//!       test smashes the root relative pointer directly and asserts `Corrupt`.

use core_storage::snapshot::{self, SnapshotState};
use core_storage::v8::MappedBase;
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
    // A string property so section 12 (the shared string table) is non-empty:
    // an empty table's rkyv root has no relative pointer to corrupt, and block
    // (f) needs one.
    props.set(0, "name", Value::Str("alice".into()));
    props.set(1, "name", Value::Str("bob".into()));

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

// ---------------------------------------------------------------------------
// Block (e) helpers
// ---------------------------------------------------------------------------

/// Return a copy of `v8` with section `section_idx`'s `len` field replaced by
/// `new_len` and the header CRC recomputed so `parse_header` passes and
/// `validate_section_bounds` is actually exercised.
///
/// V8 directory layout: `[8 + i*16 + 8 .. 8 + i*16 + 12]` = len (u32 LE).
/// Header CRC covers `[0 .. 8 + section_count * 16]`.
///
/// Returns `None` when the bytes are too short to contain the requested entry.
fn corrupt_v8_section_len(v8: &[u8], section_idx: usize, new_len: u32) -> Option<Vec<u8>> {
    if v8.len() < 12 {
        return None;
    }
    let section_count = u16::from_le_bytes([v8[6], v8[7]]) as usize;
    if section_idx >= section_count {
        return None;
    }
    let dir_end = 8 + section_count * 16;
    if dir_end + 4 > v8.len() {
        return None;
    }
    let len_offset = 8 + section_idx * 16 + 8;
    if len_offset + 4 > dir_end {
        return None;
    }
    let mut out = v8.to_vec();
    out[len_offset..len_offset + 4].copy_from_slice(&new_len.to_le_bytes());
    let crc = crc32fast::hash(&out[0..dir_end]);
    out[dir_end..dir_end + 4].copy_from_slice(&crc.to_le_bytes());
    Some(out)
}

// Block (e): V8 section-directory corruption — validate_section_bounds never panics.
//
// Mutates individual section `len` fields in a valid V8 snapshot, recomputes
// the header CRC so `parse_header` passes, then verifies that both
// `MappedBase::validate_section_bounds` and `snapshot::decode` return
// `Err(Corrupt)` rather than panicking.  Random seed (proptest default);
// deterministic hardcoded mutations (u32::MAX / 1 / arbitrary u32) provide
// the coverage floor regardless of seed.  256 cases.
proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]
    #[test]
    fn v8_section_directory_corruption_never_panics(
        section_idx in 0usize..13usize,
        mutation in 0u8..3u8,
        len_val in any::<u32>(),
    ) {
        let v8 = valid_snapshot_bytes();
        let new_len = match mutation % 3 {
            0 => u32::MAX,  // section extends past any plausible file
            1 => 1,         // below rkyv root minimum for large sections
            _ => len_val,   // arbitrary value
        };
        if let Some(bytes) = corrupt_v8_section_len(&v8, section_idx, new_len) {
            // Path 1: validate_section_bounds directly — must not panic.
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                if let Ok(base) = MappedBase::from_bytes(bytes.clone()) {
                    let _ = base.validate_section_bounds();
                }
            }));
            prop_assert!(
                outcome.is_ok(),
                "validate_section_bounds panicked on section_idx={section_idx} new_len={new_len}"
            );
            // Path 2: full snapshot::decode — must also not panic.
            check_snap_decode(&bytes)?;
        }
    }
}

// ---------------------------------------------------------------------------
// Block (f): V9 shared string table (section 12) corruption
// ---------------------------------------------------------------------------

/// Locate `(offset, len)` of the directory entry for `section_id`.
fn v8_section_span(v8: &[u8], section_id: u8) -> Option<(usize, usize)> {
    if v8.len() < 12 {
        return None;
    }
    let section_count = u16::from_le_bytes([v8[6], v8[7]]) as usize;
    for i in 0..section_count {
        let base = 8 + i * 16;
        if base + 16 > v8.len() {
            return None;
        }
        if v8[base] == section_id {
            let off = u32::from_le_bytes(v8[base + 4..base + 8].try_into().unwrap()) as usize;
            let len = u32::from_le_bytes(v8[base + 8..base + 12].try_into().unwrap()) as usize;
            return Some((off, len));
        }
    }
    None
}

/// XOR `mask` into the byte at `byte_idx` of section `section_id`, then
/// recompute that section's CRC32 and the whole-header CRC32 so the mutation
/// survives both checksum gates and is actually decoded.
fn corrupt_v8_section_payload(
    v8: &[u8],
    section_id: u8,
    byte_idx: usize,
    mask: u8,
) -> Option<Vec<u8>> {
    let (off, len) = v8_section_span(v8, section_id)?;
    if len == 0 || off + len > v8.len() {
        return None;
    }
    let mut out = v8.to_vec();
    out[off + byte_idx % len] ^= if mask == 0 { 0xff } else { mask };

    let section_count = u16::from_le_bytes([out[6], out[7]]) as usize;
    let dir_end = 8 + section_count * 16;
    for i in 0..section_count {
        let base = 8 + i * 16;
        if out[base] == section_id {
            let crc = crc32fast::hash(&out[off..off + len]);
            out[base + 12..base + 16].copy_from_slice(&crc.to_le_bytes());
            break;
        }
    }
    let header_crc = crc32fast::hash(&out[0..dir_end]);
    out[dir_end..dir_end + 4].copy_from_slice(&header_crc.to_le_bytes());
    Some(out)
}

/// The fixture must actually carry a shared string table, or block (f) would
/// silently test nothing.
#[test]
fn fixture_snapshot_carries_a_shared_string_table() {
    let v9 = valid_snapshot_bytes();
    assert_eq!(
        u16::from_le_bytes([v9[4], v9[5]]),
        snapshot::VERSION_9,
        "the fixture must be a V9 snapshot"
    );
    let (_, len) = v8_section_span(&v9, core_storage::v8::SECTION_STRINGS)
        .expect("a V9 snapshot must carry section 12");
    assert!(
        len > 16,
        "section 12 must hold a real table (got {len} bytes), else there is no \
         relative pointer to corrupt"
    );
}

/// A smashed relative pointer in the shared string table must surface as
/// `Corrupt` from the validated decode path, not as a panic or an OOB read.
///
/// The rkyv root of `StringTableData` sits in the last `size_of::<Archived>()`
/// bytes of the payload and begins with the `ArchivedVec` relative pointer, so
/// the final 8 bytes are where the pointer and length live.
#[test]
fn a_smashed_string_table_pointer_decodes_as_corrupt() {
    let v9 = valid_snapshot_bytes();
    let (off, len) = v8_section_span(&v9, core_storage::v8::SECTION_STRINGS)
        .expect("a V9 snapshot must carry section 12");
    let mut any_detected = false;
    for byte in (len - 8)..len {
        let Some(bytes) =
            corrupt_v8_section_payload(&v9, core_storage::v8::SECTION_STRINGS, byte, 0xff)
        else {
            continue;
        };
        let outcome = catch_unwind(AssertUnwindSafe(|| snapshot::decode(&bytes)));
        let decoded = match outcome {
            Ok(r) => r,
            Err(panic) => panic!(
                "snapshot::decode panicked on a corrupted string-table pointer \
                 (section offset {off}, byte {byte}): {}",
                panic_message(panic)
            ),
        };
        match decoded {
            Err(core_storage::GraphError::Corrupt { detail }) => {
                // Pins *which* layer caught it: the validated `rkyv::access` on
                // the strings section, not a CRC or a bounds check upstream.
                assert!(
                    detail.contains("strings"),
                    "a corrupted section 12 must be reported by the strings \
                     structural check; got: {detail}"
                );
                any_detected = true;
            }
            Err(other) => panic!("expected Corrupt, got {other:?}"),
            // A flip inside the length field can still describe a structurally
            // valid (if wrong) archive; that is not a safety failure.
            Ok(_) => {}
        }
    }
    assert!(
        any_detected,
        "smashing the string-table root pointer must be reported as Corrupt by \
         at least one of the eight root bytes"
    );
}

// Block (f): arbitrary bit-flips anywhere in section 12, CRCs recomputed.
// `snapshot::decode` must never panic and must never read out of bounds.
// 256 cases.
proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]
    #[test]
    fn v9_string_table_corruption_never_panics(
        byte_idx in 0usize..4096usize,
        mask in any::<u8>(),
    ) {
        let v9 = valid_snapshot_bytes();
        if let Some(bytes) =
            corrupt_v8_section_payload(&v9, core_storage::v8::SECTION_STRINGS, byte_idx, mask)
        {
            // Path 1: the validated decode path.
            check_snap_decode(&bytes)?;
            // Path 2: `mushroomdb verify`'s structural pass over section 12.
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                if let Ok(base) = MappedBase::from_bytes(bytes.clone()) {
                    let _ = base.validate_section_bounds();
                    let _ = base.validate_hot_sections();
                }
            }));
            prop_assert!(
                outcome.is_ok(),
                "validate_hot_sections panicked on a corrupted section 12 \
                 (byte_idx={byte_idx} mask={mask})"
            );
        }
    }
}
