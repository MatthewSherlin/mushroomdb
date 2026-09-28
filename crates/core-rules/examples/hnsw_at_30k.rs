//! v0.6.11 Task 11 — the HNSW numbers at the deployment's actual size.
//!
//! Spec §2.6 carries ~9 minutes for a full build and ~11 ms for one re-embed at
//! ~30,000 vectors, both **interpolated** from the committed 10k and 50k points.
//! This measures them. Same generator, same dimension, same seed family as
//! `tests/hnsw_scale.rs`, so the number is comparable to that table rather than
//! to a differently-shaped fixture.
//!
//! Run: `cargo run --release -p mushroomdb-rules --example hnsw_at_30k`
use core_rules::hnsw::{make_unit_vecs, HnswIndex};
use core_rules::index::fnv1a_u64;
use std::time::Instant;

fn main() {
    const DIM: usize = 1_536;
    let n: usize = std::env::var("HNSW_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30_000);

    println!("params: {:?}", core_rules::hnsw::hnsw_params());
    println!("building n={n} at {DIM}-D...");
    let vecs = make_unit_vecs(n, DIM, 0xB0A7_5EED);
    let mut idx = HnswIndex::new(fnv1a_u64(b"bench"));
    let t0 = Instant::now();
    for (i, v) in vecs.iter().enumerate() {
        idx.insert(i as u32, v);
    }
    let build = t0.elapsed();

    // One re-embed is a remove plus an insert — the write a talent-profile change
    // actually performs, and what the previously-masked gate measures.
    let t1 = Instant::now();
    for i in (0..n).step_by(n / 100).take(100) {
        idx.insert(i as u32, &vecs[i]);
    }
    let update = t1.elapsed() / 100;

    let mem = idx.memory_stats();
    println!(
        "n={n:>6}  build {:>10.2?}  per-insert {:>10.3?}  update {:>10.3?}  \
         adjacency {:>7.1} B/node  total {:>9.1} B/node",
        build,
        build / n as u32,
        update,
        mem.adjacency_bytes_per_node(),
        mem.bytes_per_node(),
    );
    println!(
        "\nAgainst the interpolations in spec 2.6: build ~9 min, update ~11 ms.\n\
         Measured: build {:.1} min, update {:.3} ms.",
        build.as_secs_f64() / 60.0,
        update.as_secs_f64() * 1000.0
    );
}
