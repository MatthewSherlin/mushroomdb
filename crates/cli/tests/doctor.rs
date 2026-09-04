//! Integration tests for `mushroomdb doctor`.
//!
//! All tests operate on temp directories — never touching real HOME or CWD.
//! Install is driven through `run_install_with` (no network, no real PATH),
//! but `doctor`'s self-handshake genuinely spawns a process and talks to it
//! over stdio, so most tests point the config's `command` at the real test
//! binary (`CARGO_BIN_EXE_mushroomdb`) rather than simulating anything.

use cli::doctor::{run_doctor_with, DoctorOpts};
use cli::install::{run_install_with, Externals, InstallOpts, McpCommand, Platform, Scope};
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
    assert!(
        handshake.contains("24 tools"),
        "expected the handshake to report 24 tools: {handshake}"
    );
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
