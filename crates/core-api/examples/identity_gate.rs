//! The `SAME_AS` quality gate — spec §8.3, pre-registered in
//! `benchmarks/identity/PREREGISTRATION.md` before its first run.
//!
//! ```text
//! cargo run --release -p mushroomdb --example identity_gate -- benchmarks/identity/labelled.json
//! ```
//!
//! Writes every labelled node through `remember` — `entities[]` for a node
//! whose label is known, `about` for a stub — on a store carrying the memory
//! defaults and the identity preset, exactly as a session would. Then scores
//! two predictions against the labels: the pairwise `SAME_AS` claims, and
//! co-membership in the complete-linkage identities `analyze` reports.
//!
//! Prints the summary as Markdown on stdout. Exits 1 when a floor is missed,
//! so a run that fails cannot be mistaken for one that passed.
use core_api::memory::identity::{identity_clusters, same_as_pairs, SAME_AS_FLOOR};
use core_api::memory::remember::{remember, EntityIn, RememberInput};
use core_api::memory_schema::{memory_defaults, memory_identity};
use core_api::{GraphDb, Value};
use std::collections::{BTreeMap, BTreeSet};

/// The pre-registered floors. Changing one after the first run is an
/// amendment, dated in the pre-registration, never an edit here alone.
const PRECISION_FLOOR: f64 = 0.90;
const RECALL_FLOOR: f64 = 0.40;
const MIN_TRUE_POSITIVES: usize = 6;

struct Node {
    key: String,
    label: String,
    via: String,
    name: Option<String>,
    aliases: Vec<String>,
    identity: String,
    category: String,
}

type Pair = (String, String);

fn pair(a: &str, b: &str) -> Pair {
    if a <= b {
        (a.to_string(), b.to_string())
    } else {
        (b.to_string(), a.to_string())
    }
}

fn load(path: &str) -> Vec<Node> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    let doc: serde_json::Value = serde_json::from_str(&text).expect("labelled set is JSON");
    let s = |n: &serde_json::Value, f: &str| n[f].as_str().map(str::to_string);
    doc["nodes"]
        .as_array()
        .expect("nodes array")
        .iter()
        .map(|n| Node {
            key: s(n, "key").expect("key"),
            label: s(n, "label").expect("label"),
            via: s(n, "via").expect("via"),
            name: s(n, "name"),
            aliases: n["aliases"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            identity: s(n, "identity").expect("identity"),
            category: s(n, "category").expect("category"),
        })
        .collect()
}

/// `(true positives, false positives, false negatives)` as pair sets.
fn score(predicted: &BTreeSet<Pair>, truth: &BTreeSet<Pair>) -> (Vec<Pair>, Vec<Pair>, Vec<Pair>) {
    (
        predicted.intersection(truth).cloned().collect(),
        predicted.difference(truth).cloned().collect(),
        truth.difference(predicted).cloned().collect(),
    )
}

fn ratio(n: usize, d: usize) -> f64 {
    if d == 0 {
        0.0
    } else {
        n as f64 / d as f64
    }
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: identity_gate <labelled.json>");
    let nodes = load(&path);
    let dir = std::env::temp_dir().join(format!("identity-gate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = GraphDb::open(&dir).expect("open");
    db.apply_schema(&memory_defaults()).expect("defaults");
    db.apply_schema(&memory_identity())
        .expect("identity preset");

    for (i, n) in nodes.iter().enumerate() {
        let text = format!("labelled node {i}");
        let about = vec![n.key.clone()];
        let mut props = BTreeMap::new();
        if let Some(name) = &n.name {
            props.insert("name".to_string(), Value::Str(name.clone()));
        }
        let entities = vec![EntityIn {
            key: n.key.clone(),
            label: n.label.clone(),
            props,
            aliases: n.aliases.clone(),
        }];
        let (about, entities): (&[String], &[EntityIn]) = match n.via.as_str() {
            "stub" => (&about, &[]),
            "entity" => (&[], &entities),
            other => panic!("{}: via must be entity or stub, got {other}", n.key),
        };
        remember(
            &mut db,
            &RememberInput {
                text: &text,
                about,
                kind: "note",
                ts: 1_759_000_000,
                source: Some("identity-gate"),
                entities,
                facts: &[],
            },
        )
        .unwrap_or_else(|e| panic!("{}: {e}", n.key));
    }

    let keys: Vec<String> = nodes.iter().map(|n| n.key.clone()).collect();
    let category: BTreeMap<&str, &str> = nodes
        .iter()
        .map(|n| (n.key.as_str(), n.category.as_str()))
        .collect();
    let mut truth: BTreeSet<Pair> = BTreeSet::new();
    for (i, a) in nodes.iter().enumerate() {
        for b in &nodes[i + 1..] {
            if a.identity == b.identity {
                truth.insert(pair(&a.key, &b.key));
            }
        }
    }
    let pairwise: BTreeSet<Pair> = same_as_pairs(&db, &keys)
        .into_iter()
        .map(|p| pair(&p.a, &p.b))
        .collect();
    let mut clustered: BTreeSet<Pair> = BTreeSet::new();
    for c in identity_clusters(&db, SAME_AS_FLOOR).clusters {
        for (i, a) in c.members.iter().enumerate() {
            for b in &c.members[i + 1..] {
                clustered.insert(pair(a, b));
            }
        }
    }

    let mut out = String::new();
    out.push_str("# SAME_AS quality gate\n\n");
    out.push_str(&format!(
        "Labelled set: `{path}` — {} nodes, {} labelled pairs, {} of them the same entity. \
         Identity preset: `Overlap` on `aliases` at ≥ {SAME_AS_FLOOR}.\n\n",
        nodes.len(),
        nodes.len() * (nodes.len() - 1) / 2,
        truth.len()
    ));
    out.push_str(
        "## Gate\n\nThe pre-registered §8.3 floors, each applied to both predictions:\n\n",
    );
    out.push_str(&format!(
        "1. precision ≥ {PRECISION_FLOOR:.2}\n2. recall ≥ {RECALL_FLOOR:.2}\n\
         3. at least {MIN_TRUE_POSITIVES} true positives\n\n"
    ));
    out.push_str("| prediction | TP | FP | FN | precision | recall | verdict |\n|---|---|---|---|---|---|---|\n");
    let mut passed = true;
    let mut detail = String::new();
    for (name, predicted) in [
        ("pairwise SAME_AS", &pairwise),
        ("identity clusters", &clustered),
    ] {
        let (tp, fp, fnn) = score(predicted, &truth);
        let precision = ratio(tp.len(), tp.len() + fp.len());
        let recall = ratio(tp.len(), truth.len());
        let ok = precision >= PRECISION_FLOOR
            && recall >= RECALL_FLOOR
            && tp.len() >= MIN_TRUE_POSITIVES;
        passed &= ok;
        out.push_str(&format!(
            "| {name} | {} | {} | {} | {precision:.3} | {recall:.3} | **{}** |\n",
            tp.len(),
            fp.len(),
            fnn.len(),
            if ok { "PASSED" } else { "FAILED" }
        ));
        for (kind, set) in [("false positive", &fp), ("false negative", &fnn)] {
            for (a, b) in set {
                detail.push_str(&format!(
                    "| {name} | {kind} | `{a}` | `{b}` | {} / {} |\n",
                    category[a.as_str()],
                    category[b.as_str()]
                ));
            }
        }
    }
    out.push_str(&format!(
        "\n**Verdict: {}**\n\n## Every miss\n\n| prediction | kind | a | b | categories |\n|---|---|---|---|---|\n{detail}",
        if passed { "PASSED" } else { "FAILED" }
    ));
    print!("{out}");
    let _ = std::fs::remove_dir_all(&dir);
    if !passed {
        std::process::exit(1);
    }
}
