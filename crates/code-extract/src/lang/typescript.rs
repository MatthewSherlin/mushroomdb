//! TypeScript and TSX: definitions, imports, and specifier resolution.
//!
//! JavaScript shares everything here except the grammar and the query, so the
//! interpretation helpers and [`resolve_import`] are used by
//! [`super::javascript`] too.

use super::{doc_above, field, named_children, signature, text, Definition, DocStyle, Spec};
use crate::{first_known, join, parent_dir};
use std::sync::OnceLock;
use tree_sitter::{Language, Node, Query};

const QUERY: &str = r"
(function_declaration) @def
(generator_function_declaration) @def
(function_signature) @def
(class_declaration) @def
(abstract_class_declaration) @def
(method_definition) @def
(method_signature) @def
(abstract_method_signature) @def
(interface_declaration) @def
(type_alias_declaration) @def
(enum_declaration) @def
(program (lexical_declaration (variable_declarator) @def))
(program (export_statement (lexical_declaration (variable_declarator) @def)))
(import_statement) @import
(export_statement) @import
(call_expression) @import
(call_expression) @call
";

pub(crate) const DOC_STYLE: DocStyle = DocStyle {
    comments: &["comment"],
    skipped: &["decorator"],
    wrappers: &[
        "export_statement",
        "lexical_declaration",
        "variable_declarator",
    ],
    marker_required: true,
};

pub(crate) struct TypeScript {
    pub tsx: bool,
}

impl Spec for TypeScript {
    fn language(&self) -> Language {
        if self.tsx {
            Language::new(tree_sitter_typescript::LANGUAGE_TSX)
        } else {
            Language::new(tree_sitter_typescript::LANGUAGE_TYPESCRIPT)
        }
    }

    fn query_source(&self) -> &'static str {
        QUERY
    }

    fn cache(&self) -> &'static OnceLock<Option<Query>> {
        static TS: OnceLock<Option<Query>> = OnceLock::new();
        static TSX: OnceLock<Option<Query>> = OnceLock::new();
        if self.tsx {
            &TSX
        } else {
            &TS
        }
    }

    fn definition(&self, node: Node, src: &str) -> Option<Definition> {
        definition_of(node, src)
    }

    fn imports(&self, node: Node, src: &str) -> Vec<String> {
        imports_of(node, src)
    }

    fn callee(&self, node: Node, src: &str) -> Option<String> {
        callee_of(node, src)
    }
}

/// Interpret a captured definition node, shared by TypeScript and JavaScript.
pub(crate) fn definition_of(node: Node, src: &str) -> Option<Definition> {
    let kind = match node.kind() {
        "function_declaration" | "generator_function_declaration" | "function_signature" => {
            "function"
        }
        "class_declaration" | "abstract_class_declaration" => "class",
        "method_definition" | "method_signature" | "abstract_method_signature" => "method",
        "interface_declaration" => "interface",
        "type_alias_declaration" => "type",
        "enum_declaration" => "enum",
        "variable_declarator" => "const",
        _ => return None,
    };
    // Destructuring (`const { parse } = …`) binds a pattern, not a name, and
    // is state rather than a declaration worth a symbol.
    let name_node = node.child_by_field_name("name")?;
    if !name_node.kind().ends_with("identifier") {
        return None;
    }
    let name = text(name_node, src).trim().to_string();
    if name.is_empty() {
        return None;
    }
    let qualified = match enclosing_type(node, src) {
        Some(outer) => format!("{outer}.{name}"),
        None => name,
    };
    Some(Definition {
        name: qualified,
        kind,
        signature: signature(node, src),
        doc: doc_above(node, src, &DOC_STYLE),
    })
}

/// The name of the innermost enclosing class or interface.
fn enclosing_type(node: Node, src: &str) -> Option<String> {
    let mut cursor = node.parent();
    while let Some(parent) = cursor {
        if matches!(
            parent.kind(),
            "class_declaration" | "abstract_class_declaration" | "interface_declaration"
        ) {
            return field(parent, "name", src).map(|name| name.trim().to_string());
        }
        cursor = parent.parent();
    }
    None
}

/// `import … from "x"`, `export … from "x"` and `require("x")`.
pub(crate) fn imports_of(node: Node, src: &str) -> Vec<String> {
    match node.kind() {
        "import_statement" | "export_statement" => node
            .child_by_field_name("source")
            .and_then(|source| unquote(text(source, src)))
            .map_or_else(Vec::new, |spec| vec![spec]),
        "call_expression" => {
            let function = node.child_by_field_name("function");
            let name = function.map_or("", |f| text(f, src).trim());
            if name != "require" && name != "import" {
                return Vec::new();
            }
            let Some(arguments) = node.child_by_field_name("arguments") else {
                return Vec::new();
            };
            named_children(arguments)
                .into_iter()
                .next()
                .filter(|first| first.kind() == "string")
                .and_then(|first| unquote(text(first, src)))
                .map_or_else(Vec::new, |spec| vec![spec])
        }
        _ => Vec::new(),
    }
}

pub(crate) fn callee_of(node: Node, src: &str) -> Option<String> {
    let function = node.child_by_field_name("function")?;
    let callee = text(function, src).trim();
    (!callee.is_empty() && !callee.contains('\n')).then(|| callee.to_string())
}

/// Strip one layer of matching quotes from a string literal.
fn unquote(raw: &str) -> Option<String> {
    let raw = raw.trim();
    for quote in ['"', '\'', '`'] {
        if let Some(inner) = raw.strip_prefix(quote).and_then(|r| r.strip_suffix(quote)) {
            return (!inner.is_empty()).then(|| inner.to_string());
        }
    }
    None
}

// ── resolution ──────────────────────────────────────────────────────────────

/// Source extensions tried when a specifier has none, in preference order.
const EXTENSIONS: [&str; 8] = ["ts", "tsx", "d.ts", "mts", "cts", "js", "jsx", "mjs"];

/// Resolve a TypeScript or JavaScript specifier. See [`crate::resolve_import`].
pub(crate) fn resolve_import(from: &str, raw: &str, known: &dyn Fn(&str) -> bool) -> Vec<String> {
    if !(raw.starts_with("./") || raw.starts_with("../")) {
        return Vec::new();
    }
    let spec = join(parent_dir(from), raw);
    if spec.is_empty() {
        return Vec::new();
    }

    let mut candidates = vec![spec.clone()];
    // TypeScript sources import their own compiled output by name, so a
    // specifier ending in `.js` is usually a `.ts` file on disk.
    for (from_ext, to_exts) in [
        (".js", ["ts", "tsx"]),
        (".mjs", ["mts", "ts"]),
        (".cjs", ["cts", "ts"]),
        (".jsx", ["tsx", "ts"]),
    ] {
        if let Some(stem) = spec.strip_suffix(from_ext) {
            for to in to_exts {
                candidates.push(format!("{stem}.{to}"));
            }
        }
    }
    for ext in EXTENSIONS {
        candidates.push(format!("{spec}.{ext}"));
    }
    for ext in EXTENSIONS {
        candidates.push(format!("{spec}/index.{ext}"));
    }
    first_known(&candidates, known)
}
