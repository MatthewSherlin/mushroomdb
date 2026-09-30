//! What do I already know about this?
//!
//! The 0.6.12 implementation began by extracting *identifiers* from the topic
//! — paths, `mod::name`, snake_case, backticked words — and returned nothing
//! when it found none. On a memory store that rejected every natural-language
//! topic, including a bare human name, while the note containing the word sat
//! indexed and reachable by Cypher. There is no identifier gate here.

use crate::GraphDb;
use core_storage::fs::Fs;
use std::collections::BTreeMap;

/// How many hits the digest will name.
const MAX_HITS: usize = 6;

/// Glue words dropped from a topic before it becomes a query.
///
/// The index's query grammar ANDs space-separated terms within one group —
/// there is no websearch-style stopword handling underneath it — so a natural
/// question passed through unchanged (`"who is Matthew Sherlin?"`) requires
/// the document to contain the word "who", which none does. Dropping these
/// before the terms are OR'd together is what lets a question find the
/// content word rather than demanding the whole sentence.
///
/// Duplicated from `repograph::recall::STOPWORDS` rather than shared with it:
/// this module exists precisely so `repograph` can be deleted whole later,
/// and importing from it here would grow back the dependency this move is
/// undoing.
const STOPWORDS: [&str; 146] = [
    "a",
    "about",
    "after",
    "again",
    "all",
    "also",
    "am",
    "an",
    "and",
    "any",
    "anything",
    "are",
    "as",
    "at",
    "back",
    "be",
    "because",
    "been",
    "before",
    "being",
    "below",
    "between",
    "both",
    "but",
    "by",
    "can",
    "cannot",
    "could",
    "did",
    "do",
    "does",
    "doing",
    "done",
    "down",
    "during",
    "each",
    "either",
    "else",
    "even",
    "ever",
    "every",
    "few",
    "for",
    "from",
    "further",
    "had",
    "has",
    "have",
    "having",
    "he",
    "her",
    "here",
    "hers",
    "him",
    "his",
    "how",
    "i",
    "if",
    "in",
    "into",
    "is",
    "it",
    "its",
    "itself",
    "just",
    "know",
    "let",
    "like",
    "may",
    "maybe",
    "me",
    "might",
    "more",
    "most",
    "much",
    "must",
    "my",
    "need",
    "no",
    "nor",
    "not",
    "now",
    "of",
    "off",
    "ok",
    "okay",
    "on",
    "once",
    "one",
    "only",
    "or",
    "other",
    "our",
    "out",
    "over",
    "own",
    "please",
    "same",
    "she",
    "should",
    "so",
    "some",
    "something",
    "such",
    "sure",
    "tell",
    "than",
    "thanks",
    "that",
    "the",
    "their",
    "them",
    "then",
    "there",
    "these",
    "they",
    "think",
    "this",
    "those",
    "through",
    "to",
    "too",
    "under",
    "until",
    "up",
    "us",
    "very",
    "want",
    "was",
    "we",
    "were",
    "what",
    "when",
    "where",
    "which",
    "while",
    "who",
    "whom",
    "why",
    "will",
    "with",
    "would",
    "yes",
    "you",
    "your",
    "yours",
];

fn is_stopword(term: &str) -> bool {
    STOPWORDS.binary_search(&term).is_ok()
}

/// Rewrite free-form `topic` as an OR query over its non-glue words.
///
/// Splitting on every non-alphanumeric run — not just whitespace — is what
/// lets a compound identifier like `graph_db` still match: the index tokenizes
/// the same way at write time, so the document holds `graph` and `db` as two
/// separate tokens, and an unquoted term that kept the underscore (`graphdb`)
/// would match neither. `None` when nothing searchable is left.
fn or_query(topic: &str) -> Option<String> {
    let mut terms: Vec<String> = Vec::new();
    for word in topic.split(|c: char| !c.is_alphanumeric()) {
        if word.is_empty() {
            continue;
        }
        let term = word.to_lowercase();
        if is_stopword(&term) || terms.contains(&term) {
            continue;
        }
        terms.push(term);
    }
    if terms.is_empty() {
        return None;
    }
    Some(terms.join(" OR "))
}

/// Why `recall` had nothing to say — the two reasons are different advice.
#[derive(Debug)]
pub enum RecallOutcome {
    /// The store declares no full-text index, so no topic can ever match.
    NoIndex,
    /// The store can search and this topic matched nothing.
    NoMatch,
    /// The rendered digest.
    Hits(String),
}

/// Rank nodes against a free-text topic over every declared text field.
pub fn recall_digest<F: Fs>(
    db: &GraphDb<F>,
    topic: &str,
    store_label: &str,
    max_bytes: usize,
) -> RecallOutcome {
    let topic = topic.trim();
    if topic.is_empty() {
        return RecallOutcome::NoMatch;
    }

    let mut fields: Vec<String> = db.fulltext_pairs().into_iter().map(|(_, f)| f).collect();
    fields.sort();
    fields.dedup();
    if fields.is_empty() {
        return RecallOutcome::NoIndex;
    }

    // Space-separated terms are ANDed by the index's query grammar, so the
    // topic as typed ("who is Matthew Sherlin?") would require the document
    // to contain "who" and "is" too. `or_query` drops the glue words and OR's
    // the rest, so a question or a name finds the content word that matches.
    // Fall back to the trimmed topic itself when nothing survives the
    // filter, so an all-stopword topic still probes the index rather than
    // silently skipping the search.
    let query = or_query(topic).unwrap_or_else(|| topic.to_string());
    let mut best: BTreeMap<String, f64> = BTreeMap::new();
    for field in &fields {
        for (key, score) in db.search_hybrid(field, &query, "embedding", &[], None, MAX_HITS) {
            let slot = best.entry(key).or_insert(0.0);
            if score > *slot {
                *slot = score;
            }
        }
    }
    if best.is_empty() {
        return RecallOutcome::NoMatch;
    }

    // Deterministic: score descending, then key ascending, matching every
    // other tie-break in this engine.
    let mut ranked: Vec<(String, f64)> = best.into_iter().collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.truncate(MAX_HITS);

    let mut out = format!(
        "mushroomdb recall ({} related nodes in {store_label}):\n",
        ranked.len()
    );
    for (key, _) in &ranked {
        let line = match db.node_summary_line(key) {
            Some(summary) => format!("  {key} — {summary}\n"),
            None => format!("  {key}\n"),
        };
        if out.len() + line.len() > max_bytes {
            break;
        }
        out.push_str(&line);
    }
    RecallOutcome::Hits(out)
}
