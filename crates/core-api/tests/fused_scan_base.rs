//! A filtered scan with LIMIT sees the nodes a snapshot base holds (ledger
//! row 139, QL-FS).
//!
//! `MATCH (n:L) WHERE n.f <op> <literal> RETURN … LIMIT k` (no ORDER BY) runs
//! the pull path's fused scan-and-filter: it resolves the property column once
//! and compares inline. Any label scan whose filter is that one comparison is
//! fused, so `MATCH (m:M), (n:L) WHERE n.f = …` and `MATCH (m:M) MATCH (n:L)
//! WHERE n.f = …` are too, and SKIP does not stop it. That column handle read
//! the layer and the overlay but never the archived base a snapshot leaves
//! behind, so every node whose value the snapshot held was dropped —
//! silently. Two nodes, `snapshot()`, a third: `WHERE n.val <> 3 … LIMIT 5`
//! answered only the third.
//!
//! Each test compares the fused shape with the same filter written so the
//! fusion does not fire (`… AND n.key IS NOT NULL`, always true; a compound
//! WHERE is not the single comparison the fusion matches), in order.
use mushroomdb::schema::Schema;
use mushroomdb::{GraphDb, NodeMask, RoleDef, SharedDb, Value, FOLD_EVERY_K, NS_DEFAULT, NS_PROP};
use std::collections::BTreeMap;
use std::path::PathBuf;

type Db = GraphDb<core_storage::fs::RealFs>;

fn tmp(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!("qlfs-{tag}-{}-{nanos}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn p() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

const OPS: [&str; 6] = ["=", "<>", "<", "<=", ">", ">="];

/// One literal per value type the fixtures store, a second integer, and
/// `null`.
const LITERALS: [&str; 8] = ["3", "2.5", "'m'", "true", "0", "[1, 2]", "{a: 1}", "null"];

/// The fused shape and its unfused twin, for one comparison, single-node.
fn pair(op: &str, lit: &str) -> (String, String) {
    (
        format!("MATCH (n:L) WHERE n.val {op} {lit} RETURN n.key AS k LIMIT 1000"),
        format!(
            "MATCH (n:L) WHERE n.val {op} {lit} AND n.key IS NOT NULL RETURN n.key AS k LIMIT 1000"
        ),
    )
}

/// Every fused form and its unfused twin, for one comparison: the
/// single-node MATCH, the same with SKIP, and the filtered node as the second
/// scan of a MATCH — comma-joined and as a second MATCH — where `=` is fused
/// too (0.9.x lifted an equality into a lookup on the first scan only).
fn forms(op: &str, lit: &str) -> [(String, String); 4] {
    let cmp = format!("n.val {op} {lit}");
    let twin = format!("{cmp} AND n.key IS NOT NULL");
    let both = |head: &str, tail: &str| {
        (
            format!("{head} WHERE {cmp} RETURN n.key AS k {tail}"),
            format!("{head} WHERE {twin} RETURN n.key AS k {tail}"),
        )
    };
    [
        both("MATCH (n:L)", "LIMIT 1000"),
        both("MATCH (n:L)", "SKIP 1 LIMIT 1000"),
        both("MATCH (m:M), (n:L)", "LIMIT 1000"),
        both("MATCH (m:M) MATCH (n:L)", "LIMIT 1000"),
    ]
}

fn keys(rs: &mushroomdb::ResultSet) -> Vec<String> {
    (0..rs.len())
        .map(|i| match rs.get(i, "k") {
            Some(Value::Str(s)) => s.clone(),
            other => panic!("key column: {other:?}"),
        })
        .collect()
}

/// Every operator against every literal, in every form: the fused answer
/// equals the unfused one. Returns how many comparisons kept at least one
/// row, so a caller can tell the fixture was not vacuous.
fn assert_fused_matches(
    run: &dyn Fn(&str) -> mushroomdb::Result<mushroomdb::ResultSet>,
    when: &str,
) -> usize {
    let mut nonempty = 0;
    for op in OPS {
        for lit in LITERALS {
            for (fused, unfused) in forms(op, lit) {
                let want =
                    keys(&run(&unfused).unwrap_or_else(|e| panic!("{when}: {unfused}: {e}")));
                let got = keys(&run(&fused).unwrap_or_else(|e| panic!("{when}: {fused}: {e}")));
                assert_eq!(got, want, "{when}: {fused}");
                if !want.is_empty() {
                    nonempty += 1;
                }
            }
        }
    }
    nonempty
}

fn int(i: i64) -> Value {
    Value::Int(i)
}

/// A value of every type, cycling with `i`: Int, Float, Str, Bool, List, Map.
/// `typed` keeps one type per store so the base column is a typed one;
/// otherwise the field mixes types and the base column is `Mixed`.
fn value(i: u64, typed: Option<u64>) -> Value {
    match typed.unwrap_or(i % 6) {
        0 => Value::Int((i % 7) as i64),
        1 => Value::Float((i % 7) as f64 / 2.0),
        2 => Value::Str(["a", "m", "z", "mm"][(i % 4) as usize].into()),
        3 => Value::Bool(i.is_multiple_of(2)),
        4 => Value::List(vec![int((i % 3) as i64), int(2)]),
        _ => Value::Map(BTreeMap::from([("a".to_string(), int((i % 3) as i64))])),
    }
}

/// The six node states, with `val` of the given type shape:
/// - `a`, `f`: in the base, untouched;
/// - `b`: in the base, overwritten after the snapshot;
/// - `c`: in the base, deleted after the snapshot;
/// - `d`: in the base, `val` removed after the snapshot;
/// - `e`: written after the snapshot only.
///
/// `g` in namespace `other` and `h` with label `M` are never in any answer a
/// scope or a label leaves out.
fn six_states(db: &mut Db, typed: Option<u64>) {
    for (i, k) in ["a", "b", "c", "d", "f"].iter().enumerate() {
        db.insert_node("L", k, vec![("val".into(), value(i as u64 * 5 + 1, typed))])
            .unwrap();
    }
    db.insert_node(
        "L",
        "g",
        vec![
            ("val".into(), value(2, typed)),
            (NS_PROP.into(), Value::Str("other".into())),
        ],
    )
    .unwrap();
    db.insert_node("M", "h", vec![("val".into(), value(3, typed))])
        .unwrap();
    db.snapshot().unwrap();
    db.set_prop("b", "val", value(4, typed)).unwrap();
    db.delete_node("c").unwrap();
    db.remove_prop("d", "val").unwrap();
    db.insert_node("L", "e", vec![("val".into(), value(9, typed))])
        .unwrap();
}

/// The type shapes: each single type, then all of them mixed in one field.
fn shapes() -> Vec<Option<u64>> {
    (0..6).map(Some).chain([None]).collect()
}

#[test]
fn the_reported_shape_sees_the_snapshot_base() {
    let dir = tmp("repro");
    let mut db: Db = GraphDb::open(&dir).unwrap();
    db.insert_node("L", "a", vec![("val".into(), int(1))])
        .unwrap();
    db.insert_node("L", "b", vec![("val".into(), int(2))])
        .unwrap();
    db.snapshot().unwrap();
    db.insert_node("L", "c", vec![("val".into(), int(4))])
        .unwrap();
    let rs = db
        .query(
            "MATCH (n:L) WHERE n.val <> 3 RETURN n.key AS k LIMIT 5",
            &p(),
        )
        .unwrap();
    assert_eq!(keys(&rs), vec!["a", "b", "c"]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `=` off the first scanned node is fused, so it missed the base too (seen
/// on the released 0.9.2 binary), and so did a fused scan under SKIP; the
/// single-node `=` is the control.
#[test]
fn an_equality_on_a_later_scan_sees_the_snapshot_base() {
    let dir = tmp("eq-later");
    let mut db: Db = GraphDb::open(&dir).unwrap();
    db.insert_node("L", "a", vec![("val".into(), int(3))])
        .unwrap();
    db.insert_node("L", "b", vec![("val".into(), int(3))])
        .unwrap();
    db.insert_node("M", "m", vec![]).unwrap();
    db.snapshot().unwrap();
    db.insert_node("L", "c", vec![("val".into(), int(3))])
        .unwrap();
    for q in [
        "MATCH (m:M), (n:L) WHERE n.val = 3 RETURN n.key AS k LIMIT 10",
        "MATCH (m:M) MATCH (n:L) WHERE n.val = 3 RETURN n.key AS k LIMIT 10",
        "MATCH (n:L) WHERE n.val = 3 RETURN n.key AS k LIMIT 10",
    ] {
        assert_eq!(
            keys(&db.query(q, &p()).unwrap()),
            vec!["a", "b", "c"],
            "{q}"
        );
    }
    let rs = db
        .query(
            "MATCH (n:L) WHERE n.val <> 9 RETURN n.key AS k SKIP 1 LIMIT 10",
            &p(),
        )
        .unwrap();
    assert_eq!(keys(&rs), vec!["b", "c"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn every_state_and_type_through_the_handle_and_a_reopen() {
    for typed in shapes() {
        let dir = tmp("handle");
        let mut db: Db = GraphDb::open(&dir).unwrap();
        six_states(&mut db, typed);
        let n = assert_fused_matches(&|q| db.query(q, &p()), &format!("live {typed:?}"));
        assert!(n > 0, "{typed:?}: every comparison was empty");
        drop(db);
        let db: Db = GraphDb::open(&dir).unwrap();
        assert_fused_matches(&|q| db.query(q, &p()), &format!("reopened {typed:?}"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn a_base_from_a_second_snapshot_under_later_writes() {
    let dir = tmp("twice");
    let mut db: Db = GraphDb::open(&dir).unwrap();
    six_states(&mut db, Some(0));
    db.snapshot().unwrap();
    db.set_prop("a", "val", int(6)).unwrap();
    db.remove_prop("f", "val").unwrap();
    db.insert_node("L", "i", vec![("val".into(), int(0))])
        .unwrap();
    assert_fused_matches(&|q| db.query(q, &p()), "after a second snapshot");
    let rs = db
        .query(
            "MATCH (n:L) WHERE n.val >= 0 RETURN n.key AS k LIMIT 100",
            &p(),
        )
        .unwrap();
    assert_eq!(keys(&rs), vec!["a", "b", "g", "e", "i"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn namespace_and_role_scopes_over_the_base() {
    let dir = tmp("scope");
    let mut db: Db = GraphDb::open(&dir).unwrap();
    six_states(&mut db, Some(0));
    db.apply_schema(&Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![],
        roles: vec![RoleDef {
            name: "some".into(),
            keys: vec!["a".into(), "e".into(), "g".into()],
            labels: vec![],
            visible_where: None,
            namespaces: None,
            write: None,
        }],
    })
    .unwrap();
    let masks: Vec<(&str, NodeMask)> = vec![
        ("namespace default", db.mask_for_namespace(NS_DEFAULT)),
        ("namespace other", db.mask_for_namespace("other")),
        ("role some", db.mask_for_role("some").unwrap()),
    ];
    for (name, mask) in &masks {
        assert_fused_matches(&|q| db.query_masked(q, &p(), mask), name);
    }
    let rs = db
        .query_masked(
            "MATCH (n:L) WHERE n.val >= 0 RETURN n.key AS k LIMIT 100",
            &p(),
            &masks[2].1,
        )
        .unwrap();
    assert_eq!(keys(&rs), vec!["a", "g", "e"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn open_at_over_the_base() {
    let dir = tmp("open-at");
    let mut db: Db = GraphDb::open(&dir).unwrap();
    six_states(&mut db, Some(0));
    // `six_states` commits 0..=6 before the snapshot and 7..=10 after it.
    let latest = 10;
    let (fused, _) = pair(">=", "0");
    assert_eq!(
        keys(&db.query(&fused, &p()).unwrap()),
        vec!["a", "b", "f", "g", "e"]
    );
    for commit in 7..=latest {
        let at: Db = GraphDb::open_at(&dir, commit).unwrap();
        assert_fused_matches(&|q| at.query(q, &p()), &format!("open_at {commit}"));
        assert_fused_matches(
            &|q| db.query_at(commit, q, &p()),
            &format!("query_at {commit}"),
        );
    }
    let at: Db = GraphDb::open_at(&dir, latest).unwrap();
    assert_eq!(
        keys(&at.query(&fused, &p()).unwrap()),
        keys(&db.query(&fused, &p()).unwrap())
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A reader is a fold plus, until `FOLD_EVERY_K` commits accumulate, an
/// unfolded delta tail laid over it. Both shapes, with the base under them.
#[test]
fn shared_readers_folded_and_unfolded_and_a_transaction() {
    for typed in [Some(0), Some(2), None] {
        let dir = tmp("shared");
        let shared = SharedDb::open(&dir).unwrap();
        six_states(&mut shared.write(), typed);
        let when = format!("unfolded reader {typed:?}");
        let reader = shared.reader();
        assert_fused_matches(&|q| reader.query(q, &p()), &when);
        let ns = reader.mask_for_namespace(NS_DEFAULT).unwrap();
        assert_fused_matches(
            &|q| reader.query_masked(q, &p(), &ns),
            &format!("{when} masked"),
        );

        for i in 0..FOLD_EVERY_K as u64 + 1 {
            shared
                .write()
                .insert_node("L", &format!("x{i}"), vec![("val".into(), value(i, typed))])
                .unwrap();
        }
        shared
            .write()
            .set_prop("a", "val", value(11, typed))
            .unwrap();
        let reader = shared.reader();
        assert_fused_matches(
            &|q| reader.query(q, &p()),
            &format!("folded reader {typed:?}"),
        );

        // A transaction reads its own writes over the base.
        let mut tx = shared.begin_transaction();
        tx.execute("MATCH (n:L {id: 'f'}) REMOVE n.val", &p())
            .unwrap();
        tx.execute("MATCH (n:L {id: 'e'}) SET n.val = 3", &p())
            .unwrap();
        tx.execute("CREATE (n:L {id: 'tx', val: 1})", &p()).unwrap();
        let mut run = |q: &str| tx.execute(q, &p());
        let mut nonempty = 0;
        for op in OPS {
            for lit in LITERALS {
                for (fused, unfused) in forms(op, lit) {
                    let want = keys(&run(&unfused).unwrap());
                    let got = keys(&run(&fused).unwrap());
                    assert_eq!(got, want, "transaction {typed:?}: {fused}");
                    nonempty += usize::from(!want.is_empty());
                }
            }
        }
        assert!(nonempty > 0);
        drop(tx);
        drop(shared);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// A tiny deterministic generator, so a failing sequence replays exactly.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Random interleavings of writes, overwrites, removals, deletes, snapshots
/// and reopens; after every step the fused answer equals the unfused one, in
/// every form, through the handle and through a reader taken then. Step 35 is
/// a burst of `FOLD_EVERY_K + 1` writes with no snapshot or reopen in it, so a
/// reader taken after it has folded inside the walk.
#[test]
fn randomized_fused_equals_unfused_over_snapshot_interleavings() {
    for seed in 1..=16u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let dir = tmp(&format!("rand-{seed}"));
        let mut shared = Some(SharedDb::open(&dir).unwrap());
        // Some seeds keep one type per store (typed base columns), the rest mix.
        let typed = if seed % 3 == 0 { None } else { Some(seed % 6) };
        for step in 0..70 {
            let db = shared.as_ref().unwrap();
            let burst = if step == 35 { FOLD_EVERY_K + 1 } else { 1 };
            let mut what = 0;
            for _ in 0..burst {
                let key = format!("k{}", rng.below(16));
                // A burst writes only: a snapshot or a reopen would restart the fold.
                what = rng.below(if burst > 1 { 80 } else { 100 });
                let v = value(rng.next(), typed);
                let mut w = db.write();
                let exists = w.has_node(&key);
                match what {
                    0..=34 if !exists => {
                        let label = if rng.below(5) == 0 { "M" } else { "L" };
                        w.insert_node(label, &key, vec![("val".into(), v)]).unwrap();
                    }
                    0..=54 if exists => w.set_prop(&key, "val", v).unwrap(),
                    55..=69 if exists => {
                        w.remove_prop(&key, "val").unwrap();
                    }
                    70..=79 if exists => {
                        w.delete_node(&key).unwrap();
                    }
                    80..=91 => w.snapshot().unwrap(),
                    _ => {}
                }
            }
            if what >= 92 {
                drop(shared.take());
                shared = Some(SharedDb::open(&dir).unwrap());
            }
            let db = shared.as_ref().unwrap();
            let when = format!("seed {seed} step {step}");
            assert_fused_matches(&|q| db.read().query(q, &p()), &when);
            let reader = db.reader();
            assert_fused_matches(&|q| reader.query(q, &p()), &format!("{when} reader"));
        }
        drop(shared);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
