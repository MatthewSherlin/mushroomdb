//! `mushroomdb enrich` — the optional `PostToolUse` hook body for `Grep`.
//!
//! Claude Code runs this after a `Grep` has returned, handing it the tool call
//! and its result as JSON on stdin. A grep answers "where does this name
//! appear"; the graph answers "what is it" — where it is defined, how many
//! places call it, who owns the file. This hook appends the second answer to
//! the first, so the names the search just surfaced arrive with their
//! definitions rather than as a list of line numbers.
//!
//! Unlike [`crate::intercept`], which replaces a search, this only ever adds to
//! one: adoption is by construction, since nothing has to be chosen.
//!
//! # What it looks at
//!
//! The pattern first — a grep for a bare name is usually a grep *for that
//! symbol* — and then the identifiers in the result text, which is where a
//! regex search's real subjects are. A token earns a line only if it resolves
//! to a `Symbol` the graph holds, through the same lookup `context` uses for a
//! bare name, so a token that is merely a word costs nothing but a map lookup.
//!
//! Everything else — a store that will not open, a payload that will not
//! parse, a pattern that names nothing — is silence, like every other hook
//! this binary writes.
//!
//! # Where the facts land
//!
//! The text goes out as `additionalContext` on a `hookSpecificOutput` object,
//! which puts it in the turn *beside* the tool result — not inside it. Claude
//! Code also documents `updatedToolOutput` for `PostToolUse`, which would
//! rewrite the result itself, but the reference's list of the tools that
//! support it could not be retrieved when this was written, and a key the host
//! ignores is a hook that silently does nothing. Anyone reading the grep-
//! enrichment arm's numbers should read them as "the facts arrived in the same
//! turn, adjacent to the matches", not "the matches came back annotated".
//!
//! # Cost
//!
//! One pass over the `Symbol` nodes builds the name index ([`name_index`]),
//! and every candidate is answered out of it. Asking the graph per candidate
//! instead would be up to [`MAX_CANDIDATES`] full scans of the symbol table on
//! every single `Grep`, which is the whole budget spent on names that mostly
//! turn out to be ordinary words.

use crate::hook::{cut_to, open_for_hook};
use crate::intercept::is_identifier;
use core_api::repograph::{context_with, sanitize, ContextOptions};
use core_api::Value;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

/// The most this hook may append to a tool result. Wider than the pre-edit
/// budget because a grep result is already long and the facts have to be
/// distinguishable from it, and still small enough that a search-heavy session
/// does not pay for it twice over.
pub const MAX_CONTEXT_BYTES: usize = 800;

/// Symbols reported. Past five the reader is scanning a second search result
/// rather than reading an answer.
const MAX_SYMBOLS: usize = 5;

/// Identifier-shaped tokens taken off a payload before the store is consulted.
/// A grep result can run to thousands of lines, and every candidate costs a
/// lookup; the ones that matter are near the top, because that is how a grep
/// orders its matches.
const MAX_CANDIDATES: usize = 40;

/// The text this payload has names in: the pattern, then whatever the tool
/// returned.
///
/// `tool_response` is the field Claude Code's hooks reference names for a
/// tool's result. Its shape is the tool's own, and `Grep`'s is not something
/// this hook should depend on, so every string found inside it is treated the
/// same way: as text that may contain identifiers. A payload with no usable
/// result leaves the pattern, which is on its own often the whole question.
fn candidate_text(input: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(p) = input["tool_input"]["pattern"].as_str() {
        out.push(p.to_string());
    }
    collect_strings(&input["tool_response"], &mut out);
    out
}

/// Every string leaf of `v`, in order, appended to `out`. Object *keys* are
/// deliberately not collected: they are the tool's vocabulary, not the
/// repository's.
fn collect_strings(v: &serde_json::Value, out: &mut Vec<String>) {
    match v {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Array(a) => a.iter().for_each(|x| collect_strings(x, out)),
        serde_json::Value::Object(o) => o.values().for_each(|x| collect_strings(x, out)),
        _ => {}
    }
}

/// Whether every `:` in `token` belongs to a `::` pair.
///
/// [`is_identifier`] allows a lone `:` because a search *pattern* is one token
/// by construction and a qualified name has its colons in pairs. A grep result
/// is not one token: its lines read `path:line:text`, and splitting them on
/// anything but `:` leaves tokens like `rs:1:fn` that pass the identifier test
/// and name nothing. Requiring the pairs keeps `Type::method` and drops those,
/// without splitting on `:` and losing qualified names altogether.
fn colons_are_paired(token: &str) -> bool {
    let b = token.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b':' {
            if b.get(i + 1) != Some(&b':') {
                return false;
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    true
}

/// The identifier-shaped tokens in `texts`, first occurrence first, without
/// repeats and capped at [`MAX_CANDIDATES`].
fn candidates(texts: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for text in texts {
        for token in text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == ':')) {
            // `is_identifier` also rejects a leading digit and anything under
            // three characters, which is the same floor the redirect uses:
            // a two-letter name is not evidence the symbol was meant.
            if !is_identifier(token) || !colons_are_paired(token) {
                continue;
            }
            if !out.iter().any(|t| t == token) {
                out.push(token.to_string());
            }
            if out.len() >= MAX_CANDIDATES {
                return out;
            }
        }
    }
    out
}

/// Every `Symbol` the store holds, by the bare `name` prop, from one scan.
///
/// The values are the keys sharing that name, sorted, which is exactly what
/// [`core_api::repograph::named_symbols`] returns for a single name — this is
/// that answer for every name at once, so a hook with dozens of candidates
/// pays for one pass rather than dozens.
fn name_index(db: &crate::structure::Db) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for node in db.nodes_with_label("Symbol") {
        if let Some(Value::Str(name)) = node.prop("name") {
            out.entry(name).or_default().push(sanitize(node.key()));
        }
    }
    for keys in out.values_mut() {
        keys.sort();
    }
    out
}

/// One symbol's line: where it is, how many files call it, who owns its file.
///
/// Named by its key rather than by the bare name that found it, because a key
/// is what every other command here takes as a target — the line doubles as
/// the argument for the `explore` a reader may want next.
fn describe(db: &crate::structure::Db, key: &str) -> String {
    let report = context_with(db, None, key, &ContextOptions { source: false });
    let mut out = String::new();
    let _ = write!(out, "{key} — ");
    match (report.file.is_empty(), report.lines) {
        (false, Some((line, _))) => {
            let _ = write!(out, "defined at {}:{line}", report.file);
        }
        (false, None) => {
            let _ = write!(out, "defined in {}", report.file);
        }
        (true, _) => out.push_str("defined somewhere the graph did not record"),
    }
    // Caller *files*, not call sites: "twelve places in one file" and "twelve
    // files" are different facts, and the one that predicts a change's reach
    // is the count of files.
    let callers = report.callers.len() + report.callers_not_shown;
    let _ = write!(out, ", {callers} callers");
    if let Some(owner) = &report.owner {
        let _ = write!(out, ", owner {owner}");
    }
    out
}

/// The whole hook body: parse the payload, open the store, describe whatever
/// the search named that the graph holds.
///
/// `None` for every failure and for every search that named nothing, because
/// the caller's only two options are "append this" and "stay out of the way".
#[must_use]
pub fn run(db_dir: &Path, payload: &str) -> Option<String> {
    let input: serde_json::Value = serde_json::from_str(payload).ok()?;
    let tokens = candidates(&candidate_text(&input));
    if tokens.is_empty() {
        return None;
    }
    let db = open_for_hook(db_dir)?;

    let by_name = name_index(&db);
    let mut keys: Vec<&str> = Vec::new();
    for token in &tokens {
        // A name several symbols share is ambiguous, and picking one of them
        // would be inventing an answer; a token earns a line only where it
        // resolves to exactly one symbol.
        if let Some([key]) = by_name.get(token).map(Vec::as_slice) {
            keys.push(key);
        }
        if keys.len() >= MAX_SYMBOLS {
            break;
        }
    }
    if keys.is_empty() {
        return None;
    }

    let mut out = String::from("about the symbols grep found: ");
    for (i, key) in keys.iter().enumerate() {
        let line = describe(&db, key);
        // The budget is a hard cap, and a half-written symbol is worse than
        // one fewer: the line is only added if the whole of it fits.
        let sep = if i > 0 { "; " } else { "" };
        if out.len() + sep.len() + line.len() > MAX_CONTEXT_BYTES {
            break;
        }
        out.push_str(sep);
        out.push_str(&line);
    }
    // Not one symbol fit, which takes a pathological key to manage.
    if out.ends_with(": ") {
        return None;
    }
    // The per-line check above already holds the budget; `cut_to` is the
    // backstop that makes the cap true by construction rather than by
    // argument.
    Some(cut_to(out, MAX_CONTEXT_BYTES))
}

#[cfg(test)]
mod tests {
    use super::{candidate_text, candidates, colons_are_paired};

    #[test]
    fn qualified_names_survive_but_grep_line_prefixes_do_not() {
        assert!(colons_are_paired("Type::method"));
        assert!(colons_are_paired("render_map"));
        assert!(!colons_are_paired("rs:1:fn"));
        assert!(!colons_are_paired("a:::b"));
    }

    #[test]
    fn the_pattern_comes_first_and_repeats_are_dropped() {
        let payload = serde_json::json!({
            "tool_input": {"pattern": "render_map"},
            "tool_response": {"content": "a.rs:1:fn render_map()\nb.rs:2:render_map();"},
        });
        let found = candidates(&candidate_text(&payload));
        assert_eq!(found.first().map(String::as_str), Some("render_map"));
        assert_eq!(
            found.iter().filter(|t| *t == "render_map").count(),
            1,
            "{found:?}"
        );
        assert!(
            !found.iter().any(|t| t.contains(':') && !t.contains("::")),
            "a `path:line:text` fragment is not a name: {found:?}"
        );
        assert!(!found.iter().any(|t| t == "1"), "{found:?}");
    }

    #[test]
    fn a_payload_with_no_result_still_offers_the_pattern() {
        let payload = serde_json::json!({"tool_input": {"pattern": "render_map"}});
        assert_eq!(candidates(&candidate_text(&payload)), vec!["render_map"]);
        assert!(candidates(&candidate_text(&serde_json::json!({}))).is_empty());
    }
}
