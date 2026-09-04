//! Markdown: headings, mentions, body text, and mention resolution.
//!
//! A prose document contributes two things to the graph: the headings that
//! say what it is about, and the mentions that say what it is about *in the
//! tree* — a file named in backticks or linked to. Both are recognised
//! without a parser, because Markdown mentions are a lexical matter and a
//! grammar would only add ways to disagree with the reader.

use crate::{file_name, join, normalize, parent_dir, truncate_bytes, truncate_chars};
use crate::{FileFacts, MAX_BODY_BYTES, MAX_TEXT_CHARS};

/// Characters allowed inside a backticked path mention.
fn is_token_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '/' | '-')
}

/// Fill in the Markdown half of `facts`.
pub(crate) fn extract(facts: &mut FileFacts, text: &str) {
    let mut headings = Vec::new();
    let mut mentions = Vec::new();
    let mut fence: Option<char> = None;

    for line in text.lines() {
        let trimmed = line.trim_start();
        if let Some(marker) = fence_marker(trimmed) {
            match fence {
                Some(open) if open == marker => fence = None,
                Some(_) => {}
                None => fence = Some(marker),
            }
            continue;
        }
        if fence.is_some() {
            continue;
        }
        if let Some(title) = heading_of(trimmed) {
            headings.push(truncate_chars(title, MAX_TEXT_CHARS));
        }
        collect_code_mentions(line, &mut mentions);
        collect_link_mentions(line, &mut mentions);
    }

    mentions.sort();
    mentions.dedup();
    facts.headings = headings;
    facts.mentions = mentions;
    facts.body = Some(truncate_bytes(text, MAX_BODY_BYTES));
}

/// The fence character when `line` opens or closes a fenced code block.
fn fence_marker(line: &str) -> Option<char> {
    ['`', '~']
        .into_iter()
        .find(|marker| line.starts_with(&marker.to_string().repeat(3)))
}

/// The text of an ATX heading, if `line` is one.
fn heading_of(line: &str) -> Option<&str> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = line.get(hashes..)?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let title = rest.trim().trim_end_matches('#').trim();
    (!title.is_empty()).then_some(title)
}

/// Backticked tokens that look like a path: at least one dot, followed by a
/// short extension. `` `src/loader.ts` `` is a mention; `` `let x = 1` `` is
/// not.
///
/// A span opens on a run of backticks and closes on the next run of the same
/// length, which is how Markdown lets a span contain a literal backtick.
/// Splitting on single backticks instead would let one such span flip the
/// parity of the rest of the line and hide every mention after it.
fn collect_code_mentions(line: &str, out: &mut Vec<String>) {
    let bytes = line.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        let open = backtick_run(bytes, at);
        if open == 0 {
            at += 1;
            continue;
        }
        let start = at + open;
        let Some(close) = closing_run(bytes, start, open) else {
            break;
        };
        if let Some(span) = line.get(start..close) {
            let span = span.trim();
            if looks_like_path(span) {
                out.push(span.to_string());
            }
        }
        at = close + open;
    }
}

/// Length of the run of backticks starting at `at`, zero if there is none.
fn backtick_run(bytes: &[u8], at: usize) -> usize {
    bytes[at..].iter().take_while(|b| **b == b'`').count()
}

/// Offset of the next run of exactly `width` backticks at or after `from`.
fn closing_run(bytes: &[u8], from: usize, width: usize) -> Option<usize> {
    let mut at = from;
    while at < bytes.len() {
        let run = backtick_run(bytes, at);
        if run == width {
            return Some(at);
        }
        at += run.max(1);
    }
    None
}

fn looks_like_path(span: &str) -> bool {
    if span.is_empty() || !span.chars().all(is_token_char) {
        return false;
    }
    match span.rsplit_once('.') {
        Some((stem, ext)) => {
            !stem.is_empty()
                && (1..=8).contains(&ext.len())
                && ext.chars().all(|c| c.is_ascii_alphanumeric())
        }
        None => false,
    }
}

/// Link targets: the `path` in `[text](path)`. Fragments, query strings, link
/// titles and absolute URLs are dropped.
fn collect_link_mentions(line: &str, out: &mut Vec<String>) {
    let bytes = line.as_bytes();
    let mut at = 0;
    while let Some(found) = line.get(at..).and_then(|rest| rest.find("](")) {
        let open = at + found + 2;
        let Some(close) = line.get(open..).and_then(|rest| rest.find(')')) else {
            break;
        };
        let end = open + close;
        if let Some(target) = line.get(open..end) {
            if let Some(target) = link_target(target) {
                out.push(target);
            }
        }
        at = (end + 1).min(bytes.len());
        if at >= bytes.len() {
            break;
        }
    }
}

fn link_target(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let raw = raw.strip_prefix('<').unwrap_or(raw);
    let raw = raw.strip_suffix('>').unwrap_or(raw);
    // Drop a link title: `path "Some title"`.
    let raw = raw.split_whitespace().next()?;
    // Drop a fragment or query.
    let raw = raw.split(['#', '?']).next()?;
    if raw.is_empty() || raw.contains("://") || raw.starts_with("mailto:") {
        return None;
    }
    Some(raw.to_string())
}

/// Resolve a Markdown mention to a working-tree path.
///
/// Tokens are read the way a reader would: a relative token (`./x`, `../x`)
/// means "next to this document", a bare token is tried first as a path from
/// the working-tree root and then relative to the document's own directory.
/// When neither matches, the token's file name is looked up across the tree
/// and accepted only when it is unique — an ambiguous name resolves to
/// `None`, because a wrong edge is worse than a missing one.
#[must_use]
pub fn resolve_mention(
    from_path: &str,
    token: &str,
    known: &dyn Fn(&str) -> bool,
    by_basename: &dyn Fn(&str) -> Vec<String>,
) -> Option<String> {
    let token = token.trim();
    if token.is_empty() || token.contains("://") || token.starts_with('#') {
        return None;
    }
    let from = normalize(from_path);
    let dir = parent_dir(&from);
    let relative = token.starts_with("./") || token.starts_with("../");

    let mut candidates = Vec::new();
    if relative {
        candidates.push(join(dir, token));
    } else {
        candidates.push(normalize(token));
        candidates.push(join(dir, token));
    }
    for candidate in &candidates {
        if !candidate.is_empty() && candidate != &from && known(candidate) {
            return Some(candidate.clone());
        }
    }

    let normalized = normalize(token);
    let base = file_name(&normalized);
    if base.is_empty() {
        return None;
    }
    let mut hits = by_basename(base);
    hits.sort();
    hits.dedup();
    hits.retain(|hit| hit != &from);
    match hits.len() {
        1 => hits.into_iter().next(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headings_need_a_space_after_the_hashes() {
        assert_eq!(heading_of("# Guide"), Some("Guide"));
        assert_eq!(heading_of("### Deep ###"), Some("Deep"));
        assert_eq!(heading_of("#NoSpace"), None);
        assert_eq!(heading_of("####### Too deep"), None);
    }

    #[test]
    fn only_path_shaped_backtick_spans_are_mentions() {
        assert!(looks_like_path("src/a.rs"));
        assert!(looks_like_path("a.md"));
        assert!(!looks_like_path("let x = 1"));
        assert!(!looks_like_path("plain"));
        assert!(!looks_like_path("a.averylongextension"));
    }

    #[test]
    fn a_double_backtick_span_does_not_hide_later_mentions() {
        let mut out = Vec::new();
        collect_code_mentions("a ``holds a ` backtick`` then `src/a.rs` ends", &mut out);
        assert_eq!(out, vec!["src/a.rs"]);

        let mut out = Vec::new();
        collect_code_mentions("`src/a.rs` and `docs/b.md`", &mut out);
        assert_eq!(out, vec!["src/a.rs", "docs/b.md"]);

        // A span Markdown never closes swallows the rest of the line.
        let mut out = Vec::new();
        collect_code_mentions("unclosed `src/a.rs", &mut out);
        assert!(out.is_empty());

        // CommonMark strips one padding space inside a span.
        let mut out = Vec::new();
        collect_code_mentions("`` src/a.rs ``", &mut out);
        assert_eq!(out, vec!["src/a.rs"]);
    }

    #[test]
    fn link_targets_drop_urls_titles_and_fragments() {
        assert_eq!(link_target("./a.md"), Some("./a.md".to_string()));
        assert_eq!(link_target("a.md#head"), Some("a.md".to_string()));
        assert_eq!(link_target("a.md \"Title\""), Some("a.md".to_string()));
        assert_eq!(link_target("https://example.com/a"), None);
    }
}
