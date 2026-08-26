//! `mushroomdb` CLI library: hand-rolled arg parsing and the demo dataset builder.
//!
//! The binary in `main.rs` stays thin — it dispatches on [`parse_args`] and
//! prints what the lib functions return.

use core_api::{
    default_max_edges, is_write_query, wal_commit_count_at, AlgoDir, DegreeConfig, Explanation,
    GraphDb, IngestOptions, PageRankConfig, Predicate, ResultSet, RuleDef, RuleSuggestion,
    SharedDb, SnapshotOptions, Stats, Value, WccConfig,
};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Deterministic demo: 10 Orgs, 20 Projects, 30 People.
pub const N_ORGS: usize = 10;
pub const N_PROJECTS: usize = 20;
pub const N_PEOPLE: usize = 30;

/// Sample query printed by `mushroomdb demo` and executed against the fresh store.
///
/// Scoped to one person so `ORDER BY score DESC` is visibly ranked (a global
/// `LIMIT 5` would be five 1.0 home-project hits).
pub const SAMPLE_QUERY: &str = "\
MATCH (p:Person {id: 'person-01'})-[r:FIT]->(proj:Project)
RETURN p, proj, r.score AS score
ORDER BY score DESC, proj";

const SAMPLE_EXPLAIN_A: &str = "person-01";
const SAMPLE_EXPLAIN_B: &str = "proj-01";

/// How `serve` should mount a UI. Precedence: `--ui dir` > embedded > `--no-ui`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServeUi {
    Filesystem(PathBuf),
    Embedded,
    None,
}

/// Algorithm subcommand for `mushroomdb algo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlgoSubcmd {
    Pagerank,
    Wcc,
    Degree,
}

/// Parsed `mushroomdb` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Serve {
        db_dir: PathBuf,
        addr: SocketAddr,
        ui: ServeUi,
        /// If the db dir is missing or empty, run [`run_demo`] before serving.
        /// Docker's default CMD uses this so a fresh volume is ready on first boot.
        demo_if_empty: bool,
        /// Bearer token for non-loopback binds. Loopback may omit it.
        token: Option<String>,
        /// Periodic snapshot cadence. `None` = off (default).
        snapshot_every: Option<Duration>,
    },
    Mcp {
        db_dir: PathBuf,
    },
    Stats {
        db_dir: PathBuf,
    },
    Demo {
        db_dir: PathBuf,
    },
    /// Read-only view of the database at a past commit.
    AsOf {
        db_dir: PathBuf,
        /// 0-based WAL commit index to replay up to (inclusive).
        commit: u64,
        /// Optional Cypher read query to execute against the as-of view.
        query: Option<String>,
    },
    /// Profile the database and suggest linking rules with estimated edge counts.
    Suggest {
        db_dir: PathBuf,
    },
    /// Run a graph algorithm (pagerank / wcc / degree).
    Algo {
        db_dir: PathBuf,
        subcmd: AlgoSubcmd,
        /// Print only the top N results (0 = all).
        top: usize,
    },
    /// Run a Cypher query (read or write).
    Query {
        db_dir: PathBuf,
        /// Positional after dir (remaining args joined), or `--query`.
        cypher: String,
    },
    /// Write `snapshot.bin` (default truncates WAL unless `--keep-wal`).
    Snapshot {
        db_dir: PathBuf,
        keep_wal: bool,
    },
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
    /// First suggestion from the rule suggester (teaser only — not auto-applied).
    pub suggestion: Option<RuleSuggestion>,
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
mushroomdb — embedded graph database

Usage:
  mushroomdb serve <db-dir> [--addr 127.0.0.1:8080] [--token <secret>] [--ui <dist-dir>] [--no-ui] [--demo-if-empty] [--snapshot-every <secs>]
  mushroomdb mcp <db-dir>
  mushroomdb stats <db-dir>
  mushroomdb demo <db-dir>
  mushroomdb suggest <db-dir>
  mushroomdb asof <db-dir> --commit N [--query \"MATCH ...\"]
  mushroomdb query <db-dir> [--query \"MATCH ...\"] <cypher…>
  mushroomdb snapshot <db-dir> [--keep-wal]
  mushroomdb algo pagerank <db-dir> [--top N]
  mushroomdb algo wcc <db-dir> [--top N]
  mushroomdb algo degree <db-dir> [--top N]
  mushroomdb --help

Default serve address is 127.0.0.1:8080. Non-loopback --addr requires --token or MUSHROOMDB_TOKEN.
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
        "suggest" => parse_one_dir("suggest", &args[1..]).map(|db_dir| Command::Suggest { db_dir }),
        "asof" => parse_asof(&args[1..]),
        "algo" => parse_algo(&args[1..]),
        "query" => parse_query(&args[1..]),
        "snapshot" => parse_snapshot(&args[1..]),
        other => Err(format!("unknown command: {other}")),
    }
}

fn default_addr() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 8080))
}

fn parse_serve(args: &[&str]) -> Result<Command, String> {
    let mut db_dir = None;
    let mut addr = default_addr();
    let mut ui = ServeUi::Embedded;
    let mut saw_ui = false;
    let mut saw_no_ui = false;
    let mut demo_if_empty = false;
    let mut token = None;
    let mut snapshot_every = None;
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
        } else if a == "--ui" {
            let val = args
                .get(i + 1)
                .copied()
                .ok_or_else(|| "missing value for --ui".to_string())?;
            ui = ServeUi::Filesystem(PathBuf::from(val));
            saw_ui = true;
            i += 2;
        } else if let Some(val) = a.strip_prefix("--ui=") {
            ui = ServeUi::Filesystem(PathBuf::from(val));
            saw_ui = true;
            i += 1;
        } else if a == "--no-ui" {
            ui = ServeUi::None;
            saw_no_ui = true;
            i += 1;
        } else if a == "--demo-if-empty" {
            demo_if_empty = true;
            i += 1;
        } else if a == "--token" {
            let val = args
                .get(i + 1)
                .copied()
                .ok_or_else(|| "missing value for --token".to_string())?;
            token = Some(val.to_string());
            i += 2;
        } else if let Some(val) = a.strip_prefix("--token=") {
            token = Some(val.to_string());
            i += 1;
        } else if a == "--snapshot-every" {
            let val = args
                .get(i + 1)
                .copied()
                .ok_or_else(|| "missing value for --snapshot-every".to_string())?;
            snapshot_every = Some(parse_snapshot_every(val)?);
            i += 2;
        } else if let Some(val) = a.strip_prefix("--snapshot-every=") {
            snapshot_every = Some(parse_snapshot_every(val)?);
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
    if saw_ui && saw_no_ui {
        return Err("cannot combine --ui and --no-ui".to_string());
    }
    let db_dir = db_dir.ok_or_else(|| "serve requires <db-dir>".to_string())?;
    Ok(Command::Serve {
        db_dir,
        addr,
        ui,
        demo_if_empty,
        token,
        snapshot_every,
    })
}

fn parse_snapshot_every(val: &str) -> Result<Duration, String> {
    let secs: u64 = val
        .parse()
        .map_err(|_| format!("invalid --snapshot-every: {val}"))?;
    if secs == 0 {
        return Err("--snapshot-every must be a positive number of seconds".into());
    }
    Ok(Duration::from_secs(secs))
}

/// `--ui <dir>` must be a directory that contains `index.html`.
pub fn validate_ui_dir(dir: &Path) -> Result<PathBuf, String> {
    if !dir.is_dir() {
        return Err(format!("--ui directory does not exist: {}", dir.display()));
    }
    let index = dir.join("index.html");
    if !index.is_file() {
        return Err(format!(
            "--ui directory is missing index.html: {}",
            dir.display()
        ));
    }
    Ok(dir.to_path_buf())
}

fn parse_asof(args: &[&str]) -> Result<Command, String> {
    let mut db_dir = None;
    let mut commit: Option<u64> = None;
    let mut query: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let a = args[i];
        if a == "--commit" {
            let val = args
                .get(i + 1)
                .copied()
                .ok_or_else(|| "missing value for --commit".to_string())?;
            commit = Some(
                val.parse()
                    .map_err(|_| format!("invalid commit index: {val}"))?,
            );
            i += 2;
        } else if let Some(val) = a.strip_prefix("--commit=") {
            commit = Some(
                val.parse()
                    .map_err(|_| format!("invalid commit index: {val}"))?,
            );
            i += 1;
        } else if a == "--query" {
            let val = args
                .get(i + 1)
                .copied()
                .ok_or_else(|| "missing value for --query".to_string())?;
            query = Some(val.to_string());
            i += 2;
        } else if let Some(val) = a.strip_prefix("--query=") {
            query = Some(val.to_string());
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
    let db_dir = db_dir.ok_or_else(|| "asof requires <db-dir>".to_string())?;
    let commit = commit.ok_or_else(|| "asof requires --commit N".to_string())?;
    Ok(Command::AsOf {
        db_dir,
        commit,
        query,
    })
}

/// Execute an as-of query at the given commit and print results.
pub fn run_asof(db_dir: &Path, commit: u64, query: Option<&str>) -> Result<String, CliError> {
    let total = wal_commit_count_at(db_dir)?;
    let db = GraphDb::open_at(db_dir, commit)?;
    let mut out = String::new();
    let _ = writeln!(out, "as-of commit {} of {}", commit, total);
    if let Some(cypher) = query {
        let params = BTreeMap::new();
        let rs = db.query(cypher, &params)?;
        out.push_str(&format_result_set(&rs));
    }
    Ok(out)
}

fn parse_query(args: &[&str]) -> Result<Command, String> {
    let mut db_dir = None;
    let mut query_flag: Option<String> = None;
    let mut cypher_parts: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i];
        if a == "--query" {
            let val = args
                .get(i + 1)
                .copied()
                .ok_or_else(|| "missing value for --query".to_string())?;
            query_flag = Some(val.to_string());
            i += 2;
        } else if let Some(val) = a.strip_prefix("--query=") {
            query_flag = Some(val.to_string());
            i += 1;
        } else if a.starts_with('-') {
            return Err(format!("unexpected flag: {a}"));
        } else if db_dir.is_none() {
            db_dir = Some(PathBuf::from(a));
            i += 1;
        } else {
            cypher_parts.push(a);
            i += 1;
        }
    }
    let db_dir = db_dir.ok_or_else(|| "query requires <db-dir>".to_string())?;
    let cypher = if let Some(q) = query_flag {
        if !cypher_parts.is_empty() {
            return Err(
                "query: pass Cypher as remaining arguments or --query, not both".to_string(),
            );
        }
        q
    } else {
        if cypher_parts.is_empty() {
            return Err("query requires a Cypher string".to_string());
        }
        cypher_parts.join(" ")
    };
    Ok(Command::Query { db_dir, cypher })
}

/// Run a Cypher read or write and print columns/rows like [`run_asof`].
pub fn run_query(db_dir: &Path, cypher: &str) -> Result<String, CliError> {
    let params = BTreeMap::new();
    let is_write = is_write_query(cypher).map_err(CliError)?;
    let rs = if is_write {
        let mut db = GraphDb::open(db_dir)?;
        db.query_write(cypher, &params)?
    } else {
        let db = GraphDb::open(db_dir)?;
        db.query(cypher, &params)?
    };
    Ok(format_result_set(&rs))
}

fn parse_snapshot(args: &[&str]) -> Result<Command, String> {
    let mut db_dir = None;
    let mut keep_wal = false;
    let mut i = 0;
    while i < args.len() {
        let a = args[i];
        if a == "--keep-wal" {
            keep_wal = true;
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
    let db_dir = db_dir.ok_or_else(|| "snapshot requires <db-dir>".to_string())?;
    Ok(Command::Snapshot { db_dir, keep_wal })
}

/// Open `dir` and write `snapshot.bin`. Default truncates the WAL.
pub fn run_snapshot(db_dir: &Path, keep_wal: bool) -> Result<String, CliError> {
    let mut db = GraphDb::open(db_dir)?;
    if keep_wal {
        db.snapshot_with(SnapshotOptions { keep_wal: true })?;
    } else {
        db.snapshot()?;
    }
    Ok(format!(
        "snapshot written: {}\n",
        db_dir.join("snapshot.bin").display()
    ))
}

fn format_result_set(rs: &ResultSet) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "columns: {}", rs.columns().join(", "));
    for i in 0..rs.len() {
        let cells: Vec<String> = rs
            .columns()
            .iter()
            .map(|c| format!("{c}={}", fmt_cell(rs.get(i, c))))
            .collect();
        let _ = writeln!(out, "  {}", cells.join("  "));
    }
    out
}

fn parse_algo(args: &[&str]) -> Result<Command, String> {
    if args.is_empty() {
        return Err("algo requires a subcommand: pagerank | wcc | degree".to_string());
    }
    let subcmd = match args[0] {
        "pagerank" => AlgoSubcmd::Pagerank,
        "wcc" => AlgoSubcmd::Wcc,
        "degree" => AlgoSubcmd::Degree,
        other => {
            return Err(format!(
                "unknown algo subcommand: {other}; expected pagerank | wcc | degree"
            ))
        }
    };
    let rest = &args[1..];
    let mut db_dir = None;
    let mut top: usize = 20;
    let mut i = 0;
    while i < rest.len() {
        let a = rest[i];
        if a == "--top" {
            let val = rest
                .get(i + 1)
                .copied()
                .ok_or_else(|| "missing value for --top".to_string())?;
            top = val
                .parse()
                .map_err(|_| format!("--top must be a non-negative integer, got {val}"))?;
            i += 2;
        } else if let Some(val) = a.strip_prefix("--top=") {
            top = val
                .parse()
                .map_err(|_| format!("--top must be a non-negative integer, got {val}"))?;
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
    let db_dir = db_dir.ok_or_else(|| format!("algo {} requires <db-dir>", args[0]))?;
    Ok(Command::Algo {
        db_dir,
        subcmd,
        top,
    })
}

/// Run a graph algorithm and return a formatted string.
pub fn run_algo(db_dir: &Path, subcmd: &AlgoSubcmd, top: usize) -> Result<String, CliError> {
    let db = GraphDb::open(db_dir)?;
    match subcmd {
        AlgoSubcmd::Pagerank => {
            let config = PageRankConfig::default();
            let report = db.pagerank(&config);
            Ok(format_pagerank(&report, top))
        }
        AlgoSubcmd::Wcc => {
            let config = WccConfig::default();
            let report = db.connected_components(&config);
            Ok(format_wcc(&report, top))
        }
        AlgoSubcmd::Degree => {
            let config = DegreeConfig {
                direction: AlgoDir::Both,
                ..DegreeConfig::default()
            };
            let report = db.degree_centrality(&config);
            Ok(format_degree(&report, top))
        }
    }
}

fn format_pagerank(report: &core_api::PageRankReport, top: usize) -> String {
    let mut buf = String::new();
    let _ = writeln!(buf, "== pagerank (converged={}) ==", report.converged);
    let rows = if top == 0 {
        report.scores.as_slice()
    } else {
        &report.scores[..top.min(report.scores.len())]
    };
    for (i, (key, score)) in rows.iter().enumerate() {
        let _ = writeln!(buf, "  {:>4}  {:<40}  {:.6}", i + 1, key, score);
    }
    buf
}

fn format_wcc(report: &core_api::WccReport, top: usize) -> String {
    let mut buf = String::new();
    let _ = writeln!(buf, "== wcc (truncated={}) ==", report.truncated);
    let rows = if top == 0 {
        report.components.as_slice()
    } else {
        &report.components[..top.min(report.components.len())]
    };
    for (key, comp_id) in rows {
        let _ = writeln!(buf, "  {:<40}  component={}", key, comp_id);
    }
    buf
}

fn format_degree(report: &core_api::DegreeReport, top: usize) -> String {
    let mut buf = String::new();
    let _ = writeln!(
        buf,
        "== degree centrality (truncated={}) ==",
        report.truncated
    );
    let rows = if top == 0 {
        report.scores.as_slice()
    } else {
        &report.scores[..top.min(report.scores.len())]
    };
    for (i, (key, deg)) in rows.iter().enumerate() {
        let _ = writeln!(buf, "  {:>4}  {:<40}  degree={}", i + 1, key, deg);
    }
    buf
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

/// Pretty-print [`Stats`] for `mushroomdb stats` and the demo smoke test.
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
/// then declares `skill_fit` plus the three Predicates II rules.
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
        let skill_fit = Predicate::Overlap {
            field: "skills".into(),
            min: 0.5,
        };
        let skill_fit_k = Some(default_max_edges(&skill_fit));
        w.create_rule(RuleDef {
            name: "skill_fit".into(),
            src_label: "Person".into(),
            dst_label: "Project".into(),
            predicate: skill_fit,
            edge_type: "FIT".into(),
            weight_prop: Some("score".into()),
            max_edges: skill_fit_k,
            approximate: false,
        })?;
        let founded_within = Predicate::NumericWithin {
            field: "founded_year".into(),
            tolerance: 2.0,
        };
        let founded_within_k = Some(default_max_edges(&founded_within));
        w.create_rule(RuleDef {
            name: "founded_within".into(),
            src_label: "Org".into(),
            dst_label: "Org".into(),
            predicate: founded_within,
            edge_type: "FOUNDED_WITHIN".into(),
            weight_prop: Some("score".into()),
            max_edges: founded_within_k,
            approximate: false,
        })?;
        let nearby_office = Predicate::GeoRadius {
            field: "office".into(),
            km: 50.0,
        };
        let nearby_office_k = Some(default_max_edges(&nearby_office));
        w.create_rule(RuleDef {
            name: "nearby_office".into(),
            src_label: "Org".into(),
            dst_label: "Org".into(),
            predicate: nearby_office,
            edge_type: "NEARBY_OFFICE".into(),
            weight_prop: Some("score".into()),
            max_edges: nearby_office_k,
            approximate: false,
        })?;
        let similar_interests = Predicate::VectorSimilar {
            field: "embedding".into(),
            min: 0.8,
        };
        let similar_interests_k = Some(default_max_edges(&similar_interests));
        w.create_rule(RuleDef {
            name: "similar_interests".into(),
            src_label: "Person".into(),
            dst_label: "Person".into(),
            predicate: similar_interests,
            edge_type: "SIMILAR".into(),
            weight_prop: Some("score".into()),
            max_edges: similar_interests_k,
            approximate: false,
        })?;
    }

    let r = db.read();
    let sample_result = r.query(SAMPLE_QUERY, &BTreeMap::new())?;
    let explanations = r.explain(SAMPLE_EXPLAIN_A, SAMPLE_EXPLAIN_B)?;
    let stats = r.stats();
    // Rule suggestion teaser: first suggestion sorted by est_edges desc.
    let suggestion = r.suggest_rules().into_iter().next();

    Ok(DemoOutcome {
        auto_fk_rules,
        sample_query: SAMPLE_QUERY.to_string(),
        sample_result,
        explanations,
        stats,
        suggestion,
    })
}

fn dir_is_empty_or_absent(dir: &Path) -> Result<bool, CliError> {
    if dir.is_file() {
        return Err(CliError(format!(
            "demo refuses a non-empty directory: {} is a file",
            dir.display()
        )));
    }
    if !dir.exists() {
        return Ok(true);
    }
    Ok(std::fs::read_dir(dir)?.next().is_none())
}

fn refuse_non_empty(dir: &Path) -> Result<(), CliError> {
    if dir_is_empty_or_absent(dir)? {
        Ok(())
    } else {
        Err(CliError(format!(
            "demo refuses a non-empty directory: {} \
             (directory must be empty — including hidden files)",
            dir.display()
        )))
    }
}

/// Run [`run_demo`] when `dir` is missing or empty; otherwise leave it alone.
pub fn maybe_run_demo_if_empty(dir: &Path) -> Result<Option<DemoOutcome>, CliError> {
    if dir_is_empty_or_absent(dir)? {
        Ok(Some(run_demo(dir)?))
    } else {
        Ok(None)
    }
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

/// Real city [lat, lon] for org `i` (1-based). Four clusters sit inside 50 km:
/// NYC / Jersey City / Newark, SF / Oakland / Berkeley, London / Greenwich,
/// Paris / Versailles.
fn org_office(i: usize) -> (f64, f64) {
    match i {
        1 => (40.7128, -74.0060),  // New York
        2 => (48.8566, 2.3522),    // Paris
        3 => (51.5074, -0.1278),   // London
        4 => (37.7749, -122.4194), // San Francisco
        5 => (37.8044, -122.2711), // Oakland
        6 => (37.8715, -122.2730), // Berkeley
        7 => (40.7178, -74.0431),  // Jersey City
        8 => (51.4769, 0.0005),    // Greenwich
        9 => (48.8014, 2.1301),    // Versailles
        10 => (40.7357, -74.1724), // Newark
        _ => unreachable!("demo orgs are 1..=10"),
    }
}

/// Dim-8 embedding for person `i`. Groups of three share a unit axis (cos = 1);
/// two extra groups are (0.8, 0.6, …) and (0.6, 0.8, …) so cos = 0.8 / 0.96
/// against the first two axes is hand-checkable.
fn person_embedding_json(i: usize) -> String {
    let mut v = [0.0_f64; 8];
    match i {
        9 | 19 | 29 => {
            v[0] = 0.8;
            v[1] = 0.6;
        }
        10 | 20 | 30 => {
            v[0] = 0.6;
            v[1] = 0.8;
        }
        _ => {
            let axis = (i - 1) % 10;
            debug_assert!(axis < 8);
            v[axis] = 1.0;
        }
    }
    let parts: Vec<String> = v.iter().map(|x| format!("{x}")).collect();
    format!("[{}]", parts.join(","))
}

fn org_json() -> String {
    json_array((1..=N_ORGS).map(|i| {
        let year = 2010 + (i as i64 - 1);
        let (lat, lon) = org_office(i);
        format!(
            r#"{{"id":"org-{i:02}","name":"Org {i}","founded_year":{year},"office":[{lat},{lon}],"skills":{}}}"#,
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
            r#"{{"id":"person-{i:02}","name":"Person {i}","org_id":"org-{org:02}","project_id":"proj-{proj:02}","embedding":{},"skills":{}}}"#,
            person_embedding_json(i),
            skill_window_json(proj, 3)
        )
    }))
}

/// Render a [`DemoOutcome`] the way `mushroomdb demo` prints it.
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
    let _ = writeln!(
        buf,
        "numeric rule: founded_within (Org.founded_year, tolerance 2)"
    );
    let _ = writeln!(buf, "geo rule: nearby_office (Org.office [lat,lon], 50 km)");
    let _ = writeln!(
        buf,
        "vector rule: similar_interests (Person.embedding dim 8, min 0.8)"
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
    let _ = writeln!(buf, "  mushroomdb serve {}", dir.display());

    // Teaser: one suggestion from the rule suggester (not auto-applied).
    if let Some(s) = &out.suggestion {
        let _ = writeln!(buf);
        let _ = writeln!(buf, "== suggested rule (teaser) ==");
        let _ = writeln!(buf, "  {}", s.def.name);
        let _ = writeln!(
            buf,
            "  {} → {} via {:?}",
            s.def.src_label, s.def.dst_label, s.def.predicate
        );
        let _ = writeln!(buf, "  est_edges: ~{}", s.est_edges);
        let _ = writeln!(buf, "  {}", s.rationale);
        let _ = writeln!(
            buf,
            "  (run `mushroomdb suggest {}` for full analysis)",
            dir.display()
        );
    }

    buf
}

/// Profile the database at `dir` and return all rule suggestions.
pub fn run_suggest(dir: &Path) -> Result<Vec<RuleSuggestion>, CliError> {
    let db = GraphDb::open(dir)?;
    Ok(db.suggest_rules())
}

/// Pretty-print a list of [`RuleSuggestion`]s for `mushroomdb suggest`.
pub fn format_suggest(suggestions: &[RuleSuggestion]) -> String {
    let mut buf = String::new();
    if suggestions.is_empty() {
        let _ = writeln!(
            buf,
            "no rule suggestions (database may be empty or rules already cover all patterns)"
        );
        return buf;
    }
    let _ = writeln!(buf, "== rule suggestions ({}) ==", suggestions.len());
    for (i, s) in suggestions.iter().enumerate() {
        let _ = writeln!(buf);
        let _ = writeln!(buf, "[{}] {}", i + 1, s.def.name);
        let _ = writeln!(
            buf,
            "    {} → {}  via {:?}",
            s.def.src_label, s.def.dst_label, s.def.predicate
        );
        let _ = writeln!(buf, "    est_edges : ~{}", s.est_edges);
        let _ = writeln!(buf, "    rationale : {}", s.rationale);
        if !s.examples.is_empty() {
            let _ = writeln!(buf, "    examples  :");
            for (src, dst, score) in &s.examples {
                let _ = writeln!(buf, "      {src} → {dst}  score={score:.4}");
            }
        }
        let _ = writeln!(buf, "    predicate : {:?}", s.def.predicate);
        let _ = writeln!(
            buf,
            "    to apply  : POST /rules  or  db.create_rule(suggestion.def)"
        );
    }
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
    use std::collections::BTreeSet;
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

    fn directed_pairs(db: &SharedDb, etype: &str) -> BTreeSet<(String, String)> {
        let g = db.read();
        let mut out = BTreeSet::new();
        for i in 1..=N_ORGS {
            let src = format!("org-{i:02}");
            if let Ok(nbrs) = g.neighbors(&src, etype, core_api::Direction::Out) {
                for dst in nbrs {
                    out.insert((src.clone(), dst));
                }
            }
        }
        for i in 1..=N_PEOPLE {
            let src = format!("person-{i:02}");
            if let Ok(nbrs) = g.neighbors(&src, etype, core_api::Direction::Out) {
                for dst in nbrs {
                    out.insert((src.clone(), dst));
                }
            }
        }
        out
    }

    fn assert_weight(db: &SharedDb, a: &str, b: &str, rule: &str, want: f64) {
        let hits: Vec<_> = db
            .read()
            .explain(a, b)
            .expect("explain")
            .into_iter()
            .filter(|e| e.rule == rule && e.src_key == a && e.dst_key == b)
            .collect();
        assert_eq!(hits.len(), 1, "explain {a}/{b} rule={rule}: {hits:?}");
        let got = hits[0].weight.expect("weighted");
        assert!(
            (got - want).abs() < 1e-12,
            "{rule} {a}→{b}: got {got} want {want}"
        );
    }

    fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
        const R: f64 = 6371.0088;
        let phi1 = lat1.to_radians();
        let phi2 = lat2.to_radians();
        let dphi = (lat2 - lat1).to_radians();
        let dlam = (lon2 - lon1).to_radians();
        let a = ((dphi / 2.0).sin().powi(2) + phi1.cos() * phi2.cos() * (dlam / 2.0).sin().powi(2))
            .clamp(0.0, 1.0);
        let c = 2.0 * a.sqrt().atan2((1.0 - a).sqrt());
        R * c
    }

    fn default_bind() -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 8080))
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
                    Ok(Command::Serve {
                        db_dir,
                        addr,
                        ui,
                        demo_if_empty,
                        token,
                        snapshot_every,
                    }) => {
                        assert_eq!(db_dir, PathBuf::from("/tmp/demo-db"));
                        assert_eq!(addr, default_bind());
                        assert_eq!(ui, super::ServeUi::Embedded);
                        assert!(!demo_if_empty);
                        assert_eq!(token, None);
                        assert_eq!(snapshot_every, None);
                    }
                    other => panic!("serve <dir> → Serve default addr, got {other:?}"),
                },
            },
            Case {
                args: &["serve", "/tmp/demo-db", "--addr", "127.0.0.1:8080"],
                check: |r| match r {
                    Ok(Command::Serve {
                        db_dir,
                        addr,
                        ui,
                        demo_if_empty,
                        token,
                        snapshot_every,
                    }) => {
                        assert_eq!(db_dir, PathBuf::from("/tmp/demo-db"));
                        assert_eq!(addr, "127.0.0.1:8080".parse().unwrap());
                        assert_eq!(ui, super::ServeUi::Embedded);
                        assert!(!demo_if_empty);
                        assert_eq!(token, None);
                        assert_eq!(snapshot_every, None);
                    }
                    other => panic!("serve --addr after dir, got {other:?}"),
                },
            },
            Case {
                args: &["serve", "/tmp/demo-db", "--addr=127.0.0.1:9090"],
                check: |r| match r {
                    Ok(Command::Serve {
                        db_dir,
                        addr,
                        ui,
                        demo_if_empty,
                        token,
                        snapshot_every,
                    }) => {
                        assert_eq!(db_dir, PathBuf::from("/tmp/demo-db"));
                        assert_eq!(addr, "127.0.0.1:9090".parse().unwrap());
                        assert_eq!(ui, super::ServeUi::Embedded);
                        assert!(!demo_if_empty);
                        assert_eq!(token, None);
                        assert_eq!(snapshot_every, None);
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
                args: &["serve", "/tmp/demo-db", "--ui", "/tmp/ui-dist"],
                check: |r| match r {
                    Ok(Command::Serve { ui, .. }) => {
                        assert_eq!(
                            ui,
                            super::ServeUi::Filesystem(PathBuf::from("/tmp/ui-dist"))
                        );
                    }
                    other => panic!("serve --ui <dir>, got {other:?}"),
                },
            },
            Case {
                args: &["serve", "/tmp/demo-db", "--ui=/tmp/ui-eq"],
                check: |r| match r {
                    Ok(Command::Serve { ui, .. }) => {
                        assert_eq!(ui, super::ServeUi::Filesystem(PathBuf::from("/tmp/ui-eq")));
                    }
                    other => panic!("serve --ui=VALUE, got {other:?}"),
                },
            },
            Case {
                args: &["serve", "/tmp/demo-db", "--ui"],
                check: |r| {
                    let e = r.expect_err("--ui missing value");
                    assert!(
                        e.to_lowercase().contains("ui"),
                        "--ui missing value should mention ui, got {e}"
                    );
                },
            },
            Case {
                args: &["serve", "/tmp/demo-db", "--no-ui"],
                check: |r| match r {
                    Ok(Command::Serve { ui, .. }) => {
                        assert_eq!(ui, super::ServeUi::None);
                    }
                    other => panic!("serve --no-ui, got {other:?}"),
                },
            },
            Case {
                args: &["serve", "/tmp/demo-db", "--ui", "/tmp/x", "--no-ui"],
                check: |r| {
                    let e = r.expect_err("combine --ui and --no-ui");
                    assert!(
                        e.contains("--ui") && e.contains("--no-ui"),
                        "conflict should name both flags, got {e}"
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
            Case {
                args: &[
                    "serve",
                    "/data",
                    "--addr",
                    "0.0.0.0:8080",
                    "--demo-if-empty",
                ],
                check: |r| match r {
                    Ok(Command::Serve {
                        db_dir,
                        addr,
                        demo_if_empty,
                        ui,
                        token,
                        snapshot_every,
                    }) => {
                        assert_eq!(db_dir, PathBuf::from("/data"));
                        assert_eq!(addr, "0.0.0.0:8080".parse().unwrap());
                        assert!(demo_if_empty);
                        assert_eq!(ui, super::ServeUi::Embedded);
                        assert_eq!(token, None);
                        assert_eq!(snapshot_every, None);
                    }
                    other => panic!("serve --demo-if-empty docker default, got {other:?}"),
                },
            },
        ];

        for case in &cases {
            (case.check)(parse_args(case.args));
        }
    }

    #[test]
    fn serve_default_addr_is_loopback_8080() {
        match parse_args(&["serve", "/tmp/db"]).unwrap() {
            Command::Serve { addr, .. } => {
                assert_eq!(addr, "127.0.0.1:8080".parse().unwrap());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn serve_snapshot_every_parses_seconds() {
        match parse_args(&["serve", "/tmp/db", "--snapshot-every", "30"]).unwrap() {
            Command::Serve { snapshot_every, .. } => {
                assert_eq!(snapshot_every, Some(Duration::from_secs(30)));
            }
            other => panic!("{other:?}"),
        }
        match parse_args(&["serve", "/tmp/db", "--snapshot-every=5"]).unwrap() {
            Command::Serve { snapshot_every, .. } => {
                assert_eq!(snapshot_every, Some(Duration::from_secs(5)));
            }
            other => panic!("{other:?}"),
        }
        match parse_args(&["serve", "/tmp/db"]).unwrap() {
            Command::Serve { snapshot_every, .. } => {
                assert_eq!(snapshot_every, None);
            }
            other => panic!("{other:?}"),
        }
        let err = parse_args(&["serve", "/tmp/db", "--snapshot-every"]).unwrap_err();
        assert!(
            err.contains("snapshot-every"),
            "missing value should name the flag, got {err}"
        );
        let err = parse_args(&["serve", "/tmp/db", "--snapshot-every", "0"]).unwrap_err();
        assert!(
            err.contains("snapshot-every"),
            "zero should be rejected, got {err}"
        );
        let err = parse_args(&["serve", "/tmp/db", "--snapshot-every", "nope"]).unwrap_err();
        assert!(
            err.contains("snapshot-every"),
            "invalid value should name the flag, got {err}"
        );
    }

    #[test]
    fn serve_token_flag_and_non_loopback_without_token_is_parsed() {
        // parse succeeds; main() enforces the bind rule. Token is stored.
        match parse_args(&[
            "serve",
            "/tmp/db",
            "--addr",
            "0.0.0.0:8080",
            "--token",
            "s3cret",
        ])
        .unwrap()
        {
            Command::Serve { token, addr, .. } => {
                assert_eq!(token.as_deref(), Some("s3cret"));
                assert_eq!(addr.ip().to_string(), "0.0.0.0");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parse_snapshot_and_query() {
        match parse_args(&["snapshot", "/tmp/db"]).unwrap() {
            Command::Snapshot { keep_wal, .. } => assert!(!keep_wal),
            other => panic!("{other:?}"),
        }
        match parse_args(&["snapshot", "/tmp/db", "--keep-wal"]).unwrap() {
            Command::Snapshot { keep_wal, .. } => assert!(keep_wal),
            other => panic!("{other:?}"),
        }
        match parse_args(&["query", "/tmp/db", "MATCH (n) RETURN n LIMIT 1"]).unwrap() {
            Command::Query { cypher, .. } => assert!(cypher.contains("MATCH")),
            other => panic!("{other:?}"),
        }
        match parse_args(&["query", "/tmp/db", "MATCH", "(n)", "RETURN", "n"]).unwrap() {
            Command::Query { cypher, .. } => assert_eq!(cypher, "MATCH (n) RETURN n"),
            other => panic!("{other:?}"),
        }
        match parse_args(&["query", "/tmp/db", "--query", "MATCH (n) RETURN n"]).unwrap() {
            Command::Query { cypher, .. } => assert_eq!(cypher, "MATCH (n) RETURN n"),
            other => panic!("{other:?}"),
        }
        let text = usage();
        assert!(
            text.contains("query"),
            "usage should mention query, got:\n{text}"
        );
        assert!(
            text.contains("snapshot"),
            "usage should mention snapshot, got:\n{text}"
        );
    }

    #[test]
    fn usage_lists_every_subcommand() {
        let text = usage();
        for word in [
            "serve",
            "mcp",
            "stats",
            "demo",
            "query",
            "snapshot",
            "--keep-wal",
            "mushroomdb",
            "--ui",
            "--no-ui",
            "--demo-if-empty",
            "--token",
            "--snapshot-every",
        ] {
            assert!(
                text.contains(word),
                "usage should mention {word}, got:\n{text}"
            );
        }
    }

    #[test]
    fn validate_ui_dir_requires_index_html() {
        let missing = tmp("ui-missing");
        let err = super::validate_ui_dir(&missing).expect_err("missing dir");
        assert!(
            err.contains("does not exist"),
            "missing dir error, got {err}"
        );

        let empty = tmp("ui-empty");
        std::fs::create_dir_all(&empty).unwrap();
        let err = super::validate_ui_dir(&empty).expect_err("no index");
        assert!(
            err.contains("index.html"),
            "missing index.html error, got {err}"
        );

        let ok = tmp("ui-ok");
        std::fs::create_dir_all(&ok).unwrap();
        std::fs::write(ok.join("index.html"), "<!doctype html>").unwrap();
        let got = super::validate_ui_dir(&ok).expect("valid ui dir");
        assert_eq!(got, ok);
    }

    #[test]
    fn maybe_run_demo_if_empty_seeds_then_skips() {
        let dir = tmp("boot-empty");
        let first = super::maybe_run_demo_if_empty(&dir)
            .expect("empty dir demos")
            .expect("Some(DemoOutcome)");
        assert_eq!(first.stats.nodes_live, 60);
        let db = SharedDb::open(&dir).expect("reopen");
        assert!(db.read().has_node("person-01"));
        let second = super::maybe_run_demo_if_empty(&dir).expect("non-empty is ok");
        assert!(
            second.is_none(),
            "second boot must not re-demo a populated volume"
        );

        let occupied = tmp("boot-occupied");
        std::fs::create_dir_all(&occupied).unwrap();
        std::fs::write(occupied.join("keep-me"), b"x").unwrap();
        let skipped = super::maybe_run_demo_if_empty(&occupied).expect("occupied skip");
        assert!(skipped.is_none());
        assert_eq!(
            std::fs::read(occupied.join("keep-me")).unwrap(),
            b"x",
            "existing volume contents must be untouched"
        );
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
        // founded_within: |year_i − year_j| ≤ 2 on 2010+(i-1) → 17 pairs × 2 = 34.
        // nearby_office: 4 city clusters (NYC/SF/London/Paris) → 8 pairs × 2 = 16.
        // similar_interests: dim-8 groups → 57 pairs × 2 = 114.
        // Total: 80 + 90 + 34 + 16 + 114 = 334.
        assert_eq!(out.stats.edges, 334);
        assert_eq!(
            out.stats.rules.len(),
            7,
            "3 auto-FK + overlap + numeric + geo + vector"
        );
        let fit = out
            .stats
            .rules
            .iter()
            .find(|r| r.name == "skill_fit")
            .expect("skill_fit");
        assert_eq!(fit.edges, 90, "30 people × 3 FIT edges");
        let founded = out
            .stats
            .rules
            .iter()
            .find(|r| r.name == "founded_within")
            .expect("founded_within");
        assert_eq!(founded.edges, 34);
        let nearby = out
            .stats
            .rules
            .iter()
            .find(|r| r.name == "nearby_office")
            .expect("nearby_office");
        assert_eq!(nearby.edges, 16);
        let similar = out
            .stats
            .rules
            .iter()
            .find(|r| r.name == "similar_interests")
            .expect("similar_interests");
        assert_eq!(similar.edges, 114);

        let mut names: Vec<&str> = out.stats.rules.iter().map(|r| r.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec![
                "auto_fk_person_org_id",
                "auto_fk_person_project_id",
                "auto_fk_project_org_id",
                "founded_within",
                "nearby_office",
                "similar_interests",
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

        let db = SharedDb::open(&dir).expect("reopen demo");
        assert_eq!(
            directed_pairs(&db, "FOUNDED_WITHIN"),
            [
                ("org-01", "org-02"),
                ("org-01", "org-03"),
                ("org-02", "org-01"),
                ("org-02", "org-03"),
                ("org-02", "org-04"),
                ("org-03", "org-01"),
                ("org-03", "org-02"),
                ("org-03", "org-04"),
                ("org-03", "org-05"),
                ("org-04", "org-02"),
                ("org-04", "org-03"),
                ("org-04", "org-05"),
                ("org-04", "org-06"),
                ("org-05", "org-03"),
                ("org-05", "org-04"),
                ("org-05", "org-06"),
                ("org-05", "org-07"),
                ("org-06", "org-04"),
                ("org-06", "org-05"),
                ("org-06", "org-07"),
                ("org-06", "org-08"),
                ("org-07", "org-05"),
                ("org-07", "org-06"),
                ("org-07", "org-08"),
                ("org-07", "org-09"),
                ("org-08", "org-06"),
                ("org-08", "org-07"),
                ("org-08", "org-09"),
                ("org-08", "org-10"),
                ("org-09", "org-07"),
                ("org-09", "org-08"),
                ("org-09", "org-10"),
                ("org-10", "org-08"),
                ("org-10", "org-09"),
            ]
            .into_iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect::<BTreeSet<_>>()
        );
        assert_eq!(
            directed_pairs(&db, "NEARBY_OFFICE"),
            [
                ("org-01", "org-07"),
                ("org-01", "org-10"),
                ("org-02", "org-09"),
                ("org-03", "org-08"),
                ("org-04", "org-05"),
                ("org-04", "org-06"),
                ("org-05", "org-04"),
                ("org-05", "org-06"),
                ("org-06", "org-04"),
                ("org-06", "org-05"),
                ("org-07", "org-01"),
                ("org-07", "org-10"),
                ("org-08", "org-03"),
                ("org-09", "org-02"),
                ("org-10", "org-01"),
                ("org-10", "org-07"),
            ]
            .into_iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect::<BTreeSet<_>>()
        );
        assert_weight(&db, "org-01", "org-02", "founded_within", 0.5);
        let nyc_jc = 1.0 - haversine_km(40.7128, -74.0060, 40.7178, -74.0431) / 50.0;
        assert_weight(&db, "org-01", "org-07", "nearby_office", nyc_jc);
        assert_weight(&db, "person-01", "person-11", "similar_interests", 1.0);
        assert_weight(&db, "person-01", "person-09", "similar_interests", 0.8);

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
    fn run_snapshot_writes_snapshot_bin() {
        let dir = tmp("snapshot-cli");
        {
            let mut db = GraphDb::open(&dir).expect("open");
            db.insert_node("Person", "alice", vec![]).expect("insert");
        }
        assert!(
            !dir.join("snapshot.bin").exists(),
            "GraphDb Drop must not snapshot"
        );
        let out = run_snapshot(&dir, false).expect("snapshot");
        assert!(
            dir.join("snapshot.bin").is_file(),
            "run_snapshot must write snapshot.bin"
        );
        assert!(
            out.contains("snapshot.bin"),
            "snapshot output should mention snapshot.bin, got {out}"
        );
        let db = GraphDb::open(&dir).expect("reopen");
        assert!(db.has_node("alice"), "reopen after snapshot must recover");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_query_formats_like_asof() {
        let dir = tmp("query-cli");
        {
            let mut db = GraphDb::open(&dir).expect("open");
            db.insert_node(
                "Person",
                "alice",
                vec![("id".into(), Value::Str("alice".into()))],
            )
            .expect("insert");
        }
        let out = run_query(&dir, "MATCH (n:Person) RETURN n.id AS id").expect("query");
        assert!(out.contains("columns:"), "got {out}");
        assert!(out.contains("id=alice"), "got {out}");
        let _ = run_query(&dir, "CREATE (n:Person {id: 'bob'})").expect("write");
        let db = GraphDb::open(&dir).expect("reopen");
        assert!(db.has_node("bob"), "query_write must persist CREATE");
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
            text.contains("334"),
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
