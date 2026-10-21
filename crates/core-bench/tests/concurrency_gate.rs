//! TX-7's concurrency gate: lock-free readers keep their latency under a
//! sustained writer, and the writer's cost under readers stays where the
//! owner accepted it. The scenario is `benches/shared/concurrency.rs`, shared with the
//! Criterion bench `readers_p50_p99_under_sustained_writes`.
//!
//! A timing gate, so release only: CI runs it by name in the
//! `concurrency-gate` job (`cargo test --release -p core-bench --test
//! concurrency_gate -- --ignored --nocapture`).

#[path = "../benches/shared/concurrency.rs"]
mod concurrency;

use concurrency::*;
use std::time::Duration;

#[test]
#[ignore = "timing gate: release only; CI runs it by name in the concurrency-gate job"]
fn concurrency_gate() {
    // Readers under a 50 commits/s writer: 16 calls an iteration.
    let r = measure_readers(600, Duration::from_millis(500));
    println!(
        "readers: {} calls on {} threads, writer {:.1} commits/s: p50 {:.3} ms, p99 {:.3} ms, max {:.3} ms (gate p50 <= {P50_MAX_MS} ms, p99 <= {P99_MAX_MS} ms)",
        r.samples, r.threads, r.commits_per_sec, r.p50_ms, r.p99_ms, r.max_ms
    );

    // The writer alone and under readers, alternating (the owner's decision).
    let under = measure_writer(writer_check_readers(), 10, 200);
    let ratio = under.ratio();
    println!(
        "writer: plain commit p50 {:.4} ms alone, {:.4} ms under {} readers ({:.0} reads/s): {ratio:.2}x (accepted up to {WRITER_UNDER_READERS_MAX}x with >= {READS_PER_SEC_FLOOR} reads/s)",
        under.alone_p50_ms, under.under_p50_ms, under.readers, under.reads_per_sec
    );

    assert!(
        r.commits_per_sec >= WRITES_PER_SEC as f64 * 0.8,
        "the writer ran at {:.1} commits/s, below {WRITES_PER_SEC}: the readers were not measured under the gate's load",
        r.commits_per_sec
    );
    assert!(
        r.p50_ms <= P50_MAX_MS,
        "reader p50 {:.3} ms > {P50_MAX_MS} ms",
        r.p50_ms
    );
    assert!(
        r.p99_ms <= P99_MAX_MS,
        "reader p99 {:.3} ms > {P99_MAX_MS} ms",
        r.p99_ms
    );
    assert!(
        under.reads_per_sec >= READS_PER_SEC_FLOOR,
        "the readers completed {:.0} reads/s while the writer was timed; the accepted writer cost assumes >= {READS_PER_SEC_FLOOR}",
        under.reads_per_sec
    );
    assert!(
        ratio <= WRITER_UNDER_READERS_MAX,
        "a plain commit under {} readers costs {ratio:.2}x its cost alone; the owner accepted the state measured at about 1.4x (1.16-1.21x v0.9.2), up to {WRITER_UNDER_READERS_MAX}x",
        under.readers
    );
}
