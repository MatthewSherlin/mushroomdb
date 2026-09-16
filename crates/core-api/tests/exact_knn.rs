//! Exact kNN integration tests (GEMM brute path and pairwise_similar).
//!
//! Nested-loop cosine lives **in this file**. Do not call `exact_knn::`.

use core_api::{
    with_pairwise_caps, GraphDb, GraphError, NodeMask, Predicate, PropPredicate, RuleDef, Value,
    PAIRWISE_MAX_N,
};
use core_storage::fs::RealFs;

type Db = GraphDb<RealFs>;

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-exact-knn-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn emb(xs: &[f64]) -> Value {
    Value::List(xs.iter().copied().map(Value::Float).collect())
}

fn l2(xs: &[f64]) -> f64 {
    xs.iter().map(|x| x * x).sum::<f64>().sqrt()
}

/// Nested-loop cosine: skip mixed-dim and zero-norm, then L2-normalise and dot.
fn cosine_oracle(q: &[f64], v: &[f64]) -> Option<f64> {
    if q.len() != v.len() {
        return None;
    }
    let qn = l2(q);
    let vn = l2(v);
    if qn == 0.0 || vn == 0.0 {
        return None;
    }
    Some(
        q.iter()
            .zip(v.iter())
            .map(|(a, b)| (a / qn) * (b / vn))
            .sum(),
    )
}

fn brute_oracle(nodes: &[(String, Vec<f64>)], q: &[f64], k: usize, min: f64) -> Vec<(String, f64)> {
    let mut scored: Vec<(String, f64)> = nodes
        .iter()
        .filter_map(|(key, v)| {
            let sim = cosine_oracle(q, v)?;
            if sim < min {
                return None;
            }
            Some((key.clone(), sim))
        })
        .collect();
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    scored.truncate(k);
    scored
}

fn fill_vec(seed: u64, dim: usize) -> Vec<f64> {
    let mut s = seed | 1;
    (0..dim)
        .map(|_| {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
            let x = ((s >> 33) as f64) / (f64::from(u32::MAX)) * 2.0 - 1.0;
            if x == 0.0 {
                0.5
            } else {
                x
            }
        })
        .collect()
}

/// 256 vectors, dim 16 and dim 64. Twenty queries each. Engine brute path
/// (no VectorSimilar rule) equals the nested-loop oracle: identical keys,
/// scores within 1e-9.
#[test]
fn gemm_brute_equals_scalar_on_256() {
    for dim in [16_usize, 64] {
        let dir = tmp(&format!("256-d{dim}"));
        let mut db = GraphDb::open(&dir).unwrap();
        let mut nodes: Vec<(String, Vec<f64>)> = Vec::with_capacity(256);
        for i in 0..256u64 {
            let key = format!("n{i:03}");
            let v = fill_vec(i + 1, dim);
            db.insert_node("Item", &key, vec![("emb".into(), emb(&v))])
                .unwrap();
            nodes.push((key, v));
        }
        assert!(
            !db.has_vector_rule("emb"),
            "fixture: brute path, no VectorSimilar rule"
        );

        let k = 10;
        let min = 0.0;
        for q_i in 0..20u64 {
            let q = fill_vec(10_000 + q_i, dim);
            let got = db.find_similar_vector("emb", Some("Item"), &q, k, min);
            let expect = brute_oracle(&nodes, &q, k, min);
            let got_keys: Vec<&str> = got.iter().map(|(key, _)| key.as_str()).collect();
            let expect_keys: Vec<&str> = expect.iter().map(|(key, _)| key.as_str()).collect();
            assert_eq!(got_keys, expect_keys, "dim={dim} query={q_i}: key order");
            assert_eq!(got.len(), expect.len());
            for ((gk, gs), (ek, es)) in got.iter().zip(expect.iter()) {
                assert_eq!(gk, ek);
                assert!(
                    (gs - es).abs() <= 1e-9,
                    "dim={dim} query={q_i} key={gk}: |{gs} - {es}| > 1e-9"
                );
            }
        }
    }
}

/// A 3-d leftover next to 8-d vectors must not appear. Today's scalar brute
/// `zip`s mixed lengths, so this fails until the GEMM pack skips `len() != dim`.
#[test]
fn mixed_dim_candidate_is_skipped() {
    let dir = tmp("mixed-dim");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node(
        "Item",
        "keep0",
        vec![("emb".into(), emb(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]))],
    )
    .unwrap();
    db.insert_node(
        "Item",
        "keep1",
        vec![("emb".into(), emb(&[0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]))],
    )
    .unwrap();
    db.insert_node(
        "Item",
        "keep2",
        vec![("emb".into(), emb(&[0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0]))],
    )
    .unwrap();
    // Partial-dot against q = [1,0,...] scores 1.0 if zip is used.
    db.insert_node("Item", "stray", vec![("emb".into(), emb(&[1.0, 0.0, 0.0]))])
        .unwrap();

    let q = [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    let hits = db.find_similar_vector("emb", Some("Item"), &q, 10, 0.0);
    let keys: Vec<&str> = hits.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        keys,
        vec!["keep0", "keep1", "keep2"],
        "3-d leftover must be skipped, got {hits:?}"
    );
}

fn pairwise_oracle(
    nodes: &[(String, Vec<f64>)],
    k: usize,
    min: f64,
) -> Vec<(String, Vec<(String, f64)>)> {
    let mut out = Vec::with_capacity(nodes.len());
    for (i, (src, q)) in nodes.iter().enumerate() {
        let mut neigh: Vec<(String, f64)> = nodes
            .iter()
            .enumerate()
            .filter_map(|(j, (dst, v))| {
                if i == j {
                    return None;
                }
                let sim = cosine_oracle(q, v)?;
                if sim < min {
                    return None;
                }
                Some((dst.clone(), sim))
            })
            .collect();
        neigh.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        neigh.truncate(k);
        out.push((src.clone(), neigh));
    }
    out
}

fn assert_pairwise_eq(
    got: &[(String, Vec<(String, f64)>)],
    expect: &[(String, Vec<(String, f64)>)],
    ctx: &str,
) {
    let got_keys: Vec<&str> = got.iter().map(|(k, _)| k.as_str()).collect();
    let exp_keys: Vec<&str> = expect.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(got_keys, exp_keys, "{ctx}: outer key order");
    assert_eq!(got.len(), expect.len(), "{ctx}: outer len");
    for ((gk, gneigh), (ek, eneigh)) in got.iter().zip(expect.iter()) {
        assert_eq!(gk, ek, "{ctx}: src");
        let gdst: Vec<&str> = gneigh.iter().map(|(k, _)| k.as_str()).collect();
        let edst: Vec<&str> = eneigh.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(gdst, edst, "{ctx}: {gk} neighbor keys");
        assert_eq!(gneigh.len(), eneigh.len(), "{ctx}: {gk} neighbor len");
        for ((dk, ds), (xk, xs)) in gneigh.iter().zip(eneigh.iter()) {
            assert_eq!(dk, xk, "{ctx}: {gk} dst");
            assert!(
                (ds - xs).abs() <= 1e-9,
                "{ctx}: {gk}->{dk} |{ds} - {xs}| > 1e-9"
            );
        }
    }
}

fn seed_pairwise(name: &str, n: u64, dim: usize) -> (Db, Vec<(String, Vec<f64>)>) {
    let dir = tmp(name);
    let mut db = GraphDb::open(&dir).unwrap();
    let mut nodes = Vec::with_capacity(n as usize);
    for i in 0..n {
        let key = format!("n{i:02}");
        let v = fill_vec(i + 1, dim);
        db.insert_node("Item", &key, vec![("emb".into(), emb(&v))])
            .unwrap();
        nodes.push((key, v));
    }
    (db, nodes)
}

fn sim_rule() -> RuleDef {
    RuleDef {
        name: "sim".into(),
        src_label: "Item".into(),
        dst_label: "Item".into(),
        predicate: Predicate::VectorSimilar {
            field: "emb".into(),
            min: 0.5,
        },
        edge_type: "SIM".into(),
        weight_prop: None,
        max_edges: None,
        approximate: true,
        via_label: None,
        via_edge: None,
        via_dir: None,
        namespace: None,
    }
}

/// 32 vectors, dim 8, fixed seed. Engine equals nested-loop cosine (L2,
/// drop diagonal, min, top-k, sort sim desc / key asc) to 1e-9.
#[test]
fn pairwise_similar_equals_naive_all_pairs() {
    let (db, nodes) = seed_pairwise("pairwise-32", 32, 8);
    let keys: Vec<&str> = nodes.iter().map(|(k, _)| k.as_str()).collect();
    let got = db.pairwise_similar(&keys, "emb", 5, 0.0).unwrap();
    let expect = pairwise_oracle(&nodes, 5, 0.0);
    assert_pairwise_eq(&got, &expect, "32-vector all-pairs");
}

/// A node is never in its own neighbour list, including when it is the
/// unique nearest to itself.
#[test]
fn pairwise_similar_excludes_self() {
    let dir = tmp("pairwise-self");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Item", "a", vec![("emb".into(), emb(&[1.0, 0.0]))])
        .unwrap();
    db.insert_node("Item", "b", vec![("emb".into(), emb(&[0.1, 1.0]))])
        .unwrap();
    db.insert_node("Item", "c", vec![("emb".into(), emb(&[-1.0, 0.05]))])
        .unwrap();
    let got = db
        .pairwise_similar(&["a", "b", "c"], "emb", 10, -1.0)
        .unwrap();
    assert!(!got.is_empty());
    for (src, neigh) in &got {
        assert!(
            neigh.iter().all(|(dst, _)| dst != src),
            "{src} listed itself: {neigh:?}"
        );
    }
    let a_hits = got.iter().find(|(k, _)| k == "a").expect("a packed");
    let a_self = cosine_oracle(&[1.0, 0.0], &[1.0, 0.0]).unwrap();
    assert!((a_self - 1.0).abs() < 1e-12);
    let nearest_other = a_hits
        .1
        .iter()
        .map(|(_, s)| *s)
        .fold(f64::NEG_INFINITY, f64::max);
    assert!(
        nearest_other < a_self,
        "a is uniquely nearest to itself; other max={nearest_other}"
    );
}

/// k=2 returns at most 2; a pair under min is absent.
#[test]
fn pairwise_similar_respects_k_and_min() {
    let dir = tmp("pairwise-k-min");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node(
        "Item",
        "a",
        vec![("emb".into(), emb(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]))],
    )
    .unwrap();
    db.insert_node(
        "Item",
        "b",
        vec![("emb".into(), emb(&[0.9, 0.1, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]))],
    )
    .unwrap();
    db.insert_node(
        "Item",
        "c",
        vec![("emb".into(), emb(&[0.8, 0.2, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]))],
    )
    .unwrap();
    db.insert_node(
        "Item",
        "d",
        vec![("emb".into(), emb(&[0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]))],
    )
    .unwrap();
    let got = db
        .pairwise_similar(&["a", "b", "c", "d"], "emb", 2, 0.5)
        .unwrap();
    for (src, neigh) in &got {
        assert!(neigh.len() <= 2, "{src} has {} hits", neigh.len());
        for (dst, sim) in neigh {
            assert!(*sim >= 0.5, "{src}->{dst} sim={sim} below min");
        }
    }
    let a_dst: Vec<&str> = got
        .iter()
        .find(|(k, _)| k == "a")
        .unwrap()
        .1
        .iter()
        .map(|(k, _)| k.as_str())
        .collect();
    assert!(!a_dst.contains(&"d"), "a-d cosine is 0, under min=0.5");
}

/// `"ghost"` in the key list does not error and does not appear.
#[test]
fn pairwise_similar_unknown_keys_are_skipped() {
    let dir = tmp("pairwise-ghost");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Item", "a", vec![("emb".into(), emb(&[1.0, 0.0]))])
        .unwrap();
    db.insert_node("Item", "b", vec![("emb".into(), emb(&[0.0, 1.0]))])
        .unwrap();
    let got = db
        .pairwise_similar(&["a", "ghost", "b"], "emb", 10, 0.0)
        .unwrap();
    let srcs: Vec<&str> = got.iter().map(|(k, _)| k.as_str()).collect();
    assert!(!srcs.contains(&"ghost"), "got {srcs:?}");
    assert_eq!(srcs, vec!["a", "b"]);
}

#[test]
fn pairwise_similar_empty_keys_returns_empty() {
    let dir = tmp("pairwise-empty");
    let db = GraphDb::open(&dir).unwrap();
    let got = db.pairwise_similar(&[], "emb", 10, 0.0).unwrap();
    assert!(got.is_empty());
}

/// A key with no `field`, a zero vector, and a wrong-dim vector are omitted.
#[test]
fn pairwise_similar_missing_embeddings_are_skipped() {
    let dir = tmp("pairwise-missing");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node(
        "Item",
        "keep",
        vec![("emb".into(), emb(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]))],
    )
    .unwrap();
    db.insert_node(
        "Item",
        "keep2",
        vec![("emb".into(), emb(&[0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]))],
    )
    .unwrap();
    db.insert_node(
        "Item",
        "nofield",
        vec![("name".into(), Value::Str("x".into()))],
    )
    .unwrap();
    db.insert_node(
        "Item",
        "zero",
        vec![("emb".into(), emb(&[0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]))],
    )
    .unwrap();
    db.insert_node("Item", "wrong", vec![("emb".into(), emb(&[1.0, 0.0, 0.0]))])
        .unwrap();
    let got = db
        .pairwise_similar(
            &["keep", "nofield", "zero", "wrong", "keep2"],
            "emb",
            10,
            0.0,
        )
        .unwrap();
    let srcs: Vec<&str> = got.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(srcs, vec!["keep", "keep2"], "got {got:?}");
}

/// Approximate VectorSimilar covering `field` does not send pairwise through HNSW.
#[test]
fn pairwise_similar_does_not_use_hnsw() {
    let (mut db, nodes) = seed_pairwise("pairwise-hnsw", 16, 8);
    db.create_rule(sim_rule()).unwrap();
    assert!(db.has_vector_rule("emb"));
    let keys: Vec<&str> = nodes.iter().map(|(k, _)| k.as_str()).collect();
    core_rules::hnsw_search_count_reset();
    let got = db.pairwise_similar(&keys, "emb", 5, 0.0).unwrap();
    assert_eq!(
        core_rules::hnsw_search_count(),
        0,
        "pairwise_similar must not call hnsw_search"
    );
    let expect = pairwise_oracle(&nodes, 5, 0.0);
    assert_pairwise_eq(&got, &expect, "pairwise with VectorSimilar rule");
}

/// Keys ordered `[2-d leftover, …8-d…]` still return the majority-dim graph.
#[test]
fn pairwise_similar_modal_dim_not_first() {
    let dir = tmp("pairwise-modal");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Item", "leftover", vec![("emb".into(), emb(&[1.0, 0.0]))])
        .unwrap();
    let mut majority = Vec::new();
    for i in 0..8u64 {
        let key = format!("n{i}");
        let v = fill_vec(i + 10, 8);
        db.insert_node("Item", &key, vec![("emb".into(), emb(&v))])
            .unwrap();
        majority.push((key, v));
    }
    let mut keys: Vec<&str> = vec!["leftover"];
    keys.extend(majority.iter().map(|(k, _)| k.as_str()));
    let got = db.pairwise_similar(&keys, "emb", 5, 0.0).unwrap();
    let srcs: Vec<&str> = got.iter().map(|(k, _)| k.as_str()).collect();
    assert!(
        !srcs.contains(&"leftover"),
        "2-d leftover used as dim: {srcs:?}"
    );
    let expect = pairwise_oracle(&majority, 5, 0.0);
    assert_pairwise_eq(&got, &expect, "modal dim 8");
}

/// Hook-lowered PAIRWISE_GRAM_MAX below n still equals the nested-loop oracle.
#[test]
fn pairwise_similar_gram_fallback_equals_naive() {
    let (db, nodes) = seed_pairwise("pairwise-gemv", 32, 8);
    let keys: Vec<&str> = nodes.iter().map(|(k, _)| k.as_str()).collect();
    let got = with_pairwise_caps(8, PAIRWISE_MAX_N, || {
        db.pairwise_similar(&keys, "emb", 5, 0.0).unwrap()
    });
    let expect = pairwise_oracle(&nodes, 5, 0.0);
    assert_pairwise_eq(&got, &expect, "gemv fallback");
}

/// n above hook-lowered PAIRWISE_MAX_N returns QueryError naming the limit.
#[test]
fn pairwise_similar_refuses_above_max_n() {
    let (db, nodes) = seed_pairwise("pairwise-max-n", 5, 8);
    let keys: Vec<&str> = nodes.iter().map(|(k, _)| k.as_str()).collect();
    let err = with_pairwise_caps(4, 4, || db.pairwise_similar(&keys, "emb", 5, 0.0))
        .expect_err("n=5 must exceed PAIRWISE_MAX_N=4");
    match err {
        GraphError::QueryError { detail } => {
            assert!(
                detail.contains("PAIRWISE_MAX_N"),
                "QueryError must name PAIRWISE_MAX_N, got {detail}"
            );
            assert!(
                detail.contains('4'),
                "QueryError must name the limit, got {detail}"
            );
        }
        other => panic!("expected QueryError, got {other:?}"),
    }
}

/// A pair scoring exactly `min` is kept (`>=`). Sidecar post-filters distance.
#[test]
fn pairwise_similar_boundary_min_inclusive() {
    let dir = tmp("pairwise-min-eq");
    let mut db = GraphDb::open(&dir).unwrap();
    let s = 0.75_f64.sqrt();
    db.insert_node("Item", "a", vec![("emb".into(), emb(&[1.0, 0.0]))])
        .unwrap();
    db.insert_node("Item", "b", vec![("emb".into(), emb(&[0.5, s]))])
        .unwrap();
    let got = db.pairwise_similar(&["a", "b"], "emb", 10, 0.5).unwrap();
    let a_neigh = &got.iter().find(|(k, _)| k == "a").expect("a packed").1;
    assert_eq!(a_neigh.len(), 1, "exact min must be kept, got {got:?}");
    assert_eq!(a_neigh[0].0, "b");
    assert!(
        (a_neigh[0].1 - 0.5).abs() <= 1e-9,
        "expected cosine 0.5, got {}",
        a_neigh[0].1
    );
}

fn pred_eq(field: &str, value: &str) -> PropPredicate {
    PropPredicate {
        field: field.into(),
        eq: Some(Value::Str(value.into())),
        in_: None,
    }
}

fn pred_in(field: &str, values: &[&str]) -> PropPredicate {
    PropPredicate {
        field: field.into(),
        eq: None,
        in_: Some(values.iter().map(|s| Value::Str((*s).into())).collect()),
    }
}

fn insert_doc(db: &mut Db, key: &str, scope: Option<&str>, v: &[f64]) {
    let mut props = vec![("emb".into(), emb(v))];
    if let Some(s) = scope {
        props.push(("resource_scope_id".into(), Value::Str(s.into())));
    }
    db.insert_node("Document", key, props).unwrap();
}

/// Three Document nodes, `resource_scope_id` in `{a, a, b}`; `where={eq: a}`
/// returns the same keys and scores as `mask=[those two keys]`.
#[test]
fn find_similar_where_eq_matches_key_list_mask() {
    let dir = tmp("where-eq-mask");
    let mut db = GraphDb::open(&dir).unwrap();
    insert_doc(&mut db, "d1", Some("a"), &[1.0, 0.0]);
    insert_doc(&mut db, "d2", Some("a"), &[0.9, 0.1]);
    insert_doc(&mut db, "d3", Some("b"), &[1.0, 0.0]);
    let q = [1.0, 0.0];
    let pred = pred_eq("resource_scope_id", "a");
    let by_where = db
        .find_similar_vector_filtered(
            "emb",
            Some("Document"),
            &q,
            10,
            0.0,
            None,
            Some(&pred),
            false,
        )
        .unwrap();
    let mask = NodeMask::from_keys(&db, ["d1", "d2"]);
    let by_mask = db.find_similar_vector_masked("emb", Some("Document"), &q, 10, 0.0, &mask);
    assert_eq!(by_where, by_mask, "where eq a must match mask=[d1,d2]");
    let keys: Vec<&str> = by_where.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, vec!["d1", "d2"]);
}

#[test]
fn find_similar_where_in_matches_union_of_eq() {
    let dir = tmp("where-in-union");
    let mut db = GraphDb::open(&dir).unwrap();
    insert_doc(&mut db, "d1", Some("a"), &[1.0, 0.0]);
    insert_doc(&mut db, "d2", Some("b"), &[0.0, 1.0]);
    insert_doc(&mut db, "d3", Some("c"), &[0.7, 0.7]);
    let q = [1.0, 0.0];
    let eq_a = db
        .find_similar_vector_filtered(
            "emb",
            Some("Document"),
            &q,
            10,
            0.0,
            None,
            Some(&pred_eq("resource_scope_id", "a")),
            false,
        )
        .unwrap();
    let eq_b = db
        .find_similar_vector_filtered(
            "emb",
            Some("Document"),
            &q,
            10,
            0.0,
            None,
            Some(&pred_eq("resource_scope_id", "b")),
            false,
        )
        .unwrap();
    let by_in = db
        .find_similar_vector_filtered(
            "emb",
            Some("Document"),
            &q,
            10,
            0.0,
            None,
            Some(&pred_in("resource_scope_id", &["a", "b"])),
            false,
        )
        .unwrap();
    let mut union = eq_a;
    union.extend(eq_b);
    union.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    assert_eq!(by_in, union);
    let keys: Vec<&str> = by_in.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, vec!["d1", "d2"]);
}

#[test]
fn find_similar_where_missing_property_fails() {
    let dir = tmp("where-missing");
    let mut db = GraphDb::open(&dir).unwrap();
    insert_doc(&mut db, "kept", Some("a"), &[1.0, 0.0]);
    insert_doc(&mut db, "bare", None, &[1.0, 0.0]);
    let hits = db
        .find_similar_vector_filtered(
            "emb",
            Some("Document"),
            &[1.0, 0.0],
            10,
            0.0,
            None,
            Some(&pred_eq("resource_scope_id", "a")),
            false,
        )
        .unwrap();
    let keys: Vec<&str> = hits.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, vec!["kept"], "node without the field must be absent");
}

#[test]
fn find_similar_where_empty_in_matches_nothing() {
    let dir = tmp("where-empty-in");
    let mut db = GraphDb::open(&dir).unwrap();
    insert_doc(&mut db, "d1", Some("a"), &[1.0, 0.0]);
    let hits = db
        .find_similar_vector_filtered(
            "emb",
            Some("Document"),
            &[1.0, 0.0],
            10,
            0.0,
            None,
            Some(&pred_in("resource_scope_id", &[])),
            false,
        )
        .unwrap();
    assert!(hits.is_empty(), "empty in must match nothing, got {hits:?}");
}

#[test]
fn find_similar_where_and_mask_intersect() {
    let dir = tmp("where-mask-intersect");
    let mut db = GraphDb::open(&dir).unwrap();
    insert_doc(&mut db, "d1", Some("a"), &[1.0, 0.0]);
    insert_doc(&mut db, "d2", Some("a"), &[0.9, 0.1]);
    insert_doc(&mut db, "d3", Some("b"), &[1.0, 0.0]);
    let mask = NodeMask::from_keys(&db, ["d2", "d3"]);
    let hits = db
        .find_similar_vector_filtered(
            "emb",
            Some("Document"),
            &[1.0, 0.0],
            10,
            0.0,
            Some(&mask),
            Some(&pred_eq("resource_scope_id", "a")),
            false,
        )
        .unwrap();
    let keys: Vec<&str> = hits.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        keys,
        vec!["d2"],
        "mask ∩ where must not widen, got {hits:?}"
    );
}

/// A node matching label and where but absent from the mask must not appear
/// (`nodes_with_label` does not apply the mask).
#[test]
fn find_similar_label_and_mask_and_where() {
    let dir = tmp("where-label-mask");
    let mut db = GraphDb::open(&dir).unwrap();
    insert_doc(&mut db, "d1", Some("a"), &[1.0, 0.0]);
    insert_doc(&mut db, "d2", Some("a"), &[0.95, 0.05]);
    db.insert_node(
        "Other",
        "o1",
        vec![
            ("emb".into(), emb(&[1.0, 0.0])),
            ("resource_scope_id".into(), Value::Str("a".into())),
        ],
    )
    .unwrap();
    let mask = NodeMask::from_keys(&db, ["d1", "o1"]);
    let hits = db
        .find_similar_vector_filtered(
            "emb",
            Some("Document"),
            &[1.0, 0.0],
            10,
            0.0,
            Some(&mask),
            Some(&pred_eq("resource_scope_id", "a")),
            false,
        )
        .unwrap();
    let keys: Vec<&str> = hits.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        keys,
        vec!["d1"],
        "d2 matches label+where but is not in mask; o1 is wrong label; got {hits:?}"
    );
}

#[test]
fn find_similar_where_uses_property_index_when_enabled() {
    let dir = tmp("where-index");
    let mut db = GraphDb::open(&dir).unwrap();
    insert_doc(&mut db, "d1", Some("a"), &[1.0, 0.0]);
    insert_doc(&mut db, "d2", Some("a"), &[0.8, 0.2]);
    insert_doc(&mut db, "d3", Some("b"), &[1.0, 0.0]);
    let pred = pred_eq("resource_scope_id", "a");
    let q = [1.0, 0.0];
    let scan = db
        .find_similar_vector_filtered(
            "emb",
            Some("Document"),
            &q,
            10,
            0.0,
            None,
            Some(&pred),
            false,
        )
        .unwrap();
    db.enable_index("Document", "resource_scope_id").unwrap();
    assert!(db.is_index_enabled("Document", "resource_scope_id"));
    let indexed = db
        .find_similar_vector_filtered(
            "emb",
            Some("Document"),
            &q,
            10,
            0.0,
            None,
            Some(&pred),
            false,
        )
        .unwrap();
    assert_eq!(scan, indexed, "indexed where eq must match the scan");
}

#[test]
fn find_similar_where_invalid_is_refused() {
    let dir = tmp("where-invalid");
    let db = GraphDb::open(&dir).unwrap();
    let pred = PropPredicate {
        field: "resource_scope_id".into(),
        eq: Some(Value::Str("a".into())),
        in_: Some(vec![Value::Str("b".into())]),
    };
    let err = db
        .find_similar_vector_filtered(
            "emb",
            Some("Document"),
            &[1.0, 0.0],
            10,
            0.0,
            None,
            Some(&pred),
            false,
        )
        .expect_err("both eq and in must be refused");
    match err {
        GraphError::QueryError { detail } => {
            assert!(
                detail.contains("where"),
                "QueryError must use validate_named(\"where\"), got {detail}"
            );
            assert!(
                detail.contains("both"),
                "QueryError must name both eq and in, got {detail}"
            );
        }
        other => panic!("expected QueryError, got {other:?}"),
    }
}

/// Approximate rule present: `exact=true` does not consult HNSW; `exact=false`
/// does.
#[test]
fn exact_true_skips_hnsw() {
    let (mut db, _nodes) = seed_pairwise("exact-true-hnsw", 16, 8);
    db.create_rule(sim_rule()).unwrap();
    assert!(db.has_vector_rule("emb"));
    let q = fill_vec(99, 8);
    core_rules::hnsw_search_count_reset();
    let exact = db
        .find_similar_vector_filtered("emb", Some("Item"), &q, 5, 0.0, None, None, true)
        .unwrap();
    assert_eq!(
        core_rules::hnsw_search_count(),
        0,
        "exact=true must not call hnsw_search"
    );
    core_rules::hnsw_search_count_reset();
    let approx = db.find_similar_vector("emb", Some("Item"), &q, 5, 0.0);
    assert!(
        core_rules::hnsw_search_count() > 0,
        "exact=false with a covering rule must use HNSW"
    );
    let brute = db
        .find_similar_vector_filtered("emb", Some("Item"), &q, 5, 0.0, None, None, true)
        .unwrap();
    assert_eq!(exact, brute);
    let _ = approx;
}
