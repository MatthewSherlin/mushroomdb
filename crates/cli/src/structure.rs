//! Structure ingest: the working tree, read as graph.
//!
//! [`ingest_git`](crate::ingest_git) builds the *history* half of a codebase
//! graph — who touched what, when. This module builds the *present* half from
//! the files on disk: what each file defines, what it imports, what its
//! definitions call, and what the prose says about it.
//!
//! It never writes an edge itself. Every relationship is stored as a list
//! property and left to a rule, so the engine owns retraction: rewrite a
//! file's `imports` and the stale `IMPORTS` edges retract in the same commit.
//!
//! | Written on | Prop | Derives |
//! |---|---|---|
//! | `File` | `imports: [File key]` | `IMPORTS` File → File |
//! | `File` | `mentions: [File key]` | `MENTIONS` File → File |
//! | `Symbol` | `calls_to: [Symbol key]` | `CALLS` Symbol → Symbol |
//! | `Symbol` | `file_id: File key` | `DEFINES` Symbol → File |
//!
//! Beside each list sits its evidence: `import_lines` and `call_lines` hold
//! `"<key>\t<line>"` strings, so a tool can quote the line a link came from.
//! One entry per call *site*, not one per callee: a symbol that calls another
//! twelve times contributes twelve entries and one `calls_to` element, which is
//! what lets `context` name every line a change would have to visit.
//!
//! # Resolving a call
//!
//! A file's imports are resolved before its calls, and the resolved list is fed
//! to [`resolve_call`]: a definition living in a file this one imports beats a
//! same-named definition anywhere else in the tree. Without that a call could
//! only cross a crate boundary when the callee's name happened to be unique
//! across the whole repository.
//!
//! That repository-wide tier is also the only one a call written on a receiver
//! never reaches. `.collect()` and `.ok()` name methods on types from outside
//! the tree, and uniqueness would bind them to any single same-named function
//! it found. A method call resolves through the local and imported tiers or not
//! at all — and it reaches a method there through the bare name the index files
//! every `Type.method` under, gated on a receiver that says which type it is.
//!
//! # What is read
//!
//! Only the working tree. The candidates are the `File` nodes git already put
//! in the graph — so exclusion patterns have been applied — narrowed to those
//! that exist on disk right now. A path that lives only in history keeps
//! whatever git recorded about it and is skipped here.
//!
//! # What is written
//!
//! Only differences. Each file's stored props are compared field by field
//! against the freshly extracted ones, and its `Symbol` nodes against the
//! symbols just found in it. A file whose bytes have not changed produces no
//! write at all, which is what makes a re-run byte-identical.
use crate::CliError;
use code_extract::{
    call_lookup_names, extract, indexed_under, resolve_call, resolve_import, resolve_mention,
    CallScope, FileFacts, SymbolIndex, MAX_FILE_BYTES,
};
use core_api::{default_max_edges, BatchOp, Predicate, RuleDef, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// `GraphDb` over the real filesystem, named without spelling out `RealFs` —
/// which `core-api` does not re-export. Both an open [`core_api::GraphDb`] and
/// a [`core_api::WriteGuard`] (through its `Deref`) are one of these.
pub type Db = <core_api::WriteGuard<'static> as std::ops::Deref>::Target;

/// Most `Symbol` nodes kept for one file. A generated or vendored file can
/// define tens of thousands; past this the file is still hashed and its
/// imports still resolve, it just stops contributing definitions.
pub const MAX_SYMBOLS_PER_FILE: usize = 2_000;

/// Files per write batch, and so per WAL commit.
const BATCH_FILES: usize = 500;

/// Name of the `Symbol.file_id` foreign-key rule. Identical to the name
/// zero-config FK inference would choose, so the two never both create it —
/// and declaring it here is what makes the edge `DEFINES` rather than the
/// `FILE` that inference would derive from the field name.
pub const DEFINES_RULE: &str = "auto_fk_symbol_file_id";

/// `(label, field)` pairs this module indexes for full-text search, declared
/// through [`ingest_git_schema`] on the store `ingest-git` creates.
///
/// `Note` and `Concept` appear here as well as in the memory defaults; the
/// schema deduplicates them — `remember` also ensures its own `Note.text`
/// pair, in case it is the very first write.
pub const FULLTEXT: [(&str, &str); 8] = [
    ("Concept", "name"),
    ("Concept", "summary"),
    ("File", "body"),
    ("File", "headings"),
    ("File", "path"),
    ("Note", "text"),
    ("Symbol", "doc"),
    ("Symbol", "name"),
];

/// What one refresh saw. Counts cover every file *scanned*, including those
/// that needed no write, so the numbers are stable across re-runs.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StructureReport {
    /// Files read from disk and extracted.
    pub files_scanned: usize,
    /// Symbols found across those files, after the per-file cap.
    pub symbols: usize,
    /// Resolved import targets.
    pub imports: usize,
    /// Resolved documentation mentions.
    pub mentions: usize,
    /// Resolved calls between symbols.
    pub calls: usize,
    /// Files reduced to hash, language and line count because they are over
    /// [`MAX_FILE_BYTES`] or are not text.
    pub skipped_large: usize,
    /// Files that hit [`MAX_SYMBOLS_PER_FILE`].
    pub symbols_capped: usize,
}

/// Every rule this module declares, in creation order.
///
/// Exported so a test — or a store built some other way — can recreate exactly
/// the rule set these props expect; [`ingest_git_schema`] declares all of them
/// on the store `ingest-git` creates. `ingest-git` itself writes no `Concept`:
/// the skill's `learn` pass writes `Concept.source_files` through
/// `ingest_json`, and [`concept_sources_rule`] links each concept to those
/// files with `DESCRIBED_IN`, which is how the skill tells a stale concept.
///
/// No `about_<label>` rule. `remember` writes `Note.about` *and* inserts the
/// `ABOUT` edges itself; a rule deriving the same edges would own them, and the
/// next `remember` of the same note would be refused as writing a rule-owned
/// edge.
#[must_use]
pub fn rules() -> Vec<RuleDef> {
    vec![
        key_rule(DEFINES_RULE, "Symbol", "File", "file_id", "DEFINES"),
        key_rule("imports", "File", "File", "imports", "IMPORTS"),
        key_rule("calls", "Symbol", "Symbol", "calls_to", "CALLS"),
        key_rule("mentions", "File", "File", "mentions", "MENTIONS"),
        concept_sources_rule(),
    ]
}

// The `concept_sources` rule definition, moved here in 0.7 when `repograph`
// was deleted. It was shared because `repograph::remember` declared the rules
// it sat beside; that no longer exists, and `ingest-git` is the only writer
// left, so it lives next to it.

/// `Concept.source_files` → `DESCRIBED_IN` edges to `File`.
#[must_use]
pub fn concept_sources_rule() -> RuleDef {
    key_rule(
        "concept_sources",
        "Concept",
        "File",
        "source_files",
        "DESCRIBED_IN",
    )
}

/// A `KeyMatch` rule with the engine's default fan-out for the predicate,
/// stated rather than left implicit — the convention across this crate.
fn key_rule(name: &str, src: &str, dst: &str, field: &str, edge: &str) -> RuleDef {
    let predicate = Predicate::KeyMatch {
        field: field.into(),
    };
    let max_edges = Some(default_max_edges(&predicate));
    RuleDef {
        name: name.into(),
        src_label: src.into(),
        dst_label: dst.into(),
        predicate,
        edge_type: edge.into(),
        weight_prop: None,
        max_edges,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
        namespace: None,
    }
}

/// The schema a store `ingest-git` creates is declared with: the structure
/// and `concept_sources` rules, the text fields the repository graph
/// carries, and the memory defaults on top — a repository-as-entities store is
/// an ordinary memory store in 0.7 and `recall` has to work on it.
///
/// A `Schema` rather than a sequence of `enable_fulltext` calls because
/// `enable_fulltext` is not idempotent — `Err(RuleInvalid)` on a pair already
/// declared — while `apply_schema` applies full-text idempotently. It is
/// applied once, to a store `ingest-git` is creating; an existing store is
/// never re-declared on.
#[must_use]
pub fn ingest_git_schema() -> core_api::schema::Schema {
    let mut schema = core_api::memory_schema::memory_defaults();
    schema.rules.extend(rules());
    for (label, field) in FULLTEXT {
        schema
            .fulltext
            .push(((*label).to_string(), (*field).to_string()));
    }
    schema.fulltext.sort();
    schema.fulltext.dedup();
    schema
}

/// What a store `ingest-git` did *not* create still lacks of ingest-git's own
/// declarations: the [`rules`] whose names it does not hold and the
/// [`FULLTEXT`] pairs it does not declare. Never the memory defaults.
///
/// Writing a repository into a store is an explicit act, and the structure
/// props it writes derive no edge without these rules — the ordinary order is
/// `mcp` creating the store first, then `ingest-git`. Filtered by absent name
/// rather than handed whole to `apply_schema`, which deletes and recreates a
/// live rule whose definition differs.
#[must_use]
pub fn missing_structure_schema(w: &Db) -> core_api::schema::Schema {
    let live: BTreeSet<String> = w.rules().into_iter().map(|r| r.name).collect();
    let declared = w.fulltext_pairs();
    core_api::schema::Schema {
        rules: rules()
            .into_iter()
            .filter(|r| !live.contains(&r.name))
            .collect(),
        fulltext: FULLTEXT
            .iter()
            .map(|(l, f)| ((*l).to_string(), (*f).to_string()))
            .filter(|pair| !declared.contains(pair))
            .collect(),
        ..Default::default()
    }
}

/// The `File` keys under a key prefix. `""` is the whole graph, `"vendor/lib/"`
/// one submodule; keys are repository-relative with `/` separators, which is
/// exactly what `code-extract` resolves against.
const FILE_KEYS_QUERY: &str = "MATCH (f:File) WHERE startsWith(f.id, $prefix) RETURN f.id AS id";

/// Every `Symbol` node, with the file it belongs to.
const SYMBOL_QUERY: &str =
    "MATCH (s:Symbol) RETURN s.id AS id, s.name AS name, s.file_id AS file_id";

/// Every `File` node's link lists, for the stale-key scan.
const LINK_LISTS_QUERY: &str =
    "MATCH (f:File) RETURN f.id AS id, f.imports AS imports, f.mentions AS mentions";

/// Refresh every working-tree file under `prefix`.
///
/// Writes in batches of [`BATCH_FILES`] files, one WAL commit per batch. Files
/// whose stored props already match the working tree are left untouched.
pub fn refresh_all(
    w: &mut Db,
    repo: &Path,
    prefix: &str,
    with_docs: bool,
) -> Result<StructureReport, CliError> {
    refresh(w, repo, prefix, None, with_docs)
}

/// Refresh exactly `paths` — those of them that are still files on disk.
///
/// The incremental counterpart of [`refresh_all`]: the caller passes the paths
/// this sync touched, plus every file whose link lists named a path that moved
/// or vanished. See [`importers_of`].
pub fn refresh_files(
    w: &mut Db,
    repo: &Path,
    prefix: &str,
    paths: &[String],
    with_docs: bool,
) -> Result<StructureReport, CliError> {
    refresh(w, repo, prefix, Some(paths), with_docs)
}

/// The `File` keys whose `imports` or `mentions` list still names one of
/// `keys`.
///
/// Renaming a node moves the key its list-derived edges point at, but not the
/// list that derived them: the importer's `imports` still holds the old key,
/// so the edge is stale until that file is extracted again. After a rename or
/// a delete, feed this to [`refresh_files`] alongside the paths that changed
/// and the lists — and with them the edges — are rewritten.
pub fn importers_of(w: &Db, keys: &BTreeSet<String>) -> Result<Vec<String>, CliError> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let rs = w.query(LINK_LISTS_QUERY, &BTreeMap::new())?;
    let mut out = BTreeSet::new();
    for i in 0..rs.len() {
        let Some(Value::Str(id)) = rs.get(i, "id") else {
            continue;
        };
        let names = |field: &str| {
            matches!(rs.get(i, field), Some(Value::List(l))
                if l.iter().any(|v| matches!(v, Value::Str(s) if keys.contains(s))))
        };
        if names("imports") || names("mentions") {
            out.insert(id.clone());
        }
    }
    Ok(out.into_iter().collect())
}

// ── the refresh pass ────────────────────────────────────────────────────────

/// The working tree as the resolvers see it, built once per refresh.
///
/// Every lookup `code-extract` needs is answered from this one listing, so a
/// resolution never touches the filesystem and never names a path that has no
/// `File` node behind it.
#[derive(Default)]
struct Tree {
    files: BTreeSet<String>,
    by_dir: BTreeMap<String, Vec<String>>,
    by_base: BTreeMap<String, Vec<String>>,
}

impl Tree {
    fn build(keys: impl IntoIterator<Item = String>) -> Tree {
        let mut tree = Tree::default();
        for key in keys {
            let (dir, base) = match key.rsplit_once('/') {
                Some((d, b)) => (d.to_string(), b.to_string()),
                None => (String::new(), key.clone()),
            };
            tree.by_base.entry(base).or_default().push(key.clone());
            tree.by_dir.entry(dir).or_default().push(key.clone());
            tree.files.insert(key);
        }
        tree
    }

    fn known(&self, path: &str) -> bool {
        self.files.contains(path)
    }

    fn files_in(&self, dir: &str) -> Vec<String> {
        self.by_dir.get(dir).cloned().unwrap_or_default()
    }

    fn by_basename(&self, name: &str) -> Vec<String> {
        self.by_base.get(name).cloned().unwrap_or_default()
    }

    /// Every name a path call may lead with: each directory name in the tree
    /// and each file's stem, plus the `-`/`_` spelling of both.
    ///
    /// A package directory is conventionally `code-extract` while the path that
    /// reaches it is `code_extract`, so both go in. Anything not in here and
    /// not a symbol — `std`, `serde_json`, `Vec` — names something outside the
    /// tree, and [`resolve_call`] gives a call leading with it no edge.
    fn roots(&self) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for path in &self.files {
            for (at, part) in path.split('/').enumerate() {
                let last = at + 1 == path.split('/').count();
                let name = match last {
                    true => part.rsplit_once('.').map_or(part, |(stem, _)| stem),
                    false => part,
                };
                if name.is_empty() {
                    continue;
                }
                out.insert(name.to_string());
                if name.contains('-') {
                    out.insert(name.replace('-', "_"));
                } else if name.contains('_') {
                    out.insert(name.replace('_', "-"));
                }
            }
        }
        out
    }
}

/// One symbol, resolved and ready to store.
struct SymbolWrite {
    key: String,
    name: String,
    kind: &'static str,
    line_start: u32,
    line_end: u32,
    signature: String,
    doc: String,
    /// Resolved callee keys, sorted and deduplicated.
    calls: Vec<String>,
    /// `"<callee key>\t<line>"`, sorted and deduplicated.
    call_lines: Vec<String>,
}

/// One file's resolved facts, ready to diff against what is stored.
struct FileWrite {
    path: String,
    hash: String,
    lines: u32,
    lang: &'static str,
    imports: Vec<String>,
    import_lines: Vec<String>,
    mentions: Vec<String>,
    headings: Vec<String>,
    body: Option<String>,
    symbols: Vec<SymbolWrite>,
}

fn list(items: &[String]) -> Value {
    Value::List(items.iter().map(|s| Value::Str(s.clone())).collect())
}

/// A list prop, or `None` when it is empty — an empty list carries no edge, so
/// the prop is removed rather than stored blank.
fn some_list(items: &[String]) -> Option<Value> {
    (!items.is_empty()).then(|| list(items))
}

impl FileWrite {
    /// The props this file should carry. `None` means "must not be set", which
    /// is how a prop — and the edges derived from it — is retracted.
    fn props(&self) -> Vec<(&'static str, Option<Value>)> {
        vec![
            ("hash", Some(Value::Str(self.hash.clone()))),
            ("lines", Some(Value::Int(i64::from(self.lines)))),
            ("lang", Some(Value::Str(self.lang.to_string()))),
            ("symbols_n", Some(Value::Int(self.symbols.len() as i64))),
            ("imports", some_list(&self.imports)),
            ("import_lines", some_list(&self.import_lines)),
            ("mentions", some_list(&self.mentions)),
            ("headings", some_list(&self.headings)),
            ("body", self.body.as_ref().map(|b| Value::Str(b.clone()))),
        ]
    }
}

impl SymbolWrite {
    fn props(&self, file: &str) -> Vec<(&'static str, Option<Value>)> {
        vec![
            ("id", Some(Value::Str(self.key.clone()))),
            ("name", Some(Value::Str(self.name.clone()))),
            ("kind", Some(Value::Str(self.kind.to_string()))),
            ("path", Some(Value::Str(file.to_string()))),
            ("file_id", Some(Value::Str(file.to_string()))),
            ("line_start", Some(Value::Int(i64::from(self.line_start)))),
            ("line_end", Some(Value::Int(i64::from(self.line_end)))),
            ("signature", Some(Value::Str(self.signature.clone()))),
            ("doc", Some(Value::Str(self.doc.clone()))),
            ("calls_to", some_list(&self.calls)),
            ("call_lines", some_list(&self.call_lines)),
        ]
    }
}

/// The symbols one file contributes: the key each is stored under, and its
/// index into `FileFacts::symbols`. Two definitions that qualify to the same
/// name would share a key, so the first wins; the rest are dropped.
fn symbol_keys(path: &str, facts: &FileFacts) -> (Vec<(String, usize)>, bool) {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    let mut capped = false;
    for (at, sym) in facts.symbols.iter().enumerate() {
        let key = format!("{path}#{}", sym.name);
        if !seen.insert(key.clone()) {
            continue;
        }
        if out.len() == MAX_SYMBOLS_PER_FILE {
            capped = true;
            break;
        }
        out.push((key, at));
    }
    (out, capped)
}

/// Every index name the calls in `facts` can be looked up under.
///
/// [`resolve_call`] reaches the symbol index by name and by nothing else, and
/// [`call_lookup_names`] says which names one callee produces. So an index
/// holding every definition of exactly this set resolves identically to one
/// holding the whole tree — including the repository-wide tier, which turns on
/// a name being defined exactly once and still sees every definition of the
/// names it is asked about.
///
/// The set depends on the files being extracted and on nothing else, which is
/// what makes a one-file `touch` cost one file's worth of index.
fn call_names(facts: &BTreeMap<String, FileFacts>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for f in facts.values() {
        for sym in &f.symbols {
            for call in &sym.calls {
                out.extend(call_lookup_names(&call.callee));
            }
        }
    }
    out
}

fn refresh(
    w: &mut Db,
    repo: &Path,
    prefix: &str,
    only: Option<&[String]>,
    with_docs: bool,
) -> Result<StructureReport, CliError> {
    // 1. What the graph believes, narrowed to what is on disk right now.
    let params = BTreeMap::from([("prefix".to_string(), Value::Str(prefix.to_string()))]);
    let rs = w.query(FILE_KEYS_QUERY, &params)?;
    let mut candidates = Vec::new();
    for i in 0..rs.len() {
        if let Some(Value::Str(id)) = rs.get(i, "id") {
            if repo.join(id).is_file() {
                candidates.push(id.clone());
            }
        }
    }
    let tree = Tree::build(candidates.iter().cloned());

    // 2. The files this pass is responsible for.
    let targets: Vec<String> = match only {
        None => candidates,
        Some(paths) => {
            let wanted: BTreeSet<&String> = paths.iter().collect();
            candidates
                .into_iter()
                .filter(|p| wanted.contains(p))
                .collect()
        }
    };

    // 3. Extract. The facts are kept: they are both what gets written and what
    //    the symbol index is built from.
    let mut facts: BTreeMap<String, FileFacts> = BTreeMap::new();
    let mut hash_only: BTreeSet<String> = BTreeSet::new();
    for path in &targets {
        let Ok(bytes) = std::fs::read(repo.join(path)) else {
            continue; // unreadable right now; the next sync tries again
        };
        if bytes.len() > MAX_FILE_BYTES || is_binary(&bytes) {
            hash_only.insert(path.clone());
        }
        facts.insert(path.clone(), extract(path, &bytes));
    }

    // 4. Symbols already in the graph: so a call can reach a file this pass is
    //    not touching, and so orphans — symbols whose file was renamed away or
    //    deleted, and whose keys can never be right again — can be swept.
    //
    //    A pass that was handed a path list narrows both halves. The work here
    //    used to be the same on a one-file `touch` as on a whole-tree refresh:
    //    a `has_node` graph read and an index insert for every symbol in the
    //    repository, which is the part of `touch` that grows with the codebase
    //    while the useful work stays constant.
    let looked_up = only.map(|_| call_names(&facts));
    let responsible: Option<BTreeSet<&str>> =
        only.map(|paths| paths.iter().map(String::as_str).collect());

    let stored = w.query(SYMBOL_QUERY, &BTreeMap::new())?;
    let mut by_file: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut orphans: Vec<String> = Vec::new();
    let mut index = SymbolIndex::new();
    for i in 0..stored.len() {
        let (Some(Value::Str(id)), Some(Value::Str(file))) =
            (stored.get(i, "id"), stored.get(i, "file_id"))
        else {
            continue;
        };
        // `has_node` is a graph read per row, and its answer can only change
        // what this pass writes for a path the caller named: those are the
        // files whose symbol set is being rewritten, and the ones the caller
        // says have moved or gone. A narrowed pass therefore leaves an orphan
        // it was not told about alone — `refresh_all` sweeps the lot, and
        // `ingest-git` and `sync` both hand in the keys their commit walk
        // retired, so nothing is left for a `touch` to find.
        if responsible
            .as_ref()
            .is_none_or(|r| r.contains(file.as_str()))
        {
            if !w.has_node(file.as_str()) {
                orphans.push(id.clone());
                continue;
            }
            by_file.entry(file.clone()).or_default().insert(id.clone());
        }
        if facts.contains_key(file) || !tree.known(file) {
            continue; // superseded by this pass, or not a working-tree file
        }
        let Some(Value::Str(name)) = stored.get(i, "name") else {
            continue;
        };
        // A method is filed under its bare name as well as its qualified one,
        // so a narrowed index has to keep `Store.flush` when something looks
        // up `flush` — see `indexed_under`.
        if looked_up.as_ref().is_none_or(|names| {
            indexed_under(name)
                .iter()
                .any(|under| names.contains(under))
        }) {
            index.insert(name, id);
        }
    }

    // 5. Resolve. Symbol keys first, so every call is looked up in one index
    //    spanning the whole tree and a callee in an untouched file still hits.
    let mut report = StructureReport::default();
    let mut keyed: BTreeMap<&String, Vec<(String, usize)>> = BTreeMap::new();
    for (path, f) in &facts {
        let (keys, capped) = symbol_keys(path, f);
        for (key, at) in &keys {
            index.insert(&f.symbols[*at].name, key);
        }
        report.symbols_capped += usize::from(capped);
        keyed.insert(path, keys);
    }

    // What a path call may lead with, computed once for the whole pass.
    let roots = tree.roots();
    let pass = Pass {
        tree: &tree,
        index: &index,
        roots: &roots,
        with_docs,
    };

    let mut writes: Vec<FileWrite> = Vec::new();
    for (path, f) in &facts {
        let write = resolve_file(path, f, &keyed[path], &pass);
        report.files_scanned += 1;
        report.symbols += write.symbols.len();
        report.imports += write.imports.len();
        report.mentions += write.mentions.len();
        report.calls += write.symbols.iter().map(|s| s.calls.len()).sum::<usize>();
        report.skipped_large += usize::from(hash_only.contains(path));
        writes.push(write);
    }

    // 6. Write. Orphans go first and in their own commit: their keys must be
    //    free before a renamed file re-creates its symbols under the new path.
    if !orphans.is_empty() {
        let ops = orphans
            .iter()
            .map(|key| BatchOp::DeleteNode { key: key.clone() })
            .collect();
        commit(w, ops)?;
    }
    for chunk in writes.chunks(BATCH_FILES) {
        let mut ops = Vec::new();
        for file in chunk {
            plan_file(w, file, by_file.get(&file.path), &mut ops);
        }
        commit(w, ops)?;
    }
    Ok(report)
}

/// Apply one batch as one WAL commit. An empty batch writes nothing.
fn commit(w: &mut Db, ops: Vec<BatchOp>) -> Result<(), CliError> {
    if ops.is_empty() {
        return Ok(());
    }
    let (results, sync) = w.commit_group(vec![ops]);
    for r in results {
        r?;
    }
    match sync {
        Some(e) => Err(CliError(e.to_string())),
        None => Ok(()),
    }
}

/// Whether the leading bytes look like something other than text. Mirrors the
/// probe `code-extract` applies, so the count and the extraction agree.
fn is_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(8 * 1024)].contains(&0)
}

/// Everything one file's resolution needs beyond its own facts. Built once for
/// the whole pass, except `types`, which is per file.
struct Pass<'a> {
    tree: &'a Tree,
    index: &'a SymbolIndex,
    /// What a path call may lead with, over the whole working tree.
    roots: &'a BTreeSet<String>,
    with_docs: bool,
}

/// Turn one file's raw facts into resolved keys.
fn resolve_file(path: &str, f: &FileFacts, keys: &[(String, usize)], pass: &Pass<'_>) -> FileWrite {
    let (tree, index, with_docs) = (pass.tree, pass.index, pass.with_docs);
    let known = |p: &str| tree.known(p);
    let files_in = |d: &str| tree.files_in(d);
    let by_base = |n: &str| tree.by_basename(n);

    let mut imports = BTreeSet::new();
    let mut import_lines = BTreeSet::new();
    for imp in &f.imports {
        for target in resolve_import(f.lang, path, &imp.raw, &known, &files_in) {
            if target == path {
                continue;
            }
            import_lines.insert(format!("{target}\t{}", imp.line));
            imports.insert(target);
        }
    }

    let mut mentions = BTreeSet::new();
    if with_docs {
        for token in &f.mentions {
            if let Some(target) = resolve_mention(path, token, &known, &by_base) {
                if target != path {
                    mentions.insert(target);
                }
            }
        }
    }

    // Calls are resolved against this file's own imports, so a callee defined
    // in another crate is reachable even when its name is not unique across the
    // repository. The imports are resolved above, in this same pass.
    let imported: Vec<String> = imports.iter().cloned().collect();
    let scope = CallScope {
        imports: &imported,
        roots: pass.roots,
    };

    let mut symbols = Vec::with_capacity(keys.len());
    for (key, at) in keys {
        let fact = &f.symbols[*at];
        let mut calls = BTreeSet::new();
        let mut call_lines = BTreeSet::new();
        for call in &fact.calls {
            let Some(target) = resolve_call(path, call, index, &scope) else {
                continue;
            };
            if &target == key {
                continue; // a definition calling itself is not a graph edge
            }
            call_lines.insert(format!("{target}\t{}", call.line));
            calls.insert(target);
        }
        symbols.push(SymbolWrite {
            key: key.clone(),
            name: fact.name.clone(),
            kind: fact.kind,
            line_start: fact.line_start,
            line_end: fact.line_end,
            signature: fact.signature.clone(),
            doc: fact.doc.clone(),
            calls: calls.into_iter().collect(),
            call_lines: call_lines.into_iter().collect(),
        });
    }

    FileWrite {
        path: path.to_string(),
        hash: f.hash.clone(),
        lines: f.lines,
        lang: f.lang.as_str(),
        imports: imports.into_iter().collect(),
        import_lines: import_lines.into_iter().collect(),
        mentions: mentions.into_iter().collect(),
        headings: if with_docs {
            f.headings.clone()
        } else {
            Vec::new()
        },
        body: if with_docs { f.body.clone() } else { None },
        symbols,
    }
}

/// Queue the ops that make one file's stored state match `file`.
///
/// Nothing is queued for a field that already holds the right value, so a file
/// whose bytes have not changed contributes no ops at all.
fn plan_file(w: &Db, file: &FileWrite, held: Option<&BTreeSet<String>>, ops: &mut Vec<BatchOp>) {
    for (field, want) in file.props() {
        diff_prop(w, &file.path, field, want, ops);
    }

    let wanted: BTreeSet<&String> = file.symbols.iter().map(|s| &s.key).collect();
    for key in held.into_iter().flatten() {
        if !wanted.contains(key) {
            ops.push(BatchOp::DeleteNode { key: key.clone() });
        }
    }
    for sym in &file.symbols {
        let props = sym.props(&file.path);
        match w.node_ref(&sym.key).map(|n| n.label().to_string()) {
            // A repository may contain a file whose path is literally another
            // file's symbol key — `#` is a legal character in a path. The node
            // that got there first keeps it: writing symbol props onto someone
            // else's `File` node would corrupt it, and there is no second key
            // to put the symbol under.
            Some(label) if label != "Symbol" => continue,
            Some(_) => {
                for (field, want) in props {
                    diff_prop(w, &sym.key, field, want, ops);
                }
            }
            None => ops.push(BatchOp::InsertNode {
                label: "Symbol".into(),
                key: sym.key.clone(),
                props: props
                    .into_iter()
                    .filter_map(|(f, v)| v.map(|v| (f.to_string(), v)))
                    .collect(),
            }),
        }
    }
}

/// Queue a set or a remove for one field, or nothing when it already agrees.
fn diff_prop(w: &Db, key: &str, field: &str, want: Option<Value>, ops: &mut Vec<BatchOp>) {
    let current = w.node_ref(key).and_then(|n| n.prop(field));
    if current == want {
        return;
    }
    match want {
        Some(value) => ops.push(BatchOp::SetProp {
            key: key.to_string(),
            field: field.to_string(),
            value,
        }),
        None => ops.push(BatchOp::RemoveProp {
            key: key.to_string(),
            field: field.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_cover_every_derived_structure_edge() {
        let names: Vec<String> = rules().into_iter().map(|r| r.name).collect();
        for want in [
            DEFINES_RULE,
            "imports",
            "calls",
            "mentions",
            "concept_sources",
        ] {
            assert!(names.contains(&want.to_string()), "missing rule {want}");
        }
        // `remember` inserts its own ABOUT edges; a rule owning them would
        // refuse the next `remember` of the same note.
        assert!(
            !names.iter().any(|n| n.starts_with("about_")),
            "ingest-git must declare no about_* rule: {names:?}"
        );
        for def in rules() {
            assert_eq!(
                def.max_edges,
                Some(default_max_edges(&def.predicate)),
                "{} must state its fan-out",
                def.name
            );
        }
    }

    #[test]
    fn concept_sources_rule_is_concept_to_file() {
        let r = concept_sources_rule();
        assert_eq!(r.name, "concept_sources");
        assert_eq!(r.src_label, "Concept");
        assert_eq!(r.dst_label, "File");
        assert_eq!(r.edge_type, "DESCRIBED_IN");
    }

    #[test]
    fn the_tree_answers_every_lookup_the_resolvers_need() {
        let tree = Tree::build([
            "src/lib.rs".to_string(),
            "src/net/mod.rs".to_string(),
            "README.md".to_string(),
        ]);
        assert!(tree.known("src/lib.rs"));
        assert!(!tree.known("src/gone.rs"));
        assert_eq!(tree.files_in("src"), vec!["src/lib.rs".to_string()]);
        assert_eq!(tree.files_in("nope"), Vec::<String>::new());
        assert_eq!(
            tree.by_basename("mod.rs"),
            vec!["src/net/mod.rs".to_string()]
        );
        assert_eq!(tree.files_in(""), vec!["README.md".to_string()]);
    }

    #[test]
    fn binary_probe_matches_the_extractors() {
        assert!(!is_binary(b"pub fn a() {}"));
        assert!(is_binary(b"pub fn a() {}\0"));
        assert!(!is_binary(b""));
    }
}
