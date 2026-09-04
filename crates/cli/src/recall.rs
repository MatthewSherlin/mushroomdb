//! `mushroomdb recall <db>`: the body of the UserPromptSubmit hook.
//!
//! The hook has two things to say, and says whichever one the moment calls
//! for.
//!
//! When the prompt arrives from a checkout with a **dirty working tree**, the
//! change already in progress is the more useful subject: the nudge names what
//! those files reach that is *not* already open — the files that usually change
//! with them, the files that import them, who owns them, and whether a learned
//! concept has just gone out of date. That is what an assistant would otherwise
//! only find out by reading half the repository.
//!
//! Otherwise the prompt's own words are all there is to go on, and the topic
//! digest answers: the nodes closest to it and their strongest edges. The
//! digest itself is [`core_api::repograph::recall_digest`].
//!
//! Everything specific to being a hook stays here: reading the payload, opening
//! the store read-only, keeping inside one byte budget, and staying silent on
//! any error. A recall hook must never block or slow the user's prompt.
use core_api::repograph::{
    impact, path_excluded, recall_digest, sanitize, stale_concepts, FileImpact, ImpactOptions,
    ImpactReport, DEFAULT_EXCLUDES, HINT, MAX_OUTPUT_BYTES, UNTRUSTED_FRAMING,
};
use core_api::{GraphDb, OpenOptions, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Lines the nudge prints under the framing line, its closing hint included.
/// Past this it stops being a nudge and becomes something to read.
const MAX_NUDGE_LINES: usize = 8;
/// Files named on the `usually changes with:` line.
const MAX_NUDGE_PARTNERS: usize = 3;
/// Files named on the `imported by:` line.
const MAX_NUDGE_IMPORTERS: usize = 3;
/// Changed files the nudge asks the graph about. A rebase or a generated
/// commit can dirty thousands of paths, and this hook has five seconds; the
/// count in the first line still reports the whole diff.
const MAX_NUDGE_FILES: usize = 50;

/// Extract the prompt text from a hook payload. Accepts `prompt`,
/// `user_prompt`, and `user_input` (the docs disagree on the field name).
fn prompt_from_payload(raw: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    for k in ["prompt", "user_prompt", "user_input"] {
        if let Some(s) = v.get(k).and_then(|x| x.as_str()) {
            let s = s.trim();
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// The directory the payload says the prompt was sent from.
fn cwd_from_payload(raw: &str) -> Option<PathBuf> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let s = v.get("cwd").and_then(|x| x.as_str())?.trim();
    (!s.is_empty()).then(|| PathBuf::from(s))
}

/// Rewrite free-form prompt text as a full-text OR query.
///
/// The rewrite itself lives in `core_api::repograph::or_query`, because the
/// `recall` MCP tool applies it to its `topic` argument and the two must not
/// disagree about what a prompt means. The tests below stay here: this is the
/// caller whose behaviour they describe.
fn fulltext_or_query(prompt: &str) -> Option<String> {
    core_api::repograph::or_query(prompt)
}

pub fn run_recall(db_dir: &Path, hook_stdin: &str) -> String {
    let Some(prompt) = prompt_from_payload(hook_stdin)
        .as_deref()
        .and_then(fulltext_or_query)
    else {
        return String::new();
    };
    // Guard the open: `RealFs::new` runs `create_dir_all`, so without this a
    // hook pointed at a typo'd path would keep creating empty directories.
    if !db_dir.exists() {
        return String::new();
    }
    // Read-only, with both write flags off as well. `auto_migrate` rewrites an
    // old-format snapshot and deletes a stale `.bak`; `repair_wal` writes the
    // valid prefix back over a torn tail. A digest that fires on every prompt,
    // under a 5 s kill, must never write to the user's store: a `serve`
    // mid-append would lose a frame it believes durable. `read_only` also keeps
    // the hook off the cross-process write lock entirely, so it can never make
    // a writer wait and never fails because one is running. The valid prefix is
    // still replayed in memory.
    let Ok(db) = GraphDb::open_with_options(
        db_dir,
        OpenOptions {
            auto_migrate: false,
            repair_wal: false,
            read_only: true,
        },
    ) else {
        return String::new();
    };
    // The change in progress outranks the prompt's own words: it is both more
    // specific and about to be wrong if nobody says otherwise. With no change
    // to report — a clean tree, a prompt sent from outside a checkout, a diff
    // the graph knows nothing about — the topic digest answers as it always
    // did.
    if let Some(nudge) = diff_nudge(
        &db,
        hook_stdin,
        std::env::var_os("CLAUDE_PROJECT_DIR").as_deref(),
    ) {
        return nudge;
    }
    recall_digest(
        &db,
        &prompt,
        &db_dir.display().to_string(),
        MAX_OUTPUT_BYTES,
    )
}

// ── the diff-aware nudge ────────────────────────────────────────────────────

/// The nudge for whatever is dirty in the checkout this prompt came from, or
/// `None` when there is nothing to nudge about.
fn diff_nudge(
    db: &crate::structure::Db,
    hook_stdin: &str,
    project_dir: Option<&OsStr>,
) -> Option<String> {
    let root = nudge_root(db, hook_stdin, project_dir)?;
    let changed = changed_paths(&root);
    if changed.is_empty() {
        return None;
    }
    // The whole change decides the `modified` flag: a partner that is itself
    // being edited is a different fact from one that is not, and only this set
    // tells them apart. The graph is asked about a bounded prefix of it.
    let modified: BTreeSet<String> = changed.iter().cloned().collect();
    let asked: Vec<String> = changed.iter().take(MAX_NUDGE_FILES).cloned().collect();
    let report = impact(db, &asked, &modified, &ImpactOptions::default());
    render_nudge(db, &report, &modified, &changed)
}

/// The checkout the nudge reports on.
///
/// The payload's `cwd` is where the host says the prompt was sent from, and it
/// decides outright: a prompt sent from outside a checkout is not about a diff,
/// even when the store knows a repository that has one. `$CLAUDE_PROJECT_DIR`
/// stands in for a host that sends no `cwd`, and the store's own `GitSync`
/// marker for one that sets neither.
fn nudge_root(
    db: &crate::structure::Db,
    hook_stdin: &str,
    project_dir: Option<&OsStr>,
) -> Option<PathBuf> {
    if let Some(cwd) = cwd_from_payload(hook_stdin) {
        return repo_root(&cwd);
    }
    if let Some(dir) = project_dir.filter(|d| !d.is_empty()) {
        if let Some(root) = repo_root(Path::new(dir)) {
            return Some(root);
        }
    }
    let repo = match db
        .node_ref(crate::ingest_git::SYNC_KEY)
        .and_then(|n| n.prop("repo"))
    {
        Some(Value::Str(s)) => s,
        _ => return None,
    };
    repo_root(Path::new(&repo))
}

/// The root of the checkout `dir` is in, or `None` when it is not in one.
fn repo_root(dir: &Path) -> Option<PathBuf> {
    if !dir.is_dir() {
        return None;
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!root.is_empty()).then(|| PathBuf::from(root))
}

/// Paths under `root` that differ from `HEAD` or are not tracked at all:
/// root-relative, sorted, deduplicated, and filtered by the same
/// [`DEFAULT_EXCLUDES`] the ingest applied — a path the ingest skipped has no
/// `File` node to say anything about.
///
/// The same listing the `impact` MCP tool builds its default file set from,
/// implemented again here because this crate cannot depend on the server crate.
/// Empty on any failure: a hook has nothing to say about a repository git
/// cannot read.
///
/// `-z` rather than the default listing: git escapes and quotes a path holding
/// a tab, a newline or a non-ASCII byte, and a quoted path matches no key.
/// `root` rather than the directory the prompt came from: `ls-files` lists
/// relative to the working directory while `diff` lists relative to the root,
/// so running both anywhere else would mix two conventions in one list.
fn changed_paths(root: &Path) -> Vec<String> {
    const LISTS: [&[&str]; 2] = [
        &["diff", "--name-only", "-z", "HEAD"],
        &["ls-files", "--others", "--exclude-standard", "-z"],
    ];
    let excludes: Vec<String> = DEFAULT_EXCLUDES.iter().map(|p| (*p).to_string()).collect();
    let mut out: BTreeSet<String> = BTreeSet::new();
    for args in LISTS {
        let Ok(output) = Command::new("git").arg("-C").arg(root).args(args).output() else {
            return Vec::new();
        };
        // `diff HEAD` fails in a repository with no commits yet. Nothing is
        // dirty relative to a head that does not exist, so that is not an
        // error — the other listing still answers.
        if !output.status.success() {
            continue;
        }
        for path in String::from_utf8_lossy(&output.stdout).split('\0') {
            if !path.is_empty() && !path_excluded(path, &excludes) {
                out.insert(path.to_string());
            }
        }
    }
    out.into_iter().collect()
}

/// The nudge itself, or `None` when the graph knows none of the changed files
/// — which is what a store built from a different repository, or one that has
/// never been synced, looks like.
///
/// Every line is a fact about the change as a whole rather than about one file
/// in it: the diff is what the assistant is about to work on, and which of its
/// files a partner belongs to is a detail the `impact` tool answers on demand.
/// Partners and importers already in the diff are dropped rather than marked,
/// because the point of the nudge is what is *not* open yet.
fn render_nudge(
    db: &crate::structure::Db,
    report: &ImpactReport,
    modified: &BTreeSet<String>,
    changed: &[String],
) -> Option<String> {
    if report.files.is_empty() {
        return None;
    }
    let first = changed.first()?;
    let mut lines: Vec<String> = Vec::new();
    let more = changed.len() - 1;
    lines.push(match more {
        0 => format!("mushroomdb: you are editing {}", sanitize(first)),
        n => format!(
            "mushroomdb: you are editing {} (+{n} more)",
            sanitize(first)
        ),
    });

    // Best co-change score per file across the whole diff, strongest first.
    let mut partners: BTreeMap<String, f64> = BTreeMap::new();
    let mut importers: BTreeSet<String> = BTreeSet::new();
    for f in &report.files {
        for p in f.partners.iter().filter(|p| !p.modified) {
            let slot = partners.entry(p.path.clone()).or_insert(p.score);
            if p.score > *slot {
                *slot = p.score;
            }
        }
        for p in f.importers.iter().filter(|p| !p.modified) {
            importers.insert(p.path.clone());
        }
    }
    let mut ranked: Vec<(String, f64)> = partners.into_iter().collect();
    // Score descending, then key ascending: `BTreeMap` gave us the key order,
    // and a stable sort keeps it inside a tie.
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    if !ranked.is_empty() {
        let items: Vec<String> = ranked
            .iter()
            .take(MAX_NUDGE_PARTNERS)
            .map(|(path, score)| format!("{path} ({score:.2}, not modified)"))
            .collect();
        lines.push(format!("  usually changes with: {}", items.join(", ")));
    }
    if !importers.is_empty() {
        let items: Vec<String> = importers
            .iter()
            .take(MAX_NUDGE_IMPORTERS)
            .map(|path| format!("{path} (not modified)"))
            .collect();
        lines.push(format!("  imported by: {}", items.join(", ")));
    }
    if let Some(owner) = owner_of(&report.files) {
        lines.push(format!("  owner: {owner}"));
    }
    let stale = stale_concepts_describing(db, modified);
    if stale > 0 {
        lines.push(format!(
            "  {stale} concept(s) describe files you changed — say \"re-learn\" to refresh"
        ));
    }

    // The framing line and the hint are the two the nudge cannot do without,
    // so the body gives way to them — first to the line cap, then to the byte
    // budget the topic digest is held to.
    lines.truncate(MAX_NUDGE_LINES - 1);
    loop {
        let mut out = String::from(UNTRUSTED_FRAMING);
        for l in &lines {
            let _ = writeln!(out, "{l}");
        }
        out.push_str(HINT);
        if out.len() <= MAX_OUTPUT_BYTES {
            return Some(out);
        }
        if lines.len() <= 1 {
            // Not even the first line fits, which takes a pathological path to
            // manage. The topic digest is the better answer than a truncated
            // one.
            return None;
        }
        lines.pop();
    }
}

/// Who the changed files belong to: the author owning most of them, ties
/// broken by name so the line is the same on every run. One name, because
/// "who do I ask about this change" has one useful answer.
fn owner_of(files: &[FileImpact]) -> Option<String> {
    let mut counts: BTreeMap<&String, usize> = BTreeMap::new();
    for owner in files.iter().filter_map(|f| f.owner.as_ref()) {
        *counts.entry(owner).or_default() += 1;
    }
    counts
        .into_iter()
        .max_by_key(|(name, count)| (*count, std::cmp::Reverse(*name)))
        .map(|(name, _)| name.clone())
}

/// How many stale concepts were learned from a file in this diff.
///
/// Staleness is [`stale_concepts`]'s decision — a recorded source hash that no
/// longer matches the `File` — and the diff narrows it to the concepts this
/// change is responsible for. A concept that went stale for some other file is
/// somebody else's re-learn.
fn stale_concepts_describing(db: &crate::structure::Db, modified: &BTreeSet<String>) -> usize {
    stale_concepts(db)
        .iter()
        .filter(
            |(key, _)| match db.node_ref(key).and_then(|n| n.prop("source_files")) {
                Some(Value::List(sources)) => sources.iter().any(|v| match v {
                    Value::Str(s) => modified.contains(s),
                    _ => false,
                }),
                _ => false,
            },
        )
        .count()
}

#[cfg(test)]
mod tests {
    use super::{fulltext_or_query, prompt_from_payload};
    use core_api::repograph::MAX_QUERY_TERMS;

    #[test]
    fn prompt_is_read_from_any_of_the_three_documented_fields() {
        for field in ["prompt", "user_prompt", "user_input"] {
            let payload = format!(r#"{{"{field}":"  hello  "}}"#);
            assert_eq!(prompt_from_payload(&payload).as_deref(), Some("hello"));
        }
        assert_eq!(prompt_from_payload(r#"{"prompt":"   "}"#), None);
        assert_eq!(prompt_from_payload(r#"{"other":"hi"}"#), None);
        assert_eq!(prompt_from_payload("not json"), None);
    }

    #[test]
    fn prompt_becomes_an_or_query_of_lowercased_alphanumeric_terms() {
        assert_eq!(
            fulltext_or_query("What about Person 1 and Project 5?").as_deref(),
            Some("what OR about OR person OR 1 OR project OR 5"),
        );
    }

    #[test]
    fn or_query_drops_query_keywords_repeats_and_punctuation() {
        // `and`/`or` are grammar keywords; `-x` would negate and `x*` prefix-match,
        // so splitting on non-alphanumerics is what keeps them inert.
        assert_eq!(
            fulltext_or_query("AND or foo-bar foo baz*").as_deref(),
            Some("foo OR bar OR baz"),
        );
        assert_eq!(fulltext_or_query("  ?! ,, "), None);
    }

    #[test]
    fn or_query_caps_the_number_of_terms() {
        let prompt: String = (0..MAX_QUERY_TERMS + 10)
            .map(|i| format!("w{i} "))
            .collect();
        let q = fulltext_or_query(&prompt).expect("terms");
        assert_eq!(q.split(" OR ").count(), MAX_QUERY_TERMS);
    }
}
