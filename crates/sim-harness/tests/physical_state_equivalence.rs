//! H-1 (0.10.x hardening): one logical graph, every physical state, one answer.
//!
//! A random operation sequence is applied to two stores. `plain` never
//! snapshots. `snap` snapshots wherever the sequence says so, so its reads go
//! through a snapshot base plus an overlay. A third store, `indexed`, is
//! `snap` with a range index declared on every filtered property before the
//! first write. A battery of Cypher reads then runs against each of:
//!
//!   plain live · plain reopened (WAL replay only) · plain `open_at` its last
//!   commit · snap live · snap reader · snap reopened · snap inside a
//!   transaction · indexed live · indexed reopened
//!
//! and every answer must equal plain live's. The states check one another, so
//! no reference evaluator is needed. Rows 139, 103, 107–113, 116 and 154 were
//! each a disagreement between two of these states.
//!
//! Any divergence is an engine bug. Do not weaken these assertions or skip a
//! query to get a run green; fix the engine, or show the generator is wrong.
//!
//! Cases: 24 per run by default; set `H1_CASES` to raise it (the nightly run).

use mushroomdb::{GraphDb, Predicate, RealFs, ResultSet, RuleDef, SharedDb, Value};
use proptest::prelude::*;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const KEY_MOD: u8 = 12;
const TAGS: [&str; 3] = ["a", "b", "c"];
const USER_ETYPES: [&str; 2] = ["e0", "e1"];

#[derive(Debug, Clone)]
enum Op {
    InsertNode(u8),
    InsertEdge(u8, u8, u8),
    SetInt(u8, i8),
    SetBool(u8, bool),
    SetTags(u8, u8, u8),
    RemoveInt(u8),
    DeleteEdge(u8, u8, u8),
    DeleteNode(u8),
    Snapshot,
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => any::<u8>().prop_map(Op::InsertNode),
        3 => (any::<u8>(), any::<u8>(), any::<u8>()).prop_map(|(t, s, d)| Op::InsertEdge(t, s, d)),
        3 => (any::<u8>(), any::<i8>()).prop_map(|(k, v)| Op::SetInt(k, v)),
        1 => (any::<u8>(), any::<bool>()).prop_map(|(k, b)| Op::SetBool(k, b)),
        2 => (any::<u8>(), any::<u8>(), any::<u8>()).prop_map(|(k, a, b)| Op::SetTags(k, a, b)),
        1 => any::<u8>().prop_map(Op::RemoveInt),
        1 => (any::<u8>(), any::<u8>(), any::<u8>()).prop_map(|(t, s, d)| Op::DeleteEdge(t, s, d)),
        1 => any::<u8>().prop_map(Op::DeleteNode),
        2 => Just(Op::Snapshot),
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

/// Apply one op. Results are ignored on purpose: an op that fails (an edge to a
/// missing node, a duplicate key) fails the same way on every store, and the
/// reads afterwards are what is compared.
fn apply(db: &mut GraphDb<RealFs>, op: &Op, take_snapshots: bool) {
    match op {
        Op::InsertNode(n) => {
            let props = vec![
                ("p".into(), Value::Int(i64::from(*n % 7))),
                ("q".into(), Value::Bool(n % 3 == 0)),
                ("tags".into(), tags_of(*n, n.wrapping_add(1))),
            ];
            let _ = db.insert_node(&format!("L{}", n % 2), &key_of(*n), props);
        }
        Op::InsertEdge(t, s, d) => {
            let _ = db.insert_edge(USER_ETYPES[(*t as usize) % 2], &key_of(*s), &key_of(*d));
        }
        Op::SetInt(k, v) => {
            let _ = db.set_prop(&key_of(*k), "p", Value::Int(i64::from(*v % 9)));
        }
        Op::SetBool(k, b) => {
            let _ = db.set_prop(&key_of(*k), "q", Value::Bool(*b));
        }
        Op::SetTags(k, a, b) => {
            let _ = db.set_prop(&key_of(*k), "tags", tags_of(*a, *b));
        }
        Op::RemoveInt(k) => {
            let _ = db.remove_prop(&key_of(*k), "p");
        }
        Op::DeleteEdge(t, s, d) => {
            let _ = db.delete_edge(USER_ETYPES[(*t as usize) % 2], &key_of(*s), &key_of(*d));
        }
        Op::DeleteNode(k) => {
            let _ = db.delete_node(&key_of(*k));
        }
        Op::Snapshot => {
            if take_snapshots {
                db.snapshot().expect("snapshot");
            }
        }
    }
}

/// The read battery. A query with ORDER BY orders on a total order of its rows,
/// so its rows must match in order. A query without ORDER BY may return its
/// rows in any order, so they are compared sorted; its LIMIT is larger than any
/// generated graph, so it still has to return every matching row. With SKIP and
/// no ORDER BY, which rows are skipped is not defined, so only the row count is
/// compared. The unordered LIMIT shapes run the fused scan-and-filter, where
/// row 139 lived. Shapes are chosen from the rows above:
/// filtered scans with and without LIMIT (139), every comparison operator,
/// booleans (160), aggregates over properties and variables (157, 158),
/// DISTINCT (131), hops, and rule-derived edges with their weights (103–116, 154).
fn queries() -> Vec<String> {
    let mut q = Vec::new();
    for op in ["=", "<>", "<", "<=", ">", ">="] {
        q.push(format!(
            "MATCH (n:L0) WHERE n.p {op} 2 RETURN key(n) AS k ORDER BY k"
        ));
        q.push(format!(
            "MATCH (n:L0) WHERE n.p {op} 2 RETURN key(n) AS k ORDER BY k LIMIT 3"
        ));
        q.push(format!(
            "MATCH (n:L0) WHERE n.p {op} 2 RETURN key(n) AS k ORDER BY k SKIP 1 LIMIT 2"
        ));
        q.push(format!(
            "MATCH (n:L0) WHERE n.p {op} 2 RETURN key(n) AS k LIMIT 100"
        ));
        q.push(format!(
            "MATCH (n:L0) WHERE n.p {op} 2 RETURN n.p AS p SKIP 1 LIMIT 100"
        ));
        q.push(format!(
            "MATCH (m:L1), (n:L0) WHERE n.p {op} 1 RETURN key(m) AS a, key(n) AS b LIMIT 1000"
        ));
        q.push(format!(
            "MATCH (m:L1), (n:L0) WHERE n.p {op} 1 RETURN key(m) AS a, key(n) AS b ORDER BY a, b LIMIT 5"
        ));
    }
    q.extend(
        [
            "MATCH (n) RETURN count(n) AS c",
            "MATCH (n:L0) RETURN count(n.p) AS c, sum(n.p) AS s, min(n.p) AS lo, max(n.p) AS hi",
            "MATCH (n:L1) WITH n.p AS x RETURN sum(x) AS s, max(x) AS hi, min(x) AS lo",
            "MATCH (n) RETURN labels(n)[0] AS l, count(*) AS c, sum(n.p) AS s ORDER BY l",
            "MATCH (n) WHERE n.q > false RETURN key(n) AS k ORDER BY k",
            "MATCH (n) WHERE n.q = true AND n.p IS NULL RETURN key(n) AS k ORDER BY k",
            "MATCH (n) WHERE NOT (n.p > 0) RETURN key(n) AS k ORDER BY k",
            "MATCH (n) RETURN min(n.q) AS lo, max(n.q) AS hi",
            "MATCH (n) RETURN DISTINCT n.p AS p ORDER BY p",
            "MATCH (n) RETURN DISTINCT n.tags AS t ORDER BY t",
            "MATCH (n) WHERE n.p IN [0, 1, 2] RETURN key(n) AS k ORDER BY k",
            "MATCH (a)-[:e0]->(b) RETURN key(a) AS a, key(b) AS b ORDER BY a, b",
            "MATCH (a)-[:e1]->(b) WHERE b.p >= 0 RETURN key(a) AS a, key(b) AS b ORDER BY a, b LIMIT 4",
            "MATCH (a:L0)-[r]->(b) RETURN type(r) AS t, count(*) AS c ORDER BY t",
            "MATCH (a)-[:e0]->()-[:e1]->(c) RETURN key(a) AS a, key(c) AS c ORDER BY a, c",
            "MATCH (a)-[r:OV]->(b) RETURN key(a) AS a, key(b) AS b, r.score AS s ORDER BY a, b",
            "MATCH (a)-[:OV]->(b) RETURN key(a) AS a, count(b) AS c ORDER BY a",
            "MATCH (n:L0) OPTIONAL MATCH (n)-[:e0]->(m) RETURN key(n) AS a, count(m) AS c ORDER BY a",
            "UNWIND [1, 2, 3] AS x MATCH (n:L0) WHERE n.p = x RETURN x, count(n) AS c ORDER BY x",
        ]
        .map(String::from),
    );
    q
}

/// One query's answer as text: the rows in order, or the error. Comparing the
/// error too means a state that fails a query the reference answers is a
/// divergence, and so is the reverse.
type Answers = Vec<String>;

fn render(query: &str, r: mushroomdb::Result<ResultSet>) -> String {
    match r {
        Ok(rs) => {
            let cols = rs.columns().to_vec();
            let ordered = query.contains("ORDER BY");
            if !ordered && query.contains("SKIP") {
                return format!("{cols:?}\n{} rows", rs.len());
            }
            let mut rows: Vec<String> = (0..rs.len())
                .map(|i| {
                    let row: Vec<String> =
                        cols.iter().map(|c| format!("{:?}", rs.get(i, c))).collect();
                    format!("{row:?}")
                })
                .collect();
            if !ordered {
                rows.sort();
            }
            format!("{cols:?}\n{}", rows.join("\n"))
        }
        Err(e) => format!("error: {e}"),
    }
}

fn answers(mut run: impl FnMut(&str) -> mushroomdb::Result<ResultSet>) -> Answers {
    queries().iter().map(|q| render(q, run(q))).collect()
}

fn no_params() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

static SEQ: AtomicU64 = AtomicU64::new(0);

/// A fresh directory per store per case. Names carry pid, time and a sequence
/// number, so parallel tests never share one (the FLAKE-1 lesson).
fn fresh_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "mdb-h1-{tag}-{}-{nanos}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Build a store and hand back the still-open handle, so its live state is
/// read before any reopen.
fn build(
    dir: &Path,
    ops: &[Op],
    rule_at: usize,
    snapshots: bool,
    indexed: bool,
) -> GraphDb<RealFs> {
    let mut db = GraphDb::open(dir).unwrap();
    if indexed {
        db.enable_range_index("l0_p", "L0", "p").unwrap();
        db.enable_range_index("l1_p", "L1", "p").unwrap();
    }
    for (i, op) in ops.iter().enumerate() {
        if i == rule_at {
            db.create_rule(overlap_rule()).unwrap();
        }
        apply(&mut db, op, snapshots);
    }
    if rule_at >= ops.len() {
        db.create_rule(overlap_rule()).unwrap();
    }
    db
}

fn first_divergence(reference: &Answers, other: &Answers) -> Option<(String, String, String)> {
    let qs = queries();
    reference
        .iter()
        .zip(other)
        .zip(&qs)
        .find(|((a, b), _)| a != b)
        .map(|((a, b), q)| (q.clone(), a.clone(), b.clone()))
}

fn cases() -> u32 {
    std::env::var("H1_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(24)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(), ..ProptestConfig::default() })]
    #[test]
    fn every_physical_state_answers_alike(
        ops in proptest::collection::vec(op_strategy(), 1..48),
        rule_slot in any::<u8>(),
    ) {
        let rule_at = (rule_slot as usize) % (ops.len() + 1);
        let plain = fresh_dir("plain");
        let snap = fresh_dir("snap");
        let indexed = fresh_dir("indexed");
        let mut states: Vec<(&str, Answers)> = Vec::new();
        let reference = {
            let db = build(&plain, &ops, rule_at, false, false);
            answers(|q| db.query(q, &no_params()))
        };
        {
            let db = GraphDb::open(&plain).unwrap();
            states.push(("plain reopened", answers(|q| db.query(q, &no_params()))));
            let last = db.commit_seq().checked_sub(1);
            drop(db);
            if let Some(c) = last {
                let at = GraphDb::open_at(&plain, c).unwrap();
                states.push(("plain open_at(last commit)", answers(|q| at.query(q, &no_params()))));
            }
        }
        {
            let db = build(&snap, &ops, rule_at, true, false);
            states.push(("snap live", answers(|q| db.query(q, &no_params()))));
            let reader = db.reader();
            states.push(("snap live reader", answers(|q| reader.query(q, &no_params()))));
        }
        {
            let db = build(&indexed, &ops, rule_at, true, true);
            states.push(("indexed live", answers(|q| db.query(q, &no_params()))));
        }
        {
            let db = GraphDb::open(&snap).unwrap();
            states.push(("snap reopened", answers(|q| db.query(q, &no_params()))));
            let reader = db.reader();
            states.push(("snap reader", answers(|q| reader.query(q, &no_params()))));
        }
        {
            let shared = SharedDb::open(&snap).unwrap();
            let mut tx = shared.begin_transaction();
            states.push(("snap in a transaction", answers(|q| tx.execute(q, &no_params()))));
            tx.rollback();
        }
        {
            let db = GraphDb::open(&indexed).unwrap();
            states.push(("indexed reopened", answers(|q| db.query(q, &no_params()))));
        }

        for (name, got) in &states {
            if let Some((q, want, have)) = first_divergence(&reference, got) {
                for d in [&plain, &snap, &indexed] {
                    let _ = std::fs::remove_dir_all(d);
                }
                prop_assert!(false, "{name} disagrees with plain live on\n  {q}\nplain live:\n{want}\n{name}:\n{have}");
            }
        }
        for d in [&plain, &snap, &indexed] {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

/// The battery must actually run. A query that errors everywhere compares
/// equal everywhere and checks nothing, so on a small fixed graph every query
/// has to succeed, and the battery as a whole has to return rows.
#[test]
fn every_query_in_the_battery_runs() {
    let ops: Vec<Op> = (0u8..12)
        .map(Op::InsertNode)
        .chain((0u8..12).map(|i| Op::InsertEdge(i, i, i.wrapping_add(3))))
        .chain([
            Op::Snapshot,
            Op::SetInt(1, 5),
            Op::SetBool(2, true),
            Op::RemoveInt(4),
        ])
        .collect();
    let dir = fresh_dir("battery");
    let db = build(&dir, &ops, 0, true, false);
    let mut rows = 0;
    let mut failures = Vec::new();
    for q in queries() {
        match db.query(&q, &no_params()) {
            Ok(rs) => rows += rs.len(),
            Err(e) => failures.push(format!("{q}\n    {e}")),
        }
    }
    assert!(
        failures.is_empty(),
        "battery queries fail on a plain graph:\n{}",
        failures.join("\n")
    );
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        rows > 50,
        "the battery returned only {rows} rows; it checks too little"
    );
}
