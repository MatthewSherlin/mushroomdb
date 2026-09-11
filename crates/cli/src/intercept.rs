//! `mushroomdb intercept` — the optional `PreToolUse` hook body (experimental).
//!
//! Claude Code runs this before a `Grep`, handing it the tool call as JSON on
//! stdin. When the pattern is a bare identifier the graph already holds as a
//! symbol, the search is a question `explore` answers exactly — the definition,
//! the callers and the callees, in one reply — where the grep returns every
//! line the name appears on and leaves the reading to the model. Exit 2 with a
//! message on stderr is Claude Code's way of saying so: the tool call is
//! blocked and the message reaches the model.
//!
//! # Why it is this conservative
//!
//! A hook that blocks a search wrongly is far worse than one that never fires,
//! so [`decide`] answers `Some` only where the graph provably has the better
//! answer:
//!
//! - The pattern must be a bare identifier ([`is_identifier`]). Anything with
//!   regex syntax in it — `.`, `*`, `|`, an anchor, a space — is a search, not
//!   a name, and the graph has no opinion about it.
//! - It must be at least three characters. `id` may well be a symbol, and a
//!   grep for it is still almost certainly not a request for that symbol.
//! - It must resolve to at least one `Symbol` node, through the same lookup
//!   `context` uses for a bare name, so the `explore` the message points at is
//!   one that will actually answer.
//!
//! Everything else — a store that will not open, a payload that will not parse,
//! a missing field — is silence and exit 0, like every other hook this binary
//! writes.

use crate::hook::open_for_hook;
use crate::structure::Db;
use core_api::repograph;
use std::path::Path;

/// Shortest pattern worth redirecting. Two-character names are common enough
/// as substrings that a grep for one is not evidence the symbol was meant.
const MIN_PATTERN_LEN: usize = 3;

/// Whether `pattern` is a bare identifier: `^[A-Za-z_][A-Za-z0-9_:]{2,}$`.
///
/// The `:` is there for the qualified names extractors produce (`Type::method`)
/// — still one name, still something the graph can be asked about — and it is
/// not a regex metacharacter, so a pattern containing it is no more likely to
/// be a search than a plain name is.
///
/// Shared with [`crate::enrich`], which asks the same question of the tokens in
/// a search result: the floor that makes a pattern worth a lookup is the floor
/// that makes a matched word worth one.
pub(crate) fn is_identifier(pattern: &str) -> bool {
    if pattern.len() < MIN_PATTERN_LEN {
        return false;
    }
    let mut chars = pattern.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
}

/// The payload's search pattern, when it is a name the graph could hold.
/// `None` for a missing or non-string `tool_input.pattern` and for anything
/// that is not a bare identifier — neither needs the store to be opened.
fn redirectable_pattern(input: &serde_json::Value) -> Option<&str> {
    input["tool_input"]["pattern"]
        .as_str()
        .filter(|p| is_identifier(p))
}

/// Whether this `Grep` should be redirected, and what to say if so.
///
/// `input` is Claude Code's `PreToolUse` payload; the pattern is
/// `tool_input.pattern`. `None` means "no opinion — run the grep".
#[must_use]
pub fn decide(db: &Db, input: &serde_json::Value) -> Option<String> {
    let pattern = redirectable_pattern(input)?;
    if repograph::named_symbols(db, pattern).is_empty() {
        return None;
    }
    Some(format!(
        "mushroomdb: '{pattern}' is a known symbol — call explore(\"{pattern}\") for its \
         definition, callers and callees instead of grepping the tree."
    ))
}

/// The whole hook body: parse the payload, open the store, decide.
///
/// `None` for every failure as well as for every pattern that passes through,
/// because the caller's only two options are "block with this message" and
/// "stay out of the way", and a store that is missing or busy is the second.
///
/// The pattern is tested before the store is opened. Opening one is the whole
/// cost of this hook — measured at ~360 ms on a small store, against ~0 for
/// the parse — and this runs before every single `Grep`, most of which are
/// searches no identifier test will ever accept.
#[must_use]
pub fn run_intercept(db_dir: &Path, payload: &str) -> Option<String> {
    let input: serde_json::Value = serde_json::from_str(payload).ok()?;
    redirectable_pattern(&input)?;
    let db = open_for_hook(db_dir)?;
    decide(&db, &input)
}

#[cfg(test)]
mod tests {
    use super::is_identifier;

    #[test]
    fn identifiers_are_names_and_nothing_else() {
        for ok in ["render_map", "RenderMap", "_private", "Type::method", "abc"] {
            assert!(is_identifier(ok), "{ok} is an identifier");
        }
        for no in [
            "ab",
            "",
            "1abc",
            "render_.*",
            "a|b",
            "^abc",
            "abc$",
            "two words",
            "path/to/file",
            "fn abc(",
        ] {
            assert!(!is_identifier(no), "{no} is not an identifier");
        }
    }
}
