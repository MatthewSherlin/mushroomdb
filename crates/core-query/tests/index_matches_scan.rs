//! The property index answers exactly as a scan does (0.10 TY-8).
//!
//! A randomized differential test: stored values of every indexable type and
//! some unindexable ones, nodes with the property missing, and sets, removals
//! and deletes interleaved. After each step, every comparison operator and
//! BETWEEN — with random bounds of every type — is answered twice: by the
//! index ([`UnifiedIndex::lookup`] for `=`, [`UnifiedIndex::lookup_range`] for
//! the rest), and by a scan applying [`eval_cmp`], the function Cypher's
//! `WHERE` uses, to every node. The two must be the same set. Every so often
//! the index is also rebuilt from the columns, as an open does, and must
//! answer the same.
//!
//! `<>` is not here: no index answers it, so the planner always scans.

use core_query::{eval_cmp, CmpOp};
use core_storage::range_index::UnifiedIndex;
use core_storage::v8::seam::ColumnsView;
use core_storage::{ColumnStore, IdMap, Interner, Value};
use std::collections::BTreeMap;

/// xorshift64*: deterministic, no dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len())]
    }
}

fn pool() -> Vec<Value> {
    let dt = |us: i64, off: i32| Value::DateTime {
        micros_utc: us,
        offset_seconds: off,
    };
    let two53 = 1i64 << 53;
    let two60 = 1i64 << 60;
    let mut v = vec![
        Value::Int(i64::MIN),
        Value::Int(-3),
        Value::Int(-1),
        Value::Int(0),
        Value::Int(1),
        Value::Int(2),
        Value::Int(3),
        Value::Int(two53),
        Value::Int(two53 + 1),
        Value::Int(two60 - 1),
        Value::Int(two60),
        Value::Int(two60 + 1),
        Value::Int(i64::MAX),
        Value::Float(-0.0),
        Value::Float(0.0),
        Value::Float(1.0),
        Value::Float(2.0),
        Value::Float(2.5),
        Value::Float(-1.5),
        Value::Float(-3.0),
        Value::Float(f64::NAN),
        Value::Float(-f64::NAN),
        Value::Float(f64::INFINITY),
        Value::Float(f64::NEG_INFINITY),
        Value::Float(two53 as f64),
        Value::Float(two60 as f64),
        Value::Float(i64::MAX as f64),
        Value::Float(1e300),
        Value::Str(String::new()),
        Value::Str("a".into()),
        Value::Str("ab".into()),
        Value::Str("b".into()),
        Value::Str("é".into()),
        Value::Bool(false),
        Value::Bool(true),
        Value::Date(-1),
        Value::Date(0),
        Value::Date(5),
        Value::LocalDateTime(-10),
        Value::LocalDateTime(0),
        Value::LocalDateTime(10),
        dt(10, 0),
        dt(10, 3600),
        dt(10, -3600),
        dt(20, 0),
        dt(-5, 60),
        Value::Binary(vec![]),
        Value::Binary(vec![0]),
        Value::Binary(vec![1, 2]),
        Value::Duration {
            months: 1,
            days: 0,
            nanos: 0,
        },
        Value::List(vec![Value::Int(1)]),
    ];
    // Weight the numbers: most of the bugs live there.
    v.extend([Value::Int(2), Value::Float(2.0), Value::Float(-0.0)]);
    v
}

#[derive(Debug, Clone, Copy)]
enum Query {
    Eq,
    Lt,
    Le,
    Gt,
    Ge,
    Between(bool, bool),
}

fn scan(model: &BTreeMap<u32, Value>, q: Query, a: &Value, b: &Value) -> Vec<u32> {
    let ok = |v: &Value| match q {
        Query::Eq => eval_cmp(&CmpOp::Eq, v, a),
        Query::Lt => eval_cmp(&CmpOp::Lt, v, a),
        Query::Le => eval_cmp(&CmpOp::Le, v, a),
        Query::Gt => eval_cmp(&CmpOp::Gt, v, a),
        Query::Ge => eval_cmp(&CmpOp::Ge, v, a),
        Query::Between(li, hi) => {
            eval_cmp(if li { &CmpOp::Ge } else { &CmpOp::Gt }, v, a)
                && eval_cmp(if hi { &CmpOp::Le } else { &CmpOp::Lt }, v, b)
        }
    };
    model
        .iter()
        .filter(|(_, v)| ok(v))
        .map(|(id, _)| *id)
        .collect()
}

/// `None` when the index does not answer the query (the planner scans).
fn indexed(ix: &UnifiedIndex, q: Query, a: &Value, b: &Value) -> Option<Vec<u32>> {
    let mut ids: Vec<u32> = match q {
        Query::Eq if !UnifiedIndex::answers_eq(a) => return None,
        Query::Eq => ix.lookup("S", "v", a),
        Query::Lt => ix.lookup_range("S", "v", None, Some((a, false))).collect(),
        Query::Le => ix.lookup_range("S", "v", None, Some((a, true))).collect(),
        Query::Gt => ix.lookup_range("S", "v", Some((a, false)), None).collect(),
        Query::Ge => ix.lookup_range("S", "v", Some((a, true)), None).collect(),
        Query::Between(li, hi) => ix
            .lookup_range("S", "v", Some((a, li)), Some((b, hi)))
            .collect(),
    };
    let n = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(
        ids.len(),
        n,
        "the index returned an id twice for {q:?} {a:?} {b:?}"
    );
    Some(ids)
}

struct Store {
    ids: IdMap,
    syms: Interner,
    labels: Vec<u32>,
    cols: ColumnStore,
}

impl Store {
    fn rebuilt(&self) -> UnifiedIndex {
        let mut ix = UnifiedIndex::new();
        ix.enable_range("r", "S", "v");
        ix.rebuild_all(
            &self.ids,
            &self.labels,
            &self.syms,
            ColumnsView::owned(&self.cols),
        );
        ix
    }
}

#[test]
fn the_index_answers_every_comparison_exactly_as_the_scan() {
    let values = pool();
    let queries = [
        Query::Eq,
        Query::Lt,
        Query::Le,
        Query::Gt,
        Query::Ge,
        Query::Between(true, true),
        Query::Between(true, false),
        Query::Between(false, true),
        Query::Between(false, false),
    ];
    let mut checked = 0usize;
    let mut answered = 0usize;
    for seed in 1..=120u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut store = Store {
            ids: IdMap::new(),
            syms: Interner::new(),
            labels: Vec::new(),
            cols: ColumnStore::new(),
        };
        let s = store.syms.intern("S");
        let other = store.syms.intern("Other");
        let mut ix = UnifiedIndex::new();
        ix.enable_range("r", "S", "v");
        // Live `S` nodes that hold the property, as the scan sees them.
        let mut model: BTreeMap<u32, Value> = BTreeMap::new();
        let mut live: Vec<u32> = Vec::new();
        for step in 0..160 {
            match rng.below(10) {
                // Insert, sometimes without the property, sometimes as `Other`.
                0..=3 => {
                    let id = store.ids.try_insert(&format!("n{seed}-{step}")).unwrap();
                    let is_s = rng.below(5) != 0;
                    store.labels.push(if is_s { s } else { other });
                    if rng.below(6) != 0 {
                        let v = rng.pick(&values).clone();
                        store.cols.set(id, "v", v.clone());
                        if is_s {
                            ix.set("S", "v", id, &v);
                            model.insert(id, v);
                        }
                    }
                    if is_s {
                        live.push(id);
                    }
                }
                // Update.
                4..=6 if !live.is_empty() => {
                    let id = *rng.pick(&live);
                    let v = rng.pick(&values).clone();
                    store.cols.set(id, "v", v.clone());
                    ix.set("S", "v", id, &v);
                    model.insert(id, v);
                }
                // Remove the property.
                7 if !live.is_empty() => {
                    let id = *rng.pick(&live);
                    store.cols.remove(id, "v");
                    ix.remove_node("S", "v", id);
                    model.remove(&id);
                }
                // Delete the node.
                8 if !live.is_empty() => {
                    let at = rng.below(live.len());
                    let id = live.swap_remove(at);
                    store.cols.remove_all(id);
                    store.labels[id as usize] = u32::MAX;
                    ix.remove_node_all(id);
                    model.remove(&id);
                }
                _ => {}
            }
            let rebuilt = (step % 40 == 39).then(|| store.rebuilt());
            for _ in 0..12 {
                let q = *rng.pick(&queries);
                let a = rng.pick(&values).clone();
                let b = rng.pick(&values).clone();
                let want = scan(&model, q, &a, &b);
                checked += 1;
                if let Some(got) = indexed(&ix, q, &a, &b) {
                    answered += 1;
                    assert_eq!(got, want, "seed {seed} step {step}: {q:?} {a:?} {b:?}");
                }
                if let Some(rebuilt) = &rebuilt {
                    if let Some(got) = indexed(rebuilt, q, &a, &b) {
                        assert_eq!(
                            got, want,
                            "rebuilt, seed {seed} step {step}: {q:?} {a:?} {b:?}"
                        );
                    }
                }
            }
        }
    }
    // Enough to matter: hundreds of thousands of queries, nearly all answered
    // by the index.
    assert!(checked >= 200_000, "{checked} queries");
    assert!(
        answered * 100 >= checked * 90,
        "{answered} of {checked} answered by the index"
    );
    eprintln!("index vs scan: {checked} queries, {answered} answered by the index");
}
