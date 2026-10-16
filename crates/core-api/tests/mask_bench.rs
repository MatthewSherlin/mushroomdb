//! What the mask actually costs on the paths §5.12 names, measured in the
//! crate that owns them.
//!
//! This release put `NodeMask::contains_id` in the inner loop of nine more
//! surfaces. §5.12 proposed a density-chosen representation for the allow-list
//! behind it, and this file is the measurement that decided it: it times
//! `query_masked`, `find_similar_vector_masked` and `neighborhood_masked`
//! against masks of 2,000 and 50,000 visible ids over a 200,000-node store —
//! the two sizes that straddle [`core_query::VisibleSet`]'s threshold, so one
//! mask is a `HashSet` and the other a bitset. The before/after table that came
//! out of it is in that type's module documentation; the short version is −29%
//! on `query_masked` and −22% on masked `find_similar` at 50,000 visible ids,
//! and nothing either way at 2,000, which is the size the rule leaves alone.
//!
//! It also times `contains_id` on its own and `NodeMask::intersect`, because a
//! whole-path figure cannot say whether a path is slow *at the probe*, and the
//! per-path share is what decides whether a faster probe is worth a second
//! representation. **Read the bare-probe leg as a floor, not as a delta**: it
//! calls `contains_id` across a crate boundary, so its before/after difference
//! also contains whatever the inliner decided about that call. The three real
//! legs above it do not have that problem — they call the mask from inside the
//! crate that owns them, which is the whole reason this harness lives here and
//! not in a benchmark crate of its own.
//!
//! `#[ignore]`, because it builds 200,000 nodes and prints machine-dependent
//! timings. It still asserts, so it is a test and not a stopwatch: every key
//! each masked leg returns is visible, the row counts are the ones the mask
//! implies, and every repetition returns identical results — so a
//! representation change that alters hits or ordering fails here rather than
//! quietly shipping.
//!
//! Run it:
//!
//! ```text
//! cargo test -p mushroomdb --release --test mask_bench -- --ignored --nocapture
//! MASK_BENCH_N=50000 MASK_BENCH_REPS=3 \
//!   cargo test -p mushroomdb --release --test mask_bench -- --ignored --nocapture
//! ```
//!
//! `MASK_BENCH_N` sets the store size (default 200,000), `MASK_BENCH_REPS` the
//! repetitions per leg (default 5, median reported). One run is not a result:
//! at 5 repetitions the legs move about 10% between processes, which was enough
//! to make `neighborhood_masked` look like a 9% regression it is not. The table
//! in [`core_query::VisibleSet`] is five processes of 51 repetitions, medians of
//! the medians, and anything quoted from this file should be read the same way.

use core_storage::fs::RealFs;
use mushroomdb::{Dir, GraphDb, NodeMask, Value};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

type Db = GraphDb<RealFs>;

/// Vector width. Small on purpose: the leg is here to exercise the per-
/// candidate mask probe, and a wide vector would bury it under arithmetic.
const DIM: usize = 16;
/// Out-edges per node.
const FANOUT: usize = 8;
/// The two mask sizes §5.12 names.
const MASK_SIZES: [usize; 2] = [2_000, 50_000];
/// Nodes per cluster — see [`build`] for why the graph has clusters at all.
const CLUSTER: usize = 1_000;

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// A deterministic 32-bit mix, so the graph and the vectors are the same on
/// every run and on every machine.
fn mix(mut x: u64) -> u64 {
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    x ^ (x >> 33)
}

fn emb(i: usize) -> Value {
    Value::List(
        (0..DIM)
            .map(|d| {
                let r = mix(i as u64 * 31 + d as u64) as f64 / u64::MAX as f64;
                Value::Float(r * 2.0 - 1.0)
            })
            .collect(),
    )
}

/// Build the store once: `n` nodes labelled `N`, each with a vector, joined by
/// a `FANOUT`-regular digraph. Snapshotted and reopened so the reads take the
/// mmap'd-base branch rather than the in-memory overlay.
///
/// # Why the graph is clustered
///
/// Edges stay inside a cluster, and [`mask_keys`] makes masks out of whole
/// clusters. A uniformly random graph would make the `neighborhood_masked`
/// leg meaningless at the sparse size: with 2,000 of 200,000 ids visible, a
/// node's sixteen neighbours are visible with probability 0.01 each, so the
/// BFS dies at the start node and the leg times nothing. Clustering makes the
/// expansion run to its full depth at *both* mask sizes, over the same
/// neighbourhood, so the two rows of the table differ in the mask and in
/// nothing else.
///
/// Clusters are strided, not contiguous — node `i` belongs to cluster
/// `i % (n / CLUSTER)` — so a cluster's ids are spread across the whole id
/// space and the label scan still probes a scattered allow-list.
fn build(dir: &std::path::Path, n: usize) -> Db {
    let mut db = Db::open(dir).unwrap();
    // Chunked so one batch does not hold a million queued ops in memory.
    const CHUNK: usize = 10_000;
    for lo in (0..n).step_by(CHUNK) {
        let hi = (lo + CHUNK).min(n);
        let mut b = db.batch();
        for i in lo..hi {
            b.insert_node(
                "N",
                &format!("n{i}"),
                vec![
                    ("v".to_string(), emb(i)),
                    ("i".to_string(), Value::Int(i as i64)),
                ],
            );
        }
        b.commit().unwrap();
    }
    let nclust = n / CLUSTER;
    for lo in (0..n).step_by(CHUNK) {
        let hi = (lo + CHUNK).min(n);
        let mut b = db.batch();
        for i in lo..hi {
            let c = i % nclust;
            for f in 0..FANOUT {
                let j = (mix(i as u64 * 7 + f as u64) % CLUSTER as u64) as usize;
                let dst = c + j * nclust;
                if dst != i {
                    b.insert_edge("E", &format!("n{i}"), &format!("n{dst}"));
                }
            }
        }
        b.commit().unwrap();
    }
    db.snapshot().unwrap();
    drop(db);
    Db::open(dir).unwrap()
}

/// The keys of the first `want / CLUSTER` clusters — `want` keys, spread
/// across the whole id space, always including `n0`.
///
/// Whole clusters, so the `neighborhood_masked` leg expands through a fully
/// visible neighbourhood at every mask size; see [`build`].
fn mask_keys(n: usize, want: usize) -> Vec<String> {
    let nclust = n / CLUSTER;
    (0..want / CLUSTER)
        .flat_map(|c| (0..CLUSTER).map(move |j| format!("n{}", c + j * nclust)))
        .collect()
}

fn median(mut xs: Vec<Duration>) -> Duration {
    xs.sort();
    xs[xs.len() / 2]
}

/// Time `f` `reps` times and return the median together with the last result,
/// after asserting every repetition produced the same one.
fn time<T: PartialEq + std::fmt::Debug>(reps: usize, mut f: impl FnMut() -> T) -> (Duration, T) {
    // One untimed warm-up so the first run's page faults are not the median.
    let first = f();
    let mut times = Vec::with_capacity(reps);
    let mut last = None;
    for _ in 0..reps {
        let t0 = Instant::now();
        let out = f();
        times.push(t0.elapsed());
        assert_eq!(out, first, "a repetition returned a different result");
        last = Some(out);
    }
    (median(times), last.unwrap())
}

fn us(d: Duration) -> f64 {
    d.as_secs_f64() * 1e6
}

#[test]
#[ignore = "builds a 200k-node store and prints machine-dependent timings; run it with --ignored"]
fn mask_paths_measured() {
    let n = env_usize("MASK_BENCH_N", 200_000);
    let reps = env_usize("MASK_BENCH_REPS", 5);
    assert!(
        n.is_multiple_of(CLUSTER) && n / CLUSTER >= 2,
        "MASK_BENCH_N must be a multiple of {CLUSTER} and hold at least two clusters"
    );
    let sizes: Vec<usize> = MASK_SIZES
        .iter()
        .copied()
        .filter(|&s| s <= n && s.is_multiple_of(CLUSTER))
        .collect();
    assert!(!sizes.is_empty(), "MASK_BENCH_N is smaller than every mask");

    let tmp = std::env::temp_dir().join(format!("mask-bench-{n}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let t0 = Instant::now();
    let db = build(&tmp, n);
    println!(
        "\nbuilt {n} nodes, fanout {FANOUT}, in {:.1}s",
        t0.elapsed().as_secs_f64()
    );
    // The real rule is `len * 64 >= span`, where `span` is one past the largest
    // visible id; a test outside the crate cannot see dense ids, so it reads the
    // rule against `n`. Every mask here reaches the last cluster of the store,
    // which puts its span within 0.1% of `n` and gives the same answer at both
    // sizes — far from the crossover in both directions.
    println!(
        "threshold rule: dense when visible*64 >= span (span ≈ {n} for every \
         mask below), so {} ids is the crossover",
        n.div_ceil(64)
    );

    let params: BTreeMap<String, Value> = BTreeMap::new();
    let q: Vec<f64> = match emb(7) {
        Value::List(xs) => xs
            .into_iter()
            .map(|v| match v {
                Value::Float(f) => f,
                _ => unreachable!(),
            })
            .collect(),
        _ => unreachable!(),
    };

    let mut nb_reach: Option<Vec<String>> = None;
    for want in sizes {
        let keys = mask_keys(n, want);
        let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        let mask = NodeMask::from_keys(&db, key_refs.iter().copied());
        let visible: std::collections::HashSet<String> = keys.iter().cloned().collect();
        assert_eq!(mask.len(), want, "every bench key must resolve");
        let variant = if mask.len() * 64 >= n {
            "Dense"
        } else {
            "Sparse"
        };

        println!("\n── |visible| = {want} of {n} ({variant} under the rule above) ──");

        // 1. query_masked: a full label scan, one mask probe per node.
        let (t_query, rows) = time(reps, || {
            let rs = db
                .query_masked("MATCH (x:N) RETURN x.i", &params, &mask)
                .unwrap();
            rs.len()
        });
        assert_eq!(rows, want, "the scan must return exactly the visible nodes");
        println!(
            "query_masked (label scan of {n})   {:>10.1} us",
            us(t_query)
        );

        // 2. find_similar_vector_masked: brute force, one probe per candidate.
        let (t_vec, hits) = time(reps, || {
            db.find_similar_vector_masked("v", Some("N"), &q, 10, -1.0, &mask)
        });
        assert_eq!(hits.len(), 10, "k visible hits");
        for (k, _) in &hits {
            assert!(
                visible.contains(k),
                "find_similar returned a hidden key {k}"
            );
        }
        println!("find_similar_vector_masked (k=10)  {:>10.1} us", us(t_vec));

        // 3. neighborhood_masked: one probe per neighbour of every visited node.
        let start = &keys[0];
        let (t_nb, nb_rows) = time(reps, || {
            let rs = db
                .neighborhood_masked(start, 4, None, Dir::Both, &mask)
                .expect("the start key is visible");
            (0..rs.len())
                .map(|i| match rs.get(i, "key") {
                    Some(Value::Str(s)) => s.clone(),
                    other => panic!("key column is not a string: {other:?}"),
                })
                .collect::<Vec<String>>()
        });
        for k in &nb_rows {
            assert!(
                visible.contains(k),
                "neighborhood returned a hidden key {k}"
            );
        }
        // The expansion is confined to cluster 0, which every mask contains
        // whole, so the two mask sizes must reach exactly the same nodes. A
        // leg that reached a different number at a different mask size would
        // not be measuring the mask.
        match nb_reach {
            None => nb_reach = Some(nb_rows.clone()),
            Some(ref first) => assert_eq!(
                *first, nb_rows,
                "the BFS reached different nodes under a different mask size"
            ),
        }
        assert!(
            nb_rows.len() > CLUSTER / 2,
            "the BFS should cover most of its cluster; got {} rows",
            nb_rows.len()
        );
        println!(
            "neighborhood_masked (depth 4)      {:>10.1} us  ({} rows)",
            us(t_nb),
            nb_rows.len()
        );

        // 4. The probe alone, at the rate the scans call it. This is the share
        //    a faster representation could remove, and nothing above it.
        let ids: Vec<u32> = (0..n as u32).collect();
        let (t_probe, seen) = time(reps, || {
            ids.iter().filter(|&&id| mask.contains_id(id)).count()
        });
        assert_eq!(seen, want);
        println!(
            "contains_id x {n} (bare)       {:>10.1} us  ({:.1} ns/probe)",
            us(t_probe),
            us(t_probe) * 1e3 / n as f64
        );
        println!(
            "   → the probe is {:.1}% of query_masked, {:.1}% of find_similar",
            100.0 * us(t_probe) / us(t_query),
            100.0 * us(t_probe) / us(t_vec)
        );

        // 5. intersect, which §5.1 runs on every multi-leg scope.
        let other = NodeMask::from_keys(&db, key_refs.iter().copied());
        let (t_isect, isect_len) = time(reps, || mask.intersect(&other).len());
        assert_eq!(isect_len, want);
        println!("intersect (self, {want} ids)  {:>10.1} us", us(t_isect));
    }

    let _ = std::fs::remove_dir_all(&tmp);
}
