//! Integration tests for `mushroomdb doctor`.
//!
//! All tests operate on temp directories — never touching real HOME or CWD.
//! Install is driven through `run_install_with` (no network, no real PATH),
//! but `doctor`'s self-handshake genuinely spawns a process and talks to it
//! over stdio, so most tests point the config's `command` at the real test
//! binary (`CARGO_BIN_EXE_mushroomdb`) rather than simulating anything.

use cli::doctor::{run_doctor_with, DoctorOpts};
use cli::install::{
    run_install_with, Delivery, Externals, InstallOpts, McpCommand, Platform, Scope,
};
use core_api::GraphDb;
use std::fs;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn temp_dir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "mushroomdb-doctor-test-{}-{}-{}",
        label,
        std::process::id(),
        nanos
    ));
    fs::create_dir_all(&d).unwrap();
    d
}

/// Make `root` look like a git checkout with a hooks directory.
fn git_repo(root: &Path) -> PathBuf {
    let hooks = root.join(".git").join("hooks");
    fs::create_dir_all(&hooks).unwrap();
    hooks
}

/// External programs are unreachable: doctor's `npx` check never runs in
/// these tests since every install here uses an explicit `--command`.
fn no_externals() -> Externals {
    Externals::with_path(None)
}

/// Write an executable shell script named `name` into `dir`.
#[cfg(unix)]
fn fake_program(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join(name);
    fs::write(&p, format!("#!/bin/sh\n{body}")).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    p
}

/// Project-scope, Claude Code install opts pinned to an explicit `--command`.
fn install_opts(scope: Scope, db: &Path, command: &Path) -> InstallOpts {
    InstallOpts {
        platform: Some(Platform::ClaudeCode),
        scope: Some(scope),
        db: Some(db.to_path_buf()),
        command: Some(command.to_path_buf()),
        git_hooks: true,
        prewarm: false,
        delivery: Delivery::Both,
        intercept_grep: false,
    }
}

fn doctor_project_opts() -> DoctorOpts {
    DoctorOpts {
        platform: Some(Platform::ClaudeCode),
        scope: Some(Scope::Project),
    }
}

/// The line whose second whitespace-separated field is `name`, or a panic
/// with the full report for a useful failure message.
fn find_check<'a>(output: &'a str, name: &str) -> &'a str {
    output
        .lines()
        .find(|l| l.split_whitespace().nth(1) == Some(name))
        .unwrap_or_else(|| panic!("no `{name}` check in doctor output:\n{output}"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn doctor_passes_on_fresh_project_install() {
    let root = temp_dir("fresh");
    let home = temp_dir("fresh-home");
    git_repo(&root);
    let db = root.join("mushroom-memory");
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_mushroomdb"));

    let opts = install_opts(Scope::Project, &db, &bin);
    run_install_with(
        &root,
        &home,
        &opts,
        &McpCommand::Explicit(bin),
        &no_externals(),
    )
    .expect("install failed");

    let report = run_doctor_with(&root, &home, &doctor_project_opts(), &no_externals())
        .expect("doctor errored");

    assert!(!report.had_fail, "expected no failures:\n{}", report.output);
    for line in report.output.lines() {
        assert!(
            !line.starts_with("fail"),
            "unexpected fail line: {line}\nfull output:\n{}",
            report.output
        );
    }

    let handshake = find_check(&report.output, "handshake");
    assert!(handshake.starts_with("ok"), "handshake check: {handshake}");
    // The thirteen a default `mushroomdb mcp` advertises on a memory store —
    // which is what a fresh install points at: the association surface. The
    // other twelve stay callable, and `--all-tools` lists them.
    assert!(
        handshake.contains("13 tools"),
        "expected the handshake to report 13 tools: {handshake}"
    );
    assert!(
        handshake.contains("explain_association present"),
        "the handshake must prove the task path: {handshake}"
    );
}

/// Binding: the handshake passes on a store `ingest-git` built, whose default
/// listing is three tools and does *not* include `map`.
///
/// The check proves the repository task path is served, not that one
/// particular name is listed; a code-graph store serves it through `explore`.
/// Requiring `map` would fail `doctor` on exactly the stores this surface
/// exists for.
#[test]
fn doctor_handshake_passes_on_a_code_graph_store() {
    let root = temp_dir("code-graph-handshake");
    let home = temp_dir("code-graph-handshake-home");
    git_repo(&root);
    let db = root.join("mushroom-memory");
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_mushroomdb"));

    let opts = install_opts(Scope::Project, &db, &bin);
    run_install_with(
        &root,
        &home,
        &opts,
        &McpCommand::Explicit(bin.clone()),
        &no_externals(),
    )
    .expect("install failed");

    // The marker `ingest-git` writes is what makes this a code graph.
    let out = std::process::Command::new(&bin)
        .arg("query")
        .arg(&db)
        .arg("CREATE (n:GitSync {id: '__mushroomdb_git_sync__'})")
        .output()
        .expect("query");
    assert!(out.status.success(), "{out:?}");

    let report = run_doctor_with(&root, &home, &doctor_project_opts(), &no_externals())
        .expect("doctor errored");
    let handshake = find_check(&report.output, "handshake");
    assert!(
        handshake.starts_with("ok"),
        "a code-graph store must pass the handshake: {handshake}"
    );
    assert!(
        handshake.contains("3 tools") && handshake.contains("explore present"),
        "the handshake names the task tool it found: {handshake}"
    );
}

/// A project install now names the store `--auto`, and doctor has to read
/// that: resolve it the way a hook would, check the store it lands on, and
/// recognise the hooks and git hook blocks that name it the same way.
#[test]
fn doctor_understands_auto_entries() {
    let root = temp_dir("auto-entry");
    let home = temp_dir("auto-entry-home");
    git_repo(&root);
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_mushroomdb"));

    // No `--db`: the install writes `--auto` everywhere.
    let opts = InstallOpts {
        platform: Some(Platform::ClaudeCode),
        scope: Some(Scope::Project),
        db: None,
        command: Some(bin.clone()),
        git_hooks: true,
        prewarm: false,
        delivery: Delivery::Both,
        intercept_grep: false,
    };
    run_install_with(
        &root,
        &home,
        &opts,
        &McpCommand::Explicit(bin),
        &no_externals(),
    )
    .expect("install failed");
    let db = root.join("mushroom-memory");
    GraphDb::open(&db).expect("create the store doctor will read");

    let report = run_doctor_with(&root, &home, &doctor_project_opts(), &no_externals())
        .expect("doctor errored");

    assert!(!report.had_fail, "expected no failures:\n{}", report.output);

    // The config line says both what is written and where it lands.
    let config = find_check(&report.output, "config");
    assert!(config.starts_with("ok"), "{config}");
    assert!(config.contains("--auto ->"), "{config}");
    assert!(config.contains(&db.display().to_string()), "{config}");

    // And the checks below it read the resolved directory, not the flag.
    let store = find_check(&report.output, "store");
    assert!(store.starts_with("ok"), "{store}");
    assert!(store.contains(&db.display().to_string()), "{store}");
    // All three hook events, named: a missing one is a warning, not silence.
    let hooks = find_check(&report.output, "hooks");
    assert!(hooks.starts_with("ok"), "{hooks}");
    for event in ["UserPromptSubmit", "PostToolUse", "SessionStart"] {
        assert!(hooks.contains(event), "{hooks}");
    }
    assert!(find_check(&report.output, "git-hooks").starts_with("ok"));
}

#[test]
fn doctor_fails_when_entry_missing() {
    let root = temp_dir("missing");
    let home = temp_dir("missing-home");
    git_repo(&root);
    // No install was ever run: `.mcp.json` does not exist.

    let report = run_doctor_with(&root, &home, &doctor_project_opts(), &no_externals())
        .expect("doctor errored");

    assert!(report.had_fail, "expected a failure:\n{}", report.output);
    let config = find_check(&report.output, "config");
    assert!(config.starts_with("fail"), "config check: {config}");
}

#[test]
fn doctor_warns_on_duplicate_scope() {
    let root = temp_dir("dupe");
    let home = temp_dir("dupe-home");
    git_repo(&root);
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_mushroomdb"));
    let proj_db = root.join("mushroom-memory");
    let user_db = home.join(".mushroomdb").join("memory");

    run_install_with(
        &root,
        &home,
        &install_opts(Scope::Project, &proj_db, &bin),
        &McpCommand::Explicit(bin.clone()),
        &no_externals(),
    )
    .expect("project install failed");
    run_install_with(
        &root,
        &home,
        &install_opts(Scope::User, &user_db, &bin),
        &McpCommand::Explicit(bin),
        &no_externals(),
    )
    .expect("user install failed");

    let report = run_doctor_with(&root, &home, &doctor_project_opts(), &no_externals())
        .expect("doctor errored");

    let scope_check = find_check(&report.output, "scope");
    assert!(
        scope_check.starts_with("warn"),
        "expected a duplicate-scope warning: {scope_check}\nfull output:\n{}",
        report.output
    );
}

#[cfg(unix)]
#[test]
fn doctor_self_handshake_detects_bad_server() {
    // A server that answers `initialize` with the wrong version — a stand-in
    // for a stale or misconfigured `command` entry.
    const BAD_SERVER: &str = r#"IFS= read -r _
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"mushroomdb","version":"0.0.0-bogus"}}}'
IFS= read -r _
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"map"}]}}'
"#;

    let root = temp_dir("badserver");
    let home = temp_dir("badserver-home");
    git_repo(&root);
    let db = root.join("mushroom-memory");
    let script = fake_program(&root, "fake-mushroomdb", BAD_SERVER);

    run_install_with(
        &root,
        &home,
        &install_opts(Scope::Project, &db, &script),
        &McpCommand::Explicit(script),
        &no_externals(),
    )
    .expect("install failed");

    let report = run_doctor_with(&root, &home, &doctor_project_opts(), &no_externals())
        .expect("doctor errored");

    assert!(report.had_fail, "expected a failure:\n{}", report.output);
    let handshake = find_check(&report.output, "handshake");
    assert!(
        handshake.starts_with("fail"),
        "handshake check: {handshake}"
    );
}

#[test]
fn doctor_warns_when_lock_held() {
    let root = temp_dir("lock");
    let home = temp_dir("lock-home");
    git_repo(&root);
    let db = root.join("mushroom-memory");
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_mushroomdb"));

    run_install_with(
        &root,
        &home,
        &install_opts(Scope::Project, &db, &bin),
        &McpCommand::Explicit(bin),
        &no_externals(),
    )
    .expect("install failed");

    // Hold the store's cross-process write lock in-process: a second `flock`
    // request against the same file from a different open file description
    // (even in this same process) contends exactly like a second process.
    let holder = GraphDb::open(&db).expect("cannot open store");

    let report = run_doctor_with(&root, &home, &doctor_project_opts(), &no_externals())
        .expect("doctor errored");

    let lock_check = find_check(&report.output, "lock");
    assert!(
        lock_check.starts_with("warn"),
        "expected a lock warning: {lock_check}\nfull output:\n{}",
        report.output
    );

    drop(holder);
}

/// Binding: a `--delivery cli` install has no MCP entry by design, so the two
/// checks that read one report `skip` rather than `fail` — and everything that
/// does not need one (the store, the hooks, the git hooks) still runs, against
/// the store the install recorded.
///
/// `doctor` exits 1 on any `fail`, so a `fail` here would make a perfectly
/// healthy install look broken every time it was checked.
#[test]
fn doctor_on_cli_delivery_skips_handshake_and_passes() {
    let root = temp_dir("cli-delivery");
    let home = temp_dir("cli-delivery-home");
    git_repo(&root);
    let db = root.join("mushroom-memory");
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_mushroomdb"));

    let opts = InstallOpts {
        delivery: Delivery::Cli,
        ..install_opts(Scope::Project, &db, &bin)
    };
    run_install_with(
        &root,
        &home,
        &opts,
        &McpCommand::Explicit(bin),
        &no_externals(),
    )
    .expect("install failed");

    let report = run_doctor_with(&root, &home, &doctor_project_opts(), &no_externals())
        .expect("doctor errored");

    assert!(!report.had_fail, "expected no failures:\n{}", report.output);
    for line in report.output.lines() {
        assert!(
            !line.starts_with("fail"),
            "unexpected fail line: {line}\nfull output:\n{}",
            report.output
        );
    }

    let handshake = find_check(&report.output, "handshake");
    assert!(
        handshake.starts_with("skip") && handshake.contains("delivery: cli"),
        "handshake check: {handshake}"
    );
    let config = find_check(&report.output, "config");
    assert!(
        config.starts_with("skip") && config.contains("delivery: cli"),
        "config check: {config}"
    );
    // The checks that do not need a server still have to run: this install is
    // exactly as breakable as any other in its store and its hooks.
    for name in ["store", "hooks"] {
        let check = find_check(&report.output, name);
        assert!(
            check.starts_with("ok"),
            "{name} check: {check}\nfull output:\n{}",
            report.output
        );
    }
}

/// The redirect is opt-in, so `doctor` reports it only when the manifest says
/// this install asked for it. A report line for a hook nobody wired would say
/// nothing true about the install in front of it.
#[test]
fn doctor_reports_the_grep_redirect_only_when_it_is_installed() {
    for intercept_grep in [true, false] {
        let label = if intercept_grep { "on" } else { "off" };
        let root = temp_dir(&format!("intercept-{label}"));
        let home = temp_dir(&format!("intercept-{label}-home"));
        git_repo(&root);
        let db = root.join("mushroom-memory");
        let bin = PathBuf::from(env!("CARGO_BIN_EXE_mushroomdb"));

        let opts = InstallOpts {
            intercept_grep,
            ..install_opts(Scope::Project, &db, &bin)
        };
        run_install_with(
            &root,
            &home,
            &opts,
            &McpCommand::Explicit(bin),
            &no_externals(),
        )
        .expect("install failed");

        let report = run_doctor_with(&root, &home, &doctor_project_opts(), &no_externals())
            .expect("doctor errored");
        let line = report
            .output
            .lines()
            .find(|l| l.split_whitespace().nth(1) == Some("intercept"));
        if intercept_grep {
            let line = line.unwrap_or_else(|| panic!("no intercept check:\n{}", report.output));
            assert!(line.starts_with("ok"), "{line}");
            assert!(line.contains("PreToolUse"), "{line}");
        } else {
            assert_eq!(line, None, "unasked-for line:\n{}", report.output);
        }
    }
}
