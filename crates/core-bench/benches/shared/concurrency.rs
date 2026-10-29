//! The concurrency gate's scenario (TX-7), shared by the Criterion bench
//! `readers_p50_p99_under_sustained_writes` (`benches/engine.rs`) and the
//! `concurrency_gate` test (`tests/concurrency_gate.rs`), which include this
//! file by path: the crate has no library target.
//!
//! Two measurements:
//!
//! - **Readers under a writer.** A 10k-node store; one thread commits a
//!   property write at 50 commits/s through `submit_batch`; each iteration
//!   issues 16 concurrent `ReaderSnapshot::neighborhood(key, 1)` calls, each
//!   timed from its capture (`SharedDb::reader`) to its answer. Gate: p50
//!   <= 0.5 ms and p99 <= 2 ms.
//! - **The writer under readers** (the owner's decision of 2026-10-21, below).

#![allow(dead_code)] // the bench and the test each use part of this file

use mushroomdb::{BatchOp, Dir, FsyncPolicy, SharedDb, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Nodes in the gate's store.
pub const NODES: usize = 10_000;
/// Out-edges each node has, to the next nodes by key.
pub const OUT_EDGES: usize = 4;
/// The background writer's commit rate.
pub const WRITES_PER_SEC: u64 = 50;
/// Neighbourhood calls issued concurrently per iteration.
pub const CALLS_PER_ITERATION: usize = 16;
/// Gate: median reader latency, capture included.
pub const P50_MAX_MS: f64 = 0.5;
/// Gate: 99th-percentile reader latency, capture included.
pub const P99_MAX_MS: f64 = 2.0;

/// The owner's decision of 2026-10-21, which this gate encodes and must not
/// fail on: a plain commit under 8 concurrent readers costs 1.16–1.21× what
/// it cost in v0.9.2, and that is **accepted**, because the readers now
/// complete about 430k reads/s where v0.9.2's completed about 15 (they
/// blocked behind the writer). With no readers the commit is 1.01× v0.9.2,
/// and the 0.10 bar for a write path is 1.05×.
///
/// With readers paused the tree sat at 1.10–1.12× v0.9.2 until TX-5c
/// (2026-10-22): a commit no reader has asked for no longer wakes the
/// background builder, and a write scope reads the WAL's length and
/// identity with one `stat`. One measurement now holds the bound.
///
/// Three bounds hold that, all on medians of plain fsync-off commits:
///
/// - **Against v0.9.2, on the same machine** (CI builds the v0.9.2 tag with
///   this scenario, `--cfg mdb_v092`, and points `MUSHROOMDB_GATE_V092` at
///   it; the two are run interleaved, [`V092_ROUNDS`] times each):
///   - readers paused: at most [`ALONE_MAX_VS_V092`]× v0.9.2's;
///   - readers running: at most [`UNDER_MAX_VS_V092`]× v0.9.2's, each
///     measured against its own build's busy block (below).
/// - **Against this build with as many busy threads beside it**, which
///   needs no baseline: at most [`WRITER_UNDER_READERS_MAX`]×.
///
/// Sharing the cores (2026-10-28, ledger row 167). v0.9.2's readers block
/// behind its writer and do almost no work, so they take none of its cores;
/// this build's readers do hundreds of thousands of reads a second. On a
/// machine with fewer cores than readers, writer and builder (a 4-vCPU CI
/// runner), the readers then cost the writer CPU whatever the locking: the
/// same source measured 1.04–1.23× v0.9.2 on EPYC 9V45/9V74 runners and
/// 1.34–1.42× on EPYC 7763 ones. [`measure_writer`] therefore also times the
/// writer beside as many busy threads that share the cores and memory but
/// not the store, and both builds' running medians are taken relative to
/// their own busy block before they are compared.
/// Where there are cores to spare, as on the machine the decision was
/// measured on, the busy block costs what the paused one does.
///
/// The owner's decision of 2026-10-28 (ledger row 167). The 2026-10-21
/// numbers were measured on macOS, where the cost is smallest. On Linux the
/// same 0.10.0 source costs 1.18–1.34× v0.9.2 with no readers (CI Intel
/// Xeon runners and a Linux container on the M4) and 1.44–1.58× under
/// readers with the cores' share taken out (the container). The owner
/// re-accepted those, the costs 0.10.0 shipped with, so the gate catches
/// anything worse; 0.10.1 is to cut the per-commit publish cost and bring
/// these bounds back toward 1.15× and 1.25×.
///
/// Paused: the worst measured, 1.34×, plus a 0.06 noise margin. Before
/// 2026-10-28 this was 1.15× (the 1.05× bar plus 0.10).
pub const ALONE_MAX_VS_V092: f64 = 1.40;
/// Under readers: the worst measured on Linux, 1.58×, plus a 0.07 noise
/// margin. Before 2026-10-28 this was 1.25×: the 1.21× the owner accepted on
/// macOS plus 0.04.
pub const UNDER_MAX_VS_V092: f64 = 1.65;
/// Interleaved v0.9.2 / this-build pairs behind the v0.9.2 bounds. The
/// alone bound is the 1.05× bar plus a 0.10 noise margin: on the machine
/// this was tuned on, the median of a 10 × 200-commit run moved ±8 % between
/// runs at load 7–18 (WAL `write`/`close` syscalls are most of a 60 µs
/// commit), and a median over these pairs halves that.
pub const V092_ROUNDS: usize = 5;
/// Under readers, the commit's median may cost up to this multiple of the
/// same build's median beside as many busy threads (with them paused, before
/// 2026-10-28): about 1.4× when the decision was measured, 0.98–1.29× on
/// macOS and 1.43× in a Linux container on 2026-10-28; the pre-fix state was
/// 2.6–2.8×. 1.65×, with the under-readers bound above, since 2026-10-28
/// (was 1.5×).
pub const WRITER_UNDER_READERS_MAX: f64 = 1.65;
/// When v0.9.2 is measured on the same machine, the under-readers ratio may
/// also reach this multiple of v0.9.2's own: on a runner with fewer cores
/// than readers plus the writer, the readers take the writer's CPU whatever
/// the locking, and v0.9.2 itself measured 2.01× there (CI, 2 cores).
pub const SELF_RATIO_MAX_VS_V092: f64 = 1.15;
/// Reads per second the readers must complete while the writer is timed, so
/// the writer's cost is measured against readers that run (v0.9.2's ~15/s
/// would fail it).
pub const READS_PER_SEC_FLOOR: f64 = 10_000.0;
/// Reader threads in the owner's decision.
pub const OWNER_READERS: usize = 8;

/// Cores this process may use.
pub fn cores() -> usize {
    std::thread::available_parallelism().map_or(1, usize::from)
}

/// Threads that issue an iteration's 16 calls: one per call, but no more
/// than there are cores. More runnable threads than cores would time the
/// scheduler's slice — a reader descheduled mid-call for milliseconds — and
/// not the reader.
pub fn reader_threads() -> usize {
    CALLS_PER_ITERATION.min(cores()).max(1)
}

/// Readers beside the timed writer: the decision's 8, but leaving a core
/// for the writer and one for the reader builder thread on a small machine.
pub fn writer_check_readers() -> usize {
    OWNER_READERS.min(cores().saturating_sub(2)).max(1)
}

pub fn key(i: usize) -> String {
    format!("n{:05}", i % NODES)
}

fn tmp_dir(tag: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "mdb-concurrency-gate-{tag}-{}-{n}",
        std::process::id()
    ))
}

/// A store in a temporary directory, removed on drop.
pub struct Store {
    pub db: SharedDb,
    dir: PathBuf,
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// [`NODES`] `Node`s keyed `n00000…`, each with a `score` and [`OUT_EDGES`]
/// `LINK` edges to the next nodes, written in batches of 2,000 nodes.
pub fn build_store(tag: &str) -> Store {
    let dir = tmp_dir(tag);
    let _ = std::fs::remove_dir_all(&dir);
    let db = SharedDb::open(&dir).expect("open the gate store");
    db.write().set_fsync_policy(FsyncPolicy::Relaxed);
    for chunk in (0..NODES).step_by(2_000) {
        let ops = (chunk..(chunk + 2_000).min(NODES))
            .map(|i| BatchOp::InsertNode {
                label: "Node".into(),
                key: key(i),
                props: vec![("score".into(), Value::Int(i as i64))],
            })
            .collect();
        db.submit_batch(ops).expect("insert nodes");
    }
    for chunk in (0..NODES).step_by(2_000) {
        let ops = (chunk..(chunk + 2_000).min(NODES))
            .flat_map(|i| {
                (1..=OUT_EDGES).map(move |d| BatchOp::InsertEdge {
                    edge_type: "LINK".into(),
                    src_key: key(i),
                    dst_key: key(i + d),
                })
            })
            .collect();
        db.submit_batch(ops).expect("insert edges");
    }
    Store { db, dir }
}

/// A thread committing one `score` write per tick through `submit_batch`
/// until dropped.
pub struct Writer {
    stop: Arc<AtomicBool>,
    commits: Arc<AtomicU64>,
    handle: Option<JoinHandle<()>>,
}

impl Writer {
    pub fn commits(&self) -> u64 {
        self.commits.load(Ordering::Relaxed)
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Start committing at `per_sec` commits per second, on a fixed schedule so
/// a slow commit does not lower the rate. A writer that falls more than a
/// tick behind (a stalled machine) resumes the schedule from now rather than
/// bursting to catch up, so the rate is never above `per_sec`.
pub fn start_writer(db: &SharedDb, per_sec: u64) -> Writer {
    let stop = Arc::new(AtomicBool::new(false));
    let commits = Arc::new(AtomicU64::new(0));
    let handle = {
        let (db, stop, commits) = (db.clone(), Arc::clone(&stop), Arc::clone(&commits));
        std::thread::spawn(move || {
            let tick = Duration::from_micros(1_000_000 / per_sec.max(1));
            let mut due = Instant::now();
            let mut i = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let now = Instant::now();
                match due.checked_duration_since(now) {
                    Some(wait) => std::thread::sleep(wait),
                    None if now - due > tick => due = now,
                    None => {}
                }
                due += tick;
                let op = BatchOp::SetProp {
                    key: key((i as usize).wrapping_mul(7_919)),
                    field: "score".into(),
                    value: Value::Int(i as i64),
                };
                db.submit_batch(vec![op]).expect("writer commit");
                commits.fetch_add(1, Ordering::Relaxed);
                i += 1;
            }
        })
    };
    Writer {
        stop,
        commits,
        handle: Some(handle),
    }
}

/// One reader call, as a request pays it: capture a snapshot, then the
/// depth-1 neighbourhood of `key`. Returns the rows, so the work is used.
#[cfg(not(mdb_v092))]
pub fn read_neighborhood(db: &SharedDb, key: &str) -> usize {
    db.reader()
        .neighborhood(key, 1, None, Dir::Both)
        .map_or(0, |rs| rs.len())
}

/// The v0.9.2 baseline build (`--cfg mdb_v092`): that release's reader has no
/// unmasked neighbourhood, so its full-token path, the read guard's, stands in.
#[cfg(mdb_v092)]
pub fn read_neighborhood(db: &SharedDb, key: &str) -> usize {
    let g = db.read();
    g.node_ref(key)
        .map_or(0, |n| n.neighborhood(1, None, Dir::Both).len())
}

/// Reader threads that run iterations on request: each iteration issues
/// [`CALLS_PER_ITERATION`] calls split across the threads, all released at
/// once by a barrier, and every call's latency is recorded.
pub struct ReaderPool {
    go: Arc<Barrier>,
    done: Arc<Barrier>,
    stop: Arc<AtomicBool>,
    /// Bumped by [`Self::clear`]; a thread that sees it move drops what it
    /// has recorded but not yet handed over.
    epoch: Arc<AtomicU64>,
    samples: Arc<std::sync::Mutex<Vec<f64>>>,
    handles: Vec<JoinHandle<()>>,
}

impl ReaderPool {
    pub fn new(db: &SharedDb) -> Self {
        let threads = reader_threads();
        let go = Arc::new(Barrier::new(threads + 1));
        let done = Arc::new(Barrier::new(threads + 1));
        let stop = Arc::new(AtomicBool::new(false));
        let epoch = Arc::new(AtomicU64::new(0));
        let samples = Arc::new(std::sync::Mutex::new(Vec::new()));
        let handles = (0..threads)
            .map(|t| {
                let (db, go, done, stop, epoch, samples) = (
                    db.clone(),
                    Arc::clone(&go),
                    Arc::clone(&done),
                    Arc::clone(&stop),
                    Arc::clone(&epoch),
                    Arc::clone(&samples),
                );
                // This thread's share of each iteration's calls.
                let calls: Vec<usize> = (0..CALLS_PER_ITERATION)
                    .filter(|c| c % threads == t)
                    .collect();
                std::thread::spawn(move || {
                    let mut round = 0usize;
                    let mut seen = 0u64;
                    let mut mine = Vec::with_capacity(4_096);
                    loop {
                        go.wait();
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        let now = epoch.load(Ordering::Relaxed);
                        if now != seen {
                            mine.clear();
                            seen = now;
                        }
                        for &c in &calls {
                            let k = key((round * CALLS_PER_ITERATION + c).wrapping_mul(104_729));
                            let t0 = Instant::now();
                            std::hint::black_box(read_neighborhood(&db, &k));
                            mine.push(t0.elapsed().as_secs_f64() * 1e3);
                        }
                        round += 1;
                        if mine.len() >= 1_024 {
                            samples.lock().expect("samples").append(&mut mine);
                        }
                        done.wait();
                    }
                    samples.lock().expect("samples").append(&mut mine);
                })
            })
            .collect();
        Self {
            go,
            done,
            stop,
            epoch,
            samples,
            handles,
        }
    }

    /// Run one iteration: every call issued, every call answered.
    pub fn iteration(&self) {
        self.go.wait();
        self.done.wait();
    }

    /// Stop the threads and return every latency recorded, in ms.
    pub fn finish(mut self) -> Vec<f64> {
        self.stop.store(true, Ordering::Relaxed);
        self.go.wait();
        for h in self.handles.drain(..) {
            h.join().expect("reader thread");
        }
        std::mem::take(&mut *self.samples.lock().expect("samples"))
    }

    /// Drop what was recorded so far (after a warm-up). Called between
    /// iterations, while every thread waits at the barrier.
    pub fn clear(&self) {
        self.epoch.fetch_add(1, Ordering::Relaxed);
        self.samples.lock().expect("samples").clear();
    }
}

/// The value at quantile `q` of `v` (sorted in place), nearest rank.
pub fn quantile(v: &mut [f64], q: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(f64::total_cmp);
    let rank = ((v.len() as f64 * q).ceil() as usize).clamp(1, v.len());
    v[rank - 1]
}

/// What [`measure_readers`] saw.
#[derive(Debug, Clone, Copy)]
pub struct ReaderReport {
    pub samples: usize,
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    pub commits_per_sec: f64,
    pub threads: usize,
}

/// The reader gate's scenario: build the store, start the writer, warm up
/// for `warmup`, then run `iterations` iterations.
pub fn measure_readers(iterations: usize, warmup: Duration) -> ReaderReport {
    let store = build_store("readers");
    let writer = start_writer(&store.db, WRITES_PER_SEC);
    let pool = ReaderPool::new(&store.db);
    let warm_end = Instant::now() + warmup;
    while Instant::now() < warm_end {
        pool.iteration();
    }
    pool.clear();
    let (start, commits_before) = (Instant::now(), writer.commits());
    for _ in 0..iterations {
        pool.iteration();
    }
    let secs = start.elapsed().as_secs_f64();
    let commits = writer.commits() - commits_before;
    drop(writer);
    let mut samples = pool.finish();
    let report = ReaderReport {
        samples: samples.len(),
        p50_ms: quantile(&mut samples, 0.50),
        p99_ms: quantile(&mut samples, 0.99),
        max_ms: quantile(&mut samples, 1.0),
        commits_per_sec: commits as f64 / secs,
        threads: reader_threads(),
    };
    drop(store);
    report
}

/// What [`measure_writer`] saw.
#[derive(Debug, Clone, Copy)]
pub struct WriterReport {
    pub readers: usize,
    /// Median plain commit with the readers paused.
    pub alone_p50_ms: f64,
    /// Median plain commit with as many threads as readers doing CPU and
    /// memory work that never touches the store (see [`busy_work`]).
    pub busy_p50_ms: f64,
    /// Median plain commit with the readers running.
    pub under_p50_ms: f64,
    /// Reads the readers completed per second while running.
    pub reads_per_sec: f64,
}

impl WriterReport {
    /// What running readers cost the writer beyond the cost of sharing the
    /// machine's cores: `under / busy`. Where the readers and the writer have
    /// cores to themselves, `busy` equals `alone` and this is `under / alone`.
    pub fn ratio(&self) -> f64 {
        self.under_p50_ms / self.busy_p50_ms
    }

    /// How much sharing the cores alone slows the writer: `busy / alone`.
    pub fn cpu_share(&self) -> f64 {
        self.busy_p50_ms / self.alone_p50_ms
    }
}

/// Words in each busy thread's buffer: 8 MiB, past a core's share of L2 and
/// most of a 4-vCPU runner's L3, so the threads load memory the way readers
/// walking a graph do rather than spinning in registers.
const BUSY_WORDS: usize = 1 << 20;

/// One busy thread: pseudo-random reads over its own buffer until `stop`,
/// working only while `active`. It shares the writer's cores and memory
/// bandwidth as a reader would, and none of the store's locks or data.
fn busy_work(stop: Arc<AtomicBool>, active: Arc<AtomicBool>, seed: u64) {
    let buf: Vec<u64> = (0..BUSY_WORDS as u64).map(|i| i ^ seed).collect();
    let mut x = seed | 1;
    let mut acc = 0u64;
    while !stop.load(Ordering::Relaxed) {
        if !active.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_micros(200));
            continue;
        }
        for _ in 0..4_096 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            acc = acc.wrapping_add(buf[(x as usize) & (BUSY_WORDS - 1)]);
        }
        std::hint::black_box(acc);
    }
}

/// Which threads run beside the writer during one block of commits.
#[derive(Clone, Copy, PartialEq)]
enum Beside {
    Nothing,
    Busy,
    Readers,
}

/// The owner-decision scenario: on the gate's store with fsync off, time
/// plain single-property commits through `db.write()` in `rounds` rounds of
/// three blocks of `per_block` commits — one with `readers` threads paused,
/// one with as many busy threads ([`busy_work`]) sharing the cores but not
/// the store, and one with the readers capturing, looking a node up and
/// reading its neighbourhood as fast as they can. Alternating on one store
/// means a disk or load spike lands on every side, not on one. The busy block
/// separates what sharing a small machine's cores costs the writer, which
/// any working reader would cost, from what the readers' use of the store
/// costs it, which is what the owner's decision is about.
pub fn measure_writer(readers: usize, rounds: usize, per_block: usize) -> WriterReport {
    let store = build_store("writer");
    let db = &store.db;
    let stop = Arc::new(AtomicBool::new(false));
    let active = Arc::new(AtomicBool::new(false));
    let busy = Arc::new(AtomicBool::new(false));
    let busy_threads: Vec<_> = (0..readers)
        .map(|r| {
            let (stop, busy) = (Arc::clone(&stop), Arc::clone(&busy));
            std::thread::spawn(move || busy_work(stop, busy, 0x9e37_79b9 ^ r as u64))
        })
        .collect();
    let reads = Arc::new(AtomicU64::new(0));
    let threads: Vec<_> = (0..readers)
        .map(|r| {
            let (db, stop, active, reads) = (
                db.clone(),
                Arc::clone(&stop),
                Arc::clone(&active),
                Arc::clone(&reads),
            );
            std::thread::spawn(move || {
                let mut i = r;
                while !stop.load(Ordering::Relaxed) {
                    if !active.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_micros(200));
                        continue;
                    }
                    let k = key(i.wrapping_mul(7_919));
                    std::hint::black_box(db.reader().node_info(&k));
                    std::hint::black_box(read_neighborhood(&db, &k));
                    reads.fetch_add(1, Ordering::Relaxed);
                    i += readers;
                }
            })
        })
        .collect();
    let (mut alone, mut beside_busy, mut under) = (Vec::new(), Vec::new(), Vec::new());
    let (mut reading_secs, mut reads_done) = (0.0, 0u64);
    let mut i = 0usize;
    for _ in 0..rounds {
        for beside in [Beside::Nothing, Beside::Busy, Beside::Readers] {
            let running = beside == Beside::Readers;
            active.store(running, Ordering::Relaxed);
            busy.store(beside == Beside::Busy, Ordering::Relaxed);
            // Let the threads stop, or reach full speed.
            std::thread::sleep(Duration::from_millis(20));
            let (reads_before, start) = (reads.load(Ordering::Relaxed), Instant::now());
            for _ in 0..per_block {
                let k = key(i.wrapping_mul(7_919));
                let t0 = Instant::now();
                db.write()
                    .set_prop(&k, "score", Value::Int(-(i as i64)))
                    .expect("plain commit");
                let ms = t0.elapsed().as_secs_f64() * 1e3;
                match beside {
                    Beside::Nothing => alone.push(ms),
                    Beside::Busy => beside_busy.push(ms),
                    Beside::Readers => under.push(ms),
                }
                i += 1;
            }
            if running {
                reading_secs += start.elapsed().as_secs_f64();
                reads_done += reads.load(Ordering::Relaxed) - reads_before;
            }
        }
    }
    stop.store(true, Ordering::Relaxed);
    for t in threads {
        t.join().expect("reader thread");
    }
    for t in busy_threads {
        t.join().expect("busy thread");
    }
    WriterReport {
        readers,
        alone_p50_ms: quantile(&mut alone, 0.5),
        busy_p50_ms: quantile(&mut beside_busy, 0.5),
        under_p50_ms: quantile(&mut under, 0.5),
        reads_per_sec: reads_done as f64 / reading_secs,
    }
}
