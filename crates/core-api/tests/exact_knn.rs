//! Exact kNN integration tests (GEMM brute path).
//!
//! Nested-loop cosine lives **in this file**. Do not call `exact_knn::`.

use core_api::{GraphDb, Value};

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
