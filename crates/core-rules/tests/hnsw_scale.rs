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

const DIM: usize = 1_536;
const SIZES: [usize; 3] = [2_000, 10_000, 50_000];

/// One build of each size, measured once and shared by every assertion below.
///
/// **Why these are separate `#[test]` functions over a shared fixture.** They
/// used to be six assertions inside one test. Three of them are red, and an
/// assertion that panics ends its test — so the first red one (the 2k→10k build
/// growth) meant the 50k wall clock and both update assertions never executed at
/// all. `benchmarks/results/hnsw-scale-0.6.6.md` reported "the two assertions
/// that stay red" by reading the printed table rather than by watching them run,
/// and the update-cost gate — 5.92x against a 3.0x ceiling — was never counted.
/// One test cannot report six results; six tests can.
struct ScaleMeasurements {
    /// Total build time per size, in `SIZES` order.
    build: [Duration; 3],
    /// Mean time for one re-insert of an existing id, per size.
    update: [Duration; 3],
    adjacency_bytes_per_node: [f64; 3],
    total_bytes_per_node: [f64; 3],
}

/// `None` when `MUSHROOMDB_BENCH_HNSW=1` is absent — every test below then
/// returns without asserting, which is the same skip the single test used to do.
///
/// Built at most once per process even under parallel test threads, so the six
/// assertions cost one ~20-minute fixture between them rather than six.
fn measurements() -> Option<&'static ScaleMeasurements> {
    static CELL: std::sync::OnceLock<Option<ScaleMeasurements>> = std::sync::OnceLock::new();
    CELL.get_or_init(build_measurements).as_ref()
}

fn build_measurements() -> Option<ScaleMeasurements> {
    if std::env::var("MUSHROOMDB_BENCH_HNSW").as_deref() != Ok("1") {
        // Not a pass. `--ignored` already keeps these out of a normal run, and
        // anyone who sweeps `--ignored` without meaning to spend twenty minutes
        // should see why nothing happened rather than a green tick.
        println!(
            "SKIPPED hnsw scale fixture: set MUSHROOMDB_BENCH_HNSW=1 to run it (~20 min)"
        );
        return None;
    }
    let mut build = [Duration::ZERO; 3];
    let mut update = [Duration::ZERO; 3];
    let mut adjacency_bytes_per_node = [0.0f64; 3];
    let mut total_bytes_per_node = [0.0f64; 3];

    println!("params: {:?}", core_rules::hnsw::hnsw_params());
    for (slot, &n) in SIZES.iter().enumerate() {
        let vecs = make_unit_vecs(n, DIM, 0xB0A7_5EED);
        let mut idx = HnswIndex::new(fnv1a_u64(b"bench"));
        let t0 = Instant::now();
        for (i, v) in vecs.iter().enumerate() {
            idx.insert(i as u32, v);
        }
        build[slot] = t0.elapsed();

        // Per-write cost once the index exists: re-insert 100 existing ids,
        // which is exactly what an embedding change costs.
        let t1 = Instant::now();
        for i in (0..n).step_by(n / 100).take(100) {
            idx.insert(i as u32, &vecs[i]);
        }
        update[slot] = t1.elapsed() / 100;

        let mem = idx.memory_stats();
        adjacency_bytes_per_node[slot] = mem.adjacency_bytes_per_node();
        total_bytes_per_node[slot] = mem.bytes_per_node();
        println!(
            "n={n:>6}  build {:>10.2?}  per-insert {:>10.3?}  update {:>10.3?}  \
             adjacency {:>7.1} B/node  total {:>9.1} B/node",
            build[slot],
            build[slot] / n as u32,
            update[slot],
            adjacency_bytes_per_node[slot],
            total_bytes_per_node[slot],
        );
    }
    Some(ScaleMeasurements {
        build,
        update,
        adjacency_bytes_per_node,
        total_bytes_per_node,
    })
}

/// (0) The memory claim, asserted rather than printed. Adjacency is bounded by
///     `2 * (m0 + m) * 4` bytes — forward entries by `m0` on layer 0 plus `m`
///     per layer above, and the reverse index by one entry per (source, target)
///     pair — and the vector half is exact arithmetic at `DIM` f32s, the
///     index's own copy of each vector.
#[test]
#[ignore = "slow: builds a 50k-vector index; set MUSHROOMDB_BENCH_HNSW=1"]
fn memory_per_node_is_within_the_ceiling() {
    let Some(m) = measurements() else { return };
    let p = core_rules::hnsw::hnsw_params();
    let adjacency_ceiling = 2.0 * (p.m0 + p.m) as f64 * 4.0;
    for (slot, &n) in SIZES.iter().enumerate() {
        assert!(
            m.adjacency_bytes_per_node[slot] <= adjacency_ceiling,
            "n={n}: adjacency {:.1} B/node exceeds the {adjacency_ceiling:.1} B/node \
             ceiling for m0={} m={}",
            m.adjacency_bytes_per_node[slot],
            p.m0,
            p.m
        );
        assert!(
            m.total_bytes_per_node[slot] <= adjacency_ceiling + (DIM * 4) as f64,
            "n={n}: total {:.1} B/node exceeds adjacency ceiling plus the vector \
             ({DIM} f32s — the index's own copy, the store keeps the f64s)",
            m.total_bytes_per_node[slot]
        );
    }
}

/// (1a) Sub-quadratic build. Each step multiplies n by 5. Linear would cost 5x;
///      quadratic 25x. 8x is comfortably sub-quadratic and leaves room for the
///      log(N) descent term and for CI noise.
#[test]
#[ignore = "slow: builds a 50k-vector index; set MUSHROOMDB_BENCH_HNSW=1"]
fn build_growth_2k_to_10k_is_under_8x() {
    let Some(m) = measurements() else { return };
    let r1 = m.build[1].as_secs_f64() / m.build[0].as_secs_f64();
    assert!(
        r1 < 8.0,
        "2k→10k build grew {r1:.2}x for 5x the vectors; must be < 8x"
    );
}

/// (1b) The same rule across the second step.
#[test]
#[ignore = "slow: builds a 50k-vector index; set MUSHROOMDB_BENCH_HNSW=1"]
fn build_growth_10k_to_50k_is_under_8x() {
    let Some(m) = measurements() else { return };
    let r2 = m.build[2].as_secs_f64() / m.build[1].as_secs_f64();
    assert!(
        r2 < 8.0,
        "10k→50k build grew {r2:.2}x for 5x the vectors; must be < 8x"
    );
}

/// (2) The case that did not finish in ten minutes must finish in five.
#[test]
#[ignore = "slow: builds a 50k-vector index; set MUSHROOMDB_BENCH_HNSW=1"]
fn build_50k_is_under_300s() {
    let Some(m) = measurements() else { return };
    assert!(
        m.build[2] < Duration::from_secs(300),
        "50k build took {:?}; must be under 5 minutes",
        m.build[2]
    );
}

/// (3a) A changed embedding must not cost a scan of the index. Flat, not
///      growing: the 50k update must be within 3x the 2k update.
///
/// **This assertion had never executed before the split.** It sat behind (1a)'s
/// panic, and the committed benchmark doc counted red assertions from the
/// printed table instead.
#[test]
#[ignore = "slow: builds a 50k-vector index; set MUSHROOMDB_BENCH_HNSW=1"]
fn update_cost_2k_to_50k_is_under_3x() {
    let Some(m) = measurements() else { return };
    let u = m.update[2].as_secs_f64() / m.update[0].as_secs_f64();
    assert!(
        u < 3.0,
        "update cost grew {u:.2}x from 2k to 50k. An update is a remove plus an \
         insert, and removal is pinned at O(in-degree) by \
         remove_touches_only_the_nodes_that_list_it — so this is the *insert* \
         growing with n, the same term the build-growth assertions measure, not a \
         removal scan."
    );
}

/// (3b) The absolute ceiling on one embedding update at 50k.
#[test]
#[ignore = "slow: builds a 50k-vector index; set MUSHROOMDB_BENCH_HNSW=1"]
fn update_50k_is_under_25ms() {
    let Some(m) = measurements() else { return };
    assert!(
        m.update[2] < Duration::from_millis(25),
        "a 50k embedding update took {:?}; must be under 25 ms",
        m.update[2]
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
