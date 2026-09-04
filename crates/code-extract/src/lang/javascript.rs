//! JavaScript: the same shapes TypeScript has, minus the type declarations.
//!
//! Interpretation and specifier resolution live in [`super::typescript`];
//! only the grammar and the query differ.

use super::typescript::{callee_of, definition_of, imports_of};
use super::{Definition, Spec};
use std::sync::OnceLock;
use tree_sitter::{Language, Node, Query};

const QUERY: &str = r"
(function_declaration) @def
(generator_function_declaration) @def
(class_declaration) @def
(method_definition) @def
(program (lexical_declaration (variable_declarator) @def))
(program (export_statement (lexical_declaration (variable_declarator) @def)))
(import_statement) @import
(export_statement) @import
(call_expression) @import
(call_expression) @call
";

pub(crate) struct JavaScript;

impl Spec for JavaScript {
    fn language(&self) -> Language {
        Language::new(tree_sitter_javascript::LANGUAGE)
    }

    fn query_source(&self) -> &'static str {
        QUERY
    }

    fn cache(&self) -> &'static OnceLock<Option<Query>> {
        static CACHE: OnceLock<Option<Query>> = OnceLock::new();
        &CACHE
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
