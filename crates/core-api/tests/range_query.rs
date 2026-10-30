//! The Cypher planner answers range predicates, `STARTS WITH` and
//! `ORDER BY … LIMIT` from a range index (0.10 TY-10).
//!
//! The one rule: a plan the index assists returns exactly what the label scan
//! returns — the same rows, in the same order. The randomized differential at
//! the bottom holds every rewrite to it; the tests above it pin the shapes the
//! planner rewrites and the visibility rules (transactions, roles, masks,
//! readers, `query_at`) the index plan must keep.
//!
//! The plan is read through `core_query::cypher::plan`'s `Debug` form, and
//! whether the executor actually answered from the index through
//! [`core_query::cypher::exec::range_index_scans`], a per-thread count.

use core_query::cypher::exec::{range_index_scans, range_top_k_stops, set_pull_probe_slots};
use core_query::cypher::{lex, parse, plan};
use mushroomdb::{NodeMask, RealFs, ResultSet, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;

type GraphDb = mushroomdb::GraphDb<RealFs>;

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "graphdb-range-query-{}-{}-{}",
        name,
        std::process::id(),
        nanos,
    ))
}

/// The logical plan of `cypher`, as its `Debug` text.
fn plan_of(cypher: &str) -> String {
    let ops = plan(&parse(&lex(cypher).expect("lex")).expect("parse")).expect("plan");
    format!("{ops:?}")
}

fn open_temp_with_range_index(label: &str, field: &str) -> GraphDb {
    let mut db = GraphDb::open(&tmp(label)).unwrap();
    db.enable_range_index(&format!("by_{field}"), label, field)
        .unwrap();
    db
}

fn insert_score_nodes(db: &mut GraphDb, n: i64) {
    let mut b = db.batch();
    for i in 0..n {
        b.insert_node(
            "Score",
            &format!("s{i}"),
            vec![("val".into(), Value::Int(i))],
        );
    }
    b.commit().unwrap();
}

fn insert_named_nodes(db: &mut GraphDb, names: &[&str]) {
    let mut b = db.batch();
    for name in names {
        b.insert_node(
            "Person",
            name,
            vec![("name".into(), Value::Str((*name).into()))],
        );
    }
    b.commit().unwrap();
}

fn insert_item_nodes(db: &mut GraphDb, n: usize, score: impl Fn(usize) -> i64) {
    const CHUNK: usize = 50_000;
    for start in (0..n).step_by(CHUNK) {
        let mut b = db.batch();
        for i in start..(start + CHUNK).min(n) {
            b.insert_node(
                "Item",
                &format!("i{i}"),
                vec![("score".into(), Value::Int(score(i)))],
            );
        }
        b.commit().unwrap();
    }
}

fn q(db: &GraphDb, cypher: &str) -> ResultSet {
    db.query(cypher, &BTreeMap::new()).unwrap()
}

/// `cypher` run once, with the number of range-index answers it took.
fn q_counted(db: &GraphDb, cypher: &str) -> (ResultSet, u64) {
    let before = range_index_scans();
    let rs = q(db, cypher);
    (rs, range_index_scans() - before)
}

fn keys(rs: &ResultSet, col: &str) -> Vec<String> {
    (0..rs.len())
        .map(|i| match rs.get(i, col) {
            Some(Value::Str(s)) => s.clone(),
            other => panic!("row {i} column {col}: {other:?}"),
        })
        .collect()
}

#[test]
fn test_planner_uses_range_index_for_gt() {
    let mut db = open_temp_with_range_index("Score", "val");
    // 1000 nodes, val 0..1000.
    insert_score_nodes(&mut db, 1000);
    let cypher = "MATCH (n:Score) WHERE n.val > 500 RETURN n";
    let plan = plan_of(cypher);
    assert!(plan.contains("IndexRangeScan"), "plan: {plan}");
    let (rs, used) = q_counted(&db, cypher);
    assert_eq!(rs.len(), 499);
    assert_eq!(used, 1, "the index answered the range");
}

#[test]
fn test_planner_starts_with_uses_range_index() {
    let mut db = open_temp_with_range_index("Person", "name");
    insert_named_nodes(&mut db, &["alice", "alicia", "bob"]);
    let cypher = "MATCH (n:Person) WHERE n.name STARTS WITH 'al' RETURN n";
    let plan = plan_of(cypher);
    assert!(plan.contains("IndexRangeScan"), "plan: {plan}");
    let (rs, used) = q_counted(&db, cypher);
    assert_eq!(rs.len(), 2);
    assert_eq!(used, 1);
}

#[test]
#[ignore = "release only: inserts 1M nodes (TY-14 measures it); run in release with --ignored"]
fn test_range_query_1000_results_latency() {
    // Performance gate: p50 ≤ 50ms at 1M nodes.
    // (Measured in TY-14; this test just verifies correctness.)
    let mut db = open_temp_with_range_index("Item", "score");
    insert_item_nodes(&mut db, 1_000_000, |i| (i % 1000) as i64); // scores 0..999, repeated
    let (rs, used) = q_counted(
        &db,
        "MATCH (n:Item) WHERE n.score >= 100 AND n.score <= 200 RETURN n",
    );
    assert_eq!(rs.len(), 101_000); // 101 distinct score values × 1000 nodes each
    assert_eq!(used, 1);
}

#[test]
fn test_range_predicate_mixed_column() {
    // Review Focus #3: mixed column type — no crash, no wrong results.
    let mut db = open_temp_with_range_index("Item", "val");
    db.batch()
        .insert_node("Item", "i1", vec![("val".into(), Value::Int(10))])
        .insert_node(
            "Item",
            "i2",
            vec![("val".into(), Value::Str("hello".into()))],
        )
        .commit()
        .unwrap();
    let (rs, used) = q_counted(&db, "MATCH (n:Item) WHERE n.val > 5 RETURN n");
    assert_eq!(keys(&rs, "n"), ["i1"]); // only i1 matches; i2 has Str, not in Int range
    assert_eq!(used, 1);
}

/// Median of `runs` timings of `cypher`, in microseconds.
fn median_us(db: &GraphDb, cypher: &str, runs: usize) -> (u128, usize) {
    let mut times = Vec::with_capacity(runs);
    let mut rows = 0;
    for _ in 0..runs {
        let t = std::time::Instant::now();
        rows = q(db, cypher).len();
        times.push(t.elapsed().as_micros());
    }
    times.sort_unstable();
    (times[runs / 2], rows)
}

#[test]
#[ignore = "measurement by hand: prints range and ORDER BY … LIMIT timings at 100k nodes with and without the index (release, --ignored), nothing asserted"]
fn bench_range_and_top_k_at_100k() {
    let mut indexed = open_temp_with_range_index("Item", "score");
    let mut plain = GraphDb::open(&tmp("bench-plain")).unwrap();
    for db in [&mut indexed, &mut plain] {
        insert_item_nodes(db, 100_000, |i| ((i * 7919) % 100_000) as i64);
    }
    for cypher in [
        "MATCH (n:Item) WHERE n.score >= 1000 AND n.score < 2000 RETURN n",
        "MATCH (n:Item) WHERE n.score > 99000 RETURN n.score AS s",
        "MATCH (n:Item) RETURN n ORDER BY n.score LIMIT 10",
        "MATCH (n:Item) RETURN n ORDER BY n.score DESC LIMIT 10",
        "MATCH (n:Item) WHERE n.score > 50000 RETURN n ORDER BY n.score LIMIT 10",
        "MATCH (n:Item) WHERE n.score > 50000 RETURN n ORDER BY n.score DESC LIMIT 10",
        "MATCH (n:Item) WHERE n.score > 100 RETURN n LIMIT 10",
    ] {
        let (with, rows_with) = median_us(&indexed, cypher, 7);
        let (without, rows_without) = median_us(&plain, cypher, 7);
        assert_eq!(rows_with, rows_without);
        println!("{cypher}\n  rows {rows_with}: index {with} us, scan {without} us");
    }
}

// ── Numbers, temporals, durations ────────────────────────────────────────────

fn item_db(name: &str, vals: &[(&str, Value)]) -> GraphDb {
    let mut db = GraphDb::open(&tmp(name)).unwrap();
    let mut b = db.batch();
    for (k, v) in vals {
        b.insert_node("Item", k, vec![("val".into(), v.clone())]);
    }
    b.commit().unwrap();
    db.enable_range_index("by_val", "Item", "val").unwrap();
    db
}

fn qp(db: &GraphDb, cypher: &str, params: &[(&str, Value)]) -> (ResultSet, u64) {
    let before = range_index_scans();
    let rs = db.query_with_params(cypher, params).unwrap();
    (rs, range_index_scans() - before)
}

/// Int and Float compare as numbers in a range, as they do in a scan
/// (ledger row 125's equality rule, carried to ranges).
#[test]
fn int_and_float_bounds_find_each_other() {
    let db = item_db(
        "numeric",
        &[
            ("i10", Value::Int(10)),
            ("f10", Value::Float(10.0)),
            ("f5", Value::Float(5.5)),
            ("s", Value::Str("10".into())),
        ],
    );
    let (rs, used) = qp(&db, "MATCH (n:Item) WHERE n.val > 5.5 RETURN n", &[]);
    assert_eq!(keys(&rs, "n"), ["i10", "f10"]);
    assert_eq!(used, 1);
    let (rs, used) = qp(&db, "MATCH (n:Item) WHERE n.val >= 10 RETURN n", &[]);
    assert_eq!(keys(&rs, "n"), ["i10", "f10"]);
    assert_eq!(used, 1);
    // A string bound orders only with strings: no number is in its range.
    let (rs, _) = qp(
        &db,
        "MATCH (n:Item) WHERE n.val >= $b RETURN n",
        &[("b", Value::Str("1".into()))],
    );
    assert_eq!(keys(&rs, "n"), ["s"]);
}

#[test]
fn temporal_values_range_in_their_own_order() {
    let dt = |micros_utc: i64, offset_seconds: i32| Value::DateTime {
        micros_utc,
        offset_seconds,
    };
    let db = item_db(
        "temporal",
        &[
            ("d1", Value::Date(10)),
            ("d2", Value::Date(20)),
            ("l1", Value::LocalDateTime(15)),
            ("t1", dt(1_000, 3600)),
            ("t2", dt(1_000, 0)),
            ("t3", dt(5_000, 0)),
            ("n", Value::Int(15)),
        ],
    );
    let (rs, used) = qp(
        &db,
        "MATCH (n:Item) WHERE n.val >= $lo AND n.val < $hi RETURN n",
        &[("lo", Value::Date(10)), ("hi", Value::Date(20))],
    );
    assert_eq!(keys(&rs, "n"), ["d1"]);
    assert_eq!(used, 1);
    let (rs, used) = qp(
        &db,
        "MATCH (n:Item) WHERE n.val > $b RETURN n ORDER BY n.val LIMIT 2",
        &[("b", dt(0, 0))],
    );
    // One instant, two offsets: the scan orders them by offset.
    assert_eq!(keys(&rs, "n"), ["t2", "t1"]);
    assert_eq!(used, 1);
}

/// Durations are not ordered: a range over one finds nothing, as the scan
/// finds nothing — and it is the scan that answers, not the index.
#[test]
fn a_duration_bound_is_answered_by_the_scan() {
    let dur = |days: i32| Value::Duration {
        months: 0,
        days,
        nanos: 0,
    };
    let db = item_db(
        "duration",
        &[("a", dur(1)), ("b", dur(3)), ("c", Value::Int(2))],
    );
    let cypher = "MATCH (n:Item) WHERE n.val > $b RETURN n";
    assert!(plan_of(cypher).contains("IndexRangeScan"));
    let (rs, used) = qp(&db, cypher, &[("b", dur(2))]);
    assert_eq!(rs.len(), 0);
    assert_eq!(used, 0, "a Duration bound never reads the index");
    // A null bound, likewise.
    let (rs, used) = qp(&db, cypher, &[("b", Value::Null)]);
    assert_eq!((rs.len(), used), (0, 0));
}

// ── ORDER BY over the index ──────────────────────────────────────────────────

#[test]
fn order_by_limit_is_a_top_k_from_the_index_with_nulls_where_a_sort_puts_them() {
    let mut db = item_db(
        "order",
        &[
            ("a", Value::Int(3)),
            ("b", Value::Int(1)),
            ("c", Value::Float(2.0)),
            ("d", Value::Int(2)),
            ("e", Value::Str("x".into())),
            ("f", Value::Bool(true)),
            ("g", Value::List(vec![Value::Int(1)])),
        ],
    );
    // Two nodes without the property: null, last ascending and first descending.
    db.insert_node("Item", "m1", vec![]).unwrap();
    db.insert_node("Item", "m2", vec![("other".into(), Value::Int(1))])
        .unwrap();
    let asc = "MATCH (n:Item) RETURN n ORDER BY n.val LIMIT 4";
    assert!(plan_of(asc).contains("order: Some"), "{}", plan_of(asc));
    let (rs, used) = qp(&db, asc, &[]);
    assert_eq!(keys(&rs, "n"), ["g", "e", "f", "b"]);
    assert_eq!(used, 1);
    let (rs, _) = qp(&db, "MATCH (n:Item) RETURN n ORDER BY n.val LIMIT 20", &[]);
    assert_eq!(
        keys(&rs, "n"),
        ["g", "e", "f", "b", "c", "d", "a", "m1", "m2"]
    );
    let (rs, used) = qp(
        &db,
        "MATCH (n:Item) RETURN n ORDER BY n.val DESC SKIP 1 LIMIT 3",
        &[],
    );
    assert_eq!(keys(&rs, "n"), ["m2", "a", "c"]);
    assert_eq!(used, 1);
    // A WHERE that is not the range on the sorted field keeps the sort whole.
    let other =
        "MATCH (n:Item) WHERE n.val > 1 AND n.other IS NULL RETURN n ORDER BY n.val LIMIT 2";
    assert!(plan_of(other).contains("order: None"), "{}", plan_of(other));
    let (rs, _) = qp(&db, other, &[]);
    assert_eq!(keys(&rs, "n"), ["c", "d"]);
}

/// A mixed graph: some list-valued, some int-valued, some null.  `ORDER BY`
/// in both directions with `LIMIT` matches the unindexed full sort.
/// Mutations (SET to list and back, REMOVE, node delete) keep the order
/// correct, exercising the `non_scalar` bookkeeping in the range index.
#[test]
fn order_by_limit_with_non_scalar_values_matches_full_sort() {
    let mut db = item_db(
        "nonscalar",
        &[
            ("a", Value::Int(1)),
            ("b", Value::List(vec![Value::Int(99)])),
            ("c", Value::Int(3)),
            ("d", Value::List(vec![])),
            ("e", Value::Int(2)),
        ],
    );
    // Two nodes without the property (null): sort last ascending.
    db.insert_node("Item", "n1", vec![]).unwrap();
    db.insert_node("Item", "n2", vec![("other".into(), Value::Int(0))])
        .unwrap();
    // ASC: lists before ints, nulls last.
    let asc = "MATCH (n:Item) RETURN n ORDER BY n.val LIMIT 20";
    assert_as_scan(&db, asc, &[]);
    // DESC: nulls first, then ints, then lists.
    let desc = "MATCH (n:Item) RETURN n ORDER BY n.val DESC LIMIT 20";
    assert_as_scan(&db, desc, &[]);
    // LIMIT 3 ASC: two lists (empty list < non-empty list), then the smallest int.
    let (rs, _) = qp(&db, "MATCH (n:Item) RETURN n ORDER BY n.val LIMIT 3", &[]);
    assert_eq!(keys(&rs, "n"), ["d", "b", "a"]);
    // Mutate: SET b (list) → int.  b is no longer non-scalar.
    db.set_prop("b", "val", Value::Int(0)).unwrap();
    let (rs, _) = qp(&db, "MATCH (n:Item) RETURN n ORDER BY n.val LIMIT 3", &[]);
    // d (list), b=0, a=1 (ints).
    assert_eq!(keys(&rs, "n"), ["d", "b", "a"]);
    // Mutate: REMOVE d's property (null).
    db.remove_prop("d", "val").unwrap();
    let (rs, _) = qp(&db, "MATCH (n:Item) RETURN n ORDER BY n.val LIMIT 3", &[]);
    // b=0, a=1, e=2 (no more lists, d is now null).
    assert_eq!(keys(&rs, "n"), ["b", "a", "e"]);
    // Mutate: SET d → list again.
    db.set_prop("d", "val", Value::List(vec![Value::Int(7)]))
        .unwrap();
    let (rs, _) = qp(&db, "MATCH (n:Item) RETURN n ORDER BY n.val LIMIT 2", &[]);
    // d (list), b=0.
    assert_eq!(keys(&rs, "n"), ["d", "b"]);
    // All queries answer as the unindexed scan does.
    for q in [
        "MATCH (n:Item) RETURN n ORDER BY n.val LIMIT 20",
        "MATCH (n:Item) RETURN n ORDER BY n.val DESC LIMIT 20",
        "MATCH (n:Item) RETURN n ORDER BY n.val LIMIT 3",
        "MATCH (n:Item) RETURN n ORDER BY n.val DESC LIMIT 3",
    ] {
        assert_as_scan(&db, q, &[]);
    }
}

/// `cypher` on `db` against the same query with its `WHERE` turned into
/// `(…) OR false` (or `WHERE true` added), which no fold reads: the scan.
/// Returns the shared answer as Debug text.
fn assert_as_scan(db: &GraphDb, cypher: &str, params: &[(&str, Value)]) -> String {
    let scan = match cypher.find(" WHERE ") {
        Some(at) => {
            let ret = cypher.find(" RETURN ").expect("a RETURN");
            format!(
                "{} WHERE ({}) OR false{}",
                &cypher[..at],
                &cypher[at + " WHERE ".len()..ret],
                &cypher[ret..]
            )
        }
        None => cypher.replacen(" RETURN ", " WHERE true RETURN ", 1),
    };
    let got = format!("{:?}", db.query_with_params(cypher, params));
    let want = format!("{:?}", db.query_with_params(&scan, params));
    assert_eq!(got, want, "{cypher} {params:?}");
    got
}

/// Ties that straddle the `LIMIT` and the `SKIP` boundary come out in the
/// scan's order — node id, as a stable sort leaves them — whatever order the
/// index holds them in: an `Int` key before an equal `Float` key, buckets read
/// backwards for `DESC`, and two `Int`s that are one `f64` above 2^53.
#[test]
fn ties_at_the_limit_and_skip_boundary_come_out_in_the_scans_order() {
    let big = 1i64 << 53;
    // Keys in id order; values deliberately out of that order.
    let mut db = item_db(
        "ties",
        &[
            ("a", Value::Float(10.0)),
            ("b", Value::Int(big + 1)),
            ("c", Value::Int(5)),
            ("d", Value::Int(10)),
            ("e", Value::Int(big)),
            ("f", Value::Float(10.0)),
            ("g", Value::Int(20)),
            ("h", Value::Int(10)),
            ("i", Value::Float(big as f64)),
        ],
    );
    db.insert_node("Item", "m", vec![]).unwrap();
    let cases: [(&str, &[&str]); 13] = [
        // 5, then the four 10s (a d f h, by id), 20, the 2^53 run (b e i), null.
        ("LIMIT 2", &["c", "a"]),
        ("LIMIT 3", &["c", "a", "d"]),
        ("SKIP 2 LIMIT 2", &["d", "f"]),
        ("SKIP 4 LIMIT 1", &["h"]),
        ("SKIP 6 LIMIT 2", &["b", "e"]),
        ("SKIP 7 LIMIT 3", &["e", "i", "m"]),
        // Descending: null, the 2^53 run, 20, the 10s, 5.
        ("DESC LIMIT 2", &["m", "b"]),
        ("DESC SKIP 1 LIMIT 2", &["b", "e"]),
        ("DESC SKIP 3 LIMIT 1", &["i"]),
        ("DESC SKIP 4 LIMIT 3", &["g", "a", "d"]),
        ("DESC SKIP 6 LIMIT 2", &["d", "f"]),
        ("DESC SKIP 7 LIMIT 2", &["f", "h"]),
        ("DESC SKIP 9 LIMIT 5", &["c"]),
    ];
    for (tail, want) in cases {
        let cypher = format!("MATCH (n:Item) RETURN n.key AS k, n.val AS v ORDER BY n.val {tail}");
        assert!(plan_of(&cypher).contains("order: Some"), "{cypher}");
        assert_as_scan(&db, &cypher, &[]);
        assert_eq!(keys(&q(&db, &cypher), "k"), want, "{cypher}");
    }
    // With a range on the field the cut falls inside the 10s as well.
    let ranged: [(&str, &[&str]); 4] = [
        ("LIMIT 2", &["a", "d"]),
        ("SKIP 1 LIMIT 2", &["d", "f"]),
        ("DESC LIMIT 3", &["g", "a", "d"]),
        ("DESC SKIP 2 LIMIT 2", &["d", "f"]),
    ];
    for (tail, want) in ranged {
        let cypher = format!(
            "MATCH (n:Item) WHERE n.val >= 10 AND n.val <= 20 RETURN n.key AS k ORDER BY n.val {tail}"
        );
        assert!(plan_of(&cypher).contains("order: Some"), "{cypher}");
        assert_as_scan(&db, &cypher, &[]);
        assert_eq!(keys(&q(&db, &cypher), "k"), want, "{cypher}");
    }
}

/// `LIMIT $k` and `SKIP $s` on the index plan answer as the scan does:
/// valid, missing, negative and non-integer alike.
#[test]
fn limit_and_skip_parameters_on_the_index_plan_answer_as_the_scan() {
    let db = item_db(
        "params",
        &[
            ("a", Value::Int(3)),
            ("b", Value::Int(1)),
            ("c", Value::Float(1.0)),
            ("d", Value::Int(2)),
        ],
    );
    let plain = "MATCH (n:Item) RETURN n.key AS k ORDER BY n.val SKIP $s LIMIT $k";
    let ranged =
        "MATCH (n:Item) WHERE n.val > 0 RETURN n.key AS k ORDER BY n.val DESC SKIP $s LIMIT $k";
    for query in [plain, ranged] {
        assert!(plan_of(query).contains("order: Some"), "{query}");
        let ok = assert_as_scan(&db, query, &[("s", Value::Int(1)), ("k", Value::Int(2))]);
        assert!(ok.starts_with("Ok("), "{ok}");
        for params in [
            vec![("s", Value::Int(1))],
            vec![("k", Value::Int(2))],
            vec![("s", Value::Int(0)), ("k", Value::Int(-1))],
            vec![("s", Value::Int(-1)), ("k", Value::Int(2))],
            vec![("s", Value::Int(0)), ("k", Value::Str("2".into()))],
        ] {
            let got = assert_as_scan(&db, query, &params);
            assert!(got.starts_with("Err("), "{query} {params:?}: {got}");
        }
    }
    let params = [("s", Value::Int(1)), ("k", Value::Int(2))];
    let rs = db.query_with_params(plain, &params).unwrap();
    assert_eq!(keys(&rs, "k"), ["c", "d"]);
    let rs = db.query_with_params(ranged, &params).unwrap();
    assert_eq!(keys(&rs, "k"), ["d", "b"]);
}

// ── Visibility ───────────────────────────────────────────────────────────────

/// Inside a transaction a range query sees the transaction's own writes —
/// uncommitted, so in no index — exactly as the scan does.
#[test]
fn a_range_inside_a_transaction_sees_its_own_writes() {
    let dir = tmp("tx");
    let db = mushroomdb::SharedDb::open(&dir).unwrap();
    {
        let mut w = db.write();
        w.enable_range_index("by_val", "Item", "val").unwrap();
        let mut b = w.batch();
        for i in 0..10 {
            b.insert_node(
                "Item",
                &format!("i{i}"),
                vec![("val".into(), Value::Int(i))],
            );
        }
        b.commit().unwrap();
    }
    let p = BTreeMap::new();
    let mut tx = db.begin_transaction();
    tx.execute("CREATE (n:Item {id: 'new', val: 100})", &p)
        .unwrap();
    tx.execute("MATCH (n:Item {id: 'i9'}) SET n.val = -1", &p)
        .unwrap();
    tx.execute("MATCH (n:Item {id: 'i8'}) DETACH DELETE n", &p)
        .unwrap();
    let rs = tx
        .execute("MATCH (n:Item) WHERE n.val >= 5 RETURN n.id AS k", &p)
        .unwrap();
    assert_eq!(keys(&rs, "k"), ["i5", "i6", "i7", "new"]);
    let rs = tx
        .execute("MATCH (n:Item) RETURN n.id AS k ORDER BY n.val LIMIT 2", &p)
        .unwrap();
    assert_eq!(keys(&rs, "k"), ["i9", "i0"]);
    tx.rollback();
    // Outside it, nothing changed.
    let rs = db
        .read()
        .query("MATCH (n:Item) WHERE n.val >= 8 RETURN n.id AS k", &p)
        .unwrap();
    assert_eq!(keys(&rs, "k"), ["i8", "i9"]);
}

/// Under a role the index plan returns only what the role may see.
#[test]
fn a_range_under_a_role_sees_only_the_role() {
    let mut db = item_db(
        "role",
        &[
            ("a", Value::Int(1)),
            ("b", Value::Int(2)),
            ("c", Value::Int(3)),
        ],
    );
    db.insert_node("Public", "p", vec![("val".into(), Value::Int(9))])
        .unwrap();
    let schema = mushroomdb::Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![],
        roles: vec![mushroomdb::RoleDef {
            name: "viewer".into(),
            keys: vec!["a".into(), "c".into()],
            labels: vec!["Public".into()],
            visible_where: None,
            namespaces: None,
            write: None,
        }],
    };
    db.apply_schema(&schema).unwrap();
    let mask = db.mask_for_role("viewer").unwrap();
    let p = BTreeMap::new();
    let before = range_index_scans();
    let rs = db
        .query_masked("MATCH (n:Item) WHERE n.val >= 1 RETURN n", &p, &mask)
        .unwrap();
    assert_eq!(keys(&rs, "n"), ["a", "c"]);
    let rs = db
        .query_masked(
            "MATCH (n:Item) RETURN n ORDER BY n.val DESC LIMIT 1",
            &p,
            &mask,
        )
        .unwrap();
    assert_eq!(keys(&rs, "n"), ["c"]);
    assert_eq!(range_index_scans() - before, 2);
}

/// A reader snapshot, with and without an unfolded tail, and a query as of a
/// past commit, answer as the writer's scan does.
#[test]
fn readers_and_past_commits_answer_as_the_scan() {
    let dir = tmp("reader");
    let db = mushroomdb::SharedDb::open(&dir).unwrap();
    {
        let mut w = db.write();
        w.enable_range_index("by_val", "Item", "val").unwrap();
        for i in 0..6 {
            w.insert_node(
                "Item",
                &format!("i{i}"),
                vec![("val".into(), Value::Int(i))],
            )
            .unwrap();
        }
    }
    let p = BTreeMap::new();
    let cypher = "MATCH (n:Item) WHERE n.val > 2 RETURN n.id AS k";
    let before = db.reader().query(cypher, &p).unwrap();
    assert_eq!(keys(&before, "k"), ["i3", "i4", "i5"]);
    // `query_at` takes a 0-based commit index: the last one so far.
    let commit = db.read().commit_seq() - 1;
    db.write().set_prop("i0", "val", Value::Int(10)).unwrap();
    db.write().set_prop("i5", "val", Value::Int(0)).unwrap();
    let now = db.reader().query(cypher, &p).unwrap();
    assert_eq!(keys(&now, "k"), ["i0", "i3", "i4"]);
    let top = db
        .reader()
        .query(
            "MATCH (n:Item) RETURN n.id AS k ORDER BY n.val DESC LIMIT 2",
            &p,
        )
        .unwrap();
    assert_eq!(keys(&top, "k"), ["i0", "i4"]);
    let past = db.read().query_at(commit, cypher, &p).unwrap();
    assert_eq!(keys(&past, "k"), keys(&before, "k"));
}

// ── Randomized differential: index plan vs forced scan ───────────────────────

/// xorshift64*: deterministic, no dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }
}

const STRS: [&str; 12] = [
    "", "a", "al", "alice", "alicia", "b", "ba", "bob", "é", "e\u{301}", "日", "日本",
];

/// A value the indexed field may hold, or `None` for no property.
fn random_value(rng: &mut Rng) -> Option<Value> {
    Some(match rng.below(14) {
        0 | 1 => Value::Int(rng.below(9) as i64 - 4),
        2 => Value::Int(rng.below(3) as i64 + (1 << 53)),
        3 => Value::Float(*rng.pick(&[
            -2.5,
            -0.0,
            0.0,
            1.0,
            2.0,
            2.5,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
            9007199254740992.0,
            9007199254740993.0_f64.next_up(),
            9007199254740992.0_f64.next_down(),
        ])),
        4 | 5 => Value::Str(rng.pick(&STRS).to_string()),
        6 => Value::Bool(rng.below(2) == 1),
        7 => Value::Date(rng.below(5) as i32 - 2),
        8 => Value::LocalDateTime(rng.below(5) as i64 - 2),
        9 => Value::DateTime {
            micros_utc: rng.below(3) as i64,
            offset_seconds: *rng.pick(&[0, 3600, -3600]),
        },
        10 => Value::List(vec![Value::Int(rng.below(3) as i64)]),
        11 => Value::Duration {
            months: 0,
            days: rng.below(3) as i32,
            nanos: 0,
        },
        12 => Value::Binary(vec![rng.below(3) as u8]),
        _ => return None,
    })
}

/// A bound: a literal where Cypher can spell one, else a `$param`.
fn random_bound(rng: &mut Rng, name: &str, params: &mut Vec<(String, Value)>) -> String {
    let value = match random_value(rng) {
        Some(v) => v,
        None => Value::Null,
    };
    let literal = match &value {
        Value::Int(i) if rng.below(2) == 0 => Some(i.to_string()),
        Value::Float(f) if f.is_finite() && *f != 0.0 && rng.below(2) == 0 => {
            Some(format!("{f:?}"))
        }
        Value::Str(s) if rng.below(2) == 0 => Some(format!("'{s}'")),
        _ => None,
    };
    literal.unwrap_or_else(|| {
        params.push((name.to_string(), value));
        format!("${name}")
    })
}

/// A `WHERE` body over `n.val`, or `None`.
fn random_where(rng: &mut Rng, params: &mut Vec<(String, Value)>) -> Option<String> {
    let ops = ["<", "<=", ">", ">="];
    Some(match rng.below(8) {
        0 | 1 => format!(
            "n.val {} {}",
            rng.pick(&ops),
            random_bound(rng, "a", params)
        ),
        2 => format!(
            "{} {} n.val",
            random_bound(rng, "a", params),
            rng.pick(&ops)
        ),
        3 | 4 => format!(
            "n.val {} {} AND n.val {} {}",
            rng.pick(&[">", ">="]),
            random_bound(rng, "a", params),
            rng.pick(&["<", "<="]),
            random_bound(rng, "b", params),
        ),
        5 => format!("n.val STARTS WITH {}", random_bound(rng, "a", params)),
        6 => format!(
            "n.val {} {} AND n.other = 1",
            rng.pick(&ops),
            random_bound(rng, "a", params)
        ),
        _ => return None,
    })
}

fn random_query(rng: &mut Rng) -> (String, String, Vec<(String, Value)>) {
    let mut params = Vec::new();
    let pred = random_where(rng, &mut params);
    let ret = *rng.pick(&[
        "RETURN n.key AS k, n.val AS v",
        "RETURN n",
        "RETURN count(*) AS c",
        "RETURN n.other AS o, count(*) AS c",
    ]);
    let aggregate = ret.contains("count");
    let mut tail = String::new();
    if !aggregate {
        match rng.below(4) {
            0 => tail.push_str(" ORDER BY n.val"),
            1 => tail.push_str(" ORDER BY n.val DESC"),
            2 if ret.contains(" AS v") => {
                let order: &&str = rng.pick(&[" ORDER BY v", " ORDER BY v DESC"]);
                tail.push_str(order)
            }
            _ => {}
        }
        if rng.below(3) == 0 {
            tail.push_str(&format!(" SKIP {}", rng.below(4)));
        }
        if rng.below(4) != 0 {
            tail.push_str(&format!(" LIMIT {}", rng.below(7)));
        }
    }
    let indexed = match &pred {
        Some(p) => format!("MATCH (n:Item) WHERE {p} {ret}{tail}"),
        None => format!("MATCH (n:Item) {ret}{tail}"),
    };
    // `(p) OR false` keeps every row `p` keeps, and no fold reads it.
    let scanned = match &pred {
        Some(p) => format!("MATCH (n:Item) WHERE ({p}) OR false {ret}{tail}"),
        None => format!("MATCH (n:Item) WHERE true {ret}{tail}"),
    };
    (indexed, scanned, params)
}

/// The same writes on `db`: inserts, the index (if `index`) half-way, then
/// updates, removals and deletes after it.
fn random_graph(rng: &mut Rng, db: &mut GraphDb, index: bool) {
    let n = 30 + rng.below(30);
    let insert = |rng: &mut Rng, db: &mut GraphDb, i: u64| {
        let mut props = Vec::new();
        if let Some(v) = random_value(rng) {
            props.push(("val".to_string(), v));
        }
        if rng.below(2) == 0 {
            props.push(("other".to_string(), Value::Int(rng.below(2) as i64)));
        }
        let label = if rng.below(6) == 0 { "Other" } else { "Item" };
        db.insert_node(label, &format!("k{i:02}"), props).unwrap();
    };
    for i in 0..n / 2 {
        insert(rng, db, i);
    }
    if index {
        db.enable_range_index("by_val", "Item", "val").unwrap();
    }
    for i in n / 2..n {
        insert(rng, db, i);
    }
    for _ in 0..n / 4 {
        let key = format!("k{:02}", rng.below(n));
        if db
            .query(
                &format!("MATCH (n {{id: '{key}'}}) RETURN n"),
                &BTreeMap::new(),
            )
            .unwrap()
            .is_empty()
        {
            continue;
        }
        match rng.below(3) {
            0 => match random_value(rng) {
                Some(v) => db.set_prop(&key, "val", v).unwrap(),
                None => {
                    db.remove_prop(&key, "val").unwrap();
                }
            },
            1 => {
                db.remove_prop(&key, "val").unwrap();
            }
            _ => {
                db.delete_node(&key).unwrap();
            }
        }
    }
}

fn run(db: &GraphDb, cypher: &str, params: &[(String, Value)]) -> String {
    let map: BTreeMap<String, Value> = params.iter().cloned().collect();
    // Debug text compares NaN with NaN, which `==` on values does not.
    format!("{:?}", db.query(cypher, &map))
}

#[test]
fn index_plans_return_exactly_what_the_scan_returns() {
    let mut used = 0;
    let mut queries = 0;
    let stops_before = range_top_k_stops();
    for seed in 1..=12u64 {
        let graph_seed = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        let mut indexed = GraphDb::open(&tmp(&format!("diff-ix-{seed}"))).unwrap();
        let mut plain = GraphDb::open(&tmp(&format!("diff-plain-{seed}"))).unwrap();
        random_graph(&mut Rng(graph_seed), &mut indexed, true);
        random_graph(&mut Rng(graph_seed), &mut plain, false);
        let mut rng = Rng(graph_seed ^ 0xdead_beef);
        for _ in 0..150 {
            let (index_q, scan_q, params) = random_query(&mut rng);
            // A `LIMIT` without `ORDER BY` scans the first slots before it
            // reads the index; with no probe at all the index answers whole.
            set_pull_probe_slots(*rng.pick(&[0, 1, 7, 4096]));
            let before = range_index_scans();
            let got = run(&indexed, &index_q, &params);
            used += range_index_scans() - before;
            queries += 1;
            let want = run(&indexed, &scan_q, &params);
            assert_eq!(
                got, want,
                "seed {seed}: {index_q} {params:?}\n  vs {scan_q}"
            );
            // Without the index the plan falls back to the label scan.
            let unindexed = run(&plain, &index_q, &params);
            assert_eq!(
                got, unindexed,
                "seed {seed}, no index: {index_q} {params:?}"
            );
            // Under a mask (what a role or a namespace resolves to).
            let visible: Vec<String> = (0..60)
                .filter(|_| rng.below(2) == 0)
                .map(|i| format!("k{i:02}"))
                .collect();
            let mask = NodeMask::from_keys(&indexed, visible.iter().map(String::as_str));
            let map: BTreeMap<String, Value> = params.iter().cloned().collect();
            let got = format!("{:?}", indexed.query_masked(&index_q, &map, &mask));
            let want = format!("{:?}", indexed.query_masked(&scan_q, &map, &mask));
            assert_eq!(got, want, "seed {seed}, masked: {index_q} {params:?}");
        }
    }
    // The differential is only worth something if the index answered.
    assert!(
        used * 3 > queries,
        "{used} of {queries} queries read the index"
    );
    // And the top-k cut, which stops reading the index part-way, ran often.
    let stops = range_top_k_stops() - stops_before;
    assert!(
        stops >= 50,
        "only {stops} ORDER BY … LIMIT queries stopped early"
    );
}
