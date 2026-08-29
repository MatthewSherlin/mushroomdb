/// Full-text v2 incremental inverted index with BM25 ranking.
///
/// ## Scope (v2)
/// - Tokenization: split on non-alphanumeric, lowercase, then Snowball English
///   stem (rust-stemmers 1.x). Applied at index AND query time.
/// - Query grammar (websearch-style):
///   `query := group ('OR' group)*`
///   `group := atom ('AND'? atom)*`
///   `atom  := '"' <phrase words> '"'    // stemmed-adjacency phrase`
///   `atom  |= '-' <word> ['*']          // negated term`
///   `atom  |= <word> '*'?               // regular term, optional prefix`
///   OR and AND are case-insensitive keywords.  Phrases match stemmed word forms
///   (stemming applied to both document tokens and phrase tokens at index/query time).
/// - BM25 ranking: k1 = 1.2, b = 0.75.  Scores summed across matched OR-groups.
///   Within each group, negated atoms exclude a document; phrase atoms require
///   positional adjacency in the stemmed token stream.
///
/// ## Memory cost model (v2)
/// Positions are stored per (token, node) as Vec<u32> (token offsets).  For a
/// corpus of N nodes each averaging T distinct stemmed tokens per field with
/// average tf per token, memory is O(N × T × avg_tf) per indexed field.
/// Empirically this is 2–3× the v1 footprint on text-heavy stores (measured on
/// a 10k-doc synthetic corpus: ~24 MB at avg 40 tokens/doc vs ~10 MB in v1).
///
/// ## Phrase semantics
/// Phrase tokens are stemmed; `"running fast"` matches documents containing
/// the stemmed sequence `["run", "fast"]` at consecutive positions.  This means
/// phrases match word forms, not literal strings.
use crate::idmap::IdMap;
use crate::interner::Interner;
use crate::types::Value;
use std::collections::{BTreeMap, BTreeSet};

// ---------------------------------------------------------------------------
// Stemmer
// ---------------------------------------------------------------------------

/// Apply the Snowball English stemmer to a single lowercased token.
pub fn stem(tok: &str) -> String {
    use rust_stemmers::{Algorithm, Stemmer};
    thread_local! {
        static EN: Stemmer = Stemmer::create(Algorithm::English);
    }
    EN.with(|s| s.stem(tok).into_owned())
}

// ---------------------------------------------------------------------------
// Tokenizer
// ---------------------------------------------------------------------------

/// Tokenize a raw string: split on any non-`char::is_alphanumeric` character,
/// lowercase each resulting run.  Returns UNSTEMMED tokens.
///
/// Used at the API boundary (oracle, query parsing) and internally before
/// stemming.  For indexing, use [`tokenize_stemmed_with_positions`].
pub fn tokenize(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for ch in s.chars() {
        if ch.is_alphanumeric() {
            for lc in ch.to_lowercase() {
                current.push(lc);
            }
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Tokenize a string, stem each token, and return `(stemmed_token, position)` pairs.
/// Position is the 0-based ordinal in the token stream (used for phrase adjacency).
pub fn tokenize_stemmed_with_positions(s: &str) -> Vec<(String, u32)> {
    let mut result = Vec::new();
    let mut pos: u32 = 0;
    let mut current = String::new();
    for ch in s.chars() {
        if ch.is_alphanumeric() {
            for lc in ch.to_lowercase() {
                current.push(lc);
            }
        } else if !current.is_empty() {
            result.push((stem(&current), pos));
            pos += 1;
            current.clear();
        }
    }
    if !current.is_empty() {
        result.push((stem(&current), pos));
    }
    result
}

/// Tokenize a `Value`, applying stemming and returning `(stemmed_token, position)`.
///
/// `Value::List` of `Str` elements: each element is tokenized independently.
/// A `POSITION_GAP` (> 1) is inserted between elements so that phrase queries
/// cannot match across list element boundaries — adjacency requires consecutive
/// positions (differing by exactly 1), and the gap guarantees they never are.
pub fn value_tokens_stemmed_with_positions(v: &Value) -> Vec<(String, u32)> {
    match v {
        Value::Str(s) => tokenize_stemmed_with_positions(s),
        Value::List(items) => {
            // Gap between list elements: any value > 1 breaks cross-boundary adjacency.
            const POSITION_GAP: u32 = 2;
            let mut result: Vec<(String, u32)> = Vec::new();
            let mut pos_offset: u32 = 0;
            for item in items {
                if let Value::Str(s) = item {
                    let toks = tokenize_stemmed_with_positions(s);
                    for (tok, local_pos) in &toks {
                        result.push((tok.clone(), pos_offset + local_pos));
                    }
                    if !toks.is_empty() {
                        // Advance past this element's tokens plus the gap.
                        pos_offset += toks.len() as u32 + POSITION_GAP;
                    }
                }
            }
            result
        }
        _ => vec![],
    }
}

// ---------------------------------------------------------------------------
// Query parsing
// ---------------------------------------------------------------------------

/// A single search term: stemmed token (for non-prefix) or raw prefix,
/// with optional negation and prefix flags.
///
/// With v2 grammar:
/// - `negated = true`: this term EXCLUDES matching documents from a group.
/// - `prefix = true`: match any index token that *starts with* `token`.
///   Prefix tokens are NOT stemmed (the prefix matches against stemmed index tokens).
/// - Otherwise: `token` is the Snowball-English-stemmed form of the input word.
#[derive(Debug, Clone)]
pub struct Term {
    /// Stemmed token (non-prefix) or raw lowercase prefix (prefix=true).
    pub token: String,
    /// If true, match any posting token that *starts with* `token`.
    pub prefix: bool,
    /// If true, a match of this term EXCLUDES the document from the group.
    pub negated: bool,
}

/// A query atom: either a single term or a phrase.
#[derive(Debug, Clone)]
enum QueryAtom {
    Term(Term),
    /// A sequence of stemmed tokens; adjacency in position stream required.
    Phrase(Vec<String>),
}

/// Parsed query: OR-groups of AND-atoms.
type Groups = Vec<Vec<QueryAtom>>;

/// Parse a query using the v2 grammar into OR-groups of AND-atoms (internal).
///
/// Handles `"quoted phrases"`, `-negation`, `prefix*`, and OR/AND keywords.
fn parse_query_v2(query: &str) -> Groups {
    let mut groups: Groups = vec![vec![]];
    let chars: Vec<char> = query.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        // Skip whitespace.
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }

        if chars[i] == '"' {
            // Phrase: collect tokens until closing '"'.
            i += 1; // consume opening '"'
            let mut phrase_tokens: Vec<String> = Vec::new();
            let mut current = String::new();
            while i < chars.len() && chars[i] != '"' {
                let ch = chars[i];
                if ch.is_alphanumeric() {
                    for lc in ch.to_lowercase() {
                        current.push(lc);
                    }
                } else if !current.is_empty() {
                    phrase_tokens.push(stem(&current));
                    current.clear();
                }
                i += 1;
            }
            if !current.is_empty() {
                phrase_tokens.push(stem(&current));
            }
            if chars.get(i) == Some(&'"') {
                i += 1; // consume closing '"'
            }
            if !phrase_tokens.is_empty() {
                groups
                    .last_mut()
                    .unwrap()
                    .push(QueryAtom::Phrase(phrase_tokens));
            }
        } else {
            // Collect until next whitespace.
            let start = i;
            while i < chars.len() && !chars[i].is_whitespace() {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();

            // Check for OR/AND keyword.
            match word.to_ascii_uppercase().as_str() {
                "OR" => {
                    groups.push(vec![]);
                    continue;
                }
                "AND" => continue,
                _ => {}
            }

            // Detect leading '-' for negation.
            let (negated, rest) = if let Some(stripped) = word.strip_prefix('-') {
                (true, stripped)
            } else {
                (false, word.as_str())
            };

            // Detect trailing '*' for prefix.
            let (raw, prefix) = if let Some(stripped) = rest.strip_suffix('*') {
                (stripped, true)
            } else {
                (rest, false)
            };

            // Extract alphanumeric characters only, lowercase.
            let token: String = raw
                .chars()
                .filter(|c| c.is_alphanumeric())
                .flat_map(|c| c.to_lowercase())
                .collect();

            if token.is_empty() {
                continue;
            }

            // Prefix tokens are NOT stemmed (match against stemmed index tokens as-is).
            // Non-prefix tokens are stemmed.
            let final_token = if prefix { token } else { stem(&token) };

            groups.last_mut().unwrap().push(QueryAtom::Term(Term {
                token: final_token,
                prefix,
                negated,
            }));
        }
    }

    groups.retain(|g| !g.is_empty());
    groups
}

/// Parse a query into OR-groups of AND-terms (public, oracle-compatible).
///
/// Grammar: `query := group ('OR' group)*`
///           `group := term ('AND'? term)*`
///           `term  := '-'? <word> '*'?`
///
/// Differences from v2 internal grammar: phrases (`"..."`) are NOT supported;
/// each quoted or unquoted word is treated as a plain term.  This form is used
/// by the sim-harness oracle and any caller that needs a stable external API.
///
/// Token values in returned `Term`s are Snowball-English-stemmed for non-prefix
/// terms.  Callers must stem document tokens with [`stem`] before comparing.
pub fn parse_query(query: &str) -> Vec<Vec<Term>> {
    // Re-use the v2 parser but flatten phrases to individual terms.
    parse_query_v2(query)
        .into_iter()
        .map(|group| {
            group
                .into_iter()
                .flat_map(|atom| match atom {
                    QueryAtom::Term(t) => vec![t],
                    // Flatten phrase tokens to individual non-negated non-prefix terms.
                    QueryAtom::Phrase(tokens) => tokens
                        .into_iter()
                        .map(|tok| Term {
                            token: tok,
                            prefix: false,
                            negated: false,
                        })
                        .collect(),
                })
                .collect()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Boolean evaluation (for textMatches WHERE clause)
// ---------------------------------------------------------------------------

/// Evaluate a raw field string against a v2 query (phrase + negation + prefix).
///
/// Returns `true` if any OR-group in the parsed query matches:
/// - All non-negated terms/phrases in the group are satisfied.
/// - No negated term in the group is present.
/// - Phrases require consecutive stemmed token positions.
///
/// This is O(field_length × query_terms) per call — fine for WHERE filtering;
/// prefer `db.search()` for large result-set scenarios.
pub fn eval_query_str(field_value: &str, query: &str) -> bool {
    let groups = parse_query_v2(query);
    eval_groups_str(field_value, &groups)
}

/// Evaluate a raw list field (concatenated as a single space-joined string).
pub fn eval_query_str_list(items: &[Value], query: &str) -> bool {
    let combined: String = items
        .iter()
        .filter_map(|v| {
            if let Value::Str(s) = v {
                Some(s.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    eval_query_str(&combined, query)
}

fn eval_groups_str(field_value: &str, groups: &Groups) -> bool {
    if groups.is_empty() {
        return false;
    }
    // Build stemmed position map from the field value once.
    let stemmed_with_pos = tokenize_stemmed_with_positions(field_value);
    let token_set: BTreeSet<String> = stemmed_with_pos.iter().map(|(t, _)| t.clone()).collect();
    // Build position map for phrase adjacency checking.
    let mut pos_map: BTreeMap<String, Vec<u32>> = BTreeMap::new();
    for (tok, pos) in &stemmed_with_pos {
        pos_map.entry(tok.clone()).or_default().push(*pos);
    }

    'outer: for group in groups {
        for atom in group {
            match atom {
                QueryAtom::Term(t) => {
                    let found = if t.prefix {
                        token_set.iter().any(|tk| tk.starts_with(t.token.as_str()))
                    } else {
                        token_set.contains(&t.token)
                    };
                    if t.negated {
                        if found {
                            continue 'outer; // negated term present → group fails
                        }
                    } else if !found {
                        continue 'outer; // required term absent → group fails
                    }
                }
                QueryAtom::Phrase(tokens) => {
                    if !phrase_matches_pos_map(&pos_map, tokens) {
                        continue 'outer;
                    }
                }
            }
        }
        return true; // all atoms in this group satisfied
    }
    false
}

/// Check phrase adjacency in a position map: every consecutive token pair must
/// appear at consecutive positions in at least one alignment.
fn phrase_matches_pos_map(pos_map: &BTreeMap<String, Vec<u32>>, tokens: &[String]) -> bool {
    if tokens.is_empty() {
        return false;
    }
    let Some(first_positions) = pos_map.get(&tokens[0]) else {
        return false;
    };
    'start: for &start in first_positions {
        let mut cur = start;
        for tok in &tokens[1..] {
            cur += 1;
            let Some(positions) = pos_map.get(tok) else {
                continue 'start;
            };
            if positions.binary_search(&cur).is_err() {
                continue 'start;
            }
        }
        return true;
    }
    false
}

// ---------------------------------------------------------------------------
// FulltextIndex
// ---------------------------------------------------------------------------

/// Incremental inverted index for full-text BM25 search.
///
/// Enabled per `(label, field)` pair via [`FulltextIndex::enable`].
/// Postings store stemmed tokens with per-document position lists for phrase
/// adjacency checking.  Doc lengths are tracked separately for BM25 normalization.
///
/// ## BM25 constants
/// k1 = 1.2, b = 0.75  (Okapi BM25 defaults).
///
/// ## WAL / persistence
/// The index is NOT stored in the V8 snapshot.  It is rebuilt from WAL replay
/// (EnableFulltext / DisableFulltext records + node property re-indexing) at
/// open time via [`FulltextIndex::rebuild_all`].  Postings restructuring in v2
/// has no snapshot format impact.
#[derive(Debug, Default, Clone)]
pub struct FulltextIndex {
    /// Enabled `(label, field)` pairs.
    enabled: BTreeSet<(String, String)>,
    /// Inverted index:
    ///   `field → stemmed_token → node_id → positions (u32 offsets in token stream)`
    ///
    /// Positions enable phrase adjacency checks and provide tf = positions.len().
    postings: BTreeMap<String, BTreeMap<String, BTreeMap<u32, Vec<u32>>>>,
    /// Document lengths (in stemmed tokens) per field per node.
    ///   `field → node_id → token_count`
    ///
    /// Used for BM25 length normalization (avg_dl and dl(d)).
    doc_len: BTreeMap<String, BTreeMap<u32, u32>>,
}

impl FulltextIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether `(label, field)` is currently indexed.
    pub fn is_enabled(&self, label: &str, field: &str) -> bool {
        self.enabled
            .contains(&(label.to_string(), field.to_string()))
    }

    /// Whether any field is enabled for this label.
    pub fn has_label(&self, label: &str) -> bool {
        self.enabled.iter().any(|(l, _)| l == label)
    }

    /// Whether `field` is indexed for *any* label.
    pub fn field_indexed(&self, field: &str) -> bool {
        self.enabled.iter().any(|(_, f)| f == field)
    }

    /// Whether `field` is indexed by a label OTHER THAN `label`.
    pub fn field_indexed_by_other(&self, label: &str, field: &str) -> bool {
        self.enabled.iter().any(|(l, f)| f == field && l != label)
    }

    /// Iterate all enabled `(label, field)` pairs.
    pub fn enabled_pairs(&self) -> impl Iterator<Item = &(String, String)> {
        self.enabled.iter()
    }

    /// Enable full-text indexing for `(label, field)`.  Returns `true` if newly
    /// added, `false` if already present (idempotent for replay safety).
    pub fn enable(&mut self, label: &str, field: &str) -> bool {
        self.enabled.insert((label.to_string(), field.to_string()))
    }

    /// Disable full-text indexing for `(label, field)`.
    /// Drops all postings and doc_len entries for that field.
    /// Returns `true` if the pair was present and removed.
    pub fn disable(&mut self, label: &str, field: &str) -> bool {
        let removed = self.enabled.remove(&(label.to_string(), field.to_string()));
        if removed && !self.field_indexed(field) {
            self.postings.remove(field);
            self.doc_len.remove(field);
        }
        removed
    }

    // -----------------------------------------------------------------------
    // Incremental maintenance
    // -----------------------------------------------------------------------

    /// Add stemmed tokens (with positions) for `value` under `(node_id, field)`.
    /// Replaces any existing doc_len entry for this node.
    /// Caller is responsible for ensuring `(label, field)` is enabled.
    pub fn add_tokens(&mut self, node_id: u32, field: &str, value: &Value) {
        let stemmed = value_tokens_stemmed_with_positions(value);
        let dl = stemmed.len() as u32;

        // Update doc_len.
        let dl_col = self.doc_len.entry(field.to_string()).or_default();
        dl_col.insert(node_id, dl);

        // Update postings.
        let col = self.postings.entry(field.to_string()).or_default();
        for (tok, pos) in stemmed {
            col.entry(tok)
                .or_default()
                .entry(node_id)
                .or_default()
                .push(pos);
        }
    }

    /// Remove all tokens for `node_id` in `field`'s posting list.
    pub fn remove_node_field(&mut self, node_id: u32, field: &str) {
        if let Some(col) = self.postings.get_mut(field) {
            col.retain(|_, node_map| {
                node_map.remove(&node_id);
                !node_map.is_empty()
            });
            if col.is_empty() {
                self.postings.remove(field);
            }
        }
        if let Some(dl) = self.doc_len.get_mut(field) {
            dl.remove(&node_id);
            if dl.is_empty() {
                self.doc_len.remove(field);
            }
        }
    }

    /// Remove all tokens for `node_id` across all indexed fields.
    pub fn remove_node(&mut self, node_id: u32) {
        for col in self.postings.values_mut() {
            col.retain(|_, node_map| {
                node_map.remove(&node_id);
                !node_map.is_empty()
            });
        }
        self.postings.retain(|_, col| !col.is_empty());
        for dl in self.doc_len.values_mut() {
            dl.remove(&node_id);
        }
        self.doc_len.retain(|_, dl| !dl.is_empty());
    }

    // -----------------------------------------------------------------------
    // Search (BM25)
    // -----------------------------------------------------------------------

    /// Search a field with a v2 query.  Returns `(node_id, bm25_score)` sorted
    /// by score descending, ties by node_id ascending.  Returns empty if the
    /// field is not indexed or the query produces no groups.
    ///
    /// BM25 constants: k1 = 1.2, b = 0.75.  Scores are summed across matched
    /// OR-groups; negated atoms exclude a document; phrase atoms require
    /// positional adjacency.  If `k > 0`, only the top-k results are returned.
    pub fn search(&self, field: &str, query: &str, k: usize) -> Vec<(u32, f64)> {
        let Some(col) = self.postings.get(field) else {
            return vec![];
        };
        let groups = parse_query_v2(query);
        if groups.is_empty() {
            return vec![];
        }

        let dl_map = match self.doc_len.get(field) {
            Some(m) => m,
            None => return vec![],
        };
        let n = dl_map.len() as f64;
        if n == 0.0 {
            return vec![];
        }
        let avg_dl: f64 = dl_map.values().map(|&v| v as f64).sum::<f64>() / n;

        const K1: f64 = 1.2;
        const B: f64 = 0.75;

        let mut scores: BTreeMap<u32, f64> = BTreeMap::new();

        for group in &groups {
            // Find candidate node set for this group.
            let candidates = group_candidates(col, group);

            for node_id in candidates {
                let dl = dl_map.get(&node_id).copied().unwrap_or(1) as f64;
                let mut group_score = 0.0;

                for atom in group {
                    match atom {
                        QueryAtom::Term(t) if !t.negated && !t.prefix => {
                            // Standard BM25 term score.
                            let (df, tf) = match col.get(&t.token) {
                                Some(node_map) => {
                                    let df = node_map.len() as f64;
                                    let tf = node_map
                                        .get(&node_id)
                                        .map(|v| v.len() as f64)
                                        .unwrap_or(0.0);
                                    (df, tf)
                                }
                                None => (0.0, 0.0),
                            };
                            if tf > 0.0 {
                                let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
                                let tf_norm =
                                    tf * (K1 + 1.0) / (tf + K1 * (1.0 - B + B * dl / avg_dl));
                                group_score += idf * tf_norm;
                            }
                        }
                        QueryAtom::Term(t) if !t.negated && t.prefix => {
                            // Prefix: sum BM25 scores for all matching stemmed tokens.
                            for (tok, node_map) in col
                                .range(t.token.clone()..)
                                .take_while(|(k, _)| k.starts_with(t.token.as_str()))
                            {
                                let _ = tok;
                                let df = node_map.len() as f64;
                                let tf = node_map
                                    .get(&node_id)
                                    .map(|v| v.len() as f64)
                                    .unwrap_or(0.0);
                                if tf > 0.0 {
                                    let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
                                    let tf_norm =
                                        tf * (K1 + 1.0) / (tf + K1 * (1.0 - B + B * dl / avg_dl));
                                    group_score += idf * tf_norm;
                                }
                            }
                        }
                        QueryAtom::Term(_) => {
                            // Negated: already excluded by group_candidates.
                        }
                        QueryAtom::Phrase(tokens) => {
                            // Phrase: only score if adjacency holds (group_candidates
                            // already narrowed to nodes that have all phrase tokens,
                            // but didn't check positions).
                            if !phrase_matches_col(col, node_id, tokens) {
                                // Phrase failed adjacency — this group does not match.
                                group_score = f64::NEG_INFINITY;
                                break;
                            }
                            // Contribute BM25 score for each phrase token.
                            for tok in tokens {
                                let (df, tf) = match col.get(tok) {
                                    Some(node_map) => (
                                        node_map.len() as f64,
                                        node_map
                                            .get(&node_id)
                                            .map(|v| v.len() as f64)
                                            .unwrap_or(0.0),
                                    ),
                                    None => (0.0, 0.0),
                                };
                                if tf > 0.0 && df > 0.0 {
                                    let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
                                    let tf_norm =
                                        tf * (K1 + 1.0) / (tf + K1 * (1.0 - B + B * dl / avg_dl));
                                    group_score += idf * tf_norm;
                                }
                            }
                        }
                    }
                }

                // Negation-only groups produce group_score = 0.0 (no positive atom
                // contributes) and are suppressed here.  This is deliberate: "-term"
                // alone does not rank surviving docs — it only reduces the candidate
                // set in group_candidates.  "graph OR -embedded" therefore behaves
                // identically to "graph": the negation group is silently dropped.
                if group_score > 0.0 {
                    *scores.entry(node_id).or_insert(0.0) += group_score;
                }
            }
        }

        let mut results: Vec<(u32, f64)> = scores.into_iter().collect();
        results.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        if k > 0 {
            results.truncate(k);
        }
        results
    }

    // -----------------------------------------------------------------------
    // Rebuild
    // -----------------------------------------------------------------------

    /// Rebuild the entire index from scratch.
    ///
    /// Called once after WAL replay to correct any drift accumulated by incremental
    /// `add_tokens` / `remove_node_field` calls during per-record `apply`.
    pub fn rebuild_all(
        &mut self,
        ids: &IdMap,
        labels: &[u32],
        syms: &Interner,
        props: crate::v8::seam::ColumnsView<'_>,
    ) {
        if self.enabled.is_empty() {
            return;
        }
        let enabled_vec: Vec<(String, String)> = self.enabled.iter().cloned().collect();
        // Clear postings AND doc_len for all enabled fields.
        for (_, field) in &enabled_vec {
            self.postings.remove(field);
            self.doc_len.remove(field);
        }
        let n = ids.len() as u32;
        for id in 0..n {
            let Some(&sym) = labels.get(id as usize) else {
                continue;
            };
            if sym == u32::MAX {
                continue;
            }
            let Some(label) = syms.resolve(sym) else {
                continue;
            };
            for (lbl, field) in &enabled_vec {
                if lbl == label {
                    if let Some(vr) = props.get(id, field) {
                        let value = vr.into_value();
                        self.add_tokens(id, field, &value);
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Collect the candidate node set for one OR-group, applying positive-term
/// intersection and negated-term exclusion.  Phrase atoms are treated as a
/// conjunction of their constituent tokens for the candidate set (adjacency
/// check happens during scoring).
fn group_candidates(
    col: &BTreeMap<String, BTreeMap<u32, Vec<u32>>>,
    group: &[QueryAtom],
) -> BTreeSet<u32> {
    let has_positive = group.iter().any(|a| match a {
        QueryAtom::Term(t) => !t.negated,
        QueryAtom::Phrase(_) => true,
    });

    // If no positive constraint, start with ALL nodes in this field column.
    let mut result: Option<BTreeSet<u32>> = if has_positive {
        None
    } else {
        Some(
            col.values()
                .flat_map(|node_map| node_map.keys().copied())
                .collect(),
        )
    };

    let mut negated: BTreeSet<u32> = BTreeSet::new();

    for atom in group {
        match atom {
            QueryAtom::Term(t) if !t.negated && !t.prefix => {
                let matching: BTreeSet<u32> = col
                    .get(&t.token)
                    .map(|m| m.keys().copied().collect())
                    .unwrap_or_default();
                result = Some(match result {
                    None => matching,
                    Some(prev) => prev.intersection(&matching).copied().collect(),
                });
            }
            QueryAtom::Term(t) if !t.negated && t.prefix => {
                let matching: BTreeSet<u32> = col
                    .range(t.token.clone()..)
                    .take_while(|(k, _)| k.starts_with(t.token.as_str()))
                    .flat_map(|(_, node_map)| node_map.keys().copied())
                    .collect();
                result = Some(match result {
                    None => matching,
                    Some(prev) => prev.intersection(&matching).copied().collect(),
                });
            }
            QueryAtom::Term(t) if t.negated && !t.prefix => {
                let exclude: BTreeSet<u32> = col
                    .get(&t.token)
                    .map(|m| m.keys().copied().collect())
                    .unwrap_or_default();
                negated.extend(exclude);
            }
            QueryAtom::Term(t) if t.negated && t.prefix => {
                let exclude: BTreeSet<u32> = col
                    .range(t.token.clone()..)
                    .take_while(|(k, _)| k.starts_with(t.token.as_str()))
                    .flat_map(|(_, node_map)| node_map.keys().copied())
                    .collect();
                negated.extend(exclude);
            }
            QueryAtom::Term(_) => {}
            QueryAtom::Phrase(tokens) => {
                // Intersect candidates with nodes that have ALL phrase tokens.
                // Adjacency is checked at scoring time, not here.
                let mut phrase_candidates: Option<BTreeSet<u32>> = None;
                for tok in tokens {
                    let matching: BTreeSet<u32> = col
                        .get(tok)
                        .map(|m| m.keys().copied().collect())
                        .unwrap_or_default();
                    phrase_candidates = Some(match phrase_candidates {
                        None => matching,
                        Some(prev) => prev.intersection(&matching).copied().collect(),
                    });
                }
                let phrase_set = phrase_candidates.unwrap_or_default();
                result = Some(match result {
                    None => phrase_set,
                    Some(prev) => prev.intersection(&phrase_set).copied().collect(),
                });
            }
        }
    }

    let mut candidates = result.unwrap_or_default();
    for id in &negated {
        candidates.remove(id);
    }
    candidates
}

/// Check phrase adjacency using the index column.
fn phrase_matches_col(
    col: &BTreeMap<String, BTreeMap<u32, Vec<u32>>>,
    node_id: u32,
    tokens: &[String],
) -> bool {
    if tokens.is_empty() {
        return false;
    }
    let Some(first_positions) = col.get(&tokens[0]).and_then(|m| m.get(&node_id)) else {
        return false;
    };
    'start: for &start in first_positions {
        let mut cur = start;
        for tok in &tokens[1..] {
            cur += 1;
            let Some(positions) = col.get(tok).and_then(|m| m.get(&node_id)) else {
                continue 'start;
            };
            if positions.binary_search(&cur).is_err() {
                continue 'start;
            }
        }
        return true;
    }
    false
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::columns::ColumnStore;

    fn toks(s: &str) -> Vec<String> {
        tokenize(s)
    }

    #[test]
    fn tokenizer_basic() {
        assert_eq!(toks("Hello, World!"), vec!["hello", "world"]);
        assert_eq!(toks("rust-lang"), vec!["rust", "lang"]);
        assert_eq!(toks("abc123"), vec!["abc123"]);
        assert_eq!(toks(""), Vec::<String>::new());
    }

    #[test]
    fn tokenizer_unicode() {
        assert_eq!(toks("café"), vec!["café"]);
        assert_eq!(toks("über alles"), vec!["über", "alles"]);
    }

    #[test]
    fn stem_basic() {
        // Snowball English: -ing/-ed/-s suffixes removed.
        assert_eq!(stem("running"), "run");
        assert_eq!(stem("databases"), "databas");
        assert_eq!(stem("embedded"), "embed");
        // Single-char and non-English words are unchanged.
        assert_eq!(stem("a"), "a");
        assert_eq!(stem("rust"), "rust");
    }

    #[test]
    fn tokenize_stemmed_positions() {
        let result = tokenize_stemmed_with_positions("running around the world");
        // Positions are sequential token offsets.
        assert_eq!(result[0].0, stem("running")); // "run"
        assert_eq!(result[0].1, 0);
        assert_eq!(result[1].0, stem("around")); // "around"
        assert_eq!(result[1].1, 1);
        assert_eq!(result[2].0, stem("the")); // "the"
        assert_eq!(result[2].1, 2);
        assert_eq!(result[3].0, stem("world")); // "world"
        assert_eq!(result[3].1, 3);
    }

    #[test]
    fn parse_query_and() {
        let g = parse_query("foo bar");
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].len(), 2);
        assert_eq!(g[0][0].token, stem("foo"));
        assert_eq!(g[0][1].token, stem("bar"));
        assert!(!g[0][0].prefix);
        assert!(!g[0][0].negated);
    }

    #[test]
    fn parse_query_or() {
        let g = parse_query("foo OR bar");
        assert_eq!(g.len(), 2);
        assert_eq!(g[0][0].token, stem("foo"));
        assert_eq!(g[1][0].token, stem("bar"));
    }

    #[test]
    fn parse_query_prefix() {
        let g = parse_query("foo*");
        assert_eq!(g.len(), 1);
        assert!(g[0][0].prefix);
        assert_eq!(g[0][0].token, "foo"); // prefix NOT stemmed
    }

    #[test]
    fn parse_query_negation() {
        let g = parse_query("-embedded rust");
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].len(), 2);
        assert!(g[0][0].negated);
        assert_eq!(g[0][0].token, stem("embedded"));
        assert!(!g[0][1].negated);
        assert_eq!(g[0][1].token, stem("rust"));
    }

    #[test]
    fn parse_query_explicit_and_keyword() {
        let g = parse_query("foo AND bar");
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].len(), 2);
    }

    #[test]
    fn parse_query_or_case_insensitive() {
        let g = parse_query("a or b");
        assert_eq!(g.len(), 2);
    }

    #[test]
    fn parse_query_v2_phrase() {
        let g = parse_query_v2("\"graph database\"");
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].len(), 1);
        match &g[0][0] {
            QueryAtom::Phrase(tokens) => {
                assert_eq!(tokens[0], stem("graph"));
                assert_eq!(tokens[1], stem("database"));
            }
            _ => panic!("expected Phrase"),
        }
    }

    #[test]
    fn eval_query_str_basic() {
        assert!(eval_query_str("hello world rust", "hello world"));
        assert!(!eval_query_str("hello world", "hello rust"));
        assert!(eval_query_str("hello world", "hello OR rust"));
    }

    #[test]
    fn eval_query_str_stemming() {
        // "running" and "run" share the same stem → match.
        assert!(eval_query_str("I am running fast", "running"));
        assert!(eval_query_str("I am running fast", "run"));
        // "databases" stems to "databas"; query "databases" also stems → match.
        assert!(eval_query_str("graph databases embedded", "databases"));
    }

    #[test]
    fn eval_query_str_phrase() {
        // Adjacent → matches.
        assert!(eval_query_str(
            "graph database embedded",
            "\"graph database\""
        ));
        // Not adjacent → no match.
        assert!(!eval_query_str(
            "graph embedded database",
            "\"graph database\""
        ));
        // Phrase with stemming: "running fast" stem = ["run", "fast"].
        assert!(eval_query_str(
            "I am running fast today",
            "\"running fast\""
        ));
    }

    #[test]
    fn eval_query_str_negation() {
        // Has "embedded" → excluded.
        assert!(!eval_query_str(
            "graph embedded database",
            "-embedded graph"
        ));
        // No "embedded" → not excluded.
        assert!(eval_query_str("graph database", "-embedded graph"));
    }

    #[test]
    fn eval_query_str_prefix() {
        assert!(eval_query_str("embedding graph", "emb*"));
        assert!(!eval_query_str("graph only", "emb*"));
    }

    #[test]
    fn index_and_search_bm25_basic() {
        let mut idx = FulltextIndex::new();
        idx.enable("Person", "bio");
        idx.add_tokens(0, "bio", &Value::Str("I love Rust and databases".into()));
        idx.add_tokens(1, "bio", &Value::Str("Python developer here".into()));

        // BM25 search — "rust" only in doc 0.
        let r = idx.search("bio", "rust", 0);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, 0);
        assert!(r[0].1 > 0.0);

        // "rust OR python" → both docs.
        let r2 = idx.search("bio", "rust OR python", 0);
        assert_eq!(r2.len(), 2);

        // "rust databases" (AND) → only doc 0 has both (stemmed: "rust" and "databas").
        let r3 = idx.search("bio", "rust databases", 0);
        assert_eq!(r3.len(), 1);
        assert_eq!(r3[0].0, 0);

        // "rust AND python" (AND) → no doc has both.
        let r4 = idx.search("bio", "rust AND python", 0);
        assert!(r4.is_empty());
    }

    /// BM25 ranking: rarer-term doc ranks above common-term doc.
    ///
    /// Corpus:
    ///   node 0 ("alpha"): dl=1, "alpha" has df=1 → high IDF
    ///   node 1 ("beta"):  dl=1, "beta" has df=2 → lower IDF
    ///   node 2 ("beta"):  dl=1, contributes to df("beta")=2
    ///
    /// Query: "alpha OR beta"
    ///   N=3, avg_dl=1.0
    ///   IDF("alpha") = ln((3-1+0.5)/(1+0.5)+1) = ln(2.667) ≈ 0.981
    ///   IDF("beta")  = ln((3-2+0.5)/(2+0.5)+1) = ln(1.6)   ≈ 0.470
    ///   tf_norm(all) = 1*2.2/(1+1.2*(0.25+0.75*1/1)) = 2.2/2.2 = 1.0
    ///   score(node 0) ≈ 0.981  (from "alpha" group)
    ///   score(node 1) ≈ 0.470  (from "beta" group)
    ///   score(node 2) ≈ 0.470  (from "beta" group; tiebreak: node 1 < node 2)
    ///
    /// Expected order: 0 > 1 = 2 (1 before 2 by node_id tiebreak).
    #[test]
    fn bm25_rarer_term_ranks_higher() {
        let mut idx = FulltextIndex::new();
        idx.enable("Doc", "body");
        idx.add_tokens(0, "body", &Value::Str("alpha".into()));
        idx.add_tokens(1, "body", &Value::Str("beta".into()));
        idx.add_tokens(2, "body", &Value::Str("beta".into()));

        let r = idx.search("body", "alpha OR beta", 0);
        assert_eq!(r.len(), 3);
        // Doc 0 (rare "alpha") ranks first.
        assert_eq!(r[0].0, 0, "rarer-term doc must rank first");
        // Docs 1 and 2 tie on score; tiebreak by node_id ascending.
        assert_eq!(r[1].0, 1);
        assert_eq!(r[2].0, 2);
        // Scores strictly ordered.
        assert!(r[0].1 > r[1].1, "alpha (df=1) must score above beta (df=2)");
    }

    #[test]
    fn bm25_stemming_matches_root_form() {
        let mut idx = FulltextIndex::new();
        idx.enable("Doc", "body");
        // Doc contains "run" only (stem of "running" = "run").
        idx.add_tokens(0, "body", &Value::Str("run".into()));

        // Querying "running" → stems to "run" → matches doc 0.
        let r = idx.search("body", "running", 0);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, 0);
    }

    #[test]
    fn bm25_phrase_adjacent_only() {
        let mut idx = FulltextIndex::new();
        idx.enable("Doc", "body");
        // Doc 0: "graph" and "database" are adjacent.
        idx.add_tokens(0, "body", &Value::Str("graph database embedded".into()));
        // Doc 1: scattered — "graph" and "database" not adjacent.
        idx.add_tokens(1, "body", &Value::Str("graph embedded database".into()));

        let r = idx.search("body", "\"graph database\"", 0);
        assert_eq!(r.len(), 1, "only adjacent doc must match phrase");
        assert_eq!(r[0].0, 0);
    }

    #[test]
    fn bm25_negation_excludes() {
        let mut idx = FulltextIndex::new();
        idx.enable("Doc", "body");
        idx.add_tokens(0, "body", &Value::Str("graph database embedded".into()));
        idx.add_tokens(1, "body", &Value::Str("graph database".into()));

        // "-embedded graph" → doc 0 excluded (has "embedded"); doc 1 matches.
        let r = idx.search("body", "-embedded graph", 0);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, 1);
    }

    #[test]
    fn prefix_search() {
        let mut idx = FulltextIndex::new();
        idx.enable("Doc", "body");
        idx.add_tokens(0, "body", &Value::Str("embedding graph".into()));
        idx.add_tokens(1, "body", &Value::Str("python java".into()));

        let r = idx.search("body", "emb*", 0);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, 0);
    }

    #[test]
    fn search_case_insensitive() {
        let mut idx = FulltextIndex::new();
        idx.enable("Doc", "body");
        idx.add_tokens(0, "body", &Value::Str("Rust is great".into()));

        // Query in any case → same stemmed token → matches.
        assert_eq!(idx.search("body", "RUST", 0).len(), 1);
        assert_eq!(idx.search("body", "Rust", 0).len(), 1);
        assert_eq!(idx.search("body", "rust", 0).len(), 1);
    }

    #[test]
    fn search_empty_query_returns_empty() {
        let mut idx = FulltextIndex::new();
        idx.enable("Doc", "body");
        idx.add_tokens(0, "body", &Value::Str("hello world".into()));
        assert!(idx.search("body", "", 0).is_empty());
        assert!(idx.search("body", "   ", 0).is_empty());
    }

    #[test]
    fn search_k_truncates() {
        let mut idx = FulltextIndex::new();
        idx.enable("Doc", "f");
        for i in 0..5u32 {
            idx.add_tokens(i, "f", &Value::Str(format!("word{i}")));
        }
        let r = idx.search("f", "word0 OR word1 OR word2 OR word3 OR word4", 3);
        assert_eq!(r.len(), 3);
    }

    #[test]
    fn remove_node_field_clears_tokens() {
        let mut idx = FulltextIndex::new();
        idx.enable("A", "f");
        idx.add_tokens(0, "f", &Value::Str("hello world".into()));
        idx.remove_node_field(0, "f");
        assert!(idx.search("f", "hello", 0).is_empty());
    }

    #[test]
    fn remove_node_clears_all_fields() {
        let mut idx = FulltextIndex::new();
        idx.enable("A", "f");
        idx.enable("A", "g");
        idx.add_tokens(0, "f", &Value::Str("foo".into()));
        idx.add_tokens(0, "g", &Value::Str("bar".into()));
        idx.remove_node(0);
        assert!(idx.search("f", "foo", 0).is_empty());
        assert!(idx.search("g", "bar", 0).is_empty());
    }

    #[test]
    fn unindexed_field_returns_empty() {
        let idx = FulltextIndex::new();
        assert!(idx.search("notindexed", "anything", 0).is_empty());
    }

    #[test]
    fn rebuild_all_restores_index() {
        let mut ids = IdMap::new();
        let mut syms = Interner::new();
        let mut labels: Vec<u32> = Vec::new();
        let mut props = ColumnStore::new();

        let id0 = ids.get_or_insert("k0");
        let sym = syms.intern("Person");
        labels.resize(id0 as usize + 1, u32::MAX);
        labels[id0 as usize] = sym;
        props.set(id0, "bio", Value::Str("I love Rust".into()));

        let mut idx = FulltextIndex::new();
        idx.enable("Person", "bio");
        assert!(idx.search("bio", "rust", 0).is_empty());

        idx.rebuild_all(
            &ids,
            &labels,
            &syms,
            crate::v8::seam::ColumnsView::owned(&props),
        );
        // "Rust" → stem → "rust" → found.
        let r = idx.search("bio", "rust", 0);
        assert_eq!(r.len(), 1);
    }

    /// Pin: mid-token `*` is stripped to an exact (stemmed) term; trailing `*` is prefix.
    #[test]
    fn mid_token_star_is_stripped_to_exact() {
        // Index-side tokenizer: mid-star SPLITS document text.
        let toks = tokenize("ru*st");
        assert_eq!(toks, vec!["ru".to_string(), "st".to_string()]);

        // Query-side parse_query: "ru*st" has no trailing `*` → exact term "rust"
        // (the mid-star is stripped; "rust" is then stemmed → "rust").
        let groups = parse_query("ru*st");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].len(), 1);
        assert!(!groups[0][0].prefix, "mid-token * must NOT set prefix flag");
        assert_eq!(groups[0][0].token, stem("rust")); // "rust"

        // Trailing-star prefix query still works.
        let mut idx = FulltextIndex::new();
        idx.enable("T", "f");
        idx.add_tokens(0, "f", &Value::Str("rust embedded".into()));
        assert_eq!(idx.search("f", "ru*", 0).len(), 1);
        assert_eq!(idx.search("f", "rust", 0).len(), 1);
    }

    /// Pin: a pure negation query (no positive atom) always returns empty.
    ///
    /// When a group has no positive atoms, `group_candidates` starts with ALL
    /// nodes in the field and removes matching ones.  However, the scoring loop
    /// produces `group_score = 0.0` (no positive atom contributes), and the
    /// `if group_score > 0.0` guard then suppresses every candidate.  The result
    /// is deliberately empty — negation alone does not rank surviving docs.
    #[test]
    fn all_negation_query_returns_empty() {
        let mut idx = FulltextIndex::new();
        idx.enable("Doc", "body");
        idx.add_tokens(0, "body", &Value::Str("graph database embedded".into()));
        idx.add_tokens(1, "body", &Value::Str("graph database".into()));

        let r = idx.search("body", "-embedded", 0);
        assert!(r.is_empty(), "pure negation query must return empty");
    }

    /// Pin: "graph OR -embedded" behaves identically to "graph".
    ///
    /// The negation-only OR group ("-embedded") produces `group_score = 0.0` in
    /// the scoring loop (no positive atom) and is suppressed by the guard, so it
    /// adds nothing to document scores.  The result key ordering is the same as
    /// the plain "graph" query.
    #[test]
    fn negation_only_or_group_contributes_nothing() {
        let mut idx = FulltextIndex::new();
        idx.enable("Doc", "body");
        idx.add_tokens(0, "body", &Value::Str("graph database".into()));
        idx.add_tokens(1, "body", &Value::Str("rust embedded".into()));

        let keys_plain: Vec<u32> = idx
            .search("body", "graph", 0)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        let keys_or_neg: Vec<u32> = idx
            .search("body", "graph OR -embedded", 0)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(
            keys_plain, keys_or_neg,
            "negation-only OR group must not change result ordering"
        );
    }

    /// Deterministic phrase-adjacency invariant: engine results agree with a
    /// fully independent in-test adjacency checker.
    ///
    /// The checker is truly independent: it inlines its own tokenizer (split on
    /// non-alphanumeric, lowercase) and does NOT call `tokenize_stemmed_with_positions`
    /// or any other core-storage function.  To sidestep reimplementing Snowball,
    /// the corpus is constrained to stem-stable words (stem(w) == w), which are
    /// verified by assertions at test setup.  For stable words the engine's stemmed
    /// tokens equal the raw lowercase tokens, so the checker's array-index walk
    /// and the engine's position-map path must agree on the match set — any
    /// position-assignment bug would cause a disagreement.
    ///
    /// Stem-stable corpus words: "graph", "node", "disk", "wal", "commit"
    ///
    /// Corpus:
    ///   doc 0: "graph node disk"        — phrase "graph node" adjacent at idx 0,1
    ///   doc 1: "graph disk node"        — scattered  (gap: graph idx 0, node idx 2)
    ///   doc 2: "commit graph node wal"  — phrase "graph node" adjacent at idx 1,2
    ///
    /// Expected: docs 0 and 2 match; doc 1 does not.
    #[test]
    fn phrase_adjacency_engine_matches_naive_checker() {
        // Verify stem-stability so the independent checker (no stemming) is valid.
        for w in &["graph", "node", "disk", "wal", "commit"] {
            assert_eq!(stem(w), *w, "word '{w}' must be its own Snowball stem");
        }

        let mut idx = FulltextIndex::new();
        idx.enable("Doc", "body");
        idx.add_tokens(0, "body", &Value::Str("graph node disk".into()));
        idx.add_tokens(1, "body", &Value::Str("graph disk node".into()));
        idx.add_tokens(2, "body", &Value::Str("commit graph node wal".into()));

        // Naive checker: inline tokenizer + array-index walk.
        // Zero core-storage imports — no shared position-assignment code.
        let phrase_words: &[&str] = &["graph", "node"];
        let naive_check = |doc_text: &str| -> bool {
            // Inline tokenizer: split on non-alphanumeric, lowercase.
            let mut toks: Vec<String> = Vec::new();
            let mut cur = String::new();
            for ch in doc_text.chars() {
                if ch.is_alphanumeric() {
                    for lc in ch.to_lowercase() {
                        cur.push(lc);
                    }
                } else if !cur.is_empty() {
                    toks.push(std::mem::take(&mut cur));
                }
            }
            if !cur.is_empty() {
                toks.push(cur);
            }
            // Adjacency walk: phrase must appear as a contiguous sub-sequence.
            for i in 0..toks.len() {
                if toks[i] == phrase_words[0]
                    && i + phrase_words.len() <= toks.len()
                    && phrase_words
                        .iter()
                        .enumerate()
                        .all(|(j, w)| toks[i + j] == *w)
                {
                    return true;
                }
            }
            false
        };

        let docs = [
            (0u32, "graph node disk"),
            (1u32, "graph disk node"),
            (2u32, "commit graph node wal"),
        ];

        let engine_ids: BTreeSet<u32> = idx
            .search("body", "\"graph node\"", 0)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        let naive_ids: BTreeSet<u32> = docs
            .iter()
            .filter(|(_, text)| naive_check(text))
            .map(|(id, _)| *id)
            .collect();

        assert_eq!(
            engine_ids, naive_ids,
            "engine phrase results must agree with independent naive adjacency checker"
        );
        assert!(engine_ids.contains(&0), "doc 0 (adjacent) must match");
        assert!(!engine_ids.contains(&1), "doc 1 (scattered) must not match");
        assert!(engine_ids.contains(&2), "doc 2 (preceded) must match");
    }

    /// Pin: phrase queries do NOT match across Value::List element boundaries.
    ///
    /// `value_tokens_stemmed_with_positions` inserts a POSITION_GAP (> 1) between
    /// list elements.  Phrases require consecutive positions (delta == 1), so the
    /// gap breaks cross-boundary adjacency.
    ///
    /// Corpus:
    ///   doc 0: List ["graph", "database"] — last token of elem 0 is pos 0,
    ///          first token of elem 1 is pos 0+1+GAP = 3.  Delta = 3, not 1.
    ///   doc 1: Str "graph database"       — tokens at pos 0, 1.  Delta = 1.
    ///
    /// Expected: only doc 1 matches the phrase "graph database".
    #[test]
    fn phrase_does_not_match_across_list_boundary() {
        let mut idx = FulltextIndex::new();
        idx.enable("Doc", "body");
        // Two separate list elements — phrase must NOT span them.
        idx.add_tokens(
            0,
            "body",
            &Value::List(vec![
                Value::Str("graph".into()),
                Value::Str("database".into()),
            ]),
        );
        // Single string — tokens are consecutive.
        idx.add_tokens(1, "body", &Value::Str("graph database".into()));

        let r = idx.search("body", "\"graph database\"", 0);
        assert_eq!(
            r.len(),
            1,
            "phrase must not match across list element boundary"
        );
        assert_eq!(r[0].0, 1, "only single-string doc must match");
    }
}
