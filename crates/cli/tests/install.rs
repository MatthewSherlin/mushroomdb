//! Integration tests for `mushroomdb install` / `uninstall`.
//!
//! All tests operate on temp directories — never touching real HOME or CWD.
//! Drive via `run_install`/`run_uninstall` from the library (never spawn a
//! subprocess), so tests run without the binary being on PATH.

use cli::install::{run_install, run_uninstall, InstallOpts, Platform};
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
        "mushroomdb-install-test-{}-{}-{}",
        label,
        std::process::id(),
        nanos
    ));
    fs::create_dir_all(&d).unwrap();
    d
}

/// Read a file inside `root` at `rel`. Panics if missing.
fn read(root: &Path, rel: &str) -> String {
    fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("missing {rel}: {e}"))
}

/// Assert that a file inside `root` does NOT exist.
fn assert_absent(root: &Path, rel: &str) {
    assert!(
        !root.join(rel).exists(),
        "expected {rel} to be absent but it exists"
    );
}

/// Build opts for a --platform claude-code --project install with a fixed db path.
fn claude_project_opts(db: &Path) -> InstallOpts {
    InstallOpts {
        platform: Some(Platform::ClaudeCode),
        project: true,
        db: Some(db.to_path_buf()),
    }
}

/// Build opts for a --platform cursor --project install.
fn cursor_project_opts(db: &Path) -> InstallOpts {
    InstallOpts {
        platform: Some(Platform::Cursor),
        project: true,
        db: Some(db.to_path_buf()),
    }
}

/// Build opts for --platform all --project.
fn all_project_opts(db: &Path) -> InstallOpts {
    InstallOpts {
        platform: Some(Platform::All),
        project: true,
        db: Some(db.to_path_buf()),
    }
}

// ---------------------------------------------------------------------------
// Test: Claude Code project-scope install writes expected files
// ---------------------------------------------------------------------------

#[test]
fn claude_project_writes_skill_and_mcp() {
    let root = temp_dir("cc-proj");
    let home = temp_dir("cc-home");
    let db = root.join("mushroom-memory");
    let opts = claude_project_opts(&db);

    run_install(&root, &home, &opts).expect("install failed");

    // Skill file must exist and contain the db path.
    let skill = read(&root, ".claude/skills/mushroom/SKILL.md");
    assert!(
        skill.contains(db.to_str().unwrap()),
        "SKILL.md does not contain db path"
    );

    // .mcp.json must exist with correct server entry.
    let mcp_raw = read(&root, ".mcp.json");
    let mcp: serde_json::Value = serde_json::from_str(&mcp_raw).expect("invalid mcp json");
    let server = &mcp["mcpServers"]["mushroomdb"];
    assert_eq!(server["command"], "mushroomdb");
    assert_eq!(server["args"][0], "mcp");
    assert_eq!(server["args"][1], db.to_str().unwrap());

    // Manifest must exist.
    let manifest_raw = read(&root, ".claude/skills/mushroom/.install-manifest.json");
    let manifest: serde_json::Value =
        serde_json::from_str(&manifest_raw).expect("invalid manifest json");
    assert!(manifest["files"].is_array());

    // Nothing written to home.
    assert_absent(&home, ".claude/skills/mushroom/SKILL.md");
}

// ---------------------------------------------------------------------------
// Test: install is idempotent (second run exits OK, no duplicates)
// ---------------------------------------------------------------------------

#[test]
fn install_is_idempotent() {
    let root = temp_dir("idempotent");
    let home = temp_dir("idempotent-home");
    let db = root.join("mushroom-memory");
    let opts = claude_project_opts(&db);

    run_install(&root, &home, &opts).expect("first install failed");
    let mcp_after_first = read(&root, ".mcp.json");

    run_install(&root, &home, &opts).expect("second install should be a no-op (exit 0)");
    let mcp_after_second = read(&root, ".mcp.json");

    // Content must be identical — no duplication.
    assert_eq!(mcp_after_first, mcp_after_second);

    // mushroomdb key appears exactly once.
    let mcp: serde_json::Value = serde_json::from_str(&mcp_after_second).unwrap();
    let servers = mcp["mcpServers"].as_object().unwrap();
    assert_eq!(
        servers
            .keys()
            .filter(|k| k.as_str() == "mushroomdb")
            .count(),
        1,
        "mushroomdb key duplicated in mcp.json"
    );
}

// ---------------------------------------------------------------------------
// Test: conflicting existing .mcp.json entry → non-zero error, no changes
// ---------------------------------------------------------------------------

#[test]
fn install_refuses_conflicting_mcp_entry() {
    let root = temp_dir("conflict");
    let home = temp_dir("conflict-home");
    let db = root.join("mushroom-memory");
    let opts = claude_project_opts(&db);

    // Pre-create a .mcp.json with a different mushroomdb entry.
    let existing_mcp = serde_json::json!({
        "mcpServers": {
            "mushroomdb": {
                "command": "mushroomdb",
                "args": ["mcp", "/some/other/db"]
            }
        }
    });
    fs::write(
        root.join(".mcp.json"),
        serde_json::to_string_pretty(&existing_mcp).unwrap(),
    )
    .unwrap();

    let result = run_install(&root, &home, &opts);
    assert!(result.is_err(), "expected error on conflicting mcp entry");

    // The .mcp.json must be unchanged (original content preserved).
    let mcp_after: serde_json::Value = serde_json::from_str(&read(&root, ".mcp.json")).unwrap();
    assert_eq!(
        mcp_after["mcpServers"]["mushroomdb"]["args"][1],
        "/some/other/db"
    );

    // Skill file must NOT have been written.
    assert_absent(&root, ".claude/skills/mushroom/SKILL.md");
}

// ---------------------------------------------------------------------------
// Test: uninstall removes exactly manifest contents, leaves user files
// ---------------------------------------------------------------------------

#[test]
fn uninstall_removes_manifest_contents() {
    let root = temp_dir("uninstall");
    let home = temp_dir("uninstall-home");
    let db = root.join("mushroom-memory");
    let opts = claude_project_opts(&db);

    // Plant a user file that should survive uninstall.
    fs::create_dir_all(root.join(".claude/skills/mushroom")).unwrap();
    fs::write(
        root.join(".claude/skills/mushroom/user-notes.md"),
        "my notes",
    )
    .unwrap();

    run_install(&root, &home, &opts).expect("install");
    run_uninstall(&root, &home, &opts).expect("uninstall");

    // Files we created must be gone.
    assert_absent(&root, ".claude/skills/mushroom/SKILL.md");
    assert_absent(&root, ".claude/skills/mushroom/.install-manifest.json");

    // .mcp.json must no longer have mushroomdb key.
    if root.join(".mcp.json").exists() {
        let mcp: serde_json::Value = serde_json::from_str(&read(&root, ".mcp.json")).unwrap();
        assert!(
            mcp["mcpServers"]["mushroomdb"].is_null(),
            "mushroomdb key still in .mcp.json after uninstall"
        );
    }

    // User file must survive.
    assert!(
        root.join(".claude/skills/mushroom/user-notes.md").exists(),
        "uninstall removed a user file"
    );
}

// ---------------------------------------------------------------------------
// Test: --platform cursor writes only cursor artifacts
// ---------------------------------------------------------------------------

#[test]
fn cursor_only_writes_cursor_artifacts() {
    let root = temp_dir("cursor");
    let home = temp_dir("cursor-home");
    let db = root.join("mushroom-memory");
    let opts = cursor_project_opts(&db);

    run_install(&root, &home, &opts).expect("cursor install");

    // Cursor rule file must exist.
    let rules = read(&root, ".cursor/rules/mushroom.mdc");
    assert!(
        rules.contains("alwaysApply: true"),
        "cursor rules missing alwaysApply frontmatter"
    );
    assert!(
        rules.contains(db.to_str().unwrap()),
        "cursor rules do not contain db path"
    );

    // Cursor .mcp.json must have the entry.
    let mcp_raw = read(&root, ".cursor/mcp.json");
    let mcp: serde_json::Value = serde_json::from_str(&mcp_raw).expect("invalid cursor mcp json");
    assert_eq!(mcp["mcpServers"]["mushroomdb"]["command"], "mushroomdb");

    // Claude Code files must NOT exist.
    assert_absent(&root, ".claude/skills/mushroom/SKILL.md");
    assert_absent(&root, ".mcp.json");
}

// ---------------------------------------------------------------------------
// Test: --platform all writes both platform artifacts
// ---------------------------------------------------------------------------

#[test]
fn all_platform_writes_both_artifacts() {
    let root = temp_dir("all");
    let home = temp_dir("all-home");
    let db = root.join("mushroom-memory");
    let opts = all_project_opts(&db);

    run_install(&root, &home, &opts).expect("all-platform install");

    // Both skill and cursor rules must exist.
    assert!(root.join(".claude/skills/mushroom/SKILL.md").exists());
    assert!(root.join(".cursor/rules/mushroom.mdc").exists());
    assert!(root.join(".mcp.json").exists());
    assert!(root.join(".cursor/mcp.json").exists());
}

// ---------------------------------------------------------------------------
// Test: auto-detect picks claude-code when ~/.claude exists
// ---------------------------------------------------------------------------

#[test]
fn autodetect_home_claude_selects_claude_code() {
    let root = temp_dir("autodetect-cc");
    let home = temp_dir("autodetect-cc-home");
    let db = root.join("mushroom-memory");

    // Plant ~/.claude to trigger detection.
    fs::create_dir_all(home.join(".claude")).unwrap();

    let opts = InstallOpts {
        platform: None,
        project: true,
        db: Some(db.clone()),
    };

    run_install(&root, &home, &opts).expect("autodetect install");
    assert!(root.join(".claude/skills/mushroom/SKILL.md").exists());
    assert_absent(&root, ".cursor/rules/mushroom.mdc");
}

// ---------------------------------------------------------------------------
// Test: auto-detect picks cursor when .cursor/ exists in project root
// ---------------------------------------------------------------------------

#[test]
fn autodetect_project_cursor_selects_cursor() {
    let root = temp_dir("autodetect-cursor");
    let home = temp_dir("autodetect-cursor-home");
    let db = root.join("mushroom-memory");

    // Plant project-level .cursor/ to trigger detection.
    fs::create_dir_all(root.join(".cursor")).unwrap();

    let opts = InstallOpts {
        platform: None,
        project: true,
        db: Some(db.clone()),
    };

    run_install(&root, &home, &opts).expect("autodetect cursor install");
    assert!(root.join(".cursor/rules/mushroom.mdc").exists());
    assert_absent(&root, ".claude/skills/mushroom/SKILL.md");
}

// ---------------------------------------------------------------------------
// Test: auto-detect errors when neither platform is detected
// ---------------------------------------------------------------------------

#[test]
fn autodetect_neither_returns_error() {
    let root = temp_dir("autodetect-none");
    let home = temp_dir("autodetect-none-home");
    let db = root.join("mushroom-memory");

    let opts = InstallOpts {
        platform: None,
        project: true,
        db: Some(db),
    };

    let result = run_install(&root, &home, &opts);
    assert!(result.is_err(), "expected error when no platform detected");
    let msg = result.unwrap_err().0;
    assert!(
        msg.contains("claude") || msg.contains("cursor") || msg.contains("detect"),
        "error message should mention detection: {msg}"
    );
}

// ---------------------------------------------------------------------------
// Test: parse --platform flag round-trips through parse_args
// ---------------------------------------------------------------------------

#[test]
fn parse_install_platform_flag() {
    use cli::parse_args;
    use cli::Command;

    let cmd = parse_args(&["install", "--platform", "cursor", "--project"]).unwrap();
    match cmd {
        Command::Install(opts) => {
            assert_eq!(opts.platform, Some(Platform::Cursor));
            assert!(opts.project);
        }
        other => panic!("expected Install, got {other:?}"),
    }
}

#[test]
fn parse_uninstall_platform_flag() {
    use cli::parse_args;
    use cli::Command;

    let cmd = parse_args(&["uninstall", "--platform", "claude-code"]).unwrap();
    match cmd {
        Command::Uninstall(opts) => {
            assert_eq!(opts.platform, Some(Platform::ClaudeCode));
            assert!(!opts.project);
        }
        other => panic!("expected Uninstall, got {other:?}"),
    }
}

#[test]
fn parse_install_db_flag() {
    use cli::parse_args;
    use cli::Command;

    let cmd = parse_args(&["install", "--db", "/tmp/mydb", "--platform", "all"]).unwrap();
    match cmd {
        Command::Install(opts) => {
            assert_eq!(opts.db, Some(PathBuf::from("/tmp/mydb")));
            assert_eq!(opts.platform, Some(Platform::All));
        }
        other => panic!("expected Install, got {other:?}"),
    }
}
