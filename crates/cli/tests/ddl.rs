//! `mushroomdb query` runs Cypher DDL (0.10 TY-9): a schema statement is a
//! write with the store's full access, and a `--namespace` read refuses it.

use std::path::PathBuf;
use std::process::{Command, Output};

use mushroomdb::GraphDb;

fn temp_db(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mdb-cli-ddl-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join("db")
}

fn query(db: &std::path::Path, extra: &[&str], cypher: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
        .arg("query")
        .arg(db)
        .args(extra)
        .arg(cypher)
        .output()
        .expect("run mushroomdb")
}

fn ok(out: &Output) -> String {
    assert!(
        out.status.success(),
        "failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn query_runs_ddl_and_shows_it() {
    let db = temp_db("full");
    drop(GraphDb::open(&db).expect("create"));
    ok(&query(&db, &[], "CREATE INDEX i FOR (n:P) ON (n.a)"));
    ok(&query(
        &db,
        &[],
        "CREATE CONSTRAINT c FOR (n:P) REQUIRE n.b IS UNIQUE",
    ));
    let shown = ok(&query(&db, &[], "SHOW INDEXES"));
    assert!(
        shown.contains("range") && shown.contains("eq_P_b"),
        "{shown}"
    );
    let shown = ok(&query(&db, &[], "SHOW CONSTRAINTS"));
    assert!(shown.contains("unique"), "{shown}");
    let reopened = GraphDb::open(&db).expect("open");
    assert_eq!(reopened.range_indexes().len(), 1);
    assert_eq!(reopened.constraints().len(), 1);
}

#[test]
fn a_namespaced_query_refuses_ddl() {
    let db = temp_db("ns");
    drop(GraphDb::open(&db).expect("create"));
    let out = query(
        &db,
        &["--namespace", "default"],
        "CREATE INDEX i FOR (n:P) ON (n.a)",
    );
    assert!(!out.status.success());
    assert!(GraphDb::open(&db).expect("open").range_indexes().is_empty());
}
