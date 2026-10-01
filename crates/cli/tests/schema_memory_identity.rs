//! `schema apply --memory-identity` adds the identity preset on request, and
//! says what it will backfill before it does.
use cli::{parse_args, run_schema_apply_memory_identity};
use core_api::{GraphDb, Value};

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "mdb-schemaid-{name}-{}-{nanos}",
        std::process::id()
    ))
}

/// Two people written before 0.7's aliases existed: no `aliases` at all.
fn pre_alias_store(name: &str) -> std::path::PathBuf {
    let dir = tmp(name);
    let mut db = GraphDb::open(&dir).unwrap();
    for key in ["matthew-sherlin", "msherlin"] {
        db.insert_node(
            "Person",
            key,
            vec![("name".into(), Value::Str("Matthew Sherlin".into()))],
        )
        .unwrap();
    }
    dir
}

#[test]
fn the_preset_states_its_backfill_first_then_links() {
    let dir = pre_alias_store("apply");
    let out = run_schema_apply_memory_identity(&dir).expect("apply");
    let lines: Vec<&str> = out.lines().collect();
    assert!(
        lines[0].starts_with("identity preset: 11 SAME_AS rules"),
        "the first line says what is being added: {out}"
    );
    assert!(
        lines[1].starts_with("before applying: 2 entity node(s); 2 need an aliases list"),
        "the second says what it will backfill, measured before writing: {out}"
    );
    assert!(out.contains("11 created"), "{out}");
    assert!(out.contains("SAME_AS edges now: 2"), "{out}");

    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.rules().len(), 11);
    assert!(db.get_prop("matthew-sherlin", "aliases").is_some());
}

#[test]
fn applying_it_again_writes_nothing() {
    let dir = pre_alias_store("again");
    run_schema_apply_memory_identity(&dir).unwrap();
    let out = run_schema_apply_memory_identity(&dir).unwrap();
    assert!(out.contains("0 need an aliases list"), "{out}");
    assert!(out.contains("0 created, 0 updated, 11 unchanged"), "{out}");
}

#[test]
fn the_flag_takes_no_file_and_no_other_preset() {
    for args in [
        vec!["schema", "apply", "db", "--memory-identity", "x.json"],
        vec![
            "schema",
            "apply",
            "db",
            "--memory-identity",
            "--memory-defaults",
        ],
    ] {
        let err = parse_args(&args).unwrap_err();
        assert!(
            err.contains("--memory-identity takes no schema file"),
            "{args:?}: {err}"
        );
    }
}
