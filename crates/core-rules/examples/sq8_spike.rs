//! v0.6.11 Task 9 — the SQ8 recall spike. **No production code.**
//!
//! The scale gates are bounded by memory bandwidth, not arithmetic: the distance
//! kernel already runs at the NEON ceiling (~116 ns/eval cache-resident,
//! 26.3 GFLOP/s), and the 15.48x build-growth figure decomposes as 5x more
//! vectors × ~1.7x evals-per-insert × **1.82x a per-eval cache penalty**. The
//! only lever left is touching fewer bytes per evaluation, which means storing
//! rows quantised.
//!
//! This spike answers the one question that decides whether Task 10 is written:
//! **does int8 quantisation cost recall the CI floors will not pay?**
//!
//! It deliberately does *not* rebuild the index over quantised rows. That is
//! Task 10's job and a much larger change. What it measures instead is the
//! quantisation error's effect on **ranking**, on the two corpora the CI gates
//! use, in the two arms that matter:
//!
//! - **direct** — rank by quantised distance and take the top K. This is the
//!   pessimistic bound: an index whose beam sees only quantised rows and whose
//!   answer is never re-scored.
//! - **rerank** — take the top `RERANK_FACTOR * K` by quantised distance, then
//!   re-score those in full precision and take K. This is the proposed design,
//!   where quantisation accelerates the beam and never becomes the final
//!   arithmetic.
//!
//! If **direct** already clears the floors, the design has slack. If **rerank**
//! clears them and direct does not, the rerank is load-bearing and Task 10 must
//! keep it. If neither clears them, Task 10 is cut and the release ships without
//! it — spec D5.
//!
//! Run:
//! ```sh
//! cargo run --release -p mushroomdb-rules --example sq8_spike
//! ```

use core_rules::hnsw::{make_clustered_unit_vecs, make_unit_vecs};
use std::collections::BTreeSet;
use std::time::Instant;

const K: usize = 10;
/// Rerank factors to sweep. The cheapest one that holds both gates is the
/// answer; if none does, that is the NO-GO. A factor of `n/K` is exact search,
/// so a large enough factor always "passes" — which is why the sweep reports the
/// factor and not just a verdict.
const RERANK_FACTORS: [usize; 5] = [2, 4, 8, 16, 32];

// ─── quantisation ────────────────────────────────────────────────────────────

/// One row stored as int8 plus the scale that decodes it.
///
/// Asymmetric: rows are quantised, the query is not. That halves the error
/// against a symmetric scheme — the query contributes none — and costs nothing,
/// because a query is used against many rows.
struct QRow {
    q: Vec<i8>,
    /// `value ≈ q[i] as f32 * scale`
    scale: f32,
}

fn quantise(v: &[f64]) -> QRow {
    let max = v.iter().fold(0.0f64, |m, x| m.max(x.abs()));
    // A zero row has no scale to speak of; keep 1.0 so decode is a no-op.
    let scale = if max == 0.0 { 1.0 } else { max / 127.0 };
    let q = v
        .iter()
        .map(|&x| {
            let r = (x / scale).round();
            r.clamp(-127.0, 127.0) as i8
        })
        .collect();
    QRow {
        q,
        scale: scale as f32,
    }
}

/// Dot product of a quantised row against an f32 query.
///
/// **Asymmetric, properly.** The query is *not* quantised — it stays f32 and is
/// used as-is, so it contributes no quantisation error at all. The first version
/// of this function converted each query element to an integer inside the inner
/// loop (`(q[i] * 127.0) as i32`), which both added error the design does not
/// have and made the kernel 1.5x *slower* than the f64 baseline: a float
/// multiply plus a float-to-int conversion per element per row defeats the whole
/// point of touching fewer bytes. The win here is that `row.q` is 1 byte per
/// dimension where the baseline is 4.
///
/// Mirrors the shipped kernel's shape — eight independent accumulators — so this
/// is like for like rather than a fast kernel against a naive one.
#[inline]
fn dot_q_f32(row: &QRow, q: &[f32]) -> f32 {
    let mut acc = [0.0f32; 8];
    let mut i = 0;
    let n = row.q.len().min(q.len());
    while i + 8 <= n {
        for lane in 0..8 {
            acc[lane] += row.q[i + lane] as f32 * q[i + lane];
        }
        i += 8;
    }
    let mut tail = 0.0f32;
    while i < n {
        tail += row.q[i] as f32 * q[i];
        i += 1;
    }
    (acc.iter().sum::<f32>() + tail) * row.scale
}

/// The int8 kernel over a slab slice, so the timing harness streams one
/// contiguous allocation exactly as `VecSlab` does.
#[inline]
fn dot_slab_i8(row: &[i8], scale: f32, q: &[f32]) -> f32 {
    let mut acc = [0.0f32; 8];
    let mut i = 0;
    let n = row.len().min(q.len());
    while i + 8 <= n {
        for lane in 0..8 {
            acc[lane] += row[i + lane] as f32 * q[i + lane];
        }
        i += 8;
    }
    let mut tail = 0.0f32;
    while i < n {
        tail += row[i] as f32 * q[i];
        i += 1;
    }
    (acc.iter().sum::<f32>() + tail) * scale
}

/// The f32 baseline, matching the **shipped** slab: one contiguous f32 per
/// dimension, eight accumulators. Timing int8 against an f64 baseline would
/// flatter it by 2x, because f64 touches twice the bytes the engine actually
/// stores.
#[inline]
fn dot_f32(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = [0.0f32; 8];
    let mut i = 0;
    let n = a.len().min(b.len());
    while i + 8 <= n {
        for lane in 0..8 {
            acc[lane] += a[i + lane] * b[i + lane];
        }
        i += 8;
    }
    let mut s: f32 = acc.iter().sum();
    while i < n {
        s += a[i] * b[i];
        i += 1;
    }
    s
}

#[inline]
fn dot_f64(a: &[f64], b: &[f64]) -> f64 {
    let mut acc = [0.0f64; 8];
    let mut i = 0;
    let n = a.len().min(b.len());
    while i + 8 <= n {
        for lane in 0..8 {
            acc[lane] += a[i + lane] * b[i + lane];
        }
        i += 8;
    }
    let mut s: f64 = acc.iter().sum();
    while i < n {
        s += a[i] * b[i];
        i += 1;
    }
    s
}

fn exact_top_k(vecs: &[Vec<f64>], q: &[f64], k: usize) -> Vec<usize> {
    let mut scored: Vec<(usize, f64)> = vecs
        .iter()
        .enumerate()
        .map(|(i, v)| (i, dot_f64(v, q)))
        .collect();
    // Descending by similarity, ties by id so the set is deterministic.
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap().then(a.0.cmp(&b.0)));
    scored.into_iter().take(k).map(|(i, _)| i).collect()
}

/// Rank by quantised distance. `rerank` re-scores the top `factor * k` in full
/// precision before truncating to `k`.
fn quantised_top_k(
    rows: &[QRow],
    vecs: &[Vec<f64>],
    q_f32: &[f32],
    q_f64: &[f64],
    k: usize,
    rerank_factor: usize,
) -> Vec<usize> {
    let mut scored: Vec<(usize, f32)> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| (i, dot_q_f32(r, q_f32)))
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap().then(a.0.cmp(&b.0)));

    if rerank_factor == 0 {
        return scored.into_iter().take(k).map(|(i, _)| i).collect();
    }
    let take = (k * rerank_factor).min(scored.len());
    let mut cands: Vec<(usize, f64)> = scored
        .into_iter()
        .take(take)
        .map(|(i, _)| (i, dot_f64(&vecs[i], q_f64)))
        .collect();
    cands.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap().then(a.0.cmp(&b.0)));
    cands.into_iter().take(k).map(|(i, _)| i).collect()
}

fn recall_of(got: &[usize], truth: &BTreeSet<usize>) -> f64 {
    got.iter().filter(|i| truth.contains(i)).count() as f64 / truth.len() as f64
}

struct Verdict {
    min: f64,
    mean: f64,
}

fn measure(vecs: &[Vec<f64>], queries: &[Vec<f64>], rerank_factor: usize) -> Verdict {
    let rows: Vec<QRow> = vecs.iter().map(|v| quantise(v)).collect();
    let mut recalls = Vec::with_capacity(queries.len());
    for q in queries {
        let truth: BTreeSet<usize> = exact_top_k(vecs, q, K).into_iter().collect();
        let q_f32: Vec<f32> = q.iter().map(|&x| x as f32).collect();
        let got = quantised_top_k(&rows, vecs, &q_f32, q, K, rerank_factor);
        recalls.push(recall_of(&got, &truth));
    }
    Verdict {
        min: recalls.iter().cloned().fold(f64::MAX, f64::min),
        mean: recalls.iter().sum::<f64>() / recalls.len() as f64,
    }
}

fn report(name: &str, v: &Verdict, min_floor: f64, mean_floor: f64) -> bool {
    let ok = v.min >= min_floor && v.mean >= mean_floor;
    println!(
        "  {:<34} min={:.4} (floor {:.2})  mean={:.4} (floor {:.2})  {}",
        name,
        v.min,
        min_floor,
        v.mean,
        mean_floor,
        if ok { "PASS" } else { "**FAIL**" }
    );
    ok
}

fn main() {
    println!("SQ8 recall spike — v0.6.11 Task 9. No production code.\n");

    // ── Gate 1: the corpus `hnsw_5k_1536_recall` uses ────────────────────────
    // Same sizes, same seeds, same K, so the numbers are comparable to the
    // committed 1.0000/1.0000.
    println!("Gate 1 — 5,000 x 1,536-D, 50 queries, K={K} (floors min 0.90 / mean 0.95)");
    let vecs = make_unit_vecs(
        5_000,
        1_536,
        core_rules::index::fnv1a_u64(b"recall-probe-5k-1536"),
    );
    let queries = make_unit_vecs(50, 1_536, core_rules::index::fnv1a_u64(b"recall-queries"));
    let g1d = report(
        "int8 direct (no rerank)",
        &measure(&vecs, &queries, 0),
        0.90,
        0.95,
    );
    let mut g1_at: Option<usize> = None;
    for f in RERANK_FACTORS {
        let v = measure(&vecs, &queries, f);
        let ok = report(&format!("int8 + f32 rerank {f}xK"), &v, 0.90, 0.95);
        if ok && g1_at.is_none() {
            g1_at = Some(f);
        }
    }

    // ── Gate 2: the clustered corpus — the thin margin ───────────────────────
    // 128-D and tight clusters. Quantisation error is relative to dimension, and
    // within-cluster ordering is the hardest thing the index is asked for, so
    // this is where SQ8 fails if it fails. `Prune::Both`'s committed mean is
    // 0.9950 against a 0.9500 floor — 4.7% of headroom.
    println!(
        "\nGate 2 — 40 clusters x 120, 128-D, K={K} (Prune::Both floors min 0.70 / mean 0.95)"
    );
    let cvecs = make_clustered_unit_vecs(40, 120, 128, 0xC1_05_7E_12_34_56_78_9A);
    let cqueries: Vec<Vec<f64>> = (0..40)
        .map(|c| {
            let mut q = cvecs[c * 120 + 17].clone();
            q[0] += 1e-6;
            q
        })
        .collect();
    let g2d = report(
        "int8 direct (no rerank)",
        &measure(&cvecs, &cqueries, 0),
        0.70,
        0.95,
    );
    let mut g2_at: Option<usize> = None;
    for f in RERANK_FACTORS {
        let v = measure(&cvecs, &cqueries, f);
        let ok = report(&format!("int8 + f32 rerank {f}xK"), &v, 0.70, 0.95);
        if ok && g2_at.is_none() {
            g2_at = Some(f);
        }
    }

    // ── The cache win, which is the whole point ──────────────────────────────
    println!(
        "\nBytes touched per row, 1,536-D:  f32 {} B   int8 {} B  (-75%)",
        1_536 * 4,
        1_536
    );
    println!(
        "Per-eval cost, both over ONE contiguous slab (f32 baseline vs int8).\n\
         A ratio above 1.00 means int8 is faster; the premise of the whole change\n\
         is that it should be, because it touches a quarter of the bytes:"
    );
    let mut speedups = Vec::new();
    for n in [2_000usize, 10_000, 50_000] {
        let vs = make_unit_vecs(n, 1_536, 0xB0A7_5EED);
        let rows: Vec<QRow> = vs.iter().map(|v| quantise(v)).collect();
        let q_f64 = make_unit_vecs(1, 1_536, 0xDEAD_BEEF).remove(0);
        let q_f32: Vec<f32> = q_f64.iter().map(|&x| x as f32).collect();
        // ONE contiguous allocation, which is what `VecSlab` is. A
        // `Vec<Vec<f32>>` would measure pointer-chasing between heap blocks
        // instead of streaming a slab, and the whole premise here is bandwidth.
        let dim = 1_536usize;
        let mut slab_f32: Vec<f32> = Vec::with_capacity(n * dim);
        for v in &vs {
            slab_f32.extend(v.iter().map(|&x| x as f32));
        }
        let mut slab_i8: Vec<i8> = Vec::with_capacity(n * dim);
        let mut scales: Vec<f32> = Vec::with_capacity(n);
        for r in &rows {
            slab_i8.extend_from_slice(&r.q);
            scales.push(r.scale);
        }

        // Touch every row several times so the measurement is of residency, not
        // of a single cold pass.
        const REPS: usize = 3;
        let t0 = Instant::now();
        let mut sink = 0.0f32;
        for _ in 0..REPS {
            for i in 0..n {
                sink += dot_f32(&slab_f32[i * dim..(i + 1) * dim], &q_f32);
            }
        }
        let f_ns = t0.elapsed().as_nanos() as f64 / (REPS * n) as f64;

        let t1 = Instant::now();
        let mut sink2 = 0.0f32;
        for _ in 0..REPS {
            for i in 0..n {
                sink2 += dot_slab_i8(&slab_i8[i * dim..(i + 1) * dim], scales[i], &q_f32);
            }
        }
        let q_ns = t1.elapsed().as_nanos() as f64 / (REPS * n) as f64;

        println!(
            "  n={n:>6}  f32 {f_ns:>8.1} ns   int8 {q_ns:>8.1} ns   {:>5.2}x   \
             (slab f32 {:>6.1} MB -> int8 {:>6.1} MB)",
            f_ns / q_ns,
            (n * 1_536 * 4) as f64 / 1e6,
            (n * 1_536) as f64 / 1e6,
        );
        speedups.push(f_ns / q_ns);
        std::hint::black_box((sink, sink2));
    }
    let best = speedups.iter().cloned().fold(0.0f64, f64::max);

    // ── The verdict Task 10 is gated on ──────────────────────────────────────
    println!("\n─── VERDICT ───");
    println!(
        "Gate 1 — direct {}, cheapest passing rerank: {:?}",
        pf(g1d),
        g1_at
    );
    println!(
        "Gate 2 — direct {}, cheapest passing rerank: {:?}",
        pf(g2d),
        g2_at
    );
    let recall_ok = match (g1_at, g2_at) {
        (Some(a), Some(b)) => {
            let need = a.max(b);
            println!(
                "RECALL: affordable at rerank {need}xK — the factor the tighter gate (clustered, \
                 128-D) needs.\n        Direct-only {} hold both gates, so the rerank is \
                 load-bearing.",
                if g1d && g2d { "does" } else { "does NOT" }
            );
            true
        }
        _ => {
            println!(
                "RECALL: NOT affordable at any factor swept ({:?}).",
                RERANK_FACTORS
            );
            false
        }
    };
    let perf_ok = best > 1.10;
    println!(
        "PERF:   best int8 speedup over the f32 slab = {best:.2}x  →  {}",
        if perf_ok {
            "the premise holds"
        } else {
            "**the premise does NOT hold**"
        }
    );

    if recall_ok && perf_ok {
        println!("\nGO for Task 10.");
    } else if !perf_ok {
        println!(
            "\nNO-GO for Task 10 — on PERFORMANCE, not recall.\n\n\
             Recall is payable. What is not demonstrated is the win the change exists to buy.\n\
             int8 halves nothing here: `row[i] as f32 * q[i]` needs an i8-to-f32 widening per\n\
             element, and on this machine that conversion costs about what the saved bandwidth\n\
             buys — at 1,536 dims streamed sequentially the prefetcher keeps a 307 MB f32 slab\n\
             fed well enough that touching 76.8 MB instead wins nothing.\n\n\
             A real SQ8 kernel avoids the conversion with integer SIMD — a widening\n\
             multiply-accumulate (NEON `sdot`/`smlal`) over i8 lanes, accumulating in i32 —\n\
             which scalar source will not autovectorise into. That is `unsafe` intrinsics or a\n\
             dependency, and it is a different piece of work from the one this spike scoped.\n\n\
             Taking a recall risk, a blob-version bump and an index rebuild for a change whose\n\
             speedup is unproven is the wrong trade. Task 10 is cut (spec D5); the three scale\n\
             gates stay red and honestly counted at three."
        );
    } else {
        println!("\nNO-GO for Task 10 — recall cannot be held.");
    }
    println!(
        "\nCaveat: this measures quantisation's effect on RANKING over the gates' own\n\
         corpora. It does not rebuild the HNSW graph over quantised rows — a graph\n\
         built on quantised distances can differ, and Task 10 must re-run the real\n\
         gates. A NO-GO here is conclusive; a GO here is necessary, not sufficient."
    );
}

fn pf(ok: bool) -> &'static str {
    if ok {
        "PASS"
    } else {
        "FAIL"
    }
}
