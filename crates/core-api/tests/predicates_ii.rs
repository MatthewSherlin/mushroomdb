use core_api::{Direction, GraphDb, Predicate, RuleDef, Value};
use std::collections::BTreeMap;

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-p7-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn loc(lat: f64, lon: f64) -> Value {
    Value::List(vec![Value::Float(lat), Value::Float(lon)])
}

fn emb(vals: &[f64]) -> Value {
    Value::List(vals.iter().copied().map(Value::Float).collect())
}

fn numeric_rule() -> RuleDef {
    RuleDef {
        name: "founded_near".into(),
        src_label: "Person".into(),
        dst_label: "Person".into(),
        predicate: Predicate::NumericWithin {
            field: "founded_year".into(),
            tolerance: 3.0,
        },
        edge_type: "FOUNDED_NEAR".into(),
        weight_prop: Some("score".into()),
        max_edges: None,
        approximate: false,
    }
}

fn geo_rule() -> RuleDef {
    RuleDef {
        name: "office_near".into(),
        src_label: "Office".into(),
        dst_label: "Office".into(),
        predicate: Predicate::GeoRadius {
            field: "loc".into(),
            km: 400.0,
        },
        edge_type: "OFFICE_NEAR".into(),
        weight_prop: Some("score".into()),
        max_edges: None,
        approximate: false,
    }
}

fn vec_rule() -> RuleDef {
    RuleDef {
        name: "doc_sim".into(),
        src_label: "Doc".into(),
        dst_label: "Doc".into(),
        predicate: Predicate::VectorSimilar {
            field: "emb".into(),
            min: 0.9,
        },
        edge_type: "DOC_SIM".into(),
        weight_prop: Some("score".into()),
        max_edges: None,
        approximate: false,
    }
}

fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    const R: f64 = 6371.0088;
    let phi1 = lat1.to_radians();
    let phi2 = lat2.to_radians();
    let dphi = (lat2 - lat1).to_radians();
    let dlam = (lon2 - lon1).to_radians();
    let a = ((dphi / 2.0).sin().powi(2) + phi1.cos() * phi2.cos() * (dlam / 2.0).sin().powi(2))
        .clamp(0.0, 1.0);
    let c = 2.0 * a.sqrt().atan2((1.0 - a).sqrt());
    R * c
}

fn paris_london_score() -> f64 {
    1.0 - haversine_km(48.8566, 2.3522, 51.5074, -0.1278) / 400.0
}

fn seed_matching_triple(db: &mut GraphDb<core_storage::fs::RealFs>) {
    db.insert_node(
        "Person",
        "ada",
        vec![("founded_year".into(), Value::Int(1998))],
    )
    .unwrap();
    db.insert_node(
        "Person",
        "bob",
        vec![("founded_year".into(), Value::Float(2000.0))],
    )
    .unwrap();
    db.insert_node(
        "Person",
        "cara",
        vec![("founded_year".into(), Value::Int(2010))],
    )
    .unwrap();
    db.insert_node(
        "Office",
        "paris",
        vec![("loc".into(), loc(48.8566, 2.3522))],
    )
    .unwrap();
    db.insert_node(
        "Office",
        "london",
        vec![("loc".into(), loc(51.5074, -0.1278))],
    )
    .unwrap();
    db.insert_node(
        "Office",
        "nyc",
        vec![("loc".into(), loc(40.7128, -74.0060))],
    )
    .unwrap();
    db.insert_node("Doc", "d1", vec![("emb".into(), emb(&[1.0, 0.0]))])
        .unwrap();
    db.insert_node("Doc", "d2", vec![("emb".into(), emb(&[1.0, 0.0]))])
        .unwrap();
    db.insert_node("Doc", "d3", vec![("emb".into(), emb(&[0.0, 1.0]))])
        .unwrap();
}

fn create_three(db: &mut GraphDb<core_storage::fs::RealFs>) {
    db.create_rule(numeric_rule()).unwrap();
    db.create_rule(geo_rule()).unwrap();
    db.create_rule(vec_rule()).unwrap();
}

fn assert_close(got: f64, want: f64, ctx: &str) {
    assert!(
        (got - want).abs() < 1e-9,
        "{ctx}: got {got} want {want} (Δ={})",
        (got - want).abs()
    );
}

fn assert_pair(
    db: &GraphDb<core_storage::fs::RealFs>,
    a: &str,
    b: &str,
    rule: &str,
    etype: &str,
    score: f64,
) {
    let out_a = db.neighbors(a, etype, Direction::Out).unwrap();
    assert!(
        out_a.iter().any(|k| k == b),
        "{etype} {a}→{b} missing: {out_a:?}"
    );
    let out_b = db.neighbors(b, etype, Direction::Out).unwrap();
    assert!(
        out_b.iter().any(|k| k == a),
        "{etype} {b}→{a} missing: {out_b:?}"
    );
    let ex = db.explain(a, b).unwrap();
    let hits: Vec<_> = ex.iter().filter(|e| e.rule == rule).collect();
    assert_eq!(hits.len(), 2, "explain {a}/{b} rule={rule}: {ex:?}");
    for e in hits {
        let w = e.weight.expect("weight_prop must be present");
        assert_close(w, score, &format!("{} {}→{}", e.rule, e.src_key, e.dst_key));
    }
}

fn assert_no_edge(db: &GraphDb<core_storage::fs::RealFs>, a: &str, b: &str, etype: &str) {
    assert!(
        !db.neighbors(a, etype, Direction::Out)
            .unwrap()
            .iter()
            .any(|k| k == b),
        "unexpected {etype} {a}→{b}"
    );
}

fn weighted_pairs(
    db: &GraphDb<core_storage::fs::RealFs>,
    keys: &[&str],
    etype: &str,
) -> BTreeMap<(String, String), f64> {
    let mut out = BTreeMap::new();
    for a in keys {
        for b in db.neighbors(a, etype, Direction::Out).unwrap() {
            let w = db
                .explain(a, &b)
                .unwrap()
                .into_iter()
                .find(|e| e.edge_type == etype && e.src_key == *a && e.dst_key == b)
                .and_then(|e| e.weight)
                .expect("weighted derived edge");
            out.insert(((*a).to_string(), b), w);
        }
    }
    out
}

fn assert_expected_matches(db: &GraphDb<core_storage::fs::RealFs>) {
    assert_pair(db, "ada", "bob", "founded_near", "FOUNDED_NEAR", 1.0 / 3.0);
    assert_no_edge(db, "ada", "cara", "FOUNDED_NEAR");
    assert_no_edge(db, "bob", "cara", "FOUNDED_NEAR");
    assert_pair(
        db,
        "paris",
        "london",
        "office_near",
        "OFFICE_NEAR",
        paris_london_score(),
    );
    assert_no_edge(db, "paris", "nyc", "OFFICE_NEAR");
    assert_no_edge(db, "london", "nyc", "OFFICE_NEAR");
    assert_pair(db, "d1", "d2", "doc_sim", "DOC_SIM", 1.0);
    assert_no_edge(db, "d1", "d3", "DOC_SIM");
    assert_no_edge(db, "d2", "d3", "DOC_SIM");
}

#[test]
fn new_predicates_derive_expected_edges_and_explain() {
    let dir = tmp("scores");
    let mut db = GraphDb::open(&dir).unwrap();
    seed_matching_triple(&mut db);
    create_three(&mut db);
    assert_expected_matches(&db);
}

#[test]
fn wal_replay_rederives_identical_weights_and_does_not_log_edges() {
    let dir = tmp("replay");
    let live_num;
    let live_geo;
    let live_vec;
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node(
            "Person",
            "ada",
            vec![("founded_year".into(), Value::Int(1998))],
        )
        .unwrap();
        db.create_rule(numeric_rule()).unwrap();
        db.create_rule(geo_rule()).unwrap();
        db.create_rule(vec_rule()).unwrap();
        let wal_before = std::fs::metadata(dir.join("wal.bin")).unwrap().len();
        db.insert_node(
            "Person",
            "bob",
            vec![("founded_year".into(), Value::Float(2000.0))],
        )
        .unwrap();
        let wal_after = std::fs::read(dir.join("wal.bin")).unwrap();
        assert!(wal_after.len() as u64 > wal_before);
        let suffix = &wal_after[wal_before as usize..];
        let (recs, _) = core_storage::wal::decode_all(suffix);
        let has_edge = recs.iter().any(|r| match r {
            core_storage::wal::WalRecord::InsertEdge { .. }
            | core_storage::wal::WalRecord::InsertEdgeId { .. } => true,
            core_storage::wal::WalRecord::Batch(inner) => inner.iter().any(|x| {
                matches!(
                    x,
                    core_storage::wal::WalRecord::InsertEdge { .. }
                        | core_storage::wal::WalRecord::InsertEdgeId { .. }
                )
            }),
            _ => false,
        });
        assert!(
            !has_edge,
            "derived FOUNDED_NEAR edges must not be WAL-logged"
        );
        db.insert_node(
            "Office",
            "paris",
            vec![("loc".into(), loc(48.8566, 2.3522))],
        )
        .unwrap();
        db.insert_node(
            "Office",
            "london",
            vec![("loc".into(), loc(51.5074, -0.1278))],
        )
        .unwrap();
        db.insert_node("Doc", "d1", vec![("emb".into(), emb(&[1.0, 0.0]))])
            .unwrap();
        db.insert_node("Doc", "d2", vec![("emb".into(), emb(&[1.0, 0.0]))])
            .unwrap();
        live_num = weighted_pairs(&db, &["ada", "bob"], "FOUNDED_NEAR");
        live_geo = weighted_pairs(&db, &["paris", "london"], "OFFICE_NEAR");
        live_vec = weighted_pairs(&db, &["d1", "d2"], "DOC_SIM");
        assert_eq!(live_num.len(), 2);
        assert_eq!(live_geo.len(), 2);
        assert_eq!(live_vec.len(), 2);
    }
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(
        weighted_pairs(&db, &["ada", "bob"], "FOUNDED_NEAR"),
        live_num
    );
    assert_eq!(
        weighted_pairs(&db, &["paris", "london"], "OFFICE_NEAR"),
        live_geo
    );
    assert_eq!(weighted_pairs(&db, &["d1", "d2"], "DOC_SIM"), live_vec);
}

#[test]
fn snapshot_reopen_mid_stream_converges() {
    let dir = tmp("snap");
    let live_num;
    let live_geo;
    let live_vec;
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node(
            "Person",
            "ada",
            vec![("founded_year".into(), Value::Int(1998))],
        )
        .unwrap();
        db.insert_node(
            "Office",
            "paris",
            vec![("loc".into(), loc(48.8566, 2.3522))],
        )
        .unwrap();
        db.insert_node("Doc", "d1", vec![("emb".into(), emb(&[1.0, 0.0]))])
            .unwrap();
        create_three(&mut db);
        db.snapshot().unwrap();
        db.insert_node(
            "Person",
            "bob",
            vec![("founded_year".into(), Value::Float(2000.0))],
        )
        .unwrap();
        db.insert_node(
            "Office",
            "london",
            vec![("loc".into(), loc(51.5074, -0.1278))],
        )
        .unwrap();
        db.insert_node("Doc", "d2", vec![("emb".into(), emb(&[1.0, 0.0]))])
            .unwrap();
        live_num = weighted_pairs(&db, &["ada", "bob"], "FOUNDED_NEAR");
        live_geo = weighted_pairs(&db, &["paris", "london"], "OFFICE_NEAR");
        live_vec = weighted_pairs(&db, &["d1", "d2"], "DOC_SIM");
        assert_eq!(live_num.len(), 2);
        assert_eq!(live_geo.len(), 2);
        assert_eq!(live_vec.len(), 2);
    }
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(
        weighted_pairs(&db, &["ada", "bob"], "FOUNDED_NEAR"),
        live_num
    );
    assert_eq!(
        weighted_pairs(&db, &["paris", "london"], "OFFICE_NEAR"),
        live_geo
    );
    assert_eq!(weighted_pairs(&db, &["d1", "d2"], "DOC_SIM"), live_vec);
}

#[test]
fn prop_update_retracts_each_new_predicate() {
    let dir = tmp("retract");
    let mut db = GraphDb::open(&dir).unwrap();
    seed_matching_triple(&mut db);
    create_three(&mut db);
    assert_expected_matches(&db);

    db.set_prop("bob", "founded_year", Value::Int(2010))
        .unwrap();
    assert_no_edge(&db, "ada", "bob", "FOUNDED_NEAR");

    db.set_prop("london", "loc", loc(40.7128, -74.0060))
        .unwrap();
    assert_no_edge(&db, "paris", "london", "OFFICE_NEAR");

    db.set_prop("d2", "emb", emb(&[0.0, 1.0])).unwrap();
    assert_no_edge(&db, "d1", "d2", "DOC_SIM");
}

#[test]
fn vector_topk_per_source_caps_and_not_frozen() {
    let dir = tmp("vec-topk");
    let mut db = GraphDb::open(&dir).unwrap();
    db.create_rule(RuleDef {
        name: "doc_sim".into(),
        src_label: "Doc".into(),
        dst_label: "Doc".into(),
        predicate: Predicate::VectorSimilar {
            field: "emb".into(),
            min: 0.9,
        },
        edge_type: "DOC_SIM".into(),
        weight_prop: Some("score".into()),
        max_edges: Some(2),
        approximate: false,
    })
    .unwrap();
    // 5 nodes all with emb=[1,0] → cosine sim=1.0 ≥ 0.9; top-2 per source.
    for i in 0..5 {
        db.insert_node(
            "Doc",
            &format!("d{i}"),
            vec![("emb".into(), emb(&[1.0, 0.0]))],
        )
        .unwrap();
    }
    let s = db.stats();
    // 5 nodes × top-2 each = 10 edges; top-k rules never set tripped.
    assert_eq!(s.rules[0].edges, 10, "5 nodes × top-2 = 10 edges");
    assert!(!s.rules[0].tripped, "top-k rules must never set tripped");
    assert_eq!(s.edges, 10);

    // Insert a 6th node → it adds its own top-2 out-edges; not frozen.
    db.insert_node("Doc", "d5", vec![("emb".into(), emb(&[1.0, 0.0]))])
        .unwrap();
    let s = db.stats();
    // d5's top-2 = d0, d1 (keys sorted ASC, all tied at score=1.0).
    assert_eq!(s.rules[0].edges, 12, "6 nodes × top-2 = 12 edges");
    assert!(!s.rules[0].tripped);
    assert_eq!(s.edges, 12);
    assert!(
        !db.neighbors("d5", "DOC_SIM", Direction::Out)
            .unwrap()
            .is_empty(),
        "d5 must have derived out-edges (top-k is not frozen)"
    );
}

#[test]
fn delete_numeric_coetype_survivor_keeps_edges() {
    let dir = tmp("coetype");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node(
        "Person",
        "ada",
        vec![
            ("founded_year".into(), Value::Int(1998)),
            ("ind".into(), Value::Str("arch".into())),
        ],
    )
    .unwrap();
    db.insert_node(
        "Person",
        "bob",
        vec![
            ("founded_year".into(), Value::Float(2000.0)),
            ("ind".into(), Value::Str("arch".into())),
        ],
    )
    .unwrap();
    db.create_rule(RuleDef {
        name: "by_year".into(),
        src_label: "Person".into(),
        dst_label: "Person".into(),
        predicate: Predicate::NumericWithin {
            field: "founded_year".into(),
            tolerance: 3.0,
        },
        edge_type: "NEAR".into(),
        weight_prop: Some("score".into()),
        max_edges: None,
        approximate: false,
    })
    .unwrap();
    db.create_rule(RuleDef {
        name: "by_ind".into(),
        src_label: "Person".into(),
        dst_label: "Person".into(),
        predicate: Predicate::FieldEqual {
            field: "ind".into(),
        },
        edge_type: "NEAR".into(),
        weight_prop: Some("score".into()),
        max_edges: None,
        approximate: false,
    })
    .unwrap();
    assert_eq!(
        db.neighbors("ada", "NEAR", Direction::Out).unwrap(),
        vec!["bob"]
    );
    assert_eq!(
        db.neighbors("bob", "NEAR", Direction::Out).unwrap(),
        vec!["ada"]
    );

    db.delete_rule("by_year").unwrap();
    assert_eq!(
        db.neighbors("ada", "NEAR", Direction::Out).unwrap(),
        vec!["bob"]
    );
    assert_eq!(
        db.neighbors("bob", "NEAR", Direction::Out).unwrap(),
        vec!["ada"]
    );
    let ex = db.explain("ada", "bob").unwrap();
    assert!(
        ex.iter().all(|e| e.rule == "by_ind"),
        "survivor must own after rebuild: {ex:?}"
    );
    assert_eq!(ex.len(), 2);

    db.delete_rule("by_ind").unwrap();
    assert!(db
        .neighbors("ada", "NEAR", Direction::Out)
        .unwrap()
        .is_empty());
    assert_eq!(db.edge_count(), 0);
}
