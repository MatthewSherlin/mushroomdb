//! `mushroomdb export` to CSV, JSONL and Cypher, and `mushroomdb import` of
//! JSONL: an import of an export gives back the store.
//!
//! The differential is the whole state read through the engine's own export
//! readers (every node with every property, every edge with its provenance,
//! every rule), compared after: export, import into a fresh store, and, for the
//! file formats, a second export that must be byte-identical to the first.
use cli::export::ExportFormat;
use cli::import::{run_import_inputs, ImportInputs, ImportOptions};
use cli::run_export;
use mushroomdb::{
    default_max_edges, ExportEdge, GraphDb, NodeInfo, Predicate, RealFs, RuleDef, Value,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

type Db = GraphDb<RealFs>;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "mushroomdb-roundtrip-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
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

/// The same city across one `KNOWS` hop.
fn via_city_rule() -> RuleDef {
    let predicate = Predicate::FieldEqual {
        field: "city".into(),
    };
    RuleDef {
        name: "via_city".into(),
        max_edges: Some(default_max_edges(&predicate)),
        predicate,
        edge_type: "VIA_CITY".into(),
        via_label: Some("Person".into()),
        via_edge: Some("KNOWS".into()),
        ..same_city_rule()
    }
}

/// How a test store is shaped.
#[derive(Clone, Copy)]
struct Shape {
    /// Every node carries `id` equal to its key (what `import` and `ingest`
    /// write), or none does.
    ids_are_keys: bool,
    /// Rules are declared before the data (the incremental route).
    rules: bool,
    /// Properties that only some formats can carry: an empty string, mixed
    /// types in one property, names that collide with a line's own fields.
    exotic: bool,
}

fn props(pairs: Vec<(&str, Value)>) -> Vec<(String, Value)> {
    pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

fn build(dir: &Path, shape: Shape) -> Db {
    let mut db = GraphDb::open(dir).unwrap();
    if shape.rules {
        db.create_rule(same_city_rule()).unwrap();
        db.create_rule(via_city_rule()).unwrap();
    }
    db.write_batch(|b| {
        for i in 0..12i64 {
            let key = format!("p{i}");
            let mut p = props(vec![
                ("name", Value::Str(format!("person {i}"))),
                ("age", Value::Int(20 + i)),
                ("city", Value::Str(format!("city{}", i % 3))),
                ("active", Value::Bool(i % 2 == 0)),
                (
                    "tags",
                    Value::List(vec![Value::Str("a;b".into()), Value::Str("c".into())]),
                ),
                // A float that is a whole number must stay a float.
                ("score", Value::Float(i as f64)),
            ]);
            if i % 4 == 0 {
                p.push(("ratio".into(), Value::Float(0.1 + i as f64)));
            }
            if shape.ids_are_keys {
                p.push(("id".into(), Value::Str(key.clone())));
            }
            if shape.exotic {
                match i {
                    1 => p.push(("bio".into(), Value::Str(String::new()))),
                    2 => p.push((
                        "m".into(),
                        Value::Map(BTreeMap::from([
                            ("k".to_string(), Value::Int(1)),
                            (
                                "nested".to_string(),
                                Value::List(vec![Value::Float(1.0), Value::Bool(true)]),
                            ),
                        ])),
                    )),
                    3 => {
                        // The same property is an integer here and a float elsewhere.
                        p.retain(|(k, _)| k != "score");
                        p.push(("score".into(), Value::Int(3)));
                    }
                    4 => p.push((
                        "name2".into(),
                        Value::Str("Zoë \"Z\", line1\nline2;semi\\back 'q'\t".into()),
                    )),
                    5 => {
                        p.push(("key".into(), Value::Str("not the key".into())));
                        p.push(("label".into(), Value::Int(7)));
                        p.push(("$props".into(), Value::Bool(false)));
                        p.push(("a:b".into(), Value::Str("colon".into())));
                        p.push(("type".into(), Value::Str("node".into())));
                    }
                    6 => p.push(("small".into(), Value::Int(i64::MIN))),
                    7 => p.push(("empty_list".into(), Value::List(vec![]))),
                    _ => {}
                }
            }
            b.insert_node("Person", &key, p);
        }
        for i in 0..3 {
            let key = format!("o{i}");
            let mut p = props(vec![("name", Value::Str(format!("org {i}")))]);
            if shape.ids_are_keys {
                p.push(("id".into(), Value::Str(key.clone())));
            }
            b.insert_node("Org", &key, p);
        }
        let mut weird = props(vec![("x", Value::Int(1))]);
        if shape.ids_are_keys {
            weird.push(("id".into(), Value::Str("w 1".into())));
        }
        b.insert_node("We ird `label`", "w 1", weird);
        for i in 0..11 {
            b.insert_edge("KNOWS", &format!("p{i}"), &format!("p{}", i + 1));
        }
        for i in 0..12 {
            b.insert_edge("WORKS_AT", &format!("p{i}"), &format!("o{}", i % 3));
        }
        b.insert_edge("SEES", "o0", "w 1");
    })
    .unwrap();
    db
}

struct State {
    nodes: Vec<NodeInfo>,
    edges: Vec<ExportEdge>,
    rules: Vec<RuleDef>,
}

fn state(dir: &Path) -> State {
    let db = GraphDb::open(dir).unwrap();
    let mut rules = db.rules();
    rules.sort_by(|a, b| a.name.cmp(&b.name));
    State {
        nodes: db.all_nodes_for_export(),
        edges: db.all_edges_for_export(),
        rules,
    }
}

fn assert_same(a: &State, b: &State, what: &str) {
    assert_eq!(a.nodes.len(), b.nodes.len(), "{what}: node count");
    for (x, y) in a.nodes.iter().zip(&b.nodes) {
        assert_eq!(x, y, "{what}: node {}", x.key);
    }
    assert_eq!(a.edges, b.edges, "{what}: edges (manual and derived)");
    assert_eq!(a.rules, b.rules, "{what}: rules");
}

fn read_dir_bytes(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            (
                e.file_name().to_string_lossy().into_owned(),
                std::fs::read(e.path()).unwrap(),
            )
        })
        .collect()
}

fn export(db_dir: &Path, dest: &Path, format: ExportFormat) -> String {
    run_export(db_dir, dest, &format).unwrap()
}

fn jsonl_inputs(dir: &Path) -> ImportInputs {
    ImportInputs {
        nodes_jsonl: Some(dir.join("nodes.jsonl")),
        edges_jsonl: Some(dir.join("edges.jsonl")),
        rules_jsonl: Some(dir.join("rules.jsonl")),
        ..ImportInputs::default()
    }
}

fn csv_inputs(dir: &Path) -> ImportInputs {
    ImportInputs {
        nodes_csv: Some(dir.join("nodes.csv")),
        rels_csv: Some(dir.join("relationships.csv")),
        rules_jsonl: dir
            .join("rules.jsonl")
            .is_file()
            .then(|| dir.join("rules.jsonl")),
        ..ImportInputs::default()
    }
}

fn import(db_dir: &Path, inputs: &ImportInputs, batch_size: usize) {
    let opts = ImportOptions {
        batch_size,
        ..ImportOptions::default()
    };
    run_import_inputs(db_dir, inputs, &opts, &mut |_| {}).unwrap();
}

fn shapes() -> Vec<(&'static str, Shape)> {
    let mut out = Vec::new();
    for (ids_name, ids_are_keys) in [("ids", true), ("noids", false)] {
        for (rules_name, rules) in [("rules", true), ("norules", false)] {
            out.push((
                match (ids_name, rules_name) {
                    ("ids", "rules") => "ids-rules",
                    ("ids", _) => "ids-norules",
                    (_, "rules") => "noids-rules",
                    _ => "noids-norules",
                },
                Shape {
                    ids_are_keys,
                    rules,
                    exotic: true,
                },
            ));
        }
    }
    out
}

#[test]
fn round_trip_jsonl_reproduces_the_store_with_and_without_rules() {
    for (name, shape) in shapes() {
        let dir = tmp(&format!("jsonl-{name}"));
        drop(build(&dir.join("src"), shape));
        let out1 = dir.join("out1");
        let summary = export(&dir.join("src"), &out1, ExportFormat::Jsonl);
        assert!(summary.contains("16 nodes"), "{summary}");

        // One record per line, a node's key first-class.
        let nodes_text = std::fs::read_to_string(out1.join("nodes.jsonl")).unwrap();
        assert_eq!(
            nodes_text.lines().count(),
            16,
            "{name}: one record per node"
        );
        for line in nodes_text.lines() {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(
                v.get("key").is_some_and(serde_json::Value::is_string),
                "{line}"
            );
            assert!(
                v.get("label").is_some_and(serde_json::Value::is_string),
                "{line}"
            );
        }

        let source = state(&dir.join("src"));
        assert_eq!(
            source.edges.iter().any(|e| e.derived),
            shape.rules,
            "{name}: the rules derive edges, so the comparison covers them"
        );
        // A small batch so several batches are committed.
        import(&dir.join("dst"), &jsonl_inputs(&out1), 5);
        assert_same(&state(&dir.join("src")), &state(&dir.join("dst")), name);

        // And the export of the import is the export.
        let out2 = dir.join("out2");
        export(&dir.join("dst"), &out2, ExportFormat::Jsonl);
        assert_eq!(
            read_dir_bytes(&out1),
            read_dir_bytes(&out2),
            "{name}: re-export"
        );
    }
}

#[test]
fn round_trip_csv_reproduces_the_store_with_and_without_rules() {
    for (name, shape) in shapes() {
        let dir = tmp(&format!("csv-{name}"));
        drop(build(&dir.join("src"), shape));
        let out1 = dir.join("out1");
        let summary = export(&dir.join("src"), &out1, ExportFormat::Csv);
        assert!(summary.contains("16 nodes"), "{summary}");

        let header = std::fs::read_to_string(out1.join("nodes.csv")).unwrap();
        let header = header.lines().next().unwrap();
        assert!(
            header.starts_with(if shape.ids_are_keys {
                ":ID,:LABEL,"
            } else {
                ":KEY,:LABEL,"
            }),
            "{name}: {header}"
        );
        assert_eq!(
            std::fs::read_to_string(out1.join("relationships.csv"))
                .unwrap()
                .lines()
                .next(),
            Some(":START_ID,:END_ID,:TYPE")
        );
        assert_eq!(out1.join("rules.jsonl").is_file(), shape.rules, "{name}");

        import(&dir.join("dst"), &csv_inputs(&out1), 5);
        assert_same(&state(&dir.join("src")), &state(&dir.join("dst")), name);

        let out2 = dir.join("out2");
        export(&dir.join("dst"), &out2, ExportFormat::Csv);
        assert_eq!(
            read_dir_bytes(&out1),
            read_dir_bytes(&out2),
            "{name}: re-export"
        );
    }
}

fn stream_inputs(dir: &Path) -> ImportInputs {
    ImportInputs {
        stream_jsonl: Some(dir.join("graph.jsonl")),
        ..ImportInputs::default()
    }
}

/// The brief's single stream: one file, `{"type":"node","key":…}` and
/// `{"type":"edge",…}` lines, that imports back into the same store, with and
/// without rules (which travel as `{"type":"rule"}` lines).
#[test]
fn round_trip_single_file_jsonl_reproduces_the_store_with_and_without_rules() {
    for (name, shape) in shapes() {
        let dir = tmp(&format!("single-{name}"));
        drop(build(&dir.join("src"), shape));
        let out1 = dir.join("out1");
        let summary = export(&dir.join("src"), &out1, ExportFormat::JsonlSingle);
        assert!(summary.contains("16 nodes"), "{summary}");
        assert_eq!(
            read_dir_bytes(&out1).keys().collect::<Vec<_>>(),
            ["graph.jsonl"]
        );

        let text = std::fs::read_to_string(out1.join("graph.jsonl")).unwrap();
        let types: Vec<String> = text
            .lines()
            .map(|l| {
                let v: serde_json::Value = serde_json::from_str(l).unwrap();
                v["type"].as_str().unwrap().to_string()
            })
            .collect();
        assert_eq!(
            types.iter().filter(|t| *t == "node").count(),
            16,
            "{name}: one record per node"
        );
        assert!(
            text.lines()
                .next()
                .unwrap()
                .starts_with("{\"type\":\"node\","),
            "{text}"
        );
        let first_edge = types.iter().position(|t| t == "edge").unwrap();
        assert!(
            types[..first_edge].iter().all(|t| t == "node"),
            "nodes come first"
        );
        assert_eq!(
            types.iter().filter(|t| *t == "rule").count(),
            if shape.rules { 2 } else { 0 }
        );

        import(&dir.join("dst"), &stream_inputs(&out1), 5);
        assert_same(&state(&dir.join("src")), &state(&dir.join("dst")), name);
        let out2 = dir.join("out2");
        export(&dir.join("dst"), &out2, ExportFormat::JsonlSingle);
        assert_eq!(
            read_dir_bytes(&out1),
            read_dir_bytes(&out2),
            "{name}: re-export"
        );
    }
}

#[test]
fn round_trip_single_file_jsonl_edges_after_their_nodes_only() {
    let dir = tmp("single-order");
    let run = |name: &str, text: &str, ignore: bool| {
        let path = dir.join(format!("{name}.jsonl"));
        std::fs::write(&path, text).unwrap();
        run_import_inputs(
            &dir.join(format!("db-{name}")),
            &ImportInputs {
                stream_jsonl: Some(path),
                ..ImportInputs::default()
            },
            &ImportOptions {
                ignore_errors: ignore,
                // Nodes wait in the open batch when the edge arrives.
                batch_size: 100,
                ..ImportOptions::default()
            },
            &mut |_| {},
        )
    };
    let node = |k: &str| format!("{{\"type\":\"node\",\"key\":\"{k}\",\"label\":\"T\"}}\n");
    let edge = "{\"type\":\"edge\",\"edge_type\":\"K\",\"src\":\"a\",\"dst\":\"b\"}\n";

    // Nodes still in the open batch are committed before the edge needs them.
    let ok = run("ok", &format!("{}{}{edge}", node("a"), node("b")), false).unwrap();
    assert_eq!((ok.nodes_inserted, ok.edges_inserted), (2, 1));

    // An edge ahead of its nodes is an error naming its line.
    let e = run("early", &format!("{edge}{}{}", node("a"), node("b")), false)
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("early.jsonl:1: src `a` is not a node earlier in the stream"),
        "{e}"
    );

    // Under --ignore-errors it is skipped, and counted.
    let r = run("early2", &format!("{edge}{}{}", node("a"), node("b")), true).unwrap();
    assert_eq!(
        (r.nodes_inserted, r.edges_inserted, r.rows_skipped),
        (2, 0, 1)
    );

    let e = run("kind", "{\"type\":\"vertex\"}\n", false)
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("kind.jsonl:1: unknown line type `vertex`"),
        "{e}"
    );
    let e = run("notype", "{\"key\":\"a\"}\n", false)
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("notype.jsonl:1: the line has no string `type`"),
        "{e}"
    );
}

/// A node's count of manual edges is the same through every route, and a store
/// is also the same across formats: CSV in, JSONL out equals JSONL straight out.
#[test]
fn round_trip_csv_and_jsonl_agree() {
    let shape = Shape {
        ids_are_keys: false,
        rules: true,
        exotic: true,
    };
    let dir = tmp("agree");
    drop(build(&dir.join("src"), shape));
    export(&dir.join("src"), &dir.join("csv"), ExportFormat::Csv);
    import(&dir.join("via-csv"), &csv_inputs(&dir.join("csv")), 50_000);
    export(&dir.join("via-csv"), &dir.join("j1"), ExportFormat::Jsonl);
    export(&dir.join("src"), &dir.join("j2"), ExportFormat::Jsonl);
    assert_eq!(
        read_dir_bytes(&dir.join("j1")),
        read_dir_bytes(&dir.join("j2"))
    );
}

fn cypher_statements(dest: &Path) -> Vec<String> {
    std::fs::read_to_string(dest.join("graph.cypher"))
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

fn run_statements(db_dir: &Path, statements: &[String]) {
    let mut db = GraphDb::open(db_dir).unwrap();
    for s in statements {
        db.query_write(s, &BTreeMap::new())
            .unwrap_or_else(|e| panic!("the engine must parse its own export: {e}\n{s}"));
    }
}

#[test]
fn round_trip_cypher_statements_parse_and_reproduce_the_store() {
    let shape = Shape {
        ids_are_keys: true,
        rules: false,
        // No i64::MIN: it has no Cypher literal; that case is its own test.
        exotic: true,
    };
    let dir = tmp("cypher");
    let mut db = build(&dir.join("src"), shape);
    // The one value Cypher cannot write.
    db.set_prop("p6", "small", Value::Int(7)).unwrap();
    drop(db);
    let out = dir.join("out");
    let summary = export(&dir.join("src"), &out, ExportFormat::Cypher);
    assert!(!summary.contains("warning"), "{summary}");
    let statements = cypher_statements(&out);
    assert_eq!(
        statements.len(),
        16 + 11 + 12 + 1,
        "one statement per node and manual edge"
    );
    assert!(
        statements
            .contains(&"MERGE (n:`Org` {id: 'o0'}) ON CREATE SET n.`name` = 'org 0'".to_string()),
        "{statements:?}"
    );

    run_statements(&dir.join("dst"), &statements);
    assert_same(&state(&dir.join("src")), &state(&dir.join("dst")), "cypher");

    // Running the file again is harmless.
    run_statements(&dir.join("dst"), &statements);
    assert_same(
        &state(&dir.join("src")),
        &state(&dir.join("dst")),
        "cypher twice",
    );
}

/// Cypher stores a node's key as `id`: a node without one gains it, and every
/// other property is exact.
#[test]
fn round_trip_cypher_adds_only_the_id_property_a_node_lacked() {
    let shape = Shape {
        ids_are_keys: false,
        rules: false,
        exotic: false,
    };
    let dir = tmp("cypher-noid");
    drop(build(&dir.join("src"), shape));
    let out = dir.join("out");
    let summary = export(&dir.join("src"), &out, ExportFormat::Cypher);
    assert!(
        summary.contains("16 nodes had no id property and gain one"),
        "the one place a Cypher round trip is not an identity is reported: {summary}"
    );
    run_statements(&dir.join("dst"), &cypher_statements(&out));

    let mut expected = state(&dir.join("src"));
    for node in &mut expected.nodes {
        node.props.insert("id".into(), Value::Str(node.key.clone()));
    }
    assert_same(&expected, &state(&dir.join("dst")), "cypher without ids");
}

#[test]
fn round_trip_cypher_says_what_it_could_not_write() {
    let shape = Shape {
        ids_are_keys: true,
        rules: true,
        exotic: true,
    };
    let dir = tmp("cypher-loss");
    let mut db = build(&dir.join("src"), shape);
    // A node whose `id` is not its key: Cypher cannot say so.
    db.set_prop("o1", "id", Value::Str("other".into())).ok();
    drop(db);
    let out = dir.join("out");
    let summary = export(&dir.join("src"), &out, ExportFormat::Cypher);
    assert!(
        summary.contains("1 NaN, infinite or -2^63 numbers"),
        "{summary}"
    );
    assert!(summary.contains("rules are not written"), "{summary}");
    assert!(!summary.contains("had no id property"), "{summary}");
    assert!(
        summary.contains("derived edges are not written"),
        "{summary}"
    );
    // Everything that was written still parses.
    run_statements(&dir.join("dst"), &cypher_statements(&out));
}

#[test]
fn round_trip_jsonl_import_keeps_a_float_a_float_and_reads_the_tagged_forms() {
    let dir = tmp("exact");
    let nodes = dir.join("n.jsonl");
    std::fs::write(
        &nodes,
        "{\"key\":\"a\",\"label\":\"T\",\"f\":3.0,\"i\":3,\"big\":1e300,\"l\":[1.0,2],\"m\":{\"x\":2.0}}\n\n",
    )
    .unwrap();
    import(
        &dir.join("db"),
        &ImportInputs {
            nodes_jsonl: Some(nodes),
            ..ImportInputs::default()
        },
        10,
    );
    let st = state(&dir.join("db"));
    let p = &st.nodes[0].props;
    assert_eq!(p["f"], Value::Float(3.0));
    assert_eq!(p["i"], Value::Int(3));
    assert_eq!(p["big"], Value::Float(1e300));
    assert_eq!(p["l"], Value::List(vec![Value::Float(1.0), Value::Int(2)]));
    assert_eq!(
        p["m"],
        Value::Map(BTreeMap::from([("x".to_string(), Value::Float(2.0))]))
    );
    assert!(
        !p.contains_key("id"),
        "a JSONL node's key is not also a property"
    );
}

#[test]
fn round_trip_jsonl_import_errors_name_the_file_and_line() {
    let dir = tmp("errors");
    let opts = ImportOptions::default();
    let run = |text: &str| {
        let path = dir.join("n.jsonl");
        std::fs::write(&path, text).unwrap();
        let db = dir.join(format!("db{}", text.len()));
        run_import_inputs(
            &db,
            &ImportInputs {
                nodes_jsonl: Some(path),
                ..ImportInputs::default()
            },
            &opts,
            &mut |_| {},
        )
    };
    let e = run("{\"key\":\"a\",\"label\":\"T\"}\nnot json\n")
        .unwrap_err()
        .to_string();
    assert!(e.contains("n.jsonl:2: not JSON"), "{e}");
    let e = run("{\"key\":\"a\"}\n").unwrap_err().to_string();
    assert!(e.contains("n.jsonl:1: the line has no `label`"), "{e}");
    let e = run("{\"key\":\"a\",\"label\":\"T\"}\n{\"key\":\"a\",\"label\":\"T\"}\n")
        .unwrap_err()
        .to_string();
    assert!(e.contains("n.jsonl:2: duplicate key `a`"), "{e}");
    let e = run("[1]\n").unwrap_err().to_string();
    assert!(
        e.contains("n.jsonl:1: a node line must be a JSON object"),
        "{e}"
    );

    // --ignore-errors skips them and says how many.
    let path = dir.join("m.jsonl");
    std::fs::write(
        &path,
        "{\"key\":\"a\",\"label\":\"T\"}\nbroken\n{\"key\":\"b\",\"label\":\"T\"}\n",
    )
    .unwrap();
    let report = run_import_inputs(
        &dir.join("ignore"),
        &ImportInputs {
            nodes_jsonl: Some(path),
            ..ImportInputs::default()
        },
        &ImportOptions {
            ignore_errors: true,
            ..ImportOptions::default()
        },
        &mut |_| {},
    )
    .unwrap();
    assert_eq!((report.nodes_inserted, report.rows_skipped), (2, 1));
}

#[test]
fn round_trip_jsonl_edges_skip_derived_and_refuse_dangling() {
    let dir = tmp("edges");
    let nodes = dir.join("n.jsonl");
    std::fs::write(
        &nodes,
        "{\"key\":\"a\",\"label\":\"T\"}\n{\"key\":\"b\",\"label\":\"T\"}\n",
    )
    .unwrap();
    let edges = dir.join("e.jsonl");
    std::fs::write(
        &edges,
        "{\"edge_type\":\"K\",\"src\":\"a\",\"dst\":\"b\",\"derived\":false,\"rule\":null}\n\
         {\"edge_type\":\"D\",\"src\":\"a\",\"dst\":\"b\",\"derived\":true,\"rule\":\"r\"}\n",
    )
    .unwrap();
    let report = run_import_inputs(
        &dir.join("db"),
        &ImportInputs {
            nodes_jsonl: Some(nodes.clone()),
            edges_jsonl: Some(edges),
            ..ImportInputs::default()
        },
        &ImportOptions::default(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(
        (report.edges_inserted, report.derived_edges_skipped),
        (1, 1)
    );

    let dangling = dir.join("d.jsonl");
    std::fs::write(
        &dangling,
        "{\"edge_type\":\"K\",\"src\":\"a\",\"dst\":\"zz\"}\n",
    )
    .unwrap();
    let e = run_import_inputs(
        &dir.join("db"),
        &ImportInputs {
            edges_jsonl: Some(dangling),
            ..ImportInputs::default()
        },
        &ImportOptions::default(),
        &mut |_| {},
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("d.jsonl:1: dst `zz` is not a node"), "{e}");
}

/// A store holds none of the five new types until the V11 format lands, so an
/// import that carries one is refused whole, by the engine's own message, with
/// nothing written.
#[test]
fn round_trip_importing_a_typed_value_is_refused_until_v11() {
    let dir = tmp("typed");
    let nodes = dir.join("n.jsonl");
    std::fs::write(
        &nodes,
        "{\"key\":\"a\",\"label\":\"T\",\"born\":{\"$date\":\"2024-01-15\"}}\n",
    )
    .unwrap();
    let e = run_import_inputs(
        &dir.join("db"),
        &ImportInputs {
            nodes_jsonl: Some(nodes),
            ..ImportInputs::default()
        },
        &ImportOptions::default(),
        &mut |_| {},
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("V11"), "{e}");
    assert_eq!(GraphDb::open(&dir.join("db")).unwrap().node_count(), 0);
}

#[test]
fn round_trip_jsonl_reads_standard_input() {
    use std::io::Write;
    let dir = tmp("stdin");
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
        .args(["import", "--nodes-jsonl", "-", "--db"])
        .arg(dir.join("db"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"{\"key\":\"a\",\"label\":\"T\",\"n\":1}\n{\"key\":\"b\",\"label\":\"T\"}\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("imported 2 nodes"));
    assert_eq!(GraphDb::open(&dir.join("db")).unwrap().node_count(), 2);
}

/// The CLI route end to end, through the binary.
#[test]
fn round_trip_through_the_binary() {
    let dir = tmp("binary");
    drop(build(
        &dir.join("src"),
        Shape {
            ids_are_keys: true,
            rules: true,
            exotic: true,
        },
    ));
    let bin = env!("CARGO_BIN_EXE_mushroomdb");
    for format in ["jsonl", "csv"] {
        let out = dir.join(format);
        let status = std::process::Command::new(bin)
            .args(["export"])
            .arg(dir.join("src"))
            .arg(&out)
            .args(["--format", format])
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        let mut args: Vec<String> = vec![
            "import".into(),
            "--db".into(),
            dir.join(format!("db-{format}")).display().to_string(),
        ];
        if format == "jsonl" {
            for (flag, file) in [
                ("--nodes-jsonl", "nodes.jsonl"),
                ("--edges-jsonl", "edges.jsonl"),
                ("--rules-jsonl", "rules.jsonl"),
            ] {
                args.push(flag.into());
                args.push(out.join(file).display().to_string());
            }
        } else {
            for (flag, file) in [
                ("--nodes", "nodes.csv"),
                ("--relationships", "relationships.csv"),
                ("--rules-jsonl", "rules.jsonl"),
            ] {
                args.push(flag.into());
                args.push(out.join(file).display().to_string());
            }
        }
        let status = std::process::Command::new(bin)
            .args(&args)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "{format}");
        assert_same(
            &state(&dir.join("src")),
            &state(&dir.join(format!("db-{format}"))),
            format,
        );
    }
}

/// `cargo test -p mushroomdb-cli --release --test export_round_trip -- --ignored --nocapture throughput`
/// (`MUSHROOMDB_IMPORT_BENCH_NODES=1000000` for 1M).
#[test]
#[ignore = "builds a 100k-node JSONL file (1M with MUSHROOMDB_IMPORT_BENCH_NODES) and times its import; run in release with --ignored"]
fn jsonl_import_throughput_above_50k_per_second() {
    let n: usize = std::env::var("MUSHROOMDB_IMPORT_BENCH_NODES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000);
    let dir = tmp("throughput");
    let mut text = String::with_capacity(n * 96);
    for i in 0..n {
        text.push_str(&format!(
            "{{\"key\":\"p{i}\",\"label\":\"Person\",\"age\":{},\"city\":\"city{}\",\"id\":\"p{i}\",\"name\":\"person {i}\"}}\n",
            20 + i % 60,
            i % 1000
        ));
    }
    let nodes = dir.join("n.jsonl");
    std::fs::write(&nodes, &text).unwrap();
    let start = std::time::Instant::now();
    let report = run_import_inputs(
        &dir.join("db"),
        &ImportInputs {
            nodes_jsonl: Some(nodes),
            ..ImportInputs::default()
        },
        &ImportOptions::default(),
        &mut |_| {},
    )
    .unwrap();
    let took = start.elapsed();
    assert_eq!(report.nodes_inserted, n);
    let rate = n as f64 / took.as_secs_f64();
    eprintln!("jsonl import: {n} nodes in {took:?} = {rate:.0} nodes/s");
    assert!(rate > 50_000.0, "{rate:.0} nodes/s");
}

/// A batch the engine rejects is retried row by row under `--ignore-errors`,
/// and only the rows that fail alone are skipped.
#[test]
fn round_trip_ignore_errors_retries_a_rejected_node_batch_row_by_row() {
    let dir = tmp("retry");
    let nodes = dir.join("n.jsonl");
    std::fs::write(
        &nodes,
        "{\"key\":\"a\",\"label\":\"T\"}\n\
         {\"key\":\"b\",\"label\":\"T\",\"d\":{\"$date\":\"2024-01-15\"}}\n\
         {\"key\":\"c\",\"label\":\"T\"}\n",
    )
    .unwrap();
    let report = run_import_inputs(
        &dir.join("db"),
        &ImportInputs {
            nodes_jsonl: Some(nodes),
            ..ImportInputs::default()
        },
        &ImportOptions {
            ignore_errors: true,
            ..ImportOptions::default()
        },
        &mut |_| {},
    )
    .unwrap();
    assert_eq!((report.nodes_inserted, report.rows_skipped), (2, 1));
    assert!(
        report.errors[0].contains("n.jsonl:2:"),
        "{:?}",
        report.errors
    );
    let db = GraphDb::open(&dir.join("db")).unwrap();
    assert!(db.has_node("a") && !db.has_node("b") && db.has_node("c"));
}
