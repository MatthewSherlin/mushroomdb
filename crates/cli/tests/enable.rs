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

#[test]
fn enable_restores_everything_with_current_shapes() {
    let root = temp_dir("enable");
    let home = temp_dir("enable-home");
    let db = root.join("mushroom-memory");
    let hooks_dir = git_repo(&root);
    let opts = claude_project_opts(&db);

    let old_bin = root.join("old-mushroomdb");
    fs::write(&old_bin, b"old").unwrap();
    run_install_with(
        &root,
        &home,
        &opts,
        &McpCommand::Explicit(old_bin.clone()),
        &no_externals(),
    )
    .expect("install");

    let toggle_opts = toggle(Platform::ClaudeCode, Scope::Project);
    run_disable_with(&root, &home, &toggle_opts, &no_externals()).expect("disable");

    // A different binary is "current" by the time enable runs — the
    // published package moved, or a developer pointed --command elsewhere.
    let new_bin = root.join("new-mushroomdb");
    fs::write(&new_bin, b"new").unwrap();
    let out = run_enable_with(
        &root,
        &home,
        &toggle_opts,
        &McpCommand::Explicit(new_bin.clone()),
        &no_externals(),
    )
    .expect("enable");
    assert!(out.contains("mushroomdb is enabled in"), "{out}");

    let mcp: serde_json::Value = read_json(&root, ".mcp.json");
    let command = mcp["mcpServers"]["mushroomdb"]["command"].as_str().unwrap();
    assert_eq!(command, new_bin.to_string_lossy(), "{mcp}");
    assert_ne!(command, old_bin.to_string_lossy());
    // The store name survived even though the command changed.
    assert_eq!(
        mcp["mcpServers"]["mushroomdb"]["args"][1].as_str().unwrap(),
        db.to_string_lossy()
    );

    let settings: serde_json::Value = read_json(&root, ".claude/settings.json");
    let recall_cmd = settings["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert!(
        recall_cmd.contains(new_bin.to_str().unwrap()),
        "{recall_cmd}"
    );
    assert!(
        !recall_cmd.contains(old_bin.to_str().unwrap()),
        "{recall_cmd}"
    );

    for name in ["post-commit", "post-checkout", "post-merge"] {
        let text = fs::read_to_string(hooks_dir.join(name)).unwrap();
        assert!(text.contains(new_bin.to_str().unwrap()), "{name}: {text}");
    }

    let manifest: serde_json::Value = read_json(&root, MANIFEST_REL);
    assert_eq!(manifest["disabled"], false, "{manifest}");
    assert!(
        manifest["stashed_mcp"].as_array().unwrap().is_empty(),
        "{manifest}"
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
