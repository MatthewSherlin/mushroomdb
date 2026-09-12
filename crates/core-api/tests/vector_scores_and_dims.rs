//! Two properties of the approximate vector path that the `f32` index made
//! load-bearing:
//!
//! 1. **The index supplies candidates, not scores.** Its distances are `f32`, so
//!    an exact duplicate scores 0.9999999 there. Every score this API reports,
//!    and every `min` it applies, has to come from the `f64` property vectors —
//!    otherwise `min = 1.0` finds nothing and the index and the brute-force scan
//!    disagree in the sixth decimal.
//! 2. **A slab has one stride**, so an embedding of another dimension cannot be
//!    indexed. That must never cost a real edge: a stray 3-element vector
//!    arriving *first* would otherwise elect the stride and refuse the whole
//!    corpus, leaving a non-empty index that answers nothing.

use core_api::{Direction, GraphDb, Predicate, RuleDef, Value};
use core_storage::fs::RealFs;

type Db = GraphDb<RealFs>;

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn emb(xs: &[f64]) -> Value {
    Value::List(xs.iter().copied().map(Value::Float).collect())
}

fn rule(approximate: bool) -> RuleDef {
    RuleDef {
        name: "sim".into(),
        src_label: "Doc".into(),
        dst_label: "Doc".into(),
        predicate: Predicate::VectorSimilar {
            field: "emb".into(),
            min: 0.9,
        },
        edge_type: "SIM".into(),
        weight_prop: None,
        max_edges: None,
        approximate,
        via_label: None,
        via_edge: None,
        via_dir: None,
    }
}

/// Six 2-D vectors, two of them **exactly equal**: `d0` and `d5` both sit at
/// `[0.98, 0.2]`, whose normalised self-dot is exactly 1.0 in `f64` and
/// 0.99999996 in `f32`. That gap is the whole point of the first test.
const DOCS: &[(&str, [f64; 2])] = &[
    ("d0", [0.98, 0.2]),
    ("d1", [1.0, 0.0]),
    ("d2", [0.0, 1.0]),
    ("d3", [-0.2, 0.98]),
    ("d4", [0.6, 0.8]),
    ("d5", [0.98, 0.2]),
];

/// Insertion order is what the index sees, because the backfill walks nodes in id
/// order and ids follow insertion. `strays` gives the positions in that order at
/// which a 3-element vector is inserted: `[0]` puts one ahead of every real
/// vector, `[1]` between the first and the second, `[0, 0]` two ahead of
/// everything, `[DOCS.len()]` after them all.
fn seed(dir: &std::path::Path, with_rule: Option<bool>, strays: &[usize]) -> Db {
    let mut db = GraphDb::open(dir).unwrap();
    let mut next_stray = 0usize;
    for pos in 0..=DOCS.len() {
        for _ in strays.iter().filter(|&&p| p == pos) {
            db.insert_node(
                "Doc",
                &format!("stray{next_stray}"),
                vec![("emb".into(), emb(&[1.0, 2.0, 3.0]))],
            )
            .unwrap();
            next_stray += 1;
        }
        if let Some((k, v)) = DOCS.get(pos) {
            db.insert_node("Doc", k, vec![("emb".into(), emb(v))])
                .unwrap();
        }
    }
    if let Some(approximate) = with_rule {
        db.create_rule(rule(approximate)).unwrap();
    }
    db
}

/// Every key's outgoing `SIM` edges, the real ones and both possible strays. A
/// key that does not exist yields an empty list on both sides of a comparison, so
/// listing all of them keeps the map comparable across fixtures.
fn edge_map(db: &Db) -> Vec<(String, Vec<String>)> {
    DOCS.iter()
        .map(|(k, _)| k.to_string())
        .chain(["stray0".to_string(), "stray1".to_string()])
        .map(|k| {
            let mut ns = db.neighbors(&k, "SIM", Direction::Out).unwrap_or_default();
            ns.sort();
            (k, ns)
        })
        .collect()
}

/// `min = 1.0` must find an exact duplicate through the index.
///
/// The index's own similarity for `d0` against its own vector is 0.99999996, so
/// filtering on it returns nothing at all; the exact `f64` cosine is 1.0. The
/// index is only allowed to choose candidates.
#[test]
fn an_exact_duplicate_is_found_at_min_one() {
    let dir = tmp("vec-dup-min-one");
    let db = seed(&dir, Some(true), &[]);
    assert!(
        db.has_vector_rule("emb"),
        "fixture: the ANN path must exist"
    );

    core_rules::hnsw_search_count_reset();
    let hits = db.find_similar_vector("emb", Some("Doc"), &[0.98, 0.2], 5, 1.0);
    assert!(
        core_rules::hnsw_search_count() > 0,
        "fixture: this must be the index path, not the brute-force scan"
    );

    let keys: Vec<&str> = hits.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        keys,
        vec!["d0", "d5"],
        "both exact duplicates must come back at min = 1.0; got {hits:?}"
    );
    for (k, sim) in &hits {
        assert_eq!(*sim, 1.0, "{k} scored {sim}, not an exact 1.0");
    }
}

/// The index path and the brute-force path must report the same keys and the
/// same scores, bit for bit. Same corpus, one database with an approximate rule
/// and one with no rule at all.
#[test]
fn index_and_brute_force_agree_on_scores() {
    let ann = seed(&tmp("vec-agree-ann"), Some(true), &[]);
    let scan = seed(&tmp("vec-agree-scan"), None, &[]);
    assert!(!scan.has_vector_rule("emb"), "fixture: no ANN path here");

    for q in [
        vec![0.98, 0.2],
        vec![1.0, 0.0],
        vec![0.0, 1.0],
        vec![0.6, 0.8],
        vec![-0.5, 0.5],
    ] {
        for min in [0.0, 0.5, 0.9, 0.99, 1.0] {
            core_rules::hnsw_search_count_reset();
            let a = ann.find_similar_vector("emb", Some("Doc"), &q, 6, min);
            assert!(
                core_rules::hnsw_search_count() > 0,
                "fixture: q={q:?} was not answered by the index"
            );
            core_rules::hnsw_search_count_reset();
            let b = scan.find_similar_vector("emb", Some("Doc"), &q, 6, min);
            assert_eq!(
                core_rules::hnsw_search_count(),
                0,
                "fixture: the no-rule database used an index"
            );

            assert_eq!(
                a.iter().map(|(k, _)| k).collect::<Vec<_>>(),
                b.iter().map(|(k, _)| k).collect::<Vec<_>>(),
                "q={q:?} min={min}: the two paths returned different nodes"
            );
            for ((ka, sa), (_, sb)) in a.iter().zip(b.iter()) {
                assert_eq!(
                    sa.to_bits(),
                    sb.to_bits(),
                    "q={q:?} min={min}: {ka} scored {sa} through the index and \
                     {sb} through the scan — the index's f32 similarity leaked \
                     into the answer"
                );
            }
        }
    }
}

/// A 3-element stray ingested **before** the real corpus must not cost a single
/// edge. The stride is elected from the first vector, so the index re-elects it
/// and evicts the stray; the stray itself can never be an edge, because
/// `VectorSimilar` refuses a pair of unequal length.
#[test]
fn a_stray_dimension_first_still_derives_every_real_edge() {
    let approx = seed(&tmp("vec-stray-first-approx"), Some(true), &[0]);
    let exact = seed(&tmp("vec-stray-first-exact"), Some(false), &[0]);

    assert_eq!(
        edge_map(&approx),
        edge_map(&exact),
        "the approximate rule lost edges to a stray-dimension first vector"
    );
    assert!(
        edge_map(&exact)
            .iter()
            .any(|(_, ns)| ns.contains(&"d5".to_string())),
        "fixture: the exact rule must derive some edges, or this proves nothing"
    );
    assert_eq!(
        edge_map(&approx)
            .iter()
            .find(|(k, _)| k == "stray0")
            .map(|(_, ns)| ns.len()),
        Some(0),
        "a vector of another dimension cannot be similar to anything"
    );

    // And the index is still the fast path: the eviction corrected the stride
    // rather than leaving the index incomplete.
    core_rules::hnsw_search_count_reset();
    let hits = approx.find_similar_vector("emb", Some("Doc"), &[0.98, 0.2], 3, 0.5);
    assert!(
        core_rules::hnsw_search_count() > 0,
        "after re-electing the stride the index must still serve queries"
    );
    assert_eq!(hits.first().map(|(k, _)| k.as_str()), Some("d0"));
}

/// A stray arriving **after** the stride has settled is refused, which leaves
/// the index missing a vector it was offered — so every query falls back to the
/// exhaustive scan, and the edge set is still exactly the exact rule's.
#[test]
fn a_stray_dimension_later_still_derives_every_real_edge() {
    let approx = seed(&tmp("vec-stray-late-approx"), Some(true), &[DOCS.len()]);
    let exact = seed(&tmp("vec-stray-late-exact"), Some(false), &[DOCS.len()]);

    assert_eq!(
        edge_map(&approx),
        edge_map(&exact),
        "the approximate rule lost edges to a stray-dimension late vector"
    );

    core_rules::hnsw_search_count_reset();
    let hits = approx.find_similar_vector("emb", Some("Doc"), &[0.98, 0.2], 3, 0.5);
    assert_eq!(
        core_rules::hnsw_search_count(),
        0,
        "an index that refused a vector must hand the query to the scan"
    );
    assert_eq!(
        hits.first().map(|(k, _)| k.as_str()),
        Some("d0"),
        "and the scan must still answer: {hits:?}"
    );
}

/// A query of the wrong dimension is not the index's to answer. It must reach
/// the brute-force scan rather than come back empty.
#[test]
fn a_wrong_dimension_query_falls_back_to_the_scan() {
    let db = seed(&tmp("vec-wrong-dim-query"), Some(true), &[]);
    core_rules::hnsw_search_count_reset();
    let hits = db.find_similar_vector("emb", Some("Doc"), &[1.0, 2.0, 3.0], 3, 0.0);
    assert_eq!(
        core_rules::hnsw_search_count(),
        0,
        "a 3-D query must not be put to a 2-D index"
    );
    assert!(
        !hits.is_empty(),
        "the scan compares what it can and answers; got {hits:?}"
    );
}

/// The order that the first attempt at re-election got wrong: a real vector, a
/// stray, then the rest. The stray displaces `d0` and the next real vector
/// displaces the stray, so `d0` has to come back — otherwise it is missing from
/// an index that believes itself complete, and two derived edges and one exact
/// duplicate vanish with it.
#[test]
fn a_stray_dimension_between_real_vectors_still_derives_every_real_edge() {
    let approx = seed(&tmp("vec-stray-mid-approx"), Some(true), &[1]);
    let exact = seed(&tmp("vec-stray-mid-exact"), Some(false), &[1]);

    assert_eq!(
        edge_map(&approx),
        edge_map(&exact),
        "the approximate rule lost edges to a stray between two real vectors — \
         the vector the stray displaced was not put back"
    );

    // `d0` is the displaced one, and it is `d5`'s exact duplicate. Both must come
    // back at min = 1.0, and from the index: once the stride is re-elected and
    // `d0` re-inserted, nothing of 2 dimensions is missing, so the fast path is
    // sound again.
    core_rules::hnsw_search_count_reset();
    let hits = approx.find_similar_vector("emb", Some("Doc"), &[0.98, 0.2], 5, 1.0);
    assert!(
        core_rules::hnsw_search_count() > 0,
        "after the re-insertion the index must serve the query, not brute force"
    );
    assert_eq!(
        hits.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
        vec!["d0", "d5"],
        "the displaced duplicate must be back; got {hits:?}"
    );
    for (k, sim) in &hits {
        assert_eq!(*sim, 1.0, "{k} scored {sim}");
    }
}

/// Two strays ahead of the corpus is past what a single-sample correction can
/// fix: the stride settles on 3, the real vectors are refused, and the rule has
/// to answer through the exhaustive scan. Correct, and slower.
#[test]
fn two_strays_ahead_of_the_corpus_fall_back_to_the_scan() {
    let approx = seed(&tmp("vec-two-strays-approx"), Some(true), &[0, 0]);
    let exact = seed(&tmp("vec-two-strays-exact"), Some(false), &[0, 0]);

    assert_eq!(
        edge_map(&approx),
        edge_map(&exact),
        "two strays ahead of the corpus cost the approximate rule its edges"
    );

    core_rules::hnsw_search_count_reset();
    let hits = approx.find_similar_vector("emb", Some("Doc"), &[0.98, 0.2], 5, 1.0);
    assert_eq!(
        core_rules::hnsw_search_count(),
        0,
        "an index that refused the whole corpus must not be asked"
    );
    assert_eq!(
        hits.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
        vec!["d0", "d5"],
        "and the scan answers exactly: {hits:?}"
    );
}
