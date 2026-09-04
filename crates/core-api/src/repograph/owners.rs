//! `owners` — who to ask about a file.
//!
//! Four answers, because "who owns this" means four different things. The top
//! author is who wrote most of it, and the share says whether that is a
//! majority or a plurality. The `KNOWS` authors are the people whose other
//! files change when this one does, which finds a reviewer the commit log alone
//! would not. The last touch dates the file. And the quarters say whether
//! ownership is where it was a year ago, which is the question behind asking at
//! all.
//!
//! Everything here is read from the graph: `TOP_AUTHOR` and its
//! `author_counts`, the `KNOWS` edges the co-change rule derives, and the
//! commits on the file itself.

use crate::db::GraphDb;
use crate::repograph::facts::{
    author_counts, author_name, commits_of, int_prop, label_of, neighbors, newest_commit_ts,
    owner_key, rank, score_of, CommitFact,
};
use crate::repograph::render::{quarter_index, quarter_label, sanitize};
use crate::Direction;
use core_storage::fs::Fs;
use serde::Serialize;
use std::collections::BTreeMap;

/// Quarters of history the report covers, counting back from the one "now"
/// falls in.
pub const QUARTERS: i64 = 4;
/// Authors listed as knowing the file.
const MAX_KNOWS: usize = 4;
/// Characters of a sha a digest prints. Seven is what git itself abbreviates
/// to, and the fixture's shas are distinct in the first seven.
pub(super) const SHA_LEN: usize = 7;

/// Who has written a file, and when.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OwnersReport {
    pub path: String,
    /// `(author name, author key, share of the file's commits)`. `None` for a
    /// file with no `TOP_AUTHOR` — a store built with `--no-structure`, or a
    /// file with no commits at all.
    pub top: Option<(String, String, f64)>,
    /// `(author name, co-change score)` for the authors the `knows` rule links
    /// to this file, strongest first.
    pub knows: Vec<(String, f64)>,
    /// `(abbreviated sha, timestamp, subject)` of the newest commit that
    /// touched the file.
    pub last_touch: Option<(String, i64, String)>,
    /// `(quarter, top author's name, commits)` for the last [`QUARTERS`]
    /// quarters, oldest first. A quarter in which nothing touched the file is
    /// left out rather than printed as a zero.
    pub by_quarter: Vec<(String, String, usize)>,
}

/// Who wrote `path`.
///
/// `now_ts` fixes the end of the quarter window; without one it is the newest
/// commit on the file, so the answer depends on the store alone and two runs
/// against an unchanged store agree.
///
/// `None` when `path` is not a `File` in this store — including when it names a
/// node of some other label, which has no owner to report.
#[must_use]
pub fn owners<F: Fs>(db: &GraphDb<F>, path: &str, now_ts: Option<i64>) -> Option<OwnersReport> {
    if label_of(db, path)? != "File" {
        return None;
    }
    let commits = commits_of(db, path);
    Some(OwnersReport {
        path: sanitize(path),
        top: top_author(db, path),
        knows: knows(db, path),
        last_touch: commits.first().map(|c| {
            (
                c.sha.chars().take(SHA_LEN).collect(),
                c.ts,
                sanitize(&c.subject),
            )
        }),
        by_quarter: by_quarter(db, &commits, now_ts),
    })
}

/// The `TOP_AUTHOR` and how much of the file is theirs.
///
/// The share comes from `author_counts`, the distribution `ingest-git` keeps
/// beside `n_commits` so that an incremental sync can add to it. A store
/// written before that prop existed still answers: the commits on the file name
/// their own authors, and counting those gives the same number for any history
/// the `commits` list holds in full.
fn top_author<F: Fs>(db: &GraphDb<F>, path: &str) -> Option<(String, String, f64)> {
    let key = owner_key(db, path)?;
    let recorded = author_counts(db, path);
    let total = int_prop(db, path, "n_commits").unwrap_or(0).max(0) as usize;
    let (mine, all) = if recorded.is_empty() {
        let counted = commit_authors(db, path);
        let all: usize = counted.values().sum();
        (counted.get(&key).copied().unwrap_or(0), all)
    } else {
        let mine = recorded
            .iter()
            .find(|(k, _)| *k == key)
            .map_or(0, |(_, n)| *n);
        let all = if total > 0 {
            total
        } else {
            recorded.iter().map(|(_, n)| *n).sum()
        };
        (mine, all)
    };
    let share = if all == 0 {
        0.0
    } else {
        mine as f64 / all as f64
    };
    Some((sanitize(&author_name(db, &key)), sanitize(&key), share))
}

/// How the file's commits split between their authors, counted from the
/// commits themselves.
fn commit_authors<F: Fs>(db: &GraphDb<F>, path: &str) -> BTreeMap<String, usize> {
    let mut out: BTreeMap<String, usize> = BTreeMap::new();
    for commit in commits_of(db, path) {
        for author in neighbors(db, &commit.sha, "AUTHOR", Direction::Out) {
            *out.entry(author).or_default() += 1;
        }
    }
    out
}

/// The authors the `knows` rule links to this file, by co-change score.
fn knows<F: Fs>(db: &GraphDb<F>, path: &str) -> Vec<(String, f64)> {
    let mut out: Vec<(String, f64)> = neighbors(db, path, "KNOWS", Direction::In)
        .into_iter()
        .map(|author| {
            let score = score_of(db, "KNOWS", &author, path).unwrap_or(0.0);
            (sanitize(&author_name(db, &author)), score)
        })
        .collect();
    rank(&mut out);
    out.truncate(MAX_KNOWS);
    out
}

/// Ownership quarter by quarter, over the window ending at `now_ts`.
///
/// The busiest author of a quarter wins it; a tie goes to the author key that
/// sorts first, which is the same rule `ingest-git` uses to pick a file's top
/// author, so the two never disagree about a tie.
fn by_quarter<F: Fs>(
    db: &GraphDb<F>,
    commits: &[CommitFact],
    now_ts: Option<i64>,
) -> Vec<(String, String, usize)> {
    // Without a caller-supplied "now" the window ends at the newest commit in
    // the store, not the newest on this file: a file nobody has touched for a
    // year should report empty quarters rather than borrow its own last commit
    // as the present and look busy.
    let Some(now) = now_ts.or_else(|| newest_commit_ts(db)) else {
        return Vec::new();
    };
    let last = quarter_index(now);
    let first = last - (QUARTERS - 1);

    let mut counts: BTreeMap<i64, BTreeMap<String, usize>> = BTreeMap::new();
    for commit in commits {
        let q = quarter_index(commit.ts);
        if !(first..=last).contains(&q) {
            continue;
        }
        for author in neighbors(db, &commit.sha, "AUTHOR", Direction::Out) {
            *counts.entry(q).or_default().entry(author).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .map(|(q, by_author)| {
            let total: usize = by_author.values().sum();
            let top = by_author
                .iter()
                .max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)))
                .map(|(key, _)| sanitize(&author_name(db, key)))
                .unwrap_or_default();
            (quarter_label(q), top, total)
        })
        .collect()
}
