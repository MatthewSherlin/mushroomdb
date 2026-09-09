//! Integration tests for `mushroomdb enable` / `disable`.
//!
//! All tests operate on temp directories — never touching real HOME or CWD,
//! and never reaching the network: `Externals` carries the PATH used to
//! resolve external programs, and every test either points it at a directory
//! of stand-ins or leaves it empty (mirrors `tests/install.rs`).

use cli::install::{
    run_disable, run_disable_with, run_enable_with, run_install_with, run_uninstall, Externals,
    InstallOpts, McpCommand, Platform, Scope, ToggleOpts,
};
use std::fs;
use std::path::{Path, PathBuf};

/// The version an `npx` entry pins. Same crate, so the same constant the
/// installer compiles in.
const VERSION: &str = env!("CARGO_PKG_VERSION");

// ---------------------------------------------------------------------------
// Helpers (same shapes as tests/install.rs)
// ---------------------------------------------------------------------------

fn temp_dir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "mushroomdb-enable-test-{}-{}-{}",
        label,
        std::process::id(),
        nanos
    ));
    fs::create_dir_all(&d).unwrap();
    d
}

fn read(root: &Path, rel: &str) -> String {
    fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("missing {rel}: {e}"))
}

fn read_json(root: &Path, rel: &str) -> serde_json::Value {
    serde_json::from_str(&read(root, rel)).unwrap_or_else(|e| panic!("invalid json {rel}: {e}"))
}

fn no_externals() -> Externals {
    Externals::with_path(None)
}

/// Externals that resolve programs out of `dir` and nowhere else.
fn externals_in(dir: &Path) -> Externals {
    Externals::with_path(Some(dir.as_os_str().to_os_string()))
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

fn install_on_path(root: &Path, home: &Path, opts: &InstallOpts) -> Result<String, cli::CliError> {
    run_install_with(root, home, opts, &McpCommand::OnPath, &no_externals())
}

fn base_opts() -> InstallOpts {
    InstallOpts {
        platform: None,
        scope: None,
        db: None,
        command: None,
        git_hooks: true,
        prewarm: false,
    }
}

fn claude_project_opts(db: &Path) -> InstallOpts {
    InstallOpts {
        platform: Some(Platform::ClaudeCode),
        scope: Some(Scope::Project),
        db: Some(db.to_path_buf()),
        ..base_opts()
    }
}

/// `--platform codex --project`, no `--db` — the default store, which is the
/// one `enable`'s fallback recovers (a Codex-only install stashes no store).
fn codex_project_opts() -> InstallOpts {
    InstallOpts {
        platform: Some(Platform::Codex),
        scope: Some(Scope::Project),
        ..base_opts()
    }
}

fn toggle(platform: Platform, scope: Scope) -> ToggleOpts {
    ToggleOpts {
        platform: Some(platform),
        scope: Some(scope),
    }
}

/// Make `root` look like a git checkout with a hooks directory.
fn git_repo(root: &Path) -> PathBuf {
    let hooks = root.join(".git").join("hooks");
    fs::create_dir_all(&hooks).unwrap();
    hooks
}

const MANIFEST_REL: &str = ".claude/skills/mushroom/.install-manifest.json";

// ---------------------------------------------------------------------------
// disable
// ---------------------------------------------------------------------------

#[test]
fn disable_removes_hooks_and_mcp_entry_and_keeps_skill_store_gitignore() {
    let root = temp_dir("disable");
    let home = temp_dir("disable-home");
    let db = root.join("mushroom-memory");
    let hooks_dir = git_repo(&root);
    let opts = claude_project_opts(&db);

    install_on_path(&root, &home, &opts).expect("install");

    // Sanity: everything install writes is there before we disable it.
    assert!(root.join(".mcp.json").exists());
    let settings_before: serde_json::Value = read_json(&root, ".claude/settings.json");
    assert!(settings_before["hooks"]["UserPromptSubmit"].is_array());
    assert!(settings_before["hooks"]["PostToolUse"].is_array());
    for name in ["post-commit", "post-checkout", "post-merge"] {
        assert!(
            fs::read_to_string(hooks_dir.join(name))
                .unwrap()
                .contains("mushroomdb sync"),
            "{name} missing the sync block before disable"
        );
    }

    let out = run_disable_with(
        &root,
        &home,
        &toggle(Platform::ClaudeCode, Scope::Project),
        &no_externals(),
    )
    .expect("disable");
    assert!(out.contains("disabled"), "{out}");
    assert!(
        out.contains("mushroomdb is disabled in") && out.contains("mushroomdb enable"),
        "{out}"
    );

    // MCP entry gone.
    let mcp: serde_json::Value = read_json(&root, ".mcp.json");
    assert!(
        mcp["mcpServers"]["mushroomdb"].is_null(),
        "mcp entry survived disable: {mcp}"
    );

    // Both hooks gone from settings.json.
    let settings: serde_json::Value = read_json(&root, ".claude/settings.json");
    assert!(
        settings["hooks"]["UserPromptSubmit"]
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(true),
        "{settings}"
    );
    assert!(
        settings["hooks"]["PostToolUse"]
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(true),
        "{settings}"
    );

    // Git hook blocks gone (files were created by install, so they are
    // removed entirely — same rule `uninstall` follows).
    for name in ["post-commit", "post-checkout", "post-merge"] {
        assert!(
            !hooks_dir.join(name).exists(),
            "{name} still has our block after disable"
        );
    }

    // The skill file, the .gitignore line, and the manifest itself all stay.
    let skill = read(&root, ".claude/skills/mushroom/SKILL.md");
    assert!(skill.contains(db.to_str().unwrap()));
    let gitignore = read(&root, ".gitignore");
    assert!(gitignore.contains("mushroom-memory"));
    let manifest: serde_json::Value = read_json(&root, MANIFEST_REL);
    assert_eq!(manifest["disabled"], true, "{manifest}");
    // The mcp_keys/hooks/git_hooks lists still describe what the install
    // owns — disable does not clear them, only takes the config off disk.
    assert!(!manifest["mcp_keys"].as_array().unwrap().is_empty());
    assert!(!manifest["hooks"].as_array().unwrap().is_empty());
    assert_eq!(manifest["git_hooks"].as_array().unwrap().len(), 3);
    // The stashed entry is what enable will read the store back out of.
    assert!(!manifest["stashed_mcp"].as_array().unwrap().is_empty());
}

#[test]
fn disable_is_idempotent() {
    let root = temp_dir("disable-idempotent");
    let home = temp_dir("disable-idempotent-home");
    let db = root.join("mushroom-memory");
    git_repo(&root);
    let opts = claude_project_opts(&db);
    install_on_path(&root, &home, &opts).expect("install");

    let toggle_opts = toggle(Platform::ClaudeCode, Scope::Project);
    run_disable_with(&root, &home, &toggle_opts, &no_externals()).expect("first disable");
    let manifest_after_first: serde_json::Value = read_json(&root, MANIFEST_REL);

    let second =
        run_disable_with(&root, &home, &toggle_opts, &no_externals()).expect("second disable");
    assert!(
        second.contains("already disabled"),
        "second disable should say so, got: {second}"
    );

    let manifest_after_second: serde_json::Value = read_json(&root, MANIFEST_REL);
    assert_eq!(
        manifest_after_first, manifest_after_second,
        "a repeated disable must not change the manifest"
    );
}

// ---------------------------------------------------------------------------
// enable
// ---------------------------------------------------------------------------

/// `enable` re-resolves an `npx`-form command fresh rather than replaying
/// whatever `disable` last saw — the published package may have moved to a
/// different cached path in between. Installed with prewarm off (the entry is
/// written as the literal `npx` request, unresolved); enabled with prewarm's
/// resolver pointed at a fake `npx` that answers `--print-binary` with a real
/// file. The written entry must be *that* file, not the literal string `npx`
/// replayed from the stash and not any path chosen at install time.
#[test]
fn enable_reresolves_an_npx_command_to_its_current_shape() {
    let root = temp_dir("enable-npx");
    let home = temp_dir("enable-npx-home");
    let db = root.join("mushroom-memory");
    let hooks_dir = git_repo(&root);
    let opts = claude_project_opts(&db);

    run_install_with(&root, &home, &opts, &McpCommand::npx(), &no_externals()).expect("install");
    let before: serde_json::Value = read_json(&root, ".mcp.json");
    assert_eq!(
        before["mcpServers"]["mushroomdb"]["command"], "npx",
        "sanity: install with no prewarm writes the literal npx form"
    );

    let toggle_opts = toggle(Platform::ClaudeCode, Scope::Project);
    run_disable_with(&root, &home, &toggle_opts, &no_externals()).expect("disable");

    let bin_dir = temp_dir("enable-npx-bin");
    let resolved = fake_program(&bin_dir, "resolved-mushroomdb", "exit 0\n");
    fake_program(
        &bin_dir,
        "npx",
        &format!("printf '%s\\n' '{}'\n", resolved.display()),
    );
    let out = run_enable_with(
        &root,
        &home,
        &toggle_opts,
        &McpCommand::npx(),
        &externals_in(&bin_dir),
    )
    .expect("enable");
    assert!(out.contains("mushroomdb is enabled in"), "{out}");

    let mcp: serde_json::Value = read_json(&root, ".mcp.json");
    assert_eq!(
        mcp["mcpServers"]["mushroomdb"]["command"],
        resolved.to_string_lossy().as_ref(),
        "{mcp}"
    );

    for name in ["post-commit", "post-checkout", "post-merge"] {
        let text = fs::read_to_string(hooks_dir.join(name)).unwrap();
        assert!(text.contains(resolved.to_str().unwrap()), "{name}: {text}");
    }
}

/// I1: `enable` restores an explicit `--command` pin verbatim rather than
/// falling back to auto-detection, even though the caller's `cmd` argument
/// (what `run_enable`'s real entrypoint always passes) names something else
/// entirely.
#[test]
fn enable_restores_an_explicit_command() {
    let root = temp_dir("enable-explicit");
    let home = temp_dir("enable-explicit-home");
    let db = root.join("mushroom-memory");
    let opts = claude_project_opts(&db);

    let pinned = root.join("pinned-mushroomdb");
    fs::write(&pinned, b"pinned").unwrap();
    run_install_with(
        &root,
        &home,
        &opts,
        &McpCommand::Explicit(pinned.clone()),
        &no_externals(),
    )
    .expect("install");

    let toggle_opts = toggle(Platform::ClaudeCode, Scope::Project);
    run_disable_with(&root, &home, &toggle_opts, &no_externals()).expect("disable");

    // The auto-detected fallback `run_enable`'s real entrypoint would pass —
    // deliberately something other than `pinned`, to prove it is not used.
    let out = run_enable_with(
        &root,
        &home,
        &toggle_opts,
        &McpCommand::OnPath,
        &no_externals(),
    )
    .expect("enable");
    assert!(!out.contains("warning"), "{out}");

    let mcp: serde_json::Value = read_json(&root, ".mcp.json");
    assert_eq!(
        mcp["mcpServers"]["mushroomdb"]["command"],
        pinned.to_string_lossy().as_ref(),
        "{mcp}"
    );

    let manifest: serde_json::Value = read_json(&root, MANIFEST_REL);
    assert_eq!(manifest["disabled"], false, "{manifest}");
}

/// I1: when the pinned binary a stash recorded no longer exists, `enable`
/// falls back to the caller's auto-detected command and says so.
#[test]
fn enable_falls_back_and_warns_when_the_pinned_binary_is_gone() {
    let root = temp_dir("enable-missing-pin");
    let home = temp_dir("enable-missing-pin-home");
    let db = root.join("mushroom-memory");
    let opts = claude_project_opts(&db);

    let pinned = root.join("pinned-mushroomdb");
    fs::write(&pinned, b"pinned").unwrap();
    run_install_with(
        &root,
        &home,
        &opts,
        &McpCommand::Explicit(pinned.clone()),
        &no_externals(),
    )
    .expect("install");

    let toggle_opts = toggle(Platform::ClaudeCode, Scope::Project);
    run_disable_with(&root, &home, &toggle_opts, &no_externals()).expect("disable");

    fs::remove_file(&pinned).unwrap();
    let out = run_enable_with(
        &root,
        &home,
        &toggle_opts,
        &McpCommand::OnPath,
        &no_externals(),
    )
    .expect("enable");
    assert!(out.contains("warning"), "{out}");
    assert!(out.contains(pinned.to_str().unwrap()), "{out}");

    let mcp: serde_json::Value = read_json(&root, ".mcp.json");
    assert_eq!(
        mcp["mcpServers"]["mushroomdb"]["command"], "mushroomdb",
        "{mcp}"
    );
}

/// Task 4 review finding, wired into `enable` here: after a `--platform all`
/// install, Claude Code and Cursor name the store differently (only Claude
/// Code resolves `--auto`; Cursor gets the pinned path — see
/// `platform_all_gives_each_host_the_form_it_can_resolve` in
/// tests/install.rs). A single recovered store applied to every platform
/// during `enable` would write Claude Code's `--auto` into Cursor's
/// `.cursor/mcp.json`, which Cursor cannot resolve — the same empty-store bug
/// Task 4 fixed for a fresh install. `enable` must recover each platform's
/// store from its own stashed entry and keep them apart.
#[test]
fn enable_after_platform_all_keeps_each_hosts_store_form() {
    let root = temp_dir("enable-all");
    let home = temp_dir("enable-all-home");
    let db = root.join("mushroom-memory");
    git_repo(&root);
    let opts = InstallOpts {
        platform: Some(Platform::All),
        scope: Some(Scope::Project),
        ..base_opts()
    };
    install_on_path(&root, &home, &opts).expect("install");

    // Sanity: install itself gives each host the form it can resolve.
    let claude_before: serde_json::Value = read_json(&root, ".mcp.json");
    assert_eq!(
        claude_before["mcpServers"]["mushroomdb"]["args"][1],
        "--auto"
    );
    let cursor_before: serde_json::Value = read_json(&root, ".cursor/mcp.json");
    assert_eq!(
        cursor_before["mcpServers"]["mushroomdb"]["args"][1],
        db.to_str().unwrap()
    );

    let toggle_opts = toggle(Platform::All, Scope::Project);
    run_disable_with(&root, &home, &toggle_opts, &no_externals()).expect("disable");
    assert!(read_json(&root, ".mcp.json")["mcpServers"]["mushroomdb"].is_null());
    assert!(read_json(&root, ".cursor/mcp.json")["mcpServers"]["mushroomdb"].is_null());

    let out = run_enable_with(
        &root,
        &home,
        &toggle_opts,
        &McpCommand::OnPath,
        &no_externals(),
    )
    .expect("enable");
    assert!(out.contains("mushroomdb is enabled in"), "{out}");

    let claude: serde_json::Value = read_json(&root, ".mcp.json");
    assert_eq!(
        claude["mcpServers"]["mushroomdb"]["args"][1], "--auto",
        "Claude Code must keep --auto after enable: {claude}"
    );
    let cursor: serde_json::Value = read_json(&root, ".cursor/mcp.json");
    assert_eq!(
        cursor["mcpServers"]["mushroomdb"]["args"][1],
        db.to_str().unwrap(),
        "Cursor must keep its pinned path after enable, not Claude Code's --auto: {cursor}"
    );
}

#[test]
fn enable_when_not_disabled_is_a_no_op() {
    let root = temp_dir("enable-noop");
    let home = temp_dir("enable-noop-home");
    let db = root.join("mushroom-memory");
    let opts = claude_project_opts(&db);
    install_on_path(&root, &home, &opts).expect("install");

    let before: serde_json::Value = read_json(&root, ".mcp.json");
    let out = run_enable_with(
        &root,
        &home,
        &toggle(Platform::ClaudeCode, Scope::Project),
        &McpCommand::OnPath,
        &no_externals(),
    )
    .expect("enable on an already-enabled install");
    assert!(out.contains("already enabled"), "{out}");
    let after: serde_json::Value = read_json(&root, ".mcp.json");
    assert_eq!(before, after, "enable must not touch a live install");
}

// ---------------------------------------------------------------------------
// install re-enables a disabled install
// ---------------------------------------------------------------------------

#[test]
fn install_on_disabled_enables() {
    let root = temp_dir("install-reenables");
    let home = temp_dir("install-reenables-home");
    let db = root.join("mushroom-memory");
    git_repo(&root);
    let opts = claude_project_opts(&db);

    install_on_path(&root, &home, &opts).expect("install");
    run_disable_with(
        &root,
        &home,
        &toggle(Platform::ClaudeCode, Scope::Project),
        &no_externals(),
    )
    .expect("disable");

    let manifest_disabled: serde_json::Value = read_json(&root, MANIFEST_REL);
    assert_eq!(manifest_disabled["disabled"], true);

    let out = install_on_path(&root, &home, &opts).expect("re-install");
    assert!(
        out.contains("re-enabled"),
        "install over a disabled install should say so, got: {out}"
    );

    let mcp: serde_json::Value = read_json(&root, ".mcp.json");
    assert!(!mcp["mcpServers"]["mushroomdb"].is_null(), "{mcp}");
    let manifest: serde_json::Value = read_json(&root, MANIFEST_REL);
    assert_eq!(manifest["disabled"], false, "{manifest}");
}

// ---------------------------------------------------------------------------
// uninstall from a disabled install
// ---------------------------------------------------------------------------

#[test]
fn uninstall_from_disabled_state_is_clean() {
    let root = temp_dir("uninstall-disabled");
    let home = temp_dir("uninstall-disabled-home");
    let db = root.join("mushroom-memory");
    let hooks_dir = git_repo(&root);
    let opts = claude_project_opts(&db);

    install_on_path(&root, &home, &opts).expect("install");
    run_disable(&root, &home, &toggle(Platform::ClaudeCode, Scope::Project)).expect("disable");

    run_uninstall(&root, &home, &opts).expect("uninstall from disabled state");

    assert!(!root.join(MANIFEST_REL).exists());
    assert!(!root.join(".claude/skills/mushroom/SKILL.md").exists());
    let gitignore_exists = root.join(".gitignore").exists();
    if gitignore_exists {
        let gi = fs::read_to_string(root.join(".gitignore")).unwrap();
        assert!(!gi.contains("mushroom-memory"), "{gi}");
    }
    for name in ["post-commit", "post-checkout", "post-merge"] {
        assert!(
            !hooks_dir.join(name).exists(),
            "{name} should already be gone from disable, and uninstall must not error on that"
        );
    }
    if root.join(".mcp.json").exists() {
        let mcp: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(root.join(".mcp.json")).unwrap()).unwrap();
        assert!(mcp["mcpServers"]["mushroomdb"].is_null(), "{mcp}");
    }
}

// ---------------------------------------------------------------------------
// codex
// ---------------------------------------------------------------------------

/// I2: `disable` hands the Codex removal back to `codex mcp remove`, the same
/// way `uninstall` already does (see `codex_platform_calls_codex_mcp_add` in
/// tests/install.rs, which this mirrors).
#[test]
fn disable_calls_codex_mcp_remove() {
    let root = temp_dir("disable-codex");
    let home = temp_dir("disable-codex-home");
    let bin_dir = temp_dir("disable-codex-bin");
    let log = bin_dir.join("argv.txt");
    fake_program(
        &bin_dir,
        "codex",
        &format!("printf '%s\\n' \"$@\" >> '{}'\n", log.display()),
    );

    let opts = codex_project_opts();
    run_install_with(
        &root,
        &home,
        &opts,
        &McpCommand::npx(),
        &externals_in(&bin_dir),
    )
    .expect("codex install");
    fs::remove_file(&log).unwrap();

    let out = run_disable_with(
        &root,
        &home,
        &toggle(Platform::Codex, Scope::Project),
        &externals_in(&bin_dir),
    )
    .expect("disable");
    assert!(out.contains("disabled"), "{out}");

    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        "mcp\nremove\nmushroomdb\n"
    );

    let manifest: serde_json::Value = read_json(&home, ".mushroomdb/install-manifest-codex.json");
    assert_eq!(manifest["disabled"], true, "{manifest}");
}

/// I2: `enable` re-registers Codex through `codex mcp add`, using the store
/// the original install used (recovered from the default — Codex's own
/// config is not a file this program reads, see `recover_store_for`) and the
/// command `enable` resolved (the `npx` form, since nothing here stashed a
/// different pin).
#[test]
fn enable_calls_codex_mcp_add() {
    let root = temp_dir("enable-codex");
    let home = temp_dir("enable-codex-home");
    let bin_dir = temp_dir("enable-codex-bin");
    let log = bin_dir.join("argv.txt");
    fake_program(
        &bin_dir,
        "codex",
        &format!("printf '%s\\n' \"$@\" >> '{}'\n", log.display()),
    );

    let opts = codex_project_opts();
    run_install_with(
        &root,
        &home,
        &opts,
        &McpCommand::npx(),
        &externals_in(&bin_dir),
    )
    .expect("codex install");

    let toggle_opts = toggle(Platform::Codex, Scope::Project);
    run_disable_with(&root, &home, &toggle_opts, &externals_in(&bin_dir)).expect("disable");
    fs::remove_file(&log).unwrap();

    let out = run_enable_with(
        &root,
        &home,
        &toggle_opts,
        &McpCommand::npx(),
        &externals_in(&bin_dir),
    )
    .expect("enable");
    assert!(out.contains("mushroomdb is enabled in"), "{out}");

    let db = root.join("mushroom-memory");
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        format!(
            "mcp\nadd\nmushroomdb\n--\nnpx\n-y\nmushroomdb@{VERSION}\nmcp\n{}\n",
            db.display()
        )
    );

    let manifest: serde_json::Value = read_json(&home, ".mushroomdb/install-manifest-codex.json");
    assert_eq!(manifest["disabled"], false, "{manifest}");
}

// ---------------------------------------------------------------------------
// nothing installed
// ---------------------------------------------------------------------------

/// M2: `disable`/`enable` share `uninstall`'s "no manifest" error rather than
/// panicking or reporting success over nothing.
#[test]
fn disable_with_nothing_installed_errors_clearly() {
    let root = temp_dir("disable-nothing");
    let home = temp_dir("disable-nothing-home");
    let err = run_disable_with(
        &root,
        &home,
        &toggle(Platform::ClaudeCode, Scope::Project),
        &no_externals(),
    )
    .expect_err("nothing installed");
    assert!(err.0.contains("disable"), "{}", err.0);
}

#[test]
fn enable_with_nothing_installed_errors_clearly() {
    let root = temp_dir("enable-nothing");
    let home = temp_dir("enable-nothing-home");
    let err = run_enable_with(
        &root,
        &home,
        &toggle(Platform::ClaudeCode, Scope::Project),
        &McpCommand::OnPath,
        &no_externals(),
    )
    .expect_err("nothing installed");
    assert!(err.0.contains("enable"), "{}", err.0);
}

// ---------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------

#[test]
fn doctor_reports_disabled_state() {
    use cli::doctor::{run_doctor_with, DoctorOpts};

    let root = temp_dir("doctor-disabled");
    let home = temp_dir("doctor-disabled-home");
    let db = root.join("mushroom-memory");
    let opts = claude_project_opts(&db);
    install_on_path(&root, &home, &opts).expect("install");

    run_disable(&root, &home, &toggle(Platform::ClaudeCode, Scope::Project)).expect("disable");

    let doctor_opts = DoctorOpts {
        platform: Some(Platform::ClaudeCode),
        scope: Some(Scope::Project),
    };
    let report = run_doctor_with(&root, &home, &doctor_opts, &no_externals()).expect("doctor");
    assert!(
        !report.had_fail,
        "disabled is not a failure: {}",
        report.output
    );
    let first_line = report.output.lines().next().unwrap_or_default();
    assert!(first_line.starts_with("warn"), "{first_line}");
    assert!(first_line.contains("disabled"), "{first_line}");
    assert!(first_line.contains("mushroomdb enable"), "{first_line}");
    assert_eq!(
        report.output.lines().count(),
        1,
        "no other check should run once state is reported disabled: {}",
        report.output
    );
}

// ---------------------------------------------------------------------------
// CLI parsing
// ---------------------------------------------------------------------------

#[test]
fn parse_disable_and_enable_flags() {
    use cli::parse_args;
    use cli::Command;

    match parse_args(&["disable", "--platform", "cursor", "--project"]).unwrap() {
        Command::Disable(opts) => {
            assert_eq!(opts.platform, Some(Platform::Cursor));
            assert_eq!(opts.scope, Some(Scope::Project));
        }
        other => panic!("expected Disable, got {other:?}"),
    }

    match parse_args(&["enable", "--user"]).unwrap() {
        Command::Enable(opts) => {
            assert_eq!(opts.platform, None);
            assert_eq!(opts.scope, Some(Scope::User));
        }
        other => panic!("expected Enable, got {other:?}"),
    }
}

#[test]
fn usage_mentions_enable_and_disable() {
    let text = cli::usage();
    assert!(text.contains("mushroomdb enable"), "{text}");
    assert!(text.contains("mushroomdb disable"), "{text}");
}
