//! Integration tests for `mushroomdb install` / `uninstall`.
//!
//! All tests operate on temp directories — never touching real HOME or CWD.
//! Drive via `run_install`/`run_uninstall` from the library (never spawn a
//! subprocess), so tests run without the binary being on PATH.

use cli::install::{run_install_with, run_uninstall, BinaryLocation, InstallOpts, Platform};
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

/// Install as if `mushroomdb` resolves on PATH (the bare-name MCP command).
fn install_on_path(root: &Path, home: &Path, opts: &InstallOpts) -> Result<String, cli::CliError> {
    run_install_with(root, home, opts, &BinaryLocation::OnPath)
}

/// Write a small fake executable to stand in for the running binary.
fn fake_exe(dir: &Path, bytes: &[u8]) -> PathBuf {
    let p = dir.join("fake-mushroomdb");
    fs::write(&p, bytes).unwrap();
    p
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

    install_on_path(&root, &home, &opts).expect("install failed");

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

    install_on_path(&root, &home, &opts).expect("first install failed");
    let mcp_after_first = read(&root, ".mcp.json");

    install_on_path(&root, &home, &opts).expect("second install should be a no-op (exit 0)");
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
// Test: install → install → uninstall removes everything (manifest survives
//       a no-op second install and uninstall still cleans up)
// ---------------------------------------------------------------------------

#[test]
fn double_install_then_uninstall_cleans_up() {
    let root = temp_dir("double-uninstall");
    let home = temp_dir("double-uninstall-home");
    let db = root.join("mushroom-memory");
    let opts = claude_project_opts(&db);

    install_on_path(&root, &home, &opts).expect("first install");
    install_on_path(&root, &home, &opts).expect("second install (no-op)");
    run_uninstall(&root, &home, &opts).expect("uninstall after double-install");

    // Skill file must be gone.
    assert_absent(&root, ".claude/skills/mushroom/SKILL.md");
    // Manifest must be gone.
    assert_absent(&root, ".claude/skills/mushroom/.install-manifest.json");
    // MCP entry must be gone.
    if root.join(".mcp.json").exists() {
        let mcp: serde_json::Value = serde_json::from_str(&read(&root, ".mcp.json")).unwrap();
        assert!(
            mcp["mcpServers"]["mushroomdb"].is_null(),
            "mushroomdb key still in .mcp.json after double-install → uninstall"
        );
    }
}

// ---------------------------------------------------------------------------
// Test: install → edit SKILL.md → install (repairs it) → uninstall cleans all
//       (covers manifest-union: MCP entry must stay in manifest after partial re-install)
// ---------------------------------------------------------------------------

#[test]
fn partial_drift_reinstall_then_uninstall_cleans_all() {
    let root = temp_dir("partial-drift");
    let home = temp_dir("partial-drift-home");
    let db = root.join("mushroom-memory");
    let opts = claude_project_opts(&db);

    // First install — writes SKILL.md + .mcp.json + manifest.
    install_on_path(&root, &home, &opts).expect("first install");

    // Simulate user editing SKILL.md (drift).
    let skill_path = root.join(".claude/skills/mushroom/SKILL.md");
    fs::write(&skill_path, "user edited this").unwrap();

    // Second install — detects drift, rewrites SKILL.md; MCP entry is intact so
    // nothing is added to manifest.mcp_keys this run. The manifest must be
    // union-merged so the MCP key is NOT dropped from the saved manifest.
    install_on_path(&root, &home, &opts).expect("re-install after drift");
    assert!(
        skill_path.exists(),
        "SKILL.md should be restored after re-install"
    );
    let skill_content = fs::read_to_string(&skill_path).unwrap();
    assert!(
        skill_content.contains(db.to_str().unwrap()),
        "restored SKILL.md should contain the db path"
    );

    // Uninstall must remove SKILL.md, manifest, AND the MCP key (which was in
    // the union-merged manifest, not just the partial this-run manifest).
    run_uninstall(&root, &home, &opts).expect("uninstall after partial drift");

    assert_absent(&root, ".claude/skills/mushroom/SKILL.md");
    assert_absent(&root, ".claude/skills/mushroom/.install-manifest.json");
    if root.join(".mcp.json").exists() {
        let mcp: serde_json::Value = serde_json::from_str(&read(&root, ".mcp.json")).unwrap();
        assert!(
            mcp["mcpServers"]["mushroomdb"].is_null(),
            "mushroomdb mcp key orphaned after partial-drift uninstall"
        );
    }
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

    let result = install_on_path(&root, &home, &opts);
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

    install_on_path(&root, &home, &opts).expect("install");
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

    install_on_path(&root, &home, &opts).expect("cursor install");

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

    install_on_path(&root, &home, &opts).expect("all-platform install");

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

    install_on_path(&root, &home, &opts).expect("autodetect install");
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

    install_on_path(&root, &home, &opts).expect("autodetect cursor install");
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

    let result = install_on_path(&root, &home, &opts);
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

// ---------------------------------------------------------------------------
// Test: binary NOT on PATH → install copies it to ~/.mushroomdb/bin and
//       writes that absolute path as the MCP command (npx / local build case)
// ---------------------------------------------------------------------------

#[test]
fn off_path_install_copies_binary_and_writes_absolute_command() {
    let root = temp_dir("offpath");
    let home = temp_dir("offpath-home");
    let db = root.join("mushroom-memory");
    let opts = claude_project_opts(&db);
    let src = fake_exe(&root, b"#!/bin/sh\necho v1\n");

    run_install_with(&root, &home, &opts, &BinaryLocation::CopyFrom(src.clone()))
        .expect("off-path install failed");

    // Binary copied to the stable location, byte-identical, executable.
    let copied = home.join(".mushroomdb/bin/mushroomdb");
    assert!(
        copied.exists(),
        "binary was not copied to ~/.mushroomdb/bin"
    );
    assert_eq!(fs::read(&copied).unwrap(), fs::read(&src).unwrap());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&copied).unwrap().permissions().mode();
        assert!(
            mode & 0o111 != 0,
            "copied binary is not executable: {mode:o}"
        );
    }

    // MCP command is the absolute path of the copy — no PATH lookup needed.
    let mcp: serde_json::Value = serde_json::from_str(&read(&root, ".mcp.json")).unwrap();
    let server = &mcp["mcpServers"]["mushroomdb"];
    assert_eq!(server["command"], copied.to_str().unwrap());
    assert_eq!(server["args"][0], "mcp");
    assert_eq!(server["args"][1], db.to_str().unwrap());

    // SKILL.md bootstrap uses the same absolute path so `demo` works too.
    let skill = read(&root, ".claude/skills/mushroom/SKILL.md");
    assert!(
        skill.contains(&format!("'{}' demo '{}'", copied.display(), db.display())),
        "SKILL.md bootstrap does not use the copied binary path"
    );
    assert!(
        !skill.contains("{{BIN}}"),
        "unsubstituted {{{{BIN}}}} in SKILL.md"
    );

    // Manifest tracks the copy so uninstall removes it.
    let manifest: serde_json::Value = serde_json::from_str(&read(
        &root,
        ".claude/skills/mushroom/.install-manifest.json",
    ))
    .unwrap();
    let files: Vec<&str> = manifest["files"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(
        files.contains(&copied.to_str().unwrap()),
        "manifest does not track copied binary: {files:?}"
    );

    run_uninstall(&root, &home, &opts).expect("uninstall");
    assert!(!copied.exists(), "uninstall left the copied binary behind");
}

// ---------------------------------------------------------------------------
// Test: existing entry with same db but a stale/broken command is REPAIRED,
//       not treated as a conflict (the "installed but never on PATH" case)
// ---------------------------------------------------------------------------

#[test]
fn reinstall_repairs_stale_command_for_same_db() {
    let root = temp_dir("repair");
    let home = temp_dir("repair-home");
    let db = root.join("mushroom-memory");
    let opts = claude_project_opts(&db);
    let src = fake_exe(&root, b"v1");

    // Simulate a prior install that wrote the bare name, which never resolved.
    let stale = serde_json::json!({
        "mcpServers": {
            "mushroomdb": { "command": "mushroomdb", "args": ["mcp", db.to_str().unwrap()] },
            "other": { "command": "other-tool", "args": [] }
        }
    });
    fs::write(
        root.join(".mcp.json"),
        serde_json::to_string_pretty(&stale).unwrap(),
    )
    .unwrap();

    run_install_with(&root, &home, &opts, &BinaryLocation::CopyFrom(src))
        .expect("re-install with same db must repair, not conflict");

    let mcp: serde_json::Value = serde_json::from_str(&read(&root, ".mcp.json")).unwrap();
    let copied = home.join(".mushroomdb/bin/mushroomdb");
    assert_eq!(
        mcp["mcpServers"]["mushroomdb"]["command"],
        copied.to_str().unwrap()
    );
    assert_eq!(
        mcp["mcpServers"]["mushroomdb"]["args"][1],
        db.to_str().unwrap()
    );
    // Unrelated servers untouched.
    assert_eq!(mcp["mcpServers"]["other"]["command"], "other-tool");
    // Skill bootstrap uses the repaired command too.
    let skill = read(&root, ".claude/skills/mushroom/SKILL.md");
    assert!(skill.contains(&format!("'{}' demo '{}'", copied.display(), db.display())));
}

// ---------------------------------------------------------------------------
// Test: re-running install FROM the stable copy itself (src == dest) is a
//       clean no-op — no error, no self-copy, binary still present
// ---------------------------------------------------------------------------

#[test]
fn reinstall_from_stable_copy_is_noop() {
    let root = temp_dir("selfcopy");
    let home = temp_dir("selfcopy-home");
    let db = root.join("mushroom-memory");
    let opts = claude_project_opts(&db);
    let copied = home.join(".mushroomdb/bin/mushroomdb");

    let src = fake_exe(&root, b"v1");
    run_install_with(&root, &home, &opts, &BinaryLocation::CopyFrom(src)).expect("first");

    let out = run_install_with(
        &root,
        &home,
        &opts,
        &BinaryLocation::CopyFrom(copied.clone()),
    )
    .expect("re-install from the stable copy");
    assert!(out.contains("already installed"), "{out}");
    assert_eq!(fs::read(&copied).unwrap(), b"v1");
}

// ---------------------------------------------------------------------------
// Test: if a later step fails after the binary was copied, the manifest still
//       records the copy so uninstall can remove it (no orphan)
// ---------------------------------------------------------------------------

#[test]
#[cfg(unix)]
fn partial_failure_after_copy_still_tracks_binary() {
    use std::os::unix::fs::PermissionsExt;
    let root = temp_dir("partial-fail");
    let home = temp_dir("partial-fail-home");
    let db = root.join("mushroom-memory");
    let opts = all_project_opts(&db);
    let src = fake_exe(&root, b"v1");

    // Claude Code step succeeds; make the Cursor rules dir unwritable so the
    // second platform fails after the binary copy and the first platform.
    let rules_dir = root.join(".cursor/rules");
    fs::create_dir_all(&rules_dir).unwrap();
    fs::set_permissions(&rules_dir, fs::Permissions::from_mode(0o500)).unwrap();

    let result = run_install_with(&root, &home, &opts, &BinaryLocation::CopyFrom(src));
    // Restore perms so temp cleanup works.
    fs::set_permissions(&rules_dir, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(result.is_err(), "expected cursor rules write to fail");

    let copied = home.join(".mushroomdb/bin/mushroomdb");
    assert!(copied.exists());
    let manifest: serde_json::Value = serde_json::from_str(&read(
        &root,
        ".claude/skills/mushroom/.install-manifest.json",
    ))
    .unwrap();
    let files: Vec<&str> = manifest["files"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(files.contains(&copied.to_str().unwrap()), "{files:?}");

    run_uninstall(&root, &home, &opts).expect("uninstall after partial failure");
    assert!(!copied.exists(), "orphaned binary after uninstall");
}

// ---------------------------------------------------------------------------
// Test: re-running install from a newer binary refreshes the stable copy
// ---------------------------------------------------------------------------

#[test]
fn off_path_reinstall_refreshes_copied_binary() {
    let root = temp_dir("refresh");
    let home = temp_dir("refresh-home");
    let db = root.join("mushroom-memory");
    let opts = claude_project_opts(&db);
    let copied = home.join(".mushroomdb/bin/mushroomdb");

    let v1 = fake_exe(&root, b"v1");
    run_install_with(&root, &home, &opts, &BinaryLocation::CopyFrom(v1)).expect("v1");
    assert_eq!(fs::read(&copied).unwrap(), b"v1");

    // Same bytes → no-op.
    let v1_again = fake_exe(&root, b"v1");
    let out = run_install_with(&root, &home, &opts, &BinaryLocation::CopyFrom(v1_again))
        .expect("v1 again");
    assert!(
        out.contains("already installed"),
        "same binary should be a no-op: {out}"
    );

    // New bytes → copy refreshed.
    let v2 = fake_exe(&root, b"v2-newer");
    run_install_with(&root, &home, &opts, &BinaryLocation::CopyFrom(v2)).expect("v2");
    assert_eq!(fs::read(&copied).unwrap(), b"v2-newer");
}

// ---------------------------------------------------------------------------
// Test: on-PATH install substitutes the bare name into the skill bootstrap
// ---------------------------------------------------------------------------

#[test]
fn on_path_install_uses_bare_name_everywhere() {
    let root = temp_dir("onpath-bin");
    let home = temp_dir("onpath-bin-home");
    let db = root.join("mushroom-memory");
    let opts = all_project_opts(&db);

    install_on_path(&root, &home, &opts).expect("install");

    let skill = read(&root, ".claude/skills/mushroom/SKILL.md");
    assert!(skill.contains(&format!("'mushroomdb' demo '{}'", db.display())));
    assert!(!skill.contains("{{BIN}}"));
    let rules = read(&root, ".cursor/rules/mushroom.mdc");
    assert!(rules.contains(&format!("'mushroomdb' demo '{}'", db.display())));
    assert!(!rules.contains("{{BIN}}"));
    assert_absent(&home, ".mushroomdb/bin/mushroomdb");
}

// ---------------------------------------------------------------------------
// Test: Claude Code install adds a UserPromptSubmit recall hook, merges with
//       existing hooks, is idempotent, and uninstall removes exactly it
// ---------------------------------------------------------------------------

#[test]
fn install_adds_recall_hook_and_uninstall_removes_only_it() {
    let root = temp_dir("hook");
    let home = temp_dir("hook-home");
    let db = root.join("mushroom-memory");
    let opts = claude_project_opts(&db);

    // Pre-existing user hook that must survive.
    fs::create_dir_all(root.join(".claude")).unwrap();
    fs::write(
        root.join(".claude/settings.json"),
        r#"{"permissions":{"allow":["Bash"]},"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"echo hi"}]}]}}"#,
    )
    .unwrap();

    install_on_path(&root, &home, &opts).expect("install");
    let s: serde_json::Value = serde_json::from_str(&read(&root, ".claude/settings.json")).unwrap();
    let ups = s["hooks"]["UserPromptSubmit"]
        .as_array()
        .expect("UserPromptSubmit array");
    assert_eq!(ups.len(), 1);
    let cmd = ups[0]["hooks"][0]["command"].as_str().unwrap();
    assert_eq!(cmd, format!("'mushroomdb' recall '{}'", db.display()));
    assert_eq!(ups[0]["hooks"][0]["timeout"], 5);
    // user's things untouched
    assert_eq!(s["permissions"]["allow"][0], "Bash");
    assert_eq!(
        s["hooks"]["SessionStart"][0]["hooks"][0]["command"],
        "echo hi"
    );

    // idempotent
    let out = install_on_path(&root, &home, &opts).expect("second install");
    assert!(out.contains("already installed"), "{out}");
    let s2: serde_json::Value =
        serde_json::from_str(&read(&root, ".claude/settings.json")).unwrap();
    assert_eq!(s2["hooks"]["UserPromptSubmit"].as_array().unwrap().len(), 1);

    // manifest tracks it
    let m: serde_json::Value = serde_json::from_str(&read(
        &root,
        ".claude/skills/mushroom/.install-manifest.json",
    ))
    .unwrap();
    assert_eq!(m["hooks"][0]["event"], "UserPromptSubmit");

    run_uninstall(&root, &home, &opts).expect("uninstall");
    let s3: serde_json::Value =
        serde_json::from_str(&read(&root, ".claude/settings.json")).unwrap();
    assert!(
        s3["hooks"]["UserPromptSubmit"].is_null()
            || s3["hooks"]["UserPromptSubmit"]
                .as_array()
                .unwrap()
                .is_empty()
    );
    assert_eq!(
        s3["hooks"]["SessionStart"][0]["hooks"][0]["command"],
        "echo hi"
    );
    assert_eq!(s3["permissions"]["allow"][0], "Bash");
}

#[test]
fn user_scope_hook_goes_to_home_settings_and_uses_absolute_bin() {
    let root = temp_dir("hook-user");
    let home = temp_dir("hook-user-home");
    fs::create_dir_all(home.join(".claude")).unwrap();
    let opts = InstallOpts {
        platform: Some(Platform::ClaudeCode),
        project: false,
        db: None,
    };
    let src = fake_exe(&root, b"v1");
    run_install_with(&root, &home, &opts, &BinaryLocation::CopyFrom(src)).expect("install");
    let s: serde_json::Value = serde_json::from_str(&read(&home, ".claude/settings.json")).unwrap();
    let cmd = s["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert!(
        cmd.starts_with(&format!(
            "'{}",
            home.join(".mushroomdb/bin/mushroomdb").display()
        )),
        "{cmd}"
    );
    assert!(
        cmd.ends_with(&format!(
            "recall '{}'",
            home.join(".mushroomdb/memory").display()
        )),
        "{cmd}"
    );
    assert_absent(&root, ".claude/settings.json");
}

#[test]
fn old_manifest_without_hooks_field_still_uninstalls() {
    let root = temp_dir("old-manifest");
    let home = temp_dir("old-manifest-home");
    let db = root.join("mushroom-memory");
    let opts = claude_project_opts(&db);
    install_on_path(&root, &home, &opts).expect("install");
    // Strip the hooks field to simulate a 0.4.x manifest.
    let p = root.join(".claude/skills/mushroom/.install-manifest.json");
    let mut m: serde_json::Value = serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
    m.as_object_mut().unwrap().remove("hooks");
    fs::write(&p, serde_json::to_string_pretty(&m).unwrap()).unwrap();
    run_uninstall(&root, &home, &opts).expect("uninstall with old manifest");
}

// ---------------------------------------------------------------------------
// Test: the hook command is shell-quoted — a project or db path containing a
//       space must not be word-split by the shell that runs the hook
// ---------------------------------------------------------------------------

#[test]
fn hook_command_is_shell_quoted_for_paths_with_spaces() {
    let root = temp_dir("hook-space");
    let home = temp_dir("hook-space-home");
    let db = root.join("mushroom memory"); // deliberately contains a space
    let opts = claude_project_opts(&db);

    install_on_path(&root, &home, &opts).expect("install");
    let s: serde_json::Value = serde_json::from_str(&read(&root, ".claude/settings.json")).unwrap();
    let cmd = s["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert_eq!(cmd, format!("'mushroomdb' recall '{}'", db.display()));
}

// ---------------------------------------------------------------------------
// Test: uninstall after the user hand-deletes the whole `hooks` key must
//       leave settings.json exactly as-is — no `hooks: {event: null}` clobber
// ---------------------------------------------------------------------------

#[test]
fn uninstall_after_hand_deleting_hooks_key_leaves_settings_unchanged() {
    let root = temp_dir("hooks-key-deleted");
    let home = temp_dir("hooks-key-deleted-home");
    let db = root.join("mushroom-memory");
    let opts = claude_project_opts(&db);

    install_on_path(&root, &home, &opts).expect("install");

    let settings_path = root.join(".claude/settings.json");
    let mut s: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
    s.as_object_mut().unwrap().remove("hooks");
    let stripped = serde_json::to_string_pretty(&s).unwrap();
    fs::write(&settings_path, &stripped).unwrap();

    run_uninstall(&root, &home, &opts).expect("uninstall despite missing hooks key");

    assert_eq!(read(&root, ".claude/settings.json"), stripped);
}

// ---------------------------------------------------------------------------
// Test: install refuses (does not clobber) a `hooks` value that is not a
//       JSON object
// ---------------------------------------------------------------------------

#[test]
fn install_refuses_when_hooks_key_is_not_an_object() {
    let root = temp_dir("hooks-not-object");
    let home = temp_dir("hooks-not-object-home");
    let db = root.join("mushroom-memory");
    let opts = claude_project_opts(&db);

    fs::create_dir_all(root.join(".claude")).unwrap();
    let original = r#"{"hooks":[]}"#;
    fs::write(root.join(".claude/settings.json"), original).unwrap();

    let result = install_on_path(&root, &home, &opts);
    assert!(
        result.is_err(),
        "expected error when hooks is not an object"
    );
    assert_eq!(read(&root, ".claude/settings.json"), original);
}

// ---------------------------------------------------------------------------
// Test: install refuses (does not panic) when the settings.json top level is
//       not a JSON object
// ---------------------------------------------------------------------------

#[test]
fn install_refuses_when_settings_root_is_not_an_object() {
    let root = temp_dir("settings-root-not-object");
    let home = temp_dir("settings-root-not-object-home");
    let db = root.join("mushroom-memory");
    let opts = claude_project_opts(&db);

    fs::create_dir_all(root.join(".claude")).unwrap();
    let original = "[]";
    fs::write(root.join(".claude/settings.json"), original).unwrap();

    let result = install_on_path(&root, &home, &opts);
    assert!(
        result.is_err(),
        "expected a clean error, not a panic, for a non-object settings root"
    );
    assert_eq!(read(&root, ".claude/settings.json"), original);
}

// ---------------------------------------------------------------------------
// Test: uninstall on a group that mixes a user hook with ours removes only
//       ours and leaves the user's hook in place
// ---------------------------------------------------------------------------

#[test]
fn uninstall_leaves_mixed_hook_group_with_user_hook_intact() {
    let root = temp_dir("mixed-group");
    let home = temp_dir("mixed-group-home");
    let db = root.join("mushroom-memory");
    let opts = claude_project_opts(&db);

    install_on_path(&root, &home, &opts).expect("install");

    // Hand-edit settings.json so our entry shares a group with a user hook,
    // as if the user had merged the two by hand.
    let settings_path = root.join(".claude/settings.json");
    let our_cmd = format!("'mushroomdb' recall '{}'", db.display());
    let mixed = serde_json::json!({
        "hooks": {
            "UserPromptSubmit": [
                {
                    "hooks": [
                        {"type": "command", "command": "echo hi"},
                        {"type": "command", "command": our_cmd, "timeout": 5}
                    ]
                }
            ]
        }
    });
    fs::write(
        &settings_path,
        serde_json::to_string_pretty(&mixed).unwrap(),
    )
    .unwrap();

    run_uninstall(&root, &home, &opts).expect("uninstall");

    let s: serde_json::Value = serde_json::from_str(&read(&root, ".claude/settings.json")).unwrap();
    let group_hooks = s["hooks"]["UserPromptSubmit"][0]["hooks"]
        .as_array()
        .expect("group must survive with the user's hook");
    assert_eq!(
        group_hooks.len(),
        1,
        "expected only the user's hook to remain: {group_hooks:?}"
    );
    assert_eq!(group_hooks[0]["command"], "echo hi");
}

// ---------------------------------------------------------------------------
// Test: the rendered skill states mask semantics correctly (allow-list) and
//       documents the arguments the MCP server actually accepts
// ---------------------------------------------------------------------------

#[test]
fn skill_text_is_truthful_about_masks_and_tool_args() {
    let root = temp_dir("skill-truth");
    let home = temp_dir("skill-truth-home");
    let db = root.join("mushroom-memory");
    let opts = all_project_opts(&db);
    install_on_path(&root, &home, &opts).expect("install");

    let skill = read(&root, ".claude/skills/mushroom/SKILL.md");
    let rules = read(&root, ".cursor/rules/mushroom.mdc");
    for (name, text) in [("SKILL.md", &skill), ("mushroom.mdc", &rules)] {
        assert!(
            text.contains("allow-list"),
            "{name}: mask must be described as an allow-list"
        );
        assert!(
            !text.contains("must not see"),
            "{name}: inverted mask text still present"
        );
        assert!(
            !text.contains("keys to exclude"),
            "{name}: inverted mask text still present"
        );
        assert!(
            !text.contains("keys to hide"),
            "{name}: inverted mask text still present"
        );
        assert!(
            text.contains("max_edges"),
            "{name}: create_rule max_edges undocumented"
        );
        assert!(
            text.contains("no auth"),
            "{name}: MCP trust model undocumented"
        );
        assert!(
            text.contains("ingest-git"),
            "{name}: ingest-git bootstrap undocumented"
        );
    }
    assert!(
        skill.contains("`edges`"),
        "SKILL.md: ingest_json edges arg undocumented"
    );
    assert!(
        skill.contains("ambiguous target labels"),
        "SKILL.md: polymorphic FK pattern undocumented"
    );
}

// ---------------------------------------------------------------------------
// Test: uninstall against a settings.json whose UserPromptSubmit holds only a
//       user's own hook has nothing to remove, so it must not rewrite the file
// ---------------------------------------------------------------------------

#[test]
fn uninstall_does_not_reformat_settings_it_removes_nothing_from() {
    let root = temp_dir("no-op-uninstall");
    let home = temp_dir("no-op-uninstall-home");
    let db = root.join("mushroom-memory");
    let opts = claude_project_opts(&db);

    install_on_path(&root, &home, &opts).expect("install");

    // Hand-edit: our entry is gone, the user's own hook remains, and the file
    // is laid out the way a person maintains it (4-space indent, own key order).
    let settings_path = root.join(".claude/settings.json");
    let user_owned = "{\n    \"hooks\": {\n        \"UserPromptSubmit\": [\n            {\n                \"hooks\": [\n                    {\"type\": \"command\", \"command\": \"echo hi\"}\n                ]\n            }\n        ]\n    },\n    \"env\": {\"FOO\": \"bar\"}\n}\n";
    fs::write(&settings_path, user_owned).unwrap();

    run_uninstall(&root, &home, &opts).expect("uninstall");

    assert_eq!(
        read(&root, ".claude/settings.json"),
        user_owned,
        "nothing of ours was in the file, so it must be byte-identical"
    );
}

// ---------------------------------------------------------------------------
// Test: PATH classification — a file named `mushroomdb` on PATH only means
//       "on PATH" when it IS this executable. npm/npx put a Node shim of the
//       same name on PATH; treating it as ours wrote a bare command that only
//       resolves inside the npx shell (v0.5.0 bug).
// ---------------------------------------------------------------------------

/// Build a PATH-style OsString out of directories.
fn path_var(dirs: &[&Path]) -> std::ffi::OsString {
    std::env::join_paths(dirs.iter().map(|d| d.as_os_str())).unwrap()
}

/// Write a file literally named `mushroomdb` into `dir` with the given bytes.
fn named_bin(dir: &Path, bytes: &[u8]) -> PathBuf {
    let p = dir.join("mushroomdb");
    fs::write(&p, bytes).unwrap();
    p
}

#[test]
fn npm_shim_on_path_is_not_our_binary_so_we_copy() {
    let shim_dir = temp_dir("classify-shim");
    let exe_dir = temp_dir("classify-shim-exe");

    // What `npx mushroomdb` prepends to PATH: a Node shim, same name, ours in
    // no other sense.
    named_bin(
        &shim_dir,
        b"#!/usr/bin/env node\nrequire('../lib/cli.js')\n",
    );
    let current_exe = fake_exe(&exe_dir, b"\x7fELF fake native binary\n");

    assert_eq!(
        cli::install::classify_binary_location(
            Some(path_var(&[&shim_dir]).as_os_str()),
            &current_exe
        ),
        BinaryLocation::CopyFrom(current_exe.clone()),
        "a Node shim named mushroomdb must not count as our binary"
    );
}

#[test]
fn symlink_on_path_pointing_at_current_exe_counts_as_on_path() {
    let link_dir = temp_dir("classify-link");
    let exe_dir = temp_dir("classify-link-exe");
    let current_exe = fake_exe(&exe_dir, b"\x7fELF fake native binary\n");

    // cargo install / brew put a symlink to the real binary on PATH.
    #[cfg(unix)]
    std::os::unix::fs::symlink(&current_exe, link_dir.join("mushroomdb")).unwrap();
    #[cfg(windows)]
    fs::copy(&current_exe, link_dir.join("mushroomdb")).unwrap();

    assert_eq!(
        cli::install::classify_binary_location(
            Some(path_var(&[&link_dir]).as_os_str()),
            &current_exe
        ),
        BinaryLocation::OnPath,
        "a symlink to this executable is this executable"
    );
}

#[test]
fn current_exe_itself_on_path_counts_as_on_path() {
    let dir = temp_dir("classify-self");
    let current_exe = named_bin(&dir, b"\x7fELF fake native binary\n");

    assert_eq!(
        cli::install::classify_binary_location(Some(path_var(&[&dir]).as_os_str()), &current_exe),
        BinaryLocation::OnPath
    );
}

#[test]
fn no_path_hit_copies_the_current_exe() {
    let empty_dir = temp_dir("classify-empty");
    let exe_dir = temp_dir("classify-empty-exe");
    let current_exe = fake_exe(&exe_dir, b"\x7fELF fake native binary\n");

    // PATH set but holding no mushroomdb.
    assert_eq!(
        cli::install::classify_binary_location(
            Some(path_var(&[&empty_dir]).as_os_str()),
            &current_exe
        ),
        BinaryLocation::CopyFrom(current_exe.clone())
    );

    // PATH unset entirely.
    assert_eq!(
        cli::install::classify_binary_location(None, &current_exe),
        BinaryLocation::CopyFrom(current_exe)
    );
}

#[test]
fn a_different_native_binary_first_on_path_shadows_us_so_we_copy() {
    let other_dir = temp_dir("classify-other");
    let exe_dir = temp_dir("classify-other-exe");

    // Some other mushroomdb (an older global install) wins PATH resolution.
    named_bin(&other_dir, b"\x7fELF a different build\n");
    let current_exe = fake_exe(&exe_dir, b"\x7fELF fake native binary\n");

    assert_eq!(
        cli::install::classify_binary_location(
            Some(path_var(&[&other_dir]).as_os_str()),
            &current_exe
        ),
        BinaryLocation::CopyFrom(current_exe.clone()),
        "PATH resolves to a binary that is not us, so name the copy explicitly"
    );
}

// ---------------------------------------------------------------------------
// Test: the git post-commit hook block — writing it twice must be a no-op, and
//       removing it must leave a user's own hook lines exactly as they were.
// ---------------------------------------------------------------------------

#[test]
fn git_hook_block_is_idempotent_and_removable_leaving_user_lines() {
    use cli::install::{git_hook_block, merge_git_hook, remove_git_hook};

    let dir = temp_dir("git-hook");
    let db = dir.join("mushroom memory"); // a space, so quoting has to work

    // The block is a marked, self-contained fragment that backgrounds a sync.
    let block = git_hook_block("mushroomdb", &db.to_string_lossy());
    assert!(block.starts_with("# >>> mushroomdb >>>\n"), "{block}");
    assert!(block.ends_with("# <<< mushroomdb <<<\n"), "{block}");
    assert!(block.contains(" sync "), "{block}");
    assert!(
        block.contains(&format!("'{}'", db.display())),
        "the db path must be shell-quoted: {block}"
    );
    assert!(block.contains(">/dev/null 2>&1 &"), "{block}");

    // 1. No hook file yet: one is created, with a shebang, and executable.
    let hook = dir.join("hooks").join("post-commit");
    assert!(merge_git_hook(&hook, "mushroomdb", &db.to_string_lossy()).unwrap());
    let created = fs::read_to_string(&hook).unwrap();
    assert!(created.starts_with("#!/bin/sh\n"), "{created}");
    assert!(created.contains(&block), "{created}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&hook).unwrap().permissions().mode() & 0o777,
            0o755,
            "a git hook has to be executable"
        );
    }

    // 2. Idempotent: a second merge changes nothing at all.
    assert!(
        !merge_git_hook(&hook, "mushroomdb", &db.to_string_lossy()).unwrap(),
        "the block is already there"
    );
    assert_eq!(fs::read_to_string(&hook).unwrap(), created);

    // 3. Removal deletes the file we created, since nothing of the user's is
    //    left in it.
    assert!(remove_git_hook(&hook).unwrap());
    assert!(!hook.exists(), "an empty hook of ours is removed entirely");
    assert!(!remove_git_hook(&hook).unwrap(), "already gone");

    // 4. A hook the user wrote: our block is appended, then removed, and their
    //    file comes back byte-for-byte.
    let user_hook = dir.join("hooks").join("pre-commit");
    let user_text = "#!/usr/bin/env bash\nset -eu\nmake lint\n";
    fs::write(&user_hook, user_text).unwrap();

    assert!(merge_git_hook(&user_hook, "mushroomdb", &db.to_string_lossy()).unwrap());
    let merged = fs::read_to_string(&user_hook).unwrap();
    assert!(merged.starts_with(user_text), "user lines lead: {merged}");
    assert!(merged.contains(&block), "{merged}");

    // Idempotent over a user file too.
    assert!(!merge_git_hook(&user_hook, "mushroomdb", &db.to_string_lossy()).unwrap());
    assert_eq!(fs::read_to_string(&user_hook).unwrap(), merged);

    assert!(remove_git_hook(&user_hook).unwrap());
    assert_eq!(
        fs::read_to_string(&user_hook).unwrap(),
        user_text,
        "the user's hook must survive removal unchanged"
    );
    assert!(
        !remove_git_hook(&user_hook).unwrap(),
        "nothing of ours is left to remove"
    );

    // 5. Changing the database path rewrites the block in place rather than
    //    stacking a second one.
    let other = dir.join("other-memory");
    assert!(merge_git_hook(&user_hook, "mushroomdb", &db.to_string_lossy()).unwrap());
    assert!(merge_git_hook(&user_hook, "mushroomdb", &other.to_string_lossy()).unwrap());
    let rewritten = fs::read_to_string(&user_hook).unwrap();
    assert_eq!(
        rewritten.matches("# >>> mushroomdb >>>").count(),
        1,
        "exactly one block: {rewritten}"
    );
    assert!(
        rewritten.contains(&format!("'{}'", other.display())),
        "{rewritten}"
    );
    assert!(rewritten.starts_with(user_text), "{rewritten}");
}
