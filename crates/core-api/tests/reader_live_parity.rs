//! Ledger row 162: a reader sees the same rule-derived edges, and the same
//! edge weights, as the live handle at the same commit.
//!
//! A seeded random walk over weighted rules that share node pairs: rules are
//! created and deleted, source writes assert and retract derived edges, and
//! snapshots, reopens and folds land between them. After every step the
//! reader — a fresh `GraphDb::reader()`, and `SharedDb::reader()` through the
//! published snapshot — must answer as the live handle does: the edge set
//! with each edge's properties, every node's edges with their `derived` flag,
//! and the counts.

use mushroomdb::{Predicate, RealFs, RuleDef, SharedDb, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;

type GraphDb = mushroomdb::GraphDb<RealFs>;
type Rows = Vec<Vec<Option<Value>>>;

const NODES: usize = 6;
const STEPS: usize = 60;
const SEEDS: u64 = 4;
const RULES: [(&str, &str, &str); 3] = [
    ("ra", "region", "wa"),
    ("rb", "team", "wb"),
    ("rc", "tier", "wc"),
];
const FIELDS: [&str; 3] = ["region", "team", "tier"];
const EDGES: &str = "MATCH (a)-[r]->(b) RETURN key(a) AS a, type(r) AS t, key(b) AS b, \
                     properties(r) AS p ORDER BY a, t, b";

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "graphdb-row162-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// A small deterministic generator (xorshift64*), so a failing seed replays.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) % n
    }
}

fn weighted(name: &str, field: &str, weight: &str) -> RuleDef {
    RuleDef {
        name: name.into(),
        src_label: "P".into(),
        dst_label: "P".into(),
        predicate: Predicate::FieldEqual {
            field: field.into(),
        },
        edge_type: "SAME".into(),
        weight_prop: Some(weight.into()),
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
        namespace: None,
    }
}

/// One step of the walk, chosen from the seed.
#[derive(Debug, Clone)]
enum Step {
    /// Set a node's field to one of two values: asserts or retracts edges.
    Set(usize, usize, u64),
    CreateRule(usize),
    DeleteRule(usize),
    Snapshot,
    Reopen,
    /// Enough unrelated commits to cross a fold boundary.
    Fold,
    /// Several source writes in one commit (a transaction on `SharedDb`,
    /// one statement on `GraphDb`): a retraction and a re-derivation can
    /// land together.
    Together(Vec<(usize, usize, u64)>),
}

fn plan(seed: u64) -> Vec<Step> {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    (0..STEPS)
        .map(|_| match rng.below(20) {
            0..=10 => Step::Set(
                rng.below(NODES as u64) as usize,
                rng.below(FIELDS.len() as u64) as usize,
                rng.below(2),
            ),
            11..=13 => Step::CreateRule(rng.below(RULES.len() as u64) as usize),
            14..=15 => Step::DeleteRule(rng.below(RULES.len() as u64) as usize),
            16 => Step::Snapshot,
            17 => Step::Reopen,
            18 => Step::Fold,
            _ => Step::Together(
                (0..2 + rng.below(3))
                    .map(|_| {
                        (
                            rng.below(NODES as u64) as usize,
                            rng.below(FIELDS.len() as u64) as usize,
                            rng.below(2),
                        )
                    })
                    .collect(),
            ),
        })
        .collect()
}

fn p() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

fn rows(rs: &mushroomdb::ResultSet) -> Rows {
    (0..rs.len()).map(|i| rs.row(i).to_vec()).collect()
}

fn seed_nodes(db: &mut GraphDb) {
    for n in 0..NODES {
        db.query_write(
            &format!(
                "CREATE (:P {{id: 'p{n}', region: 'r0', team: 't{}', tier: 'x0'}})",
                n % 2
            ),
            &p(),
        )
        .unwrap();
    }
    db.query_write("CREATE (:F {id: 'filler', n: 0})", &p())
        .unwrap();
}

fn set(n: usize, f: usize, v: u64) -> String {
    format!("MATCH (n:P {{id: 'p{n}'}}) SET n.{} = 'v{v}'", FIELDS[f])
}

/// Several writes as one statement: `MATCH (n0 …), (n1 …) SET n0.f = …, n1.g = …`.
fn together(sets: &[(usize, usize, u64)]) -> String {
    let patterns: Vec<String> = (0..sets.len())
        .map(|i| format!("(n{i}:P {{id: 'p{}'}})", sets[i].0))
        .collect();
    let writes: Vec<String> = sets
        .iter()
        .enumerate()
        .map(|(i, &(_, f, v))| format!("n{i}.{} = 'v{v}'", FIELDS[f]))
        .collect();
    format!("MATCH {} SET {}", patterns.join(", "), writes.join(", "))
}

fn apply(db: &mut GraphDb, step: &Step, rules: &mut [bool; 3]) {
    match *step {
        Step::Set(n, f, v) => {
            db.query_write(&set(n, f, v), &p()).unwrap();
        }
        Step::Together(ref sets) => {
            db.query_write(&together(sets), &p()).unwrap();
        }
        Step::CreateRule(r) if !rules[r] => {
            let (name, field, weight) = RULES[r];
            db.create_rule(weighted(name, field, weight)).unwrap();
            rules[r] = true;
        }
        Step::DeleteRule(r) if rules[r] => {
            db.delete_rule(RULES[r].0).unwrap();
            rules[r] = false;
        }
        Step::Snapshot => db.snapshot().unwrap(),
        Step::Fold => {
            for i in 0..mushroomdb::FOLD_EVERY_K as i64 {
                db.query_write(&format!("MATCH (f:F {{id: 'filler'}}) SET f.n = {i}"), &p())
                    .unwrap();
            }
        }
        _ => {}
    }
}

/// Everything the comparison reads, as one value.
#[derive(Debug, PartialEq)]
struct Answer {
    edges: Rows,
    node_edges: Vec<Vec<mushroomdb::EdgeInfo>>,
    counts: (usize, usize, u64),
}

fn live(db: &GraphDb) -> Answer {
    Answer {
        edges: rows(&db.query(EDGES, &p()).unwrap()),
        node_edges: (0..NODES)
            .map(|n| sorted(db.node_edges(&format!("p{n}")).unwrap()))
            .collect(),
        counts: {
            let stats = db.stats();
            (stats.nodes_live, stats.nodes_tombstoned, stats.edges)
        },
    }
}

fn read(r: &mushroomdb::ReaderSnapshot) -> Answer {
    Answer {
        edges: rows(&r.query(EDGES, &p()).unwrap()),
        node_edges: (0..NODES)
            .map(|n| sorted(r.node_edges(&format!("p{n}")).unwrap()))
            .collect(),
        counts: r.counts(),
    }
}

fn sorted(mut v: Vec<mushroomdb::EdgeInfo>) -> Vec<mushroomdb::EdgeInfo> {
    v.sort_by(|a, b| {
        (&a.edge_type, &a.src_key, &a.dst_key).cmp(&(&b.edge_type, &b.src_key, &b.dst_key))
    });
    v
}

/// Mismatch counts over a run: (checks, edge-set or weight rows, node_edges, counts).
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct Tally {
    checks: usize,
    edges: usize,
    node_edges: usize,
    counts: usize,
    /// Edge rows compared, and how many carried a rule weight: the walk is
    /// not vacuous.
    rows: usize,
    weighted: usize,
    derived_flags: usize,
}

impl Tally {
    fn compare(&mut self, want: &Answer, got: &Answer, label: &str) {
        self.checks += 1;
        self.rows += want.edges.len();
        self.weighted += want
            .edges
            .iter()
            .filter(|r| matches!(&r[3], Some(Value::Map(m)) if !m.is_empty()))
            .count();
        self.derived_flags += want
            .node_edges
            .iter()
            .flatten()
            .filter(|e| e.derived)
            .count();
        if want.edges != got.edges {
            self.edges += 1;
            eprintln!(
                "{label}: edges\n  live   {:?}\n  reader {:?}",
                want.edges, got.edges
            );
        }
        if want.node_edges != got.node_edges {
            self.node_edges += 1;
            eprintln!("{label}: node_edges differ");
        }
        if want.counts != got.counts {
            self.counts += 1;
            eprintln!("{label}: counts {:?} vs {:?}", want.counts, got.counts);
        }
    }
}

fn run_graphdb(seed: u64) -> Tally {
    let dir = tmp(&format!("g{seed}"));
    let mut db = GraphDb::open(&dir).unwrap();
    seed_nodes(&mut db);
    let mut rules = [false; 3];
    let mut tally = Tally::default();
    for (i, step) in plan(seed).iter().enumerate() {
        if let Step::Reopen = step {
            drop(db);
            db = GraphDb::open(&dir).unwrap();
        } else {
            apply(&mut db, step, &mut rules);
        }
        tally.compare(
            &live(&db),
            &read(&db.reader()),
            &format!("GraphDb seed {seed} step {i} {step:?}"),
        );
    }
    tally
}

fn run_shared(seed: u64) -> Tally {
    let dir = tmp(&format!("s{seed}"));
    let mut shared = SharedDb::open(&dir).unwrap();
    seed_nodes(&mut shared.write());
    let mut rules = [false; 3];
    let mut tally = Tally::default();
    for (i, step) in plan(seed).iter().enumerate() {
        if let Step::Reopen = step {
            drop(shared);
            shared = SharedDb::open(&dir).unwrap();
        } else if let Step::Together(sets) = step {
            let mut tx = shared.begin_transaction();
            for &(n, f, v) in sets {
                tx.execute(&set(n, f, v), &p()).unwrap();
            }
            tx.commit().unwrap();
        } else {
            apply(&mut shared.write(), step, &mut rules);
        }
        let want = live(&shared.read());
        tally.compare(
            &want,
            &read(&shared.reader()),
            &format!("SharedDb seed {seed} step {i} {step:?}"),
        );
    }
    tally
}

#[test]
fn row_162_a_reader_sees_the_live_handles_derived_edges_and_weights() {
    let mut graph = Tally::default();
    let mut shared = Tally::default();
    for seed in 1..=SEEDS {
        // The two walks share nothing, so they run side by side.
        let (g, s) = std::thread::scope(|scope| {
            let g = scope.spawn(|| run_graphdb(seed));
            let s = run_shared(seed);
            (g.join().expect("GraphDb walk"), s)
        });
        for (total, one) in [(&mut graph, g), (&mut shared, s)] {
            total.checks += one.checks;
            total.edges += one.edges;
            total.node_edges += one.node_edges;
            total.counts += one.counts;
            total.rows += one.rows;
            total.weighted += one.weighted;
            total.derived_flags += one.derived_flags;
        }
    }
    eprintln!("GraphDb::reader  {graph:?}");
    eprintln!("SharedDb::reader {shared:?}");
    let clean = |t: &Tally| t.edges == 0 && t.node_edges == 0 && t.counts == 0;
    for t in [&graph, &shared] {
        assert!(
            t.weighted > 100 && t.derived_flags > 100,
            "vacuous walk: {t:?}"
        );
    }
    assert!(clean(&graph), "GraphDb::reader differs: {graph:?}");
    assert!(clean(&shared), "SharedDb::reader differs: {shared:?}");
}
