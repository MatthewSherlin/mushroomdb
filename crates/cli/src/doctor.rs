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
    claude_mcp_file, cursor_mcp_file, default_db, entry_db, expand_platform, git_hooks_dir,
    has_our_server, installed_shape, intercept_installed, is_disabled, is_our_hook_command,
    line_runs_for_store, resolve_platform, resolve_scope, Externals, Platform, Scope, StoreRef,
    AUTO_ARG, BRIEF_EVENT, GIT_HOOKS, HOOK_BEGIN, HOOK_EVENT, INTERCEPT_EVENT, TOUCH_EVENT,
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
    /// The check does not apply to this install — not a finding either way.
    Skip,
    Warn,
    Fail,
}

impl Status {
    fn word(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Skip => "skip",
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
    fn skip(name: &'static str, message: impl Into<String>) -> Self {
        Check {
            status: Status::Skip,
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

    // A disabled install has no config to check — every check below it would
    // report exactly what `disable` intentionally removed, which is not a
    // failure. Report the state and stop; `warn` never sets the exit code.
    if is_disabled(project_root, home, scope, &platforms) {
        return Ok(DoctorReport {
            output: Check::warn("state", "disabled — enable with: mushroomdb enable", None)
                .render(),
            had_fail: false,
        });
    }

    // A `cli` install registers no MCP server on purpose, so the two checks
    // that read one have nothing to read and no fault to report; the store it
    // is wired to comes from the hooks it did write. Everything else — the
    // store, the hooks, the git hooks — is as breakable here as anywhere and
    // still runs.
    let (delivery, recorded_store) = installed_shape(project_root, home, scope, &platforms);
    let mcp_delivery = delivery.wires_mcp();

    let mut checks: Vec<Check> = Vec::new();
    let mut primary: Option<(Platform, ConfigEntry)> = None;

    // 1. config — one line per requested platform; the first entry that reads
    //    cleanly becomes the target of every check below it.
    for plat in &platforms {
        // A `cli` delivery closes Claude Code's door and no other's: Cursor
        // and Codex are registered as servers whatever it asked for, so their
        // entries are still there to check (see [`install::Delivery`]).
        if !mcp_delivery && matches!(plat, Platform::ClaudeCode) {
            checks.push(Check::skip(
                "config",
                format!(
                    "delivery: {} — the skill teaches the binary, no MCP entry to check",
                    delivery.label()
                ),
            ));
            continue;
        }
        match mcp_file_for(plat, project_root, home, scope) {
            None => checks.push(Check::warn(
                "config",
                format!(
                    "{}'s configuration is owned by its own CLI — not checked here",
                    plat.label()
                ),
                None,
            )),
            Some(mcp_file) => match read_config_entry(&mcp_file, project_root, home) {
                Ok(entry) => {
                    checks.push(Check::ok(
                        "config",
                        format!(
                            "{} — {} -> {}",
                            plat.label(),
                            mcp_file.display(),
                            entry.describe_store()
                        ),
                    ));
                    if primary.is_none() {
                        primary = Some((plat.clone(), entry));
                    }
                }
                Err(msg) => checks.push(Check::fail("config", msg, Some(install_fix(scope, plat)))),
            },
        }
    }

    // The store every check below reads: the one the config entry names, or —
    // with no entry to name it — the one this install's hooks were written for.
    let store: Option<StoreRef> = match &primary {
        Some((_, entry)) => Some(entry.store.clone()),
        None if !mcp_delivery => recorded_store,
        None => None,
    };

    // 2. how the server is spawned — `npx` fetches the package, a resolved
    //    binary or launcher is a file that has to still be there.
    if let Some((_, entry)) = &primary {
        if entry.command == "npx" {
            checks.push(check_npx(entry, ext));
        } else if let Some(check) = check_resolved_path(entry) {
            checks.push(check);
        }
    }

    // 3. store, then the write-lock probe (same check family, adjacent lines).
    match &store {
        Some(store) => checks.extend(check_store_and_lock(store.path())),
        None => checks.push(Check::fail(
            "store",
            no_store_message(mcp_delivery),
            Some(install_fix_for_scope(scope)),
        )),
    }

    // 4. hooks — Claude Code only; Cursor has no prompt/tool-use hooks to check.
    if platforms.contains(&Platform::ClaudeCode) {
        if let Some(store) = &store {
            checks.push(check_hooks(project_root, home, scope, store));
            // The fourth hook is opt-in, so it earns a line only where the
            // manifest says this install asked for it. Reporting it otherwise
            // would say something about every install that is true of none.
            if intercept_installed(project_root, home, scope, &platforms) {
                checks.push(check_intercept(project_root, home, scope, store));
            }
        }
    }

    // 5. git hooks — project scope, and only for the platforms whose install
    //    wires the repository (matches `install::write_everything`).
    if scope == Scope::Project
        && platforms
            .iter()
            .any(|p| matches!(p, Platform::ClaudeCode | Platform::Cursor))
    {
        if let Some(store) = &store {
            if let Some(check) = check_git_hooks(project_root, store) {
                checks.push(check);
            }
        }
    }

    // 6. self-handshake — spawn the configured command for real.
    match &primary {
        Some((_, entry)) => checks.push(check_handshake(entry)),
        // No entry and none expected: nothing to spawn, and nothing wrong.
        None if !mcp_delivery => checks.push(Check::skip(
            "handshake",
            format!("delivery: {} — no server to spawn", delivery.label()),
        )),
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

/// Why doctor could not find a store to check, in the terms of whichever
/// artifact was supposed to name one.
fn no_store_message(mcp_delivery: bool) -> &'static str {
    if mcp_delivery {
        "no usable config entry — cannot locate a database to check"
    } else {
        "no recorded SessionStart hook — cannot locate a database to check"
    }
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
    /// The store the entry names, as written: a path, or `--auto`.
    store: StoreRef,
    command: String,
    args: Vec<String>,
}

impl ConfigEntry {
    /// How the config line reports the store. An `--auto` entry says what it
    /// resolves to as well, since that is the directory every check below it
    /// reads and the one the reader wants to see.
    fn describe_store(&self) -> String {
        if self.store.is_auto() {
            format!("{AUTO_ARG} -> {}", self.store.path().display())
        } else {
            self.store.path().display().to_string()
        }
    }
}

/// The store an entry's argument names, resolved the way the command it was
/// written for would resolve it.
///
/// `--auto` is resolved from `project_root` rather than doctor's own working
/// directory: doctor was already told which project it is checking, and a
/// report that changes depending on which subdirectory it was typed in would
/// be a worse report. That is the same directory a hook resolves to, since a
/// hook walks up to the working tree root from wherever it fired.
fn entry_store(arg: &str, project_root: &Path, home: &Path) -> StoreRef {
    if arg == AUTO_ARG {
        return StoreRef::auto(crate::resolve_auto_db(None, project_root, home));
    }
    let path = PathBuf::from(arg);
    if path == default_db(Scope::Project, project_root, home) {
        return StoreRef::pinned(path).also_auto();
    }
    StoreRef::pinned(path)
}

fn read_json(path: &Path) -> Result<Js, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    serde_json::from_str(&raw).map_err(|e| format!("invalid JSON in {}: {e}", path.display()))
}

fn read_config_entry(
    mcp_file: &Path,
    project_root: &Path,
    home: &Path,
) -> Result<ConfigEntry, String> {
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
    let store = entry_store(
        entry_db(entry).ok_or_else(|| {
            format!(
                "{}: mushroomdb entry has no `mcp <db>|--auto` argument",
                mcp_file.display()
            )
        })?,
        project_root,
        home,
    );
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
    Ok(ConfigEntry {
        store,
        command,
        args,
    })
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

/// The resolved-path counterpart to [`check_npx`].
///
/// `install` writes a path rather than `npx` so the hooks do not spawn `npx`
/// on every prompt: the package's native binary, or `node <launcher.js>` when
/// the binary was not fetched. Both live in npm's cache, and pruning it — or
/// npx evicting an old version — leaves a command that cannot run. The file
/// existing is the whole check; what it does once it runs is the handshake's
/// business.
///
/// A bare `command` is a PATH lookup, not a file, and is left to the handshake.
fn check_resolved_path(entry: &ConfigEntry) -> Option<Check> {
    let (name, path) = if entry.command == "node" {
        ("launcher", Path::new(entry.args.first()?))
    } else if Path::new(&entry.command).is_absolute() {
        ("binary", Path::new(&entry.command))
    } else {
        return None;
    };
    Some(if path.is_file() {
        Check::ok(name, path.display().to_string())
    } else {
        Check::fail(
            name,
            format!("{} no longer exists", path.display()),
            Some("mushroomdb install (re-resolves it)".to_string()),
        )
    })
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

fn check_hooks(project_root: &Path, home: &Path, scope: Scope, store: &StoreRef) -> Check {
    let settings_file = match scope {
        Scope::Project => project_root.join(".claude").join("settings.json"),
        Scope::User => home.join(".claude").join("settings.json"),
    };
    let root = read_json(&settings_file).unwrap_or(Js::Null);
    let has_recall = has_hook_matching(&root, HOOK_EVENT, "recall", store);
    let has_touch = has_hook_matching(&root, TOUCH_EVENT, "touch", store);
    let has_brief = has_hook_matching(&root, BRIEF_EVENT, "brief", store);
    if has_recall && has_touch && has_brief {
        Check::ok(
            "hooks",
            format!(
                "{HOOK_EVENT} + {TOUCH_EVENT} + {BRIEF_EVENT} present in {}",
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
        if !has_brief {
            missing.push(BRIEF_EVENT);
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

/// The experimental grep redirect, for an install whose manifest asked for it.
fn check_intercept(project_root: &Path, home: &Path, scope: Scope, store: &StoreRef) -> Check {
    let settings_file = match scope {
        Scope::Project => project_root.join(".claude").join("settings.json"),
        Scope::User => home.join(".claude").join("settings.json"),
    };
    let root = read_json(&settings_file).unwrap_or(Js::Null);
    if has_hook_matching(&root, INTERCEPT_EVENT, "intercept", store) {
        Check::ok(
            "intercept",
            format!(
                "{INTERCEPT_EVENT} (Grep) present in {}",
                settings_file.display()
            ),
        )
    } else {
        Check::warn(
            "intercept",
            format!("missing {INTERCEPT_EVENT} in {}", settings_file.display()),
            Some("mushroomdb install --platform claude-code --intercept-grep".to_string()),
        )
    }
}

fn has_hook_matching(root: &Js, event: &str, sub: &str, store: &StoreRef) -> bool {
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
                                .is_some_and(|c| is_our_hook_command(c, sub, store))
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

fn check_git_hooks(project_root: &Path, store: &StoreRef) -> Option<Check> {
    let dir = git_hooks_dir(project_root)?;
    // The block belongs to this store if it names it either way. An `--auto`
    // block in a checkout whose store is the default is the same store, and
    // that is the shape every project install now writes.
    let missing: Vec<&str> = GIT_HOOKS
        .iter()
        .filter(|name| {
            let content = std::fs::read_to_string(dir.join(name)).unwrap_or_default();
            let names_store = content
                .lines()
                .any(|l| line_runs_for_store(l, "sync", store));
            !(content.contains(HOOK_BEGIN) && names_store)
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
    /// The repository task tool the listing carried — see [`TASK_PATH_TOOLS`].
    task_tool: String,
}

fn check_handshake(entry: &ConfigEntry) -> Check {
    match self_handshake(&entry.command, &entry.args) {
        Ok(HandshakeOk {
            version,
            tool_count,
            task_tool,
        }) => Check::ok(
            "handshake",
            format!(
                "initialize + tools/list ok — version {version}, {tool_count} tools \
                 ({task_tool} present)"
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
    let task_tool = tools
        .iter()
        .filter_map(|t| t["name"].as_str())
        .find(|name| TASK_PATH_TOOLS.contains(name))
        .ok_or_else(|| {
            format!(
                "`{command}`: tools/list includes none of {}",
                TASK_PATH_TOOLS.join(", ")
            )
        })?
        .to_string();

    Ok(HandshakeOk {
        version,
        tool_count: tools.len(),
        task_tool,
    })
}

/// The tool names that prove the repository task path is served rather than the
/// graph API alone.
///
/// Either is enough, because which one is listed follows the store: a store
/// `ingest-git` built advertises `explore` and hides the rest, and any other
/// store advertises `map` among its eleven. Requiring one particular name would
/// fail `doctor` on exactly the stores the other surface exists for.
const TASK_PATH_TOOLS: [&str; 2] = ["explore", "map"];
