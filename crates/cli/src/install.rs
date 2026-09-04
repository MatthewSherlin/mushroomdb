//! `mushroomdb install` / `uninstall` — wire the /mushroom skill, the MCP
//! server, the prompt hooks and the git hooks into an assistant.
//!
//! # Design notes
//!
//! - Idempotent: running install twice is a no-op (exit 0).
//! - Non-destructive: refuses to overwrite user files install didn't create.
//! - Manifest-driven uninstall: tracks every file, key, hook, ignore line and
//!   external registration it wrote; removes exactly that.
//! - The only network access is the optional pre-warm, which is a best-effort
//!   warm cache and never fails the install.
//!
//! # User-scope MCP config location (verified 2026-09-02 by live inspection)
//!
//! Claude Code user-level MCP servers live in `~/.claude.json` under the
//! top-level `"mcpServers"` key. This was verified empirically on a live
//! Claude Code install: `~/.claude/settings.json` holds env/permissions/hooks
//! but NO mcpServers key. Cursor uses `~/.cursor/mcp.json` (same format as
//! project-level `.cursor/mcp.json`). Codex keeps its own config and is
//! written through the `codex` CLI rather than by editing a file.

use crate::CliError;
use serde::{Deserialize, Serialize};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

// Template files embedded at compile time. Files live inside the crates/cli
// package so `cargo package` includes them in the published tarball.
// Path is relative to this source file (crates/cli/src/install.rs).
const SKILL_TEMPLATE: &str = include_str!("../skills/mushroom/SKILL.md");
const CURSOR_RULES_TEMPLATE: &str = include_str!("../skills/mushroom/cursor-rules.mdc");

/// Placeholder string replaced with the real db path in embedded templates.
const DB_PATH_PLACEHOLDER: &str = "{{DB_PATH}}";

/// Placeholder string replaced with the command that invokes mushroomdb.
/// Substituted with [`McpCommand::shell`], which is already shell-quoted, so
/// the templates must leave it unquoted.
const BIN_PLACEHOLDER: &str = "{{BIN}}";

/// The MCP server name we write. Must not be changed without a migration.
const SERVER_NAME: &str = "mushroomdb";

/// The binary name looked up on PATH and used as the bare MCP command.
const BIN_NAME: &str = "mushroomdb";

/// The npm package the `npx` form runs. Same name as the binary.
const NPM_PACKAGE: &str = "mushroomdb";

/// The version an `npx` entry pins: the one that wrote it.
const CRATE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long the optional pre-warm may take before it is abandoned. A cold
/// `npx` download of a native package is slow on a slow link, and the whole
/// point is to pay that cost here rather than at the assistant's first prompt.
const PREWARM_TIMEOUT_SECS: u64 = 180;

// ---------------------------------------------------------------------------
// How the server is invoked
// ---------------------------------------------------------------------------

/// How the MCP server entry (and the skill's bootstrap commands) invoke
/// mushroomdb.
///
/// The assistant host spawns the MCP server by `command`, so that command has
/// to resolve from *its* process, not from the shell install ran in. A bare
/// name only works when it resolves on the host's PATH, and the one case where
/// that is provable is when the `mushroomdb` PATH resolves to is this very
/// executable. Everything else — npm's Node shim, a different build, a local
/// `target/release` binary, no hit at all — pins the published package and
/// lets `npx` fetch it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpCommand {
    /// `npx -y mushroomdb@<version> …` — the default, and the only form that
    /// works from a machine where nothing is installed globally.
    Npx { version: String },
    /// An absolute path the user named with `--command`.
    Explicit(PathBuf),
    /// `mushroomdb` resolves on PATH *and* is this executable: the bare name
    /// is safe and follows upgrades.
    OnPath,
}

impl McpCommand {
    /// The `npx` form pinned to the version of the binary writing it.
    #[must_use]
    pub fn npx() -> Self {
        McpCommand::Npx {
            version: CRATE_VERSION.to_string(),
        }
    }

    /// The program to exec and the arguments that come before the subcommand.
    fn program(&self) -> (String, Vec<String>) {
        match self {
            McpCommand::Npx { version } => (
                "npx".to_string(),
                vec!["-y".to_string(), format!("{NPM_PACKAGE}@{version}")],
            ),
            McpCommand::Explicit(p) => (p.to_string_lossy().into_owned(), Vec::new()),
            McpCommand::OnPath => (BIN_NAME.to_string(), Vec::new()),
        }
    }

    /// The MCP server entry: `{"command": …, "args": [… , sub, db]}`.
    ///
    /// Nothing here is shell-quoted. An MCP host spawns the command with an
    /// argv, so a path with a space in it is one element and quoting it would
    /// make the quotes part of the filename.
    #[must_use]
    pub fn json_entry(&self, sub: &str, db: &str) -> serde_json::Value {
        let (command, mut args) = self.program();
        args.push(sub.to_string());
        args.push(db.to_string());
        serde_json::json!({ "command": command, "args": args })
    }

    /// The same invocation as a command *prefix* for a POSIX shell, already
    /// quoted where quoting is needed.
    ///
    /// Hook entries and the skill's copy-paste lines are read by a shell, so
    /// an explicit path has to survive a space in it. The bare name and the
    /// `npx` form contain no metacharacters and are left as they read.
    #[must_use]
    pub fn shell(&self) -> String {
        let (command, args) = self.program();
        let mut out = match self {
            McpCommand::Explicit(_) => sh_quote(&command),
            _ => command,
        };
        for a in args {
            out.push(' ');
            out.push_str(&a);
        }
        out
    }

    /// The full argv, for handing to another CLI that registers servers.
    fn argv(&self, sub: &str, db: &str) -> Vec<String> {
        let (command, mut args) = self.program();
        args.push(sub.to_string());
        args.push(db.to_string());
        let mut out = vec![command];
        out.extend(args);
        out
    }
}

/// Decide how the MCP entry should invoke mushroomdb, from the options and the
/// real environment.
#[must_use]
pub fn detect_mcp_command(opts: &InstallOpts) -> McpCommand {
    if let Some(path) = &opts.command {
        return McpCommand::Explicit(path.clone());
    }
    match std::env::current_exe() {
        Ok(exe) => classify_mcp_command(std::env::var_os("PATH").as_deref(), &exe),
        // Cannot locate ourselves — the pinned package always resolves.
        Err(_) => McpCommand::npx(),
    }
}

/// Pure classifier behind [`detect_mcp_command`]: decide whether the
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
/// survives upgrades. Anything else means pinning the published package.
#[must_use]
pub fn classify_mcp_command(path_var: Option<&OsStr>, current_exe: &Path) -> McpCommand {
    // What a bare `mushroomdb` would resolve to: the first PATH entry holding
    // a file by that name (`is_file` follows symlinks, so links count).
    let Some(hit) = path_var.and_then(|p| {
        std::env::split_paths(p)
            .map(|dir| dir.join(BIN_NAME))
            .find(|candidate| candidate.is_file())
    }) else {
        return McpCommand::npx();
    };

    // Identity, not name. Canonicalizing resolves symlinks and `..`, so a link
    // to us compares equal; if either side cannot be resolved we cannot prove
    // it is us, and the pinned package is the answer that always works.
    match (fs::canonicalize(&hit), fs::canonicalize(current_exe)) {
        (Ok(on_path), Ok(running)) if on_path == running => McpCommand::OnPath,
        _ => McpCommand::npx(),
    }
}

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

/// Which assistant platform(s) to wire up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Platform {
    ClaudeCode,
    Cursor,
    Codex,
    All,
}

impl Platform {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "claude-code" => Ok(Platform::ClaudeCode),
            "cursor" => Ok(Platform::Cursor),
            "codex" => Ok(Platform::Codex),
            "all" => Ok(Platform::All),
            other => Err(format!(
                "--platform must be claude-code | cursor | codex | all, got: {other}"
            )),
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Platform::ClaudeCode => "claude-code",
            Platform::Cursor => "cursor",
            Platform::Codex => "codex",
            Platform::All => "all",
        }
    }
}

/// Where the install lives: alongside one repository, or once for the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Project,
    User,
}

impl Scope {
    fn label(self) -> &'static str {
        match self {
            Scope::Project => "project",
            Scope::User => "user",
        }
    }
}

/// Options parsed from `mushroomdb install [flags]` or `mushroomdb uninstall [flags]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallOpts {
    /// Which platform to wire up. `None` = auto-detect.
    pub platform: Option<Platform>,
    /// Project or user scope. `None` = auto: project inside a git checkout.
    pub scope: Option<Scope>,
    /// Database directory. `None` = use the scope default.
    pub db: Option<PathBuf>,
    /// `--command <path>`: invoke this binary instead of `npx`/the bare name.
    pub command: Option<PathBuf>,
    /// Write the `post-commit` / `post-checkout` / `post-merge` sync hooks.
    pub git_hooks: bool,
    /// Run `npx -y mushroomdb@<v> --version` once so the first real spawn is
    /// not a cold download.
    pub prewarm: bool,
}

/// The store directory an install with no `--db` uses.
#[must_use]
pub fn default_db(scope: Scope, project_root: &Path, home: &Path) -> PathBuf {
    match scope {
        Scope::Project => project_root.join("mushroom-memory"),
        Scope::User => home.join(".mushroomdb").join("memory"),
    }
}

/// Resolve the scope, and say whether it was inferred.
///
/// A git checkout is a project: its store belongs beside it, is ignored by the
/// repository, and its hooks fire on its commits. Anywhere else there is no
/// project to scope to, so the install is the user's.
fn resolve_scope(project_root: &Path, requested: Option<Scope>) -> (Scope, bool) {
    match requested {
        Some(s) => (s, false),
        None if project_root.join(".git").exists() => (Scope::Project, true),
        None => (Scope::User, true),
    }
}

// ---------------------------------------------------------------------------
// External programs
// ---------------------------------------------------------------------------

/// The world outside the two directories install is given: the programs it
/// shells out to (`codex`, `npx`) and how long it will wait for them.
///
/// Carried explicitly rather than read from the process environment at the
/// point of use, so a test can point PATH at a directory of stand-ins without
/// mutating global state that its neighbours share.
#[derive(Debug, Clone)]
pub struct Externals {
    /// PATH used to resolve external programs. `None` resolves nothing.
    pub path: Option<OsString>,
    /// Budget for the pre-warm.
    pub prewarm_timeout: Duration,
}

impl Externals {
    /// The real process environment.
    #[must_use]
    pub fn from_env() -> Self {
        Self::with_path(std::env::var_os("PATH"))
    }

    /// The same, with an explicit PATH.
    #[must_use]
    pub fn with_path(path: Option<OsString>) -> Self {
        Self {
            path,
            prewarm_timeout: Duration::from_secs(PREWARM_TIMEOUT_SECS),
        }
    }

    /// The first executable named `program` on this PATH.
    fn which(&self, program: &str) -> Option<PathBuf> {
        let path = self.path.as_ref()?;
        std::env::split_paths(path)
            .map(|dir| dir.join(program))
            .find(|c| is_executable(c))
    }
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Run `bin` to completion, returning its stderr (trimmed) on a non-zero exit.
fn run_and_capture(bin: &Path, args: &[String]) -> Result<(), String> {
    let out = std::process::Command::new(bin)
        .args(args)
        .output()
        .map_err(|e| format!("cannot run {}: {e}", bin.display()))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let detail = if stderr.is_empty() {
        String::new()
    } else {
        format!(": {stderr}")
    };
    Err(format!(
        "{} {} exited with {}{detail}",
        bin.display(),
        args.join(" "),
        out.status
    ))
}

/// Run `bin`, giving up after `timeout`. Output is discarded — only the exit
/// status matters — so the child cannot block on a pipe nobody drains.
fn run_with_timeout(bin: &Path, args: &[String], timeout: Duration) -> Result<(), String> {
    let mut child = std::process::Command::new(bin)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", bin.display()))?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(format!("exited with {status}")),
            Ok(None) => {}
            Err(e) => return Err(format!("cannot wait for {}: {e}", bin.display())),
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("timed out after {}s", timeout.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(25));
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
    /// Git hook files this install put its block into.
    #[serde(default)]
    git_hooks: Vec<PathBuf>,
    /// Single lines added to a file the user owns (the `.gitignore` entry).
    #[serde(default)]
    gitignore: Vec<ManagedLine>,
    /// Whether a Codex MCP server was registered through the `codex` CLI.
    #[serde(default)]
    codex: bool,
}

impl Manifest {
    fn is_empty(&self) -> bool {
        self.files.is_empty()
            && self.mcp_keys.is_empty()
            && self.hooks.is_empty()
            && self.git_hooks.is_empty()
            && self.gitignore.is_empty()
            && !self.codex
    }
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

/// One line this install appended to a text file the user owns.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
struct ManagedLine {
    /// The file the line was added to (absolute path).
    file: PathBuf,
    /// The exact line, without its newline.
    line: String,
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
/// arguments — quoting keeps the command exact.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The exact command string written into the hook entry. `shell` is already
/// quoted; only the database path still needs it.
fn recall_hook_command(shell: &str, db_str: &str) -> String {
    format!("{shell} recall {}", sh_quote(db_str))
}

/// The exact command string written into the post-edit hook entry. `touch` in
/// hook mode prints nothing and exits 0 whatever it is handed.
fn touch_hook_command(shell: &str, db_str: &str) -> String {
    format!("{shell} touch {}", sh_quote(db_str))
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

/// Everything the write phase needs, gathered once so the per-step functions
/// stay readable.
struct Ctx<'a> {
    project_root: &'a Path,
    home: &'a Path,
    scope: Scope,
    db: &'a str,
    cmd: &'a McpCommand,
    ext: &'a Externals,
    git_hooks: bool,
}

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
    run_install_with(
        project_root,
        home,
        opts,
        &detect_mcp_command(opts),
        &Externals::from_env(),
    )
}

/// Like [`run_install`], but with the server command and the external
/// environment supplied by the caller instead of detected. Tests use this to
/// stay deterministic and offline; `run_install` is the real-environment
/// wrapper.
pub fn run_install_with(
    project_root: &Path,
    home: &Path,
    opts: &InstallOpts,
    cmd: &McpCommand,
    ext: &Externals,
) -> Result<String, CliError> {
    let (scope, auto_scope) = resolve_scope(project_root, opts.scope);
    let db = opts
        .db
        .clone()
        .unwrap_or_else(|| default_db(scope, project_root, home));
    let db_str = db.to_string_lossy();

    let resolved = resolve_platform(project_root, home, opts.platform.as_ref())?;
    let platforms = expand_platform(&resolved);

    // Check for anything that would make this install fail halfway before
    // writing a single byte.
    for plat in &platforms {
        preflight_check(project_root, home, plat, scope, &db_str, ext)?;
    }

    let ctx = Ctx {
        project_root,
        home,
        scope,
        db: &db_str,
        cmd,
        ext,
        git_hooks: opts.git_hooks,
    };

    let manifest_path = manifest_path(project_root, home, scope, &platforms);

    // Load the existing manifest so we can union it with what this run writes.
    // This covers partial-drift re-installs: if SKILL.md was edited but the MCP
    // entry is still intact, only the file is re-written this run; unioning
    // preserves the MCP key in the saved manifest so uninstall cleans it up too.
    let existing = load_manifest(&manifest_path);

    let mut manifest = Manifest::default();
    let mut notes: Vec<String> = Vec::new();

    let outcome = write_everything(&ctx, &platforms, &mut manifest, &mut notes);
    if let Err(e) = outcome {
        // Persist whatever was already written (an earlier platform's files,
        // a git hook) so uninstall can still clean up after a partial
        // failure. Best effort: the original error wins.
        if !manifest.is_empty() {
            let merged = union_manifests(load_manifest(&manifest_path), &manifest);
            let _ = write_manifest(&manifest_path, &merged);
        }
        return Err(e);
    }

    let anything_written = !manifest.is_empty();
    if anything_written {
        // Union this-run entries with the existing manifest (dedup by path/key).
        let merged = union_manifests(existing, &manifest);
        write_manifest(&manifest_path, &merged)?;
    }

    let labels: Vec<&str> = platforms.iter().map(Platform::label).collect();
    let mut out = format!("mushroomdb installed ({})\n", labels.join(", "));
    out.push_str(&format!(
        "  scope  {}{}\n",
        scope.label(),
        if auto_scope { " (auto-detected)" } else { "" }
    ));
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
    for g in &manifest.gitignore {
        out.push_str(&format!("  added  {} to {}\n", g.line, g.file.display()));
    }
    for h in &manifest.git_hooks {
        out.push_str(&format!("  added  git hook {}\n", h.display()));
    }
    if manifest.codex {
        out.push_str(&format!("  added  codex mcp server {SERVER_NAME}\n"));
    }
    if anything_written {
        out.push_str(&format!("  manifest  {}\n", manifest_path.display()));
        out.push_str(&format!("  mcp command  {}\n", cmd.shell()));
    } else {
        out.push_str("  (already installed — no changes)\n");
    }
    for n in &notes {
        out.push_str(&format!("  {n}\n"));
    }
    out.push_str(&format!(
        "next: restart Claude Code in {}, then type /mushroom\n",
        project_root.display()
    ));
    Ok(out)
}

/// The whole write phase, so a failure anywhere in it still leaves the caller
/// holding the partial manifest.
fn write_everything(
    ctx: &Ctx<'_>,
    platforms: &[Platform],
    manifest: &mut Manifest,
    notes: &mut Vec<String>,
) -> Result<(), CliError> {
    if let Some(w) = scope_conflict_note(ctx, platforms) {
        notes.push(w);
    }

    for plat in platforms {
        install_platform(ctx, plat, manifest, notes)?;
    }

    // The repository-level wiring is platform-independent: the store is
    // ignored by git and the graph is re-synced after commits whichever
    // assistant reads it.
    if ctx.scope == Scope::Project {
        ensure_gitignore_line(ctx, manifest)?;
        if ctx.git_hooks {
            install_git_hooks(ctx, manifest)?;
        }
    }

    if let Some(w) = prewarm(ctx) {
        notes.push(w);
    }
    Ok(())
}

/// Uninstall: remove exactly what install wrote. Reads the manifest.
pub fn run_uninstall(
    project_root: &Path,
    home: &Path,
    opts: &InstallOpts,
) -> Result<String, CliError> {
    run_uninstall_with(project_root, home, opts, &Externals::from_env())
}

/// Like [`run_uninstall`], with the external environment supplied by the
/// caller (Codex removal shells out to the `codex` CLI).
pub fn run_uninstall_with(
    project_root: &Path,
    home: &Path,
    opts: &InstallOpts,
    ext: &Externals,
) -> Result<String, CliError> {
    let (scope, _) = resolve_scope(project_root, opts.scope);
    let resolved = resolve_platform(project_root, home, opts.platform.as_ref())?;
    let platforms = expand_platform(&resolved);

    let manifest_path = manifest_path(project_root, home, scope, &platforms);
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

    // Git hooks: the marked region only, never the user's own lines.
    for h in &manifest.git_hooks {
        if remove_git_hook(h)? {
            removed.push(format!("removed  git hook block from {}", h.display()));
        }
    }

    // The ignore line, exactly as it was written.
    for g in &manifest.gitignore {
        if remove_line(&g.file, &g.line)? {
            removed.push(format!("removed  {} from {}", g.line, g.file.display()));
        }
    }

    // Codex holds its own config; hand the removal back to its CLI.
    if manifest.codex {
        match ext.which("codex") {
            Some(bin) => {
                run_and_capture(&bin, &["mcp".into(), "remove".into(), SERVER_NAME.into()])
                    .map_err(|e| CliError(format!("codex mcp remove failed: {e}")))?;
                removed.push(format!("removed  codex mcp server {SERVER_NAME}"));
            }
            // Not being able to reach `codex` must not strand every other
            // thing the manifest lists.
            None => removed.push(
                "warning: codex is not on PATH — run `codex mcp remove mushroomdb` yourself"
                    .to_string(),
            ),
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

    // Auto-detect. Codex is never inferred: registering with it runs another
    // program, which is not something to do because a directory exists.
    let has_claude = home.join(".claude").exists() || project_root.join(".claude").exists();
    let has_cursor = project_root.join(".cursor").exists() || home.join(".cursor").exists();

    match (has_claude, has_cursor) {
        (true, true) => Ok(Platform::All),
        (true, false) => Ok(Platform::ClaudeCode),
        (false, true) => Ok(Platform::Cursor),
        (false, false) => Err(CliError(
            "cannot auto-detect platform: neither ~/.claude nor .cursor/ found.\n\
             Pass --platform claude-code, --platform cursor, --platform codex, or --platform all."
                .to_string(),
        )),
    }
}

/// `All` is the two platforms whose config this program writes itself. Codex
/// is deliberately not in it: it is wired by running the `codex` CLI, which
/// may not exist, and `--platform all` must not fail on a machine that simply
/// does not have it.
fn expand_platform(p: &Platform) -> Vec<Platform> {
    match p {
        Platform::All => vec![Platform::ClaudeCode, Platform::Cursor],
        other => vec![other.clone()],
    }
}

// ---------------------------------------------------------------------------
// Pre-flight conflict check (no writes)
// ---------------------------------------------------------------------------

fn preflight_check(
    project_root: &Path,
    home: &Path,
    platform: &Platform,
    scope: Scope,
    db_str: &str,
    ext: &Externals,
) -> Result<(), CliError> {
    match platform {
        Platform::ClaudeCode => {
            check_mcp_conflict(&claude_mcp_file(project_root, home, scope), db_str)
        }
        Platform::Cursor => check_mcp_conflict(&cursor_mcp_file(project_root, home, scope), db_str),
        // Nothing of Codex's is a file we read; what can fail early is the CLI
        // being absent, and that is worth saying before anything is written.
        Platform::Codex => codex_bin(ext).map(|_| ()),
        Platform::All => unreachable!("expand_platform never produces All"),
    }
}

fn claude_mcp_file(project_root: &Path, home: &Path, scope: Scope) -> PathBuf {
    match scope {
        Scope::Project => project_root.join(".mcp.json"),
        // User-scope: verified empirically on a live Claude Code install.
        // ~/.claude.json holds top-level mcpServers; ~/.claude/settings.json
        // holds env/permissions/hooks but no mcpServers key.
        Scope::User => home.join(".claude.json"),
    }
}

fn cursor_mcp_file(project_root: &Path, home: &Path, scope: Scope) -> PathBuf {
    match scope {
        Scope::Project => project_root.join(".cursor").join("mcp.json"),
        Scope::User => home.join(".cursor").join("mcp.json"),
    }
}

/// The database an existing entry serves: the argument straight after `mcp`.
///
/// Its position moved between versions — 0.5.x wrote `["mcp", db]`, the npx
/// form writes `["-y", "mushroomdb@x.y.z", "mcp", db]` — so the subcommand is
/// what locates it, not an index.
fn entry_db(entry: &serde_json::Value) -> Option<&str> {
    let args = entry["args"].as_array()?;
    let at = args.iter().position(|a| a == "mcp")?;
    args.get(at + 1)?.as_str()
}

/// Check if a MCP JSON file has a conflicting `mushroomdb` entry.
///
/// A conflict is: the file exists, has `mcpServers.mushroomdb`, and the
/// database it names differs from what we'd write. An entry for the SAME db
/// with a different `command` is ours to repair (a bare name that never
/// resolved, an absolute path from 0.5.x, an older version pin), so it is not
/// a conflict.
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

    let existing_db = entry_db(existing).unwrap_or("");
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

/// A server registered in the *other* scope still shows up in the assistant,
/// and two of them pointed at two stores is a confusing place to be. Say so;
/// never touch the other scope's file.
fn scope_conflict_note(ctx: &Ctx<'_>, platforms: &[Platform]) -> Option<String> {
    if !platforms.contains(&Platform::ClaudeCode) {
        return None;
    }
    let (other, label, flag) = match ctx.scope {
        Scope::Project => (
            claude_mcp_file(ctx.project_root, ctx.home, Scope::User),
            "user",
            "--user",
        ),
        Scope::User => (
            claude_mcp_file(ctx.project_root, ctx.home, Scope::Project),
            "project",
            "--project",
        ),
    };
    if !has_our_server(&other) {
        return None;
    }
    Some(format!(
        "warning: a {label}-scope mushroomdb server also exists ({}) — \
         both will load; to remove that one run: mushroomdb uninstall {flag}",
        other.display()
    ))
}

fn has_our_server(mcp_file: &Path) -> bool {
    let Ok(raw) = fs::read_to_string(mcp_file) else {
        return false;
    };
    serde_json::from_str::<serde_json::Value>(&raw)
        .map(|v| !v["mcpServers"][SERVER_NAME].is_null())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Per-platform installation
// ---------------------------------------------------------------------------

fn install_platform(
    ctx: &Ctx<'_>,
    platform: &Platform,
    manifest: &mut Manifest,
    notes: &mut Vec<String>,
) -> Result<(), CliError> {
    match platform {
        Platform::ClaudeCode => install_claude_code(ctx, manifest, notes),
        Platform::Cursor => install_cursor(ctx, manifest, notes),
        Platform::Codex => install_codex(ctx, manifest),
        Platform::All => unreachable!("expand_platform never produces All"),
    }
}

/// Substitute both template placeholders. `bin_cmd` is the pre-quoted shell
/// form, so the templates carry `{{BIN}}` unquoted.
fn render_template(template: &str, db_str: &str, bin_cmd: &str) -> String {
    template
        .replace(DB_PATH_PLACEHOLDER, db_str)
        .replace(BIN_PLACEHOLDER, bin_cmd)
}

fn install_claude_code(
    ctx: &Ctx<'_>,
    manifest: &mut Manifest,
    notes: &mut Vec<String>,
) -> Result<(), CliError> {
    let shell = ctx.cmd.shell();
    let skill_content = render_template(SKILL_TEMPLATE, ctx.db, &shell);

    let skill_dir = match ctx.scope {
        Scope::Project => ctx
            .project_root
            .join(".claude")
            .join("skills")
            .join("mushroom"),
        Scope::User => ctx.home.join(".claude").join("skills").join("mushroom"),
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

    let mcp_file = claude_mcp_file(ctx.project_root, ctx.home, ctx.scope);
    merge_mcp_entry(&mcp_file, ctx, manifest, notes)?;

    // Both hooks: settings.json in the same scope as the skill. The prompt
    // hook first, so a manifest lists them in the order they were written.
    let settings_file = match ctx.scope {
        Scope::Project => ctx.project_root.join(".claude").join("settings.json"),
        Scope::User => ctx.home.join(".claude").join("settings.json"),
    };
    let recall = recall_hook_command(&shell, ctx.db);
    merge_hook_entry(
        &settings_file,
        HOOK_EVENT,
        &recall,
        hook_entry(&recall),
        manifest,
    )?;
    let touch = touch_hook_command(&shell, ctx.db);
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
    ctx: &Ctx<'_>,
    manifest: &mut Manifest,
    notes: &mut Vec<String>,
) -> Result<(), CliError> {
    let rules_content = render_template(CURSOR_RULES_TEMPLATE, ctx.db, &ctx.cmd.shell());

    let rules_dir = match ctx.scope {
        Scope::Project => ctx.project_root.join(".cursor").join("rules"),
        Scope::User => ctx.home.join(".cursor").join("rules"),
    };
    let rules_file = rules_dir.join("mushroom.mdc");

    if !file_matches(&rules_file, &rules_content) {
        fs::create_dir_all(&rules_dir)
            .map_err(|e| CliError(format!("cannot create {}: {e}", rules_dir.display())))?;
        fs::write(&rules_file, &rules_content)
            .map_err(|e| CliError(format!("cannot write {}: {e}", rules_file.display())))?;
        manifest.files.push(rules_file);
    }

    let mcp_file = cursor_mcp_file(ctx.project_root, ctx.home, ctx.scope);
    merge_mcp_entry(&mcp_file, ctx, manifest, notes)?;

    Ok(())
}

/// The `codex` executable, or an error that says what to do about it.
fn codex_bin(ext: &Externals) -> Result<PathBuf, CliError> {
    ext.which("codex").ok_or_else(|| {
        CliError(
            "codex was not found on PATH — install the Codex CLI, or drop \
             `--platform codex`"
                .to_string(),
        )
    })
}

/// Register the server with Codex through its own CLI.
///
/// Codex owns its configuration file and its format is its business, so this
/// writes nothing: it runs `codex mcp add mushroomdb -- <command> <args…>` and
/// lets Codex record it. 0.6.0 ships no Codex skill — the MCP tools carry
/// their own descriptions, which is what Codex reads.
fn install_codex(ctx: &Ctx<'_>, manifest: &mut Manifest) -> Result<(), CliError> {
    let bin = codex_bin(ctx.ext)?;
    let mut args = vec![
        "mcp".to_string(),
        "add".to_string(),
        SERVER_NAME.to_string(),
        "--".to_string(),
    ];
    args.extend(ctx.cmd.argv("mcp", ctx.db));
    run_and_capture(&bin, &args).map_err(|e| CliError(format!("codex mcp add failed: {e}")))?;
    manifest.codex = true;
    Ok(())
}

// ---------------------------------------------------------------------------
// Repository wiring: the ignore line and the sync hooks
// ---------------------------------------------------------------------------

/// The git hooks a sync belongs in: after a commit lands, after a branch
/// changes the working tree, and after a merge brings other people's commits
/// in. All three leave the graph a commit behind if they are skipped.
const GIT_HOOKS: &[&str] = &["post-commit", "post-checkout", "post-merge"];

/// The `.gitignore` line for a store kept inside the repository, or `None`
/// when it is kept outside — a repository has no business ignoring a path it
/// does not contain.
fn gitignore_line(project_root: &Path, db: &str) -> Option<String> {
    let rel = Path::new(db).strip_prefix(project_root).ok()?;
    if rel.as_os_str().is_empty() {
        return None;
    }
    Some(format!("{}/", rel.to_string_lossy().replace('\\', "/")))
}

/// Append the store directory to the repository's `.gitignore` unless some
/// spelling of it is already listed. Creates the file if it is absent.
fn ensure_gitignore_line(ctx: &Ctx<'_>, manifest: &mut Manifest) -> Result<(), CliError> {
    let Some(line) = gitignore_line(ctx.project_root, ctx.db) else {
        return Ok(());
    };
    let path = ctx.project_root.join(".gitignore");
    let existed = path.exists();
    let current = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(CliError(format!("cannot read {}: {e}", path.display()))),
    };
    let bare = line.trim_end_matches('/');
    if current
        .lines()
        .map(str::trim)
        .any(|l| l == line || l == bare || l == format!("/{line}") || l == format!("/{bare}"))
    {
        return Ok(());
    }
    let mut next = current;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(&line);
    next.push('\n');
    fs::write(&path, next)
        .map_err(|e| CliError(format!("cannot write {}: {e}", path.display())))?;
    // A `.gitignore` that only exists because of us is ours to take away
    // again: uninstall strips the line first and then removes the file, so a
    // repository that had none is left with none.
    if !existed {
        manifest.files.push(path.clone());
    }
    manifest.gitignore.push(ManagedLine { file: path, line });
    Ok(())
}

/// Remove one exact line from a text file. Returns whether anything changed;
/// a file that does not hold the line is not rewritten at all.
fn remove_line(path: &Path, line: &str) -> Result<bool, CliError> {
    let Ok(current) = fs::read_to_string(path) else {
        return Ok(false);
    };
    if !current.lines().any(|l| l == line) {
        return Ok(false);
    }
    let kept: Vec<&str> = current.lines().filter(|l| *l != line).collect();
    let mut next = kept.join("\n");
    if !next.is_empty() {
        next.push('\n');
    }
    fs::write(path, next).map_err(|e| CliError(format!("cannot write {}: {e}", path.display())))?;
    Ok(true)
}

/// The directory holding this checkout's hooks, following the `gitdir:` link a
/// worktree or submodule leaves in place of a `.git` directory.
fn git_hooks_dir(project_root: &Path) -> Option<PathBuf> {
    let dot_git = project_root.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git.join("hooks"));
    }
    let text = fs::read_to_string(&dot_git).ok()?;
    let target = text.strip_prefix("gitdir:")?.trim();
    let target = Path::new(target);
    let resolved = if target.is_absolute() {
        target.to_path_buf()
    } else {
        project_root.join(target)
    };
    Some(resolved.join("hooks"))
}

fn install_git_hooks(ctx: &Ctx<'_>, manifest: &mut Manifest) -> Result<(), CliError> {
    // Not a checkout: there is nothing to hook into, and that is not an error.
    let Some(dir) = git_hooks_dir(ctx.project_root) else {
        return Ok(());
    };
    let shell = ctx.cmd.shell();
    for name in GIT_HOOKS {
        let file = dir.join(name);
        if merge_git_hook(&file, &shell, ctx.db)? {
            manifest.git_hooks.push(file);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Pre-warm
// ---------------------------------------------------------------------------

/// Fetch the pinned package once, so the assistant's first spawn of the MCP
/// server is not a cold `npx` download inside a startup timeout.
///
/// Best effort in every direction: it only applies to the `npx` form, it is
/// skipped when asked to be, and a failure is a line in the summary rather
/// than a failed install — the entry that was written is correct either way.
fn prewarm(ctx: &Ctx<'_>) -> Option<String> {
    let McpCommand::Npx { version } = ctx.cmd else {
        return None;
    };
    let args = vec![
        "-y".to_string(),
        format!("{NPM_PACKAGE}@{version}"),
        "--version".to_string(),
    ];
    let Some(npx) = ctx.ext.which("npx") else {
        return Some(
            "warning: pre-warm skipped — npx is not on PATH; the first MCP \
             spawn will download the package"
                .to_string(),
        );
    };
    match run_with_timeout(&npx, &args, ctx.ext.prewarm_timeout) {
        Ok(()) => None,
        Err(e) => Some(format!(
            "warning: pre-warm of {NPM_PACKAGE}@{version} failed ({e}) — \
             the first MCP spawn will download the package"
        )),
    }
}

// ---------------------------------------------------------------------------
// MCP JSON merge helpers
// ---------------------------------------------------------------------------

/// Add `mcpServers.mushroomdb` to a JSON config file. Creates the file if
/// absent. No-op if the entry already matches (idempotent). An entry that is
/// present but different is an upgrade: it is rewritten, and the summary says
/// so, because a stale command is exactly the failure this replaces.
fn merge_mcp_entry(
    mcp_file: &Path,
    ctx: &Ctx<'_>,
    manifest: &mut Manifest,
    notes: &mut Vec<String>,
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

    let desired = ctx.cmd.json_entry("mcp", ctx.db);
    let existing = &root["mcpServers"][SERVER_NAME];

    if existing == &desired {
        return Ok(()); // Exact match — idempotent.
    }
    let replaced = !existing.is_null();

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
    if replaced {
        notes.push(format!(
            "updated mcp command in {} → {}",
            mcp_file.display(),
            ctx.cmd.shell()
        ));
    }

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

// ---------------------------------------------------------------------------
// Manifest helpers
// ---------------------------------------------------------------------------

fn manifest_path(
    project_root: &Path,
    home: &Path,
    scope: Scope,
    platforms: &[Platform],
) -> PathBuf {
    // Codex writes nothing project-local — its registration lives wherever the
    // Codex CLI keeps it — so a Codex-only install records itself under the
    // home directory whatever the scope, in its own file so it cannot collide
    // with a user-scope Claude Code manifest.
    if platforms == [Platform::Codex] {
        return home.join(".mushroomdb").join("install-manifest-codex.json");
    }
    if scope == Scope::User {
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

/// Union `existing` with `this_run`, deduplicating by path (files, git hooks),
/// by (file, server) pair (mcp_keys), and by full equality (hooks, lines).
/// Entries from `this_run` win on collision so the manifest always reflects
/// the latest state.
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
    for h in &this_run.git_hooks {
        if !existing.git_hooks.contains(h) {
            existing.git_hooks.push(h.clone());
        }
    }
    for l in &this_run.gitignore {
        if !existing.gitignore.contains(l) {
            existing.gitignore.push(l.clone());
        }
    }
    existing.codex |= this_run.codex;
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
/// `shell` is the already-quoted command prefix from [`McpCommand::shell`];
/// the database path is quoted here, since a path with a space in it would
/// otherwise be word-split into two arguments.
#[must_use]
pub fn git_hook_block(shell: &str, db: &str) -> String {
    format!(
        "{HOOK_BEGIN}\n( {shell} sync {} >/dev/null 2>&1 & )\n{HOOK_END}\n",
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
pub fn merge_git_hook(hook_file: &Path, shell: &str, db: &str) -> Result<bool, CliError> {
    let existing = if hook_file.exists() {
        Some(
            fs::read_to_string(hook_file)
                .map_err(|e| CliError(format!("cannot read {}: {e}", hook_file.display())))?,
        )
    } else {
        None
    };
    let next = merged_hook_text(existing.as_deref(), &git_hook_block(shell, db))
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
