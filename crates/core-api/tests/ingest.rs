use core_api::{
    AutoFk, Direction, FkSkip, GraphDb, GraphError, IngestOptions, IngestReport, Predicate, Value,
};
use core_storage::fs::{FileId, Fs, RealFs};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn wal_len(dir: &std::path::Path) -> u64 {
    std::fs::metadata(dir.join("wal.bin"))
        .map(|m| m.len())
        .unwrap_or(0)
}

fn row(pairs: &[(&str, &str)]) -> BTreeMap<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), Value::Str((*v).into())))
        .collect()
}

fn row_val(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), v.clone()))
        .collect()
}

/// Binding: org_id rows auto-link to pre-existing Org nodes; rule is a real
/// incremental KeyMatch (`auto_fk_person_org_id`, edge type `ORG`); explain works;
/// a later plain `insert_node` with the same FK also links.
#[test]
fn org_id_auto_links_and_later_insert_fires_rule() {
    let dir = tmp("ingest-org");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Org", "acme", vec![]).unwrap();
    db.insert_node("Org", "beta", vec![]).unwrap();

    let report = db
        .ingest(
            "Person",
            vec![
                row(&[("id", "p1"), ("org_id", "acme")]),
                row(&[("id", "p2"), ("org_id", "beta")]),
            ],
            &IngestOptions::default(),
        )
        .unwrap();

    assert_eq!(report.inserted, 2);
    assert!(report.row_errors.is_empty());
    assert_eq!(
        report.rules_created,
        vec!["auto_fk_person_org_id".to_string()]
    );
    assert!(report.skipped_fk_fields.is_empty());

    let rules = db.rules();
    let fk = rules
        .iter()
        .find(|r| r.name == "auto_fk_person_org_id")
        .unwrap();
    assert_eq!(fk.src_label, "Person");
    assert_eq!(fk.dst_label, "Org");
    assert_eq!(fk.edge_type, "ORG");
    assert_eq!(
        fk.predicate,
        Predicate::KeyMatch {
            field: "org_id".into()
        }
    );
    assert_eq!(fk.weight_prop, None);
    assert_eq!(fk.max_edges, None);

    assert_eq!(
        db.neighbors("p1", "ORG", Direction::Out).unwrap(),
        vec!["acme".to_string()]
    );
    assert_eq!(
        db.neighbors("p2", "ORG", Direction::Out).unwrap(),
        vec!["beta".to_string()]
    );
    assert_eq!(
        db.get_prop("p1", "id"),
        Some(&Value::Str("p1".into())),
        "key_field is stored as a normal prop"
    );
    assert_eq!(
        db.get_prop("p1", "org_id"),
        Some(&Value::Str("acme".into()))
    );

    let ex = db.explain("p1", "acme").unwrap();
    assert_eq!(ex.len(), 1);
    assert_eq!(ex[0].rule, "auto_fk_person_org_id");
    assert_eq!(ex[0].edge_type, "ORG");
    assert_eq!(ex[0].src_key, "p1");
    assert_eq!(ex[0].dst_key, "acme");
    assert_eq!(ex[0].weight, None);

    db.insert_node(
        "Person",
        "p3",
        vec![("org_id".into(), Value::Str("acme".into()))],
    )
    .unwrap();
    assert_eq!(
        db.neighbors("p3", "ORG", Direction::Out).unwrap(),
        vec!["acme".to_string()],
        "auto-FK is a real incremental rule, not a one-shot backfill"
    );
    let ex3 = db.explain("p3", "acme").unwrap();
    assert_eq!(ex3.len(), 1);
    assert_eq!(ex3[0].rule, "auto_fk_person_org_id");

    // Same-label re-ingest: name collision on (Person, org_id) skips creating
    // a second rule; new rows still link incrementally.
    let again = db
        .ingest(
            "Person",
            vec![row(&[("id", "p4"), ("org_id", "beta")])],
            &IngestOptions::default(),
        )
        .unwrap();
    assert_eq!(again.inserted, 1);
    assert!(again.rules_created.is_empty());
    assert!(again.skipped_fk_fields.is_empty());
    assert_eq!(
        db.neighbors("p4", "ORG", Direction::Out).unwrap(),
        vec!["beta".to_string()]
    );
}

/// Binding: matching keys under more than one label → no rule, reason reported.
/// Keys are globally unique, so ambiguity is distinct FK values resolving to
/// different labels (not one key living under two labels).
#[test]
fn ambiguous_fk_targets_are_skipped() {
    let dir = tmp("ingest-ambig");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Org", "acme", vec![]).unwrap();
    db.insert_node("Team", "eng", vec![]).unwrap();

    let report = db
        .ingest(
            "Person",
            vec![
                row(&[("id", "p1"), ("org_id", "acme")]),
                row(&[("id", "p2"), ("org_id", "eng")]),
            ],
            &IngestOptions::default(),
        )
        .unwrap();

    assert_eq!(report.inserted, 2);
    assert!(report.rules_created.is_empty());
    assert_eq!(
        report.skipped_fk_fields,
        vec![FkSkip {
            field: "org_id".into(),
            reason: "ambiguous target labels: Org, Team".into(),
        }]
    );
    assert!(db.rules().is_empty());
    assert!(db
        .neighbors("p1", "ORG", Direction::Out)
        .unwrap()
        .is_empty());
}

/// Binding: FK field whose Str values match no live (or intra-ingest) key.
#[test]
fn unmatched_fk_field_is_skipped() {
    let dir = tmp("ingest-nomatch");
    let mut db = GraphDb::open(&dir).unwrap();

    let report = db
        .ingest(
            "Person",
            vec![row(&[("id", "p1"), ("dept_id", "ghost")])],
            &IngestOptions::default(),
        )
        .unwrap();

    assert_eq!(report.inserted, 1);
    assert!(report.rules_created.is_empty());
    assert_eq!(
        report.skipped_fk_fields,
        vec![FkSkip {
            field: "dept_id".into(),
            reason: "no matching target keys".into(),
        }]
    );
    assert!(db.rules().is_empty());
    assert!(db.has_node("p1"));
}

/// Binding: missing / non-Str key_field is a per-row error; remaining rows insert.
#[test]
fn missing_key_field_rows_are_counted_not_fatal() {
    let dir = tmp("ingest-missing-key");
    let mut db = GraphDb::open(&dir).unwrap();

    let report = db
        .ingest(
            "Person",
            vec![
                row(&[("name", "anon")]),
                row(&[("id", "p1"), ("name", "ada")]),
                row_val(&[("id", Value::Int(7)), ("name", Value::Str("num".into()))]),
                row(&[("id", "p2")]),
            ],
            &IngestOptions::default(),
        )
        .unwrap();

    assert_eq!(report.inserted, 2);
    assert_eq!(
        report.row_errors,
        vec![
            (0, "missing key field id".into()),
            (2, "key field id is not a string".into()),
        ]
    );
    assert!(db.has_node("p1"));
    assert!(db.has_node("p2"));
    assert!(!db.has_node("anon"));
    assert_eq!(db.get_prop("p1", "name"), Some(&Value::Str("ada".into())));
}

/// Binding: duplicate vs existing db and vs an earlier accepted row → row errors.
#[test]
fn duplicate_keys_are_row_errors() {
    let dir = tmp("ingest-dups");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Person", "p1", vec![]).unwrap();

    let report = db
        .ingest(
            "Person",
            vec![
                row(&[("id", "p1")]),
                row(&[("id", "p2")]),
                row(&[("id", "p2")]),
                row(&[("id", "p3")]),
            ],
            &IngestOptions::default(),
        )
        .unwrap();

    assert_eq!(report.inserted, 2);
    assert_eq!(
        report.row_errors,
        vec![
            (0, "duplicate key p1".into()),
            (2, "duplicate key p2".into()),
        ]
    );
    assert!(db.has_node("p2"));
    assert!(db.has_node("p3"));
}

/// Binding: commit-level Err applies nothing; WAL bytes unchanged.
#[test]
fn commit_failure_applies_nothing() {
    let dir = tmp("ingest-atomic");
    let fail = Arc::new(AtomicBool::new(false));
    let fs = FailWalAppend {
        inner: RealFs::new(&dir).unwrap(),
        fail: fail.clone(),
    };
    let mut db = GraphDb::open_with(fs).unwrap();
    db.insert_node("Org", "acme", vec![]).unwrap();

    let before_wal = wal_len(&dir);
    let before_nodes = db.stats().nodes_live;
    fail.store(true, Ordering::SeqCst);

    let err = db
        .ingest(
            "Person",
            vec![row(&[("id", "p1"), ("org_id", "acme")])],
            &IngestOptions::default(),
        )
        .unwrap_err();
    assert!(matches!(err, GraphError::Io(_)), "got {err:?}");

    assert_eq!(wal_len(&dir), before_wal);
    assert_eq!(db.stats().nodes_live, before_nodes);
    assert!(!db.has_node("p1"));
    assert!(db.rules().is_empty());
    assert!(db.has_node("acme"));
}

/// Binding: AutoFk::Off inserts rows and does not declare rules or edges.
#[test]
fn auto_fk_off_disables_inference() {
    let dir = tmp("ingest-off");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Org", "acme", vec![]).unwrap();

    let report = db
        .ingest(
            "Person",
            vec![row(&[("id", "p1"), ("org_id", "acme")])],
            &IngestOptions {
                auto_fk: AutoFk::Off,
                ..IngestOptions::default()
            },
        )
        .unwrap();

    assert_eq!(
        report,
        IngestReport {
            inserted: 1,
            row_errors: vec![],
            rules_created: vec![],
            skipped_fk_fields: vec![],
            edges_inserted: 0,
        }
    );
    assert!(db.rules().is_empty());
    assert!(db.has_node("p1"));
    assert!(db
        .neighbors("p1", "ORG", Direction::Out)
        .unwrap()
        .is_empty());
}

/// Binding: rows may FK other rows in the same ingest (self-label rule + edges).
#[test]
fn intra_ingest_fk_builds_self_label_rule() {
    let dir = tmp("ingest-intra");
    let mut db = GraphDb::open(&dir).unwrap();

    // Child first so the dest-side KeyMatch path has to fire when p1 arrives.
    let report = db
        .ingest(
            "Person",
            vec![
                row(&[("id", "p2"), ("manager_id", "p1")]),
                row(&[("id", "p1")]),
            ],
            &IngestOptions::default(),
        )
        .unwrap();

    assert_eq!(report.inserted, 2);
    assert_eq!(
        report.rules_created,
        vec!["auto_fk_person_manager_id".to_string()]
    );
    assert!(report.skipped_fk_fields.is_empty());

    let fk = db
        .rules()
        .into_iter()
        .find(|r| r.name == "auto_fk_person_manager_id")
        .unwrap();
    assert_eq!(fk.src_label, "Person");
    assert_eq!(fk.dst_label, "Person");
    assert_eq!(fk.edge_type, "MANAGER");
    assert_eq!(
        fk.predicate,
        Predicate::KeyMatch {
            field: "manager_id".into()
        }
    );

    assert_eq!(
        db.neighbors("p2", "MANAGER", Direction::Out).unwrap(),
        vec!["p1".to_string()]
    );
    let ex = db.explain("p2", "p1").unwrap();
    assert_eq!(ex.len(), 1);
    assert_eq!(ex[0].rule, "auto_fk_person_manager_id");
    assert_eq!(ex[0].edge_type, "MANAGER");
}

/// Spec amendment: rule names are `auto_fk_<src_label_lowercase>_<field>`, so
/// Person.org_id and Device.org_id each get their own rule. Before this,
/// the second ingest silently skipped (name collision) and Devices did not link.
#[test]
fn second_src_label_same_fk_field_creates_own_rule_and_links() {
    let dir = tmp("ingest-two-src");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Org", "acme", vec![]).unwrap();

    let people = db
        .ingest(
            "Person",
            vec![row(&[("id", "p1"), ("org_id", "acme")])],
            &IngestOptions::default(),
        )
        .unwrap();
    assert_eq!(
        people.rules_created,
        vec!["auto_fk_person_org_id".to_string()]
    );
    assert_eq!(
        db.neighbors("p1", "ORG", Direction::Out).unwrap(),
        vec!["acme".to_string()]
    );

    let devices = db
        .ingest(
            "Device",
            vec![row(&[("id", "d1"), ("org_id", "acme")])],
            &IngestOptions::default(),
        )
        .unwrap();
    assert_eq!(devices.inserted, 1);
    assert_eq!(
        devices.rules_created,
        vec!["auto_fk_device_org_id".to_string()]
    );
    assert!(devices.skipped_fk_fields.is_empty());

    let names: Vec<String> = db.rules().into_iter().map(|r| r.name).collect();
    assert_eq!(
        names,
        vec![
            "auto_fk_device_org_id".to_string(),
            "auto_fk_person_org_id".to_string(),
        ]
    );

    let device_fk = db
        .rules()
        .into_iter()
        .find(|r| r.name == "auto_fk_device_org_id")
        .unwrap();
    assert_eq!(device_fk.src_label, "Device");
    assert_eq!(device_fk.dst_label, "Org");
    assert_eq!(device_fk.edge_type, "ORG");

    assert_eq!(
        db.neighbors("d1", "ORG", Direction::Out).unwrap(),
        vec!["acme".to_string()],
        "Device.org_id must link; must not silently collide with Person.org_id"
    );
    let ex = db.explain("d1", "acme").unwrap();
    assert_eq!(ex.len(), 1);
    assert_eq!(ex[0].rule, "auto_fk_device_org_id");
}

/// WAL append that can be flipped to fail so ingest's single batch commit errors.
struct FailWalAppend {
    inner: RealFs,
    fail: Arc<AtomicBool>,
}

impl Fs for FailWalAppend {
    fn append(&mut self, file: FileId, data: &[u8]) -> std::io::Result<()> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(std::io::Error::other("forced ingest commit failure"));
        }
        self.inner.append(file, data)
    }

    fn sync(&mut self, file: FileId) -> std::io::Result<()> {
        self.inner.sync(file)
    }

    fn read(&self, file: FileId) -> std::io::Result<Vec<u8>> {
        self.inner.read(file)
    }

    fn write_atomic(&mut self, file: FileId, data: &[u8]) -> std::io::Result<()> {
        self.inner.write_atomic(file, data)
    }
}
