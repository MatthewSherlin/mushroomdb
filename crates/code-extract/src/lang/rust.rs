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
(macro_invocation) @call
";

/// How deep a macro's token tree is followed looking for calls. Real nesting
/// is a handful of levels; the bound keeps a pathological file from recursing
/// into the stack.
const MAX_TOKEN_TREE_DEPTH: u32 = 32;

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

    /// The calls written inside a macro invocation.
    ///
    /// `format!`, `assert_eq!`, `write!` and `vec!` take expressions, but the
    /// grammar cannot know that: a macro body is a `token_tree` of loose
    /// tokens, so a call written inside one is never a `call_expression` and
    /// was invisible to the graph. On this repository that hid every call made
    /// from inside a `format!` — which, in code whose job is rendering, is most
    /// of them.
    fn hidden_calls<'t>(&self, node: Node<'t>, src: &str) -> Vec<(String, Node<'t>, bool)> {
        if node.kind() != "macro_invocation" {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "token_tree" {
                calls_in_token_tree(child, src, &mut out, 0);
            }
        }
        out
    }
}

/// Whether a name inside a macro's token tree is a type or a variant rather
/// than a function.
///
/// A token tree is unparsed, so a tuple-struct *pattern* and a call are the
/// same three tokens: `matches!(e, Kind::Io(_))` looks exactly like
/// `wrap(Kind::Io(_))`. Rust settles it by convention and by the
/// `non_snake_case` lint — a function is `snake_case`, a struct or variant is
/// `CamelCase` — so a leading uppercase letter means this is not a call, and
/// `Io`, `Some` and `Str` stop being recorded as ones.
///
/// The cost is a tuple-struct constructor written inside a macro:
/// `vec![Foo(1)]` records no call where `let x = Foo(1);` does, because outside
/// a macro the grammar says which of the two it is and here nothing does. A
/// constructor is a thin edge to lose; a pattern is a wrong one to keep.
fn names_a_type(name: &str) -> bool {
    name.chars().next().is_some_and(char::is_uppercase)
}

/// Collect `name(` shapes inside a macro's token tree.
///
/// A call in an unparsed token tree is an `identifier` immediately followed by
/// a `token_tree` — no whitespace between them, which is what separates
/// `sanitize(x)` from `match x { … }`-style token runs. The identifier is the
/// last segment of the path, since `::` and the segments before it are separate
/// tokens; that is exactly the form [`crate::resolve_call`] resolves anyway.
/// Nested token trees are followed, so a call inside a call's arguments counts.
///
/// The token before the name says how the call was written. A `.` there is a
/// receiver — `format!("{}", s.trim())` — and the call is reported as a method
/// call, which [`crate::resolve_call`] never resolves on repository-wide
/// uniqueness. `::` or anything else is a path or a bare name.
fn calls_in_token_tree<'t>(
    node: Node<'t>,
    src: &str,
    out: &mut Vec<(String, Node<'t>, bool)>,
    depth: u32,
) {
    if depth > MAX_TOKEN_TREE_DEPTH {
        return;
    }
    let mut cursor = node.walk();
    let children: Vec<Node<'t>> = node.children(&mut cursor).collect();
    for (at, child) in children.iter().enumerate() {
        if child.kind() == "token_tree" {
            // A `(` opens an argument list only when it follows the name with
            // nothing in between; anywhere else it is a grouping or a tuple.
            if let Some(prev) = at.checked_sub(1).map(|i| children[i]) {
                let name = text(prev, src);
                if prev.kind() == "identifier"
                    && prev.end_byte() == child.start_byte()
                    && src.as_bytes().get(child.start_byte()) == Some(&b'(')
                    && !names_a_type(name)
                {
                    let method = at.checked_sub(2).is_some_and(|i| children[i].kind() == ".");
                    out.push((name.to_string(), prev, method));
                }
            }
            calls_in_token_tree(*child, src, out, depth + 1);
        }
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

    let mut segments: Vec<&str> = raw
        .trim_start_matches("::")
        .split("::")
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "*")
        .collect();
    // A trailing `self` (`use a::b::{self}`) names the module the preceding
    // segments already name. A *leading* `self` is meaningful and kept.
    while segments.len() > 1 && segments.last() == Some(&"self") {
        segments.pop();
    }
    let Some((first, rest)) = segments.split_first() else {
        return Vec::new();
    };

    match *first {
        "crate" => match crate_root(from, known) {
            Some(root) => under(&join(&root, "src"), rest, known),
            None => Vec::new(),
        },
        "self" => under(&module_dir(from), rest, known),
        "super" => {
            let mut base = module_dir(from);
            let mut rest = segments.as_slice();
            while rest.first() == Some(&"super") {
                base = parent_dir(&base).to_string();
                rest = &rest[1..];
            }
            under(&base, rest, known)
        }
        _ => package_path(from, first, rest, known),
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

/// A path into a sibling package: the module the remaining segments name,
/// falling back to the package's `src/lib.rs`.
///
/// `beta_core::net::Client` reaches `beta-core/src/net.rs` the same way
/// `crate::net::Client` reaches `src/net.rs` inside the importing package —
/// [`under`] tries the longest module prefix first, so the trailing item name
/// costs nothing. Only when no module answers does the import fall back to the
/// package root, which is the right answer for `beta_core::Client`, an item
/// re-exported from `lib.rs`.
///
/// Naming the defining module rather than the package root is what lets a call
/// be resolved across crates: the importer's `imports` then contains the file
/// the callee is actually defined in.
fn package_path(
    from: &str,
    name: &str,
    rest: &[&str],
    known: &dyn Fn(&str) -> bool,
) -> Vec<String> {
    for root in package_roots(from, name, known) {
        let src = join(&root, "src");
        let inner = under(&src, rest, known);
        if !inner.is_empty() {
            return inner;
        }
        let lib = join(&src, "lib.rs");
        if known(&lib) {
            return vec![lib];
        }
    }
    Vec::new()
}

/// Every directory that could be the package `name`, nearest first.
///
/// A candidate directory counts as a package only when it has its own
/// `Cargo.toml`, which is what makes this a package lookup rather than a guess
/// at a directory name. No layout convention is assumed: the directories
/// searched are the crate root's parent and every ancestor of the importing
/// file, so a workspace that keeps its members under `crates/`, `libs/` or
/// directly at the root all work the same way. Package directories are
/// conventionally named after the package, so `beta_core` also tries
/// `beta-core`.
fn package_roots(from: &str, name: &str, known: &dyn Fn(&str) -> bool) -> Vec<String> {
    let mut parents: Vec<String> = Vec::new();
    if let Some(root) = crate_root(from, known) {
        parents.push(parent_dir(&root).to_string());
    }
    parents.extend(ancestors(parent_dir(from)));
    // `dedup` only removes neighbours, and these two sources overlap out of
    // order, so drop repeats by hand.
    let mut seen: Vec<String> = Vec::new();
    parents.retain(|parent| {
        let fresh = !seen.contains(parent);
        if fresh {
            seen.push(parent.clone());
        }
        fresh
    });

    let variants = [name.to_string(), name.replace('_', "-")];
    let mut roots = Vec::new();
    for parent in &parents {
        for variant in &variants {
            let dir = join(parent, variant);
            if known(&join(&dir, "Cargo.toml")) {
                roots.push(dir);
            }
        }
    }
    roots
}
