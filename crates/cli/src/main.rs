//! `mushroomdb` — thin dispatcher over [`cli`] lib functions.

use cli::{
    format_backup, format_demo, format_stats, format_suggest, install, maybe_run_demo_if_empty,
    parse_args, read_stats, resolve_statement, run_algo, run_asof, run_backup, run_build_index,
    run_demo, run_export, run_import, run_migrate, run_query, run_schema_apply,
    run_schema_apply_memory_defaults, run_schema_apply_memory_identity, run_snapshot, run_suggest,
    run_verify, usage, Command, ServeUi,
};
use mushroomdb::{GraphError, SharedDb};
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{self, IsTerminal as _, Read as _, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// The default log filter.
///
/// The targets are the **lib names** — `server` and `cli` — not the package
/// names. Filtering on `mushroomdb_server` matches nothing at all, and a filter
/// that matches nothing is indistinguishable from a server with nothing to say;
/// `default_filter_enables_the_crates_that_log` exists because that is exactly
/// the mistake this line shipped with for an afternoon.
const DEFAULT_LOG_FILTER: &str = "mushroomdb=info,server=info,bolt=info,cli=info,warn";

/// Install the log subscriber for a long-running `serve`.
///
/// **Default: `mushroomdb=info,warn`.** A healthy server is quiet — one line
/// when it binds, one when it stops, and then nothing until something is
/// actually wrong. Successful requests log at `debug` and are therefore off;
/// failures, refusals and slow requests are on. This is the difference between
/// a log someone reads and a log that fills a disk.
///
/// `MUSHROOMDB_LOG` overrides it with the usual filter syntax, so
/// `MUSHROOMDB_LOG=mushroomdb_server=debug` gives a line per request when
/// someone is actually debugging, with no rebuild.
///
/// Only `serve` installs this. The one-shot commands print their results to
/// stdout and have nothing to log.
fn init_logging() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_env("MUSHROOMDB_LOG")
        .unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_FILTER));
    let format = match parse_log_format(std::env::var("MUSHROOMDB_LOG_FORMAT").ok().as_deref()) {
        Ok(format) => format,
        Err(message) => {
            // A hosted deployment that typo'd the format must not silently ship
            // text to a collector expecting JSON: refuse to start.
            eprintln!("error: {message}");
            std::process::exit(2);
        }
    };
    // `try_init` rather than `init`: a second call must not panic the server,
    // and an embedder that already installed a subscriber keeps theirs.
    let builder = fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr);
    let _ = match format {
        LogFormat::Text => builder.try_init(),
        LogFormat::Json => builder.json().try_init(),
    };
}

/// Log output format, chosen by `MUSHROOMDB_LOG_FORMAT`.
#[derive(Debug, PartialEq, Eq)]
enum LogFormat {
    Text,
    Json,
}

/// `unset`/empty/`text` -> text (today's output); `json` -> newline-delimited
/// JSON; anything else is an error naming the accepted values.
fn parse_log_format(value: Option<&str>) -> Result<LogFormat, String> {
    match value.map(str::trim) {
        None | Some("") => Ok(LogFormat::Text),
        Some(v) if v.eq_ignore_ascii_case("text") => Ok(LogFormat::Text),
        Some(v) if v.eq_ignore_ascii_case("json") => Ok(LogFormat::Json),
        Some(other) => Err(format!(
            "MUSHROOMDB_LOG_FORMAT={other:?} is not recognised; use \"text\" (the default) or \"json\""
        )),
    }
}

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    match parse_args(&raw) {
        Ok(Command::Help) => {
            print!("{}", usage());
            ExitCode::SUCCESS
        }
        Ok(Command::RetiredHook { sub }) => {
            // Drain a hook payload so the runner's write never meets a closed
            // pipe. A terminal is not drained: nobody is going to type EOF.
            if !io::stdin().is_terminal() {
                let _ = io::copy(&mut io::stdin().lock(), &mut io::sink());
            }
            let _ = writeln!(
                io::stderr(),
                "mushroomdb {sub}: retired in 0.7; run `mushroomdb install` to remove this hook (`mushroomdb doctor` lists any that remain)"
            );
            ExitCode::SUCCESS
        }
        Ok(Command::Recall { db_dir, auto }) => {
            let mut raw = String::new();
            let _ = io::stdin().read_to_string(&mut raw);
            // `run_recall` returns a digest, never an error: it is silent on
            // every failure by contract. A *panic* is the one way it could
            // still reach the user, and it would do so before every prompt, so
            // it is caught here and the digest is simply empty.
            let digest = silently(|| cli::recall::run_recall(&resolve_db(db_dir, auto), &raw))
                .unwrap_or_default();
            // Not `print!`: that panics on EPIPE (exit 101) if the hook runner
            // closes the pipe. Every write error is swallowed instead.
            let mut stdout = io::stdout();
            let _ = stdout.write_all(digest.as_bytes());
            let _ = stdout.flush();
            ExitCode::SUCCESS // never block the prompt
        }
        Ok(Command::Brief { db_dir, auto }) => {
            // A SessionStart hook is the first thing a session sees. A store
            // that cannot be opened at all — missing, held by a writer, not a
            // store — has no brief to give, and greeting the user with that
            // error is worse than opening in silence: every failure here
            // prints nothing and exits 0. (A store that opens but is *empty*
            // does have something to say, and says it.)
            let brief = silently(|| cli::run_brief(&resolve_db(db_dir, auto)))
                .and_then(Result::ok)
                .unwrap_or_default();
            let mut stdout = io::stdout();
            let _ = stdout.write_all(brief.as_bytes());
            let _ = stdout.flush();
            ExitCode::SUCCESS
        }
        Ok(Command::Why { db_dir, a, b }) => print_or_fail(cli::run_why(&db_dir, &a, &b)),
        Ok(Command::Version) => {
            println!("{}", cli::version_string());
            ExitCode::SUCCESS
        }
        Ok(Command::IngestGit { db_dir, opts }) => {
            match cli::ingest_git::run_ingest_git(&db_dir, &opts) {
                Ok(report) => {
                    print!("{}", cli::ingest_git::format_ingest_git(&report));
                    ExitCode::SUCCESS
                }
                Err(e) => busy_aware(&e),
            }
        }
        Ok(Command::Serve {
            db_dir,
            addr,
            ui,
            demo_if_empty,
            token,
            role_tokens,
            snapshot_every,
            shutdown_timeout,
            restore_from,
            tls_cert,
            tls_key,
            bolt,
        }) => {
            // Before anything touches the store (restore, demo, open).
            if let Err(message) =
                parse_log_format(std::env::var("MUSHROOMDB_LOG_FORMAT").ok().as_deref())
            {
                eprintln!("error: {message}");
                return ExitCode::from(2);
            }
            let token = token.filter(|s| !s.is_empty()).or_else(|| {
                std::env::var("MUSHROOMDB_TOKEN")
                    .ok()
                    .filter(|s| !s.is_empty())
            });
            // The documentation placeholder is a publicly known value; a deploy
            // that forgot to replace it must not come up authenticated by it.
            if token.as_deref() == Some(PLACEHOLDER_TOKEN) {
                return fail(
                    "the token is the documentation placeholder `<set-me>`; \
                     set --token or MUSHROOMDB_TOKEN to a real secret",
                );
            }
            if !addr.ip().is_loopback() && token.as_ref().map(|s| s.is_empty()).unwrap_or(true) {
                // An admin key in keys.json is authentication too. Read and
                // write keys are not: they leave no way to administer the server.
                let admin_key = match mushroomdb::AccessKeys::load(db_dir.join("keys.json")) {
                    Ok(keys) => keys.has_admin(),
                    Err(e) => return fail(&e.to_string()),
                };
                if !admin_key {
                    return fail(
                        "non-loopback --addr requires --token, MUSHROOMDB_TOKEN or an admin key \
                         in keys.json (see SECURITY.md)",
                    );
                }
            }
            // Fail before the store is opened: a bad origin list must not be
            // discovered after a long restore or open.
            if let Err(e) = server::cors_layer_from_env() {
                return fail(&e);
            }
            // The Bolt listener binds --addr's IP, so the non-loopback rule
            // above already covers it. Its own settings are checked here, for
            // the same reason as the origin list.
            let bolt = match cli::serve_bolt::resolve_bolt_settings(
                &bolt,
                &|name| std::env::var(name).ok(),
                addr,
                tls_cert.is_some(),
            ) {
                Ok(bolt) => bolt,
                Err(e) => return fail(&e),
            };
            // Merge --role-token flags with MUSHROOMDB_ROLE_TOKENS env var.
            // Format: "TOKEN1:ROLE1,TOKEN2:ROLE2". Flag entries win over env on
            // collision (flags are inserted last; HashMap retains last writer).
            let mut all_role_tokens: HashMap<String, String> = HashMap::new();
            if let Ok(env_val) = std::env::var("MUSHROOMDB_ROLE_TOKENS") {
                for pair in env_val.split(',') {
                    let pair = pair.trim();
                    if pair.is_empty() {
                        continue;
                    }
                    if let Some((tok, role)) = pair.split_once(':') {
                        if !tok.is_empty() && !role.is_empty() {
                            all_role_tokens.insert(tok.to_string(), role.to_string());
                        }
                    }
                }
            }
            for (tok, role) in role_tokens {
                all_role_tokens.insert(tok, role);
            }
            // Before the demo, so a restored store is never overwritten by it.
            if let Some(from) = restore_from {
                match cli::restore_if_empty(&db_dir, &from) {
                    Ok(cli::RestoreOutcome::Restored { from, files, bytes }) => println!(
                        "restored from {}: {} files, {} bytes",
                        from.display(),
                        files.len(),
                        bytes
                    ),
                    Ok(cli::RestoreOutcome::AlreadyPresent) => eprintln!(
                        "restore-from: {} already holds a store; not restoring",
                        db_dir.display()
                    ),
                    // A warning, not an error: a first boot with an empty
                    // backup volume must still start.
                    Ok(cli::RestoreOutcome::Empty) => {
                        eprintln!("restore-from: no backup found under {}", from.display())
                    }
                    Err(e) => return fail(&e.to_string()),
                }
            }
            if demo_if_empty {
                match maybe_run_demo_if_empty(&db_dir) {
                    Ok(Some(out)) => print!("{}", format_demo(&db_dir, &out)),
                    Ok(None) => {}
                    Err(e) => return fail(&e.to_string()),
                }
            }
            let ui = match ui {
                ServeUi::Filesystem(dir) => match cli::validate_ui_dir(&dir) {
                    Ok(dir) => ServeUi::Filesystem(dir),
                    Err(e) => return fail(&e),
                },
                other => other,
            };
            exit(run_serve(
                db_dir,
                addr,
                ui,
                token,
                all_role_tokens,
                snapshot_every,
                shutdown_timeout,
                tls_cert,
                tls_key,
                bolt,
            ))
        }
        Ok(Command::Mcp {
            db_dir,
            auto,
            all_tools,
        }) => exit(run_mcp(resolve_db(db_dir, auto), all_tools)),
        Ok(Command::Stats { db_dir }) => match read_stats(&db_dir) {
            Ok(stats) => {
                print!("{}", format_stats(&stats));
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e.to_string()),
        },
        Ok(Command::Demo { db_dir }) => match run_demo(&db_dir) {
            Ok(out) => {
                print!("{}", format_demo(&db_dir, &out));
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e.to_string()),
        },
        Ok(Command::Suggest { db_dir }) => match run_suggest(&db_dir) {
            Ok(suggestions) => {
                print!("{}", format_suggest(&suggestions));
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e.to_string()),
        },
        Ok(Command::AsOf {
            db_dir,
            commit,
            at,
            query,
            file,
            stdin,
            namespace,
        }) => match resolve_statement(
            query.as_deref(),
            file.as_deref(),
            stdin,
            &mut std::io::stdin(),
        )
        .and_then(|statement| {
            run_asof(
                &db_dir,
                commit,
                at.as_deref(),
                statement.as_deref(),
                namespace.as_deref(),
            )
        }) {
            Ok(out) => {
                print!("{out}");
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e.to_string()),
        },
        Ok(Command::Algo {
            db_dir,
            subcmd,
            top,
            dir,
            edge_types,
            weight_prop,
            min_weight,
        }) => match run_algo(
            &db_dir,
            &subcmd,
            top,
            dir,
            edge_types,
            weight_prop,
            min_weight,
        ) {
            Ok(out) => {
                print!("{out}");
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e.to_string()),
        },
        Ok(Command::Query {
            db_dir,
            cypher,
            file,
            stdin,
            role,
            namespace,
        }) => match resolve_statement(Some(&cypher), file.as_deref(), stdin, &mut std::io::stdin())
            .and_then(|statement| {
                run_query(
                    &db_dir,
                    statement.as_deref().unwrap_or_default(),
                    role.as_deref(),
                    namespace.as_deref(),
                )
            }) {
            Ok(out) => {
                print!("{out}");
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e.to_string()),
        },
        Ok(Command::Snapshot {
            db_dir,
            wal,
            retention,
        }) => match run_snapshot(&db_dir, wal, retention) {
            Ok(out) => {
                print!("{out}");
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e.to_string()),
        },
        Ok(Command::BuildIndex { db_dir, rule }) => {
            match run_build_index(&db_dir, rule.as_deref()) {
                Ok(out) => {
                    print!("{out}");
                    ExitCode::SUCCESS
                }
                Err(e) => fail(&e.to_string()),
            }
        }
        Ok(Command::SchemaApply {
            db_dir,
            schema_file,
            memory_defaults,
            memory_identity,
        }) => {
            let result = if memory_identity {
                run_schema_apply_memory_identity(&db_dir)
            } else if memory_defaults {
                run_schema_apply_memory_defaults(&db_dir)
            } else {
                match schema_file {
                    Some(f) => run_schema_apply(&db_dir, &f),
                    None => Err(cli::CliError(
                        "schema apply requires <schema.json> or --memory-defaults".to_string(),
                    )),
                }
            };
            match result {
                Ok(out) => {
                    print!("{out}");
                    ExitCode::SUCCESS
                }
                Err(e) => fail(&e.to_string()),
            }
        }
        Ok(Command::Migrate { db_dir }) => match run_migrate(&db_dir) {
            Ok(out) => {
                print!("{out}");
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e.to_string()),
        },
        Ok(Command::KeysAdd {
            db_dir,
            name,
            tier,
            role,
        }) => match run_keys_add(&db_dir, &name, &tier, role.as_deref()) {
            Ok(token) => {
                println!("{token}");
                eprintln!(
                    "key {name:?} added to {} (tier {tier}). The token above is shown once \
                     and is not stored; only its hash is. A running server loads it on SIGHUP \
                     (kill -HUP <pid>; not available on Windows, where you restart).",
                    db_dir.join("keys.json").display()
                );
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e),
        },
        Ok(Command::KeysHash) => match run_keys_hash() {
            Ok(hash) => {
                println!("{hash}");
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e),
        },
        Ok(Command::Verify { db_dir }) => match run_verify(&db_dir) {
            Ok(msg) => {
                println!("{msg}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: {}", e);
                ExitCode::from(1)
            }
        },
        Ok(Command::Backup { db_dir, dest }) => match run_backup(&db_dir, &dest) {
            Ok(report) => {
                print!("{}", format_backup(&dest, &report));
                if !report.verified {
                    eprintln!("warning: backup verification failed");
                    return ExitCode::from(1);
                }
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e.to_string()),
        },
        Ok(Command::RestoreS3 { from, at, into }) => {
            print_or_fail(cli::run_restore_s3(&from, at.as_deref(), into.as_deref()))
        }
        Ok(Command::Import {
            db_dir,
            inputs,
            opts,
        }) => match run_import(&db_dir, &inputs, &opts) {
            Ok(report) => {
                for w in &report.warnings {
                    eprintln!("warning: {w}");
                }
                for e in &report.errors {
                    eprintln!("skipped: {e}");
                }
                if report.rows_skipped > report.errors.len() {
                    eprintln!(
                        "skipped: ... and {} more",
                        report.rows_skipped - report.errors.len()
                    );
                }
                print!("{}", cli::import::format_import(&report));
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e.to_string()),
        },
        Ok(Command::Export {
            db_dir,
            dest,
            format,
        }) => match run_export(&db_dir, &dest, &format) {
            Ok(out) => {
                print!("{out}");
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e.to_string()),
        },
        Ok(Command::Install(opts)) => {
            let home = home_dir();
            let cwd = std::env::current_dir().unwrap_or_default();
            match install::run_install(&cwd, &home, &opts) {
                Ok(out) => {
                    print!("{out}");
                    ExitCode::SUCCESS
                }
                Err(e) => fail(&e.to_string()),
            }
        }
        Ok(Command::Uninstall(opts)) => {
            let home = home_dir();
            let cwd = std::env::current_dir().unwrap_or_default();
            match install::run_uninstall(&cwd, &home, &opts) {
                Ok(out) => {
                    print!("{out}");
                    ExitCode::SUCCESS
                }
                Err(e) => fail(&e.to_string()),
            }
        }
        Ok(Command::Disable(opts)) => {
            let home = home_dir();
            let cwd = std::env::current_dir().unwrap_or_default();
            match install::run_disable(&cwd, &home, &opts) {
                Ok(out) => {
                    print!("{out}");
                    ExitCode::SUCCESS
                }
                Err(e) => fail(&e.to_string()),
            }
        }
        Ok(Command::Enable(opts)) => {
            let home = home_dir();
            let cwd = std::env::current_dir().unwrap_or_default();
            match install::run_enable(&cwd, &home, &opts) {
                Ok(out) => {
                    print!("{out}");
                    ExitCode::SUCCESS
                }
                Err(e) => fail(&e.to_string()),
            }
        }
        Ok(Command::Doctor(opts)) => {
            let home = home_dir();
            let cwd = std::env::current_dir().unwrap_or_default();
            match cli::doctor::run_doctor(&cwd, &home, &opts) {
                Ok(report) => {
                    print!("{}", report.output);
                    if report.had_fail {
                        ExitCode::from(1)
                    } else {
                        ExitCode::SUCCESS
                    }
                }
                Err(e) => fail(&e.to_string()),
            }
        }
        Err(e) => {
            let _ = writeln!(io::stderr(), "{e}");
            eprint!("{}", usage());
            ExitCode::from(1)
        }
    }
}

fn exit(r: Result<(), String>) -> ExitCode {
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&e),
    }
}

/// Print a digest, or its error on stderr. The graph tools all answer the same
/// way: a string on success, one line on stderr and exit 1 otherwise.
fn print_or_fail(r: Result<String, cli::CliError>) -> ExitCode {
    match r {
        Ok(out) => {
            print!("{out}");
            ExitCode::SUCCESS
        }
        Err(e) => fail(&e.to_string()),
    }
}

/// The placeholder the deploy templates and docs show where a token goes.
const PLACEHOLDER_TOKEN: &str = "<set-me>";

fn fail(msg: &str) -> ExitCode {
    let _ = writeln!(io::stderr(), "{msg}");
    ExitCode::from(1)
}

/// Report a store-writing command's failure, distinguishing "busy" from the
/// rest. Exit 3 is "the store is busy, nothing was written" — a caller (a git
/// hook, a retry loop) can act on that without parsing the message.
fn busy_aware(e: &cli::CliError) -> ExitCode {
    let _ = writeln!(io::stderr(), "error: {e}");
    if e.0 == cli::ingest_git::BUSY_MESSAGE {
        ExitCode::from(3)
    } else {
        ExitCode::FAILURE
    }
}

/// Run `f`, returning `None` if it panicked and printing nothing either way.
///
/// The panic hook is replaced for the duration: an unwind would otherwise print
/// a message and a backtrace to stderr and exit 101, which for a hook body is
/// the noisiest possible outcome and the one a user can do least about. Both
/// hook bodies (`recall` and `brief`) go through this.
fn silently<T>(f: impl FnOnce() -> T) -> Option<T> {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    std::panic::set_hook(previous);
    outcome.ok()
}

/// The database a `<db-dir>`-or-`--auto` command should use.
fn resolve_db(db_dir: Option<PathBuf>, auto: bool) -> PathBuf {
    match db_dir {
        Some(dir) => dir,
        None => {
            debug_assert!(auto, "the parser rejects neither a dir nor --auto");
            cli::resolve_auto_db(
                std::env::var_os("CLAUDE_PROJECT_DIR").as_deref(),
                &std::env::current_dir().unwrap_or_default(),
                &home_dir(),
            )
        }
    }
}

/// A new random token: 32 bytes from the operating system's random source (any
/// platform), in hex.
fn random_token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| format!("no random source: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// `keys add`: issue a token, store only its hash, return the token to print.
fn run_keys_add(
    db_dir: &std::path::Path,
    name: &str,
    tier: &str,
    role: Option<&str>,
) -> Result<String, String> {
    let token = random_token()?;
    let hash = mushroomdb::hash_token(&token).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(db_dir).map_err(|e| format!("{}: {}", db_dir.display(), e.kind()))?;
    mushroomdb::add_key(&db_dir.join("keys.json"), name, &hash, tier, role)
        .map_err(|e| e.to_string())?;
    Ok(token)
}

/// `keys hash`: the hash of a token read from stdin (one trailing newline dropped).
fn run_keys_hash() -> Result<String, String> {
    use std::io::Read;
    let mut text = String::new();
    std::io::stdin()
        .take(4096)
        .read_to_string(&mut text)
        .map_err(|e| format!("could not read the token: {}", e.kind()))?;
    // A trailing newline, or a Windows `\r\n`, is not part of the token.
    let token = text.strip_suffix('\n').unwrap_or(&text);
    let token = token.strip_suffix('\r').unwrap_or(token);
    mushroomdb::hash_token(token).map_err(|e| e.to_string())
}

#[allow(clippy::too_many_arguments)]
fn run_serve(
    db_dir: PathBuf,
    addr: SocketAddr,
    ui: ServeUi,
    token: Option<String>,
    role_tokens: HashMap<String, String>,
    snapshot_every: Option<Duration>,
    shutdown_timeout: Duration,
    tls_cert: Option<PathBuf>,
    tls_key: Option<PathBuf>,
    bolt: Option<cli::serve_bolt::BoltSettings>,
) -> Result<(), String> {
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    let result = rt.block_on(async {
        // Same guard as `run_mcp`: checked before `open` creates the
        // directory's files, so it tells a brand-new store from one this
        // invocation is merely reopening (or one `--demo-if-empty` /
        // `--restore-from` already populated above, in which case this is
        // already `true` and nothing here re-declares anything). Declaring
        // full-text on a populated store builds the index at each open until
        // a snapshot carries it (ledger row 36), and changes what an existing
        // store indexes, so an existing store is never touched here.
        let is_new_store = !mushroomdb::restore::holds_a_store(&db_dir);
        // `keys.json` beside the store: extra tokens with tiers, re-read on
        // SIGHUP. An invalid file stops the start, before the store is opened,
        // rather than serve with the keys the operator meant to add missing.
        let keys_path = db_dir.join("keys.json");
        let access_keys = mushroomdb::AccessKeys::load(&keys_path).map_err(|e| e.to_string())?;
        // A token configured twice, here and as a key, would be ambiguous.
        if token
            .iter()
            .chain(role_tokens.keys())
            .any(|t| access_keys.resolve(t).is_some())
        {
            return Err(format!(
                "a token given by --token, MUSHROOMDB_TOKEN or a role token is also a key in {}; \
                 each token may be configured in one place",
                keys_path.display()
            ));
        }
        // Always, on Unix: SIGHUP re-reads keys.json, picks up one created after
        // the start, and with none is a logged no-op. It must never end the process.
        server::watch_sighup(&access_keys);
        // `GET /ready` answers 503 until the store is open and its schema is in.
        let ready_flag = Arc::new(AtomicBool::new(false));
        let db = SharedDb::open(&db_dir).map_err(|e| e.to_string())?;
        // A shared open takes no lock, so an unwritable data directory would
        // otherwise surface at the first write, long after the platform called
        // the deploy healthy. Try the write lock once: only a permission
        // refusal is fatal. A peer holding the lock (`Busy`) is not this
        // check's business, and serving reads alongside it stays as it was.
        match db.write_with_wait(Duration::ZERO) {
            // A read-only mount is a deliberate setup that 0.9.2 served reads
            // from, so an existing store on one still starts; its writes fail
            // with the cause. A new store has nothing to serve without a write.
            Err(GraphError::Unwritable {
                detail,
                read_only_filesystem: true,
                ..
            }) if !is_new_store => {
                eprintln!("serve: read-only store, writes will fail: {detail}");
            }
            Err(e @ GraphError::Unwritable { .. }) => return Err(e.to_string()),
            _ => {}
        }
        if is_new_store {
            // The first store most HTTP callers open. Declare the memory
            // schema before the first write so `recall` can answer on it.
            db.write()
                .apply_schema(&mushroomdb::memory_schema::memory_defaults())
                .map_err(|e| e.to_string())?;
        }
        // Continuous backup, when MUSHROOMDB_BACKUP_S3_URL asks for it. Held
        // for the life of the server: dropping the sender stops the worker.
        let (backup, plan) = start_backup(&db, &db_dir, shutdown_timeout)?;
        ready_flag.store(true, Ordering::Release);
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut snapshot_task = None;
        if let Some(period) = snapshot_every {
            let db_snap = db.clone();
            let backup_tx = backup.as_ref().map(|(tx, _, _)| tx.clone());
            snapshot_task = Some(tokio::spawn(async move {
                let mut interval = tokio::time::interval(period);
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                interval.tick().await; // skip immediate fire
                loop {
                    interval.tick().await;
                    let db_snap = db_snap.clone();
                    // A snapshot replaces wal.bin, so it needs the store's
                    // cross-process write lock. If another process holds it,
                    // skip this tick rather than wait: the next one is only a
                    // period away, and a snapshot is never urgent.
                    let taken =
                        tokio::task::spawn_blocking(move || cli::snapshot_shared(&db_snap)).await;
                    match taken {
                        Ok(Ok(())) => {
                            // Upload it now rather than at the worker's next
                            // tick.
                            if let Some(tx) = &backup_tx {
                                let _ = tx.send(backup::BackupTrigger::Snapshot);
                            }
                        }
                        Ok(Err(GraphError::Busy { .. })) => {
                            eprintln!(
                                "snapshot-every skipped: another process holds the write lock"
                            );
                        }
                        Ok(Err(e)) => eprintln!("snapshot-every failed: {e}"),
                        Err(e) => eprintln!("snapshot-every task panicked: {e}"),
                    }
                }
            }));
        }
        // A quiescent server still finishes a vector-index build it was handed:
        // one slice a second, and the rule's edges appear when it completes.
        // Cheap when nothing is pending — a map lookup under the write lock.
        {
            let db_build = db.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(1));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                interval.tick().await; // skip immediate fire
                let mut last: Vec<(String, u64)> = Vec::new();
                loop {
                    interval.tick().await;
                    // Under the *read* lock first: a store with nothing
                    // building must not pay a write lock once a second, and an
                    // idle server must not force the first-write index scan
                    // just because it has been running for a second. Open
                    // registers a build a snapshot cut short, so a restarted
                    // serve sees it here without a write.
                    if db_build.read().builds_in_progress().is_empty() {
                        continue;
                    }
                    let db_build = db_build.clone();
                    let pumped =
                        tokio::task::spawn_blocking(move || db_build.write().pump_index_build())
                            .await;
                    let now = match pumped {
                        Ok(Ok(v)) => v,
                        Ok(Err(GraphError::Busy { .. })) => continue,
                        Ok(Err(e)) => {
                            eprintln!("build-index failed: {e}");
                            continue;
                        }
                        Err(e) => {
                            eprintln!("build-index task panicked: {e}");
                            continue;
                        }
                    };
                    for (rule, total) in &last {
                        if !now.iter().any(|b| &b.rule == rule) {
                            eprintln!("built {rule}: {total} vectors");
                        }
                    }
                    for b in &now {
                        if last.iter().any(|(r, _)| r == &b.rule) {
                            eprintln!("building {}: {}/{}", b.rule, b.indexed, b.total);
                        }
                    }
                    last = now.into_iter().map(|b| (b.rule, b.total)).collect();
                }
            });
        }
        init_logging();
        // Bound before HTTP starts, so a taken port stops the start with
        // nothing served yet.
        let bolt = match bolt {
            Some(settings) => Some(
                bind_bolt(
                    settings,
                    tls_cert.as_deref().zip(tls_key.as_deref()),
                    token.clone(),
                    role_tokens.clone(),
                    access_keys.clone(),
                    plan.drain,
                )
                .await?,
            ),
            None => None,
        };
        let db_serve = db.clone();
        // The graceful-stop channel. Firing it — or dropping the sender — makes
        // the server stop accepting and let in-flight requests and subscribers
        // finish. Before 0.6.12 shutdown was `serve.abort()`, which severed
        // them mid-response.
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let mut serve = tokio::spawn(async move {
            // TLS path: both --tls-cert and --tls-key were supplied.
            if let (Some(cert), Some(key)) = (tls_cert, tls_key) {
                #[cfg(feature = "tls")]
                {
                    return server::serve_tls_with_shutdown_with_ready_flag(
                        db_serve,
                        addr,
                        tx,
                        cert,
                        key,
                        token,
                        role_tokens,
                        stop_rx,
                        ready_flag,
                        Some(access_keys),
                    )
                    .await;
                }
                #[cfg(not(feature = "tls"))]
                {
                    let _ = (
                        cert,
                        key,
                        db_serve,
                        addr,
                        tx,
                        token,
                        role_tokens,
                        stop_rx,
                        ready_flag,
                        access_keys,
                    );
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Unsupported,
                        "this binary was built without TLS support; \
                         rebuild with --features tls or terminate TLS at a \
                         reverse proxy (see docs/site/deployment.md)",
                    ));
                }
            }
            match ui {
                ServeUi::Filesystem(dir) => {
                    server::serve_with_ui_and_role_tokens_and_shutdown_with_ready_flag(
                        db_serve,
                        addr,
                        tx,
                        dir,
                        token,
                        role_tokens,
                        stop_rx,
                        ready_flag,
                        Some(access_keys),
                    )
                    .await
                }
                ServeUi::None => {
                    server::serve_with_role_tokens_and_shutdown_with_ready_flag(
                        db_serve,
                        addr,
                        tx,
                        token,
                        role_tokens,
                        stop_rx,
                        ready_flag,
                        Some(access_keys),
                    )
                    .await
                }
                ServeUi::Embedded => {
                    #[cfg(feature = "embed-ui")]
                    {
                        server::serve_with_embedded_ui_and_shutdown_with_ready_flag(
                            db_serve,
                            addr,
                            tx,
                            token,
                            role_tokens,
                            stop_rx,
                            ready_flag,
                            Some(access_keys),
                        )
                        .await
                    }
                    #[cfg(not(feature = "embed-ui"))]
                    {
                        server::serve_with_role_tokens_and_shutdown_with_ready_flag(
                            db_serve,
                            addr,
                            tx,
                            token,
                            role_tokens,
                            stop_rx,
                            ready_flag,
                            Some(access_keys),
                        )
                        .await
                    }
                }
            }
        });
        match rx.await {
            Ok(bound) => {
                // stdout stays exactly as it was: the docs, the tests and
                // `install` all read this line.
                println!("listening on http://{bound}");
                tracing::info!(addr = %bound, "listening");
            }
            Err(_) => {
                return match serve.await {
                    Ok(Ok(())) => Err("server exited before readiness".into()),
                    Ok(Err(e)) => Err(e.to_string()),
                    Err(e) => Err(e.to_string()),
                };
            }
        }
        // After the HTTP line: the docs and tests read that one first.
        let mut bolt = bolt.map(|bound| bound.start(&db));
        let mut backup = backup;
        let outcome = tokio::select! {
            result = &mut serve => match result {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(e.to_string()),
                Err(e) => Err(e.to_string()),
            },
            _ = shutdown_signal() => {
                let deadline = std::time::Instant::now() + shutdown_timeout;
                // The periodic snapshot must not run beside the shutdown one.
                if let Some(task) = &snapshot_task {
                    task.abort();
                }
                // Listen for a second signal before the sequence starts, so
                // none is swallowed by the handler tokio keeps installed.
                let second = shutdown_signal();
                let sequence = graceful_stop(
                    &mut serve,
                    stop_tx,
                    bolt.take(),
                    &db,
                    backup.take(),
                    plan,
                    deadline,
                );
                tokio::select! {
                    result = sequence => result,
                    _ = second => {
                        tracing::warn!(
                            "second shutdown signal: exiting now; the backup lease, if held, \
                             expires on its own after its TTL"
                        );
                        Err("shutdown forced by a second signal".to_string())
                    }
                }
            }
        };
        // The server ended by itself: the Bolt listener stops with it, and
        // the backup still gets its last flush.
        if let Some(running) = bolt.take() {
            running.stop(plan.drain).await;
        }
        if let Some((tx, handle, budget)) = backup.take() {
            stop_backup(tx, handle, budget).await;
        }
        outcome
    });
    // A snapshot abandoned past the deadline runs on a blocking thread; do not
    // wait for it (the WAL already holds every commit).
    rt.shutdown_background();
    result
}

/// Test hook, compiled only with the `test-hooks` feature (enabled for this
/// package's own tests, never in a release build): delay the shutdown
/// snapshot by this many milliseconds, to exercise the overrun path.
#[cfg(feature = "test-hooks")]
const TEST_SNAPSHOT_DELAY_ENV: &str = "MUSHROOMDB_TEST_SHUTDOWN_SNAPSHOT_DELAY_MS";

/// The graceful shutdown, in three bounded phases inside one `deadline`:
///
/// 1. **Drain**: stop accepting and let in-flight requests finish, for at most
///    `plan.drain` (25 s at the default timeout, scaled down for a shorter one). What is still open then is cut off.
///    The Bolt listener, when there is one, stops accepting at the same moment
///    and closes each connection at its next idle point, which rolls back an
///    open transaction; it shares the same drain bound.
/// 2. **Snapshot**, on a blocking thread, in what remains once the backup's
///    reserve is set aside. If it overruns it is skipped: the WAL holds every
///    commit and the next open replays it.
/// 3. **Backup**: flush, final manifest, lease release, for at most the
///    backup's own bound (or what the deadline leaves).
///
/// Every phase runs even if an earlier one failed or ran out, so the lease is
/// always released when the bucket is reachable.
async fn graceful_stop(
    serve: &mut tokio::task::JoinHandle<io::Result<()>>,
    stop_tx: tokio::sync::oneshot::Sender<()>,
    bolt: Option<RunningBolt>,
    db: &SharedDb,
    backup: Option<RunningBackup>,
    plan: cli::ShutdownPlan,
    deadline: std::time::Instant,
) -> Result<(), String> {
    let left = || deadline.saturating_duration_since(std::time::Instant::now());
    let reserve = backup.as_ref().map_or(Duration::ZERO, |(_, _, b)| *b);

    tracing::info!("shutdown signal received; draining in-flight requests");
    let _ = stop_tx.send(());
    let drain = plan.drain.min(left().saturating_sub(reserve));
    let drain_ends = std::time::Instant::now() + drain;
    let bolt_stopping = bolt.map(|running| {
        let _ = running.stop.send(true);
        running.task
    });
    match tokio::time::timeout(drain, &mut *serve).await {
        Ok(Ok(Ok(()))) => tracing::info!("drained; stopped"),
        Ok(Ok(Err(e))) => tracing::error!(error = %e, "server exited with an error"),
        Ok(Err(e)) => tracing::error!(error = %e, "server task failed"),
        Err(_) => {
            tracing::warn!(
                drain_s = drain.as_secs(),
                "requests still open after the drain period; closing them"
            );
            serve.abort();
        }
    }
    if let Some(mut task) = bolt_stopping {
        // The listener holds itself to the same bound; this is the backstop.
        let left_of_drain = drain_ends.saturating_duration_since(std::time::Instant::now());
        match tokio::time::timeout(left_of_drain + BOLT_DRAIN_SLACK, &mut task).await {
            Ok(Ok(Ok(()))) => tracing::info!("bolt: drained; stopped"),
            Ok(Ok(Err(e))) => tracing::error!(error = %e, "bolt: listener exited with an error"),
            Ok(Err(e)) => tracing::error!(error = %e, "bolt: listener task failed"),
            Err(_) => {
                tracing::warn!("bolt: connections still open after the drain period; closing them");
                task.abort();
            }
        }
    }

    // Same rule as the periodic snapshot: it needs the store's write lock.
    // Shutting down without one is fine: the WAL holds every commit.
    let mut outcome = Ok(());
    let snapshot_budget = left().saturating_sub(reserve);
    if snapshot_budget.is_zero() {
        tracing::warn!("no time left for the shutdown snapshot; skipped (the WAL is durable)");
    } else {
        let db_snap = db.clone();
        let snapshot = tokio::task::spawn_blocking(move || {
            #[cfg(feature = "test-hooks")]
            if let Some(ms) = std::env::var(TEST_SNAPSHOT_DELAY_ENV)
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
            {
                std::thread::sleep(Duration::from_millis(ms));
            }
            cli::snapshot_shared(&db_snap)
        });
        match tokio::time::timeout(snapshot_budget, snapshot).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(GraphError::Busy { .. }))) => {
                eprintln!("shutdown snapshot skipped: another process holds the write lock");
            }
            Ok(Ok(Err(e))) => outcome = Err(e.to_string()),
            Ok(Err(e)) => outcome = Err(format!("shutdown snapshot task failed: {e}")),
            Err(_) => tracing::warn!(
                budget_s = snapshot_budget.as_secs(),
                "the shutdown snapshot overran its budget; skipped (the WAL is durable)"
            ),
        }
    }

    if let Some((tx, handle, budget)) = backup {
        stop_backup(tx, handle, budget.min(left())).await;
    }
    outcome
}

/// How long past the drain bound the Bolt listener's task is waited for. The
/// listener aborts its own connections at the bound; this covers the abort.
const BOLT_DRAIN_SLACK: Duration = Duration::from_millis(500);

/// A Bolt listener that is bound and not yet serving.
struct BoundBolt {
    listener: bolt::server::BoltListener,
    authenticator: Arc<bolt::messages::hello::TokenAuthenticator>,
    config: bolt::server::BoltConfig,
    scheme: &'static str,
    bound: SocketAddr,
}

/// A serving Bolt listener: its task and its stop signal.
struct RunningBolt {
    task: tokio::task::JoinHandle<io::Result<()>>,
    stop: tokio::sync::watch::Sender<bool>,
}

impl BoundBolt {
    /// Starts serving and prints the line that says where.
    fn start(self, db: &SharedDb) -> RunningBolt {
        println!("listening on {}://{}", self.scheme, self.bound);
        let advertised = self
            .config
            .connection
            .routing
            .as_ref()
            .map(|r| r.advertised_address().to_string())
            .unwrap_or_default();
        tracing::info!(addr = %self.bound, %advertised, scheme = self.scheme, "bolt: listening");
        let (stop, stop_rx) = tokio::sync::watch::channel(false);
        let task =
            tokio::spawn(
                self.listener
                    .serve(db.clone(), self.authenticator, self.config, stop_rx),
            );
        RunningBolt { task, stop }
    }
}

impl RunningBolt {
    /// Stops the listener and waits for it, no longer than `drain` and the
    /// slack.
    async fn stop(mut self, drain: Duration) {
        let _ = self.stop.send(true);
        if tokio::time::timeout(drain + BOLT_DRAIN_SLACK, &mut self.task)
            .await
            .is_err()
        {
            self.task.abort();
        }
    }
}

/// Binds the Bolt listener `settings` describes. Its credentials are exactly
/// the HTTP server's: the token, the role tokens and the `keys.json` set (the
/// same set, so a SIGHUP reload reaches both). With TLS it serves the HTTP
/// certificate.
async fn bind_bolt(
    settings: cli::serve_bolt::BoltSettings,
    tls: Option<(&std::path::Path, &std::path::Path)>,
    token: Option<String>,
    role_tokens: HashMap<String, String>,
    access_keys: mushroomdb::AccessKeys,
    drain: Duration,
) -> Result<BoundBolt, String> {
    let mut config = bolt::server::BoltConfig {
        max_connections: settings.max_connections,
        drain_timeout: drain,
        ..bolt::server::BoltConfig::default()
    };
    config.connection.tx_timeout = settings.tx_timeout;
    config.connection.bookmark_timeout = settings.bookmark_timeout;
    if settings.tls {
        let Some((cert, key)) = tls else {
            return Err("--bolt-tls needs --tls-cert and --tls-key".into());
        };
        #[cfg(feature = "tls")]
        {
            config.tls = Some(server::load_server_config(cert, key)?);
        }
        #[cfg(not(feature = "tls"))]
        {
            let _ = (cert, key);
            return Err(
                "this binary was built without TLS support; rebuild with --features tls, \
                 or give --bolt-plaintext and terminate TLS at a proxy \
                 (see docs/site/bolt.md)"
                    .into(),
            );
        }
    }
    let listener = bolt::server::BoltListener::bind(settings.bind)
        .await
        .map_err(|e| format!("--bolt-port: cannot listen on {}: {e}", settings.bind))?;
    let bound = listener.local_addr().map_err(|e| e.to_string())?;
    config.connection.routing = Some(settings.routing(bound)?);
    let authenticator = Arc::new(
        bolt::messages::hello::TokenAuthenticator::new(token, role_tokens)
            .with_access_keys(access_keys),
    );
    Ok(BoundBolt {
        listener,
        authenticator,
        config,
        scheme: settings.scheme(),
        bound,
    })
}

/// Send the backup worker `Shutdown` and wait for it, no longer than `wait`.
/// The worker flushes the last WAL segment and any new snapshot, writes a
/// final manifest, and deletes its lease. A timeout is logged and ignored: the
/// local store is already safe, and the lease then expires by itself.
async fn stop_backup(
    tx: tokio::sync::watch::Sender<backup::BackupTrigger>,
    handle: backup::BackupHandle,
    wait: Duration,
) {
    tracing::info!("shutdown: flushing the S3 backup");
    let _ = tx.send(backup::BackupTrigger::Shutdown);
    match tokio::time::timeout(wait, handle.join).await {
        Ok(Ok(())) => tracing::info!("shutdown: backup flushed"),
        Ok(Err(e)) => tracing::warn!(error = %e, "shutdown: the backup worker failed"),
        Err(_) => tracing::warn!(
            wait_s = wait.as_secs(),
            "shutdown: the S3 flush did not finish in time; the local store is safe"
        ),
    }
}

/// The backup worker, the sender that drives it and the longest its shutdown
/// may take, or `None` when `MUSHROOMDB_BACKUP_S3_URL` is unset.
type RunningBackup = (
    tokio::sync::watch::Sender<backup::BackupTrigger>,
    backup::BackupHandle,
    Duration,
);

/// Start the backup worker when the environment configures one.
///
/// A configuration that is set but malformed refuses to start the server: a
/// typo must not quietly leave a store without the backup its operator asked
/// for. The worker itself never stops the server — its failures are retried
/// and counted on the handle.
///
/// Also divides `shutdown_timeout` between the shutdown phases. The backup's
/// flush grace and request deadline are shrunk when its share of a short
/// timeout is smaller than they are, so the lease release still fits.
fn start_backup(
    db: &SharedDb,
    db_dir: &std::path::Path,
    shutdown_timeout: Duration,
) -> Result<(Option<RunningBackup>, cli::ShutdownPlan), String> {
    let Some(mut config) = backup::BackupConfig::from_env().map_err(|e| e.to_string())? else {
        return Ok((None, cli::shutdown_plan(shutdown_timeout, Duration::ZERO)));
    };
    let plan = cli::shutdown_plan(shutdown_timeout, config.shutdown_budget());
    config.fit_shutdown_within(plan.backup);
    let store = backup::build_store(&config).map_err(|e| e.to_string())?;
    let budget = plan.backup;
    let (tx, rx) = tokio::sync::watch::channel(backup::BackupTrigger::Idle);
    let handle = backup::BackupWorker::spawn(
        config,
        store,
        std::sync::Arc::new(db.clone()),
        db_dir.to_path_buf(),
        rx,
    )
    .map_err(|e| e.to_string())?;
    Ok((Some((tx, handle, budget)), plan))
}

async fn shutdown_signal() {
    let ctrl_c = async {
        match tokio::signal::ctrl_c().await {
            Ok(()) => {}
            // Install failure is not SIGINT; park like SIGTERM handler Err.
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                let _ = sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

fn run_mcp(db_dir: PathBuf, all_tools: bool) -> Result<(), String> {
    // `holds_a_store` (the same predicate `restore_if_empty` gates on) must be
    // checked before `open` creates the directory's files — it is what tells
    // a brand-new store from one this invocation is merely reopening. Declaring
    // full-text on a populated store builds the index at each open until a
    // snapshot carries it (ledger row 36), and changes what an existing store
    // indexes, so an existing store is never touched here; only a store this
    // call is creating gets the memory schema.
    let is_new_store = !mushroomdb::restore::holds_a_store(&db_dir);
    let db = SharedDb::open(&db_dir).map_err(|e| e.to_string())?;
    if is_new_store {
        // The first store most MCP hosts open. Declare the memory schema
        // before the first write so `recall` can answer on it from the start.
        db.write()
            .apply_schema(&mushroomdb::memory_schema::memory_defaults())
            .map_err(|e| e.to_string())?;
    }
    // Nothing else snapshots a store that is only ever used through this
    // server, and without one every session replays the store's whole
    // history to open it. Here as well as at the end of the session: a host
    // is as likely to kill this process as to close its stdin.
    cli::snapshot_if_wal_grew(&db, &db_dir);
    let stdin = io::stdin();
    let stdout = io::stdout();
    // The store's path, not just a handle: the `recall` tool names the store
    // by that path when it tells the caller how to fix an unindexed one, and
    // an open database cannot say where it lives.
    let served = server::run_mcp_stdio_with(
        db.clone(),
        Some(db_dir.clone()),
        all_tools,
        stdin.lock(),
        stdout.lock(),
    );
    cli::snapshot_if_wal_grew(&db, &db_dir);
    served.map_err(|e| e.to_string())
}

fn home_dir() -> PathBuf {
    home_from(std::env::var_os("HOME"), std::env::var_os("USERPROFILE"))
}

/// The user's home: `HOME`, then `USERPROFILE` — a native Windows shell sets
/// only the second — then `/tmp` for safety (never panic). A variable set to
/// nothing counts as unset.
fn home_from(home: Option<OsString>, userprofile: Option<OsString>) -> PathBuf {
    home.into_iter()
        .chain(userprofile)
        .find(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

#[cfg(test)]
mod tests {
    use super::{parse_log_format, LogFormat, DEFAULT_LOG_FILTER};
    use std::path::PathBuf;

    #[test]
    fn home_is_the_user_profile_where_home_is_not_set() {
        let profile: Option<std::ffi::OsString> = Some("C:\\Users\\me".into());
        assert_eq!(
            super::home_from(None, profile.clone()),
            PathBuf::from("C:\\Users\\me")
        );
        assert_eq!(
            super::home_from(Some("".into()), profile.clone()),
            PathBuf::from("C:\\Users\\me")
        );
        assert_eq!(
            super::home_from(Some("/home/me".into()), profile),
            PathBuf::from("/home/me")
        );
        assert_eq!(super::home_from(None, None), PathBuf::from("/tmp"));
    }

    /// Binding: the default filter names the target this crate's server
    /// actually logs under.
    ///
    /// A typo here is silent — the server simply says nothing, which looks
    /// like health. This shipped wrong for an afternoon: the filter named
    /// `mushroomdb_server`, the events are emitted under `server`, and the
    /// result was a 404 that logged at INFO and appeared nowhere.
    #[test]
    fn default_filter_names_the_servers_real_log_target() {
        let root = server::LOG_TARGET_ROOT;
        assert!(
            DEFAULT_LOG_FILTER.contains(&format!("{root}=")),
            "the default filter {DEFAULT_LOG_FILTER:?} does not name the \
             server's log target {root:?}, so nothing it logs would appear"
        );
    }

    #[test]
    fn log_format_values() {
        assert_eq!(parse_log_format(None), Ok(LogFormat::Text));
        assert_eq!(parse_log_format(Some("")), Ok(LogFormat::Text));
        assert_eq!(parse_log_format(Some("text")), Ok(LogFormat::Text));
        assert_eq!(parse_log_format(Some("JSON")), Ok(LogFormat::Json));
        assert!(parse_log_format(Some("yaml")).unwrap_err().contains("yaml"));
    }

    /// The filter parses. An unparseable default falls back to silence.
    #[test]
    fn default_filter_parses() {
        use tracing_subscriber::EnvFilter;
        let parsed = EnvFilter::builder().parse(DEFAULT_LOG_FILTER);
        assert!(parsed.is_ok(), "default filter does not parse: {parsed:?}");
    }
}
