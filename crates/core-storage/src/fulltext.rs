/// Full-text-lite incremental inverted index.
///
/// ## Scope (v1)
/// - Tokenization: split on non-alphanumeric, lowercase each run of `char::is_alphanumeric`.
/// - Queries: space-separated terms, AND by default, explicit OR between groups.
///   Trailing `*` on a term enables prefix matching.
/// - No stemming, no phrase matching, no relevance scoring beyond match-count-desc.
///
/// ## Memory cost model
/// O(unique_tokens × avg_postings_per_token) per indexed field. Each token string
/// is stored once as a BTreeMap key; each node_id appears once per token it contains.
/// For a corpus of N nodes each averaging T distinct tokens per field, memory is
/// O(N × T) per indexed field. No cap is enforced in v1 — callers should consider
/// selective enable_fulltext declarations on high-value fields only.
use crate::columns::ColumnStore;
use crate::idmap::IdMap;
use crate::interner::Interner;
use crate::types::Value;
use std::collections::{BTreeMap, BTreeSet};

// ---------------------------------------------------------------------------
// Tokenizer
// ---------------------------------------------------------------------------

/// Tokenize a string: split on any non-`char::is_alphanumeric` character,
/// lowercase each resulting run.  Non-string `Value`s produce no tokens.
/// `Value::List` tokenizes each `Str` element independently.
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

/// Extract all tokens from a `Value`.
fn value_tokens(v: &Value) -> Vec<String> {
    match v {
        Value::Str(s) => tokenize(s),
        Value::List(items) => items
            .iter()
            .flat_map(|item| {
                if let Value::Str(s) = item {
                    tokenize(s)
                } else {
                    vec![]
                }
            })
            .collect(),
        _ => vec![],
    }
}

// ---------------------------------------------------------------------------
// Query parsing
// ---------------------------------------------------------------------------

/// A single search term: lowercased alphanumeric token + optional prefix flag.
#[derive(Debug, Clone)]
pub struct Term {
    /// Lowercased alphanumeric token.
    pub token: String,
    /// If true, match any posting token that *starts with* `token` (trailing `*`).
    pub prefix: bool,
}

/// Parse a query into OR-groups of AND-terms.
///
/// Grammar: `query := group ('OR' group)*`
///           `group := term ('AND'? term)*`
///           `term  := <word> '*'?`
///
/// `AND` and `OR` are case-insensitive keywords.  `AND` between consecutive
/// words is the default and may be omitted.  Empty groups (produced by `OR OR`
/// or a leading / trailing OR) are silently dropped.
///
/// Examples:
/// - `"rust lang"` → `[[rust, lang]]`
/// - `"rust OR python"` → `[[rust], [python]]`
/// - `"rust* lang OR py*"` → `[[rust*(prefix), lang], [py*(prefix)]]`
pub fn parse_query(query: &str) -> Vec<Vec<Term>> {
    let mut groups: Vec<Vec<Term>> = vec![vec![]];
    for word in query.split_whitespace() {
        match word.to_ascii_uppercase().as_str() {
            "OR" => groups.push(vec![]),
            "AND" => { /* default; skip */ }
            _ => {
                let (raw, prefix) = if let Some(stripped) = word.strip_suffix('*') {
                    (stripped, true)
                } else {
                    (word, false)
                };
                let token: String = raw
                    .chars()
                    .filter(|c| c.is_alphanumeric())
                    .flat_map(|c| c.to_lowercase())
                    .collect();
                if !token.is_empty() {
                    groups.last_mut().unwrap().push(Term { token, prefix });
                }
            }
        }
    }
    groups.retain(|g| !g.is_empty());
    groups
}

/// Evaluate a pre-parsed query against a node's token set (for Cypher WHERE use).
/// Returns true if the node matches any OR-group (i.e., matches all AND-terms
/// in at least one group).
pub fn eval_query_terms(node_tokens: &BTreeSet<String>, groups: &[Vec<Term>]) -> bool {
    if groups.is_empty() {
        return false;
    }
    'outer: for group in groups {
        for term in group {
            let matched = if term.prefix {
                node_tokens.iter().any(|t| t.starts_with(term.token.as_str()))
            } else {
                node_tokens.contains(&term.token)
            };
            if !matched {
                continue 'outer;
            }
        }
        return true; // all terms in this group matched
    }
    false
}

/// Convenience: parse and evaluate a raw query string against a set of tokens.
pub fn eval_query(node_tokens: &BTreeSet<String>, query: &str) -> bool {
    let groups = parse_query(query);
    eval_query_terms(node_tokens, &groups)
}

// ---------------------------------------------------------------------------
// FulltextIndex
// ---------------------------------------------------------------------------

/// Incremental inverted index for full-text search.
///
/// Enabled per `(label, field)` pair via [`FulltextIndex::enable`].
/// Only nodes of the declared label have the declared field indexed.
/// Index maintenance is incremental: set_prop / delete / remove_prop
/// update only the affected postings.  WAL replay calls [`FulltextIndex::rebuild_all`]
/// after full replay to correct any drift accumulated during per-record `apply` calls.
#[derive(Debug, Default)]
pub struct FulltextIndex {
    /// Enabled `(label, field)` pairs.
    enabled: BTreeSet<(String, String)>,
    /// Inverted index: `field -> token -> set of node_id`.
    ///
    /// Field is the outer key so searches over one field scan only that field's
    /// token map.  A field shared by multiple labels merges their node_ids.
    postings: BTreeMap<String, BTreeMap<String, BTreeSet<u32>>>,
}

impl FulltextIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether `(label, field)` is currently indexed.
    pub fn is_enabled(&self, label: &str, field: &str) -> bool {
        self.enabled.contains(&(label.to_string(), field.to_string()))
    }

    /// Whether any field is enabled for this label.
    pub fn has_label(&self, label: &str) -> bool {
        self.enabled.iter().any(|(l, _)| l == label)
    }

    /// Whether `field` is indexed for *any* label.
    pub fn field_indexed(&self, field: &str) -> bool {
        self.enabled.iter().any(|(_, f)| f == field)
    }

    /// Iterate all enabled `(label, field)` pairs.
    pub fn enabled_pairs(&self) -> impl Iterator<Item = &(String, String)> {
        self.enabled.iter()
    }

    /// Enable full-text indexing for `(label, field)`.  Returns `true` if newly
    /// added, `false` if already present (idempotent for replay safety).
    pub fn enable(&mut self, label: &str, field: &str) -> bool {
        self.enabled
            .insert((label.to_string(), field.to_string()))
    }

    /// Disable full-text indexing for `(label, field)`.
    /// Drops all postings for that field that belong to nodes of `label`.
    /// Returns `true` if the pair was present and removed.
    ///
    /// Note: this removes postings for ALL node_ids under that field
    /// because we do not track per-posting which label produced it.
    /// After disable, if another label still has the same field enabled,
    /// those nodes' tokens remain.  Callers should call `rebuild_all`
    /// to purge stale postings when label-level precision is required.
    /// For v1, we clear the whole field's postings on disable and rely
    /// on rebuild_all (called on open) to restore surviving-label entries.
    pub fn disable(&mut self, label: &str, field: &str) -> bool {
        let removed = self
            .enabled
            .remove(&(label.to_string(), field.to_string()));
        if removed && !self.field_indexed(field) {
            // No other label indexes this field — drop the whole column.
            self.postings.remove(field);
        }
        // If another label still indexes this field, rebuild_all on next open
        // will re-populate only the surviving label's nodes.
        removed
    }

    // -----------------------------------------------------------------------
    // Incremental maintenance
    // -----------------------------------------------------------------------

    /// Add tokens for `value` under `(node_id, field)`.
    /// Caller is responsible for ensuring `(label, field)` is enabled.
    pub fn add_tokens(&mut self, node_id: u32, field: &str, value: &Value) {
        let col = self.postings.entry(field.to_string()).or_default();
        for tok in value_tokens(value) {
            col.entry(tok).or_default().insert(node_id);
        }
    }

    /// Remove all tokens for `node_id` in `field`'s posting list.
    pub fn remove_node_field(&mut self, node_id: u32, field: &str) {
        if let Some(col) = self.postings.get_mut(field) {
            col.retain(|_, ids| {
                ids.remove(&node_id);
                !ids.is_empty()
            });
            if col.is_empty() {
                self.postings.remove(field);
            }
        }
    }

    /// Remove all tokens for `node_id` across all indexed fields.
    pub fn remove_node(&mut self, node_id: u32) {
        for col in self.postings.values_mut() {
            col.retain(|_, ids| {
                ids.remove(&node_id);
                !ids.is_empty()
            });
        }
        self.postings.retain(|_, col| !col.is_empty());
    }

    // -----------------------------------------------------------------------
    // Search
    // -----------------------------------------------------------------------

    /// Search a field with a query.  Returns `(node_id, match_count)` sorted
    /// by match_count descending, ties by node_id ascending.  Returns empty
    /// if the field is not indexed or the query is empty.
    ///
    /// `match_count` = number of OR-groups matched by the node (maximum: number
    /// of OR-groups in the query).  AND queries always return match_count = 1 for
    /// every result.
    ///
    /// Callers should filter results by resolving node_ids through `IdMap::key_of`
    /// to exclude tombstoned nodes.
    pub fn search(&self, field: &str, query: &str) -> Vec<(u32, usize)> {
        let Some(col) = self.postings.get(field) else {
            return vec![];
        };
        let groups = parse_query(query);
        if groups.is_empty() {
            return vec![];
        }

        let mut counts: BTreeMap<u32, usize> = BTreeMap::new();
        for group in &groups {
            let matching = and_match(col, group);
            for id in matching {
                *counts.entry(id).or_insert(0) += 1;
            }
        }

        let mut results: Vec<(u32, usize)> = counts.into_iter().collect();
        // Sort: match_count desc, then node_id asc (stable ranking)
        results.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        results
    }

    // -----------------------------------------------------------------------
    // Rebuild
    // -----------------------------------------------------------------------

    /// Rebuild the entire index from scratch.
    ///
    /// Called once after WAL replay to correct any drift accumulated by incremental
    /// `add_tokens` / `remove_node_field` calls during per-record `apply`.
    /// Walks all live nodes, resolves their labels, and re-indexes every
    /// enabled `(label, field)` pair.
    pub fn rebuild_all(
        &mut self,
        ids: &IdMap,
        labels: &[u32],
        syms: &Interner,
        props: &ColumnStore,
    ) {
        if self.enabled.is_empty() {
            return;
        }
        // Collect enabled pairs before any mutable borrows to satisfy borrow checker.
        let enabled_vec: Vec<(String, String)> = self.enabled.iter().cloned().collect();
        // Clear all postings for enabled fields; preserve enabled set.
        for (_, field) in &enabled_vec {
            self.postings.remove(field);
        }
        let n = ids.len() as u32;
        for id in 0..n {
            let Some(&sym) = labels.get(id as usize) else {
                continue;
            };
            if sym == u32::MAX {
                continue; // tombstoned
            }
            let Some(label) = syms.resolve(sym) else {
                continue;
            };
            for (lbl, field) in &enabled_vec {
                if lbl == label {
                    if let Some(value) = props.get(id, field) {
                        self.add_tokens(id, field, value);
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Find the intersection of node_id sets for all AND-terms in a group.
fn and_match(col: &BTreeMap<String, BTreeSet<u32>>, terms: &[Term]) -> BTreeSet<u32> {
    let mut result: Option<BTreeSet<u32>> = None;
    for term in terms {
        let matching: BTreeSet<u32> = if term.prefix {
            // Filter all tokens that start with the prefix; union their postings.
            // O(unique_tokens) — acceptable for v1 where token maps are small.
            col.iter()
                .filter(|(k, _)| k.starts_with(term.token.as_str()))
                .flat_map(|(_, ids)| ids.iter().copied())
                .collect()
        } else {
            col.get(&term.token).cloned().unwrap_or_default()
        };
        result = Some(match result {
            None => matching,
            Some(prev) => prev.intersection(&matching).copied().collect(),
        });
    }
    result.unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
        // Unicode letters are alphanumeric and included
        assert_eq!(toks("café"), vec!["café"]);
        assert_eq!(toks("über alles"), vec!["über", "alles"]);
    }

    #[test]
    fn parse_query_and() {
        let g = parse_query("foo bar");
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].len(), 2);
        assert_eq!(g[0][0].token, "foo");
        assert_eq!(g[0][1].token, "bar");
        assert!(!g[0][0].prefix);
    }

    #[test]
    fn parse_query_or() {
        let g = parse_query("foo OR bar");
        assert_eq!(g.len(), 2);
        assert_eq!(g[0][0].token, "foo");
        assert_eq!(g[1][0].token, "bar");
    }

    #[test]
    fn parse_query_prefix() {
        let g = parse_query("foo*");
        assert_eq!(g.len(), 1);
        assert!(g[0][0].prefix);
        assert_eq!(g[0][0].token, "foo");
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
    fn eval_query_and_all_must_match() {
        let toks: BTreeSet<_> = ["hello", "world"].iter().map(|s| s.to_string()).collect();
        assert!(eval_query(&toks, "hello world"));
        assert!(!eval_query(&toks, "hello rust"));
    }

    #[test]
    fn eval_query_or_any_group_matches() {
        let toks: BTreeSet<_> = ["hello"].iter().map(|s| s.to_string()).collect();
        assert!(eval_query(&toks, "hello OR rust"));
        assert!(eval_query(&toks, "nope OR hello"));
        assert!(!eval_query(&toks, "nope OR missing"));
    }

    #[test]
    fn eval_query_prefix() {
        let toks: BTreeSet<_> = ["rustlang", "python"].iter().map(|s| s.to_string()).collect();
        assert!(eval_query(&toks, "rust*"));
        assert!(!eval_query(&toks, "java*"));
    }

    #[test]
    fn index_and_search_basic() {
        let mut idx = FulltextIndex::new();
        idx.enable("Person", "bio");
        idx.add_tokens(0, "bio", &Value::Str("I love Rust and databases".into()));
        idx.add_tokens(1, "bio", &Value::Str("Python developer here".into()));

        let r = idx.search("bio", "rust");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, 0);

        let r2 = idx.search("bio", "rust OR python");
        assert_eq!(r2.len(), 2);

        let r3 = idx.search("bio", "rust databases");
        assert_eq!(r3.len(), 1);
        assert_eq!(r3[0].0, 0);

        let r4 = idx.search("bio", "rust AND python");
        assert!(r4.is_empty()); // no node has both
    }

    #[test]
    fn search_case_insensitive() {
        let mut idx = FulltextIndex::new();
        idx.enable("Doc", "body");
        idx.add_tokens(0, "body", &Value::Str("Rust is great".into()));

        assert_eq!(idx.search("body", "RUST").len(), 1);
        assert_eq!(idx.search("body", "Rust").len(), 1);
        assert_eq!(idx.search("body", "rust").len(), 1);
    }

    #[test]
    fn remove_node_field_clears_tokens() {
        let mut idx = FulltextIndex::new();
        idx.enable("A", "f");
        idx.add_tokens(0, "f", &Value::Str("hello world".into()));
        idx.remove_node_field(0, "f");
        assert!(idx.search("f", "hello").is_empty());
    }

    #[test]
    fn remove_node_clears_all_fields() {
        let mut idx = FulltextIndex::new();
        idx.enable("A", "f");
        idx.enable("A", "g");
        idx.add_tokens(0, "f", &Value::Str("foo".into()));
        idx.add_tokens(0, "g", &Value::Str("bar".into()));
        idx.remove_node(0);
        assert!(idx.search("f", "foo").is_empty());
        assert!(idx.search("g", "bar").is_empty());
    }

    #[test]
    fn unindexed_field_returns_empty() {
        let idx = FulltextIndex::new();
        assert!(idx.search("notindexed", "anything").is_empty());
    }

    #[test]
    fn prefix_search() {
        let mut idx = FulltextIndex::new();
        idx.enable("Doc", "body");
        idx.add_tokens(0, "body", &Value::Str("rustlang rusty".into()));
        idx.add_tokens(1, "body", &Value::Str("python java".into()));

        let r = idx.search("body", "rust*");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, 0);

        let r2 = idx.search("body", "java* OR rust*");
        assert_eq!(r2.len(), 2);
    }

    #[test]
    fn ranking_by_match_count_desc() {
        let mut idx = FulltextIndex::new();
        idx.enable("A", "f");
        // node 0 matches both OR-groups; node 1 matches one
        idx.add_tokens(0, "f", &Value::Str("alpha beta".into()));
        idx.add_tokens(1, "f", &Value::Str("beta".into()));

        let r = idx.search("f", "alpha OR beta");
        assert_eq!(r[0].0, 0); // higher match count first
        assert_eq!(r[0].1, 2);
        assert_eq!(r[1].0, 1);
        assert_eq!(r[1].1, 1);
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
        // Intentionally do NOT add_tokens — rebuild should restore
        assert!(idx.search("bio", "rust").is_empty());

        idx.rebuild_all(&ids, &labels, &syms, &props);
        let r = idx.search("bio", "rust");
        assert_eq!(r.len(), 1);
    }
}
