//! QL-CX: the everyday Cypher forms QL-12 refused by name now run, through the
//! public API.
//!
//! - top-level `MATCH … CREATE` / `MATCH … MERGE`, run exactly as the
//!   `UNWIND [1] AS _ MATCH … CREATE/MERGE` form (randomized differential);
//! - defect-ledger row 143: a created relationship is counted in `created`, on
//!   every create path;
//! - `RETURN *` / `WITH *`;
//! - label predicates `n:Label` (and `r:TYPE`) in any expression, three-valued
//!   (randomized differential against `labels(n)`);
//! - a leading `OPTIONAL MATCH`.

use mushroomdb::{schema::Schema, GraphError, RealFs, RoleDef, SharedDb, Value, WriteScope};
use std::collections::BTreeMap;
use std::path::PathBuf;

type GraphDb = mushroomdb::GraphDb<RealFs>;
type Rows = Vec<Vec<Option<Value>>>;

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "graphdb-qlcx-{name}-{}-{nanos}",
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

fn detail(e: GraphError) -> String {
    match e {
        GraphError::QueryError { detail } => detail,
        other => panic!("expected QueryError, got {other:?}"),
    }
}

fn count(rs: &mushroomdb::ResultSet, col: &str) -> i64 {
    match rs.get(0, col) {
        Some(Value::Int(n)) => *n,
        other => panic!("column {col}: {other:?}"),
    }
}

fn rows(rs: &mushroomdb::ResultSet) -> Rows {
    (0..rs.len()).map(|i| rs.row(i).to_vec()).collect()
}

fn read(db: &GraphDb, q: &str) -> Rows {
    rows(&db.query(q, &none()).unwrap_or_else(|e| panic!("{q}: {e}")))
}

fn write(db: &mut GraphDb, q: &str) -> mushroomdb::ResultSet {
    db.query_write(q, &none())
        .unwrap_or_else(|e| panic!("{q}: {e}"))
}

/// Every node (key, label, properties) and every edge, sorted: the whole
/// observable state of a small store.
fn state(db: &GraphDb) -> (Rows, Rows) {
    let nodes = read(
        db,
        "MATCH (n) RETURN n.key AS k, labels(n) AS l, properties(n) AS p ORDER BY k",
    );
    let edges = read(
        db,
        "MATCH (a)-[r]->(b) RETURN a.key AS a, type(r) AS t, b.key AS b ORDER BY a, t, b",
    );
    (nodes, edges)
}

/// People `a`..`d` (`v` 1..4) and a company `c1`.
fn people(name: &str) -> GraphDb {
    let mut db = GraphDb::open(&tmp(name)).unwrap();
    for q in [
        "CREATE (:Person {id: 'a', v: 1})",
        "CREATE (:Person {id: 'b', v: 2})",
        "CREATE (:Person {id: 'c', v: 3})",
        "CREATE (:Person {id: 'd', v: 4})",
        "CREATE (:Company {id: 'c1'})",
    ] {
        write(&mut db, q);
    }
    db
}

// ── MATCH … CREATE / MATCH … MERGE ─────────────────────────────────────────

#[test]
fn match_then_create_links_the_matched_nodes() {
    let mut db = people("mc-link");
    let rs = write(
        &mut db,
        "MATCH (a:Person {id: 'a'}), (b:Person {id: 'b'}) CREATE (a)-[:KNOWS]->(b)",
    );
    assert_eq!(count(&rs, "created"), 1, "one relationship created");
    assert_eq!(
        read(&db, "MATCH (x)-[:KNOWS]->(y) RETURN x.id AS x, y.id AS y"),
        vec![vec![Some(s("a")), Some(s("b"))]]
    );
}

#[test]
fn match_then_create_runs_once_per_row_with_where_set_and_return() {
    let mut db = people("mc-rows");
    let rs = write(
        &mut db,
        "MATCH (p:Person) WHERE p.v >= 3 \
         CREATE (t:Tag {id: 'tag-' + p.id, of: p.v}) SET p.tagged = true \
         RETURN t.id AS t, p.tagged AS tagged ORDER BY t",
    );
    assert_eq!(
        rows(&rs),
        vec![
            vec![Some(s("tag-c")), Some(Value::Bool(true))],
            vec![Some(s("tag-d")), Some(Value::Bool(true))],
        ]
    );
    assert_eq!(
        read(
            &db,
            "MATCH (p:Person) WHERE p.tagged RETURN p.id ORDER BY p.id"
        ),
        vec![vec![Some(s("c"))], vec![Some(s("d"))]]
    );
    // Zero matched rows: nothing is created.
    let rs = write(
        &mut db,
        "MATCH (p:Person) WHERE p.v > 100 CREATE (:Tag {id: 'never'})",
    );
    assert_eq!(count(&rs, "created"), 0);
    assert!(!db.has_node("never"));
}

#[test]
fn match_then_create_takes_the_read_clauses_of_the_unwind_form() {
    let mut db = people("mc-clauses");
    let rs = write(
        &mut db,
        "MATCH (p:Person {id: 'a'}) UNWIND ['x', 'y'] AS k CREATE (:T {id: k})-[:OF]->(p)",
    );
    assert_eq!(count(&rs, "created"), 4);
    let rs = write(
        &mut db,
        "MATCH (p:Person) WHERE p.v < 3 OPTIONAL MATCH (p)<-[:OF]-(t:T) \
         WITH p, count(t) AS n CREATE (:Count {id: 'n-' + p.id, n: n})",
    );
    assert_eq!(count(&rs, "created"), 2);
    assert_eq!(
        read(&db, "MATCH (c:Count) RETURN c.id, c.n ORDER BY c.id"),
        vec![
            vec![Some(s("n-a")), Some(Value::Int(2))],
            vec![Some(s("n-b")), Some(Value::Int(0))],
        ]
    );
    // A MERGE end bound to null (OPTIONAL MATCH found nothing) is an error,
    // and nothing is written.
    let e = db
        .query_write(
            "MATCH (p:Person {id: 'b'}) OPTIONAL MATCH (p)-[:OF]->(q) \
             MERGE (q)-[:K]->(:T {id: 'z'})",
            &none(),
        )
        .unwrap_err();
    assert!(detail(e).contains("is null in this row"));
    assert!(!db.has_node("z"));
}

#[test]
fn match_then_merge_merges_per_row_and_bound_ends_are_matched() {
    let mut db = people("mc-merge");
    // A MERGE pattern keyed from the matched row.
    let rs = write(
        &mut db,
        "MATCH (p:Person) WHERE p.v <= 2 MERGE (g:Group {id: 'g' + toString(p.v % 2)})",
    );
    assert_eq!(count(&rs, "created"), 2);
    // The form QL-12 refused: a MERGE from a node the MATCH bound.
    let q = "MATCH (a {id: 'a'}) MERGE (a)-[:K]->(b:B {id: 'y'})";
    let rs = write(&mut db, q);
    assert_eq!(count(&rs, "created"), 2, "node b and the relationship");
    let rs = write(&mut db, q);
    assert_eq!(count(&rs, "created"), 0, "the second run matches both");
    // Both ends bound; ON CREATE / ON MATCH set the bound nodes.
    let q = "MATCH (a:Person {id: 'a'}), (b:Person {id: 'b'}) \
             MERGE (a)-[:LIKES]->(b) ON CREATE SET a.n = 1 ON MATCH SET a.n = a.n + 1";
    assert_eq!(count(&write(&mut db, q), "created"), 1);
    assert_eq!(count(&write(&mut db, q), "created"), 0);
    assert_eq!(
        read(
            &db,
            "MATCH (a:Person {id: 'a'})-[:LIKES]->(b) RETURN a.n, b.id"
        ),
        vec![vec![Some(Value::Int(2)), Some(s("b"))]]
    );
}

#[test]
fn match_then_create_and_merge_are_writes_everywhere() {
    for q in [
        "MATCH (a), (b) CREATE (a)-[:K]->(b)",
        "MATCH (a) MERGE (:X {id: a.id})",
    ] {
        assert!(mushroomdb::is_write_query(q).unwrap(), "{q}");
    }
    let mut db = people("mc-readonly");
    // The read entry point does not write.
    assert!(db
        .query("MATCH (a {id: 'a'}) CREATE (:T {id: 't'})", &none())
        .is_err());
    assert!(!db.has_node("t"));
    write(&mut db, "MATCH (a {id: 'a'}) CREATE (:T {id: 't'})");
    assert!(db.has_node("t"));
}

#[test]
fn match_then_create_under_a_role_stays_inside_its_scope() {
    let mut db = GraphDb::open(&tmp("mc-role")).unwrap();
    db.apply_schema(&Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![],
        roles: vec![RoleDef {
            name: "writer".into(),
            keys: vec![],
            labels: vec!["Mine".into()],
            visible_where: None,
            namespaces: None,
            write: Some(WriteScope {
                create_labels: vec!["Mine".into()],
                update_labels: vec!["Mine".into()],
                delete_labels: vec![],
                create_edge_types: vec![],
                delete_edge_types: vec![],
            }),
        }],
    })
    .unwrap();
    db.insert_node("Mine", "m1", vec![]).unwrap();
    db.insert_node("Secret", "s1", vec![]).unwrap();
    db.query_write_authz(
        "writer",
        "MATCH (m:Mine {id: 'm1'}) CREATE (:Mine {id: 'm2'})",
        &none(),
    )
    .unwrap();
    assert!(db.has_node("m2"));
    let e = db
        .query_write_authz(
            "writer",
            "MATCH (m:Mine {id: 'm1'}) CREATE (:Secret {id: 's2'})",
            &none(),
        )
        .unwrap_err();
    assert!(matches!(e, GraphError::RoleWriteDenied { .. }), "{e:?}");
    // The hidden node is not matched, so nothing is created from it.
    let rs = db
        .query_write_authz(
            "writer",
            "MATCH (x:Secret {id: 's1'}) CREATE (:Mine {id: 'm3'})",
            &none(),
        )
        .unwrap();
    assert_eq!(count(&rs, "created"), 0);
    assert!(!db.has_node("m3"));
}

#[test]
fn match_then_create_inside_a_transaction_is_refused_by_name() {
    let db: SharedDb = SharedDb::open(&tmp("mc-tx")).unwrap();
    let mut tx = db.begin_transaction();
    tx.execute("CREATE (:P {id: 'x'})", &none()).unwrap();
    let e = tx
        .execute("MATCH (p:P {id: 'x'}) CREATE (:P {id: 'y'})", &none())
        .unwrap_err();
    assert_eq!(
        detail(e),
        "not supported in mushroomdb 0.10 inside a transaction: MATCH … CREATE / MERGE"
    );
    tx.commit().unwrap();
    assert!(db.reader().node_info("x").is_some());
    assert!(db.reader().node_info("y").is_none());
}

/// A small xorshift generator: the differential is reproducible by seed.
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

/// A random small graph, built the same way on every store it is given.
fn random_graph(rng: &mut Rng, dbs: &mut [&mut GraphDb]) {
    let n = 4 + rng.below(6);
    let mut stmts = Vec::new();
    for i in 0..n {
        let label = rng.pick(&["A", "B", "C"]);
        let v = rng.below(4);
        stmts.push(format!("CREATE (:{label} {{id: 'n{i}', v: {v}}})"));
    }
    for _ in 0..rng.below(8) {
        let (x, y) = (rng.below(n), rng.below(n));
        let t = rng.pick(&["K", "J"]);
        stmts.push(format!(
            "UNWIND [1] AS _ MATCH (x {{id: 'n{x}'}}), (y {{id: 'n{y}'}}) CREATE (x)-[:{t}]->(y)"
        ));
    }
    for db in dbs.iter_mut() {
        for q in &stmts {
            write(db, q);
        }
    }
}

/// A random `MATCH … <write>` statement body (everything after the leading
/// clause the two forms differ by).
fn random_match_write(rng: &mut Rng, step: usize) -> String {
    let l1 = rng.pick(&["A", "B", "C"]);
    let l2 = rng.pick(&["A", "B", "C"]);
    let k = rng.below(4);
    let op = rng.pick(&["=", "<", ">=", "<>"]);
    let t = rng.pick(&["K", "J", "M"]);
    let ret = rng.pick(&["", " RETURN count(*) AS c", " RETURN a.id AS a ORDER BY a"]);
    match rng.below(6) {
        0 => format!("MATCH (a:{l1}), (b:{l2}) WHERE a.v {op} {k} CREATE (a)-[:{t}]->(b){ret}"),
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
            "MATCH (a:{l1}) WHERE a.v {op} {k} SET a.v = a.v + 1 \
             CREATE (a)-[:{t}]->(:C {{id: a.id + '-t{step}'}}){ret}"
        ),
        _ => format!(
            "MATCH (a:{l1}) OPTIONAL MATCH (a)-[:K]->(b:{l2}) WITH a, b WHERE b IS NOT NULL \
             MERGE (a)-[:{t}]->(b){ret}"
        ),
    }
}

#[test]
fn match_then_write_equals_the_unwind_form_randomized() {
    // Statements that changed the store, and ones that failed (the same way on
    // both forms): the differential must see plenty of both kinds of outcome.
    let (mut changed, mut failed) = (0, 0);
    for seed in 1..=60u64 {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ seed.wrapping_mul(0x1000_0001));
        let mut top = GraphDb::open(&tmp(&format!("diff-top-{seed}"))).unwrap();
        let mut unwound = GraphDb::open(&tmp(&format!("diff-uw-{seed}"))).unwrap();
        random_graph(&mut rng, &mut [&mut top, &mut unwound]);
        for step in 0..6 {
            let q = random_match_write(&mut rng, step);
            let before = state(&top);
            let a = top.query_write(&q, &none()).map(|rs| rows(&rs));
            let b = unwound
                .query_write(&format!("UNWIND [1] AS _ {q}"), &none())
                .map(|rs| rows(&rs));
            match (&a, &b) {
                (Ok(x), Ok(y)) => assert_eq!(x, y, "seed {seed}: {q}"),
                (Err(x), Err(y)) => assert_eq!(x.to_string(), y.to_string(), "seed {seed}: {q}"),
                _ => panic!("seed {seed}: {q}: {a:?} vs {b:?}"),
            }
            let after = state(&top);
            assert_eq!(after, state(&unwound), "seed {seed}: {q}");
            changed += usize::from(after != before);
            failed += usize::from(a.is_err());
        }
    }
    assert!(changed >= 100, "only {changed} of 360 statements wrote");
    assert!(failed < 180, "{failed} of 360 statements failed");
}

// ── Row 143: a created relationship is counted ─────────────────────────────

#[test]
fn row_143_every_create_path_counts_its_relationships() {
    let mut db = people("row143");
    // The reported shape: UNWIND … MATCH … CREATE (a)-[:K]->(b).
    let rs = write(
        &mut db,
        "UNWIND [1] AS x MATCH (a:Person {id: 'a'}), (b:Person {id: 'b'}) CREATE (a)-[:K]->(b)",
    );
    assert_eq!(count(&rs, "created"), 1);
    // CREATE of two nodes and the relationship between them.
    let rs = write(&mut db, "CREATE (:N {id: 'n1'})-[:K]->(:N {id: 'n2'})");
    assert_eq!(count(&rs, "created"), 3);
    // Per-row CREATE of a node and a relationship to a bound node.
    let rs = write(
        &mut db,
        "UNWIND ['p', 'q'] AS k MATCH (a:Person {id: 'a'}) CREATE (a)-[:K]->(:N {id: k})",
    );
    assert_eq!(count(&rs, "created"), 4);
    // MERGE of a relationship: nodes and relationship created, then matched.
    let q = "MERGE (:M {id: 'm1'})-[:K]->(:M {id: 'm2'})";
    assert_eq!(count(&write(&mut db, q), "created"), 3);
    assert_eq!(count(&write(&mut db, q), "created"), 0);
    // MERGE of only the relationship between existing nodes.
    let q = "MERGE (:M {id: 'm2'})-[:J]->(:M {id: 'm1'})";
    assert_eq!(count(&write(&mut db, q), "created"), 1);
    // Per-row MERGE of a relationship.
    let q = "UNWIND ['m1', 'm2'] AS k MERGE (:M {id: k})-[:L]->(:M {id: 'm3'})";
    assert_eq!(
        count(&write(&mut db, q), "created"),
        3,
        "m3 and two relationships"
    );
    assert_eq!(count(&write(&mut db, q), "created"), 0);
}

#[test]
fn row_143_a_transaction_counts_its_relationships_too() {
    let db: SharedDb = SharedDb::open(&tmp("row143-tx")).unwrap();
    let mut tx = db.begin_transaction();
    let rs = tx
        .execute("CREATE (:N {id: 'a'})-[:K]->(:N {id: 'b'})", &none())
        .unwrap();
    assert_eq!(count(&rs, "created"), 3);
    let rs = tx
        .execute("MERGE (:N {id: 'a'})-[:J]->(:N {id: 'b'})", &none())
        .unwrap();
    assert_eq!(count(&rs, "created"), 1);
    tx.commit().unwrap();
}

// ── RETURN * / WITH * ──────────────────────────────────────────────────────

fn columns(db: &GraphDb, q: &str) -> Vec<String> {
    db.query(q, &none())
        .unwrap_or_else(|e| panic!("{q}: {e}"))
        .columns()
        .to_vec()
}

/// `RETURN *` lists node, path and value names; a relationship is not returned
/// bare in 0.10 (`RETURN r` is an error), so relationship names are left out.
#[test]
fn return_star_returns_every_bound_name_alphabetically() {
    let mut db = people("star");
    write(
        &mut db,
        "MATCH (a:Person {id: 'a'}), (b:Person {id: 'b'}) CREATE (a)-[:K]->(b)",
    );
    assert_eq!(
        columns(&db, "MATCH (p:Person)-[r:K]->(c) RETURN *"),
        ["c", "p"]
    );
    // Anonymous nodes and relationships are not names; a named path is.
    assert_eq!(
        columns(&db, "MATCH path = (z)-[:K]->()<-[]-(:Person) RETURN *"),
        ["path", "z"]
    );
    // So is a variable-length relationship list.
    assert_eq!(
        columns(&db, "MATCH (x)-[rs:K*1..2]->(y) RETURN *"),
        ["x", "y"]
    );
    // The relationship stays usable beside the star.
    let rs = db
        .query("MATCH (p)-[r:K]->(q) RETURN *, type(r) AS t", &none())
        .unwrap();
    assert_eq!(rs.columns(), ["p", "q", "t"]);
    assert_eq!(rs.get(0, "t"), Some(&s("K")));
    // UNWIND aliases and WITH aliases; the RETURN takes what the WITH keeps.
    assert_eq!(
        columns(
            &db,
            "MATCH (p:Person) UNWIND [1, 2] AS i WITH p, i * 10 AS big RETURN *"
        ),
        ["big", "p"]
    );
    let rs = db
        .query("MATCH (p:Person {id: 'a'})-[r:K]->(q) RETURN *", &none())
        .unwrap();
    assert_eq!(rs.len(), 1);
    assert_eq!(rs.get(0, "q"), Some(&s("b")));
}

#[test]
fn with_star_carries_everything_and_takes_more_items() {
    let db = people("with-star");
    let rs = db
        .query(
            "MATCH (p:Person) WHERE p.v <= 2 WITH *, p.v * 2 AS dbl \
             RETURN p.id AS id, dbl ORDER BY id",
            &none(),
        )
        .unwrap();
    assert_eq!(
        rows(&rs),
        vec![
            vec![Some(s("a")), Some(Value::Int(2))],
            vec![Some(s("b")), Some(Value::Int(4))],
        ]
    );
    assert_eq!(
        columns(&db, "MATCH (p:Person), (c:Company) WITH * RETURN *"),
        ["c", "p"]
    );
    // RETURN DISTINCT * de-duplicates whole rows.
    let rs = db
        .query(
            "MATCH (p:Person) UNWIND [1, 1, 2] AS i WITH i RETURN DISTINCT * ORDER BY i",
            &none(),
        )
        .unwrap();
    assert_eq!(
        rows(&rs),
        vec![vec![Some(Value::Int(1))], vec![Some(Value::Int(2))]]
    );
    // An UNWIND-led read and a write statement's RETURN take it too.
    assert_eq!(
        columns(&db, "UNWIND [1] AS x MATCH (c:Company) RETURN *"),
        ["c", "x"]
    );
}

#[test]
fn return_star_in_write_statements() {
    let mut db = people("star-write");
    let rs = write(
        &mut db,
        "MATCH (p:Person {id: 'a'}) CREATE (t:Tag {id: 't1'}) RETURN *",
    );
    assert_eq!(rs.columns(), ["p", "t"]);
    assert_eq!(rs.len(), 1);
    let rs = write(&mut db, "CREATE (n:Tag {id: 't2'}) RETURN *");
    assert_eq!(rs.columns(), ["n"]);
    let rs = write(&mut db, "MERGE (m:Tag {id: 't3'}) RETURN *");
    assert_eq!(rs.columns(), ["m"]);
    let rs = write(&mut db, "MATCH (q:Person {id: 'b'}) SET q.x = 1 RETURN *");
    assert_eq!(rs.columns(), ["q"]);
}

#[test]
fn star_with_nothing_in_scope_is_an_error() {
    let db = people("star-empty");
    for q in [
        "MATCH () RETURN *",
        "MATCH (:Person)-[:K]->() RETURN *",
        "MATCH ()-[r]->() RETURN *",
        "RETURN *",
    ] {
        let e = detail(db.query(q, &none()).unwrap_err());
        assert!(
            e.contains("RETURN * / WITH * with no variables in scope"),
            "{q}: {e}"
        );
    }
    // A star comes first.
    let e = detail(
        db.query("MATCH (p) WITH 1 AS one RETURN one, *", &none())
            .unwrap_err(),
    );
    assert!(e.starts_with("parse:"), "{e}");
    // A name a WITH keeps is in scope for a later star.
    assert_eq!(
        columns(&db, "MATCH (p) WITH 1 AS one MATCH () WITH * RETURN one"),
        ["one"]
    );
}

// ── Label predicates ────────────────────────────────────────────────────────

#[test]
fn a_label_predicate_filters_and_projects_three_valued() {
    let mut db = people("label");
    write(
        &mut db,
        "MATCH (a:Person {id: 'a'}), (c:Company {id: 'c1'}) CREATE (a)-[:WORKS_AT]->(c)",
    );
    assert_eq!(
        read(&db, "MATCH (n) WHERE n:Company RETURN n.id"),
        vec![vec![Some(s("c1"))]]
    );
    assert_eq!(
        read(&db, "MATCH (n) WHERE NOT n:Person RETURN n.id"),
        vec![vec![Some(s("c1"))]]
    );
    assert_eq!(
        read(
            &db,
            "MATCH (n) WHERE n:Company OR n.v = 4 RETURN n.id ORDER BY n.id"
        ),
        vec![vec![Some(s("c1"))], vec![Some(s("d"))]]
    );
    // In a projection: true, false, or null for a null node.
    let rs = db
        .query(
            "MATCH (p:Person) OPTIONAL MATCH (p)-[:WORKS_AT]->(c) \
             RETURN p.id AS id, p:Person AS person, c:Company AS employed ORDER BY id",
            &none(),
        )
        .unwrap();
    assert_eq!(rs.get(0, "person"), Some(&Value::Bool(true)));
    assert_eq!(rs.get(0, "employed"), Some(&Value::Bool(true)));
    assert_eq!(rs.get(1, "employed"), None, "null node -> null");
    // A null node never passes a label test, nor its negation.
    assert_eq!(
        read(
            &db,
            "MATCH (p:Person {id: 'b'}) OPTIONAL MATCH (p)-[:WORKS_AT]->(c) \
             WITH p, c WHERE NOT c:Company RETURN p.id"
        ),
        Rows::new()
    );
    // In CASE, a list comprehension and a WITH filter.
    assert_eq!(
        read(
            &db,
            "MATCH (n) WITH n, CASE WHEN n:Person THEN 'p' ELSE 'o' END AS k \
             WHERE k = 'o' RETURN n.id"
        ),
        vec![vec![Some(s("c1"))]]
    );
    // On a relationship it tests the type.
    assert_eq!(
        read(
            &db,
            "MATCH (a)-[r]->(b) WHERE r:WORKS_AT RETURN a.id, r:KNOWS AS k"
        ),
        vec![vec![Some(s("a")), Some(Value::Bool(false))]]
    );
}

#[test]
fn a_stored_label_property_does_not_fool_the_label_test() {
    let mut db = GraphDb::open(&tmp("label-prop")).unwrap();
    write(&mut db, "CREATE (:Person {id: 'x', label: 'Company'})");
    assert_eq!(
        read(&db, "MATCH (n) WHERE n:Company RETURN n.id"),
        Rows::new()
    );
    assert_eq!(
        read(&db, "MATCH (n) WHERE n:Person RETURN n.id"),
        vec![vec![Some(s("x"))]]
    );
}

#[test]
fn more_than_one_label_is_refused_by_name() {
    let db = people("label-multi");
    for q in [
        "MATCH (n) WHERE n:Person:Company RETURN n",
        "MATCH (n) WHERE n:Person|Company RETURN n",
        "MATCH (n) RETURN n:A:B AS x",
    ] {
        let e = detail(db.query(q, &none()).unwrap_err());
        assert!(
            e.contains("not supported in mushroomdb 0.10: more than one label"),
            "{q}: {e}"
        );
    }
    // A label test on a value that is not a node or a relationship names it.
    let e = detail(
        db.query("UNWIND [1] AS x RETURN x:Person AS p", &none())
            .unwrap_err(),
    );
    assert!(e.contains("x:Person"), "{e}");
}

#[test]
fn a_label_test_is_never_pushed_into_an_index_or_a_fused_scan() {
    let mut db = GraphDb::open(&tmp("label-index")).unwrap();
    for i in 0..40 {
        let label = if i % 3 == 0 { "A" } else { "B" };
        db.insert_node(
            label,
            &format!("n{i}"),
            vec![("v".into(), Value::Int(i % 7))],
        )
        .unwrap();
    }
    write(&mut db, "CREATE INDEX FOR (n:A) ON (n.v)");
    write(&mut db, "CREATE INDEX FOR (n:B) ON (n.v)");
    db.snapshot().unwrap();
    for i in 40..60 {
        db.insert_node("A", &format!("n{i}"), vec![("v".into(), Value::Int(i % 7))])
            .unwrap();
    }
    for (with_label, reference) in [
        (
            "MATCH (n:A) WHERE n.v > 2 AND n:A RETURN n.key AS k ORDER BY k",
            "MATCH (n:A) WHERE n.v > 2 RETURN n.key AS k ORDER BY k",
        ),
        (
            "MATCH (n:A) WHERE n.v > 2 AND n:B RETURN n.key AS k ORDER BY k",
            "MATCH (n:A) WHERE n.v > 2 AND false RETURN n.key AS k ORDER BY k",
        ),
        (
            "MATCH (n) WHERE n.v = 3 AND n:B RETURN n.key AS k ORDER BY k",
            "MATCH (n:B) WHERE n.v = 3 RETURN n.key AS k ORDER BY k",
        ),
        (
            "MATCH (n:A) WHERE n:A AND n.v <> 3 RETURN n.key AS k LIMIT 1000",
            "MATCH (n:A) WHERE n.v <> 3 RETURN n.key AS k LIMIT 1000",
        ),
        (
            "MATCH (n) WHERE n:B AND n.v <> 3 RETURN n.key AS k LIMIT 1000",
            "MATCH (n:B) WHERE n.v <> 3 RETURN n.key AS k LIMIT 1000",
        ),
    ] {
        let mut got = read(&db, with_label);
        let mut want = read(&db, reference);
        got.sort_by_key(|r| format!("{r:?}"));
        want.sort_by_key(|r| format!("{r:?}"));
        assert_eq!(got, want, "{with_label}");
    }
}

#[test]
fn a_label_predicate_equals_labels_randomized() {
    for seed in 1..=40u64 {
        let mut rng = Rng(0xD1B5_4A32_D192_ED03 ^ seed.wrapping_mul(0x2545_F491));
        let mut db = GraphDb::open(&tmp(&format!("label-diff-{seed}"))).unwrap();
        random_graph(&mut rng, &mut [&mut db]);
        for _ in 0..8 {
            let l = rng.pick(&["A", "B", "C", "D"]);
            let m = rng.pick(&["A", "B", "C"]);
            let k = rng.below(4);
            let neg = rng.pick(&["", "NOT "]);
            let conn = rng.pick(&["AND", "OR", "XOR"]);
            let (with_label, reference) = match rng.below(4) {
                0 => (
                    format!("MATCH (n) WHERE {neg}n:{l} {conn} n.v = {k} RETURN n.key AS k"),
                    format!(
                        "MATCH (n) WHERE {neg}('{l}' IN labels(n)) {conn} n.v = {k} RETURN n.key AS k"
                    ),
                ),
                1 => (
                    format!("MATCH (n:{m}) RETURN n.key AS k, n:{l} AS t"),
                    format!("MATCH (n:{m}) RETURN n.key AS k, '{l}' IN labels(n) AS t"),
                ),
                2 => (
                    format!(
                        "MATCH (n) OPTIONAL MATCH (n)-[:K]->(o) \
                         RETURN n.key AS k, {neg}o:{l} AS t, o.key AS ok"
                    ),
                    format!(
                        "MATCH (n) OPTIONAL MATCH (n)-[:K]->(o) \
                         RETURN n.key AS k, {neg}('{l}' IN labels(o)) AS t, o.key AS ok"
                    ),
                ),
                _ => (
                    format!(
                        "MATCH (n)-[r]->(o) WHERE {neg}r:K {conn} o:{l} \
                         RETURN n.key AS k, o.key AS ok, type(r) AS t"
                    ),
                    format!(
                        "MATCH (n)-[r]->(o) WHERE {neg}(type(r) = 'K') {conn} ('{l}' IN labels(o)) \
                         RETURN n.key AS k, o.key AS ok, type(r) AS t"
                    ),
                ),
            };
            let mut got = read(&db, &with_label);
            let mut want = read(&db, &reference);
            got.sort_by_key(|r| format!("{r:?}"));
            want.sort_by_key(|r| format!("{r:?}"));
            assert_eq!(got, want, "seed {seed}: {with_label}");
        }
    }
}

// ── A leading OPTIONAL MATCH ───────────────────────────────────────────────

#[test]
fn a_leading_optional_match_gives_one_row_of_nulls_when_nothing_matches() {
    let db = people("lead-opt");
    let rs = db
        .query("OPTIONAL MATCH (n:Nobody) RETURN n, n.id AS id", &none())
        .unwrap();
    assert_eq!(rs.columns(), ["n", "id"]);
    assert_eq!(rows(&rs), vec![vec![None, None]]);
    assert_eq!(
        read(
            &db,
            "OPTIONAL MATCH (p:Person) WHERE p.v > 2 RETURN p.id AS id ORDER BY id"
        ),
        vec![vec![Some(s("c"))], vec![Some(s("d"))]]
    );
    assert_eq!(
        read(&db, "OPTIONAL MATCH (n:Nobody) RETURN count(n) AS c"),
        vec![vec![Some(Value::Int(0))]]
    );
    // Then more clauses; and RETURN * shows only the user's names.
    assert_eq!(
        read(
            &db,
            "OPTIONAL MATCH (c:Company) WITH c MATCH (p:Person {id: 'a'}) RETURN c.id, p.id"
        ),
        vec![vec![Some(s("c1")), Some(s("a"))]]
    );
    assert_eq!(columns(&db, "OPTIONAL MATCH (x:Nobody) RETURN *"), ["x"]);
}
