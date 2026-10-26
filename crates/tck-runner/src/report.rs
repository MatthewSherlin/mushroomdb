//! The conformance page (`docs/site/conformance.md`) and the Cypher function
//! table, both built from one TCK run.

use crate::feature::Scenario;
use crate::runner::{error_stage, run_query, Outcome, TempGraph};
use crate::{format_rate, pass_rate, strict_passes, tally};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

#[derive(Default, Clone, Copy)]
struct Counts {
    passed: usize,
    failed: usize,
    skipped: usize,
}

impl Counts {
    fn add(&mut self, o: &Outcome) {
        match o {
            Outcome::Pass(_) => self.passed += 1,
            Outcome::Fail(_) => self.failed += 1,
            Outcome::Skip(_) => self.skipped += 1,
        }
    }

    fn rate(&self) -> String {
        if self.passed + self.failed == 0 {
            "—".into()
        } else {
            percent(pass_rate(self.passed, self.failed))
        }
    }
}

fn percent(rate: f64) -> String {
    format!("{:.1}%", rate * 100.0)
}

/// `YYYY-MM-DD` of today, UTC (days-from-civil, inverted).
pub fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// The failure groups, largest first: `(group, count, first scenario)`.
pub fn failure_groups(
    scenarios: &[Scenario],
    outcomes: &[Outcome],
) -> Vec<(String, usize, String)> {
    let mut groups: BTreeMap<String, (usize, String)> = BTreeMap::new();
    for (s, o) in scenarios.iter().zip(outcomes) {
        if let Outcome::Fail(failures) = o {
            let group = failures
                .first()
                .map(|f| f.group.clone())
                .unwrap_or_default();
            let entry = groups.entry(group).or_insert((0, s.label()));
            entry.0 += 1;
        }
    }
    let mut out: Vec<(String, usize, String)> =
        groups.into_iter().map(|(g, (n, e))| (g, n, e)).collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    out
}

/// The kind of cause a failure group is: what a fix would have to change.
pub fn failure_category(group: &str) -> &'static str {
    let engine_error = |g: &str| {
        if g.starts_with("parse:") || g.starts_with("lex:") {
            "parser gap"
        } else if g.starts_with("refused as not supported") {
            "named unsupported feature"
        } else {
            "runtime error (semantics difference)"
        }
    };
    if group.starts_with("ruled deviation") {
        "ruled deviation"
    } else if group.starts_with("harness") {
        "harness"
    } else if group.starts_with("engine panicked") {
        "engine panic"
    } else if let Some(rest) = group.strip_prefix("setup failed: ") {
        match engine_error(rest) {
            "parser gap" => "setup: parser gap",
            "named unsupported feature" => "setup: named unsupported feature",
            _ => "setup: runtime error",
        }
    } else if let Some(rest) = group.strip_prefix("unexpected error: ") {
        engine_error(rest)
    } else if group.starts_with("expected ") && group.ends_with(" not raised") {
        "error not raised (missing validation)"
    } else {
        "semantics difference"
    }
}

fn md_cell(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

/// The conformance page. `functions` is the function table, when one was
/// built, placed under its own heading.
pub fn conformance_page(
    scenarios: &[Scenario],
    outcomes: &[Outcome],
    version: &str,
    functions: Option<&str>,
) -> String {
    let (passed, failed, skipped) = tally(outcomes);
    let rate = pass_rate(passed, failed);
    let mut by_area: BTreeMap<String, Counts> = BTreeMap::new();
    let mut by_feature: BTreeMap<String, Counts> = BTreeMap::new();
    let mut skips: BTreeMap<&'static str, (usize, &'static str)> = BTreeMap::new();
    for (s, o) in scenarios.iter().zip(outcomes) {
        by_area.entry(s.area()).or_default().add(o);
        by_feature.entry(s.feature.clone()).or_default().add(o);
        if let Outcome::Skip(rule) = o {
            skips.entry(rule.id).or_insert((0, rule.reason)).0 += 1;
        }
    }

    let mut p = String::new();
    let _ = writeln!(p, "# openCypher conformance\n");
    let _ = writeln!(
        p,
        "mushroomdb runs the openCypher Technology Compatibility Kit (TCK) against \
         every change. The TCK is a suite of scenarios, each a query with the result, \
         error or graph change it must give. This page is generated from one run: \
         every number on it is a count of scenarios.\n"
    );
    let _ = writeln!(
        p,
        "- TCK version: `{version}`, vendored unmodified in `vendor/opencypher-tck/`.\n\
         - Generated: {} by `cargo run --release -p tck-runner -- --report docs/site/conformance.md`.\n\
         - The pass rate is passed / (passed + failed). Skipped scenarios are in neither: \
         a scenario is skipped only when it uses a feature this release deliberately \
         does not have (listed under [Skipped scenarios](#skipped-scenarios)).\n\
         - CI fails when the pass rate falls below `tck-baseline.txt`; the baseline is \
         raised by an ordinary change when the rate rises.\n\
         - [Cypher function conformance](conformance-functions.md) breaks the same run down by function; \
         a change that moves a TCK result regenerates it with `bash scripts/gen-conformance-table.sh`, and CI fails when it is stale.\n\
         - The Cypher this release supports is documented in [Cypher Query Reference](query.md).\n",
        today()
    );

    let _ = writeln!(p, "## Summary\n");
    let _ = writeln!(p, "| | Scenarios |\n|---|---|");
    let _ = writeln!(p, "| Passed | {passed} |");
    let _ = writeln!(p, "| Failed | {failed} |");
    let _ = writeln!(p, "| Skipped | {skipped} |");
    let _ = writeln!(p, "| Total | {} |", passed + failed + skipped);
    let _ = writeln!(
        p,
        "| Pass rate | {} (`{}`) |\n",
        percent(rate),
        format_rate(rate)
    );
    let error_passes = outcomes
        .iter()
        .filter(|o| matches!(o, Outcome::Pass(Some(_))))
        .count();
    let strict = strict_passes(outcomes);
    let matched = strict - (passed - error_passes);
    let strict_rate = pass_rate(strict, passed + failed - strict);
    let _ = writeln!(
        p,
        "A scenario that expects an error passes on any error, so the pass rate counts \
         two kinds of pass. The strict rate counts only the passes that checked a result, \
         plus the error passes whose error came from the phase the TCK names (compile \
         time: the lexer, parser or planner; runtime: execution) and, when the engine's \
         error names its openCypher kind (`VariableTypeConflict: …`), is that kind. CI \
         holds both rates to `tck-baseline.txt`.\n"
    );
    let _ = writeln!(p, "| Pass | Scenarios |\n|---|---|");
    let _ = writeln!(
        p,
        "| Checked a result or side effects | {} |",
        passed - error_passes
    );
    let _ = writeln!(p, "| Raised an error the TCK expects | {error_passes} |");
    let _ = writeln!(
        p,
        "| … in the phase (and of the kind) the TCK names | {matched} |"
    );
    let _ = writeln!(
        p,
        "| … in another phase or of another kind (any error accepted) | {} |",
        error_passes - matched
    );
    let _ = writeln!(
        p,
        "| Strict pass rate | {} (`{}`) |\n",
        percent(strict_rate),
        format_rate(strict_rate)
    );

    let _ = writeln!(p, "## By area\n");
    let _ = writeln!(
        p,
        "| Area | Passed | Failed | Skipped | Pass rate |\n|---|---|---|---|---|"
    );
    for (area, c) in &by_area {
        let _ = writeln!(
            p,
            "| `{area}` | {} | {} | {} | {} |",
            c.passed,
            c.failed,
            c.skipped,
            c.rate()
        );
    }
    p.push('\n');

    let _ = writeln!(p, "## Skipped scenarios\n");
    let _ = writeln!(p, "| Rule | Scenarios | Reason |\n|---|---|---|");
    let mut skip_rows: Vec<_> = skips.into_iter().collect();
    skip_rows.sort_by(|a, b| b.1 .0.cmp(&a.1 .0).then(a.0.cmp(b.0)));
    for (id, (n, reason)) in skip_rows {
        let _ = writeln!(p, "| `{id}` | {n} | {} |", md_cell(reason));
    }
    p.push('\n');

    let _ = writeln!(p, "## Failures by cause\n");
    let _ = writeln!(
        p,
        "Each failing scenario is counted once, under the first step that failed.\n"
    );
    let groups = failure_groups(scenarios, outcomes);
    let mut categories: BTreeMap<&'static str, usize> = BTreeMap::new();
    for (group, n, _) in &groups {
        *categories.entry(failure_category(group)).or_default() += n;
    }
    let mut categories: Vec<_> = categories.into_iter().collect();
    categories.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let _ = writeln!(p, "| Kind | Scenarios |\n|---|---|");
    for (category, n) in categories {
        let _ = writeln!(p, "| {category} | {n} |");
    }
    p.push('\n');
    let _ = writeln!(
        p,
        "| Cause | Kind | Scenarios | First scenario |\n|---|---|---|---|"
    );
    for (group, n, example) in groups.iter().take(40) {
        let _ = writeln!(
            p,
            "| {} | {} | {n} | {} |",
            md_cell(group),
            failure_category(group),
            md_cell(example)
        );
    }
    p.push('\n');

    let _ = writeln!(p, "### Scenarios that ran and failed one check\n");
    let _ = writeln!(
        p,
        "A scenario whose query ran is checked to the end even after a check fails. \
         These failed exactly one check (a failed setup or a query that raised an \
         error leaves the rest unchecked, so those are not counted here).\n"
    );
    let mut single: BTreeMap<String, usize> = BTreeMap::new();
    for o in outcomes {
        if let Outcome::Fail(f) = o {
            let ran = !f[0].group.starts_with("setup failed")
                && !f[0].group.starts_with("unexpected error")
                && !f[0].group.starts_with("harness")
                && !f[0].group.starts_with("engine panicked");
            if f.len() == 1 && ran {
                *single.entry(f[0].group.clone()).or_default() += 1;
            }
        }
    }
    let mut single: Vec<_> = single.into_iter().collect();
    single.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let _ = writeln!(p, "| Cause | Scenarios |\n|---|---|");
    for (group, n) in single.iter().take(15) {
        let _ = writeln!(p, "| {} | {n} |", md_cell(group));
    }
    p.push('\n');

    let _ = writeln!(p, "## Expected errors\n");
    let _ = writeln!(
        p,
        "A scenario that expects an error passes on any error. This table shows, for the \
         ones that passed that way, the stage the engine's error came from: a TCK \
         `SyntaxError` met by an `execute:` error, or a runtime error met by a `parse:` \
         error, passed for a different reason than the TCK names.\n"
    );
    let mut raised: BTreeMap<(String, String), BTreeMap<&'static str, usize>> = BTreeMap::new();
    for o in outcomes {
        if let Outcome::Pass(Some(r)) = o {
            *raised
                .entry((r.expected_type.clone(), r.expected_phase.clone()))
                .or_default()
                .entry(error_stage(&r.engine_message))
                .or_default() += 1;
        }
    }
    let _ = writeln!(
        p,
        "| TCK error | Phase | Passed | Engine `lex:` / `parse:` | Engine `plan:` / `execute:` | Other |\n|---|---|---|---|---|---|"
    );
    for ((ty, phase), stages) in &raised {
        let get = |k: &str| stages.get(k).copied().unwrap_or(0);
        let total: usize = stages.values().sum();
        let _ = writeln!(
            p,
            "| `{ty}` | {phase} | {total} | {} | {} | {} |",
            get("lex") + get("parse"),
            get("plan") + get("execute"),
            get("other")
        );
    }
    p.push('\n');

    if let Some(table) = functions {
        let _ = writeln!(p, "## Functions\n");
        let _ = writeln!(
            p,
            "Every function a TCK query calls. Supported means the engine knows the name \
             (a call of it does not fail as an unknown function); the counts are the \
             scenarios whose queries call it.\n"
        );
        p.push_str(table);
        p.push('\n');
    }

    let _ = writeln!(p, "## By feature file\n");
    let _ = writeln!(
        p,
        "| Feature | Passed | Failed | Skipped |\n|---|---|---|---|"
    );
    for (feature, c) in &by_feature {
        let _ = writeln!(
            p,
            "| `{feature}` | {} | {} | {} |",
            c.passed, c.failed, c.skipped
        );
    }
    p
}

// ---------------------------------------------------------------------------
// Function table (contract C5)

const NOT_FUNCTIONS: &[&str] = &[
    "match",
    "optional",
    "create",
    "merge",
    "where",
    "and",
    "or",
    "xor",
    "not",
    "in",
    "return",
    "with",
    "unwind",
    "delete",
    "detach",
    "set",
    "remove",
    "call",
    "as",
    "case",
    "when",
    "then",
    "else",
    "end",
    "distinct",
    "on",
    "union",
    "all",
    "yield",
    "foreach",
    "order",
    "by",
    "skip",
    "limit",
    "is",
    "null",
    "exists",
    "count_subquery",
    "starts",
    "ends",
    "contains",
    "asc",
    "desc",
    "ascending",
    "descending",
    "true",
    "false",
    "profile",
    "explain",
    "any",
    "none",
    "single",
];

/// The function names a query calls: a name (possibly dotted, `date.truncate`)
/// directly before `(`, lowercased. List predicates (`any`, `all`, …) and
/// keywords are left out; a procedure after `CALL` is too.
pub fn functions_called(query: &str) -> BTreeSet<String> {
    let chars: Vec<char> = query.chars().collect();
    let mut out = BTreeSet::new();
    let mut i = 0;
    let mut prev_word = String::new();
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' || c == '"' {
            let q = c;
            i += 1;
            while i < chars.len() && chars[i] != q {
                if chars[i] == '\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len()
                && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '.')
            {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let mut j = i;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            let preceded_by_dot_or_colon = start > 0 && matches!(chars[start - 1], '.' | ':' | '$');
            let lower = word.to_lowercase();
            if j < chars.len()
                && chars[j] == '('
                && !preceded_by_dot_or_colon
                && !prev_word.eq_ignore_ascii_case("call")
                && !NOT_FUNCTIONS.contains(&lower.as_str())
                && !lower.contains("..")
            {
                out.insert(lower.trim_end_matches('.').to_string());
            }
            prev_word = word;
            continue;
        }
        if !c.is_whitespace() {
            prev_word.clear();
        }
        i += 1;
    }
    out
}

/// Whether the engine knows `name`: some call of it, with zero to three
/// null arguments or one integer, succeeds, or fails at plan or execute time
/// with an error that is not `unknown function`. A call the parser refuses
/// is not known.
pub fn engine_knows_function(name: &str) -> bool {
    let probes = [
        format!("RETURN {name}() AS v"),
        format!("RETURN {name}(null) AS v"),
        format!("RETURN {name}(null, null) AS v"),
        format!("RETURN {name}(null, null, null) AS v"),
        format!("UNWIND [1] AS x RETURN {name}(x) AS v"),
    ];
    let mut graph = TempGraph::new();
    probes
        .iter()
        .any(|q| match run_query(&mut graph.db, q, &BTreeMap::new()) {
            Ok(_) => true,
            Err(e) => {
                matches!(error_stage(&e), "plan" | "execute") && !e.contains("unknown function")
            }
        })
}

/// The function table: every function a TCK query calls, whether the engine
/// knows it, and the scenarios that call it (passed / run, and skipped).
pub fn function_table(
    scenarios: &[Scenario],
    outcomes: &[Outcome],
    knows: impl Fn(&str) -> bool,
) -> String {
    let mut uses: BTreeMap<String, Counts> = BTreeMap::new();
    for (s, o) in scenarios.iter().zip(outcomes) {
        let mut called = BTreeSet::new();
        for q in s.queries() {
            called.extend(functions_called(q));
        }
        for f in called {
            uses.entry(f).or_default().add(o);
        }
    }
    let mut t = String::new();
    let _ = writeln!(
        t,
        "| Function | Supported | TCK scenarios passed / run | Skipped |\n|---|---|---|---|"
    );
    for (f, c) in &uses {
        let _ = writeln!(
            t,
            "| `{f}` | {} | {} / {} | {} |",
            if knows(f) { "yes" } else { "no" },
            c.passed,
            c.passed + c.failed,
            c.skipped
        );
    }
    t
}

/// The functions the engine lists in its own `unknown function` error,
/// lowercased and sorted. Aggregates are not in that list; the TCK exercises
/// them, so they still get a row.
pub fn engine_function_names() -> BTreeSet<String> {
    let mut graph = TempGraph::new();
    let message = run_query(
        &mut graph.db,
        "RETURN no_such_function_zz() AS v",
        &BTreeMap::new(),
    )
    .err()
    .unwrap_or_default();
    message
        .split_once("supported: ")
        .map(|(_, list)| list)
        .unwrap_or("")
        .split(',')
        .map(|n| n.trim().trim_end_matches('`').to_lowercase())
        .filter(|n| !n.is_empty())
        .collect()
}

/// Features whose scenarios read the clock and flip between runs, so the
/// function page leaves them out: a committed table that changes run to run
/// cannot be gated. Drop an entry when its flake is fixed.
const VOLATILE_FEATURES: &[&str] = &["expressions/temporal/Temporal10.feature"];

/// The page `--function-table` writes: one row per Cypher function the TCK
/// calls, plus every function the engine lists that no scenario calls.
/// Supported is `yes` when every scenario that runs the function passes,
/// `partial` when the engine knows the name but some run fails, and `no`
/// when it does not know it. Deterministic: sorted, counts only, no dates;
/// scenarios of [`VOLATILE_FEATURES`] are not counted.
pub fn function_page(
    scenarios: &[Scenario],
    outcomes: &[Outcome],
    knows: impl Fn(&str) -> bool,
    engine_functions: &BTreeSet<String>,
) -> String {
    let mut uses: BTreeMap<String, Counts> = BTreeMap::new();
    for (s, o) in scenarios.iter().zip(outcomes) {
        if VOLATILE_FEATURES.iter().any(|v| s.feature.ends_with(v)) {
            continue;
        }
        let mut called = BTreeSet::new();
        for q in s.queries() {
            called.extend(functions_called(q));
        }
        for f in called {
            uses.entry(f).or_default().add(o);
        }
    }
    for f in engine_functions {
        uses.entry(f.clone()).or_default();
    }
    let mut t = String::new();
    let _ = writeln!(
        t,
        "# Cypher function conformance\n\n\
         <!-- Generated by `bash scripts/gen-conformance-table.sh`; do not edit by hand. CI fails when it is stale. -->\n\n\
         Every Cypher function the openCypher TCK calls, and every function the engine lists that no TCK scenario calls. \
         Supported is `yes` when every scenario that runs the function passes, `partial` when the engine knows the name \
         but some scenario that runs it fails, and `no` when the engine does not know the name. \
         A scenario that calls several functions counts toward each. \
         The scenarios of `Temporal10.feature` are not counted: they read the clock and pass or fail from run to run, and a table that changed from run to run could not be checked in CI. \
         The page is generated from the same TCK run as [openCypher conformance](conformance.md).\n\n\
         | Function | Supported | Scenarios passing / total | Notes |\n|---|---|---|---|"
    );
    for (f, c) in &uses {
        let run = c.passed + c.failed;
        let known = knows(f) || engine_functions.contains(f);
        let supported = if !known {
            "no"
        } else if c.failed == 0 {
            "yes"
        } else {
            "partial"
        };
        let mut notes = Vec::new();
        if run + c.skipped == 0 {
            notes.push("no TCK scenarios".to_string());
        }
        if known && run > 0 && c.passed == 0 {
            notes.push("the engine knows the name; no scenario passes".to_string());
        }
        if c.skipped > 0 {
            notes.push(format!("{} skipped", c.skipped));
        }
        let _ = writeln!(
            t,
            "| `{f}` | {supported} | {} / {run} | {} |",
            c.passed,
            notes.join("; ")
        );
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_called_functions_only() {
        let f = functions_called(
            "MATCH (n:A) WHERE size(n.list) > 1 AND any(x IN l WHERE x) \
             CALL db.labels() YIELD label RETURN toUpper('x(') AS a, date.truncate('day', d), count(*)",
        );
        let names: Vec<&str> = f.iter().map(String::as_str).collect();
        assert_eq!(names, ["count", "date.truncate", "size", "toupper"]);
    }

    #[test]
    fn the_engine_lists_its_functions() {
        let names = engine_function_names();
        assert!(
            names.contains("tolower") && names.contains("abs"),
            "{names:?}"
        );
    }

    #[test]
    fn the_function_page_marks_functions_no_scenario_calls() {
        let engine: BTreeSet<String> = ["abs".to_string()].into();
        let page = function_page(&[], &[], |_| true, &engine);
        assert!(page.starts_with("# Cypher function conformance"));
        assert!(
            page.contains("| `abs` | yes | 0 / 0 | no TCK scenarios |"),
            "{page}"
        );
    }

    #[test]
    fn today_is_a_date() {
        let d = today();
        assert_eq!(d.len(), 10);
        assert!(d.starts_with("20"));
    }
}
