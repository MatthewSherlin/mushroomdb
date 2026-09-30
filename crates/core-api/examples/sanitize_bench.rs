//! The harness behind the `sanitize` ordering in `digest`.
//!
//! 0.6.9 widened `sanitize` from a bare `is_ascii_control()` to the full
//! bidi/zero-width class, and every character of every digest string began
//! falling through six `matches!` arms that a plain ASCII byte can never hit.
//! 0.6.10 reordered it to test `is_ascii()` first and return.
//!
//! The reorder is provably behaviour-preserving — the `matches!` set's minimum
//! member is U+0085, so it is disjoint from ASCII, and `is_ascii_control()` is
//! false above U+007F — which means no behavioural test can tell the two forms
//! apart. Only a measurement can, and a measurement nobody can re-run is not
//! evidence. Hence this file.
//!
//! ```text
//! cargo run --release -p mushroomdb --example sanitize_bench
//! ```
//!
//! Figures quoted in the 0.6.10 changelog came from this harness on an Apple
//! Silicon laptop. They are a ratio between three predicates measured in one
//! process, not a portable number: re-run it rather than trusting the absolute
//! microseconds, and expect the *ordering* to hold while the magnitudes move.

use core_api::digest::sanitize;
use std::time::Instant;

/// 0.6.8: ASCII controls only.
fn is_forging_0_6_8(c: char) -> bool {
    c.is_ascii_control()
}

/// 0.6.9: the full class, ASCII falling through every arm.
fn is_forging_0_6_9(c: char) -> bool {
    c.is_ascii_control()
        || matches!(c,
            '\u{0085}'
            | '\u{200b}'..='\u{200f}'
            | '\u{2028}' | '\u{2029}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
            | '\u{feff}'
        )
}

/// 0.6.10: the same class, ASCII answered and returned first.
fn is_forging_0_6_10(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_control();
    }
    matches!(c,
        '\u{0085}'
        | '\u{200b}'..='\u{200f}'
        | '\u{2028}' | '\u{2029}'
        | '\u{202a}'..='\u{202e}'
        | '\u{2066}'..='\u{2069}'
        | '\u{feff}'
    )
}

fn sanitize_with(f: fn(char) -> bool, s: &str) -> String {
    s.chars().map(|c| if f(c) { ' ' } else { c }).collect()
}

fn main() {
    // Representative digest text: paths, identifiers, counts, addresses. A real
    // digest is ASCII-dominant, which is the whole point — the 0.6.9 form pays
    // the Unicode arms on every byte that can never match them.
    let line = "crates/core-api/src/db.rs · fn find_similar_vector_filtered · \
                1,204 commits · alice@example.com";
    let corpus: String = std::iter::repeat_n(line, 4_000)
        .collect::<Vec<_>>()
        .join("\n");
    let reps = 50;

    println!("corpus {} bytes, {} reps\n", corpus.len(), reps);

    // Equivalence first: a speed claim about a predicate that answers
    // differently is not a speed claim about anything.
    let mut disagreements = 0usize;
    for cp in 0u32..=0x10_FFFF {
        if let Some(c) = char::from_u32(cp) {
            if is_forging_0_6_9(c) != is_forging_0_6_10(c) {
                disagreements += 1;
            }
        }
    }
    println!("0.6.9 vs 0.6.10 over every code point: {disagreements} disagreements\n");
    assert_eq!(disagreements, 0, "the reorder must be behaviour-preserving");

    // The shipped function must produce exactly what the 0.6.10 predicate does,
    // or the timings below are of a copy that is not what runs in production.
    assert_eq!(
        sanitize(&corpus),
        sanitize_with(is_forging_0_6_10, &corpus),
        "digest::sanitize must match the 0.6.10 predicate"
    );

    for (name, f) in [
        (
            "0.6.8  ascii-control only ",
            is_forging_0_6_8 as fn(char) -> bool,
        ),
        ("0.6.9  full class, no fast", is_forging_0_6_9),
        ("0.6.10 ascii-first        ", is_forging_0_6_10),
    ] {
        // One warm pass so the first measured rep is not paying for cold pages.
        let _ = sanitize_with(f, &corpus);
        let t = Instant::now();
        let mut sink = 0usize;
        for _ in 0..reps {
            sink += sanitize_with(f, &corpus).len();
        }
        let per = t.elapsed() / reps;
        println!("{name} {per:>10.2?}   (checksum {sink})");
    }

    // The shipped function itself, not a re-implementation of it.
    let _ = sanitize(&corpus);
    let t = Instant::now();
    let mut sink = 0usize;
    for _ in 0..reps {
        sink += sanitize(&corpus).len();
    }
    let per = t.elapsed() / reps;
    println!("digest::sanitize (shipped) {per:>10.2?}   (checksum {sink})");
}
