//! v0.6.11 Task 11 — does a large *entity* store open slowly?
//!
//! Ledger row 36 recorded a 371 ms open and Task 1 reproduced it at 380 ms — on a
//! 524-file **code-graph** store with 11–12 full-text pairs including
//! `File.body`, the entire source text of every file. A 60-node demo entity store
//! opened in ~2 ms with zero section decodes.
//!
//! Neither figure decides anything about a 30k-entity store, which is what the
//! deployment actually is. Task 8 (guarding the duplicate full-text rebuild) was
//! deferred to this measurement: if a large entity store with full-text enabled
//! opens slowly, the guard is justified; if it opens in milliseconds, row 36 is
//! code-graph-only and 0.7's removal retires it.
//!
//! Run: `cargo run --release -p mushroomdb --example entity_open_bench`
use core_api::GraphDb;
use core_storage::types::Value;
use std::time::Instant;

fn main() {
    let n: usize = std::env::var("ENTITY_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30_000);
    let with_fulltext = std::env::var("NO_FULLTEXT").is_err();

    let dir = std::env::temp_dir().join(format!("mdb-entity-open-{n}-{with_fulltext}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    println!("building a {n}-entity store, fulltext={with_fulltext}...");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        if with_fulltext {
            // What a real entity store enables: `remember` ensures Note.text, and
            // hybrid_search needs a text field indexed.
            db.enable_fulltext("Talent", "bio").unwrap();
            db.enable_fulltext("Talent", "name").unwrap();
            db.enable_fulltext("Company", "name").unwrap();
        }
        let t = Instant::now();
        for i in 0..n {
            let label = if i % 10 == 0 { "Company" } else { "Talent" };
            db.insert_node(
                label,
                &format!("{}-{i}", label.to_lowercase()),
                vec![
                    ("name".to_string(), Value::Str(format!("name {i}"))),
                    (
                        "bio".to_string(),
                        // Representative of a talent profile: a paragraph, not a
                        // source file. This is the axis that separates an entity
                        // store from a code graph.
                        Value::Str(format!(
                            "Experienced practitioner number {i} with a background in \
                             design and delivery across several sectors, focused on \
                             outcomes and measurement."
                        )),
                    ),
                ],
            )
            .unwrap();
        }
        println!("  inserted in {:?}", t.elapsed());
        db.snapshot().unwrap();
        println!("  snapshotted");
    }

    // Cold-process opens are what a CLI or MCP invocation pays. In-process here,
    // but the file cache is warm either way, which matches how row 36 was read.
    let mut times = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        let db = GraphDb::open(&dir).unwrap();
        let elapsed = t.elapsed();
        std::hint::black_box(db.stats());
        times.push(elapsed);
    }
    times.sort();
    println!(
        "\nopen x5 (sorted): {:?}\n  median {:?}",
        times,
        times[times.len() / 2]
    );
}
