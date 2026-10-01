//! The store's rule proposals, filtered: nothing over a field the store
//! writes for itself, and arguments `create_rule` accepts unchanged on every
//! surface. Moved out of `crates/server` in 0.7.
use core_api::memory::remember::{describe_entity, remember, RememberInput};
use core_api::memory::suggest::{
    create_rule_args, filtered_suggestions, round_to, BOOKKEEPING_FIELDS,
};
use core_api::memory_schema::memory_defaults;
use core_api::{GraphDb, Predicate, RuleDef, Value};
use std::collections::BTreeSet;

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "mdb-memsuggest-{name}-{}-{nanos}",
        std::process::id()
    ))
}

fn rule(weight_prop: Option<&str>, approximate: bool) -> RuleDef {
    RuleDef {
        name: "same_team".into(),
        src_label: "Person".into(),
        dst_label: "Person".into(),
        predicate: if approximate {
            Predicate::VectorSimilar {
                field: "emb".into(),
                min: 0.9,
            }
        } else {
            Predicate::FieldEqual {
                field: "team".into(),
            }
        },
        edge_type: "SAME_TEAM".into(),
        weight_prop: weight_prop.map(str::to_string),
        max_edges: Some(32),
        approximate,
        via_label: None,
        via_edge: None,
        via_dir: None,
        namespace: None,
    }
}

fn keys(v: &serde_json::Value) -> BTreeSet<String> {
    v.as_object().expect("an object").keys().cloned().collect()
}

#[test]
fn create_rule_args_carry_no_nulls_and_always_a_weight_prop() {
    let args = create_rule_args(&rule(None, false));
    let want: BTreeSet<String> = [
        "name",
        "src_label",
        "dst_label",
        "predicate",
        "edge_type",
        "weight_prop",
        "max_edges",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    assert_eq!(keys(&args), want, "no null, and no `approximate: false`");
    assert_eq!(
        args["weight_prop"], "weight",
        "written out: MCP would default a missing one to it and Python would not"
    );

    // What is shown is what is created: the arguments decode to a rule.
    let back: RuleDef = serde_json::from_value(args).expect("create_rule accepts them");
    assert_eq!(back.weight_prop.as_deref(), Some("weight"));
    assert_eq!(back.max_edges, Some(32));
    assert!(!back.approximate);

    let kept = create_rule_args(&rule(Some("score"), true));
    assert_eq!(
        kept["weight_prop"], "score",
        "a named one is not overwritten"
    );
    assert_eq!(kept["approximate"], true, "kept when set");
}

#[test]
fn round_to_is_the_precision_the_text_prints() {
    assert_eq!(round_to(0.126, 2), 0.13);
    assert_eq!(round_to(1.0 / 3.0, 3), 0.333);
    assert_eq!(round_to(2.0, 2), 2.0);
}

/// The server test `suggest_rules_never_proposes_a_bookkeeping_field`, at the
/// layer the filter now lives in: thirty notes sharing `kind`, `source` and
/// `ts`-adjacent values, and twelve people sharing a `team`.
#[test]
fn nothing_is_proposed_over_a_field_the_store_writes_for_itself() {
    let mut db = GraphDb::open(&tmp("filter")).unwrap();
    db.apply_schema(&memory_defaults()).unwrap();
    for i in 0..30i64 {
        let text = format!("note {i} about the release");
        remember(
            &mut db,
            &RememberInput {
                text: &text,
                about: &[],
                kind: ["note", "decision", "todo"][(i % 3) as usize],
                ts: 1_759_000_000 + i,
                source: Some(["session-a", "session-b"][(i % 2) as usize]),
                entities: &[],
                facts: &[],
            },
        )
        .unwrap();
    }
    for i in 0..12 {
        let key = format!("p{i}");
        describe_entity(
            &mut db,
            &key,
            Some("Person"),
            &[
                ("id".to_string(), Value::Str(key.clone())),
                ("name".to_string(), Value::Str(format!("Person {i}"))),
                (
                    "team".to_string(),
                    Value::Str(["infra", "ui"][i % 2].to_string()),
                ),
            ],
        )
        .unwrap();
    }

    let found = filtered_suggestions(&db);

    assert!(
        found.bookkeeping_hidden > 0,
        "the notes share kind and source; the filter must have dropped those: {found:?}"
    );
    assert_eq!(
        found.total,
        found.suggestions.len(),
        "uncapped at this layer"
    );
    for s in &found.suggestions {
        let args = s.create_rule_args.to_string();
        for f in BOOKKEEPING_FIELDS {
            assert!(
                !args.contains(&format!("\"field\":\"{f}\"")),
                "proposed a rule over bookkeeping field {f}: {args}"
            );
        }
        assert!(
            s.create_rule_args.get("weight_prop").is_some(),
            "every proposal's arguments are the filtered shape: {args}"
        );
    }
    assert!(
        found.suggestions.iter().any(|s| s
            .create_rule_args
            .to_string()
            .contains("\"field\":\"team\"")),
        "the one real pattern survives: {found:?}"
    );
}
