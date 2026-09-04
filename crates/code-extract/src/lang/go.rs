//! Go: definitions, imports, and package-directory resolution.

use super::{doc_above, field, signature, text, Definition, DocStyle, Spec};
use crate::{ancestors, join, parent_dir};
use std::sync::OnceLock;
use tree_sitter::{Language, Node, Query};

const QUERY: &str = r"
(function_declaration) @def
(method_declaration) @def
(type_declaration (type_spec) @def)
(const_declaration (const_spec) @def)
(import_spec) @import
(call_expression) @call
";

const DOC_STYLE: DocStyle = DocStyle {
    comments: &["comment"],
    skipped: &[],
    wrappers: &["type_declaration", "const_declaration", "var_declaration"],
    // Go has no doc marker: a comment directly above a declaration is its doc.
    marker_required: false,
};

pub(crate) struct Go;

impl Spec for Go {
    fn language(&self) -> Language {
        Language::new(tree_sitter_go::LANGUAGE)
    }

    fn query_source(&self) -> &'static str {
        QUERY
    }

    fn cache(&self) -> &'static OnceLock<Option<Query>> {
        static CACHE: OnceLock<Option<Query>> = OnceLock::new();
        &CACHE
    }

    fn definition(&self, node: Node, src: &str) -> Option<Definition> {
        let name = field(node, "name", src)?.trim().to_string();
        if name.is_empty() {
            return None;
        }
        let (kind, qualified) = match node.kind() {
            "function_declaration" => ("function", name),
            "method_declaration" => {
                let receiver = node
                    .child_by_field_name("receiver")
                    .map(|node| receiver_type(text(node, src)))
                    .filter(|receiver| !receiver.is_empty());
                match receiver {
                    Some(receiver) => ("method", format!("{receiver}.{name}")),
                    None => ("method", name),
                }
            }
            "type_spec" => {
                let kind = match node.child_by_field_name("type").map(|ty| ty.kind()) {
                    Some("struct_type") => "struct",
                    Some("interface_type") => "interface",
                    _ => "type",
                };
                (kind, name)
            }
            "const_spec" => ("const", name),
            _ => return None,
        };
        Some(Definition {
            name: qualified,
            kind,
            signature: signature(node, src),
            doc: doc_above(node, src, &DOC_STYLE),
        })
    }

    fn imports(&self, node: Node, src: &str) -> Vec<String> {
        // One capture per spec, so a grouped `import ( … )` block still gives
        // every path its own line.
        let Some(path) = node.child_by_field_name("path") else {
            return Vec::new();
        };
        let raw = text(path, src).trim().trim_matches('"').trim_matches('`');
        if raw.is_empty() {
            return Vec::new();
        }
        vec![raw.to_string()]
    }

    fn callee(&self, node: Node, src: &str) -> Option<String> {
        let function = node.child_by_field_name("function")?;
        let callee = text(function, src).trim();
        (!callee.is_empty() && !callee.contains('\n')).then(|| callee.to_string())
    }
}

/// `(s *Store)` → `Store`, `(r Reader[T])` → `Reader`.
fn receiver_type(raw: &str) -> String {
    let inner = raw.trim().trim_start_matches('(').trim_end_matches(')');
    let last = inner.split_whitespace().last().unwrap_or("");
    let last = last.trim_start_matches(['*', '&']);
    let last = last.split('[').next().unwrap_or(last);
    last.rsplit('.').next().unwrap_or(last).to_string()
}

// ── resolution ──────────────────────────────────────────────────────────────

/// Resolve a Go import path to the package **directory** it names, with a
/// trailing `/`. See [`crate::resolve_import`] for why this is a directory
/// rather than a file.
///
/// The module prefix is stripped by trying successively shorter suffixes of
/// the import path, longest first, so `example.com/demo/store` finds
/// `store/` without anyone reading `go.mod`. Within one suffix the search
/// starts at the working-tree root and moves inwards, which prefers the
/// module's own layout over a same-named directory nested beside the
/// importing file.
pub(crate) fn resolve_import(from: &str, raw: &str, known: &dyn Fn(&str) -> bool) -> Vec<String> {
    let segments: Vec<&str> = raw.split('/').filter(|seg| !seg.is_empty()).collect();
    if segments.is_empty() {
        return Vec::new();
    }
    let mut bases = ancestors(parent_dir(from));
    bases.reverse();

    for start in 0..segments.len() {
        let suffix = segments[start..].join("/");
        for base in &bases {
            let dir = join(base, &suffix);
            if dir.is_empty() {
                continue;
            }
            let probe = format!("{dir}/");
            if known(&probe) {
                return vec![probe];
            }
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receivers_reduce_to_a_bare_type_name() {
        assert_eq!(receiver_type("(s *Store)"), "Store");
        assert_eq!(receiver_type("(r Reader)"), "Reader");
        assert_eq!(receiver_type("(b *Buf[T])"), "Buf");
    }
}
