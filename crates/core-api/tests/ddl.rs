//! Cypher DDL (0.10 TY-9): `CREATE INDEX`, `DROP INDEX`, `SHOW INDEXES`,
//! `CREATE CONSTRAINT`, `DROP CONSTRAINT`, `SHOW CONSTRAINTS`.
//!
//! A DDL statement goes through the same API calls the Rust surface has
//! (`enable_range_index`, `create_constraint`, …), so it is a write: it runs
//! through `query_write` and is refused wherever a write is refused — a
//! transaction, an as-of handle, a masked or role-scoped query. `SHOW …` is a
//! read and answers wherever a read does, the lock-free reader included.

use mushroomdb::{
    schema::Schema, GraphError, RealFs, ResultSet, RoleDef, SharedDb, Value, WriteScope,
};
use std::collections::BTreeMap;
use std::path::PathBuf;

type GraphDb = mushroomdb::GraphDb<RealFs>;

/// A temporary directory removed on drop, so every test path — pass or fail — is
/// cleaned up when the guard goes out of scope.
struct TmpDir(PathBuf);

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl std::ops::Deref for TmpDir {
    type Target = std::path::Path;
    fn deref(&self) -> &std::path::Path {
        &self.0
    }
}

impl AsRef<std::path::Path> for TmpDir {
    fn as_ref(&self) -> &std::path::Path {
        &self.0
    }
}

/// A GraphDb together with its temporary directory; the directory is removed
/// when this value is dropped.
struct TmpDb {
    db: GraphDb,
    _dir: TmpDir,
}

impl std::ops::Deref for TmpDb {
    type Target = GraphDb;
    fn deref(&self) -> &GraphDb {
        &self.db
    }
}

impl std::ops::DerefMut for TmpDb {
    fn deref_mut(&mut self) -> &mut GraphDb {
        &mut self.db
    }
}

/// A fresh directory per call. The clock alone is not enough: on macOS it
/// ticks in microseconds, so two tests running in parallel could get the same
/// name and share a store; the counter keeps every name in the process unique.
fn tmp(name: &str) -> TmpDir {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "graphdb-ddl-{name}-{}-{nanos}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    TmpDir(d)
}

fn open_temp() -> TmpDb {
    let _dir = tmp("db");
    let db = GraphDb::open(&_dir).unwrap();
    TmpDb { db, _dir }
}

fn p() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

fn ddl(db: &mut GraphDb, q: &str) -> ResultSet {
    db.query_write(q, &p())
        .unwrap_or_else(|e| panic!("{q}: {e}"))
}

fn ddl_err(db: &mut GraphDb, q: &str) -> String {
    match db.query_write(q, &p()) {
        Ok(_) => panic!("{q}: expected an error"),
        Err(e) => e.to_string(),
    }
}

/// Every row of `rs` as `column -> text`, null as `""`.
fn rows(rs: &ResultSet) -> Vec<BTreeMap<String, String>> {
    (0..rs.len())
        .map(|i| {
            rs.columns()
                .iter()
                .map(|c| {
                    let text = match rs.get(i, c) {
                        Some(Value::Str(s)) => s.clone(),
                        Some(v) => format!("{v:?}"),
                        None => String::new(),
                    };
                    (c.clone(), text)
                })
                .collect()
        })
        .collect()
}

fn row(name: &str, kind: &str, label: &str, property: &str) -> BTreeMap<String, String> {
    let mut r = BTreeMap::new();
    r.insert("name".to_string(), name.to_string());
    r.insert("type".to_string(), kind.to_string());
    r.insert("label".to_string(), label.to_string());
    r.insert("property".to_string(), property.to_string());
    r
}

fn index_row(name: &str, kind: &str, label: &str, property: &str) -> BTreeMap<String, String> {
    let mut r = row(name, kind, label, property);
    r.insert("state".to_string(), "online".to_string());
    r
}

fn show_indexes(db: &GraphDb) -> Vec<BTreeMap<String, String>> {
    rows(&db.query("SHOW INDEXES", &p()).unwrap())
}

fn show_constraints(db: &GraphDb) -> Vec<BTreeMap<String, String>> {
    rows(&db.query("SHOW CONSTRAINTS", &p()).unwrap())
}

// ── The plan's four ─────────────────────────────────────────────────────────

#[test]
fn test_ddl_create_range_index() {
    let mut db = open_temp();
    ddl(&mut db, "CREATE INDEX idx_age FOR (n:Person) ON (n.age)");
    let rs = db.query("SHOW INDEXES", &p()).unwrap();
    assert_eq!(rs.columns(), ["name", "type", "label", "property", "state"]);
    assert!(rows(&rs)
        .iter()
        .any(|r| r["name"] == "idx_age" && r["type"] == "range"));
    assert_eq!(
        db.range_indexes(),
        vec![("idx_age".into(), "Person".into(), "age".into())]
    );
}

#[test]
fn test_ddl_create_constraint() {
    let mut db = open_temp();
    ddl(
        &mut db,
        "CREATE CONSTRAINT uniq_email FOR (n:User) REQUIRE n.email IS UNIQUE",
    );
    let rs = db.query("SHOW CONSTRAINTS", &p()).unwrap();
    assert_eq!(rs.columns(), ["name", "type", "label", "property"]);
    assert_eq!(
        rows(&rs),
        vec![row("uniq_email", "unique", "User", "email")]
    );
    // And it is the constraint the API declares: a duplicate is refused.
    db.insert_node("User", "a", vec![("email".into(), Value::Str("x".into()))])
        .unwrap();
    let err = db
        .insert_node("User", "b", vec![("email".into(), Value::Str("x".into()))])
        .unwrap_err();
    assert!(
        matches!(err, GraphError::ConstraintViolation { .. }),
        "{err}"
    );
}

#[test]
fn test_ddl_drop_constraint() {
    let mut db = open_temp();
    ddl(
        &mut db,
        "CREATE CONSTRAINT c FOR (n:X) REQUIRE n.p IS UNIQUE",
    );
    ddl(&mut db, "DROP CONSTRAINT c");
    assert!(show_constraints(&db).iter().all(|r| r["name"] != "c"));
    assert!(db.constraints().is_empty());
}

#[test]
fn test_ddl_anonymous_index_auto_names() {
    let mut db = open_temp();
    ddl(&mut db, "CREATE INDEX FOR (n:Person) ON (n.score)");
    assert!(show_indexes(&db)
        .iter()
        .any(|r| r["name"] == "idx_Person_score"));
}

// ── Shapes ──────────────────────────────────────────────────────────────────

#[test]
fn keywords_are_case_insensitive_and_names_may_be_quoted() {
    let mut db = open_temp();
    ddl(&mut db, "create index `my idx` for (p:Person) on (p.age)");
    ddl(
        &mut db,
        "Create Constraint `my c` For (u:User) Require (u.email) Is Unique",
    );
    assert_eq!(
        show_indexes(&db)
            .into_iter()
            .filter(|r| r["type"] == "range")
            .collect::<Vec<_>>(),
        vec![index_row("my idx", "range", "Person", "age")]
    );
    assert_eq!(
        show_constraints(&db),
        vec![row("my c", "unique", "User", "email")]
    );
    ddl(&mut db, "drop index `my idx`");
    ddl(&mut db, "drop constraint `my c`");
    assert!(db.range_indexes().is_empty());
    assert!(db.constraints().is_empty());
    // `SHOW INDEX` and `SHOW CONSTRAINT` are accepted too.
    db.query("show index", &p()).unwrap();
    db.query("show constraint", &p()).unwrap();
}

#[test]
fn drop_index_withdraws_a_range_index() {
    let mut db = open_temp();
    ddl(&mut db, "CREATE INDEX i FOR (n:P) ON (n.a)");
    ddl(&mut db, "DROP INDEX i");
    assert!(db.range_indexes().is_empty());
    assert!(show_indexes(&db).is_empty());
}

#[test]
fn a_ddl_statement_answers_with_no_rows() {
    let mut db = open_temp();
    let rs = ddl(&mut db, "CREATE INDEX i FOR (n:P) ON (n.a)");
    assert!(rs.is_empty());
    assert!(rs.columns().is_empty());
}

#[test]
fn show_lists_every_kind_of_index_sorted_by_name() {
    let mut db = open_temp();
    db.enable_index("Doc", "slug").unwrap();
    db.enable_fulltext("Doc", "body").unwrap();
    ddl(&mut db, "CREATE INDEX by_year FOR (d:Doc) ON (d.year)");
    ddl(
        &mut db,
        "CREATE CONSTRAINT uniq_isbn FOR (d:Doc) REQUIRE d.isbn IS UNIQUE",
    );
    assert_eq!(
        show_indexes(&db),
        vec![
            index_row("by_year", "range", "Doc", "year"),
            index_row("eq_Doc_isbn", "equality", "Doc", "isbn"),
            index_row("eq_Doc_slug", "equality", "Doc", "slug"),
            index_row("ft_Doc_body", "fulltext", "Doc", "body"),
        ]
    );
}

#[test]
fn drop_index_drops_an_equality_or_fulltext_index_by_its_listed_name() {
    let mut db = open_temp();
    db.enable_index("Doc", "slug").unwrap();
    db.enable_fulltext("Doc", "body").unwrap();
    ddl(&mut db, "DROP INDEX eq_Doc_slug");
    ddl(&mut db, "DROP INDEX ft_Doc_body");
    assert!(!db.is_index_enabled("Doc", "slug"));
    assert!(!db.is_fulltext_enabled("Doc", "body"));
    assert!(show_indexes(&db).is_empty());
}

#[test]
fn drop_index_refuses_the_index_a_constraint_owns() {
    let mut db = open_temp();
    ddl(
        &mut db,
        "CREATE CONSTRAINT owner FOR (n:U) REQUIRE n.e IS UNIQUE",
    );
    let err = ddl_err(&mut db, "DROP INDEX eq_U_e");
    assert!(err.contains("owner"), "{err}");
    assert!(db.is_index_enabled("U", "e"));
    // Dropped with the constraint gone, the index goes too.
    ddl(&mut db, "DROP CONSTRAINT owner");
    ddl(&mut db, "DROP INDEX eq_U_e");
    assert!(!db.is_index_enabled("U", "e"));
}

// ── IF [NOT] EXISTS ─────────────────────────────────────────────────────────

#[test]
fn create_index_twice_is_an_error_naming_it_unless_if_not_exists() {
    let mut db = open_temp();
    ddl(&mut db, "CREATE INDEX i FOR (n:P) ON (n.a)");
    let err = ddl_err(&mut db, "CREATE INDEX i FOR (n:P) ON (n.b)");
    assert!(err.contains("'i'"), "{err}");
    ddl(&mut db, "CREATE INDEX i IF NOT EXISTS FOR (n:P) ON (n.a)");
    // An anonymous one over a pair already range-indexed is the same index.
    ddl(&mut db, "CREATE INDEX IF NOT EXISTS FOR (n:P) ON (n.a)");
    let err = ddl_err(&mut db, "CREATE INDEX FOR (n:P) ON (n.a)");
    assert!(err.contains("'i'"), "{err}");
    // A name asked for that would not exist after the no-op is an error.
    let err = ddl_err(&mut db, "CREATE INDEX j IF NOT EXISTS FOR (n:P) ON (n.a)");
    assert!(err.contains("'i'"), "{err}");
    assert_eq!(
        db.range_indexes(),
        vec![("i".into(), "P".into(), "a".into())]
    );
}

#[test]
fn drop_index_missing_is_an_error_naming_it_unless_if_exists() {
    let mut db = open_temp();
    let err = ddl_err(&mut db, "DROP INDEX nope");
    assert!(err.contains("'nope'"), "{err}");
    ddl(&mut db, "DROP INDEX nope IF EXISTS");
}

#[test]
fn create_constraint_twice_is_an_error_naming_it_unless_if_not_exists() {
    let mut db = open_temp();
    ddl(
        &mut db,
        "CREATE CONSTRAINT c FOR (n:U) REQUIRE n.e IS UNIQUE",
    );
    let err = ddl_err(
        &mut db,
        "CREATE CONSTRAINT c FOR (n:U) REQUIRE n.f IS UNIQUE",
    );
    assert!(err.contains("'c'"), "{err}");
    ddl(
        &mut db,
        "CREATE CONSTRAINT c IF NOT EXISTS FOR (n:U) REQUIRE n.e IS UNIQUE",
    );
    assert_eq!(db.constraints().len(), 1);
}

#[test]
fn drop_constraint_missing_is_an_error_naming_it_unless_if_exists() {
    let mut db = open_temp();
    let err = ddl_err(&mut db, "DROP CONSTRAINT nope");
    assert!(err.contains("'nope'"), "{err}");
    ddl(&mut db, "DROP CONSTRAINT nope IF EXISTS");
}

#[test]
fn a_constraint_over_duplicates_fails_and_declares_nothing() {
    let mut db = open_temp();
    for k in ["a", "b"] {
        db.insert_node("U", k, vec![("e".into(), Value::Int(1))])
            .unwrap();
    }
    let err = db
        .query_write("CREATE CONSTRAINT c FOR (n:U) REQUIRE n.e IS UNIQUE", &p())
        .unwrap_err();
    assert!(
        matches!(err, GraphError::ConstraintCreationConflict { total: 2, .. }),
        "{err}"
    );
    assert!(show_constraints(&db).is_empty());
}

// ── A DDL statement is the whole statement ──────────────────────────────────

#[test]
fn ddl_mixed_with_clauses_is_a_parse_error() {
    let mut db = open_temp();
    for q in [
        "CREATE INDEX i FOR (n:P) ON (n.a) RETURN 1",
        "CREATE CONSTRAINT c FOR (n:P) REQUIRE n.a IS UNIQUE RETURN n",
        "DROP INDEX i MATCH (n) RETURN n",
        "CREATE INDEX i FOR (n:P) ON (m.a)",
        "CREATE INDEX i FOR (n:P) ON (n.a, n.b)",
        "CREATE CONSTRAINT FOR (n:P) REQUIRE n.a IS UNIQUE",
        "CREATE CONSTRAINT c FOR (n:P) REQUIRE n.a IS NOT NULL",
        "CREATE INDEX",
        "DROP INDEX",
        "DROP TABLE t",
    ] {
        let err = ddl_err(&mut db, q);
        assert!(err.starts_with("query error: parse:"), "{q}: {err}");
    }
    for q in [
        "SHOW INDEXES RETURN 1",
        "SHOW INDEXES WHERE 1 = 1",
        "SHOW TABLES",
    ] {
        let err = db.query(q, &p()).unwrap_err().to_string();
        assert!(err.contains("parse:"), "{q}: {err}");
    }
    assert!(db.range_indexes().is_empty());
    assert!(db.constraints().is_empty());
}

#[test]
fn the_ddl_words_still_name_properties_labels_and_variables() {
    let mut db = open_temp();
    db.query_write(
        "CREATE (n:Index {id: 'a', index: 1, show: 2, drop: 3, constraint: 4, unique: 5, \
         require: 6, online: 7})",
        &p(),
    )
    .unwrap();
    let rs = db
        .query(
            "MATCH (show:Index) WHERE show.index = 1 AND show.unique IS NOT NULL \
             RETURN show.drop AS drop, show.constraint AS constraint, show.require AS require, \
             show.online AS online",
            &p(),
        )
        .unwrap();
    assert_eq!(rs.len(), 1);
    assert_eq!(rs.get(0, "drop"), Some(&Value::Int(3)));
    assert_eq!(rs.get(0, "constraint"), Some(&Value::Int(4)));
    assert_eq!(rs.get(0, "require"), Some(&Value::Int(6)));
    assert_eq!(rs.get(0, "online"), Some(&Value::Int(7)));
    // And the DDL itself can index a property with one of those names.
    ddl(&mut db, "CREATE INDEX FOR (n:Index) ON (n.index)");
    ddl(
        &mut db,
        "CREATE CONSTRAINT show FOR (n:Index) REQUIRE n.show IS UNIQUE",
    );
    assert!(show_indexes(&db)
        .iter()
        .any(|r| r["name"] == "idx_Index_index"));
    assert_eq!(show_constraints(&db)[0]["name"], "show");
    db.query_write("MATCH (n:Index) SET n.drop = 9", &p())
        .unwrap();
    assert_eq!(db.get_prop("a", "drop"), Some(Value::Int(9)));
}

// ── Where DDL may run ───────────────────────────────────────────────────────

#[test]
fn ddl_is_refused_by_the_read_entry_point() {
    let db = open_temp();
    for q in ["CREATE INDEX i FOR (n:P) ON (n.a)", "DROP CONSTRAINT c"] {
        let err = db.query(q, &p()).unwrap_err().to_string();
        assert!(err.contains("query_write"), "{q}: {err}");
    }
}

#[test]
fn ddl_is_refused_inside_a_transaction_and_show_answers_there() {
    let dir = tmp("tx");
    let db = SharedDb::open(&dir).unwrap();
    db.write()
        .query_write("CREATE INDEX i FOR (n:P) ON (n.a)", &p())
        .unwrap();
    let seq = db.read().commit_seq();
    let mut tx = db.begin_transaction();
    for q in [
        "CREATE INDEX j FOR (n:P) ON (n.b)",
        "DROP INDEX i",
        "CREATE CONSTRAINT c FOR (n:P) REQUIRE n.a IS UNIQUE",
        "DROP CONSTRAINT c",
    ] {
        let err = tx.execute(q, &p()).unwrap_err().to_string();
        assert!(err.contains("transaction"), "{q}: {err}");
    }
    let rs = tx.execute("SHOW INDEXES", &p()).unwrap();
    assert_eq!(rows(&rs), vec![index_row("i", "range", "P", "a")]);
    let rs = tx.execute("SHOW CONSTRAINTS", &p()).unwrap();
    assert!(rs.is_empty());
    tx.commit().unwrap();
    assert_eq!(db.read().commit_seq(), seq, "nothing was logged");
    assert_eq!(db.read().range_indexes().len(), 1);
    assert!(db.read().constraints().is_empty());
}

#[test]
fn ddl_is_refused_on_an_as_of_handle_and_by_query_at() {
    let dir = tmp("asof");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("P", "a", vec![("a".into(), Value::Int(1))])
        .unwrap();
    ddl(&mut db, "CREATE INDEX i FOR (n:P) ON (n.a)");
    let mut past = GraphDb::open_at(&dir, 1).unwrap();
    let err = past
        .query_write("CREATE INDEX j FOR (n:P) ON (n.b)", &p())
        .unwrap_err();
    assert!(matches!(err, GraphError::ReadOnly), "{err}");
    let err = past.query_write("DROP INDEX i", &p()).unwrap_err();
    assert!(matches!(err, GraphError::ReadOnly), "{err}");
    let err = db
        .query_at(1, "CREATE INDEX j FOR (n:P) ON (n.b)", &p())
        .unwrap_err()
        .to_string();
    assert!(err.contains("read-only"), "{err}");
    // SHOW is a read, so it answers as of the commit asked for.
    let now = rows(&db.query_at(1, "SHOW INDEXES", &p()).unwrap());
    assert_eq!(now, vec![index_row("i", "range", "P", "a")]);
    let before = db.query_at(0, "SHOW INDEXES", &p()).unwrap();
    assert!(before.is_empty());
}

fn role_store() -> TmpDb {
    let mut db = open_temp();
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
    db.insert_node("P", "p1", vec![("ns".into(), Value::Str("a".into()))])
        .unwrap();
    db
}

#[test]
fn ddl_is_refused_on_a_role_scoped_write_and_a_masked_read() {
    let mut db = role_store();
    for q in [
        "CREATE INDEX i FOR (n:P) ON (n.a)",
        "DROP INDEX i",
        "CREATE CONSTRAINT c FOR (n:P) REQUIRE n.a IS UNIQUE",
        "DROP CONSTRAINT c",
    ] {
        let err = db.query_write_authz("writer", q, &p()).unwrap_err();
        assert!(
            matches!(err, GraphError::RoleWriteDenied { .. }),
            "{q}: {err}"
        );
        let mask = db.mask_for_role("writer").unwrap();
        let err = db.query_masked(q, &p(), &mask).unwrap_err();
        assert!(matches!(err, GraphError::MaskedReadOnly), "{q}: {err}");
        let snap = db.reader();
        assert!(snap.query_masked(q, &p(), &mask).is_err(), "{q}");
    }
    assert!(db.range_indexes().is_empty());
    assert!(db.constraints().is_empty());
    // SHOW is a read: it answers on a masked read like any other.
    ddl(&mut db, "CREATE INDEX i FOR (n:P) ON (n.a)");
    let mask = db.mask_for_role("writer").unwrap();
    let rs = db.query_masked("SHOW INDEXES", &p(), &mask).unwrap();
    assert_eq!(rows(&rs), vec![index_row("i", "range", "P", "a")]);
    let rs = db
        .reader()
        .query_masked("SHOW INDEXES", &p(), &mask)
        .unwrap();
    assert_eq!(rows(&rs), vec![index_row("i", "range", "P", "a")]);
}

#[test]
fn is_write_query_classifies_ddl() {
    for q in [
        "CREATE INDEX i FOR (n:P) ON (n.a)",
        "DROP INDEX i",
        "CREATE CONSTRAINT c FOR (n:P) REQUIRE n.a IS UNIQUE",
        "drop constraint c",
    ] {
        assert!(mushroomdb::is_write_query(q).unwrap(), "{q}");
    }
    for q in ["SHOW INDEXES", "show constraints"] {
        assert!(!mushroomdb::is_write_query(q).unwrap(), "{q}");
    }
}

// ── The lock-free reader, persistence and the format ────────────────────────

#[test]
fn the_reader_shows_what_the_handle_shows_after_every_statement() {
    let dir = tmp("reader");
    let db = SharedDb::open(&dir).unwrap();
    let check = |db: &SharedDb| {
        let live_i = rows(&db.read().query("SHOW INDEXES", &p()).unwrap());
        let snap_i = rows(&db.reader().query("SHOW INDEXES", &p()).unwrap());
        assert_eq!(live_i, snap_i);
        let live_c = rows(&db.read().query("SHOW CONSTRAINTS", &p()).unwrap());
        let snap_c = rows(&db.reader().query("SHOW CONSTRAINTS", &p()).unwrap());
        assert_eq!(live_c, snap_c);
        (live_i.len(), live_c.len())
    };
    for (q, want) in [
        ("CREATE INDEX i FOR (n:P) ON (n.a)", (1, 0)),
        (
            "CREATE CONSTRAINT c FOR (n:P) REQUIRE n.b IS UNIQUE",
            (2, 1),
        ),
        ("DROP INDEX i", (1, 1)),
        ("DROP CONSTRAINT c", (1, 0)),
        ("DROP INDEX eq_P_b", (0, 0)),
    ] {
        db.write().query_write(q, &p()).unwrap();
        assert_eq!(check(&db), want, "after {q}");
    }
    db.write().enable_fulltext("P", "t").unwrap();
    assert_eq!(check(&db), (1, 0), "after enable_fulltext");
}

#[test]
fn declarations_survive_a_reopen_and_a_snapshot() {
    let dir = tmp("reopen");
    let (indexes, constraints) = {
        let mut db = GraphDb::open(&dir).unwrap();
        db.enable_fulltext("P", "t").unwrap();
        ddl(&mut db, "CREATE INDEX i FOR (n:P) ON (n.a)");
        ddl(
            &mut db,
            "CREATE CONSTRAINT c FOR (n:P) REQUIRE n.b IS UNIQUE",
        );
        (show_indexes(&db), show_constraints(&db))
    };
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(show_indexes(&db), indexes);
    assert_eq!(show_constraints(&db), constraints);
    drop(db);
    let mut db = GraphDb::open(&dir).unwrap();
    db.snapshot().unwrap();
    drop(db);
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(show_indexes(&db), indexes);
    assert_eq!(show_constraints(&db), constraints);
    assert_eq!(
        rows(&db.reader().query("SHOW INDEXES", &p()).unwrap()),
        indexes
    );
}

#[test]
fn the_first_ddl_declaration_moves_the_store_to_v11() {
    for q in [
        "CREATE INDEX i FOR (n:P) ON (n.a)",
        "CREATE CONSTRAINT c FOR (n:P) REQUIRE n.a IS UNIQUE",
    ] {
        let dir = tmp("v11");
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("P", "a", vec![("a".into(), Value::Int(1))])
            .unwrap();
        assert!(!db.is_v11());
        // SHOW writes nothing and leaves the format alone.
        db.query("SHOW INDEXES", &p()).unwrap();
        assert!(!db.is_v11());
        ddl(&mut db, q);
        assert!(db.is_v11(), "{q}");
        // A V11 snapshot is on disk: the version a 0.9.x reader refuses.
        assert_eq!(
            mushroomdb::snapshot_version_at(&dir).unwrap(),
            Some(11),
            "{q}"
        );
    }
}

// ── Fix round 1: names cannot collide ───────────────────────────────────────

#[test]
fn a_range_index_cannot_take_a_name_reserved_for_listed_indexes() {
    let mut db = open_temp();
    // Before the equality index exists, and after it: both refused.
    for q in [
        "CREATE INDEX eq_X_y FOR (n:X) ON (n.y)",
        "CREATE INDEX ft_X_y FOR (n:X) ON (n.z)",
    ] {
        let err = ddl_err(&mut db, q);
        assert!(err.contains("reserved"), "{q}: {err}");
    }
    let err = db
        .enable_range_index("eq_a", "X", "y")
        .unwrap_err()
        .to_string();
    assert!(err.contains("reserved"), "{err}");
    db.enable_index("X", "y").unwrap();
    db.enable_fulltext("X", "y").unwrap();
    for q in [
        "CREATE INDEX eq_X_y FOR (n:X) ON (n.y)",
        "CREATE INDEX ft_X_y IF NOT EXISTS FOR (n:X) ON (n.y)",
    ] {
        let err = ddl_err(&mut db, q);
        assert!(err.contains("reserved"), "{q}: {err}");
    }
    assert!(db.range_indexes().is_empty());
    // So each listed name is one index, and DROP INDEX takes the right one.
    ddl(&mut db, "DROP INDEX eq_X_y");
    assert!(!db.is_index_enabled("X", "y"));
    assert!(db.is_fulltext_enabled("X", "y"));
}

#[test]
fn drop_index_refuses_a_listed_name_two_indexes_share() {
    let mut db = open_temp();
    db.enable_index("A_b", "c").unwrap();
    db.enable_index("A", "b_c").unwrap();
    let names: Vec<String> = show_indexes(&db)
        .into_iter()
        .map(|r| r["name"].clone())
        .collect();
    assert_eq!(names, ["eq_A_b_c", "eq_A_b_c"]);
    let err = ddl_err(&mut db, "DROP INDEX eq_A_b_c");
    for part in ["(A, b_c)", "(A_b, c)", "disable_index"] {
        assert!(err.contains(part), "{part}: {err}");
    }
    let err = ddl_err(&mut db, "DROP INDEX eq_A_b_c IF EXISTS");
    assert!(err.contains("disable_index"), "{err}");
    assert!(db.is_index_enabled("A_b", "c"));
    assert!(db.is_index_enabled("A", "b_c"));
    // The API drops either by its pair.
    db.disable_index("A", "b_c").unwrap();
    ddl(&mut db, "DROP INDEX eq_A_b_c");
    assert!(show_indexes(&db).is_empty());
}

// ── Fix round 1: SHOW lists only what the caller can see ────────────────────

/// Role `writer` sees label `P` only. `P` and `Q` each have a node, a range
/// index, an equality index and a constraint; `P` lives in namespace `a`,
/// `Q` in `b`.
fn two_label_store() -> TmpDb {
    let mut db = role_store();
    db.insert_node("Q", "q1", vec![("ns".into(), Value::Str("b".into()))])
        .unwrap();
    for l in ["P", "Q"] {
        ddl(&mut db, &format!("CREATE INDEX FOR (n:{l}) ON (n.a)"));
        ddl(
            &mut db,
            &format!("CREATE CONSTRAINT c_{l} FOR (n:{l}) REQUIRE n.b IS UNIQUE"),
        );
    }
    db
}

fn labels_of(rs: &ResultSet) -> Vec<String> {
    let mut v: Vec<String> = rows(rs).into_iter().map(|r| r["label"].clone()).collect();
    v.dedup();
    v
}

#[test]
fn show_is_narrowed_to_the_labels_a_scoped_read_can_see() {
    let mut db = two_label_store();
    assert_eq!(
        labels_of(&db.query("SHOW INDEXES", &p()).unwrap()),
        ["P", "Q", "P", "Q"]
    );
    let role = db.mask_for_role("writer").unwrap();
    let ns_a = db.mask_for_namespace("a");
    let ns_b = db.mask_for_namespace("b");
    for (mask, want) in [(&role, "P"), (&ns_a, "P"), (&ns_b, "Q")] {
        for q in ["SHOW INDEXES", "SHOW CONSTRAINTS"] {
            let live = db.query_masked(q, &p(), mask).unwrap();
            let snap = db.reader().query_masked(q, &p(), mask).unwrap();
            assert_eq!(rows(&live), rows(&snap), "{q}");
            assert!(!live.is_empty(), "{q}");
            assert!(
                labels_of(&live).iter().all(|l| l == want),
                "{q}: {:?}",
                rows(&live)
            );
        }
    }
    // A role's own SHOW through the write entry point is narrowed the same way.
    let rs = db
        .query_write_authz("writer", "SHOW CONSTRAINTS", &p())
        .unwrap();
    assert_eq!(rows(&rs), vec![row("c_P", "unique", "P", "b")]);
    // A mask that sees nothing sees no schema.
    let none = mushroomdb::NodeMask::from_ids(std::iter::empty());
    assert!(db
        .query_masked("SHOW INDEXES", &p(), &none)
        .unwrap()
        .is_empty());
}
