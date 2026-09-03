//! `mushroomdb recall <db>` reads a Claude Code UserPromptSubmit JSON payload on
//! stdin and prints related graph facts as plain text (or nothing).
use cli::recall::run_recall;
use cli::run_demo;
use std::path::PathBuf;

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "mushroomdb-recall-{name}-{}-{nanos}",
        std::process::id()
    ))
}

#[test]
fn recall_on_demo_store_names_matching_nodes() {
    let dir = tmp("demo");
    run_demo(&dir).expect("demo");
    let payload = r#"{"session_id":"s","cwd":"/x","hook_event_name":"UserPromptSubmit","prompt":"what do we know about Person 1 and Project 5?"}"#;
    let out = run_recall(&dir, payload);
    assert!(
        out.starts_with("mushroomdb recall"),
        "unexpected header: {out:?}"
    );
    assert!(out.contains("person-01"), "missing person-01 in {out}");
    assert!(out.contains("proj-05"), "missing proj-05 in {out}");
    assert!(
        out.len() < 2000,
        "recall output must stay small: {} bytes",
        out.len()
    );
}

#[test]
fn recall_accepts_user_prompt_and_user_input_field_names() {
    let dir = tmp("fields");
    run_demo(&dir).expect("demo");
    for field in ["user_prompt", "user_input"] {
        let payload = format!(r#"{{"{field}":"Person 1"}}"#);
        assert!(
            run_recall(&dir, &payload).contains("person-01"),
            "field {field}"
        );
    }
}

#[test]
fn recall_lists_edges_with_the_rules_own_weight_property() {
    let dir = tmp("edges");
    run_demo(&dir).expect("demo");
    let out = run_recall(&dir, r#"{"prompt":"Person 1"}"#);
    // skill_fit declares weight_prop "score", not the HTTP default "weight".
    assert!(out.contains("FIT -> proj-01 (score 1.00)"), "{out}");
}

#[test]
fn recall_drops_trailing_nodes_rather_than_blow_the_size_budget() {
    let dir = tmp("budget");
    let long_name = format!("alpha {}", "x".repeat(400));
    {
        let mut db = core_api::GraphDb::open(&dir).expect("open");
        db.enable_fulltext("Doc", "name").expect("fulltext");
        for i in 1..=6 {
            db.insert_node(
                "Doc",
                &format!("doc-{i}"),
                vec![("name".to_string(), core_api::Value::Str(long_name.clone()))],
            )
            .expect("insert");
        }
    }
    let out = run_recall(&dir, r#"{"prompt":"alpha"}"#);
    assert!(
        out.contains("\n    …\n"),
        "expected an elision marker: {out}"
    );
    // The header counts what printed, not what matched.
    let printed = out.lines().filter(|l| l.starts_with("- doc-")).count();
    assert!(printed < 6, "budget must drop nodes, printed {printed}");
    assert!(
        out.starts_with(&format!("mushroomdb recall ({printed} related nodes")),
        "{out}"
    );
    // Header, hint and elision marker are charged against the same budget.
    assert!(
        out.len() <= 1800,
        "whole digest must fit the budget: {} bytes",
        out.len()
    );
}

#[test]
fn recall_is_silent_when_no_fulltext_index_is_enabled() {
    let dir = tmp("nofts");
    drop(core_api::GraphDb::open(&dir).expect("open"));
    assert_eq!(run_recall(&dir, r#"{"prompt":"Person 1"}"#), "");
}

#[test]
fn recall_writes_nothing_to_an_empty_directory() {
    let dir = tmp("emptydir");
    std::fs::create_dir_all(&dir).expect("mkdir");
    assert_eq!(run_recall(&dir, r#"{"prompt":"Person 1"}"#), "");
    let left: Vec<_> = std::fs::read_dir(&dir)
        .expect("readdir")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert!(left.is_empty(), "recall must not create files: {left:?}");
}

#[test]
fn recall_leaves_an_old_format_store_byte_identical() {
    // The default OpenOptions would migrate this V5 snapshot in place and write
    // a .bak. A prompt hook must only read.
    let bytes = include_bytes!("../../core-api/tests/fixtures/golden_v5.bin");
    let dir = tmp("v5");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join("snapshot.bin"), bytes).expect("snapshot");
    std::fs::write(dir.join("wal.bin"), b"").expect("wal");

    assert_eq!(run_recall(&dir, r#"{"prompt":"Person 1"}"#), "");
    assert_eq!(
        std::fs::read(dir.join("snapshot.bin")).expect("reread"),
        bytes,
        "recall must not rewrite the snapshot"
    );
    assert!(
        !dir.join("snapshot.bin.bak").exists(),
        "recall must not write a .bak"
    );
}

#[test]
fn recall_is_silent_when_nothing_matches_or_store_missing() {
    let dir = tmp("silent");
    run_demo(&dir).expect("demo");
    assert_eq!(run_recall(&dir, r#"{"prompt":"zzqx nothing here"}"#), "");
    assert_eq!(run_recall(&tmp("absent"), r#"{"prompt":"Person 1"}"#), "");
    assert_eq!(run_recall(&dir, "not json"), "");
}
