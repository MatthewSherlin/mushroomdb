//! Running one TCK scenario against a fresh store.
//!
//! Step meanings (the TCK's README, `tck/README.adoc`):
//!
//! | Step | Here |
//! |---|---|
//! | `an empty graph`, `any graph` | a fresh store |
//! | `the <name> graph` | a fresh store, then the named graph's `.cypher` file |
//! | `having executed` | run the query; an error fails the scenario as a setup failure |
//! | `parameters are` | the table's values become the query's parameters |
//! | `executing query`, `executing control query` | run the query; the result or error is kept |
//! | `the result should be, in any order` | the rows as a multiset, columns by name |
//! | `the result should be, in order` | the rows as a sequence |
//! | `… (ignoring element order for lists)` | lists compare as multisets |
//! | `the result should be empty` | no rows |
//! | `a <Type> should be raised at <phase>` | the query failed (any error) |
//! | `no side effects`, `the side effects should be` | the graph's observable change |
//!
//! A write statement goes through `GraphDb::query_write`, a read through
//! `GraphDb::query` (as `is_write_query` sorts them, the dispatch every
//! surface uses), each inside `with_entity_detail` so a node, relationship or
//! path cell carries the element behind it.
//!
//! Every step is checked even after one fails, so a report can tell a
//! scenario that fails for one reason from one that fails for several.

use crate::feature::Scenario;
use crate::skip::{skip_rule_for, SkipRule};
use crate::value::{self, from_cell, multiset_equal, values_equal, TckNode, TckValue};
use mushroomdb::{
    is_write_query, with_entity_detail, Entity, FsyncPolicy, GraphDb, RealFs, ResultSet, Value,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The vendored TCK directory (`vendor/opencypher-tck/tck`).
pub fn tck_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vendor/opencypher-tck/tck")
}

/// The columns of the one-row result a write statement without `RETURN`
/// gives (docs/site/query.md, Write statements).
pub const WRITE_COUNTER_COLUMNS: [&str; 4] = [
    "created",
    "properties_set",
    "deleted",
    "relationships_created",
];

/// A store in its own temporary directory, removed when dropped.
pub struct TempGraph {
    pub db: GraphDb<RealFs>,
    dir: PathBuf,
}

impl TempGraph {
    /// A fresh store with `FsyncPolicy::Relaxed`: a scenario never outlives
    /// the process, so nothing it writes needs to reach the disk.
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mushroomdb-tck-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create a TCK store directory");
        let mut db = GraphDb::open(&dir).expect("open a fresh TCK store");
        db.set_fsync_policy(FsyncPolicy::Relaxed);
        TempGraph { db, dir }
    }
}

impl Default for TempGraph {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TempGraph {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Why a scenario failed: the group it is counted under, and the detail.
#[derive(Debug, Clone)]
pub struct Failure {
    pub group: String,
    pub detail: String,
}

/// An expected error the engine did raise: what the TCK named and what the
/// engine said. A scenario passes on any error; these let a report show
/// whether the error was the right kind.
#[derive(Debug, Clone)]
pub struct RaisedError {
    pub expected_type: String,
    pub expected_phase: String,
    pub engine_message: String,
}

/// The engine stage an error message names: `lex`, `parse`, `plan`,
/// `execute`, or `other`.
pub fn error_stage(message: &str) -> &'static str {
    let m = message.strip_prefix("query error: ").unwrap_or(message);
    for stage in ["lex", "parse", "plan", "execute"] {
        if m.starts_with(stage) && m[stage.len()..].starts_with(':') {
            return stage;
        }
    }
    "other"
}

/// Whether the engine raised the error in the phase the TCK names: a
/// `compile time` error from lexing, parsing or planning, a `runtime` error
/// from execution, an `any time` error from either. An error that passes
/// only because some error was raised (a parse error for an unsupported
/// construct where the TCK expects a runtime `TypeError`) does not match.
pub fn error_phase_matches(raised: &RaisedError) -> bool {
    let stage = error_stage(&raised.engine_message);
    match raised.expected_phase.as_str() {
        "compile time" => matches!(stage, "lex" | "parse" | "plan"),
        "runtime" => stage == "execute",
        "any time" => stage != "other",
        _ => false,
    }
}

#[derive(Debug, Clone)]
pub enum Outcome {
    Pass(Option<RaisedError>),
    /// Every step failure, in step order; the first is the scenario's group.
    Fail(Vec<Failure>),
    Skip(&'static SkipRule),
}

impl Outcome {
    pub fn is_pass(&self) -> bool {
        matches!(self, Outcome::Pass(_))
    }
}

/// Scenarios that fail by a decision already taken for 0.10: they run and
/// count as failures, under a group that names the decision rather than the
/// symptom. `(feature file, scenario name prefix, decision)`.
pub const RULED_DEVIATIONS: &[(&str, &str, &str)] = &[
    (
        "expressions/boolean/Boolean1.feature",
        "[8] Fail on conjunction of at least one non-booleans",
        "0.10 keeps boolean coercion (AND/OR/XOR/NOT accept non-booleans)",
    ),
    (
        "expressions/boolean/Boolean2.feature",
        "[8] Fail on disjunction of at least one non-booleans",
        "0.10 keeps boolean coercion (AND/OR/XOR/NOT accept non-booleans)",
    ),
    (
        "expressions/boolean/Boolean3.feature",
        "[8] Fail on exclusive disjunction of at least one non-booleans",
        "0.10 keeps boolean coercion (AND/OR/XOR/NOT accept non-booleans)",
    ),
    (
        "expressions/boolean/Boolean4.feature",
        "[4] Fail when using NOT on a non-boolean literal",
        "0.10 keeps boolean coercion (AND/OR/XOR/NOT accept non-booleans)",
    ),
];

fn ruled_deviation(scenario: &Scenario) -> Option<&'static str> {
    RULED_DEVIATIONS
        .iter()
        .find(|(feature, name, _)| scenario.feature == *feature && scenario.name.starts_with(name))
        .map(|(_, _, label)| *label)
}

/// Run one scenario against the store `db_factory` makes.
pub fn run_scenario(scenario: &Scenario, db_factory: &dyn Fn() -> TempGraph) -> Outcome {
    if let Some(rule) = skip_rule_for(scenario) {
        return Outcome::Skip(rule);
    }
    let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        execute_steps(scenario, db_factory)
    }));
    match run {
        Ok(Outcome::Fail(mut failures)) => {
            // Only the failure the decision explains is relabelled: a
            // scenario that fails for any other reason keeps its own group.
            if let Some(label) = ruled_deviation(scenario) {
                if let Some(first) = failures.first_mut() {
                    if first.group.starts_with("expected ") && first.group.ends_with(" not raised")
                    {
                        first.group = format!("ruled deviation: {label}");
                    }
                }
            }
            Outcome::Fail(failures)
        }
        Ok(outcome) => outcome,
        Err(panic) => {
            let msg = panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "panic".into());
            Outcome::Fail(vec![Failure {
                group: "engine panicked".into(),
                detail: msg,
            }])
        }
    }
}

/// The graph as the TCK observes it: nodes by id, relationships, and their
/// properties.
#[derive(Debug, Clone, Default)]
struct GraphState {
    nodes: BTreeMap<u32, TckNode>,
    /// `(start id, end id, type)`, one entry per relationship.
    rels: Vec<(u32, u32, String)>,
    /// `(entity, key, value)` triples.
    props: Vec<(String, String, String)>,
}

impl GraphState {
    fn labels(&self) -> std::collections::BTreeSet<String> {
        self.nodes
            .values()
            .flat_map(|n| n.labels.iter().cloned())
            .collect()
    }
}

fn observe(db: &mut GraphDb<RealFs>) -> Result<GraphState, String> {
    let mut state = GraphState::default();
    let nodes = run_query(db, "MATCH (n) RETURN n", &BTreeMap::new())?;
    for row in 0..nodes.len() {
        if let Some(Entity::Node(n)) = nodes.entity(row, 0) {
            for (k, v) in &n.properties {
                state.props.push((
                    format!("n{}", n.id),
                    k.clone(),
                    value::from_value(v).to_string(),
                ));
            }
            state.nodes.insert(n.id, value::node_of(n));
        }
    }
    let rels = run_query(db, "MATCH ()-[r]->() RETURN r", &BTreeMap::new())?;
    let mut seen: BTreeMap<(u32, u32, String), usize> = BTreeMap::new();
    for row in 0..rels.len() {
        if let Some(Entity::Relationship(r)) = rels.entity(row, 0) {
            let key = (r.start_id, r.end_id, r.rel_type.clone());
            let nth = seen.entry(key.clone()).or_insert(0);
            for (k, v) in &r.properties {
                state.props.push((
                    format!("r{}-{}-{}-{}", r.start_id, r.rel_type, r.end_id, nth),
                    k.clone(),
                    value::from_value(v).to_string(),
                ));
            }
            *nth += 1;
            state.rels.push(key);
        }
    }
    Ok(state)
}

/// `(added, removed)` between two multisets.
fn multiset_diff<T: Ord + Clone>(before: &[T], after: &[T]) -> (i64, i64) {
    let mut b = before.to_vec();
    let mut a = after.to_vec();
    b.sort();
    a.sort();
    let (mut i, mut j, mut common) = (0, 0, 0);
    while i < b.len() && j < a.len() {
        match b[i].cmp(&a[j]) {
            std::cmp::Ordering::Equal => {
                common += 1;
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
        }
    }
    ((a.len() - common) as i64, (b.len() - common) as i64)
}

fn side_effects(before: &GraphState, after: &GraphState) -> BTreeMap<String, i64> {
    let mut out = BTreeMap::new();
    let before_ids: Vec<u32> = before.nodes.keys().copied().collect();
    let after_ids: Vec<u32> = after.nodes.keys().copied().collect();
    let (n_add, n_del) = multiset_diff(&before_ids, &after_ids);
    let (r_add, r_del) = multiset_diff(&before.rels, &after.rels);
    let (p_add, p_del) = multiset_diff(&before.props, &after.props);
    let bl: Vec<String> = before.labels().into_iter().collect();
    let al: Vec<String> = after.labels().into_iter().collect();
    let (l_add, l_del) = multiset_diff(&bl, &al);
    for (k, v) in [
        ("+nodes", n_add),
        ("-nodes", n_del),
        ("+relationships", r_add),
        ("-relationships", r_del),
        ("+properties", p_add),
        ("-properties", p_del),
        ("+labels", l_add),
        ("-labels", l_del),
    ] {
        if v != 0 {
            out.insert(k.to_string(), v);
        }
    }
    out
}

/// Run one statement the way every surface dispatches it.
pub fn run_query(
    db: &mut GraphDb<RealFs>,
    cypher: &str,
    params: &BTreeMap<String, Value>,
) -> Result<ResultSet, String> {
    let is_write = is_write_query(cypher)?;
    with_entity_detail(|| {
        if is_write {
            db.query_write(cypher, params)
        } else {
            db.query(cypher, params)
        }
    })
    .map_err(|e| e.to_string())
}

/// The group an engine error message is counted under: the feature a
/// `not supported in mushroomdb 0.10:` refusal names, else the error's stage
/// and its first words with names and numbers taken out.
pub fn error_group(message: &str) -> String {
    const NOT_SUPPORTED: &str = "not supported in mushroomdb 0.10: ";
    let message = message.strip_prefix("query error: ").unwrap_or(message);
    if let Some(at) = message.find(NOT_SUPPORTED) {
        let feature = &message[at + NOT_SUPPORTED.len()..];
        let feature = feature.split(['(', ';']).next().unwrap_or(feature).trim();
        return format!("refused as not supported: {feature}");
    }
    // The node it names differs per scenario; the rule is the same one.
    if message.contains("requires a string 'id' property") {
        return "CREATE/MERGE node requires a string `id` property (the node key)".into();
    }
    let mut out = String::new();
    let mut in_quote: Option<char> = None;
    for c in message.chars() {
        match in_quote {
            Some(q) if c == q => {
                in_quote = None;
                out.push('…');
            }
            Some(_) => {}
            None if matches!(c, '`' | '\'' | '"') => in_quote = Some(c),
            None if c.is_ascii_digit() => {
                if !out.ends_with('#') {
                    out.push('#');
                }
            }
            None => out.push(c),
        }
        if out.chars().count() >= 140 {
            break;
        }
    }
    let out = out.replace("parse error at token #: ", "");
    // An example in parentheses is the same error whatever it shows.
    let out = match out.find(" (e.g.") {
        Some(at) => {
            let rest = &out[at..];
            let tail = rest.find(") (").map(|i| &rest[i + 1..]).unwrap_or("");
            format!("{}{tail}", &out[..at])
        }
        None => out,
    };
    let mut out = out.trim().to_string();
    if out.chars().count() > 110 {
        out = out.chars().take(110).collect::<String>() + "…";
    }
    out
}

struct Run {
    params: BTreeMap<String, Value>,
    result: Option<Result<ResultSet, String>>,
    before: Option<GraphState>,
    after: Option<GraphState>,
    failures: Vec<Failure>,
    raised: Option<RaisedError>,
}

impl Run {
    fn fail(&mut self, group: impl Into<String>, detail: impl Into<String>) {
        self.failures.push(Failure {
            group: group.into(),
            detail: detail.into(),
        });
    }
}

fn execute_steps(scenario: &Scenario, db_factory: &dyn Fn() -> TempGraph) -> Outcome {
    let mut graph = db_factory();
    let mut run = Run {
        params: BTreeMap::new(),
        result: None,
        before: None,
        after: None,
        failures: Vec::new(),
        raised: None,
    };
    for step in &scenario.steps {
        let text = step.text.trim();
        let query = step.docstring.as_deref().unwrap_or("");
        if text == "an empty graph" || text == "any graph" {
            continue;
        }
        if let Some(name) = text
            .strip_prefix("the ")
            .and_then(|t| t.strip_suffix(" graph"))
        {
            let path = tck_root()
                .join("graphs")
                .join(name)
                .join(format!("{name}.cypher"));
            match std::fs::read_to_string(&path) {
                Ok(cypher) => {
                    if let Err(e) = run_query(&mut graph.db, &cypher, &BTreeMap::new()) {
                        run.fail(format!("setup failed: {}", error_group(&e)), e);
                        break;
                    }
                }
                Err(e) => {
                    run.fail(
                        "harness: named graph missing",
                        format!("{}: {e}", path.display()),
                    );
                    break;
                }
            }
        } else if text.starts_with("having executed") {
            if let Err(e) = run_query(&mut graph.db, query, &BTreeMap::new()) {
                run.fail(
                    format!("setup failed: {}", error_group(&e)),
                    format!("{query}\n=> {e}"),
                );
                break;
            }
        } else if text.starts_with("parameters are") {
            for row in step.table.iter().flatten() {
                let (Some(name), Some(cell)) = (row.first(), row.get(1)) else {
                    continue;
                };
                match value::parse(cell).ok().and_then(|v| value::to_param(&v)) {
                    Some(v) => {
                        run.params.insert(name.clone(), v);
                    }
                    None => run.fail("harness: parameter not understood", cell.clone()),
                }
            }
        } else if text.starts_with("executing query") {
            run.before = observe(&mut graph.db).ok();
            run.result = Some(run_query(&mut graph.db, query, &run.params));
            run.after = observe(&mut graph.db).ok();
        } else if text.starts_with("executing control query") {
            run.result = Some(run_query(&mut graph.db, query, &run.params));
        } else if text.starts_with("the result should be") {
            check_result(&mut run, text, step.table.as_deref().unwrap_or(&[]));
        } else if text.starts_with("a ") && text.contains(" should be raised at ") {
            check_error(&mut run, text);
        } else if text == "no side effects" {
            check_side_effects(&mut run, &[]);
        } else if text.starts_with("the side effects should be") {
            check_side_effects(&mut run, step.table.as_deref().unwrap_or(&[]));
        } else {
            run.fail("harness: step not understood", text.to_string());
        }
    }
    if run.failures.is_empty() {
        Outcome::Pass(run.raised)
    } else {
        Outcome::Fail(run.failures)
    }
}

fn check_error(run: &mut Run, text: &str) {
    // `a SyntaxError should be raised at compile time: InvalidArgumentType`
    let rest = text.trim_start_matches("a ").trim_start_matches("an ");
    let (ty, after) = rest
        .split_once(" should be raised at ")
        .unwrap_or((rest, ""));
    let phase = after.split(':').next().unwrap_or("").trim();
    match &run.result {
        Some(Err(e)) => {
            run.raised = Some(RaisedError {
                expected_type: ty.to_string(),
                expected_phase: phase.to_string(),
                engine_message: e.clone(),
            });
        }
        Some(Ok(_)) => run.fail(
            format!("expected {ty} not raised"),
            format!("expected {ty} at {phase}; the query succeeded"),
        ),
        None => run.fail("harness: no query ran", text.to_string()),
    }
}

fn check_result(run: &mut Run, text: &str, table: &[Vec<String>]) {
    let rs = match &run.result {
        Some(Ok(rs)) => rs.clone(),
        Some(Err(e)) => {
            let group = format!("unexpected error: {}", error_group(e));
            run.fail(group, e.clone());
            return;
        }
        None => {
            run.fail("harness: no query ran", text.to_string());
            return;
        }
    };
    let ordered = text.contains("in order");
    let unordered_lists = text.contains("ignoring element order for lists");
    let (header, expected_rows): (&[String], &[Vec<String>]) =
        if text.starts_with("the result should be empty") {
            (&[], &[])
        } else {
            match table.split_first() {
                Some((h, rows)) => (h.as_slice(), rows),
                None => (&[], &[]),
            }
        };

    let counter_row = rs.len() == 1 && rs.columns() == WRITE_COUNTER_COLUMNS;
    if expected_rows.is_empty() && counter_row {
        run.fail(
            "a write without RETURN returns its counter row (the TCK expects no rows)",
            format!("got {:?}", render_rows(&rs, rs.columns())),
        );
        return;
    }

    let mut expected = Vec::with_capacity(expected_rows.len());
    for row in expected_rows {
        let mut values = Vec::with_capacity(row.len());
        for cell in row {
            match value::parse(cell) {
                Ok(v) => values.push(v),
                Err(e) => {
                    run.fail("harness: expected value not understood", e);
                    return;
                }
            }
        }
        expected.push(values);
    }

    if expected.is_empty() {
        if !rs.is_empty() {
            run.fail(
                "wrong result: rows returned where none are expected",
                format!(
                    "got {} rows: {:?}",
                    rs.len(),
                    render_rows(&rs, rs.columns())
                ),
            );
        }
        return;
    }

    // Columns are matched by name. When the names differ but the count is
    // the same, the values are still compared by position, so a report can
    // tell a naming difference alone from one that hides a wrong value.
    if rs.columns().len() != header.len() {
        run.fail(
            "wrong result: column names differ",
            format!("expected columns {header:?}, got {:?}", rs.columns()),
        );
        return;
    }
    let by_name: Option<Vec<usize>> = header
        .iter()
        .map(|name| rs.columns().iter().position(|c| c == name))
        .collect();
    let index = match by_name {
        Some(index) => index,
        None => {
            run.fail(
                "wrong result: column names differ",
                format!("expected columns {header:?}, got {:?}", rs.columns()),
            );
            (0..header.len()).collect()
        }
    };
    let actual: Vec<Vec<TckValue>> = (0..rs.len())
        .map(|r| {
            index
                .iter()
                .map(|&c| from_cell(rs.entity(r, c), rs.row(r)[c].as_ref()))
                .collect()
        })
        .collect();

    let row_eq = |a: &Vec<TckValue>, b: &Vec<TckValue>| {
        a.len() == b.len()
            && a.iter()
                .zip(b)
                .all(|(x, y)| values_equal(x, y, unordered_lists))
    };
    let same = if ordered {
        actual.len() == expected.len() && actual.iter().zip(&expected).all(|(a, b)| row_eq(a, b))
    } else {
        multiset_equal(&actual, &expected, row_eq)
    };
    if same {
        return;
    }
    let group = if actual.len() != expected.len() {
        "wrong result: row count differs"
    } else if ordered && multiset_equal(&actual, &expected, row_eq) {
        "wrong result: row order differs"
    } else {
        "wrong result: values differ"
    };
    run.fail(
        group,
        format!("expected {}\n   got {}", show(&expected), show(&actual)),
    );
}

fn show(rows: &[Vec<TckValue>]) -> String {
    let rows: Vec<String> = rows
        .iter()
        .take(8)
        .map(|r| {
            let cells: Vec<String> = r.iter().map(|v| v.to_string()).collect();
            format!("| {} |", cells.join(" | "))
        })
        .collect();
    rows.join(" ")
}

fn render_rows(rs: &ResultSet, columns: &[String]) -> Vec<String> {
    (0..rs.len().min(4))
        .map(|r| {
            let cells: Vec<String> = (0..columns.len())
                .map(|c| from_cell(rs.entity(r, c), rs.row(r)[c].as_ref()).to_string())
                .collect();
            cells.join(", ")
        })
        .collect()
}

fn check_side_effects(run: &mut Run, table: &[Vec<String>]) {
    let mut expected: BTreeMap<String, i64> = BTreeMap::new();
    for row in table {
        if let (Some(k), Some(v)) = (row.first(), row.get(1)) {
            if let Ok(n) = v.trim().parse::<i64>() {
                if n != 0 {
                    expected.insert(k.trim().to_string(), n);
                }
            }
        }
    }
    let (Some(before), Some(after)) = (&run.before, &run.after) else {
        if run.result.is_some() {
            run.fail(
                "harness: graph could not be observed",
                "MATCH (n) RETURN n or MATCH ()-[r]->() RETURN r failed",
            );
        }
        return;
    };
    let actual = side_effects(before, after);
    if actual != expected {
        run.fail(
            "side effects differ",
            format!("expected {expected:?}, got {actual:?}"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn side_effects_count_what_a_write_adds() {
        let mut g = TempGraph::new();
        let before = observe(&mut g.db).unwrap();
        run_query(
            &mut g.db,
            "CREATE (a:A {id: 'a', name: 'x'})-[:T]->(b:B {id: 'b'})",
            &BTreeMap::new(),
        )
        .unwrap();
        let after = observe(&mut g.db).unwrap();
        let fx = side_effects(&before, &after);
        println!("{fx:?} {:?}", after.nodes);
        assert_eq!(fx.get("+nodes"), Some(&2));
        assert_eq!(fx.get("+relationships"), Some(&1));
        assert_eq!(fx.get("+labels"), Some(&2));
    }

    fn raised(phase: &str, message: &str) -> RaisedError {
        RaisedError {
            expected_type: "SyntaxError".into(),
            expected_phase: phase.into(),
            engine_message: message.into(),
        }
    }

    #[test]
    fn an_error_matches_only_in_the_phase_the_tck_names() {
        assert!(error_phase_matches(&raised(
            "compile time",
            "query error: parse: x"
        )));
        assert!(error_phase_matches(&raised("compile time", "lex: x")));
        assert!(!error_phase_matches(&raised(
            "compile time",
            "query error: execute: x"
        )));
        assert!(error_phase_matches(&raised(
            "runtime",
            "query error: execute: x"
        )));
        assert!(!error_phase_matches(&raised(
            "runtime",
            "query error: parse: x"
        )));
        assert!(error_phase_matches(&raised(
            "any time",
            "query error: plan: x"
        )));
        assert!(!error_phase_matches(&raised("runtime", "something else")));
    }
}
