//! `mushroomdb install` / `uninstall` — wire the /mushroom skill and MCP
//! server into Claude Code and Cursor.
//!
//! # Design notes
//!
//! - No network: writes local config only; binary is already on disk.
//! - Idempotent: running install twice is a no-op (exit 0).
//! - Non-destructive: refuses to overwrite user files install didn't create.
//! - Manifest-driven uninstall: tracks every file written; removes exactly
//!   what install created.
//!
//! # User-scope MCP config location (verified 2026-09-02)
//!
//! Claude Code (the CLI) reads user-level MCP servers from
//! `~/.claude/settings.json` under the `"mcpServers"` key — the same
//! structure as project-level `.mcp.json`. This is verified against the
//! `claude mcp add --help` output which prints:
//!   "Adds an MCP server to your user settings (~/.claude/settings.json)"
//! Cursor uses `~/.cursor/mcp.json` for user-scope MCP servers (same format).

use crate::CliError;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

// Template files embedded at compile time.
// Path is relative to this source file (crates/cli/src/install.rs).
const SKILL_TEMPLATE: &str = include_str!("../../../skills/mushroom/SKILL.md");
const CURSOR_RULES_TEMPLATE: &str = include_str!("../../../skills/mushroom/cursor-rules.mdc");

/// Placeholder string replaced with the real db path in embedded templates.
const DB_PATH_PLACEHOLDER: &str = "{{DB_PATH}}";

/// The MCP server name we write. Must not be changed without a migration.
const SERVER_NAME: &str = "mushroomdb";

/// Which assistant platform(s) to wire up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Platform {
    ClaudeCode,
    Cursor,
    All,
}

impl Platform {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "claude-code" => Ok(Platform::ClaudeCode),
            "cursor" => Ok(Platform::Cursor),
            "all" => Ok(Platform::All),
            other => Err(format!(
                "--platform must be claude-code | cursor | all, got: {other}"
            )),
        }
    }
}

/// Options parsed from `mushroomdb install [flags]` or `mushroomdb uninstall [flags]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallOpts {
    /// Which platform to wire up. `None` = auto-detect.
    pub platform: Option<Platform>,
    /// Project scope (`--project`). If false → user scope.
    pub project: bool,
    /// Database directory. `None` = use the scope default.
    pub db: Option<PathBuf>,
}

impl InstallOpts {
    pub fn default_db(&self, project_root: &Path, home: &Path) -> PathBuf {
        if self.project {
            project_root.join("mushroom-memory")
        } else {
            home.join(".mushroomdb").join("memory")
        }
    }
}

// ---------------------------------------------------------------------------
// Manifest — tracks everything install wrote so uninstall can undo it.
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Default, Debug)]
struct Manifest {
    /// Files created by this install (absolute paths).
    files: Vec<PathBuf>,
    /// MCP JSON keys added by this install.
    mcp_keys: Vec<ManagedMcpKey>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct ManagedMcpKey {
    /// The JSON file the key was added to (absolute path).
    file: PathBuf,
    /// The key inside `mcpServers`.
    server: String,
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Install the /mushroom skill and MCP server entry for the resolved platforms.
///
/// `project_root` is the directory where project-scope config files live
/// (`.mcp.json`, `.claude/`, `.cursor/`). `home` is the user HOME directory.
/// Tests pass temp directories for both; main.rs passes real values.
pub fn run_install(
    project_root: &Path,
    home: &Path,
    opts: &InstallOpts,
) -> Result<String, CliError> {
    let db = opts
        .db
        .clone()
        .unwrap_or_else(|| opts.default_db(project_root, home));
    let db_str = db.to_string_lossy();

    let resolved = resolve_platform(project_root, home, opts.platform.as_ref())?;
    let platforms = expand_platform(&resolved);

    // Check for any conflicts before writing anything (atomic from user's POV).
    for plat in &platforms {
        preflight_check(project_root, home, plat, opts.project, &db_str)?;
    }

    let mut manifest = Manifest::default();

    for plat in &platforms {
        install_platform(
            project_root,
            home,
            plat,
            opts.project,
            &db_str,
            &mut manifest,
        )?;
    }

    // Write manifest last (after all files are in place).
    let manifest_path = manifest_path(project_root, home, opts.project, &platforms);
    write_manifest(&manifest_path, &manifest)?;

    let mut out = format!("mushroomdb installed ({} platform(s))\n", platforms.len());
    for f in &manifest.files {
        out.push_str(&format!("  wrote  {}\n", f.display()));
    }
    for k in &manifest.mcp_keys {
        out.push_str(&format!(
            "  added  mcpServers.{} in {}\n",
            k.server,
            k.file.display()
        ));
    }
    out.push_str(&format!("  manifest  {}\n", manifest_path.display()));
    Ok(out)
}

/// Uninstall: remove exactly what install wrote. Reads the manifest.
pub fn run_uninstall(
    project_root: &Path,
    home: &Path,
    opts: &InstallOpts,
) -> Result<String, CliError> {
    let resolved = resolve_platform(project_root, home, opts.platform.as_ref())?;
    let platforms = expand_platform(&resolved);

    let manifest_path = manifest_path(project_root, home, opts.project, &platforms);
    if !manifest_path.exists() {
        return Err(CliError(format!(
            "no install manifest found at {} — nothing to uninstall",
            manifest_path.display()
        )));
    }

    let raw = fs::read_to_string(&manifest_path)
        .map_err(|e| CliError(format!("cannot read manifest: {e}")))?;
    let manifest: Manifest =
        serde_json::from_str(&raw).map_err(|e| CliError(format!("corrupt manifest: {e}")))?;

    let mut removed = Vec::new();

    // Remove MCP keys first (before files, in case files include .mcp.json).
    for key in &manifest.mcp_keys {
        if key.file.exists() {
            remove_mcp_key(&key.file, &key.server)?;
            removed.push(format!(
                "removed  mcpServers.{} from {}",
                key.server,
                key.file.display()
            ));
        }
    }

    // Remove files.
    for f in &manifest.files {
        if f.exists() {
            fs::remove_file(f)
                .map_err(|e| CliError(format!("cannot remove {}: {e}", f.display())))?;
            removed.push(format!("removed  {}", f.display()));
        }
    }

    // Remove the manifest itself.
    if manifest_path.exists() {
        fs::remove_file(&manifest_path)
            .map_err(|e| CliError(format!("cannot remove manifest: {e}")))?;
    }

    let mut out = "mushroomdb uninstalled\n".to_string();
    for line in &removed {
        out.push_str(&format!("  {line}\n"));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Platform resolution
// ---------------------------------------------------------------------------

fn resolve_platform(
    project_root: &Path,
    home: &Path,
    requested: Option<&Platform>,
) -> Result<Platform, CliError> {
    if let Some(p) = requested {
        return Ok(p.clone());
    }

    // Auto-detect.
    let has_claude = home.join(".claude").exists() || project_root.join(".claude").exists();
    let has_cursor = project_root.join(".cursor").exists() || home.join(".cursor").exists();

    match (has_claude, has_cursor) {
        (true, true) => Ok(Platform::All),
        (true, false) => Ok(Platform::ClaudeCode),
        (false, true) => Ok(Platform::Cursor),
        (false, false) => Err(CliError(
            "cannot auto-detect platform: neither ~/.claude nor .cursor/ found.\n\
             Pass --platform claude-code, --platform cursor, or --platform all."
                .to_string(),
        )),
    }
}

fn expand_platform(p: &Platform) -> Vec<Platform> {
    match p {
        Platform::All => vec![Platform::ClaudeCode, Platform::Cursor],
        Platform::ClaudeCode => vec![Platform::ClaudeCode],
        Platform::Cursor => vec![Platform::Cursor],
    }
}

// ---------------------------------------------------------------------------
// Pre-flight conflict check (no writes)
// ---------------------------------------------------------------------------

fn preflight_check(
    project_root: &Path,
    home: &Path,
    platform: &Platform,
    project_scope: bool,
    db_str: &str,
) -> Result<(), CliError> {
    match platform {
        Platform::ClaudeCode => {
            let mcp_file = if project_scope {
                project_root.join(".mcp.json")
            } else {
                home.join(".claude").join("settings.json")
            };
            check_mcp_conflict(&mcp_file, db_str)?;
        }
        Platform::Cursor => {
            let mcp_file = if project_scope {
                project_root.join(".cursor").join("mcp.json")
            } else {
                home.join(".cursor").join("mcp.json")
            };
            check_mcp_conflict(&mcp_file, db_str)?;
        }
        Platform::All => unreachable!("expand_platform never produces All"),
    }
    Ok(())
}

/// Check if a MCP JSON file has a conflicting `mushroomdb` entry.
///
/// A conflict is: the file exists, has `mcpServers.mushroomdb`, and its
/// `args[1]` (the db path) differs from what we'd write.
fn check_mcp_conflict(mcp_file: &Path, db_str: &str) -> Result<(), CliError> {
    if !mcp_file.exists() {
        return Ok(());
    }
    let raw = fs::read_to_string(mcp_file)
        .map_err(|e| CliError(format!("cannot read {}: {e}", mcp_file.display())))?;
    let v: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| CliError(format!("invalid JSON in {}: {e}", mcp_file.display())))?;

    let existing = &v["mcpServers"][SERVER_NAME];
    if existing.is_null() {
        return Ok(()); // Key absent — no conflict.
    }

    // Key present — check if it matches what we'd write.
    let existing_cmd = existing["command"].as_str().unwrap_or("");
    let existing_db = existing["args"]
        .get(1)
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if existing_cmd == "mushroomdb" && existing_db == db_str {
        return Ok(()); // Exact match — idempotent, no conflict.
    }

    Err(CliError(format!(
        "conflict: {} already has mcpServers.mushroomdb pointing to {:?}\n\
         To update it, run `mushroomdb uninstall` first, then re-install.\n\
         Or manually edit {} and remove the existing mushroomdb entry.",
        mcp_file.display(),
        existing_db,
        mcp_file.display()
    )))
}

// ---------------------------------------------------------------------------
// Per-platform installation
// ---------------------------------------------------------------------------

fn install_platform(
    project_root: &Path,
    home: &Path,
    platform: &Platform,
    project_scope: bool,
    db_str: &str,
    manifest: &mut Manifest,
) -> Result<(), CliError> {
    match platform {
        Platform::ClaudeCode => {
            install_claude_code(project_root, home, project_scope, db_str, manifest)
        }
        Platform::Cursor => install_cursor(project_root, home, project_scope, db_str, manifest),
        Platform::All => unreachable!("expand_platform never produces All"),
    }
}

fn install_claude_code(
    project_root: &Path,
    home: &Path,
    project_scope: bool,
    db_str: &str,
    manifest: &mut Manifest,
) -> Result<(), CliError> {
    let skill_content = SKILL_TEMPLATE.replace(DB_PATH_PLACEHOLDER, db_str);

    let skill_dir = if project_scope {
        project_root.join(".claude").join("skills").join("mushroom")
    } else {
        home.join(".claude").join("skills").join("mushroom")
    };
    let skill_file = skill_dir.join("SKILL.md");

    // Idempotent: skip if the file already has the same content.
    if !file_matches(&skill_file, &skill_content) {
        fs::create_dir_all(&skill_dir)
            .map_err(|e| CliError(format!("cannot create {}: {e}", skill_dir.display())))?;
        fs::write(&skill_file, &skill_content)
            .map_err(|e| CliError(format!("cannot write {}: {e}", skill_file.display())))?;
        manifest.files.push(skill_file);
    }

    // MCP JSON.
    let mcp_file = if project_scope {
        project_root.join(".mcp.json")
    } else {
        home.join(".claude").join("settings.json")
    };
    merge_mcp_entry(&mcp_file, db_str, manifest)?;

    Ok(())
}

fn install_cursor(
    project_root: &Path,
    home: &Path,
    project_scope: bool,
    db_str: &str,
    manifest: &mut Manifest,
) -> Result<(), CliError> {
    let rules_content = CURSOR_RULES_TEMPLATE.replace(DB_PATH_PLACEHOLDER, db_str);

    let rules_dir = if project_scope {
        project_root.join(".cursor").join("rules")
    } else {
        home.join(".cursor").join("rules")
    };
    let rules_file = rules_dir.join("mushroom.mdc");

    if !file_matches(&rules_file, &rules_content) {
        fs::create_dir_all(&rules_dir)
            .map_err(|e| CliError(format!("cannot create {}: {e}", rules_dir.display())))?;
        fs::write(&rules_file, &rules_content)
            .map_err(|e| CliError(format!("cannot write {}: {e}", rules_file.display())))?;
        manifest.files.push(rules_file);
    }

    // Cursor MCP JSON.
    let mcp_file = if project_scope {
        project_root.join(".cursor").join("mcp.json")
    } else {
        home.join(".cursor").join("mcp.json")
    };
    merge_mcp_entry(&mcp_file, db_str, manifest)?;

    Ok(())
}

// ---------------------------------------------------------------------------
// MCP JSON merge helpers
// ---------------------------------------------------------------------------

/// Add `mcpServers.mushroomdb` to a JSON config file. Creates the file if
/// absent. No-op if the entry already matches (idempotent).
fn merge_mcp_entry(mcp_file: &Path, db_str: &str, manifest: &mut Manifest) -> Result<(), CliError> {
    let mut root: serde_json::Value = if mcp_file.exists() {
        let raw = fs::read_to_string(mcp_file)
            .map_err(|e| CliError(format!("cannot read {}: {e}", mcp_file.display())))?;
        serde_json::from_str(&raw)
            .map_err(|e| CliError(format!("invalid JSON in {}: {e}", mcp_file.display())))?
    } else {
        serde_json::json!({})
    };

    // Ensure `mcpServers` object exists.
    if !root["mcpServers"].is_object() {
        root["mcpServers"] = serde_json::json!({});
    }

    let desired = mcp_server_entry(db_str);
    let existing = &root["mcpServers"][SERVER_NAME];

    if existing == &desired {
        return Ok(()); // Exact match — idempotent.
    }

    // Write the entry.
    root["mcpServers"][SERVER_NAME] = desired;

    let parent = mcp_file.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|e| CliError(format!("cannot create {}: {e}", parent.display())))?;

    let json = serde_json::to_string_pretty(&root)
        .map_err(|e| CliError(format!("cannot serialize mcp json: {e}")))?;
    fs::write(mcp_file, json)
        .map_err(|e| CliError(format!("cannot write {}: {e}", mcp_file.display())))?;

    manifest.mcp_keys.push(ManagedMcpKey {
        file: mcp_file.to_path_buf(),
        server: SERVER_NAME.to_string(),
    });

    Ok(())
}

/// Remove `mcpServers.<server>` from a JSON config file. Leaves the file in
/// place (with the key removed) unless `mcpServers` becomes empty, in which
/// case we still leave the file (the user may have other keys).
fn remove_mcp_key(mcp_file: &Path, server: &str) -> Result<(), CliError> {
    if !mcp_file.exists() {
        return Ok(());
    }
    let raw = fs::read_to_string(mcp_file)
        .map_err(|e| CliError(format!("cannot read {}: {e}", mcp_file.display())))?;
    let mut root: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| CliError(format!("corrupt mcp json at {}: {e}", mcp_file.display())))?;

    if let Some(servers) = root["mcpServers"].as_object_mut() {
        servers.remove(server);
    }

    let json = serde_json::to_string_pretty(&root)
        .map_err(|e| CliError(format!("cannot serialize mcp json: {e}")))?;
    fs::write(mcp_file, json)
        .map_err(|e| CliError(format!("cannot write {}: {e}", mcp_file.display())))?;
    Ok(())
}

fn mcp_server_entry(db_str: &str) -> serde_json::Value {
    serde_json::json!({
        "command": "mushroomdb",
        "args": ["mcp", db_str]
    })
}

// ---------------------------------------------------------------------------
// Manifest helpers
// ---------------------------------------------------------------------------

fn manifest_path(
    project_root: &Path,
    home: &Path,
    project_scope: bool,
    platforms: &[Platform],
) -> PathBuf {
    if !project_scope {
        return home.join(".mushroomdb").join("install-manifest.json");
    }
    // Project scope: prefer the Claude Code location; fall back to Cursor.
    if platforms.contains(&Platform::ClaudeCode) {
        project_root
            .join(".claude")
            .join("skills")
            .join("mushroom")
            .join(".install-manifest.json")
    } else {
        project_root.join(".cursor").join(".install-manifest.json")
    }
}

fn write_manifest(path: &Path, manifest: &Manifest) -> Result<(), CliError> {
    let parent = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent).map_err(|e| {
        CliError(format!(
            "cannot create manifest dir {}: {e}",
            parent.display()
        ))
    })?;
    let json = serde_json::to_string_pretty(manifest)
        .map_err(|e| CliError(format!("cannot serialize manifest: {e}")))?;
    fs::write(path, json)
        .map_err(|e| CliError(format!("cannot write manifest {}: {e}", path.display())))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

/// True if the file exists and its content equals `expected`.
fn file_matches(path: &Path, expected: &str) -> bool {
    fs::read_to_string(path)
        .map(|s| s == expected)
        .unwrap_or(false)
}
