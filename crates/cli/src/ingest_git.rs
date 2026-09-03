//! `mushroomdb ingest-git <db> <repo>`: build and maintain a graph of a git
//! repository. First run ingests the whole history; later runs apply only the
//! commits after the recorded `GitSync` head, so deletes and renames retract
//! or move derived edges instead of leaving them stale.
//!
//! Graph shape:
//!
//! | Label | key (`id`) | props |
//! |---|---|---|
//! | `Author` | email | `name` |
//! | `Commit` | full sha | `message`, `ts`, `author_id` |
//! | `File` | path | `path`, `dir`, `ext`, `commits`, `n_commits`, `top_author_id`, `author_counts` |
//! | `GitSync` | `"__mushroomdb_git_sync__"` | `sha` |
//!
//! Edges: user `TOUCHED` Commit→File, auto-FK `AUTHOR` Commit→Author and
//! `TOP_AUTHOR` File→Author, rule-derived `CO_CHANGED` File→File and `KNOWS`
//! Author→File.
use crate::CliError;
use core_api::{Direction, IngestOptions, Predicate, ResultSet, RuleDef, SharedDb, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Cap on the `commits` list stored per file. Bounds both node size and the
/// cost of the jaccard overlap the `co_changed` rule runs over that list.
pub const DEFAULT_MAX_COMMITS_PER_FILE: usize = 200;

/// Minimum jaccard overlap of two files' `commits` lists for `CO_CHANGED`.
const CO_CHANGE_MIN: f64 = 0.25;

/// Key of the singleton `GitSync` node holding the last ingested sha.
///
/// Node keys are a single namespace shared with `File` keys, which are repo
/// paths — so this cannot be `"HEAD"`. A repository with a file named `HEAD`
/// (git's own `.git/HEAD` aside, plenty of projects ship one) would otherwise
/// have the sha written onto its `File` node, leaving no sync marker and
/// forcing a full re-ingest on every run.
const SYNC_KEY: &str = "__mushroomdb_git_sync__";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestGitOpts {
    pub repo: PathBuf,
    /// Paths to skip. Pattern ending in `/` = path prefix, pattern starting
    /// with `*.` = extension, otherwise = substring of the path.
    pub exclude: Vec<String>,
    pub max_commits_per_file: usize,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct IngestGitReport {
    pub commits: usize,
    pub files: usize,
    pub authors: usize,
    pub renamed: usize,
    pub deleted: usize,
    pub incremental: bool,
    pub rules_created: Vec<String>,
}

#[derive(Debug)]
enum Change {
    Added(String),
    Modified(String),
    Deleted(String),
    Renamed { from: String, to: String },
}

#[derive(Debug)]
struct GitCommit {
    sha: String,
    author_name: String,
    author_email: String,
    ts: i64,
    subject: String,
    changes: Vec<Change>,
}

/// Simple, dependency-free path matcher. Documented in `docs/site/ingest-git.md`.
fn excluded(path: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| {
        if let Some(prefix) = p.strip_suffix('/') {
            path.starts_with(&format!("{prefix}/"))
        } else if let Some(ext) = p.strip_prefix("*.") {
            path.contains('.') && path.rsplit('.').next() == Some(ext)
        } else {
            path.contains(p.as_str())
        }
    })
}

fn git_output(repo: &Path, args: &[&str]) -> Result<std::process::Output, CliError> {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|e| CliError(format!("cannot run git in {}: {e}", repo.display())))
}

/// `git log --reverse --name-status -M --format=<RS>%H<US>%an<US>%ae<US>%at<US>%s <range>`
///
/// Returns oldest commit first. The walk ends at `head` — never at the symbolic
/// `HEAD` — so the range is pinned to the same sha the caller will record as the
/// sync marker. See [`head_sha`] for why that matters.
fn read_log(repo: &Path, since: Option<&str>, head: &str) -> Result<Vec<GitCommit>, CliError> {
    if let Some(s) = since {
        let spec = format!("{s}^{{commit}}");
        if !git_output(repo, &["cat-file", "-e", &spec])?
            .status
            .success()
        {
            return Err(CliError(format!(
                "recorded sync head {s} is not in {} (history rewritten?); \
                 ingest into a fresh database directory",
                repo.display()
            )));
        }
    }
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo).args([
        // Without this git renders any non-ASCII byte in a path as an octal
        // escape, so `src/café.rs` would be stored under a mangled key that no
        // later run matches. Paths containing a tab or newline stay quoted and
        // escaped either way — git has to, or they would break the format below.
        "-c",
        "core.quotePath=false",
        "log",
        "--reverse",
        "--name-status",
        "-M",
        "--no-color",
        // Record separator \x1e between commits, unit separator \x1f between
        // header fields. A commit subject containing either byte splits its own
        // record: the message is truncated at the first \x1f, and a \x1e drops
        // the remainder of that commit's header. Accepted — the parse degrades
        // to a skipped or shortened message, never a panic or a wrong sha.
        "--format=%x1e%H%x1f%an%x1f%ae%x1f%at%x1f%s",
    ]);
    match since {
        Some(s) => cmd.arg(format!("{s}..{head}")),
        None => cmd.arg(head),
    };
    let out = cmd
        .output()
        .map_err(|e| CliError(format!("cannot run git: {e}")))?;
    if !out.status.success() {
        return Err(CliError(format!(
            "git log failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut commits = Vec::new();
    for block in text.split('\x1e').filter(|b| !b.trim().is_empty()) {
        let mut lines = block.lines();
        let header = lines.next().unwrap_or("");
        let f: Vec<&str> = header.split('\x1f').collect();
        if f.len() < 5 {
            continue;
        }
        let mut changes = Vec::new();
        for l in lines {
            let cols: Vec<&str> = l.split('\t').collect();
            match cols.as_slice() {
                [s, p] if s.starts_with('A') => changes.push(Change::Added((*p).to_string())),
                [s, p] if s.starts_with('M') || s.starts_with('T') => {
                    changes.push(Change::Modified((*p).to_string()))
                }
                [s, p] if s.starts_with('D') => changes.push(Change::Deleted((*p).to_string())),
                [s, from, to] if s.starts_with('R') => changes.push(Change::Renamed {
                    from: (*from).to_string(),
                    to: (*to).to_string(),
                }),
                // A copy is a brand-new path with no prior history here.
                [s, _from, to] if s.starts_with('C') => {
                    changes.push(Change::Added((*to).to_string()))
                }
                _ => {}
            }
        }
        commits.push(GitCommit {
            sha: f[0].into(),
            author_name: f[1].into(),
            author_email: f[2].into(),
            ts: f[3].parse().unwrap_or(0),
            subject: f[4].into(),
            changes,
        });
    }
    Ok(commits)
}

/// Resolve `HEAD` to a concrete sha. `Ok(None)` means the repository has no
/// commits yet; a path that is not a repository at all is an error.
///
/// Called **before** [`read_log`], and the resulting sha is both the end of the
/// walk and the recorded sync marker. Resolving it afterwards instead would open
/// a window: a commit landing between the walk and the `rev-parse` would push
/// the marker past a commit that was never ingested, and every later run would
/// skip it silently. Pinning both to one sha closes that window — a commit that
/// lands mid-run is simply outside this range and gets picked up next time.
///
/// Asking git also beats taking `log.last()`. The two agree in practice, since a
/// reachability walk emits its tip first and reversing puts it last, but that is
/// a property of the traversal and of this module's parser rather than something
/// the resume marker should rest on.
fn head_sha(repo: &Path) -> Result<Option<String>, CliError> {
    let out = git_output(repo, &["rev-parse", "--verify", "-q", "HEAD^{commit}"])?;
    if !out.status.success() {
        // No commits yet, or not a repository at all — only the latter is an error.
        if !git_output(repo, &["rev-parse", "--git-dir"])?
            .status
            .success()
        {
            return Err(CliError(format!(
                "not a git repository: {}",
                repo.display()
            )));
        }
        return Ok(None);
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if sha.is_empty() {
        return Err(CliError(format!(
            "git could not resolve HEAD in {}",
            repo.display()
        )));
    }
    Ok(Some(sha))
}

/// Separator inside an `author_counts` entry. An email address cannot contain a
/// tab, so `email\tcount` round-trips unambiguously.
const AUTHOR_COUNT_SEP: char = '\t';

fn file_props(st: &FileState, path: &str) -> Vec<(String, Value)> {
    let commits = &st.commits;
    let dir = path
        .rsplit_once('/')
        .map(|(d, _)| d)
        .unwrap_or("")
        .to_string();
    let ext = path
        .rsplit_once('.')
        .map(|(_, e)| e)
        .unwrap_or("")
        .to_string();
    vec![
        ("id".into(), Value::Str(path.into())),
        ("path".into(), Value::Str(path.into())),
        ("dir".into(), Value::Str(dir)),
        ("ext".into(), Value::Str(ext)),
        (
            "commits".into(),
            Value::List(commits.iter().map(|s| Value::Str(s.clone())).collect()),
        ),
        // The true total, which past `--max-commits-per-file` is larger than
        // the `commits` list it is stored beside. It is also what
        // `author_counts` sums to, so the two props agree at any history
        // length.
        ("n_commits".into(), Value::Int(st.n_commits as i64)),
        ("top_author_id".into(), Value::Str(st.top_author())),
        // Written on every touch so the next incremental run can rebuild the
        // distribution instead of crediting the whole history to the incumbent.
        ("author_counts".into(), st.author_counts_value()),
    ]
}

/// In-memory per-file state accumulated while walking commits.
#[derive(Default, Clone)]
struct FileState {
    commits: Vec<String>,
    by_author: BTreeMap<String, usize>,
    n_commits: usize,
}

impl FileState {
    fn touch(&mut self, sha: &str, author: &str, cap: usize) {
        self.commits.push(sha.to_string());
        if cap > 0 && self.commits.len() > cap {
            self.commits.remove(0);
        }
        self.n_commits += 1;
        *self.by_author.entry(author.to_string()).or_default() += 1;
    }

    /// Most commits wins; ties break on the lexicographically smallest email
    /// so the result is deterministic across runs.
    fn top_author(&self) -> String {
        self.by_author
            .iter()
            .max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)))
            .map(|(a, _)| a.clone())
            .unwrap_or_default()
    }

    /// The per-author distribution as a `File.author_counts` prop: a list of
    /// `"email\tcount"` strings in email order.
    ///
    /// This is the state an incremental run needs and cannot recompute — the
    /// walk only sees the new window, and `top_author_id` alone cannot say how
    /// far ahead the incumbent is. Without it a challenger's commits reset on
    /// every sync and ownership can never change.
    fn author_counts_value(&self) -> Value {
        Value::List(
            self.by_author
                .iter()
                .map(|(email, n)| Value::Str(format!("{email}{AUTHOR_COUNT_SEP}{n}")))
                .collect(),
        )
    }

    /// Inverse of [`FileState::author_counts_value`]. Entries that are not
    /// `email<TAB>count` are skipped rather than failing the run.
    fn set_author_counts(&mut self, list: &[Value]) {
        for v in list {
            let Value::Str(s) = v else { continue };
            let Some((email, n)) = s.rsplit_once(AUTHOR_COUNT_SEP) else {
                continue;
            };
            let Ok(n) = n.parse::<usize>() else { continue };
            if !email.is_empty() {
                *self.by_author.entry(email.to_string()).or_default() += n;
            }
        }
    }
}

/// Cypher behind [`file_state_from`] — every live `File` node's cumulative state.
const FILE_STATE_QUERY: &str = "MATCH (f:File) RETURN f.id AS id, f.commits AS commits, \
     f.n_commits AS n, f.top_author_id AS top, f.author_counts AS author_counts";

/// Rebuild the in-memory per-file state from the `File` nodes already in the
/// graph so incremental runs keep `commits` and ownership counts cumulative.
fn file_state_from(rs: &ResultSet) -> BTreeMap<String, FileState> {
    let mut files = BTreeMap::new();
    for i in 0..rs.len() {
        let id = match rs.get(i, "id") {
            Some(Value::Str(s)) => s.clone(),
            _ => continue,
        };
        let mut st = FileState::default();
        if let Some(Value::List(l)) = rs.get(i, "commits") {
            st.commits = l
                .iter()
                .filter_map(|v| match v {
                    Value::Str(s) => Some(s.clone()),
                    _ => None,
                })
                .collect();
        }
        if let Some(Value::Int(n)) = rs.get(i, "n") {
            st.n_commits = *n as usize;
        }
        match rs.get(i, "author_counts") {
            Some(Value::List(l)) => st.set_author_counts(l),
            // A node written before `author_counts` existed (a store built by
            // 0.4.x). Fall back to the old approximation — the whole prior
            // history credited to the incumbent — so those stores keep
            // working. The prop is written on the next touch, from which point
            // ownership tracks reality; a full re-ingest repairs it at once.
            _ => {
                if let Some(Value::Str(t)) = rs.get(i, "top") {
                    st.by_author.insert(t.clone(), st.n_commits.max(1));
                }
            }
        }
        files.insert(id, st);
    }
    files
}

/// Accumulated effect of one log window, before anything is written.
#[derive(Default)]
struct Walk {
    files: BTreeMap<String, FileState>,
    authors: BTreeMap<String, String>,
    commit_rows: Vec<BTreeMap<String, Value>>,
    touched_edges: Vec<(String, String, String)>,
    /// Paths whose `File` props changed in this window.
    dirty: BTreeSet<String>,
    deleted: BTreeSet<String>,
    /// Node renames to apply, collapsed across chained renames.
    renamed: Vec<(String, String)>,
    /// Any old path → its final path in this window, for edge retargeting.
    alias: BTreeMap<String, String>,
}

impl Walk {
    fn rename(&mut self, from: &str, to: &str, node_exists: bool) {
        if let Some(e) = self.renamed.iter_mut().find(|(_, t)| t == from) {
            e.1 = to.to_string();
        } else if node_exists {
            // A node cannot be both deleted and moved; the move wins.
            self.deleted.remove(from);
            self.renamed.push((from.to_string(), to.to_string()));
        } else {
            self.deleted.insert(from.to_string());
        }
        for v in self.alias.values_mut() {
            if v == from {
                *v = to.to_string();
            }
        }
        self.alias.insert(from.to_string(), to.to_string());
    }
}

pub fn run_ingest_git(db_dir: &Path, opts: &IngestGitOpts) -> Result<IngestGitReport, CliError> {
    let db = SharedDb::open(db_dir)?;
    let since: Option<String> = {
        let r = db.read();
        r.node_ref(SYNC_KEY)
            .and_then(|n| n.prop("sha"))
            .and_then(|v| match v {
                Value::Str(s) if !s.is_empty() => Some(s),
                _ => None,
            })
    };
    let incremental = since.is_some();
    let mut report = IngestGitReport {
        incremental,
        ..Default::default()
    };
    // Pin the end of the walk and the marker to one sha, resolved first. A
    // commit landing mid-run then falls outside this range instead of being
    // skipped by a marker that advanced past it.
    let Some(head) = head_sha(&opts.repo)? else {
        return Ok(report); // repository has no commits yet
    };
    let log = read_log(&opts.repo, since.as_deref(), &head)?;
    if log.is_empty() {
        // Nothing new: leave the store untouched so `commit_seq` does not move.
        return Ok(report);
    }

    let mut w = db.write();
    let ingest = IngestOptions::default(); // key `id`, auto-FK suffix `_id`

    let mut walk = Walk {
        files: if incremental {
            file_state_from(&w.query(FILE_STATE_QUERY, &BTreeMap::new())?)
        } else {
            BTreeMap::new()
        },
        ..Default::default()
    };

    for c in &log {
        walk.authors
            .entry(c.author_email.clone())
            .or_insert_with(|| c.author_name.clone());
        walk.commit_rows.push(BTreeMap::from([
            ("id".to_string(), Value::Str(c.sha.clone())),
            ("message".to_string(), Value::Str(c.subject.clone())),
            ("ts".to_string(), Value::Int(c.ts)),
            ("author_id".to_string(), Value::Str(c.author_email.clone())),
        ]));
        for ch in &c.changes {
            match ch {
                Change::Added(p) | Change::Modified(p) => {
                    if excluded(p, &opts.exclude) {
                        continue;
                    }
                    walk.deleted.remove(p);
                    walk.files.entry(p.clone()).or_default().touch(
                        &c.sha,
                        &c.author_email,
                        opts.max_commits_per_file,
                    );
                    walk.dirty.insert(p.clone());
                    walk.touched_edges
                        .push(("TOUCHED".into(), c.sha.clone(), p.clone()));
                }
                Change::Deleted(p) => {
                    if excluded(p, &opts.exclude) {
                        continue;
                    }
                    walk.files.remove(p);
                    walk.dirty.remove(p);
                    walk.deleted.insert(p.clone());
                }
                Change::Renamed { from, to } => {
                    if excluded(to, &opts.exclude) {
                        // Moved out of scope: drop the old node, keep no alias
                        // so its TOUCHED edges are filtered out below.
                        walk.files.remove(from);
                        walk.dirty.remove(from);
                        walk.deleted.insert(from.clone());
                        continue;
                    }
                    let mut st = walk.files.remove(from).unwrap_or_default();
                    st.touch(&c.sha, &c.author_email, opts.max_commits_per_file);
                    walk.files.insert(to.clone(), st);
                    walk.dirty.remove(from);
                    walk.dirty.insert(to.clone());
                    walk.deleted.remove(to);
                    walk.touched_edges
                        .push(("TOUCHED".into(), c.sha.clone(), to.clone()));
                    let exists = incremental && w.has_node(from);
                    walk.rename(from, to, exists);
                }
            }
        }
    }

    // 1. Authors first: the auto-FK rules for `Commit.author_id` and
    //    `File.top_author_id` only infer once their targets resolve to Author.
    let author_rows: Vec<BTreeMap<String, Value>> = walk
        .authors
        .iter()
        .filter(|(email, _)| !w.has_node(email))
        .map(|(email, name)| {
            BTreeMap::from([
                ("id".to_string(), Value::Str(email.clone())),
                ("name".to_string(), Value::Str(name.clone())),
            ])
        })
        .collect();
    let a = w.ingest_with_edges("Author", author_rows, &ingest, &[])?;
    report.rules_created.extend(a.rules_created);
    report.authors = walk.authors.len();

    // 2. Deletes run first so a rename can claim a path freed in this same
    //    window, then renames carry each node (and its history) to its new path.
    //    `walk.files` is the authority on what is still live: a rename whose
    //    destination is not in it is a delete, not a move.
    for p in &walk.deleted {
        if w.has_node(p) {
            w.delete_node(p)?;
            report.deleted += 1;
        }
    }
    for (from, to) in &walk.renamed {
        if !w.has_node(from) || from == to {
            // Nothing to move, or a rename that swapped back to its own path.
            continue;
        }
        if !walk.files.contains_key(to) {
            // The destination did not survive the window — it was deleted, or
            // moved into an excluded path, after this rename. The node goes
            // with it; renaming into a dead path would strand a phantom node
            // that no later phase refreshes.
            w.delete_node(from)?;
            report.deleted += 1;
            continue;
        }
        if w.has_node(to) {
            // A pre-existing node already holds the destination path (deleted
            // earlier in this window, then claimed by this rename).
            w.delete_node(to)?;
            report.deleted += 1;
        }
        w.rename_node(from, to)?;
        // The key moved, so the `id` prop must move with it. Phase 3 also sets
        // it for every dirty path; doing it here keeps the invariant local to
        // the rename and independent of that filter.
        w.set_prop(to, "id", Value::Str(to.clone()))?;
        report.renamed += 1;
    }

    // 3. File nodes. Existing nodes are updated in place (including `id`, which
    //    must follow the key after a rename); new paths go through ingest so
    //    the `top_author_id` auto-FK rule is inferred.
    let mut new_file_rows = Vec::new();
    for (path, st) in &walk.files {
        if incremental && !walk.dirty.contains(path) {
            continue;
        }
        let props = file_props(st, path);
        if w.has_node(path) {
            for (k, v) in props {
                w.set_prop(path, &k, v)?;
            }
        } else {
            new_file_rows.push(props.into_iter().collect::<BTreeMap<_, _>>());
        }
    }
    let f = w.ingest_with_edges("File", new_file_rows, &ingest, &[])?;
    report.rules_created.extend(f.rules_created);
    report.files = walk.files.len();

    // 4. Commits, then their TOUCHED edges. The two must be separate batches:
    //    a batch that both inserts nodes firing a new rule and carries a user
    //    edge of a not-yet-interned type writes a WAL frame that cannot be
    //    replayed (`Intern` records are emitted in a pre-pass, but on replay the
    //    rule fires — and interns its edge type — before the later `Intern`
    //    record is read). See the report for a reproducer.
    let c = w.ingest_with_edges("Commit", walk.commit_rows, &ingest, &[])?;
    report.rules_created.extend(c.rules_created);
    report.commits = log.len();

    // Edges name File keys, so files must already exist. A path renamed later
    // in this same window is retargeted to where its node ended up.
    let touched: Vec<(String, String, String)> = walk
        .touched_edges
        .into_iter()
        .map(|(t, sha, p)| {
            let p = walk.alias.get(&p).cloned().unwrap_or(p);
            (t, sha, p)
        })
        .filter(|(_, _, p)| walk.files.contains_key(p))
        .collect();
    if !touched.is_empty() {
        w.ingest_with_edges("Commit", Vec::new(), &ingest, &touched)?;
    }

    // 5. Rules and fulltext, first run only. Created after the data so each
    //    rule backfills once.
    if !incremental {
        let co = Predicate::Overlap {
            field: "commits".into(),
            min: CO_CHANGE_MIN,
        };
        w.create_rule(RuleDef {
            name: "co_changed".into(),
            src_label: "File".into(),
            dst_label: "File".into(),
            predicate: co.clone(),
            edge_type: "CO_CHANGED".into(),
            weight_prop: Some("score".into()),
            max_edges: Some(10),
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        })?;
        w.create_rule(RuleDef {
            name: "knows".into(),
            src_label: "Author".into(),
            dst_label: "File".into(),
            predicate: co,
            edge_type: "KNOWS".into(),
            weight_prop: Some("score".into()),
            max_edges: Some(20),
            approximate: false,
            via_label: Some("File".into()),
            via_edge: Some("TOP_AUTHOR".into()),
            via_dir: Some(Direction::In),
        })?;
        report
            .rules_created
            .extend(["co_changed".to_string(), "knows".to_string()]);
        for (l, field) in [("File", "path"), ("Commit", "message"), ("Author", "name")] {
            w.enable_fulltext(l, field)?;
        }
    }

    // 6. Sync marker for the next incremental run, resolved above. It carries
    //    `id` like every other label here, so the key is readable from Cypher.
    if w.has_node(SYNC_KEY) {
        w.set_prop(SYNC_KEY, "sha", Value::Str(head))?;
    } else {
        w.insert_node(
            "GitSync",
            SYNC_KEY,
            vec![
                ("id".into(), Value::Str(SYNC_KEY.into())),
                ("sha".into(), Value::Str(head)),
            ],
        )?;
    }
    Ok(report)
}

pub fn format_ingest_git(r: &IngestGitReport) -> String {
    let mut out = format!(
        "ingest-git: {} commit(s), {} file(s), {} author(s){}\n",
        r.commits,
        r.files,
        r.authors,
        if r.incremental { " (incremental)" } else { "" }
    );
    if r.renamed + r.deleted > 0 {
        out.push_str(&format!("  renamed {}  deleted {}\n", r.renamed, r.deleted));
    }
    if !r.rules_created.is_empty() {
        out.push_str(&format!("  rules: {}\n", r.rules_created.join(", ")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclude_matches_prefix_extension_and_substring() {
        let pats = vec![
            "target/".to_string(),
            "*.lock".into(),
            "node_modules".into(),
        ];
        assert!(excluded("target/debug/foo.rs", &pats));
        assert!(
            !excluded("targeted/foo.rs", &pats),
            "prefix needs the slash"
        );
        assert!(excluded("Cargo.lock", &pats));
        assert!(!excluded("Cargo.toml", &pats));
        assert!(excluded("ui/node_modules/x/y.js", &pats));
        assert!(!excluded("src/lib.rs", &pats));
        assert!(!excluded("anything", &[]));
    }

    #[test]
    fn file_props_split_dir_and_ext() {
        let mut st = FileState::default();
        st.touch("sha1", "a@x.test", 200);
        let m: BTreeMap<_, _> = file_props(&st, "src/a/b.rs").into_iter().collect();
        assert_eq!(m["dir"], Value::Str("src/a".into()));
        assert_eq!(m["ext"], Value::Str("rs".into()));
        assert_eq!(m["n_commits"], Value::Int(1));
        assert_eq!(m["id"], Value::Str("src/a/b.rs".into()));
        assert_eq!(m["top_author_id"], Value::Str("a@x.test".into()));
        assert_eq!(
            m["author_counts"],
            Value::List(vec![Value::Str("a@x.test\t1".into())])
        );
        let m: BTreeMap<_, _> = file_props(&FileState::default(), "README")
            .into_iter()
            .collect();
        assert_eq!(m["dir"], Value::Str(String::new()));
        assert_eq!(m["ext"], Value::Str(String::new()));
    }

    /// The prop is the whole point of the incremental fix: it must survive a
    /// round trip so the next run resumes the real distribution, not the
    /// incumbent's total.
    #[test]
    fn author_counts_round_trip_preserves_the_distribution() {
        let mut st = FileState::default();
        for _ in 0..3 {
            st.touch("s", "alice@x.test", 200);
        }
        for _ in 0..4 {
            st.touch("s", "bob@x.test", 200);
        }
        let Value::List(encoded) = st.author_counts_value() else {
            panic!("author_counts must be a list");
        };
        assert_eq!(
            encoded,
            vec![
                Value::Str("alice@x.test\t3".into()),
                Value::Str("bob@x.test\t4".into()),
            ],
            "email order, so the prop is stable across runs"
        );
        let mut reloaded = FileState::default();
        reloaded.set_author_counts(&encoded);
        assert_eq!(reloaded.by_author, st.by_author);
        assert_eq!(reloaded.top_author(), "bob@x.test");
    }

    /// Malformed entries are skipped, not fatal: the run degrades to the counts
    /// it can read rather than refusing to sync.
    #[test]
    fn author_counts_skips_entries_it_cannot_parse() {
        let mut st = FileState::default();
        st.set_author_counts(&[
            Value::Str("alice@x.test\t2".into()),
            Value::Str("no-tab-here".into()),
            Value::Str("bob@x.test\tnotanumber".into()),
            Value::Str("\t5".into()),
            Value::Int(7),
        ]);
        assert_eq!(st.by_author, BTreeMap::from([("alice@x.test".into(), 2)]));
    }

    #[test]
    fn commits_list_is_capped_and_top_author_is_deterministic() {
        let mut st = FileState::default();
        for i in 0..5 {
            st.touch(&format!("sha{i}"), "b@x.test", 3);
        }
        st.touch("shaX", "a@x.test", 3);
        assert_eq!(st.commits, vec!["sha3", "sha4", "shaX"]);
        assert_eq!(st.n_commits, 6);
        assert_eq!(st.top_author(), "b@x.test");

        // Past the cap the stored `n_commits` is the true total, not the length
        // of the truncated `commits` list, and `author_counts` sums to the same
        // number. A file over `--max-commits-per-file` would otherwise report a
        // history frozen at the cap.
        let m: BTreeMap<_, _> = file_props(&st, "src/hot.rs").into_iter().collect();
        assert_eq!(m["n_commits"], Value::Int(6));
        let Value::List(commits) = &m["commits"] else {
            panic!("commits must be a list");
        };
        assert_eq!(commits.len(), 3, "the list is still capped at 3");
        assert!(
            matches!(m["n_commits"], Value::Int(n) if n as usize > commits.len()),
            "n_commits must exceed the capped list once the cap is passed"
        );
        assert_eq!(
            m["author_counts"],
            Value::List(vec![
                Value::Str("a@x.test\t1".into()),
                Value::Str("b@x.test\t5".into()),
            ]),
            "the per-author counts sum to n_commits, not to the capped list"
        );

        let mut tie = FileState::default();
        tie.touch("s", "b@x.test", 10);
        tie.touch("s", "a@x.test", 10);
        assert_eq!(tie.top_author(), "a@x.test", "ties break on smallest email");
    }
}
