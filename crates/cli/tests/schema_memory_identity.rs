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
        lines[0].starts_with("identity preset: 16 SAME_AS rules"),
        "the first line says what is being added: {out}"
    );
    assert!(
        lines[1].starts_with("before applying: 2 entity node(s); 2 need an aliases list"),
        "the second says what it will backfill, measured before writing: {out}"
    );
    assert!(
        lines[2].starts_with("alias_keys: nothing to move"),
        "the third says no node carried an alias of its owner's: {out}"
    );
    assert!(out.contains("16 created"), "{out}");
    assert!(out.contains("SAME_AS edges now: 2 (1 pair(s))"), "{out}");

    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.rules().len(), 16);
    assert!(
        db.get_prop("matthew-sherlin", "alias_keys").is_none(),
        "no declared list is invented"
    );
    assert!(db.get_prop("matthew-sherlin", "aliases").is_some());
}

/// A node written before 0.7 may carry its owner's own `aliases` property.
/// The backfill says how many such nodes there are, before writing, and moves
/// what the key and name do not imply to `alias_keys` as written: nothing is
/// dropped, and a second apply finds nothing left to do.
#[test]
fn the_backfill_moves_a_users_own_aliases_to_alias_keys_and_says_so() {
    let dir = pre_alias_store("moves");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.set_prop(
            "msherlin",
            "aliases",
            Value::List(vec![
                Value::Str("Matt".into()),
                Value::Str("sherlin".into()),
                Value::Str("The Boss".into()),
            ]),
        )
        .unwrap();
    }
    let out = run_schema_apply_memory_identity(&dir).expect("apply");
    let lines: Vec<&str> = out.lines().collect();
    assert!(
        lines[1].starts_with("before applying: 2 entity node(s); 2 need an aliases list"),
        "{out}"
    );
    assert!(
        lines[2].starts_with(
            "alias_keys: 1 node(s) carry aliases their key and name do not imply; those are \
             kept, moved to alias_keys as declared aliases"
        ),
        "{out}"
    );
    assert!(out.contains("SAME_AS edges now: 2 (1 pair(s))"), "{out}");
    let strs = |xs: &[&str]| Value::List(xs.iter().map(|s| Value::Str((*s).into())).collect());
    {
        let db = GraphDb::open(&dir).unwrap();
        assert_eq!(
            db.get_prop("msherlin", "aliases"),
            Some(strs(&["matthew", "matthew sherlin", "msherlin", "sherlin"]))
        );
        assert_eq!(
            db.get_prop("msherlin", "alias_keys"),
            Some(strs(&["Matt", "The Boss"])),
            "verbatim; `sherlin` was derivable and is not moved"
        );
        assert_eq!(db.get_prop("matthew-sherlin", "alias_keys"), None);
    }
    let again = run_schema_apply_memory_identity(&dir).unwrap();
    assert!(again.contains("0 need an aliases list"), "{again}");
    assert!(again.contains("alias_keys: nothing to move"), "{again}");
    assert!(
        again.contains("0 created, 0 updated, 16 unchanged"),
        "{again}"
    );
}

#[test]
fn applying_it_again_writes_nothing() {
    let dir = pre_alias_store("again");
    run_schema_apply_memory_identity(&dir).unwrap();
    let out = run_schema_apply_memory_identity(&dir).unwrap();
    assert!(out.contains("0 need an aliases list"), "{out}");
    assert!(out.contains("0 created, 0 updated, 16 unchanged"), "{out}");
}

/// A store that took the preset when it was eleven rules — `Overlap` only —
/// gains the five claim rules, keeps its eleven, and a claim then links.
#[test]
fn a_store_with_the_eleven_rule_preset_gains_the_five_claim_rules() {
    let dir = pre_alias_store("upgrade");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        let mut eleven = core_api::memory_schema::memory_identity();
        eleven
            .rules
            .retain(|r| matches!(r.predicate, core_api::Predicate::Overlap { .. }));
        assert_eq!(eleven.rules.len(), 11);
        db.apply_schema(&eleven).unwrap();
    }
    let out = run_schema_apply_memory_identity(&dir).expect("apply");
    assert!(
        out.starts_with("identity preset: 16 SAME_AS rules"),
        "{out}"
    );
    assert!(out.contains("5 created, 0 updated, 11 unchanged"), "{out}");
    let again = run_schema_apply_memory_identity(&dir).unwrap();
    assert!(
        again.contains("0 created, 0 updated, 16 unchanged"),
        "{again}"
    );

    let mut db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.rules().len(), 16);
    let about = vec!["matt".to_string()];
    core_api::memory::remember::describe_entity_with_aliases(
        &mut db,
        "matthew-sherlin",
        None,
        &[],
        &about,
    )
    .unwrap();
    let report = core_api::memory::remember::remember(
        &mut db,
        &core_api::memory::remember::RememberInput {
            text: "Matt owns 0.7",
            about: &about,
            kind: "note",
            ts: 1_759_000_000,
            source: None,
            entities: &[],
            facts: &[],
        },
    )
    .unwrap();
    assert_eq!(report.same_as.len(), 1, "{:?}", report.same_as);
    assert_eq!(report.same_as[0].score, 1.0);
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
