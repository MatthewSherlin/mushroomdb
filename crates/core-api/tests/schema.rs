//! Tests for `GraphDb::apply_schema` — idempotent schema-as-code.
//!
//! Covers: first-apply creates all items; second-apply with identical schema is
//! all-unchanged AND emits zero new WAL commits; updating a rule definition
//! triggers an update entry.

use core_api::schema::Schema;
use core_api::{Direction, GraphDb, Predicate, RuleDef, ViewDef, ViewSource};

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "graphdb-schema-{}-{}-{}",
        name,
        std::process::id(),
        nanos
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn sample_rule(name: &str) -> RuleDef {
    RuleDef {
        name: name.into(),
        src_label: "A".into(),
        dst_label: "B".into(),
        predicate: Predicate::FieldEqual {
            field: "key".into(),
        },
        edge_type: "REL".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    }
}

fn sample_view(name: &str) -> ViewDef {
    ViewDef {
        name: name.into(),
        label: "A".into(),
        view_prop: format!("{name}_prop"),
        source: ViewSource::Degree {
            edge_type: "REL".into(),
            direction: Direction::Out,
        },
    }
}

#[test]
fn apply_schema_is_idempotent_and_diffs() {
    let dir = tmp("idempotent");
    let mut db = GraphDb::open(&dir).unwrap();

    // Schema: 1 fulltext pair, 1 rule, 1 view.
    let schema = Schema {
        fulltext: vec![("A".into(), "body".into())],
        indexes: vec![],
        rules: vec![sample_rule("rel_rule")],
        views: vec![sample_view("deg_view")],
        roles: vec![],
    };

    // First apply: everything is created.
    let d1 = db.apply_schema(&schema).unwrap();
    assert_eq!(
        d1.created.len(),
        3,
        "first apply must create 3 items: {d1:?}"
    );
    assert!(d1.updated.is_empty(), "no updates on first apply: {d1:?}");
    assert!(
        d1.unchanged.is_empty(),
        "no unchanged on first apply: {d1:?}"
    );

    // Entry names must follow the spec.
    assert!(
        d1.created.contains(&"fulltext:A.body".to_string()),
        "fulltext entry: {d1:?}"
    );
    assert!(
        d1.created.contains(&"view:deg_view".to_string()),
        "view entry: {d1:?}"
    );
    assert!(
        d1.created.contains(&"rule:rel_rule".to_string()),
        "rule entry: {d1:?}"
    );

    // Record WAL commits before second apply. Drop and reopen to flush.
    drop(db);
    let commits_before = core_api::wal_commit_count_at(&dir).unwrap();

    let mut db = GraphDb::open(&dir).unwrap();

    // Second apply with identical schema: all unchanged, zero new WAL commits.
    let d2 = db.apply_schema(&schema).unwrap();
    assert_eq!(
        d2.unchanged.len(),
        3,
        "second apply must be all-unchanged: {d2:?}"
    );
    assert!(d2.created.is_empty(), "no creates on re-apply: {d2:?}");
    assert!(d2.updated.is_empty(), "no updates on re-apply: {d2:?}");

    // Zero WAL writes: drop and reopen to flush, then count.
    drop(db);
    let commits_after = core_api::wal_commit_count_at(&dir).unwrap();
    assert_eq!(
        commits_after, commits_before,
        "unchanged apply must emit zero WAL commits (before={commits_before}, after={commits_after})"
    );
}

#[test]
fn apply_schema_update_replaces_changed_rule() {
    let dir = tmp("update_rule");
    let mut db = GraphDb::open(&dir).unwrap();

    let schema_v1 = Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![sample_rule("link")],
        views: vec![sample_view("outdeg")],
        roles: vec![],
    };
    let d1 = db.apply_schema(&schema_v1).unwrap();
    assert_eq!(d1.created.len(), 2);

    // Change the rule's edge_type — this should trigger an update.
    let mut changed_rule = sample_rule("link");
    changed_rule.edge_type = "CHANGED".into();

    let schema_v2 = Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![changed_rule],
        views: vec![sample_view("outdeg")], // view unchanged
        roles: vec![],
    };
    let d2 = db.apply_schema(&schema_v2).unwrap();
    assert!(
        d2.updated.contains(&"rule:link".to_string()),
        "changed rule must be in updated: {d2:?}"
    );
    assert!(
        d2.unchanged.contains(&"view:outdeg".to_string()),
        "unchanged view stays unchanged: {d2:?}"
    );
    assert!(d2.created.is_empty(), "nothing new created: {d2:?}");

    // The new edge_type must be live.
    assert!(
        db.rules()
            .iter()
            .any(|r| r.name == "link" && r.edge_type == "CHANGED"),
        "updated rule must reflect new edge_type"
    );
}

#[test]
fn apply_schema_no_pruning() {
    // Items in the DB but absent from the schema are left untouched.
    let dir = tmp("no_prune");
    let mut db = GraphDb::open(&dir).unwrap();

    // Manually create a rule outside the schema.
    db.create_rule(sample_rule("orphan")).unwrap();

    let schema = Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![],
        roles: vec![],
    };
    let d = db.apply_schema(&schema).unwrap();
    assert!(d.created.is_empty());
    assert!(d.updated.is_empty());
    assert!(d.unchanged.is_empty());

    // Orphan rule must still exist.
    assert!(
        db.rules().iter().any(|r| r.name == "orphan"),
        "orphan rule must not be pruned"
    );
}

#[test]
fn apply_schema_view_update() {
    // Changing a view's source triggers an update.
    let dir = tmp("update_view");
    let mut db = GraphDb::open(&dir).unwrap();

    let schema_v1 = Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![sample_view("degview")],
        roles: vec![],
    };
    db.apply_schema(&schema_v1).unwrap();

    // Change the direction.
    let changed_view = ViewDef {
        name: "degview".into(),
        label: "A".into(),
        view_prop: "degview_prop".into(),
        source: ViewSource::Degree {
            edge_type: "REL".into(),
            direction: Direction::In,
        },
    };
    let schema_v2 = Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![changed_view],
        roles: vec![],
    };
    let d = db.apply_schema(&schema_v2).unwrap();
    assert!(
        d.updated.contains(&"view:degview".to_string()),
        "changed view must be updated: {d:?}"
    );
}

#[test]
fn apply_schema_invalid_rule_update_is_rejected_before_mutation() {
    // A changed rule whose new definition fails validate() must: (a) return Err,
    // (b) leave the original rule alive and unchanged, (c) emit zero new WAL commits.
    let dir = tmp("invalid_rule_update");
    let mut db = GraphDb::open(&dir).unwrap();

    // Seed with a valid rule.
    let schema_v1 = Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![sample_rule("link")],
        views: vec![],
        roles: vec![],
    };
    db.apply_schema(&schema_v1).unwrap();

    drop(db);
    let commits_before = core_api::wal_commit_count_at(&dir).unwrap();
    let mut db = GraphDb::open(&dir).unwrap();

    // Build a changed rule with an invalid definition (empty edge_type fails validate()).
    let mut bad_rule = sample_rule("link");
    bad_rule.edge_type = String::new(); // validate() rejects empty edge_type

    let schema_bad = Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![bad_rule],
        views: vec![],
        roles: vec![],
    };
    let result = db.apply_schema(&schema_bad);
    assert!(result.is_err(), "invalid update must return Err");

    // Original rule must still be live and unchanged.
    let rules = db.rules();
    let live = rules
        .iter()
        .find(|r| r.name == "link")
        .expect("original rule must still exist");
    assert_eq!(
        live.edge_type, "REL",
        "original rule edge_type must be unchanged"
    );

    // Zero new WAL commits — no mutation was made.
    drop(db);
    let commits_after = core_api::wal_commit_count_at(&dir).unwrap();
    assert_eq!(
        commits_after, commits_before,
        "invalid update must not emit any WAL commits (before={commits_before}, after={commits_after})"
    );
}

#[test]
fn apply_schema_invalid_view_update_is_rejected_before_mutation() {
    // A changed view whose new definition fails validate() must: (a) return Err,
    // (b) leave the original view alive and unchanged, (c) emit zero new WAL commits.
    let dir = tmp("invalid_view_update");
    let mut db = GraphDb::open(&dir).unwrap();

    // Seed with a valid view.
    let schema_v1 = Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![sample_view("degview")],
        roles: vec![],
    };
    db.apply_schema(&schema_v1).unwrap();

    drop(db);
    let commits_before = core_api::wal_commit_count_at(&dir).unwrap();
    let mut db = GraphDb::open(&dir).unwrap();

    // Build a changed view with an invalid definition (empty name fails validate()).
    let bad_view = ViewDef {
        name: "degview".into(),
        label: String::new(), // validate() rejects empty label
        view_prop: "degview_prop".into(),
        source: ViewSource::Degree {
            edge_type: "REL".into(),
            direction: Direction::Out,
        },
    };
    let schema_bad = Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![bad_view],
        roles: vec![],
    };
    let result = db.apply_schema(&schema_bad);
    assert!(result.is_err(), "invalid view update must return Err");

    // Original view must still be live.
    let views = db.views();
    let live = views
        .iter()
        .find(|v| v.name == "degview")
        .expect("original view must still exist");
    assert_eq!(live.label, "A", "original view label must be unchanged");

    // Zero new WAL commits.
    drop(db);
    let commits_after = core_api::wal_commit_count_at(&dir).unwrap();
    assert_eq!(
        commits_after, commits_before,
        "invalid view update must not emit any WAL commits (before={commits_before}, after={commits_after})"
    );
}

#[test]
fn schema_json_round_trips() {
    // Schema is serde-JSON round-trippable.
    let schema = Schema {
        fulltext: vec![("Label".into(), "field".into())],
        indexes: vec![],
        rules: vec![sample_rule("r")],
        views: vec![sample_view("v")],
        roles: vec![],
    };
    let json = serde_json::to_string(&schema).unwrap();
    let back: Schema = serde_json::from_str(&json).unwrap();
    assert_eq!(back.fulltext, schema.fulltext);
    assert_eq!(back.rules.len(), 1);
    assert_eq!(back.views.len(), 1);
}

// ---------------------------------------------------------------------------
// Item 16: duplicate names within one schema are rejected pre-validation
// ---------------------------------------------------------------------------

#[test]
fn apply_schema_rejects_duplicate_rule_names() {
    let dir = tmp("schema-dup-rules");
    let mut db = GraphDb::open(&dir).unwrap();

    let schema = Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![sample_rule("my_rule"), sample_rule("my_rule")],
        views: vec![],
        roles: vec![],
    };
    let result = db.apply_schema(&schema);
    assert!(
        result.is_err(),
        "duplicate rule names within one schema must be rejected"
    );
    // Database must not have been mutated (no rule created).
    assert!(db.rules().is_empty(), "no rule created on error");
}

#[test]
fn apply_schema_rejects_duplicate_view_names() {
    let dir = tmp("schema-dup-views");
    let mut db = GraphDb::open(&dir).unwrap();

    let schema = Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![sample_view("my_view"), sample_view("my_view")],
        roles: vec![],
    };
    let result = db.apply_schema(&schema);
    assert!(
        result.is_err(),
        "duplicate view names within one schema must be rejected"
    );
    assert!(db.views().is_empty(), "no view created on error");
}
