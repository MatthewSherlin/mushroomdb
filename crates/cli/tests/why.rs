//! `mushroomdb why <db> <a> <b>` — the shell form of the product's headline
//! question, which the code-graph `why` used to occupy and which
//! `explain_association` had no CLI door for.
use std::process::Command;

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("mdb-why-{name}-{}-{nanos}", std::process::id()))
}

fn run(args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
        .args(args)
        .output()
        .expect("run");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr),
    )
}

/// Two people who share a skill, and a rule that derives the link.
///
/// The rule is created through `schema apply <db> <schema.json>`: there is no
/// `create-rule` CLI subcommand, and `schema apply` is the CLI's one door to a
/// `RuleDef`. The binary requires an `id:` property on `CREATE`, and that id
/// is the node key `explain` resolves.
fn store(name: &str) -> String {
    let dir = tmp(name);
    let db = dir.to_string_lossy().to_string();
    for q in [
        "CREATE (n:Person {id:'ada', skills:['rust','graphs']})",
        "CREATE (n:Person {id:'grace', skills:['rust','compilers']})",
    ] {
        let (ok, out) = run(&["query", &db, q]);
        assert!(ok, "seed failed: {out}");
    }
    let schema = dir.with_extension("schema.json");
    std::fs::write(
        &schema,
        r#"{
  "fulltext": [],
  "indexes": [],
  "views": [],
  "roles": [],
  "rules": [
    {
      "name": "shared_skill",
      "src_label": "Person",
      "dst_label": "Person",
      "edge_type": "SHARES_SKILL",
      "predicate": { "Overlap": { "field": "skills", "min": 0.3 } },
      "weight_prop": null,
      "max_edges": null
    }
  ]
}"#,
    )
    .expect("write schema");
    let (ok, out) = run(&["schema", "apply", &db, &schema.to_string_lossy()]);
    assert!(ok, "schema apply failed: {out}");
    db
}

#[test]
fn why_names_the_rule_and_the_values_the_two_share() {
    let db = store("shared");
    let (ok, out) = run(&["why", &db, "ada", "grace"]);
    assert!(ok, "why failed: {out}");
    assert!(out.contains("shared_skill"), "no rule named: {out}");
    assert!(out.contains("rust"), "no shared value shown: {out}");
}

#[test]
fn why_on_two_unrelated_keys_says_so_rather_than_erroring() {
    let db = store("unrelated");
    let (ok, out) = run(&[
        "query",
        &db,
        "CREATE (n:Person {id:'lone', skills:['cobol']})",
    ]);
    assert!(ok, "{out}");
    let (ok, out) = run(&["why", &db, "ada", "lone"]);
    assert!(ok, "an honest 'nothing links these' is not an error: {out}");
    assert!(!out.contains("shared_skill"), "{out}");
}
