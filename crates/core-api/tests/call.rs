//! `CALL ns.name(args) [YIELD …]` (0.10 QL-12) is a read: every read entry
//! point answers it — the handle, `query_write`, an as-of handle and
//! `query_at`, a masked read, a role-scoped write, the lock-free reader and a
//! transaction — with the same rows, and the openCypher forms this release
//! refuses are named on all of them.

use mushroomdb::{
    schema::Schema, GraphError, RealFs, ResultSet, RoleDef, SharedDb, Value, WriteScope,
};
use std::collections::BTreeMap;
use std::path::PathBuf;

type GraphDb = mushroomdb::GraphDb<RealFs>;

fn tmp(name: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "graphdb-call-{name}-{}-{nanos}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn p() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

const PING: &str = "CALL db.ping() YIELD success AS ok RETURN ok";

/// The answer `PING` must give everywhere.
fn assert_ping(rs: Result<ResultSet, GraphError>, at: &str) {
    let rs = rs.unwrap_or_else(|e| panic!("{at}: {e}"));
    assert_eq!(rs.columns(), ["ok"], "{at}");
    assert_eq!(rs.len(), 1, "{at}");
    assert_eq!(rs.get(0, "ok"), Some(&Value::Bool(true)), "{at}");
}

fn role_store(dir: &std::path::Path) -> GraphDb {
    let mut db = GraphDb::open(dir).unwrap();
    db.apply_schema(&Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![],
        roles: vec![RoleDef {
            name: "writer".into(),
            keys: vec![],
            labels: vec!["P".into()],
            visible_where: None,
            namespaces: None,
            write: Some(WriteScope {
                create_labels: vec!["P".into()],
                update_labels: vec!["P".into()],
                delete_labels: vec!["P".into()],
                create_edge_types: vec![],
                delete_edge_types: vec![],
            }),
        }],
    })
    .unwrap();
    db.insert_node("P", "p1", vec![]).unwrap();
    db
}

#[test]
fn every_read_entry_point_runs_a_call() {
    let dir = tmp("entry");
    let mut db = role_store(&dir);
    assert_ping(db.query(PING, &p()), "query");
    assert_ping(db.query_write(PING, &p()), "query_write");
    let mask = db.mask_for_role("writer").unwrap();
    assert_ping(db.query_masked(PING, &p(), &mask), "query_masked");
    assert_ping(
        db.query_write_authz("writer", PING, &p()),
        "query_write_authz",
    );
    let snap = db.reader();
    assert_ping(snap.query(PING, &p()), "reader query");
    assert_ping(snap.query_masked(PING, &p(), &mask), "reader query_masked");
    let commit = db.commit_seq() - 1;
    assert_ping(db.query_at(commit, PING, &p()), "query_at");
    let past = GraphDb::open_at(&dir, commit).unwrap();
    assert!(past.is_read_only());
    assert_ping(past.query(PING, &p()), "as-of handle");
    drop(db);

    let shared = SharedDb::open(&tmp("tx")).unwrap();
    let mut tx = shared.begin_transaction();
    assert_ping(tx.execute(PING, &p()), "transaction");
    tx.execute("CREATE (n:P {id: 'x'})", &p()).unwrap();
    assert_ping(tx.execute(PING, &p()), "transaction after a write");
    tx.commit().unwrap();
    assert_ping(shared.reader().query(PING, &p()), "shared reader");
}

#[test]
fn a_call_writes_nothing() {
    let mut db = GraphDb::open(&tmp("nowrite")).unwrap();
    let before = db.commit_seq();
    db.query_write(PING, &p()).unwrap();
    db.query("CALL db.ping()", &p()).unwrap();
    assert_eq!(db.commit_seq(), before);
}

#[test]
fn call_errors_carry_their_stage() {
    let db = GraphDb::open(&tmp("errors")).unwrap();
    let err = db
        .query("CALL unknown.proc()", &p())
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("execute: unknown procedure: unknown.proc"),
        "{err}"
    );
    let err = db
        .query("CALL db.ping() YIELD nope", &p())
        .unwrap_err()
        .to_string();
    assert!(err.contains("`nope`"), "{err}");
    let err = db.query("CALL ping()", &p()).unwrap_err().to_string();
    assert!(err.contains("parse:"), "{err}");
    let p_x = BTreeMap::from([("x".to_string(), Value::Int(1))]);
    let err = db.query("CALL db.ping($x)", &p_x).unwrap_err().to_string();
    assert!(err.contains("db.ping takes no arguments, got 1"), "{err}");
}

#[test]
fn refused_forms_are_named_on_the_read_and_write_entry_points() {
    let mut db = GraphDb::open(&tmp("refused")).unwrap();
    let ns = "not supported in mushroomdb 0.10: ";
    for (q, want) in [
        ("CALL { MATCH (n) RETURN n }", "CALL subquery"),
        (
            "MATCH (n) CALL db.ping() RETURN n",
            "CALL inside a larger query",
        ),
        ("FOREACH (x IN [1,2] | CREATE (n:X {id:x}))", "FOREACH"),
        (
            "MATCH (n) WHERE EXISTS { (n)-[:K]->() } RETURN n",
            "EXISTS { } subquery",
        ),
    ] {
        let err = db.query(q, &p()).unwrap_err().to_string();
        assert!(err.contains(&format!("{ns}{want}")), "query {q}: {err}");
        let err = db.query_write(q, &p()).unwrap_err().to_string();
        assert!(
            err.contains(&format!("{ns}{want}")),
            "query_write {q}: {err}"
        );
    }
    for (q, want) in [
        ("MATCH (n) REMOVE n:Label", "REMOVE n:Label (label removal)"),
        ("MATCH (n) SET n:Label", "SET n:Label (label assignment)"),
        ("MATCH (n) FOREACH (x IN [1] | SET n.a = x)", "FOREACH"),
    ] {
        let err = db.query_write(q, &p()).unwrap_err().to_string();
        assert!(
            err.contains(&format!("{ns}{want}")),
            "query_write {q}: {err}"
        );
    }
}

#[test]
fn a_subscription_refuses_a_call_by_name() {
    let mut db = GraphDb::open(&tmp("sub")).unwrap();
    let err = db
        .subscribe_query("CALL db.ping()")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("not supported in mushroomdb 0.10: CALL here"),
        "{err}"
    );
}

#[test]
fn a_call_is_not_a_write_query() {
    for q in [
        "CALL db.ping()",
        "CALL db.ping() YIELD success RETURN success",
    ] {
        assert!(!mushroomdb::is_write_query(q).unwrap(), "{q}");
    }
}

// ------------------------------------------------------------ catalog procedures (TX-15)

/// `Public` nodes `p1`, `p2` in namespace `a` with `KNOWS` between them, and
/// `Secret` nodes with `OWNS`; an index and a constraint on each label, and a
/// role `scoped` that reads `Public` only. With `cross`, the one `Secret`
/// node `s1` is in namespace `a` too and `OWNS`/`LINKS` join it to `p1`; without
/// it, two `Secret` nodes in namespace `b` stand apart.
fn scoped_store(name: &str, cross: bool) -> GraphDb {
    let mut db = GraphDb::open(&tmp(name)).unwrap();
    let node = |db: &mut GraphDb, label: &str, key: &str, ns: &str, prop: (&str, Value)| {
        db.insert_node(
            label,
            key,
            vec![
                ("ns".into(), Value::Str(ns.into())),
                (prop.0.into(), prop.1),
            ],
        )
        .unwrap();
    };
    node(
        &mut db,
        "Public",
        "p1",
        "a",
        ("name", Value::Str("P".into())),
    );
    node(
        &mut db,
        "Public",
        "p2",
        "a",
        ("title", Value::Str("T".into())),
    );
    db.insert_edge("KNOWS", "p1", "p2").unwrap();
    if cross {
        node(&mut db, "Secret", "s1", "a", ("code", Value::Int(7)));
        db.insert_edge("OWNS", "s1", "p1").unwrap();
        db.insert_edge("LINKS", "p1", "s1").unwrap();
    } else {
        node(&mut db, "Secret", "s1", "b", ("code", Value::Int(7)));
        node(&mut db, "Secret", "s2", "b", ("code", Value::Int(8)));
        db.insert_edge("OWNS", "s1", "s2").unwrap();
    }
    for q in [
        "CREATE INDEX public_name FOR (n:Public) ON (n.name)",
        "CREATE INDEX secret_code FOR (n:Secret) ON (n.code)",
        "CREATE CONSTRAINT public_title FOR (n:Public) REQUIRE n.title IS UNIQUE",
        "CREATE CONSTRAINT secret_unique FOR (n:Secret) REQUIRE n.code IS UNIQUE",
    ] {
        db.query_write(q, &p()).unwrap();
    }
    db.apply_schema(&Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![],
        roles: vec![RoleDef {
            name: "scoped".into(),
            keys: vec![],
            labels: vec!["Public".into()],
            visible_where: None,
            namespaces: None,
            write: Some(WriteScope {
                create_labels: vec!["Public".into()],
                ..WriteScope::default()
            }),
        }],
    })
    .unwrap();
    db
}

fn names(rs: &ResultSet, column: &str) -> Vec<String> {
    (0..rs.len())
        .map(|i| match rs.get(i, column) {
            Some(Value::Str(s)) => s.clone(),
            other => panic!("{column} row {i}: {other:?}"),
        })
        .collect()
}

/// Every catalog procedure, asked through `ask`, lists only what the
/// `Public` / `KNOWS` / namespace-`a` scope holds.
fn assert_scoped(ask: &dyn Fn(&str) -> Result<ResultSet, GraphError>, at: &str) {
    let run = |q: &str| ask(q).unwrap_or_else(|e| panic!("{at}: {q}: {e}"));
    assert_eq!(names(&run("CALL db.labels()"), "label"), ["Public"], "{at}");
    assert_eq!(
        names(&run("CALL db.relationshipTypes()"), "relationshipType"),
        ["KNOWS"],
        "{at}"
    );
    let keys = names(&run("CALL db.propertyKeys()"), "propertyKey");
    assert!(
        !keys.contains(&"code".to_string()),
        "{at}: code in {keys:?}"
    );
    for shown in ["name", "title"] {
        assert!(
            keys.contains(&shown.to_string()),
            "{at}: {shown} in {keys:?}"
        );
    }
    assert_eq!(
        names(&run("CALL db.indexes()"), "name"),
        // The constraint's backing index is listed with the declared one.
        ["eq_Public_title", "public_name"],
        "{at}"
    );
    let constraints = run("CALL db.constraints()");
    assert_eq!(names(&constraints, "name"), ["public_title"], "{at}");
    let picture = run("CALL db.schema.visualization()");
    let text = format!("{:?}", picture.get(0, "nodes"))
        + &format!("{:?}", picture.get(0, "relationships"));
    for hidden in ["Secret", "OWNS", "LINKS", "secret"] {
        assert!(!text.contains(hidden), "{at}: {hidden} in {text}");
    }
    assert!(
        text.contains("Public") && text.contains("KNOWS"),
        "{at}: {text}"
    );
}

#[test]
fn the_catalog_procedures_list_everything_to_an_unscoped_caller() {
    let db = scoped_store("catalog-full", true);
    let rs = db.query("CALL db.labels()", &p()).unwrap();
    assert_eq!(names(&rs, "label"), ["Public", "Secret"]);
    let rs = db.query("CALL db.relationshipTypes()", &p()).unwrap();
    assert_eq!(names(&rs, "relationshipType"), ["KNOWS", "LINKS", "OWNS"]);
    let keys = names(
        &db.query("CALL db.propertyKeys()", &p()).unwrap(),
        "propertyKey",
    );
    for key in ["code", "name", "title"] {
        assert!(keys.contains(&key.to_string()), "{key} in {keys:?}");
    }
    let rs = db.query("CALL db.indexes()", &p()).unwrap();
    assert_eq!(
        rs.columns(),
        ["name", "type", "labelsOrTypes", "properties", "state"]
    );
    let rs = db.query("CALL db.constraints()", &p()).unwrap();
    assert_eq!(names(&rs, "name"), ["public_title", "secret_unique"]);
    let rs = db.query("CALL db.schema.visualization()", &p()).unwrap();
    let rels = format!("{:?}", rs.get(0, "relationships"));
    for want in ["KNOWS", "LINKS", "OWNS"] {
        assert!(rels.contains(want), "{want} in {rels}");
    }
    // One relationship per (type, from, to) present: OWNS runs Secret to
    // Public only.
    let Some(Value::List(rels)) = rs.get(0, "relationships") else {
        panic!("relationships is a list")
    };
    assert_eq!(rels.len(), 3, "{rels:?}");
    let owns = rels
        .iter()
        .find_map(|r| match r {
            Value::Map(m) if m.get("type") == Some(&Value::Str("OWNS".into())) => Some(m),
            _ => None,
        })
        .expect("OWNS listed");
    assert_eq!(owns.get("from"), Some(&Value::Str("Secret".into())));
    assert_eq!(owns.get("to"), Some(&Value::Str("Public".into())));
    let rs = db
        .query(
            "CALL dbms.components() YIELD name, edition RETURN name, edition",
            &p(),
        )
        .unwrap();
    assert_eq!(rs.get(0, "name"), Some(&Value::Str("mushroomdb".into())));
}

#[test]
fn a_role_sees_only_its_own_schema_on_every_graph_db_entry_point() {
    let mut db = scoped_store("catalog-role", true);
    let mask = db.mask_for_role("scoped").unwrap();
    assert_scoped(&|q| db.query_masked(q, &p(), &mask), "query_masked(role)");
    let snap = db.reader();
    assert_scoped(
        &|q| snap.query_masked(q, &p(), &mask),
        "reader query_masked(role)",
    );
    let rmask = snap.mask_for_role("scoped").unwrap();
    assert_scoped(&|q| snap.query_masked(q, &p(), &rmask), "reader mask(role)");
    drop(snap);
    let calls: Vec<_> = [
        "CALL db.labels()",
        "CALL db.relationshipTypes()",
        "CALL db.propertyKeys()",
        "CALL db.indexes()",
        "CALL db.constraints()",
        "CALL db.schema.visualization()",
    ]
    .into_iter()
    .map(|q| (q, db.query_write_authz("scoped", q, &p()).unwrap()))
    .collect();
    let answers: BTreeMap<&str, ResultSet> = calls.into_iter().collect();
    assert_scoped(
        &|q| Ok(answers.get(q).unwrap().clone()),
        "query_write_authz(role)",
    );
}

#[test]
fn a_mask_and_a_namespace_see_only_their_own_schema() {
    let db = scoped_store("catalog-mask", false);
    let by_key = mushroomdb::NodeMask::from_keys(&db, ["p1", "p2"]);
    assert_scoped(&|q| db.query_masked(q, &p(), &by_key), "key mask");
    let by_ns = db.mask_for_namespace("a");
    assert_scoped(&|q| db.query_masked(q, &p(), &by_ns), "namespace mask");
    let snap = db.reader();
    let reader_ns = snap.mask_for_namespace("a").unwrap();
    assert_scoped(
        &|q| snap.query_masked(q, &p(), &reader_ns),
        "reader namespace",
    );
}

#[test]
fn a_mask_that_hides_everything_lists_nothing() {
    let db = scoped_store("catalog-empty", true);
    let none = mushroomdb::NodeMask::from_keys(&db, Vec::<&str>::new());
    for q in [
        "CALL db.labels()",
        "CALL db.relationshipTypes()",
        "CALL db.propertyKeys()",
        "CALL db.indexes()",
        "CALL db.constraints()",
    ] {
        assert_eq!(db.query_masked(q, &p(), &none).unwrap().len(), 0, "{q}");
    }
}

#[test]
fn the_catalog_procedures_run_inside_a_transaction_and_see_its_writes() {
    let shared = SharedDb::open(&tmp("catalog-tx")).unwrap();
    let mut tx = shared.begin_transaction();
    tx.execute("CREATE (n:Fresh {id: 'x', tag: 1})", &p())
        .unwrap();
    let rs = tx.execute("CALL db.labels()", &p()).unwrap();
    assert_eq!(names(&rs, "label"), ["Fresh"]);
    tx.rollback();
    let rs = shared.reader().query("CALL db.labels()", &p()).unwrap();
    assert_eq!(rs.len(), 0);
}
