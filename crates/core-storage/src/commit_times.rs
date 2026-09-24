//! Commit → wall-clock sidecar.
//!
//! The engine has never recorded when anything happened. `edges_at`,
//! `was_linked` and the two history readouts address history by 0-based WAL
//! commit index, and `docs/site/mcp.md` says so outright: *"commits carry no
//! dates: take `at` from node_history/edge_history commit numbers or the
//! dataset's date→commit map."* Every time-travel question a caller actually
//! asks is phrased in calendar time, so the caller has to build that map — and
//! a wrong guess returns a plausible wrong graph rather than an error.
//!
//! This module is the map, and it deliberately lives **outside** both formats.
//! It is a sidecar in the sense [`crate::fs::FileId::Roles`] is one: written
//! atomically beside the store, loaded at open, and **never part of the WAL or
//! snapshot**. A release that does not know this file does not read it, and
//! opens the store exactly as it did before.
//!
//! That choice is not stylistic. A new WAL discriminant is permitted —
//! `docs/format-stability.md:120` makes discriminants 0–23 append-only — but
//! `decode_all` treats any frame it cannot deserialise as a corrupt tail and
//! returns the valid prefix, and `repair_wal` defaults to `true`, so an older
//! binary would **persist that truncation**. `format-stability.md:140` records
//! exactly that outcome for discriminant 23. A file an old reader ignores has
//! none of that failure mode.
//!
//! # Ordering
//!
//! Entries are kept sorted by **commit**, never by time, and resolution is
//! defined on commit order. A clock that steps backwards — NTP correction,
//! a VM restored from a snapshot — must not make a date ambiguous or cause an
//! entry to be rejected. What was observed is what is stored.

use crate::types::GraphError;

/// Magic for the sidecar's 4-byte header. "Mushroom TiMeS".
pub const COMMIT_TIMES_MAGIC: [u8; 4] = *b"MTMS";

/// Format version of the sidecar itself. Independent of the snapshot and WAL
/// versions, because this file is part of neither.
pub const COMMIT_TIMES_VERSION: u16 = 1;

/// Header: magic(4) + version(2) + floor_commit(8).
///
/// Deliberately carries **no entry count**. The count is derived from the file
/// length, which is what lets a commit append its own 16 bytes with
/// [`crate::fs::Fs::append`] instead of rewriting the map. A counted header
/// would make every commit O(entries) — unacceptable on the write path of a
/// store that runs for weeks.
pub const HEADER_LEN: usize = 4 + 2 + 8;
/// One entry: commit(8) + unix_ms(8).
pub const ENTRY_LEN: usize = 8 + 8;

/// A commit → wall-clock map, sorted by commit and append-only in practice.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommitTimes {
    /// The lowest commit this map still describes. Raised by truncation; an
    /// instant below its recorded time cannot be resolved and must be refused
    /// by name rather than clamped.
    floor_commit: u64,
    /// `(commit, unix_ms)`, ascending by commit. Never sorted by time.
    entries: Vec<(u64, i64)>,
}

impl CommitTimes {
    /// The lowest commit described. `0` on an empty map.
    pub fn floor_commit(&self) -> u64 {
        self.floor_commit
    }

    /// The recorded time of the oldest entry, or `None` when empty.
    pub fn floor_ms(&self) -> Option<i64> {
        self.entries.first().map(|&(_, ms)| ms)
    }

    /// The highest commit described, or `None` when empty.
    pub fn newest_commit(&self) -> Option<u64> {
        self.entries.last().map(|&(c, _)| c)
    }

    /// Number of entries held.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the map describes no commits at all.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The recorded time of `commit`, when the map holds one.
    pub fn time_of(&self, commit: u64) -> Option<i64> {
        self.entries
            .binary_search_by_key(&commit, |&(c, _)| c)
            .ok()
            .map(|i| self.entries[i].1)
    }

    /// Record `commit` as having happened at `unix_ms`.
    ///
    /// A commit at or below [`Self::newest_commit`] is ignored. Replay and
    /// `refresh()` can present a commit twice, and a second entry for one
    /// commit would make the map's sort order ambiguous. The **first**
    /// observation wins, because that is the one that happened.
    ///
    /// A `unix_ms` lower than its predecessor's is stored as observed — see the
    /// module note on ordering.
    pub fn push(&mut self, commit: u64, unix_ms: i64) {
        if let Some(newest) = self.newest_commit() {
            if commit <= newest {
                return;
            }
        } else {
            self.floor_commit = commit;
        }
        self.entries.push((commit, unix_ms));
    }

    /// Drop every entry below `commit` and raise the floor to it.
    ///
    /// Called when the WAL horizon moves — `snapshot --truncate` discards
    /// history, and an entry that outlives the commit it describes is worse
    /// than no entry: it would resolve a date to a commit the engine can no
    /// longer replay.
    pub fn truncate_below(&mut self, commit: u64) {
        self.entries.retain(|&(c, _)| c >= commit);
        self.floor_commit = self.entries.first().map(|&(c, _)| c).unwrap_or(commit);
    }

    /// The greatest commit whose recorded time is at or before `unix_ms`.
    ///
    /// `wal_horizon_floor` is the oldest commit the WAL can still replay. A
    /// resolved commit below it is refused with [`GraphError::CommitOutOfRange`]
    /// rather than returned, because the engine cannot produce that graph.
    ///
    /// Errors, each naming what it can answer rather than guessing:
    ///
    /// - [`GraphError::NoRecordedTime`] — the map is empty, so this store
    ///   records no times at all.
    /// - [`GraphError::TimeBeforeFloor`] — `unix_ms` predates the oldest entry.
    /// - [`GraphError::CommitOutOfRange`] — resolved below the WAL horizon.
    pub fn resolve_instant(&self, unix_ms: i64, wal_horizon_floor: u64) -> Result<u64, GraphError> {
        let Some(&(_, first_ms)) = self.entries.first() else {
            return Err(GraphError::NoRecordedTime);
        };
        if unix_ms < first_ms {
            return Err(GraphError::TimeBeforeFloor {
                floor_ms: first_ms,
                floor_commit: self.floor_commit,
            });
        }
        // Walk by commit order, not by time: a backwards clock step must not
        // reorder the map, so a binary search on `unix_ms` would be unsound.
        // The scan takes the last entry at or before the target.
        let mut answer = self.entries[0].0;
        for &(commit, ms) in &self.entries {
            if ms <= unix_ms {
                answer = commit;
            }
        }
        if answer < wal_horizon_floor {
            return Err(GraphError::CommitOutOfRange {
                commit: answer,
                total: self.newest_commit().map(|c| c + 1).unwrap_or(0),
                floor: wal_horizon_floor,
            });
        }
        Ok(answer)
    }
}

/// Serialise the whole map. Used on first write and after truncation; a plain
/// commit appends [`encode_entry`] instead.
pub fn encode(t: &CommitTimes) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + t.entries.len() * ENTRY_LEN);
    out.extend_from_slice(&COMMIT_TIMES_MAGIC);
    out.extend_from_slice(&COMMIT_TIMES_VERSION.to_le_bytes());
    out.extend_from_slice(&t.floor_commit.to_le_bytes());
    for &(commit, ms) in &t.entries {
        out.extend_from_slice(&encode_entry(commit, ms));
    }
    out
}

/// One entry's 16 bytes, for appending to an existing sidecar.
///
/// Appending is what keeps stamping O(1) per commit. A torn append leaves a
/// partial trailing entry, which [`decode`] refuses as `Corrupt` — degrading
/// the date surface, never the store.
pub fn encode_entry(commit: u64, unix_ms: i64) -> [u8; ENTRY_LEN] {
    let mut buf = [0u8; ENTRY_LEN];
    buf[..8].copy_from_slice(&commit.to_le_bytes());
    buf[8..].copy_from_slice(&unix_ms.to_le_bytes());
    buf
}

/// Parse the sidecar's on-disk bytes.
///
/// Every disagreement between the header and the body is
/// [`GraphError::Corrupt`] naming what disagreed. Nothing here panics or
/// unwraps on caller bytes: the file is not covered by WAL CRC or snapshot
/// integrity, so a torn write must degrade the date surface, not the store.
pub fn decode(bytes: &[u8]) -> Result<CommitTimes, GraphError> {
    let corrupt = |detail: String| GraphError::Corrupt { detail };
    if bytes.len() < HEADER_LEN {
        return Err(corrupt(format!(
            "commit_times: {} bytes is shorter than the {HEADER_LEN}-byte header",
            bytes.len()
        )));
    }
    if bytes[..4] != COMMIT_TIMES_MAGIC {
        return Err(corrupt(format!(
            "commit_times: bad magic {:02x?}, expected {:02x?}",
            &bytes[..4],
            COMMIT_TIMES_MAGIC
        )));
    }
    let version = u16::from_le_bytes([bytes[4], bytes[5]]);
    if version != COMMIT_TIMES_VERSION {
        return Err(corrupt(format!(
            "commit_times: unsupported version {version}, this build writes {COMMIT_TIMES_VERSION}"
        )));
    }
    let floor_commit = u64::from_le_bytes(
        bytes[6..14]
            .try_into()
            .map_err(|_| corrupt("commit_times: truncated floor".into()))?,
    );

    // The count is the body length, not a header field — see HEADER_LEN. A
    // partial trailing entry means a torn append and is refused rather than
    // silently dropped: a half-written commit time would resolve dates to the
    // wrong commit.
    let body = &bytes[HEADER_LEN..];
    if !body.len().is_multiple_of(ENTRY_LEN) {
        return Err(corrupt(format!(
            "commit_times: body is {} bytes, not a whole number of {ENTRY_LEN}-byte \
             entries — a torn append",
            body.len()
        )));
    }
    let count = body.len() / ENTRY_LEN;

    let mut entries = Vec::with_capacity(count);
    let mut prev: Option<u64> = None;
    for chunk in body.chunks_exact(ENTRY_LEN) {
        let commit = u64::from_le_bytes(chunk[..8].try_into().expect("8 bytes"));
        let ms = i64::from_le_bytes(chunk[8..].try_into().expect("8 bytes"));
        if let Some(p) = prev {
            if commit <= p {
                return Err(corrupt(format!(
                    "commit_times: entries must ascend by commit; {commit} followed {p}"
                )));
            }
        }
        prev = Some(commit);
        entries.push((commit, ms));
    }
    Ok(CommitTimes {
        floor_commit,
        entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_round_trip_preserves_every_entry() {
        let mut t = CommitTimes::default();
        t.push(1, 1_700_000_000_000);
        t.push(2, 1_700_000_001_000);
        let back = decode(&encode(&t)).expect("decode");
        assert_eq!(back, t);
        assert_eq!(back.newest_commit(), Some(2));
        assert_eq!(back.floor_commit(), 1);
    }

    #[test]
    fn a_backwards_clock_is_stored_as_observed() {
        // NTP can step the clock back. The map is ordered by commit, not by
        // time, so a lower timestamp at a higher commit must not be rejected,
        // reordered, or lost — resolution is defined on commit order.
        let mut t = CommitTimes::default();
        t.push(1, 1_700_000_005_000);
        t.push(2, 1_700_000_000_000);
        let back = decode(&encode(&t)).expect("decode");
        assert_eq!(back.newest_commit(), Some(2));
        assert_eq!(back.len(), 2);
    }

    #[test]
    fn a_repeated_commit_does_not_create_a_second_entry() {
        // Replay and refresh() can present a commit twice.
        let mut t = CommitTimes::default();
        t.push(7, 1_000);
        t.push(7, 9_999);
        t.push(6, 9_999);
        assert_eq!(t.len(), 1);
        assert_eq!(t.resolve_instant(5_000, 0).unwrap(), 7);
    }

    #[test]
    fn truncate_below_raises_the_floor_and_drops_entries() {
        let mut t = CommitTimes::default();
        for c in 1..=5 {
            t.push(c, 1_700_000_000_000 + c as i64 * 1000);
        }
        t.truncate_below(3);
        assert_eq!(t.floor_commit(), 3);
        assert_eq!(t.newest_commit(), Some(5));
        assert_eq!(t.len(), 3);
    }

    #[test]
    fn truncating_past_the_end_empties_the_map_and_keeps_the_floor() {
        let mut t = CommitTimes::default();
        t.push(1, 1_000);
        t.truncate_below(99);
        assert!(t.is_empty());
        assert_eq!(t.floor_commit(), 99);
        assert!(matches!(
            t.resolve_instant(1_000, 0),
            Err(GraphError::NoRecordedTime)
        ));
    }

    #[test]
    fn a_wrong_magic_is_corrupt_not_a_panic() {
        let mut bytes = encode(&CommitTimes::default());
        bytes[0] = b'X';
        assert!(matches!(decode(&bytes), Err(GraphError::Corrupt { .. })));
    }

    #[test]
    fn an_unknown_version_is_corrupt_not_a_panic() {
        let mut bytes = encode(&CommitTimes::default());
        bytes[4] = 0xFF;
        bytes[5] = 0xFF;
        assert!(matches!(decode(&bytes), Err(GraphError::Corrupt { .. })));
    }

    #[test]
    fn a_truncated_body_is_corrupt_not_a_panic() {
        let mut t = CommitTimes::default();
        t.push(1, 1_700_000_000_000);
        let mut bytes = encode(&t);
        bytes.truncate(bytes.len() - 3);
        assert!(matches!(decode(&bytes), Err(GraphError::Corrupt { .. })));
    }

    #[test]
    fn an_empty_slice_is_corrupt_not_a_panic() {
        assert!(matches!(decode(&[]), Err(GraphError::Corrupt { .. })));
    }

    #[test]
    fn descending_entries_on_disk_are_refused() {
        // Hand-built bytes: the encoder cannot produce this, so only a damaged
        // or forged file can. Refuse rather than resolve against it.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&COMMIT_TIMES_MAGIC);
        bytes.extend_from_slice(&COMMIT_TIMES_VERSION.to_le_bytes());
        bytes.extend_from_slice(&1u64.to_le_bytes());
        bytes.extend_from_slice(&encode_entry(5, 1_000));
        bytes.extend_from_slice(&encode_entry(3, 2_000));
        assert!(matches!(decode(&bytes), Err(GraphError::Corrupt { .. })));
    }

    #[test]
    fn an_appended_entry_decodes_as_if_the_map_were_rewritten() {
        // This is the property that lets stamping be O(1) per commit: the
        // writer appends 16 bytes rather than rewriting the file.
        let mut t = CommitTimes::default();
        t.push(1, 1_000);
        let mut on_disk = encode(&t);
        on_disk.extend_from_slice(&encode_entry(2, 2_000));

        let mut rewritten = CommitTimes::default();
        rewritten.push(1, 1_000);
        rewritten.push(2, 2_000);

        assert_eq!(decode(&on_disk).expect("decode"), rewritten);
    }

    #[test]
    fn a_torn_append_is_refused_not_silently_dropped() {
        // A half-written trailing entry must not be discarded: the map would
        // then resolve dates to the wrong commit and never say so.
        let mut t = CommitTimes::default();
        t.push(1, 1_000);
        let mut on_disk = encode(&t);
        on_disk.extend_from_slice(&encode_entry(2, 2_000)[..9]);
        assert!(matches!(decode(&on_disk), Err(GraphError::Corrupt { .. })));
    }

    #[test]
    fn a_header_only_file_decodes_as_empty() {
        let bytes = encode(&CommitTimes::default());
        assert_eq!(bytes.len(), HEADER_LEN);
        assert!(decode(&bytes).expect("decode").is_empty());
    }

    fn fixture() -> CommitTimes {
        let mut t = CommitTimes::default();
        t.push(10, 1_000);
        t.push(20, 2_000);
        t.push(30, 3_000);
        t
    }

    #[test]
    fn resolves_to_the_last_commit_at_or_before_the_instant() {
        let t = fixture();
        assert_eq!(t.resolve_instant(2_500, 0).unwrap(), 20);
        assert_eq!(
            t.resolve_instant(2_000, 0).unwrap(),
            20,
            "an exact match is inclusive"
        );
    }

    #[test]
    fn an_instant_after_the_newest_commit_resolves_to_it() {
        assert_eq!(fixture().resolve_instant(9_999, 0).unwrap(), 30);
    }

    #[test]
    fn an_instant_before_the_floor_names_the_range() {
        match fixture().resolve_instant(500, 0) {
            Err(GraphError::TimeBeforeFloor {
                floor_ms,
                floor_commit,
            }) => assert_eq!((floor_ms, floor_commit), (1_000, 10)),
            other => panic!("expected TimeBeforeFloor, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_map_refuses_by_name() {
        assert!(matches!(
            CommitTimes::default().resolve_instant(1_000, 0),
            Err(GraphError::NoRecordedTime)
        ));
    }

    #[test]
    fn a_resolved_commit_is_never_below_the_wal_horizon() {
        // The map may outlive the WAL. Resolving to a commit the engine can no
        // longer replay would hand back a graph it cannot produce.
        match fixture().resolve_instant(1_500, 25) {
            Err(GraphError::CommitOutOfRange { floor, commit, .. }) => {
                assert_eq!(floor, 25);
                assert_eq!(commit, 10, "it names the commit it resolved to");
            }
            other => panic!("expected CommitOutOfRange, got {other:?}"),
        }
    }

    #[test]
    fn a_backwards_step_resolves_on_commit_order() {
        // Commit 20 is recorded as *earlier* than commit 10. Asking for 1_500
        // must still answer on commit order: the last entry whose time is at or
        // before the target, scanning ascending by commit — which is 20.
        let mut t = CommitTimes::default();
        t.push(10, 1_000);
        t.push(20, 500);
        t.push(30, 3_000);
        assert_eq!(t.resolve_instant(1_500, 0).unwrap(), 20);
    }
}
