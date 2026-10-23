//! QL-LQ: `all`, `any`, `none` and `single` over lists, three-valued.
//!
//! The truth tables are the openCypher TCK's (Quantifier1-4, "lists
//! containing nulls"): a predicate answer of null makes the quantifier null
//! unless the answer is already decided.

use mushroomdb::{GraphDb, Value};
use std::collections::BTreeMap;

fn no_params() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

fn open(tag: &str) -> (GraphDb<core_storage::fs::RealFs>, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("graphdb-listpred-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node(
        "T",
        "a",
        vec![
            ("x".into(), Value::Int(2)),
            (
                "xs".into(),
                Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
            ),
        ],
    )
    .unwrap();
    db.insert_node("T", "b", vec![("x".into(), Value::Int(9))])
        .unwrap();
    (db, dir)
}

/// The value of `expr` as a one-column query; null reads as `Value::Null`.
fn eval(db: &GraphDb<core_storage::fs::RealFs>, expr: &str) -> Value {
    let rs = db
        .query(&format!("RETURN {expr} AS r"), &no_params())
        .unwrap_or_else(|e| panic!("{expr}: {e}"));
    rs.get(0, "r").cloned().unwrap_or(Value::Null)
}

/// `T`, `F` or `N` (null), one letter per quantifier: all, any, none, single.
fn answer(letter: char) -> Value {
    match letter {
        'T' => Value::Bool(true),
        'F' => Value::Bool(false),
        _ => Value::Null,
    }
}

#[test]
fn truth_tables_over_literal_lists_with_nulls() {
    let (db, dir) = open("tables");
    // (list, condition, "all any none single")
    let cases = [
        ("[]", "x", "TFTF"),
        ("[true]", "x", "TTFT"),
        ("[false]", "x", "FFTF"),
        ("[true, false]", "x", "FTFT"),
        ("[true, true]", "x", "TTFF"),
        ("[null]", "x = 2", "NNNN"),
        ("[null, null]", "x = 2", "NNNN"),
        ("[0, null]", "x = 2", "FNNN"),
        ("[2, null]", "x = 2", "NTFN"),
        ("[null, 2]", "x = 2", "NTFN"),
        ("[2, 2, null]", "x = 2", "NTFF"),
        ("[34, 0, null, 5, 900]", "x < 10", "FTFF"),
        ("[34, 10, null, 15, 900]", "x < 10", "FNNN"),
        ("[4, 0, null, -15, 9]", "x < 10", "NTFF"),
        ("[1, 2, 3]", "x < 10", "TTFF"),
        ("[1, 20, 30]", "x < 10", "FTFT"),
    ];
    for (list, cond, letters) in cases {
        for (func, letter) in ["all", "any", "none", "single"]
            .into_iter()
            .zip(letters.chars())
        {
            let q = format!("{func}(x IN {list} WHERE {cond})");
            assert_eq!(eval(&db, &q), answer(letter), "{q}");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_null_list_gives_null() {
    let (db, dir) = open("nulllist");
    for func in ["all", "any", "none", "single"] {
        assert_eq!(
            eval(&db, &format!("{func}(x IN null WHERE x > 1)")),
            Value::Null,
            "{func}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_property_list_in_where_filters_rows() {
    let (db, dir) = open("where");
    let rs = db
        .query(
            "MATCH (n:T) WHERE any(v IN n.xs WHERE v = n.x) RETURN n.id AS id",
            &no_params(),
        )
        .unwrap();
    assert_eq!(rs.len(), 1);
    assert_eq!(rs.get(0, "id"), Some(&Value::Str("a".into())));
    // `b` has no list: the predicate is null, NOT null is null, no row.
    let rs = db
        .query(
            "MATCH (n:T) WHERE NOT any(v IN n.xs WHERE v = 99) RETURN n.id AS id",
            &no_params(),
        )
        .unwrap();
    assert_eq!(rs.len(), 1);
    assert_eq!(rs.get(0, "id"), Some(&Value::Str("a".into())));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn works_in_return_with_case_function_args_and_comprehensions() {
    let (db, dir) = open("positions");
    assert_eq!(
        eval(
            &db,
            "CASE WHEN all(x IN [1, 2] WHERE x > 0) THEN 'y' ELSE 'n' END"
        ),
        Value::Str("y".into())
    );
    assert_eq!(
        eval(&db, "toString(none(x IN [1, 2] WHERE x > 5))"),
        Value::Str("true".into())
    );
    assert_eq!(
        eval(&db, "[l IN [[1], [5], []] | any(y IN l WHERE y > 2)]"),
        Value::List(vec![
            Value::Bool(false),
            Value::Bool(true),
            Value::Bool(false)
        ])
    );
    // Nested: the inner predicate reads the outer iteration variable.
    assert_eq!(
        eval(&db, "all(x IN [1, 2] WHERE any(y IN [2, 3] WHERE y > x))"),
        Value::Bool(true)
    );
    assert_eq!(
        eval(
            &db,
            "single(x IN [[1], [2, 3]] WHERE all(y IN x WHERE y > 1))"
        ),
        Value::Bool(true)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_variable_shadows_an_outer_name_inside_the_predicate_only() {
    let (db, dir) = open("shadow");
    let rs = db
        .query(
            "WITH 100 AS x RETURN all(x IN [1, 2] WHERE x < 10) AS inside, x AS outside",
            &no_params(),
        )
        .unwrap();
    assert_eq!(rs.get(0, "inside"), Some(&Value::Bool(true)));
    assert_eq!(rs.get(0, "outside"), Some(&Value::Int(100)));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_names_are_case_insensitive_and_still_usable_as_identifiers() {
    let (db, dir) = open("names");
    assert_eq!(eval(&db, "ALL(x IN [1] WHERE x = 1)"), Value::Bool(true));
    assert_eq!(eval(&db, "Single(x IN [1] WHERE x = 1)"), Value::Bool(true));
    let rs = db
        .query("WITH 3 AS any RETURN any + 1 AS r", &no_params())
        .unwrap();
    assert_eq!(rs.get(0, "r"), Some(&Value::Int(4)));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn set_value_may_be_a_list_predicate() {
    let (mut db, dir) = open("set");
    db.query_write(
        "MATCH (n:T) WHERE n.id = 'a' SET n.ok = all(v IN n.xs WHERE v > 0)",
        &no_params(),
    )
    .unwrap();
    let rs = db
        .query("MATCH (n:T {id: 'a'}) RETURN n.ok AS ok", &no_params())
        .unwrap();
    assert_eq!(rs.get(0, "ok"), Some(&Value::Bool(true)));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_list_predicate_in_a_write_return_is_refused_and_writes_nothing() {
    let (mut db, dir) = open("writeret");
    let err = db
        .query_write(
            "MATCH (n:T) WHERE n.id = 'a' SET n.y = 1 RETURN any(v IN [1] WHERE v > 0) AS r",
            &no_params(),
        )
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("a list predicate is not supported in the RETURN of a write statement"),
        "{err}"
    );
    let rs = db
        .query("MATCH (n:T {id: 'a'}) RETURN n.y AS y", &no_params())
        .unwrap();
    assert_eq!(rs.get(0, "y"), None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_quantifier_name_without_in_is_an_ordinary_function_call() {
    let (db, dir) = open("plain-call");
    for q in [
        "RETURN any(1) AS r",
        "RETURN all('x') AS r",
        "RETURN single() AS r",
    ] {
        let err = db.query(q, &no_params()).expect_err(q);
        assert!(err.to_string().contains("unknown function"), "{q}: {err}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_malformed_list_predicate_is_a_parse_error() {
    let (db, dir) = open("malformed");
    for q in [
        "RETURN all(x IN [1])",
        "RETURN any(x [1] WHERE x)",
        "RETURN none(x IN [1] WHERE)",
        "RETURN single(x IN [1] WHERE x > 0",
    ] {
        assert!(db.query(q, &no_params()).is_err(), "{q}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
