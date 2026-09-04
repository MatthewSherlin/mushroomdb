//! `mushroomdb doctor` — verify an install end to end: the config entry, the
//! store, the hooks, and a real stdio handshake with the configured MCP
//! command.
//!
//! # Design
//!
//! Every check reads what is actually on disk — the same files `install`
//! wrote — rather than recomputing what an install *should* look like, so
//! doctor catches drift (a hand-edited config, a stale command) that a
//! `run_install_with` re-derivation would paper over. Checks run in a fixed
//! order and each prints exactly one line: `ok|warn|fail  <name>  <message>`,
//! with a trailing `fix: …` when there is something to run. The same store
//! state always produces the same output.
//!
//! `fail` on any check is the only thing that sets the process exit code;
//! `warn` is informational.

use crate::install::{
    claude_mcp_file, cursor_mcp_file, entry_db, expand_platform, git_hooks_dir, has_our_server,
    is_our_hook_command, resolve_platform, resolve_scope, Externals, Platform, Scope, GIT_HOOKS,
    HOOK_BEGIN, HOOK_EVENT, TOUCH_EVENT,
};
use crate::CliError;
use core_api::{GraphDb, GraphError, OpenOptions};
use serde_json::Value as Js;
use std::io::{BufRead, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Options parsed from `mushroomdb doctor [flags]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorOpts {
    /// Which platform's config to check. `None` = auto-detect, same as `install`.
    pub platform: Option<Platform>,
    /// Project or user scope. `None` = auto: project inside a git checkout.
    pub scope: Option<Scope>,
}

/// Outcome of `mushroomdb doctor`: the rendered report and whether to exit 1.
pub struct DoctorReport {
    /// One line per check, already newline-terminated.
    pub output: String,
    /// True when any check is `fail`. The caller exits 1 on this and only this.
    pub had_fail: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    fn word(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Fail => "fail",
        }
    }
}

/// One printed line: a status, the check's name, a message, and an optional
/// one-line fix.
struct Check {
    status: Status,
    name: &'static str,
    message: String,
    fix: Option<String>,
}

impl Check {
    fn ok(name: &'static str, message: impl Into<String>) -> Self {
        Check {
            status: Status::Ok,
            name,
            message: message.into(),
            fix: None,
        }
    }
    fn warn(name: &'static str, message: impl Into<String>, fix: Option<String>) -> Self {
        Check {
            status: Status::Warn,
            name,
            message: message.into(),
            fix,
        }
    }
    fn fail(name: &'static str, message: impl Into<String>, fix: Option<String>) -> Self {
        Check {
            status: Status::Fail,
            name,
            message: message.into(),
            fix,
        }
    }
    fn render(&self) -> String {
        let mut line = format!(
            "{:<4} {:<9} {}",
            self.status.word(),
            self.name,
            self.message
        );
        if let Some(fix) = &self.fix {
            line.push_str(&format!("  fix: {fix}"));
        }
        line.push('\n');
        line
    }
}

/// Run `doctor` against the real environment: `HOME` and the process PATH.
pub fn run_doctor(
    project_root: &Path,
    home: &Path,
    opts: &DoctorOpts,
) -> Result<DoctorReport, CliError> {
    run_doctor_with(project_root, home, opts, &Externals::from_env())
}

/// Like [`run_doctor`], with the external environment (PATH) supplied by the
/// caller. Tests use this to stay deterministic and to point `npx` lookups at
/// a directory of stand-ins.
pub fn run_doctor_with(
    project_root: &Path,
    home: &Path,
    opts: &DoctorOpts,
    ext: &Externals,
) -> Result<DoctorReport, CliError> {
    let (scope, _auto_scope) = resolve_scope(project_root, opts.scope);
    let resolved = resolve_platform(project_root, home, opts.platform.as_ref())?;
    let platforms = expand_platform(&resolved);

    let mut checks: Vec<Check> = Vec::new();
    let mut primary: Option<(Platform, ConfigEntry)> = None;

    // 1. config — one line per requested platform; the first entry that reads
    //    cleanly becomes the target of every check below it.
    for plat in &platforms {
        match mcp_file_for(plat, project_root, home, scope) {
            None => checks.push(Check::warn(
                "config",
                format!(
                    "{}'s configuration is owned by its own CLI — not checked here",
                    plat.label()
                ),
                None,
            )),
            Some(mcp_file) => match read_config_entry(&mcp_file) {
                Ok(entry) => {
                    checks.push(Check::ok(
                        "config",
                        format!("{} — {} -> {}", plat.label(), mcp_file.display(), entry.db),
                    ));
                    if primary.is_none() {
                        primary = Some((plat.clone(), entry));
                    }
                }
                Err(msg) => checks.push(Check::fail("config", msg, Some(install_fix(scope, plat)))),
            },
        }
    }

    // 2. npx — only when the resolved command actually is npx.
    if let Some((_, entry)) = &primary {
        if entry.command == "npx" {
            checks.push(check_npx(entry, ext));
        }
    }

    // 3. store, then the write-lock probe (same check family, adjacent lines).
    match &primary {
        Some((_, entry)) => checks.extend(check_store_and_lock(Path::new(&entry.db))),
        None => checks.push(Check::fail(
            "store",
            "no usable config entry — cannot locate a database to check",
            Some(install_fix_for_scope(scope)),
        )),
    }

    // 4. hooks — Claude Code only; Cursor has no prompt/tool-use hooks to check.
    if platforms.contains(&Platform::ClaudeCode) {
        if let Some((_, entry)) = &primary {
            checks.push(check_hooks(project_root, home, scope, &entry.db));
        }
    }

    // 5. git hooks — project scope, and only for the platforms whose install
    //    wires the repository (matches `install::write_everything`).
    if scope == Scope::Project
        && platforms
            .iter()
            .any(|p| matches!(p, Platform::ClaudeCode | Platform::Cursor))
    {
        if let Some((_, entry)) = &primary {
            if let Some(check) = check_git_hooks(project_root, &entry.db) {
                checks.push(check);
            }
        }
    }

    // 6. self-handshake — spawn the configured command for real.
    match &primary {
        Some((_, entry)) => checks.push(check_handshake(entry)),
        None => checks.push(Check::fail(
            "handshake",
            "no usable config entry — nothing to spawn",
            Some(install_fix_for_scope(scope)),
        )),
    }

    // 7. duplicate scope — a second Claude Code server in the other scope.
    if platforms.contains(&Platform::ClaudeCode) {
        checks.push(check_scope_conflict(project_root, home, scope));
    }

    let had_fail = checks.iter().any(|c| c.status == Status::Fail);
    let mut output = String::new();
    for c in &checks {
        output.push_str(&c.render());
    }
    Ok(DoctorReport { output, had_fail })
}

fn install_fix_for_scope(scope: Scope) -> String {
    format!(
        "mushroomdb install {}",
        match scope {
            Scope::Project => "--project",
            Scope::User => "--user",
        }
    )
}

fn install_fix(scope: Scope, plat: &Platform) -> String {
    format!(
        "mushroomdb install --platform {} {}",
        plat.label(),
        match scope {
            Scope::Project => "--project",
            Scope::User => "--user",
        }
    )
}

fn mcp_file_for(
    plat: &Platform,
    project_root: &Path,
    home: &Path,
    scope: Scope,
) -> Option<PathBuf> {
    match plat {
        Platform::ClaudeCode => Some(claude_mcp_file(project_root, home, scope)),
        Platform::Cursor => Some(cursor_mcp_file(project_root, home, scope)),
        Platform::Codex | Platform::All => None,
    }
}

// ---------------------------------------------------------------------------
// config
// ---------------------------------------------------------------------------

/// The MCP server entry doctor found: what it points at and how it is spawned.
struct ConfigEntry {
    db: String,
    command: String,
    args: Vec<String>,
}

fn read_json(path: &Path) -> Result<Js, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    serde_json::from_str(&raw).map_err(|e| format!("invalid JSON in {}: {e}", path.display()))
}

fn read_config_entry(mcp_file: &Path) -> Result<ConfigEntry, String> {
    if !mcp_file.exists() {
        return Err(format!("{} does not exist", mcp_file.display()));
    }
    let root = read_json(mcp_file)?;
    let entry = &root["mcpServers"]["mushroomdb"];
    if entry.is_null() {
        return Err(format!(
            "no mcpServers.mushroomdb entry in {}",
            mcp_file.display()
        ));
    }
    let db = entry_db(entry)
        .ok_or_else(|| {
            format!(
                "{}: mushroomdb entry has no `mcp <db>` argument",
                mcp_file.display()
            )
        })?
        .to_string();
    let command = entry["command"]
        .as_str()
        .ok_or_else(|| format!("{}: mushroomdb entry has no `command`", mcp_file.display()))?
        .to_string();
    let args = entry["args"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Ok(ConfigEntry { db, command, args })
}

// ---------------------------------------------------------------------------
// npx
// ---------------------------------------------------------------------------

const NPX_TIMEOUT: Duration = Duration::from_secs(60);

fn check_npx(entry: &ConfigEntry, ext: &Externals) -> Check {
    let pinned = entry
        .args
        .iter()
        .find_map(|a| a.strip_prefix("mushroomdb@"))
        .unwrap_or(crate::VERSION);
    let Some(npx) = ext.which("npx") else {
        return Check::fail(
            "npx",
            "npx is not on PATH",
            Some(
                "install Node.js (which provides npx), or re-install with --command <path>"
                    .to_string(),
            ),
        );
    };
    let args = vec![
        "-y".to_string(),
        format!("mushroomdb@{pinned}"),
        "--version".to_string(),
    ];
    match run_capturing(&npx, &args, NPX_TIMEOUT) {
        RunOutcome::Done(out) if out.contains(pinned) => Check::ok(
            "npx",
            format!("npx -y mushroomdb@{pinned} --version -> {}", out.trim()),
        ),
        RunOutcome::Done(out) => Check::fail(
            "npx",
            format!(
                "npx -y mushroomdb@{pinned} --version printed {:?}, expected to contain {pinned}",
                out.trim()
            ),
            Some("re-run `mushroomdb install` to repin the version".to_string()),
        ),
        RunOutcome::TimedOut => Check::warn(
            "npx",
            format!("npx -y mushroomdb@{pinned} --version timed out after {NPX_TIMEOUT:?}"),
            Some("check network access to the npm registry".to_string()),
        ),
        RunOutcome::Failed(e) => Check::fail("npx", e, None),
    }
}

enum RunOutcome {
    Done(String),
    TimedOut,
    Failed(String),
}

/// Run `bin`, capturing stdout, giving up after `timeout`.
fn run_capturing(bin: &Path, args: &[String], timeout: Duration) -> RunOutcome {
    let mut child = match Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return RunOutcome::Failed(format!("cannot run {}: {e}", bin.display())),
    };
    let mut stdout = child.stdout.take().expect("piped stdout");
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut out = String::new();
        let _ = stdout.read_to_string(&mut out);
        let _ = tx.send(out);
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let out = rx.recv_timeout(Duration::from_secs(1)).unwrap_or_default();
                return if status.success() {
                    RunOutcome::Done(out)
                } else {
                    RunOutcome::Failed(format!("{} exited with {status}", bin.display()))
                };
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return RunOutcome::TimedOut;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(e) => return RunOutcome::Failed(format!("cannot wait for {}: {e}", bin.display())),
        }
    }
}

// ---------------------------------------------------------------------------
// store + lock
// ---------------------------------------------------------------------------

fn check_store_and_lock(db_dir: &Path) -> Vec<Check> {
    let mut out = Vec::new();
    let store = GraphDb::open_with_options(
        db_dir,
        OpenOptions {
            read_only: true,
            auto_migrate: true,
            repair_wal: true,
        },
    );
    match store {
        Ok(db) => {
            let stats = db.stats();
            let stale = db.is_stale().unwrap_or(false);
            out.push(Check::ok(
                "store",
                format!(
                    "{} — {} nodes live ({} tombstoned), {} edges{}",
                    db_dir.display(),
                    stats.nodes_live,
                    stats.nodes_tombstoned,
                    stats.edges,
                    if stale {
                        ", stale (newer commits pending refresh)"
                    } else {
                        ""
                    }
                ),
            ));
            drop(db);

            // Briefly try to take the write lock. Success means nobody else
            // holds it; the handle is dropped immediately, before this
            // function returns, so the lock is never held past the check.
            match GraphDb::open_with_options(db_dir, OpenOptions::default()) {
                Ok(handle) => {
                    drop(handle);
                    out.push(Check::ok(
                        "lock",
                        "free — no other process is writing".to_string(),
                    ));
                }
                Err(GraphError::Busy { .. }) => out.push(Check::warn(
                    "lock",
                    "another process is writing".to_string(),
                    Some("re-run once the other process finishes".to_string()),
                )),
                Err(e) => out.push(Check::warn("lock", format!("could not verify: {e}"), None)),
            }
        }
        Err(e) => out.push(Check::fail(
            "store",
            format!("cannot open {}: {e}", db_dir.display()),
            Some(format!("mushroomdb verify {}", db_dir.display())),
        )),
    }
    out
}

// ---------------------------------------------------------------------------
// hooks
// ---------------------------------------------------------------------------

fn check_hooks(project_root: &Path, home: &Path, scope: Scope, db_str: &str) -> Check {
    let settings_file = match scope {
        Scope::Project => project_root.join(".claude").join("settings.json"),
        Scope::User => home.join(".claude").join("settings.json"),
    };
    let root = read_json(&settings_file).unwrap_or(Js::Null);
    let has_recall = has_hook_matching(&root, HOOK_EVENT, "recall", db_str);
    let has_touch = has_hook_matching(&root, TOUCH_EVENT, "touch", db_str);
    if has_recall && has_touch {
        Check::ok(
            "hooks",
            format!(
                "{HOOK_EVENT} + {TOUCH_EVENT} present in {}",
                settings_file.display()
            ),
        )
    } else {
        let mut missing = Vec::new();
        if !has_recall {
            missing.push(HOOK_EVENT);
        }
        if !has_touch {
            missing.push(TOUCH_EVENT);
        }
        Check::warn(
            "hooks",
            format!(
                "missing {} in {}",
                missing.join(", "),
                settings_file.display()
            ),
            Some("mushroomdb install --platform claude-code".to_string()),
        )
    }
}

fn has_hook_matching(root: &Js, event: &str, sub: &str, db_str: &str) -> bool {
    root["hooks"][event]
        .as_array()
        .map(|groups| {
            groups.iter().any(|g| {
                g["hooks"]
                    .as_array()
                    .map(|hs| {
                        hs.iter().any(|h| {
                            h["command"]
                                .as_str()
                                .is_some_and(|c| is_our_hook_command(c, sub, db_str))
                        })
                    })
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// git hooks
// ---------------------------------------------------------------------------

fn check_git_hooks(project_root: &Path, db_str: &str) -> Option<Check> {
    let dir = git_hooks_dir(project_root)?;
    let missing: Vec<&str> = GIT_HOOKS
        .iter()
        .filter(|name| {
            let content = std::fs::read_to_string(dir.join(name)).unwrap_or_default();
            !(content.contains(HOOK_BEGIN) && content.contains(db_str))
        })
        .copied()
        .collect();
    Some(if missing.is_empty() {
        Check::ok(
            "git-hooks",
            format!("{} present in {}", GIT_HOOKS.join("/"), dir.display()),
        )
    } else {
        Check::warn(
            "git-hooks",
            format!("missing in {}: {}", dir.display(), missing.join(", ")),
            Some("mushroomdb install --project (omit --no-git-hooks)".to_string()),
        )
    })
}

// ---------------------------------------------------------------------------
// duplicate scope
// ---------------------------------------------------------------------------

fn check_scope_conflict(project_root: &Path, home: &Path, scope: Scope) -> Check {
    let (other_file, other_label, other_flag) = match scope {
        Scope::Project => (
            claude_mcp_file(project_root, home, Scope::User),
            "user",
            "--user",
        ),
        Scope::User => (
            claude_mcp_file(project_root, home, Scope::Project),
            "project",
            "--project",
        ),
    };
    if has_our_server(&other_file) {
        Check::warn(
            "scope",
            format!(
                "a {other_label}-scope mushroomdb server also exists ({}) — both will load",
                other_file.display()
            ),
            Some(format!("mushroomdb uninstall {other_flag}")),
        )
    } else {
        Check::ok(
            "scope",
            "no duplicate server in the other scope".to_string(),
        )
    }
}

// ---------------------------------------------------------------------------
// self-handshake
// ---------------------------------------------------------------------------

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

struct HandshakeOk {
    version: String,
    tool_count: usize,
}

fn check_handshake(entry: &ConfigEntry) -> Check {
    match self_handshake(&entry.command, &entry.args) {
        Ok(HandshakeOk {
            version,
            tool_count,
        }) => Check::ok(
            "handshake",
            format!(
                "initialize + tools/list ok — version {version}, {tool_count} tools (map present)"
            ),
        ),
        Err(msg) => Check::fail(
            "handshake",
            msg,
            Some(format!(
                "verify `{} {}` runs mushroomdb's MCP server, or re-run `mushroomdb install` \
                 to rewrite the command",
                entry.command,
                entry.args.join(" ")
            )),
        ),
    }
}

/// Spawn `command args…`, speak one `initialize` and one `tools/list` request
/// over its stdio, and check the response.
///
/// Reads with a 10s deadline; closes stdin once both responses are in (or the
/// deadline passes) so a well-behaved server exits on EOF, then reaps it with
/// a short bounded wait — a broken server never hangs `doctor`.
fn self_handshake(command: &str, args: &[String]) -> Result<HandshakeOk, String> {
    let mut child = Command::new(command)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot spawn `{command}`: {e}"))?;

    let mut stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");

    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if tx.send(line.trim().to_string()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let sent = writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2024-11-05","capabilities":{{}},"clientInfo":{{"name":"mushroomdb-doctor","version":"1"}}}}}}"#
    )
    .and_then(|()| writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"tools/list"}}"#))
    .and_then(|()| stdin.flush());

    let mut init_resp: Option<Js> = None;
    let mut list_resp: Option<Js> = None;
    if sent.is_ok() {
        let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        while (init_resp.is_none() || list_resp.is_none()) && Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(remaining.min(Duration::from_millis(50))) {
                Ok(line) if !line.is_empty() => {
                    if let Ok(v) = serde_json::from_str::<Js>(&line) {
                        match v.get("id").and_then(Js::as_i64) {
                            Some(1) => init_resp = Some(v),
                            Some(2) => list_resp = Some(v),
                            _ => {}
                        }
                    }
                }
                Ok(_) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    }

    // EOF on stdin is how a well-behaved server knows to exit; then reap it
    // with a short bounded wait so a broken one cannot hang doctor.
    drop(stdin);
    let reap_deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => break,
            Ok(None) if Instant::now() >= reap_deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
        }
    }

    if sent.is_err() {
        return Err(format!("cannot write to `{command}`'s stdin"));
    }
    let init = init_resp.ok_or_else(|| {
        format!("`{command}` did not answer `initialize` within {HANDSHAKE_TIMEOUT:?}")
    })?;
    let list = list_resp.ok_or_else(|| {
        format!("`{command}` did not answer `tools/list` within {HANDSHAKE_TIMEOUT:?}")
    })?;

    let version = init["result"]["serverInfo"]["version"]
        .as_str()
        .ok_or_else(|| format!("`{command}`: initialize response has no serverInfo.version"))?
        .to_string();
    if version != crate::VERSION {
        return Err(format!(
            "`{command}` reports version {version}, expected {}",
            crate::VERSION
        ));
    }
    let tools = list["result"]["tools"]
        .as_array()
        .ok_or_else(|| format!("`{command}`: tools/list response has no tools array"))?;
    if !tools.iter().any(|t| t["name"].as_str() == Some("map")) {
        return Err(format!("`{command}`: tools/list does not include `map`"));
    }

    Ok(HandshakeOk {
        version,
        tool_count: tools.len(),
    })
}
