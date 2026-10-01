//! Build the store `benchmarks/hook-latency/PROCEDURE.md` describes.
//!
//!   cargo run --release -p mushroomdb-cli --example hook_latency_store -- <dir> <notes>
//!
//! Deterministic: a seeded generator, so the same arguments build the same
//! store. Refuses a directory that already exists.
use core_api::memory_schema::memory_defaults;
use core_api::{GraphDb, Value};

const PEOPLE: usize = 1_000;
const VOCABULARY: usize = 2_000;
const WORDS_PER_NOTE: usize = 12;
const CHUNK: usize = 2_000;
const SEED: u64 = 7;

/// The generator `core_rules::suggest` uses for sampling, restated: a 64-bit
/// LCG. Good enough to spread words; not for anything else.
fn next(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    *state >> 33
}

/// Word `i` of the vocabulary: pronounceable, distinct, and never a stopword.
fn word(i: usize) -> String {
    const SYLLABLES: [&str; 10] = ["ka", "lo", "mi", "ne", "pu", "ra", "so", "ti", "vu", "ze"];
    let mut out = String::from("w");
    let mut n = i;
    for _ in 0..4 {
        out.push_str(SYLLABLES[n % 10]);
        n /= 10;
    }
    out
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: hook_latency_store <dir> <notes>");
        std::process::exit(2);
    }
    let dir = std::path::PathBuf::from(&args[1]);
    let notes: usize = args[2].parse().expect("<notes> must be a number");
    if dir.exists() {
        eprintln!("{} already exists; pass a fresh path", dir.display());
        std::process::exit(2);
    }

    let mut db = GraphDb::open(&dir).expect("open");
    db.apply_schema(&memory_defaults()).expect("schema");

    let mut batch = db.batch();
    for p in 0..PEOPLE {
        batch.insert_node(
            "Person",
            &format!("person-{p:04}"),
            vec![(
                "name".into(),
                Value::Str(format!("{} {}", word(p), word(p + PEOPLE))),
            )],
        );
    }
    batch.commit().expect("people");

    let mut state = SEED;
    let mut written = 0usize;
    while written < notes {
        let upto = (written + CHUNK).min(notes);
        let mut batch = db.batch();
        for n in written..upto {
            let text: Vec<String> = (0..WORDS_PER_NOTE)
                .map(|_| word((next(&mut state) as usize) % VOCABULARY))
                .collect();
            let key = format!("note:{n:08}");
            batch.insert_node(
                "Note",
                &key,
                vec![
                    ("text".into(), Value::Str(text.join(" "))),
                    ("kind".into(), Value::Str("note".into())),
                    ("ts".into(), Value::Int(1_759_000_000 + n as i64)),
                ],
            );
            let person = format!("person-{:04}", (next(&mut state) as usize) % PEOPLE);
            batch.insert_edge("ABOUT", &key, &person);
        }
        batch.commit().expect("notes");
        written = upto;
    }
    db.snapshot().expect("snapshot");
    println!(
        "built {} people and {notes} notes at {}",
        PEOPLE,
        dir.display()
    );
}
