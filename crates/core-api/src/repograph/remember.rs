//! `remember` — write a `Note` the graph can later `recall`.
//!
//! The other half of [`recall`](super::recall): where that module reads,
//! this one writes the one label an assistant is expected to create itself.
//! A note's `about` list is exactly the field the `about_<label>` rules (see
//! [`super::rules`], shared with `structure::ensure_rules_and_fulltext` in
//! the CLI crate) match on, so writing it derives the `ABOUT` edges in the
//! same commit — nothing here inserts an edge directly.
//!
//! `structure::ensure_rules_and_fulltext` only ever runs from `ingest-git`
//! and `sync`, and even then only declares an `about_<label>` rule once a
//! node of that label already exists. A store's very first `remember` call —
//! or one whose `about` names a `Note` or `Concept` created since the last
//! sync — would otherwise pass validation and silently write no `ABOUT`
//! edge at all until the next sync backfills it. [`remember`] closes that
//! gap itself: before writing, it ensures the specific `about_<label>`
//! rule(s) its own `about` keys need already exist, creating whichever are
//! missing from the same shared definitions `structure` uses.

use crate::db::GraphDb;
use crate::repograph::facts::label_of;
use crate::repograph::rules::{about_rule, ABOUT_LABELS};
use core_storage::fs::Fs;
use core_storage::{GraphError, Result, Value};
use std::collections::BTreeSet;

/// Text length bounds, in characters, after trimming.
const MIN_TEXT_CHARS: usize = 1;
const MAX_TEXT_CHARS: usize = 4000;

/// The `kind` values a `Note` may carry.
pub const NOTE_KINDS: [&str; 3] = ["note", "decision", "todo"];

/// What to remember.
pub struct RememberInput<'a> {
    /// The note's text, trimmed to [`MIN_TEXT_CHARS`]..=[`MAX_TEXT_CHARS`]
    /// characters.
    pub text: &'a str,
    /// Keys the note is about. Every one must already exist; an
    /// `about_<label>` rule turns each into an `ABOUT` edge.
    pub about: &'a [String],
    /// One of [`NOTE_KINDS`].
    pub kind: &'a str,
    /// Unix seconds the note was written at. Part of the note's key, so
    /// remembering the same text again at the same `ts` is a no-op rather
    /// than a duplicate.
    pub ts: i64,
}

/// Write `input` as a `Note`, returning its key.
///
/// Validated before anything is written: `text` must be
/// [`MIN_TEXT_CHARS`]..=[`MAX_TEXT_CHARS`] characters after trimming, `kind`
/// must be one of [`NOTE_KINDS`], and every `about` key must already exist —
/// [`GraphError::KeyNotFound`] names the first missing one, sorted, so a
/// caller with several bad keys is told about the same one twice rather than
/// a different one each retry.
///
/// The key is `"note:"` followed by 16 hex characters of a stable 64-bit
/// hash of `ts` and `text` (see [`note_key`] for why it is not `blake3`),
/// so remembering the same text at the same `ts` again returns the same key
/// without writing a second node — the caller's insertion order into
/// `about` does not affect the key, but does affect which edges backfill
/// first, which the engine already makes deterministic.
///
/// Also ensures full-text search is enabled on `Note.text`, and that the
/// `about_<label>` rule for every label named among `about` already exists
/// (see the module docs), so a store whose very first write is a `remember`
/// call — never having gone through `structure::ensure_rules_and_fulltext` —
/// can still be recalled from and still derives its `ABOUT` edges.
pub fn remember<F: Fs>(w: &mut GraphDb<F>, input: &RememberInput<'_>) -> Result<String> {
    let text = input.text.trim();
    let len = text.chars().count();
    if !(MIN_TEXT_CHARS..=MAX_TEXT_CHARS).contains(&len) {
        return Err(GraphError::IngestError {
            detail: format!(
                "remember: text must be {MIN_TEXT_CHARS}..={MAX_TEXT_CHARS} characters \
                 after trimming, got {len}"
            ),
        });
    }
    if !NOTE_KINDS.contains(&input.kind) {
        return Err(GraphError::IngestError {
            detail: format!(
                "remember: kind must be one of {}, got {:?}",
                NOTE_KINDS.join(", "),
                input.kind
            ),
        });
    }
    let mut about: Vec<String> = input.about.to_vec();
    about.sort();
    about.dedup();
    if let Some(missing) = about.iter().find(|key| !w.has_node(key)) {
        return Err(GraphError::KeyNotFound {
            key: missing.clone(),
        });
    }

    if !w
        .fulltext_pairs()
        .contains(&("Note".to_string(), "text".to_string()))
    {
        w.enable_fulltext("Note", "text")?;
    }
    ensure_about_rules(w, &about)?;

    let key = note_key(input.ts, text);
    if !w.has_node(&key) {
        let mut props: Vec<(String, Value)> = vec![
            ("id".into(), Value::Str(key.clone())),
            ("text".into(), Value::Str(text.to_string())),
            ("kind".into(), Value::Str(input.kind.to_string())),
            ("ts".into(), Value::Int(input.ts)),
            ("source".into(), Value::Str("agent".to_string())),
        ];
        if !about.is_empty() {
            props.push((
                "about".into(),
                Value::List(about.into_iter().map(Value::Str).collect()),
            ));
        }
        w.insert_node("Note", &key, props)?;
    }
    Ok(key)
}

/// Create whichever `about_<label>` rules `about_keys` need and do not exist
/// yet, from the same definitions [`about_rule`] gives `structure` — so a
/// note written before the label's rule was ever backfilled by a sync still
/// derives its `ABOUT` edge in this commit.
///
/// A label outside [`ABOUT_LABELS`] has no rule to create — that key's
/// `ABOUT` edge simply does not derive, same as it never has; `remember`
/// only guarantees the edge for the labels the plan enumerates. Idempotent:
/// existing rule names are read once, so a label whose rule already exists
/// costs nothing and a repeat call creates nothing new.
fn ensure_about_rules<F: Fs>(w: &mut GraphDb<F>, about_keys: &[String]) -> Result<()> {
    let mut labels: BTreeSet<String> = about_keys
        .iter()
        .filter_map(|key| label_of(w, key))
        .filter(|label| ABOUT_LABELS.contains(&label.as_str()))
        .collect();
    if labels.is_empty() {
        return Ok(());
    }
    let existing: BTreeSet<String> = w.rules().into_iter().map(|r| r.name).collect();
    labels.retain(|label| {
        let name = format!("about_{}", label.to_lowercase());
        !existing.contains(&name)
    });
    for label in labels {
        w.create_rule(about_rule(&label))?;
    }
    Ok(())
}

/// The key one `remember` call writes to: `"note:"` followed by 16 hex
/// characters of a 64-bit FNV-1a hash of `ts` and `text`.
///
/// The plan calls for a `blake3`-derived key, but `blake3` is a dependency
/// this crate may not take — the workspace's dependency ruling confines it
/// to `crates/code-extract`, which `core-api` cannot depend on either
/// (`remember` lives here so a `WriteGuard` can call it directly). FNV-1a is
/// already how this codebase derives a stable content hash without a
/// dependency (see the test fixture's own `hash_of`), and a single 64-bit
/// hash formats to exactly 16 hex characters, so the key keeps the shape the
/// plan describes — 16 hex characters, content-derived, deterministic — with
/// a different, dependency-free hash underneath it.
fn note_key(ts: i64, text: &str) -> String {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = FNV_OFFSET;
    for b in ts.to_string().bytes().chain(text.bytes()) {
        h ^= u64::from(b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    format!("note:{h:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_is_a_function_of_ts_and_text_alone() {
        assert_eq!(note_key(1, "a"), note_key(1, "a"));
        assert_ne!(note_key(1, "a"), note_key(2, "a"));
        assert_ne!(note_key(1, "a"), note_key(1, "b"));
        assert!(note_key(1, "a").strip_prefix("note:").unwrap().len() == 16);
    }
}
