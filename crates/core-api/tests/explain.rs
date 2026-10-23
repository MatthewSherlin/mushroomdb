//! `EXPLAIN` (0.10 QL-11): the plan of a statement as one JSON string in a
//! one-row `plan` column, without running the statement. Every read entry
//! point answers it; it writes nothing; `PROFILE` is refused by name; a plan
//! names the index a lookup reads, and under a scope only the indexes the
//! caller could list.

use mushroomdb::{
    explain_plan_value, is_explain_query, schema::Schema, GraphError, RealFs, ResultSet, RoleDef,
    SharedDb, Value, WriteScope,
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
        "graphdb-explain-{name}-{}-{nanos}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn p() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

/// The plan an `EXPLAIN` result holds, parsed: one row, one column `plan`, a
/// JSON array.
fn plan_of(rs: Result<ResultSet, GraphError>, at: &str) -> serde_json::Value {
    let rs = rs.unwrap_or_else(|e| panic!("{at}: {e}"));
    assert_eq!(rs.columns(), ["plan"], "{at}");
    assert_eq!(rs.len(), 1, "{at}");
    let Some(Value::Str(json)) = rs.get(0, "plan") else {
        panic!("{at}: plan is not a string");
    };
    serde_json::from_str(json).unwrap_or_else(|e| panic!("{at}: not JSON ({e}): {json}"))
}

fn ops(plan: &serde_json::Value) -> Vec<&str> {
    plan.as_array()
        .expect("array")
        .iter()
        .map(|o| o["op"].as_str().expect("op"))
        .collect()
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
                update_edge_types: vec![],
            }),
        }],
    })
    .unwrap();
    db.insert_node("P", "p1", vec![("age".into(), Value::Int(3))])
        .unwrap();
    db.insert_node("Secret", "s1", vec![("level".into(), Value::Int(9))])
        .unwrap();
    db.enable_range_index("idx_secret_level", "Secret", "level")
        .unwrap();
    db.enable_range_index("idx_p_age", "P", "age").unwrap();
    db
}

const READ: &str = "EXPLAIN MATCH (n:P) RETURN n.age";

#[test]
fn a_read_returns_a_one_row_plan_column() {
    let mut db = GraphDb::open(&tmp("read")).unwrap();
    db.insert_node("Person", "a", vec![]).unwrap();
    let plan = plan_of(
        db.query("EXPLAIN MATCH (n:Person) RETURN n.name", &p()),
        "query",
    );
    assert_eq!(ops(&plan), ["ScanLabel", "Project"]);
    assert_eq!(plan[0]["label"], "Person");
    let count = plan_of(db.query("explain MATCH (n) RETURN count(n)", &p()), "count");
    assert!(ops(&count).contains(&"Aggregate"));
}

#[test]
fn explain_never_writes_whatever_the_statement() {
    let mut db = GraphDb::open(&tmp("nowrite")).unwrap();
    db.insert_node("P", "keep", vec![("v".into(), Value::Int(1))])
        .unwrap();
    let before_seq = db.commit_seq();
    for q in [
        "EXPLAIN CREATE (n:X {id: 'y'})",
        "EXPLAIN MATCH (n:P) SET n.v = 2",
        "EXPLAIN MATCH (n:P) DETACH DELETE n",
        "EXPLAIN MERGE (n:P {id: 'm'})",
        "EXPLAIN UNWIND [1, 2] AS x CREATE (n:Q {id: 'q'})",
        "EXPLAIN CREATE INDEX FOR (n:P) ON (n.v)",
        "EXPLAIN DROP INDEX nope",
    ] {
        let write_plan = plan_of(db.query_write(q, &p()), q);
        assert!(!ops(&write_plan).is_empty(), "{q}");
        // The read entry point answers it too, with the same plan.
        assert_eq!(plan_of(db.query(q, &p()), q), write_plan, "{q}");
    }
    assert_eq!(db.commit_seq(), before_seq, "no commit");
    assert_eq!(db.node_count(), 1);
    let rs = db.query("MATCH (n:P) RETURN n.v", &p()).unwrap();
    assert_eq!(rs.get(0, "n.v"), Some(&Value::Int(1)));
    let create = plan_of(db.query("EXPLAIN CREATE (n:X {id: 'y'})", &p()), "create");
    assert_eq!(ops(&create), ["Create"]);
    let schema = plan_of(
        db.query("EXPLAIN CREATE INDEX FOR (n:P) ON (n.v)", &p()),
        "ddl",
    );
    assert_eq!(ops(&schema), ["Schema"]);
}

#[test]
fn a_plan_names_the_index_a_lookup_reads() {
    let mut db = GraphDb::open(&tmp("index")).unwrap();
    db.insert_node("P", "p1", vec![("age".into(), Value::Int(3))])
        .unwrap();
    let eq = "EXPLAIN MATCH (n:P {name: 'a'}) RETURN n";
    let range = "EXPLAIN MATCH (n:P) WHERE n.age > 2 RETURN n";
    let none = plan_of(db.query(eq, &p()), "eq without an index");
    assert_eq!(none[0]["op"], "IndexScan");
    assert!(none[0]["index"].is_null(), "{none}");
    let none = plan_of(db.query(range, &p()), "range without an index");
    assert_eq!(none[0]["op"], "IndexRangeScan");
    assert!(none[0]["index"].is_null(), "{none}");

    db.enable_index("P", "name").unwrap();
    db.enable_range_index("idx_p_age", "P", "age").unwrap();
    let eq_plan = plan_of(db.query(eq, &p()), "eq");
    assert_eq!(eq_plan[0]["index"], "eq_P_name");
    let range_plan = plan_of(db.query(range, &p()), "range");
    assert_eq!(range_plan[0]["index"], "idx_p_age");
    assert_eq!(range_plan[0]["lo"]["inclusive"], false);
    let key = plan_of(
        db.query("EXPLAIN MATCH (n:P {id: 'p1'}) RETURN n", &p()),
        "key",
    );
    assert_eq!(key[0]["index"], "key");
    let fused = plan_of(
        db.query("EXPLAIN MATCH (n:P) WHERE n.age <> 3 RETURN n", &p()),
        "fused",
    );
    assert_eq!(fused[0]["op"], "ScanLabel");
    assert_eq!(fused[0]["fused_filter"], "age");
}

#[test]
fn profile_is_refused_by_name() {
    let mut db = GraphDb::open(&tmp("profile")).unwrap();
    for e in [
        db.query("PROFILE MATCH (n) RETURN n", &p()).unwrap_err(),
        db.query_write("PROFILE CREATE (n:X {id: 'y'})", &p())
            .unwrap_err(),
    ] {
        let text = e.to_string();
        assert!(
            text.contains("not supported in mushroomdb 0.10: PROFILE"),
            "{text}"
        );
    }
    assert_eq!(db.node_count(), 0);
}

#[test]
fn a_bad_statement_after_explain_names_its_stage() {
    let db = GraphDb::open(&tmp("bad")).unwrap();
    let err = db
        .query("EXPLAIN MATCH (n RETURN n", &p())
        .unwrap_err()
        .to_string();
    assert!(err.contains("parse:"), "{err}");
    let err = db
        .query("EXPLAIN MATCH (n) RETURN m", &p())
        .unwrap_err()
        .to_string();
    assert!(err.contains("plan:"), "{err}");
}

#[test]
fn explain_stays_an_ordinary_name_elsewhere() {
    let mut db = GraphDb::open(&tmp("name")).unwrap();
    db.insert_node("Explain", "e", vec![("explain".into(), Value::Int(1))])
        .unwrap();
    let rs = db
        .query("MATCH (explain:Explain) RETURN explain.explain", &p())
        .unwrap();
    assert_eq!(rs.get(0, "explain.explain"), Some(&Value::Int(1)));
}

#[test]
fn every_read_entry_point_answers_explain() {
    let dir = tmp("entry");
    let mut db = role_store(&dir);
    let want = plan_of(db.query(READ, &p()), "query");
    assert_eq!(want[0]["index"], serde_json::Value::Null);
    assert_eq!(plan_of(db.query_write(READ, &p()), "query_write"), want);
    let mask = db.mask_for_role("writer").unwrap();
    assert_eq!(
        plan_of(db.query_masked(READ, &p(), &mask), "query_masked"),
        want
    );
    assert_eq!(
        plan_of(
            db.query_write_authz("writer", READ, &p()),
            "query_write_authz"
        ),
        want
    );
    let snap = db.reader();
    assert_eq!(plan_of(snap.query(READ, &p()), "reader"), want);
    assert_eq!(
        plan_of(snap.query_masked(READ, &p(), &mask), "reader masked"),
        want
    );
    let commit = db.commit_seq() - 1;
    assert_eq!(plan_of(db.query_at(commit, READ, &p()), "query_at"), want);
    drop(db);

    let shared = SharedDb::open(&tmp("tx")).unwrap();
    let mut tx = shared.begin_transaction();
    assert_eq!(plan_of(tx.execute(READ, &p()), "transaction"), want);
    // Nothing the transaction planned was buffered.
    plan_of(
        tx.execute("EXPLAIN CREATE (n:P {id: 'x'})", &p()),
        "tx create",
    );
    tx.commit().unwrap();
    assert_eq!(
        shared
            .reader()
            .query("MATCH (n) RETURN n", &p())
            .unwrap()
            .len(),
        0
    );
}

#[test]
fn a_scoped_caller_sees_only_the_indexes_it_could_list() {
    let mut db = role_store(&tmp("scoped"));
    let q = "EXPLAIN MATCH (n:Secret) WHERE n.level > 1 RETURN n";
    let open = plan_of(db.query(q, &p()), "unscoped");
    assert_eq!(open[0]["index"], "idx_secret_level");
    let mask = db.mask_for_role("writer").unwrap();
    for (at, plan) in [
        (
            "query_masked",
            plan_of(db.query_masked(q, &p(), &mask), "m"),
        ),
        (
            "authz",
            plan_of(db.query_write_authz("writer", q, &p()), "a"),
        ),
        (
            "reader",
            plan_of(db.reader().query_masked(q, &p(), &mask), "r"),
        ),
    ] {
        assert_eq!(plan[0]["op"], "IndexRangeScan", "{at}");
        assert!(plan[0]["index"].is_null(), "{at}: hidden index named");
    }
    // The label the role can see still names its index.
    let own = plan_of(
        db.query_masked("EXPLAIN MATCH (n:P) WHERE n.age > 1 RETURN n", &p(), &mask),
        "own",
    );
    assert_eq!(own[0]["index"], "idx_p_age");
}

#[test]
fn the_bolt_summary_plan_is_a_tree_of_operators() {
    assert!(is_explain_query("EXPLAIN MATCH (n) RETURN n").unwrap());
    assert!(is_explain_query("explain CREATE (n:X {id: 'y'})").unwrap());
    assert!(!is_explain_query("MATCH (n) RETURN n").unwrap());
    assert!(!is_explain_query("PROFILE MATCH (n) RETURN n").unwrap());

    let db = GraphDb::open(&tmp("tree")).unwrap();
    let rs = db
        .query("EXPLAIN MATCH (n:P)-[r:KNOWS]->(m) RETURN m.name", &p())
        .unwrap();
    let Some(Value::Map(root)) = explain_plan_value(&rs) else {
        panic!("plan value");
    };
    assert_eq!(root["operatorType"], Value::Str("Project".into()));
    let Value::List(children) = &root["children"] else {
        panic!("children");
    };
    let Value::Map(expand) = &children[0] else {
        panic!("child");
    };
    assert_eq!(expand["operatorType"], Value::Str("Expand".into()));
    let Value::List(ids) = &expand["identifiers"] else {
        panic!("identifiers");
    };
    assert!(ids.contains(&Value::Str("m".into())));
    let ordinary = db.query("MATCH (n) RETURN n", &p()).unwrap();
    assert!(explain_plan_value(&ordinary).is_none());
}
