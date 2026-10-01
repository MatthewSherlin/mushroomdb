//! An evidence line is bounded in length, not only the digest in lines.
use core_api::explain_digest::{explain_with_evidence, render_explain, MAX_EVIDENCE_CHARS};
use core_api::{GraphDb, Predicate, RuleDef, Value};

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("mdb-evcap-{name}-{}-{nanos}", std::process::id()))
}

#[test]
fn a_long_shared_list_is_cut_in_the_digest_and_whole_in_the_report() {
    let mut db = GraphDb::open(&tmp("shared")).unwrap();
    // Sixty shared values of fifteen characters: about a kilobyte of evidence.
    let skills: Vec<Value> = (0..60)
        .map(|i| Value::Str(format!("skill-number-{i:02}")))
        .collect();
    for key in ["a", "b"] {
        db.insert_node(
            "Person",
            key,
            vec![("skills".into(), Value::List(skills.clone()))],
        )
        .unwrap();
    }
    db.create_rule(RuleDef {
        name: "skill_fit".into(),
        src_label: "Person".into(),
        dst_label: "Person".into(),
        predicate: Predicate::Overlap {
            field: "skills".into(),
            min: 0.5,
        },
        edge_type: "FIT".into(),
        weight_prop: Some("weight".into()),
        max_edges: Some(32),
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
        namespace: None,
    })
    .unwrap();

    let found = explain_with_evidence(&db, "a", "b").unwrap();
    assert!(!found.is_empty(), "fixture: the rule linked them");
    let text = render_explain("a", "b", &found);

    for line in text.lines().skip(1) {
        let bracket = line.split_once(" [").map_or("", |(_, rest)| rest);
        assert!(
            bracket.chars().count() <= MAX_EVIDENCE_CHARS + 2,
            "evidence of {} characters on one line: {line}",
            bracket.chars().count()
        );
        assert!(line.contains("…]"), "a cut is marked: {line}");
        assert!(
            line.contains("via rule skill_fit") && line.contains("overlap on skills"),
            "the rule and the predicate survive the cut: {line}"
        );
    }
    // The report a program asks for still carries every value.
    let report = serde_json::to_string(&found).unwrap();
    assert!(report.contains("skill-number-59"), "{report}");
}
