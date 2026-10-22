//! Ledger row 126: the write path shares the column store, the property
//! index and the id map with the reader's fold, and since TX-7 a commit
//! copies only the pieces of them it writes, not the structures whole.
//!
//! What that must not change: a snapshot captured at any commit keeps
//! answering as that commit left the store, whatever is written after it
//! (in the same piece or not), and the store answers the same live, after a
//! reopen, through `open_at`, and with snapshots taken along the way or not.
use core_storage::fs::RealFs;
use mushroomdb::{GraphDb, ReaderSnapshot, ResultSet, Value};
use std::collections::BTreeMap;

type Db = GraphDb<RealFs>;

/// A small deterministic generator, so a failing run can be repeated.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const NAMES: usize = 40;

fn name_value(r: &mut Rng) -> Value {
    // Mostly strings from a small set (shared postings), some unique, and
    // now and then an Int, which turns the column mixed.
    match r.below(10) {
        0 => Value::Int(r.below(5) as i64),
        1 => Value::Str(format!("unique {}", r.next())),
        _ => Value::Str(format!("name {}", r.below(NAMES))),
    }
}

/// Everything the probes ask, as text: each key's label and properties,
/// its edge count, and indexed and scanned lookups.
fn probe(
    keys: &[String],
    info: impl Fn(&str) -> Option<String>,
    query: impl Fn(&str, &BTreeMap<String, Value>) -> String,
) -> String {
    let mut out = String::new();
    for k in keys {
        out.push_str(&format!("{k}={:?}\n", info(k)));
    }
    for i in [0, 3, 7, 19] {
        let params = BTreeMap::from([("v".to_string(), Value::Str(format!("name {i}")))]);
        out.push_str(&query(
            "MATCH (n:P {name: $v}) RETURN n.key AS k ORDER BY k",
            &params,
        ));
    }
    let none = BTreeMap::new();
    out.push_str(&query(
        "MATCH (n:P) WHERE n.n = 3 RETURN n.key AS k ORDER BY k",
        &none,
    ));
    out.push_str(&query(
        "MATCH (n:P) WHERE n.n >= 2 AND n.n < 5 RETURN n.key AS k ORDER BY k",
        &none,
    ));
    out.push_str(&query(
        "MATCH (a:P)-[:KNOWS]->(b) RETURN a.key AS a, b.key AS b ORDER BY a, b",
        &none,
    ));
    out
}

fn rows(rs: mushroomdb::Result<ResultSet>) -> String {
    match rs {
        Ok(rs) => format!(
            "{:?}\n",
            (0..rs.len())
                .map(|i| rs.row(i).to_vec())
                .collect::<Vec<_>>()
        ),
        Err(e) => format!("error {e:?}\n"),
    }
}

fn probe_db(db: &Db, keys: &[String]) -> String {
    probe(
        keys,
        |k| {
            db.node_info(k).map(|i| {
                format!(
                    "{} {:?} edges={}",
                    i.label,
                    i.props,
                    db.node_edges(k).map_or(0, |e| e.len())
                )
            })
        },
        |q, p| rows(db.query(q, p)),
    )
}

fn probe_snap(snap: &ReaderSnapshot, keys: &[String]) -> String {
    probe(
        keys,
        |k| {
            snap.node_info(k).map(|i| {
                format!(
                    "{} {:?} edges={}",
                    i.label,
                    i.props,
                    snap.node_edges(k).map_or(0, |e| e.len())
                )
            })
        },
        |q, p| rows(snap.query(q, p)),
    )
}

/// Run the seeded history on a fresh store at `dir`, taking a store snapshot
/// every 170 ops when `snapshots`. Every 11 ops a reader snapshot is
/// captured with what it answered; at the end each must answer the same.
/// Returns the final live probe and the keys it covers.
fn run(dir: &std::path::Path, seed: u64, snapshots: bool) -> (String, Vec<String>) {
    let _ = std::fs::remove_dir_all(dir);
    let mut db = Db::open(dir).unwrap();
    db.enable_index("P", "name").unwrap();
    db.enable_index("P", "n").unwrap();
    let mut r = Rng(seed);
    let mut live: Vec<String> = Vec::new();
    let mut universe: Vec<String> = Vec::new();
    let mut held: Vec<(ReaderSnapshot, Vec<String>, String)> = Vec::new();
    let mut fresh = 0usize;
    for step in 0..900 {
        let pick = |r: &mut Rng, live: &Vec<String>| live[r.below(live.len())].clone();
        match r.below(12) {
            0..=2 => {
                let k = format!("k{fresh}");
                fresh += 1;
                let mut props = vec![
                    ("name".to_string(), name_value(&mut r)),
                    ("n".to_string(), Value::Int(r.below(8) as i64)),
                ];
                if r.below(3) == 0 {
                    props.push((
                        "tags".to_string(),
                        Value::List(vec![Value::Int(r.below(4) as i64)]),
                    ));
                }
                db.insert_node("P", &k, props).unwrap();
                live.push(k.clone());
                universe.push(k);
            }
            3..=5 if !live.is_empty() => {
                let k = pick(&mut r, &live);
                db.set_prop(&k, "name", name_value(&mut r)).unwrap();
            }
            6 | 7 if !live.is_empty() => {
                let k = pick(&mut r, &live);
                db.set_prop(&k, "n", Value::Int(r.below(8) as i64)).unwrap();
            }
            8 if !live.is_empty() => {
                let k = pick(&mut r, &live);
                db.remove_prop(&k, if r.below(2) == 0 { "name" } else { "n" })
                    .unwrap();
            }
            9 if live.len() > 5 => {
                let i = r.below(live.len());
                let k = live.swap_remove(i);
                db.delete_node(&k).unwrap();
            }
            10 if !live.is_empty() => {
                let i = r.below(live.len());
                let new = format!("r{step}");
                db.rename_node(&live[i], &new).unwrap();
                live[i] = new.clone();
                universe.push(new);
            }
            11 if live.len() > 1 => {
                let (a, b) = (pick(&mut r, &live), pick(&mut r, &live));
                db.insert_edge("KNOWS", &a, &b).unwrap();
            }
            _ => {}
        }
        if snapshots && step % 170 == 169 {
            db.snapshot().unwrap();
        }
        if step % 11 == 0 {
            let snap = db.reader();
            let answer = probe_snap(&snap, &universe);
            assert_eq!(
                answer,
                probe_db(&db, &universe),
                "step {step}: a fresh snapshot answers as the live store"
            );
            held.push((snap, universe.clone(), answer));
        }
    }
    for (i, (snap, keys, answer)) in held.iter().enumerate() {
        assert_eq!(
            &probe_snap(snap, keys),
            answer,
            "snapshot {i} (captured at step {}) answers differently after later writes",
            i * 11
        );
    }
    drop(held);
    let final_probe = probe_db(&db, &universe);
    let commits = db.commit_seq();
    drop(db);
    let reopened = Db::open(dir).unwrap();
    assert_eq!(
        probe_db(&reopened, &universe),
        final_probe,
        "reopen differs"
    );
    drop(reopened);
    let mut last = commits.saturating_sub(1);
    while Db::open_at(dir, last + 1).is_ok() {
        last += 1;
    }
    let as_of = Db::open_at(dir, last).unwrap();
    assert_eq!(probe_db(&as_of, &universe), final_probe, "open_at differs");
    (final_probe, universe)
}

#[test]
fn held_snapshots_keep_their_answers_and_every_path_agrees() {
    let base = std::env::temp_dir().join(format!("mdb-fold-cow-{}", std::process::id()));
    for seed in [0x9E37_79B9_7F4A_7C15_u64, 0xD1B5_4A32_D192_ED03] {
        let (with, keys) = run(&base.join("with"), seed, true);
        let (without, keys2) = run(&base.join("without"), seed, false);
        assert_eq!(keys, keys2);
        assert_eq!(with, without, "snapshotting changed the answers");
    }
    let _ = std::fs::remove_dir_all(&base);
}
