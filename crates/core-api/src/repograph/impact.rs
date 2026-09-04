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

use crate::db::GraphDb;
use crate::repograph::facts::{
    label_of, neighbors, neighbors_both, owner_name, rank, score_of, symbol_file,
};
use crate::repograph::render::sanitize;
use crate::Direction;
use core_storage::fs::Fs;
use serde::Serialize;
use std::collections::BTreeSet;

/// Symbols named per file. Past a handful the list stops being a warning and
/// becomes a table of contents.
const MAX_SYMBOLS: usize = 6;

/// How much of the graph one `impact` call reports per file.
#[derive(Debug, Clone, PartialEq)]
pub struct ImpactOptions {
    /// Weakest co-change score worth naming. Below this the pair changed
    /// together a few times out of many, which is noise in a review.
    pub min_score: f64,
    pub max_partners: usize,
    pub max_importers: usize,
}

impl Default for ImpactOptions {
    fn default() -> Self {
        Self {
            min_score: 0.3,
            max_partners: 6,
            max_importers: 6,
        }
    }
}

/// One file the change reaches, and whether the caller has it open already.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Partner {
    pub path: String,
    /// The co-change score for a partner. An importer is not a statistical
    /// association but a stated dependency, so its score is `1.0` and no
    /// digest prints it.
    pub score: f64,
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

/// Files this one changes with, strongest first, above the score floor.
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
    scored
        .into_iter()
        .map(|(other, score)| Partner {
            modified: modified.contains(&other),
            path: sanitize(&other),
            score,
        })
        .collect()
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
