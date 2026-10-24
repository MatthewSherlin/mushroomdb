//! QL-CL (defect row 161): one clock instant per statement and per
//! transaction, on every write path. The clock is a counter that moves on
//! every read, so a value written twice from two reads could never match.

use core_query::cypher::with_test_clock;
use mushroomdb::{RealFs, ResultSet, SharedDb, Value};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;

type GraphDb = mushroomdb::GraphDb<RealFs>;

const START: i64 = 1_700_000_000_000_000;

fn tmp(name: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("mushroomdb-qlcl-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn p() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

/// Reads `START`, `START + 1`, … one per read of the clock.
fn ticking() -> impl FnMut() -> i64 + 'static {
    let reads = Rc::new(Cell::new(0));
    move || {
        let n = reads.get();
        reads.set(n + 1);
        START + n
    }
}

fn column(rs: &ResultSet, name: &str) -> Vec<Option<Value>> {
    (0..rs.len()).map(|i| rs.get(i, name).cloned()).collect()
}

fn distinct(values: Vec<Option<Value>>) -> Vec<Option<Value>> {
    let mut out: Vec<Option<Value>> = Vec::new();
    for v in values {
        if !out.contains(&v) {
            out.push(v);
        }
    }
    out
}

#[test]
fn set_over_many_rows_writes_one_value() {
    let mut db = GraphDb::open(&tmp("set")).unwrap();
    with_test_clock(ticking(), || {
        db.query_write(
            "UNWIND range(1, 50) AS i CREATE (:P {id: toString(i)})",
            &p(),
        )
        .unwrap();
        db.query_write("MATCH (n:P) SET n.t = datetime()", &p())
            .unwrap();
        let rs = db.query("MATCH (n:P) RETURN n.t AS t", &p()).unwrap();
        assert_eq!(rs.len(), 50);
        let values = distinct(column(&rs, "t"));
        assert_eq!(values.len(), 1, "{values:?}");
        assert!(matches!(values[0], Some(Value::DateTime { .. })));
    });
}

#[test]
fn unwind_create_and_set_map_write_one_value() {
    let mut db = GraphDb::open(&tmp("unwind")).unwrap();
    with_test_clock(ticking(), || {
        db.query_write(
            "UNWIND range(1, 20) AS i CREATE (:Q {id: toString(i), t: localdatetime(), u: localdatetime.statement()})",
            &p(),
        )
        .unwrap();
        db.query_write(
            "MATCH (n:Q) SET n += {v: date(), w: datetime.transaction()}",
            &p(),
        )
        .unwrap();
        for field in ["t", "u", "v", "w"] {
            let rs = db
                .query(&format!("MATCH (n:Q) RETURN n.{field} AS x"), &p())
                .unwrap();
            let values = distinct(column(&rs, "x"));
            assert_eq!(values.len(), 1, "{field}: {values:?}");
        }
        // `t` and `u` are the same statement's instant.
        let rs = db
            .query("MATCH (n:Q) RETURN n.t = n.u AS same", &p())
            .unwrap();
        assert_eq!(distinct(column(&rs, "same")), vec![Some(Value::Bool(true))]);
    });
}

#[test]
fn a_write_with_a_read_part_is_one_statement() {
    let mut db = GraphDb::open(&tmp("merge")).unwrap();
    with_test_clock(ticking(), || {
        db.query_write(
            "UNWIND range(1, 5) AS i MERGE (n:M {id: toString(i)}) ON CREATE SET n.created = datetime(), n.seen = datetime()",
            &p(),
        )
        .unwrap();
        let rs = db
            .query("MATCH (n:M) RETURN n.created AS c, n.seen AS s", &p())
            .unwrap();
        assert_eq!(rs.len(), 5);
        assert_eq!(distinct(column(&rs, "c")).len(), 1);
        assert_eq!(column(&rs, "c"), column(&rs, "s"));
    });
}

#[test]
fn a_transaction_holds_one_instant_and_each_statement_its_own() {
    let db = SharedDb::open(&tmp("tx")).unwrap();
    with_test_clock(ticking(), || {
        let mut tx = db.begin_transaction(); // BEGIN reads START
        let first = tx
            .execute(
                "RETURN datetime.transaction() AS t, datetime.statement() AS s, datetime() AS d, datetime.realtime() AS r",
                &p(),
            )
            .unwrap();
        let second = tx
            .execute(
                "RETURN datetime.transaction() AS t, datetime.statement() AS s",
                &p(),
            )
            .unwrap();
        let get = |rs: &ResultSet, c: &str| rs.get(0, c).cloned();
        let begun = Some(Value::DateTime {
            micros_utc: START,
            offset_seconds: 0,
        });
        assert_eq!(get(&first, "t"), begun);
        assert_eq!(get(&second, "t"), begun);
        // `.statement()` and `datetime()` agree within a statement, and move on
        // with the next one; `.realtime()` is neither.
        assert_eq!(get(&first, "s"), get(&first, "d"));
        assert_ne!(get(&first, "s"), get(&second, "s"));
        assert_ne!(get(&first, "s"), begun);
        assert_ne!(get(&first, "r"), get(&first, "s"));
    });
}

#[test]
fn transaction_writes_share_the_begin_instant() {
    let db = SharedDb::open(&tmp("txw")).unwrap();
    with_test_clock(ticking(), || {
        let mut tx = db.begin_transaction();
        tx.execute(
            "CREATE (:T {id: 'a', t: datetime.transaction(), s: datetime()})",
            &p(),
        )
        .unwrap();
        tx.execute(
            "CREATE (:T {id: 'b', t: datetime.transaction(), s: datetime()})",
            &p(),
        )
        .unwrap();
        tx.execute("MATCH (n:T) SET n.u = datetime.transaction()", &p())
            .unwrap();
        let rs = tx
            .execute("MATCH (n:T) RETURN n.t AS t, n.u AS u, n.s AS s", &p())
            .unwrap();
        assert_eq!(rs.len(), 2);
        assert_eq!(distinct(column(&rs, "t")).len(), 1);
        assert_eq!(column(&rs, "t"), column(&rs, "u"));
        // Two statements, two statement instants.
        assert_eq!(distinct(column(&rs, "s")).len(), 2);
    });
}
