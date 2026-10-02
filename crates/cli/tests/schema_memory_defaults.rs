//! `schema apply --memory-defaults` upgrades an existing store on request —
//! and only on request.
use cli::run_schema_apply_memory_defaults;
use core_api::GraphDb;

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "mdb-schemadef-{name}-{}-{nanos}",
        std::process::id()
    ))
}

#[test]
fn an_existing_store_is_upgraded_only_when_asked() {
    let dir = tmp("optin");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("Person", "p1", vec![]).unwrap();
        // Opening and writing must not declare anything on its own.
        assert!(
            db.fulltext_pairs().is_empty(),
            "a plain open declared full-text: {:?}",
            db.fulltext_pairs()
        );
    }
    let report = run_schema_apply_memory_defaults(&dir).expect("apply");
    assert!(
        report.contains("Person"),
        "the report must name what it declared: {report}"
    );
    let db = GraphDb::open(&dir).unwrap();
    assert!(db.fulltext_pairs().iter().any(|(l, _)| l == "Person"));
}
