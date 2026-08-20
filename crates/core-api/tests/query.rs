use core_api::{GraphDb, GraphError, IngestOptions, Predicate, ResultSet, RuleDef, Value};
use std::collections::BTreeMap;

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-query-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn tags(items: &[&str]) -> Value {
    Value::List(items.iter().map(|s| Value::Str((*s).into())).collect())
}

fn overlap(name: &str, field: &str, edge_type: &str) -> RuleDef {
    RuleDef {
        name: name.into(),
        src_label: "Org".into(),
        dst_label: "Person".into(),
        predicate: Predicate::Overlap {
            field: field.into(),
            min: 0.2,
        },
        edge_type: edge_type.into(),
        weight_prop: Some("score".into()),
        max_edges: None,
        approximate: false,
    }
}

/// Generic two-label graph. Two scored Overlap rules (Org → Person) so a
/// dogfood-shaped 3-MATCH can filter on two rule-derived `score` props.
///
/// Jaccard (hand): inter/union of list tokens. t1 industries {a,b,c,d},
/// specialties {w,x,y,z}.
///
/// | org     | industries | ind | specialties | spec | both ≥ 0.5? |
/// |---------|------------|-----|-------------|------|-------------|
/// | acme    | a,b,c,d    | 1.0 | w,x,y,z     | 1.0  | yes         |
/// | zeta    | a,b,c      | .75 | w,x,y,z     | 1.0  | yes         |
/// | beta    | a,b,c      | .75 | w,x,y       | .75  | yes         |
/// | echo    | a,b        | .50 | w,x         | .50  | yes         |
/// | gamma   | a          | .25 | w,x,y,z     | 1.0  | no (ind)    |
/// | delta   | a,b,c,d    | 1.0 | w           | .25  | no (spec)   |
/// | foxtrot | a,b,c,d    | 1.0 | q,r         | —    | no (no edge)|
fn open_fixture(name: &str) -> GraphDb<core_storage::fs::RealFs> {
    let mut db = GraphDb::open(&tmp(name)).unwrap();
    db.insert_node(
        "Person",
        "t1",
        vec![
            ("id".into(), Value::Str("t1".into())),
            ("industries".into(), tags(&["a", "b", "c", "d"])),
            ("specialties".into(), tags(&["w", "x", "y", "z"])),
        ],
    )
    .unwrap();
    db.insert_node("Person", "t2", vec![("id".into(), Value::Str("t2".into()))])
        .unwrap();
    for (key, industries, specialties) in [
        ("acme", &["a", "b", "c", "d"][..], &["w", "x", "y", "z"][..]),
        ("zeta", &["a", "b", "c"], &["w", "x", "y", "z"]),
        ("beta", &["a", "b", "c"], &["w", "x", "y"]),
        ("echo", &["a", "b"], &["w", "x"]),
        ("gamma", &["a"], &["w", "x", "y", "z"]),
        ("delta", &["a", "b", "c", "d"], &["w"]),
        ("foxtrot", &["a", "b", "c", "d"], &["q", "r"]),
    ] {
        db.insert_node(
            "Org",
            key,
            vec![
                ("industries".into(), tags(industries)),
                ("specialties".into(), tags(specialties)),
            ],
        )
        .unwrap();
    }
    db.create_rule(overlap("industry_align", "industries", "INDUSTRY"))
        .unwrap();
    db.create_rule(overlap("specialty_match", "specialties", "SPECIALTY"))
        .unwrap();
    db.insert_edge("KNOWS", "t1", "t2").unwrap();
    db
}

fn tid(id: &str) -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("tid".into(), Value::Str(id.into()));
    p
}

fn f(x: f64) -> Value {
    Value::Float(x)
}

fn s(x: &str) -> Value {
    Value::Str(x.into())
}

fn rows_of(rs: &ResultSet) -> Vec<Vec<Option<Value>>> {
    (0..rs.len()).map(|i| rs.row(i).to_vec()).collect()
}

#[test]
fn dogfood_shaped_scored_query_orders_by_rule_weights() {
    let db = open_fixture("dogfood");
    let rs = db
        .query(
            "\
MATCH (t:Person {id: $tid})
MATCH (c:Org)-[i:INDUSTRY]->(t)
MATCH (c)-[s:SPECIALTY]->(t)
WHERE i.score >= 0.5 AND s.score >= 0.5
RETURN c, i.score AS industry, s.score AS specialty
ORDER BY industry DESC, specialty DESC
LIMIT 10",
            &tid("t1"),
        )
        .expect("dogfood-shaped query");
    assert_eq!(
        rs.columns(),
        &[
            "c".to_string(),
            "industry".to_string(),
            "specialty".to_string()
        ]
    );
    // industry DESC, specialty DESC. gamma (.25 ind) / delta (.25 spec) /
    // foxtrot (no SPECIALTY edge) dropped by WHERE / second MATCH.
    assert_eq!(
        rows_of(&rs),
        vec![
            vec![Some(s("acme")), Some(f(1.0)), Some(f(1.0))],
            vec![Some(s("zeta")), Some(f(0.75)), Some(f(1.0))],
            vec![Some(s("beta")), Some(f(0.75)), Some(f(0.75))],
            vec![Some(s("echo")), Some(f(0.5)), Some(f(0.5))],
        ]
    );
}

#[test]
fn props_map_param_query_resolves_bound_node() {
    let db = open_fixture("props-param");
    let hit = db
        .query("MATCH (t:Person {id: $tid}) RETURN t", &tid("t1"))
        .expect("props + param");
    assert_eq!(hit.columns(), &["t".to_string()]);
    assert_eq!(rows_of(&hit), vec![vec![Some(s("t1"))]]);

    let miss = db
        .query("MATCH (t:Person {id: $tid}) RETURN t", &tid("nope"))
        .expect("unknown param value is Ok(empty)");
    assert!(miss.is_empty());
}

#[test]
fn undirected_query_finds_both_orientations() {
    let db = open_fixture("undirected");
    let rs = db
        .query(
            "MATCH (a:Person)-[r:KNOWS]-(b:Person) RETURN a, b",
            &BTreeMap::new(),
        )
        .expect("undirected");
    assert_eq!(
        rows_of(&rs),
        vec![
            vec![Some(s("t1")), Some(s("t2"))],
            vec![Some(s("t2")), Some(s("t1"))],
        ]
    );
}

#[test]
fn syntax_error_is_query_error_with_detail() {
    let db = open_fixture("syntax");
    let err = db
        .query("MATCH (n)", &BTreeMap::new())
        .expect_err("missing RETURN is a query error");
    match &err {
        GraphError::QueryError { detail } => {
            assert!(
                detail.starts_with("parse:"),
                "syntax errors must be prefixed parse:, got: {detail}"
            );
            let d = detail.to_ascii_lowercase();
            assert!(
                d.contains("token") || d.contains("return"),
                "detail must name the failing token, got: {detail}"
            );
        }
        other => panic!("expected QueryError, got {other:?}"),
    }
    let shown = err.to_string();
    assert!(
        shown.to_ascii_lowercase().contains("query"),
        "Display must mention query, got: {shown}"
    );
}

#[test]
fn cypher_neighbors_match_grouped_by_edge_type() {
    let db = open_fixture("cross-check");
    let t1 = db.node_ref("t1").expect("t1");
    let grouped = t1.grouped_by_edge_type();
    let mut traversal = grouped
        .get("INDUSTRY")
        .cloned()
        .expect("INDUSTRY neighbors from traversal");
    traversal.sort();
    traversal.dedup();

    let rs = db
        .query(
            "MATCH (t:Person {id: $tid})-[r:INDUSTRY]-(n) RETURN n",
            &tid("t1"),
        )
        .expect("cypher neighbor fetch");
    let mut cypher: Vec<String> = (0..rs.len())
        .map(|i| match rs.get(i, "n") {
            Some(Value::Str(k)) => k.clone(),
            other => panic!("row {i} not a node key: {other:?}"),
        })
        .collect();
    cypher.sort();
    cypher.dedup();
    assert_eq!(traversal, cypher);
}

#[test]
fn query_stage_prefixes_lex_plan_and_execute() {
    let db = open_fixture("stage-prefix");
    match db.query("@", &BTreeMap::new()) {
        Err(GraphError::QueryError { detail }) => {
            assert!(
                detail.starts_with("lex:"),
                "lex errors must be prefixed lex:, got: {detail}"
            );
        }
        other => panic!("expected QueryError, got {other:?}"),
    }
    // Lexes and parses; planning rejects the unbound RETURN variable.
    match db.query("MATCH (a) RETURN b", &BTreeMap::new()) {
        Err(GraphError::QueryError { detail }) => {
            assert!(
                detail.starts_with("plan:"),
                "plan errors must be prefixed plan:, got: {detail}"
            );
            assert!(
                detail.contains("b") && (detail.contains("unbound") || detail.contains("Unbound")),
                "plan error must name the unbound variable, got: {detail}"
            );
        }
        other => panic!("expected QueryError, got {other:?}"),
    }
    match db.query("MATCH (t:Person {id: $tid}) RETURN t", &BTreeMap::new()) {
        Err(GraphError::QueryError { detail }) => {
            assert!(
                detail.starts_with("execute:"),
                "execute errors must be prefixed execute:, got: {detail}"
            );
            assert!(
                detail.contains("tid"),
                "missing-param execute error must name the parameter, got: {detail}"
            );
        }
        other => panic!("expected QueryError, got {other:?}"),
    }
}

/// Timing test for the INDUSTRY_ALIGNMENT two-hop harness query.
///
/// Builds a 5k-scale Talent/Company graph with FieldEqual industry rule,
/// then measures the pull-based executor's time for LIMIT 200.
///
/// This is `#[ignore]` because rule backfill takes ~2–5 s at this scale.
/// Run explicitly:
///   cargo test -p core-api --test query harness_industry_alignment_timing -- --ignored --nocapture
#[test]
#[ignore]
fn harness_industry_alignment_timing() {
    const N_TALENT: usize = 3_500;
    const N_COMPANY: usize = 1_000;
    const N_INDUSTRY: usize = 3;
    const LIMIT: usize = 200;

    let dir = tmp("ia-timing");
    let mut db = GraphDb::open(&dir).expect("open");
    let opts = IngestOptions {
        key_field: "id".into(),
        auto_fk: core_api::AutoFk::Off,
    };

    // Ingest Talent and Company nodes with an industry tag.
    let talent_rows: Vec<BTreeMap<String, Value>> = (0..N_TALENT)
        .map(|i| {
            let mut row = BTreeMap::new();
            row.insert("id".into(), Value::Str(format!("t{i:05}")));
            row.insert(
                "industry".into(),
                Value::Str(format!("ind{}", i % N_INDUSTRY)),
            );
            row
        })
        .collect();
    db.ingest("Talent", talent_rows, &opts)
        .expect("talent ingest");

    let company_rows: Vec<BTreeMap<String, Value>> = (0..N_COMPANY)
        .map(|i| {
            let mut row = BTreeMap::new();
            row.insert("id".into(), Value::Str(format!("c{i:05}")));
            row.insert(
                "industry".into(),
                Value::Str(format!("ind{}", i % N_INDUSTRY)),
            );
            row
        })
        .collect();
    db.ingest("Company", company_rows, &opts)
        .expect("company ingest");

    // IA edges: Talent → Company when industry matches (FieldEqual).
    let rule = RuleDef {
        name: "INDUSTRY_ALIGNMENT".into(),
        src_label: "Talent".into(),
        dst_label: "Company".into(),
        predicate: Predicate::FieldEqual {
            field: "industry".into(),
        },
        edge_type: "INDUSTRY_ALIGNMENT".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
    };
    let t_rule = std::time::Instant::now();
    db.create_rule(rule).expect("create IA rule");
    let backfill_ms = t_rule.elapsed().as_millis();
    println!("IA rule backfill: {backfill_ms} ms ({N_TALENT}T × {N_COMPANY}C)");

    let params = BTreeMap::new();
    let query = format!(
        "MATCH (t:Talent)-[:INDUSTRY_ALIGNMENT]->(c:Company)\
         <-[:INDUSTRY_ALIGNMENT]-(t2:Talent) RETURN t, c, t2 LIMIT {LIMIT}"
    );

    // Warm up (3 iterations).
    for _ in 0..3 {
        let rs = db.query(&query, &params).expect("warm-up query");
        assert_eq!(rs.len(), LIMIT, "warm-up: expected {LIMIT} rows");
    }

    // Measure 20 iterations.
    let mut times_us: Vec<u64> = Vec::new();
    for _ in 0..20 {
        let t0 = std::time::Instant::now();
        let rs = db.query(&query, &params).expect("timed query");
        times_us.push(t0.elapsed().as_micros() as u64);
        assert_eq!(rs.len(), LIMIT, "expected {LIMIT} rows");
    }

    times_us.sort();
    let min_us = times_us[0];
    let median_us = times_us[times_us.len() / 2];
    let p95_us = times_us[(times_us.len() as f64 * 0.95) as usize];
    println!(
        "INDUSTRY_ALIGNMENT two-hop LIMIT {LIMIT} at {N_TALENT}T+{N_COMPANY}C:\n\
         \tmin={min_us} µs  median={median_us} µs  p95={p95_us} µs"
    );

    // Sanity: pull-based must complete with no error and return exactly LIMIT rows.
    assert_eq!(times_us.len(), 20);
    assert!(min_us < 500_000, "query should complete in <500 ms");
}
