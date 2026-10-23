//! `tck-runner`: run the vendored openCypher TCK once and act on the result.
//!
//! ```text
//! tck-runner [--report <path>] [--function-table <path>] [--failures <path>]
//!            [--check] [--update-baseline] [--threads <n>] [--filter <text>]
//! ```
//!
//! - `--report <path>` writes the conformance page (docs/site/conformance.md).
//! - `--function-table <path>` writes the Cypher function table (contract C5).
//! - `--failures <path>` writes every failing scenario with each failed step.
//! - `--check` exits 1 when the pass rate is below `tck-baseline.txt`, and
//!   prints the new rate when it is above (it never writes the file).
//! - `--update-baseline` writes the rate to `tck-baseline.txt` (a local step
//!   for the change that raises the baseline; CI never runs it).
//! - `--filter <text>` runs only scenarios whose label contains the text.
//! - `--query <cypher>` runs one statement against a fresh store and prints
//!   the rows or the error (repeat it to run several in order on one store);
//!   nothing else runs.
//!
//! With no action flag it prints the summary.

use std::fmt::Write as _;
use std::process::ExitCode;
use tck_runner::report::{conformance_page, engine_knows_function, failure_groups, function_table};
use tck_runner::value::from_cell;
use tck_runner::{
    baseline_from_file, baseline_path, default_threads, features_dir, format_rate, holds_baseline,
    parse_features_checked, pass_rate, run_all, run_query, tally, tck_root, Outcome, TempGraph,
};

struct Args {
    report: Option<String>,
    function_table: Option<String>,
    failures: Option<String>,
    check: bool,
    update_baseline: bool,
    threads: usize,
    filter: Option<String>,
    queries: Vec<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        report: None,
        function_table: None,
        failures: None,
        check: false,
        update_baseline: false,
        threads: default_threads(),
        filter: None,
        queries: Vec::new(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut value = |flag: &str| it.next().ok_or_else(|| format!("{flag} needs a value"));
        match a.as_str() {
            "--report" => args.report = Some(value("--report")?),
            "--function-table" => args.function_table = Some(value("--function-table")?),
            "--failures" => args.failures = Some(value("--failures")?),
            "--filter" => args.filter = Some(value("--filter")?),
            "--query" => args.queries.push(value("--query")?),
            "--threads" => {
                args.threads = value("--threads")?
                    .parse()
                    .map_err(|e| format!("--threads: {e}"))?
            }
            "--check" => args.check = true,
            "--update-baseline" => args.update_baseline = true,
            "-h" | "--help" => {
                return Err(
                    "usage: tck-runner [--report <path>] [--function-table <path>] \
                            [--failures <path>] [--check] [--update-baseline] \
                            [--threads <n>] [--filter <text>] [--query <cypher>]"
                        .into(),
                )
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(args)
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    if !args.queries.is_empty() {
        return run_queries(&args.queries);
    }
    let (mut scenarios, errors) = parse_features_checked(&features_dir());
    for e in &errors {
        eprintln!("feature did not parse: {}: {}", e.feature, e.message);
    }
    if let Some(f) = &args.filter {
        scenarios.retain(|s| s.label().contains(f.as_str()));
    }
    let started = std::time::Instant::now();
    let outcomes = run_all(&scenarios, TempGraph::new, args.threads);
    let (passed, failed, skipped) = tally(&outcomes);
    let rate = pass_rate(passed, failed);
    println!(
        "TCK: {passed} passed, {failed} failed, {skipped} skipped; pass rate {} ({:.1}%) in {:.1}s",
        format_rate(rate),
        rate * 100.0,
        started.elapsed().as_secs_f64()
    );
    for (group, n, _) in failure_groups(&scenarios, &outcomes).into_iter().take(10) {
        println!("  {n:>5}  {group}");
    }

    let version = std::fs::read_to_string(tck_root().join("../VERSION"))
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|_| "unknown".into());

    let functions = (args.report.is_some() || args.function_table.is_some())
        .then(|| function_table(&scenarios, &outcomes, engine_knows_function));
    if let Some(path) = &args.report {
        let page = conformance_page(&scenarios, &outcomes, &version, functions.as_deref());
        if let Err(e) = std::fs::write(path, page) {
            eprintln!("write {path}: {e}");
            return ExitCode::FAILURE;
        }
        println!("wrote {path}");
    }
    if let (Some(path), Some(table)) = (&args.function_table, &functions) {
        if let Err(e) = std::fs::write(path, table) {
            eprintln!("write {path}: {e}");
            return ExitCode::FAILURE;
        }
        println!("wrote {path}");
    }
    if let Some(path) = &args.failures {
        let mut out = String::new();
        for (s, o) in scenarios.iter().zip(&outcomes) {
            match o {
                Outcome::Fail(failures) => {
                    let _ = writeln!(out, "FAIL {}", s.label());
                    for f in failures {
                        let _ =
                            writeln!(out, "  [{}] {}", f.group, f.detail.replace('\n', "\n    "));
                    }
                }
                Outcome::Pass(Some(raised)) => {
                    let _ = writeln!(
                        out,
                        "PASS-ERROR {}\n  expected {} at {}; engine: {}",
                        s.label(),
                        raised.expected_type,
                        raised.expected_phase,
                        raised.engine_message.replace('\n', " ")
                    );
                }
                Outcome::Skip(rule) => {
                    let _ = writeln!(out, "SKIP {} [{}]", s.label(), rule.id);
                }
                Outcome::Pass(None) => {
                    let _ = writeln!(out, "PASS {}", s.label());
                }
            }
        }
        if let Err(e) = std::fs::write(path, out) {
            eprintln!("write {path}: {e}");
            return ExitCode::FAILURE;
        }
        println!("wrote {path}");
    }
    if args.update_baseline {
        let path = baseline_path();
        let text = format!(
            "# The openCypher TCK pass rate CI holds: passed / (passed + failed).\n\
             # Raise it in the change that raises the rate: cargo run --release -p tck-runner -- --update-baseline\n\
             {}\n",
            format_rate(rate)
        );
        if let Err(e) = std::fs::write(&path, text) {
            eprintln!("write {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
        println!("wrote {} = {}", path.display(), format_rate(rate));
    }
    if !errors.is_empty() {
        eprintln!("{} feature files did not parse", errors.len());
        return ExitCode::FAILURE;
    }
    if args.check {
        let baseline = match baseline_from_file(baseline_path()) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::FAILURE;
            }
        };
        if !holds_baseline(rate, baseline) {
            eprintln!(
                "TCK pass rate {} is below the baseline {} (tck-baseline.txt)",
                format_rate(rate),
                format_rate(baseline)
            );
            return ExitCode::FAILURE;
        }
        if format_rate(rate) != format_rate(baseline) {
            println!(
                "TCK pass rate rose: {} -> {}. Raise tck-baseline.txt to {} in a change of its own \
                 (cargo run --release -p tck-runner -- --update-baseline).",
                format_rate(baseline),
                format_rate(rate),
                format_rate(rate)
            );
        } else {
            println!("TCK pass rate {} holds the baseline", format_rate(rate));
        }
    }
    ExitCode::SUCCESS
}

/// `--query`: each statement in turn on one fresh store, rows in the TCK's
/// notation.
fn run_queries(queries: &[String]) -> ExitCode {
    let mut graph = TempGraph::new();
    for q in queries {
        println!("> {q}");
        match run_query(&mut graph.db, q, &Default::default()) {
            Ok(rs) => {
                println!("  | {} |", rs.columns().join(" | "));
                for r in 0..rs.len() {
                    let cells: Vec<String> = (0..rs.columns().len())
                        .map(|c| from_cell(rs.entity(r, c), rs.row(r)[c].as_ref()).to_string())
                        .collect();
                    println!("  | {} |", cells.join(" | "));
                }
            }
            Err(e) => println!("  error: {e}"),
        }
    }
    ExitCode::SUCCESS
}
