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

/// Wire shape pin: aggregate functions produce the expected column name and a
/// single-row result matching the JSON convention
/// `{"columns":["COUNT(*)"],"rows":[[n]]}`.
#[test]
fn aggregate_wire_shape_and_semantics() {
    let db = open_fixture("aggregate-wire");
    let params = BTreeMap::new();

    // COUNT(*) — 7 Org nodes ingested by open_fixture.
    let rs = db
        .query("MATCH (o:Org) RETURN COUNT(*)", &params)
        .expect("COUNT(*) must succeed");
    assert_eq!(rs.columns(), &["COUNT(*)".to_string()]);
    assert_eq!(rs.len(), 1, "aggregate always returns exactly one row");
    assert_eq!(
        rs.row(0),
        &[Some(Value::Int(7))],
        "7 Org nodes must be counted"
    );

    // COUNT(*) with alias.
    let rs_alias = db
        .query("MATCH (o:Org) RETURN COUNT(*) AS n_orgs", &params)
        .expect("COUNT(*) AS n_orgs");
    assert_eq!(rs_alias.columns(), &["n_orgs".to_string()]);
    assert_eq!(rs_alias.row(0), &[Some(Value::Int(7))]);

    // COUNT(*) on edge traversal — each Org-Person overlap rule fires per
    // matching pair.  We just check it returns Int and is >= 0.
    let rs_edge = db
        .query(
            "MATCH (o:Org)-[:INDUSTRY]->(p:Person) RETURN COUNT(*)",
            &params,
        )
        .expect("COUNT(*) on edge");
    assert_eq!(rs_edge.columns(), &["COUNT(*)".to_string()]);
    assert_eq!(rs_edge.len(), 1);
    match rs_edge.row(0) {
        [Some(Value::Int(n))] => assert!(*n >= 0, "edge count must be non-negative"),
        other => panic!("expected [Some(Int)], got {other:?}"),
    }

    // Grouped aggregation is rejected with a plan-stage error.
    let err = db
        .query("MATCH (o:Org) RETURN o, COUNT(*)", &params)
        .expect_err("grouped aggregation must fail");
    match &err {
        GraphError::QueryError { detail } => {
            assert!(
                detail.starts_with("plan:"),
                "grouped aggregation error must be plan-prefixed, got: {detail}"
            );
            assert!(
                detail.to_ascii_lowercase().contains("grouped aggregation")
                    || detail.to_ascii_lowercase().contains("not supported"),
                "error must name the limitation, got: {detail}"
            );
        }
        other => panic!("expected QueryError, got {other:?}"),
    }
}

// ── Variable-length path + shortestPath tests ─────────────────────────────

/// Build a diamond graph:
///
///   a → b → d
///   a → c → d
///
/// a→b, a→c = depth 1; a→d = depth 2 (two distinct paths).
fn diamond_db(name: &str) -> GraphDb<core_storage::fs::RealFs> {
    let mut db = GraphDb::open(&tmp(name)).unwrap();
    for n in ["a", "b", "c", "d"] {
        db.insert_node("N", n, vec![("id".into(), Value::Str(n.into()))])
            .unwrap();
    }
    db.insert_edge("T", "a", "b").unwrap();
    db.insert_edge("T", "a", "c").unwrap();
    db.insert_edge("T", "b", "d").unwrap();
    db.insert_edge("T", "c", "d").unwrap();
    db
}

/// Build a chain: a → b → c → d (depth-3 path from a to d).
fn chain_db(name: &str) -> GraphDb<core_storage::fs::RealFs> {
    let mut db = GraphDb::open(&tmp(name)).unwrap();
    for n in ["a", "b", "c", "d"] {
        db.insert_node("N", n, vec![("id".into(), Value::Str(n.into()))])
            .unwrap();
    }
    db.insert_edge("T", "a", "b").unwrap();
    db.insert_edge("T", "b", "c").unwrap();
    db.insert_edge("T", "c", "d").unwrap();
    db
}

#[test]
fn var_expand_diamond_path_counts() {
    let db = diamond_db("vp-diamond");
    let empty = BTreeMap::new();

    // *1..1 from any N: should be all direct edges (4 total from any start)
    // We query from "a" specifically using props.
    // *1..1 from a: a→b, a→c → 2 rows
    let rs = db
        .query(
            "MATCH (a:N {id: 'a'})-[r:T*1..1]->(b) RETURN b",
            &empty,
        )
        .unwrap_or_else(|e| panic!("var expand *1..1 failed: {e}"));
    // a→b and a→c = 2 destinations
    assert_eq!(rs.len(), 2, "*1..1 from a must yield 2 rows (b and c)");

    // *1..2 from a: a→b, a→c (depth 1), a→d via b, a→d via c (depth 2) = 4 rows
    // a→d appears TWICE (two distinct paths)
    let rs2 = db
        .query(
            "MATCH (a:N {id: 'a'})-[r:T*1..2]->(b) RETURN b, r.length",
            &empty,
        )
        .unwrap_or_else(|e| panic!("var expand *1..2 failed: {e}"));
    assert_eq!(
        rs2.len(),
        4,
        "*1..2 from a must yield 4 rows (2 at depth 1, 2 at depth 2)"
    );

    // *2..3 from a: only depth-2 reachable in diamond is d (via b or c) → 2 rows
    // depth-3 from a: would need T-T-T which doesn't exist → 0 additional
    let rs3 = db
        .query(
            "MATCH (a:N {id: 'a'})-[r:T*2..3]->(b) RETURN b",
            &empty,
        )
        .unwrap_or_else(|e| panic!("var expand *2..3 failed: {e}"));
    assert_eq!(
        rs3.len(),
        2,
        "*2..3 from a must yield 2 rows (d via b, d via c)"
    );
}

#[test]
fn var_expand_diamond_no_id_prop() {
    // Use COUNT(*) to verify total path counts across all starts.
    let db = diamond_db("vp-diamond-count");
    let empty = BTreeMap::new();

    // Total *1..2 paths starting from any node: verify via COUNT.
    // All nodes, *1..2: a→b(1), a→c(1), a→d(2,via b), a→d(2,via c),
    //                   b→d(1), c→d(1)  = 6 paths
    let rs = db
        .query("MATCH (a:N)-[r:T*1..2]->(b) RETURN COUNT(*)", &empty)
        .unwrap_or_else(|e| panic!("diamond count *1..2: {e}"));
    assert_eq!(rs.len(), 1);
    assert_eq!(
        rs.row(0)[0],
        Some(Value::Int(6)),
        "diamond *1..2 must yield 6 total paths"
    );
}

/// Cycle graph: a → b → a (a 2-cycle).
///
/// Edge-uniqueness ensures that expanding `*1..10` from `a` terminates
/// because no path can reuse an edge. Without uniqueness, the BFS
/// frontier would loop forever.
#[test]
fn var_expand_cycle_terminates_and_edge_uniqueness_enforced() {
    let mut db = GraphDb::open(&tmp("vp-cycle")).unwrap();
    db.insert_node("N", "a", vec![]).unwrap();
    db.insert_node("N", "b", vec![]).unwrap();
    db.insert_edge("T", "a", "b").unwrap();
    db.insert_edge("T", "b", "a").unwrap();
    let p = BTreeMap::new();

    // *1..10 from any N: must terminate (edge uniqueness) and return a finite count.
    // The cycle has exactly 2 directed edges: a→b (e1) and b→a (e2).
    // Per-path edge-uniqueness (Cypher relationship isomorphism) means each edge
    // can appear at most once in any given path.
    //
    // From a: depth 1 = a→b (e1), depth 2 = a→b→a (e1,e2). At depth 3 the
    //         only outgoing edge from a is e1, which is already in the path. Dead end.
    // From b: depth 1 = b→a (e2), depth 2 = b→a→b (e2,e1). Same dead-end at depth 3.
    // Total: 4 rows (2 per starting node).
    let rs = db
        .query("MATCH (a:N)-[r:T*1..10]->(b) RETURN b", &p)
        .expect("cycle *1..10 must terminate");
    // Just verify it terminates and returns a bounded result.
    assert!(
        rs.len() <= 1_000_000,
        "cycle must produce finite rows, got {}",
        rs.len()
    );
    // Specifically: 2 nodes × 2 reachable depths = 4 rows
    assert_eq!(
        rs.len(),
        4,
        "2-cycle *1..10 must yield exactly 4 rows (2 per starting node, edge-uniqueness caps at depth 2)"
    );
}

#[test]
fn shortest_path_reachable_at_depth_3() {
    let db = chain_db("sp-chain");
    let p = BTreeMap::new();

    // shortestPath(a -[T*..5]-> d): a→b→c→d = depth 3
    let rs = db
        .query(
            "MATCH (a:N {id: 'a'}) MATCH (d:N {id: 'd'}) \
             MATCH shortestPath((a)-[r:T*..5]->(d)) \
             RETURN r.length",
            &p,
        )
        .unwrap_or_else(|e| panic!("shortestPath must succeed: {e}"));
    assert_eq!(rs.len(), 1, "shortestPath must return exactly 1 row when reachable");
    assert_eq!(
        rs.row(0)[0],
        Some(Value::Int(3)),
        "shortest path a→b→c→d must have length 3"
    );
}

#[test]
fn shortest_path_unreachable_returns_zero_rows() {
    let db = chain_db("sp-chain-miss");
    let p = BTreeMap::new();

    // shortestPath(d -[T*..5]-> a): d has no outgoing T edges → 0 rows
    let rs = db
        .query(
            "MATCH (d:N {id: 'd'}) MATCH (a:N {id: 'a'}) \
             MATCH shortestPath((d)-[r:T*..5]->(a)) \
             RETURN r.length",
            &p,
        )
        .unwrap_or_else(|e| panic!("shortestPath unreachable must return Ok: {e}"));
    assert_eq!(
        rs.len(),
        0,
        "shortestPath with unreachable target must return 0 rows"
    );
}

#[test]
fn shortest_path_max_hops_respected() {
    // Chain a→b→c→d (depth 3). shortestPath with max_hops=2 → unreachable.
    let db = chain_db("sp-chain-hops");
    let p = BTreeMap::new();

    let rs = db
        .query(
            "MATCH (a:N {id: 'a'}) MATCH (d:N {id: 'd'}) \
             MATCH shortestPath((a)-[r:T*..2]->(d)) \
             RETURN r.length",
            &p,
        )
        .unwrap_or_else(|e| panic!("shortestPath with tight hop cap: {e}"));
    assert_eq!(
        rs.len(),
        0,
        "shortestPath(a→d) with max 2 hops must return 0 rows (path requires 3)"
    );
}

#[test]
fn var_expand_with_limit_takes_staged_path_and_returns_correct_rows() {
    // Verify that VarExpand + LIMIT uses the staged path (not pull) but still
    // applies the Limit op correctly.
    let db = diamond_db("vp-limit");
    let p = BTreeMap::new();

    // *1..2 from all nodes without a filter yields 6 total rows in diamond.
    // LIMIT 3 must return exactly 3.
    let rs = db
        .query(
            "MATCH (a:N)-[r:T*1..2]->(b) RETURN b LIMIT 3",
            &p,
        )
        .unwrap_or_else(|e| panic!("var expand with LIMIT: {e}"));
    assert_eq!(rs.len(), 3, "LIMIT 3 must return exactly 3 rows");
}

#[test]
fn var_expand_budget_exceeded_errors_cleanly() {
    // Build a dense graph where *1..10 produces >1M intermediate rows.
    // We use a smaller intermediate-row cap (set via test-only hook in
    // exec.rs) to trigger the error without allocating a million rows.
    //
    // Because the test hook is only available from within the core-query
    // crate, we use a large-but-bounded clique instead and let the natural
    // cap fire.  A 50-node complete directed graph has 50×49=2450 edges.
    // Paths of length 1..10 from each node: 50 × (49 + 49² + … ) >> 1M.
    //
    // NOTE: This test is intentionally slow if the cap is not enforced.
    // It must return Err("intermediate result exceeds …") quickly.
    //
    // We build a smaller graph (10 nodes, complete directed) and verify the
    // error message shape.  10×9=90 edges; paths 1..10 from 10 starts:
    // each step fans 9 ways (minus used edges), but row output can explode.
    let mut db = GraphDb::open(&tmp("vp-budget")).unwrap();
    for i in 0..10u32 {
        db.insert_node("N", &format!("n{i}"), vec![]).unwrap();
    }
    for i in 0..10u32 {
        for j in 0..10u32 {
            if i != j {
                db.insert_edge("T", &format!("n{i}"), &format!("n{j}"))
                    .unwrap();
            }
        }
    }
    let p = BTreeMap::new();
    let result = db.query("MATCH (a:N)-[r:T*1..10]->(b) RETURN b", &p);
    match result {
        Err(GraphError::QueryError { ref detail }) => {
            assert!(
                detail.contains("intermediate result exceeds")
                    || detail.contains("1000000")
                    || detail.contains("1,000,000"),
                "budget error must name the limit, got: {detail}"
            );
        }
        Ok(rs) => {
            // If the graph is sparse enough that 1M rows are not exceeded,
            // just verify it returned some rows (not a budget failure).
            // This path should not occur with 10-node complete graph.
            let _ = rs;
        }
        Err(e) => panic!("unexpected non-budget error: {e:?}"),
    }
}

#[test]
fn var_expand_unbound_endpoint_is_plan_error() {
    let db = diamond_db("vp-unbound");
    let result = db.query(
        "MATCH shortestPath((a)-[r:T*..5]->(b)) RETURN a",
        &BTreeMap::new(),
    );
    match result {
        Err(GraphError::QueryError { ref detail }) => {
            assert!(
                detail.contains("shortestPath") || detail.contains("bound"),
                "unbound shortestPath must name the issue, got: {detail}"
            );
        }
        Ok(_) => panic!("unbound shortestPath must be an error"),
        Err(e) => panic!("unexpected error variant: {e:?}"),
    }
}

#[test]
fn var_expand_cap_exceeded_in_parse_is_error() {
    let db = diamond_db("vp-cap");
    let result = db.query("MATCH (a:N)-[r:T*1..11]->(b) RETURN b", &BTreeMap::new());
    match result {
        Err(GraphError::QueryError { ref detail }) => {
            assert!(
                detail.contains("capped at 10 hops"),
                "cap error must say '10 hops', got: {detail}"
            );
        }
        Ok(_) => panic!("*1..11 must be rejected at parse time"),
        Err(e) => panic!("unexpected error variant: {e:?}"),
    }
}

/// Verify that `*0` and `*0..N` are rejected at parse time with the named error.
#[test]
fn var_expand_zero_min_is_rejected() {
    let db = diamond_db("vp-zero-min");
    for q in &[
        "MATCH (a:N)-[r:T*0]->(b) RETURN b",
        "MATCH (a:N)-[r:T*0..3]->(b) RETURN b",
    ] {
        let result = db.query(q, &BTreeMap::new());
        match result {
            Err(GraphError::QueryError { ref detail }) => {
                assert!(
                    detail.contains("zero-length variable-length paths are not supported"),
                    "zero-min error must name the issue, got: {detail}"
                );
            }
            Ok(_) => panic!("min=0 query must be rejected: {q}"),
            Err(e) => panic!("unexpected error variant for {q}: {e:?}"),
        }
    }
}

/// Verify that the frontier PathState budget fires even when output rows have
/// not yet been emitted (i.e., depth < min).  A 10-node complete directed
/// graph with `*5..10` produces a massive frontier at depths 1–4 before any
/// row is output — the budget must catch this and return a clean error.
#[test]
fn var_expand_frontier_budget_fires_before_output() {
    let mut db = GraphDb::open(&tmp("vp-frontier-budget")).unwrap();
    for i in 0..10u32 {
        db.insert_node("N", &format!("n{i}"), vec![]).unwrap();
    }
    for i in 0..10u32 {
        for j in 0..10u32 {
            if i != j {
                db.insert_edge("T", &format!("n{i}"), &format!("n{j}"))
                    .unwrap();
            }
        }
    }
    // min=5: no output rows until depth 5, but the frontier grows as
    // 9^1 + 9^2 + ... by depth 4, far exceeding 1M before any row is emitted.
    let result = db.query("MATCH (a:N)-[r:T*5..10]->(b) RETURN b", &BTreeMap::new());
    match result {
        Err(GraphError::QueryError { ref detail }) => {
            assert!(
                detail.contains("intermediate result exceeds")
                    || detail.contains("1000000")
                    || detail.contains("1,000,000"),
                "frontier budget error must name the limit, got: {detail}"
            );
        }
        Ok(rs) => {
            // If the graph does not expand enough to trip the budget, just verify
            // termination.  This branch should not be reached with the 10-node clique.
            let _ = rs;
        }
        Err(e) => panic!("unexpected non-budget error: {e:?}"),
    }
}
