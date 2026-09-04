//! `map` — the whole repository in one screen.
//!
//! What a person new to a codebase asks first: how big is it, what are its
//! parts, which files does everything else lean on, who knows them, and what
//! has moved lately. Every answer is computed from the graph `ingest-git`
//! wrote; nothing here reads the working tree, and the only clock it reads is
//! the one behind "synced 3h ago" (see *Time* below).
//!
//! # How each answer is found
//!
//! | Section | From |
//! |---|---|
//! | clusters | Louvain over `CO_CHANGED` (weight `score`, ≥ 0.3) ∪ `IMPORTS` (1.0), members labelled `File` |
//! | key files | PageRank over `IMPORTS` ∪ `CO_CHANGED` ∪ `CALLS`, the last projected onto files by `Symbol.file_id` |
//! | owners | `TOP_AUTHOR` in-degree, printed as `Author.name` |
//! | hot | files a commit inside the window touched, by `TOUCHED` |
//! | stale concepts | a `Concept` whose `source_hashes` no longer match its `source_files` |
//!
//! # Time
//!
//! Two clocks, for two different questions.
//!
//! *Which files are hot* is a question about the store, so it is measured
//! against the newest `Commit.ts` — the answer then depends on nothing but the
//! data, and two runs against an unchanged store agree.
//!
//! *How stale is the graph* is a question about the present, so it is measured
//! against the wall clock and the marker's `synced_at`, which `ingest-git`
//! stamps whenever it takes new data. Reading `Commit.ts` here would be
//! useless: on a store synced to its repository's head the newest commit *is*
//! the sync point, so the age would always be `0s`.
//!
//! [`MapOptions::now_ts`] overrides both, which is how a test pins the output.
//! Determinism therefore means byte-identical for the same store *and* the
//! same `now_ts`; without one, only the sync age moves.
//!
//! # Budget
//!
//! [`MapOptions::budget_ms`] is checked once before each phase, and passed on
//! to Louvain, which checks it per sweep. When it fires the phases that have
//! not run are skipped and `truncated` is set, which the rendered digest
//! reports as `(truncated)`.

use crate::algo::LouvainConfig;
use crate::db::GraphDb;
use crate::repograph::facts::{rank, str_list, str_prop};
use crate::repograph::render::{basename, cluster_name, common_dir_prefix, sanitize};
use core_storage::fs::Fs;
use core_storage::Value;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

/// Key of the singleton marker `ingest-git` writes the synced sha on.
pub(super) const SYNC_KEY: &str = "__mushroomdb_git_sync__";
/// Marker prop holding when the store last took new data, in Unix seconds.
/// Absent on a store built before it existed.
const SYNCED_AT: &str = "synced_at";
/// A `CO_CHANGED` edge below this score is too weak to shape a cluster.
const CO_CHANGED_MIN_WEIGHT: f64 = 0.3;
/// Most entries any one-line section prints.
const MAX_KEY_FILES: usize = 5;
const MAX_OWNERS: usize = 5;
const MAX_HOT: usize = 5;
/// A cluster of one file names nothing; the smallest useful group is a pair.
const MIN_CLUSTER: usize = 2;
/// PageRank parameters, matching [`crate::algo::PageRankConfig`]'s defaults.
const DAMPING: f64 = 0.85;
const MAX_ITERS: u32 = 50;
const TOL: f64 = 1e-6;
const SECS_PER_DAY: i64 = 86_400;

/// What [`repo_map`] is allowed to spend and how much it may print.
#[derive(Debug, Clone, PartialEq)]
pub struct MapOptions {
    /// Clusters listed, largest first.
    pub max_communities: usize,
    /// Files named as examples inside each cluster.
    pub max_samples: usize,
    /// Width of the "hot" window, in days back from now.
    pub hot_days: i64,
    /// Wall-clock budget in milliseconds. `0` means no budget.
    pub budget_ms: u64,
    /// Treat this Unix timestamp as now, for both the hot window and the sync
    /// age. Without it the window falls back to the newest `Commit.ts` and the
    /// sync age to the wall clock. Set it to pin the whole output.
    pub now_ts: Option<i64>,
}

impl Default for MapOptions {
    fn default() -> Self {
        Self {
            max_communities: 8,
            max_samples: 3,
            hot_days: 90,
            budget_ms: 3_000,
            now_ts: None,
        }
    }
}

/// How current the graph is: the sha it was synced to, and how long ago that
/// sync ran.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SyncInfo {
    /// The full sha recorded on the `GitSync` marker.
    pub sha: String,
    /// The marker's `synced_at`: Unix seconds at which this store last took
    /// new data from the repository. `None` on a store written before the
    /// marker carried one.
    pub synced_at: Option<i64>,
    /// Seconds between `synced_at` and now. `None` whenever `synced_at` is,
    /// and the digest then reports the sha without an age.
    pub age_secs: Option<i64>,
}

/// One group of files that change and import together.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MapCommunity {
    /// The directory its members share, followed by the subdirectories most
    /// of them sit in — or `<mixed>` when they share no directory at all.
    pub name: String,
    /// Just the directory every member is under. Empty when there is none,
    /// which is the machine-readable form of a `<mixed>` name.
    pub dir: String,
    /// Members in the cluster, of which `samples` names a few.
    pub size: usize,
    /// Share of the cluster's edge weight that stays inside it, `0.0..=1.0`.
    pub cohesion: f64,
    /// The most depended-on members, highest first. Full keys.
    pub samples: Vec<String>,
}

/// The repository, summarised.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RepoMap {
    pub files: usize,
    pub symbols: usize,
    pub commits: usize,
    pub authors: usize,
    /// Absent when the store carries no `GitSync` marker.
    pub last_sync: Option<SyncInfo>,
    pub communities: Vec<MapCommunity>,
    /// `(file key, PageRank score)`, highest first.
    pub key_files: Vec<(String, f64)>,
    /// `(author name, files owned)`, most first.
    pub owners: Vec<(String, usize)>,
    /// `(file key, commits inside the window)`, most first.
    pub hot_files: Vec<(String, usize)>,
    /// Width of the hot window in days, so a reader knows what "hot" meant.
    pub hot_days: i64,
    /// Concepts whose sources changed since they were learned.
    pub stale_concepts: usize,
    /// Three questions this graph can answer well, phrased for asking.
    pub questions: Vec<String>,
    /// The budget fired: some sections are missing or partial.
    pub truncated: bool,
}

/// Whether the deadline has passed. `None` is a run with no budget.
fn spent(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|dl| Instant::now() >= dl)
}

/// Wall-clock seconds since the Unix epoch. The only clock this module reads,
/// and only for the sync age — never for anything that decides content.
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Milliseconds left, floored at 1 so a budgeted call never reads as
/// unbudgeted. `None` in, `0` out: no budget either way.
fn remaining_ms(deadline: Option<Instant>) -> u64 {
    match deadline {
        None => 0,
        Some(dl) => u64::try_from(dl.saturating_duration_since(Instant::now()).as_millis())
            .unwrap_or(u64::MAX)
            .max(1),
    }
}

/// Summarise the repository the store was built from.
///
/// Deterministic for the same store state *and* the same
/// [`MapOptions::now_ts`]: every collection is sorted, ties break on the key,
/// and every section but one is decided by the graph alone. The exception is
/// [`SyncInfo::age_secs`], which without a `now_ts` is measured against the
/// system clock and so moves between runs. See the module docs for what each
/// section means and why that one reads a clock.
#[must_use]
pub fn repo_map<F: Fs>(db: &GraphDb<F>, opts: &MapOptions) -> RepoMap {
    let deadline =
        (opts.budget_ms > 0).then(|| Instant::now() + Duration::from_millis(opts.budget_ms));
    let mut truncated = false;

    let mut file_keys: Vec<String> = db
        .nodes_with_label("File")
        .iter()
        .map(|n| n.key().to_string())
        .collect();
    file_keys.sort();
    let files = file_keys.len();
    let symbols = db.nodes_with_label("Symbol").len();
    let authors = db.nodes_with_label("Author").len();

    // Commit timestamps, read once: both the hot window and "now" need them.
    let mut commit_ts: BTreeMap<String, i64> = BTreeMap::new();
    for n in db.nodes_with_label("Commit") {
        if let Some(Value::Int(ts)) = n.prop("ts") {
            commit_ts.insert(n.key().to_string(), ts);
        }
    }
    let commits = db.nodes_with_label("Commit").len();
    // Two clocks, deliberately. The hot window is measured against the newest
    // commit, so which files count as hot depends only on the store. The sync
    // age is measured against the wall clock, because "how stale is my graph"
    // is a question about the present — and `now_ts` overrides it, which is
    // how the tests pin the answer.
    let now = opts.now_ts.or_else(|| commit_ts.values().copied().max());
    let sync_now = opts.now_ts.unwrap_or_else(now_unix);

    let mut map = RepoMap {
        files,
        symbols,
        commits,
        authors,
        last_sync: None,
        communities: Vec::new(),
        key_files: Vec::new(),
        owners: Vec::new(),
        hot_files: Vec::new(),
        hot_days: opts.hot_days,
        stale_concepts: 0,
        questions: Vec::new(),
        truncated: false,
    };
    if files == 0 {
        return map; // nothing keyed on a file is worth computing
    }

    map.last_sync = str_prop(db, SYNC_KEY, "sha").map(|sha| {
        let synced_at = match db.node_ref(SYNC_KEY).and_then(|n| n.prop(SYNCED_AT)) {
            Some(Value::Int(at)) => Some(at),
            _ => None, // a store built before the marker carried a stamp
        };
        SyncInfo {
            sha: sanitize(&sha),
            synced_at,
            age_secs: synced_at.map(|at| sync_now - at),
        }
    });

    // ── key files ───────────────────────────────────────────────────────────
    // PageRank first: the cluster samples are ranked by it too.
    let scores = if spent(deadline) {
        truncated = true;
        Vec::new()
    } else {
        let (scores, hit_budget) = file_pagerank(db, &file_keys, deadline);
        truncated |= hit_budget;
        scores
    };
    let by_score: BTreeMap<&str, f64> = scores.iter().map(|(k, s)| (k.as_str(), *s)).collect();
    map.key_files = scores
        .iter()
        .take(MAX_KEY_FILES)
        .map(|(k, s)| (sanitize(k), *s))
        .collect();

    // ── clusters ────────────────────────────────────────────────────────────
    if !truncated && !spent(deadline) {
        let report = db.communities(&LouvainConfig {
            // One weight property covers both edge types: a `CO_CHANGED` edge
            // is worth its `score`, and an `IMPORTS` edge, which carries no
            // such property, falls back to 1.0 — above the threshold, so
            // every import counts while a weak co-change does not.
            edge_types: vec!["CO_CHANGED".to_string(), "IMPORTS".to_string()],
            weight_prop: Some("score".to_string()),
            min_weight: Some(CO_CHANGED_MIN_WEIGHT),
            budget_ms: remaining_ms(deadline),
            node_label: Some("File".to_string()),
            ..LouvainConfig::default()
        });
        truncated |= report.truncated;
        for c in report
            .communities
            .iter()
            .filter(|c| c.members.len() >= MIN_CLUSTER)
            .take(opts.max_communities)
        {
            let mut ranked: Vec<(String, f64)> = c
                .members
                .iter()
                .map(|k| (k.clone(), by_score.get(k.as_str()).copied().unwrap_or(0.0)))
                .collect();
            rank(&mut ranked);
            map.communities.push(MapCommunity {
                name: sanitize(&cluster_name(&c.members)),
                dir: sanitize(&common_dir_prefix(&c.members)),
                size: c.members.len(),
                cohesion: c.cohesion,
                samples: ranked
                    .into_iter()
                    .take(opts.max_samples)
                    .map(|(k, _)| sanitize(&k))
                    .collect(),
            });
        }
    } else {
        truncated = true;
    }

    // ── owners ──────────────────────────────────────────────────────────────
    if !spent(deadline) {
        let mut owned: BTreeMap<String, usize> = BTreeMap::new();
        for (_file, author, _w) in db.weighted_edges("TOP_AUTHOR", None) {
            *owned.entry(author).or_default() += 1;
        }
        let mut named: Vec<(String, usize)> = owned
            .into_iter()
            .map(|(key, n)| {
                // Authors are printed by name. The key — a mail address — is
                // only ever a fallback for a store that has none.
                let name = str_prop(db, &key, "name").unwrap_or(key);
                (sanitize(&name), n)
            })
            .collect();
        rank(&mut named);
        named.truncate(MAX_OWNERS);
        map.owners = named;
    } else {
        truncated = true;
    }

    // ── hot ─────────────────────────────────────────────────────────────────
    if let (Some(now), false) = (now, spent(deadline)) {
        let cutoff = now.saturating_sub(opts.hot_days.saturating_mul(SECS_PER_DAY));
        // A closed window: a commit dated after "now" is outside it too, so
        // asking the map what was hot at an earlier point answers about then.
        let recent: BTreeSet<&str> = commit_ts
            .iter()
            .filter(|(_, ts)| (cutoff..=now).contains(ts))
            .map(|(sha, _)| sha.as_str())
            .collect();
        let is_file: BTreeSet<&str> = file_keys.iter().map(String::as_str).collect();
        let mut touched: BTreeMap<String, usize> = BTreeMap::new();
        for (commit, file, _w) in db.weighted_edges("TOUCHED", None) {
            if recent.contains(commit.as_str()) && is_file.contains(file.as_str()) {
                *touched.entry(file).or_default() += 1;
            }
        }
        let mut hot: Vec<(String, usize)> = touched
            .into_iter()
            .map(|(k, n)| (sanitize(&k), n))
            .collect();
        rank(&mut hot);
        hot.truncate(MAX_HOT);
        map.hot_files = hot;
    } else if now.is_some() {
        truncated = true;
    }

    // ── stale concepts ──────────────────────────────────────────────────────
    if !spent(deadline) {
        map.stale_concepts = stale_concepts(db);
    } else {
        truncated = true;
    }

    // Questions are phrased from the raw keys, not the sanitized ones printed
    // above: they have to match a graph key to look a partner up.
    map.questions = questions(db, &map, &scores);
    map.truncated = truncated;
    map
}

/// PageRank over the files, on the union of the three edge types that say one
/// file depends on another.
///
/// `CALLS` runs between symbols, so it is projected onto the files that define
/// them; a call inside one file is not a dependency and is dropped. Weights
/// accumulate across the three sources, and rank flows along the edge — so a
/// file many others import collects it, which is what "most depended-on"
/// means. The iteration mirrors [`crate::algo::pagerank`]: same damping,
/// tolerance, iteration cap, dangling-mass handling and per-iteration budget
/// check.
///
/// Returns the ranking and whether the deadline cut the iteration short. Cut
/// short, the scores are still a valid partial ranking — more iterations would
/// only refine them — but the caller reports the map as truncated.
fn file_pagerank<F: Fs>(
    db: &GraphDb<F>,
    file_keys: &[String],
    deadline: Option<Instant>,
) -> (Vec<(String, f64)>, bool) {
    let n = file_keys.len();
    if n == 0 {
        return (Vec::new(), false);
    }
    let idx: BTreeMap<&str, usize> = file_keys
        .iter()
        .enumerate()
        .map(|(i, k)| (k.as_str(), i))
        .collect();

    // Where each symbol is defined, so a call can be read as a file edge.
    let mut sym_file: BTreeMap<String, String> = BTreeMap::new();
    for node in db.nodes_with_label("Symbol") {
        if let Some(Value::Str(file)) = node.prop("file_id") {
            sym_file.insert(node.key().to_string(), file);
        }
    }

    let mut weight: BTreeMap<(usize, usize), f64> = BTreeMap::new();
    let mut add = |src: Option<&usize>, dst: Option<&usize>, w: f64| {
        if let (Some(&a), Some(&b)) = (src, dst) {
            if a != b {
                *weight.entry((a, b)).or_default() += w;
            }
        }
    };
    for (src, dst, _) in db.weighted_edges("IMPORTS", None) {
        add(idx.get(src.as_str()), idx.get(dst.as_str()), 1.0);
    }
    for (src, dst, w) in db.weighted_edges("CO_CHANGED", Some("score")) {
        add(
            idx.get(src.as_str()),
            idx.get(dst.as_str()),
            w.unwrap_or(1.0),
        );
    }
    for (src, dst, _) in db.weighted_edges("CALLS", None) {
        let (Some(sf), Some(df)) = (sym_file.get(&src), sym_file.get(&dst)) else {
            continue;
        };
        add(idx.get(sf.as_str()), idx.get(df.as_str()), 1.0);
    }

    let mut send_to: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
    for ((a, b), w) in weight {
        send_to[a].push((b, w));
    }
    let mut receive_from: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
    let mut dangling: Vec<usize> = Vec::new();
    for (i, send) in send_to.iter().enumerate() {
        let out: f64 = send.iter().map(|(_, w)| w).sum();
        if send.is_empty() || out <= 0.0 {
            dangling.push(i);
            continue;
        }
        for &(j, w) in send {
            receive_from[j].push((i, w / out));
        }
    }

    let (pr, hit_budget) = power_iteration(n, &receive_from, &dangling, deadline);
    let mut scores: Vec<(String, f64)> = file_keys.iter().cloned().zip(pr).collect();
    rank(&mut scores);
    (scores, hit_budget)
}

/// The power iteration itself, split out so the budget check has a test that
/// does not depend on how fast a machine is.
///
/// `receive_from[j]` holds `(i, share)` for every node that sends rank to `j`,
/// already normalised by `i`'s outgoing weight; `dangling` lists the nodes with
/// no outgoing weight, whose mass spreads uniformly. Returns the ranks and
/// whether the deadline fired before convergence — checked before each
/// iteration, so an already-expired deadline returns the uniform vector.
fn power_iteration(
    n: usize,
    receive_from: &[Vec<(usize, f64)>],
    dangling: &[usize],
    deadline: Option<Instant>,
) -> (Vec<f64>, bool) {
    let nf = n as f64;
    let teleport = (1.0 - DAMPING) / nf;
    let mut pr: Vec<f64> = vec![1.0 / nf; n];
    for _ in 0..MAX_ITERS {
        if spent(deadline) {
            return (pr, true);
        }
        let leaked = dangling.iter().map(|&i| pr[i]).sum::<f64>() * DAMPING / nf;
        let mut next = vec![teleport + leaked; n];
        for (j, slot) in next.iter_mut().enumerate() {
            *slot += DAMPING * receive_from[j].iter().map(|&(i, w)| pr[i] * w).sum::<f64>();
        }
        let delta: f64 = pr.iter().zip(next.iter()).map(|(a, b)| (a - b).abs()).sum();
        pr = next;
        if delta < TOL {
            break;
        }
    }
    (pr, false)
}

/// Concepts whose recorded source hashes no longer match the files they were
/// learned from.
///
/// Three things count as changed, because in all three the concept can no
/// longer be trusted to describe what is there:
///
/// - a `source_files` entry whose `File` now hashes to something else;
/// - a `source_files` entry with no `File` behind it at all, deleted or never
///   written;
/// - lists of unequal length, where a source has no hash to check it against
///   or a hash has no source. Nothing pairs them, so nothing vouches for them.
fn stale_concepts<F: Fs>(db: &GraphDb<F>) -> usize {
    db.nodes_with_label("Concept")
        .iter()
        .filter(|c| {
            let files = str_list(c.prop("source_files"));
            let hashes = str_list(c.prop("source_hashes"));
            if files.len() != hashes.len() {
                return true;
            }
            files
                .iter()
                .zip(hashes.iter())
                .any(|(file, hash)| str_prop(db, file, "hash").as_ref() != Some(hash))
        })
        .count()
}

/// Three questions worth asking of this graph, each naming something the map
/// just showed is important.
///
/// A question is only offered when the graph can answer it: with no key file
/// there is nothing to ask about, and with no cluster there is no directory to
/// ask who owns.
///
/// `ranked` holds the file keys exactly as the graph stores them, which is what
/// a lookup has to match; only the phrasing that comes out is sanitized.
fn questions<F: Fs>(db: &GraphDb<F>, map: &RepoMap, ranked: &[(String, f64)]) -> Vec<String> {
    let mut out = Vec::new();
    if let Some((first, _)) = ranked.first() {
        // The partner it changes with most often — the pairing a newcomer
        // would not guess from the directory tree.
        let mut partners: Vec<(String, f64)> = db
            .weighted_edges("CO_CHANGED", Some("score"))
            .into_iter()
            .filter(|(src, _, _)| src == first)
            .map(|(_, dst, w)| (dst, w.unwrap_or(1.0)))
            .collect();
        rank(&mut partners);
        if let Some((partner, _)) = partners.first() {
            let a = basename(first);
            // Two files with the same name would make the question unreadable,
            // so the partner keeps its path when its name collides.
            let b = if basename(partner) == a {
                partner.as_str()
            } else {
                basename(partner)
            };
            out.push(sanitize(&format!("why does {a} co-change with {b}?")));
        }
    }
    // The largest cluster that has a directory to name: asking who owns a
    // group of files that share no directory is not a question anyone can
    // answer.
    if let Some(cluster) = map.communities.iter().find(|c| !c.dir.is_empty()) {
        out.push(sanitize(&format!("who owns {}?", cluster.dir)));
    }
    if let Some((second, _)) = ranked.get(1) {
        out.push(sanitize(&format!("what imports {}?", basename(second))));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three nodes in a line: 0 → 1 → 2, with 2 dangling.
    fn line() -> (Vec<Vec<(usize, f64)>>, Vec<usize>) {
        let receive_from = vec![Vec::new(), vec![(0, 1.0)], vec![(1, 1.0)]];
        (receive_from, vec![2])
    }

    #[test]
    fn an_expired_deadline_stops_the_iteration_before_it_starts() {
        let (receive_from, dangling) = line();
        let expired = Some(Instant::now() - Duration::from_secs(1));
        let (pr, hit) = power_iteration(3, &receive_from, &dangling, expired);
        assert!(hit, "the budget must be reported as spent");
        assert_eq!(
            pr,
            vec![1.0 / 3.0; 3],
            "nothing ran, so the ranks are still uniform — a valid partial answer"
        );
    }

    #[test]
    fn without_a_deadline_the_iteration_converges_and_ranks_the_sink_top() {
        let (receive_from, dangling) = line();
        let (pr, hit) = power_iteration(3, &receive_from, &dangling, None);
        assert!(!hit, "no budget means nothing was cut short");
        assert!(
            pr[2] > pr[1] && pr[1] > pr[0],
            "rank flows along the line and pools at the end: {pr:?}"
        );
        let total: f64 = pr.iter().sum();
        assert!((total - 1.0).abs() < 1e-6, "ranks sum to one, got {total}");
    }

    #[test]
    fn a_deadline_still_ahead_lets_the_iteration_finish() {
        let (receive_from, dangling) = line();
        let ample = Some(Instant::now() + Duration::from_secs(60));
        let (pr, hit) = power_iteration(3, &receive_from, &dangling, ample);
        assert!(!hit);
        assert!(pr[2] > pr[0]);
    }
}
