//! The commit-times sidecar, at the engine level.
//!
//! The unit tests in `core-storage` cover the map's own arithmetic. These cover
//! the thing that can actually go wrong in the engine: **a hole**. Stamping
//! anywhere but the one place a commit is born leaves a commit with no recorded
//! time, and a hole does not announce itself — it resolves a date to the wrong
//! commit and returns a plausible graph.

use core_api::GraphDb;
use core_storage::types::GraphError;
use std::path::PathBuf;

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mdb-commit-times-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

/// Every commit gets an entry, and the entries are contiguous.
///
/// A gap here is the failure this whole task exists to prevent.
#[test]
fn every_commit_is_stamped_and_the_map_has_no_holes() {
    let dir = tmp("no-holes");
    let mut db = GraphDb::open(&dir).expect("open");
    for i in 0..25 {
        db.insert_node("N", &format!("n{i}"), vec![])
            .expect("insert");
    }
    let seq = db.commit_seq();
    assert!(seq >= 25, "expected at least 25 commits, saw {seq}");

    // Frame indices are 0-based, so N commits are indices 0..N-1. Recording
    // `commit_seq` instead would shift every one of these by a commit.
    let mut missing = Vec::new();
    for c in 0..seq {
        if db.commit_time_ms(c).is_none() {
            missing.push(c);
        }
    }
    assert!(
        missing.is_empty(),
        "commits with no recorded time (a hole in the map): {missing:?}"
    );
}

/// Recorded times never go backwards across ordinary commits, and a date
/// resolves to a commit that actually exists.
#[test]
fn a_date_resolves_to_a_real_commit() {
    let dir = tmp("resolves");
    let mut db = GraphDb::open(&dir).expect("open");
    db.insert_node("N", "a", vec![]).expect("insert");
    let early = db
        .commit_time_ms(db.commit_seq() - 1)
        .expect("the first commit is frame 0");

    std::thread::sleep(std::time::Duration::from_millis(5));
    for i in 0..5 {
        db.insert_node("N", &format!("later{i}"), vec![])
            .expect("insert");
    }
    let last = db.commit_seq();

    // An instant at or after the newest commit resolves to the newest frame,
    // which is `commit_seq - 1`.
    let resolved = db.resolve_instant(i64::MAX / 2).expect("resolve");
    assert_eq!(resolved, last - 1);

    // An instant at the first commit's own time resolves to a real frame.
    let at_first = db.resolve_instant(early).expect("resolve");
    assert!(
        at_first < last,
        "resolved frame {at_first} is not a frame of {last} commits"
    );
}

/// A store written before this release records no times. It must open, read and
/// write exactly as before, and say so plainly rather than guess a commit.
#[test]
fn a_store_with_no_sidecar_reads_identically_and_refuses_dates_by_name() {
    let dir = tmp("no-sidecar");
    {
        let mut db = GraphDb::open(&dir).expect("open");
        db.insert_node("N", "a", vec![]).expect("insert");
        db.insert_node("N", "b", vec![]).expect("insert");
        db.insert_edge("KNOWS", "a", "b").expect("edge");
        db.snapshot().expect("snapshot");
    }
    // Remove the sidecar: this is exactly the on-disk shape a pre-v0.6.11
    // release leaves behind.
    std::fs::remove_file(dir.join("commit_times.bin")).expect("rm sidecar");

    let db = GraphDb::open(&dir).expect("reopen without a sidecar");
    // Ordinary reads are untouched.
    assert!(db.node_info("a").is_some(), "the store still reads");
    assert!(db.node_info("b").is_some());
    let stats = db.stats();
    assert_eq!(stats.nodes_live, 2);

    // A date is refused by name — never resolved to a guessed commit.
    match db.resolve_instant(1_700_000_000_000) {
        Err(GraphError::NoRecordedTime) => {}
        other => panic!("expected NoRecordedTime, got {other:?}"),
    }
}

/// The map survives a reopen, because it is a file and not in-memory state.
#[test]
fn reopen_preserves_the_map() {
    let dir = tmp("reopen");
    let before;
    {
        let mut db = GraphDb::open(&dir).expect("open");
        for i in 0..4 {
            db.insert_node("N", &format!("n{i}"), vec![])
                .expect("insert");
        }
        before = db.commit_time_ms(2).expect("frame 2 is stamped");
    }
    let db = GraphDb::open(&dir).expect("reopen");
    assert_eq!(
        db.commit_time_ms(2),
        Some(before),
        "the sidecar did not survive the reopen"
    );
}

/// Replay must not re-stamp. A reopened store's recorded times are the times
/// the commits happened, not the time the WAL was replayed.
#[test]
fn replay_does_not_restamp_with_the_replay_time() {
    let dir = tmp("no-restamp");
    let original;
    {
        let mut db = GraphDb::open(&dir).expect("open");
        db.insert_node("N", "a", vec![]).expect("insert");
        original = db.commit_time_ms(0).expect("the first commit is frame 0");
    }
    // Long enough that a re-stamp would be obvious rather than a rounding blur.
    std::thread::sleep(std::time::Duration::from_millis(40));

    let db = GraphDb::open(&dir).expect("reopen — this replays the WAL");
    let after = db.commit_time_ms(0).expect("still stamped");
    assert_eq!(
        after, original,
        "replay re-stamped frame 0: {original} became {after}. apply_frames \
         must not call SystemTime::now — it re-applies commits that already \
         happened."
    );
}

/// A damaged sidecar degrades the date surface and nothing else. It must not
/// read as "this store records no times", because that is what an honest
/// pre-v0.6.11 store says.
#[test]
fn a_damaged_sidecar_is_reported_as_damage_not_as_absence() {
    let dir = tmp("damaged");
    {
        let mut db = GraphDb::open(&dir).expect("open");
        db.insert_node("N", "a", vec![]).expect("insert");
    }
    std::fs::write(dir.join("commit_times.bin"), b"XXXXnot a time map").expect("corrupt it");

    let db = GraphDb::open(&dir).expect("a damaged sidecar must not fail the open");
    assert!(db.node_info("a").is_some(), "the store still reads");
    match db.resolve_instant(1_700_000_000_000) {
        Err(GraphError::Corrupt { .. }) => {}
        other => panic!("expected Corrupt, got {other:?}"),
    }
}

/// `edges_at` and `was_linked`, addressed by a date.
///
/// This is the shape the association benchmark's four time-travel tasks ask in
/// — "On 2026-06-19, which companies was X linked to by all three of …" — and
/// the shape that previously had no answer but a guessed commit index.
#[test]
fn the_history_surface_takes_a_date() {
    let dir = tmp("by-date");
    let mut db = GraphDb::open(&dir).expect("open");
    db.insert_node("N", "a", vec![]).expect("insert");
    db.insert_node("N", "b", vec![]).expect("insert");
    db.insert_edge("KNOWS", "a", "b").expect("edge");

    // "Now" in the far future resolves to the newest commit, so the edge is
    // there; a date is just a way of naming a commit.
    let far_future = "2099-01-01T00:00:00Z";
    let rows = db
        .edges_at_instant(
            "a",
            core_storage::commit_times::parse_rfc3339_ms(far_future).unwrap(),
        )
        .expect("edges_at by instant");
    assert!(
        rows.iter().any(|e| e.dst_key == "b" || e.src_key == "b"),
        "the edge should be live at the newest commit, got {rows:?}"
    );

    assert!(
        db.was_linked_at_instant(
            "a",
            "b",
            "KNOWS",
            core_storage::commit_times::parse_rfc3339_ms(far_future).unwrap()
        )
        .expect("was_linked by instant"),
        "a and b are linked as of now"
    );

    // resolve_date is the one parser every surface shares.
    let by_string = db.resolve_date(far_future).expect("resolve_date");
    assert_eq!(
        by_string,
        db.commit_seq() - 1,
        "a resolved date is a 0-based frame index, ready for edges_at"
    );
}

/// An unparseable date is refused, and the message says what was expected. A
/// guessed instant would become a confidently wrong graph.
#[test]
fn an_unparseable_date_is_refused_with_a_useful_message() {
    let dir = tmp("bad-date");
    let mut db = GraphDb::open(&dir).expect("open");
    db.insert_node("N", "a", vec![]).expect("insert");

    match db.resolve_date("last Tuesday") {
        Err(GraphError::QueryError { detail }) => {
            assert!(detail.contains("RFC 3339"), "unhelpful message: {detail}");
        }
        other => panic!("expected QueryError, got {other:?}"),
    }
}

/// A date before anything the store recorded names the range it can answer.
#[test]
fn a_date_before_the_first_commit_names_the_floor() {
    let dir = tmp("before-floor");
    let mut db = GraphDb::open(&dir).expect("open");
    db.insert_node("N", "a", vec![]).expect("insert");

    match db.resolve_date("1971-01-01") {
        Err(GraphError::TimeBeforeFloor { floor_commit, .. }) => {
            assert_eq!(floor_commit, 0, "the oldest recorded frame is 0");
        }
        other => panic!("expected TimeBeforeFloor, got {other:?}"),
    }
}

// ─── backfilled history: caller-asserted commit times ───────────────────────
//
// The live clock is right for a live store and wrong for an import. A mirror
// replaying a year of rows stamps every one of them "now", so the data is there
// and no date reaches it — every query answers `TimeBeforeFloor`. These cover
// asserting the instant instead.

fn ms(date: &str) -> i64 {
    core_storage::commit_times::parse_rfc3339_ms(date).expect("parse")
}

#[test]
fn an_asserted_time_is_recorded_instead_of_the_clock() {
    let dir = tmp("asserted");
    let mut db = GraphDb::open(&dir).expect("open");
    db.record_commits_at(Some(ms("2026-06-02")))
        .expect("assert");
    db.insert_node("N", "a", vec![]).expect("insert");

    let t = db.commit_time_ms(0).expect("frame 0 stamped");
    assert_eq!(
        t,
        ms("2026-06-02"),
        "the clock was used instead of the assertion"
    );
}

#[test]
fn the_override_is_sticky_until_changed_or_cleared() {
    let dir = tmp("sticky");
    let mut db = GraphDb::open(&dir).expect("open");
    db.record_commits_at(Some(ms("2026-06-02")))
        .expect("assert");
    for i in 0..3 {
        db.insert_node("N", &format!("a{i}"), vec![])
            .expect("insert");
    }
    for f in 0..3 {
        assert_eq!(db.commit_time_ms(f), Some(ms("2026-06-02")), "frame {f}");
    }

    db.record_commits_at(Some(ms("2026-06-03")))
        .expect("advance");
    db.insert_node("N", "b", vec![]).expect("insert");
    assert_eq!(db.commit_time_ms(3), Some(ms("2026-06-03")));

    // Cleared, the clock comes back — and "now" is far later than 2026-06.
    db.record_commits_at(None).expect("clear");
    db.insert_node("N", "c", vec![]).expect("insert");
    let live = db.commit_time_ms(4).expect("stamped");
    assert!(
        live > ms("2026-06-03"),
        "the clock did not come back: {live}"
    );
}

#[test]
fn going_backwards_is_refused_by_name() {
    let dir = tmp("backwards");
    let mut db = GraphDb::open(&dir).expect("open");
    db.record_commits_at(Some(ms("2026-06-10")))
        .expect("assert");
    db.insert_node("N", "a", vec![]).expect("insert");

    match db.record_commits_at(Some(ms("2026-06-02"))) {
        Err(GraphError::CommitTimeNotMonotonic {
            supplied_ms,
            newest_ms,
        }) => {
            assert_eq!(supplied_ms, ms("2026-06-02"));
            assert_eq!(newest_ms, ms("2026-06-10"));
        }
        other => panic!("expected CommitTimeNotMonotonic, got {other:?}"),
    }
}

#[test]
fn the_same_instant_again_is_allowed() {
    // A day of backfilled rows shares one instant; equal is not backwards.
    let dir = tmp("equal");
    let mut db = GraphDb::open(&dir).expect("open");
    db.record_commits_at(Some(ms("2026-06-02")))
        .expect("assert");
    db.insert_node("N", "a", vec![]).expect("insert");
    db.record_commits_at(Some(ms("2026-06-02")))
        .expect("the same instant must be allowed");
}

#[test]
fn a_backfilled_history_answers_the_dates_it_was_given() {
    // The shape a mirror actually performs, and the thing the association
    // benchmark's world needs: replay a month in order, then ask what the graph
    // looked like mid-month.
    let dir = tmp("backfill");
    let mut db = GraphDb::open(&dir).expect("open");

    db.record_commits_at(Some(ms("2026-06-01"))).expect("day 1");
    db.insert_node("N", "a", vec![]).expect("insert");
    db.insert_node("N", "b", vec![]).expect("insert");

    db.record_commits_at(Some(ms("2026-06-15")))
        .expect("day 15");
    db.insert_edge("KNOWS", "a", "b").expect("edge");

    db.record_commits_at(Some(ms("2026-06-30")))
        .expect("day 30");
    db.insert_node("N", "c", vec![]).expect("insert");

    // Before the edge existed.
    let early = db.resolve_date("2026-06-10").expect("resolve");
    assert!(
        !db.was_linked("a", "b", "KNOWS", early).expect("was_linked"),
        "the edge is dated 2026-06-15 and must not exist on the 10th"
    );
    // After it.
    let late = db.resolve_date("2026-06-20").expect("resolve");
    assert!(
        db.was_linked("a", "b", "KNOWS", late).expect("was_linked"),
        "the edge is dated 2026-06-15 and must exist on the 20th"
    );
    // And a date before the backfill starts is refused, not guessed.
    assert!(matches!(
        db.resolve_date("2026-05-01"),
        Err(GraphError::TimeBeforeFloor { .. })
    ));
}

#[test]
fn a_read_only_handle_refuses_to_assert_a_time() {
    let dir = tmp("ro-assert");
    {
        let mut db = GraphDb::open(&dir).expect("open");
        db.insert_node("N", "a", vec![]).expect("insert");
    }
    let mut ro = GraphDb::open_with_options(
        &dir,
        core_api::OpenOptions {
            read_only: true,
            ..Default::default()
        },
    )
    .expect("open read-only");
    assert!(matches!(
        ro.record_commits_at(Some(ms("2026-06-02"))),
        Err(GraphError::ReadOnly)
    ));
}

/// Asking for a date must include what happened **on** that date.
///
/// The bug this pins: a bare date resolved to the midnight that *starts* the
/// day, so an edge written at 09:00 on the 14th was invisible to a caller asking
/// for "2026-07-14". A date denotes a day, not its first instant.
#[test]
fn a_bare_date_includes_that_days_own_writes() {
    let dir = tmp("day-inclusive");
    let mut db = GraphDb::open(&dir).expect("open");

    db.record_commits_at(Some(ms("2026-07-13"))).expect("13th");
    db.insert_node("N", "a", vec![]).expect("insert");
    db.insert_node("N", "b", vec![]).expect("insert");

    // Mid-morning on the 14th.
    let nine_am =
        core_storage::commit_times::parse_rfc3339_ms("2026-07-14T09:00:00Z").expect("parse");
    db.record_commits_at(Some(nine_am)).expect("14th 09:00");
    db.insert_edge("KNOWS", "a", "b").expect("edge");

    assert!(
        db.was_linked(
            "a",
            "b",
            "KNOWS",
            db.resolve_date("2026-07-14").expect("resolve")
        )
        .expect("was_linked"),
        "an edge written at 09:00 on the 14th must be visible when asking for \
         \"2026-07-14\" — a date is a day, not the midnight that starts it"
    );

    // And the day before still excludes it.
    assert!(
        !db.was_linked(
            "a",
            "b",
            "KNOWS",
            db.resolve_date("2026-07-13").expect("resolve")
        )
        .expect("was_linked"),
        "the edge is dated the 14th and must not be visible on the 13th"
    );

    // A named instant still means exactly itself: 08:00 is before the edge.
    let eight = db.resolve_date("2026-07-14T08:00:00Z").expect("resolve");
    assert!(
        !db.was_linked("a", "b", "KNOWS", eight).expect("was_linked"),
        "08:00 precedes the 09:00 edge"
    );
}

/// A date and a commit index reach exactly as far as each other — no further,
/// and no less far.
///
/// This exists because of a mistake worth not repeating. A truncating snapshot
/// folds the WAL and discards it, so *all* history becomes unreachable; a date
/// query then answers `CommitOutOfRange`, which reads alarmingly like the date
/// surface having broken. It has not: the commit-index call refuses identically,
/// because the history is genuinely gone. Pinning the two together means nobody
/// has to re-derive that from a confusing error, and a future change that makes
/// the date path reach further than the index path — which would mean it is
/// answering from something the engine cannot replay — fails here.
#[test]
fn a_date_reaches_exactly_as_far_as_a_commit_index() {
    let dir = tmp("reach-parity");
    let mut db = GraphDb::open(&dir).expect("open");
    db.record_commits_at(Some(ms("2026-06-01"))).expect("day 1");
    db.insert_node("N", "a", vec![]).expect("insert");
    db.insert_node("N", "b", vec![]).expect("insert");
    db.record_commits_at(Some(ms("2026-06-15")))
        .expect("day 15");
    db.insert_edge("KNOWS", "a", "b").expect("edge");

    // Before any snapshot both paths agree, and both answer.
    let by_date = db.resolve_date("2026-06-20").expect("resolve");
    assert!(db.was_linked("a", "b", "KNOWS", by_date).expect("by date"));
    assert!(db
        .was_linked(
            "a",
            "b",
            "KNOWS",
            db.wal_total_commits().expect("total") - 1
        )
        .expect("by index"));

    // A truncating snapshot discards the WAL: nothing historical is reachable.
    db.snapshot().expect("snapshot");
    assert_eq!(
        db.wal_total_commits().expect("total"),
        0,
        "a truncating snapshot leaves no reachable commits"
    );

    let index_err = db.was_linked("a", "b", "KNOWS", 0).unwrap_err();
    let date_err = db.resolve_date("2026-06-20").unwrap_err();
    assert!(
        matches!(index_err, GraphError::CommitOutOfRange { .. }),
        "a commit index must refuse once history is discarded, got {index_err:?}"
    );
    assert!(
        matches!(
            date_err,
            GraphError::CommitOutOfRange { .. } | GraphError::TimeBeforeFloor { .. }
        ),
        "a date must refuse exactly where an index does, got {date_err:?}"
    );

    // The store itself is intact — only its history went.
    assert!(db.node_info("a").is_some(), "the snapshot kept the data");
}
