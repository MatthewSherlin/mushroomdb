//! QL-PM: `CREATE` and `MERGE` take parameters and expressions in their
//! property maps — `CREATE (:P {id: $id})`, `CREATE (n:P $props)`,
//! `MERGE (n:P {id: $id}) ON CREATE SET n += $props` — auto-commit and inside a
//! transaction, with the store left exactly as the same statement with the
//! values written as literals leaves it.

use mushroomdb::{GraphError, SharedDb, Value, WriteStats};
use std::collections::BTreeMap;
use std::path::PathBuf;

type Rows = Vec<Vec<Option<Value>>>;

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "graphdb-qlpm-{name}-{}-{nanos}",
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

fn params(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

fn map(pairs: &[(&str, Value)]) -> Value {
    Value::Map(
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
    )
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

fn detail(e: GraphError) -> String {
    match e {
        GraphError::QueryError { detail } => detail,
        other => format!("{other}"),
    }
}

fn read(db: &SharedDb, q: &str) -> Rows {
    rows(&db.reader().query(q, &none()).unwrap())
}

/// Every node (key, label, properties) and every edge, sorted.
fn state(db: &SharedDb) -> (Rows, Rows) {
    (
        read(
            db,
            "MATCH (n) RETURN n.key AS k, labels(n) AS l, properties(n) AS p ORDER BY k",
        ),
        read(
            db,
            "MATCH (a)-[r]->(b) RETURN a.key AS a, type(r) AS t, b.key AS b ORDER BY a, t, b",
        ),
    )
}

fn open(name: &str) -> SharedDb {
    SharedDb::open(&tmp(name)).unwrap()
}

fn write(db: &SharedDb, q: &str, p: &BTreeMap<String, Value>) -> mushroomdb::ResultSet {
    db.write()
        .query_write(q, p)
        .unwrap_or_else(|e| panic!("{q}: {e}"))
}

fn write_err(db: &SharedDb, q: &str, p: &BTreeMap<String, Value>) -> String {
    detail(
        db.write()
            .query_write(q, p)
            .expect_err(&format!("{q} must fail")),
    )
}

/// A statement with its values as `$parameters`, and the same statement with
/// the values written as literals.
struct Pair {
    with_params: &'static str,
    params: BTreeMap<String, Value>,
    literal: &'static str,
}

fn pairs() -> Vec<Pair> {
    vec![
        Pair {
            with_params: "CREATE (:P {id: $id, v: $v, f: $f, ok: $ok, tags: $tags, at: $at})",
            params: params(&[
                ("id", s("a")),
                ("v", Value::Int(7)),
                ("f", Value::Float(1.5)),
                ("ok", Value::Bool(true)),
                ("tags", Value::List(vec![s("x"), s("y")])),
                // A V11 typed parameter: 2024-01-15.
                ("at", Value::Date(19737)),
            ]),
            literal: "CREATE (:P {id: 'a', v: 7, f: 1.5, ok: true, tags: ['x', 'y'], \
                      at: date('2024-01-15')})",
        },
        Pair {
            with_params: "CREATE (a:P {id: $p.id, name: toUpper($p.name), n: $n + 1, \
                          l: [$n, 2], m: {k: $n}})-[:T]->(:Q {id: $q})",
            params: params(&[
                ("p", map(&[("id", s("b")), ("name", s("bee"))])),
                ("n", Value::Int(4)),
                ("q", s("q1")),
            ]),
            literal: "CREATE (a:P {id: 'b', name: 'BEE', n: 5, l: [4, 2], m: {k: 4}})\
                      -[:T]->(:Q {id: 'q1'})",
        },
        Pair {
            with_params: "MERGE (n:P {id: $id}) ON CREATE SET n.v = $v ON MATCH SET n.hits = $h",
            params: params(&[("id", s("a")), ("v", Value::Int(1)), ("h", Value::Int(2))]),
            literal: "MERGE (n:P {id: 'a'}) ON CREATE SET n.v = 1 ON MATCH SET n.hits = 2",
        },
        Pair {
            with_params: "MERGE (n:P {id: $id}) ON CREATE SET n.v = $v ON MATCH SET n.hits = $h",
            params: params(&[("id", s("c")), ("v", Value::Int(1)), ("h", Value::Int(2))]),
            literal: "MERGE (n:P {id: 'c'}) ON CREATE SET n.v = 1 ON MATCH SET n.hits = 2",
        },
        Pair {
            with_params: "MERGE (a:P {id: $a})-[:K]->(b:Q {id: $b, ns: $ns}) \
                          ON CREATE SET b.made = $at",
            params: params(&[
                ("a", s("a")),
                ("b", s("q2")),
                ("ns", s("default")),
                ("at", Value::Int(5)),
            ]),
            literal: "MERGE (a:P {id: 'a'})-[:K]->(b:Q {id: 'q2', ns: 'default'}) \
                      ON CREATE SET b.made = 5",
        },
        Pair {
            with_params: "MERGE (n:P {name: $name})",
            params: params(&[("name", s("named"))]),
            literal: "MERGE (n:P {name: 'named'})",
        },
    ]
}

/// Results with their values, the write row and the driver counters
/// (`properties_set` included: the per-row form a parameter takes and the
/// plain form a literal takes count `ON CREATE` / `ON MATCH` items alike).
fn result(rs: &mushroomdb::ResultSet) -> (Vec<String>, Rows, Option<WriteStats>) {
    (rs.columns().to_vec(), rows(rs), rs.stats().copied())
}

// ── The differential: parameters vs literals ────────────────────────────────

#[test]
fn parameters_leave_the_store_the_literal_form_leaves_auto_commit() {
    let by_params = open("diff-params");
    let by_literals = open("diff-literals");
    for pair in pairs() {
        let q = pair.with_params;
        let a = write(&by_params, q, &pair.params);
        let b = write(&by_literals, pair.literal, &none());
        assert!(a.stats().is_some(), "{q}: no driver counters");
        assert_eq!(result(&a), result(&b), "{q}");
    }
    // Every statement a second time: the CREATEs fail alike, the MERGEs match.
    for pair in pairs() {
        let q = pair.with_params;
        let a = by_params.write().query_write(q, &pair.params);
        let b = by_literals.write().query_write(pair.literal, &none());
        match (a, b) {
            (Ok(a), Ok(b)) => assert_eq!(result(&a), result(&b), "{q}"),
            (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string()),
            (a, b) => panic!("{}: {a:?} vs {b:?}", pair.with_params),
        }
    }
    assert_eq!(state(&by_params), state(&by_literals));
    let (nodes, edges) = state(&by_params);
    assert_eq!(nodes.len(), 6, "{nodes:?}");
    assert_eq!(edges.len(), 2, "{edges:?}");
}

#[test]
fn parameters_inside_a_transaction_leave_the_store_auto_commit_leaves() {
    let auto = open("tx-auto");
    let in_tx = open("tx-tx");
    let mut tx = in_tx.begin_transaction();
    for pair in pairs() {
        let a = write(&auto, pair.with_params, &pair.params);
        let b = tx
            .execute(pair.with_params, &pair.params)
            .unwrap_or_else(|e| panic!("{}: {e}", pair.with_params));
        assert_eq!(result(&a), result(&b), "{}", pair.with_params);
    }
    // Nothing is visible outside before COMMIT.
    assert!(state(&in_tx).0.is_empty());
    tx.commit().unwrap();
    assert_eq!(state(&auto), state(&in_tx));
}

#[test]
fn a_parameterised_create_survives_a_reopen() {
    let dir = tmp("reopen");
    let db = SharedDb::open(&dir).unwrap();
    for pair in pairs() {
        write(&db, pair.with_params, &pair.params);
    }
    let before = state(&db);
    drop(db);
    let db = SharedDb::open(&dir).unwrap();
    assert_eq!(state(&db), before);
}

// ── Values ──────────────────────────────────────────────────────────────────

#[test]
fn a_typed_parameter_is_stored_as_its_type() {
    let db = open("typed");
    write(
        &db,
        "CREATE (:E {id: $id, on: $on})",
        &params(&[("id", s("e")), ("on", Value::Date(19737))]),
    );
    assert_eq!(
        read(&db, "MATCH (e:E) RETURN e.on AS on"),
        vec![vec![Some(Value::Date(19737))]]
    );
    // And it matches an equality on the same date.
    assert_eq!(
        read(
            &db,
            "MATCH (e:E) WHERE e.on = date('2024-01-15') RETURN e.id AS id"
        ),
        vec![vec![Some(s("e"))]]
    );
}

#[test]
fn a_parameter_field_reads_the_map_entry() {
    let db = open("param-field");
    let p = params(&[(
        "p",
        map(&[("id", s("a")), ("in", map(&[("b", Value::Int(2))]))]),
    )]);
    let rs = db
        .reader()
        .query(
            "RETURN $p.id AS id, $p.missing AS gone, $p.in.b AS b, $p['id'] AS sub",
            &p,
        )
        .unwrap();
    assert_eq!(
        rows(&rs),
        vec![vec![Some(s("a")), None, Some(Value::Int(2)), Some(s("a"))]]
    );
}

#[test]
fn a_null_parameter_value_is_an_absent_property() {
    let db = open("null");
    write(
        &db,
        "CREATE (:P {id: $id, v: $v})",
        &params(&[("id", s("a")), ("v", Value::Null)]),
    );
    assert_eq!(
        read(&db, "MATCH (n:P) RETURN properties(n) AS p"),
        vec![vec![Some(map(&[("id", s("a"))]))]]
    );
}

#[test]
fn a_missing_parameter_is_named() {
    let db = open("missing");
    let e = write_err(&db, "CREATE (:P {id: $id})", &none());
    assert!(e.contains("id"), "{e}");
    assert!(e.contains("parameter"), "{e}");
    assert!(state(&db).0.is_empty());
}

#[test]
fn create_returns_what_it_created() {
    let db = open("return");
    let rs = write(
        &db,
        "CREATE (n:P {id: $id, v: $v}) RETURN n.id AS id, n.v AS v",
        &params(&[("id", s("a")), ("v", Value::Int(3))]),
    );
    assert_eq!(rows(&rs), vec![vec![Some(s("a")), Some(Value::Int(3))]]);
}

#[test]
fn a_variable_bound_earlier_feeds_a_property_with_a_parameter() {
    let db = open("bound");
    write(&db, "CREATE (:P {id: 'a', v: 2})", &none());
    write(
        &db,
        "MATCH (a:P {id: 'a'}) CREATE (:Q {id: $id, from: a.id, v: a.v * $k})",
        &params(&[("id", s("q")), ("k", Value::Int(5))]),
    );
    assert_eq!(
        read(&db, "MATCH (q:Q) RETURN q.from AS f, q.v AS v"),
        vec![vec![Some(s("a")), Some(Value::Int(10))]]
    );
}

// ── The whole-map forms ─────────────────────────────────────────────────────

#[test]
fn create_takes_a_whole_map_parameter() {
    let db = open("map");
    let props = map(&[
        ("id", s("a")),
        ("v", Value::Int(1)),
        ("gone", Value::Null),
        ("at", Value::Date(19737)),
    ]);
    let rs = write(
        &db,
        "CREATE (n:P $props) RETURN n.id AS id",
        &params(&[("props", props)]),
    );
    assert_eq!(rows(&rs), vec![vec![Some(s("a"))]]);
    // A null value in the map is not stored.
    assert_eq!(
        read(&db, "MATCH (n:P) RETURN properties(n) AS p"),
        vec![vec![Some(map(&[
            ("at", Value::Date(19737)),
            ("id", s("a")),
            ("v", Value::Int(1)),
        ]))]]
    );
    // The same as the literal map.
    let lit = open("map-literal");
    write(
        &lit,
        "CREATE (n:P {id: 'a', v: 1, at: date('2024-01-15')}) RETURN n.id AS id",
        &none(),
    );
    assert_eq!(state(&db), state(&lit));
}

#[test]
fn create_takes_a_whole_map_per_row() {
    let db = open("map-rows");
    let rows_param = Value::List(vec![
        map(&[("id", s("a")), ("v", Value::Int(1))]),
        map(&[("id", s("b")), ("v", Value::Int(2))]),
    ]);
    // Two rows, one key: the second CREATE of `x` fails the whole statement.
    let e = write_err(
        &db,
        "UNWIND $rows AS r CREATE (n:P $props) SET n.v = r.v",
        &params(&[("rows", rows_param), ("props", map(&[("id", s("x"))]))]),
    );
    assert!(e.contains('x'), "{e}");
    assert!(state(&db).0.is_empty());
    write(
        &db,
        "CREATE (n:P $props)",
        &params(&[("props", map(&[("id", s("x")), ("v", Value::Int(9))]))]),
    );
    assert_eq!(
        read(&db, "MATCH (n:P) RETURN n.id AS id, n.v AS v"),
        vec![vec![Some(s("x")), Some(Value::Int(9))]]
    );
}

#[test]
fn a_whole_map_parameter_that_is_not_a_map_is_named() {
    let db = open("map-not");
    let e = write_err(&db, "CREATE (n:P $props)", &params(&[("props", s("a"))]));
    assert!(e.contains("props"), "{e}");
    assert!(e.contains("map"), "{e}");
    let e = write_err(
        &db,
        "CREATE (n:P $props)",
        &params(&[("props", map(&[("v", Value::Int(1))]))]),
    );
    assert!(e.contains("'id'"), "{e}");
    assert!(state(&db).0.is_empty());
}

#[test]
fn merge_on_create_set_takes_a_map_parameter() {
    let db = open("merge-map");
    let q = "MERGE (n:P {id: $id}) ON CREATE SET n += $props ON MATCH SET n += $more \
             RETURN n.v AS v, n.w AS w, n.x AS x";
    let p = params(&[
        ("id", s("a")),
        ("props", map(&[("v", Value::Int(1)), ("x", Value::Int(5))])),
        ("more", map(&[("w", Value::Int(2)), ("x", Value::Null)])),
    ]);
    let rs = write(&db, q, &p);
    assert_eq!(
        rows(&rs),
        vec![vec![Some(Value::Int(1)), None, Some(Value::Int(5))]]
    );
    // Matched: `+=` sets `w` and a null value removes `x`.
    let rs = write(&db, q, &p);
    assert_eq!(
        rows(&rs),
        vec![vec![Some(Value::Int(1)), Some(Value::Int(2)), None]]
    );
    // The literal map gives the same store.
    let lit = open("merge-map-literal");
    let lq = "MERGE (n:P {id: 'a'}) ON CREATE SET n += {v: 1, x: 5} \
              ON MATCH SET n += {w: 2, x: null} RETURN n.v AS v, n.w AS w, n.x AS x";
    write(&lit, lq, &none());
    write(&lit, lq, &none());
    assert_eq!(state(&db), state(&lit));
}

/// Ledger row 150 on the path a parameter takes: a parameterised `MERGE`
/// counts its `ON CREATE SET` / `ON MATCH SET` items in the write row, as the
/// literal form does since TX-NS, auto-commit and inside a transaction.
#[test]
fn a_parameterised_merge_counts_its_set_items_as_the_literal_form_does() {
    let statements = [
        (
            "MERGE (n:P {id: $id}) ON CREATE SET n.x = $x, n.y = $x",
            "MERGE (n:P {id: 'a'}) ON CREATE SET n.x = 1, n.y = 1",
            2,
        ),
        (
            "MERGE (n:P {id: $id}) ON CREATE SET n.x = $x ON MATCH SET n.z = $x",
            "MERGE (n:P {id: 'a'}) ON CREATE SET n.x = 1 ON MATCH SET n.z = 1",
            1,
        ),
        ("MERGE (n:P {id: $id})", "MERGE (n:P {id: 'a'})", 0),
        (
            "MERGE (a:P {id: $id})-[:K]->(b:P {id: $b}) ON CREATE SET b.v = $x, a.w = $x",
            "MERGE (a:P {id: 'a'})-[:K]->(b:P {id: 'b'}) ON CREATE SET b.v = 1, a.w = 1",
            2,
        ),
        (
            "MERGE (a:P {id: $id})-[:K]->(b:P {id: $b}) ON MATCH SET b.v = $x",
            "MERGE (a:P {id: 'a'})-[:K]->(b:P {id: 'b'}) ON MATCH SET b.v = 1",
            1,
        ),
    ];
    let p = params(&[("id", s("a")), ("b", s("b")), ("x", Value::Int(1))]);
    let by_params = open("row150-params");
    let by_literals = open("row150-literals");
    let in_tx = open("row150-params-tx");
    let mut tx = in_tx.begin_transaction();
    for (q, literal, want) in statements {
        let a = write(&by_params, q, &p);
        let b = write(&by_literals, literal, &none());
        let t = tx.execute(q, &p).unwrap_or_else(|e| panic!("{q}: {e}"));
        assert_eq!(count(&a, "properties_set"), want, "auto-commit: {q}");
        assert_eq!(count(&t, "properties_set"), want, "in a transaction: {q}");
        assert_eq!(result(&a), result(&b), "{q}");
        assert_eq!(result(&a), result(&t), "{q}");
    }
    tx.commit().unwrap();
    assert_eq!(state(&by_params), state(&by_literals));
    assert_eq!(state(&by_params), state(&in_tx));
}

#[test]
fn merge_set_map_replaces_with_equals_and_keeps_the_key() {
    let db = open("merge-map-eq");
    write(&db, "CREATE (:P {id: 'a', v: 1, w: 2})", &none());
    let rs = write(
        &db,
        "MERGE (n:P {id: $id}) ON MATCH SET n = $props",
        &params(&[("id", s("a")), ("props", map(&[("z", Value::Int(3))]))]),
    );
    assert_eq!(count(&rs, "properties_set"), 3);
    assert_eq!(
        read(&db, "MATCH (n:P) RETURN properties(n) AS p"),
        vec![vec![Some(map(&[("id", s("a")), ("z", Value::Int(3))]))]]
    );
}

#[test]
fn merge_set_map_with_a_value_that_is_not_a_map_is_named() {
    let db = open("merge-map-not");
    let e = write_err(
        &db,
        "MERGE (n:P {id: 'a'}) ON CREATE SET n += $props",
        &params(&[("props", Value::Int(1))]),
    );
    assert!(e.contains("map"), "{e}");
    assert!(state(&db).0.is_empty());
}

#[test]
fn merge_with_a_relationship_takes_a_map_set_on_either_end() {
    let db = open("merge-edge-map");
    write(
        &db,
        "MERGE (a:P {id: $a})-[:K]->(b:Q {id: $b}) ON CREATE SET b += $props, a.n = 1",
        &params(&[
            ("a", s("a")),
            ("b", s("b")),
            ("props", map(&[("v", Value::Int(4))])),
        ]),
    );
    assert_eq!(
        read(&db, "MATCH (a:P)-[:K]->(b:Q) RETURN a.n AS n, b.v AS v"),
        vec![vec![Some(Value::Int(1)), Some(Value::Int(4))]]
    );
}

// ── MERGE matching ──────────────────────────────────────────────────────────

#[test]
fn a_null_merge_key_parameter_is_an_error() {
    let db = open("merge-null");
    let e = write_err(
        &db,
        "MERGE (n:P {id: $id})",
        &params(&[("id", Value::Null)]),
    );
    assert!(e.contains("null"), "{e}");
    assert!(state(&db).0.is_empty());
}

#[test]
fn a_parameter_map_in_a_merge_pattern_is_refused_by_name() {
    let db = open("merge-param-map");
    let e = write_err(
        &db,
        "MERGE (n:P $props)",
        &params(&[("props", map(&[("id", s("a"))]))]),
    );
    assert!(e.contains("not supported in mushroomdb 0.10"), "{e}");
    assert!(e.contains("MERGE"), "{e}");
}

#[test]
fn merge_on_a_parameter_finds_a_node_created_with_a_literal() {
    let db = open("merge-finds");
    write(&db, "CREATE (:P {id: 'a', v: 1})", &none());
    let rs = write(
        &db,
        "MERGE (n:P {id: $id}) ON MATCH SET n.v = n.v + $d RETURN n.v AS v",
        &params(&[("id", s("a")), ("d", Value::Int(10))]),
    );
    assert_eq!(rows(&rs), vec![vec![Some(Value::Int(11))]]);
    assert_eq!(state(&db).0.len(), 1);
}

#[test]
fn a_unique_constraint_refuses_a_parameterised_write_as_a_literal_one() {
    let db = open("unique");
    write(
        &db,
        "CREATE CONSTRAINT uniq_email FOR (u:U) REQUIRE u.email IS UNIQUE",
        &none(),
    );
    write(&db, "CREATE (:U {id: 'u1', email: 'x@y'})", &none());
    let literal = write_err(&db, "CREATE (:U {id: 'u2', email: 'x@y'})", &none());
    let with_params = write_err(
        &db,
        "CREATE (:U {id: $id, email: $email})",
        &params(&[("id", s("u2")), ("email", s("x@y"))]),
    );
    assert_eq!(literal, with_params);
    let merged = write_err(
        &db,
        "MERGE (u:U {id: $id}) ON CREATE SET u += $props",
        &params(&[("id", s("u3")), ("props", map(&[("email", s("x@y"))]))]),
    );
    assert!(merged.contains("uniq_email"), "{merged}");
    assert_eq!(state(&db).0.len(), 1);
}

#[test]
fn an_equality_index_finds_a_parameterised_write() {
    let db = open("index");
    write(&db, "CREATE INDEX idx_v FOR (n:P) ON (n.v)", &none());
    for i in 0..5 {
        write(
            &db,
            "CREATE (:P {id: $id, v: $v})",
            &params(&[("id", s(&format!("p{i}"))), ("v", Value::Int(i))]),
        );
    }
    assert_eq!(
        read(&db, "MATCH (n:P) WHERE n.v = 3 RETURN n.id AS id"),
        vec![vec![Some(s("p3"))]]
    );
}

// ── Literal refusals keep their meaning, without the "literals only" text ──

#[test]
fn no_error_says_literals_only() {
    let db = open("no-literals-only");
    for q in [
        "CREATE (:L {id: 'c', p: null})",
        "CREATE (:L {id: 'c', p: x})",
        "MERGE (n:L {id: x})",
    ] {
        let e = write_err(&db, q, &none());
        assert!(!e.contains("literals only"), "{q}: {e}");
    }
}
