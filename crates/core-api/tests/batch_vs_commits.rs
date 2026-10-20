//! One batch against the same ops as separate commits.
//!
//! A batch is checked against a preview of the store and then rewritten to
//! dense ids before anything is applied, so it has two places to disagree
//! with the plain write path that a single op does not: what the preview sees
//! of a key an earlier op touched, and which node a reused key resolves to.
//! Each case here commits a short op list once as one batch and once op by
//! op, and asks for the same store either way — and the same store again
//! after a reopen. The harness came out of the review of ledger row 79.
use core_storage::fs::RealFs;
use mushroomdb::{Direction, GraphDb, OnConflict, Predicate, RuleDef, Value};

type Db = GraphDb<RealFs>;

#[derive(Clone, Debug)]
enum Op {
    Rename(&'static str, &'static str),
    Insert(&'static str, &'static str, Vec<(&'static str, Value)>),
    Upsert(&'static str, &'static str, Vec<(&'static str, Value)>, bool),
    DelNode(&'static str),
    Set(&'static str, &'static str, Value),
    Rm(&'static str, &'static str),
    InsEdge(&'static str, &'static str, &'static str),
    DelEdge(&'static str, &'static str, &'static str),
    EdgeUpsert(&'static str, &'static str, &'static str),
    DelRule(&'static str),
}

const KEYS: [&str; 5] = ["a", "b", "c", "d", "e"];
const ETYPES: [&str; 3] = ["KNOWS", "SAME", "VIA"];

fn own(p: &[(&'static str, Value)]) -> Vec<(String, Value)> {
    p.iter().map(|(f, v)| (f.to_string(), v.clone())).collect()
}

fn queue(b: &mut mushroomdb::BatchBuilder<'_, RealFs>, op: &Op) {
    match op {
        Op::Rename(o, n) => b.rename_node(o, n),
        Op::Insert(l, k, p) => b.insert_node(l, k, own(p)),
        Op::Upsert(l, k, p, replace) => b.insert_node_on_conflict(
            l,
            k,
            own(p),
            if *replace {
                OnConflict::Replace
            } else {
                OnConflict::Skip
            },
        ),
        Op::DelNode(k) => b.delete_node(k),
        Op::Set(k, f, v) => b.set_prop(k, f, v.clone()),
        Op::Rm(k, f) => b.remove_prop(k, f),
        Op::InsEdge(t, s, d) => b.insert_edge(t, s, d),
        Op::DelEdge(t, s, d) => b.delete_edge(t, s, d),
        Op::EdgeUpsert(t, s, d) => b.insert_edge_upsert(t, s, d, "Q"),
        Op::DelRule(n) => b.delete_rule(n),
    };
}

fn rule(name: &str, edge: &str, via: bool) -> RuleDef {
    RuleDef {
        name: name.into(),
        src_label: "P".into(),
        dst_label: "P".into(),
        predicate: Predicate::FieldEqual { field: "g".into() },
        edge_type: edge.into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: via.then(|| "P".to_string()),
        via_edge: via.then(|| "KNOWS".to_string()),
        via_dir: None,
        namespace: None,
    }
}

fn seed(dir: &std::path::Path) -> Db {
    let _ = std::fs::remove_dir_all(dir);
    let mut db = GraphDb::open(dir).unwrap();
    let mut b = db.batch();
    b.insert_node(
        "P",
        "a",
        vec![
            ("g".into(), Value::Int(1)),
            ("name".into(), Value::Str("x".into())),
            ("id".into(), Value::Str("a".into())),
        ],
    )
    .insert_node("P", "c", vec![("g".into(), Value::Int(1))])
    .insert_node(
        "P",
        "d",
        vec![
            ("g".into(), Value::Int(2)),
            ("id".into(), Value::Str("b".into())),
        ],
    )
    .insert_edge("KNOWS", "a", "d")
    .insert_edge("KNOWS", "d", "c");
    b.commit().unwrap();
    db.create_rule(rule("same", "SAME", false)).unwrap();
    db.create_rule(rule("via", "VIA", true)).unwrap();
    db
}

fn dump(db: &Db) -> String {
    let mut out = String::new();
    for k in KEYS {
        if let Some(info) = db.node_info(k) {
            out.push_str(&format!("{k}:{}{:?}", info.label, info.props));
            for t in ETYPES {
                let n = db.neighbors(k, t, Direction::Out).unwrap_or_default();
                if !n.is_empty() {
                    out.push_str(&format!(" {t}->{n:?}"));
                }
            }
            out.push_str("; ");
        }
    }
    let mut rules: Vec<String> = db.rules().into_iter().map(|r| r.name).collect();
    rules.sort();
    out.push_str(&format!("rules{rules:?}"));
    out
}

/// The store every case starts from is [`seed`]: `a`, `c` and `d` are `P`
/// nodes, `a` and `c` share `g`, `a`'s stored `id` is its key and `d`'s is
/// `b`; `a → d → c` are hand-written `KNOWS` edges; rule `same` links equal
/// `g` as `SAME`, and rule `via` does the same across one `KNOWS` hop as `VIA`.
///
/// `None` when the batch and the separate commits agree; otherwise how they
/// differ.
fn run_case(ops: &[Op], base: &std::path::Path) -> (Option<String>, bool) {
    let mut one = seed(&base.join("one"));
    let mut b = one.batch();
    for op in ops {
        queue(&mut b, op);
    }
    let batched = b.commit_outcome();

    let mut seq = seed(&base.join("seq"));
    let mut seq_err = None;
    let mut rows = 0usize;
    let (mut skipped, mut replaced) = (0, 0);
    for (i, op) in ops.iter().enumerate() {
        let mut b = seq.batch();
        queue(&mut b, op);
        match b.commit_outcome() {
            Ok(o) => {
                rows += o.row_errors.len();
                skipped += o.skipped;
                replaced += o.replaced;
            }
            Err(e) => {
                seq_err = Some(format!("op{i}: {e:?}"));
                break;
            }
        }
    }
    let verdict = match (&batched, &seq_err) {
        (Err(_), Some(_)) => return (None, false),
        (Err(e), None) => format!("batch REFUSED ({e:?}) but separate commits all succeed"),
        (Ok(_), Some(e)) => format!("batch ACCEPTED but separate commits fail at {e}"),
        (Ok(o), None) => {
            let (x, y) = (dump(&one), dump(&seq));
            let counts_b = (o.row_errors.len(), o.skipped, o.replaced);
            let counts_s = (rows, skipped, replaced);
            // Reopen: what the WAL replays must be what the handle held.
            let commits = one.commit_seq();
            drop(one);
            let reopened = dump(&GraphDb::open(&base.join("one")).unwrap());
            // And the last commit read back through `open_at`.
            // `commit_seq` can undercount the frames on disk, so walk up from
            // it to the last commit `open_at` accepts.
            let mut last = commits.saturating_sub(1);
            while GraphDb::open_at(&base.join("one"), last + 1).is_ok() {
                last += 1;
            }
            let as_of = GraphDb::open_at(&base.join("one"), last)
                .map(|db| dump(&db))
                .unwrap_or_else(|e| format!("open_at failed: {e:?}"));
            if x != as_of {
                format!("OPEN_AT differs\n   live  {x}\n   as_of {as_of}")
            } else if x != reopened {
                format!("REOPEN differs\n   live   {x}\n   reopen {reopened}")
            } else if x != y {
                format!("STATE differs\n   batch {x}\n   seq   {y}")
            } else if counts_b != counts_s {
                format!("COUNTS differ batch {counts_b:?} seq {counts_s:?}")
            } else {
                return (None, true);
            }
        }
    };
    (Some(format!("{ops:?}\n   {verdict}")), batched.is_ok())
}

/// Returns how many cases the batch accepted (the rest were refused alike).
fn assert_all_agree(name: &str, cases: Vec<Vec<Op>>) -> usize {
    let base = std::env::temp_dir().join(format!(
        "mdb-batch-vs-commits-{name}-{}",
        std::process::id()
    ));
    let mut differing = Vec::new();
    let mut accepted = 0;
    for ops in &cases {
        let (diff, ok) = run_case(ops, &base);
        accepted += usize::from(ok);
        differing.extend(diff);
    }
    let _ = std::fs::remove_dir_all(&base);
    assert!(differing.is_empty(), "\n{}", differing.join("\n"));
    accepted
}

fn s(x: &str) -> Value {
    Value::Str(x.into())
}

/// Binding: a rule deleted after a rename takes its edges with it under the
/// node's new key, so the batch can write one of them by hand. The preview
/// used to record the rule's edges as gone under the store's key and then
/// read the renamed endpoint through to the store, where the edge still was:
/// the insert was judged a duplicate and dropped.
#[test]
fn a_rule_deleted_after_a_rename_frees_its_edges_under_the_new_key() {
    assert_all_agree(
        "delete-rule",
        vec![
            vec![
                Op::Rename("a", "b"),
                Op::DelRule("same"),
                Op::InsEdge("SAME", "b", "c"),
            ],
            vec![
                Op::Rename("a", "b"),
                Op::DelRule("same"),
                Op::InsEdge("SAME", "c", "b"),
            ],
            vec![
                Op::Rename("a", "b"),
                Op::DelRule("same"),
                Op::DelEdge("SAME", "b", "c"),
            ],
            vec![
                Op::DelRule("same"),
                Op::Rename("a", "b"),
                Op::InsEdge("SAME", "b", "c"),
            ],
            vec![
                Op::Rename("a", "b"),
                Op::Rename("b", "e"),
                Op::DelRule("same"),
                Op::InsEdge("SAME", "e", "c"),
            ],
        ],
    );
}

/// Binding: a key a batch frees — by renaming the node away or deleting it —
/// and then uses again names the new node for the rest of the batch. The
/// rewrite to dense ids used to ask the store first, and the store still had
/// the key on the old node: a property set on the new node was lost, an edge
/// from it landed on the old one, and a rename's own `id` rewrite went to the
/// wrong node.
#[test]
fn a_key_reused_inside_a_batch_names_the_new_node() {
    assert_all_agree(
        "key-reuse",
        vec![
            vec![Op::Rename("d", "b"), Op::Rename("a", "d")],
            vec![Op::DelNode("c"), Op::Rename("a", "c")],
            vec![Op::Rename("c", "b"), Op::EdgeUpsert("KNOWS", "c", "a")],
            vec![
                Op::Rename("a", "b"),
                Op::Insert("P", "a", vec![("id", s("c"))]),
                Op::Set("a", "g", Value::Int(1)),
            ],
            vec![
                Op::Rename("a", "b"),
                Op::Insert("Q", "a", vec![]),
                Op::InsEdge("KNOWS", "a", "c"),
            ],
            vec![
                Op::DelNode("a"),
                Op::Insert("P", "a", vec![]),
                Op::Set("a", "name", s("y")),
            ],
            vec![Op::DelNode("a"), Op::EdgeUpsert("KNOWS", "a", "c")],
            vec![
                Op::DelNode("a"),
                Op::Insert("Q", "a", vec![]),
                Op::InsEdge("KNOWS", "a", "c"),
            ],
            vec![
                Op::DelNode("a"),
                Op::Insert("P", "a", vec![]),
                Op::Insert("P", "e", vec![]),
                Op::Set("e", "name", s("y")),
                Op::Set("a", "name", s("x")),
            ],
        ],
    );
}

/// Guards: sequences that already agreed, kept so they keep agreeing.
#[test]
fn renames_beside_other_ops_agree_with_separate_commits() {
    assert_all_agree(
        "guards",
        vec![
            vec![Op::Rename("a", "b"), Op::Rename("b", "a")],
            vec![
                Op::Rename("a", "b"),
                Op::Rename("b", "a"),
                Op::Rm("a", "name"),
                Op::DelEdge("KNOWS", "a", "d"),
            ],
            vec![
                Op::Rename("a", "b"),
                Op::Rename("b", "e"),
                Op::Rename("e", "a"),
                Op::Set("a", "g", Value::Int(2)),
            ],
            vec![Op::Rename("a", "b"), Op::DelNode("b")],
            vec![
                Op::Rename("a", "b"),
                Op::DelNode("b"),
                Op::Insert("P", "b", vec![("g", Value::Int(1))]),
            ],
            vec![
                Op::Rename("a", "b"),
                Op::Set("b", "g", Value::Int(2)),
                Op::Rm("b", "g"),
                Op::Set("b", "g", Value::Int(1)),
            ],
            vec![
                Op::Rename("a", "b"),
                Op::Upsert("P", "b", vec![("g", Value::Int(2))], true),
            ],
            vec![
                Op::Rename("a", "b"),
                Op::Upsert("Q", "b", vec![("g", Value::Int(2))], true),
            ],
            vec![Op::Rename("a", "b"), Op::DelEdge("VIA", "b", "c")],
            vec![Op::Rename("d", "b"), Op::DelEdge("VIA", "a", "c")],
            vec![
                Op::Rename("a", "b"),
                Op::DelEdge("KNOWS", "b", "d"),
                Op::InsEdge("KNOWS", "b", "d"),
            ],
        ],
    );
}

/// A small deterministic generator, so a failing case can be rerun.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[(self.next() % items.len() as u64) as usize]
    }
}

/// One op that is mostly valid against `present`, the keys the batch has
/// left standing so far (which it updates), so most batches are accepted and
/// the overlay is exercised rather than the refusal path.
fn random_op(r: &mut Rng, present: &mut Vec<&'static str>) -> Op {
    let absent: Vec<&'static str> = KEYS
        .iter()
        .copied()
        .filter(|k| !present.contains(k))
        .collect();
    let label = r.pick(&["P", "Q"]);
    let props = |r: &mut Rng| {
        let mut p = Vec::new();
        if r.next().is_multiple_of(2) {
            p.push(("g", Value::Int((r.next() % 2 + 1) as i64)));
        }
        if r.next().is_multiple_of(3) {
            p.push(("name", Value::Str("n".into())));
        }
        p
    };
    for _ in 0..8 {
        match r.next() % 10 {
            0 | 1 if !absent.is_empty() => {
                let k = r.pick(&absent);
                present.push(k);
                return Op::Insert(label, k, props(r));
            }
            2 | 3 if !present.is_empty() || !absent.is_empty() => {
                // Replace/Skip over a taken key, or a fresh one.
                let k = r.pick(&KEYS);
                if !present.contains(&k) {
                    present.push(k);
                }
                return Op::Upsert(label, k, props(r), r.next().is_multiple_of(2));
            }
            4 | 5 if !present.is_empty() => {
                let value = if r.next().is_multiple_of(3) {
                    Value::Str("y".into())
                } else {
                    Value::Int((r.next() % 2 + 1) as i64)
                };
                return Op::Set(r.pick(present), r.pick(&["g", "name"]), value);
            }
            6 if !present.is_empty() => {
                return Op::Rm(r.pick(present), r.pick(&["g", "name"]));
            }
            7 if !present.is_empty() => {
                let k = r.pick(present);
                present.retain(|p| *p != k);
                return Op::DelNode(k);
            }
            8 if !present.is_empty() && !absent.is_empty() => {
                let (old, new) = (r.pick(present), r.pick(&absent));
                present.retain(|p| *p != old);
                present.push(new);
                return Op::Rename(old, new);
            }
            9 if present.len() >= 2 => {
                return Op::InsEdge("KNOWS", r.pick(present), r.pick(present));
            }
            _ => {}
        }
    }
    Op::EdgeUpsert("KNOWS", r.pick(&KEYS), r.pick(&KEYS))
}

/// Binding (ledger row 118): the overlay a batch keeps for validation is
/// looked up by key, not walked. Random batches over five keys, so the same
/// key is inserted, set, deleted, renamed, upserted (Replace and Skip) and
/// inserted again inside one batch, beside the `same` and `via` rules, must
/// leave the store separate commits leave: live, after a reopen, and read back
/// at the last commit through `open_at`.
#[test]
fn random_batches_with_key_reuse_agree_with_separate_commits() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let cases: Vec<Vec<Op>> = (0..300)
        .map(|_| {
            let len = 2 + (rng.next() % 8) as usize;
            let mut present = vec!["a", "c", "d"];
            (0..len)
                .map(|_| random_op(&mut rng, &mut present))
                .collect()
        })
        .collect();
    let accepted = assert_all_agree("random", cases);
    assert!(
        accepted >= 100,
        "only {accepted} of 300 random batches were accepted; the generator no longer reaches the overlay"
    );
}

/// The shape row 118 names: insert, set, delete, insert of one key.
#[test]
fn insert_set_delete_insert_of_one_key_agrees() {
    let accepted = assert_all_agree(
        "isdi",
        vec![
            vec![
                Op::Insert("P", "b", vec![("g", Value::Int(1))]),
                Op::Set("b", "name", s("x")),
                Op::DelNode("b"),
                Op::Insert("P", "b", vec![("g", Value::Int(2))]),
            ],
            vec![
                Op::Insert("P", "b", vec![("g", Value::Int(1))]),
                Op::Set("b", "g", Value::Int(2)),
                Op::Rm("b", "g"),
                Op::DelNode("b"),
                Op::Insert("Q", "b", vec![]),
                Op::Upsert("P", "b", vec![("g", Value::Int(1))], true),
            ],
            vec![
                Op::Upsert("P", "a", vec![("g", Value::Int(2))], true),
                Op::Set("a", "name", s("z")),
                Op::DelNode("a"),
                Op::Upsert("P", "a", vec![], false),
                Op::Rename("a", "e"),
                Op::Insert("P", "a", vec![("g", Value::Int(1))]),
            ],
        ],
    );
    assert_eq!(accepted, 3);
}
