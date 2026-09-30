//! What do I already know about this?
//!
//! The 0.6.12 implementation began by extracting *identifiers* from the topic
//! — paths, `mod::name`, snake_case, backticked words — and returned nothing
//! when it found none. On a memory store that rejected every natural-language
//! topic, including a bare human name, while the note containing the word sat
//! indexed and reachable by Cypher. There is no identifier gate here.

use crate::GraphDb;
use core_storage::fs::Fs;
use core_storage::fulltext::{stem, value_tokens_stemmed_with_positions};
use std::collections::{BTreeMap, HashSet};

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

/// `topic` split into its non-glue words: lowercase, split on every
/// non-alphanumeric run — not just whitespace — so a compound identifier like
/// `graph_db` still matches. The index tokenizes the same way at write time,
/// so the document holds `graph` and `db` as two separate tokens, and an
/// unquoted term that kept the underscore (`graphdb`) would match neither.
/// Stopwords and repeats are dropped; order of first appearance is kept.
fn search_terms(topic: &str) -> Vec<String> {
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
    terms
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
    let mut fields: Vec<String> = db.fulltext_pairs().into_iter().map(|(_, f)| f).collect();
    fields.sort();
    fields.dedup();
    // Checked before the topic itself: a store that cannot search at all
    // owes the caller that answer regardless of what they typed, including
    // an empty topic — "no text index" is the fix either way, "no match" is
    // not.
    if fields.is_empty() {
        return RecallOutcome::NoIndex;
    }
    let topic = topic.trim();
    if topic.is_empty() {
        return RecallOutcome::NoMatch;
    }

    // Space-separated terms are ANDed by the index's query grammar, so the
    // topic as typed ("who is Matthew Sherlin?") would require the document
    // to contain "who" and "is" too. `search_terms` drops the glue words, and
    // the query ORs the rest, so a question or a name finds the content word
    // that matches. Fall back to the trimmed topic itself when nothing
    // survives the filter, so an all-stopword topic still probes the index
    // rather than silently skipping the search.
    let terms = search_terms(topic);
    let query = if terms.is_empty() {
        topic.to_string()
    } else {
        terms.join(" OR ")
    };

    let mut best: BTreeMap<String, f64> = BTreeMap::new();
    for field in &fields {
        for (key, score) in db.search_hybrid(field, &query, "embedding", &[], None, MAX_HITS) {
            let slot = best.entry(key).or_insert(0.0);
            if score > *slot {
                *slot = score;
            }
        }
    }

    // Relevance floor. An OR query returns something the moment any one of
    // its terms matches anywhere, and the fused score `search_hybrid` returns
    // cannot be used to tell a real hit from an accident: RRF replaces every
    // BM25 score with `1/(60 + rank)`, so the top hit of any non-empty query
    // scores exactly 1/61 whether it satisfied one word of a five-word topic
    // or all five.
    //
    // An absolute BM25 score cutoff (the approach `repograph::recall` used,
    // `MIN_HIT_SCORE`) does not fix this. The operative fact is not that a
    // young memory store is small — it is that BM25's idf term grows with the
    // *corpus size* for any term that occurs in only one document, at any N:
    // `repograph`'s own floor test, three documents and a term in one of
    // them, scores idf≈0.981, about twenty times its own 0.05 floor. A
    // coincidental single-document match clears an absolute floor whether the
    // corpus has three documents or three million, so no fixed number closes
    // this off.
    //
    // Nor is it enough to ask "does this word appear anywhere in the store",
    // checked once for the whole corpus: a three-node store where one node
    // holds only "apple", another only "banana", another only "cherry" would
    // pass a corpus-wide check on the topic "apple banana cherry" — every
    // word is present *somewhere* — while no single node answers more than a
    // third of it. Coverage has to be asked of the same thing relevance is
    // asked of: one candidate node, not the corpus, and a candidate survives
    // only when more than half of the topic's own terms are in *its own*
    // set. A one-word topic is 1 of 1 — full coverage, not a special case —
    // so every topic is held to the same rule. A one- or two-word topic has
    // to be present whole in the one node it names (a 50/50 split is not a
    // majority); a longer one tolerates a minority of its words being absent
    // from any single node — a paraphrase, a typo, a word that node never
    // used — without demanding the whole sentence appear verbatim in it.
    //
    // Asked of the candidate's own text, not the index: `best` already holds
    // every candidate this call will ever consider (at most
    // `fields.len() * MAX_HITS`, ranked by `search_hybrid` above), so coverage
    // for each is answered by tokenizing *that node's own* declared-field
    // values with the index's own tokenizer
    // (`value_tokens_stemmed_with_positions`, stemmed exactly as indexing
    // stems, so "orchards" in the text matches a topic term "orchard") and
    // checking membership directly — O(candidates × fields × terms) reading
    // props already in hand, not O(corpus × fields × terms) re-querying the
    // index per term. Before this, one `recall` call ran `fields.len()` full,
    // untruncated (`k == 0`) posting-list scans per topic term — self-declaring
    // full-text for a caller-supplied `entities[].label` (`remember`, fix
    // round 2) grows `fields` without bound, so this was the one thing bounded
    // to be able to say yes to that at all. `search_top` is never called here.
    if !terms.is_empty() {
        let stemmed_terms: Vec<String> = terms.iter().map(|t| stem(t)).collect();
        best.retain(|key, _| {
            let mut candidate_tokens: HashSet<String> = HashSet::new();
            for field in &fields {
                if let Some(value) = db.get_prop(key, field) {
                    candidate_tokens.extend(
                        value_tokens_stemmed_with_positions(&value)
                            .into_iter()
                            .map(|(tok, _)| tok),
                    );
                }
            }
            let present = stemmed_terms
                .iter()
                .filter(|t| candidate_tokens.contains(*t))
                .count();
            present * 2 > terms.len()
        });
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

#[cfg(test)]
mod tests {
    use super::{is_stopword, STOPWORDS};

    /// Binding: the list stays sorted and duplicate-free, because
    /// [`is_stopword`] binary-searches it. An out-of-order insert would
    /// silently stop matching that word — and every other word past it —
    /// with nothing else in the suite noticing.
    ///
    /// Ported from `repograph::recall`'s `the_stopword_lists_are_sorted_and_unique`:
    /// the list came across the split (see the doc comment on [`STOPWORDS`]),
    /// but this binding test did not, leaving the copy's own sortedness
    /// invariant unguarded.
    #[test]
    fn the_stopword_list_is_sorted_and_unique() {
        for pair in STOPWORDS.windows(2) {
            assert!(
                pair[0] < pair[1],
                "STOPWORDS must be sorted and duplicate-free: {:?} then {:?}",
                pair[0],
                pair[1]
            );
        }
        // And every word in it is actually found by the lookup that
        // searches it.
        for word in STOPWORDS {
            assert!(is_stopword(word), "STOPWORDS: {word:?} is not matched");
        }
        assert!(
            !is_stopword("matthew"),
            "a subject word must stay searchable"
        );
        assert!(!is_stopword("recall"));
    }
}
