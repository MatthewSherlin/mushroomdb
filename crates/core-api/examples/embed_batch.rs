//! Several writes as one commit, and a write that is refused if a node moved.
//! Quoted on docs/site/rust.md. Run: cargo run -p mushroomdb --example embed_batch

use core_api::{BatchOp, GraphDb, GraphError, Precondition, Value};

fn main() -> core_api::Result<()> {
    let dir = std::env::temp_dir().join(format!("mushroomdb-embed-batch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = GraphDb::open(&dir)?;

    // docs:start
    // One WAL frame, one fsync: after a crash, all of it is there or none.
    db.write_batch(|batch| {
        batch
            .insert_node(
                "Person",
                "ada",
                vec![("name".into(), Value::Str("Ada".into()))],
            )
            .insert_node(
                "Person",
                "grace",
                vec![("name".into(), Value::Str("Grace".into()))],
            )
            .insert_edge("KNOWS", "ada", "grace");
    })?;

    // Compare-and-set: write only if `ada` has not changed since we read it.
    let seen = db.last_changed("ada").expect("ada exists");
    db.set_prop("ada", "name", Value::Str("Ada L.".into()))?; // someone else writes first

    let refused = db.write_batch_cas(
        vec![Precondition::NodeUnchangedSince {
            key: "ada".into(),
            expected: seen,
        }],
        vec![BatchOp::SetProp {
            key: "ada".into(),
            field: "name".into(),
            value: Value::Str("A.".into()),
        }],
    );
    assert!(matches!(refused, Err(GraphError::CasConflict { .. })));
    // docs:end

    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
