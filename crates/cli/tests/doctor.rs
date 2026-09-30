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
        prewarm: false,
        delivery: Delivery::Both,
        always_load: false,
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
    // The nineteen a default `mushroomdb mcp` advertises on a memory store —
    // which is what a fresh install points at: the association surface. The
    // other nine stay callable, and `--all-tools` lists all twenty-eight.
    assert!(
        handshake.contains("19 tools"),
        "expected the handshake to report 19 tools: {handshake}"
    );
    assert!(
        handshake.contains("explain_association present"),
        "the handshake must prove the task path: {handshake}"
    );
}

/// Binding: the handshake passes on a store `ingest-git` built, which is served
/// the same nineteen as any other store and proves the task path through
/// `explain_association`.
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

    // The marker `ingest-git` writes, which no longer changes the listing.
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
        handshake.contains("19 tools") && handshake.contains("explain_association present"),
        "the handshake names the task tool it found: {handshake}"
    );
}

/// A project install now names the store `--auto`, and doctor has to read
/// that: resolve it the way a hook would, check the store it lands on, and
/// recognise the hooks that name it the same way.
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
        prewarm: false,
        delivery: Delivery::Both,
        always_load: false,
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
    // And it says how far back the history behind those counts reaches.
    assert!(store.contains("history from commit 0 of "), "{store}");
    // Both hook events, named: a missing one is a warning, not silence.
    let hooks = find_check(&report.output, "hooks");
    assert!(hooks.starts_with("ok"), "{hooks}");
    for event in ["UserPromptSubmit", "SessionStart"] {
        assert!(hooks.contains(event), "{hooks}");
    }
    assert!(!hooks.contains("PostToolUse"), "{hooks}");
    // 0.7 writes no git hooks, so a clean checkout has nothing to report.
    assert!(!report.output.contains("git-hooks"), "{}", report.output);
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

/// A hook a 0.6 install wrote and 0.7 retired is reported, one line per hook
/// found, naming it — and nothing is said about a hook that is not there.
/// The same goes for the `sync` block in a git hook.
///
/// The seeded strings are the exact shape a 0.6.12 install wrote with an
/// explicit `--command` and a pinned store: `<bin> <sub> '<store>'`, and
/// `( <bin> sync '<store>' >/dev/null 2>&1 & )` inside the marked block.
#[test]
fn doctor_reports_each_retired_hook_still_on_disk() {
    let root = temp_dir("old-hooks");
    let home = temp_dir("old-hooks-home");
    let git_hooks = git_repo(&root);
    let db = root.join("mushroom-memory");
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_mushroomdb"));
    run_install_with(
        &root,
        &home,
        &install_opts(Scope::Project, &db, &bin),
        &McpCommand::Explicit(bin.clone()),
        &no_externals(),
    )
    .expect("install failed");

    // Put back two of the four settings hooks and one git hook block.
    let prefix = format!("'{}'", bin.display());
    let quoted = format!("'{}'", db.display());
    let settings_path = root.join(".claude/settings.json");
    let mut settings: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
    settings["hooks"]["PostToolUse"] = serde_json::json!([{
        "matcher": "Edit|Write|MultiEdit",
        "hooks": [{"type": "command", "command": format!("{prefix} touch {quoted}"), "timeout": 30, "async": true}]
    }]);
    settings["hooks"]["PreToolUse"] = serde_json::json!([{
        "matcher": "Grep",
        "hooks": [{"type": "command", "command": format!("{prefix} intercept {quoted}"), "timeout": 5}]
    }]);
    fs::write(
        &settings_path,
        serde_json::to_string_pretty(&settings).unwrap(),
    )
    .unwrap();
    fs::write(
        git_hooks.join("post-merge"),
        format!("#!/bin/sh\n\n# >>> mushroomdb >>>\n( {prefix} sync {quoted} >/dev/null 2>&1 & )\n# <<< mushroomdb <<<\n"),
    )
    .unwrap();

    let report = run_doctor_with(&root, &home, &doctor_project_opts(), &no_externals())
        .expect("doctor errored");
    assert!(
        !report.had_fail,
        "a retired hook is a warning:\n{}",
        report.output
    );
    for sub in ["touch", "intercept"] {
        let line = find_check(&report.output, sub);
        assert!(line.starts_with("warn"), "{line}");
        assert!(line.contains("retired"), "{line}");
        assert!(
            line.contains("fix: mushroomdb install --project"),
            "the fix names this install's scope: {line}"
        );
    }
    for absent in ["impact-hook", "enrich"] {
        assert!(
            !report
                .output
                .lines()
                .any(|l| l.split_whitespace().nth(1) == Some(absent)),
            "{absent} is not on disk and must not be reported:\n{}",
            report.output
        );
    }
    let git_lines: Vec<&str> = report
        .output
        .lines()
        .filter(|l| l.split_whitespace().nth(1) == Some("git-hooks"))
        .collect();
    assert_eq!(git_lines.len(), 1, "{}", report.output);
    assert!(git_lines[0].starts_with("warn"), "{}", git_lines[0]);
    assert!(git_lines[0].contains("post-merge"), "{}", git_lines[0]);

    // Re-running install is the fix, and after it doctor has nothing to say.
    run_install_with(
        &root,
        &home,
        &install_opts(Scope::Project, &db, &bin),
        &McpCommand::Explicit(bin),
        &no_externals(),
    )
    .expect("reinstall failed");
    let report = run_doctor_with(&root, &home, &doctor_project_opts(), &no_externals())
        .expect("doctor errored");
    assert!(!report.output.contains("retired "), "{}", report.output);
}

// ---------------------------------------------------------------------------
// Test: always-load is reported from `.mcp.json`, not from the manifest flag
// ---------------------------------------------------------------------------

/// Install with `--always-load` and hand back the doctor report.
fn install_with_always_load(
    root: &Path,
    home: &Path,
    delivery: Delivery,
) -> cli::doctor::DoctorReport {
    git_repo(root);
    let db = root.join("mushroom-memory");
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_mushroomdb"));
    let opts = InstallOpts {
        always_load: true,
        delivery,
        ..install_opts(Scope::Project, &db, &bin)
    };
    run_install_with(
        root,
        home,
        &opts,
        &McpCommand::Explicit(bin),
        &no_externals(),
    )
    .expect("install failed");
    run_doctor_with(root, home, &doctor_project_opts(), &no_externals()).expect("doctor errored")
}

#[test]
fn always_load_is_ok_when_the_entry_carries_the_key() {
    let root = temp_dir("always-ok");
    let home = temp_dir("always-ok-home");
    let report = install_with_always_load(&root, &home, Delivery::Both);

    let line = find_check(&report.output, "always-load");
    assert!(line.starts_with("ok"), "{line}");
    assert!(line.contains("alwaysLoad"), "{line}");
    assert!(!report.had_fail, "{}", report.output);
}

/// The manifest flag is a record of what was asked for; `.mcp.json` is what
/// the host actually reads. Hand-editing the key away has to show up, or a
/// benchmark arm would quietly measure the install it was meant to replace.
#[test]
fn always_load_warns_when_the_entry_lost_the_key() {
    let root = temp_dir("always-drift");
    let home = temp_dir("always-drift-home");
    install_with_always_load(&root, &home, Delivery::Both);

    let mcp_file = root.join(".mcp.json");
    let mut mcp: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&mcp_file).unwrap()).unwrap();
    mcp["mcpServers"]["mushroomdb"]
        .as_object_mut()
        .unwrap()
        .remove("alwaysLoad");
    fs::write(&mcp_file, serde_json::to_string_pretty(&mcp).unwrap()).unwrap();

    let report = run_doctor_with(&root, &home, &doctor_project_opts(), &no_externals())
        .expect("doctor errored");
    let line = find_check(&report.output, "always-load");
    assert!(line.starts_with("warn"), "{line}");
    assert!(line.contains("manifest records it but"), "{line}");
    assert!(line.contains(".mcp.json"), "{line}");
    // A warn is informational: it must not fail the exit code.
    assert!(!report.had_fail, "{}", report.output);
}

/// `--delivery cli` registers no server, so there is no entry for the key to
/// sit on. The flag is still recorded, and doctor has to say the truth about
/// it rather than echoing the manifest.
#[test]
fn always_load_warns_on_a_cli_delivery_install() {
    let root = temp_dir("always-cli");
    let home = temp_dir("always-cli-home");
    let report = install_with_always_load(&root, &home, Delivery::Cli);

    let line = find_check(&report.output, "always-load");
    assert!(
        line.starts_with("warn"),
        "a cli install has no entry to mark: {line}"
    );
    assert!(!report.had_fail, "{}", report.output);
}

/// Binding: the `store` check counts namespaces once a store has more than one,
/// and says nothing about them when it has only the implicit `default` — so the
/// line on a single-tenant store is what it always was.
#[test]
fn doctor_store_check_counts_namespaces() {
    let root = temp_dir("tenancy");
    let home = temp_dir("tenancy-home");
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

    {
        let mut store = GraphDb::open(&db).expect("open");
        store.insert_node("Doc", "d1", vec![]).expect("insert");
        let report = run_doctor_with(&root, &home, &doctor_project_opts(), &no_externals())
            .expect("doctor errored");
        let line = find_check(&report.output, "store");
        assert!(
            !line.contains("namespaces"),
            "one namespace is no namespaces clause: {line}"
        );
    }

    let mut store = GraphDb::open(&db).expect("reopen");
    store
        .insert_node(
            "Doc",
            "a1",
            vec![("ns".into(), core_api::Value::Str("tenant-a".into()))],
        )
        .expect("insert");
    drop(store);
    let report = run_doctor_with(&root, &home, &doctor_project_opts(), &no_externals())
        .expect("doctor errored");
    let line = find_check(&report.output, "store");
    assert!(line.contains(", 2 namespaces"), "{line}");
}
