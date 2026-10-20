//! `mushroomdb import`: bulk CSV load of nodes and relationships.
//!
//! Pinned here: the loaded graph, every error with its file and line, the
//! differential against the engine's own incremental ingest (including after a
//! reopen), a process abort mid-import, and (ignored) the throughput.
use cli::import::{import_into, run_import, run_import_with_progress, ImportOptions};
use mushroomdb::{
    default_max_edges, AutoFk, GraphDb, IngestOptions, Predicate, RealFs, RuleDef, Value,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "mushroomdb-csvimport-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, text).unwrap();
    p
}

fn opts() -> ImportOptions {
    ImportOptions::default()
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

fn both_rules() -> [RuleDef; 2] {
    [same_city_rule(), via_city_rule()]
}

const NODES: &str =
    "personId:ID,:LABEL,name,age:int,score:float,active:boolean,tags:string[],city\n\
p1,Person,\"Lovelace, Ada\",36,1.5,true,math;poetry,London\n\
p2,Person,Alan,41,2.25,false,logic,London\n\
p3,Person,Grace,85,,true,,Arlington\n\
c1,Company,\"Acme \"\"Rockets\"\" Ltd\",,,,,\n";

const RELS: &str = ":START_ID,:END_ID,:TYPE\np1,c1,WORKS_AT\np2,c1,WORKS_AT\np1,p2,KNOWS\n";

#[test]
fn csv_import_loads_typed_nodes_and_relationships() {
    let dir = tmp("basic");
    let nodes = write(&dir, "nodes.csv", NODES);
    let rels = write(&dir, "rels.csv", RELS);
    let report = run_import(&dir.join("db"), Some(&nodes), Some(&rels), &opts()).unwrap();
    assert_eq!((report.nodes_inserted, report.edges_inserted), (4, 3));
    assert!(report.errors.is_empty() && report.warnings.is_empty());

    let db = GraphDb::open(&dir.join("db")).unwrap();
    let ada = db.node_ref("p1").expect("p1");
    assert_eq!(ada.label(), "Person");
    let props = ada.props();
    assert_eq!(props["personId"], Value::Str("p1".into()));
    assert_eq!(props["name"], Value::Str("Lovelace, Ada".into()));
    assert_eq!(props["age"], Value::Int(36));
    assert_eq!(props["score"], Value::Float(1.5));
    assert_eq!(props["active"], Value::Bool(true));
    assert_eq!(
        props["tags"],
        Value::List(vec![Value::Str("math".into()), Value::Str("poetry".into())])
    );
    let grace = db.node_ref("p3").unwrap().props();
    assert!(!grace.contains_key("score") && !grace.contains_key("tags"));
    assert_eq!(
        db.node_ref("c1").unwrap().props()["name"],
        Value::Str("Acme \"Rockets\" Ltd".into())
    );
    assert_eq!(db.edge_count(), 3);
    // The import ended with a snapshot: a second open reads it.
    assert!(dir.join("db").join("snapshot.bin").exists());
}

#[test]
fn csv_import_custom_delimiters() {
    let dir = tmp("delims");
    let nodes = write(&dir, "n.csv", ":ID|:LABEL|tags:string[]\na|P|x,y\n");
    let o = ImportOptions {
        delimiter: b'|',
        array_delimiter: ',',
        ..opts()
    };
    run_import(&dir.join("db"), Some(&nodes), None, &o).unwrap();
    let db = GraphDb::open(&dir.join("db")).unwrap();
    assert_eq!(
        db.node_ref("a").unwrap().props()["tags"],
        Value::List(vec![Value::Str("x".into()), Value::Str("y".into())])
    );
}

fn import_err(dir: &Path, nodes: &str, rels: Option<&str>, o: &ImportOptions) -> String {
    let n = write(dir, "n.csv", nodes);
    let r = rels.map(|t| write(dir, "r.csv", t));
    let _ = std::fs::remove_dir_all(dir.join("db"));
    run_import(&dir.join("db"), Some(&n), r.as_deref(), o)
        .unwrap_err()
        .0
}

#[test]
fn csv_import_bad_header_is_an_error_with_file_and_line() {
    let dir = tmp("badheader");
    let e = import_err(&dir, ":LABEL,name\nPerson,x\n", None, &opts());
    assert!(e.contains("n.csv:1:") && e.contains(":ID"), "{e}");
    let e = import_err(&dir, ":ID,name\n1,x\n", None, &opts());
    assert!(e.contains("n.csv:1:") && e.contains(":LABEL"), "{e}");
    let e = import_err(&dir, "", None, &opts());
    assert!(e.contains("n.csv:1:") && e.contains("no header"), "{e}");
    let e = import_err(
        &dir,
        ":ID,:LABEL\n1,P\n",
        Some(":START_ID,:TYPE\n"),
        &opts(),
    );
    assert!(e.contains("r.csv:1:") && e.contains(":END_ID"), "{e}");
}

#[test]
fn csv_import_unknown_type_suffix_warns_and_stores_a_string() {
    let dir = tmp("unknowntype");
    let n = write(&dir, "n.csv", ":ID,:LABEL,born:date\na,P,1815-12-10\n");
    let report = run_import(&dir.join("db"), Some(&n), None, &opts()).unwrap();
    assert_eq!(report.warnings.len(), 1);
    assert!(report.warnings[0].contains("n.csv:1:") && report.warnings[0].contains("born:date"));
    let db = GraphDb::open(&dir.join("db")).unwrap();
    assert_eq!(
        db.node_ref("a").unwrap().props()["born"],
        Value::Str("1815-12-10".into())
    );
}

#[test]
fn csv_import_row_errors_stop_with_file_and_line() {
    let dir = tmp("rowerrors");
    let cases: [(&str, &str, &str); 6] = [
        (
            ":ID,:LABEL\n1,P\n2,Person;Employee\n",
            "n.csv:3:",
            "more than one label",
        ),
        (":ID,:LABEL\n1,P\n2,\n", "n.csv:3:", "empty :LABEL"),
        (":ID,:LABEL\n,P\n", "n.csv:2:", "empty :ID"),
        (":ID,:LABEL,age:int\n1,P,x\n", "n.csv:2:", "age"),
        (":ID,:LABEL\n1,P\n1,P\n", "n.csv:3:", "duplicate :ID"),
        (":ID,:LABEL\n1,P,extra\n", "n.csv:2:", "fields"),
    ];
    for (text, at, what) in cases {
        let e = import_err(&dir, text, None, &opts());
        assert!(e.contains(at) && e.contains(what), "{text:?} -> {e}");
    }
}

#[test]
fn csv_import_dangling_relationship_endpoints_name_the_line() {
    let dir = tmp("dangling");
    let nodes = ":ID,:LABEL\na,P\nb,P\n";
    let e = import_err(
        &dir,
        nodes,
        Some(":START_ID,:END_ID,:TYPE\na,b,K\na,zzz,K\n"),
        &opts(),
    );
    assert!(e.contains("r.csv:3:") && e.contains(":END_ID `zzz`"), "{e}");
    let e = import_err(
        &dir,
        nodes,
        Some(":START_ID,:END_ID,:TYPE\nqqq,b,K\n"),
        &opts(),
    );
    assert!(
        e.contains("r.csv:2:") && e.contains(":START_ID `qqq`"),
        "{e}"
    );
}

#[test]
fn csv_import_relationship_properties_are_refused() {
    let dir = tmp("relprops");
    let e = import_err(
        &dir,
        ":ID,:LABEL\na,P\n",
        Some(":START_ID,:END_ID,:TYPE,since:int\na,a,K,1\n"),
        &opts(),
    );
    assert!(
        e.contains("r.csv:1:") && e.contains("since") && e.contains("not supported"),
        "{e}"
    );
}

#[test]
fn csv_import_malformed_quoting_is_fatal_and_writes_nothing() {
    let dir = tmp("quoting");
    let bad = ":ID,:LABEL,name\n1,P,fine\n2,P,\"never closed\n3,P,x\n";
    for ignore in [false, true] {
        let o = ImportOptions {
            ignore_errors: ignore,
            ..opts()
        };
        let e = import_err(&dir, bad, None, &o);
        assert!(e.contains("n.csv:3:") && e.contains("never closed"), "{e}");
        assert!(
            !dir.join("db").join("wal.bin").exists()
                || GraphDb::open(&dir.join("db")).unwrap().node_count() == 0,
            "a quoting error is found before anything is written"
        );
    }
    let e = import_err(&dir, ":ID,:LABEL,name\n1,P,ab\"c\n", None, &opts());
    assert!(e.contains("n.csv:2:") && e.contains("quote"), "{e}");
}

#[test]
fn csv_import_ignore_errors_skips_bad_rows_and_counts_them() {
    let dir = tmp("ignore");
    let n = write(
        &dir,
        "n.csv",
        ":ID,:LABEL,age:int\na,P,1\nb,P,oops\nc,Person;Employee,2\na,P,3\nd,P,4\n",
    );
    let r = write(
        &dir,
        "r.csv",
        ":START_ID,:END_ID,:TYPE\na,d,K\na,ghost,K\n,d,K\n",
    );
    let o = ImportOptions {
        ignore_errors: true,
        ..opts()
    };
    let report = run_import(&dir.join("db"), Some(&n), Some(&r), &o).unwrap();
    assert_eq!((report.nodes_inserted, report.edges_inserted), (2, 1));
    assert_eq!(report.rows_skipped, 5);
    assert_eq!(report.errors.len(), 5);
    assert!(report.errors[0].contains("n.csv:3:"), "{:?}", report.errors);
    assert!(report
        .errors
        .iter()
        .any(|e| e.contains("r.csv:3:") && e.contains("ghost")));
    assert!(report
        .errors
        .iter()
        .any(|e| e.contains("r.csv:4:") && e.contains(":START_ID")));
    let db = GraphDb::open(&dir.join("db")).unwrap();
    assert_eq!(db.node_count(), 2);
}

#[test]
fn csv_import_batches_commit_independently() {
    let dir = tmp("batches");
    let mut text = String::from(":ID,:LABEL\n");
    for i in 0..25 {
        text.push_str(&format!("n{i},P\n"));
    }
    let n = write(&dir, "n.csv", &text);
    let o = ImportOptions {
        batch_size: 10,
        ..opts()
    };
    let mut seen = Vec::new();
    let report = run_import_with_progress(&dir.join("db"), Some(&n), None, &o, &mut |p| {
        seen.push((p.batches_committed, p.nodes_inserted));
    })
    .unwrap();
    assert_eq!(report.batches, 3);
    assert_eq!(seen, vec![(1, 10), (2, 20), (3, 25)]);
}

// ---------------------------------------------------------------- differential

/// A store's nodes and edges in a comparable form.
fn dump(db: &GraphDb<RealFs>) -> (Vec<String>, Vec<String>) {
    let mut nodes: Vec<String> = db
        .all_nodes_for_export()
        .into_iter()
        .map(|n| format!("{}|{}|{:?}", n.key, n.label, n.props))
        .collect();
    nodes.sort();
    let mut edges: Vec<String> = db
        .all_edges_for_export()
        .into_iter()
        .map(|e| {
            format!(
                "{}|{}|{}|{}|{:?}",
                e.edge_type, e.src, e.dst, e.derived, e.rule
            )
        })
        .collect();
    edges.sort();
    (nodes, edges)
}

/// A few hundred rows across two labels, cities shared so the rule derives.
fn generated_csv(people: usize) -> (String, String) {
    let mut nodes = String::from("id:ID,:LABEL,name,age:int,city\n");
    let mut rels = String::from(":START_ID,:END_ID,:TYPE\n");
    for i in 0..people {
        nodes.push_str(&format!(
            "p{i},Person,name {i},{},city{}\n",
            20 + i % 50,
            i % 7
        ));
        if i > 0 {
            rels.push_str(&format!("p{i},p{},KNOWS\n", i / 2));
        }
    }
    for c in 0..5 {
        nodes.push_str(&format!("c{c},Company,Co {c},,\n"));
    }
    for i in (0..people).step_by(3) {
        rels.push_str(&format!("p{i},c{},WORKS_AT\n", i % 5));
    }
    (nodes, rels)
}

/// The engine's own incremental path over the same rows: `ingest` per label
/// (auto foreign-key rules off, as the import has them) and `insert_edge`.
fn incremental(dir: &Path, nodes_csv: &str, rels_csv: &str, rule_first: bool) -> GraphDb<RealFs> {
    let mut db = GraphDb::open(dir).unwrap();
    if rule_first {
        for rule in both_rules() {
            db.create_rule(rule).unwrap();
        }
    }
    let mut r = csv::Reader::from_reader(nodes_csv.as_bytes());
    let header = r.headers().unwrap().clone();
    let mut by_label: BTreeMap<String, Vec<BTreeMap<String, Value>>> = BTreeMap::new();
    for rec in r.records() {
        let rec = rec.unwrap();
        let mut row = BTreeMap::new();
        row.insert("id".to_string(), Value::Str(rec[0].to_string()));
        row.insert("name".to_string(), Value::Str(rec[2].to_string()));
        if !rec[3].is_empty() {
            row.insert("age".to_string(), Value::Int(rec[3].parse().unwrap()));
        }
        if !rec[4].is_empty() {
            row.insert("city".to_string(), Value::Str(rec[4].to_string()));
        }
        by_label.entry(rec[1].to_string()).or_default().push(row);
    }
    assert_eq!(header.len(), 5);
    let io = IngestOptions {
        key_field: "id".into(),
        auto_fk: AutoFk::Off,
    };
    for (label, rows) in by_label {
        db.ingest(&label, rows, &io).unwrap();
    }
    let mut r = csv::Reader::from_reader(rels_csv.as_bytes());
    for rec in r.records() {
        let rec = rec.unwrap();
        db.insert_edge(&rec[2], &rec[0], &rec[1]).unwrap();
    }
    if !rule_first {
        for rule in both_rules() {
            db.create_rule(rule).unwrap();
        }
    }
    db
}

#[test]
fn csv_import_equals_incremental_ingest_including_derived_edges_and_reopen() {
    let dir = tmp("differential");
    let (nodes_csv, rels_csv) = generated_csv(300);
    let nodes = write(&dir, "n.csv", &nodes_csv);
    let rels = write(&dir, "r.csv", &rels_csv);

    // Reference: rule declared first, so it fires on every ingest batch.
    let reference = incremental(&dir.join("inc"), &nodes_csv, &rels_csv, true);
    let want = dump(&reference);
    assert!(
        want.1
            .iter()
            .any(|e| e.contains("SAME_CITY") && e.contains("true")),
        "the fixture must derive edges"
    );
    drop(reference);

    assert!(
        want.1.iter().any(|e| e.contains("VIA_CITY")),
        "the fixture must derive via-hop edges"
    );

    // (a) bulk import into a store that already holds both rules: they fire
    // per batch. Read while the importing handle is live, after a snapshot of
    // it, and after a reopen.
    let a = dir.join("bulk-rule-first");
    let o = ImportOptions {
        batch_size: 64,
        ..opts()
    };
    {
        let mut db = GraphDb::open(&a).unwrap();
        for rule in both_rules() {
            db.create_rule(rule).unwrap();
        }
        import_into(&mut db, Some(&nodes), Some(&rels), &o, &mut |_| {}).unwrap();
        assert_eq!(dump(&db), want, "rule first, live");
        db.snapshot_with(cli::AUTOMATIC_SNAPSHOT).unwrap();
        assert_eq!(dump(&db), want, "rule first, after its snapshot");
    }
    assert_eq!(
        dump(&GraphDb::open(&a).unwrap()),
        want,
        "rule first, after reopen"
    );

    // (b) bulk import with no rules, then the rules: one backfill at the end.
    let b = dir.join("bulk-backfill");
    run_import(&b, Some(&nodes), Some(&rels), &o).unwrap();
    {
        let mut db = GraphDb::open(&b).unwrap();
        for rule in both_rules() {
            db.create_rule(rule).unwrap();
        }
        assert_eq!(dump(&db), want, "backfill, never closed");
    }
    assert_eq!(
        dump(&GraphDb::open(&b).unwrap()),
        want,
        "backfill, after reopen"
    );

    // (c) batch size must not change the result.
    let c = dir.join("bulk-one-batch");
    run_import(&c, Some(&nodes), Some(&rels), &opts()).unwrap();
    {
        let mut db = GraphDb::open(&c).unwrap();
        for rule in both_rules() {
            db.create_rule(rule).unwrap();
        }
    }
    assert_eq!(dump(&GraphDb::open(&c).unwrap()), want, "one batch");
}

#[test]
fn csv_import_header_only_files_import_nothing_and_leave_no_intent() {
    let dir = tmp("headeronly");
    let n = write(&dir, "n.csv", ":ID,:LABEL\n");
    let r = write(&dir, "r.csv", ":START_ID,:END_ID,:TYPE\n");
    let report = run_import(&dir.join("db"), Some(&n), Some(&r), &opts()).unwrap();
    assert_eq!(
        (report.nodes_inserted, report.edges_inserted, report.batches),
        (0, 0, 0)
    );
    let intent = std::fs::read(dir.join("db").join("snapshot.intent")).unwrap_or_default();
    assert!(intent.is_empty(), "no snapshot intent left behind");
    assert_eq!(GraphDb::open(&dir.join("db")).unwrap().node_count(), 0);
}

#[test]
fn csv_import_a_different_array_delimiter_cannot_smuggle_a_multi_label() {
    let dir = tmp("labeldelim");
    let o = ImportOptions {
        array_delimiter: b'|' as char,
        ..opts()
    };
    let e = import_err(&dir, ":ID,:LABEL\n1,Person;Employee\n", None, &o);
    assert!(
        e.contains("n.csv:2:") && e.contains("more than one label"),
        "{e}"
    );
    let e = import_err(&dir, ":ID,:LABEL\n1,Person|Employee\n", None, &o);
    assert!(
        e.contains("n.csv:2:") && e.contains("more than one label"),
        "{e}"
    );
}

#[test]
fn csv_import_id_spaces_are_refused_with_the_file_and_line() {
    let dir = tmp("idspace");
    let e = import_err(&dir, "personId:ID(People),:LABEL\n1,P\n", None, &opts());
    assert!(
        e.contains("n.csv:1:") && e.contains("id spaces are not supported in mushroomdb 0.10"),
        "{e}"
    );
    let e = import_err(
        &dir,
        ":ID,:LABEL\n1,P\n",
        Some(":START_ID(People),:END_ID,:TYPE\n"),
        &opts(),
    );
    assert!(e.contains("r.csv:1:") && e.contains("id spaces"), "{e}");
}

/// Nodes in one city with a rule over them, so `SAME_CITY` between two of
/// them is rule-owned and a by-hand insert of it is refused by the engine.
fn store_with_rule_owned_edge(dir: &Path) -> PathBuf {
    let db_dir = dir.join("db");
    let nodes = write(
        dir,
        "n.csv",
        "id:ID,:LABEL,city\na,Person,x\nb,Person,x\nc,Person,y\nd,Person,y\n",
    );
    let mut db = GraphDb::open(&db_dir).unwrap();
    db.create_rule(same_city_rule()).unwrap();
    import_into(&mut db, Some(&nodes), None, &opts(), &mut |_| {}).unwrap();
    assert!(db.edge_count() >= 1, "SAME_CITY a-b is derived");
    drop(db);
    db_dir
}

#[test]
fn csv_import_strict_mode_commits_no_part_of_a_failing_edge_batch() {
    let dir = tmp("strictedges");
    let db_dir = store_with_rule_owned_edge(&dir);
    let before = GraphDb::open(&db_dir).unwrap().edge_count();
    // Batches of two: [KNOWS a-c, KNOWS a-d] commit, then [KNOWS c-d, SAME_CITY
    // a-b] is refused whole, so c-d must not be there.
    let rels = write(
        &dir,
        "r.csv",
        ":START_ID,:END_ID,:TYPE\na,c,KNOWS\na,d,KNOWS\nc,d,KNOWS\na,b,SAME_CITY\n",
    );
    let o = ImportOptions {
        batch_size: 2,
        ..opts()
    };
    let mut batches = 0;
    let mut db = GraphDb::open(&db_dir).unwrap();
    let e = import_into(&mut db, None, Some(&rels), &o, &mut |p| {
        batches = p.batches_committed
    })
    .unwrap_err()
    .0;
    assert!(e.contains("r.csv:4:") && e.contains("rejected"), "{e}");
    assert!(e.contains("2 edges in 1 committed batches"), "{e}");
    assert_eq!(batches, 1, "only the first batch committed");
    assert_eq!(
        db.edge_count(),
        before + 2,
        "the first batch stays, none of the second"
    );
    drop(db);
    assert_eq!(GraphDb::open(&db_dir).unwrap().edge_count(), before + 2);
}

#[test]
fn csv_import_ignore_errors_skips_the_failing_edge_and_counts_one_batch() {
    let dir = tmp("ignoreedges");
    let db_dir = store_with_rule_owned_edge(&dir);
    let rels = write(
        &dir,
        "r.csv",
        ":START_ID,:END_ID,:TYPE\na,c,KNOWS\na,b,SAME_CITY\nc,d,KNOWS\n",
    );
    let o = ImportOptions {
        ignore_errors: true,
        ..opts()
    };
    let mut db = GraphDb::open(&db_dir).unwrap();
    let report = import_into(&mut db, None, Some(&rels), &o, &mut |_| {}).unwrap();
    assert_eq!(
        (report.edges_inserted, report.rows_skipped, report.batches),
        (2, 1, 1)
    );
    assert!(report.errors[0].contains("r.csv:3:"), "{:?}", report.errors);
}

// ---------------------------------------------------------------- crash safety

const CHILD_ENV: &str = "MUSHROOMDB_CSV_IMPORT_CRASH_CHILD";

/// Re-entered as a subprocess: imports, and `abort()`s once `abort_after`
/// batches have committed. Does nothing when run as an ordinary test.
#[test]
fn csv_import_crash_child() {
    let Ok(spec) = std::env::var(CHILD_ENV) else {
        return;
    };
    let mut parts = spec.split('|');
    let dir = PathBuf::from(parts.next().unwrap());
    let abort_after: usize = parts.next().unwrap().parse().unwrap();
    let o = ImportOptions {
        batch_size: 100,
        ..opts()
    };
    let _ = run_import_with_progress(
        &dir.join("db"),
        Some(&dir.join("n.csv")),
        Some(&dir.join("r.csv")),
        &o,
        &mut |p| {
            if p.batches_committed == abort_after {
                std::process::abort();
            }
        },
    );
}

fn crash_after(dir: &Path, abort_after: usize) {
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "csv_import_crash_child", "--nocapture"])
        .env(CHILD_ENV, format!("{}|{abort_after}", dir.display()))
        .status()
        .unwrap();
    assert!(
        !status.success(),
        "the child must have aborted, got {status:?}"
    );
}

#[test]
fn csv_import_killed_mid_import_leaves_whole_batches_only() {
    let dir = tmp("crash");
    // 450 nodes -> 5 batches of 100 (the last 50); 400 edges -> 4 batches.
    let mut nodes = String::from(":ID,:LABEL,city\n");
    let mut rels = String::from(":START_ID,:END_ID,:TYPE\n");
    for i in 0..450 {
        nodes.push_str(&format!("n{i},Person,c{}\n", i % 9));
    }
    for i in 0..400 {
        rels.push_str(&format!("n{i},n{},KNOWS\n", i + 1));
    }
    write(&dir, "n.csv", &nodes);
    write(&dir, "r.csv", &rels);
    {
        // A rule that has derived edges when the process dies.
        let mut db = GraphDb::open(&dir.join("db")).unwrap();
        db.create_rule(same_city_rule()).unwrap();
    }

    // Dies after batch 3: three whole node batches, no edges.
    crash_after(&dir, 3);
    let db = GraphDb::open(&dir.join("db")).unwrap();
    assert_eq!(
        db.node_count(),
        300,
        "exactly the committed batches, never part of one"
    );
    assert_eq!(
        db.node_ref("n299")
            .map(|n| n.label().to_string())
            .as_deref(),
        Some("Person")
    );
    assert!(
        db.node_ref("n300").is_none(),
        "nothing from the batch in flight"
    );
    let manual: usize = dump(&db).1.iter().filter(|e| e.contains("KNOWS")).count();
    assert_eq!(manual, 0);
    // Base data is whole, and the derived edges are exactly what the committed
    // base data derives: a store that never saw the import, given the same 300
    // nodes the engine's own way, has the same edges.
    let reopened = dump(&db);
    drop(db);
    let reference = tmp("crash-reference");
    let mut want = GraphDb::open(&reference).unwrap();
    want.create_rule(same_city_rule()).unwrap();
    let rows = (0..300)
        .map(|i| {
            BTreeMap::from([
                ("id".to_string(), Value::Str(format!("n{i}"))),
                ("city".to_string(), Value::Str(format!("c{}", i % 9))),
            ])
        })
        .collect();
    let io = IngestOptions {
        key_field: "id".into(),
        auto_fk: AutoFk::Off,
    };
    want.ingest("Person", rows, &io).unwrap();
    assert!(reopened.1.iter().any(|e| e.contains("SAME_CITY")));
    assert_eq!(
        reopened,
        dump(&want),
        "derived edges after a crash, reopened"
    );

    // Dies after batch 7 of 9: 5 node batches (450) + 2 whole edge batches (200).
    let dir2 = tmp("crash2");
    write(&dir2, "n.csv", &nodes);
    write(&dir2, "r.csv", &rels);
    crash_after(&dir2, 7);
    let db = GraphDb::open(&dir2.join("db")).unwrap();
    assert_eq!(db.node_count(), 450);
    assert_eq!(
        db.edge_count(),
        200,
        "two whole edge batches, never part of one"
    );
    assert!(
        !dir2.join("db").join("snapshot.bin").exists(),
        "the snapshot is written last, so a killed import has none"
    );
}

// ---------------------------------------------------------------- throughput

/// `MUSHROOMDB_IMPORT_BENCH_NODES` (default 100000) nodes through
/// `mushroomdb import` and through the engine's incremental `ingest`, same
/// rows, both printed. Run in release:
/// `cargo test -p mushroomdb-cli --release --test csv_import -- --ignored --nocapture throughput`
#[test]
#[ignore = "builds a 100k-node store (1M with MUSHROOMDB_IMPORT_BENCH_NODES) and times it; run in release with --ignored"]
fn csv_import_throughput_above_50k_per_second() {
    let n: usize = std::env::var("MUSHROOMDB_IMPORT_BENCH_NODES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000);
    let dir = tmp("throughput");
    let mut nodes = String::with_capacity(n * 48);
    nodes.push_str("id:ID,:LABEL,name,age:int,city\n");
    for i in 0..n {
        nodes.push_str(&format!(
            "p{i},Person,person {i},{},city{}\n",
            20 + i % 60,
            i % 1000
        ));
    }
    let nodes_path = write(&dir, "n.csv", &nodes);

    let start = std::time::Instant::now();
    let report = run_import(&dir.join("bulk"), Some(&nodes_path), None, &opts()).unwrap();
    let bulk = start.elapsed();
    assert_eq!(report.nodes_inserted, n);

    let mut db = GraphDb::open(&dir.join("inc")).unwrap();
    let mut r = csv::Reader::from_reader(nodes.as_bytes());
    let io = IngestOptions {
        key_field: "id".into(),
        auto_fk: AutoFk::Off,
    };
    let start = std::time::Instant::now();
    let mut rows = Vec::new();
    for rec in r.records() {
        let rec = rec.unwrap();
        let mut row = BTreeMap::new();
        row.insert("id".to_string(), Value::Str(rec[0].to_string()));
        row.insert("name".to_string(), Value::Str(rec[2].to_string()));
        row.insert("age".to_string(), Value::Int(rec[3].parse().unwrap()));
        row.insert("city".to_string(), Value::Str(rec[4].to_string()));
        rows.push(row);
        if rows.len() == 50_000 {
            db.ingest("Person", std::mem::take(&mut rows), &io).unwrap();
        }
    }
    db.ingest("Person", rows, &io).unwrap();
    let inc = start.elapsed();

    let rate = |d: std::time::Duration| n as f64 / d.as_secs_f64();
    println!(
        "csv_import n={n}: import {:.2}s = {:.0} nodes/s (incl. final snapshot); \
         incremental ingest {:.2}s = {:.0} nodes/s (no snapshot)",
        bulk.as_secs_f64(),
        rate(bulk),
        inc.as_secs_f64(),
        rate(inc)
    );
    assert!(
        rate(bulk) > 50_000.0,
        "import ran at {:.0} nodes/s",
        rate(bulk)
    );
}
