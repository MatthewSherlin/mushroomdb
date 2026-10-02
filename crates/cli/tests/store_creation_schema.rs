//! Every path that creates a store declares a schema on it.
//!
//! `mcp`, `serve` and `demo` create a store and declare the memory schema on
//! it; `ingest-git` created one and wrote a repository into it with no memory
//! schema at all, so the store a user arrived on depended on which command
//! happened to make it. `install` creates none — it only names the store the
//! others will create — and says so when that store does not exist yet.
//!
//! This is one test file over all of them rather than a patch per path,
//! because the failure it guards is a *new* path being added without one: the
//! source census at the bottom fails the moment a store-opening call appears
//! in the CLI crate without either a schema or a stated reason.
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// Distinguishes two directories made in the same clock tick by parallel tests.
static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn tmp(name: &str) -> PathBuf {
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "mdb-screate-{name}-{}-{nanos}-{seq}",
        std::process::id()
    ))
}

fn mushroomdb(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
        .args(args)
        .output()
        .expect("spawn mushroomdb")
}

fn ok(out: Output) -> String {
    assert!(out.status.success(), "{out:?}");
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// What the `UserPromptSubmit` hook prints for `prompt`, sent on stdin the
/// way the host sends it.
fn hook_digest(db: &Path, prompt: &str) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
        .args(["recall", &db.to_string_lossy()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn recall");
    let payload = serde_json::json!({ "prompt": prompt }).to_string();
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(payload.as_bytes())
        .expect("write payload");
    let out = child.wait_with_output().expect("wait");
    // An empty digest is only evidence when the hook ran cleanly: a crash
    // prints nothing too.
    assert!(out.status.success(), "recall hook failed: {out:?}");
    assert!(out.stderr.is_empty(), "recall hook wrote stderr: {out:?}");
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// The full-text pairs `ingest-git` declares for its own data.
fn own_fulltext() -> Vec<(String, String)> {
    cli::structure::FULLTEXT
        .iter()
        .map(|(l, f)| ((*l).to_string(), (*f).to_string()))
        .collect()
}

/// A store opened read-only, for asking what it declares.
fn open_read_only(db: &Path) -> cli::structure::Db {
    core_api::GraphDb::open_with_options(
        db,
        core_api::OpenOptions {
            auto_migrate: false,
            repair_wal: false,
            read_only: true,
        },
    )
    .expect("open read-only")
}

/// Whether a store can answer `recall` about `term`, something it holds.
///
/// Two halves, because either alone can pass on a store with no schema. The
/// hook is silent on a store with no text index, so "the hook did not
/// complain" proves nothing; the brief is where the no-index notice lives. And
/// a brief without the notice proves only that *some* pair is declared, so the
/// hook must also actually find the thing the store holds.
fn can_recall(db: &Path, term: &str) -> Result<(), String> {
    let brief = ok(mushroomdb(&["brief", &db.to_string_lossy()]));
    if brief.contains("no text index") {
        return Err(format!("brief reports no text index:\n{brief}"));
    }
    let digest = hook_digest(db, term);
    if digest.trim().is_empty() {
        return Err(format!("the hook found nothing for {term:?}"));
    }
    Ok(())
}

fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(repo)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Ada")
        .env("GIT_AUTHOR_EMAIL", "ada@example.com")
        .env("GIT_COMMITTER_NAME", "Ada")
        .env("GIT_COMMITTER_EMAIL", "ada@example.com")
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

/// An initialised repository with no commits.
fn empty_repo() -> PathBuf {
    let repo = tmp("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    repo
}

fn commit_file(repo: &Path, path: &str, body: &str, message: &str) {
    let full = repo.join(path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, body).unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", message]);
}

/// A small repository with two commits and one distinctive word — `zanzibar`
/// — in a file name and a commit message, so recall over it has something
/// certain to find.
fn scratch_repo() -> PathBuf {
    let repo = empty_repo();
    commit_file(&repo, "README.md", "# scratch\n", "start the scratch repo");
    commit_file(
        &repo,
        "src/zanzibar.rs",
        "pub fn route() {}\n",
        "add the zanzibar router",
    );
    repo
}

fn ingest_git(db: &Path, repo: &Path) -> String {
    ok(mushroomdb(&[
        "ingest-git",
        &db.to_string_lossy(),
        &repo.to_string_lossy(),
    ]))
}

/// A `Person` written straight through `query`, which declares nothing: it is
/// findable by name only if the store already carries the memory schema.
fn add_person(db: &Path, name: &str) {
    let cypher = format!("CREATE (n:Person {{id:'p-{name}', name:'{name}'}})");
    ok(mushroomdb(&["query", &db.to_string_lossy(), &cypher]));
}

#[test]
fn demo_creates_a_store_that_can_recall() {
    let db = tmp("demo");
    ok(mushroomdb(&["demo", &db.to_string_lossy()]));
    // The demo seed names its people "Person 1" … "Person 30".
    can_recall(&db, "who is Person 3").expect("demo store cannot recall");
}

#[test]
fn ingest_git_creates_a_store_that_can_recall() {
    // A repository as entities is a memory store in 0.7: one surface, one
    // schema. Its own text fields must be searchable, and so must the memory
    // surface's entities once someone writes one.
    let db = tmp("ingest");
    let repo = scratch_repo();
    ingest_git(&db, &repo);
    can_recall(&db, "zanzibar").expect("ingest-git store cannot recall its repository");

    add_person(&db, "Quillfeather");
    can_recall(&db, "Quillfeather").expect("ingest-git store has no memory schema");
}

#[test]
fn ingest_git_into_an_empty_repository_still_declares_on_the_first_real_run() {
    // A run with nothing to ingest writes nothing — so it must not leave the
    // store looking "existing" to the next run's gate either.
    let db = tmp("empty-first");
    let repo = empty_repo();
    ingest_git(&db, &repo);
    assert!(
        !core_api::restore::holds_a_store(&db),
        "a run over an empty repository wrote a store"
    );

    commit_file(
        &repo,
        "src/zanzibar.rs",
        "pub fn route() {}\n",
        "add zanzibar",
    );
    ingest_git(&db, &repo);
    add_person(&db, "Quillfeather");
    can_recall(&db, "Quillfeather").expect("the first real run declared no memory schema");
}

#[test]
fn reingesting_into_an_existing_store_succeeds_and_declares_nothing() {
    let db = tmp("reingest");
    let repo = scratch_repo();
    ingest_git(&db, &repo);
    commit_file(&repo, "src/zanzibar.rs", "pub fn route() { }\n", "tidy");
    // A second declaration through the non-idempotent setter would fail here.
    let again = ingest_git(&db, &repo);
    assert!(
        !again.contains("rules:"),
        "a re-run declared rules: {again}"
    );
    can_recall(&db, "zanzibar").expect("re-ingested store cannot recall");
}

#[test]
fn ingest_git_never_applies_the_memory_defaults_to_a_store_it_did_not_create() {
    // The memory defaults reach an existing store only through an explicit
    // `schema apply` (the 227 ms index rebuild, ledger row 36). ingest-git
    // still declares what its own data needs there; this checks exactly the
    // pairs and indexes the memory defaults add that ingest-git never does.
    let db = tmp("existing");
    add_person(&db, "Quillfeather");
    assert!(core_api::restore::holds_a_store(&db));
    ingest_git(&db, &scratch_repo());

    let store = open_read_only(&db);
    let pairs = store.fulltext_pairs();
    let defaults = core_api::memory_schema::memory_defaults();
    let own = own_fulltext();
    let memory_only: Vec<&(String, String)> = defaults
        .fulltext
        .iter()
        .filter(|p| !own.contains(p))
        .collect();
    assert!(!memory_only.is_empty(), "nothing to check");
    for pair in memory_only {
        assert!(
            !pairs.contains(pair),
            "ingest-git declared memory-default full-text {pair:?}: {pairs:?}"
        );
    }
    for (label, field) in &defaults.indexes {
        assert!(
            !store.is_index_enabled(label, field),
            "ingest-git declared memory-default index {label}.{field}"
        );
    }
    drop(store);
    assert_eq!(
        hook_digest(&db, "Quillfeather").trim(),
        "",
        "Person.name became searchable on a store ingest-git did not create"
    );
}

#[test]
fn ingest_git_declares_its_structure_rules_on_a_store_mcp_created() {
    // The ordinary order: install, then `mcp` creates the store with the
    // memory defaults, then `ingest-git` writes a repository into it. The
    // structure props are useless without the rules that derive edges from
    // them, so ingest-git declares those — and only those — on a store it
    // did not create, as 0.6 did.
    let db = tmp("mcp-first");
    ok(mushroomdb(&[
        "schema",
        "apply",
        &db.to_string_lossy(),
        "--memory-defaults",
    ]));
    let repo = scratch_repo();
    let first = ingest_git(&db, &repo);
    assert!(first.contains("auto_fk_symbol_file_id"), "{first}");

    let defines = ok(mushroomdb(&[
        "query",
        &db.to_string_lossy(),
        "MATCH (s:Symbol)-[:DEFINES]->(f:File) RETURN f.id AS file",
    ]));
    assert!(
        defines.contains("src/zanzibar.rs"),
        "no DEFINES edge derived: {defines}"
    );
    let store = open_read_only(&db);
    for pair in own_fulltext() {
        assert!(store.fulltext_pairs().contains(&pair), "missing {pair:?}");
    }
    drop(store);

    // A second run declares nothing and does not fail on what is there.
    commit_file(&repo, "src/zanzibar.rs", "pub fn route() { }\n", "tidy");
    let again = ingest_git(&db, &repo);
    assert!(
        !again.contains("rules:"),
        "a re-run declared rules: {again}"
    );
}

/// Rule names on a store that start `about_`.
fn about_rules(db: &Path) -> Vec<String> {
    open_read_only(db)
        .rules()
        .into_iter()
        .map(|r| r.name)
        .filter(|n| n.starts_with("about_"))
        .collect()
}

/// One `remember` call through `mushroomdb mcp`, returning whether it errored
/// and its text.
fn remember_over_mcp(db: &Path, text: &str, about: &str) -> (bool, String) {
    let lines = [
        serde_json::json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{
            "protocolVersion":"2024-11-05","capabilities":{},
            "clientInfo":{"name":"store-creation","version":"1"}}}),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
            "name":"remember","arguments":{
                "text": text, "about": [about], "ts": 1_759_000_000}}}),
    ]
    .map(|l| l.to_string())
    .join("\n");
    let mut child = Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
        .args(["mcp", &db.to_string_lossy()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mcp");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(format!("{lines}\n").as_bytes())
        .unwrap();
    drop(child.stdin.take());
    let out = child.wait_with_output().expect("mcp exited");
    let reply = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["id"] == 1)
        .expect("a reply to the remember call");
    let is_error = reply["result"]["isError"].as_bool().unwrap_or(false);
    (is_error, reply.to_string())
}

#[test]
fn remembering_twice_about_an_ingested_file_succeeds() {
    // install, `mcp` creates the store, `ingest-git` writes a repository into
    // it, then the same note about one of its files is remembered twice.
    let db = tmp("remember-twice");
    ok(mushroomdb(&[
        "schema",
        "apply",
        &db.to_string_lossy(),
        "--memory-defaults",
    ]));
    ingest_git(&db, &scratch_repo());
    for call in ["first", "second"] {
        let (is_error, reply) =
            remember_over_mcp(&db, "the zanzibar router lives here", "src/zanzibar.rs");
        assert!(!is_error, "{call} remember failed: {reply}");
    }
}

#[test]
fn ingest_git_declares_no_about_rule_on_a_new_or_an_existing_store() {
    // `remember` writes its own ABOUT edges; a rule deriving the same edges
    // would own them and refuse the next write.
    let fresh = tmp("about-new");
    ingest_git(&fresh, &scratch_repo());
    assert_eq!(about_rules(&fresh), Vec::<String>::new());

    let existing = tmp("about-existing");
    ok(mushroomdb(&[
        "schema",
        "apply",
        &existing.to_string_lossy(),
        "--memory-defaults",
    ]));
    ingest_git(&existing, &scratch_repo());
    assert_eq!(about_rules(&existing), Vec::<String>::new());
}

#[test]
fn applying_the_schema_twice_is_not_an_error() {
    // `enable_fulltext` returns Err on an already-enabled pair. Every path
    // above goes through `apply_schema`, which does not.
    let db = tmp("twice");
    for _ in 0..2 {
        ok(mushroomdb(&[
            "schema",
            "apply",
            &db.to_string_lossy(),
            "--memory-defaults",
        ]));
    }
}

// ---------------------------------------------------------------------------
// The census
// ---------------------------------------------------------------------------

/// Every spelling of "open a store" in the CLI crate. A prefix match, so
/// `GraphDb::open` also covers `open_with_options`, `open_at` and
/// `open_unlocked`, and `SharedDb::open` every `SharedDb` constructor.
const OPENS: [&str; 2] = ["GraphDb::open", "SharedDb::open"];

/// Functions that open a store read-write with no schema, on purpose. Each is
/// a general-purpose command whose documented behaviour is to operate on
/// whatever store it is pointed at; giving it a memory schema would change what
/// the command means. A name here that no longer exists fails the census.
const ALLOWLIST: &[(&str, &str)] = &[
    (
        "run_query",
        "general-purpose; a bare store on a fresh path is the documented \
         behaviour — schema is opt-in via `schema apply`",
    ),
    (
        "run_asof",
        "time-travel read of an existing store's history; a fresh path has \
         no history to read",
    ),
    (
        "run_migrate",
        "rewrites an existing store's snapshot format; declaring anything \
         would change the store it is migrating",
    ),
    (
        "run_snapshot",
        "compacts an existing store's WAL; must not add to what it compacts",
    ),
    (
        "run_build_index",
        "finishes builds already pending on an existing store's rules",
    ),
    (
        "run_backup",
        "copies an existing store; the backup must match the source",
    ),
    ("run_export", "reads an existing store out to a file"),
    ("run_algo", "runs a graph algorithm over an existing store"),
    (
        "read_stats",
        "reports counts; `stats` on a fresh path answers zero, not a schema",
    ),
    (
        "run_suggest",
        "proposes rules for an existing store's data; writes nothing",
    ),
    (
        "check_store_and_lock",
        "doctor's write-lock probe, run only after the read-only open above \
         it succeeded on the same store; drops the handle unwritten",
    ),
];

/// Store-opening calls classified, one per line: `file:line fn class`.
struct Site {
    at: String,
    function: String,
    class: &'static str,
}

/// Whether line `i` starts a `#[cfg(test)]` module, and if so the index of the
/// module's closing brace. Relies on `cargo fmt`, which the verification bar
/// enforces: the module's `}` sits at the indentation of its `mod` line.
fn test_module_end(lines: &[&str], i: usize) -> Option<usize> {
    if lines[i].trim() != "#[cfg(test)]" {
        return None;
    }
    let next = lines.get(i + 1)?;
    let trimmed = next.trim_start();
    if !(trimmed.starts_with("mod ") && trimmed.trim_end().ends_with('{')) {
        return None;
    }
    let indent = &next[..next.len() - trimmed.len()];
    let close = format!("{indent}}}");
    let end = (i + 2..lines.len())
        .find(|&j| lines[j] == close)
        .expect("a formatted test module closes at its own indentation");
    Some(end)
}

/// The nearest enclosing `fn` above line `i`: its name, the index of its
/// `fn` line, and the index of its closing brace (again at the `fn` line's own
/// indentation, per rustfmt).
fn enclosing_fn(lines: &[&str], i: usize) -> (String, usize, usize) {
    for start in (0..=i).rev() {
        let line = lines[start];
        let trimmed = line.trim_start();
        let Some(at) = trimmed.find("fn ") else {
            continue;
        };
        let head = &trimmed[..at];
        let is_decl = head
            .split_whitespace()
            .all(|w| w == "pub" || w.starts_with("pub(") || w == "async" || w == "const");
        if !is_decl {
            continue;
        }
        let name: String = trimmed[at + 3..]
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        let indent = &line[..line.len() - trimmed.len()];
        let close = format!("{indent}}}");
        let end = (start + 1..lines.len())
            .find(|&j| lines[j] == close)
            .unwrap_or(lines.len() - 1);
        if end >= i {
            return (name, start, end);
        }
    }
    panic!("no enclosing fn for line {}", i + 1);
}

/// For an `open_with_options` call, whether its options literal says
/// `read_only: true` — read up to the call's closing `)`.
fn opens_read_only(lines: &[&str], i: usize) -> bool {
    if !lines[i].contains("open_with_options") {
        return false;
    }
    for line in &lines[i..(i + 10).min(lines.len())] {
        if line.contains("read_only: true") {
            return true;
        }
        let t = line.trim();
        if t.starts_with(')') || t.ends_with(");") || t.ends_with(")?;") {
            return false;
        }
    }
    false
}

fn census() -> (Vec<Site>, Vec<String>) {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&src)
        .expect("read src")
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
        .collect();
    files.sort();
    assert!(
        files.iter().any(|p| p.ends_with("install.rs")),
        "the census must see install.rs"
    );
    let mut sites = Vec::new();
    let mut functions = Vec::new();
    for path in files {
        let rel = path.file_name().unwrap().to_string_lossy().to_string();
        let text = std::fs::read_to_string(&path).expect("read source");
        let lines: Vec<&str> = text.lines().collect();
        let mut i = 0;
        while i < lines.len() {
            if let Some(end) = test_module_end(&lines, i) {
                i = end + 1;
                continue;
            }
            let line = lines[i];
            let trimmed = line.trim_start();
            if let Some(at) = trimmed.find("fn ") {
                let name: String = trimmed[at + 3..]
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                functions.push(name);
            }
            let is_code = !trimmed.starts_with("//");
            if is_code && OPENS.iter().any(|o| line.contains(o)) {
                let (function, start, end) = enclosing_fn(&lines, i);
                // Never reaches above the function's own `fn` line: an
                // `apply_schema` in the function before is not this one's.
                let from = i.saturating_sub(20).max(start);
                let applies = lines[from..=end].iter().any(|l| {
                    let t = l.trim_start();
                    !t.starts_with("//") && t.contains("apply_schema(")
                });
                let class = if opens_read_only(&lines, i) {
                    "read-only"
                } else if applies {
                    "applies a schema"
                } else if ALLOWLIST.iter().any(|(f, _)| *f == function) {
                    "allowlisted"
                } else {
                    "OFFENDER"
                };
                sites.push(Site {
                    at: format!("{rel}:{}", i + 1),
                    function,
                    class,
                });
            }
            i += 1;
        }
    }
    (sites, functions)
}

/// The census: no store-opening call without a schema or a stated reason.
///
/// A source scan, not a runtime one, because the failure mode is a *new* call
/// site added later by someone who did not read this file. A read-write open
/// must either be followed by an `apply_schema` in the same function (up to
/// 20 lines before it count too, never above the `fn` line), or sit in a
/// function on [`ALLOWLIST`].
///
/// **Every constructor is scanned.** The first draft of this test looked for
/// `GraphDb::open` alone and would have reported the tree clean while missing
/// the one real offender: `ingest-git` creates its store through
/// `SharedDb::open`. A census that cannot see the thing it guards is the
/// failure this project keeps finding — this is its fifth appearance — and
/// scanning one spelling of "open a store" is exactly that shape.
///
/// **Known blind spots.** The tree is clean of each today; none is guarded:
///
/// - It reads only the top-level `crates/cli/src/*.rs` files. A nested module
///   (`src/foo/bar.rs`) is not scanned.
/// - It matches the spellings in [`OPENS`] by text, so it does not see an open
///   through an alias (`type Db = GraphDb<…>; Db::open(…)`) or through a
///   turbofish (`GraphDb::<F>::open`).
/// - It sees the CLI crate alone. A store created outside it — the Python
///   binding's, for one — is not counted.
#[test]
fn every_store_creating_call_site_applies_a_schema() {
    let (sites, functions) = census();
    let listing: String = sites
        .iter()
        .map(|s| format!("  {} {} — {}\n", s.at, s.function, s.class))
        .collect();
    let offenders: Vec<&Site> = sites.iter().filter(|s| s.class == "OFFENDER").collect();
    assert!(
        offenders.is_empty(),
        "store-opening call sites with no schema and no allowlist entry:\n{listing}\
         Apply a schema there behind `holds_a_store`, open read-only, or add the \
         function to ALLOWLIST with its reason."
    );
    // Guards that the scan sees what it guards: a census that found nothing
    // would pass vacuously.
    for expected in ["run_mcp", "run_serve", "run_demo", "run_ingest_git"] {
        assert!(
            sites
                .iter()
                .any(|s| s.function == expected && s.class == "applies a schema"),
            "census did not see {expected} apply a schema:\n{listing}"
        );
    }
    assert!(
        !sites.iter().any(|s| s.at.starts_with("install.rs:")),
        "install.rs opens a store — this plan says it does not:\n{listing}"
    );
    for (name, reason) in ALLOWLIST {
        assert!(!reason.is_empty(), "{name} has no reason");
        assert!(
            functions.iter().any(|f| f == name),
            "ALLOWLIST names {name}, which no longer exists"
        );
        assert!(
            sites.iter().any(|s| s.function == *name),
            "ALLOWLIST names {name}, which no longer opens a store"
        );
    }
}
