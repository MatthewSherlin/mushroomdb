//! Integration tests for `mushroomdb install` / `uninstall`.
//!
//! All tests operate on temp directories — never touching real HOME or CWD.
//! Drive via `run_install_with`/`run_uninstall_with` from the library (never
//! spawn the real binary), so tests run without the binary being on PATH and
//! without ever reaching the network: `Externals` carries the PATH used to
//! resolve external programs, and every test either points it at a directory
//! of stand-ins or leaves it empty.

use cli::install::{
    classify_mcp_command, run_install_with, run_uninstall, run_uninstall_with, Externals,
    InstallOpts, McpCommand, Platform, Scope,
};
use std::fs;
use std::path::{Path, PathBuf};

/// The version an `npx` entry pins. Same crate, so the same constant the
/// installer compiles in.
const VERSION: &str = env!("CARGO_PKG_VERSION");

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

/// External programs are unreachable: no PATH at all. Nothing in these tests
/// may depend on a program that happens to be installed on the machine.
fn no_externals() -> Externals {
    Externals::with_path(None)
}

/// Externals that resolve programs out of `dir` and nowhere else.
fn externals_in(dir: &Path) -> Externals {
    Externals::with_path(Some(dir.as_os_str().to_os_string()))
}

/// Install as if `mushroomdb` resolves on PATH (the bare-name MCP command).
fn install_on_path(root: &Path, home: &Path, opts: &InstallOpts) -> Result<String, cli::CliError> {
    run_install_with(root, home, opts, &McpCommand::OnPath, &no_externals())
}

/// Write a small fake executable to stand in for the running binary.
fn fake_exe(dir: &Path, bytes: &[u8]) -> PathBuf {
    let p = dir.join("fake-mushroomdb");
    fs::write(&p, bytes).unwrap();
    p
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

/// Base options: no platform, no scope, no db, no command, hooks on, pre-warm
/// off. Tests never pre-warm — that would run `npx` against the network.
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

/// Build opts for a --platform claude-code --project install with a fixed db path.
fn claude_project_opts(db: &Path) -> InstallOpts {
    InstallOpts {
        platform: Some(Platform::ClaudeCode),
        scope: Some(Scope::Project),
        db: Some(db.to_path_buf()),
        ..base_opts()
    }
}

/// Build opts for a --platform cursor --project install.
fn cursor_project_opts(db: &Path) -> InstallOpts {
    InstallOpts {
        platform: Some(Platform::Cursor),
        scope: Some(Scope::Project),
        db: Some(db.to_path_buf()),
        ..base_opts()
    }
}

/// Build opts for --platform all --project.
fn all_project_opts(db: &Path) -> InstallOpts {
    InstallOpts {
        platform: Some(Platform::All),
        scope: Some(Scope::Project),
        db: Some(db.to_path_buf()),
        ..base_opts()
    }
}

/// Make `root` look like a git checkout with a hooks directory.
fn git_repo(root: &Path) -> PathBuf {
    let hooks = root.join(".git").join("hooks");
    fs::create_dir_all(&hooks).unwrap();
    hooks
}

// ---------------------------------------------------------------------------
// Test: project scope with the default (npx) command writes the pinned entry,
//       the rendered skill and both hooks
// ---------------------------------------------------------------------------

#[test]
fn project_install_writes_npx_entry_and_hooks() {
    let root = temp_dir("npx-proj");
    let home = temp_dir("npx-proj-home");
    let db = root.join("mushroom-memory");
    let opts = claude_project_opts(&db);

    let out = run_install_with(&root, &home, &opts, &McpCommand::npx(), &no_externals())
        .expect("install failed");

    // The MCP entry runs the published package at this exact version: no PATH
    // lookup, and an upgrade of the assistant host cannot change what it runs.
    let mcp: serde_json::Value = serde_json::from_str(&read(&root, ".mcp.json")).unwrap();
    let server = &mcp["mcpServers"]["mushroomdb"];
    assert_eq!(server["command"], "npx");
    assert_eq!(
        server["args"],
        serde_json::json!([
            "-y",
            format!("mushroomdb@{VERSION}"),
            "mcp",
            db.to_str().unwrap()
        ])
    );

    // The skill is rendered with the same command, pre-quoted so a copy-paste
    // line works under a path with a space in it.
    let skill = read(&root, ".claude/skills/mushroom/SKILL.md");
    assert!(
        skill.contains(&format!(
            "npx -y mushroomdb@{VERSION} ingest-git '{}' .",
            db.display()
        )),
        "skill bootstrap does not use the npx command"
    );
    assert!(!skill.contains("{{BIN}}"), "unsubstituted BIN placeholder");

    // Both hooks, in the same scope, driven by the same command.
    let s: serde_json::Value = serde_json::from_str(&read(&root, ".claude/settings.json")).unwrap();
    let prompt = &s["hooks"]["UserPromptSubmit"][0]["hooks"][0];
    assert_eq!(
        prompt["command"],
        format!("npx -y mushroomdb@{VERSION} recall '{}'", db.display())
    );
    assert_eq!(prompt["timeout"], 5);
    let post = &s["hooks"]["PostToolUse"][0]["hooks"][0];
    assert_eq!(
        post["command"],
        format!("npx -y mushroomdb@{VERSION} touch '{}'", db.display())
    );
    assert_eq!(post["timeout"], 30);
    assert_eq!(post["async"], true);
    assert_eq!(
        s["hooks"]["PostToolUse"][0]["matcher"],
        "Edit|Write|MultiEdit"
    );

    // The summary names the command and closes with the one thing left to do.
    assert!(
        out.contains(&format!("npx -y mushroomdb@{VERSION}")),
        "{out}"
    );
    assert!(
        out.trim_end().ends_with(&format!(
            "next: restart Claude Code in {}, then type /mushroom",
            root.display()
        )),
        "{out}"
    );

    // Nothing written to home.
    assert_absent(&home, ".claude/skills/mushroom/SKILL.md");
    assert_absent(&home, ".claude.json");
}

// ---------------------------------------------------------------------------
// Test: user scope writes only home files, with the user-scope store default
// ---------------------------------------------------------------------------

#[test]
fn user_install_targets_home_files() {
    let root = temp_dir("user-scope");
    let home = temp_dir("user-scope-home");
    let opts = InstallOpts {
        platform: Some(Platform::ClaudeCode),
        scope: Some(Scope::User),
        ..base_opts()
    };

    let out = run_install_with(&root, &home, &opts, &McpCommand::npx(), &no_externals())
        .expect("install failed");

    let db = home.join(".mushroomdb").join("memory");
    let mcp: serde_json::Value = serde_json::from_str(&read(&home, ".claude.json")).unwrap();
    assert_eq!(
        mcp["mcpServers"]["mushroomdb"]["args"],
        serde_json::json!([
            "-y",
            format!("mushroomdb@{VERSION}"),
            "mcp",
            db.to_str().unwrap()
        ])
    );
    assert!(home.join(".claude/skills/mushroom/SKILL.md").exists());
    let s: serde_json::Value = serde_json::from_str(&read(&home, ".claude/settings.json")).unwrap();
    assert_eq!(
        s["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"],
        format!("npx -y mushroomdb@{VERSION} recall '{}'", db.display())
    );
    assert!(out.contains("scope  user"), "{out}");

    // Nothing project-local at all: no config, no ignore line, no git hooks.
    assert_absent(&root, ".mcp.json");
    assert_absent(&root, ".claude/settings.json");
    assert_absent(&root, ".gitignore");
    assert!(home.join(".mushroomdb/install-manifest.json").exists());
}

// ---------------------------------------------------------------------------
// Test: scope auto-detection — a git checkout is a project, anything else is
//       the user
// ---------------------------------------------------------------------------

#[test]
fn auto_scope_is_project_inside_git_repo() {
    let root = temp_dir("auto-project");
    let home = temp_dir("auto-project-home");
    git_repo(&root);
    let opts = InstallOpts {
        platform: Some(Platform::ClaudeCode),
        scope: None,
        ..base_opts()
    };

    let out = install_on_path(&root, &home, &opts).expect("install failed");

    assert!(out.contains("scope  project"), "{out}");
    assert!(
        out.contains("auto"),
        "the chosen scope must say it was inferred: {out}"
    );
    assert!(root.join(".mcp.json").exists());
    assert_absent(&home, ".claude.json");
    // The default store is the project one.
    let mcp: serde_json::Value = serde_json::from_str(&read(&root, ".mcp.json")).unwrap();
    assert_eq!(
        mcp["mcpServers"]["mushroomdb"]["args"][1],
        root.join("mushroom-memory").to_str().unwrap()
    );
}

#[test]
fn auto_scope_is_user_outside() {
    let root = temp_dir("auto-user");
    let home = temp_dir("auto-user-home");
    let opts = InstallOpts {
        platform: Some(Platform::ClaudeCode),
        scope: None,
        ..base_opts()
    };

    let out = install_on_path(&root, &home, &opts).expect("install failed");

    assert!(out.contains("scope  user"), "{out}");
    assert!(home.join(".claude.json").exists());
    assert_absent(&root, ".mcp.json");
}

// ---------------------------------------------------------------------------
// Test: --command names the binary verbatim in JSON and shell-quoted wherever
//       a shell will see it
// ---------------------------------------------------------------------------

#[test]
fn explicit_command_is_used_verbatim_and_shell_quoted() {
    let root = temp_dir("explicit-cmd");
    let home = temp_dir("explicit-cmd-home");
    let db = root.join("mushroom-memory");
    // A path with a space in it: JSON carries it as one argv element, a shell
    // needs it quoted, and the two must not be confused.
    let bin = root.join("my tools").join("mushroomdb");
    let opts = InstallOpts {
        command: Some(bin.clone()),
        ..claude_project_opts(&db)
    };

    run_install_with(
        &root,
        &home,
        &opts,
        &McpCommand::Explicit(bin.clone()),
        &no_externals(),
    )
    .expect("install failed");

    // JSON: verbatim, no quoting — argv is not a shell.
    let mcp: serde_json::Value = serde_json::from_str(&read(&root, ".mcp.json")).unwrap();
    assert_eq!(
        mcp["mcpServers"]["mushroomdb"]["command"],
        bin.to_str().unwrap()
    );
    assert_eq!(
        mcp["mcpServers"]["mushroomdb"]["args"],
        serde_json::json!(["mcp", db.to_str().unwrap()])
    );

    // Hook: quoted, because Claude Code runs it through a shell.
    let s: serde_json::Value = serde_json::from_str(&read(&root, ".claude/settings.json")).unwrap();
    assert_eq!(
        s["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"],
        format!("'{}' recall '{}'", bin.display(), db.display())
    );

    // Skill: the same quoted form, so its copy-paste lines survive the space.
    let skill = read(&root, ".claude/skills/mushroom/SKILL.md");
    assert!(
        skill.contains(&format!(
            "'{}' ingest-git '{}' .",
            bin.display(),
            db.display()
        )),
        "skill bootstrap is not shell-safe"
    );
}

// ---------------------------------------------------------------------------
// Test: the bare name is written only when PATH really resolves to this
//       executable
// ---------------------------------------------------------------------------

#[test]
fn on_path_real_binary_keeps_bare_name() {
    let root = temp_dir("onpath-bin");
    let home = temp_dir("onpath-bin-home");
    let db = root.join("mushroom-memory");
    let opts = all_project_opts(&db);

    install_on_path(&root, &home, &opts).expect("install");

    let mcp: serde_json::Value = serde_json::from_str(&read(&root, ".mcp.json")).unwrap();
    assert_eq!(mcp["mcpServers"]["mushroomdb"]["command"], "mushroomdb");
    assert_eq!(
        mcp["mcpServers"]["mushroomdb"]["args"],
        serde_json::json!(["mcp", db.to_str().unwrap()])
    );

    // A bare name needs no quoting anywhere.
    let skill = read(&root, ".claude/skills/mushroom/SKILL.md");
    assert!(skill.contains(&format!("mushroomdb ingest-git '{}' .", db.display())));
    assert!(!skill.contains("{{BIN}}"));
    let rules = read(&root, ".cursor/rules/mushroom.mdc");
    assert!(rules.contains(&format!("mushroomdb ingest-git '{}' .", db.display())));
    assert!(!rules.contains("{{BIN}}"));
    let s: serde_json::Value = serde_json::from_str(&read(&root, ".claude/settings.json")).unwrap();
    assert_eq!(
        s["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"],
        format!("mushroomdb recall '{}'", db.display())
    );

    // 0.5.x copied the binary into HOME. Nothing does that any more.
    assert_absent(&home, ".mushroomdb/bin/mushroomdb");
}

// ---------------------------------------------------------------------------
// Test: the store directory is ignored by git exactly once, and the line goes
//       away with the install
// ---------------------------------------------------------------------------

#[test]
fn gitignore_line_added_once_and_removed_on_uninstall() {
    let root = temp_dir("gitignore");
    let home = temp_dir("gitignore-home");
    let db = root.join("mushroom-memory");
    let opts = claude_project_opts(&db);

    // A .gitignore the user already maintains.
    fs::write(root.join(".gitignore"), "target/\n").unwrap();

    install_on_path(&root, &home, &opts).expect("install");
    assert_eq!(read(&root, ".gitignore"), "target/\nmushroom-memory/\n");

    // Idempotent: no second line.
    install_on_path(&root, &home, &opts).expect("second install");
    assert_eq!(read(&root, ".gitignore"), "target/\nmushroom-memory/\n");

    // The manifest records the exact line, so uninstall removes that and only
    // that.
    let m: serde_json::Value = serde_json::from_str(&read(
        &root,
        ".claude/skills/mushroom/.install-manifest.json",
    ))
    .unwrap();
    assert_eq!(m["gitignore"][0]["line"], "mushroom-memory/");

    run_uninstall(&root, &home, &opts).expect("uninstall");
    assert_eq!(read(&root, ".gitignore"), "target/\n");
}

#[test]
fn a_gitignore_we_created_is_removed_again() {
    let root = temp_dir("gitignore-new");
    let home = temp_dir("gitignore-new-home");
    let db = root.join("mushroom-memory");
    let opts = claude_project_opts(&db);

    // No .gitignore at all before this.
    install_on_path(&root, &home, &opts).expect("install");
    assert_eq!(read(&root, ".gitignore"), "mushroom-memory/\n");

    run_uninstall(&root, &home, &opts).expect("uninstall");
    assert_absent(&root, ".gitignore");
}

#[test]
fn uninstall_keeps_a_gitignore_the_user_extended() {
    let root = temp_dir("gitignore-extended");
    let home = temp_dir("gitignore-extended-home");
    let db = root.join("mushroom-memory");
    let opts = claude_project_opts(&db);

    // The file exists only because install created it...
    install_on_path(&root, &home, &opts).expect("install");
    // ...but by now it is the repository's, and holds a line of the user's.
    fs::write(root.join(".gitignore"), "mushroom-memory/\nnode_modules/\n").unwrap();

    run_uninstall(&root, &home, &opts).expect("uninstall");

    assert_eq!(
        read(&root, ".gitignore"),
        "node_modules/\n",
        "only our line may go; the file is the user's now"
    );
}

#[test]
fn gitignore_untouched_when_the_store_is_outside_the_project() {
    let root = temp_dir("gitignore-outside");
    let home = temp_dir("gitignore-outside-home");
    let db = temp_dir("gitignore-outside-db");
    let opts = claude_project_opts(&db);

    install_on_path(&root, &home, &opts).expect("install");

    // The repository has no business ignoring a path it does not contain.
    assert_absent(&root, ".gitignore");
}

// ---------------------------------------------------------------------------
// Test: git hooks — three of them, marked, opt-out, and removed on uninstall
// ---------------------------------------------------------------------------

#[test]
fn git_hooks_installed_and_removed() {
    let root = temp_dir("git-hooks");
    let home = temp_dir("git-hooks-home");
    let db = root.join("mushroom-memory");
    let hooks = git_repo(&root);
    let opts = claude_project_opts(&db);

    // A pre-existing post-commit hook of the user's must survive both ways.
    let user_text = "#!/bin/sh\nmake lint\n";
    fs::write(hooks.join("post-commit"), user_text).unwrap();

    install_on_path(&root, &home, &opts).expect("install");

    for name in ["post-commit", "post-checkout", "post-merge"] {
        let text = fs::read_to_string(hooks.join(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(text.contains("# >>> mushroomdb >>>"), "{name}: {text}");
        assert!(
            text.contains(&format!("mushroomdb sync '{}'", db.display())),
            "{name}: {text}"
        );
    }
    assert!(fs::read_to_string(hooks.join("post-commit"))
        .unwrap()
        .starts_with(user_text));

    // The manifest lists them so uninstall knows which files to open.
    let m: serde_json::Value = serde_json::from_str(&read(
        &root,
        ".claude/skills/mushroom/.install-manifest.json",
    ))
    .unwrap();
    assert_eq!(m["git_hooks"].as_array().unwrap().len(), 3, "{m}");

    run_uninstall(&root, &home, &opts).expect("uninstall");
    assert_eq!(
        fs::read_to_string(hooks.join("post-commit")).unwrap(),
        user_text
    );
    assert!(
        !hooks.join("post-checkout").exists(),
        "ours was the whole file"
    );
    assert!(!hooks.join("post-merge").exists());
}

#[test]
fn no_git_hooks_flag_writes_none() {
    let root = temp_dir("no-git-hooks");
    let home = temp_dir("no-git-hooks-home");
    let db = root.join("mushroom-memory");
    let hooks = git_repo(&root);
    let opts = InstallOpts {
        git_hooks: false,
        ..claude_project_opts(&db)
    };

    install_on_path(&root, &home, &opts).expect("install");

    for name in ["post-commit", "post-checkout", "post-merge"] {
        assert!(!hooks.join(name).exists(), "{name} was written anyway");
    }
}

// ---------------------------------------------------------------------------
// Test: an install in the other scope is reported, not edited
// ---------------------------------------------------------------------------

#[test]
fn scope_conflict_warns_and_leaves_other_file() {
    let root = temp_dir("scope-conflict");
    let home = temp_dir("scope-conflict-home");
    let db = root.join("mushroom-memory");
    let opts = claude_project_opts(&db);

    // A user-scope server from an earlier install.
    let user_cfg = serde_json::json!({
        "mcpServers": {
            "mushroomdb": { "command": "mushroomdb", "args": ["mcp", "/elsewhere/memory"] }
        }
    });
    let user_cfg = serde_json::to_string_pretty(&user_cfg).unwrap();
    fs::write(home.join(".claude.json"), &user_cfg).unwrap();

    let out = install_on_path(&root, &home, &opts).expect("install must not fail on this");

    assert!(
        out.contains("a user-scope mushroomdb server also exists"),
        "{out}"
    );
    assert!(out.contains("mushroomdb uninstall --user"), "{out}");
    assert_eq!(
        read(&home, ".claude.json"),
        user_cfg,
        "the other scope's file must not be touched at all"
    );
}

#[test]
fn user_install_warns_about_a_project_scope_server() {
    let root = temp_dir("scope-conflict-rev");
    let home = temp_dir("scope-conflict-rev-home");
    let project_cfg = serde_json::to_string_pretty(&serde_json::json!({
        "mcpServers": { "mushroomdb": { "command": "mushroomdb", "args": ["mcp", "/x"] } }
    }))
    .unwrap();
    fs::write(root.join(".mcp.json"), &project_cfg).unwrap();

    let opts = InstallOpts {
        platform: Some(Platform::ClaudeCode),
        scope: Some(Scope::User),
        ..base_opts()
    };
    let out = install_on_path(&root, &home, &opts).expect("install");

    assert!(
        out.contains("a project-scope mushroomdb server also exists"),
        "{out}"
    );
    assert!(out.contains("mushroomdb uninstall --project"), "{out}");
    assert_eq!(read(&root, ".mcp.json"), project_cfg);
}

// ---------------------------------------------------------------------------
// Test: pre-warm is best-effort — a failure is reported, never fatal
// ---------------------------------------------------------------------------

#[test]
#[cfg(unix)]
fn prewarm_failure_is_a_warning() {
    let root = temp_dir("prewarm-fail");
    let home = temp_dir("prewarm-fail-home");
    let bin_dir = temp_dir("prewarm-fail-bin");
    let db = root.join("mushroom-memory");
    let log = bin_dir.join("argv.txt");

    // An `npx` that records what it was asked and then fails.
    fake_program(
        &bin_dir,
        "npx",
        &format!("printf '%s\\n' \"$@\" > '{}'\nexit 7\n", log.display()),
    );

    let opts = InstallOpts {
        prewarm: true,
        ..claude_project_opts(&db)
    };
    let out = run_install_with(
        &root,
        &home,
        &opts,
        &McpCommand::npx(),
        &externals_in(&bin_dir),
    )
    .expect("a failed pre-warm must not fail the install");

    assert!(out.contains("warning"), "{out}");
    assert!(out.contains("pre-warm"), "{out}");
    // It really did try the pinned package.
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        format!("-y\nmushroomdb@{VERSION}\n--version\n")
    );
    // And the install itself completed.
    assert!(root.join(".mcp.json").exists());
}

#[test]
#[cfg(unix)]
fn prewarm_is_skipped_with_no_prewarm() {
    let root = temp_dir("prewarm-off");
    let home = temp_dir("prewarm-off-home");
    let bin_dir = temp_dir("prewarm-off-bin");
    let db = root.join("mushroom-memory");
    let log = bin_dir.join("argv.txt");

    // An `npx` that is right there on the resolution path and must not run.
    fake_program(
        &bin_dir,
        "npx",
        &format!("printf '%s\\n' \"$@\" > '{}'\n", log.display()),
    );

    let opts = InstallOpts {
        prewarm: false,
        ..claude_project_opts(&db)
    };
    let out = run_install_with(
        &root,
        &home,
        &opts,
        &McpCommand::npx(),
        &externals_in(&bin_dir),
    )
    .expect("install");

    assert!(!log.exists(), "--no-prewarm still spawned npx");
    assert!(!out.contains("warning"), "{out}");
    assert!(root.join(".mcp.json").exists());
}

#[test]
#[cfg(unix)]
fn prewarm_success_is_silent() {
    let root = temp_dir("prewarm-ok");
    let home = temp_dir("prewarm-ok-home");
    let bin_dir = temp_dir("prewarm-ok-bin");
    let db = root.join("mushroom-memory");
    fake_program(&bin_dir, "npx", "exit 0\n");

    let opts = InstallOpts {
        prewarm: true,
        ..claude_project_opts(&db)
    };
    let out = run_install_with(
        &root,
        &home,
        &opts,
        &McpCommand::npx(),
        &externals_in(&bin_dir),
    )
    .expect("install");

    assert!(!out.contains("warning"), "{out}");
}

#[test]
fn prewarm_is_skipped_for_a_command_that_is_not_npx() {
    let root = temp_dir("prewarm-skip");
    let home = temp_dir("prewarm-skip-home");
    let db = root.join("mushroom-memory");
    let opts = InstallOpts {
        prewarm: true,
        ..claude_project_opts(&db)
    };

    // No PATH at all: if a bare-name install tried to pre-warm, it could only
    // fail to find `npx` and would say so.
    let out = install_on_path(&root, &home, &opts).expect("install");
    assert!(!out.contains("warning"), "{out}");
}

// ---------------------------------------------------------------------------
// Test: Codex is wired through its own CLI
// ---------------------------------------------------------------------------

#[test]
#[cfg(unix)]
fn codex_platform_calls_codex_mcp_add() {
    let root = temp_dir("codex");
    let home = temp_dir("codex-home");
    let bin_dir = temp_dir("codex-bin");
    let db = root.join("mushroom-memory");
    let hooks = git_repo(&root);
    let log = bin_dir.join("argv.txt");
    fake_program(
        &bin_dir,
        "codex",
        &format!("printf '%s\\n' \"$@\" >> '{}'\n", log.display()),
    );

    let opts = InstallOpts {
        platform: Some(Platform::Codex),
        scope: Some(Scope::Project),
        db: Some(db.clone()),
        ..base_opts()
    };
    let out = run_install_with(
        &root,
        &home,
        &opts,
        &McpCommand::npx(),
        &externals_in(&bin_dir),
    )
    .expect("codex install");

    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        format!(
            "mcp\nadd\nmushroomdb\n--\nnpx\n-y\nmushroomdb@{VERSION}\nmcp\n{}\n",
            db.display()
        )
    );
    assert!(out.contains("codex"), "{out}");
    // 0.6.0 ships no Codex skill, and Codex config is the CLI's own business.
    assert_absent(&root, ".mcp.json");
    assert_absent(&root, ".claude/skills/mushroom/SKILL.md");
    // Nothing project-local at all. The ignore line and the git hooks are
    // shared with whatever else is installed in the repository, and this
    // install's manifest lives under HOME, so recording them here would let a
    // Codex uninstall strip them out from under a Claude Code install.
    assert_absent(&root, ".gitignore");
    for name in ["post-commit", "post-checkout", "post-merge"] {
        assert!(!hooks.join(name).exists(), "{name} was written anyway");
    }

    // The manifest remembers it, so uninstall hands the removal back to Codex.
    fs::remove_file(&log).unwrap();
    run_uninstall_with(&root, &home, &opts, &externals_in(&bin_dir)).expect("uninstall");
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        "mcp\nremove\nmushroomdb\n"
    );
}

#[test]
fn codex_platform_without_the_codex_cli_is_a_clear_error() {
    let root = temp_dir("codex-missing");
    let home = temp_dir("codex-missing-home");
    let db = root.join("mushroom-memory");
    let opts = InstallOpts {
        platform: Some(Platform::Codex),
        scope: Some(Scope::Project),
        db: Some(db),
        ..base_opts()
    };

    let err = run_install_with(&root, &home, &opts, &McpCommand::npx(), &no_externals())
        .expect_err("no codex on PATH");
    assert!(err.0.contains("codex"), "{}", err.0);
    assert!(
        err.0.contains("PATH") || err.0.contains("not found"),
        "{}",
        err.0
    );
}

// ---------------------------------------------------------------------------
// Test: an entry from an older version is rewritten, and the rewrite is said
//       out loud
// ---------------------------------------------------------------------------

#[test]
fn upgrade_rewrites_entry_with_new_version() {
    let root = temp_dir("upgrade");
    let home = temp_dir("upgrade-home");
    let db = root.join("mushroom-memory");
    let opts = claude_project_opts(&db);

    // What 0.5.x wrote: the absolute path of a copied binary.
    let old = serde_json::json!({
        "mcpServers": {
            "mushroomdb": {
                "command": home.join(".mushroomdb/bin/mushroomdb").to_str().unwrap(),
                "args": ["mcp", db.to_str().unwrap()]
            },
            "other": { "command": "other-tool", "args": [] }
        }
    });
    fs::write(
        root.join(".mcp.json"),
        serde_json::to_string_pretty(&old).unwrap(),
    )
    .unwrap();

    let out = run_install_with(&root, &home, &opts, &McpCommand::npx(), &no_externals())
        .expect("an entry for the same db is ours to repair");

    assert!(out.contains("updated mcp command"), "{out}");
    let mcp: serde_json::Value = serde_json::from_str(&read(&root, ".mcp.json")).unwrap();
    assert_eq!(mcp["mcpServers"]["mushroomdb"]["command"], "npx");
    assert_eq!(
        mcp["mcpServers"]["mushroomdb"]["args"],
        serde_json::json!([
            "-y",
            format!("mushroomdb@{VERSION}"),
            "mcp",
            db.to_str().unwrap()
        ])
    );
    // Unrelated servers untouched.
    assert_eq!(mcp["mcpServers"]["other"]["command"], "other-tool");

    // And an older pin of our own is rewritten just the same.
    let older = serde_json::json!({
        "mcpServers": {
            "mushroomdb": {
                "command": "npx",
                "args": ["-y", "mushroomdb@0.5.0", "mcp", db.to_str().unwrap()]
            }
        }
    });
    fs::write(
        root.join(".mcp.json"),
        serde_json::to_string_pretty(&older).unwrap(),
    )
    .unwrap();
    let out =
        run_install_with(&root, &home, &opts, &McpCommand::npx(), &no_externals()).expect("re-pin");
    assert!(out.contains("updated mcp command"), "{out}");
    let mcp: serde_json::Value = serde_json::from_str(&read(&root, ".mcp.json")).unwrap();
    assert_eq!(
        mcp["mcpServers"]["mushroomdb"]["args"][1],
        format!("mushroomdb@{VERSION}")
    );
}

// ---------------------------------------------------------------------------
// Test: an upgrade replaces the settings hooks instead of stacking a second
//       pair beside them. The 0.5.x pair names a binary that is still on disk,
//       so leaving them means two recall digests on every prompt.
// ---------------------------------------------------------------------------

#[test]
fn upgrade_replaces_stale_hooks_from_a_0_5_install() {
    let root = temp_dir("upgrade-hooks");
    let home = temp_dir("upgrade-hooks-home");
    let db = root.join("mushroom-memory");
    let opts = claude_project_opts(&db);

    // What 0.5.x left behind: hooks naming the copied binary, and a hook of
    // the user's own under another event.
    let old_bin = home.join(".mushroomdb/bin/mushroomdb");
    let old_recall = format!("'{}' recall '{}'", old_bin.display(), db.display());
    let old_touch = format!("'{}' touch '{}'", old_bin.display(), db.display());
    fs::create_dir_all(root.join(".claude")).unwrap();
    fs::write(
        root.join(".claude/settings.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "hooks": {
                "UserPromptSubmit": [
                    {"hooks": [{"type": "command", "command": old_recall, "timeout": 5}]}
                ],
                "PostToolUse": [
                    {"matcher": "Edit|Write|MultiEdit",
                     "hooks": [{"type": "command", "command": old_touch, "timeout": 30, "async": true}]}
                ],
                "SessionStart": [
                    {"hooks": [{"type": "command", "command": "echo hi"}]}
                ]
            }
        }))
        .unwrap(),
    )
    .unwrap();
    // And the manifest that recorded them.
    let manifest_dir = root.join(".claude/skills/mushroom");
    fs::create_dir_all(&manifest_dir).unwrap();
    let settings = root.join(".claude/settings.json");
    fs::write(
        manifest_dir.join(".install-manifest.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "files": [manifest_dir.join("SKILL.md").to_str().unwrap()],
            "mcp_keys": [{"file": root.join(".mcp.json").to_str().unwrap(), "server": "mushroomdb"}],
            "hooks": [
                {"file": settings.to_str().unwrap(), "event": "UserPromptSubmit", "command": old_recall},
                {"file": settings.to_str().unwrap(), "event": "PostToolUse", "command": old_touch}
            ]
        }))
        .unwrap(),
    )
    .unwrap();

    let out = run_install_with(&root, &home, &opts, &McpCommand::npx(), &no_externals())
        .expect("upgrade install");

    let s: serde_json::Value = serde_json::from_str(&read(&root, ".claude/settings.json")).unwrap();
    let ups = s["hooks"]["UserPromptSubmit"].as_array().unwrap();
    assert_eq!(ups.len(), 1, "exactly one recall hook must remain: {s}");
    assert_eq!(
        ups[0]["hooks"][0]["command"],
        format!("npx -y mushroomdb@{VERSION} recall '{}'", db.display())
    );
    let ptu = s["hooks"]["PostToolUse"].as_array().unwrap();
    assert_eq!(ptu.len(), 1, "exactly one touch hook must remain: {s}");
    assert_eq!(
        ptu[0]["hooks"][0]["command"],
        format!("npx -y mushroomdb@{VERSION} touch '{}'", db.display())
    );
    // The user's own hook is not ours to replace.
    assert_eq!(
        s["hooks"]["SessionStart"][0]["hooks"][0]["command"],
        "echo hi"
    );
    assert!(
        out.contains("replaced stale UserPromptSubmit hook"),
        "{out}"
    );

    // And the manifest now points at the new commands, so uninstall finds them.
    run_uninstall(&root, &home, &opts).expect("uninstall");
    let s: serde_json::Value = serde_json::from_str(&read(&root, ".claude/settings.json")).unwrap();
    assert!(
        s["hooks"]["UserPromptSubmit"].is_null(),
        "the new hooks must be removable: {s}"
    );
    assert_eq!(
        s["hooks"]["SessionStart"][0]["hooks"][0]["command"],
        "echo hi"
    );
}

// ---------------------------------------------------------------------------
// Test: a relative --command / --db is anchored before anything is written.
//       The assistant spawns the server from a directory of its own choosing.
// ---------------------------------------------------------------------------

#[test]
fn relative_command_and_db_are_made_absolute() {
    let root = temp_dir("relative-paths");
    let home = temp_dir("relative-paths-home");
    let opts = InstallOpts {
        platform: Some(Platform::ClaudeCode),
        scope: Some(Scope::Project),
        db: Some(PathBuf::from("./mushroom-memory")),
        command: Some(PathBuf::from("./target/debug/mushroomdb")),
        ..base_opts()
    };

    run_install_with(
        &root,
        &home,
        &opts,
        &McpCommand::Explicit(PathBuf::from("./target/debug/mushroomdb")),
        &no_externals(),
    )
    .expect("install");

    let bin = root.join("target/debug/mushroomdb");
    let db = root.join("mushroom-memory");
    let mcp: serde_json::Value = serde_json::from_str(&read(&root, ".mcp.json")).unwrap();
    assert_eq!(
        mcp["mcpServers"]["mushroomdb"]["command"],
        bin.to_str().unwrap()
    );
    assert_eq!(
        mcp["mcpServers"]["mushroomdb"]["args"],
        serde_json::json!(["mcp", db.to_str().unwrap()])
    );
    let s: serde_json::Value = serde_json::from_str(&read(&root, ".claude/settings.json")).unwrap();
    assert_eq!(
        s["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"],
        format!("'{}' recall '{}'", bin.display(), db.display())
    );
    // And the ignore line is still the repository-relative form.
    assert_eq!(read(&root, ".gitignore"), "mushroom-memory/\n");
}

#[test]
fn bare_name_command_is_kept_verbatim() {
    let root = temp_dir("bare-command");
    let home = temp_dir("bare-command-home");
    let db = root.join("mushroom-memory");
    // `--command mushroomdb` names whatever PATH resolves, not a file here.
    let bin = PathBuf::from("mushroomdb");
    let opts = InstallOpts {
        command: Some(bin.clone()),
        ..claude_project_opts(&db)
    };

    run_install_with(
        &root,
        &home,
        &opts,
        &McpCommand::Explicit(bin),
        &no_externals(),
    )
    .expect("install");

    // Anchoring it would invent <root>/mushroomdb, which does not exist.
    let mcp: serde_json::Value = serde_json::from_str(&read(&root, ".mcp.json")).unwrap();
    assert_eq!(
        mcp["mcpServers"]["mushroomdb"]["command"], "mushroomdb",
        "a bare program name must survive as a PATH lookup: {mcp}"
    );
    let s: serde_json::Value = serde_json::from_str(&read(&root, ".claude/settings.json")).unwrap();
    assert_eq!(
        s["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"],
        format!("'mushroomdb' recall '{}'", db.display()),
        "quoting a bare name does not defeat the PATH lookup, but inventing a path does"
    );
}

// ---------------------------------------------------------------------------
// Test: a `.gitignore` listed in an older manifest's `files` is still only
//       ever touched through the managed-line path. The first 0.6.0 build on
//       this branch recorded it there, and that loop deletes outright.
// ---------------------------------------------------------------------------

#[test]
fn a_gitignore_in_an_older_manifests_files_is_not_deleted() {
    let root = temp_dir("gitignore-legacy-manifest");
    let home = temp_dir("gitignore-legacy-manifest-home");
    let db = root.join("mushroom-memory");
    let opts = claude_project_opts(&db);

    install_on_path(&root, &home, &opts).expect("install");

    // Rewrite the manifest the way the earlier build wrote it: the created
    // `.gitignore` in `files`, and a `gitignore` entry with no `created` key.
    let manifest_file = root.join(".claude/skills/mushroom/.install-manifest.json");
    let gitignore = root.join(".gitignore");
    let mut m: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&manifest_file).unwrap()).unwrap();
    m["files"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!(gitignore.to_str().unwrap()));
    m["gitignore"] = serde_json::json!([{
        "file": gitignore.to_str().unwrap(),
        "line": "mushroom-memory/"
    }]);
    fs::write(&manifest_file, serde_json::to_string_pretty(&m).unwrap()).unwrap();

    // The user has since made the file theirs.
    fs::write(&gitignore, "mushroom-memory/\nnode_modules/\n").unwrap();

    run_uninstall(&root, &home, &opts).expect("uninstall");

    assert_eq!(
        read(&root, ".gitignore"),
        "node_modules/\n",
        "the file must survive with the user's line"
    );
}

// ---------------------------------------------------------------------------
// Test: an inferred scope that finds no manifest looks at the other one before
//       telling the user there is nothing to uninstall (0.5.x installed
//       user-scope inside checkouts, having no scope detection at all)
// ---------------------------------------------------------------------------

#[test]
fn uninstall_auto_scope_falls_back_to_the_other_scope() {
    let root = temp_dir("uninstall-fallback");
    let home = temp_dir("uninstall-fallback-home");
    git_repo(&root); // so auto-detection says "project"

    let installed = InstallOpts {
        platform: Some(Platform::ClaudeCode),
        scope: Some(Scope::User),
        ..base_opts()
    };
    install_on_path(&root, &home, &installed).expect("user-scope install");
    assert!(home.join(".claude.json").exists());

    // No --user this time: the checkout says project, and there is nothing there.
    let bare = InstallOpts {
        platform: Some(Platform::ClaudeCode),
        scope: None,
        ..base_opts()
    };
    let out = run_uninstall(&root, &home, &bare).expect("must find the user-scope install");

    assert!(out.contains("scope  user"), "{out}");
    let mcp: serde_json::Value = serde_json::from_str(&read(&home, ".claude.json")).unwrap();
    assert!(mcp["mcpServers"]["mushroomdb"].is_null(), "{mcp}");
    assert_absent(&home, ".claude/skills/mushroom/SKILL.md");

    // With nothing installed in either scope it still errors.
    let err = run_uninstall(&root, &home, &bare).expect_err("nothing left");
    assert!(err.0.contains("no install manifest"), "{}", err.0);
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

    let out = install_on_path(&root, &home, &opts).expect("second install should be a no-op");
    let mcp_after_second = read(&root, ".mcp.json");

    // Content must be identical — no duplication.
    assert_eq!(mcp_after_first, mcp_after_second);
    assert!(out.contains("already installed"), "{out}");

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
// Test: conflicting existing .mcp.json entry → non-zero error, no changes.
//       The comparison is the argument after `mcp`, whatever leads it.
// ---------------------------------------------------------------------------

#[test]
fn install_refuses_conflicting_mcp_entry() {
    let root = temp_dir("conflict");
    let home = temp_dir("conflict-home");
    let db = root.join("mushroom-memory");
    let opts = claude_project_opts(&db);

    // Pre-create a .mcp.json with a different mushroomdb entry — in the npx
    // shape, where the db is not args[1].
    let existing_mcp = serde_json::json!({
        "mcpServers": {
            "mushroomdb": {
                "command": "npx",
                "args": ["-y", "mushroomdb@0.5.1", "mcp", "/some/other/db"]
            }
        }
    });
    let before = serde_json::to_string_pretty(&existing_mcp).unwrap();
    fs::write(root.join(".mcp.json"), &before).unwrap();

    let result = install_on_path(&root, &home, &opts);
    assert!(result.is_err(), "expected error on conflicting mcp entry");

    // The .mcp.json must be unchanged (original content preserved).
    assert_eq!(read(&root, ".mcp.json"), before);

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
        scope: Some(Scope::Project),
        db: Some(db.clone()),
        ..base_opts()
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
        scope: Some(Scope::Project),
        db: Some(db.clone()),
        ..base_opts()
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
        scope: Some(Scope::Project),
        db: Some(db),
        ..base_opts()
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
// Test: the flags parse into the options the installer acts on
// ---------------------------------------------------------------------------

#[test]
fn parse_install_platform_flag() {
    use cli::parse_args;
    use cli::Command;

    let cmd = parse_args(&["install", "--platform", "cursor", "--project"]).unwrap();
    match cmd {
        Command::Install(opts) => {
            assert_eq!(opts.platform, Some(Platform::Cursor));
            assert_eq!(opts.scope, Some(Scope::Project));
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
            assert_eq!(opts.scope, None, "scope is auto unless asked for");
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

#[test]
fn parse_install_new_flags() {
    use cli::parse_args;
    use cli::Command;

    let cmd = parse_args(&[
        "install",
        "--user",
        "--platform=codex",
        "--command",
        "/opt/mushroomdb",
        "--no-git-hooks",
        "--no-prewarm",
    ])
    .unwrap();
    match cmd {
        Command::Install(opts) => {
            assert_eq!(opts.scope, Some(Scope::User));
            assert_eq!(opts.platform, Some(Platform::Codex));
            assert_eq!(opts.command, Some(PathBuf::from("/opt/mushroomdb")));
            assert!(!opts.git_hooks);
            assert!(!opts.prewarm);
        }
        other => panic!("expected Install, got {other:?}"),
    }

    // Defaults: git hooks on, pre-warm on, scope and platform auto.
    let cmd = parse_args(&["install"]).unwrap();
    match cmd {
        Command::Install(opts) => {
            assert!(opts.git_hooks);
            assert!(opts.prewarm);
            assert_eq!(opts.scope, None);
            assert_eq!(opts.command, None);
        }
        other => panic!("expected Install, got {other:?}"),
    }

    // The two scopes are mutually exclusive.
    assert!(parse_args(&["install", "--project", "--user"]).is_err());
}

// ---------------------------------------------------------------------------
// Test: if a later step fails, the manifest still records what was written
//       (no orphaned entry that uninstall would miss)
// ---------------------------------------------------------------------------

#[test]
#[cfg(unix)]
fn partial_failure_still_tracks_what_was_written() {
    use std::os::unix::fs::PermissionsExt;
    let root = temp_dir("partial-fail");
    let home = temp_dir("partial-fail-home");
    let db = root.join("mushroom-memory");
    let opts = all_project_opts(&db);

    // Claude Code step succeeds; make the Cursor rules dir unwritable so the
    // second platform fails after the first one wrote its files.
    let rules_dir = root.join(".cursor/rules");
    fs::create_dir_all(&rules_dir).unwrap();
    fs::set_permissions(&rules_dir, fs::Permissions::from_mode(0o500)).unwrap();

    let result = install_on_path(&root, &home, &opts);
    // Restore perms so temp cleanup works.
    fs::set_permissions(&rules_dir, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(result.is_err(), "expected cursor rules write to fail");

    let skill = root.join(".claude/skills/mushroom/SKILL.md");
    assert!(skill.exists(), "the first platform did write");
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
    assert!(files.contains(&skill.to_str().unwrap()), "{files:?}");

    run_uninstall(&root, &home, &opts).expect("uninstall after partial failure");
    assert!(!skill.exists(), "orphaned skill after uninstall");
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
    assert_eq!(cmd, format!("mushroomdb recall '{}'", db.display()));
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
fn user_scope_hook_goes_to_home_settings_and_quotes_an_explicit_command() {
    let root = temp_dir("hook-user");
    let home = temp_dir("hook-user-home");
    fs::create_dir_all(home.join(".claude")).unwrap();
    let bin = root.join("bin").join("mushroomdb");
    let opts = InstallOpts {
        platform: Some(Platform::ClaudeCode),
        scope: Some(Scope::User),
        command: Some(bin.clone()),
        ..base_opts()
    };
    run_install_with(
        &root,
        &home,
        &opts,
        &McpCommand::Explicit(bin.clone()),
        &no_externals(),
    )
    .expect("install");

    let s: serde_json::Value = serde_json::from_str(&read(&home, ".claude/settings.json")).unwrap();
    let cmd = s["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert_eq!(
        cmd,
        format!(
            "'{}' recall '{}'",
            bin.display(),
            home.join(".mushroomdb/memory").display()
        )
    );
    assert_absent(&root, ".claude/settings.json");
}

#[test]
fn old_manifest_without_new_fields_still_uninstalls() {
    let root = temp_dir("old-manifest");
    let home = temp_dir("old-manifest-home");
    let db = root.join("mushroom-memory");
    let opts = claude_project_opts(&db);
    install_on_path(&root, &home, &opts).expect("install");
    // Strip the fields a 0.4.x/0.5.x manifest never had.
    let p = root.join(".claude/skills/mushroom/.install-manifest.json");
    let mut m: serde_json::Value = serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
    let obj = m.as_object_mut().unwrap();
    for k in ["hooks", "git_hooks", "gitignore", "codex"] {
        obj.remove(k);
    }
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
    assert_eq!(cmd, format!("mushroomdb recall '{}'", db.display()));
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
// Test: Claude Code install also wires the PostToolUse refresh hook — matched
//       to the file-editing tools, async, on its own longer budget — and
//       uninstall takes it back out
// ---------------------------------------------------------------------------

#[test]
fn install_writes_post_tool_use_async_hook_and_uninstall_removes_it() {
    let root = temp_dir("post-tool-use");
    let home = temp_dir("post-tool-use-home");
    let db = root.join("mushroom-memory");
    let opts = claude_project_opts(&db);

    install_on_path(&root, &home, &opts).expect("install");

    let s: serde_json::Value = serde_json::from_str(&read(&root, ".claude/settings.json")).unwrap();
    let groups = s["hooks"]["PostToolUse"]
        .as_array()
        .expect("PostToolUse array");
    assert_eq!(groups.len(), 1, "{groups:?}");
    assert_eq!(groups[0]["matcher"], "Edit|Write|MultiEdit");
    let hook = &groups[0]["hooks"][0];
    assert_eq!(hook["type"], "command");
    assert_eq!(
        hook["command"],
        format!("mushroomdb touch '{}'", db.display())
    );
    // A re-extraction is not on the prompt's critical path: its own budget,
    // and it must not hold the tool call open.
    assert_eq!(hook["timeout"], 30);
    assert_eq!(hook["async"], true);

    // The prompt hook is still there, and is still the 5 s synchronous one.
    assert_eq!(s["hooks"]["UserPromptSubmit"][0]["hooks"][0]["timeout"], 5);

    // Idempotent: a second install adds no second group.
    install_on_path(&root, &home, &opts).expect("second install");
    let s2: serde_json::Value =
        serde_json::from_str(&read(&root, ".claude/settings.json")).unwrap();
    assert_eq!(s2["hooks"]["PostToolUse"].as_array().unwrap().len(), 1);

    // The manifest tracks it under its own event, so uninstall knows where to
    // look for it.
    let m: serde_json::Value = serde_json::from_str(&read(
        &root,
        ".claude/skills/mushroom/.install-manifest.json",
    ))
    .unwrap();
    let events: Vec<&str> = m["hooks"]
        .as_array()
        .expect("hooks array")
        .iter()
        .map(|h| h["event"].as_str().expect("event"))
        .collect();
    assert_eq!(events, vec!["UserPromptSubmit", "PostToolUse"], "{m}");

    run_uninstall(&root, &home, &opts).expect("uninstall");
    let s3: serde_json::Value =
        serde_json::from_str(&read(&root, ".claude/settings.json")).unwrap();
    assert!(
        s3["hooks"]["PostToolUse"].is_null()
            || s3["hooks"]["PostToolUse"].as_array().unwrap().is_empty(),
        "PostToolUse must be gone: {s3}"
    );
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
    let our_cmd = format!("mushroomdb recall '{}'", db.display());
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
// Test: the rendered skill states mask semantics correctly (allow-list),
//       documents the arguments the MCP server actually accepts, and names
//       every task tool an assistant is expected to reach for. The task tools
//       are the skill's whole subject; a rewrite that dropped one would leave
//       the assistant with no instruction to call it.
// ---------------------------------------------------------------------------

/// The task tools plus the two entry points the skill has to name, written the
/// way the text writes them so a bare word inside another word cannot pass.
const REQUIRED_TOOL_MENTIONS: &[&str] = &[
    "`map`",
    "`context`",
    "`impact`",
    "`owners`",
    "`why`",
    "`recall`",
    "`remember`",
    "`sync`",
    "`learn`",
    "`serve`",
];

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
        for tool in REQUIRED_TOOL_MENTIONS {
            assert!(
                text.contains(tool),
                "{name}: {tool} is never named — the assistant has no cue to call it"
            );
        }
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
//       resolves inside the npx shell (v0.5.0 bug). Everything that is not
//       provably this executable pins the published package instead.
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
fn npm_shim_on_path_is_not_our_binary_so_we_pin_npx() {
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
        classify_mcp_command(Some(path_var(&[&shim_dir]).as_os_str()), &current_exe),
        McpCommand::npx(),
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
        classify_mcp_command(Some(path_var(&[&link_dir]).as_os_str()), &current_exe),
        McpCommand::OnPath,
        "a symlink to this executable is this executable"
    );
}

#[test]
fn current_exe_itself_on_path_counts_as_on_path() {
    let dir = temp_dir("classify-self");
    let current_exe = named_bin(&dir, b"\x7fELF fake native binary\n");

    assert_eq!(
        classify_mcp_command(Some(path_var(&[&dir]).as_os_str()), &current_exe),
        McpCommand::OnPath
    );
}

#[test]
fn no_path_hit_pins_the_published_package() {
    let empty_dir = temp_dir("classify-empty");
    let exe_dir = temp_dir("classify-empty-exe");
    let current_exe = fake_exe(&exe_dir, b"\x7fELF fake native binary\n");

    // PATH set but holding no mushroomdb.
    assert_eq!(
        classify_mcp_command(Some(path_var(&[&empty_dir]).as_os_str()), &current_exe),
        McpCommand::npx()
    );

    // PATH unset entirely.
    assert_eq!(classify_mcp_command(None, &current_exe), McpCommand::npx());
}

#[test]
fn a_different_native_binary_first_on_path_shadows_us_so_we_pin_npx() {
    let other_dir = temp_dir("classify-other");
    let exe_dir = temp_dir("classify-other-exe");

    // Some other mushroomdb (an older global install) wins PATH resolution.
    named_bin(&other_dir, b"\x7fELF a different build\n");
    let current_exe = fake_exe(&exe_dir, b"\x7fELF fake native binary\n");

    assert_eq!(
        classify_mcp_command(Some(path_var(&[&other_dir]).as_os_str()), &current_exe),
        McpCommand::npx(),
        "PATH resolves to a binary that is not us, so pin the package instead"
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

// ---------------------------------------------------------------------------
// Test: a hook file whose mushroomdb block was hand-edited so its closing
//       marker is gone. Everything after the opening marker could be the
//       user's own work, so neither merge nor remove may guess.
// ---------------------------------------------------------------------------

#[test]
fn unterminated_hook_block_is_refused_rather_than_swallowed() {
    use cli::install::{merge_git_hook, remove_git_hook};

    let dir = temp_dir("git-hook-unterminated");
    let db = dir.join("mushroom-memory");
    let hook = dir.join("post-commit");

    // The closing marker is missing: the user deleted it, or an editor mangled
    // the file. Their `make lint` line sits below what would be our region.
    let corrupt =
        "#!/bin/sh\nmake lint\n# >>> mushroomdb >>>\n( mushroomdb sync '/old' & )\necho done\n";
    fs::write(&hook, corrupt).unwrap();

    let err = merge_git_hook(&hook, "mushroomdb", &db.to_string_lossy())
        .expect_err("an unterminated block must not be rewritten");
    assert!(
        err.0.contains("never closes"),
        "the message must say what is wrong: {}",
        err.0
    );
    assert!(
        err.0.contains(&hook.display().to_string()),
        "and which file: {}",
        err.0
    );
    assert_eq!(
        fs::read_to_string(&hook).unwrap(),
        corrupt,
        "refusing means writing nothing at all"
    );

    let err = remove_git_hook(&hook).expect_err("removal must refuse too");
    assert!(err.0.contains("never closes"), "{}", err.0);
    assert_eq!(
        fs::read_to_string(&hook).unwrap(),
        corrupt,
        "the user's `echo done` must survive"
    );

    // Repaired by hand, both work again.
    fs::write(&hook, format!("{corrupt}# <<< mushroomdb <<<\n")).unwrap();
    assert!(merge_git_hook(&hook, "mushroomdb", &db.to_string_lossy()).unwrap());
    let merged = fs::read_to_string(&hook).unwrap();
    assert_eq!(
        merged.matches("# >>> mushroomdb >>>").count(),
        1,
        "{merged}"
    );
    assert!(merged.contains("make lint"), "{merged}");
    assert!(
        !merged.contains("echo done"),
        "that line was inside our region once the block closed: {merged}"
    );
}
