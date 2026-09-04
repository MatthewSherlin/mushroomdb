//! Reading the code graph back as Rust values.
//!
//! One place for the prop reads and the little conversions every tool in this
//! module needs: a string prop, a list prop, the name behind an author key, the
//! line an import or a call was recorded on. Each returns an empty or `None`
//! answer for a node that is missing or shaped differently, because a digest
//! reports what the graph has rather than failing on what it lacks.

use crate::db::GraphDb;
use crate::Direction;
use core_storage::fs::Fs;
use core_storage::Value;

/// A `String` prop, or `None` when the node, the prop or its type is missing.
pub(super) fn str_prop<F: Fs>(db: &GraphDb<F>, key: &str, field: &str) -> Option<String> {
    match db.node_ref(key).and_then(|n| n.prop(field)) {
        Some(Value::Str(s)) => Some(s),
        _ => None,
    }
}

/// An `Int` prop, or `None` when the node, the prop or its type is missing.
pub(super) fn int_prop<F: Fs>(db: &GraphDb<F>, key: &str, field: &str) -> Option<i64> {
    match db.node_ref(key).and_then(|n| n.prop(field)) {
        Some(Value::Int(i)) => Some(i),
        _ => None,
    }
}

/// The string elements of a list property, in order.
pub(super) fn str_list(v: Option<Value>) -> Vec<String> {
    match v {
        Some(Value::List(items)) => items
            .into_iter()
            .filter_map(|i| match i {
                Value::Str(s) => Some(s),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The string elements of a node's list property, in stored order.
pub(super) fn list_prop<F: Fs>(db: &GraphDb<F>, key: &str, field: &str) -> Vec<String> {
    str_list(db.node_ref(key).and_then(|n| n.prop(field)))
}

/// The label of a node, or `None` when the store has no such key.
pub(super) fn label_of<F: Fs>(db: &GraphDb<F>, key: &str) -> Option<String> {
    db.node_ref(key).map(|n| n.label().to_string())
}

/// Sort `(key, value)` pairs the way every digest prints them: biggest first,
/// ties broken on the key so the order never wobbles.
pub(super) fn rank<T: PartialOrd + Copy>(items: &mut [(String, T)]) {
    items.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
}

/// Neighbours of `key` along one edge type, sorted. An unknown key or an
/// unknown edge type has none — a question about a node the store never heard
/// of is answered, not refused.
pub(super) fn neighbors<F: Fs>(
    db: &GraphDb<F>,
    key: &str,
    edge_type: &str,
    dir: Direction,
) -> Vec<String> {
    let mut out = db.neighbors(key, edge_type, dir).unwrap_or_default();
    out.sort();
    out.dedup();
    out
}

/// Neighbours along one edge type in either direction, sorted and deduped.
/// A rule may write a symmetric association as one edge or as two; a reader
/// asking "what is this file associated with" wants the same answer either way.
pub(super) fn neighbors_both<F: Fs>(db: &GraphDb<F>, key: &str, edge_type: &str) -> Vec<String> {
    let mut out = neighbors(db, key, edge_type, Direction::Out);
    out.extend(neighbors(db, key, edge_type, Direction::In));
    out.sort();
    out.dedup();
    out
}

/// A rule-written edge weight, from whichever direction carries it.
pub(super) fn score_of<F: Fs>(db: &GraphDb<F>, edge_type: &str, a: &str, b: &str) -> Option<f64> {
    let read = |src: &str, dst: &str| match db.get_edge_prop(edge_type, src, dst, "score") {
        Some(Value::Float(f)) => Some(f),
        Some(Value::Int(i)) => Some(i as f64),
        _ => None,
    };
    read(a, b).or_else(|| read(b, a))
}

/// The author's `name`, falling back to the key for a store that has none.
/// Tools print names; the key is only ever a last resort.
pub(super) fn author_name<F: Fs>(db: &GraphDb<F>, key: &str) -> String {
    str_prop(db, key, "name").unwrap_or_else(|| key.to_string())
}

/// The author key `TOP_AUTHOR` points at, if any.
pub(super) fn owner_key<F: Fs>(db: &GraphDb<F>, file: &str) -> Option<String> {
    neighbors(db, file, "TOP_AUTHOR", Direction::Out)
        .into_iter()
        .next()
}

/// The name of the file's top author.
pub(super) fn owner_name<F: Fs>(db: &GraphDb<F>, file: &str) -> Option<String> {
    owner_key(db, file).map(|k| author_name(db, &k))
}

/// The file a symbol is defined in: its `file_id`, or its `path` on a node
/// written without one.
pub(super) fn symbol_file<F: Fs>(db: &GraphDb<F>, symbol: &str) -> Option<String> {
    str_prop(db, symbol, "file_id").or_else(|| str_prop(db, symbol, "path"))
}

/// The line a `"<key>\t<line>"` evidence list records for `target`.
///
/// `File.import_lines` and `Symbol.call_lines` are written alongside the lists
/// the rules match on, one entry per edge. A malformed entry — no tab, or a
/// line that is not a number — is skipped rather than guessed at.
pub(super) fn evidence_line(entries: &[String], target: &str) -> Option<u32> {
    entries.iter().find_map(|e| {
        let (key, line) = e.split_once('\t')?;
        (key == target).then(|| line.parse().ok())?
    })
}

/// One commit, as every digest quotes it.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct CommitFact {
    /// The full sha, as the graph keys it.
    pub sha: String,
    pub ts: i64,
    /// The first line of the commit message.
    pub subject: String,
}

/// Read a commit. `None` when the key is not a commit or carries no `ts` —
/// without a timestamp it cannot be placed in any ordering.
pub(super) fn commit_fact<F: Fs>(db: &GraphDb<F>, sha: &str) -> Option<CommitFact> {
    let ts = int_prop(db, sha, "ts")?;
    let message = str_prop(db, sha, "message").unwrap_or_default();
    Some(CommitFact {
        sha: sha.to_string(),
        ts,
        subject: message.lines().next().unwrap_or_default().to_string(),
    })
}

/// Every commit that touched `file`, newest first, ties broken on the sha.
///
/// Read from `File.commits`, which `ingest-git` writes and caps; the `TOUCHED`
/// edges say the same thing, and this way one node read answers the question.
pub(super) fn commits_of<F: Fs>(db: &GraphDb<F>, file: &str) -> Vec<CommitFact> {
    let mut out: Vec<CommitFact> = list_prop(db, file, "commits")
        .iter()
        .filter_map(|sha| commit_fact(db, sha))
        .collect();
    out.sort_by(|a, b| b.ts.cmp(&a.ts).then(a.sha.cmp(&b.sha)));
    out.dedup_by(|a, b| a.sha == b.sha);
    out
}

/// The newest `Commit.ts` in the store — the store's own "now", which is what
/// a window measured against the data rather than the wall clock ends at.
/// `None` on a store with no dated commit.
pub(super) fn newest_commit_ts<F: Fs>(db: &GraphDb<F>) -> Option<i64> {
    db.nodes_with_label("Commit")
        .iter()
        .filter_map(|n| match n.prop("ts") {
            Some(Value::Int(ts)) => Some(ts),
            _ => None,
        })
        .max()
}

/// The per-author commit distribution `ingest-git` records on a file, as
/// `(author key, commits)`. Entries that are not `key<TAB>count` are skipped.
pub(super) fn author_counts<F: Fs>(db: &GraphDb<F>, file: &str) -> Vec<(String, usize)> {
    list_prop(db, file, "author_counts")
        .iter()
        .filter_map(|e| {
            let (key, n) = e.rsplit_once('\t')?;
            let n = n.parse().ok()?;
            (!key.is_empty()).then(|| (key.to_string(), n))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_evidence_entry_yields_its_line_and_nothing_else() {
        let entries = vec![
            "src/a.rs\t12".to_string(),
            "src/b.rs\tnot-a-line".to_string(),
            "src/c.rs".to_string(),
        ];
        assert_eq!(evidence_line(&entries, "src/a.rs"), Some(12));
        assert_eq!(evidence_line(&entries, "src/b.rs"), None);
        assert_eq!(evidence_line(&entries, "src/c.rs"), None);
        assert_eq!(evidence_line(&entries, "src/d.rs"), None);
        assert_eq!(evidence_line(&[], "src/a.rs"), None);
    }

    #[test]
    fn rank_puts_the_biggest_first_and_breaks_ties_on_the_key() {
        let mut items = vec![
            ("b".to_string(), 1.0),
            ("a".to_string(), 1.0),
            ("c".to_string(), 2.0),
        ];
        rank(&mut items);
        let keys: Vec<&str> = items.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["c", "a", "b"]);
    }
}
