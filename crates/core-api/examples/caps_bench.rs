//! v0.10 PF-07 (ledger row 62) — measure the four engine caps at scale.
//!
//! Row 62 named four reasoned first guesses: `MAX_QUERY_TERMS` (24, `recall.rs`),
//! the half-coverage floor (`recall_rows`), `MAX_FULLTEXT_PAIRS` (32,
//! `remember.rs`) and `MAX_PROVISIONAL_PER_COMMIT` (20, `remember.rs`), plus
//! "`analyze` is unmeasured above 100,000 nodes". This example reads every count
//! from the engine's own reports (`RecallRows::terms`, `RememberReport::
//! provisional`, `provisional_capped`, `fulltext_declared`) — it does not
//! re-implement the tokenizer. The one deliberate duplication is the coverage
//! test in `floor`, which re-asks `recall_rows`' own retention rule at several
//! thresholds, because the engine only reports the threshold it ships.
//!
//! Run (release only; numbers from a debug build mean nothing):
//!   CAPS_N=1000000 CAPS_DIR=/tmp/caps cargo run --release -p mushroomdb --example caps_bench
//! Env: CAPS_N (nodes, default 100000), CAPS_DIR (scratch, default temp dir),
//!      CAPS_DOCS (directory of .md files used as stand-in prompts, default docs/site),
//!      CAPS_PHASES (comma list of: build,terms,floor,fulltext,provisional,analyze; default all).
use core_storage::fulltext::{stem, value_tokens_stemmed_with_positions};
use mushroomdb::memory::recall::{recall_rows, MAX_QUERY_TERMS};
use mushroomdb::memory::remember::{remember, EntityIn, RememberInput};
use mushroomdb::memory_schema::{memory_defaults, NAME_FIELD};
use mushroomdb::{GraphDb, Value};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

type Db = GraphDb<core_storage::fs::RealFs>;

const VOCAB: usize = 3_000;
const CHUNK: usize = 10_000;
/// The pair count the memory schema ships, and the engine's ceiling.
const DEFAULT_PAIRS: usize = 8;
const ENGINE_MAX_PAIRS: usize = 32;
const EXTRA_LABELS: usize = ENGINE_MAX_PAIRS - DEFAULT_PAIRS;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

struct Zipf {
    vocab: Vec<String>,
    cumulative: Vec<f64>,
}
impl Zipf {
    fn new(rng: &mut Rng) -> Self {
        let mut set = std::collections::BTreeSet::new();
        while set.len() < VOCAB {
            let len = 3 + rng.below(7);
            let word: String = (0..len)
                .map(|i| {
                    let pool: &[u8] = if i % 2 == 0 {
                        b"bcdfghjklmnpqrtvwz"
                    } else {
                        b"aeiou"
                    };
                    pool[rng.below(pool.len())] as char
                })
                .collect();
            // Ends in a letter the stemmer leaves alone, so stems stay distinct.
            set.insert(format!("{word}x"));
        }
        let vocab: Vec<String> = set.into_iter().collect();
        let total: f64 = (0..VOCAB).map(|i| 1.0 / (i + 1) as f64).sum();
        let mut acc = 0.0;
        let cumulative = (0..VOCAB)
            .map(|i| {
                acc += 1.0 / (i + 1) as f64 / total;
                acc
            })
            .collect();
        Zipf { vocab, cumulative }
    }
    fn word(&self, rng: &mut Rng) -> &str {
        let r = rng.unit();
        let idx = self.cumulative.partition_point(|c| *c < r).min(VOCAB - 1);
        &self.vocab[idx]
    }
    fn text(&self, rng: &mut Rng, words: usize) -> String {
        (0..words)
            .map(|_| self.word(rng))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}
fn p95(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[(v.len() * 95 / 100).min(v.len() - 1)]
}
fn ms(d: Duration) -> String {
    format!("{:.3} ms", d.as_secs_f64() * 1000.0)
}
fn rss_mib() -> f64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok();
    out.and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<f64>().ok())
        .map_or(0.0, |kib| kib / 1024.0)
}
fn dir_bytes(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| {
                    let p = e.path();
                    if p.is_dir() {
                        dir_bytes(&p)
                    } else {
                        e.metadata().map_or(0, |m| m.len())
                    }
                })
                .sum()
        })
        .unwrap_or(0)
}

/// A needle word no vocabulary word can collide with (vocabulary words end in
/// `x` after a vowel/consonant alternation of letters from a fixed pool; these
/// start `qq`).
fn needle_word(target: usize, slot: usize) -> String {
    format!("qq{}{}x", base26(target), base26(slot))
}
fn base26(mut n: usize) -> String {
    let mut s = String::new();
    loop {
        s.push((b'a' + (n % 26) as u8) as char);
        n /= 26;
        if n == 0 {
            break;
        }
    }
    s
}

const NEEDLES: usize = 300;
const NEEDLE_WORDS: usize = 4;

/// Memory-shaped store: Person 40 %, Org 10 %, Project 20 %, Concept 20 %,
/// Event 10 %, ~1.6 edges/node. With `spread`, the Org, Project and Event nodes (40 %) go to
/// `EXTRA_LABELS` additional labels (each with its own declared `name` pair) so
/// the declared surface is the engine's maximum of 32 with the same amount of
/// indexed text.
fn build(dir: &Path, n: usize, spread: bool, seed: u64) -> Db {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
    let mut db = GraphDb::open(dir).unwrap();
    db.apply_schema(&memory_defaults()).unwrap();
    let mut rng = Rng(seed);
    let zipf = Zipf::new(&mut Rng(seed ^ 0x9e37_79b9));
    let base = ["Person", "Org", "Project", "Concept", "Event"];
    let extra: Vec<String> = (0..EXTRA_LABELS).map(|i| format!("Extra{i:02}")).collect();
    if spread {
        for l in &extra {
            db.enable_fulltext(l, NAME_FIELD).unwrap();
        }
    }
    db.enable_index("Person", "team_id").unwrap();
    let t = Instant::now();
    let mut done = 0usize;
    while done < n {
        let upto = (done + CHUNK).min(n);
        db.write_batch(|b| {
            for i in done..upto {
                let label: String = if spread && matches!(i % 10, 4..=6 | 9) {
                    extra[i % EXTRA_LABELS].clone()
                } else {
                    let pick = match i % 10 {
                        0..=3 => 0,
                        4 => 1,
                        5 | 6 => 2,
                        7 | 8 => 3,
                        _ => 4,
                    };
                    base[pick].to_string()
                };
                let mut props = vec![
                    (
                        "name".to_string(),
                        Value::Str(format!("{} {}", zipf.word(&mut rng), zipf.word(&mut rng))),
                    ),
                    (
                        "team_id".to_string(),
                        Value::Str(format!("team-{}", i % 12_500)),
                    ),
                ];
                if label == "Concept" {
                    let k = 5 + rng.below(6);
                    props.push(("summary".to_string(), Value::Str(zipf.text(&mut rng, k))));
                }
                b.insert_node(&label, &format!("n{i:08}"), props);
            }
        })
        .unwrap();
        done = upto;
    }
    // Needles: one Concept per target, name = four words found nowhere else.
    db.write_batch(|b| {
        for t in 0..NEEDLES {
            let name = (0..NEEDLE_WORDS)
                .map(|s| needle_word(t, s))
                .collect::<Vec<_>>()
                .join(" ");
            b.insert_node(
                "Concept",
                &format!("needle{t:04}"),
                vec![("name".to_string(), Value::Str(name))],
            );
        }
    })
    .unwrap();
    // ~1.6 edges per node: a KNOWS ring plus one MEMBER_OF per 2 nodes, in chunks.
    let mut done = 0usize;
    while done < n {
        let upto = (done + CHUNK).min(n);
        db.write_batch(|b| {
            for i in done..upto {
                b.insert_edge(
                    "KNOWS",
                    &format!("n{i:08}"),
                    &format!("n{:08}", (i + 1) % n),
                );
                if i % 2 == 0 {
                    b.insert_edge(
                        "MEMBER_OF",
                        &format!("n{i:08}"),
                        &format!("n{:08}", (i * 7 + 3) % n),
                    );
                }
                if i % 10 < 1 {
                    b.insert_edge(
                        "RELATED_TO",
                        &format!("n{i:08}"),
                        &format!("n{:08}", (i * 13 + 5) % n),
                    );
                }
            }
        })
        .unwrap();
        done = upto;
    }
    println!(
        "build: {n} nodes, spread={spread}, pairs={} in {:.1}s, rss {:.0} MiB",
        db.fulltext_pairs().len(),
        t.elapsed().as_secs_f64(),
        rss_mib()
    );
    db.snapshot().unwrap();
    println!(
        "snapshot written, store {:.1} MiB",
        dir_bytes(dir) as f64 / 1_048_576.0
    );
    db
}

fn open_times(dir: &Path, runs: usize) -> Vec<Duration> {
    (0..runs)
        .map(|_| {
            let t = Instant::now();
            let db = GraphDb::open(dir).unwrap();
            let e = t.elapsed();
            std::hint::black_box(db.stats());
            e
        })
        .collect()
}

// --- cap 1: MAX_QUERY_TERMS -------------------------------------------------

fn paragraphs(docs: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut files: Vec<PathBuf> = std::fs::read_dir(docs)
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    files.sort();
    for f in files {
        if f.extension().is_some_and(|e| e == "md") {
            if let Ok(text) = std::fs::read_to_string(&f) {
                for p in text.split("\n\n") {
                    let p = p.trim();
                    if p.split_whitespace().count() >= 3
                        && !p.starts_with('|')
                        && !p.starts_with("```")
                    {
                        out.push(p.to_string());
                    }
                }
            }
        }
    }
    out
}

fn terms(db: &Db, docs: &Path, zipf: &Zipf, seed: u64) {
    println!("\n## cap 1: MAX_QUERY_TERMS = {MAX_QUERY_TERMS}");
    // (a) Saturation: how many terms do real-looking prompts produce?
    let paras = paragraphs(docs);
    let mut counts: Vec<usize> = Vec::new();
    let mut lat: Vec<Duration> = Vec::new();
    for p in &paras {
        let t = Instant::now();
        let r = recall_rows(db, p);
        lat.push(t.elapsed());
        counts.push(r.terms);
    }
    counts.sort();
    let n = counts.len();
    let saturated = counts.iter().filter(|c| **c == MAX_QUERY_TERMS).count();
    println!(
        "stand-in prompts: {n} paragraphs of {}; terms/prompt min {} p50 {} p90 {} p99 {} max {}; \
         at the cap {} ({:.1} %); recall p50 {} p95 {}",
        docs.display(),
        counts[0],
        counts[n / 2],
        counts[n * 90 / 100],
        counts[n * 99 / 100],
        counts[n - 1],
        saturated,
        100.0 * saturated as f64 / n as f64,
        ms(median(lat.clone())),
        ms(p95(lat)),
    );
    // (b) Cost against term count, including past the cap (query built the way
    // `topic_query` builds it: terms OR'd), over every declared field.
    let mut fields: Vec<String> = db.fulltext_pairs().into_iter().map(|(_, f)| f).collect();
    fields.sort();
    fields.dedup();
    let mut rng = Rng(seed ^ 0x51);
    println!("| terms | word source | p50 | p95 | note |\n|---|---|---|---|---|");
    for source in ["zipf", "head-100"] {
        for count in [1usize, 2, 4, 8, 12, 24, 32, 48, 96] {
            let mut samples = Vec::new();
            for _ in 0..15 {
                let mut seen = HashSet::new();
                let mut words: Vec<String> = Vec::new();
                while words.len() < count {
                    let w = if source == "zipf" {
                        zipf.word(&mut rng).to_string()
                    } else {
                        zipf.vocab[rng.below(100)].clone()
                    };
                    if seen.insert(stem(&w)) {
                        words.push(w);
                    }
                }
                let query = words.join(" OR ");
                let t = Instant::now();
                for field in &fields {
                    std::hint::black_box(db.search_hybrid(
                        field,
                        &query,
                        "embedding",
                        &[],
                        None,
                        6,
                    ));
                }
                samples.push(t.elapsed());
            }
            let note = if count > MAX_QUERY_TERMS {
                "past the cap: not reachable through recall"
            } else if count == MAX_QUERY_TERMS {
                "the cap"
            } else {
                ""
            };
            println!(
                "| {count} | {source} | {} | {} | {note} |",
                ms(median(samples.clone())),
                ms(p95(samples))
            );
        }
    }
    // (c) The engine's own count for an over-long topic.
    let long = zipf.text(&mut Rng(seed ^ 0x77), 400);
    println!(
        "400-word topic -> RecallRows::terms = {}",
        recall_rows(db, &long).terms
    );
}

// --- cap 2: the half-coverage floor ------------------------------------------

/// `recall_rows`' own retention rule, asked at an arbitrary threshold.
fn coverage_of(db: &Db, fields: &[String], key: &str, stemmed: &[String]) -> usize {
    let mut tokens: HashSet<String> = HashSet::new();
    for field in fields {
        if let Some(v) = db.get_prop(key, field) {
            tokens.extend(
                value_tokens_stemmed_with_positions(&v)
                    .into_iter()
                    .map(|(t, _)| t),
            );
        }
    }
    stemmed.iter().filter(|t| tokens.contains(*t)).count()
}

fn candidates(db: &Db, fields: &[String], query: &str) -> HashSet<String> {
    let mut keys = HashSet::new();
    for field in fields {
        for (k, _) in db.search_hybrid(field, query, "embedding", &[], None, 6) {
            keys.insert(k);
        }
    }
    keys
}

fn floor(db: &Db, zipf: &Zipf, seed: u64) {
    println!("\n## cap 2: the half-coverage floor (present * 2 >= terms)");
    let mut fields: Vec<String> = db.fulltext_pairs().into_iter().map(|(_, f)| f).collect();
    fields.sort();
    fields.dedup();
    // Floors as num/den of the topic's terms; the shipped one is 1/2.
    let floors: [(&str, usize, usize); 5] = [
        ("1/4", 1, 4),
        ("1/3", 1, 3),
        ("1/2 (shipped)", 1, 2),
        ("2/3", 2, 3),
        ("all", 1, 1),
    ];
    let mut rng = Rng(seed ^ 0xf1);

    // Recall side: a planted target found by `real` of `total` terms, the rest
    // common words the target does not hold.
    println!(
        "\nplanted-target recall (target is the only node with its words; {NEEDLES} per cell)"
    );
    println!(
        "| terms (real/total) | 1/4 | 1/3 | 1/2 (shipped) | 2/3 | all |\n|---|---|---|---|---|---|"
    );
    for (real, total) in [
        (1usize, 1usize),
        (2, 2),
        (1, 2),
        (2, 3),
        (1, 3),
        (2, 4),
        (1, 4),
        (3, 5),
        (2, 5),
        (1, 5),
    ] {
        let mut found = [0usize; 5];
        for target in 0..NEEDLES {
            let mut words: Vec<String> = (0..real).map(|s| needle_word(target, s)).collect();
            let mut seen: HashSet<String> = words.iter().map(|w| stem(w)).collect();
            while words.len() < total {
                let w = zipf.vocab[rng.below(30)].clone();
                if seen.insert(stem(&w)) {
                    words.push(w);
                }
            }
            let stemmed: Vec<String> = words.iter().map(|w| stem(w)).collect();
            let query = words.join(" OR ");
            let key = format!("needle{target:04}");
            if candidates(db, &fields, &query).contains(&key) {
                let present = coverage_of(db, &fields, &key, &stemmed);
                for (i, (_, num, den)) in floors.iter().enumerate() {
                    if present * den >= total * num {
                        found[i] += 1;
                    }
                }
            }
        }
        println!(
            "| {real}/{total} | {} | {} | {} | {} | {} |",
            found[0], found[1], found[2], found[3], found[4]
        );
    }

    // Precision side: topics of unrelated mid/rare-tail words. Any survivor is
    // a false positive; count topics (of 300) with at least one.
    println!("\nfalse positives: 300 noise topics per row of unrelated vocabulary words (rank 100..3000), topics with >= 1 surviving hit");
    println!("| terms | 1/4 | 1/3 | 1/2 (shipped) | 2/3 | all |\n|---|---|---|---|---|---|");
    for total in [1usize, 2, 3, 4, 6, 10] {
        let mut hit_topics = [0usize; 5];
        for _ in 0..300 {
            let mut words: Vec<String> = Vec::new();
            let mut seen = HashSet::new();
            while words.len() < total {
                let w = zipf.vocab[100 + rng.below(VOCAB - 100)].clone();
                if seen.insert(stem(&w)) {
                    words.push(w);
                }
            }
            let stemmed: Vec<String> = words.iter().map(|w| stem(w)).collect();
            let query = words.join(" OR ");
            let mut survives = [false; 5];
            for key in candidates(db, &fields, &query) {
                let present = coverage_of(db, &fields, &key, &stemmed);
                for (i, (_, num, den)) in floors.iter().enumerate() {
                    if present * den >= total * num {
                        survives[i] = true;
                    }
                }
            }
            for i in 0..5 {
                if survives[i] {
                    hit_topics[i] += 1;
                }
            }
        }
        println!(
            "| {total} | {} | {} | {} | {} | {} |",
            hit_topics[0], hit_topics[1], hit_topics[2], hit_topics[3], hit_topics[4]
        );
    }
}

// --- caps 3 and 4: remember ---------------------------------------------------

fn note_text(zipf: &Zipf, rng: &mut Rng) -> String {
    format!("{} noted", zipf.text(rng, 10))
}

fn remember_once(
    db: &mut Db,
    text: &str,
    about: &[String],
    entities: &[EntityIn],
    ts: i64,
) -> mushroomdb::memory::remember::RememberReport {
    remember(
        db,
        &RememberInput {
            text,
            about,
            kind: "note",
            ts,
            source: None,
            entities,
            facts: &[],
        },
    )
    .unwrap()
}

fn provisional(db: &mut Db, zipf: &Zipf, seed: u64) {
    println!("\n## cap 4: MAX_PROVISIONAL_PER_COMMIT = 20");
    let mut rng = Rng(seed ^ 0xa1);
    let ks = [0usize, 1, 5, 10, 20, 21, 40, 100];
    const ROUNDS: usize = 60;
    // Round-robin over the key counts, so drift in the disk's fsync latency
    // lands on every row alike instead of on whichever ran last.
    let mut samples: Vec<Vec<Duration>> = vec![Vec::new(); ks.len()];
    let mut reports = vec![(0usize, 0usize); ks.len()];
    let mut ts = 1_800_000_000i64;
    let mut serial = 0usize;
    for _ in 0..ROUNDS {
        for (row, k) in ks.iter().enumerate() {
            ts += 1;
            let about: Vec<String> = (0..*k)
                .map(|_| {
                    serial += 1;
                    format!("prov-{serial:08}")
                })
                .collect();
            let text = note_text(zipf, &mut rng);
            let t = Instant::now();
            let r = remember_once(db, &text, &about, &[], ts);
            samples[row].push(t.elapsed());
            reports[row] = (r.provisional.len(), r.provisional_capped.len());
        }
    }
    println!("| unknown `about` keys in one call | calls | p50 | p95 | stubs created (engine report) | capped (engine report) |\n|---|---|---|---|---|---|");
    for (row, k) in ks.iter().enumerate() {
        println!(
            "| {k} | {ROUNDS} | {} | {} | {} | {} |",
            ms(median(samples[row].clone())),
            ms(p95(samples[row].clone())),
            reports[row].0,
            reports[row].1
        );
    }
}

fn fulltext(a: &mut Db, b: &mut Db, zipf: &Zipf, seed: u64) {
    println!("\n## cap 3: MAX_FULLTEXT_PAIRS = {ENGINE_MAX_PAIRS}");
    let mut rng = Rng(seed ^ 0xb2);
    let (pairs_a, pairs_b) = (a.fulltext_pairs().len(), b.fulltext_pairs().len());
    // Per-call write cost, the two stores alternating call by call.
    let (mut sa, mut sb) = (Vec::new(), Vec::new());
    for i in 0..300 {
        let text = note_text(zipf, &mut rng);
        let about = vec![format!("n{:08}", rng.below(1000))];
        let t = Instant::now();
        remember_once(a, &text, &about, &[], 1_810_000_000 + i);
        sa.push(t.elapsed());
        let t = Instant::now();
        remember_once(b, &text, &about, &[], 1_810_000_000 + i);
        sb.push(t.elapsed());
    }
    for (name, pairs, s) in [("A", pairs_a, sa), ("B", pairs_b, sb)] {
        println!(
            "store {name}: declared pairs {pairs}; remember (1 known about key) p50 {} p95 {} over 300 alternating calls",
            ms(median(s.clone())),
            ms(p95(s))
        );
    }
    // Saturation: one call naming 40 fresh labels. The engine's report says
    // which it declared.
    for (name, db) in [("A", a), ("B", b)] {
        let entities: Vec<EntityIn> = (0..40)
            .map(|i| EntityIn {
                key: format!("fresh-label-entity-{i}-{name}"),
                label: format!("FreshLabel{i:02}{name}"),
                props: BTreeMap::from([(NAME_FIELD.to_string(), Value::Str(format!("fresh {i}")))]),
                aliases: Vec::new(),
            })
            .collect();
        let t = Instant::now();
        let r = remember_once(
            db,
            "a call that names forty fresh labels",
            &[],
            &entities,
            1_810_100_000,
        );
        println!(
            "store {name}: one call naming 40 fresh labels: declared {} (engine report), pairs now {}, took {}",
            r.fulltext_declared.len(),
            db.fulltext_pairs().len(),
            ms(t.elapsed())
        );
    }
}

fn declare_cost(db: &mut Db) {
    // The price of ONE declaration is a build over the label's existing nodes.
    println!("\n## cost of declaring one pair on an already-populated label");
    for (label, field) in [("Person", "team_id"), ("Org", "team_id")] {
        let t = Instant::now();
        match db.enable_fulltext(label, field) {
            Ok(()) => println!(
                "enable_fulltext({label}, {field}) on populated label: {}",
                ms(t.elapsed())
            ),
            Err(e) => println!("enable_fulltext({label}, {field}): {e}"),
        }
    }
}

// --- analyze ------------------------------------------------------------------

fn analyze(db: &Db) {
    println!("\n## analyze (the four kinds, budget_ms 0 as the tool runs them)");
    let t = Instant::now();
    let r = db.pagerank(&mushroomdb::PageRankConfig {
        budget_ms: 0,
        ..Default::default()
    });
    println!(
        "central (pagerank): {} nodes scored, converged={}, {}",
        r.scores.len(),
        r.converged,
        ms(t.elapsed())
    );
    let t = Instant::now();
    let r = db.degree_centrality(&mushroomdb::DegreeConfig {
        budget_ms: 0,
        ..Default::default()
    });
    println!(
        "degree: {} nodes scored, {}",
        r.scores.len(),
        ms(t.elapsed())
    );
    let t = Instant::now();
    let r = db.connected_components(&mushroomdb::WccConfig {
        budget_ms: 0,
        ..Default::default()
    });
    println!(
        "components (wcc): {} rows, {}",
        r.components.len(),
        ms(t.elapsed())
    );
    let t = Instant::now();
    let r = db.communities(&mushroomdb::LouvainConfig {
        budget_ms: 0,
        ..Default::default()
    });
    println!(
        "clusters (louvain): {} rows, {}",
        r.communities.len(),
        ms(t.elapsed())
    );
    println!("rss {:.0} MiB", rss_mib());
}

fn main() {
    let n: usize = std::env::var("CAPS_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100_000);
    let root = std::env::var("CAPS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("mdb-caps"));
    let docs =
        PathBuf::from(std::env::var("CAPS_DOCS").unwrap_or_else(|_| "docs/site".to_string()));
    let phases = std::env::var("CAPS_PHASES")
        .unwrap_or_else(|_| "build,terms,floor,fulltext,provisional,analyze".to_string());
    let want = |p: &str| phases.split(',').any(|x| x == p);
    let seed = 20_261_020u64;
    let zipf = Zipf::new(&mut Rng(seed ^ 0x9e37_79b9));
    let dir_a = root.join("a8");
    let dir_b = root.join("b32");
    println!(
        "# caps_bench N={n} build={} os={} cores={}",
        if cfg!(debug_assertions) {
            "DEBUG (invalid)"
        } else {
            "release"
        },
        std::env::consts::OS,
        std::thread::available_parallelism().map_or(0, |c| c.get())
    );

    if want("build") {
        drop(build(&dir_a, n, false, seed));
        drop(build(&dir_b, n, true, seed));
    }

    // Store A: 8 declared pairs. Reads and the remember caps run here.
    let a_opens = open_times(&dir_a, 5);
    println!(
        "\nopen from snapshot, 8 pairs: median {} (runs {})",
        ms(median(a_opens.clone())),
        a_opens
            .iter()
            .map(|d| ms(*d))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let b_opens = open_times(&dir_b, 5);
    println!(
        "open from snapshot, 32 pairs: median {} (runs {})",
        ms(median(b_opens.clone())),
        b_opens
            .iter()
            .map(|d| ms(*d))
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "store size: 8 pairs {:.1} MiB, 32 pairs {:.1} MiB",
        dir_bytes(&dir_a) as f64 / 1_048_576.0,
        dir_bytes(&dir_b) as f64 / 1_048_576.0
    );

    let mut a: Db = GraphDb::open(&dir_a).unwrap();
    println!(
        "store A: {} nodes, {} pairs, rss {:.0} MiB",
        a.stats().nodes_live,
        a.fulltext_pairs().len(),
        rss_mib()
    );
    // Read-only phases first, then the writes; the two stores take their
    // remember calls in the same order, each on a freshly opened handle.
    if want("terms") {
        terms(&a, &docs, &zipf, seed);
    }
    if want("floor") {
        floor(&a, &zipf, seed);
    }
    if want("analyze") {
        analyze(&a);
    }
    if want("provisional") {
        provisional(&mut a, &zipf, seed);
    }
    if want("fulltext") {
        let mut b: Db = GraphDb::open(&dir_b).unwrap();
        fulltext(&mut a, &mut b, &zipf, seed);
    }
    if want("fulltext") {
        declare_cost(&mut a);
    }
}
