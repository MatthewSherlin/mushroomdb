//! `mushroomdb schema indexes|constraints <db-dir>` (0.10 TY-11): the tables
//! `SHOW INDEXES` / `SHOW CONSTRAINTS` give, read from a store opened
//! read-only — so they work while a server holds it.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use mushroomdb::{GraphDb, SharedDb};

fn temp_db(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mdb-cli-schema-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join("db")
}

fn schema(db: &Path, what: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
        .args(["schema", what])
        .arg(db)
        .output()
        .expect("run mushroomdb")
}

fn stdout_of(out: &Output) -> String {
    assert!(
        out.status.success(),
        "failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn seed(db: &Path) {
    let mut g = GraphDb::open(db).expect("create");
    g.schema_create_range_index(None, "Person", "age").unwrap();
    g.schema_create_constraint("uc_email", "User", "email")
        .unwrap();
}

#[test]
fn indexes_prints_the_show_table() {
    let db = temp_db("idx");
    seed(&db);
    let out = stdout_of(&schema(&db, "indexes"));
    let lines: Vec<Vec<&str>> = out
        .lines()
        .map(|l| l.split('|').map(str::trim).collect())
        .collect();
    assert_eq!(lines[0], ["name", "type", "label", "property", "state"]);
    assert_eq!(
        lines[1],
        ["eq_User_email", "equality", "User", "email", "online"]
    );
    assert_eq!(
        lines[2],
        ["idx_Person_age", "range", "Person", "age", "online"]
    );
    assert_eq!(lines.len(), 3, "{out}");
}

#[test]
fn constraints_prints_the_show_table() {
    let db = temp_db("con");
    seed(&db);
    let out = stdout_of(&schema(&db, "constraints"));
    let lines: Vec<Vec<&str>> = out
        .lines()
        .map(|l| l.split('|').map(str::trim).collect())
        .collect();
    assert_eq!(lines[0], ["name", "type", "label", "property"]);
    assert_eq!(lines[1], ["uc_email", "unique", "User", "email"]);
    assert_eq!(lines.len(), 2, "{out}");
}

#[test]
fn an_empty_schema_prints_the_header_alone() {
    let db = temp_db("empty");
    drop(GraphDb::open(&db).expect("create"));
    let out = stdout_of(&schema(&db, "indexes"));
    assert_eq!(out.lines().count(), 1, "{out}");
}

#[test]
fn it_works_while_a_server_handle_holds_the_store() {
    let db = temp_db("held");
    seed(&db);
    let held = SharedDb::open(&db).expect("server handle");
    let out = stdout_of(&schema(&db, "indexes"));
    assert!(out.contains("idx_Person_age"), "{out}");
    let out = stdout_of(&schema(&db, "constraints"));
    assert!(out.contains("uc_email"), "{out}");
    // It changed nothing: the holder still sees one range index.
    assert_eq!(held.read().range_indexes().len(), 1);
}

#[test]
fn bad_usage_is_an_error() {
    let out = Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
        .args(["schema", "indexes"])
        .output()
        .expect("run");
    assert!(!out.status.success());
    let out = Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
        .args(["schema", "nonsense", "x"])
        .output()
        .expect("run");
    assert!(!out.status.success());
}
