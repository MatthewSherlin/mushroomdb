//! The shared parsing driver.
//!
//! Every language contributes three things: a tree-sitter grammar, one query
//! that captures definitions, imports and calls, and the small amount of
//! judgement needed to turn a captured node into a name, a kind, a signature
//! and a doc line. The driver owns everything else — parsing under a time
//! budget, attaching calls to the innermost enclosing definition, and the
//! sorting that makes the output reproducible.
//!
//! Queries are deliberately thin. They say *which* nodes matter and nothing
//! about what they mean, because naming rules (`Type.method`, `mod::fn`) need
//! to look at ancestors, which is far easier to read as Rust than as a
//! pattern.

pub(crate) mod go;
pub(crate) mod javascript;
pub(crate) mod python;
pub(crate) mod rust;
pub(crate) mod typescript;

use crate::{squeeze, truncate_chars};
use crate::{ImportFact, Lang, SymbolFact, MAX_CALLS, MAX_TEXT_CHARS, PARSE_BUDGET_MS};
use std::ops::{ControlFlow, Range};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tree_sitter::{
    Language, Node, ParseOptions, ParseState, Parser, Point, Query, QueryCursor, StreamingIterator,
    Tree,
};

/// Capture name for a definition node.
const CAP_DEF: &str = "def";
/// Capture name for an import node.
const CAP_IMPORT: &str = "import";
/// Capture name for a call node.
const CAP_CALL: &str = "call";

/// A definition, already qualified within its file.
pub(crate) struct Definition {
    pub name: String,
    pub kind: &'static str,
    pub signature: String,
    pub doc: String,
}

/// What one language contributes to the driver.
pub(crate) trait Spec: Sync {
    fn language(&self) -> Language;
    fn query_source(&self) -> &'static str;
    /// Process-wide cache for the compiled query.
    fn cache(&self) -> &'static OnceLock<Option<Query>>;
    /// Interpret a `@def` capture, or skip it.
    fn definition(&self, node: Node, src: &str) -> Option<Definition>;
    /// Interpret an `@import` capture as zero or more raw import strings.
    fn imports(&self, node: Node, src: &str) -> Vec<String>;
    /// Interpret a `@call` capture as the callee, as written.
    fn callee(&self, node: Node, src: &str) -> Option<String>;
}

fn spec_for(lang: Lang) -> Option<&'static dyn Spec> {
    static RUST: rust::Rust = rust::Rust;
    static PYTHON: python::Python = python::Python;
    static TYPESCRIPT: typescript::TypeScript = typescript::TypeScript { tsx: false };
    static TSX: typescript::TypeScript = typescript::TypeScript { tsx: true };
    static JAVASCRIPT: javascript::JavaScript = javascript::JavaScript;
    static GO: go::Go = go::Go;
    match lang {
        Lang::Rust => Some(&RUST),
        Lang::Python => Some(&PYTHON),
        Lang::TypeScript => Some(&TYPESCRIPT),
        Lang::Tsx => Some(&TSX),
        Lang::JavaScript => Some(&JAVASCRIPT),
        Lang::Go => Some(&GO),
        Lang::Markdown | Lang::Other => None,
    }
}

/// Parse `src` and return its symbols and imports.
///
/// `None` means the file could not be read structurally — an unsupported
/// language, a query that failed to compile, or a parse that ran past the
/// budget. The caller degrades to hash-only facts.
pub(crate) fn extract(lang: Lang, src: &str) -> Option<(Vec<SymbolFact>, Vec<ImportFact>)> {
    let spec = spec_for(lang)?;
    let query = compiled_query(spec)?;
    let tree = parse(&spec.language(), src)?;

    let mut cursor = QueryCursor::new();
    let names = query.capture_names();
    let mut defs: Vec<(Range<usize>, SymbolFact)> = Vec::new();
    let mut imports: Vec<ImportFact> = Vec::new();
    let mut calls: Vec<(usize, String, u32)> = Vec::new();

    let mut matches = cursor.matches(query, tree.root_node(), src.as_bytes());
    while let Some(matched) = matches.next() {
        for capture in matched.captures() {
            let node = capture.node;
            match names.get(capture.index as usize).copied().unwrap_or("") {
                CAP_DEF => {
                    if let Some(def) = spec.definition(node, src) {
                        defs.push((node.byte_range(), symbol(def, node)));
                    }
                }
                CAP_IMPORT => {
                    let line = line_of(node);
                    for raw in spec.imports(node, src) {
                        imports.push(ImportFact { raw, line });
                    }
                }
                CAP_CALL => {
                    if let Some(callee) = spec.callee(node, src) {
                        calls.push((node.start_byte(), callee, line_of(node)));
                    }
                }
                _ => {}
            }
        }
    }

    attach_calls(&mut defs, calls);

    let mut symbols: Vec<SymbolFact> = defs.into_iter().map(|(_, fact)| fact).collect();
    symbols.sort_by(|a, b| {
        (a.line_start, &a.name, a.line_end).cmp(&(b.line_start, &b.name, b.line_end))
    });
    symbols.dedup_by(|a, b| a.name == b.name && a.line_start == b.line_start);

    imports.sort_by(|a, b| (a.line, &a.raw).cmp(&(b.line, &b.raw)));
    imports.dedup();

    Some((symbols, imports))
}

/// Give every call to the innermost definition that encloses it.
///
/// Calls outside any definition — a top-level statement, a module
/// initialiser — have no symbol to hang from and are dropped.
///
/// Definition ranges come from a syntax tree, so they nest: two of them are
/// either disjoint or one contains the other, never partially overlapping.
/// That lets a single sweep over both lists in position order do the work with
/// a stack of open definitions, instead of rescanning every definition for
/// every call site. The parse budget does not cover this step, and a
/// machine-generated source can hold thousands of each, so the difference is
/// the difference between linear and quadratic on exactly the input least
/// likely to have been tried by hand.
fn attach_calls(defs: &mut [(Range<usize>, SymbolFact)], mut calls: Vec<(usize, String, u32)>) {
    // Definitions in the order the sweep opens them. Equal starts put the
    // shorter range later so it lands on top of the stack, and a full tie
    // falls back to the earlier definition, which is the one a naive
    // innermost-wins scan would have picked.
    let mut order: Vec<usize> = (0..defs.len()).collect();
    order.sort_by(|a, b| {
        let (left, right) = (&defs[*a].0, &defs[*b].0);
        left.start
            .cmp(&right.start)
            .then(right.end.cmp(&left.end))
            .then(b.cmp(a))
    });
    calls.sort();

    let mut open: Vec<usize> = Vec::new();
    let mut next = 0;
    for (at, callee, line) in calls {
        while let Some(index) = order.get(next).copied() {
            if defs[index].0.start > at {
                break;
            }
            open.push(index);
            next += 1;
        }
        while let Some(index) = open.last().copied() {
            if defs[index].0.end > at {
                break;
            }
            open.pop();
        }
        if let Some(index) = open.last().copied() {
            defs[index].1.calls.push((callee, line));
        }
    }

    for (_, fact) in defs.iter_mut() {
        fact.calls.sort();
        fact.calls.dedup();
        fact.calls.truncate(MAX_CALLS);
    }
}

fn symbol(def: Definition, node: Node) -> SymbolFact {
    SymbolFact {
        name: def.name,
        kind: def.kind,
        line_start: line_of(node),
        line_end: row_to_line(node.end_position()),
        signature: def.signature,
        doc: def.doc,
        calls: Vec::new(),
    }
}

fn compiled_query(spec: &'static dyn Spec) -> Option<&'static Query> {
    spec.cache()
        .get_or_init(|| Query::new(&spec.language(), spec.query_source()).ok())
        .as_ref()
}

/// Parse under a wall-clock budget. Tree-sitter calls the progress callback
/// periodically; returning `Break` cancels the parse, and `parse_with_options`
/// then yields `None`.
fn parse(language: &Language, src: &str) -> Option<Tree> {
    parse_within(language, src, Duration::from_millis(PARSE_BUDGET_MS))
}

fn parse_within(language: &Language, src: &str, budget: Duration) -> Option<Tree> {
    let mut parser = Parser::new();
    parser.set_language(language).ok()?;
    let deadline = Instant::now() + budget;
    let bytes = src.as_bytes();
    let mut input = |offset: usize, _: Point| -> &[u8] { bytes.get(offset..).unwrap_or(&[]) };
    let mut progress = |_: &ParseState| -> ControlFlow<()> {
        if Instant::now() >= deadline {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let options = ParseOptions::new().progress_callback(&mut progress);
    parser.parse_with_options(&mut input, None, Some(options))
}

// ── node helpers shared by the language modules ─────────────────────────────

/// 1-based start line of `node`.
pub(crate) fn line_of(node: Node) -> u32 {
    row_to_line(node.start_position())
}

fn row_to_line(point: Point) -> u32 {
    point.row.saturating_add(1).try_into().unwrap_or(u32::MAX)
}

/// Source text of `node`, or `""` when it is not valid UTF-8.
pub(crate) fn text<'a>(node: Node, src: &'a str) -> &'a str {
    node.utf8_text(src.as_bytes()).unwrap_or("")
}

/// Source text of the named field of `node`.
pub(crate) fn field<'a>(node: Node, name: &str, src: &'a str) -> Option<&'a str> {
    node.child_by_field_name(name).map(|child| text(child, src))
}

/// Every named child of `node`, in source order.
pub(crate) fn named_children<'tree>(node: Node<'tree>) -> Vec<Node<'tree>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

/// Every child of `node` bound to the field `name`. Unlike
/// `child_by_field_name` this returns all of them, which is what a Python
/// `import a, b` or `from x import a, b` needs.
pub(crate) fn field_children<'tree>(node: Node<'tree>, name: &str) -> Vec<Node<'tree>> {
    let mut cursor = node.walk();
    let mut out = Vec::new();
    if cursor.goto_first_child() {
        loop {
            if cursor.field_name() == Some(name) {
                out.push(cursor.node());
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    out
}

/// The declaration header: everything before the body, cut at the first line
/// break, with a trailing brace removed.
pub(crate) fn signature(node: Node, src: &str) -> String {
    let start = node.start_byte();
    let end = node
        .child_by_field_name("body")
        .map_or(node.end_byte(), |body| body.start_byte())
        .max(start);
    let raw = src.get(start..end).unwrap_or("");
    let first_line = raw.split('\n').next().unwrap_or("");
    let trimmed = first_line.trim().trim_end_matches(['{', '(']).trim_end();
    truncate_chars(&squeeze(trimmed), MAX_TEXT_CHARS)
}

/// The comment kinds a language uses, and how to tell a doc comment from an
/// ordinary one.
pub(crate) struct DocStyle {
    /// Node kinds that count as comments.
    pub comments: &'static [&'static str],
    /// Node kinds that sit between a doc comment and its definition —
    /// attributes, decorators — and should be stepped over.
    pub skipped: &'static [&'static str],
    /// Node kinds that wrap a definition, whose own siblings carry the doc.
    pub wrappers: &'static [&'static str],
    /// When true, only an *outer* doc comment counts: `///` or `/** … */`.
    ///
    /// An inner doc comment — `//!`, `/*! … */` — documents the module it sits
    /// in, not whatever happens to follow it, so it is never a definition's
    /// doc. Without this, the first item in a file would inherit the module's
    /// own description.
    pub marker_required: bool,
}

/// The last row a node's text actually covers.
///
/// Some grammars end a line comment at column 0 of the *following* row,
/// because the node swallows its trailing newline. Taking `end_position().row`
/// at face value would then make every comment look adjacent to the line after
/// the blank one below it.
fn last_row(node: Node) -> usize {
    let end = node.end_position();
    if end.column == 0 && end.row > node.start_position().row {
        end.row - 1
    } else {
        end.row
    }
}

/// The first line of the doc comment above `node`, if there is one.
///
/// Walks upwards through wrappers (`export …`, `@decorator`), then backwards
/// over attributes and contiguous comment lines, and returns the first
/// non-empty line of the topmost comment. A blank line between the comment
/// and the definition breaks the association, the same way a reader would
/// read it.
pub(crate) fn doc_above(node: Node, src: &str, style: &DocStyle) -> String {
    let mut anchor = node;
    while let Some(parent) = anchor.parent() {
        if style.wrappers.contains(&parent.kind()) {
            anchor = parent;
        } else {
            break;
        }
    }

    let mut block: Vec<String> = Vec::new();
    let mut next_row = anchor.start_position().row;
    let mut cursor = anchor;
    while let Some(prev) = cursor.prev_sibling() {
        let kind = prev.kind();
        if style.skipped.contains(&kind) {
            next_row = prev.start_position().row;
            cursor = prev;
            continue;
        }
        if !style.comments.contains(&kind) {
            break;
        }
        if last_row(prev) + 1 < next_row {
            break;
        }
        let raw = text(prev, src);
        if style.marker_required && !is_outer_doc_marker(raw) {
            break;
        }
        block.push(raw.to_string());
        next_row = prev.start_position().row;
        cursor = prev;
    }

    let Some(top) = block.pop() else {
        return String::new();
    };
    first_doc_line(&top)
}

/// Whether `raw` opens a doc comment that documents what comes *after* it.
/// `//!` and `/*!` are deliberately absent: they document the enclosing
/// module.
fn is_outer_doc_marker(raw: &str) -> bool {
    raw.starts_with("///") || raw.starts_with("/**")
}

/// Strip comment punctuation and return the first line with content.
pub(crate) fn first_doc_line(raw: &str) -> String {
    let body = raw
        .trim()
        .trim_start_matches("/**")
        .trim_start_matches("/*")
        .trim_end_matches("*/");
    for line in body.lines() {
        let line = line
            .trim()
            .trim_start_matches("///")
            .trim_start_matches("//!")
            .trim_start_matches("//")
            .trim_start_matches('*')
            .trim();
        if !line.is_empty() {
            return truncate_chars(&squeeze(line), MAX_TEXT_CHARS);
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_language_query_compiles() {
        for lang in [
            Lang::Rust,
            Lang::Python,
            Lang::TypeScript,
            Lang::Tsx,
            Lang::JavaScript,
            Lang::Go,
        ] {
            let spec = spec_for(lang).expect("spec");
            assert!(
                Query::new(&spec.language(), spec.query_source()).is_ok(),
                "query for {lang:?} does not compile: {:?}",
                Query::new(&spec.language(), spec.query_source()).err()
            );
        }
    }

    #[test]
    fn an_exhausted_budget_cancels_the_parse() {
        let spec = spec_for(Lang::Rust).expect("spec");
        let src = "pub fn generated() -> u32 { 1 }\n".repeat(20_000);
        assert!(parse_within(&spec.language(), &src, Duration::ZERO).is_none());
        assert!(parse_within(&spec.language(), &src, Duration::from_secs(30)).is_some());
    }

    #[test]
    fn doc_lines_lose_their_punctuation() {
        assert_eq!(first_doc_line("/// A record."), "A record.");
        assert_eq!(first_doc_line("//! Crate root."), "Crate root.");
        assert_eq!(first_doc_line("/** A queue. */"), "A queue.");
        assert_eq!(first_doc_line("/**\n * A queue.\n */"), "A queue.");
        assert_eq!(first_doc_line("//"), "");
    }
}
