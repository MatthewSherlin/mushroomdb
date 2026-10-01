//! Identity: the alias set `SAME_AS` compares.
//!
//! `SAME_AS` is a rule, and a rule can only compare what a node holds. The
//! engine's `Overlap` predicate is an exact Jaccard over list tokens — no case
//! folding, no tokenising — so `"Matt"` and `"matt"` never meet unless both
//! sides were written the same way. This module is the one place that writes
//! them the same way: every entity `remember` or `upsert_entity` touches gets an
//! `aliases` list built here, from its key, its `name` and whatever aliases the
//! caller supplied, normalised identically on every node.
//!
//! Normalisation is idempotent and deterministic: the list is sorted and
//! deduplicated, and normalising an already-normalised alias returns it
//! unchanged, so a node rewritten with the same inputs is not rewritten at all.

use crate::GraphDb;
use core_storage::fs::Fs;
use core_storage::{GraphError, Result, Value};
use std::collections::BTreeSet;

/// The property every entity's alias list lives in.
pub const ALIASES_FIELD: &str = "aliases";

/// Most aliases one node may carry.
///
/// A real entity carries its key, its full name, that name's two to four
/// words and a few nicknames — about ten. Thirty-two is three times that, and
/// it keeps one node from becoming a bag of tokens that is a rule candidate
/// for half the store on every write.
pub const MAX_ALIASES: usize = 32;

/// `s` lowercased, with every run of characters that are not letters or digits
/// collapsed to one space and the ends trimmed.
///
/// `"Matthew  Sherlin"`, `"matthew_sherlin"` and `"MATTHEW-SHERLIN"` all
/// become `"matthew sherlin"`. Letters are Unicode letters: `"José Ñúñez"`
/// becomes `"josé ñúñez"`.
#[must_use]
pub fn canonical(s: &str) -> String {
    tokens(s).join(" ")
}

/// The words of `s`: maximal runs of letters and digits, lowercased.
fn tokens(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// The aliases a node's own fields imply, plus the caller's.
///
/// - the key, lowercased — keys are identifiers, so they are not tokenised;
/// - the name in [`canonical`] form, and each of its words when it has more
///   than one;
/// - each caller alias in [`canonical`] form.
///
/// Sorted and deduplicated. Empty strings never appear.
#[must_use]
pub fn derive_aliases(key: &str, name: Option<&str>, caller: &[String]) -> Vec<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    let key = key.to_lowercase();
    if !key.trim().is_empty() {
        out.insert(key);
    }
    if let Some(name) = name {
        let words = tokens(name);
        if !words.is_empty() {
            out.insert(words.join(" "));
        }
        if words.len() > 1 {
            out.extend(words);
        }
    }
    for alias in caller {
        let c = canonical(alias);
        if !c.is_empty() {
            out.insert(c);
        }
    }
    out.into_iter().collect()
}

/// `existing` (what the node already holds) united with `derived`.
///
/// Aliases accumulate: a node renamed keeps its former name as an alias, which
/// is what an identity rule should compare. `forget` with `prop: "aliases"`
/// clears them. Refused, naming the node, when the union would exceed
/// [`MAX_ALIASES`].
pub fn merge_aliases(
    key: &str,
    existing: Option<&Value>,
    derived: &[String],
) -> Result<Vec<String>> {
    let mut out: BTreeSet<String> = derived.iter().cloned().collect();
    if let Some(Value::List(items)) = existing {
        for item in items {
            if let Value::Str(s) = item {
                out.insert(s.clone());
            }
        }
    }
    if out.len() > MAX_ALIASES {
        return Err(GraphError::IngestError {
            detail: format!(
                "'{key}' would carry {} aliases, more than {MAX_ALIASES}; forget its \
                 'aliases' property first, or pass fewer",
                out.len()
            ),
        });
    }
    Ok(out.into_iter().collect())
}

/// The list as a stored value.
#[must_use]
pub fn aliases_value(aliases: &[String]) -> Value {
    Value::List(aliases.iter().cloned().map(Value::Str).collect())
}

/// The alias list `key` should carry after a write that sets `props` on it,
/// or `None` when that is exactly what it already carries.
///
/// The name is the one `props` sets, else the one the node already has. Reads
/// the store, so call it before a batch takes `db` mutably.
pub fn aliases_after_write<F: Fs>(
    db: &GraphDb<F>,
    key: &str,
    props: &[(String, Value)],
    caller: &[String],
) -> Result<Option<Vec<String>>> {
    if props.iter().any(|(f, _)| f == ALIASES_FIELD) {
        return Err(GraphError::IngestError {
            detail: format!(
                "'{ALIASES_FIELD}' is maintained by the store; pass aliases as the \
                 'aliases' argument instead of a property"
            ),
        });
    }
    let name = props
        .iter()
        .find(|(f, _)| f == crate::memory_schema::NAME_FIELD)
        .map(|(_, v)| v.clone())
        .or_else(|| db.get_prop(key, crate::memory_schema::NAME_FIELD));
    let name = match &name {
        Some(Value::Str(s)) => Some(s.as_str()),
        _ => None,
    };
    let existing = db.get_prop(key, ALIASES_FIELD);
    let merged = merge_aliases(key, existing.as_ref(), &derive_aliases(key, name, caller))?;
    if existing.as_ref() == Some(&aliases_value(&merged)) {
        return Ok(None);
    }
    Ok(Some(merged))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_is_idempotent() {
        for s in [
            "Matthew  Sherlin",
            "matthew_sherlin",
            "JOSÉ-Ñúñez",
            "",
            "!!",
            "a1 B2",
        ] {
            assert_eq!(canonical(&canonical(s)), canonical(s), "{s:?}");
        }
    }

    #[test]
    fn derive_is_idempotent_over_its_own_output() {
        let once = derive_aliases("matthew-sherlin", Some("Matthew Sherlin"), &["Matt".into()]);
        let twice = derive_aliases("matthew-sherlin", Some("Matthew Sherlin"), &once);
        assert_eq!(once, twice);
    }
}
