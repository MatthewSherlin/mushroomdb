//! `mushroomdb ingest-git <db> <repo>`: build and maintain a graph of a git
//! repository. First run ingests the whole history; later runs apply only the
//! commits after the recorded `GitSync` head, so deletes and renames retract
//! or move derived edges instead of leaving them stale.
//!
//! Graph shape:
//!
//! | Label | key (`id`) | props |
//! |---|---|---|
//! | `Author` | mailmap-resolved email | `name` |
//! | `Commit` | full sha | `message`, `ts`, `author_id`, `pr_id` (with `--prs`) |
//! | `File` | path | `path`, `dir`, `ext`, `commits`, `n_commits`, `top_author_id`, `author_counts`, plus the working-tree props in [`structure`](crate::structure) |
//! | `Symbol` | `"<path>#<qualified name>"` | see [`structure`](crate::structure) |
//! | `PR` | `"pr:<number>"` | `number`, `title`, `url`, `merged_at`, `author_login` |
//! | `GitSync` | `"__mushroomdb_git_sync__"` | `sha`, `synced_at`, `repo`, `recurse`, `prs`, `structure`, `docs` |
//!
//! Edges: user `TOUCHED` Commit→File and `MERGED_AS` PR→Commit, auto-FK
//! `AUTHOR` Commit→Author, `TOP_AUTHOR` File→Author and `PR` Commit→PR,
//! rule-derived `CO_CHANGED` File→File and `KNOWS` Author→File — and, from the
//! working-tree pass, `DEFINES`, `IMPORTS`, `CALLS` and `MENTIONS`.
//!
//! With `--recurse-submodules` each initialised submodule is walked as its own
//! *unit*: its file keys carry the submodule's path in the parent, and it
//! resumes from its own `GitSync` marker.
use crate::structure;
use crate::CliError;
use core_api::{
    default_max_edges, Direction, GraphError, IngestOptions, Predicate, ResultSet, RuleDef,
    SharedDb, Value, WriteGuard, WRITE_LOCK_WAIT,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Cap on the `commits` list stored per file. Bounds both node size and the
/// cost of the jaccard overlap the `co_changed` rule runs over that list.
pub const DEFAULT_MAX_COMMITS_PER_FILE: usize = 200;

/// Applied when the user names no `--exclude` pattern of their own.
///
/// These are the paths a repository carries that are not its source: build
/// output, vendored dependencies, generated bundles, and lockfiles nobody
/// reads. Excluding them keeps them out of the history graph *and* out of the
/// working-tree pass, which would otherwise hash and parse every one.
pub const DEFAULT_EXCLUDES: [&str; 6] = [
    "target/",
    "node_modules/",
    "dist/",
    ".git/",
    "*.lock",
    "*.min.js",
];

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

/// What the CLI prints, and exits 3 on, when another process holds the store's
/// cross-process write lock. Nothing was written, so retrying is always safe.
pub const BUSY_MESSAGE: &str = "another mushroomdb process is writing; retry";

/// Name of the `Commit.pr_id` foreign-key rule. Identical to the name the
/// zero-config FK inference would choose, so the two never both create it.
const PR_FK_RULE: &str = "auto_fk_commit_pr_id";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestGitOpts {
    pub repo: PathBuf,
    /// Paths to skip. Pattern ending in `/` = path prefix, pattern starting
    /// with `*.` = extension, otherwise = substring of the path.
    pub exclude: Vec<String>,
    pub max_commits_per_file: usize,
    /// Walk every initialised submodule as its own unit.
    pub recurse_submodules: bool,
    /// Ask `gh` for merged pull requests and link them to their commits.
    pub prs: bool,
    /// Read the working tree: content hashes, `Symbol` nodes, imports and
    /// calls. Off with `--no-structure`. Recorded on the `GitSync` node.
    pub structure: bool,
    /// Index Markdown bodies, headings and mentions. Off with `--no-docs`, and
    /// inert without `structure`. Recorded on the `GitSync` node.
    pub docs: bool,
    /// Add the database directory to the repository's `.gitignore`.
    pub ensure_gitignore: bool,
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
    /// Submodules walked as their own units.
    pub submodules: usize,
    /// Pull requests inserted by this run.
    pub prs: usize,
    /// Whether this run appended the database directory to `.gitignore`.
    pub gitignore_added: bool,
    /// What the working-tree pass saw. All zeros with `--no-structure`.
    pub structure: crate::structure::StructureReport,
}

/// Paths the commit walk left for the working-tree pass to look at.
#[derive(Default)]
struct StructureWork {
    /// Files added, modified, or renamed *into* this window's paths.
    touched: BTreeSet<String>,
    /// Keys that no longer name a file: renamed away, or deleted. Any file
    /// whose `imports` or `mentions` list still holds one of these has to be
    /// extracted again, or the edge it derived stays behind.
    stale: BTreeSet<String>,
}

/// One git working tree walked by a run: the repository itself, or one of its
/// submodules.
///
/// A submodule's paths are keys under `prefix` (its path in the parent), and it
/// carries its own sync marker, so the two histories advance independently.
#[derive(Debug, Clone)]
struct RepoUnit {
    path: PathBuf,
    /// `""` for the repository itself, `"<displaypath>/"` for a submodule.
    prefix: String,
    sync_key: String,
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
///
/// A `*.` pattern is a *file-name suffix*, not a single extension: `*.min.js`
/// matches `ui/bundle.min.js` the same way `*.lock` matches `Cargo.lock`.
/// Matching only the last dot segment would leave every compound suffix inert,
/// and a compound suffix is exactly how generated files announce themselves.
fn excluded(path: &str, patterns: &[String]) -> bool {
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

fn git_output(repo: &Path, args: &[&str]) -> Result<std::process::Output, CliError> {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|e| CliError(format!("cannot run git in {}: {e}", repo.display())))
}

/// `git log --reverse --name-status -M --format=<RS>%H<US>%aN<US>%aE<US>%at<US>%s <range>`
///
/// Returns oldest commit first. The walk ends at `head` — never at the symbolic
/// `HEAD` — so the range is pinned to the same sha the caller will record as the
/// sync marker. See [`head_sha`] for why that matters.
///
/// `%aN` and `%aE` are the mailmap-resolved name and address, so a repository
/// with a `.mailmap` reports one identity for a contributor who has committed
/// under several addresses. Without one they are exactly `%an`/`%ae`.
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
        "--format=%x1e%H%x1f%aN%x1f%aE%x1f%at%x1f%s",
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

/// Absolute, symlink-free form of `p`, falling back to `p` when it cannot be
/// resolved (a path that does not exist yet, or a permission error). The result
/// is what `GitSync.repo` records, so a later run can find the repository again
/// from any working directory.
fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// The submodule paths recorded in a unit's `.gitmodules`, relative to that
/// unit.
///
/// These are the paths git reports as ordinary changes in `--name-status`
/// output while being gitlinks — a commit pointer, not a file. They get no
/// `File` node whether or not the submodule is walked, and the list is read
/// from configuration so it is the same for an uninitialised submodule.
fn gitlink_paths(unit: &Path) -> BTreeSet<String> {
    let out = Command::new("git")
        .arg("config")
        .arg("--file")
        .arg(unit.join(".gitmodules"))
        .args(["--get-regexp", r"^submodule\..*\.path$"])
        .output();
    let Ok(out) = out else { return BTreeSet::new() };
    if !out.status.success() {
        return BTreeSet::new(); // no .gitmodules, so no submodules
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(_, path)| path.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

/// The repository itself, then one unit per initialised submodule when
/// `recurse` is set.
///
/// `git submodule foreach` visits only initialised submodules and says nothing
/// about the rest, which is the behaviour wanted here: a submodule that was
/// never checked out has no working tree to walk. `$displaypath` is relative to
/// the top-level repository, so it is exactly the key prefix its files need.
fn repo_units(repo: &Path, recurse: bool) -> Result<Vec<RepoUnit>, CliError> {
    let mut units = vec![RepoUnit {
        path: repo.to_path_buf(),
        prefix: String::new(),
        sync_key: SYNC_KEY.to_string(),
    }];
    if !recurse {
        return Ok(units);
    }
    let out = git_output(
        repo,
        &[
            "submodule",
            "foreach",
            "--quiet",
            "--recursive",
            "echo \"$displaypath\"",
        ],
    )?;
    if !out.status.success() {
        return Ok(units);
    }
    let mut paths: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim_end_matches('/').trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    paths.sort();
    paths.dedup();
    for dp in paths {
        let path = repo.join(&dp);
        // `foreach` already skips the uninitialised, but a stale entry or a
        // removed checkout would otherwise fail the whole run.
        if !git_output(&path, &["rev-parse", "--git-dir"])?
            .status
            .success()
        {
            continue;
        }
        units.push(RepoUnit {
            prefix: format!("{dp}/"),
            sync_key: format!("{SYNC_KEY}:{dp}"),
            path,
        });
    }
    Ok(units)
}

/// Rewrite one unit's changes into repository-wide keys: drop gitlinks, then
/// prepend the unit's prefix so a submodule's `src/lib.rs` is stored under
/// `vendor/lib/src/lib.rs`.
///
/// Done before anything else looks at the log, so exclusion patterns, the
/// stored keys and the `File` state query all speak the same path.
fn localise(log: &mut [GitCommit], prefix: &str, gitlinks: &BTreeSet<String>) {
    let keep = |p: &String| !gitlinks.contains(p.as_str());
    for c in log.iter_mut() {
        c.changes.retain(|ch| match ch {
            Change::Added(p) | Change::Modified(p) | Change::Deleted(p) => keep(p),
            Change::Renamed { from, to } => keep(from) && keep(to),
        });
        if prefix.is_empty() {
            continue;
        }
        for ch in c.changes.iter_mut() {
            match ch {
                Change::Added(p) | Change::Modified(p) | Change::Deleted(p) => {
                    *p = format!("{prefix}{p}")
                }
                Change::Renamed { from, to } => {
                    *from = format!("{prefix}{from}");
                    *to = format!("{prefix}{to}");
                }
            }
        }
    }
}

/// Append `<db dir>/` to the repository's `.gitignore` unless it is already
/// listed, creating the file if it does not exist. Returns whether it was
/// written.
///
/// A database kept outside the repository is left alone: the repository has no
/// business ignoring a path it does not contain.
fn ensure_gitignore(repo: &Path, db_dir: &Path) -> Result<bool, CliError> {
    let Ok(rel) = canonical(db_dir)
        .strip_prefix(repo)
        .map(|p| p.to_path_buf())
    else {
        return Ok(false);
    };
    if rel.as_os_str().is_empty() {
        return Ok(false);
    }
    let line = format!("{}/", rel.to_string_lossy().replace('\\', "/"));
    let path = repo.join(".gitignore");
    let current = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(CliError(format!("cannot read {}: {e}", path.display()))),
    };
    let bare = line.trim_end_matches('/');
    if current
        .lines()
        .map(|l| l.trim())
        .any(|l| l == line || l == bare || l == format!("/{line}") || l == format!("/{bare}"))
    {
        return Ok(false);
    }
    let mut next = current;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(&line);
    next.push('\n');
    std::fs::write(&path, next)
        .map_err(|e| CliError(format!("cannot write {}: {e}", path.display())))?;
    Ok(true)
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

/// One merged pull request as reported by `gh`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PullRequest {
    number: i64,
    title: String,
    url: String,
    merged_at: String,
    author_login: String,
    /// `mergeCommit.oid`, absent for a pull request merged some other way (or
    /// whose merge commit has since been rewritten).
    merge_sha: Option<String>,
}

fn pr_key(number: i64) -> String {
    format!("pr:{number}")
}

/// The pull request number in a squash-merge subject: `\(#(\d+)\)$`.
///
/// Hand-rolled rather than pulled in as a dependency — the pattern is anchored
/// at the end of the subject and made of two literals around a run of digits.
fn subject_pr(subject: &str) -> Option<i64> {
    let rest = subject.strip_suffix(')')?;
    let at = rest.rfind("(#")?;
    let digits = &rest[at + 2..];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// `gh pr list --state merged` in `repo`, or an empty list with one warning.
///
/// Every failure is a skip: `gh` may not be installed, the repository may have
/// no GitHub remote, and the user may not be authenticated. None of that is a
/// reason to fail an ingest that is otherwise complete.
fn fetch_prs(repo: &Path) -> Vec<PullRequest> {
    let out = Command::new("gh")
        .current_dir(repo)
        .args([
            "pr",
            "list",
            "--state",
            "merged",
            "--limit",
            "1000",
            "--json",
            "number,title,url,mergedAt,mergeCommit,author",
        ])
        .output();
    let out = match out {
        Ok(o) => o,
        Err(_) => {
            eprintln!("ingest-git: --prs skipped: gh is not on PATH");
            return Vec::new();
        }
    };
    if !out.status.success() {
        let detail = String::from_utf8_lossy(&out.stderr)
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("no detail")
            .to_string();
        eprintln!("ingest-git: --prs skipped: gh pr list failed: {detail}");
        return Vec::new();
    }
    let parsed: serde_json::Value = match serde_json::from_slice(&out.stdout) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("ingest-git: --prs skipped: gh pr list output is not JSON: {e}");
            return Vec::new();
        }
    };
    let Some(items) = parsed.as_array() else {
        eprintln!("ingest-git: --prs skipped: gh pr list did not return a list");
        return Vec::new();
    };
    let mut prs: Vec<PullRequest> = items
        .iter()
        .filter_map(|v| {
            let number = v.get("number")?.as_i64()?;
            let str_at = |k: &str| {
                v.get(k)
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string()
            };
            Some(PullRequest {
                number,
                title: str_at("title"),
                url: str_at("url"),
                merged_at: str_at("mergedAt"),
                author_login: v
                    .get("author")
                    .and_then(|a| a.get("login"))
                    .and_then(|l| l.as_str())
                    .unwrap_or_default()
                    .to_string(),
                merge_sha: v
                    .get("mergeCommit")
                    .and_then(|m| m.get("oid"))
                    .and_then(|o| o.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
            })
        })
        .collect();
    // Ascending by number: the insert order, and so the node order, is the
    // same on every run whatever order gh listed them in.
    prs.sort_by_key(|p| p.number);
    prs.dedup_by_key(|p| p.number);
    prs
}

/// Insert the `PR` nodes that are new, declare the `Commit.pr_id` foreign key,
/// and index titles for search. Runs before the commits so the FK resolves.
fn ingest_prs(
    w: &mut WriteGuard<'_>,
    prs: &[PullRequest],
    ingest: &IngestOptions,
    report: &mut IngestGitReport,
) -> Result<(), CliError> {
    let rows: Vec<BTreeMap<String, Value>> = prs
        .iter()
        .filter(|p| !w.has_node(&pr_key(p.number)))
        .map(|p| {
            BTreeMap::from([
                ("id".to_string(), Value::Str(pr_key(p.number))),
                ("number".to_string(), Value::Int(p.number)),
                ("title".to_string(), Value::Str(p.title.clone())),
                ("url".to_string(), Value::Str(p.url.clone())),
                ("merged_at".to_string(), Value::Str(p.merged_at.clone())),
                (
                    "author_login".to_string(),
                    Value::Str(p.author_login.clone()),
                ),
            ])
        })
        .collect();
    if !rows.is_empty() {
        let r = w.ingest_with_edges("PR", rows, ingest, &[])?;
        report.prs = r.inserted;
        report.rules_created.extend(r.rules_created);
    }
    // Declared here rather than left to FK inference, which only fires on a
    // batch of `Commit` rows that already carry `pr_id` — a run that links a
    // pull request to a commit ingested earlier would otherwise leave the
    // property with no edge behind it.
    if !w.rules().iter().any(|r| r.name == PR_FK_RULE) {
        let predicate = Predicate::KeyMatch {
            field: "pr_id".into(),
        };
        let max_edges = Some(default_max_edges(&predicate));
        w.create_rule(RuleDef {
            name: PR_FK_RULE.into(),
            src_label: "Commit".into(),
            dst_label: "PR".into(),
            predicate,
            edge_type: "PR".into(),
            weight_prop: None,
            max_edges,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        })?;
        report.rules_created.push(PR_FK_RULE.to_string());
    }
    if !w
        .fulltext_pairs()
        .contains(&("PR".to_string(), "title".to_string()))
    {
        w.enable_fulltext("PR", "title")?;
    }
    Ok(())
}

/// Point every commit that carries a pull request at it: the merge commit by
/// sha, a squash merge by its `(#N)` subject. Runs after the commits are in the
/// graph, so a pull request merged before the last sync is linked too.
fn link_prs(
    w: &mut WriteGuard<'_>,
    prs: &[PullRequest],
    ingest: &IngestOptions,
) -> Result<(), CliError> {
    let by_sha: BTreeMap<&str, i64> = prs
        .iter()
        .filter_map(|p| p.merge_sha.as_deref().map(|s| (s, p.number)))
        .collect();
    let known: BTreeSet<i64> = prs.iter().map(|p| p.number).collect();

    let rs = w.query(
        "MATCH (c:Commit) RETURN c.id AS id, c.message AS message, c.pr_id AS pr_id",
        &BTreeMap::new(),
    )?;
    let mut updates: Vec<(String, String)> = Vec::new();
    let mut links: BTreeMap<i64, BTreeSet<String>> = BTreeMap::new();
    for i in 0..rs.len() {
        let Some(Value::Str(sha)) = rs.get(i, "id") else {
            continue;
        };
        let subject = match rs.get(i, "message") {
            Some(Value::Str(s)) => s.as_str(),
            _ => "",
        };
        let Some(number) = by_sha
            .get(sha.as_str())
            .copied()
            .or_else(|| subject_pr(subject).filter(|n| known.contains(n)))
        else {
            continue;
        };
        let key = pr_key(number);
        links.entry(number).or_default().insert(sha.clone());
        if rs.get(i, "pr_id") != Some(&Value::Str(key.clone())) {
            updates.push((sha.clone(), key));
        }
    }
    updates.sort(); // by sha: the same store state writes the same records
    for (sha, key) in updates {
        w.set_prop(&sha, "pr_id", Value::Str(key))?;
    }

    let mut edges: Vec<(String, String, String)> = Vec::new();
    for (number, shas) in links {
        let src = pr_key(number);
        let existing: BTreeSet<String> = w
            .neighbors(&src, "MERGED_AS", Direction::Out)
            .unwrap_or_default()
            .into_iter()
            .collect();
        for sha in shas {
            if !existing.contains(&sha) {
                edges.push(("MERGED_AS".to_string(), src.clone(), sha));
            }
        }
    }
    if !edges.is_empty() {
        w.ingest_with_edges("PR", Vec::new(), ingest, &edges)?;
    }
    Ok(())
}

/// Cypher behind [`file_state_from`] — the cumulative state of every live
/// `File` node belonging to one unit.
///
/// The prefix filter is what keeps a submodule's files out of the parent's
/// walk and vice versa. `startsWith` is the documented Cypher form (see
/// `docs/site/query.md`); the parent unit's empty prefix matches everything, so
/// its own submodules' keys are dropped in [`file_state_from`].
const FILE_STATE_QUERY: &str =
    "MATCH (f:File) WHERE startsWith(f.id, $prefix) RETURN f.id AS id, f.commits AS commits, \
     f.n_commits AS n, f.top_author_id AS top, f.author_counts AS author_counts";

/// Rebuild the in-memory per-file state from the `File` nodes already in the
/// graph so incremental runs keep `commits` and ownership counts cumulative.
///
/// `nested` holds the key prefixes of any submodules inside this unit; their
/// files belong to their own unit's walk and are dropped here.
fn file_state_from(rs: &ResultSet, nested: &[String]) -> BTreeMap<String, FileState> {
    let mut files = BTreeMap::new();
    for i in 0..rs.len() {
        let id = match rs.get(i, "id") {
            Some(Value::Str(s)) => s.clone(),
            _ => continue,
        };
        if nested.iter().any(|p| id.starts_with(p.as_str())) {
            continue;
        }
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

/// The `GitSync` props that say *how* a unit was ingested, as opposed to how
/// far. Compared against the stored node so that changing a flag refreshes the
/// marker even when there is nothing new to walk.
fn marker_flag_props(unit: &RepoUnit, opts: &IngestGitOpts) -> Vec<(String, Value)> {
    vec![
        (
            "repo".into(),
            Value::Str(unit.path.to_string_lossy().into_owned()),
        ),
        ("recurse".into(), Value::Bool(opts.recurse_submodules)),
        ("prs".into(), Value::Bool(opts.prs)),
        ("structure".into(), Value::Bool(opts.structure)),
        ("docs".into(), Value::Bool(opts.docs)),
    ]
}

/// One unit and everything read about it before the write pass opens.
struct Pending {
    unit: RepoUnit,
    /// The sha its marker resumes from, absent on a first run.
    since: Option<String>,
    /// The stored marker disagrees with this run's flags.
    stale: bool,
    /// `None` when the unit has no commits at all.
    head: Option<String>,
    log: Vec<GitCommit>,
    /// Key prefixes of the submodules nested inside this unit, whose files
    /// belong to their own walk.
    nested: Vec<String>,
}

pub fn run_ingest_git(db_dir: &Path, opts: &IngestGitOpts) -> Result<IngestGitReport, CliError> {
    // Absolute and symlink-free: it is recorded on the marker, and a later run
    // has no reason to share this one's working directory.
    let repo = canonical(&opts.repo);
    let units = repo_units(&repo, opts.recurse_submodules)?;
    let prefixes: Vec<String> = units
        .iter()
        .map(|u| u.prefix.clone())
        .filter(|p| !p.is_empty())
        .collect();

    let db = SharedDb::open(db_dir)?;
    let mut report = IngestGitReport {
        submodules: units.len() - 1,
        ..Default::default()
    };
    if opts.ensure_gitignore {
        report.gitignore_added = ensure_gitignore(&repo, db_dir)?;
    }

    let mut pending: Vec<Pending> = Vec::new();
    {
        let r = db.read();
        for unit in units {
            let nested = prefixes
                .iter()
                .filter(|p| **p != unit.prefix && p.starts_with(&unit.prefix))
                .cloned()
                .collect();
            let (since, stale) = match r.node_ref(&unit.sync_key) {
                Some(n) => (
                    match n.prop("sha") {
                        Some(Value::Str(s)) if !s.is_empty() => Some(s),
                        _ => None,
                    },
                    marker_flag_props(&unit, opts)
                        .into_iter()
                        .any(|(k, v)| n.prop(&k) != Some(v)),
                ),
                None => (None, false),
            };
            pending.push(Pending {
                unit,
                since,
                stale,
                head: None,
                log: Vec::new(),
                nested,
            });
        }
    }
    // The repository itself is always the first unit, and it is what "this
    // database has seen this repo before" means.
    report.incremental = pending[0].since.is_some();

    for p in &mut pending {
        // Pin the end of the walk and the marker to one sha, resolved first. A
        // commit landing mid-run then falls outside this range instead of being
        // skipped by a marker that advanced past it.
        let Some(head) = head_sha(&p.unit.path)? else {
            continue; // this unit has no commits yet
        };
        let mut log = read_log(&p.unit.path, p.since.as_deref(), &head)?;
        localise(&mut log, &p.unit.prefix, &gitlink_paths(&p.unit.path));
        p.head = Some(head);
        p.log = log;
    }

    let prs = if opts.prs {
        fetch_prs(&repo)
    } else {
        Vec::new()
    };
    if prs.is_empty() && pending.iter().all(|p| p.log.is_empty() && !p.stale) {
        // Nothing new: leave the store untouched so `commit_seq` does not move.
        return Ok(report);
    }

    // Held for the rest of the run. Another process holding the store's
    // cross-process write lock is a retry, not a failure: nothing was written.
    let mut w = db.write_with_wait(WRITE_LOCK_WAIT).map_err(|e| match e {
        GraphError::Busy { .. } => CliError(BUSY_MESSAGE.to_string()),
        other => CliError(other.to_string()),
    })?;
    let ingest = IngestOptions::default(); // key `id`, auto-FK suffix `_id`

    // Pull requests first: their nodes are what `Commit.pr_id` resolves to.
    if !prs.is_empty() {
        ingest_prs(&mut w, &prs, &ingest, &mut report)?;
    }

    let mut authors: BTreeSet<String> = BTreeSet::new();
    let mut work = StructureWork::default();
    for p in &pending {
        if !p.log.is_empty() {
            ingest_unit(
                &mut w,
                p,
                opts,
                &ingest,
                &mut report,
                &mut authors,
                &mut work,
            )?;
        }
    }
    report.authors = authors.len();

    // Rules and fulltext, created after the data so each backfills once. They
    // span every unit, so they are declared once for the database rather than
    // once per repository walked.
    if report.commits > 0 && !w.rules().iter().any(|r| r.name == "co_changed") {
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
            if !w
                .fulltext_pairs()
                .contains(&(l.to_string(), field.to_string()))
            {
                w.enable_fulltext(l, field)?;
            }
        }
    }

    // The working tree, on top of the history. It runs after the commit walk
    // so every `File` node it reads exists, and its rules are declared after
    // its props so each one backfills exactly once.
    if opts.structure {
        // A first run has nothing to be incremental against, and a run whose
        // flags changed (structure or docs just turned on) has to revisit
        // files its predecessor deliberately skipped.
        let full = !report.incremental || pending.iter().any(|p| p.stale);
        report.structure = if full {
            structure::refresh_all(&mut w, &repo, "", opts.docs)?
        } else {
            let mut paths = work.touched;
            paths.extend(structure::importers_of(&w, &work.stale)?);
            let paths: Vec<String> = paths.into_iter().collect();
            structure::refresh_files(&mut w, &repo, "", &paths, opts.docs)?
        };
        report
            .rules_created
            .extend(structure::ensure_rules_and_fulltext(&mut w)?);
    }

    // Every commit this run could link is in the graph by now.
    if !prs.is_empty() {
        link_prs(&mut w, &prs, &ingest)?;
    }

    // The markers go last, once every phase of the run has succeeded. They say
    // how far a *complete* run got, so a failure anywhere above — a working-tree
    // batch that will not commit, a `gh` link pass that errors — leaves them
    // where they were and the next run re-walks the same window rather than
    // stepping over it. Re-walking a window that was partly applied is safe:
    // commits already in the graph are skipped as duplicate keys, file props
    // are rewritten from the recomputed state, and a rename whose node already
    // moved finds nothing to move.
    for p in &pending {
        write_marker(&mut w, p, opts)?;
    }
    Ok(report)
}

/// Wall-clock seconds since the Unix epoch, for [`SYNCED_AT`].
///
/// This is the one place the ingest reads a clock. A clock before the epoch
/// reads as `0` rather than going negative.
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Marker prop holding when this store last took data from the repository.
///
/// `Commit.ts` says when the work was *written*, which on a store synced to
/// its repository's head is indistinguishable from now — so it cannot answer
/// "how stale is my graph". This can. Absent on a store built before it
/// existed, which readers must tolerate.
pub const SYNCED_AT: &str = "synced_at";

/// Record how far this unit got, and under what flags.
///
/// Props are written only where they differ, so a run that touches one unit
/// does not churn the markers of the others — and a run that changes nothing
/// writes nothing at all, [`SYNCED_AT`] included. The stamp therefore means
/// "when this store last took something new", which is what a reader wants
/// from it; a no-op re-run leaving it alone is the point, not a gap.
fn write_marker(w: &mut WriteGuard<'_>, p: &Pending, opts: &IngestGitOpts) -> Result<(), CliError> {
    let Some(head) = p.head.as_deref() else {
        return Ok(()); // no commits, so nothing to resume from
    };
    let mut props = marker_flag_props(&p.unit, opts);
    if !p.log.is_empty() {
        props.push(("sha".into(), Value::Str(head.to_string())));
    }
    let key = p.unit.sync_key.clone();
    if w.has_node(&key) {
        let mut changed = false;
        for (k, v) in props {
            let current = w.node_ref(&key).and_then(|n| n.prop(&k));
            if current.as_ref() != Some(&v) {
                w.set_prop(&key, &k, v)?;
                changed = true;
            }
        }
        if changed {
            w.set_prop(&key, SYNCED_AT, Value::Int(now_unix()))?;
        }
        return Ok(());
    }
    if p.log.is_empty() {
        return Ok(());
    }
    // It carries `id` like every other label here, so the key is readable from
    // Cypher.
    props.push(("id".into(), Value::Str(key.clone())));
    props.push((SYNCED_AT.into(), Value::Int(now_unix())));
    props.sort_by(|a, b| a.0.cmp(&b.0));
    w.insert_node("GitSync", &key, props)?;
    Ok(())
}

/// Walk one unit's new commits into the graph.
///
/// Every path in `p.log` is already a repository-wide key, so this is the
/// single-repository algorithm unchanged: only the `File` state it starts from
/// and the counts it adds to are scoped to the unit.
fn ingest_unit(
    w: &mut WriteGuard<'_>,
    p: &Pending,
    opts: &IngestGitOpts,
    ingest: &IngestOptions,
    report: &mut IngestGitReport,
    authors: &mut BTreeSet<String>,
    work: &mut StructureWork,
) -> Result<(), CliError> {
    let log = &p.log;
    let incremental = p.since.is_some();

    let mut walk = Walk {
        files: if incremental {
            let params =
                BTreeMap::from([("prefix".to_string(), Value::Str(p.unit.prefix.clone()))]);
            file_state_from(&w.query(FILE_STATE_QUERY, &params)?, &p.nested)
        } else {
            BTreeMap::new()
        },
        ..Default::default()
    };

    for c in log {
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

    // What the working-tree pass has to look at afterwards: the paths this
    // window changed, and the keys it left pointing at nothing. A key that
    // ended the window live again — a file moved away and back — is neither.
    work.touched.extend(walk.dirty.iter().cloned());
    for key in walk.deleted.iter().chain(walk.alias.keys()) {
        if !walk.files.contains_key(key) {
            work.stale.insert(key.clone());
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
    let a = w.ingest_with_edges("Author", author_rows, ingest, &[])?;
    report.rules_created.extend(a.rules_created);
    authors.extend(walk.authors.keys().cloned());

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
    //    Updates to existing nodes go in one batch rather than one WAL commit
    //    per property. Every frame in the WAL is replayed on every later open,
    //    and each one re-fires the rules watching the property it carries, so a
    //    run that appends a frame per property makes every subsequent open
    //    slower for as long as that frame lives. The op order inside the batch
    //    is the order the individual writes had, so the resulting state is the
    //    same one either form produces.
    let mut new_file_rows = Vec::new();
    let mut updates = Vec::new();
    let mut written = 0usize;
    for (path, st) in &walk.files {
        if incremental && !walk.dirty.contains(path) {
            continue;
        }
        written += 1;
        let props = file_props(st, path);
        if w.has_node(path) {
            updates.extend(props.into_iter().map(|(k, v)| (path.clone(), k, v)));
        } else {
            new_file_rows.push(props.into_iter().collect::<BTreeMap<_, _>>());
        }
    }
    if !updates.is_empty() {
        let mut b = w.batch();
        for (key, field, value) in updates {
            b.set_prop(&key, &field, value);
        }
        b.commit()?;
    }
    let f = w.ingest_with_edges("File", new_file_rows, ingest, &[])?;
    report.rules_created.extend(f.rules_created);
    // What this run wrote, not every file it has ever seen: an incremental run
    // loads the whole known file set to fold the new commits into it, and
    // reporting that set made a one-file run print the size of the repository.
    report.files += written;

    // 4. Commits, then their TOUCHED edges. The two must be separate batches:
    //    a batch that both inserts nodes firing a new rule and carries a user
    //    edge of a not-yet-interned type writes a WAL frame that cannot be
    //    replayed (`Intern` records are emitted in a pre-pass, but on replay the
    //    rule fires — and interns its edge type — before the later `Intern`
    //    record is read). See the report for a reproducer.
    let c = w.ingest_with_edges("Commit", walk.commit_rows, ingest, &[])?;
    report.rules_created.extend(c.rules_created);
    report.commits += log.len();

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
        w.ingest_with_edges("Commit", Vec::new(), ingest, &touched)?;
    }
    Ok(())
}

// ── sync and touch ──────────────────────────────────────────────────────────
//
// `ingest-git` is the command a person runs. These two are what a *hook* runs:
// `sync` after a commit lands, `touch` after a single file is edited. Both read
// the repository out of the `GitSync` marker rather than taking it as an
// argument, so a hook line carries only the database path and keeps working
// when the checkout moves.

/// What a store with no `GitSync` node is told. Naming the fix matters: this is
/// the error a hook installed against the wrong database prints.
const NO_MARKER: &str = "store has no git sync marker; run ingest-git first";

/// The `GitSync` props that say how this store was built, read back so a later
/// `sync` repeats the same run without being told any of it again.
///
/// `exclude` and `max_commits_per_file` are not on the marker, so a `sync`
/// applies [`DEFAULT_EXCLUDES`] and [`DEFAULT_MAX_COMMITS_PER_FILE`]. A store
/// first built with custom `--exclude` patterns should keep being maintained
/// with `ingest-git`, which takes them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SyncMarker {
    repo: PathBuf,
    recurse: bool,
    prs: bool,
    structure: bool,
    docs: bool,
}

impl SyncMarker {
    fn opts(&self) -> IngestGitOpts {
        IngestGitOpts {
            repo: self.repo.clone(),
            exclude: DEFAULT_EXCLUDES.iter().map(|p| (*p).to_string()).collect(),
            max_commits_per_file: DEFAULT_MAX_COMMITS_PER_FILE,
            recurse_submodules: self.recurse,
            prs: self.prs,
            structure: self.structure,
            docs: self.docs,
            ensure_gitignore: false,
        }
    }
}

/// Read the marker off an already-open handle.
///
/// Deliberately not "open the store and read the marker": opening this store is
/// by far the most expensive thing either command does — it replays the whole
/// WAL — so both of them open once and read the marker through that same
/// handle. A separate read-only open just to learn the repository path would
/// double the cost of every hook invocation.
fn marker_of(r: &structure::Db) -> Result<SyncMarker, CliError> {
    let node = r
        .node_ref(SYNC_KEY)
        .ok_or_else(|| CliError(NO_MARKER.into()))?;
    let flag = |name: &str| matches!(node.prop(name), Some(Value::Bool(true)));
    match node.prop("repo") {
        Some(Value::Str(repo)) if !repo.is_empty() => Ok(SyncMarker {
            repo: PathBuf::from(repo),
            recurse: flag("recurse"),
            prs: flag("prs"),
            // A marker written before these two flags existed carries neither,
            // and a working-tree pass is what such a run did.
            structure: node.prop("structure") != Some(Value::Bool(false)),
            docs: node.prop("docs") != Some(Value::Bool(false)),
        }),
        _ => Err(CliError(NO_MARKER.into())),
    }
}

/// Open a store that must already exist.
///
/// `SharedDb::open` runs `create_dir_all`, so without this guard a hook line
/// carrying a typo'd path would keep creating empty databases and reporting
/// that they hold no marker — the same trap [`run_recall`] guards against.
///
/// [`run_recall`]: crate::recall::run_recall
fn open_existing(db_dir: &Path) -> Result<SharedDb, CliError> {
    if !db_dir.exists() {
        return Err(CliError(format!(
            "no database directory at {}",
            db_dir.display()
        )));
    }
    Ok(SharedDb::open(db_dir)?)
}

/// What one [`run_sync`] did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SyncReport {
    /// The incremental history walk.
    pub git: IngestGitReport,
    /// The working-tree pass over the dirty paths, which the history walk does
    /// not see: an edit that has not been committed is in no commit.
    pub structure: crate::structure::StructureReport,
    /// Dirty paths handed to that pass. Higher than `structure.files_scanned`
    /// when some of them are new to the graph, or no longer on disk.
    pub dirty_refreshed: usize,
}

/// Paths that differ from `HEAD` or are not tracked at all, repository-relative
/// and sorted.
///
/// `-z` rather than the default listing: git escapes and quotes a path holding
/// a tab, a newline or a non-ASCII byte, and a quoted path matches no key.
fn dirty_paths(repo: &Path, exclude: &[String]) -> Result<Vec<String>, CliError> {
    const LISTS: [&[&str]; 2] = [
        &["diff", "--name-only", "-z", "HEAD"],
        &["ls-files", "--others", "--exclude-standard", "-z"],
    ];
    let mut out = BTreeSet::new();
    for args in LISTS {
        let o = git_output(repo, args)?;
        if !o.status.success() {
            // `diff HEAD` fails in a repository with no commits yet. Nothing is
            // dirty relative to a head that does not exist.
            continue;
        }
        for path in String::from_utf8_lossy(&o.stdout).split('\0') {
            if path.is_empty() || excluded(path, exclude) {
                continue;
            }
            out.insert(path.to_string());
        }
    }
    Ok(out.into_iter().collect())
}

/// Bring the store up to date with the repository it was built from: the
/// commits since the marker, then the working tree where it differs from
/// `HEAD`.
///
/// The second half is what a plain `ingest-git` cannot do. Its working-tree
/// pass only visits the paths the *commits* touched, so a file edited and not
/// yet committed keeps whatever the graph last recorded about it. A hook that
/// runs on every commit wants the uncommitted remainder refreshed too.
pub fn run_sync(db_dir: &Path) -> Result<SyncReport, CliError> {
    // One handle for the whole run. It is opened before `run_ingest_git`, which
    // opens its own and commits through it, but a `SharedDb` refreshes off the
    // WAL when a write scope is entered — so the dirty pass below sees every
    // commit the ingest just made without this handle being reopened.
    let db = open_existing(db_dir)?;
    let marker = marker_of(&db.read())?;
    let opts = marker.opts();
    let mut report = SyncReport {
        git: run_ingest_git(db_dir, &opts)?,
        ..Default::default()
    };
    if !opts.structure {
        return Ok(report);
    }

    // Only the root repository's working tree. A submodule's dirty files are
    // its own checkout's business, and `--recurse-submodules` resumes each unit
    // from its own marker on the next commit there.
    let repo = canonical(&marker.repo);
    let paths = dirty_paths(&repo, &opts.exclude)?;
    report.dirty_refreshed = paths.len();
    if paths.is_empty() {
        // Nothing to refresh: never enter a write scope, so the run takes no
        // lock and `commit_seq` cannot move.
        return Ok(report);
    }

    let mut w = db.write_with_wait(WRITE_LOCK_WAIT).map_err(|e| match e {
        GraphError::Busy { .. } => CliError(BUSY_MESSAGE.to_string()),
        other => CliError(other.to_string()),
    })?;
    report.structure = structure::refresh_files(&mut w, &repo, "", &paths, opts.docs)?;
    Ok(report)
}

pub fn format_touch(r: &structure::StructureReport) -> String {
    format!(
        "touch: {} file(s), {} symbol(s), {} import(s), {} call(s), {} mention(s)\n",
        r.files_scanned, r.symbols, r.imports, r.calls, r.mentions
    )
}

pub fn format_sync(r: &SyncReport) -> String {
    let mut out = format_ingest_git(&r.git);
    let s = &r.structure;
    out.push_str(&format!(
        "  dirty {} path(s): scanned {}, {} symbol(s), {} import(s), {} call(s)\n",
        r.dirty_refreshed, s.files_scanned, s.symbols, s.imports, s.calls
    ));
    out
}

/// Re-extract exactly the files named, and nothing else.
///
/// `files` comes from argv when a caller has the paths; otherwise they are read
/// out of a `PostToolUse` hook payload on stdin, the same way [`run_recall`]
/// reads a prompt. Anything that is not a working-tree file this store already
/// knows — a path outside the repository, an excluded one, one the graph has
/// never seen — is dropped without comment, because a hook fires on every edit
/// the assistant makes and most of them are none of this store's business.
///
/// [`run_recall`]: crate::recall::run_recall
pub fn run_touch(
    db_dir: &Path,
    files: &[PathBuf],
    hook_stdin: Option<&str>,
) -> Result<structure::StructureReport, CliError> {
    let named: Vec<PathBuf> = if files.is_empty() {
        hook_stdin.map(paths_from_payload).unwrap_or_default()
    } else {
        files.to_vec()
    };
    if named.is_empty() {
        return Ok(structure::StructureReport::default());
    }

    let db = open_existing(db_dir)?;
    let marker = marker_of(&db.read())?;
    if !marker.structure {
        // The store was built with `--no-structure`, so it holds no working-tree
        // props at all and re-extracting one file would be the only exception.
        return Ok(structure::StructureReport::default());
    }
    let repo = canonical(&marker.repo);
    let exclude: Vec<String> = DEFAULT_EXCLUDES.iter().map(|p| (*p).to_string()).collect();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut paths: BTreeSet<String> = BTreeSet::new();
    for path in &named {
        let Some(rel) = repo_relative(&repo, &cwd, path) else {
            continue;
        };
        if !excluded(&rel, &exclude) {
            paths.insert(rel);
        }
    }
    if paths.is_empty() {
        // Nothing of ours changed: never enter a write scope, so no lock is
        // taken and a running writer is never made to wait.
        return Ok(structure::StructureReport::default());
    }

    let paths: Vec<String> = paths.into_iter().collect();
    let mut w = db.write_with_wait(WRITE_LOCK_WAIT).map_err(|e| match e {
        GraphError::Busy { .. } => CliError(BUSY_MESSAGE.to_string()),
        other => CliError(other.to_string()),
    })?;
    structure::refresh_files(&mut w, &repo, "", &paths, marker.docs)
}

/// The file paths in a `PostToolUse` payload.
///
/// `tool_input.file_path` covers Edit and Write; `tool_input.edits[].file_path`
/// covers the multi-edit shape. Both are read, so a payload carrying either (or
/// both) is handled without knowing which tool produced it. A payload that is
/// not JSON, or that names no file, yields nothing — never an error.
fn paths_from_payload(raw: &str) -> Vec<PathBuf> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
        return Vec::new();
    };
    let input = &v["tool_input"];
    let mut out = Vec::new();
    let mut push = |value: &serde_json::Value| {
        if let Some(s) = value.as_str().map(str::trim).filter(|s| !s.is_empty()) {
            out.push(PathBuf::from(s));
        }
    };
    push(&input["file_path"]);
    if let Some(edits) = input["edits"].as_array() {
        for e in edits {
            push(&e["file_path"]);
        }
    }
    out
}

/// `path` as a key under `repo`, or `None` when it is not inside it.
///
/// A hook payload carries absolute paths and a person typing the command uses
/// relative ones, so a relative path is taken against `cwd`. Both sides are
/// then resolved through symlinks before comparing: the marker records the
/// canonical repository path, and on macOS a checkout under `/tmp` is reached
/// through a symlink that never compares equal as written.
fn repo_relative(repo: &Path, cwd: &Path, path: &Path) -> Option<String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let resolved = resolve_symlinks(&absolute);
    let rel = resolved.strip_prefix(repo).ok()?;
    let key: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let key = key.join("/");
    (!key.is_empty()).then_some(key)
}

/// [`canonical`] that still works for a path that no longer exists: a file
/// deleted between the edit and the hook resolves through its parent directory.
fn resolve_symlinks(p: &Path) -> PathBuf {
    if let Ok(resolved) = std::fs::canonicalize(p) {
        return resolved;
    }
    match (p.parent(), p.file_name()) {
        (Some(dir), Some(name)) => std::fs::canonicalize(dir)
            .map(|d| d.join(name))
            .unwrap_or_else(|_| p.to_path_buf()),
        _ => p.to_path_buf(),
    }
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
    if r.submodules + r.prs > 0 {
        out.push_str(&format!(
            "  submodules {}  pull requests {}\n",
            r.submodules, r.prs
        ));
    }
    let s = &r.structure;
    if s.files_scanned > 0 {
        out.push_str(&format!(
            "  scanned {} file(s): {} symbol(s), {} import(s), {} call(s), {} mention(s)\n",
            s.files_scanned, s.symbols, s.imports, s.calls, s.mentions
        ));
        if s.skipped_large + s.symbols_capped > 0 {
            out.push_str(&format!(
                "  hash-only {}  symbol cap hit on {}\n",
                s.skipped_large, s.symbols_capped
            ));
        }
    }
    if r.gitignore_added {
        out.push_str("  added the database directory to .gitignore\n");
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

    /// A `*.` pattern is a file-name suffix, so a compound one works. Reading
    /// only the last dot segment would make `*.min.js` — a default — inert,
    /// and generated bundles are both the largest files in a tree and the ones
    /// least worth parsing.
    #[test]
    fn a_compound_suffix_pattern_matches() {
        let defaults: Vec<String> = DEFAULT_EXCLUDES.iter().map(|p| (*p).to_string()).collect();
        // Not under `dist/`, so only the suffix rule can match it.
        assert!(excluded("ui/build/bundle.min.js", &defaults));
        assert!(excluded("bundle.min.js", &defaults));
        assert!(
            !excluded("ui/src/app.js", &defaults),
            "an ordinary source file is not a bundle"
        );
        assert!(!excluded("ui/src/minify.js", &defaults));
        // The single-extension form is unchanged, and a bare suffix is not a
        // match: `*.lock` means something *dot* lock.
        assert!(excluded("Cargo.lock", &defaults));
        assert!(!excluded(".lock", &defaults));
        assert!(!excluded("src/lib.rs", &defaults));
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
