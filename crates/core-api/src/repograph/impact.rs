//! `impact` — what else a change touches.
//!
//! Given the files a diff changes, three questions have useful answers before
//! the change is finished: which files usually change with these and are *not*
//! in the diff, who imports them, and which of their symbols are called from
//! elsewhere. Each is a fact the graph already holds — the co-change rule, the
//! import edges, the call edges — and each names something a reviewer would
//! otherwise have to remember.
//!
//! Partners already in the diff are kept and marked `modified`, because "you
//! changed both, as usual" is as useful as "you changed one of the two".
//!
//! # Two ways to be a partner
//!
//! The `co_changed` rule writes an edge on jaccard similarity over the two
//! files' commit lists, above a floor. Similarity is the right measure for the
//! graph — it keeps a busy file from being everyone's partner — but it is a
//! *ratio*, so a file that changes with this one often and also changes a lot on
//! its own scores low and gets no edge at all. On this repository
//! `crates/cli/src/lib.rs` shares six of `install.rs`'s fifteen commits and
//! scores 0.10, well under any floor worth setting, yet it is the third most
//! frequent partner there is.
//!
//! So `impact` reads the commit lists too and names files by shared-commit
//! *count* once the scored partners run out. They are labelled with the count
//! rather than a score, because the two are not comparable and pretending
//! otherwise would be the more misleading answer.

use crate::db::GraphDb;
use crate::repograph::facts::{
    label_of, list_prop, neighbors, neighbors_both, owner_name, rank, score_of, symbol_file,
};
use crate::repograph::render::sanitize;
use crate::Direction;
use core_storage::fs::Fs;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

/// Symbols named per file. Past a handful the list stops being a warning and
/// becomes a table of contents.
const MAX_SYMBOLS: usize = 6;

/// The paths a repository carries that are not its source: build output,
/// vendored dependencies, generated bundles, and lockfiles nobody reads.
///
/// Applied when the user names no `--exclude` pattern of their own, which keeps
/// them out of the history graph *and* out of the working-tree pass. It lives
/// here, rather than only in the ingest, because a caller that builds a file
/// list from a working tree — `impact`'s default diff — has to leave out
/// exactly the paths the ingest left out, or it asks about files no store was
/// ever going to hold and is told they are unknown.
pub const DEFAULT_EXCLUDES: [&str; 6] = [
    "target/",
    "node_modules/",
    "dist/",
    ".git/",
    "*.lock",
    "*.min.js",
];

/// Whether `path` matches any of `patterns`.
///
/// A `foo/` pattern is a *directory prefix*. A `*.` pattern is a **file-name
/// suffix**, not a single extension: `*.min.js` matches `ui/bundle.min.js` the
/// same way `*.lock` matches `Cargo.lock`. Matching only the last dot segment
/// would leave every compound suffix inert, and a compound suffix is exactly
/// how generated files announce themselves. Anything else is a substring.
#[must_use]
pub fn path_excluded(path: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| {
        if let Some(prefix) = p.strip_suffix('/') {
            path.starts_with(&format!("{prefix}/"))
        } else if let Some(suffix) = p.strip_prefix('*').filter(|s| s.starts_with('.')) {
            // The suffix must follow something, so `*.lock` does not claim a
            // path that is nothing but the suffix itself.
            path.len() > suffix.len() && path.ends_with(suffix)
        } else {
            path.contains(p.as_str())
        }
    })
}

/// How much of the graph one `impact` call reports per file.
#[derive(Debug, Clone, PartialEq)]
pub struct ImpactOptions {
    /// Weakest co-change score worth naming. Below this the pair changed
    /// together a few times out of many, which is noise in a review.
    pub min_score: f64,
    /// Fewest shared commits worth naming a partner the score floor hid.
    /// `0` turns the count pass off and leaves only scored partners.
    pub min_shared_commits: usize,
    pub max_partners: usize,
    pub max_importers: usize,
}

impl Default for ImpactOptions {
    fn default() -> Self {
        Self {
            min_score: 0.3,
            min_shared_commits: MIN_SHARED_COMMITS,
            max_partners: 6,
            max_importers: 6,
        }
    }
}

/// Fewest commits two files must share before `impact` names one for the other
/// on count alone. Two is a coincidence; three is a habit.
pub const MIN_SHARED_COMMITS: usize = 3;

/// One file the change reaches, and whether the caller has it open already.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Partner {
    pub path: String,
    /// The co-change score for a partner. An importer is not a statistical
    /// association but a stated dependency, so its score is `1.0` and no
    /// digest prints it.
    pub score: f64,
    /// Commits the two files share, for a partner found by count rather than by
    /// score. `None` for a scored partner and for an importer, and a digest
    /// prints the two differently — a count and a similarity do not compare.
    pub shared_commits: Option<usize>,
    /// The path is in the caller's set of modified files.
    pub modified: bool,
}

/// What changing one file reaches.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FileImpact {
    pub path: String,
    /// The file's top author, by name.
    pub owner: Option<String>,
    /// Files that usually change with this one, strongest first.
    pub partners: Vec<Partner>,
    /// Files that import this one, by key.
    pub importers: Vec<Partner>,
    /// `(symbol, callers in other files)`, most called first.
    pub symbols_used_elsewhere: Vec<(String, usize)>,
}

/// What a set of changed files reaches.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ImpactReport {
    pub files: Vec<FileImpact>,
    /// Requested paths the store has no `File` for: renamed, excluded from the
    /// ingest, or not yet synced. Named rather than dropped, because a missing
    /// answer and an empty one mean different things.
    pub unknown: Vec<String>,
}

/// What changing `files` reaches, one report per file.
///
/// `modified` is the caller's own set — usually the whole diff — and decides
/// only the `modified` flag; a partner in it is still reported. Paths are
/// sorted and deduplicated, so the answer does not depend on the order the
/// caller listed them in.
#[must_use]
pub fn impact<F: Fs>(
    db: &GraphDb<F>,
    files: &[String],
    modified: &BTreeSet<String>,
    opts: &ImpactOptions,
) -> ImpactReport {
    let mut wanted: Vec<&String> = files.iter().collect();
    wanted.sort();
    wanted.dedup();

    let mut report = ImpactReport {
        files: Vec::new(),
        unknown: Vec::new(),
    };
    for path in wanted {
        if label_of(db, path).as_deref() != Some("File") {
            report.unknown.push(sanitize(path));
            continue;
        }
        report.files.push(FileImpact {
            path: sanitize(path),
            owner: owner_name(db, path).map(|n| sanitize(&n)),
            partners: partners(db, path, modified, opts),
            importers: importers(db, path, modified, opts),
            symbols_used_elsewhere: used_elsewhere(db, path),
        });
    }
    report
}

/// Files this one changes with: the scored partners first, then the ones the
/// score floor hides but the commit lists do not.
fn partners<F: Fs>(
    db: &GraphDb<F>,
    path: &str,
    modified: &BTreeSet<String>,
    opts: &ImpactOptions,
) -> Vec<Partner> {
    let mut scored: Vec<(String, f64)> = neighbors_both(db, path, "CO_CHANGED")
        .into_iter()
        .map(|other| {
            let score = score_of(db, "CO_CHANGED", path, &other).unwrap_or(0.0);
            (other, score)
        })
        .filter(|(_, score)| *score >= opts.min_score)
        .collect();
    rank(&mut scored);
    scored.truncate(opts.max_partners);

    let named: BTreeSet<String> = scored.iter().map(|(other, _)| other.clone()).collect();
    let mut out: Vec<Partner> = scored
        .into_iter()
        .map(|(other, score)| Partner {
            modified: modified.contains(&other),
            path: sanitize(&other),
            score,
            shared_commits: None,
        })
        .collect();

    // Whatever room is left goes to the files that change with this one often
    // enough to matter but score too low for an edge.
    for (other, shared) in frequent_partners(db, path, &named, opts.min_shared_commits) {
        if out.len() >= opts.max_partners {
            break;
        }
        out.push(Partner {
            modified: modified.contains(&other),
            path: sanitize(&other),
            score: 0.0,
            shared_commits: Some(shared),
        });
    }
    out
}

/// Files sharing at least `min` commits with `path`, most first, ties on the
/// key. `skip` is what the scored pass already named; `min` of `0` is off.
///
/// The count comes from the `TOUCHED` edges of the commits on `path`, so it
/// sees the pairs the rule's similarity floor left out. `File.commits` is
/// capped by `ingest-git`, so the window counted over is the same one every
/// other co-change answer here is drawn from.
fn frequent_partners<F: Fs>(
    db: &GraphDb<F>,
    path: &str,
    skip: &BTreeSet<String>,
    min: usize,
) -> Vec<(String, usize)> {
    if min == 0 {
        return Vec::new();
    }
    let mine: BTreeSet<String> = list_prop(db, path, "commits").into_iter().collect();
    if mine.is_empty() {
        return Vec::new();
    }
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for sha in &mine {
        for other in neighbors(db, sha, "TOUCHED", Direction::Out) {
            if other != path && !skip.contains(&other) {
                *counts.entry(other).or_default() += 1;
            }
        }
    }
    let mut out: Vec<(String, usize)> = counts.into_iter().filter(|(_, n)| *n >= min).collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    out
}

/// Files that import this one, by key.
fn importers<F: Fs>(
    db: &GraphDb<F>,
    path: &str,
    modified: &BTreeSet<String>,
    opts: &ImpactOptions,
) -> Vec<Partner> {
    neighbors(db, path, "IMPORTS", Direction::In)
        .into_iter()
        .take(opts.max_importers)
        .map(|other| Partner {
            modified: modified.contains(&other),
            path: sanitize(&other),
            score: 1.0,
            shared_commits: None,
        })
        .collect()
}

/// The file's symbols that something outside it calls, and how many callers
/// each has. A call from one symbol to another in the same file says nothing
/// about what a change reaches.
fn used_elsewhere<F: Fs>(db: &GraphDb<F>, path: &str) -> Vec<(String, usize)> {
    let mut out: Vec<(String, usize)> = Vec::new();
    for symbol in neighbors(db, path, "DEFINES", Direction::In) {
        let callers = neighbors(db, &symbol, "CALLS", Direction::In)
            .into_iter()
            .filter(|caller| symbol_file(db, caller).as_deref() != Some(path))
            .count();
        if callers > 0 {
            out.push((sanitize(&symbol), callers));
        }
    }
    rank(&mut out);
    out.truncate(MAX_SYMBOLS);
    out
}
