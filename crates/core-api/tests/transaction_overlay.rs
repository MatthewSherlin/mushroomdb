//! A transaction's private overlay (TX-1b): what a statement inside an open
//! transaction reads and costs.
//!
//! - `bench_statement_cost_does_not_scale_with_the_store` (release, ignored):
//!   per-statement cost at 1k / 100k / 1M nodes, asserted within 2× across
//!   sizes.
//! - `randomized_tx_reads_match_write_batch_after_every_statement`: a
//!   differential probe — after every statement, reads inside the transaction
//!   equal reads of a copy that applied the same ops through `write_batch`.
//! - `fulltext_inside_tx_sees_own_writes_not_own_deletes`.

use mushroomdb::{BatchOp, Predicate, PropPredicate, RoleDef, RuleDef, SharedDb, Value};

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "mushroomdb-txo-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn p() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

fn key_param(key: &str) -> BTreeMap<String, Value> {
    let mut m = BTreeMap::new();
    m.insert("k".to_string(), Value::Str(key.to_string()));
    m
}

/// Every row of `rs`, rendered and sorted — a canonical dump to compare.
fn dump(rs: &mushroomdb::ResultSet) -> Vec<String> {
    let mut rows: Vec<String> = (0..rs.len()).map(|i| format!("{:?}", rs.row(i))).collect();
    rows.sort();
    rows
}

/// A tiny deterministic generator, so a failing sequence replays exactly.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

// ── Scaling bench ────────────────────────────────────────────────────────────

/// A store of `n` `P` nodes `k0 … k{n-1}` with `x` and two `KNOWS` out-edges
/// each, snapshotted. Built once under `root` and reused while its marker
/// file exists.
fn fixture(root: &Path, n: usize) -> PathBuf {
    let dir = root.join(format!("tx1b-fixture-{n}"));
    let marker = dir.join("TX1B_FIXTURE_COMPLETE");
    if marker.exists() {
        return dir;
    }
    let _ = std::fs::remove_dir_all(&dir);
    let t = Instant::now();
    {
        let db = SharedDb::open(&dir).unwrap();
        db.write()
            .set_fsync_policy(mushroomdb::FsyncPolicy::Relaxed);
        const CHUNK: usize = 20_000;
        for start in (0..n).step_by(CHUNK) {
            let ops = (start..(start + CHUNK).min(n))
                .map(|i| BatchOp::InsertNode {
                    label: "P".into(),
                    key: format!("k{i}"),
                    props: vec![
                        ("id".into(), Value::Str(format!("k{i}"))),
                        ("x".into(), Value::Int(i as i64)),
                    ],
                })
                .collect();
            db.submit_batch(ops).unwrap();
        }
        for start in (0..n).step_by(CHUNK) {
            let mut ops = Vec::with_capacity(2 * CHUNK);
            for i in start..(start + CHUNK).min(n) {
                for j in [(i + 1) % n, (i * 7 + 3) % n] {
                    ops.push(BatchOp::InsertEdge {
                        edge_type: "KNOWS".into(),
                        src_key: format!("k{i}"),
                        dst_key: format!("k{j}"),
                    });
                }
            }
            db.submit_batch(ops).unwrap();
        }
        db.write().snapshot().unwrap();
    }
    std::fs::write(&marker, b"ok").unwrap();
    eprintln!("built fixture n={n} in {:?}", t.elapsed());
    dir
}

const READ: &str = "MATCH (n:P {id: $k}) RETURN n.x AS x";
const EXPAND: &str = "MATCH (n:P {id: $k})-[:KNOWS]->(m:P) RETURN m.id AS m";
const SET: &str = "MATCH (n:P {id: $k}) SET n.y = 1";
const KINDS: [(&str, &str); 3] = [("point read", READ), ("1-hop expand", EXPAND), ("SET", SET)];

/// Buffer `count` statements in `tx`: CREATEs of new connected pairs and SETs
/// on existing nodes, alternating.
fn buffer_statements(tx: &mut mushroomdb::Transaction, count: usize, n: usize, rng: &mut Rng) {
    for i in 0..count {
        if i % 2 == 0 {
            tx.execute(
                &format!(
                    "CREATE (a:P {{id: 'new{i}a', x: {i}}})-[:KNOWS]->(b:P {{id: 'new{i}b'}})"
                ),
                &p(),
            )
            .unwrap();
        } else {
            let k = format!("k{}", rng.below(n));
            tx.execute("MATCH (n:P {id: $k}) SET n.z = 2", &key_param(&k))
                .unwrap();
        }
    }
}

fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    xs[xs.len() / 2]
}

/// Per-statement cost inside an open transaction at 1k / 100k / 1M nodes.
///
/// For each store size and each buffer depth (1, 10, 100 statements already
/// buffered) it times a point read, a 1-hop expand and a property SET, each
/// the median of `PER_ROUND` executions on random keys; a round visits the
/// sizes in a rotating order, and the reported number is the median of the
/// rounds. It asserts every cell is within 2× across the sizes; the < 1 ms
/// absolute bound is printed, not asserted, because the machine's load moves
/// it.
///
/// Fixtures are built once under `MUSHROOMDB_TX1B_FIXTURES` (default: the
/// system temp dir) and reused. Release build:
/// `cargo test --release -p mushroomdb --test transaction_overlay bench_ -- --ignored --nocapture`
#[test]
#[ignore]
fn bench_statement_cost_does_not_scale_with_the_store() {
    let rounds: usize = std::env::var("MUSHROOMDB_TX1B_ROUNDS")
        .map(|s| s.parse().unwrap())
        .unwrap_or(7);
    const PER_ROUND: usize = 15;
    let sizes: Vec<usize> = std::env::var("MUSHROOMDB_TX1B_SIZES")
        .map(|s| s.split(',').map(|n| n.trim().parse().unwrap()).collect())
        .unwrap_or_else(|_| vec![1_000, 100_000, 1_000_000]);
    let depths = [1usize, 10, 100];
    let root = std::env::var("MUSHROOMDB_TX1B_FIXTURES")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    let dbs: Vec<(usize, SharedDb)> = sizes
        .iter()
        .map(|&n| (n, SharedDb::open(&fixture(&root, n)).unwrap()))
        .collect();
    // Fault the snapshot's mapped pages in first, for every size alike: a
    // cold page is the operating system's cost, not the statement's.
    for (_, db) in &dbs {
        db.reader()
            .query(
                "MATCH (n:P)-[:KNOWS]->(m:P) RETURN count(m) AS c, sum(n.x) AS s, max(m.id) AS k",
                &p(),
            )
            .unwrap();
    }
    let mut rng = Rng(0x7E57_1B1B_0000_0001);
    // cells[size][depth][kind] = one median per round (µs).
    let mut cells = vec![vec![vec![Vec::new(); KINDS.len()]; depths.len()]; dbs.len()];
    // raw[size][depth][kind] = every execution (µs), for the mean and the max:
    // a cost paid once per transaction hides from a median.
    let mut raw = vec![vec![vec![Vec::new(); KINDS.len()]; depths.len()]; dbs.len()];
    // first[size] = BEGIN + the first statement (µs), one per round.
    let mut first = vec![Vec::new(); dbs.len()];
    for round in 0..rounds {
        for step in 0..dbs.len() {
            let s = (round + step) % dbs.len();
            let (n, db) = &dbs[s];
            let t = Instant::now();
            let mut tx = db.begin_transaction();
            tx.execute(READ, &key_param(&format!("k{}", rng.below(*n))))
                .unwrap();
            first[s].push(t.elapsed().as_secs_f64() * 1e6);
            tx.rollback();
            for (d, &depth) in depths.iter().enumerate() {
                let mut tx = db.begin_transaction();
                buffer_statements(&mut tx, depth, *n, &mut rng);
                for (k, (_, stmt)) in KINDS.iter().enumerate() {
                    let mut xs = Vec::with_capacity(PER_ROUND);
                    for _ in 0..PER_ROUND {
                        let key = format!("k{}", rng.below(*n));
                        let params = key_param(&key);
                        let t = Instant::now();
                        tx.execute(stmt, &params).unwrap();
                        xs.push(t.elapsed().as_secs_f64() * 1e6);
                    }
                    raw[s][d][k].extend_from_slice(&xs);
                    cells[s][d][k].push(median(xs));
                }
                tx.rollback();
            }
        }
    }
    let load = std::process::Command::new("uptime")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    println!("machine: {load}");
    println!("rounds={rounds} per_round={PER_ROUND} (median of per-round medians, µs)");
    let header: Vec<String> = dbs.iter().map(|(n, _)| format!("{n:>10}")).collect();
    println!(
        "| {:<14} | {:>8} | {} | max/min |",
        "statement",
        "buffered",
        header.join(" | ")
    );
    let mut worst: f64 = 1.0;
    let mut over_1ms = Vec::new();
    for (k, (name, _)) in KINDS.iter().enumerate() {
        for (d, depth) in depths.iter().enumerate() {
            let per_size: Vec<f64> = (0..dbs.len())
                .map(|s| median(cells[s][d][k].clone()))
                .collect();
            let max = per_size.iter().cloned().fold(f64::MIN, f64::max);
            let min = per_size.iter().cloned().fold(f64::MAX, f64::min);
            let ratio = max / min;
            worst = worst.max(ratio);
            for (s, us) in per_size.iter().enumerate() {
                if *us >= 1000.0 {
                    over_1ms.push(format!("{name} n={} buffered={depth}: {us:.0}µs", dbs[s].0));
                }
            }
            let cols: Vec<String> = per_size.iter().map(|us| format!("{us:>10.1}")).collect();
            println!(
                "| {name:<14} | {depth:>8} | {} | {ratio:>7.2} |",
                cols.join(" | ")
            );
        }
    }
    println!("mean / max of every execution (µs):");
    for (k, (name, _)) in KINDS.iter().enumerate() {
        for (d, depth) in depths.iter().enumerate() {
            let cols: Vec<String> = (0..dbs.len())
                .map(|s| {
                    let xs = &raw[s][d][k];
                    let mean = xs.iter().sum::<f64>() / xs.len() as f64;
                    let max = xs.iter().cloned().fold(f64::MIN, f64::max);
                    format!("{mean:>8.1} / {max:>8.1}")
                })
                .collect();
            println!("| {name:<14} | {depth:>8} | {} |", cols.join(" | "));
        }
    }
    let firsts: Vec<String> = (0..dbs.len())
        .map(|s| format!("n={}: {:.1}µs", dbs[s].0, median(first[s].clone())))
        .collect();
    println!("BEGIN + first point read: {}", firsts.join(", "));
    if over_1ms.is_empty() {
        println!("< 1 ms bound: every cell under 1 ms");
    } else {
        println!("< 1 ms bound NOT met (reported, not asserted): {over_1ms:?}");
    }
    assert!(
        worst <= 2.0,
        "a statement's cost moved {worst:.2}× across store sizes (bound 2×)"
    );
}

// ── Differential: in-tx reads vs write_batch on a copy ──────────────────────

const WORDS: [&str; 5] = ["apple", "pear", "plum", "fig", "quince"];

fn text(rng: &mut Rng) -> String {
    format!("{} {}", WORDS[rng.below(5)], WORDS[rng.below(5)])
}

/// The same history on two stores: 30 `P` nodes with `x` and `t` and two
/// `KNOWS` out-edges each, a snapshot, then 70 single-op commits (one fold
/// past the snapshot and a few commits of delta tail), so reads cross every
/// layer below a transaction — mmap base, folded overlay, unfolded tail.
fn seed(db: &SharedDb, rng: &mut Rng) -> (Vec<String>, BTreeSet<(String, String)>) {
    let mut live: Vec<String> = Vec::new();
    let mut edges = BTreeSet::new();
    let mut ops = Vec::new();
    for i in 0..30 {
        let k = format!("p{i}");
        ops.push(BatchOp::InsertNode {
            label: "P".into(),
            key: k.clone(),
            props: vec![
                ("id".into(), Value::Str(k.clone())),
                ("x".into(), Value::Int((i % 5) as i64)),
                ("t".into(), Value::Str(text(rng))),
            ],
        });
        live.push(k);
    }
    for i in 0..30usize {
        for j in [(i + 1) % 30, (i * 7 + 3) % 30] {
            let e = (format!("p{i}"), format!("p{j}"));
            if edges.insert(e.clone()) {
                ops.push(BatchOp::InsertEdge {
                    edge_type: "KNOWS".into(),
                    src_key: e.0,
                    dst_key: e.1,
                });
            }
        }
    }
    db.submit_batch(ops).unwrap();
    db.write().snapshot().unwrap();
    for i in 0..70 {
        let op = if i % 3 == 0 {
            let k = format!("q{i}");
            live.push(k.clone());
            BatchOp::InsertNode {
                label: "P".into(),
                key: k.clone(),
                props: vec![
                    ("id".into(), Value::Str(k)),
                    ("x".into(), Value::Int(rng.below(5) as i64)),
                ],
            }
        } else {
            BatchOp::SetProp {
                key: live[rng.below(live.len())].clone(),
                field: "x".into(),
                value: Value::Int(rng.below(5) as i64),
            }
        };
        db.submit_batch(vec![op]).unwrap();
    }
    (live, edges)
}

const DIFF_PROBES: &[&str] = &[
    "MATCH (n:P) RETURN n.id AS k, n.x AS x, n.t AS t",
    "MATCH (a:P)-[:KNOWS]->(b:P) RETURN a.id AS s, b.id AS d",
    "MATCH (n:P) RETURN count(n) AS c",
    "MATCH (n) RETURN count(n) AS c",
    "MATCH (n:P) WHERE n.x = 3 RETURN n.id AS k",
    "MATCH (n:P) WHERE textMatches(n.t, 'apple') RETURN n.id AS k",
    "MATCH (n:P {id: $k}) RETURN n.x AS x, n.t AS t",
    "MATCH (n:P {id: $k})-[:KNOWS]-(m) RETURN m.id AS m",
    "MATCH (a:P)-[:KNOWS]->(b)-[:KNOWS]->(c) RETURN count(*) AS c",
];

/// One random statement: its Cypher for the transaction, and the same ops for
/// the copy's `write_batch`.
type CopyOps = Vec<BatchOp>;

fn random_statement(
    rng: &mut Rng,
    step: usize,
    live: &mut Vec<String>,
    dead: &mut Vec<String>,
    edges: &mut BTreeSet<(String, String)>,
) -> (String, CopyOps) {
    let id_props = |k: &str| vec![("id".to_string(), Value::Str(k.to_string()))];
    let pick = |rng: &mut Rng, live: &[String]| live[rng.below(live.len())].clone();
    loop {
        match rng.below(9) {
            // CREATE one node; sometimes the key of a node deleted earlier.
            0 | 1 => {
                let k = if !dead.is_empty() && rng.below(3) == 0 {
                    dead.swap_remove(rng.below(dead.len()))
                } else {
                    format!("n{step}")
                };
                let x = rng.below(5) as i64;
                let t = text(rng);
                live.push(k.clone());
                let mut props = id_props(&k);
                props.push(("x".into(), Value::Int(x)));
                props.push(("t".into(), Value::Str(t.clone())));
                return (
                    format!("CREATE (n:P {{id: '{k}', x: {x}, t: '{t}'}})"),
                    vec![BatchOp::InsertNode {
                        label: "P".into(),
                        key: k,
                        props,
                    }],
                );
            }
            // CREATE a connected pair.
            2 => {
                let (a, b) = (format!("n{step}a"), format!("n{step}b"));
                let x = rng.below(5) as i64;
                live.push(a.clone());
                live.push(b.clone());
                edges.insert((a.clone(), b.clone()));
                let mut a_props = id_props(&a);
                a_props.push(("x".into(), Value::Int(x)));
                return (
                    format!("CREATE (a:P {{id: '{a}', x: {x}}})-[:KNOWS]->(b:P {{id: '{b}'}})"),
                    vec![
                        BatchOp::InsertNode {
                            label: "P".into(),
                            key: a.clone(),
                            props: a_props,
                        },
                        BatchOp::InsertNode {
                            label: "P".into(),
                            key: b.clone(),
                            props: id_props(&b),
                        },
                        BatchOp::InsertEdge {
                            edge_type: "KNOWS".into(),
                            src_key: a,
                            dst_key: b,
                        },
                    ],
                );
            }
            // SET x or t on a live node.
            3 | 4 if !live.is_empty() => {
                let k = pick(rng, live);
                let (field, lit, value) = if rng.below(2) == 0 {
                    let x = rng.below(5) as i64;
                    ("x", x.to_string(), Value::Int(x))
                } else {
                    let t = text(rng);
                    ("t", format!("'{t}'"), Value::Str(t))
                };
                return (
                    format!("MATCH (n:P {{id: '{k}'}}) SET n.{field} = {lit}"),
                    vec![BatchOp::SetProp {
                        key: k,
                        field: field.into(),
                        value,
                    }],
                );
            }
            // DELETE an edge.
            5 if !edges.is_empty() => {
                let i = rng.below(edges.len());
                let (s, d) = edges.iter().nth(i).cloned().unwrap();
                edges.remove(&(s.clone(), d.clone()));
                return (
                    format!("MATCH (a:P {{id: '{s}'}})-[r:KNOWS]->(b:P {{id: '{d}'}}) DELETE r"),
                    vec![BatchOp::DeleteEdge {
                        edge_type: "KNOWS".into(),
                        src_key: s,
                        dst_key: d,
                    }],
                );
            }
            // DETACH DELETE a live node.
            6 if live.len() > 5 => {
                let k = live.swap_remove(rng.below(live.len()));
                edges.retain(|(s, d)| s != &k && d != &k);
                dead.push(k.clone());
                return (
                    format!("MATCH (n:P {{id: '{k}'}}) DETACH DELETE n"),
                    vec![BatchOp::DeleteNode { key: k }],
                );
            }
            // MERGE a live or a new key.
            7 => {
                let k = if rng.below(2) == 0 && !live.is_empty() {
                    pick(rng, live)
                } else {
                    format!("m{step}")
                };
                let ops = if live.contains(&k) {
                    vec![BatchOp::SetProp {
                        key: k.clone(),
                        field: "x".into(),
                        value: Value::Int(2),
                    }]
                } else {
                    live.push(k.clone());
                    vec![
                        BatchOp::InsertNode {
                            label: "P".into(),
                            key: k.clone(),
                            props: id_props(&k),
                        },
                        BatchOp::SetProp {
                            key: k.clone(),
                            field: "x".into(),
                            value: Value::Int(1),
                        },
                    ]
                };
                return (
                    format!("MERGE (n:P {{id: '{k}'}}) ON CREATE SET n.x = 1 ON MATCH SET n.x = 2"),
                    ops,
                );
            }
            // A read: changes nothing.
            8 => {
                let k = pick(rng, live);
                return (
                    format!("MATCH (n:P {{id: '{k}'}})-[:KNOWS]->(m) RETURN m.id AS m"),
                    vec![],
                );
            }
            _ => continue,
        }
    }
}

/// After every statement of a random sequence — inserts, SETs, edge and node
/// deletes, MERGEs, reads — every probe read inside the transaction equals the
/// same read of a copy that applied the same ops through `write_batch`, read
/// through the copy's live handle.
#[test]
fn randomized_tx_reads_match_write_batch_after_every_statement() {
    for seed_value in [0xD1FF_0001u64, 0xD1FF_0002, 0xD1FF_0003] {
        let mut rng = Rng(seed_value);
        let tx_db = SharedDb::open(&tmp("diff-tx")).unwrap();
        let copy = SharedDb::open(&tmp("diff-copy")).unwrap();
        let (mut live, mut edges) = seed(&tx_db, &mut Rng(seed_value ^ 0xABCD));
        seed(&copy, &mut Rng(seed_value ^ 0xABCD));
        let mut dead = Vec::new();
        let mut tx = tx_db.begin_transaction();
        for step in 0..150 {
            let (cypher, ops) = random_statement(&mut rng, step, &mut live, &mut dead, &mut edges);
            tx.execute(&cypher, &p())
                .unwrap_or_else(|e| panic!("seed {seed_value:#x} step {step}: {cypher}: {e:?}"));
            if !ops.is_empty() {
                copy.write()
                    .write_batch(|b| {
                        for op in ops {
                            match op {
                                BatchOp::InsertNode { label, key, props } => {
                                    b.insert_node(&label, &key, props);
                                }
                                BatchOp::SetProp { key, field, value } => {
                                    b.set_prop(&key, &field, value);
                                }
                                BatchOp::InsertEdge {
                                    edge_type,
                                    src_key,
                                    dst_key,
                                } => {
                                    b.insert_edge(&edge_type, &src_key, &dst_key);
                                }
                                BatchOp::DeleteEdge {
                                    edge_type,
                                    src_key,
                                    dst_key,
                                } => {
                                    b.delete_edge(&edge_type, &src_key, &dst_key);
                                }
                                BatchOp::DeleteNode { key } => {
                                    b.delete_node(&key);
                                }
                                _ => unreachable!("random_statement emits only these ops"),
                            }
                        }
                    })
                    .unwrap();
            }
            let probe_key = if !dead.is_empty() && rng.below(4) == 0 {
                dead[rng.below(dead.len())].clone()
            } else {
                live[rng.below(live.len())].clone()
            };
            let params = key_param(&probe_key);
            // The oracle is the copy's live write handle, which reads its own
            // state and never a delta layer: a defect a layered reader shared
            // with the transaction would not hide.
            let live_copy = copy.read();
            for probe in DIFF_PROBES {
                let inside = dump(&tx.execute(probe, &params).unwrap());
                let want = dump(&live_copy.query(probe, &params).unwrap());
                assert_eq!(
                    inside, want,
                    "seed {seed_value:#x} step {step} after `{cypher}`: probe `{probe}` (k = {probe_key})"
                );
            }
        }
        tx.rollback();
    }
}

// ── Full text inside a transaction ───────────────────────────────────────────

/// Full-text matching inside a transaction answers from the transaction's own
/// state: its inserted and rewritten text matches, the text of a node it
/// deleted or rewrote does not — and after the commit the store's full-text
/// index agrees.
#[test]
fn fulltext_inside_tx_sees_own_writes_not_own_deletes() {
    let dir = tmp("fulltext");
    let db = SharedDb::open(&dir).unwrap();
    db.write().enable_fulltext("Doc", "body").unwrap();
    let doc = |key: &str, body: &str| BatchOp::InsertNode {
        label: "Doc".into(),
        key: key.into(),
        props: vec![
            ("id".into(), Value::Str(key.into())),
            ("body".into(), Value::Str(body.into())),
        ],
    };
    db.submit_batch(vec![
        doc("old-fox", "a fox in the den"),
        doc("rewritten", "the fox ran"),
        doc("kept", "fox and hound"),
    ])
    .unwrap();
    db.write().snapshot().unwrap();
    let foxes = "MATCH (n:Doc) WHERE textMatches(n.body, 'fox') RETURN n.id AS k";

    let mut tx = db.begin_transaction();
    tx.execute(
        "CREATE (n:Doc {id: 'new-fox', body: 'quick brown fox'})",
        &p(),
    )
    .unwrap();
    tx.execute("MATCH (n:Doc {id: 'old-fox'}) DETACH DELETE n", &p())
        .unwrap();
    tx.execute(
        "MATCH (n:Doc {id: 'rewritten'}) SET n.body = 'the cat sat'",
        &p(),
    )
    .unwrap();
    tx.execute(
        "MATCH (n:Doc {id: 'kept'}) SET n.body = 'fox, renamed fox'",
        &p(),
    )
    .unwrap();
    let inside = dump(&tx.execute(foxes, &p()).unwrap());
    let key = |k: &str| format!("{:?}", vec![Some(Value::Str(k.into()))]);
    assert_eq!(inside, vec![key("kept"), key("new-fox")]);
    // Outside, nothing moved yet.
    let outside = dump(&db.reader().query(foxes, &p()).unwrap());
    assert_eq!(outside, vec![key("kept"), key("old-fox"), key("rewritten")]);

    tx.commit().unwrap();
    assert_eq!(dump(&db.reader().query(foxes, &p()).unwrap()), inside);
    let mut indexed: Vec<String> = db
        .write()
        .search("body", "fox")
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    indexed.sort();
    assert_eq!(indexed, ["kept", "new-fox"]);
}

// ── Reads outside a transaction (hot-path regression check) ─────────────────

/// Reader-snapshot query cost outside any transaction, for comparing builds:
/// the transaction layer added a branch to label scans and key lookups that
/// every read takes. Same fixtures as the scaling bench; prints medians,
/// asserts nothing. Release build:
/// `cargo test --release -p mushroomdb --test transaction_overlay bench_reader -- --ignored --nocapture`
#[test]
#[ignore]
fn bench_reader_queries_outside_a_tx() {
    let rounds: usize = std::env::var("MUSHROOMDB_TX1B_ROUNDS")
        .map(|s| s.parse().unwrap())
        .unwrap_or(7);
    const PER_ROUND: usize = 11;
    let sizes: Vec<usize> = std::env::var("MUSHROOMDB_TX1B_SIZES")
        .map(|s| s.split(',').map(|n| n.trim().parse().unwrap()).collect())
        .unwrap_or_else(|_| vec![100_000, 1_000_000]);
    let root = std::env::var("MUSHROOMDB_TX1B_FIXTURES")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    let queries = [
        ("label scan count", "MATCH (n:P) RETURN count(n) AS c"),
        (
            "scan + x filter",
            "MATCH (n:P) WHERE n.x = 5 RETURN n.id AS k",
        ),
        ("point read", READ),
        ("1-hop expand", EXPAND),
    ];
    // `MUSHROOMDB_TX1B_TAIL=N`: read a copy of each fixture with N single-SET
    // commits of unfolded delta tail (N < FOLD_EVERY_K).
    let tail: usize = std::env::var("MUSHROOMDB_TX1B_TAIL")
        .map(|s| s.parse().unwrap())
        .unwrap_or(0);
    let mut copies = Vec::new();
    let dbs: Vec<(usize, SharedDb)> = sizes
        .iter()
        .map(|&n| {
            let mut dir = fixture(&root, n);
            if tail > 0 {
                let copy = tmp(&format!("reader-tail-{n}"));
                copy_dir(&dir, &copy);
                copies.push(copy.clone());
                dir = copy;
            }
            let db = SharedDb::open(&dir).unwrap();
            let mut rng = Rng(0x7A11);
            for _ in 0..tail {
                db.submit_batch(vec![BatchOp::SetProp {
                    key: format!("k{}", rng.below(n)),
                    field: "x".into(),
                    value: Value::Int(7),
                }])
                .unwrap();
            }
            db.reader()
                .query(
                    "MATCH (n:P)-[:KNOWS]->(m:P) RETURN count(m) AS c, sum(n.x) AS s, max(m.id) AS k",
                    &p(),
                )
                .unwrap();
            (n, db)
        })
        .collect();
    let mut rng = Rng(0x0BAD_5EED);
    let mut cells = vec![vec![Vec::new(); queries.len() + 1]; dbs.len()];
    for round in 0..rounds {
        for step in 0..dbs.len() {
            let s = (round + step) % dbs.len();
            let (n, db) = &dbs[s];
            let reader = db.reader();
            for (q, (_, cypher)) in queries.iter().enumerate() {
                let mut xs = Vec::with_capacity(PER_ROUND);
                for _ in 0..PER_ROUND {
                    let params = key_param(&format!("k{}", rng.below(*n)));
                    let t = Instant::now();
                    reader.query(cypher, &params).unwrap();
                    xs.push(t.elapsed().as_secs_f64() * 1e6);
                }
                cells[s][q].push(median(xs));
            }
            // A fresh snapshot per read, as an HTTP request takes one: the
            // snapshot's first read builds its view of the tail.
            let mut xs = Vec::with_capacity(PER_ROUND);
            for _ in 0..PER_ROUND {
                let params = key_param(&format!("k{}", rng.below(*n)));
                let t = Instant::now();
                db.reader().query(READ, &params).unwrap();
                xs.push(t.elapsed().as_secs_f64() * 1e6);
            }
            cells[s][queries.len()].push(median(xs));
        }
    }
    println!(
        "machine: {}",
        std::process::Command::new("uptime")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    );
    println!("delta tail: {tail} commits");
    let names = queries
        .iter()
        .map(|(name, _)| *name)
        .chain(["fresh reader + point read"]);
    for (q, name) in names.enumerate() {
        let cols: Vec<String> = (0..dbs.len())
            .map(|s| format!("n={}: {:.1}µs", dbs[s].0, median(cells[s][q].clone())))
            .collect();
        println!("| {name:<25} | {} |", cols.join(" | "));
    }
    drop(dbs);
    for copy in copies {
        let _ = std::fs::remove_dir_all(copy);
    }
}

// ── Readers under a full-text writer (the TX-7 gate, in process) ────────────

/// A `memory_defaults()` store (full-text and an equality index on
/// `Person.name`, the store `mushroomdb serve` creates) with `n` `Person`
/// nodes and two `KNOWS` out-edges each, snapshotted.
fn memory_fixture(root: &Path, n: usize) -> PathBuf {
    let dir = root.join(format!("tx1b-memory-{n}"));
    let marker = dir.join("TX1B_FIXTURE_COMPLETE");
    if marker.exists() {
        return dir;
    }
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = SharedDb::open(&dir).unwrap();
        db.write()
            .apply_schema(&mushroomdb::memory_schema::memory_defaults())
            .unwrap();
        db.write()
            .set_fsync_policy(mushroomdb::FsyncPolicy::Relaxed);
        let mut rng = Rng(0x3E30_0001);
        const CHUNK: usize = 10_000;
        for start in (0..n).step_by(CHUNK) {
            let ops = (start..(start + CHUNK).min(n))
                .map(|i| BatchOp::InsertNode {
                    label: "Person".into(),
                    key: format!("p{i}"),
                    props: vec![
                        ("id".into(), Value::Str(format!("p{i}"))),
                        ("name".into(), Value::Str(text(&mut rng))),
                    ],
                })
                .collect();
            db.submit_batch(ops).unwrap();
        }
        for start in (0..n).step_by(CHUNK) {
            let mut ops = Vec::new();
            for i in start..(start + CHUNK).min(n) {
                for j in [(i + 1) % n, (i * 7 + 3) % n] {
                    ops.push(BatchOp::InsertEdge {
                        edge_type: "KNOWS".into(),
                        src_key: format!("p{i}"),
                        dst_key: format!("p{j}"),
                    });
                }
            }
            db.submit_batch(ops).unwrap();
        }
        db.write().snapshot().unwrap();
    }
    std::fs::write(&marker, b"ok").unwrap();
    dir
}

/// Reader latency while a writer sets the full-text-indexed `name` at 50/s:
/// 16 threads × 100 depth-1 neighbourhood reads 20 ms apart (so a run spans
/// about 100 commits and a fold), each on a fresh reader snapshot (as an HTTP
/// request takes one), at 100k nodes. Prints p50 / p99 / max per run and
/// asserts the TX-7 gate, p99 ≤ 2 ms, on the median run.
/// Fixture under `MUSHROOMDB_TX1B_FIXTURES`. Release build:
/// `cargo test --release -p mushroomdb --test transaction_overlay bench_reader_p99 -- --ignored --nocapture`
#[test]
#[ignore]
fn bench_reader_p99_under_a_fulltext_writer() {
    let runs: usize = std::env::var("MUSHROOMDB_TX1B_ROUNDS")
        .map(|s| s.parse().unwrap())
        .unwrap_or(6);
    let n: usize = std::env::var("MUSHROOMDB_TX1B_MEMORY_NODES")
        .map(|s| s.parse().unwrap())
        .unwrap_or(100_000);
    let root = std::env::var("MUSHROOMDB_TX1B_FIXTURES")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    // `MUSHROOMDB_TX1B_P99_QUERY=indexed`: an equality lookup on the indexed
    // `Person.name` instead of the neighbourhood read.
    let indexed = std::env::var("MUSHROOMDB_TX1B_P99_QUERY").is_ok_and(|q| q == "indexed");
    let cypher: &'static str = if indexed {
        "MATCH (n:Person {name: $k}) RETURN n.id AS id"
    } else {
        "MATCH (n:Person {id: $k})-[:KNOWS]-(m) RETURN m.id AS m"
    };
    let fixture = memory_fixture(&root, n);
    let mut p99s = Vec::new();
    for run in 0..runs {
        // A fresh copy each run: the writer's commits would otherwise pile up.
        let dir = tmp(&format!("p99-{run}"));
        copy_dir(&fixture, &dir);
        let db = SharedDb::open(&dir).unwrap();
        db.reader()
            .query(
                "MATCH (n:Person)-[:KNOWS]->(m) RETURN count(m) AS c, max(m.name) AS k",
                &p(),
            )
            .unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writer = {
            let (db, stop) = (db.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut rng = Rng(0x0057_A7E5 + run as u64);
                let mut commits = 0usize;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    db.submit_batch(vec![BatchOp::SetProp {
                        key: format!("p{}", rng.below(n)),
                        field: "name".into(),
                        value: Value::Str(text(&mut rng)),
                    }])
                    .unwrap();
                    commits += 1;
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                commits
            })
        };
        // Let the tail fill before reading.
        std::thread::sleep(std::time::Duration::from_millis(200));
        let readers: Vec<_> = (0..16)
            .map(|t| {
                let db = db.clone();
                std::thread::spawn(move || {
                    let mut rng = Rng(0x00EA_D000 + (run * 16 + t) as u64);
                    let mut xs = Vec::with_capacity(100);
                    for _ in 0..100 {
                        let params = if indexed {
                            let mut m = BTreeMap::new();
                            m.insert("k".to_string(), Value::Str(text(&mut rng)));
                            m
                        } else {
                            key_param(&format!("p{}", rng.below(n)))
                        };
                        let t = Instant::now();
                        let reader = db.reader();
                        let capture = t.elapsed().as_secs_f64() * 1e3;
                        reader.query(cypher, &params).unwrap();
                        xs.push((t.elapsed().as_secs_f64() * 1e3, capture));
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    xs
                })
            })
            .collect();
        let samples: Vec<(f64, f64)> = readers
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let commits = writer.join().unwrap();
        let sorted = |mut v: Vec<f64>| {
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            v
        };
        let xs = sorted(samples.iter().map(|s| s.0).collect());
        // Capture (`SharedDb::reader`) takes the store's read lock, so it waits
        // out any write guard; the rest is the read itself.
        let captures = sorted(samples.iter().map(|s| s.1).collect());
        let reads = sorted(samples.iter().map(|s| s.0 - s.1).collect());
        let at = |v: &[f64], q: f64| v[((v.len() as f64 - 1.0) * q) as usize];
        println!(
            "run {run}: n={n} reads={} writer commits={commits} p50={:.3}ms p99={:.3}ms \
             max={:.3}ms | capture p99={:.3}ms | read-after-capture p99={:.3}ms",
            xs.len(),
            at(&xs, 0.5),
            at(&xs, 0.99),
            xs[xs.len() - 1],
            at(&captures, 0.99),
            at(&reads, 0.99),
        );
        p99s.push(at(&xs, 0.99));
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }
    println!(
        "machine: {}",
        std::process::Command::new("uptime")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    );
    let p99 = median(p99s);
    println!("median p99 over {runs} runs: {p99:.3}ms (gate 2 ms)");
    assert!(
        p99 <= 2.0,
        "reader p99 {p99:.3}ms under a full-text writer (gate 2 ms)"
    );
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

// ── Differential: a reader's delta tail vs the live handle ──────────────────

/// After every commit of a random sequence — inserts (some re-using a deleted
/// key), SETs (some of a full-text field), property removals, renames, edge
/// and node deletes — a fresh reader snapshot, which reads the unfolded tail
/// through its layer, answers every probe exactly as the live handle does.
/// The sequence crosses a fold (more than `FOLD_EVERY_K` commits) and starts
/// over a snapshot, so every layer below the tail is in play.
#[test]
fn reader_tail_layer_matches_the_live_handle_after_every_commit() {
    for seed_value in [0x7A11_0001u64, 0x7A11_0002] {
        let mut rng = Rng(seed_value);
        let db = SharedDb::open(&tmp("tail-live")).unwrap();
        db.write().enable_fulltext("P", "t").unwrap();
        let (mut live, mut edges) = seed(&db, &mut Rng(seed_value ^ 0x5EED));
        let mut dead: Vec<String> = Vec::new();
        for step in 0..(mushroomdb::FOLD_EVERY_K + 40) {
            let pick = |rng: &mut Rng, live: &[String]| live[rng.below(live.len())].clone();
            let ops: Vec<BatchOp> = match rng.below(8) {
                0 | 1 => {
                    let k = if !dead.is_empty() && rng.below(3) == 0 {
                        dead.swap_remove(rng.below(dead.len()))
                    } else {
                        format!("r{step}")
                    };
                    live.push(k.clone());
                    vec![BatchOp::InsertNode {
                        label: "P".into(),
                        key: k.clone(),
                        props: vec![
                            ("id".into(), Value::Str(k)),
                            ("t".into(), Value::Str(text(&mut rng))),
                        ],
                    }]
                }
                2 => vec![BatchOp::SetProp {
                    key: pick(&mut rng, &live),
                    field: "t".into(),
                    value: Value::Str(text(&mut rng)),
                }],
                3 => vec![BatchOp::SetProp {
                    key: pick(&mut rng, &live),
                    field: "x".into(),
                    value: Value::Int(rng.below(5) as i64),
                }],
                4 => vec![BatchOp::RemoveProp {
                    key: pick(&mut rng, &live),
                    field: if rng.below(2) == 0 { "x" } else { "t" }.into(),
                }],
                5 if !edges.is_empty() => {
                    let (s, d) = edges.iter().nth(rng.below(edges.len())).cloned().unwrap();
                    edges.remove(&(s.clone(), d.clone()));
                    vec![BatchOp::DeleteEdge {
                        edge_type: "KNOWS".into(),
                        src_key: s,
                        dst_key: d,
                    }]
                }
                6 if live.len() > 5 => {
                    let k = live.swap_remove(rng.below(live.len()));
                    edges.retain(|(s, d)| s != &k && d != &k);
                    dead.push(k.clone());
                    vec![BatchOp::DeleteNode { key: k }]
                }
                _ => {
                    let i = rng.below(live.len());
                    let (old, new) = (live[i].clone(), format!("{}-renamed{step}", live[i]));
                    live[i] = new.clone();
                    edges = edges
                        .into_iter()
                        .map(|(s, d)| {
                            let s = if s == old { new.clone() } else { s };
                            let d = if d == old { new.clone() } else { d };
                            (s, d)
                        })
                        .collect();
                    vec![BatchOp::RenameNode {
                        old_key: old,
                        new_key: new,
                    }]
                }
            };
            db.submit_batch(ops).unwrap();
            let probe_key = live[rng.below(live.len())].clone();
            let params = key_param(&probe_key);
            let reader = db.reader();
            let live_db = db.read();
            for probe in DIFF_PROBES {
                let got = dump(&reader.query(probe, &params).unwrap());
                let want = dump(&live_db.query(probe, &params).unwrap());
                assert_eq!(
                    got, want,
                    "seed {seed_value:#x} step {step}: probe `{probe}` (k = {probe_key})"
                );
            }
            assert_eq!(
                reader.node_info(&probe_key).map(|i| (i.label, i.props)),
                live_db.node_info(&probe_key).map(|i| (i.label, i.props)),
                "seed {seed_value:#x} step {step}: node_info({probe_key})"
            );
            let reader_edges: Vec<_> = reader
                .node_edges(&probe_key)
                .unwrap()
                .into_iter()
                .map(|e| (e.edge_type, e.src_key, e.dst_key, e.derived))
                .collect();
            let live_edges: Vec<_> = live_db
                .node_edges(&probe_key)
                .unwrap()
                .into_iter()
                .map(|e| (e.edge_type, e.src_key, e.dst_key, e.derived))
                .collect();
            assert_eq!(
                reader_edges, live_edges,
                "seed {seed_value:#x} step {step}: node_edges"
            );
        }
    }
}

// ── Committed-tail cases against the live handle ────────────────────────────

/// How a scenario's store is prepared below its delta tail.
#[derive(Clone, Copy, Debug)]
enum Below {
    /// Never snapshotted: the fold holds everything.
    NoSnapshot,
    /// Snapshotted first: the mmap base holds the seed.
    Snapshot,
    /// Snapshotted, then a fold's worth of commits: base, folded overlay
    /// and tail all non-empty.
    AfterFold,
}

const BELOW: [Below; 3] = [Below::NoSnapshot, Below::Snapshot, Below::AfterFold];

const TAIL_PROBES: &[&str] = &[
    "MATCH (a)-[r]->(b) RETURN a.id AS s, type(r) AS t, b.id AS d",
    "MATCH ()-[r]->() RETURN count(r) AS c",
    "MATCH (n) RETURN n.id AS k, labels(n) AS l, n.x AS x, n.tags AS tags",
    "MATCH (n {id: $k})-[]-(m) RETURN m.id AS m",
];

fn p_node(key: &str, props: Vec<(&str, Value)>) -> BatchOp {
    let mut all = vec![("id".to_string(), Value::Str(key.into()))];
    all.extend(props.into_iter().map(|(f, v)| (f.to_string(), v)));
    BatchOp::InsertNode {
        label: "P".into(),
        key: key.into(),
        props: all,
    }
}

fn knows(src: &str, dst: &str) -> BatchOp {
    BatchOp::InsertEdge {
        edge_type: "KNOWS".into(),
        src_key: src.into(),
        dst_key: dst.into(),
    }
}

fn unknows(src: &str, dst: &str) -> BatchOp {
    BatchOp::DeleteEdge {
        edge_type: "KNOWS".into(),
        src_key: src.into(),
        dst_key: dst.into(),
    }
}

fn set(key: &str, field: &str, value: Value) -> BatchOp {
    BatchOp::SetProp {
        key: key.into(),
        field: field.into(),
        value,
    }
}

fn tags(list: &[&str]) -> Value {
    Value::List(list.iter().map(|t| Value::Str((*t).into())).collect())
}

/// A store with `a`, `b`, `c` (`KNOWS` a→b, b→c), prepared as `below` says.
fn tail_store(name: &str, below: Below, setup: impl FnOnce(&SharedDb)) -> SharedDb {
    let db = SharedDb::open(&tmp(&format!("{name}-{below:?}"))).unwrap();
    setup(&db);
    db.submit_batch(vec![
        p_node("a", vec![("x", Value::Int(1))]),
        p_node("b", vec![("x", Value::Int(2))]),
        p_node("c", vec![("x", Value::Int(1))]),
        knows("a", "b"),
        knows("b", "c"),
    ])
    .unwrap();
    if !matches!(below, Below::NoSnapshot) {
        db.write().snapshot().unwrap();
    }
    if matches!(below, Below::AfterFold) {
        for i in 0..(mushroomdb::FOLD_EVERY_K + 2) {
            db.submit_batch(vec![p_node(&format!("f{i}"), vec![])])
                .unwrap();
        }
    }
    db
}

/// A fresh reader — which reads the unfolded tail through its layer — equals
/// the live handle on every probe and on `node_edges` (`derived` included)
/// for each of `keys`.
fn assert_tail_matches_live(db: &SharedDb, keys: &[&str], what: &str) {
    let reader = db.reader();
    assert!(
        !reader.deltas.is_empty(),
        "{what}: the tail is empty, so the layer is not under test"
    );
    let live = db.read();
    for key in keys {
        let params = key_param(key);
        for probe in TAIL_PROBES {
            assert_eq!(
                dump(&reader.query(probe, &params).unwrap()),
                dump(&live.query(probe, &params).unwrap()),
                "{what}: probe `{probe}` (k = {key})"
            );
        }
        let edges = |r: mushroomdb::Result<Vec<mushroomdb::EdgeInfo>>| {
            r.ok().map(|es| {
                es.into_iter()
                    .map(|e| (e.edge_type, e.src_key, e.dst_key, e.derived))
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(
            edges(reader.node_edges(key)),
            edges(live.node_edges(key)),
            "{what}: node_edges({key})"
        );
        assert_eq!(
            reader.node_info(key).map(|i| (i.label, i.props)),
            live.node_info(key).map(|i| (i.label, i.props)),
            "{what}: node_info({key})"
        );
    }
}

/// Run `steps` one commit each over every `Below`, checking after each one and
/// again once a fold has taken the steps in.
fn run_tail_case(name: &str, keys: &[&str], steps: impl Fn() -> Vec<Vec<BatchOp>>) {
    run_tail_case_with(name, keys, |_| {}, steps);
}

fn run_tail_case_with(
    name: &str,
    keys: &[&str],
    setup: impl Fn(&SharedDb),
    steps: impl Fn() -> Vec<Vec<BatchOp>>,
) {
    for below in BELOW {
        let db = tail_store(name, below, &setup);
        for (i, step) in steps().into_iter().enumerate() {
            db.submit_batch(step).unwrap();
            assert_tail_matches_live(&db, keys, &format!("{name} {below:?} step {i}"));
        }
        // Fold the steps in, then read them from the fold under a new tail.
        for i in 0..mushroomdb::FOLD_EVERY_K {
            db.submit_batch(vec![p_node(&format!("g{i}"), vec![])])
                .unwrap();
        }
        assert_tail_matches_live(&db, keys, &format!("{name} {below:?} folded"));
    }
}

#[test]
fn tail_edge_insert_matches_live() {
    run_tail_case("tail-edge-insert", &["a", "b", "c", "d"], || {
        vec![
            vec![knows("c", "a")],
            vec![p_node("d", vec![]), knows("d", "a"), knows("b", "d")],
        ]
    });
}

#[test]
fn tail_put_back_matches_live() {
    // An edge from below deleted, put back and deleted again.
    run_tail_case("tail-put-back", &["a", "b", "c"], || {
        vec![
            vec![unknows("a", "b")],
            vec![knows("a", "b")],
            vec![unknows("a", "b")],
            vec![knows("a", "b"), unknows("b", "c"), knows("b", "c")],
        ]
    });
}

#[test]
fn tail_self_loop_matches_live() {
    run_tail_case("tail-self-loop", &["a", "b", "e"], || {
        vec![
            vec![knows("a", "a")],
            vec![p_node("e", vec![]), knows("e", "e"), knows("e", "a")],
            vec![unknows("a", "a")],
            vec![BatchOp::DeleteNode { key: "e".into() }],
        ]
    });
}

#[test]
fn tail_rule_derived_edges_match_live_including_the_derived_flag() {
    let same_tags = |db: &SharedDb| {
        db.write()
            .create_rule(RuleDef {
                name: "same_tags".into(),
                src_label: "P".into(),
                dst_label: "P".into(),
                predicate: Predicate::Overlap {
                    field: "tags".into(),
                    min: 1.0,
                },
                edge_type: "SAME".into(),
                weight_prop: None,
                max_edges: None,
                approximate: false,
                via_label: None,
                via_edge: None,
                via_dir: None,
                namespace: None,
            })
            .unwrap();
    };
    let steps = || {
        vec![
            // Derive a–b.
            vec![
                set("a", "tags", tags(&["t"])),
                set("b", "tags", tags(&["t"])),
            ],
            // Extend to c and a new node.
            vec![
                set("c", "tags", tags(&["t", "u"])),
                p_node("h", vec![("tags", tags(&["u"]))]),
            ],
            // Retract a's, and delete a derived endpoint.
            vec![set("a", "tags", tags(&["v"]))],
            vec![BatchOp::DeleteNode { key: "h".into() }],
        ]
    };
    run_tail_case_with("tail-derived", &["a", "b", "c", "h"], same_tags, steps);
    // Non-vacuous: the rule did derive edges the check saw.
    let db = tail_store("tail-derived-count", Below::NoSnapshot, same_tags);
    db.submit_batch(steps().remove(0)).unwrap();
    let derived = db
        .reader()
        .node_edges("a")
        .unwrap()
        .into_iter()
        .filter(|e| e.derived)
        .count();
    assert!(derived > 0, "the rule derived nothing: vacuous");
}

#[test]
fn tail_rename_and_reinsert_match_live() {
    run_tail_case("tail-rename", &["a", "a2", "b", "c", "c2"], || {
        vec![
            vec![BatchOp::RenameNode {
                old_key: "a".into(),
                new_key: "a2".into(),
            }],
            // The old key comes back as a new node.
            vec![p_node("a", vec![("x", Value::Int(9))]), knows("a", "c")],
            // Rename and back in one batch.
            vec![
                BatchOp::RenameNode {
                    old_key: "c".into(),
                    new_key: "c2".into(),
                },
                BatchOp::RenameNode {
                    old_key: "c2".into(),
                    new_key: "c".into(),
                },
            ],
            vec![BatchOp::DeleteNode { key: "a2".into() }],
        ]
    });
}

#[test]
fn tail_delete_and_recreate_in_one_batch_matches_live() {
    run_tail_case("tail-delete-recreate", &["a", "b", "c"], || {
        vec![
            vec![
                BatchOp::DeleteNode { key: "b".into() },
                p_node("b", vec![("x", Value::Int(7))]),
                knows("b", "a"),
            ],
            vec![BatchOp::DeleteNode { key: "b".into() }, p_node("b", vec![])],
        ]
    });
}

/// A transaction over a committed tail deletes a node and re-creates its key
/// with a new edge: the read inside equals the live read after the commit.
#[test]
fn tx_delete_and_recreate_over_a_tail_matches_live_after_commit() {
    for below in BELOW {
        let db = tail_store("tx-delete-recreate", below, |_| {});
        db.submit_batch(vec![knows("c", "a"), set("b", "x", Value::Int(5))])
            .unwrap();
        let mut tx = db.begin_transaction();
        tx.execute("MATCH (n:P {id: 'b'}) DETACH DELETE n", &p())
            .unwrap();
        tx.execute(
            "CREATE (n:P {id: 'b', x: 3})-[:KNOWS]->(m:P {id: 'z'})",
            &p(),
        )
        .unwrap();
        let inside: Vec<Vec<String>> = ["a", "b", "c", "z"]
            .iter()
            .flat_map(|k| {
                TAIL_PROBES
                    .iter()
                    .map(|q| dump(&tx.execute(q, &key_param(k)).unwrap()))
                    .collect::<Vec<_>>()
            })
            .collect();
        tx.commit().unwrap();
        let live = db.read();
        let after: Vec<Vec<String>> = ["a", "b", "c", "z"]
            .iter()
            .flat_map(|k| {
                TAIL_PROBES
                    .iter()
                    .map(|q| dump(&live.query(q, &key_param(k)).unwrap()))
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(inside, after, "{below:?}");
    }
}

/// A role mask whose label leg is narrowed by `visible_where` resolves over
/// the tail exactly as on the live handle, and so do masked queries.
#[test]
fn tail_role_mask_with_visible_where_matches_live() {
    let role = |db: &SharedDb| {
        db.write()
            .apply_schema(&mushroomdb::schema::Schema {
                roles: vec![RoleDef {
                    name: "ones".into(),
                    keys: vec!["k".into()],
                    labels: vec!["P".into(), "Nope".into()],
                    visible_where: Some(PropPredicate {
                        field: "x".into(),
                        eq: Some(Value::Int(1)),
                        in_: None,
                    }),
                    namespaces: None,
                    write: None,
                }],
                ..Default::default()
            })
            .unwrap();
    };
    let steps = || {
        vec![
            vec![set("b", "x", Value::Int(1))],
            vec![set("a", "x", Value::Int(2))],
            vec![BatchOp::RemoveProp {
                key: "c".into(),
                field: "x".into(),
            }],
            vec![p_node("k", vec![("x", Value::Int(5))])],
            vec![p_node("n", vec![("x", Value::Int(1))])],
            vec![BatchOp::RenameNode {
                old_key: "n".into(),
                new_key: "n2".into(),
            }],
            vec![BatchOp::DeleteNode { key: "b".into() }],
            vec![
                BatchOp::DeleteNode { key: "k".into() },
                p_node("k", vec![("x", Value::Int(1))]),
            ],
        ]
    };
    let probe = "MATCH (n) RETURN n.id AS k, n.x AS x";
    for below in BELOW {
        let db = tail_store("tail-role", below, role);
        let mut saw_rows = 0;
        for (i, step) in steps().into_iter().enumerate() {
            db.submit_batch(step).unwrap();
            let reader = db.reader();
            assert!(!reader.deltas.is_empty());
            let live = db.read();
            let reader_mask = reader.mask_for_role("ones").unwrap();
            let live_mask = live.mask_for_role("ones").unwrap();
            let got = dump(&reader.query_masked(probe, &p(), &reader_mask).unwrap());
            let want = dump(&live.query_masked(probe, &p(), &live_mask).unwrap());
            assert_eq!(got, want, "{below:?} step {i}");
            saw_rows = saw_rows.max(got.len());
        }
        // Fold the steps in, then resolve the mask from the fold under a new
        // tail.
        for i in 0..mushroomdb::FOLD_EVERY_K {
            db.submit_batch(vec![p_node(&format!("g{i}"), vec![])])
                .unwrap();
        }
        let reader = db.reader();
        assert!(!reader.deltas.is_empty());
        let live = db.read();
        let reader_mask = reader.mask_for_role("ones").unwrap();
        let live_mask = live.mask_for_role("ones").unwrap();
        assert_eq!(
            dump(&reader.query_masked(probe, &p(), &reader_mask).unwrap()),
            dump(&live.query_masked(probe, &p(), &live_mask).unwrap()),
            "{below:?} folded"
        );
        assert!(saw_rows > 0, "{below:?}: the mask hid everything: vacuous");
    }
}
