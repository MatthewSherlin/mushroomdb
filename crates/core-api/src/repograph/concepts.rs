//! Concept provenance: which learned concepts have drifted from their
//! sources.
//!
//! A `Concept` node records the `File` keys it was learned from
//! (`source_files`) alongside the hash each carried at the time
//! (`source_hashes`). Once a source file changes, the concept can no longer
//! be trusted to describe what is there — this is the one place that
//! decision is made, so [`map`](super::map) and `remember`'s callers agree.

use crate::db::GraphDb;
use crate::repograph::facts::{str_list, str_prop};
use core_storage::fs::Fs;

/// Concepts whose recorded source hashes no longer match the files they were
/// learned from, as `(concept key, reason)`, sorted by key.
///
/// Three things count as changed, because in all three the concept can no
/// longer be trusted to describe what is there:
///
/// - a `source_files` entry whose `File` now hashes to something else — the
///   reason names that file;
/// - a `source_files` entry with no `File` behind it at all, deleted or
///   never written — the reason names it too, since a missing hash is a
///   mismatch like any other;
/// - lists of unequal length, where a source has no hash to check it against
///   or a hash has no source — nothing pairs them, so nothing vouches for
///   them, and the reason says so.
#[must_use]
pub fn stale_concepts<F: Fs>(db: &GraphDb<F>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = db
        .nodes_with_label("Concept")
        .iter()
        .filter_map(|c| stale_reason(db, c).map(|reason| (c.key().to_string(), reason)))
        .collect();
    out.sort();
    out
}

/// Why one concept is stale, or `None` when its sources still check out.
fn stale_reason<F: Fs>(db: &GraphDb<F>, c: &crate::db::NodeRef<'_, F>) -> Option<String> {
    let files = str_list(c.prop("source_files"));
    let hashes = str_list(c.prop("source_hashes"));
    if files.len() != hashes.len() {
        return Some("source_files and source_hashes disagree in length".to_string());
    }
    files
        .iter()
        .zip(hashes.iter())
        .find(|(file, hash)| str_prop(db, file, "hash").as_ref() != Some(*hash))
        .map(|(file, _)| file.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::GraphDb;
    use core_storage::fs::RealFs;
    use core_storage::Value;

    fn tmp(name: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let d = std::env::temp_dir().join(format!(
            "graphdb-repograph-concepts-{name}-{}-{nanos}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn s(v: &str) -> Value {
        Value::Str(v.to_string())
    }

    #[test]
    fn a_concept_with_no_sources_is_never_stale() {
        let dir = tmp("no-sources");
        let mut db: GraphDb<RealFs> = GraphDb::open(&dir).expect("open");
        db.insert_node(
            "Concept",
            "concept:empty",
            vec![("id".into(), s("concept:empty")), ("name".into(), s("x"))],
        )
        .expect("concept");
        assert_eq!(stale_concepts(&db), Vec::new());
    }
}
