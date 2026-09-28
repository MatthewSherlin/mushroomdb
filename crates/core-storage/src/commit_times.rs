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
///
/// **v1 → v2 (v0.6.12).** The entries are unchanged in shape and changed in
/// meaning. v1 recorded `commit_seq - 1`, which is a WAL frame index only on a
/// store whose rules never fire: a rule-firing commit appends a second frame
/// for its history marker, so v1 entries fall one frame further behind per such
/// commit and resolve every date to an ever-earlier graph. v2 records the
/// global frame index of the last frame the commit wrote, which is the space
/// `edges_at`, `was_linked` and the history readouts actually address.
///
/// A v1 file cannot be repaired in place — the drift depends on which commits
/// fired rules, which the file does not record — so it is discarded on open
/// rather than reinterpreted. See [`superseded_version`].
pub const COMMIT_TIMES_VERSION: u16 = 2;

/// Versions this build recognises, cannot use, and must not mistake for damage.
///
/// A file at one of these versions was written correctly by an older release;
/// it is superseded, not corrupt. The difference matters to a caller: damage
/// warrants investigating the store, while a superseded map simply means dates
/// start again from the next commit.
pub const COMMIT_TIMES_SUPERSEDED_VERSIONS: &[u16] = &[1];

/// `Some(version)` when `bytes` is a well-formed sidecar this build recognises
/// but can no longer read, so the caller can discard it instead of reporting
/// the store as damaged.
pub fn superseded_version(bytes: &[u8]) -> Option<u16> {
    if bytes.len() < HEADER_LEN || bytes[..4] != COMMIT_TIMES_MAGIC {
        return None;
    }
    let version = u16::from_le_bytes([bytes[4], bytes[5]]);
    COMMIT_TIMES_SUPERSEDED_VERSIONS
        .contains(&version)
        .then_some(version)
}

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

    /// The greatest time any entry records.
    ///
    /// Not the newest commit's time: a live clock can step backwards, so the
    /// newest commit may carry a lower instant than one before it. An asserted
    /// time is checked against this rather than against the last entry, because
    /// the question is "does this go backwards against anything already
    /// recorded", not "against the most recent".
    pub fn max_ms(&self) -> Option<i64> {
        self.entries.iter().map(|&(_, ms)| ms).max()
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

// ─── RFC 3339 → unix milliseconds ────────────────────────────────────────────
//
// Hand-rolled on purpose. The workspace has no date crate — `ingest-git` stores
// a git commit time as a plain `Int` and the only other time handling in the
// tree is `duration_since(UNIX_EPOCH)` — and adding one to parse a timestamp
// would be a runtime dependency for sixty lines of arithmetic. The civil-date
// conversion below is the standard days-from-civil algorithm: closed form, no
// tables, no leap seconds, and deterministic on every platform.

/// Days since 1970-01-01 for a proleptic-Gregorian civil date.
///
/// `m` is 1–12 and `d` is 1–31; callers validate the ranges. Correct for any
/// year the `i64` holds, negative ones included.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn digits(s: &str) -> Option<i64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<i64>().ok()
}

/// Parse an RFC 3339 instant, or a bare `YYYY-MM-DD`, to unix milliseconds.
///
/// Accepted:
/// - `2026-06-19` — midnight UTC, which is what a caller asking "on that day"
///   means
/// - `2026-06-19T12:34:56Z`
/// - `2026-06-19T12:34:56.789Z` — fractional seconds, truncated to ms
/// - `2026-06-19T12:34:56+01:00` / `-05:00` — offsets are applied
/// - a space instead of `T`
///
/// Returns `None` on anything else. A caller that cannot parse a date must say
/// so rather than guess an instant — the whole point of this module is that a
/// guessed time becomes a confidently wrong graph.
pub fn parse_rfc3339_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, rest) = match s.find(['T', 't', ' ']) {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    };

    let mut dp = date.split('-');
    let (y, mo, d) = (dp.next()?, dp.next()?, dp.next()?);
    if dp.next().is_some() {
        return None;
    }
    let (y, mo, d) = (digits(y)?, digits(mo)?, digits(d)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || y < 1 {
        return None;
    }
    let days = days_from_civil(y, mo, d);

    let Some(rest) = rest else {
        return Some(days * 86_400_000);
    };

    // Split the offset off the time-of-day.
    let (time, offset_ms) = if let Some(t) = rest.strip_suffix('Z').or(rest.strip_suffix('z')) {
        (t, 0i64)
    } else if let Some(i) = rest.rfind(['+', '-']) {
        let (t, off) = rest.split_at(i);
        let sign = if off.starts_with('-') { -1 } else { 1 };
        let off = &off[1..];
        let (oh, om) = match off.split_once(':') {
            Some((a, b)) => (digits(a)?, digits(b)?),
            None if off.len() == 4 => (digits(&off[..2])?, digits(&off[2..])?),
            _ => return None,
        };
        if !(0..=23).contains(&oh) || !(0..=59).contains(&om) {
            return None;
        }
        (t, sign * (oh * 3_600_000 + om * 60_000))
    } else {
        // No zone. RFC 3339 requires one; treating a naked time as UTC is the
        // forgiving reading and the one an agent writing "2026-06-19T12:00:00"
        // means.
        (rest, 0i64)
    };

    let mut tp = time.split(':');
    let h = digits(tp.next()?)?;
    let mi = digits(tp.next()?)?;
    let (sec, frac_ms) = match tp.next() {
        None => (0i64, 0i64),
        Some(sec_field) => match sec_field.split_once('.') {
            None => (digits(sec_field)?, 0),
            Some((whole, frac)) => {
                if frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                // Truncate rather than round: the resolver takes the last commit
                // at or before the instant, so rounding up could step past one.
                let mut ms = 0i64;
                for (i, b) in frac.bytes().take(3).enumerate() {
                    ms += (b - b'0') as i64 * 10i64.pow(2 - i as u32);
                }
                (digits(whole)?, ms)
            }
        },
    };
    if tp.next().is_some() {
        return None;
    }
    // 60 allows a leap second, which we fold into the following second.
    if !(0..=23).contains(&h) || !(0..=59).contains(&mi) || !(0..=60).contains(&sec) {
        return None;
    }

    Some(days * 86_400_000 + h * 3_600_000 + mi * 60_000 + sec * 1_000 + frac_ms - offset_ms)
}

/// The **inclusive end** of the instant a string denotes.
///
/// A bare `YYYY-MM-DD` denotes a *day*, not the midnight that starts it, so its
/// end is `23:59:59.999` — otherwise asking for a date excludes everything that
/// happened on that date, which is never what the question means. A string that
/// names a time denotes that instant exactly, and its end is itself.
///
/// This is what date resolution wants: "the last commit at or before the end of
/// what you named".
pub fn parse_rfc3339_end_ms(s: &str) -> Option<i64> {
    let t = s.trim();
    let ms = parse_rfc3339_ms(t)?;
    // Date-only when there is no time separator at all.
    let date_only = !t.contains(['T', 't']) && !t.contains(' ');
    Some(if date_only { ms + 86_400_000 - 1 } else { ms })
}

#[cfg(test)]
mod rfc3339_tests {
    use super::*;

    #[test]
    fn a_bare_date_denotes_the_whole_day() {
        // The bug this exists to stop: a bare date resolving to the midnight
        // that *starts* the day excludes everything that happened on it.
        let start = parse_rfc3339_ms("2026-07-14").unwrap();
        let end = parse_rfc3339_end_ms("2026-07-14").unwrap();
        assert_eq!(end - start, 86_400_000 - 1, "a date must cover its own day");
        // Something that happened at 09:00 that day falls inside it.
        let nine = parse_rfc3339_ms("2026-07-14T09:00:00Z").unwrap();
        assert!(nine > start && nine < end, "09:00 must fall within the day");
    }

    #[test]
    fn a_named_instant_denotes_only_itself() {
        for s in [
            "2026-07-14T12:00:00Z",
            "2026-07-14T12:00:00+01:00",
            "2026-07-14 12:00:00Z",
        ] {
            assert_eq!(
                parse_rfc3339_end_ms(s),
                parse_rfc3339_ms(s),
                "{s} names a time, so its end is itself"
            );
        }
    }

    #[test]
    fn the_epoch_is_zero() {
        assert_eq!(parse_rfc3339_ms("1970-01-01"), Some(0));
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
    }

    #[test]
    fn a_bare_date_is_midnight_utc() {
        // 2026-06-19 is the date the association benchmark's first time-travel
        // task asks about.
        let d = parse_rfc3339_ms("2026-06-19").expect("parse");
        assert_eq!(d % 86_400_000, 0, "a bare date must land on midnight");
        assert_eq!(parse_rfc3339_ms("2026-06-19T00:00:00Z"), Some(d));
    }

    #[test]
    fn a_leap_day_parses() {
        let feb29 = parse_rfc3339_ms("2024-02-29").expect("2024 is a leap year");
        let mar01 = parse_rfc3339_ms("2024-03-01").expect("parse");
        assert_eq!(mar01 - feb29, 86_400_000);
    }

    #[test]
    fn a_century_non_leap_year_is_handled() {
        // 1900 was not a leap year; 2000 was. The closed form must get both.
        let a = parse_rfc3339_ms("1900-03-01").expect("parse");
        let b = parse_rfc3339_ms("1900-02-28").expect("parse");
        assert_eq!(a - b, 86_400_000, "1900 had no Feb 29");
        let c = parse_rfc3339_ms("2000-03-01").expect("parse");
        let d = parse_rfc3339_ms("2000-02-28").expect("parse");
        assert_eq!(c - d, 2 * 86_400_000, "2000 did have Feb 29");
    }

    #[test]
    fn time_of_day_adds_up() {
        let base = parse_rfc3339_ms("2026-06-19").unwrap();
        assert_eq!(
            parse_rfc3339_ms("2026-06-19T01:02:03Z").unwrap() - base,
            3_600_000 + 2 * 60_000 + 3_000
        );
    }

    #[test]
    fn fractional_seconds_truncate_to_milliseconds() {
        let base = parse_rfc3339_ms("2026-06-19T00:00:00Z").unwrap();
        assert_eq!(
            parse_rfc3339_ms("2026-06-19T00:00:00.5Z").unwrap() - base,
            500
        );
        assert_eq!(
            parse_rfc3339_ms("2026-06-19T00:00:00.789Z").unwrap() - base,
            789
        );
        // Truncate, never round up: rounding could step past a commit.
        assert_eq!(
            parse_rfc3339_ms("2026-06-19T00:00:00.9999Z").unwrap() - base,
            999
        );
    }

    #[test]
    fn offsets_are_applied_in_the_right_direction() {
        let utc = parse_rfc3339_ms("2026-06-19T12:00:00Z").unwrap();
        // Noon in +01:00 is 11:00 UTC — an hour *earlier* in absolute time.
        assert_eq!(
            parse_rfc3339_ms("2026-06-19T12:00:00+01:00").unwrap(),
            utc - 3_600_000
        );
        assert_eq!(
            parse_rfc3339_ms("2026-06-19T12:00:00-05:00").unwrap(),
            utc + 5 * 3_600_000
        );
        assert_eq!(
            parse_rfc3339_ms("2026-06-19T12:00:00+0100").unwrap(),
            utc - 3_600_000
        );
    }

    #[test]
    fn a_space_separator_is_accepted() {
        assert_eq!(
            parse_rfc3339_ms("2026-06-19 12:00:00Z"),
            parse_rfc3339_ms("2026-06-19T12:00:00Z")
        );
    }

    #[test]
    fn rubbish_is_refused_rather_than_guessed() {
        for bad in [
            "",
            "not a date",
            "2026",
            "2026-06",
            "2026-13-01",
            "2026-06-32",
            "2026-06-19T25:00:00Z",
            "2026-06-19T12:60:00Z",
            "2026-06-19T12:00:00+99:00",
            "2026-06-19T12:00:00.Z",
            "2026-06-19-01",
            "0000-01-01",
        ] {
            assert_eq!(parse_rfc3339_ms(bad), None, "{bad:?} must not parse");
        }
    }
}
