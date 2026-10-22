//! `restore::cut_at_commit`: a closed store cut back to one commit opens to
//! exactly what `open_at` shows at that commit (HO-09, point-in-time restore).
//!
//! Differential throughout: every commit of every history below is cut in a
//! copy of the store, the copy is opened normally, and its nodes, edges and
//! commit count are compared with `open_at` on the uncut store. A commit
//! `open_at` refuses, the cut refuses too.

use mushroomdb::restore::{commit_at_instant, cut_at_commit, trim_commit_times, InstantAt};
use mushroomdb::{GraphDb, GraphError, Predicate, RealFs, RuleDef, SnapshotOptions, Value};
use std::path::Path;

/// A directory under the system temp dir, removed on drop.
struct TmpDir(std::path::PathBuf);

impl TmpDir {
    fn new() -> Self {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("mushroomdb-cut-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Self(d)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const ARCHIVING: SnapshotOptions = SnapshotOptions {
    keep_wal: false,
    archive_wal: true,
};
const TRUNCATING: SnapshotOptions = SnapshotOptions {
    keep_wal: false,
    archive_wal: false,
};
const KEEP_WAL: SnapshotOptions = SnapshotOptions {
    keep_wal: true,
    archive_wal: false,
};

fn same_tag_rule() -> RuleDef {
    RuleDef {
        name: "same_tag".into(),
        src_label: "T".into(),
        dst_label: "T".into(),
        predicate: Predicate::FieldEqual {
            field: "tag".into(),
        },
        edge_type: "SAME".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
        namespace: None,
    }
}

fn tag(v: &str) -> Vec<(String, Value)> {
    vec![("tag".into(), Value::Str(v.into()))]
}

/// A round of writes of every kind: inserts the rule links, a manual edge, a
/// property change that retracts a derived edge, and a delete.
fn round(db: &mut GraphDb<RealFs>, r: &str) {
    db.insert_node("T", &format!("{r}a"), tag("x")).unwrap();
    db.insert_node("T", &format!("{r}b"), tag("x")).unwrap();
    db.insert_node("T", &format!("{r}c"), tag("y")).unwrap();
    db.insert_edge("MANUAL", &format!("{r}a"), &format!("{r}c"))
        .unwrap();
    db.set_prop(&format!("{r}b"), "tag", Value::Str("y".into()))
        .unwrap();
    db.delete_node(&format!("{r}a")).unwrap();
}

type Dump = (
    Vec<mushroomdb::NodeInfo>,
    Vec<mushroomdb::ExportEdge>,
    usize,
    u64,
);

fn dump(db: &GraphDb<RealFs>) -> Dump {
    (
        db.all_nodes_for_export(),
        db.all_edges_for_export(),
        db.node_count(),
        db.edge_count(),
    )
}

/// Copy a closed store's files.
fn copy_store(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        if e.file_type().unwrap().is_file() && e.file_name() != "LOCK" {
            std::fs::copy(e.path(), to.join(e.file_name())).unwrap();
        }
    }
}

/// Cut a copy of the closed store in `dir` at every commit and compare it
/// with `open_at`. Returns how many commits were reachable and how many were
/// refused, so a scenario can assert it exercised both.
fn assert_every_commit_cuts_like_open_at(dir: &Path) -> (u64, u64) {
    let total = GraphDb::open(dir).unwrap().wal_total_commits().unwrap();
    assert!(total > 0);
    let (mut reached, mut refused) = (0, 0);
    for commit in 0..=total {
        let copy = TmpDir::new();
        copy_store(dir, copy.path());
        let cut = cut_at_commit(copy.path(), commit);
        match GraphDb::open_at(dir, commit) {
            Ok(want) => {
                cut.unwrap_or_else(|e| {
                    panic!("commit {commit}: open_at reaches it, the cut said {e}")
                });
                let got = GraphDb::open(copy.path()).unwrap();
                assert_eq!(dump(&got), dump(&want), "commit {commit}");
                assert_eq!(
                    got.wal_total_commits().unwrap(),
                    commit + 1,
                    "commit {commit}"
                );
                reached += 1;
            }
            Err(GraphError::CommitOutOfRange { .. }) => {
                assert!(
                    matches!(cut, Err(GraphError::CommitOutOfRange { .. })),
                    "commit {commit}: open_at refuses it, the cut said {cut:?}"
                );
                refused += 1;
            }
            Err(e) => panic!("commit {commit}: open_at: {e}"),
        }
    }
    (reached, refused)
}

#[test]
fn archives_and_a_live_wal_cut_like_open_at_at_every_commit() {
    let dir = TmpDir::new();
    {
        let mut db = GraphDb::open(dir.path()).unwrap();
        db.create_rule(same_tag_rule()).unwrap();
        round(&mut db, "r0");
        db.snapshot_with(ARCHIVING).unwrap();
        round(&mut db, "r1");
        db.snapshot_with(ARCHIVING).unwrap();
        round(&mut db, "r2");
    }
    assert!(dir.path().join("wal.genesis").exists());
    let (reached, refused) = assert_every_commit_cuts_like_open_at(dir.path());
    assert_eq!(refused, 1, "only the commit past the end");
    assert!(reached > 15, "{reached}");
}

#[test]
fn a_truncating_snapshot_puts_earlier_commits_out_of_reach_of_both() {
    let dir = TmpDir::new();
    {
        let mut db = GraphDb::open(dir.path()).unwrap();
        db.create_rule(same_tag_rule()).unwrap();
        round(&mut db, "r0");
        db.snapshot_with(TRUNCATING).unwrap();
        round(&mut db, "r1");
        db.snapshot_with(ARCHIVING).unwrap();
        round(&mut db, "r2");
        assert!(db.wal_horizon_floor() > 0);
    }
    let (reached, refused) = assert_every_commit_cuts_like_open_at(dir.path());
    assert!(reached > 0 && refused > 1, "{reached} / {refused}");
}

#[test]
fn a_keep_wal_snapshot_cuts_like_open_at_on_both_sides_of_its_mark() {
    let dir = TmpDir::new();
    {
        let mut db = GraphDb::open(dir.path()).unwrap();
        db.create_rule(same_tag_rule()).unwrap();
        round(&mut db, "r0");
        db.snapshot_with(KEEP_WAL).unwrap();
        round(&mut db, "r1");
    }
    let (reached, refused) = assert_every_commit_cuts_like_open_at(dir.path());
    assert_eq!(refused, 1, "only the commit past the end");
    assert!(reached > 10, "{reached}");
}

#[test]
fn a_keep_wal_snapshot_over_archives_is_refused_where_open_at_refuses() {
    let dir = TmpDir::new();
    {
        let mut db = GraphDb::open(dir.path()).unwrap();
        db.create_rule(same_tag_rule()).unwrap();
        round(&mut db, "r0");
        db.snapshot_with(ARCHIVING).unwrap();
        round(&mut db, "r1");
        db.snapshot_with(KEEP_WAL).unwrap();
        round(&mut db, "r2");
    }
    let (reached, refused) = assert_every_commit_cuts_like_open_at(dir.path());
    assert!(reached > 0 && refused > 1, "{reached} / {refused}");
}

/// A cut store is an ordinary store: it takes writes, numbers them after the
/// cut, and its commit times end at the cut, so the next commit is stamped.
#[test]
fn a_cut_store_keeps_writing_and_its_commit_times_end_at_the_cut() {
    let dir = TmpDir::new();
    {
        let mut db = GraphDb::open(dir.path()).unwrap();
        round(&mut db, "r0");
        db.snapshot_with(ARCHIVING).unwrap();
        round(&mut db, "r1");
    }
    let total = GraphDb::open(dir.path())
        .unwrap()
        .wal_total_commits()
        .unwrap();
    for commit in [2, total - 3] {
        let copy = TmpDir::new();
        copy_store(dir.path(), copy.path());
        cut_at_commit(copy.path(), commit).unwrap();
        let mut db = GraphDb::open(copy.path()).unwrap();
        assert!(db.commit_time_ms(commit).is_some());
        assert_eq!(db.commit_time_ms(commit + 1), None, "no time past the cut");
        db.insert_node("T", "after", vec![]).unwrap();
        assert!(
            db.commit_time_ms(commit + 1).is_some(),
            "the next commit is stamped"
        );
        drop(db);
        let db = GraphDb::open(copy.path()).unwrap();
        assert_eq!(db.wal_total_commits().unwrap(), commit + 2);
        assert!(db.has_node("after"));
    }
}

#[test]
fn an_interrupted_snapshot_is_not_cut() {
    let dir = TmpDir::new();
    {
        let mut db = GraphDb::open(dir.path()).unwrap();
        round(&mut db, "r0");
    }
    std::fs::write(dir.path().join("snapshot.intent"), [1u8; 32]).unwrap();
    let before = std::fs::read(dir.path().join("wal.bin")).unwrap();
    assert!(cut_at_commit(dir.path(), 1).is_err());
    assert_eq!(std::fs::read(dir.path().join("wal.bin")).unwrap(), before);
}

#[test]
fn an_instant_resolves_through_the_commit_times_sidecar() {
    let dir = TmpDir::new();
    {
        let mut db = GraphDb::open(dir.path()).unwrap();
        db.record_commits_at(Some(1_000)).unwrap();
        db.insert_node("T", "a", vec![]).unwrap();
        db.record_commits_at(Some(2_000)).unwrap();
        db.insert_node("T", "b", vec![]).unwrap();
        db.insert_node("T", "c", vec![]).unwrap();
    }
    let bytes = std::fs::read(dir.path().join("commit_times.bin")).unwrap();
    let at = |ms, commits| commit_at_instant(&bytes, 0, ms, commits);
    assert_eq!(
        at(1_500, 3).unwrap(),
        InstantAt {
            commit: 0,
            newest_recorded: 2
        }
    );
    assert_eq!(at(2_000, 3).unwrap().commit, 2);
    // Entries past the commits a store holds are not answers.
    assert_eq!(
        at(2_000, 2).unwrap(),
        InstantAt {
            commit: 1,
            newest_recorded: 1
        }
    );
    assert!(matches!(
        at(999, 3),
        Err(GraphError::TimeBeforeFloor { .. })
    ));
    assert!(matches!(
        commit_at_instant(&bytes, 1, 1_500, 3),
        Err(GraphError::CommitOutOfRange { .. })
    ));
}

/// A sidecar ahead of the WAL beside it — read after it, as a backup reads
/// it — is trimmed to the commits the WAL holds, so the next commit gets its
/// own time and dates resolve to this store's commits.
#[test]
fn trimming_the_commit_times_lets_the_next_commit_stamp_its_own() {
    let dir = TmpDir::new();
    {
        let mut db = GraphDb::open(dir.path()).unwrap();
        db.record_commits_at(Some(1_000)).unwrap();
        db.insert_node("T", "a", vec![]).unwrap();
        db.insert_node("T", "b", vec![]).unwrap();
    }
    let sidecar = std::fs::read(dir.path().join("commit_times.bin")).unwrap();
    {
        let mut db = GraphDb::open(dir.path()).unwrap();
        db.record_commits_at(Some(5_000)).unwrap();
        db.insert_node("T", "c", vec![]).unwrap();
    }
    // A copy whose WAL holds 2 commits beside a sidecar recording 3.
    let copy = TmpDir::new();
    {
        let mut db = GraphDb::open(copy.path()).unwrap();
        db.record_commits_at(Some(1_000)).unwrap();
        db.insert_node("T", "a", vec![]).unwrap();
        db.insert_node("T", "b", vec![]).unwrap();
    }
    let mut ahead = sidecar.clone();
    ahead.extend_from_slice(
        &std::fs::read(dir.path().join("commit_times.bin")).unwrap()[sidecar.len()..],
    );
    std::fs::write(copy.path().join("commit_times.bin"), &ahead).unwrap();
    trim_commit_times(copy.path(), 2).unwrap();
    let mut db = GraphDb::open(copy.path()).unwrap();
    assert_eq!(db.commit_time_ms(2), None);
    db.record_commits_at(Some(9_000)).unwrap();
    db.insert_node("T", "z", vec![]).unwrap();
    assert_eq!(
        db.commit_time_ms(2),
        Some(9_000),
        "the next commit has its own time"
    );
    assert_eq!(db.resolve_instant(5_000).unwrap(), 1);
}
