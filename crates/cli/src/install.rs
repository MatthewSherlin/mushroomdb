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
//! # User-scope MCP config location (verified 2026-09-02 by live inspection)
//!
//! Claude Code user-level MCP servers live in `~/.claude.json` under the
//! top-level `"mcpServers"` key. This was verified empirically on a live
//! Claude Code install: `~/.claude/settings.json` holds env/permissions/hooks
//! but NO mcpServers key. Cursor uses `~/.cursor/mcp.json` (same format as
//! project-level `.cursor/mcp.json`).

use crate::CliError;
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

// Template files embedded at compile time. Files live inside the crates/cli
// package so `cargo package` includes them in the published tarball.
// Path is relative to this source file (crates/cli/src/install.rs).
const SKILL_TEMPLATE: &str = include_str!("../skills/mushroom/SKILL.md");
const CURSOR_RULES_TEMPLATE: &str = include_str!("../skills/mushroom/cursor-rules.mdc");

/// Placeholder string replaced with the real db path in embedded templates.
const DB_PATH_PLACEHOLDER: &str = "{{DB_PATH}}";

/// Placeholder string replaced with the command that invokes mushroomdb —
/// the bare name when it is on PATH, else the absolute path of the stable copy.
const BIN_PLACEHOLDER: &str = "{{BIN}}";

/// The MCP server name we write. Must not be changed without a migration.
const SERVER_NAME: &str = "mushroomdb";

/// The binary name looked up on PATH and used as the bare MCP command.
const BIN_NAME: &str = "mushroomdb";

/// How the MCP server entry (and the skill's bootstrap commands) invoke
/// mushroomdb.
///
/// The assistant host spawns the MCP server by `command`; a bare name only
/// works if it resolves on the host's PATH. `npx mushroomdb install` and a
/// local `target/release` build both run install from a binary that is NOT
/// on PATH, so writing the bare name silently produces a server that never
/// connects. In that case we copy the running executable to a stable,
/// install-owned location and write its absolute path instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BinaryLocation {
    /// `mushroomdb` resolves on PATH: write the bare name (upgrade-safe).
    OnPath,
    /// Not on PATH: copy this executable to `<home>/.mushroomdb/bin/mushroomdb`
    /// and write that absolute path.
    CopyFrom(PathBuf),
}

/// Decide how the MCP entry should invoke mushroomdb, from the real
/// environment.
pub fn detect_binary_location() -> BinaryLocation {
    match std::env::current_exe() {
        Ok(exe) => classify_binary_location(std::env::var_os("PATH").as_deref(), &exe),
        // Cannot locate ourselves — fall back to the bare name rather than fail.
        Err(_) => BinaryLocation::OnPath,
    }
}

/// Pure classifier behind [`detect_binary_location`]: decide whether the
/// `mushroomdb` that PATH resolves to is the executable now running.
///
/// A file named `mushroomdb` on PATH is not enough. `npx mushroomdb install`
/// prepends `~/.npm/_npx/<hash>/node_modules/.bin` to PATH, and the
/// `mushroomdb` there is npm's Node shim (`#!/usr/bin/env node`), not our
/// native binary; `npm i -g mushroomdb` installs the same shim. Treating that
/// as "on PATH" wrote a bare `mushroomdb` command that resolved only inside
/// the npx-spawned shell, so the MCP server and recall hook died with ENOENT
/// everywhere else (the v0.5.0 bug).
///
/// So: take the first PATH hit — that is what a bare name would resolve to —
/// and canonicalize both it and `current_exe`. Equal paths mean the bare name
/// runs this very executable, including via a symlink (how `cargo install` and
/// Homebrew expose it), which is the one case where the bare name is safe and
/// survives upgrades. Anything else — a shim, a different build, no hit at
/// all — means naming the copy explicitly.
pub fn classify_binary_location(path_var: Option<&OsStr>, current_exe: &Path) -> BinaryLocation {
    let copy = || BinaryLocation::CopyFrom(current_exe.to_path_buf());

    // What a bare `mushroomdb` would resolve to: the first PATH entry holding
    // a file by that name (`is_file` follows symlinks, so links count).
    let Some(hit) = path_var.and_then(|p| {
        std::env::split_paths(p)
            .map(|dir| dir.join(BIN_NAME))
            .find(|candidate| candidate.is_file())
    }) else {
        return copy();
    };

    // Identity, not name. Canonicalizing resolves symlinks and `..`, so a link
    // to us compares equal; if either side cannot be resolved we cannot prove
    // it is us, and copying is the answer that always works.
    match (fs::canonicalize(&hit), fs::canonicalize(current_exe)) {
        (Ok(on_path), Ok(running)) if on_path == running => BinaryLocation::OnPath,
        _ => copy(),
    }
}

/// Stable, install-owned location for the copied binary (user-level, so a
/// project-scope install still yields a command that works from any cwd).
fn stable_bin_path(home: &Path) -> PathBuf {
    home.join(".mushroomdb").join("bin").join(BIN_NAME)
}

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
    /// Hook entries added to a settings.json by this install.
    #[serde(default)]
    hooks: Vec<ManagedHook>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct ManagedMcpKey {
    /// The JSON file the key was added to (absolute path).
    file: PathBuf,
    /// The key inside `mcpServers`.
    server: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
struct ManagedHook {
    /// The settings.json file the hook was added to (absolute path).
    file: PathBuf,
    /// The hook event name (e.g. `UserPromptSubmit`).
    event: String,
    /// The exact command string that was added.
    command: String,
}

/// Claude Code hook event this install wires: fires before each prompt is
/// sent, so the recall digest lands as context ahead of the user's turn.
const HOOK_EVENT: &str = "UserPromptSubmit";
/// Kept short: the hook must never noticeably slow a prompt.
const HOOK_TIMEOUT_SECS: u64 = 5;

/// The second hook event: fires after a tool call, so an edit reaches the
/// graph while the assistant is still working rather than at the next commit.
const TOUCH_EVENT: &str = "PostToolUse";
/// The tools that change a file on disk. Anything else — a read, a search, a
/// shell command — leaves the working tree as the graph already has it.
const TOUCH_MATCHER: &str = "Edit|Write|MultiEdit";
/// Longer than the prompt hook's: re-extracting a file costs more than reading
/// a digest, and nothing is waiting on the answer. The run is `async`, so this
/// bounds a background process rather than the assistant's turn.
const TOUCH_TIMEOUT_SECS: u64 = 30;

/// Single-quote `s` for embedding in a POSIX shell command line, escaping
/// embedded single quotes as `'\''`. Claude Code runs a `type: "command"`
/// hook through a shell, so an unquoted path containing whitespace or shell
/// metacharacters is word-split and the hook silently receives the wrong
/// arguments — quoting both interpolations keeps the command exact.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The exact command string written into the hook entry.
fn recall_hook_command(bin_cmd: &str, db_str: &str) -> String {
    format!("{} recall {}", sh_quote(bin_cmd), sh_quote(db_str))
}

/// The exact command string written into the post-edit hook entry. `touch` in
/// hook mode prints nothing and exits 0 whatever it is handed.
fn touch_hook_command(bin_cmd: &str, db_str: &str) -> String {
    format!("{} touch {}", sh_quote(bin_cmd), sh_quote(db_str))
}

/// One `hooks.<event>` array entry in Claude Code's settings.json shape.
fn hook_entry(command: &str) -> serde_json::Value {
    serde_json::json!({ "hooks": [ { "type": "command", "command": command, "timeout": HOOK_TIMEOUT_SECS } ] })
}

/// The `PostToolUse` entry: matched to the file-editing tools, and `async` so
/// the assistant's tool call returns without waiting for the re-extraction.
fn touch_hook_entry(command: &str) -> serde_json::Value {
    serde_json::json!({
        "matcher": TOUCH_MATCHER,
        "hooks": [ {
            "type": "command",
            "command": command,
            "timeout": TOUCH_TIMEOUT_SECS,
            "async": true
        } ]
    })
}

/// True if any hook group under `event` contains a command hook equal to `command`.
fn settings_has_hook(root: &serde_json::Value, event: &str, command: &str) -> bool {
    root["hooks"][event]
        .as_array()
        .map(|groups| {
            groups.iter().any(|g| {
                g["hooks"]
                    .as_array()
                    .map(|hs| hs.iter().any(|h| h["command"] == command))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// Add one hook to `settings_file` (created if absent). Idempotent: no-op if
/// `command` is already present under `event`. Every other key in the file —
/// including other hook events and groups — is preserved. Errors out (no
/// write) rather than overwriting if `hooks` or `hooks.<event>` already exists
/// with an unexpected JSON type, or if the file's top level is not a JSON
/// object.
///
/// `entry` is the group to append, built by the caller: the two events this
/// install wires want different shapes, and only the caller knows which.
fn merge_hook_entry(
    settings_file: &Path,
    event: &str,
    command: &str,
    entry: serde_json::Value,
    manifest: &mut Manifest,
) -> Result<(), CliError> {
    let mut root: serde_json::Value = if settings_file.exists() {
        let raw = fs::read_to_string(settings_file)
            .map_err(|e| CliError(format!("cannot read {}: {e}", settings_file.display())))?;
        serde_json::from_str(&raw)
            .map_err(|e| CliError(format!("invalid JSON in {}: {e}", settings_file.display())))?
    } else {
        serde_json::json!({})
    };

    if !root.is_object() {
        return Err(CliError(format!(
            "{} is not a JSON object at its top level — refusing to add a hook",
            settings_file.display()
        )));
    }

    if settings_has_hook(&root, event, command) {
        return Ok(());
    }

    // Validate the shapes we are about to write into before touching
    // anything: a wrong-shaped `hooks` or `hooks.<event>` value belongs to
    // the user (or another tool) and must never be silently overwritten.
    match root.get("hooks") {
        None => root["hooks"] = serde_json::json!({}),
        Some(v) if v.is_object() => {}
        Some(_) => {
            return Err(CliError(format!(
                "{}: \"hooks\" is not a JSON object — refusing to overwrite it",
                settings_file.display()
            )));
        }
    }
    match root["hooks"].get(event) {
        None => root["hooks"][event] = serde_json::json!([]),
        Some(v) if v.is_array() => {}
        Some(_) => {
            return Err(CliError(format!(
                "{}: \"hooks.{event}\" is not a JSON array — refusing to overwrite it",
                settings_file.display()
            )));
        }
    }
    root["hooks"][event].as_array_mut().unwrap().push(entry);

    let parent = settings_file.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|e| CliError(format!("cannot create {}: {e}", parent.display())))?;
    let json = serde_json::to_string_pretty(&root)
        .map_err(|e| CliError(format!("cannot serialize settings: {e}")))?;
    fs::write(settings_file, json)
        .map_err(|e| CliError(format!("cannot write {}: {e}", settings_file.display())))?;

    manifest.hooks.push(ManagedHook {
        file: settings_file.to_path_buf(),
        event: event.into(),
        command: command.into(),
    });
    Ok(())
}

/// Remove exactly the hook groups whose only command is `command`; drop the
/// command from mixed groups; leave everything else semantically unchanged
/// (every key is re-serialized — comments are not supported since
/// `serde_json` is strict JSON).
///
/// Reads `hooks.<event>` through immutable accessors first, so a settings
/// file where the user removed the `hooks` key (or `<event>`, or shaped
/// either as something other than an object/array) is left byte-for-byte
/// untouched rather than having a stray `null` written back in.
fn remove_hook_entry(settings_file: &Path, event: &str, command: &str) -> Result<(), CliError> {
    if !settings_file.exists() {
        return Ok(());
    }
    let raw = fs::read_to_string(settings_file)
        .map_err(|e| CliError(format!("cannot read {}: {e}", settings_file.display())))?;
    let mut root: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
        CliError(format!(
            "corrupt settings json at {}: {e}",
            settings_file.display()
        ))
    })?;

    let Some(mut groups) = root
        .get("hooks")
        .and_then(|h| h.get(event))
        .and_then(|g| g.as_array())
        .cloned()
    else {
        // No matching (or well-shaped) event array — nothing of ours to
        // remove; leave the file exactly as it is, no write at all.
        return Ok(());
    };

    for g in groups.iter_mut() {
        if let Some(hs) = g["hooks"].as_array_mut() {
            hs.retain(|h| h["command"] != command);
        }
    }
    groups.retain(|g| {
        g["hooks"]
            .as_array()
            .map(|hs| !hs.is_empty())
            .unwrap_or(true)
    });

    let before = root.clone();
    if groups.is_empty() {
        root["hooks"].as_object_mut().unwrap().remove(event);
    } else {
        root["hooks"][event] = serde_json::Value::Array(groups);
    }
    if root == before {
        // The event array held none of our commands, so there is nothing to
        // remove. Writing anyway would re-serialize a file we do not own —
        // `serde_json` is built without `preserve_order`, so the user's key
        // order and indentation would be rewritten for no reason.
        return Ok(());
    }

    let json = serde_json::to_string_pretty(&root)
        .map_err(|e| CliError(format!("cannot serialize settings: {e}")))?;
    fs::write(settings_file, json)
        .map_err(|e| CliError(format!("cannot write {}: {e}", settings_file.display())))?;
    Ok(())
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
    run_install_with(project_root, home, opts, &detect_binary_location())
}

/// Like [`run_install`], but with the binary location supplied by the caller
/// instead of detected from PATH / `current_exe`. Tests use this to stay
/// deterministic; `run_install` is the real-environment wrapper.
pub fn run_install_with(
    project_root: &Path,
    home: &Path,
    opts: &InstallOpts,
    bin: &BinaryLocation,
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

    let manifest_path = manifest_path(project_root, home, opts.project, &platforms);

    // Load the existing manifest so we can union it with what this run writes.
    // This covers partial-drift re-installs: if SKILL.md was edited but the MCP
    // entry is still intact, only the file is re-written this run; unioning
    // preserves the MCP key in the saved manifest so uninstall cleans it up too.
    let existing = load_manifest(&manifest_path);

    let mut manifest = Manifest::default();

    // Resolve the command the MCP entry and skill templates will use. For the
    // off-PATH case this copies the binary first so the path it names exists.
    let bin_cmd = match bin {
        BinaryLocation::OnPath => BIN_NAME.to_string(),
        BinaryLocation::CopyFrom(src) => {
            let dest = stable_bin_path(home);
            copy_binary(src, &dest, &mut manifest)?;
            dest.to_string_lossy().into_owned()
        }
    };

    for plat in &platforms {
        let step = install_platform(
            project_root,
            home,
            plat,
            opts.project,
            &db_str,
            &bin_cmd,
            &mut manifest,
        );
        if let Err(e) = step {
            // Persist whatever was already written (binary copy, earlier
            // platform's files) so uninstall can still clean up after a
            // partial failure. Best effort: the original error wins.
            let anything_written = !manifest.files.is_empty()
                || !manifest.mcp_keys.is_empty()
                || !manifest.hooks.is_empty();
            if anything_written {
                let merged = union_manifests(load_manifest(&manifest_path), &manifest);
                let _ = write_manifest(&manifest_path, &merged);
            }
            return Err(e);
        }
    }

    let anything_written =
        !manifest.files.is_empty() || !manifest.mcp_keys.is_empty() || !manifest.hooks.is_empty();

    if anything_written {
        // Union this-run entries with the existing manifest (dedup by path/key).
        let merged = union_manifests(existing, &manifest);
        write_manifest(&manifest_path, &merged)?;
    }

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
    for h in &manifest.hooks {
        out.push_str(&format!(
            "  added  {} hook in {}\n",
            h.event,
            h.file.display()
        ));
    }
    if anything_written {
        out.push_str(&format!("  manifest  {}\n", manifest_path.display()));
        out.push_str(&format!(
            "  mcp command  {bin_cmd}\n  restart your assistant to connect the MCP server\n"
        ));
    } else {
        out.push_str("  (already installed — no changes)\n");
    }
    Ok(out)
}

/// Copy the running binary to its stable location. No-op if the bytes at
/// `dest` already match `src` (idempotent re-install); overwrites when they
/// differ (upgrade). Records `dest` in the manifest so uninstall removes it.
fn copy_binary(src: &Path, dest: &Path, manifest: &mut Manifest) -> Result<(), CliError> {
    let bytes = fs::read(src)
        .map_err(|e| CliError(format!("cannot read binary {}: {e}", src.display())))?;
    if fs::read(dest).map(|cur| cur == bytes).unwrap_or(false) {
        return Ok(());
    }
    let parent = dest.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|e| CliError(format!("cannot create {}: {e}", parent.display())))?;
    // Write to a temp name and rename so a running MCP server holding the old
    // inode keeps working and the swap is atomic.
    let tmp = parent.join(format!(".{BIN_NAME}.tmp-{}", std::process::id()));
    fs::write(&tmp, &bytes)
        .map_err(|e| CliError(format!("cannot write {}: {e}", tmp.display())))?;
    let finish = || -> Result<(), CliError> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))
                .map_err(|e| CliError(format!("cannot chmod {}: {e}", tmp.display())))?;
        }
        fs::rename(&tmp, dest)
            .map_err(|e| CliError(format!("cannot move binary into {}: {e}", dest.display())))
    };
    if let Err(e) = finish() {
        let _ = fs::remove_file(&tmp); // never leave an untracked temp file behind
        return Err(e);
    }
    manifest.files.push(dest.to_path_buf());
    Ok(())
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

    // Remove hooks (before files, same reasoning as MCP keys).
    for h in &manifest.hooks {
        if h.file.exists() {
            remove_hook_entry(&h.file, &h.event, &h.command)?;
            removed.push(format!(
                "removed  {} hook from {}",
                h.event,
                h.file.display()
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
                // User-scope: verified empirically on a live Claude Code install.
                // ~/.claude.json holds top-level mcpServers; ~/.claude/settings.json
                // holds env/permissions/hooks but no mcpServers key.
                home.join(".claude.json")
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
/// `args[1]` (the db path) differs from what we'd write. An entry for the
/// SAME db with a different `command` is ours to repair (e.g. a bare name
/// that never resolved, or a stale absolute path after an upgrade), so it is
/// not a conflict.
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

    let existing_db = existing["args"]
        .get(1)
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if existing_db == db_str {
        return Ok(()); // Same db — idempotent or repairable, no conflict.
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
    bin_cmd: &str,
    manifest: &mut Manifest,
) -> Result<(), CliError> {
    match platform {
        Platform::ClaudeCode => {
            install_claude_code(project_root, home, project_scope, db_str, bin_cmd, manifest)
        }
        Platform::Cursor => {
            install_cursor(project_root, home, project_scope, db_str, bin_cmd, manifest)
        }
        Platform::All => unreachable!("expand_platform never produces All"),
    }
}

/// Substitute both template placeholders.
fn render_template(template: &str, db_str: &str, bin_cmd: &str) -> String {
    template
        .replace(DB_PATH_PLACEHOLDER, db_str)
        .replace(BIN_PLACEHOLDER, bin_cmd)
}

fn install_claude_code(
    project_root: &Path,
    home: &Path,
    project_scope: bool,
    db_str: &str,
    bin_cmd: &str,
    manifest: &mut Manifest,
) -> Result<(), CliError> {
    let skill_content = render_template(SKILL_TEMPLATE, db_str, bin_cmd);

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

    // MCP JSON. User-scope writes to ~/.claude.json (top-level mcpServers),
    // not ~/.claude/settings.json (which holds env/hooks, not mcpServers).
    let mcp_file = if project_scope {
        project_root.join(".mcp.json")
    } else {
        home.join(".claude.json")
    };
    merge_mcp_entry(&mcp_file, db_str, bin_cmd, manifest)?;

    // Both hooks: settings.json in the same scope as the skill. The prompt
    // hook first, so a manifest lists them in the order they were written.
    let settings_file = if project_scope {
        project_root.join(".claude").join("settings.json")
    } else {
        home.join(".claude").join("settings.json")
    };
    let recall = recall_hook_command(bin_cmd, db_str);
    merge_hook_entry(
        &settings_file,
        HOOK_EVENT,
        &recall,
        hook_entry(&recall),
        manifest,
    )?;
    let touch = touch_hook_command(bin_cmd, db_str);
    merge_hook_entry(
        &settings_file,
        TOUCH_EVENT,
        &touch,
        touch_hook_entry(&touch),
        manifest,
    )?;

    Ok(())
}

fn install_cursor(
    project_root: &Path,
    home: &Path,
    project_scope: bool,
    db_str: &str,
    bin_cmd: &str,
    manifest: &mut Manifest,
) -> Result<(), CliError> {
    let rules_content = render_template(CURSOR_RULES_TEMPLATE, db_str, bin_cmd);

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
    merge_mcp_entry(&mcp_file, db_str, bin_cmd, manifest)?;

    Ok(())
}

// ---------------------------------------------------------------------------
// MCP JSON merge helpers
// ---------------------------------------------------------------------------

/// Add `mcpServers.mushroomdb` to a JSON config file. Creates the file if
/// absent. No-op if the entry already matches (idempotent).
fn merge_mcp_entry(
    mcp_file: &Path,
    db_str: &str,
    bin_cmd: &str,
    manifest: &mut Manifest,
) -> Result<(), CliError> {
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

    let desired = mcp_server_entry(db_str, bin_cmd);
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

fn mcp_server_entry(db_str: &str, bin_cmd: &str) -> serde_json::Value {
    serde_json::json!({
        "command": bin_cmd,
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

/// Load an existing manifest from `path`. Returns an empty manifest if absent or unparseable.
fn load_manifest(path: &Path) -> Manifest {
    let raw = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return Manifest::default(),
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// Union `existing` with `this_run`, deduplicating by path (files), by
/// (file, server) pair (mcp_keys), and by full equality (hooks). Entries from
/// `this_run` win on collision so the manifest always reflects the latest
/// state.
fn union_manifests(mut existing: Manifest, this_run: &Manifest) -> Manifest {
    for f in &this_run.files {
        if !existing.files.contains(f) {
            existing.files.push(f.clone());
        }
    }
    for k in &this_run.mcp_keys {
        let already = existing
            .mcp_keys
            .iter()
            .any(|e| e.file == k.file && e.server == k.server);
        if !already {
            existing.mcp_keys.push(k.clone());
        }
    }
    for h in &this_run.hooks {
        if !existing.hooks.contains(h) {
            existing.hooks.push(h.clone());
        }
    }
    existing
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
// Git hook block — `mushroomdb sync` after every commit
// ---------------------------------------------------------------------------
//
// A git hook file belongs to the repository owner, not to us. Everything below
// therefore edits one marked region and nothing else: the region is rewritten
// in place when it changes, and removing it restores the user's lines exactly.
// The pure text transforms are split out from the filesystem wrappers so the
// merge and removal rules can be reasoned about — and tested — without a disk.

/// Opening marker of the region this module owns inside a git hook.
pub const HOOK_BEGIN: &str = "# >>> mushroomdb >>>";
/// Closing marker of that region.
pub const HOOK_END: &str = "# <<< mushroomdb <<<";
/// Written as the first line when we create a hook file ourselves.
const HOOK_SHEBANG: &str = "#!/bin/sh";

/// The block a git hook runs: one backgrounded, silenced `sync`.
///
/// Backgrounded (`( … & )` in a subshell, so no job-control notice reaches the
/// terminal) because a hook must not make `git commit` wait on a graph
/// refresh, and silenced because a hook that prints — or fails — on a store
/// that is momentarily busy would be noise on every commit. `sync` exits 3 when
/// another process holds the write lock, and the next commit picks the work up.
///
/// Both interpolations are shell-quoted: a database under a path with a space
/// in it would otherwise be word-split into two arguments.
#[must_use]
pub fn git_hook_block(bin_cmd: &str, db: &str) -> String {
    format!(
        "{HOOK_BEGIN}\n( {} sync {} >/dev/null 2>&1 & )\n{HOOK_END}\n",
        sh_quote(bin_cmd),
        sh_quote(db)
    )
}

/// What [`strip_hook_block`] found in a hook file.
enum Stripped {
    /// No opening marker: every line belongs to whoever wrote the file.
    Absent,
    /// A complete region was removed; this is what is left.
    Removed(String),
    /// An opening marker with no closing marker. Where our region ends is
    /// unknowable, so nothing may be removed.
    Unterminated,
}

/// `text` with our marked region removed.
///
/// Blank lines left dangling at the end are dropped, so a merge followed by a
/// removal returns the original bytes rather than the original plus the blank
/// separator the merge inserted.
///
/// An opening marker with no closing marker is [`Stripped::Unterminated`]
/// rather than "ours to the end of the file". Someone hand-edited the region,
/// and the lines below the opening marker are now as likely to be theirs as
/// ours — a `make lint` they added under it would be deleted by the guess.
/// Both public helpers turn this into an error and write nothing, which is the
/// same rule the rest of this module follows for a config file whose shape it
/// does not recognise.
fn strip_hook_block(text: &str) -> Stripped {
    let mut kept: Vec<&str> = Vec::new();
    let mut inside = false;
    let mut found = false;
    for line in text.lines() {
        if !inside && line.trim_end() == HOOK_BEGIN {
            inside = true;
            found = true;
            continue;
        }
        if inside {
            if line.trim_end() == HOOK_END {
                inside = false;
            }
            continue;
        }
        kept.push(line);
    }
    if !found {
        return Stripped::Absent;
    }
    if inside {
        return Stripped::Unterminated;
    }
    while kept.last().is_some_and(|l| l.trim().is_empty()) {
        kept.pop();
    }
    let mut out = kept.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    Stripped::Removed(out)
}

/// The error both helpers return for [`Stripped::Unterminated`].
fn unterminated(hook_file: &Path) -> CliError {
    CliError(format!(
        "{}: a mushroomdb block opens with `{HOOK_BEGIN}` but never closes \
         — refusing to edit it; delete the block by hand and re-run",
        hook_file.display()
    ))
}

/// What the hook file should contain once `block` is in it.
///
/// Idempotent by construction: any existing region is stripped first and the
/// fresh one appended, so a re-merge of the same block reproduces the same
/// bytes and a merge of a *different* block rewrites in place instead of
/// stacking a second region.
fn merged_hook_text(existing: Option<&str>, block: &str) -> Result<String, ()> {
    let base = match existing {
        None => String::new(),
        Some(text) => match strip_hook_block(text) {
            Stripped::Absent => text.to_string(),
            Stripped::Removed(rest) => rest,
            Stripped::Unterminated => return Err(()),
        },
    };
    let mut lines: Vec<&str> = base.lines().collect();
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    // A file we are creating needs an interpreter line; one the user wrote
    // already has whichever they chose, and we must not add a second.
    if lines.is_empty() {
        lines.push(HOOK_SHEBANG);
    }
    let mut out = lines.join("\n");
    out.push_str("\n\n");
    out.push_str(block);
    Ok(out)
}

/// Whether `text` is nothing but an interpreter line — the shape a hook file we
/// created is left in once our region is stripped out of it.
fn only_a_shebang(text: &str) -> bool {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .all(|l| l.starts_with("#!"))
}

/// Put the sync block in `hook_file`, creating the file (mode 755, with a
/// `#!/bin/sh` line) if it is not there. Returns whether anything changed.
///
/// Every line the user has in the file is preserved, and running this twice
/// with the same arguments writes nothing the second time. A file whose
/// mushroomdb block was hand-edited so its closing marker is gone is an error
/// and is left byte-for-byte alone; see [`strip_hook_block`].
pub fn merge_git_hook(hook_file: &Path, bin_cmd: &str, db: &str) -> Result<bool, CliError> {
    let existing = if hook_file.exists() {
        Some(
            fs::read_to_string(hook_file)
                .map_err(|e| CliError(format!("cannot read {}: {e}", hook_file.display())))?,
        )
    } else {
        None
    };
    let next = merged_hook_text(existing.as_deref(), &git_hook_block(bin_cmd, db))
        .map_err(|()| unterminated(hook_file))?;
    if existing.as_deref() == Some(next.as_str()) {
        return Ok(false);
    }
    let parent = hook_file.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|e| CliError(format!("cannot create {}: {e}", parent.display())))?;
    fs::write(hook_file, &next)
        .map_err(|e| CliError(format!("cannot write {}: {e}", hook_file.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // git ignores a hook that is not executable, so this is not cosmetic.
        fs::set_permissions(hook_file, fs::Permissions::from_mode(0o755)).map_err(|e| {
            CliError(format!(
                "cannot make {} executable: {e}",
                hook_file.display()
            ))
        })?;
    }
    Ok(true)
}

/// Take the sync block back out of `hook_file`. Returns whether anything
/// changed.
///
/// The file itself is deleted only when nothing but an interpreter line is
/// left, which is exactly the state a hook *we* created is in — a hook the user
/// wrote has their lines in it and is rewritten rather than removed. An empty
/// stub of theirs would be deleted too, which git cannot tell apart from the
/// stub never having existed.
///
/// An unterminated block is an error and the file is left alone; see
/// [`strip_hook_block`].
pub fn remove_git_hook(hook_file: &Path) -> Result<bool, CliError> {
    if !hook_file.exists() {
        return Ok(false);
    }
    let existing = fs::read_to_string(hook_file)
        .map_err(|e| CliError(format!("cannot read {}: {e}", hook_file.display())))?;
    let next = match strip_hook_block(&existing) {
        // None of it is ours; leave the file untouched.
        Stripped::Absent => return Ok(false),
        Stripped::Removed(rest) => rest,
        Stripped::Unterminated => return Err(unterminated(hook_file)),
    };
    if only_a_shebang(&next) {
        fs::remove_file(hook_file)
            .map_err(|e| CliError(format!("cannot remove {}: {e}", hook_file.display())))?;
        return Ok(true);
    }
    fs::write(hook_file, next)
        .map_err(|e| CliError(format!("cannot write {}: {e}", hook_file.display())))?;
    Ok(true)
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
