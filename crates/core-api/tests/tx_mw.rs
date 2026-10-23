//! TX-MW (defect-ledger row 144): the per-row write forms — `MATCH … CREATE`,
//! `MATCH … MERGE`, `UNWIND … <write>`, the `SET` map forms and `SET` /
//! `REMOVE … RETURN` — run inside a [`Transaction`] exactly as they run
//! auto-commit: the same results and counters, reading the transaction's own
//! earlier writes, nothing visible outside until COMMIT, nothing left by
//! ROLLBACK, and the commit-time checks (conflicts, constraints) still applied.

use mushroomdb::{GraphError, SharedDb, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;

type Rows = Vec<Vec<Option<Value>>>;

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "graphdb-txmw-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn s(v: &str) -> Value {
    Value::Str(v.into())
}

fn none() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

fn ab(a: &str, b: &str) -> BTreeMap<String, Value> {
    BTreeMap::from([("a".to_string(), s(a)), ("b".to_string(), s(b))])
}

fn rows(rs: &mushroomdb::ResultSet) -> Rows {
    (0..rs.len()).map(|i| rs.row(i).to_vec()).collect()
}

fn count(rs: &mushroomdb::ResultSet, col: &str) -> i64 {
    match rs.get(0, col) {
        Some(Value::Int(n)) => *n,
        other => panic!("column {col}: {other:?}"),
    }
}

fn outside(db: &SharedDb, q: &str) -> Rows {
    rows(&db.reader().query(q, &none()).unwrap())
}

/// Every node (key, label, properties) and every edge, sorted.
fn state(db: &SharedDb) -> (Rows, Rows) {
    (
        outside(
            db,
            "MATCH (n) RETURN n.key AS k, labels(n) AS l, properties(n) AS p ORDER BY k",
        ),
        outside(
            db,
            "MATCH (a)-[r]->(b) RETURN a.key AS a, type(r) AS t, b.key AS b ORDER BY a, t, b",
        ),
    )
}

const PEOPLE: [&str; 4] = [
    "CREATE (:Person {id: 'a', v: 1})",
    "CREATE (:Person {id: 'b', v: 2})",
    "CREATE (:Person {id: 'c', v: 3})",
    "CREATE (:Person {id: 'd', v: 4})",
];

fn people(name: &str) -> SharedDb {
    let db = SharedDb::open(&tmp(name)).unwrap();
    for q in PEOPLE {
        db.write().query_write(q, &none()).unwrap();
    }
    db
}

const CONNECT: &str = "MATCH (a {id: $a}), (b {id: $b}) CREATE (a)-[:K]->(b)";
const EDGES: &str = "MATCH (x)-[:K]->(y) RETURN x.id AS x, y.id AS y ORDER BY x, y";

// ── The driver's everyday write ─────────────────────────────────────────────

#[test]
fn match_then_create_connects_two_nodes_inside_a_transaction() {
    let db = people("connect");
    let mut tx = db.begin_transaction();
    let rs = tx.execute(CONNECT, &ab("a", "b")).unwrap();
    assert_eq!(count(&rs, "created"), 0);
    assert_eq!(count(&rs, "relationships_created"), 1);
    // Read-your-write inside the transaction; nothing outside yet.
    assert_eq!(
        rows(&tx.execute(EDGES, &none()).unwrap()),
        vec![vec![Some(s("a")), Some(s("b"))]]
    );
    assert!(outside(&db, EDGES).is_empty());
    tx.commit().unwrap();
    assert_eq!(outside(&db, EDGES), vec![vec![Some(s("a")), Some(s("b"))]]);
}

#[test]
fn rollback_of_match_driven_writes_leaves_no_trace() {
    let dir = tmp("rollback");
    let db = SharedDb::open(&dir).unwrap();
    for q in PEOPLE {
        db.write().query_write(q, &none()).unwrap();
    }
    let before = state(&db);
    let frames = mushroomdb::wal_commit_count_at(&dir).unwrap();
    let mut tx = db.begin_transaction();
    tx.execute(CONNECT, &ab("a", "b")).unwrap();
    tx.execute(
        "UNWIND ['x', 'y'] AS k MATCH (p:Person {id: 'c'}) CREATE (p)-[:K]->(:Tag {id: k})",
        &none(),
    )
    .unwrap();
    tx.execute("MATCH (p:Person) MERGE (p)-[:IN]->(:G {id: 'g'})", &none())
        .unwrap();
    tx.execute("MATCH (p:Person {id: 'd'}) SET p += {w: 9}", &none())
        .unwrap();
    tx.rollback();
    assert_eq!(state(&db), before);
    assert_eq!(mushroomdb::wal_commit_count_at(&dir).unwrap(), frames);
}

/// A later statement reads what an earlier one wrote, and a per-row write
/// reads both the transaction's earlier writes and its own earlier rows.
#[test]
fn per_row_writes_read_the_transactions_own_writes() {
    let db = SharedDb::open(&tmp("ryw")).unwrap();
    let mut tx = db.begin_transaction();
    let rs = tx
        .execute(
            "UNWIND $ids AS k CREATE (:N {id: k, c: 0})",
            &BTreeMap::from([(
                "ids".to_string(),
                Value::List(vec![s("n1"), s("n2"), s("n3")]),
            )]),
        )
        .unwrap();
    assert_eq!(count(&rs, "created"), 3);
    // Matches the nodes the previous statement created (only in the tx).
    let rs = tx
        .execute(
            "MATCH (a:N {id: 'n1'}), (b:N) WHERE b.id <> 'n1' CREATE (a)-[:K]->(b)",
            &none(),
        )
        .unwrap();
    assert_eq!(count(&rs, "relationships_created"), 2);
    // Two rows match the same node: the second reads the first's write.
    let rs = tx
        .execute(
            "UNWIND [1, 2] AS i MATCH (n:N {id: 'n1'}) SET n.c = n.c + 1",
            &none(),
        )
        .unwrap();
    assert_eq!(count(&rs, "properties_set"), 2);
    // MERGE finds the edge an earlier statement created: no new relationship.
    let rs = tx
        .execute(
            "MATCH (a:N {id: 'n1'}), (b:N {id: 'n2'}) MERGE (a)-[:K]->(b) ON MATCH SET a.seen = true",
            &none(),
        )
        .unwrap();
    assert_eq!(count(&rs, "relationships_created"), 0);
    assert_eq!(count(&rs, "properties_set"), 1);
    // A RETURN reads the statement's own writes over the transaction's.
    let rs = tx
        .execute(
            "MATCH (n:N {id: 'n1'}) CREATE (n)-[:K]->(m:N {id: 'n4', c: n.c}) \
             RETURN n.c AS c, n.seen AS seen, m.id AS m",
            &none(),
        )
        .unwrap();
    assert_eq!(
        rows(&rs),
        vec![vec![
            Some(Value::Int(2)),
            Some(Value::Bool(true)),
            Some(s("n4"))
        ]]
    );
    // DETACH DELETE through the per-row form removes the tx's own edges too.
    let rs = tx
        .execute(
            "UNWIND ['n3'] AS k MATCH (n:N {id: k}) DETACH DELETE n",
            &none(),
        )
        .unwrap();
    assert_eq!(count(&rs, "deleted"), 1);
    let inside = rows(&tx.execute(EDGES, &none()).unwrap());
    assert!(db.reader().node_info("n1").is_none(), "not committed yet");
    tx.commit().unwrap();
    assert_eq!(outside(&db, EDGES), inside);
    assert_eq!(
        inside,
        vec![
            vec![Some(s("n1")), Some(s("n2"))],
            vec![Some(s("n1")), Some(s("n4"))],
        ]
    );
    let n1 = db.reader().node_info("n1").unwrap();
    assert_eq!(n1.props.get("c"), Some(&Value::Int(2)));
    assert!(db.reader().node_info("n3").is_none());
}

#[test]
fn a_failed_per_row_statement_buffers_nothing_and_the_transaction_goes_on() {
    let db = people("failed");
    let mut tx = db.begin_transaction();
    tx.execute(CONNECT, &ab("a", "b")).unwrap();
    // The second row creates a key the first row created: DuplicateKey.
    let e = tx
        .execute(
            "UNWIND [1, 2] AS i MATCH (p:Person {id: 'a'}) CREATE (:T {id: 'dup'})",
            &none(),
        )
        .unwrap_err();
    assert!(matches!(e, GraphError::DuplicateKey { .. }), "{e:?}");
    assert!(rows(
        &tx.execute("MATCH (t:T {id: 'dup'}) RETURN t.id AS t", &none())
            .unwrap()
    )
    .is_empty());
    tx.commit().unwrap();
    assert_eq!(outside(&db, EDGES), vec![vec![Some(s("a")), Some(s("b"))]]);
    assert!(db.reader().node_info("dup").is_none());
}

// ── The forms that were refused ─────────────────────────────────────────────

#[test]
fn set_map_and_set_or_remove_with_return_run_inside_a_transaction() {
    let db = people("maps");
    let mut tx = db.begin_transaction();
    let rs = tx
        .execute(
            "MATCH (n:Person {id: 'a'}) SET n += {w: 7} RETURN n.w AS w",
            &none(),
        )
        .unwrap();
    assert_eq!(rows(&rs), vec![vec![Some(Value::Int(7))]]);
    let rs = tx
        .execute("MATCH (n:Person {id: 'b'}) SET n = {z: 1}", &none())
        .unwrap();
    assert_eq!(count(&rs, "properties_set"), 2, "z set, v removed");
    let rs = tx
        .execute(
            "MATCH (n:Person {id: 'c'}) SET n.v = n.v + 10 RETURN n.v AS v",
            &none(),
        )
        .unwrap();
    assert_eq!(rows(&rs), vec![vec![Some(Value::Int(13))]]);
    let rs = tx
        .execute(
            "MATCH (n:Person {id: 'd'}) REMOVE n.v RETURN n.v AS v, n.id AS id",
            &none(),
        )
        .unwrap();
    assert_eq!(rows(&rs), vec![vec![None, Some(s("d"))]]);
    assert!(outside(&db, "MATCH (n:Person) WHERE n.w = 7 RETURN n.id").is_empty());
    tx.commit().unwrap();
    let props = |k: &str| db.reader().node_info(k).unwrap().props;
    assert_eq!(props("a").get("w"), Some(&Value::Int(7)));
    assert_eq!(props("b").get("v"), None);
    assert_eq!(props("b").get("z"), Some(&Value::Int(1)));
    assert_eq!(props("c").get("v"), Some(&Value::Int(13)));
    assert_eq!(props("d").get("v"), None);
}

/// `MATCH … SET` inside a transaction reads the statement's own earlier rows
/// and items, as auto-commit does. Before TX-MW the transaction evaluated
/// every value against the state before the statement: four rows for one node
/// added 1, not 4, and `n.w = n.v + 0` read the old `v`.
#[test]
fn match_set_reads_its_own_earlier_rows_inside_a_transaction() {
    let db = people("set-rows");
    let mut tx = db.begin_transaction();
    let rs = tx
        .execute(
            "MATCH (n:Person {id: 'a'}), (m:Person) SET n.v = n.v + 1",
            &none(),
        )
        .unwrap();
    assert_eq!(count(&rs, "properties_set"), 4);
    tx.execute(
        "MATCH (n:Person {id: 'b'}) SET n.v = 10, n.w = n.v + 0",
        &none(),
    )
    .unwrap();
    tx.commit().unwrap();
    let props = |k: &str| db.reader().node_info(k).unwrap().props;
    assert_eq!(props("a").get("v"), Some(&Value::Int(5)));
    assert_eq!(props("b").get("w"), Some(&Value::Int(10)));
}

/// An `id` property write that breaks `id == key` is refused at the
/// statement, for the per-row forms as for the others, and buffers nothing.
#[test]
fn an_id_rewrite_is_refused_at_the_statement() {
    let db = people("id-rule");
    let mut tx = db.begin_transaction();
    for q in [
        "MATCH (n:Person {id: 'a'}) SET n.id = 'zz'",
        "MATCH (n:Person {id: 'a'}) SET n += {id: 'zz'}",
        "UNWIND [1] AS i MATCH (n:Person {id: 'a'}) SET n.id = 'zz'",
        "MATCH (n:Person {id: 'a'}) REMOVE n.id RETURN n.v AS v",
    ] {
        assert!(tx.execute(q, &none()).is_err(), "{q}");
    }
    tx.execute(CONNECT, &ab("a", "b")).unwrap();
    tx.commit().unwrap();
    assert_eq!(
        db.reader().node_info("a").unwrap().props.get("id"),
        Some(&s("a"))
    );
}

// ── Commit-time checks ──────────────────────────────────────────────────────

/// Two transactions connecting the same pair: both endpoints are in each
/// one's conflict set, so the second to commit fails with `CasConflict` and
/// writes nothing (TX-1's semantics for an edge written in a transaction).
#[test]
fn two_transactions_connecting_the_same_pair_conflict() {
    let db = people("conflict");
    let mut t1 = db.begin_transaction();
    let mut t2 = db.begin_transaction();
    t1.execute(CONNECT, &ab("a", "b")).unwrap();
    t2.execute(CONNECT, &ab("a", "b")).unwrap();
    t1.commit().unwrap();
    let e = t2.commit().unwrap_err();
    assert!(
        matches!(e, GraphError::CasConflict { ref key, .. } if key == "a" || key == "b"),
        "{e:?}"
    );
    assert_eq!(outside(&db, EDGES), vec![vec![Some(s("a")), Some(s("b"))]]);
}

#[test]
fn a_constraint_is_enforced_at_commit_for_match_driven_writes() {
    let db = people("constraint");
    db.write()
        .query_write(
            "CREATE CONSTRAINT person_v FOR (n:Person) REQUIRE n.v IS UNIQUE",
            &none(),
        )
        .unwrap();
    let before = state(&db);
    let mut tx = db.begin_transaction();
    // Copies a's `v` onto a new Person: a unique-constraint violation.
    tx.execute(
        "MATCH (a:Person {id: 'a'}) CREATE (:Person {id: 'e', v: a.v})",
        &none(),
    )
    .unwrap();
    tx.execute(CONNECT, &ab("a", "b")).unwrap();
    assert!(tx.commit().is_err());
    assert_eq!(state(&db), before, "the whole transaction is rejected");
}

#[test]
fn schema_statements_are_still_refused_inside_a_transaction() {
    let db = people("ddl");
    let mut tx = db.begin_transaction();
    let e = tx
        .execute(
            "CREATE CONSTRAINT c FOR (n:Person) REQUIRE n.v IS UNIQUE",
            &none(),
        )
        .unwrap_err();
    assert!(
        e.to_string().contains("cannot run inside a transaction"),
        "{e}"
    );
}

// ── Differential: auto-commit vs one transaction ────────────────────────────

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.below(xs.len())]
    }
}

fn random_seed_graph(rng: &mut Rng) -> Vec<String> {
    let n = 4 + rng.below(5);
    let mut stmts = Vec::new();
    for i in 0..n {
        let label = rng.pick(&["A", "B", "C"]);
        let v = rng.below(4);
        stmts.push(format!("CREATE (:{label} {{id: 'n{i}', v: {v}}})"));
    }
    for _ in 0..rng.below(6) {
        let (x, y) = (rng.below(n), rng.below(n));
        if x == y {
            continue; // no self-loops: see `random_write`
        }
        stmts.push(format!(
            "MATCH (x {{id: 'n{x}'}}), (y {{id: 'n{y}'}}) CREATE (x)-[:K]->(y)"
        ));
    }
    stmts
}

/// A random write statement of a form that was refused in a transaction,
/// or a plain one, so later statements read the earlier ones' writes.
///
/// No statement makes a self-loop: auto-commit `DETACH DELETE` counts a
/// self-loop as two deleted relationships (once from each end), which the
/// transaction does not copy; reported with TX-MW, outside its scope.
fn random_write(rng: &mut Rng, step: usize) -> String {
    let l1 = rng.pick(&["A", "B", "C"]);
    let l2 = rng.pick(&["A", "B", "C"]);
    let k = rng.below(4);
    let op = rng.pick(&["=", "<", ">=", "<>"]);
    let t = rng.pick(&["K", "J"]);
    let ret = rng.pick(&["", " RETURN count(*) AS c", " RETURN a.id AS a ORDER BY a"]);
    match rng.below(13) {
        0 => format!(
            "MATCH (a:{l1}), (b:{l2}) WHERE a.v {op} {k} AND a.id <> b.id \
             CREATE (a)-[:{t}]->(b){ret}"
        ),
        1 => format!(
            "MATCH (a:{l1}) WHERE a.v {op} {k} \
             CREATE (c:C {{id: a.id + '-s{step}', v: a.v + 1}})-[:{t}]->(a){ret}"
        ),
        2 => format!(
            "MATCH (a:{l1})-[:K]->(b) MERGE (a)-[r:{t}]->(b) \
             ON CREATE SET a.made = {k} ON MATCH SET a.seen = {k}{ret}"
        ),
        3 => format!(
            "MATCH (a:{l1}) WHERE a.v {op} {k} \
             MERGE (g:G {{id: 'g' + toString(a.v)}})-[:{t}]->(h:G {{id: 'h{k}'}}){ret}"
        ),
        4 => format!(
            "UNWIND range(0, {k}) AS i MATCH (a:{l1}) WHERE a.v >= i SET a.v = a.v + 1{}",
            if ret.is_empty() { "" } else { " RETURN count(*) AS c" }
        ),
        5 => format!("UNWIND ['x{k}', 'y{k}', 'x{k}'] AS key MERGE (:{l1} {{id: key}}){ret_m}", ret_m = if ret.is_empty() { "" } else { " RETURN count(*) AS c" }),
        6 => format!("MATCH (a:{l1}) WHERE a.v {op} {k} DETACH DELETE a"),
        7 => format!("MATCH (a:{l1}) WHERE a.v {op} {k} SET a += {{w: a.v * 2, made: null}}"),
        8 => format!("MATCH (a:{l1}) WHERE a.v {op} {k} SET a.v = a.v + 1 RETURN a.id AS a, a.v AS v ORDER BY a"),
        9 => format!("MATCH (a:{l1}) WHERE a.v {op} {k} REMOVE a.w RETURN a.id AS a ORDER BY a"),
        10 => format!("MATCH (a:{l1}) WHERE a.v {op} {k} SET a = {{v: {k}, id: a.id}}"),
        11 => format!("MATCH (a:{l1})-[r:{t}]->(b:{l2}) WHERE a.v {op} {k} DELETE r"),
        _ => format!(
            "MATCH (a:{l1}) OPTIONAL MATCH (a)-[:K]->(b:{l2}) WITH a, b WHERE b IS NOT NULL \
             MERGE (a)-[:{t}]->(b){ret}"
        ),
    }
}

#[test]
fn the_same_writes_auto_commit_and_in_one_transaction_agree_randomized() {
    let (mut changed, mut failed) = (0, 0);
    let mut total = 0;
    for seed in 1..=50u64 {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ seed.wrapping_mul(0x1000_0001));
        let auto = SharedDb::open(&tmp(&format!("diff-auto-{seed}"))).unwrap();
        let txd = SharedDb::open(&tmp(&format!("diff-tx-{seed}"))).unwrap();
        for q in random_seed_graph(&mut rng) {
            auto.write().query_write(&q, &none()).unwrap();
            txd.write().query_write(&q, &none()).unwrap();
        }
        let start = state(&auto);
        let mut tx = txd.begin_transaction();
        for step in 0..8 {
            let q = random_write(&mut rng, step);
            let a = auto.write().query_write(&q, &none()).map(|rs| rows(&rs));
            let b = tx.execute(&q, &none()).map(|rs| rows(&rs));
            match (&a, &b) {
                (Ok(x), Ok(y)) => assert_eq!(x, y, "seed {seed}: {q}"),
                (Err(x), Err(y)) => assert_eq!(x.to_string(), y.to_string(), "seed {seed}: {q}"),
                _ => panic!("seed {seed}: {q}: {a:?} vs {b:?}"),
            }
            failed += usize::from(a.is_err());
            total += 1;
        }
        tx.commit().unwrap();
        let end = state(&auto);
        assert_eq!(end, state(&txd), "seed {seed}");
        changed += usize::from(end != start);
    }
    assert!(changed >= 40, "only {changed} of 50 runs changed the store");
    assert!(failed * 3 < total, "{failed} of {total} statements failed");
}
