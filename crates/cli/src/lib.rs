//! `graphdb` CLI library: hand-rolled arg parsing and the demo dataset builder.
//!
//! The binary in `main.rs` stays thin — it dispatches on [`parse_args`] and
//! prints what the lib functions return.

use core_api::{Explanation, IngestOptions, Predicate, ResultSet, RuleDef, SharedDb, Stats, Value};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

/// Deterministic demo: 10 Orgs, 20 Projects, 30 People.
pub const N_ORGS: usize = 10;
pub const N_PROJECTS: usize = 20;
pub const N_PEOPLE: usize = 30;

/// Sample query printed by `graphdb demo` and executed against the fresh store.
///
/// Scoped to one person so `ORDER BY score DESC` is visibly ranked (a global
/// `LIMIT 5` would be five 1.0 home-project hits).
pub const SAMPLE_QUERY: &str = "\
MATCH (p:Person {id: 'person-01'})-[r:FIT]->(proj:Project)
RETURN p, proj, r.score AS score
ORDER BY score DESC, proj";

const SAMPLE_EXPLAIN_A: &str = "person-01";
const SAMPLE_EXPLAIN_B: &str = "proj-01";

/// Parsed `graphdb` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Serve { db_dir: PathBuf, addr: SocketAddr },
    Mcp { db_dir: PathBuf },
    Stats { db_dir: PathBuf },
    Demo { db_dir: PathBuf },
    Help,
}

/// Outcome of [`run_demo`]. Counts are deterministic.
#[derive(Debug)]
pub struct DemoOutcome {
    pub auto_fk_rules: Vec<String>,
    pub sample_query: String,
    pub sample_result: ResultSet,
    pub explanations: Vec<Explanation>,
    pub stats: Stats,
}

/// CLI-facing error. [`Display`] is the message printed to stderr.
#[derive(Debug)]
pub struct CliError(pub String);

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CliError {}

impl From<core_api::GraphError> for CliError {
    fn from(e: core_api::GraphError) -> Self {
        CliError(e.to_string())
    }
}

impl From<std::io::Error> for CliError {
    fn from(e: std::io::Error) -> Self {
        CliError(e.to_string())
    }
}

/// Usage text for no-args / `--help` / `-h`.
pub fn usage() -> &'static str {
    "\
graphdb — embedded graph database

Usage:
  graphdb serve <db-dir> [--addr 127.0.0.1:0]
  graphdb mcp <db-dir>
  graphdb stats <db-dir>
  graphdb demo <db-dir>
  graphdb --help
"
}

/// Parse argv after the binary name. Hand-rolled — no clap.
pub fn parse_args<S: AsRef<str>>(args: &[S]) -> Result<Command, String> {
    let args: Vec<&str> = args.iter().map(AsRef::as_ref).collect();
    if args.is_empty() {
        return Ok(Command::Help);
    }
    match args[0] {
        "--help" | "-h" | "help" => Ok(Command::Help),
        "serve" => parse_serve(&args[1..]),
        "mcp" => parse_one_dir("mcp", &args[1..]).map(|db_dir| Command::Mcp { db_dir }),
        "stats" => parse_one_dir("stats", &args[1..]).map(|db_dir| Command::Stats { db_dir }),
        "demo" => parse_one_dir("demo", &args[1..]).map(|db_dir| Command::Demo { db_dir }),
        other => Err(format!("unknown command: {other}")),
    }
}

fn default_addr() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

fn parse_serve(args: &[&str]) -> Result<Command, String> {
    let mut db_dir = None;
    let mut addr = default_addr();
    let mut i = 0;
    while i < args.len() {
        let a = args[i];
        if a == "--addr" {
            let val = args
                .get(i + 1)
                .copied()
                .ok_or_else(|| "missing value for --addr".to_string())?;
            addr = val.parse().map_err(|_| format!("invalid address: {val}"))?;
            i += 2;
        } else if let Some(val) = a.strip_prefix("--addr=") {
            addr = val.parse().map_err(|_| format!("invalid address: {val}"))?;
            i += 1;
        } else if a.starts_with('-') {
            return Err(format!("unexpected flag: {a}"));
        } else if db_dir.is_none() {
            db_dir = Some(PathBuf::from(a));
            i += 1;
        } else {
            return Err(format!("unexpected extra argument: {a}"));
        }
    }
    let db_dir = db_dir.ok_or_else(|| "serve requires <db-dir>".to_string())?;
    Ok(Command::Serve { db_dir, addr })
}

fn parse_one_dir(cmd: &str, args: &[&str]) -> Result<PathBuf, String> {
    let mut db_dir = None;
    for a in args {
        if a.starts_with('-') {
            return Err(format!("unexpected flag: {a}"));
        }
        if db_dir.is_some() {
            return Err(format!("unexpected extra argument: {a}"));
        }
        db_dir = Some(PathBuf::from(*a));
    }
    db_dir.ok_or_else(|| format!("{cmd} requires <db-dir>"))
}

/// Pretty-print [`Stats`] for `graphdb stats` and the demo smoke test.
pub fn format_stats(stats: &Stats) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "nodes: {} live, {} tombstoned",
        stats.nodes_live, stats.nodes_tombstoned
    );
    let _ = writeln!(out, "edges: {}", stats.edges);
    let _ = writeln!(out, "rules: {}", stats.rules.len());
    for r in &stats.rules {
        let _ = writeln!(
            out,
            "  {:<28} edges={}  tripped={}",
            r.name, r.edges, r.tripped
        );
    }
    out
}

/// Open `dir` and return live stats.
pub fn read_stats(dir: &Path) -> Result<Stats, CliError> {
    let db = SharedDb::open(dir)?;
    let stats = db.read().stats();
    Ok(stats)
}

/// Build the deterministic demo dataset in an empty `dir`.
///
/// Refuses if `dir` already exists and is not empty. Ingests 10 Orgs, 20
/// Projects, 30 People via [`SharedDb`] / `ingest_json` (auto-FK on `*_id`)
/// then declares one scored `Overlap` rule (`skill_fit`).
pub fn run_demo(dir: &Path) -> Result<DemoOutcome, CliError> {
    refuse_non_empty(dir)?;

    let db = SharedDb::open(dir)?;
    let opts = IngestOptions::default();
    let mut auto_fk_rules = Vec::new();

    {
        let mut w = db.write();
        for (label, json) in [
            ("Org", org_json()),
            ("Project", project_json()),
            ("Person", person_json()),
        ] {
            let report = w.ingest_json(label, &json, &opts)?;
            if !report.row_errors.is_empty() {
                return Err(CliError(format!(
                    "demo ingest of {label} had row errors: {:?}",
                    report.row_errors
                )));
            }
            auto_fk_rules.extend(report.rules_created);
        }
        w.create_rule(RuleDef {
            name: "skill_fit".into(),
            src_label: "Person".into(),
            dst_label: "Project".into(),
            predicate: Predicate::Overlap {
                field: "skills".into(),
                min: 0.5,
            },
            edge_type: "FIT".into(),
            weight_prop: Some("score".into()),
            max_edges: None,
        })?;
    }

    let r = db.read();
    let sample_result = r.query(SAMPLE_QUERY, &BTreeMap::new())?;
    let explanations = r.explain(SAMPLE_EXPLAIN_A, SAMPLE_EXPLAIN_B)?;
    let stats = r.stats();

    Ok(DemoOutcome {
        auto_fk_rules,
        sample_query: SAMPLE_QUERY.to_string(),
        sample_result,
        explanations,
        stats,
    })
}

fn refuse_non_empty(dir: &Path) -> Result<(), CliError> {
    if dir.is_file() {
        return Err(CliError(format!(
            "demo refuses a non-empty directory: {} is a file",
            dir.display()
        )));
    }
    if dir.exists() {
        let mut entries = std::fs::read_dir(dir)?;
        if entries.next().is_some() {
            return Err(CliError(format!(
                "demo refuses a non-empty directory: {} \
                 (directory must be empty — including hidden files)",
                dir.display()
            )));
        }
    }
    Ok(())
}

fn json_array(rows: impl IntoIterator<Item = String>) -> String {
    let mut out = String::from("[");
    let mut first = true;
    for row in rows {
        if !first {
            out.push(',');
        }
        first = false;
        out.push_str(&row);
    }
    out.push(']');
    out
}

/// Wrap a 1-based project index into `1..=N_PROJECTS`.
fn wrap_proj(i: usize) -> usize {
    (i - 1) % N_PROJECTS + 1
}

/// Sliding window of `len` skill tokens starting at project `start`.
fn skill_window_json(start: usize, len: usize) -> String {
    let parts: Vec<String> = (0..len)
        .map(|k| format!(r#""s{:02}""#, wrap_proj(start + k)))
        .collect();
    format!("[{}]", parts.join(","))
}

fn org_json() -> String {
    json_array((1..=N_ORGS).map(|i| {
        format!(
            r#"{{"id":"org-{i:02}","name":"Org {i}","skills":{}}}"#,
            skill_window_json(i, 3)
        )
    }))
}

fn project_json() -> String {
    json_array((1..=N_PROJECTS).map(|i| {
        let org = (i - 1) % N_ORGS + 1;
        format!(
            r#"{{"id":"proj-{i:02}","name":"Project {i}","org_id":"org-{org:02}","skills":{}}}"#,
            skill_window_json(i, 3)
        )
    }))
}

fn person_json() -> String {
    json_array((1..=N_PEOPLE).map(|i| {
        let org = (i - 1) % N_ORGS + 1;
        let proj = (i - 1) % N_PROJECTS + 1;
        format!(
            r#"{{"id":"person-{i:02}","name":"Person {i}","org_id":"org-{org:02}","project_id":"proj-{proj:02}","skills":{}}}"#,
            skill_window_json(proj, 3)
        )
    }))
}

/// Render a [`DemoOutcome`] the way `graphdb demo` prints it.
pub fn format_demo(dir: &Path, out: &DemoOutcome) -> String {
    let mut buf = String::new();
    let _ = writeln!(buf, "== demo ==");
    let _ = writeln!(
        buf,
        "ingested {N_ORGS} Orgs, {N_PROJECTS} Projects, {N_PEOPLE} People"
    );
    let _ = writeln!(
        buf,
        "overlap rule: skill_fit (Person.skills ∩ Project.skills, min 0.5)"
    );
    let _ = writeln!(buf);
    let _ = writeln!(buf, "== auto-FK rules ==");
    let mut names = out.auto_fk_rules.clone();
    names.sort();
    for name in names {
        let _ = writeln!(buf, "  {name}");
    }
    let _ = writeln!(buf);
    let _ = writeln!(buf, "== query ==");
    let _ = writeln!(buf, "{}", out.sample_query);
    let _ = writeln!(buf);
    let _ = writeln!(buf, "columns: {}", out.sample_result.columns().join(", "));
    for i in 0..out.sample_result.len() {
        let cells: Vec<String> = out
            .sample_result
            .columns()
            .iter()
            .map(|c| format!("{c}={}", fmt_cell(out.sample_result.get(i, c))))
            .collect();
        let _ = writeln!(buf, "  {}", cells.join("  "));
    }
    let _ = writeln!(buf);
    let _ = writeln!(
        buf,
        "== explain ({SAMPLE_EXPLAIN_A}, {SAMPLE_EXPLAIN_B}) =="
    );
    for e in &out.explanations {
        let weight = e
            .weight
            .map(|w| fmt_value(&Value::Float(w)))
            .unwrap_or_else(|| "none".into());
        let _ = writeln!(
            buf,
            "  rule={}  type={}  {}→{}  weight={}",
            e.rule, e.edge_type, e.src_key, e.dst_key, weight
        );
    }
    let _ = writeln!(buf);
    let _ = writeln!(buf, "== serve ==");
    let _ = writeln!(buf, "  graphdb serve {}", dir.display());
    buf
}

fn fmt_value(v: &Value) -> String {
    match v {
        Value::Int(i) => i.to_string(),
        Value::Float(f) => {
            let s = format!("{f}");
            if s.contains('.') || s.contains('e') || s.contains('E') {
                s
            } else {
                format!("{s}.0")
            }
        }
        Value::Str(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::List(xs) => {
            let inner: Vec<String> = xs.iter().map(fmt_value).collect();
            format!("[{}]", inner.join(", "))
        }
    }
}

fn fmt_cell(cell: Option<&Value>) -> String {
    match cell {
        None => "null".into(),
        Some(v) => fmt_value(v),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::path::PathBuf;

    fn tmp(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let d = std::env::temp_dir().join(format!(
            "graphdb-cli-{}-{}-{}",
            name,
            std::process::id(),
            nanos
        ));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn default_bind() -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 0))
    }

    #[test]
    fn parse_args_table() {
        struct Case {
            args: &'static [&'static str],
            check: fn(Result<Command, String>),
        }

        let cases = [
            Case {
                args: &[],
                check: |r| match r {
                    Ok(Command::Help) => {}
                    other => panic!("no-args → Help, got {other:?}"),
                },
            },
            Case {
                args: &["--help"],
                check: |r| match r {
                    Ok(Command::Help) => {}
                    other => panic!("--help → Help, got {other:?}"),
                },
            },
            Case {
                args: &["-h"],
                check: |r| match r {
                    Ok(Command::Help) => {}
                    other => panic!("-h → Help, got {other:?}"),
                },
            },
            Case {
                args: &["serve", "/tmp/demo-db"],
                check: |r| match r {
                    Ok(Command::Serve { db_dir, addr }) => {
                        assert_eq!(db_dir, PathBuf::from("/tmp/demo-db"));
                        assert_eq!(addr, default_bind());
                    }
                    other => panic!("serve <dir> → Serve default addr, got {other:?}"),
                },
            },
            Case {
                args: &["serve", "/tmp/demo-db", "--addr", "127.0.0.1:8080"],
                check: |r| match r {
                    Ok(Command::Serve { db_dir, addr }) => {
                        assert_eq!(db_dir, PathBuf::from("/tmp/demo-db"));
                        assert_eq!(addr, "127.0.0.1:8080".parse().unwrap());
                    }
                    other => panic!("serve --addr after dir, got {other:?}"),
                },
            },
            Case {
                args: &["serve", "/tmp/demo-db", "--addr=127.0.0.1:9090"],
                check: |r| match r {
                    Ok(Command::Serve { db_dir, addr }) => {
                        assert_eq!(db_dir, PathBuf::from("/tmp/demo-db"));
                        assert_eq!(addr, "127.0.0.1:9090".parse().unwrap());
                    }
                    other => panic!("serve --addr=VALUE, got {other:?}"),
                },
            },
            Case {
                args: &["mcp", "/tmp/demo-db"],
                check: |r| match r {
                    Ok(Command::Mcp { db_dir }) => {
                        assert_eq!(db_dir, PathBuf::from("/tmp/demo-db"));
                    }
                    other => panic!("mcp <dir>, got {other:?}"),
                },
            },
            Case {
                args: &["stats", "/tmp/demo-db"],
                check: |r| match r {
                    Ok(Command::Stats { db_dir }) => {
                        assert_eq!(db_dir, PathBuf::from("/tmp/demo-db"));
                    }
                    other => panic!("stats <dir>, got {other:?}"),
                },
            },
            Case {
                args: &["demo", "/tmp/demo-db"],
                check: |r| match r {
                    Ok(Command::Demo { db_dir }) => {
                        assert_eq!(db_dir, PathBuf::from("/tmp/demo-db"));
                    }
                    other => panic!("demo <dir>, got {other:?}"),
                },
            },
            Case {
                args: &["serve"],
                check: |r| {
                    let e = r.expect_err("serve without dir");
                    assert!(
                        e.to_lowercase().contains("db-dir") || e.to_lowercase().contains("dir"),
                        "missing-dir error should mention dir, got {e}"
                    );
                },
            },
            Case {
                args: &["mcp"],
                check: |r| {
                    let e = r.expect_err("mcp without dir");
                    assert!(
                        e.to_lowercase().contains("db-dir") || e.to_lowercase().contains("dir"),
                        "missing-dir error should mention dir, got {e}"
                    );
                },
            },
            Case {
                args: &["stats"],
                check: |r| {
                    let e = r.expect_err("stats without dir");
                    assert!(
                        e.to_lowercase().contains("db-dir") || e.to_lowercase().contains("dir"),
                        "missing-dir error should mention dir, got {e}"
                    );
                },
            },
            Case {
                args: &["demo"],
                check: |r| {
                    let e = r.expect_err("demo without dir");
                    assert!(
                        e.to_lowercase().contains("db-dir") || e.to_lowercase().contains("dir"),
                        "missing-dir error should mention dir, got {e}"
                    );
                },
            },
            Case {
                args: &["serve", "/tmp/demo-db", "--addr"],
                check: |r| {
                    let e = r.expect_err("--addr missing value");
                    assert!(
                        e.to_lowercase().contains("addr"),
                        "--addr missing value should mention addr, got {e}"
                    );
                },
            },
            Case {
                args: &["serve", "/tmp/demo-db", "--addr", "not-an-addr"],
                check: |r| {
                    let e = r.expect_err("invalid addr");
                    assert!(
                        e.to_lowercase().contains("addr") || e.to_lowercase().contains("address"),
                        "invalid addr should mention address, got {e}"
                    );
                },
            },
            Case {
                args: &["frobnicate", "/tmp/demo-db"],
                check: |r| {
                    let e = r.expect_err("unknown command");
                    assert!(
                        e.to_lowercase().contains("unknown")
                            || e.to_lowercase().contains("frobnicate"),
                        "unknown command should name it, got {e}"
                    );
                },
            },
            Case {
                args: &["serve", "/tmp/demo-db", "extra"],
                check: |r| {
                    let e = r.expect_err("extra positional");
                    assert!(
                        e.to_lowercase().contains("unexpected")
                            || e.to_lowercase().contains("extra"),
                        "extra arg should be rejected, got {e}"
                    );
                },
            },
        ];

        for case in &cases {
            (case.check)(parse_args(case.args));
        }
    }

    #[test]
    fn usage_lists_every_subcommand() {
        let text = usage();
        for word in ["serve", "mcp", "stats", "demo", "graphdb"] {
            assert!(
                text.contains(word),
                "usage should mention {word}, got:\n{text}"
            );
        }
    }

    #[test]
    fn demo_builder_is_deterministic_and_refuses_second_run() {
        let dir = tmp("demo");
        let out = run_demo(&dir).expect("first demo run");

        assert_eq!(
            out.stats.nodes_live, 60,
            "10 orgs + 20 projects + 30 people"
        );
        assert_eq!(out.stats.nodes_tombstoned, 0);
        // Auto-FK: 20 project→org + 30 person→org + 30 person→project = 80.
        // FIT: each of 30 people matches home (Jaccard 1.0) and two adjacent
        // projects (3-skill window shifted ±1 → Jaccard 2/4 = 0.5) = 30*3 = 90.
        // Total edges: 80 + 90 = 170.
        assert_eq!(out.stats.edges, 170);
        assert_eq!(out.stats.rules.len(), 4, "3 auto-FK + 1 overlap");
        let fit = out
            .stats
            .rules
            .iter()
            .find(|r| r.name == "skill_fit")
            .expect("skill_fit");
        assert_eq!(fit.edges, 90, "30 people × 3 FIT edges");

        let mut names: Vec<&str> = out.stats.rules.iter().map(|r| r.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec![
                "auto_fk_person_org_id",
                "auto_fk_person_project_id",
                "auto_fk_project_org_id",
                "skill_fit",
            ]
        );

        let mut auto = out.auto_fk_rules.clone();
        auto.sort();
        assert_eq!(
            auto,
            vec![
                "auto_fk_person_org_id".to_string(),
                "auto_fk_person_project_id".to_string(),
                "auto_fk_project_org_id".to_string(),
            ]
        );

        assert!(
            !out.sample_result.is_empty(),
            "sample Cypher query must return rows"
        );
        assert!(
            out.sample_query.contains("ORDER BY score DESC"),
            "sample query must rank by score, got {}",
            out.sample_query
        );
        let scores: Vec<f64> = (0..out.sample_result.len())
            .map(|i| match out.sample_result.get(i, "score") {
                Some(Value::Float(f)) => *f,
                other => panic!("score col should be Float, got {other:?}"),
            })
            .collect();
        let distinct: std::collections::BTreeSet<u64> =
            scores.iter().map(|s| s.to_bits()).collect();
        assert!(
            distinct.len() >= 2,
            "sample results must be visibly ranked, got {scores:?}"
        );
        for w in scores.windows(2) {
            assert!(
                w[0] >= w[1],
                "scores must be non-increasing, got {scores:?}"
            );
        }
        assert!(
            !out.explanations.is_empty(),
            "explain(person-01, proj-01) must find the derived edges"
        );

        let err = run_demo(&dir).expect_err("second run into the same dir");
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("not empty") || msg.contains("non-empty") || msg.contains("non empty"),
            "refuse message must mention non-empty dir, got {err}"
        );
        assert!(
            msg.contains("hidden"),
            "refuse message must mention hidden files, got {err}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn format_stats_contains_counts() {
        let dir = tmp("stats-smoke");
        let out = run_demo(&dir).expect("demo for stats smoke");
        let text = format_stats(&out.stats);
        assert!(
            text.contains("60"),
            "stats output should include live node count, got:\n{text}"
        );
        assert!(
            text.contains("170"),
            "stats output should include edge count, got:\n{text}"
        );
        assert!(
            text.to_lowercase().contains("node"),
            "stats output should mention nodes, got:\n{text}"
        );
        assert!(
            text.to_lowercase().contains("edge"),
            "stats output should mention edges, got:\n{text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
