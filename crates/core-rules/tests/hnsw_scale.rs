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
        //     arithmetic at `DIM` f64s.
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
            mem.bytes_per_node() <= adjacency_ceiling + (DIM * 8) as f64,
            "n={n}: total {:.1} B/node exceeds adjacency ceiling plus the vector",
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
        "update cost grew {u:.2}x from 2k to 50k; removal is still O(N)"
    );
    assert!(
        update[2] < Duration::from_millis(25),
        "a 50k embedding update took {:?}; must be under 25 ms",
        update[2]
    );
}

/// Memory the graph holds per indexed vector, at 5,000 × 1,536-D.
///
/// Separate from the benchmark above because it is cheap enough to run on its
/// own, and because the before/after comparison the parameter change is judged
/// on is a memory comparison: set `MUSHROOMDB_HNSW_PARAMS` to the old shape
/// (`32,128,400,400`) to print the row this release is measured against.
#[test]
#[ignore = "slow: builds a 5k-vector index at 1536-D"]
fn hnsw_memory_per_node_5k_1536() {
    const N: usize = 5_000;
    const DIM: usize = 1_536;

    let vecs = make_unit_vecs(N, DIM, fnv1a_u64(b"recall-probe-5k-1536"));
    let mut idx = HnswIndex::new(fnv1a_u64(b"recall-probe-5k-1536"));
    for (i, v) in vecs.iter().enumerate() {
        idx.insert(i as u32, v);
    }

    let mem = idx.memory_stats();
    println!(
        "params {:?}\n\
         n={} adjacency {:.1} B/node ({:.1} entries/node forward, {:.1} reverse)  \
         vector {:.1} B/node  total {:.1} B/node",
        core_rules::hnsw::hnsw_params(),
        mem.live_nodes,
        mem.adjacency_bytes_per_node(),
        mem.neighbour_slots as f64 / mem.live_nodes as f64,
        mem.back_ref_entries as f64 / mem.live_nodes as f64,
        (mem.vector_floats * 8) as f64 / mem.live_nodes as f64,
        mem.bytes_per_node(),
    );

    assert_eq!(mem.live_nodes, N);
    // The vector half is arithmetic, not a measurement: 1536 f64s.
    assert_eq!(mem.vector_floats, N * DIM);
}
