//! Deterministic structure extraction for source files.
//!
//! Bytes in, facts out. This crate never opens a file, never walks a
//! directory and never touches a database: everything it knows about the
//! surrounding tree arrives through the caller's closures. That is what makes
//! the resulting graph reproducible — two runs over the same working tree
//! produce byte-identical facts.
//!
//! # What comes out
//!
//! [`extract`] turns one file's bytes into a [`FileFacts`]: a content hash, a
//! line count, the symbols defined in the file (qualified in-file, with their
//! doc line, signature and outgoing calls), the imports as written, and — for
//! Markdown — headings, mentions and the body text.
//!
//! # Resolving what came out
//!
//! The extraction step deliberately keeps raw text. [`resolve_import`],
//! [`resolve_mention`] and [`resolve_call`] turn that raw text into paths and
//! symbol keys, using caller-supplied lookups:
//!
//! * `known(path)` — true when `path` names a file in the working tree.
//! * `files_in(dir)` — the working-tree paths of the files directly inside
//!   `dir`, empty when the directory does not exist.
//! * `by_basename(name)` — every working-tree path whose file name is `name`.
//!
//! All paths, in and out, are working-tree-relative files that use `/`
//! separators. No result is ever a directory.
//!
//! [`SymbolIndex`] keys are `<file path>#<qualified symbol name>`; the caller
//! builds the index in that shape so [`resolve_call`] can prefer a definition
//! in the calling file or its directory. [`resolve_call`] also takes the
//! calling file's resolved imports, which is what lets a call reach a
//! definition in another crate whose name is not unique repository-wide — and
//! is the only thing that can reach one at all for a call written on a
//! receiver, which never falls back to repository-wide uniqueness.
//!
//! A method is stored qualified — `Store.flush` — and written on a receiver —
//! `store.flush()` — so the index files it under the bare name too. Those
//! entries are read only where the receiver identifies the type, because
//! stripping a receiver says which method is wanted and nothing about what it
//! belongs to: see [`resolve_call`] and [`mentioned_types`].
//!
//! # Determinism
//!
//! Every returned collection is sorted and deduplicated, and nothing depends
//! on hash-map iteration order. Output is capped so a pathological file cannot
//! blow up the store: see [`MAX_FILE_BYTES`], [`MAX_BODY_BYTES`],
//! [`MAX_TEXT_CHARS`] and [`MAX_CALLS`].

mod docs;
mod hash;
mod lang;

pub use docs::resolve_mention;

use std::collections::{BTreeMap, BTreeSet};

/// Files larger than this are reduced to hash, language and line count.
pub const MAX_FILE_BYTES: usize = 1024 * 1024;
/// Upper bound on the stored Markdown body, in bytes.
pub const MAX_BODY_BYTES: usize = 65_536;
/// Upper bound, in characters, on a symbol signature or doc line.
pub const MAX_TEXT_CHARS: usize = 200;
/// Upper bound on the calls recorded for one symbol.
pub const MAX_CALLS: usize = 256;
/// Wall-clock budget for parsing one file.
pub const PARSE_BUDGET_MS: u64 = 500;

/// How many leading bytes are inspected when deciding whether a file is binary.
const BINARY_PROBE_BYTES: usize = 8 * 1024;

/// The languages this crate can read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Lang {
    Rust,
    Python,
    TypeScript,
    Tsx,
    JavaScript,
    Go,
    Markdown,
    Other,
}

impl Lang {
    /// A stable lowercase name, suitable for storing as a node property.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Lang::Rust => "rust",
            Lang::Python => "python",
            Lang::TypeScript => "typescript",
            Lang::Tsx => "tsx",
            Lang::JavaScript => "javascript",
            Lang::Go => "go",
            Lang::Markdown => "markdown",
            Lang::Other => "other",
        }
    }
}

/// Classify a path by its extension. Unknown extensions are [`Lang::Other`],
/// which yields hash-only facts.
#[must_use]
pub fn lang_of(path: &str) -> Lang {
    let name = file_name(path);
    let ext = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => ext,
        _ => return Lang::Other,
    };
    let lower = ext.to_ascii_lowercase();
    match lower.as_str() {
        "rs" => Lang::Rust,
        "py" | "pyi" => Lang::Python,
        "ts" | "mts" | "cts" => Lang::TypeScript,
        "tsx" => Lang::Tsx,
        "js" | "mjs" | "cjs" | "jsx" => Lang::JavaScript,
        "go" => Lang::Go,
        "md" | "markdown" => Lang::Markdown,
        _ => Lang::Other,
    }
}

/// One definition found in a file.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SymbolFact {
    /// Qualified within the file: `Type.method`, `mod::fn`, `Receiver.method`.
    pub name: String,
    /// One of `function`, `method`, `class`, `struct`, `enum`, `trait`,
    /// `interface`, `type`, `const`, `module`.
    pub kind: &'static str,
    /// 1-based first line of the definition.
    pub line_start: u32,
    /// 1-based last line of the definition.
    pub line_end: u32,
    /// The declaration line, whitespace-collapsed, at most
    /// [`MAX_TEXT_CHARS`] characters.
    pub signature: String,
    /// The first line of the doc comment, at most [`MAX_TEXT_CHARS`]
    /// characters. Empty when the definition is undocumented.
    pub doc: String,
    /// One entry per call site. Sorted, deduplicated, at most [`MAX_CALLS`]
    /// entries.
    pub calls: Vec<CallFact>,
}

/// One call as written, and enough about how it was written to resolve it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CallFact {
    /// The callee as written: `flush`, `Store::flush`, `self.flush`.
    pub callee: String,
    /// 1-based line of the call site.
    pub line: u32,
    /// The call was written in *method* position — a receiver, a dot, a name.
    /// See [`written_as_method`].
    pub method: bool,
}

impl CallFact {
    /// A call written as a bare name or a path: `flush`, `a::b::flush`.
    #[must_use]
    pub fn plain(callee: &str, line: u32) -> Self {
        CallFact {
            callee: callee.to_string(),
            line,
            method: false,
        }
    }

    /// A call written on a receiver: `self.flush`, `store.flush`.
    #[must_use]
    pub fn method(callee: &str, line: u32) -> Self {
        CallFact {
            callee: callee.to_string(),
            line,
            method: true,
        }
    }
}

/// Whether a callee as written names a receiver rather than a path.
///
/// Every language here reports the callee as the source wrote it, and in all of
/// them a `.` separates a value or a module from the name being called —
/// `self.flush`, `store.flush`, `os.path.join`, `pkg.Func`. A path uses `::`
/// (Rust) or nothing at all, neither of which contains a dot. So one rule
/// covers every grammar, and it is the same rule
/// [`resolve_call`]'s candidate splitting already uses to find the last segment.
#[must_use]
pub fn written_as_method(callee: &str) -> bool {
    callee.contains('.')
}

/// One import as written in the source.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ImportFact {
    /// The module path or specifier as written, normalised per language:
    /// a Rust `use` path with any group expanded (`a::b::c`) or a module
    /// declaration (`mod x`); a Python dotted module (`a.b`, `.sibling`); a
    /// TypeScript or JavaScript specifier (`./util`); a Go import path.
    pub raw: String,
    /// 1-based line of the import.
    pub line: u32,
}

/// Everything one file contributes to the graph.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileFacts {
    pub lang: Lang,
    /// First 16 bytes of the BLAKE3 digest, as 32 hex characters.
    pub hash: String,
    pub lines: u32,
    /// Sorted by `(line_start, name)`.
    pub symbols: Vec<SymbolFact>,
    /// Sorted by `(line, raw)`, deduplicated.
    pub imports: Vec<ImportFact>,
    /// Markdown headings in document order.
    pub headings: Vec<String>,
    /// Markdown mention tokens as written. Sorted, deduplicated.
    pub mentions: Vec<String>,
    /// Markdown body text, at most [`MAX_BODY_BYTES`] bytes, cut on a
    /// character boundary. `None` for every other language.
    pub body: Option<String>,
}

impl FileFacts {
    fn bare(lang: Lang, hash: String, lines: u32) -> Self {
        FileFacts {
            lang,
            hash,
            lines,
            symbols: Vec::new(),
            imports: Vec::new(),
            headings: Vec::new(),
            mentions: Vec::new(),
            body: None,
        }
    }
}

/// Extract the facts for one file.
///
/// Never panics and never fails: anything it cannot read — a binary file, a
/// file over [`MAX_FILE_BYTES`], an unknown extension, a parse that overruns
/// [`PARSE_BUDGET_MS`] — degrades to hash, language and line count.
#[must_use]
pub fn extract(path: &str, bytes: &[u8]) -> FileFacts {
    let lang = lang_of(path);
    let mut facts = FileFacts::bare(lang, hash::hex32(bytes), hash::count_lines(bytes));
    if bytes.len() > MAX_FILE_BYTES || lang == Lang::Other {
        return facts;
    }
    let Some(text) = decode(bytes) else {
        return facts;
    };
    if lang == Lang::Markdown {
        docs::extract(&mut facts, text);
    } else if let Some((symbols, imports)) = lang::extract(lang, text) {
        facts.symbols = symbols;
        facts.imports = imports;
    }
    facts
}

/// Decode as UTF-8, rejecting anything that looks binary.
fn decode(bytes: &[u8]) -> Option<&str> {
    let probe = &bytes[..bytes.len().min(BINARY_PROBE_BYTES)];
    if probe.contains(&0) {
        return None;
    }
    std::str::from_utf8(bytes).ok()
}

/// Resolve one import to the working-tree paths it names.
///
/// Returns an empty vector for anything outside the working tree — the
/// standard library, a registry dependency, a bare npm specifier. The result
/// is sorted and deduplicated.
///
/// # Rules
///
/// **Rust.** `mod x` resolves against the declaring file's module directory
/// (`lib.rs`, `main.rs` and `mod.rs` own their own directory; every other
/// file owns a directory named after its stem) to `<dir>/x.rs` or
/// `<dir>/x/mod.rs`. `crate::a::b` resolves under the nearest ancestor
/// directory that has both a `Cargo.toml` and a `src/`, trying the longest
/// module prefix first: `src/a/b.rs`, `src/a/b/mod.rs`, `src/a.rs`,
/// `src/a/mod.rs`. `self::` resolves under the declaring file's module
/// directory and `super::` one directory further up per `super`. A leading
/// segment that names a sibling directory holding its own `Cargo.toml` (with
/// `_` and `-` treated as interchangeable) resolves to that package's
/// `src/lib.rs`; the directories searched are the crate root's parent and
/// every ancestor of the declaring file, so no layout convention is assumed.
///
/// **Python.** `a.b` and `.b` resolve to `<dir>/a/b.py` or
/// `<dir>/a/b/__init__.py`, first relative to the importing file's directory
/// and then relative to the working-tree root. Leading dots walk upwards.
/// Modules whose first segment is in the standard library are skipped.
///
/// **TypeScript and JavaScript.** Only relative specifiers resolve. The
/// specifier is tried as written, then with each source extension appended,
/// then as a directory with an `index.*`. A specifier ending in `.js` also
/// tries `.ts` and `.tsx`, which is how TypeScript sources refer to their own
/// compiled output.
///
/// **Go.** Imports name a package, not a file, so the result is every
/// non-test `.go` file directly inside the matching package directory —
/// `_test.go` files are excluded, since a test file is never what an import
/// reaches. The longest suffix of the import path that names a directory
/// holding Go sources wins, which strips the module prefix without needing to
/// read `go.mod`. This is the one rule that needs `files_in`; every other
/// language resolves through `known` alone.
///
/// **Markdown.** Markdown has no imports; use [`resolve_mention`].
#[must_use]
pub fn resolve_import(
    lang: Lang,
    from_path: &str,
    raw: &str,
    known: &dyn Fn(&str) -> bool,
    files_in: &dyn Fn(&str) -> Vec<String>,
) -> Vec<String> {
    let from = normalize(from_path);
    let raw = raw.trim();
    let mut out = match lang {
        Lang::Rust => lang::rust::resolve_import(&from, raw, known),
        Lang::Python => lang::python::resolve_import(&from, raw, known),
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => {
            lang::typescript::resolve_import(&from, raw, known)
        }
        Lang::Go => lang::go::resolve_import(&from, raw, files_in),
        Lang::Markdown | Lang::Other => Vec::new(),
    };
    out.sort();
    out.dedup();
    out
}

/// Symbol keys grouped by symbol name, built by the caller.
///
/// A key is `<file path>#<qualified symbol name>` — everything before the
/// last `#` is taken as the defining file. [`resolve_call`] uses that to
/// prefer nearby definitions, so a caller that builds keys in some other
/// shape loses the same-file and same-directory tiers and falls back to
/// repo-wide uniqueness.
#[derive(Clone, Debug, Default)]
pub struct SymbolIndex {
    by_name: BTreeMap<String, Vec<String>>,
    by_method: BTreeMap<String, Vec<String>>,
}

impl SymbolIndex {
    #[must_use]
    pub fn new() -> Self {
        SymbolIndex::default()
    }

    /// Record that `name` is defined by the symbol at `key`. Inserting the
    /// same pair twice is a no-op, and the stored keys stay sorted, so the
    /// index does not depend on insertion order.
    ///
    /// A method — a name qualified `Type.method` — is filed twice: under the
    /// name as written, and under the bare `method`. Source almost never
    /// writes the qualified form. `store.flush()` says `flush` after the
    /// receiver is stripped, and without the second entry no call on a
    /// receiver could ever reach a method definition. The two sets are kept
    /// apart because they carry different weight: see [`resolve_call`], which
    /// consults the bare entries only where the tier itself is evidence.
    pub fn insert(&mut self, name: &str, key: &str) {
        insert_sorted(self.by_name.entry(name.to_string()).or_default(), key);
        if let Some(bare) = method_name(name) {
            insert_sorted(self.by_method.entry(bare.to_string()).or_default(), key);
        }
    }

    /// Number of distinct names in the index.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    fn keys_for(&self, name: &str) -> &[String] {
        self.by_name.get(name).map_or(&[], Vec::as_slice)
    }

    /// Keys of the methods whose bare name is `name`, whatever type they
    /// belong to.
    fn methods_for(&self, name: &str) -> &[String] {
        self.by_method.get(name).map_or(&[], Vec::as_slice)
    }
}

fn insert_sorted(slot: &mut Vec<String>, key: &str) {
    if let Err(at) = slot.binary_search_by(|held| held.as_str().cmp(key)) {
        slot.insert(at, key.to_string());
    }
}

/// The bare name of a qualified method: `flush` in `Store.flush`, `None` for a
/// name that is not written as a method.
fn method_name(name: &str) -> Option<&str> {
    let (_, bare) = name.rsplit_once('.')?;
    (!bare.is_empty()).then_some(bare)
}

/// The type a method belongs to: `Store` in `Store.flush`, and in
/// `mod::Store.flush`.
fn receiver_type(name: &str) -> Option<&str> {
    let (owner, _) = name.rsplit_once('.')?;
    let owner = owner.rsplit(['.', ':']).next().unwrap_or(owner);
    (!owner.is_empty()).then_some(owner)
}

/// What a call was written on: `store` in `store.flush`, and `symbols` — not
/// `self` — in `self.symbols.len`. The segment immediately before the method
/// is the thing whose method is being called.
fn receiver_of(callee: &str) -> Option<&str> {
    receiver_type(callee.trim())
}

/// Whether the receiver a call was written on identifies `symbol`'s type.
///
/// This is the guard that keeps the bare-name entries from turning every
/// `.len()` in the repository into an edge. Stripping the receiver tells you
/// *which* method is wanted and nothing about *what* it belongs to, and on a
/// real codebase the reachable `Type.len` is almost never the `Vec` the source
/// meant. Two receivers say what the type is:
///
/// * `self` — and `this`, `cls`, `Self` — is the type being implemented, so a
///   candidate in the calling file is it. That is where `impl` blocks put
///   their methods, and it is the case that matters: `self.flush()` is how
///   most method calls inside a type are written.
/// * a variable named after its type — `store: Store`, `symbol_index:
///   SymbolIndex` — which every language here spells consistently enough to be
///   evidence once case and `_` are collapsed.
///
/// Anything else is a variable whose type the graph does not know, and the
/// truthful answer for it is no edge. That costs real calls — `rs.len()` on a
/// `ResultSet` gets nothing — and the trade is deliberate: this resolver's
/// whole design is that a wrong edge is worse than a missing one.
fn receiver_names(receiver: &str, symbol: &str, in_calling_file: bool) -> bool {
    if matches!(receiver, "self" | "Self" | "this" | "cls" | "me") {
        return in_calling_file;
    }
    receiver_type(symbol).is_some_and(|ty| squash(receiver) == squash(ty))
}

/// Lowercased with `_` dropped, so `symbol_index` and `SymbolIndex` meet.
fn squash(name: &str) -> String {
    name.chars()
        .filter(|c| *c != '_')
        .flat_map(char::to_lowercase)
        .collect()
}

/// Every name [`SymbolIndex`] files a definition called `name` under.
///
/// The mirror of [`call_lookup_names`], and the other half of what a caller
/// building a *narrowed* index needs: a `Store.flush` has to be kept when
/// something looks up the bare `flush`, or a method call loses the very
/// definition the bare entry exists to reach.
#[must_use]
pub fn indexed_under(name: &str) -> Vec<String> {
    let mut out = vec![name.to_string()];
    if let Some(bare) = method_name(name) {
        if bare != name {
            out.push(bare.to_string());
        }
    }
    out
}

/// What the calling file can see, beyond the symbol index itself.
///
/// Both halves are built by the caller from the working tree it already walked,
/// so [`resolve_call`] stays a pure function of its inputs.
#[derive(Clone, Copy, Debug)]
pub struct CallScope<'a> {
    /// The calling file's resolved import targets, as [`resolve_import`]
    /// returned them and the `File` node stores them.
    pub imports: &'a [String],
    /// Every name a path call may lead with: package, directory and module
    /// names the tree holds, each also under the `-`/`_` spelling a package
    /// path uses. A leading segment that is not in here and is not a symbol
    /// names a dependency, and a call into a dependency resolves to nothing.
    pub roots: &'a BTreeSet<String>,
    /// The type names the calling file's own source writes, as
    /// [`mentioned_types`] reads them. Used only to choose between method
    /// definitions a tier has already reached — never to reach one.
    pub types: &'a BTreeSet<String>,
}

/// Resolve a callee written in `from_file` to the key of the symbol it names.
///
/// The callee is tried as written and then as its last segment, so
/// `self.flush`, `this.flush` and `Store::flush` all reach `flush`. Within
/// each attempt the search narrows outwards, and the first tier that yields
/// exactly one definition wins:
///
/// 1. a definition in the same file;
/// 2. a definition in the same directory;
/// 3. a definition in a file `from_file` imports;
/// 4. a single definition anywhere in the tree.
///
/// Anything still ambiguous resolves to `None` — a wrong edge is worse than a
/// missing one. The two tiers that search by name rather than by a stated
/// relationship — the directory and the whole tree — also require the
/// definition to be in the calling file's language: see [`same_language`].
///
/// # Why imports matter
///
/// Tier 4 can only answer when the name is unique across the whole repository,
/// so two crates that each define a `render` left every call to either one
/// unresolved, and a call that crossed a crate boundary was the common case
/// for that. `imports` is the caller file's already-resolved import targets —
/// the same list [`resolve_import`] produced and the `File` node stores — and a
/// definition living in one of them is the one the source actually named. It
/// sits below the directory tiers because a file that both imports a module and
/// defines the name itself means the local one.
///
/// # Why a method call stops at tier 3
///
/// Tier 4 is uniqueness, not reachability: it claims any name defined exactly
/// once anywhere in the tree, whether or not the calling file could name it.
/// For a bare or path call that is a fair guess, because the source wrote a
/// name it expected to be in scope. For a call written on a receiver it is not:
/// `.collect()`, `.take()`, `.get()` and `.ok()` belong to types the graph has
/// never seen, and the tier happily bound them to whatever single test helper
/// happened to share the name.
///
/// So a [`CallFact::method`] call reaches tier 4 only on the callee **as
/// written**, never on the bare last segment it falls back to. `Store.flush`
/// matching a symbol qualified `Store.flush` names the receiver's type, which
/// is how a static call reads in Python, TypeScript and JavaScript, and is
/// evidence rather than a guess. `v.collect` falling back to `collect` is the
/// guess, and it gets no edge — which is the truthful answer for a call into a
/// dependency the graph does not hold. Tier 3 is narrowed the same way: an
/// imported match for a method call must itself be a method.
///
/// # Calls into a dependency
///
/// Before any of that, a path call whose leading segment names nothing in the
/// tree resolves to nothing at all — see [`path_reaches_the_tree`].
/// `std::mem::take` is not a call to whatever single `take` this repository
/// defines.
#[must_use]
pub fn resolve_call(
    from_file: &str,
    call: &CallFact,
    index: &SymbolIndex,
    scope: &CallScope<'_>,
) -> Option<String> {
    if !path_reaches_the_tree(&call.callee, index, scope.roots) {
        return None;
    }
    let from = normalize(from_file);
    let from_dir = parent_dir(&from);
    for name in callee_candidates(&call.callee) {
        // A method call reaches the repository-wide tier only on a candidate
        // that still carries its receiver. `Store.flush` matching a symbol
        // qualified `Store.flush` names the receiver's type and is evidence;
        // the bare `flush` it falls back to is a guess, and that guess is what
        // bound `.collect()` to an unrelated helper. Testing the candidate
        // rather than its position matters because a call recovered from a
        // macro's token tree arrives as the bare segment already.
        let repo_wide = !call.method || written_as_method(&name);
        let keys = index.keys_for(&name);
        // The bare-name entries of the index, consulted only for the exact
        // case they exist for: a call written on a receiver, fallen back to
        // the bare segment. `store.flush()` reaches `Store.flush` this way and
        // no other, because no source ever writes the qualified form.
        let methods: &[String] = match call.method && !written_as_method(&name) {
            true => index.methods_for(&name),
            false => &[],
        };
        if keys.is_empty() && methods.is_empty() {
            continue;
        }
        let pick = |filter: &dyn Fn(&str, &str) -> bool| -> Option<String> {
            let mut hit = None;
            for key in keys {
                if filter(key_file(key), key_symbol(key)) {
                    if hit.is_some() {
                        return None;
                    }
                    hit = Some(key.clone());
                }
            }
            hit
        };
        // The same, over the bare-name entries, with two differences. First,
        // the receiver has to identify the type — see `receiver_names`. Second,
        // several types in one file or directory can still define the same
        // method name, and the caller's own source breaks that tie: a file
        // that writes `Store` is the one calling `Store.flush`, and a file
        // that never writes the name gets no edge rather than a coin toss.
        let receiver = receiver_of(&call.callee);
        let pick_method = |filter: &dyn Fn(&str) -> bool| -> Option<String> {
            let mut hits: Vec<&String> = methods
                .iter()
                .filter(|key| {
                    filter(key_file(key))
                        && receiver.is_some_and(|r| {
                            receiver_names(r, key_symbol(key), key_file(key) == from)
                        })
                })
                .collect();
            if hits.len() > 1 {
                hits.retain(|key| {
                    receiver_type(key_symbol(key)).is_some_and(|t| scope.types.contains(t))
                });
            }
            match hits.as_slice() {
                [one] => Some((*one).clone()),
                _ => None,
            }
        };
        let same_dir = |file: &str| parent_dir(file) == from_dir && same_language(&from, file);
        let imported = |file: &str| scope.imports.iter().any(|i| i == file);

        if let Some(key) = pick(&|file, _| file == from).or_else(|| pick_method(&|f| f == from)) {
            return Some(key);
        }
        if let Some(key) = pick(&|file, _| same_dir(file)).or_else(|| pick_method(&same_dir)) {
            return Some(key);
        }
        // An import brings a *file* into scope, not a type. For a method call
        // that is not enough: the receiver's type is unknown, and a free
        // function in an imported file that happens to share the method's name
        // is not the thing being called. `repo.join(id)` is `Path::join`, but
        // the calling file imports a module that defines a `join`. So an
        // imported match has to be a method — a symbol qualified `Type.name`,
        // which is how every extractor here names one. The bare-name entries
        // are all methods by construction, and are held to the calling file's
        // language as well, which the exact-name imports tier does not need to
        // be: an import is a stated relationship, a bare method name is not.
        if let Some(key) =
            pick(&|file, symbol| imported(file) && (!call.method || symbol.contains('.')))
                .or_else(|| pick_method(&|f| imported(f) && same_language(&from, f)))
        {
            return Some(key);
        }
        if repo_wide {
            if let Some(key) = pick(&|file, _| same_language(&from, file)) {
                return Some(key);
            }
        }
    }
    None
}

/// The type names a file's own source writes.
///
/// A call written on a receiver says which method is wanted but not what it
/// belongs to. When one file or directory holds two types that both define
/// `flush`, this is the tie-break [`resolve_call`] uses: a file that writes
/// `Store` somewhere is the one calling `Store.flush`, and a file that never
/// writes the name is not.
///
/// Four spellings count, and they are the ones a type appears in across every
/// language here: `Store::…`, `Store {`, `: Store` and `impl Store`. Only
/// names that begin with an uppercase letter are collected — every grammar
/// here spells its types that way, and the alternative is to treat `if x {` as
/// naming a type `x`.
///
/// Deliberately textual, deliberately cheap, and deliberately unable to *make*
/// a resolution: it only chooses between candidates a tier has already
/// reached, so a name it misses costs an edge and never invents one. Binary
/// input and anything over [`MAX_FILE_BYTES`] yield nothing.
#[must_use]
pub fn mentioned_types(bytes: &[u8]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    if bytes.len() > MAX_FILE_BYTES {
        return out;
    }
    let Some(text) = decode(bytes) else {
        return out;
    };
    let b = text.as_bytes();
    let mut at = 0;
    while at < b.len() {
        if !is_ident_start(b[at]) {
            at += 1;
            continue;
        }
        let start = at;
        while at < b.len() && is_ident_byte(b[at]) {
            at += 1;
        }
        if out.len() >= MAX_MENTIONED_TYPES || !b[start].is_ascii_uppercase() {
            continue;
        }
        if leads_a_path(b, at) || opens_a_literal(b, at) || is_annotated(b, start) {
            out.insert(text[start..at].to_string());
        }
    }
    out
}

/// Most type names kept for one file. A generated file can name thousands;
/// past this the tie-break simply has less to work with, which costs an edge
/// rather than inventing one.
const MAX_MENTIONED_TYPES: usize = 1_024;

const fn is_ident_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

const fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// `Store::new` — the identifier ending at `at` is followed by a path
/// separator.
fn leads_a_path(b: &[u8], at: usize) -> bool {
    b.get(at) == Some(&b':') && b.get(at + 1) == Some(&b':')
}

/// `Store {` — the identifier ending at `at` opens a braced literal or body.
fn opens_a_literal(b: &[u8], at: usize) -> bool {
    let mut i = at;
    while b.get(i) == Some(&b' ') {
        i += 1;
    }
    b.get(i) == Some(&b'{')
}

/// `: Store` or `impl Store` — the identifier starting at `start` is preceded
/// by a type annotation or an impl header.
fn is_annotated(b: &[u8], start: usize) -> bool {
    let mut i = start;
    loop {
        let was = i;
        // The sigils an annotation can put between the `:` and the name:
        // `&Path`, `&mut Store`, `Vec<Store>`, `*const Frame`.
        while i > 0 && matches!(b[i - 1], b' ' | b'\t' | b'&' | b'*' | b'<') {
            i -= 1;
        }
        for word in [b"mut".as_slice(), b"dyn".as_slice(), b"const".as_slice()] {
            if i >= word.len()
                && &b[i - word.len()..i] == word
                && (i == word.len() || !is_ident_byte(b[i - word.len() - 1]))
            {
                i -= word.len();
            }
        }
        if i == was {
            break;
        }
    }
    if i > 0 && b[i - 1] == b':' {
        return true;
    }
    // `impl` must be a word of its own, not the tail of an identifier.
    i >= 4 && &b[i - 4..i] == b"impl" && (i == 4 || !is_ident_byte(b[i - 5]))
}

/// Whether two files are written in the same language.
///
/// A call site and a definition that share nothing but a name are not the same
/// function. `str(x)` in Python is a builtin; that this repository also holds a
/// Rust `fn str` is a coincidence, and the tiers that search by name alone —
/// the same directory, and the whole tree — happily turned it into an edge from
/// a `.py` file to a `.rs` one.
///
/// TypeScript, TSX and JavaScript are one family, because a `.ts` module really
/// does call into a `.tsx` one and neither is a different language for this
/// purpose. Every other pairing must match exactly. Two files of unknown type
/// count as the same language and contribute no symbols either way.
fn same_language(a: &str, b: &str) -> bool {
    family(lang_of(a)) == family(lang_of(b))
}

/// The language a file resolves calls as, collapsing the ECMAScript variants.
const fn family(lang: Lang) -> Lang {
    match lang {
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => Lang::TypeScript,
        other => other,
    }
}

/// Whether a `::`-separated path call names anything the working tree holds.
///
/// `std::mem::take`, `serde_json::from_str` and `Vec::new` are calls into
/// dependencies. The graph has no node for any of them, and the tiers below
/// would happily bind the last segment to whatever single `take`, `from_str` or
/// `new` the repository happened to define — on this repository that was mostly
/// test helpers. A path is worth resolving only when its first segment names
/// something here:
///
/// * `crate`, `self`, `super` and `Self`, which are always the current tree;
/// * a package, directory or module name the caller listed in `roots`;
/// * a symbol — `Store::flush` leads with a type, not a module.
///
/// A callee with no `::` is not a path and passes untouched, which leaves bare
/// calls and every dotted form to the tiers and the method rule.
fn path_reaches_the_tree(callee: &str, index: &SymbolIndex, roots: &BTreeSet<String>) -> bool {
    let Some((first, _)) = callee.split_once("::") else {
        return true;
    };
    let first = first.trim();
    matches!(first, "crate" | "self" | "super" | "Self")
        || roots.contains(first)
        || !index.keys_for(first).is_empty()
}

/// Every name [`resolve_call`] may look `callee` up under in the index.
///
/// The resolver reaches a [`SymbolIndex`] by name and by nothing else: it tries
/// the callee as written, then its last segment, and before either it asks
/// whether a `::` path's leading segment names a symbol here
/// ([`path_reaches_the_tree`]). Those are the only three forms.
///
/// That is what lets a caller build a *narrowed* index — one holding only the
/// definitions a particular pass could need — without changing a single edge.
/// An index holding every definition of exactly these names answers every
/// lookup a whole-tree index would, tier for tier, the repository-wide tier
/// included: that tier turns on a name being defined exactly once, and the
/// narrowed index still holds every definition of the names it is asked about.
/// Ask for less than this and resolution silently changes.
#[must_use]
pub fn call_lookup_names(callee: &str) -> Vec<String> {
    let mut out = callee_candidates(callee);
    if let Some((first, _)) = callee.trim().split_once("::") {
        let first = first.trim().to_string();
        if !first.is_empty() && !out.contains(&first) {
            out.push(first);
        }
    }
    out
}

/// The callee as written, then its last `.`- or `::`-separated segment.
fn callee_candidates(callee: &str) -> Vec<String> {
    let callee = callee.trim();
    let mut out = vec![callee.to_string()];
    let after_dot = callee.rfind('.').map(|at| at + 1);
    let after_colon = callee.rfind("::").map(|at| at + 2);
    if let Some(cut) = after_dot.into_iter().chain(after_colon).max() {
        if let Some(tail) = callee.get(cut..) {
            if !tail.is_empty() && tail != callee {
                out.push(tail.to_string());
            }
        }
    }
    out
}

/// The file half of a symbol key.
fn key_file(key: &str) -> &str {
    key.rsplit_once('#').map_or(key, |(file, _)| file)
}

/// The symbol half of a symbol key: the name, qualified within its file.
fn key_symbol(key: &str) -> &str {
    key.rsplit_once('#').map_or("", |(_, name)| name)
}

// ── path helpers ────────────────────────────────────────────────────────────
// Working-tree-relative, `/`-separated, no symlink or filesystem access.

/// Everything before the last `/`, or `""` for a top-level path.
pub(crate) fn parent_dir(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// Everything after the last `/`.
pub(crate) fn file_name(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}

/// The file name without its final extension.
pub(crate) fn file_stem(path: &str) -> &str {
    let name = file_name(path);
    match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => name,
    }
}

/// Collapse `.`, `..` and empty segments. Leading `..` segments survive,
/// which keeps a path that escapes the working tree from silently becoming a
/// path inside it.
pub(crate) fn normalize(path: &str) -> String {
    let mut stack: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => match stack.last() {
                Some(&last) if last != ".." => {
                    stack.pop();
                }
                _ => stack.push(".."),
            },
            other => stack.push(other),
        }
    }
    stack.join("/")
}

/// Join `rest` onto directory `base` and normalise.
pub(crate) fn join(base: &str, rest: &str) -> String {
    if base.is_empty() {
        normalize(rest)
    } else {
        normalize(&format!("{base}/{rest}"))
    }
}

/// `dir` and every ancestor of it, longest first, ending with `""`.
pub(crate) fn ancestors(dir: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = dir;
    loop {
        out.push(cur.to_string());
        if cur.is_empty() {
            break;
        }
        cur = parent_dir(cur);
    }
    out
}

/// The first candidate `known` accepts, as a one-element vector.
pub(crate) fn first_known(candidates: &[String], known: &dyn Fn(&str) -> bool) -> Vec<String> {
    for cand in candidates {
        if known(cand) {
            return vec![cand.clone()];
        }
    }
    Vec::new()
}

// ── text helpers ────────────────────────────────────────────────────────────

/// Truncate to at most `max` characters.
pub(crate) fn truncate_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((at, _)) => text[..at].to_string(),
        None => text.to_string(),
    }
}

/// Truncate to at most `max` bytes, backing up to a character boundary.
pub(crate) fn truncate_bytes(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// Collapse every run of whitespace to a single space and trim.
pub(crate) fn squeeze(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            space = !out.is_empty();
        } else {
            if space {
                out.push(' ');
            }
            space = false;
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_map_to_languages() {
        assert_eq!(lang_of("a/b.rs"), Lang::Rust);
        assert_eq!(lang_of("a/b.py"), Lang::Python);
        assert_eq!(lang_of("a/b.ts"), Lang::TypeScript);
        assert_eq!(lang_of("a/b.tsx"), Lang::Tsx);
        assert_eq!(lang_of("a/b.mjs"), Lang::JavaScript);
        assert_eq!(lang_of("a/b.go"), Lang::Go);
        assert_eq!(lang_of("a/b.md"), Lang::Markdown);
        assert_eq!(lang_of("a/b.bin"), Lang::Other);
        assert_eq!(lang_of("Makefile"), Lang::Other);
        assert_eq!(lang_of(".gitignore"), Lang::Other);
    }

    #[test]
    fn paths_normalise_without_touching_disk() {
        assert_eq!(normalize("./a//b/../c.rs"), "a/c.rs");
        assert_eq!(normalize("../a/b.rs"), "../a/b.rs");
        assert_eq!(join("src/net", "../util.rs"), "src/util.rs");
        assert_eq!(parent_dir("a/b/c.rs"), "a/b");
        assert_eq!(parent_dir("c.rs"), "");
        assert_eq!(file_stem("a/b/mod.rs"), "mod");
        assert_eq!(ancestors("a/b"), vec!["a/b", "a", ""]);
    }

    #[test]
    fn text_helpers_stay_on_character_boundaries() {
        assert_eq!(truncate_chars("héllo", 2), "hé");
        assert_eq!(truncate_bytes("héllo", 2), "h");
        assert_eq!(squeeze("  pub  fn\n  run() "), "pub fn run()");
    }
}
