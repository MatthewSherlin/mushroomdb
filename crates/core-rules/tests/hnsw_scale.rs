//! Scale benchmark for the in-tree HNSW index.
//!
//! Lives at the index level, not the rule-engine level, so what it measures is
//! the graph and not the surrounding write path. It is a wall-clock
//! measurement and therefore stays out of CI: `recall-gates` exists for floors
//! that hold on any machine, and this is not one of those.
//!
//! Run it with:
//!
//! ```sh
//! MUSHROOMDB_BENCH_HNSW=1 cargo test --release -p mushroomdb-rules \
//!   --test hnsw_scale -- --ignored --nocapture
//! ```

use core_rules::hnsw::{make_unit_vecs, HnswIndex};
use core_rules::index::fnv1a_u64;
use std::time::{Duration, Instant};

/// Insert cost at 2k / 10k / 50k, 1,536-D. Prints a table and asserts the
/// growth is sub-quadratic, that the 50k case finishes, and that the cost of
/// re-embedding one node does not grow with the index.
///
/// Gated twice: `#[ignore]` keeps it out of `cargo test --workspace`, and
/// `MUSHROOMDB_BENCH_HNSW=1` keeps it out of an `--ignored` sweep that did not
/// mean to spend ten minutes.
#[test]
#[ignore = "slow: builds a 50k-vector index; set MUSHROOMDB_BENCH_HNSW=1"]
fn hnsw_insert_cost_is_sublinear_per_vector() {
    if std::env::var("MUSHROOMDB_BENCH_HNSW").as_deref() != Ok("1") {
        // Not a pass. `--ignored` already keeps this out of a normal run, and
        // anyone who sweeps `--ignored` without meaning to spend ten minutes
        // should see why nothing happened rather than a green tick.
        println!(
            "SKIPPED hnsw_insert_cost_is_sublinear_per_vector: set \
             MUSHROOMDB_BENCH_HNSW=1 to run it (~20 min)"
        );
        return;
    }
    const DIM: usize = 1_536;
    let sizes = [2_000usize, 10_000, 50_000];
    let mut build = Vec::new(); // total build time per size
    let mut update = Vec::new(); // mean time per single re-insert at that size

    println!("params: {:?}", core_rules::hnsw::hnsw_params());
    for &n in &sizes {
        let vecs = make_unit_vecs(n, DIM, 0xB0A7_5EED);
        let mut idx = HnswIndex::new(fnv1a_u64(b"bench"));
        let t0 = Instant::now();
        for (i, v) in vecs.iter().enumerate() {
            idx.insert(i as u32, v);
        }
        build.push(t0.elapsed());

        // Per-write cost once the index exists: re-insert 100 existing ids,
        // which is exactly what an embedding change costs.
        let t1 = Instant::now();
        for i in (0..n).step_by(n / 100).take(100) {
            idx.insert(i as u32, &vecs[i]);
        }
        update.push(t1.elapsed() / 100);

        let mem = idx.memory_stats();
        println!(
            "n={n:>6}  build {:>10.2?}  per-insert {:>10.3?}  update {:>10.3?}  \
             adjacency {:>7.1} B/node  total {:>9.1} B/node",
            build[build.len() - 1],
            build[build.len() - 1] / n as u32,
            update[update.len() - 1],
            mem.adjacency_bytes_per_node(),
            mem.bytes_per_node(),
        );

        // (0) The memory claim, asserted rather than printed. Adjacency is
        //     bounded by `2 * (m0 + m) * 4` bytes — forward entries by `m0` on
        //     layer 0 plus `m` per layer above, and the reverse index by one
        //     entry per (source, target) pair — and the vector half is exact
        //     arithmetic at `DIM` f32s, the index's own copy of each vector.
        let p = core_rules::hnsw::hnsw_params();
        let adjacency_ceiling = 2.0 * (p.m0 + p.m) as f64 * 4.0;
        assert!(
            mem.adjacency_bytes_per_node() <= adjacency_ceiling,
            "n={n}: adjacency {:.1} B/node exceeds the {adjacency_ceiling:.1} B/node \
             ceiling for m0={} m={}",
            mem.adjacency_bytes_per_node(),
            p.m0,
            p.m
        );
        assert!(
            mem.bytes_per_node() <= adjacency_ceiling + (DIM * 4) as f64,
            "n={n}: total {:.1} B/node exceeds adjacency ceiling plus the vector \
             ({DIM} f32s — the index's own copy, the store keeps the f64s)",
            mem.bytes_per_node()
        );
    }

    // (1) Sub-quadratic build. Each step multiplies n by 5. Linear would cost
    //     5x; quadratic 25x. 8x is comfortably sub-quadratic and leaves room
    //     for the log(N) descent term and for CI noise.
    let r1 = build[1].as_secs_f64() / build[0].as_secs_f64();
    let r2 = build[2].as_secs_f64() / build[1].as_secs_f64();
    assert!(
        r1 < 8.0,
        "2k→10k build grew {r1:.2}x for 5x the vectors; must be < 8x"
    );
    assert!(
        r2 < 8.0,
        "10k→50k build grew {r2:.2}x for 5x the vectors; must be < 8x"
    );

    // (2) The case that did not finish in ten minutes must finish in five.
    assert!(
        build[2] < Duration::from_secs(300),
        "50k build took {:?}; must be under 5 minutes",
        build[2]
    );

    // (3) A changed embedding must not cost a scan of the index. Flat, not
    //     growing: the 50k update must be within 3x the 2k update.
    let u = update[2].as_secs_f64() / update[0].as_secs_f64();
    assert!(
        u < 3.0,
        "update cost grew {u:.2}x from 2k to 50k. An update is a remove plus an \
         insert, and removal is pinned at O(in-degree) by \
         remove_touches_only_the_nodes_that_list_it — so this is the *insert* \
         growing with n, the same term assertion (1) measures, not a removal scan."
    );
    assert!(
        update[2] < Duration::from_millis(25),
        "a 50k embedding update took {:?}; must be under 25 ms",
        update[2]
    );
}

/// Distance evaluations per insert at 1,000 and 5,000 nodes.
///
/// A **count**, not a wall clock, so it holds on any machine — which is what
/// makes it the gate that notices the insert path doing more work per vector as
/// the index grows. Nothing timing-free noticed that before.
///
/// Dimension 32 deliberately: the evaluation count is a property of the graph —
/// the beam's reach and the prune's candidate walk — and not of the dimension,
/// so paying 1,536-D for it would buy nothing but minutes.
///
/// Run with:
/// ```sh
/// cargo test --release -p mushroomdb-rules --features test-hooks \
///   -- dist_evals_per_insert_is_bounded --ignored --nocapture
/// ```
///
/// Compiled only with `test-hooks`, which is what exposes the counters. Without
/// it this file must still build, because `cargo test -p mushroomdb-rules` — what
/// CI's other `recall-gates` lines run — enables no features of its own.
#[cfg(feature = "test-hooks")]
#[test]
#[ignore = "slow: builds a 1k and a 5k index"]
fn dist_evals_per_insert_is_bounded() {
    const DIM: usize = 32;
    const PROBES: u64 = 20;
    let sizes = [1_000usize, 5_000];
    let mut per_insert = Vec::new();

    println!("params: {:?}", core_rules::hnsw::hnsw_params());
    for &n in &sizes {
        let vecs = make_unit_vecs(n + PROBES as usize, DIM, 0xE7A1_5EED);
        let mut idx = HnswIndex::new(fnv1a_u64(b"dist-evals"));
        for (i, v) in vecs.iter().take(n).enumerate() {
            idx.insert(i as u32, v);
        }

        core_rules::hnsw_dist_evals_reset();
        let t0 = Instant::now();
        for (i, v) in vecs.iter().enumerate().skip(n) {
            idx.insert(i as u32, v);
        }
        let elapsed = t0.elapsed();
        let total = core_rules::hnsw_dist_evals();
        let pairwise = core_rules::hnsw_dist_evals_pairwise();
        let beam = total - pairwise;
        println!(
            "n={n:>6}  evals/insert {:>9.1}  (pairwise {:>9.1}  beam+descent {:>8.1})  \
             per-insert {:>9.3?}  ns/eval {:>6.0}",
            total as f64 / PROBES as f64,
            pairwise as f64 / PROBES as f64,
            beam as f64 / PROBES as f64,
            elapsed / PROBES as u32,
            elapsed.as_nanos() as f64 / total as f64,
        );
        per_insert.push(total as f64 / PROBES as f64);
    }

    // Measured 12 739.5 at 1k and 23 901.7 at 5k, a ratio of 1.876 — and
    // identical before and after the f32 slab, because the slab changed no
    // decision the graph makes. Both ceilings are those numbers plus 50 %.
    //
    // The count climbs because both terms that make it are still short of their
    // bound: at 1 000 nodes the beam cannot reach more than 1 000, and the
    // prune's candidate walk gets more expensive as the corpus densifies
    // (pairwise is 92 % of the 1k count and 84 % of the 5k one). Both saturate
    // at `ef_construction × m0`; what this gate exists to catch is a count that
    // does not saturate, i.e. an insert path that has become a scan.
    let ratio = per_insert[1] / per_insert[0];
    println!("5k/1k evals-per-insert ratio: {ratio:.3}");
    assert!(
        per_insert[1] < 36_000.0,
        "5k insert evaluated {:.1} distances; the ceiling is 36,000 (measured \
         23,901.7 plus 50 %)",
        per_insert[1]
    );
    assert!(
        ratio < 2.8,
        "evals per insert grew {ratio:.3}x from 1k to 5k; the ceiling is 2.8x \
         (measured 1.876 plus 50 %)"
    );
}

/// Memory the graph holds per indexed vector, at 5,000 × 1,536-D.
///
/// Separate from the benchmark above because it is cheap enough to run on its
/// own, and because the before/after comparison the parameter change is judged
/// on is a memory comparison: set `MUSHROOMDB_HNSW_PARAMS` to the old shape
/// (`32,128,400,400`) to print the row this release is measured against.
///
/// Also prints the build wall clock, which is the repeatable successor to the
/// 45.8 s (`prune = own`) / 254 s (`prune = both`) the distance kernel was
/// measured against.
#[test]
#[ignore = "slow: builds a 5k-vector index at 1536-D"]
fn hnsw_memory_per_node_5k_1536() {
    const N: usize = 5_000;
    const DIM: usize = 1_536;

    let vecs = make_unit_vecs(N, DIM, fnv1a_u64(b"recall-probe-5k-1536"));
    let mut idx = HnswIndex::new(fnv1a_u64(b"recall-probe-5k-1536"));
    let t0 = Instant::now();
    for (i, v) in vecs.iter().enumerate() {
        idx.insert(i as u32, v);
    }
    let build = t0.elapsed();

    let mem = idx.memory_stats();
    println!(
        "params {:?}\n\
         n={} build {:.2?} ({:.3?}/insert)  adjacency {:.1} B/node ({:.1} entries/node \
         forward, {:.1} reverse)  vector {:.1} B/node  total {:.1} B/node",
        core_rules::hnsw::hnsw_params(),
        mem.live_nodes,
        build,
        build / N as u32,
        mem.adjacency_bytes_per_node(),
        mem.neighbour_slots as f64 / mem.live_nodes as f64,
        mem.back_ref_entries as f64 / mem.live_nodes as f64,
        (mem.vector_floats * 4) as f64 / mem.live_nodes as f64,
        mem.bytes_per_node(),
    );

    assert_eq!(mem.live_nodes, N);
    // The vector half is arithmetic, not a measurement: 1536 f32s, the index's
    // own copy. The store keeps the f64s and is not counted here.
    assert_eq!(mem.vector_floats, N * DIM);
}
