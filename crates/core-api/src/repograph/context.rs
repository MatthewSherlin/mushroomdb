//! `context` — everything the graph knows about one file or symbol.
//!
//! The question an assistant asks before editing something it has not read:
//! what is this, what does it look like, who owns it, what calls it, what does
//! it call, what imports it, what changes with it, what has been said about it.
//! One node lookup answers all of it, because `ingest-git` already wrote the
//! edges; the only thing read from outside the graph is the source itself,
//! quoted from the working tree so the excerpt is what is on disk now rather
//! than what was committed.
//!
//! # Pointers, not bodies
//!
//! The body is the expensive half of the answer and the half a caller can
//! fetch itself, so it is off unless asked for: a default answer carries the
//! line range, the signature and the graph's facts, and
//! [`ContextOptions::source`] adds the lines. A reader who only needed to know
//! where something lives pays for a pointer.
//!
//! # Naming a target
//!
//! A key is taken as it stands: `src/core/db.rs` is a file, and
//! `src/core/db.rs#open` a symbol. Anything else is looked up as a bare symbol
//! name, which is how a person refers to a function they have only heard of.
//! Two symbols can share a name, and then the answer is the choice itself — the
//! candidates and nothing else, so no caller mistakes one for the other.
//!
//! # Callers are call sites
//!
//! "What calls this" is answered from the callers' `call_lines`, not from the
//! `CALLS` edges alone, and grouped by the file the calls sit in. An edge is
//! written once however many times a call is written, so counting edges
//! reported a symbol called twelve times from six functions as six call sites —
//! and a person changing a signature has twelve lines to visit, not six.

use crate::db::GraphDb;
use crate::repograph::facts::{
    commits_of, evidence_line, evidence_lines, int_prop, label_of, list_prop, neighbors,
    neighbors_both, owner_name, rank, score_of, str_prop, symbol_file,
};
use crate::repograph::map::SYNC_KEY;
use crate::repograph::render::sanitize;
use crate::Direction;
use core_storage::fs::Fs;
use core_storage::Value;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Source lines quoted at most, whichever end of a symbol they come from.
pub const MAX_SOURCE_LINES: usize = 80;
/// Callees named.
const MAX_CALLS: usize = 8;
/// Files named on the `callers` line. Generous, because an incomplete answer
/// here is the one that misleads: a caller left out of a blast radius is a
/// caller nobody goes and looks at.
const MAX_CALLER_FILES: usize = 12;
/// Call sites named per calling file, past which the count stands in for the
/// lines.
///
/// Eight is where the line stops being read and starts being skimmed: what a
/// reader needs from a file with fifty call sites is the file and the order of
/// magnitude, not fifty numbers. Naming the file is what makes the answer
/// complete; naming every line in it is what made the digest twice as wide as
/// the one it replaced.
const MAX_SITES_PER_FILE: usize = 8;
/// Files named on the import lines, each way.
const MAX_IMPORTS: usize = 8;
/// Co-change partners named.
const MAX_PARTNERS: usize = 6;
/// Commits named.
const MAX_COMMITS: usize = 5;
/// Notes and concepts named, each.
const MAX_NOTES: usize = 3;

/// What a `context` call was asked about, once resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    File {
        path: String,
    },
    Symbol {
        key: String,
    },
    /// No file or symbol answers to this name. Either nothing does, or several
    /// symbols do — [`ContextReport::candidates`] tells the two apart.
    Unknown {
        target: String,
    },
}

/// Everywhere one file calls the target.
///
/// A `CALLS` edge says *which symbol* calls another, one edge however many
/// times the call is written. That answers "who calls this" and cannot answer
/// "where is it called": a function called twelve times from six others looked
/// like six call sites. The lines come from the caller's `call_lines`, which
/// records every site, so this is the whole list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CallSites {
    /// The file the calling symbols are defined in.
    pub file: String,
    /// The calling symbols, by key, sorted.
    pub symbols: Vec<String>,
    /// Lines of `file` a call sits on, ascending, at most
    /// [`MAX_SITES_PER_FILE`] of them. `sites` is the true total either way, so
    /// a reader can always tell a short list from a truncated one.
    pub lines: Vec<u32>,
    /// Call sites in `file`, including any the `lines` cap left out.
    pub sites: usize,
}

/// One file or symbol, from every side the graph can see it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ContextReport {
    pub target: Target,
    /// Symbol keys sharing the bare name that was asked for. Non-empty only
    /// when the name was ambiguous, and then nothing else is filled in.
    pub candidates: Vec<String>,
    pub signature: Option<String>,
    pub doc: Option<String>,
    /// `(first line, last line)` of a symbol, as extraction recorded them.
    pub lines: Option<(u32, u32)>,
    /// At most [`MAX_SOURCE_LINES`] lines from the working tree. `None` when
    /// the caller did not ask for a body ([`ContextOptions::source`]), when no
    /// repository path is known, or when the file cannot be read there.
    pub source: Option<String>,
    /// The file itself, or the file a symbol is defined in.
    pub file: String,
    /// The file's top author, by name.
    pub owner: Option<String>,
    /// Every call site into the target, grouped by the file the calls sit in,
    /// most call sites first. See [`CallSites`].
    pub callers: Vec<CallSites>,
    /// How many caller files [`MAX_CALLER_FILES`] left out of `callers`.
    pub callers_not_shown: usize,
    /// `(symbol, the line it is called from)`, sorted by key.
    pub callees: Vec<(String, u32)>,
    pub importers: Vec<String>,
    pub imports: Vec<String>,
    /// `(file, co-change score)`, strongest first.
    pub partners: Vec<(String, f64)>,
    /// `(sha, timestamp, subject)`, newest first.
    pub recent_commits: Vec<(String, i64, String)>,
    /// `(note key, text)` for the notes written about it.
    pub notes: Vec<(String, String)>,
    /// `(concept key, name)` for the concepts learned from its file.
    pub concepts: Vec<(String, String)>,
}

impl ContextReport {
    /// An answer with the target named and nothing else known yet.
    fn empty(target: Target) -> Self {
        Self {
            target,
            candidates: Vec::new(),
            signature: None,
            doc: None,
            lines: None,
            source: None,
            file: String::new(),
            owner: None,
            callers: Vec::new(),
            callers_not_shown: 0,
            callees: Vec::new(),
            importers: Vec::new(),
            imports: Vec::new(),
            partners: Vec::new(),
            recent_commits: Vec::new(),
            notes: Vec::new(),
            concepts: Vec::new(),
        }
    }
}

/// What a `context` call reads beyond the graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ContextOptions {
    /// Quote the target's body from the working tree.
    ///
    /// Off by default, because a body is the expensive half of the answer and
    /// rarely the half that decides anything: a caller reading a digest wants
    /// to know where the thing is, what it looks like and what touches it, and
    /// can open the file itself once it has the pointer. On, the report carries
    /// [`ContextReport::source`] as before.
    pub source: bool,
}

/// Everything known about `target`, with the body quoted.
///
/// The pointer-only form is [`context_with`]; this is it with
/// [`ContextOptions::source`] set, kept as its own function because every
/// caller that wants the body wants nothing else configured.
#[must_use]
pub fn context<F: Fs>(db: &GraphDb<F>, repo: Option<&Path>, target: &str) -> ContextReport {
    context_with(db, repo, target, &ContextOptions { source: true })
}

/// Everything known about `target`.
///
/// `repo` is the working tree the source is quoted from; without one the
/// `GitSync` marker's `repo` is used, and a file that cannot be read there
/// simply has no `source`. Everything else is read from the graph, so the
/// answer is byte-identical for the same store and the same working tree.
///
/// With `opts.source` clear the working tree is not read at all — not read and
/// discarded — so the answer is the graph's and nothing else's.
#[must_use]
pub fn context_with<F: Fs>(
    db: &GraphDb<F>,
    repo: Option<&Path>,
    target: &str,
    opts: &ContextOptions,
) -> ContextReport {
    match resolve(db, target) {
        Resolved::File(path) => {
            let mut report = ContextReport::empty(Target::File {
                path: sanitize(&path),
            });
            let symbols = neighbors(db, &path, "DEFINES", Direction::In);
            (report.callers, report.callers_not_shown) = callers_of(db, &symbols, &path);
            report.callees = callees_of(db, &symbols, &path);
            if opts.source {
                report.source = read_source(db, repo, &path, None);
            }
            fill_file(db, &mut report, &path);
            report.notes = notes_about(db, &[path]);
            report
        }
        Resolved::Symbol(key) => {
            let mut report = ContextReport::empty(Target::Symbol {
                key: sanitize(&key),
            });
            // An undocumented symbol carries an empty `doc`, and an empty
            // string is not a fact worth a line of the digest.
            report.signature = text_prop(db, &key, "signature");
            report.doc = text_prop(db, &key, "doc");
            report.lines = symbol_lines(db, &key);
            (report.callers, report.callers_not_shown) =
                callers_of(db, std::slice::from_ref(&key), "");
            report.callees = callees_of(db, std::slice::from_ref(&key), "");
            let file = symbol_file(db, &key).unwrap_or_default();
            if opts.source {
                report.source = read_source(db, repo, &file, report.lines);
            }
            fill_file(db, &mut report, &file);
            report.notes = notes_about(db, &[key, file]);
            report
        }
        Resolved::Ambiguous(candidates) => {
            let mut report = ContextReport::empty(Target::Unknown {
                target: sanitize(target),
            });
            report.candidates = candidates;
            report
        }
        Resolved::Unknown => ContextReport::empty(Target::Unknown {
            target: sanitize(target),
        }),
    }
}

/// What a caller's target turned out to name.
enum Resolved {
    File(String),
    Symbol(String),
    /// Several symbols carry the bare name that was asked for.
    Ambiguous(Vec<String>),
    Unknown,
}

/// What the caller's `target` names: a key as it stands, or the symbols that
/// carry it as a bare name.
fn resolve<F: Fs>(db: &GraphDb<F>, target: &str) -> Resolved {
    match label_of(db, target).as_deref() {
        Some("File") => return Resolved::File(target.to_string()),
        Some("Symbol") => return Resolved::Symbol(target.to_string()),
        // A key of some other label — an author, a commit — is not something
        // `context` describes, so it falls through to the name lookup and is
        // reported unknown if nothing answers to it.
        _ => {}
    }
    let mut named = named_symbols(db, target);
    match named.len() {
        0 => Resolved::Unknown,
        1 => Resolved::Symbol(named.remove(0)),
        _ => Resolved::Ambiguous(named),
    }
}

/// Symbols whose bare `name` is `name`, by key.
///
/// A scan of the `Symbol` nodes, which is what an exact-match lookup on a
/// field with no index costs — and this runs only when the caller's target is
/// not a key, so a tool call that names one never pays for it.
fn named_symbols<F: Fs>(db: &GraphDb<F>, name: &str) -> Vec<String> {
    let mut out: Vec<String> = db
        .nodes_with_label("Symbol")
        .iter()
        .filter(|n| matches!(n.prop("name"), Some(Value::Str(s)) if s == name))
        .map(|n| sanitize(n.key()))
        .collect();
    out.sort();
    out
}

/// A string prop that has something in it, sanitized. Blank is the same as
/// absent: neither is worth a line.
fn text_prop<F: Fs>(db: &GraphDb<F>, key: &str, field: &str) -> Option<String> {
    str_prop(db, key, field)
        .map(|s| sanitize(&s))
        .filter(|s| !s.trim().is_empty())
}

/// The `(first, last)` line a symbol was extracted from.
fn symbol_lines<F: Fs>(db: &GraphDb<F>, key: &str) -> Option<(u32, u32)> {
    let start = u32::try_from(int_prop(db, key, "line_start")?).ok()?;
    let end = u32::try_from(int_prop(db, key, "line_end")?).ok()?;
    Some((start, end.max(start)))
}

/// Every call site into any of `symbols`, grouped by the file it sits in.
///
/// `exclude_file` drops calls that stay inside one file: asking what calls a
/// file means what calls it from outside. It is empty when the target is a
/// symbol, where a caller in the same file is still a caller.
///
/// Returns the groups and how many files were cut by [`MAX_CALLER_FILES`].
/// Files come back with the most call sites first, ties on the path, so a cut
/// only ever loses the least-involved callers — and the count says so rather
/// than leaving the answer looking complete.
fn callers_of<F: Fs>(
    db: &GraphDb<F>,
    symbols: &[String],
    exclude_file: &str,
) -> (Vec<CallSites>, usize) {
    let mut by_file: BTreeMap<String, (BTreeSet<String>, BTreeSet<u32>)> = BTreeMap::new();
    for symbol in symbols {
        for caller in neighbors(db, symbol, "CALLS", Direction::In) {
            let file = symbol_file(db, &caller).unwrap_or_default();
            if !exclude_file.is_empty() && file == exclude_file {
                continue;
            }
            let lines = evidence_lines(&list_prop(db, &caller, "call_lines"), symbol);
            let slot = by_file.entry(sanitize(&file)).or_default();
            slot.0.insert(sanitize(&caller));
            // A call the evidence list has no line for still happened, so the
            // caller is named; line 0 is how every digest here says "unknown".
            slot.1
                .extend(if lines.is_empty() { vec![0] } else { lines });
        }
    }
    let total = by_file.len();
    let mut out: Vec<CallSites> = by_file
        .into_iter()
        .map(|(file, (symbols, lines))| CallSites {
            file,
            symbols: symbols.into_iter().collect(),
            sites: lines.len(),
            lines: lines.into_iter().take(MAX_SITES_PER_FILE).collect(),
        })
        .collect();
    out.sort_by(|a, b| b.sites.cmp(&a.sites).then(a.file.cmp(&b.file)));
    out.truncate(MAX_CALLER_FILES);
    let cut = total - out.len();
    (out, cut)
}

/// Symbols any of `symbols` calls, with the line the call sits on.
fn callees_of<F: Fs>(
    db: &GraphDb<F>,
    symbols: &[String],
    exclude_file: &str,
) -> Vec<(String, u32)> {
    let mut out: Vec<(String, u32)> = Vec::new();
    for symbol in symbols {
        let lines = list_prop(db, symbol, "call_lines");
        for callee in neighbors(db, symbol, "CALLS", Direction::Out) {
            if !exclude_file.is_empty() && symbol_file(db, &callee).as_deref() == Some(exclude_file)
            {
                continue;
            }
            out.push((
                sanitize(&callee),
                evidence_line(&lines, &callee).unwrap_or(0),
            ));
        }
    }
    out.sort();
    out.dedup();
    out.truncate(MAX_CALLS);
    out
}

/// The half of the report that is about the file, whichever kind of target
/// led to it.
fn fill_file<F: Fs>(db: &GraphDb<F>, report: &mut ContextReport, file: &str) {
    report.file = sanitize(file);
    if file.is_empty() {
        return;
    }
    report.owner = owner_name(db, file).map(|n| sanitize(&n));
    report.importers = neighbors(db, file, "IMPORTS", Direction::In)
        .iter()
        .take(MAX_IMPORTS)
        .map(|k| sanitize(k))
        .collect();
    report.imports = neighbors(db, file, "IMPORTS", Direction::Out)
        .iter()
        .take(MAX_IMPORTS)
        .map(|k| sanitize(k))
        .collect();

    let mut partners: Vec<(String, f64)> = neighbors_both(db, file, "CO_CHANGED")
        .into_iter()
        .map(|other| {
            let score = score_of(db, "CO_CHANGED", file, &other).unwrap_or(0.0);
            (sanitize(&other), score)
        })
        .collect();
    rank(&mut partners);
    partners.truncate(MAX_PARTNERS);
    report.partners = partners;

    report.recent_commits = commits_of(db, file)
        .into_iter()
        .take(MAX_COMMITS)
        .map(|c| (sanitize(&c.sha), c.ts, sanitize(&c.subject)))
        .collect();

    report.concepts = neighbors(db, file, "DESCRIBED_IN", Direction::In)
        .iter()
        .take(MAX_NOTES)
        .map(|key| {
            let name = str_prop(db, key, "name").unwrap_or_else(|| key.clone());
            (sanitize(key), sanitize(&name))
        })
        .collect();
}

/// The notes written about any of `keys`, by note key.
fn notes_about<F: Fs>(db: &GraphDb<F>, keys: &[String]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for key in keys.iter().filter(|k| !k.is_empty()) {
        for note in neighbors(db, key, "ABOUT", Direction::In) {
            let text = str_prop(db, &note, "text").unwrap_or_default();
            out.push((sanitize(&note), sanitize(&text)));
        }
    }
    out.sort();
    out.dedup();
    out.truncate(MAX_NOTES);
    out
}

/// Where a `File` key sits in the working tree, or `None` when it does not sit
/// inside it at all.
///
/// A `File` key is graph content: anything that can write a node chooses it,
/// and nothing constrains it to a repository-relative path. Joined unchecked,
/// an absolute key would replace the root outright and a `..` component would
/// climb out of it, so `context` would quote whatever the process can read into
/// an assistant's context. Three checks, in order:
///
/// - the key must be relative and made only of ordinary path segments, which
///   rules out an absolute path, a Windows drive prefix and every `..`;
/// - both the root and the joined path are canonicalised, which resolves every
///   symlink on the way — including one planted inside the tree;
/// - the result must still be under the canonical root.
///
/// Canonicalising also means a key naming a file that is not there fails here
/// rather than at the read, which is the same answer: no source.
fn inside_repo(root: &Path, file: &str) -> Option<std::path::PathBuf> {
    let rel = Path::new(file);
    if rel
        .components()
        .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return None;
    }
    let real_root = root.canonicalize().ok()?;
    let real = real_root.join(rel).canonicalize().ok()?;
    real.starts_with(&real_root).then_some(real)
}

/// The source of `file` from the working tree: a symbol's own lines, or the
/// head of the file when no line range is given.
///
/// `repo` wins over the `GitSync` marker, so a caller working in a checkout
/// elsewhere reads that one. Only a path [`inside_repo`] accepts is read.
/// Anything that goes wrong — no repository, a key that leaves it, no such
/// file, unreadable bytes — is simply no source: the rest of the report is
/// still worth having.
fn read_source<F: Fs>(
    db: &GraphDb<F>,
    repo: Option<&Path>,
    file: &str,
    lines: Option<(u32, u32)>,
) -> Option<String> {
    if file.is_empty() {
        return None;
    }
    let root = match repo {
        Some(p) => p.to_path_buf(),
        None => std::path::PathBuf::from(str_prop(db, SYNC_KEY, "repo")?),
    };
    let text = std::fs::read_to_string(inside_repo(&root, file)?).ok()?;
    let (first, last) = lines.unwrap_or((1, u32::MAX));
    let skip = first.saturating_sub(1) as usize;
    let take = (last.saturating_sub(first) as usize).saturating_add(1);
    let excerpt: Vec<&str> = text
        .lines()
        .skip(skip)
        .take(take.min(MAX_SOURCE_LINES))
        .collect();
    (!excerpt.is_empty()).then(|| excerpt.join("\n"))
}
