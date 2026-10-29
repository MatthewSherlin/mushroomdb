//! TX-7's concurrency gate: lock-free readers keep their latency under a
//! sustained writer, and the writer's cost stays where the owner accepted
//! it. The scenario is `benches/shared/concurrency.rs`, shared with the
//! Criterion bench `readers_p50_p99_under_sustained_writes`.
//!
//! A timing gate, so release only: CI runs it by name in the
//! `concurrency-gate` job (`cargo test --release -p core-bench --test
//! concurrency_gate -- concurrency_gate --ignored --nocapture`), with
//! `MUSHROOMDB_GATE_V092` naming this same file built against the v0.9.2
//! tag (`--cfg mdb_v092`), whose `writer_probe` is the baseline.

#[path = "../benches/shared/concurrency.rs"]
mod concurrency;

use concurrency::*;
use std::time::Duration;

/// Writer blocks per probe: rounds × commits per block, alternating paused
/// and running readers.
const PROBE_ROUNDS: usize = 10;
const PROBE_BLOCK: usize = 200;

/// One writer measurement, as the line `writer_probe` prints.
fn probe_line(w: &WriterReport) -> String {
    format!(
        "WRITER_PROBE alone={:.6} busy={:.6} under={:.6} reads_per_sec={:.0} readers={}",
        w.alone_p50_ms, w.busy_p50_ms, w.under_p50_ms, w.reads_per_sec, w.readers
    )
}

/// Parse [`probe_line`]'s `alone`, `busy` and `under`.
fn parse_probe(out: &str) -> Option<(f64, f64, f64)> {
    let line = out.lines().find(|l| l.starts_with("WRITER_PROBE"))?;
    let field = |name: &str| {
        line.split_whitespace()
            .find_map(|kv| kv.strip_prefix(name)?.strip_prefix('='))?
            .parse::<f64>()
            .ok()
    };
    Some((field("alone")?, field("busy")?, field("under")?))
}

/// One set of writer measurements: [`V092_ROUNDS`] interleaved pairs of the
/// v0.9.2 baseline's probe and this build's when `baseline` names the
/// baseline build, else one of this build's.
struct WriterSet {
    alone: f64,
    /// The writer beside busy threads that share the cores but not the store.
    busy: f64,
    under: f64,
    reads: f64,
    /// This build's paused median over v0.9.2's, and its running median over
    /// v0.9.2's with each taken relative to its own build's busy block:
    /// `(under / busy) / (v0.9.2 under / v0.9.2 busy)`. On a machine with
    /// fewer cores than threads, threads beside the writer cost it CPU
    /// whatever the locking; each build's busy block measures that on the
    /// same runner, so the ratio compares what the readers' use of the store
    /// costs. With cores to spare both busy blocks equal the paused ones and
    /// this is the plain running ratio.
    vs_v092: Option<(f64, f64)>,
    /// v0.9.2's own running median over its busy one.
    v092_ratio: Option<f64>,
}

#[cfg(not(mdb_v092))]
impl WriterSet {
    /// The bound on `under / busy`: [`WRITER_UNDER_READERS_MAX`], or
    /// [`SELF_RATIO_MAX_VS_V092`] times v0.9.2's own ratio when that is higher.
    fn ratio_bound(&self) -> f64 {
        self.v092_ratio.map_or(WRITER_UNDER_READERS_MAX, |r| {
            WRITER_UNDER_READERS_MAX.max(r * SELF_RATIO_MAX_VS_V092)
        })
    }

    fn within_bounds(&self) -> bool {
        self.reads >= READS_PER_SEC_FLOOR
            && self.under / self.busy <= self.ratio_bound()
            && self.vs_v092.is_none_or(|(alone_x, under_x)| {
                alone_x <= ALONE_MAX_VS_V092 && under_x <= UNDER_MAX_VS_V092
            })
    }
}

#[cfg(not(mdb_v092))]
fn measure_writer_set(baseline: Option<&str>) -> WriterSet {
    let mut head: Vec<WriterReport> = Vec::new();
    let mut base: Vec<(f64, f64, f64)> = Vec::new();
    let rounds = if baseline.is_some() { V092_ROUNDS } else { 1 };
    for _ in 0..rounds {
        if let Some(exe) = baseline {
            let out = std::process::Command::new(exe)
                .args(["writer_probe", "--ignored", "--nocapture", "--exact"])
                .output()
                .expect("run the v0.9.2 baseline probe");
            let text = String::from_utf8_lossy(&out.stdout);
            base.push(parse_probe(&text).unwrap_or_else(|| {
                panic!("the v0.9.2 baseline printed no WRITER_PROBE line:\n{text}")
            }));
        }
        let w = measure_writer(writer_check_readers(), PROBE_ROUNDS, PROBE_BLOCK);
        println!("this build: {}", probe_line(&w));
        head.push(w);
    }
    let mut alone: Vec<f64> = head.iter().map(|w| w.alone_p50_ms).collect();
    let mut busy: Vec<f64> = head.iter().map(|w| w.busy_p50_ms).collect();
    let mut under: Vec<f64> = head.iter().map(|w| w.under_p50_ms).collect();
    let (alone, busy, under) = (
        quantile(&mut alone, 0.5),
        quantile(&mut busy, 0.5),
        quantile(&mut under, 0.5),
    );
    let reads = head
        .iter()
        .map(|w| w.reads_per_sec)
        .fold(f64::INFINITY, f64::min);
    let vs_v092 = (!base.is_empty()).then(|| {
        let mut b_alone: Vec<f64> = base.iter().map(|b| b.0).collect();
        let mut b_busy: Vec<f64> = base.iter().map(|b| b.1).collect();
        let mut b_under: Vec<f64> = base.iter().map(|b| b.2).collect();
        let (b_alone, b_busy, b_under) = (
            quantile(&mut b_alone, 0.5),
            quantile(&mut b_busy, 0.5),
            quantile(&mut b_under, 0.5),
        );
        let under_x = (under / busy) / (b_under / b_busy);
        println!(
            "v0.9.2: paused {b_alone:.4} ms, beside busy threads {b_busy:.4} ms, under readers {b_under:.4} ms; this build {:.2}x paused, {under_x:.2}x under readers against each build's busy block ({:.2}x raw)",
            alone / b_alone,
            under / b_under
        );
        (alone / b_alone, under_x, b_under / b_busy)
    });
    WriterSet {
        alone,
        busy,
        under,
        reads,
        vs_v092: vs_v092.map(|(a, u, _)| (a, u)),
        v092_ratio: vs_v092.map(|(_, _, r)| r),
    }
}

/// The writer measurement on its own, for the v0.9.2 build of this file to
/// print as the gate's baseline.
#[test]
#[ignore = "baseline probe: the gate runs it from the v0.9.2 build"]
fn writer_probe() {
    let w = measure_writer(writer_check_readers(), PROBE_ROUNDS, PROBE_BLOCK);
    println!("{}", probe_line(&w));
}

/// The reader gate, measured once and, if it misses, once more: a shared
/// runner can stall a handful of iterations, and p99 rests on the worst 1 %.
/// Both measurements must miss for the gate to fail.
fn readers_within_gate() -> (bool, Vec<ReaderReport>) {
    let mut seen = Vec::new();
    for _ in 0..2 {
        let r = measure_readers(1_500, Duration::from_millis(500));
        let ok = r.p50_ms <= P50_MAX_MS
            && r.p99_ms <= P99_MAX_MS
            && r.commits_per_sec >= WRITES_PER_SEC as f64 * 0.8;
        seen.push(r);
        if ok {
            return (true, seen);
        }
    }
    (false, seen)
}

#[cfg(not(mdb_v092))]
#[test]
#[ignore = "timing gate: release only; CI runs it by name in the concurrency-gate job"]
fn concurrency_gate() {
    // Readers under a 50 commits/s writer: 16 calls an iteration.
    let (readers_ok, seen) = readers_within_gate();
    for r in &seen {
        println!(
            "readers: {} calls on {} threads, writer {:.1} commits/s: p50 {:.3} ms, p99 {:.3} ms, max {:.3} ms (gate p50 <= {P50_MAX_MS} ms, p99 <= {P99_MAX_MS} ms)",
            r.samples, r.threads, r.commits_per_sec, r.p50_ms, r.p99_ms, r.max_ms
        );
    }

    // The writer, paused and running readers alternating; against v0.9.2 on
    // this machine when a baseline build is given, interleaved.
    let baseline = std::env::var("MUSHROOMDB_GATE_V092").ok();
    // Like the reader gate, the writer gate fails only when it misses twice:
    // a shared runner's noise can move one set's median past a bound.
    let mut writer = measure_writer_set(baseline.as_deref());
    if !writer.within_bounds() {
        println!("writer: a bound missed in the first set; measuring once more");
        writer = measure_writer_set(baseline.as_deref());
    }
    let (alone, busy, under, reads) = (writer.alone, writer.busy, writer.under, writer.reads);
    let ratio = under / busy;
    let ratio_bound = writer.ratio_bound();
    println!(
        "writer: plain commit p50 {alone:.4} ms paused, {busy:.4} ms beside {n} busy threads, {under:.4} ms under {n} readers (>= {reads:.0} reads/s): {ratio:.2}x the busy block (bound {ratio_bound:.2}x, reads/s floor {READS_PER_SEC_FLOOR})",
        n = writer_check_readers()
    );
    let vs_v092 = writer.vs_v092;
    match vs_v092 {
        Some((alone_x, under_x)) => println!(
            "v0.9.2: paused {alone_x:.2}x (bound {ALONE_MAX_VS_V092}x); running {under_x:.2}x (bound {UNDER_MAX_VS_V092}x)"
        ),
        None => println!(
            "v0.9.2: no baseline (MUSHROOMDB_GATE_V092 unset); the v0.9.2 bounds were not checked"
        ),
    }

    assert!(
        readers_ok,
        "reader gate missed twice (p50 <= {P50_MAX_MS} ms, p99 <= {P99_MAX_MS} ms, writer >= {} commits/s)",
        WRITES_PER_SEC as f64 * 0.8
    );
    assert!(
        reads >= READS_PER_SEC_FLOOR,
        "the readers completed {reads:.0} reads/s while the writer was timed; the accepted writer cost assumes >= {READS_PER_SEC_FLOOR}"
    );
    assert!(
        ratio <= ratio_bound,
        "a plain commit under readers costs {ratio:.2}x its cost beside as many busy threads; bound {ratio_bound:.2}x"
    );
    if let Some((alone_x, under_x)) = vs_v092 {
        assert!(
            alone_x <= ALONE_MAX_VS_V092,
            "a plain commit with no readers costs {alone_x:.2}x v0.9.2's on this machine; bound {ALONE_MAX_VS_V092}x (the Linux cost 0.10.0 shipped with, ledger row 167)"
        );
        assert!(
            under_x <= UNDER_MAX_VS_V092,
            "a plain commit under readers costs {under_x:.2}x v0.9.2's on this machine, with the cost of sharing the cores taken out; bound {UNDER_MAX_VS_V092}x (the Linux cost 0.10.0 shipped with, ledger row 167)"
        );
    }
}
