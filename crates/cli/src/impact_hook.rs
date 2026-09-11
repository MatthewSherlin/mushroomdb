//! `mushroomdb impact-hook` — the optional `PreToolUse` hook body for edits.
//!
//! Claude Code runs this before an `Edit`, a `Write` or a `MultiEdit`, handing
//! it the tool call as JSON on stdin. The file about to change has a blast
//! radius the graph already knows — what imports it, what usually changes with
//! it, which tests cover it — and that is exactly the thing an assistant
//! otherwise finds out by reading half the repository, or does not find out at
//! all. So the hook says it, in one line, before the edit lands.
//!
//! # Why it never blocks
//!
//! Unlike [`crate::intercept`], this hook has no opinion about whether the
//! edit should happen: it exits 0 always, and the only thing it can do is put
//! a sentence in front of the change. That is what makes the budget the real
//! constraint rather than the accuracy — a wrong redirect costs a tool call, a
//! wrong line of context costs [`MAX_CONTEXT_BYTES`] bytes of attention.
//!
//! Every failure is silence: a payload that will not parse, a store that is
//! missing or busy, a path the graph has no `File` for. Silence here means the
//! edit proceeds exactly as it would with no hook installed.

use crate::hook::{cut_to, open_for_hook};
use core_api::repograph::{impact, ImpactOptions, ImpactReport};
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// The most this hook may add to a turn. It fires before every edit, so the
/// line has to be worth reading at a glance and cheap enough to pay for on a
/// long editing session; past this it stops being a nudge and becomes
/// something to skim past.
pub const MAX_CONTEXT_BYTES: usize = 600;

/// Paths named on each of the three lists. Three is what fits alongside the
/// counts inside the budget, and a fourth co-change partner has never been the
/// one that decided anything.
const MAX_NAMED: usize = 3;

/// The payload's edited file, as written. `None` for a missing or non-string
/// `tool_input.file_path` — neither needs the store to be opened.
fn edited_path(input: &serde_json::Value) -> Option<&str> {
    input["tool_input"]["file_path"]
        .as_str()
        .filter(|p| !p.trim().is_empty())
}

/// Whether `path` looks like a test rather than a dependency.
///
/// A heuristic on the path alone, because that is all the graph stores about a
/// file's role. It only ever decides which of three lists a path is printed
/// on, so a miss costs a reader nothing but a slightly worse label.
fn looks_like_test(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.starts_with("test/")
        || lower.starts_with("tests/")
        || lower.contains("/test/")
        || lower.contains("/tests/")
        || lower.contains("_test.")
        || lower.contains(".test.")
        || lower.contains("/test_")
        || lower.starts_with("test_")
        || lower.contains("_spec.")
        || lower.contains(".spec.")
}

/// The store's own idea of where the repository is, as the `ingest-git` marker
/// recorded it.
fn recorded_repo(db: &crate::structure::Db) -> Option<PathBuf> {
    match db
        .node_ref(crate::ingest_git::SYNC_KEY)
        .and_then(|n| n.prop("repo"))
    {
        Some(core_api::Value::Str(s)) if !s.is_empty() => Some(PathBuf::from(s)),
        _ => None,
    }
}

/// The key the graph would hold for `path`, which is always repo-relative with
/// `/` separators.
///
/// Claude Code sends an absolute path, so the common case is stripping the
/// recorded repository root off the front. A relative path is taken as already
/// relative to that root — that is the only root it could be relative to — and
/// a leading `./` is dropped either way. An absolute path *outside* the
/// recorded repository belongs to some other checkout and resolves to nothing:
/// answering about a same-named file in this one would be worse than silence.
///
/// # It is lexical, deliberately
///
/// Nothing here touches the filesystem: no `canonicalize`, no `metadata`, no
/// symlink resolution. Two consequences a caller should know about, both of
/// which end in silence rather than in a wrong answer:
///
/// - `..` components are **dropped**, not resolved. `a/../b/x.rs` becomes
///   `a/b/x.rs`, which is very unlikely to be a key the graph holds, so such a
///   path simply finds nothing.
/// - A path that reaches the repository by a **different spelling** than the
///   recorded root — through a symlink, or macOS's `/var` → `/private/var` —
///   does not match the prefix and finds nothing either.
///
/// Resolving either would mean `stat`-ing paths inside a hook that runs before
/// every edit, to rescue cases a host does not produce: Claude Code sends the
/// path it opened the file at, and the marker records the root `git rev-parse`
/// printed. Silence on the odd one out is the cheaper trade.
fn repo_relative(path: &str, repo: Option<&Path>) -> Option<String> {
    let p = Path::new(path);
    let rel = if p.is_absolute() {
        p.strip_prefix(repo?).ok()?
    } else {
        p.strip_prefix("./").unwrap_or(p)
    };
    let joined = rel
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/");
    (!joined.is_empty()).then_some(joined)
}

/// The blast radius of editing `path`, in one line, or `None` when the graph
/// has nothing to say about it.
///
/// The three lists are disjoint, and each path lands on the most specific one
/// that claims it. A test that also imports the file is a test — that is the
/// more useful thing to know about it — and a file that both imports this one
/// and changes with it is named as a caller, because an import is a stated
/// dependency and a co-change is a statistic about the same relationship.
/// Repeating it would spend the budget saying one thing twice.
#[must_use]
fn render(report: &ImpactReport) -> Option<String> {
    let file = report.files.first()?;

    let mut tests: Vec<&str> = Vec::new();
    let mut callers: Vec<&str> = Vec::new();
    for p in &file.importers {
        if looks_like_test(&p.path) {
            tests.push(&p.path);
        } else {
            callers.push(&p.path);
        }
    }
    let mut partners: Vec<&str> = Vec::new();
    for p in &file.partners {
        if looks_like_test(&p.path) {
            if !tests.contains(&p.path.as_str()) {
                tests.push(&p.path);
            }
        } else if !callers.contains(&p.path.as_str()) {
            partners.push(&p.path);
        }
    }

    let mut sections: Vec<String> = Vec::new();
    if !callers.is_empty() {
        // `(N)` counts what this line is about — the non-test importers — so a
        // reader can tell a three-name list that is complete from one the
        // display cap shortened. It is deliberately *not* the file's fan-in:
        // `impact` stops at `ImpactOptions::max_importers`, so when its list
        // came back full the true count is unknown and the line says `+more`
        // rather than reporting the cap as if it were the answer.
        let truncated = file.importers.len() >= ImpactOptions::default().max_importers;
        sections.push(format!(
            "callers {} ({}{})",
            join_capped(&callers),
            callers.len(),
            if truncated { "+more" } else { "" }
        ));
    }
    if !partners.is_empty() {
        sections.push(format!("changes with {}", join_capped(&partners)));
    }
    if !tests.is_empty() {
        sections.push(format!("tests {}", join_capped(&tests)));
    }
    if sections.is_empty() {
        // A file nothing imports, nothing changes with and nothing tests has
        // no blast radius, and "no blast radius" is not worth a turn's
        // attention.
        return None;
    }

    let mut out = format!("impact of editing {}: ", file.path);
    for (i, s) in sections.iter().enumerate() {
        if i > 0 {
            let _ = write!(out, "; ");
        }
        out.push_str(s);
    }
    Some(cut_to(out, MAX_CONTEXT_BYTES))
}

/// At most [`MAX_NAMED`] paths, comma-separated, with `…` where the rest were.
fn join_capped(paths: &[&str]) -> String {
    let shown = paths.len().min(MAX_NAMED);
    let mut out = paths[..shown].join(", ");
    if paths.len() > shown {
        out.push('…');
    }
    out
}

/// The whole hook body: parse the payload, open the store, render the radius.
///
/// `None` for every failure as well as for every file the graph has nothing to
/// say about, because the caller's only two options are "add this line" and
/// "stay out of the way".
#[must_use]
pub fn run(db_dir: &Path, payload: &str) -> Option<String> {
    let input: serde_json::Value = serde_json::from_str(payload).ok()?;
    let path = edited_path(&input)?;
    let db = open_for_hook(db_dir)?;
    let key = repo_relative(path, recorded_repo(&db).as_deref())?;
    let report = impact(&db, &[key], &BTreeSet::new(), &ImpactOptions::default());
    render(&report)
}

#[cfg(test)]
mod tests {
    use super::{looks_like_test, repo_relative};
    use std::path::Path;

    #[test]
    fn paths_resolve_against_the_recorded_root() {
        let repo = Path::new("/home/me/proj");
        assert_eq!(
            repo_relative("/home/me/proj/src/lib.rs", Some(repo)).as_deref(),
            Some("src/lib.rs")
        );
        assert_eq!(
            repo_relative("./src/lib.rs", Some(repo)).as_deref(),
            Some("src/lib.rs")
        );
        assert_eq!(
            repo_relative("src/lib.rs", None).as_deref(),
            Some("src/lib.rs"),
            "a relative path needs no root"
        );
        assert_eq!(repo_relative("/etc/hosts", Some(repo)), None);
        assert_eq!(repo_relative("/etc/hosts", None), None);
        assert_eq!(repo_relative("./", Some(repo)), None);
    }

    #[test]
    fn tests_are_told_apart_from_dependencies() {
        for yes in [
            "tests/install.rs",
            "crates/cli/tests/install.rs",
            "src/install_test.rs",
            "web/app.test.ts",
            "app/models_spec.rb",
        ] {
            assert!(looks_like_test(yes), "{yes}");
        }
        for no in ["src/install.rs", "crates/cli/src/latest.rs", "protest/a.rs"] {
            assert!(!looks_like_test(no), "{no}");
        }
    }

    #[test]
    fn a_dot_dot_path_is_flattened_rather_than_resolved() {
        let repo = Path::new("/home/me/proj");
        assert_eq!(
            repo_relative("/home/me/proj/src/../src/lib.rs", Some(repo)).as_deref(),
            Some("src/src/lib.rs"),
            "lexical only: `..` is dropped, so the key simply does not match"
        );
    }
}
