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

    // Grouped aggregation now succeeds: RETURN o, COUNT(*) must produce one row
    // per distinct node o (each Org node is unique, so count is 1 per group).
    let rs_grouped = db
        .query("MATCH (o:Org) RETURN o, COUNT(*) AS n", &params)
        .expect("grouped aggregation must now succeed");
    // 7 Org nodes → 7 groups (each org appears exactly once).
    assert_eq!(
        rs_grouped.len(),
        7,
        "7 Org nodes must produce 7 groups; got {}",
        rs_grouped.len()
    );
    assert_eq!(
        rs_grouped.columns(),
        &["o".to_string(), "n".to_string()],
        "columns must be [o, n]"
    );
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
        .query("MATCH (a:N {id: 'a'})-[r:T*1..1]->(b) RETURN b", &empty)
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
        .query("MATCH (a:N {id: 'a'})-[r:T*2..3]->(b) RETURN b", &empty)
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
    assert_eq!(
        rs.len(),
        1,
        "shortestPath must return exactly 1 row when reachable"
    );
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
        .query("MATCH (a:N)-[r:T*1..2]->(b) RETURN b LIMIT 3", &p)
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
        Ok(_) => panic!("expected budget error on 10-node complete graph *1..10, got Ok"),
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
        Ok(_) => panic!("expected budget error on 10-node complete graph *5..10, got Ok"),
        Err(e) => panic!("unexpected non-budget error: {e:?}"),
    }
}

/// Left-directed `<-[:T*1..2]-` var-expand: traverses edges in reverse.
/// diamond_db has edges a→b, a→c, b→d, c→d.
/// From d going left: d←b (depth 1), d←c (depth 1), d←b←a (depth 2), d←c←a (depth 2).
/// MATCH (d:N {id: 'd'})<-[r:T*1..2]-(x) from d: 4 rows.
#[test]
fn var_expand_left_directed() {
    let db = diamond_db("vp-left");
    let rs = db
        .query(
            "MATCH (d:N {id: 'd'})<-[r:T*1..2]-(x) RETURN x",
            &BTreeMap::new(),
        )
        .expect("left-directed *1..2 must succeed");
    assert_eq!(
        rs.len(),
        4,
        "left-directed from d: expected 4 rows, got {}",
        rs.len()
    );
}

/// Undirected `-[:T*1..2]-` var-expand: traverses both orientations.
/// diamond_db has a→b, a→c, b→d, c→d.
/// From a undirected *1: reaches b, c (2 rows).
/// From a undirected *2: reaches d (via b), d (via c) = 2 rows.
/// Total: 4 rows from a with *1..2.
/// (b→a and c→a reverse edges not present in the graph, so no additional reverse paths.)
#[test]
fn var_expand_undirected() {
    let db = diamond_db("vp-undirected");
    let rs = db
        .query(
            "MATCH (a:N {id: 'a'})-[r:T*1..2]-(x) RETURN x",
            &BTreeMap::new(),
        )
        .expect("undirected *1..2 must succeed");
    // Undirected from a: right-direction gives 4 rows (b, c at depth 1; d, d at depth 2).
    // Left-direction from a: no incoming T edges to a, so 0 additional rows.
    assert_eq!(
        rs.len(),
        4,
        "undirected from a: expected 4 rows, got {}",
        rs.len()
    );
}

/// shortestPath with min>1 must be rejected at planning time with a named error.
#[test]
fn shortest_path_min_gt_1_is_plan_error() {
    let db = chain_db("sp-min-gt1");
    let result = db.query(
        "MATCH (a:N {id: 'a'}) MATCH (d:N {id: 'd'}) \
         MATCH shortestPath((a)-[r:T*2..5]->(d)) RETURN r.length",
        &BTreeMap::new(),
    );
    match result {
        Err(GraphError::QueryError { ref detail }) => {
            assert!(
                detail.contains("shortestPath") && detail.contains("minimum"),
                "error must name shortestPath and minimum, got: {detail}"
            );
        }
        Ok(_) => panic!("shortestPath with min>1 must be rejected at planning time"),
        Err(e) => panic!("unexpected error variant: {e:?}"),
    }
}

// ── Grouped aggregation integration tests ─────────────────────────────────

/// Grouped aggregation returns the correct per-group counts from a real db.
///
/// Uses the `aggregate-wire` fixture (7 Org nodes) to verify that
/// `RETURN label, COUNT(*)` produces one group per distinct label value.
#[test]
fn grouped_aggregate_counts_by_prop() {
    let db = open_fixture("grouped-count");
    let params = BTreeMap::new();

    // All Org nodes — group by the "name" prop (each org has a unique name so
    // we expect 7 groups each with count 1).
    let rs = db
        .query("MATCH (o:Org) RETURN o, COUNT(*) AS n", &params)
        .expect("grouped COUNT must succeed");
    assert_eq!(rs.len(), 7, "7 Org nodes must produce 7 groups");
    assert_eq!(
        rs.columns(),
        &["o".to_string(), "n".to_string()],
        "columns must be [o, n]"
    );
    // Each group has exactly one row — count per group is 1.
    for i in 0..rs.len() {
        assert_eq!(
            rs.row(i)[1],
            Some(Value::Int(1)),
            "row {i}: each node appears in its own group, count must be 1"
        );
    }
}

/// ORDER BY + LIMIT on a grouped aggregate returns the top-k groups.
#[test]
fn grouped_aggregate_order_by_count_limit() {
    let db = open_fixture("grouped-limit");
    let empty = BTreeMap::new();

    // Count Org-Person INDUSTRY edges per Org, top 3.
    let rs = db
        .query(
            "MATCH (o:Org)-[:INDUSTRY]->(p:Person) \
             RETURN o, COUNT(*) AS n \
             ORDER BY n DESC LIMIT 3",
            &empty,
        )
        .expect("grouped COUNT ORDER BY LIMIT must succeed");
    assert!(rs.len() <= 3, "LIMIT 3 must return at most 3 rows");
    // Rows must be in descending count order.
    for i in 1..rs.len() {
        let prev = rs.row(i - 1)[1].as_ref();
        let curr = rs.row(i)[1].as_ref();
        let ord = match (prev, curr) {
            (Some(Value::Int(a)), Some(Value::Int(b))) => a.cmp(b),
            _ => std::cmp::Ordering::Equal,
        };
        assert!(
            ord != std::cmp::Ordering::Less,
            "rows must be in descending order; row {i} has count > row {}",
            i - 1
        );
    }
}

// ── Plan 12 carryover: V4-reopen pin for aggregate + grouped aggregate ─────

/// Carryover requirement from Plan 12 final review (M-1):
/// aggregate AND grouped-aggregate queries must return identical results
/// against a V4-snapshotted db and against the never-closed reference.
///
/// Test shape: open a db, insert data, run both queries → reference results.
/// Snapshot, reopen, run same queries → must match reference exactly.
#[test]
fn aggregate_and_grouped_aggregate_survive_v4_reopen() {
    let dir = tmp("agg-reopen-pin");

    // Build a simple graph: 5 N nodes with a "cat" prop (3 "A", 2 "B").
    let build_db = |dir: &std::path::Path| {
        let mut db = GraphDb::open(dir).unwrap();
        for k in ["n1", "n2", "n3"] {
            db.insert_node("N", k, vec![("cat".into(), Value::Str("A".into()))])
                .unwrap();
        }
        for k in ["n4", "n5"] {
            db.insert_node("N", k, vec![("cat".into(), Value::Str("B".into()))])
                .unwrap();
        }
        db
    };

    let empty = BTreeMap::new();
    let total_count_q = "MATCH (n:N) RETURN COUNT(*)";
    let grouped_q = "MATCH (n:N) RETURN n.cat, COUNT(*) AS cnt ORDER BY n.cat";

    // Reference: queries against the never-closed db.
    let ref_db = build_db(&dir);
    let ref_total = ref_db
        .query(total_count_q, &empty)
        .expect("reference COUNT(*) must succeed");
    let ref_grouped = ref_db
        .query(grouped_q, &empty)
        .expect("reference grouped aggregate must succeed");

    // Snapshot and reopen.
    drop(ref_db); // close reference db — the snapshot was NOT taken yet,
                  // so reopen reads from the WAL only.
    {
        // Reopen, take snapshot, close.
        let mut db = GraphDb::open(&dir).unwrap();
        db.snapshot().unwrap();
    }

    // Reopen from snapshot.
    let db2 = GraphDb::open(&dir).unwrap();
    let after_total = db2
        .query(total_count_q, &empty)
        .expect("post-reopen COUNT(*) must succeed");
    let after_grouped = db2
        .query(grouped_q, &empty)
        .expect("post-reopen grouped aggregate must succeed");

    // Results must be identical.
    assert_eq!(
        ref_total.row(0),
        after_total.row(0),
        "COUNT(*) must match after V4 reopen: ref={:?} after={:?}",
        ref_total.row(0),
        after_total.row(0)
    );
    assert_eq!(
        ref_grouped.len(),
        after_grouped.len(),
        "grouped aggregate row count must match after V4 reopen"
    );
    assert_eq!(
        ref_grouped.columns(),
        after_grouped.columns(),
        "grouped aggregate columns must match after V4 reopen"
    );
    for i in 0..ref_grouped.len() {
        assert_eq!(
            ref_grouped.row(i),
            after_grouped.row(i),
            "grouped aggregate row {i} must match after V4 reopen: ref={:?} after={:?}",
            ref_grouped.row(i),
            after_grouped.row(i)
        );
    }
}

// ── OPTIONAL MATCH tests ──────────────────────────────────────────────────────

/// Helper: assert rs.get returns a cloned value.
fn get_val(rs: &ResultSet, row: usize, col: &str) -> Option<Value> {
    rs.get(row, col).cloned()
}

/// Classic pin: edgeless node returns COUNT(b) = 0 (not 0 rows).
#[test]
fn optional_match_count_zero_for_edgeless() {
    let dir = tmp("optional_count_zero");
    let mut db = GraphDb::open(&dir).unwrap();
    {
        let mut batch = db.batch();
        batch.insert_node("Person", "n1", vec![]);
        batch.insert_node("Person", "n2", vec![]);
        batch.insert_edge("KNOWS", "n1", "n2");
        batch.insert_node("Person", "n3", vec![]); // edgeless node
        batch.commit().unwrap();
    }

    let rs = db
        .query(
            "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b) RETURN a, COUNT(b)",
            &BTreeMap::new(),
        )
        .unwrap();

    // 3 nodes: n1 has 1 edge (b=n2), n2 has no outgoing KNOWS, n3 has no outgoing.
    assert_eq!(rs.len(), 3, "must have 3 rows, one per node");
    // At least one row must have COUNT(b) = 0 (edgeless nodes).
    let counts: Vec<Option<Value>> = (0..rs.len()).map(|i| get_val(&rs, i, "COUNT(b)")).collect();
    assert!(
        counts.iter().any(|c| c.as_ref() == Some(&Value::Int(0))),
        "edgeless node must return COUNT(b) = 0, got: {counts:?}"
    );
}

/// Left-outer: optional pattern with WHERE inside the optional scope.
#[test]
fn optional_match_with_where_inside_optional() {
    let dir = tmp("optional_where");
    let mut db = GraphDb::open(&dir).unwrap();
    {
        let mut batch = db.batch();
        // Use unique label OW to avoid cross-test label collisions.
        // Only one "anchor" node: alice.  bob is the optional neighbour.
        batch.insert_node(
            "OW",
            "alice",
            vec![("name".into(), Value::Str("Alice".into()))],
        );
        batch.insert_node("OW", "bob", vec![("name".into(), Value::Str("Bob".into()))]);
        batch.insert_edge("FRIEND", "alice", "bob");
        batch.commit().unwrap();
    }
    // WHERE inside OPTIONAL MATCH: the FRIEND edge from alice reaches bob whose
    // name='Bob', not 'nonexistent'.  The WHERE blocks the match, so b is null
    // and the outer alice row still survives (left-outer semantics).
    // We restrict the MATCH to alice via a.name so we get exactly 1 outer row.
    let rs = db
        .query(
            "MATCH (a:OW) WHERE a.name = 'Alice' \
         OPTIONAL MATCH (a)-[:FRIEND]->(b) WHERE b.name = 'nonexistent' \
         RETURN a, b",
            &BTreeMap::new(),
        )
        .unwrap();
    assert_eq!(rs.len(), 1, "one row expected (left-outer fallback)");
    assert_eq!(
        get_val(&rs, 0, "b"),
        None,
        "b must be null when WHERE inside optional fails"
    );
}

/// Multiple chained OPTIONAL MATCHes.
#[test]
fn optional_match_chained() {
    let dir = tmp("optional_chained");
    let mut db = GraphDb::open(&dir).unwrap();
    {
        let mut batch = db.batch();
        // Use unique labels NdA/NdB/NdC so only one node per label.
        // The outer MATCH finds exactly one NdA node ("a").
        batch.insert_node("NdA", "a", vec![]);
        batch.insert_node("NdB", "b", vec![]);
        batch.insert_node("NdC", "c_node", vec![]);
        batch.insert_edge("X", "a", "b");
        // no Y edge from a to anything
        batch.commit().unwrap();
    }
    let rs = db
        .query(
            "MATCH (a:NdA) \
         OPTIONAL MATCH (a)-[:X]->(b) \
         OPTIONAL MATCH (a)-[:Y]->(c) \
         RETURN a, b, c",
            &BTreeMap::new(),
        )
        .unwrap();
    assert_eq!(rs.len(), 1);
    // b = "b" (found via X), c = null (no Y edge)
    assert_eq!(get_val(&rs, 0, "b"), Some(Value::Str("b".into())));
    assert_eq!(get_val(&rs, 0, "c"), None);
}

// ── Parameters tests ──────────────────────────────────────────────────────────

#[test]
fn query_with_params_basic() {
    let dir = tmp("params_basic");
    let mut db = GraphDb::open(&dir).unwrap();
    {
        let mut batch = db.batch();
        batch.insert_node("Person", "alice", vec![("age".into(), Value::Int(30))]);
        batch.insert_node("Person", "bob", vec![("age".into(), Value::Int(25))]);
        batch.commit().unwrap();
    }
    let rs = db
        .query_with_params(
            "MATCH (n:Person) WHERE n.age = $age RETURN n",
            &[("age", Value::Int(30))],
        )
        .unwrap();
    assert_eq!(rs.len(), 1);
    assert_eq!(get_val(&rs, 0, "n"), Some(Value::Str("alice".into())));
}

#[test]
fn query_with_params_unknown_param_error() {
    let dir = tmp("params_unknown");
    let db = GraphDb::open(&dir).unwrap();
    let err = db.query_with_params(
        "MATCH (n:Person) WHERE n.age = $missing RETURN n",
        &[], // no params provided
    );
    assert!(err.is_err(), "unknown param must return Err");
    let msg = format!("{:?}", err.unwrap_err());
    assert!(
        msg.contains("missing") || msg.contains("parameter"),
        "error must mention the missing param: {msg}"
    );
}

#[test]
fn set_with_param() {
    let dir = tmp("set_param");
    let mut db = GraphDb::open(&dir).unwrap();
    {
        let mut batch = db.batch();
        // Use a unique label so MATCH finds exactly one node.
        batch.insert_node("SWP", "alice", vec![("age".into(), Value::Int(30))]);
        batch.commit().unwrap();
    }
    let mut params = BTreeMap::new();
    params.insert("newage".to_string(), Value::Int(99));
    // Match the sole SWP node and SET age to the $newage param.
    db.query_write(
        "MATCH (n:SWP) WHERE n.age = 30 SET n.age = $newage",
        &params,
    )
    .unwrap();
    let rs = db
        .query("MATCH (n:SWP) RETURN n.age", &BTreeMap::new())
        .unwrap();
    assert_eq!(rs.len(), 1);
    assert_eq!(get_val(&rs, 0, "n.age"), Some(Value::Int(99)));
}

/// HTTP injection: a param containing Cypher syntax stays a literal.
#[test]
fn params_injection_safe() {
    let dir = tmp("params_injection");
    let db = GraphDb::open(&dir).unwrap();
    // If param value were interpolated as Cypher, this would parse as a statement
    // and might return rows or error differently. As a literal it's just a string.
    let rs = db
        .query_with_params(
            "MATCH (n:Person {id: $id}) RETURN n",
            &[("id", Value::Str("' RETURN 1//".into()))],
        )
        .unwrap();
    // No node with that (injected) id exists — should return 0 rows.
    assert_eq!(
        rs.len(),
        0,
        "injection payload must be treated as literal string"
    );
}

// ── Core function tests ───────────────────────────────────────────────────────

#[test]
fn fn_tolower_happy() {
    let dir = tmp("fn_tolower");
    let mut db = GraphDb::open(&dir).unwrap();
    {
        let mut batch = db.batch();
        batch.insert_node(
            "Tx",
            "alice",
            vec![("name".into(), Value::Str("Alice".into()))],
        );
        batch.commit().unwrap();
    }
    let rs = db
        .query("MATCH (n:Tx) RETURN toLower(n.name)", &BTreeMap::new())
        .unwrap();
    assert_eq!(
        get_val(&rs, 0, "toLower(n.name)"),
        Some(Value::Str("alice".into()))
    );
}

#[test]
fn fn_tolower_null_propagation() {
    let dir = tmp("fn_tolower_null");
    let mut db = GraphDb::open(&dir).unwrap();
    {
        let mut batch = db.batch();
        batch.insert_node("Tx", "n1", vec![]); // no name prop → null
        batch.commit().unwrap();
    }
    let rs = db
        .query("MATCH (n:Tx) RETURN toLower(n.name)", &BTreeMap::new())
        .unwrap();
    assert_eq!(get_val(&rs, 0, "toLower(n.name)"), None);
}

#[test]
fn fn_toupper_happy() {
    let dir = tmp("fn_toupper");
    let mut db = GraphDb::open(&dir).unwrap();
    {
        let mut batch = db.batch();
        batch.insert_node("Ty", "x", vec![("v".into(), Value::Str("hello".into()))]);
        batch.commit().unwrap();
    }
    let rs = db
        .query("MATCH (n:Ty) RETURN toUpper(n.v)", &BTreeMap::new())
        .unwrap();
    assert_eq!(
        get_val(&rs, 0, "toUpper(n.v)"),
        Some(Value::Str("HELLO".into()))
    );
}

#[test]
fn fn_toupper_null_propagation() {
    let dir = tmp("fn_toupper_null");
    let mut db = GraphDb::open(&dir).unwrap();
    {
        let mut batch = db.batch();
        batch.insert_node("Ty", "x", vec![]);
        batch.commit().unwrap();
    }
    let rs = db
        .query("MATCH (n:Ty) RETURN toUpper(n.v)", &BTreeMap::new())
        .unwrap();
    assert_eq!(get_val(&rs, 0, "toUpper(n.v)"), None);
}

#[test]
fn fn_size_string() {
    let dir = tmp("fn_size_str");
    let mut db = GraphDb::open(&dir).unwrap();
    {
        let mut batch = db.batch();
        batch.insert_node("Ts", "x", vec![("s".into(), Value::Str("hello".into()))]);
        batch.commit().unwrap();
    }
    let rs = db
        .query("MATCH (n:Ts) RETURN size(n.s)", &BTreeMap::new())
        .unwrap();
    assert_eq!(get_val(&rs, 0, "size(n.s)"), Some(Value::Int(5)));
}

#[test]
fn fn_size_list() {
    let dir = tmp("fn_size_list");
    let mut db = GraphDb::open(&dir).unwrap();
    {
        let mut batch = db.batch();
        batch.insert_node(
            "Tsl",
            "x",
            vec![(
                "tags".into(),
                Value::List(vec![
                    Value::Str("a".into()),
                    Value::Str("b".into()),
                    Value::Str("c".into()),
                ]),
            )],
        );
        batch.commit().unwrap();
    }
    let rs = db
        .query("MATCH (n:Tsl) RETURN size(n.tags)", &BTreeMap::new())
        .unwrap();
    assert_eq!(get_val(&rs, 0, "size(n.tags)"), Some(Value::Int(3)));
}

#[test]
fn fn_size_null_propagation() {
    let dir = tmp("fn_size_null");
    let mut db = GraphDb::open(&dir).unwrap();
    {
        let mut batch = db.batch();
        batch.insert_node("Tsnull", "x", vec![]);
        batch.commit().unwrap();
    }
    let rs = db
        .query("MATCH (n:Tsnull) RETURN size(n.missing)", &BTreeMap::new())
        .unwrap();
    assert_eq!(get_val(&rs, 0, "size(n.missing)"), None);
}

#[test]
fn fn_coalesce_happy() {
    let dir = tmp("fn_coalesce");
    let mut db = GraphDb::open(&dir).unwrap();
    {
        let mut batch = db.batch();
        batch.insert_node("Tc", "x", vec![("b".into(), Value::Int(42))]);
        batch.commit().unwrap();
    }
    // coalesce(n.a, n.b) — n.a is null, n.b = 42
    let rs = db
        .query("MATCH (n:Tc) RETURN coalesce(n.a, n.b)", &BTreeMap::new())
        .unwrap();
    assert_eq!(get_val(&rs, 0, "coalesce(n.a, n.b)"), Some(Value::Int(42)));
}

#[test]
fn fn_coalesce_all_null() {
    let dir = tmp("fn_coalesce_null");
    let mut db = GraphDb::open(&dir).unwrap();
    {
        let mut batch = db.batch();
        batch.insert_node("Tc", "x", vec![]);
        batch.commit().unwrap();
    }
    let rs = db
        .query("MATCH (n:Tc) RETURN coalesce(n.a, n.b)", &BTreeMap::new())
        .unwrap();
    assert_eq!(get_val(&rs, 0, "coalesce(n.a, n.b)"), None);
}

#[test]
fn fn_type_happy() {
    let dir = tmp("fn_type");
    let mut db = GraphDb::open(&dir).unwrap();
    {
        let mut batch = db.batch();
        batch.insert_node("Pt", "ta", vec![]);
        batch.insert_node("Pt", "tb", vec![]);
        batch.insert_edge("KNOWS", "ta", "tb");
        batch.commit().unwrap();
    }
    let rs = db
        .query("MATCH (a:Pt)-[r]->(b:Pt) RETURN type(r)", &BTreeMap::new())
        .unwrap();
    assert_eq!(get_val(&rs, 0, "type(r)"), Some(Value::Str("KNOWS".into())));
}

#[test]
fn fn_type_null_propagation() {
    // type() with null binding (OPTIONAL MATCH that misses) → null out.
    let dir = tmp("fn_type_null");
    let mut db = GraphDb::open(&dir).unwrap();
    {
        let mut batch = db.batch();
        batch.insert_node("Ptn", "a", vec![]);
        batch.commit().unwrap();
    }
    // r is null because there are no edges from a.
    let rs = db
        .query(
            "MATCH (a:Ptn) OPTIONAL MATCH (a)-[r]->() RETURN type(r)",
            &BTreeMap::new(),
        )
        .unwrap();
    assert_eq!(get_val(&rs, 0, "type(r)"), None);
}

#[test]
fn fn_abs_happy() {
    let dir = tmp("fn_abs");
    let mut db = GraphDb::open(&dir).unwrap();
    {
        let mut batch = db.batch();
        batch.insert_node("Tab", "x", vec![("v".into(), Value::Int(-7))]);
        batch.commit().unwrap();
    }
    let rs = db
        .query("MATCH (n:Tab) RETURN abs(n.v)", &BTreeMap::new())
        .unwrap();
    assert_eq!(get_val(&rs, 0, "abs(n.v)"), Some(Value::Int(7)));
}

#[test]
fn fn_abs_null_propagation() {
    let dir = tmp("fn_abs_null");
    let mut db = GraphDb::open(&dir).unwrap();
    {
        let mut batch = db.batch();
        batch.insert_node("Tab", "x", vec![]);
        batch.commit().unwrap();
    }
    let rs = db
        .query("MATCH (n:Tab) RETURN abs(n.missing)", &BTreeMap::new())
        .unwrap();
    assert_eq!(get_val(&rs, 0, "abs(n.missing)"), None);
}

#[test]
fn fn_round_happy() {
    let dir = tmp("fn_round");
    let mut db = GraphDb::open(&dir).unwrap();
    {
        let mut batch = db.batch();
        batch.insert_node("Tr", "x", vec![("v".into(), Value::Float(2.7))]);
        batch.commit().unwrap();
    }
    let rs = db
        .query("MATCH (n:Tr) RETURN round(n.v)", &BTreeMap::new())
        .unwrap();
    assert_eq!(get_val(&rs, 0, "round(n.v)"), Some(Value::Float(3.0)));
}

#[test]
fn fn_round_null_propagation() {
    let dir = tmp("fn_round_null");
    let mut db = GraphDb::open(&dir).unwrap();
    {
        let mut batch = db.batch();
        batch.insert_node("Tr", "x", vec![]);
        batch.commit().unwrap();
    }
    let rs = db
        .query("MATCH (n:Tr) RETURN round(n.missing)", &BTreeMap::new())
        .unwrap();
    assert_eq!(get_val(&rs, 0, "round(n.missing)"), None);
}

/// Regression: abs(n.age - 27) — BinArith in function argument (Dash token inside func call).
/// Before the fix, the parser rejected this with "expected ')' to close function call (found Dash)".
#[test]
fn fn_abs_binarith_sub_arg() {
    let dir = tmp("fn_abs_binarith");
    let mut db = GraphDb::open(&dir).unwrap();
    {
        let mut batch = db.batch();
        batch.insert_node("Ba", "x", vec![("age".into(), Value::Int(30))]);
        batch.commit().unwrap();
    }
    // abs(n.age - 27) => abs(30 - 27) => abs(3) => 3
    let rs = db
        .query("MATCH (n:Ba) RETURN abs(n.age - 27)", &BTreeMap::new())
        .unwrap();
    assert_eq!(get_val(&rs, 0, "abs(<arith>)"), Some(Value::Int(3)));
}

/// Regression: round(n.score * 1.5) — BinArith with Star token inside function argument.
/// Before the fix, the parser rejected this with "expected ')' to close function call (found Star)".
#[test]
fn fn_round_binarith_mul_arg() {
    let dir = tmp("fn_round_binarith");
    let mut db = GraphDb::open(&dir).unwrap();
    {
        let mut batch = db.batch();
        batch.insert_node("Br", "x", vec![("score".into(), Value::Float(2.0))]);
        batch.commit().unwrap();
    }
    // round(n.score * 1.5) => round(2.0 * 1.5) => round(3.0) => 3.0
    let rs = db
        .query("MATCH (n:Br) RETURN round(n.score * 1.5)", &BTreeMap::new())
        .unwrap();
    assert_eq!(get_val(&rs, 0, "round(<arith>)"), Some(Value::Float(3.0)));
}

/// Regression: OPTIONAL MATCH null-binding property access.
/// Before the fix, accessing b.name when b is null from OPTIONAL MATCH returned
/// "execute: unbound variable `b`" instead of propagating null.
#[test]
fn optional_match_null_property_access() {
    let dir = tmp("optional_null_prop");
    let mut db = GraphDb::open(&dir).unwrap();
    {
        let mut batch = db.batch();
        // Anchor node with no outgoing KNOWS edges — OPTIONAL MATCH will find nothing.
        batch.insert_node(
            "ONP",
            "solo",
            vec![("name".into(), Value::Str("Solo".into()))],
        );
        batch.commit().unwrap();
    }
    // b is null (no KNOWS edge), so b.name must be null (not an error).
    let rs = db
        .query(
            "MATCH (a:ONP) OPTIONAL MATCH (a)-[:KNOWS]->(b) RETURN a.name, b.name",
            &BTreeMap::new(),
        )
        .unwrap();
    assert_eq!(
        rs.len(),
        1,
        "one outer row must survive OPTIONAL MATCH miss"
    );
    assert_eq!(
        get_val(&rs, 0, "a.name"),
        Some(Value::Str("Solo".into())),
        "outer node property must be accessible"
    );
    assert_eq!(
        get_val(&rs, 0, "b.name"),
        None,
        "null-binding property access must propagate null, not error"
    );
}

#[test]
fn fn_unknown_function_error() {
    let dir = tmp("fn_unknown");
    let mut db = GraphDb::open(&dir).unwrap();
    {
        let mut batch = db.batch();
        batch.insert_node("Tu", "x", vec![("v".into(), Value::Int(1))]);
        batch.commit().unwrap();
    }
    let err = db.query("MATCH (n:Tu) RETURN unknownFn(n.v)", &BTreeMap::new());
    assert!(err.is_err(), "unknown function must return Err");
    let msg = format!("{:?}", err.unwrap_err());
    assert!(
        msg.contains("unknown function") || msg.contains("unknownFn"),
        "error must name the unknown function: {msg}"
    );
}

#[test]
fn unknown_function_lists_text_matches() {
    let dir = tmp("fn_unknown_text_matches");
    let mut db = GraphDb::open(&dir).unwrap();
    {
        let mut batch = db.batch();
        batch.insert_node("N", "k", vec![]);
        batch.commit().unwrap();
    }
    let err = db
        .query("MATCH (n) RETURN nosuch(n)", &BTreeMap::new())
        .unwrap_err();
    let s = err.to_string();
    assert!(s.contains("textMatches"), "{s}");
}

// ── LIMIT/SKIP $param tests ───────────────────────────────────────────────────

/// LIMIT $n resolves the named parameter at runtime.
#[test]
fn limit_param_basic() {
    let dir = tmp("limit_param");
    let mut db = GraphDb::open(&dir).unwrap();
    {
        let mut batch = db.batch();
        for i in 0..5u32 {
            batch.insert_node("LP", &format!("n{i}"), vec![]);
        }
        batch.commit().unwrap();
    }
    let rs = db
        .query_with_params(
            "MATCH (n:LP) RETURN n LIMIT $cap",
            &[("cap", Value::Int(2))],
        )
        .unwrap();
    assert_eq!(rs.len(), 2, "LIMIT $cap=2 must return exactly 2 rows");
}

/// SKIP $n resolves the named parameter at runtime.
#[test]
fn skip_param_basic() {
    let dir = tmp("skip_param");
    let mut db = GraphDb::open(&dir).unwrap();
    {
        let mut batch = db.batch();
        for i in 0..5u32 {
            batch.insert_node("SP", &format!("n{i}"), vec![]);
        }
        batch.commit().unwrap();
    }
    let rs = db
        .query_with_params(
            "MATCH (n:SP) RETURN n SKIP $offset LIMIT 10",
            &[("offset", Value::Int(3))],
        )
        .unwrap();
    assert_eq!(
        rs.len(),
        2,
        "SKIP $offset=3 with 5 nodes must return 2 rows"
    );
}

/// Negative integer for LIMIT $param is a named error.
#[test]
fn limit_param_negative_is_error() {
    let dir = tmp("limit_param_neg");
    let db = GraphDb::open(&dir).unwrap();
    let err = db.query_with_params(
        "MATCH (n:LPN) RETURN n LIMIT $cap",
        &[("cap", Value::Int(-1))],
    );
    assert!(err.is_err(), "negative LIMIT param must return Err");
    let msg = format!("{:?}", err.unwrap_err());
    assert!(
        msg.contains("non-negative") || msg.contains("cap"),
        "error must mention the param or non-negative: {msg}"
    );
}

/// Unknown $param in LIMIT is caught upfront.
#[test]
fn limit_param_unknown_is_error() {
    let dir = tmp("limit_param_unknown");
    let db = GraphDb::open(&dir).unwrap();
    let err = db.query_with_params(
        "MATCH (n:LPUK) RETURN n LIMIT $missing",
        &[], // no params provided
    );
    assert!(err.is_err(), "missing LIMIT param must return Err");
    let msg = format!("{:?}", err.unwrap_err());
    assert!(
        msg.contains("missing") || msg.contains("missing_param") || msg.contains("missing"),
        "error must mention missing parameter: {msg}"
    );
}

// ── Regression: pipeline GroupAggregate without OPTIONAL MATCH ────────────────

/// Regression pin for the pre-existing pipeline GroupAggregate bug fixed in T3.
/// `MATCH (a) WITH a RETURN COUNT(a)` went through the pipeline path even without
/// OPTIONAL MATCH (because GroupAggregate-then-Filter counts as pipeline).  The
/// executor returned an empty ResultSet instead of one row with the aggregate.
/// This test uses a concrete query shape that exercises the same code path.
#[test]
fn pipeline_group_aggregate_without_optional_match() {
    let dir = tmp("pipeline_gagg");
    let mut db = GraphDb::open(&dir).unwrap();
    {
        let mut batch = db.batch();
        batch.insert_node("PGA", "a", vec![]);
        batch.insert_node("PGA", "b", vec![]);
        batch.insert_node("PGA", "c", vec![]);
        batch.commit().unwrap();
    }
    // This query forces the pipeline path via GroupAggregate in staged executor.
    // Without the fix, it would return 0 rows.
    let rs = db
        .query("MATCH (a:PGA) WITH a RETURN COUNT(a)", &BTreeMap::new())
        .unwrap();
    assert_eq!(
        rs.len(),
        1,
        "grouped aggregate with no keys must return exactly 1 row"
    );
    assert_eq!(
        get_val(&rs, 0, "COUNT(a)"),
        Some(Value::Int(3)),
        "COUNT(a) over 3 nodes must be 3"
    );
}

// ── collect_params pre-flight for RETURN FuncCall ─────────────────────────────

/// A missing $param referenced inside a RETURN FuncCall must be caught by the
/// pre-flight param check, not per-row inside eval_func.
#[test]
fn params_preflight_catches_missing_param_in_return_funccall() {
    let dir = tmp("params_preflight");
    let db = GraphDb::open(&dir).unwrap();
    let err = db.query(
        "MATCH (n:PF) RETURN toLower($val)",
        &BTreeMap::new(), // $val not provided
    );
    assert!(
        err.is_err(),
        "missing $val in RETURN FuncCall must return Err"
    );
    let msg = format!("{:?}", err.unwrap_err());
    assert!(
        msg.contains("missing") || msg.contains("val"),
        "error must mention the missing parameter: {msg}"
    );
}

// ── Minor gap tests ───────────────────────────────────────────────────────────

/// OPTIONAL MATCH as the first clause (no preceding MATCH) must be rejected
/// with a named parse error, not a panic.
#[test]
fn optional_match_as_first_clause_is_parse_error() {
    let dir = tmp("optional_first");
    let db = GraphDb::open(&dir).unwrap();
    let err = db.query("OPTIONAL MATCH (a:Person) RETURN a", &BTreeMap::new());
    assert!(
        err.is_err(),
        "OPTIONAL MATCH without preceding MATCH must fail"
    );
    let msg = format!("{:?}", err.unwrap_err());
    assert!(
        msg.contains("MATCH") || msg.contains("parse") || msg.contains("expected"),
        "error must indicate a parse issue: {msg}"
    );
}

/// size() on a non-string non-list value (Int) propagates null, not an error.
#[test]
fn fn_size_non_string_non_list_is_null() {
    let dir = tmp("fn_size_int");
    let mut db = GraphDb::open(&dir).unwrap();
    {
        let mut batch = db.batch();
        batch.insert_node("Si", "x", vec![("v".into(), Value::Int(42))]);
        batch.commit().unwrap();
    }
    let rs = db
        .query("MATCH (n:Si) RETURN size(n.v)", &BTreeMap::new())
        .unwrap();
    assert_eq!(rs.len(), 1);
    // size(<Int>) → null (not an error; openCypher null propagation)
    assert_eq!(
        get_val(&rs, 0, "size(n.v)"),
        None,
        "size on Int must return null"
    );
}

/// type(r) on a rule-derived edge returns the rule's edge_type string.
#[test]
fn fn_type_on_derived_edge() {
    let dir = tmp("fn_type_derived");
    let mut db = GraphDb::open(&dir).unwrap();
    // Create a rule that produces a LINKED_TO edge.
    db.create_rule(RuleDef {
        name: "link_rule".into(),
        src_label: "TypeOrg".into(),
        dst_label: "TypePerson".into(),
        predicate: Predicate::Overlap {
            field: "tags".into(),
            min: 0.1,
        },
        edge_type: "LINKED_TO".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
    })
    .unwrap();
    {
        let mut batch = db.batch();
        batch.insert_node(
            "TypeOrg",
            "org1",
            vec![("tags".into(), Value::List(vec![Value::Str("rust".into())]))],
        );
        batch.insert_node(
            "TypePerson",
            "person1",
            vec![("tags".into(), Value::List(vec![Value::Str("rust".into())]))],
        );
        batch.commit().unwrap();
    }
    let rs = db
        .query(
            "MATCH (a:TypeOrg)-[r]->(b:TypePerson) RETURN type(r)",
            &BTreeMap::new(),
        )
        .unwrap();
    assert_eq!(rs.len(), 1, "derived edge must appear in MATCH");
    assert_eq!(
        get_val(&rs, 0, "type(r)"),
        Some(Value::Str("LINKED_TO".into())),
        "type(r) must return the rule's edge_type for derived edges"
    );
}

// ── Round-2 regression tests ──────────────────────────────────────────────────

/// Exposing test: SKIP $n LIMIT 3 over 20 nodes with $n=15 must return 3 rows.
/// Pre-fix: row_bound returned Some(3) (ignoring param skip), routed to pull
/// path which fetched only 3 rows then applied SKIP 15 → 0 rows.
#[test]
fn skip_param_does_not_route_to_pull_path() {
    let dir = tmp("skip_param_pull");
    let mut db = GraphDb::open(&dir).unwrap();
    {
        let mut batch = db.batch();
        for i in 0..20u32 {
            batch.insert_node("SPP", &format!("n{i:02}"), vec![]);
        }
        batch.commit().unwrap();
    }
    let rs = db
        .query_with_params(
            "MATCH (n:SPP) RETURN n SKIP $offset LIMIT 3",
            &[("offset", Value::Int(15))],
        )
        .unwrap();
    assert_eq!(
        rs.len(),
        3,
        "SKIP 15 LIMIT 3 over 20 nodes must return 3 rows"
    );
}

/// Non-integer (string) LIMIT param produces a named error.
#[test]
fn limit_param_wrong_type_is_named_error() {
    let dir = tmp("limit_param_type");
    let db = GraphDb::open(&dir).unwrap();
    let err = db.query_with_params(
        "MATCH (n:LPWT) RETURN n LIMIT $cap",
        &[("cap", Value::Str("five".into()))],
    );
    assert!(err.is_err(), "string LIMIT param must return Err");
    let msg = format!("{:?}", err.unwrap_err());
    assert!(
        msg.contains("integer") || msg.contains("cap"),
        "error must mention integer type or param name: {msg}"
    );
}

// ── Cross-feature composites (final-review pins) ──────────────────────────────

/// Composite pin: OPTIONAL MATCH + LIMIT $param in one query (Minor-1).
/// Both shapes force staged routing independently (LeftOuterApply guard +
/// Param(Limit) guard in `row_bound`); both guards fire for the composite,
/// meaning the query reaches the staged executor and returns correct rows.
#[test]
fn optional_match_with_limit_param() {
    let dir = tmp("opt_match_limit_param");
    let mut db = GraphDb::open(&dir).unwrap();
    {
        let mut batch = db.batch();
        // Four outer nodes; n0 has a KNOWS edge, the rest do not.
        for i in 0..4u32 {
            batch.insert_node("OPL", &format!("n{i}"), vec![]);
        }
        batch.insert_edge("KNOWS", "n0", "n1");
        batch.commit().unwrap();
    }
    // OPTIONAL MATCH (LeftOuterApply) + LIMIT $cap (Param(Limit)) composite.
    let rs = db
        .query_with_params(
            "MATCH (a:OPL) OPTIONAL MATCH (a)-[:KNOWS]->(b) RETURN a LIMIT $cap",
            &[("cap", Value::Int(2))],
        )
        .unwrap();
    assert_eq!(
        rs.len(),
        2,
        "LIMIT $cap=2 must cap result to 2 rows despite OPTIONAL MATCH"
    );
}

/// Composite pin: $param inside BinArith function argument — happy path (Minor-2).
/// `resolve_operand` recurses into `BinArith.left` / `.right`, hitting
/// `Operand::Param` and looking up the params map; same chain as $param in WHERE
/// but exercised via a different call site (RETURN expression evaluation).
#[test]
fn abs_binarith_param_arg_happy() {
    let dir = tmp("abs_param_arg");
    let mut db = GraphDb::open(&dir).unwrap();
    {
        let mut batch = db.batch();
        batch.insert_node("PB", "x", vec![]);
        batch.commit().unwrap();
    }
    // abs($x - 1) with $x = 5 → abs(4) → 4.
    let rs = db
        .query_with_params("MATCH (n:PB) RETURN abs($x - 1)", &[("x", Value::Int(5))])
        .unwrap();
    assert_eq!(rs.len(), 1);
    assert_eq!(
        get_val(&rs, 0, "abs(<arith>)"),
        Some(Value::Int(4)),
        "abs($x - 1) with $x=5 must return 4"
    );
}

/// Composite pin: $param inside BinArith function arg — missing param is a named
/// error when the RETURN expression is actually evaluated (Minor-2).
#[test]
fn abs_binarith_param_arg_missing_is_error() {
    let dir = tmp("abs_param_arg_missing");
    let mut db = GraphDb::open(&dir).unwrap();
    {
        let mut batch = db.batch();
        // One node so that the RETURN expression is evaluated and the
        // missing $x param triggers a named error.
        batch.insert_node("PBM", "x", vec![]);
        batch.commit().unwrap();
    }
    let err = db.query_with_params(
        "MATCH (n:PBM) RETURN abs($x - 1)",
        &[], // $x not provided
    );
    assert!(err.is_err(), "missing $x must return Err");
    let msg = format!("{:?}", err.unwrap_err());
    assert!(
        msg.contains("x") || msg.contains("parameter"),
        "error must name the missing parameter: {msg}"
    );
}

// ── IN / DISTINCT / still-named-errors ────────────────────────────────────────

#[test]
fn where_in_list_and_param() {
    let dir = tmp("where_in_list");
    let mut db = GraphDb::open(&dir).unwrap();
    {
        let mut batch = db.batch();
        batch.insert_node(
            "Person",
            "austin",
            vec![
                ("id".into(), Value::Str("austin".into())),
                ("city".into(), Value::Str("Austin".into())),
            ],
        );
        batch.insert_node(
            "Person",
            "paris",
            vec![
                ("id".into(), Value::Str("paris".into())),
                ("city".into(), Value::Str("Paris".into())),
            ],
        );
        batch.insert_node(
            "Person",
            "london",
            vec![
                ("id".into(), Value::Str("london".into())),
                ("city".into(), Value::Str("London".into())),
            ],
        );
        batch.commit().unwrap();
    }
    let mut params = BTreeMap::new();
    params.insert("c".into(), Value::Str("Paris".into()));
    let rs = db
        .query(
            "MATCH (n:Person) WHERE n.city IN ['Austin', $c] RETURN n.city AS city ORDER BY city",
            &params,
        )
        .unwrap();
    assert_eq!(rs.len(), 2);
    assert_eq!(get_val(&rs, 0, "city"), Some(Value::Str("Austin".into())));
    assert_eq!(get_val(&rs, 1, "city"), Some(Value::Str("Paris".into())));

    // `$cities` as Value::List
    let mut list_params = BTreeMap::new();
    list_params.insert(
        "cities".into(),
        Value::List(vec![
            Value::Str("Austin".into()),
            Value::Str("Paris".into()),
        ]),
    );
    let rs2 = db
        .query(
            "MATCH (n:Person) WHERE n.city IN $cities RETURN n.city AS city ORDER BY city",
            &list_params,
        )
        .unwrap();
    assert_eq!(rs2.len(), 2);
    assert_eq!(get_val(&rs2, 0, "city"), Some(Value::Str("Austin".into())));
    assert_eq!(get_val(&rs2, 1, "city"), Some(Value::Str("Paris".into())));
}

#[test]
fn return_distinct_cities() {
    let dir = tmp("return_distinct");
    let mut db = GraphDb::open(&dir).unwrap();
    {
        let mut batch = db.batch();
        batch.insert_node(
            "Person",
            "a1",
            vec![
                ("id".into(), Value::Str("a1".into())),
                ("city".into(), Value::Str("Austin".into())),
            ],
        );
        batch.insert_node(
            "Person",
            "a2",
            vec![
                ("id".into(), Value::Str("a2".into())),
                ("city".into(), Value::Str("Austin".into())),
            ],
        );
        batch.commit().unwrap();
    }
    let rs = db
        .query(
            "MATCH (n:Person) RETURN DISTINCT n.city AS city",
            &BTreeMap::new(),
        )
        .unwrap();
    assert_eq!(
        rs.len(),
        1,
        "two Austin nodes must collapse to one DISTINCT row"
    );
    assert_eq!(get_val(&rs, 0, "city"), Some(Value::Str("Austin".into())));
}

#[test]
fn union_case_collect_are_named_errors() {
    let db = open_fixture("named-err-union");
    for (cypher, needle) in [
        (
            "MATCH (n:Person) RETURN n UNION MATCH (m:Person) RETURN m",
            "UNION",
        ),
        (
            "MATCH (n:Person) RETURN CASE WHEN n.id = 't1' THEN 1 ELSE 0 END",
            "CASE",
        ),
        ("MATCH (n:Person) RETURN collect(n)", "collect"),
    ] {
        let err = db.query(cypher, &BTreeMap::new()).expect_err(cypher);
        let detail = match err {
            GraphError::QueryError { detail } => detail,
            other => panic!("{cypher}: expected QueryError, got {other:?}"),
        };
        assert!(
            detail
                .to_ascii_lowercase()
                .contains(&needle.to_ascii_lowercase()),
            "{cypher}: error must name {needle}, got: {detail}"
        );
    }
}

/// Composite pin: float BinArith null propagation (Minor-3).
/// `abs(n.missing_float - 1.5)` — left operand is null because the property
/// is absent → the shared `(None, _) | (_, None) => Ok(None)` guard in
/// `resolve_operand` fires before float-path dispatch → abs receives None →
/// null row value, not an error.
#[test]
fn abs_float_binarith_null_propagation() {
    let dir = tmp("abs_float_null");
    let mut db = GraphDb::open(&dir).unwrap();
    {
        let mut batch = db.batch();
        // Node has no 'missing_float' property → evaluates to null.
        batch.insert_node("FBN", "x", vec![]);
        batch.commit().unwrap();
    }
    let rs = db
        .query(
            "MATCH (n:FBN) RETURN abs(n.missing_float - 1.5)",
            &BTreeMap::new(),
        )
        .unwrap();
    assert_eq!(
        rs.len(),
        1,
        "one row must be produced even when BinArith arg is null"
    );
    assert_eq!(
        get_val(&rs, 0, "abs(<arith>)"),
        None,
        "abs(null - 1.5) must propagate null, not error"
    );
}
