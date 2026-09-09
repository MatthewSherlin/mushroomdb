//! `brief` — the repository in one block, computed once per session.
//!
//! A `SessionStart` hook runs before the assistant has asked anything, so the
//! brief cannot be about a question: it is the orientation every question
//! afterwards starts from. What it names is what the graph already says is
//! central — the files the most rank flows to, and the symbols the most other
//! symbols call — plus how big the graph is and which commit it was synced to.
//!
//! # Why it is byte-stable
//!
//! The host caches the hook's output for the whole session, so the same store
//! must render the same bytes however often it is asked. Nothing here reads a
//! clock: unlike [`repo_map`](super::repo_map) there is no sync *age*, only
//! the sha, and no window measured against a "now". Every collection is
//! sorted with the ties broken on the key, so hash iteration order cannot
//! reach the output either.
//!
//! # Where the two rankings come from
//!
//! | Section | From |
//! |---|---|
//! | key files | the same [`file_pagerank`] `map` ranks with — deeper, not different |
//! | key symbols | incoming `CALLS`, ties on the key |
//!
//! Sharing `map`'s ranking is the point: two tools that disagreed about which
//! files matter would each be wrong half the time.

use crate::db::GraphDb;
use crate::repograph::facts::str_prop;
use crate::repograph::map::{file_pagerank, SYNC_KEY};
use crate::repograph::render::{basename, dir_components, sanitize, top_tokens};
use core_storage::fs::Fs;
use core_storage::Value;
use serde::Serialize;
use std::collections::BTreeMap;

/// Characters of the synced sha the brief prints — the usual abbreviation,
/// and the same width [`render_map`](super::render_map) uses.
const SHORT_SHA: usize = 7;
/// Subdirectory names a file's role may be built from.
const ROLE_TOKENS: usize = 2;

/// How much of each ranking the brief lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BriefOptions {
    /// Files listed, most central first.
    pub max_files: usize,
    /// Symbols listed, most called first.
    pub max_symbols: usize,
}

impl Default for BriefOptions {
    fn default() -> Self {
        Self {
            max_files: 25,
            max_symbols: 25,
        }
    }
}

/// The repository, as a session starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BriefReport {
    /// The repository's own name: the last segment of the path the `GitSync`
    /// marker records. Empty on a store no repository was ingested into.
    pub repo: String,
    pub files: usize,
    pub symbols: usize,
    pub edges: usize,
    /// The abbreviated sha the store was synced to. `None` without a marker.
    /// The sha and not an age: an age would change between two prompts of the
    /// same session and the host caches this output.
    pub last_sync: Option<String>,
    /// `(path, role)`, most central first, ties on the path. The role is
    /// empty whenever the path has already said everything there is to say.
    pub key_files: Vec<(String, String)>,
    /// `(key, first line of the signature)`, most called first, ties on the
    /// key.
    pub key_symbols: Vec<(String, String)>,
}

/// Summarise the repository for the start of a session.
///
/// Deterministic for the same store state, with no `now` to pin: see the
/// module docs for why this one tool reads no clock at all.
#[must_use]
pub fn brief<F: Fs>(db: &GraphDb<F>, opts: &BriefOptions) -> BriefReport {
    let mut file_keys: Vec<String> = db
        .nodes_with_label("File")
        .iter()
        .map(|n| n.key().to_string())
        .collect();
    file_keys.sort();

    // `file_pagerank` returns the ranking already sorted the way every digest
    // prints one — score first, ties on the key — so the brief takes its head.
    // No budget: the caller is a hook with five seconds and one job.
    let (ranked, _truncated) = file_pagerank(db, &file_keys, None);
    let key_files = ranked
        .iter()
        .take(opts.max_files)
        .map(|(k, _)| (sanitize(k), role_of(db, k, &file_keys)))
        .collect();

    // Who calls whom, counted once: a symbol many others call is one a reader
    // will meet whichever thread they pull.
    let mut callers: BTreeMap<String, usize> = BTreeMap::new();
    for (_src, dst, _w) in db.weighted_edges("CALLS", None) {
        *callers.entry(dst).or_default() += 1;
    }
    let symbols = db.nodes_with_label("Symbol");
    let mut ranked_symbols: Vec<(String, usize, String)> = symbols
        .iter()
        .map(|n| {
            let key = n.key().to_string();
            let called = callers.get(&key).copied().unwrap_or(0);
            (key, called, first_line(n.prop("signature")))
        })
        .collect();
    ranked_symbols.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let key_symbols = ranked_symbols
        .into_iter()
        .take(opts.max_symbols)
        .map(|(k, _, sig)| (sanitize(&k), sig))
        .collect();

    BriefReport {
        repo: str_prop(db, SYNC_KEY, "repo")
            .map(|p| sanitize(basename(p.trim_end_matches('/'))))
            .unwrap_or_default(),
        files: file_keys.len(),
        symbols: symbols.len(),
        edges: usize::try_from(db.edge_count()).unwrap_or(usize::MAX),
        last_sync: str_prop(db, SYNC_KEY, "sha")
            .map(|s| sanitize(&s).chars().take(SHORT_SHA).collect()),
        key_files,
        key_symbols,
    }
}

/// The first line of a signature prop, or nothing for a node without one.
///
/// One line, because a multi-line signature would forge a section heading in
/// a digest that is read as lines — [`sanitize`] would flatten the break, but
/// the rest of the signature would still be spliced into someone else's line.
fn first_line(v: Option<Value>) -> String {
    match v {
        Some(Value::Str(s)) => sanitize(s.lines().next().unwrap_or_default().trim()),
        _ => String::new(),
    }
}

/// What a file is, in a few words.
///
/// Its `role` prop when the graph carries one. Otherwise what its directory is
/// made of: the subdirectory names most of its neighbours sit in, which is the
/// part of the directory's cluster name the path printed beside it does not
/// already show. A file in a leaf directory therefore has no role, and the
/// brief spends no bytes repeating its own path back at the reader.
fn role_of<F: Fs>(db: &GraphDb<F>, key: &str, file_keys: &[String]) -> String {
    if let Some(role) = str_prop(db, key, "role") {
        let role = sanitize(role.trim());
        if !role.is_empty() {
            return role;
        }
    }
    let dir = dir_components(key).join("/");
    if dir.is_empty() {
        return String::new(); // a file at the root is under nothing
    }
    let prefix = format!("{dir}/");
    let neighbours: Vec<String> = file_keys
        .iter()
        .filter(|k| k.starts_with(&prefix))
        .cloned()
        .collect();
    sanitize(&top_tokens(&neighbours, &dir, ROLE_TOKENS, true).join(", "))
}
