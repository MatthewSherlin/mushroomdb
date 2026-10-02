//! What do I already know about this?
//!
//! The 0.6.12 implementation began by extracting *identifiers* from the topic
//! — paths, `mod::name`, snake_case, backticked words — and returned nothing
//! when it found none. On a memory store that rejected every natural-language
//! topic, including a bare human name, while the note containing the word sat
//! indexed and reachable by Cypher. There is no identifier gate here.

use crate::digest::sanitize;
use crate::GraphDb;
use core_storage::fs::Fs;
use core_storage::fulltext::{stem, value_tokens_stemmed_with_positions};
use std::collections::{BTreeMap, HashSet};

/// How many hits the digest will name.
const MAX_HITS: usize = 6;

/// Terms a topic contributes to the query, at most.
///
/// `repograph::recall` capped at the same 24 and this module shipped without
/// one, which was harmless only while the caller was an MCP `recall` argument
/// a person had typed. The `UserPromptSubmit` hook passes a whole prompt, and
/// the query the index receives is every surviving term OR'd together — so
/// without a cap a long prompt is a long query, once per turn, for a digest
/// bounded to six lines. The first 24 in order of appearance: a prompt's
/// subject is at its start far more often than at its end.
pub const MAX_QUERY_TERMS: usize = 24;

/// Glue words dropped from a topic before it becomes a query.
///
/// The index's query grammar ANDs space-separated terms within one group —
/// there is no websearch-style stopword handling underneath it — so a natural
/// question passed through unchanged (`"who is Matthew Sherlin?"`) requires
/// the document to contain the word "who", which none does. Dropping these
/// before the terms are OR'd together is what lets a question find the
/// content word rather than demanding the whole sentence.
///
/// Copied from the code-graph `recall`'s list when this module moved out of
/// `repograph` in 0.7; that module has since been deleted, so this is now the
/// only copy.
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
///
/// A repeat is a word that *stems* like an earlier one — "bugs" after "bug" —
/// because coverage is counted on stems: kept as two terms, a node holding
/// the one word would count twice toward its coverage.
fn search_terms(topic: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    let mut stems: HashSet<String> = HashSet::new();
    for word in topic.split(|c: char| !c.is_alphanumeric()) {
        if word.is_empty() {
            continue;
        }
        let term = word.to_lowercase();
        if is_stopword(&term) || !stems.insert(stem(&term)) {
            continue;
        }
        terms.push(term);
        if terms.len() == MAX_QUERY_TERMS {
            break;
        }
    }
    terms
}

/// The words of an all-stopword topic, for the fallback's one AND-group:
/// split and lowercased as [`search_terms`] splits, so no `"`, `-` or `*` the
/// index's parser reads as grammar survives, and without `or` and `and`,
/// which it reads as keywords and so could never match as words anyway.
/// Repeats dropped, capped at [`MAX_QUERY_TERMS`] like the ordinary query.
fn fallback_words(topic: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    for word in topic.split(|c: char| !c.is_alphanumeric()) {
        let word = word.to_lowercase();
        if word.is_empty() || word == "or" || word == "and" || words.contains(&word) {
            continue;
        }
        words.push(word);
        if words.len() == MAX_QUERY_TERMS {
            break;
        }
    }
    words
}

/// The topic's search terms and the query the index is asked.
///
/// Space-separated terms are ANDed by the index's query grammar, so the
/// topic as typed ("who is Matthew Sherlin?") would require the document
/// to contain "who" and "is" too. [`search_terms`] drops the glue words, and
/// the query ORs the rest, so a question or a name finds the content word
/// that matches. Fall back to the topic's own words, ANDed, when nothing
/// survives the filter, so an all-stopword topic still probes the index
/// rather than silently skipping the search; see [`fallback_words`]. The
/// terms are empty exactly when the fallback ran, and the query is empty
/// only when the topic held no searchable word at all.
fn topic_query(topic: &str) -> (Vec<String>, String) {
    let terms = search_terms(topic);
    let query = if terms.is_empty() {
        fallback_words(topic).join(" ")
    } else {
        terms.join(" OR ")
    };
    (terms, query)
}

/// One node a topic matched.
///
/// `key`, `label` and `summary` are stored content, unsanitized: only
/// [`recall_digest`] passes them through [`sanitize`]. A caller rendering
/// them into an assistant's context owes `core_api::digest::sanitize`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct RecallHit {
    pub key: String,
    /// The node's label, or empty if it vanished between the search and here.
    pub label: String,
    /// The first of `text`, `summary` or `name` the node carries, as
    /// [`GraphDb::node_summary_line`] cuts it.
    pub summary: Option<String>,
    /// How many of the topic's terms this node's own text holds. `0` when
    /// the topic was all stopwords and its words were searched as one
    /// AND-group, where every hit holds all of them.
    pub covered: usize,
    /// The fused score. Rank-derived, so nearly flat across hits: order by
    /// `covered` first, as this list already is.
    pub score: f64,
}

/// A topic's hits, ranked: coverage descending, then score, then key.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct RecallRows {
    /// False when the store declares no full-text index, so no topic can
    /// match — a different answer from "this topic matched nothing".
    pub indexed: bool,
    /// The topic's search terms after stopwords and repeats are dropped, at
    /// most [`MAX_QUERY_TERMS`] (24). `0` with hits present means the topic
    /// was all stopwords and the fallback ran, where every hit's `covered`
    /// is `0`.
    pub terms: usize,
    /// At most six, best first.
    pub hits: Vec<RecallHit>,
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

/// Rank nodes against a free-text topic over every declared text field, and
/// return the hits as data. [`recall_digest`] renders exactly these rows.
pub fn recall_rows<F: Fs>(db: &GraphDb<F>, topic: &str) -> RecallRows {
    let none = |indexed: bool| RecallRows {
        indexed,
        terms: 0,
        hits: Vec::new(),
    };
    let mut fields: Vec<String> = db.fulltext_pairs().into_iter().map(|(_, f)| f).collect();
    fields.sort();
    fields.dedup();
    // Checked before the topic itself: a store that cannot search at all
    // owes the caller that answer regardless of what they typed, including
    // an empty topic — "no text index" is the fix either way, "no match" is
    // not.
    if fields.is_empty() {
        return none(false);
    }
    let topic = topic.trim();
    if topic.is_empty() {
        return none(true);
    }

    let (terms, query) = topic_query(topic);
    if query.is_empty() {
        return none(true);
    }

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
    // only when at least half of the topic's own terms are in *its own*
    // set. A one-word topic is 1 of 1 — full coverage, not a special case —
    // so every topic is held to the same rule.
    //
    // The bar is *half* the topic's terms, not more than half. A strict
    // majority reads well and refuses the shape this product is made of: an
    // entity node holds a name and a note holds the fact, so a two-word topic
    // naming a person and an action has one word in each, and the entity the
    // question is about scored 1 of 2 and was dropped from the answer to it.
    // Half still refuses a *minority* — one word of three, one of five — which
    // is what the false-positive cases actually are.
    //
    // And the count survives the filter, because a digest that shows it does
    // not have to be believed: `reid — reid (1/3 terms)` is a hit a reader can
    // discount, where an unannotated line cannot be told from a full match.
    // RRF flattens every field's top hit to 1/(60+1), so the fused score
    // carries almost no ranking information; coverage carries it instead.
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
    let mut covered: BTreeMap<String, usize> = BTreeMap::new();
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
            if present * 2 >= terms.len() {
                covered.insert(key.clone(), present);
                true
            } else {
                false
            }
        });
    }

    // Deterministic: coverage descending, then score descending, then key
    // ascending. Coverage leads because it is the measurement that means
    // something — see the floor above — and the key breaks the last tie the
    // way every other tie in this engine breaks.
    let mut ranked: Vec<(String, usize, f64)> = best
        .into_iter()
        .map(|(key, score)| {
            let present = covered.get(&key).copied().unwrap_or(0);
            (key, present, score)
        })
        .collect();
    ranked.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| b.2.total_cmp(&a.2))
            .then_with(|| a.0.cmp(&b.0))
    });
    ranked.truncate(MAX_HITS);

    RecallRows {
        indexed: true,
        terms: terms.len(),
        hits: ranked
            .into_iter()
            .map(|(key, covered, score)| RecallHit {
                label: db
                    .node_ref(&key)
                    .map(|n| n.label().to_string())
                    .unwrap_or_default(),
                summary: db.node_summary_line(&key),
                key,
                covered,
                score,
            })
            .collect(),
    }
}

/// The memory defaults' full-text fields this store has not declared and has
/// something to find in: each `(label, field)` of
/// [`memory_defaults`](crate::memory_schema::memory_defaults) that
/// `fulltext_pairs()` lacks while a live node under the label carries the
/// field. In the defaults' order.
///
/// `remember` declares `Note.text`, and the name of each `entities[].label`,
/// as it writes. So a store that never took the defaults stops being
/// "a store with no text index" at its first note, while the entities already
/// in it stay unsearchable by name. This is what `recall` and the session
/// brief name from then on, with the one command that indexes them.
///
/// Empty on a store that took the defaults. Empty too on a store whose schema
/// is its owner's and holds no node under a memory label: the defaults would
/// index nothing there, so it is never told to apply them. A field no node
/// carries is not reported, whatever the label holds.
///
/// Reads nodes only under a label whose default is undeclared, and stops at
/// the first that carries the field.
pub fn unindexed_memory_fields<F: Fs>(db: &GraphDb<F>) -> Vec<(String, String)> {
    let declared = db.fulltext_pairs();
    crate::memory_schema::memory_defaults()
        .fulltext
        .into_iter()
        .filter(|pair| !declared.contains(pair))
        .filter(|(label, field)| {
            db.nodes_with_label(label)
                .iter()
                .any(|n| n.prop(field).is_some())
        })
        .collect()
}

/// `Person.name, Entity.name` — [`unindexed_memory_fields`] as a notice
/// prints them.
pub fn unindexed_fields_list(fields: &[(String, String)]) -> String {
    fields
        .iter()
        .map(|(label, field)| format!("{label}.{field}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// [`recall_rows`], rendered as the digest an assistant reads: one header,
/// one line per hit, inside `max_bytes`.
pub fn recall_digest<F: Fs>(
    db: &GraphDb<F>,
    topic: &str,
    store_label: &str,
    max_bytes: usize,
) -> RecallOutcome {
    let rows = recall_rows(db, topic);
    if !rows.indexed {
        return RecallOutcome::NoIndex;
    }
    if rows.hits.is_empty() {
        return RecallOutcome::NoMatch;
    }
    let total = rows.terms;

    // The label is a caller-supplied path, and it lands in the same context
    // as the hits, so it is held to the same rule.
    let label = sanitize(store_label);

    // Pointers are rendered first so the header can count what actually
    // printed. The header and the elision marker are charged up front, so
    // `max_bytes` bounds the whole digest rather than only the pointers. The
    // reservation uses `rows.hits.len()`, an upper bound on the count the header
    // ends up printing. The same budgeting the 0.6 code-graph digest used.
    let reserved = header(rows.hits.len(), &label).len() + ELISION.len();
    let Some(mut budget) = max_bytes.checked_sub(reserved) else {
        // A pathologically long store label: nothing useful fits.
        return RecallOutcome::NoMatch;
    };
    let mut lines: Vec<String> = Vec::new();
    let mut truncated = false;
    for hit in &rows.hits {
        // The all-stopword fallback searched the topic's words as one AND-group,
        // which required every word in one document — 100% coverage, a
        // stricter gate than the half rule, not a missing one — so there
        // are no per-term counts to report and none are printed.
        let cover = if total == 0 {
            String::new()
        } else {
            format!(" ({}/{total} terms)", hit.covered)
        };
        // Keys and summaries are stored content, read back into an
        // assistant's context: a newline or an escape sequence in one must
        // not be able to split this line or forge a header.
        let shown = sanitize(&hit.key);
        let line = match &hit.summary {
            Some(summary) => format!("  {shown} — {}{cover}\n", sanitize(summary)),
            None => format!("  {shown}{cover}\n"),
        };
        if line.len() > budget {
            truncated = true;
            break;
        }
        budget -= line.len();
        lines.push(line);
    }
    if lines.is_empty() {
        // Not one pointer fits the budget: a header announcing hits it cannot
        // show would be noise, so this answers as the 0.6 digest did.
        return RecallOutcome::NoMatch;
    }

    let mut out = header(lines.len(), &label);
    for line in &lines {
        out.push_str(line);
    }
    if truncated {
        out.push_str(ELISION);
    }
    RecallOutcome::Hits(out)
}

/// The line a digest ends with when the byte budget dropped hits from it.
const ELISION: &str = "  …\n";

/// The digest's first line. `count` is the number of hits printed under it,
/// never the number that matched.
fn header(count: usize, label: &str) -> String {
    format!("mushroomdb recall ({count} related nodes in {label}):\n")
}

#[cfg(test)]
mod tests {
    use super::{is_stopword, search_terms, topic_query, MAX_QUERY_TERMS, STOPWORDS};
    use core_storage::fulltext::parse_query;

    /// The all-stopword fallback is held to the same cap as the ordinary
    /// query, and carries none of the grammar the index's parser reads: a
    /// `"` phrase, a leading `-` negation, a trailing `*` prefix, or an
    /// `OR`/`AND` keyword. It stays one AND-group of plain terms however the
    /// prompt was written.
    #[test]
    fn the_all_stopword_fallback_is_capped_and_carries_no_grammar() {
        let prompt: String = STOPWORDS
            .iter()
            .map(|w| format!("-{w}* \"{w} OR {w}\" AND {w}* ({w}) or "))
            .collect();
        let (terms, query) = topic_query(&prompt);
        assert!(terms.is_empty(), "the prompt is all stopwords: {terms:?}");
        let words: Vec<&str> = query.split(' ').collect();
        assert!(
            !query.is_empty() && words.len() <= MAX_QUERY_TERMS,
            "{} words: {query:?}",
            words.len()
        );
        assert!(
            !query.contains(['"', '-', '*', '(', ')']),
            "grammar characters reached the query: {query:?}"
        );
        let groups = parse_query(&query);
        assert_eq!(groups.len(), 1, "one AND-group, no OR: {query:?}");
        assert_eq!(groups[0].len(), words.len(), "no word read as a keyword");
        assert!(
            groups[0].iter().all(|t| !t.negated && !t.prefix),
            "{query:?}"
        );
    }

    /// A topic term and its plural are one term: they stem alike, so a node
    /// holding the word would otherwise count twice toward its coverage.
    #[test]
    fn search_terms_drops_a_word_that_stems_like_an_earlier_one() {
        assert_eq!(search_terms("bug bugs Bugs crash"), vec!["bug", "crash"]);
    }

    /// The cap is enforced where the terms are built, so it is pinned there:
    /// an integration test that only checks the call returns cannot fail if
    /// the `break` is removed.
    #[test]
    fn search_terms_stops_at_the_cap_and_keeps_the_earliest() {
        let long: String = (0..200).map(|i| format!("tok{i} ")).collect();
        let terms = search_terms(&format!("{long} matthew"));
        assert_eq!(terms.len(), MAX_QUERY_TERMS);
        assert_eq!(terms[0], "tok0");
        assert!(!terms.contains(&"matthew".to_string()), "{terms:?}");
    }

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
