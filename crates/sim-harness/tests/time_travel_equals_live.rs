//! H-4 (0.10.x hardening): a past commit answers as the store did then.
//!
//! A random operation sequence is applied to one store. After every op that
//! commits, a query battery is answered live and the answers are recorded with
//! the commit number. Afterwards, `GraphDb::open_at` at each recorded commit
//! must answer the battery exactly as the live store did at that moment.
//!
//! Snapshots in the sequence are taken in one of three modes per case: never,
//! `keep_wal` (the WAL survives the snapshot) or `archive_wal` (the WAL is
//! archived and `open_at` reads across archives). In each of these modes every
//! commit stays reachable, so every recorded commit is checked. A truncating
//! snapshot (the default) drops the commits before it from `open_at`'s reach
//! by design, so in that mode `open_at` may refuse a recorded commit, but when
//! it answers, it must answer as that commit did: never another commit's graph.
//!
//! Any divergence is an engine bug. Do not weaken these assertions.
//!
//! Cases: 24 per run by default; set `H4_CASES` to raise it (the nightly run).

use mushroomdb::{GraphDb, Predicate, RealFs, ResultSet, RuleDef, SnapshotOptions, Value};
use proptest::prelude::*;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

const KEY_MOD: u8 = 10;
const TAGS: [&str; 3] = ["a", "b", "c"];

#[derive(Debug, Clone)]
enum Op {
    InsertNode(u8),
    InsertEdge(u8, u8, u8),
    SetInt(u8, i8),
    SetTags(u8, u8, u8),
    RemoveInt(u8),
    DeleteEdge(u8, u8, u8),
    DeleteNode(u8),
    CreateRule,
    Snapshot,
}

#[derive(Debug, Clone, Copy)]
enum SnapMode {
    Never,
    KeepWal,
    ArchiveWal,
    Truncate,
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => any::<u8>().prop_map(Op::InsertNode),
        3 => (any::<u8>(), any::<u8>(), any::<u8>()).prop_map(|(t, s, d)| Op::InsertEdge(t, s, d)),
        3 => (any::<u8>(), any::<i8>()).prop_map(|(k, v)| Op::SetInt(k, v)),
        2 => (any::<u8>(), any::<u8>(), any::<u8>()).prop_map(|(k, a, b)| Op::SetTags(k, a, b)),
        1 => any::<u8>().prop_map(Op::RemoveInt),
        1 => (any::<u8>(), any::<u8>(), any::<u8>()).prop_map(|(t, s, d)| Op::DeleteEdge(t, s, d)),
        1 => any::<u8>().prop_map(Op::DeleteNode),
        1 => Just(Op::CreateRule),
        2 => Just(Op::Snapshot),
    ]
}

fn mode_strategy() -> impl Strategy<Value = SnapMode> {
    prop_oneof![
        Just(SnapMode::Never),
        Just(SnapMode::KeepWal),
        Just(SnapMode::ArchiveWal),
        Just(SnapMode::Truncate)
    ]
}

fn key_of(n: u8) -> String {
    format!("k{}", n % KEY_MOD)
}

fn tags_of(a: u8, b: u8) -> Value {
    let mut items = vec![
        Value::Str(TAGS[(a as usize) % TAGS.len()].into()),
        Value::Str(TAGS[(b as usize) % TAGS.len()].into()),
    ];
    items.dedup();
    Value::List(items)
}

fn overlap_rule() -> RuleDef {
    RuleDef {
        name: "ov".into(),
        src_label: "L0".into(),
        dst_label: "L1".into(),
        predicate: Predicate::Overlap {
            field: "tags".into(),
            min: 0.3,
        },
        edge_type: "OV".into(),
        weight_prop: Some("score".into()),
        max_edges: Some(2),
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
        namespace: None,
    }
}

/// Apply one op; failures are part of the sequence and are ignored.
fn apply(db: &mut GraphDb<RealFs>, op: &Op, mode: SnapMode) {
    match op {
        Op::InsertNode(n) => {
            let props = vec![
                ("p".into(), Value::Int(i64::from(*n % 7))),
                ("tags".into(), tags_of(*n, n.wrapping_add(1))),
            ];
            let _ = db.insert_node(&format!("L{}", n % 2), &key_of(*n), props);
        }
        Op::InsertEdge(t, s, d) => {
            let _ = db.insert_edge(["e0", "e1"][(*t as usize) % 2], &key_of(*s), &key_of(*d));
        }
        Op::SetInt(k, v) => {
            let _ = db.set_prop(&key_of(*k), "p", Value::Int(i64::from(*v % 9)));
        }
        Op::SetTags(k, a, b) => {
            let _ = db.set_prop(&key_of(*k), "tags", tags_of(*a, *b));
        }
        Op::RemoveInt(k) => {
            let _ = db.remove_prop(&key_of(*k), "p");
        }
        Op::DeleteEdge(t, s, d) => {
            let _ = db.delete_edge(["e0", "e1"][(*t as usize) % 2], &key_of(*s), &key_of(*d));
        }
        Op::DeleteNode(k) => {
            let _ = db.delete_node(&key_of(*k));
        }
        Op::CreateRule => {
            let _ = db.create_rule(overlap_rule());
        }
        Op::Snapshot => {
            let opts = match mode {
                SnapMode::Never => return,
                SnapMode::KeepWal => SnapshotOptions {
                    keep_wal: true,
                    archive_wal: false,
                },
                SnapMode::ArchiveWal => SnapshotOptions {
                    keep_wal: false,
                    archive_wal: true,
                },
                SnapMode::Truncate => SnapshotOptions {
                    keep_wal: false,
                    archive_wal: false,
                },
            };
            db.snapshot_with(opts).expect("snapshot");
        }
    }
}

/// A small battery: nodes and properties, user edges, derived edges with their
/// weights, and two aggregates. Every LIMIT sits under a total ORDER BY.
fn queries() -> [&'static str; 7] {
    [
        "MATCH (n) RETURN key(n) AS k, labels(n)[0] AS l, n.p AS p, n.tags AS t ORDER BY k",
        "MATCH (n:L0) WHERE n.p > 2 RETURN key(n) AS k ORDER BY k LIMIT 3",
        "MATCH (a)-[r]->(b) WHERE type(r) <> 'OV' RETURN type(r) AS t, key(a) AS a, key(b) AS b ORDER BY t, a, b",
        "MATCH (a)-[r:OV]->(b) RETURN key(a) AS a, key(b) AS b, r.score AS s ORDER BY a, b",
        "MATCH (n) RETURN count(n) AS c, count(n.p) AS cp, sum(n.p) AS s",
        "MATCH (a)-[:e0]->()-[:e1]->(c) RETURN key(a) AS a, key(c) AS c ORDER BY a, c",
        "MATCH (n) WHERE n.p IS NULL RETURN key(n) AS k ORDER BY k",
    ]
}

fn render(r: mushroomdb::Result<ResultSet>) -> String {
    match r {
        Ok(rs) => {
            let cols = rs.columns().to_vec();
            let rows: Vec<String> = (0..rs.len())
                .map(|i| {
                    let row: Vec<String> =
                        cols.iter().map(|c| format!("{:?}", rs.get(i, c))).collect();
                    format!("{row:?}")
                })
                .collect();
            format!("{cols:?}\n{}", rows.join("\n"))
        }
        Err(e) => format!("error: {e}"),
    }
}

fn answers(db: &GraphDb<RealFs>) -> Vec<String> {
    queries()
        .iter()
        .map(|q| render(db.query(q, &BTreeMap::new())))
        .collect()
}

static SEQ: AtomicU64 = AtomicU64::new(0);

fn fresh_dir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "mdb-h4-{}-{nanos}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn cases() -> u32 {
    std::env::var("H4_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(24)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(), ..ProptestConfig::default() })]
    #[test]
    fn a_past_commit_answers_as_the_store_did_then(
        ops in proptest::collection::vec(op_strategy(), 1..40),
        mode in mode_strategy(),
    ) {
        let dir = fresh_dir();
        // open_at index (global frames, archives included, minus one) -> live answers right after it
        let mut history: Vec<(u64, Vec<String>)> = Vec::new();
        {
            let mut db = GraphDb::open(&dir).unwrap();
            let mut seen = db.wal_total_commits().unwrap();
            for op in &ops {
                apply(&mut db, op, mode);
                let now = db.wal_total_commits().unwrap();
                if now != seen {
                    history.push((now - 1, answers(&db)));
                    seen = now;
                }
            }
        }
        let mut failure = None;
        for (commit, want) in &history {
            let have = match GraphDb::open_at(&dir, *commit) {
                Ok(at) => answers(&at),
                // A truncating snapshot may put a commit out of reach; refusing
                // it is correct. Answering it wrongly is not.
                Err(mushroomdb::GraphError::CommitOutOfRange { .. })
                    if matches!(mode, SnapMode::Truncate) => continue,
                Err(e) => vec![format!("open_at({commit}) failed: {e}")],
            };
            if &have != want {
                let (q, w, h) = queries()
                    .iter()
                    .zip(want.iter().zip(have.iter().chain(std::iter::repeat(&String::new()))))
                    .find(|(_, (w, h))| w != h)
                    .map(|(q, (w, h))| (q.to_string(), w.clone(), h.clone()))
                    .unwrap_or_default();
                failure = Some(format!(
                    "open_at({commit}) [{mode:?}] disagrees with the live store at that commit on\n  {q}\nlive then:\n{w}\nopen_at:\n{h}"
                ));
                break;
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        if let Some(f) = failure {
            prop_assert!(false, "{}", f);
        }
    }
}
