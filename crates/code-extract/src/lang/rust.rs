//! Rust: definitions, `use` trees, module declarations, and path resolution.

use super::{doc_above, field, named_children, signature, text, Definition, DocStyle, Spec};
use crate::{ancestors, file_stem, first_known, join, parent_dir};
use std::sync::OnceLock;
use tree_sitter::{Language, Node, Query};

/// `mod x;` declarations are recorded as imports with this prefix, which
/// keeps them distinguishable from `use` paths in a single string field.
pub(crate) const MOD_PREFIX: &str = "mod ";

const QUERY: &str = r"
(function_item) @def
(function_signature_item) @def
(struct_item) @def
(union_item) @def
(enum_item) @def
(trait_item) @def
(type_item) @def
(const_item) @def
(static_item) @def
(mod_item) @def
(use_declaration) @import
(mod_item) @import
(call_expression) @call
";

const DOC_STYLE: DocStyle = DocStyle {
    comments: &["line_comment", "block_comment"],
    skipped: &["attribute_item"],
    wrappers: &[],
    marker_required: true,
};

pub(crate) struct Rust;

impl Spec for Rust {
    fn language(&self) -> Language {
        Language::new(tree_sitter_rust::LANGUAGE)
    }

    fn query_source(&self) -> &'static str {
        QUERY
    }

    fn cache(&self) -> &'static OnceLock<Option<Query>> {
        static CACHE: OnceLock<Option<Query>> = OnceLock::new();
        &CACHE
    }

    fn definition(&self, node: Node, src: &str) -> Option<Definition> {
        let name = field(node, "name", src)?.to_string();
        let kind = match node.kind() {
            "function_item" | "function_signature_item" => {
                if in_impl_or_trait(node) {
                    "method"
                } else {
                    "function"
                }
            }
            "struct_item" | "union_item" => "struct",
            "enum_item" => "enum",
            "trait_item" => "trait",
            "type_item" => "type",
            "const_item" | "static_item" => "const",
            // A `mod x;` declaration is an import, not a definition.
            "mod_item" if node.child_by_field_name("body").is_some() => "module",
            _ => return None,
        };
        Some(Definition {
            name: qualify(node, src, &name),
            kind,
            signature: signature(node, src),
            doc: doc_above(node, src, &DOC_STYLE),
        })
    }

    fn imports(&self, node: Node, src: &str) -> Vec<String> {
        match node.kind() {
            "use_declaration" => {
                let mut out = Vec::new();
                if let Some(argument) = node.child_by_field_name("argument") {
                    expand_use(argument, src, "", &mut out);
                }
                out
            }
            "mod_item" if node.child_by_field_name("body").is_none() => field(node, "name", src)
                .map_or_else(Vec::new, |name| vec![format!("{MOD_PREFIX}{name}")]),
            _ => Vec::new(),
        }
    }

    fn callee(&self, node: Node, src: &str) -> Option<String> {
        let function = node.child_by_field_name("function")?;
        let callee = text(function, src).trim();
        (!callee.is_empty() && !callee.contains('\n')).then(|| callee.to_string())
    }
}

/// True when the nearest definition-bearing ancestor is an `impl` or `trait`.
fn in_impl_or_trait(node: Node) -> bool {
    let mut cursor = node.parent();
    while let Some(parent) = cursor {
        match parent.kind() {
            "impl_item" | "trait_item" => return true,
            "mod_item" | "source_file" => return false,
            _ => cursor = parent.parent(),
        }
    }
    false
}

/// Qualify `name` with the `impl`, `trait` and `mod` blocks around it:
/// `Record.new`, `Summary.summary`, `inner::seed`.
fn qualify(node: Node, src: &str, name: &str) -> String {
    let mut qualified = name.to_string();
    let mut cursor = node.parent();
    while let Some(parent) = cursor {
        let step = match parent.kind() {
            "impl_item" => field(parent, "type", src).map(|ty| (strip_generics(ty), '.')),
            "trait_item" => field(parent, "name", src).map(|n| (n.to_string(), '.')),
            "mod_item" => field(parent, "name", src).map(|n| (n.to_string(), ':')),
            _ => None,
        };
        if let Some((outer, sep)) = step {
            if !outer.is_empty() {
                qualified = if sep == ':' {
                    format!("{outer}::{qualified}")
                } else {
                    format!("{outer}.{qualified}")
                };
            }
        }
        cursor = parent.parent();
    }
    qualified
}

/// `Vec<u8>` → `Vec`, `&Record` → `Record`.
fn strip_generics(ty: &str) -> String {
    ty.split('<')
        .next()
        .unwrap_or(ty)
        .trim_start_matches(['&', '*'])
        .trim()
        .to_string()
}

/// How deep a `use` tree is followed. Real nesting is a handful of levels;
/// the bound keeps a pathological file from recursing into the stack.
const MAX_USE_DEPTH: u32 = 64;

/// Flatten a `use` tree into one path per leaf: `use a::{b, c::d}` becomes
/// `a::b` and `a::c::d`. Aliases are dropped and globs lose their `::*`,
/// because what matters downstream is the module, not the binding.
fn expand_use(node: Node, src: &str, prefix: &str, out: &mut Vec<String>) {
    expand_use_at(node, src, prefix, out, 0);
}

fn expand_use_at(node: Node, src: &str, prefix: &str, out: &mut Vec<String>, depth: u32) {
    if depth > MAX_USE_DEPTH {
        return;
    }
    let expand_use = |node: Node, src: &str, prefix: &str, out: &mut Vec<String>| {
        expand_use_at(node, src, prefix, out, depth + 1);
    };
    match node.kind() {
        "scoped_use_list" => {
            let path = field(node, "path", src).unwrap_or("");
            let inner = joined(prefix, path);
            if let Some(list) = node.child_by_field_name("list") {
                for child in named_children(list) {
                    expand_use(child, src, &inner, out);
                }
            }
        }
        "use_list" => {
            for child in named_children(node) {
                expand_use(child, src, prefix, out);
            }
        }
        "use_as_clause" => {
            if let Some(path) = node.child_by_field_name("path") {
                expand_use(path, src, prefix, out);
            }
        }
        "use_wildcard" => {
            let raw = text(node, src).trim_end_matches('*').trim_end_matches(':');
            let path = joined(prefix, raw);
            if !path.is_empty() {
                out.push(path);
            }
        }
        _ => {
            let path = joined(prefix, text(node, src).trim());
            if !path.is_empty() {
                out.push(path);
            }
        }
    }
}

fn joined(prefix: &str, rest: &str) -> String {
    match (prefix.is_empty(), rest.is_empty()) {
        (true, _) => rest.to_string(),
        (false, true) => prefix.to_string(),
        (false, false) => format!("{prefix}::{rest}"),
    }
}

// ── resolution ──────────────────────────────────────────────────────────────

/// Resolve a Rust `use` path or `mod` declaration. See [`crate::resolve_import`].
pub(crate) fn resolve_import(from: &str, raw: &str, known: &dyn Fn(&str) -> bool) -> Vec<String> {
    if let Some(name) = raw.strip_prefix(MOD_PREFIX) {
        let name = name.trim().trim_end_matches(';').trim();
        if name.is_empty() {
            return Vec::new();
        }
        let dir = module_dir(from);
        return first_known(
            &[
                join(&dir, &format!("{name}.rs")),
                join(&dir, &format!("{name}/mod.rs")),
            ],
            known,
        );
    }

    let segments: Vec<&str> = raw
        .trim_start_matches("::")
        .split("::")
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "*" && *s != "self")
        .collect();
    let Some((first, rest)) = segments.split_first() else {
        return Vec::new();
    };

    match *first {
        "crate" => match crate_root(from, known) {
            Some(root) => under(&join(&root, "src"), rest, known),
            None => Vec::new(),
        },
        "super" => {
            let mut base = module_dir(from);
            let mut rest = segments.as_slice();
            while rest.first() == Some(&"super") {
                base = parent_dir(&base).to_string();
                rest = &rest[1..];
            }
            under(&base, rest, known)
        }
        _ => package_root(from, first, known),
    }
}

/// Try the longest module prefix first: `a::b` prefers `a/b.rs` over `a.rs`.
fn under(base: &str, segments: &[&str], known: &dyn Fn(&str) -> bool) -> Vec<String> {
    let mut candidates = Vec::new();
    for take in (1..=segments.len()).rev() {
        let path = join(base, &segments[..take].join("/"));
        candidates.push(format!("{path}.rs"));
        candidates.push(format!("{path}/mod.rs"));
    }
    first_known(&candidates, known)
}

/// The directory a file's submodules live in. `lib.rs`, `main.rs` and
/// `mod.rs` own the directory they sit in; every other file owns a directory
/// named after its stem.
fn module_dir(from: &str) -> String {
    let dir = parent_dir(from);
    let stem = file_stem(from);
    if matches!(stem, "lib" | "main" | "mod") {
        dir.to_string()
    } else {
        join(dir, stem)
    }
}

/// The nearest ancestor that has a `Cargo.toml` and holds `from` under its
/// `src/`.
fn crate_root(from: &str, known: &dyn Fn(&str) -> bool) -> Option<String> {
    for dir in ancestors(parent_dir(from)) {
        let manifest = join(&dir, "Cargo.toml");
        let src = join(&dir, "src");
        if known(&manifest) && from.starts_with(&format!("{src}/")) {
            return Some(dir);
        }
    }
    None
}

/// A leading path segment that names a sibling package directory resolves to
/// that package's `src/lib.rs`. Package directories are conventionally named
/// after the package, so `beta_core` also tries `beta-core`.
fn package_root(from: &str, name: &str, known: &dyn Fn(&str) -> bool) -> Vec<String> {
    let mut parents: Vec<String> = Vec::new();
    if let Some(root) = crate_root(from, known) {
        parents.push(parent_dir(&root).to_string());
    }
    parents.extend(ancestors(parent_dir(from)));
    parents.push("crates".to_string());
    parents.dedup();

    let variants = [name.to_string(), name.replace('_', "-")];
    let mut candidates = Vec::new();
    for parent in &parents {
        for variant in &variants {
            candidates.push(join(parent, &format!("{variant}/src/lib.rs")));
        }
    }
    first_known(&candidates, known)
}
