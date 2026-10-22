//! Randomized differential: a random store, exported in each format and
//! imported into a fresh store, is the same store.
//!
//! Each seed builds nodes of random labels with random properties (strings with
//! quotes, commas, newlines, backslashes, backticks and non-ASCII; integers at
//! the extremes; floats from `5e-324` to `f64::MAX`; lists and maps nested;
//! names such as `key`, `label`, `type`, `$props` and `a:b`), random manual
//! edges, and sometimes a rule. "Same" is every node with every property, every
//! edge with its provenance, and every rule, read through the engine's own
//! export readers. A seed that fails prints itself, so it can be replayed.
use cli::export::ExportFormat;
use cli::import::{run_import_inputs, ImportInputs, ImportOptions};
use cli::run_export;
use mushroomdb::{default_max_edges, GraphDb, Predicate, RealFs, RuleDef, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

type Db = GraphDb<RealFs>;

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, one_in: usize) -> bool {
        self.below(one_in) == 0
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

const CHARS: &[char] = &[
    'a', 'Z', '0', ' ', ',', '"', '\'', '\\', '\n', '\r', '\t', ';', ':', '`', '$', 'é', 'ß', '日',
    '😀', '{', '}', '[', ']', '%', '|', '\u{7f}', '\u{1}', '.', '-', '/',
];
const LABELS: &[&str] = &["Person", "Org", "We ird", "A`b", "日本", "L,\"q\""];
const EDGE_TYPES: &[&str] = &["KNOWS", "W ORKS", "T`x", "a:b"];
const NAMES: &[&str] = &[
    "name", "age", "city", "key", "label", "type", "$props", "a:b", "x y", "日本", "`q`", "a.b",
    "score", "ns2",
];

fn text(rng: &mut Rng, max: usize) -> String {
    let len = rng.below(max + 1);
    (0..len).map(|_| *rng.pick(CHARS)).collect()
}

/// `min_int_ok`: whether `i64::MIN` may appear (Cypher has no literal for it).
fn value(rng: &mut Rng, depth: usize, min_int_ok: bool) -> Value {
    match rng.below(if depth >= 2 { 4 } else { 6 }) {
        0 => Value::Str(text(rng, 8)),
        1 => Value::Int(match rng.below(6) {
            0 => i64::MAX,
            1 if min_int_ok => i64::MIN,
            2 => 0,
            3 => -1,
            _ => rng.next() as i64 >> rng.below(60),
        }),
        2 => Value::Float(match rng.below(8) {
            0 => 0.1,
            1 => 1.0,
            2 => 1e300,
            3 => 1e-300,
            4 => f64::MAX,
            5 => 5e-324,
            6 => -2.5,
            _ => (rng.next() >> 11) as f64 / 3.0,
        }),
        3 => Value::Bool(rng.chance(2)),
        4 => Value::List(
            (0..rng.below(4))
                .map(|_| value(rng, depth + 1, min_int_ok))
                .collect(),
        ),
        _ => Value::Map(
            (0..rng.below(4))
                .map(|_| (text(rng, 4) + "k", value(rng, depth + 1, min_int_ok)))
                .collect(),
        ),
    }
}

fn same_city_rule() -> RuleDef {
    let predicate = Predicate::FieldEqual {
        field: "city".into(),
    };
    RuleDef {
        name: "same_city".into(),
        src_label: "Person".into(),
        dst_label: "Person".into(),
        max_edges: Some(default_max_edges(&predicate)),
        predicate,
        edge_type: "SAME_CITY".into(),
        weight_prop: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
        namespace: None,
    }
}

/// What a format can carry: Cypher cannot write `i64::MIN`, a node's `id`
/// different from its key, or a rule.
#[derive(Clone, Copy)]
struct Limits {
    cypher: bool,
}

fn build(dir: &Path, seed: u64, limits: Limits) -> Db {
    let mut rng = Rng::new(seed);
    let mut db = GraphDb::open(dir).unwrap();
    let with_rule = !limits.cypher && rng.chance(3);
    if with_rule {
        db.create_rule(same_city_rule()).unwrap();
    }
    // `id` on every node, on none, or (where a format can say it) on some.
    let id_mode = rng.below(if limits.cypher { 2 } else { 3 });
    let count = 1 + rng.below(40);
    let keys: Vec<String> = (0..count)
        .map(|i| format!("{i}{}", text(&mut rng, 5)))
        .collect();
    let labels: Vec<&str> = (0..count).map(|_| *rng.pick(LABELS)).collect();
    db.write_batch(|b| {
        for (key, label) in keys.iter().zip(&labels) {
            let mut props: BTreeMap<String, Value> = BTreeMap::new();
            for _ in 0..rng.below(6) {
                props.insert(
                    rng.pick(NAMES).to_string(),
                    value(&mut rng, 0, !limits.cypher),
                );
            }
            if *label == "Person" {
                props.insert("city".into(), Value::Str(format!("c{}", rng.below(3))));
            }
            let id_here = match id_mode {
                0 => false,
                1 => true,
                _ => rng.chance(2),
            };
            if id_here {
                let id = if !limits.cypher && rng.chance(4) {
                    Value::Str(text(&mut rng, 3))
                } else {
                    Value::Str(key.clone())
                };
                props.insert("id".into(), id);
            }
            b.insert_node(label, key, props.into_iter().collect());
        }
        for _ in 0..rng.below(count * 2) {
            let (a, c) = (rng.below(count), rng.below(count));
            let edge_type = *rng.pick(EDGE_TYPES);
            b.insert_edge(edge_type, &keys[a], &keys[c]);
        }
    })
    .unwrap();

    // The new value types are refused by the engine, whole, with nothing written.
    if rng.chance(2) {
        for typed in [
            Value::Date(19_737),
            Value::List(vec![Value::Int(1), Value::Date(0)]),
            Value::Binary(vec![1, 2]),
            Value::Duration {
                months: 1,
                days: 2,
                nanos: 3,
            },
        ] {
            let r = db.write_batch(|b| {
                b.insert_node("Typed", "typed-node", vec![("v".into(), typed.clone())]);
            });
            assert!(r.is_err(), "seed {seed}: {typed:?} must be refused");
            assert!(!db.has_node("typed-node"), "seed {seed}: nothing written");
        }
    }
    db
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("mushroomdb-random-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn import(db_dir: &Path, inputs: &ImportInputs, batch_size: usize) {
    let opts = ImportOptions {
        batch_size,
        ..ImportOptions::default()
    };
    run_import_inputs(db_dir, inputs, &opts, &mut |_| {}).unwrap();
}

fn has_line_break_key(v: &Value) -> bool {
    match v {
        Value::List(xs) => xs.iter().any(has_line_break_key),
        Value::Map(m) => m
            .iter()
            .any(|(k, v)| k.contains(['\n', '\r']) || has_line_break_key(v)),
        _ => false,
    }
}

fn assert_same(seed: u64, format: &str, a: &Path, b: &Path, adds_id: bool) {
    let (a, b) = (
        GraphDb::<RealFs>::open(a).unwrap(),
        GraphDb::<RealFs>::open(b).unwrap(),
    );
    let mut expected = a.all_nodes_for_export();
    if adds_id {
        for node in &mut expected {
            // Cypher skips (and counts) a property whose name, or a map key in
            // it, has a line break.
            node.props
                .retain(|name, v| !name.contains(['\n', '\r']) && !has_line_break_key(v));
            node.props
                .entry("id".into())
                .or_insert_with(|| Value::Str(node.key.clone()));
        }
    }
    let actual = b.all_nodes_for_export();
    assert_eq!(
        expected.len(),
        actual.len(),
        "seed {seed} {format}: node count"
    );
    for (x, y) in expected.iter().zip(&actual) {
        assert_eq!(x, y, "seed {seed} {format}: node {:?}", x.key);
    }
    assert_eq!(
        a.all_edges_for_export(),
        b.all_edges_for_export(),
        "seed {seed} {format}: edges"
    );
    let rules = |db: &Db| {
        let mut r = db.rules();
        r.sort_by(|x, y| x.name.cmp(&y.name));
        r
    };
    assert_eq!(rules(&a), rules(&b), "seed {seed} {format}: rules");
}

const SEEDS: u64 = 15;

#[test]
fn random_stores_round_trip_through_every_file_format() {
    for seed in 1..=SEEDS {
        let dir = tmp(&format!("files-{seed}"));
        drop(build(&dir.join("src"), seed, Limits { cypher: false }));
        let src = dir.join("src");
        // A small batch size so a store spans several batches.
        let batch = 1 + (seed as usize % 7);

        run_export(&src, &dir.join("jsonl"), &ExportFormat::Jsonl).unwrap();
        import(
            &dir.join("d-jsonl"),
            &ImportInputs {
                nodes_jsonl: Some(dir.join("jsonl/nodes.jsonl")),
                edges_jsonl: Some(dir.join("jsonl/edges.jsonl")),
                rules_jsonl: Some(dir.join("jsonl/rules.jsonl")),
                ..ImportInputs::default()
            },
            batch,
        );
        assert_same(seed, "jsonl", &src, &dir.join("d-jsonl"), false);

        run_export(&src, &dir.join("single"), &ExportFormat::JsonlSingle).unwrap();
        import(
            &dir.join("d-single"),
            &ImportInputs {
                stream_jsonl: Some(dir.join("single/graph.jsonl")),
                ..ImportInputs::default()
            },
            batch,
        );
        assert_same(
            seed,
            "single-file jsonl",
            &src,
            &dir.join("d-single"),
            false,
        );

        run_export(&src, &dir.join("csv"), &ExportFormat::Csv).unwrap();
        import(
            &dir.join("d-csv"),
            &ImportInputs {
                nodes_csv: Some(dir.join("csv/nodes.csv")),
                rels_csv: Some(dir.join("csv/relationships.csv")),
                rules_jsonl: dir
                    .join("csv/rules.jsonl")
                    .is_file()
                    .then(|| dir.join("csv/rules.jsonl")),
                ..ImportInputs::default()
            },
            batch,
        );
        assert_same(seed, "csv", &src, &dir.join("d-csv"), false);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn random_stores_round_trip_through_cypher() {
    for seed in 1..=SEEDS {
        let dir = tmp(&format!("cypher-{seed}"));
        drop(build(&dir.join("src"), seed, Limits { cypher: true }));
        let src = dir.join("src");
        let summary = run_export(&src, &dir.join("out"), &ExportFormat::Cypher).unwrap();
        let mut db = GraphDb::<RealFs>::open(&dir.join("dst")).unwrap();
        let file = std::fs::read_to_string(dir.join("out/graph.cypher")).unwrap();
        for statement in file.lines() {
            db.query_write(statement, &BTreeMap::new())
                .unwrap_or_else(|e| {
                    panic!("seed {seed}: the engine must parse its own export: {e}\n{statement}")
                });
        }
        drop(db);
        // Cypher stores each key as `id`; the export said how many gain one.
        let src_db = GraphDb::<RealFs>::open(&src).unwrap();
        let without_id = src_db
            .all_nodes_for_export()
            .iter()
            .filter(|n| !n.props.contains_key("id"))
            .count();
        drop(src_db);
        assert_eq!(
            summary.contains("had no id property"),
            without_id > 0,
            "seed {seed}: {summary}"
        );
        if without_id > 0 {
            assert!(
                summary.contains(&format!("{without_id} nodes had no id property")),
                "seed {seed}: {summary}"
            );
        }
        assert_same(seed, "cypher", &src, &dir.join("dst"), true);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
