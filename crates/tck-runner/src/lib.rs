//! Runs the vendored openCypher TCK (`vendor/opencypher-tck/`) against
//! mushroomdb and holds its pass rate to `tck-baseline.txt`.
//!
//! - [`parse_features`] reads the `.feature` files into scenarios (an outline
//!   row is one scenario).
//! - [`run_scenarios`] runs each against a fresh store and counts
//!   `(passed, failed, skipped)`; [`run_all`] keeps every outcome.
//! - [`skip`] holds the rules that keep a scenario out of the run, each with
//!   the reason it prints; a scenario is only skipped for a feature 0.10
//!   deliberately does not have.
//! - [`report`] writes `docs/site/conformance.md` and the Cypher function
//!   table.
//!
//! `pass_rate = passed / (passed + failed)`: skipped scenarios are in neither.

pub mod feature;
pub mod report;
pub mod runner;
pub mod skip;
pub mod value;

pub use feature::{parse_features, parse_features_checked, Scenario, Step};
pub use runner::{run_query, run_scenario, tck_root, Failure, Outcome, TempGraph};

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

/// The TCK's features directory.
pub fn features_dir() -> PathBuf {
    tck_root().join("features")
}

/// `tck-baseline.txt` at the repository root.
pub fn baseline_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tck-baseline.txt")
}

/// The two rates `tck-baseline.txt` holds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Baseline {
    /// `passed / (passed + failed)`.
    pub rate: f64,
    /// The same with only strict passes counted (see [`strict_passes`]).
    pub strict: f64,
}

/// The baselines in `text`: the first line that is not blank or a `#`
/// comment is the pass rate (`0.4321`); the line `strict <rate>` is the
/// strict rate. Both are required.
pub fn parse_baseline(text: &str) -> Result<Baseline, String> {
    let mut lines = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'));
    let first = lines.next().ok_or("no pass rate in the file")?;
    let rate = first
        .parse::<f64>()
        .map_err(|e| format!("{first:?} is not a pass rate: {e}"))?;
    let strict_line = lines
        .find_map(|l| l.strip_prefix("strict "))
        .ok_or("no `strict <rate>` line in the file")?;
    let strict = strict_line
        .trim()
        .parse::<f64>()
        .map_err(|e| format!("{strict_line:?} is not a strict pass rate: {e}"))?;
    Ok(Baseline { rate, strict })
}

/// The baselines recorded in `path`. A missing file is an error.
pub fn baselines_from_file(path: impl AsRef<Path>) -> Result<Baseline, String> {
    let path = path.as_ref();
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    parse_baseline(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// The pass rate recorded in `path` (see [`baselines_from_file`]).
pub fn baseline_from_file(path: impl AsRef<Path>) -> Result<f64, String> {
    baselines_from_file(path).map(|b| b.rate)
}

/// The text `--update-baseline` writes.
pub fn baseline_text(rate: f64, strict: f64) -> String {
    format!(
        "# The openCypher TCK pass rates CI holds; neither may fall.\n\
         # Line 1: passed / (passed + failed). `strict`: the same, counting a scenario\n\
         # that expects an error as passed only when the error came from the phase\n\
         # the TCK names (see docs/site/conformance.md).\n\
         # Raise them in the change that raises the rates:\n\
         #   cargo run --release -p tck-runner -- --update-baseline\n\
         # A change that adds or edits a skip rule says, beside the new rates, how\n\
         # many scenarios the rule moved: a skip changes the denominator.\n\
         {}\n\
         strict {}\n",
        format_rate(rate),
        format_rate(strict)
    )
}

/// The passes that count toward the strict rate: every scenario that checked
/// a result, and every error pass whose error came from the expected phase.
pub fn strict_passes(outcomes: &[Outcome]) -> usize {
    outcomes
        .iter()
        .filter(|o| match o {
            Outcome::Pass(None) => true,
            Outcome::Pass(Some(raised)) => runner::error_phase_matches(raised),
            _ => false,
        })
        .count()
}

/// The two rates of a run: `(rate, strict)`.
pub fn rates(outcomes: &[Outcome]) -> (f64, f64) {
    let (passed, failed, _) = tally(outcomes);
    let strict = strict_passes(outcomes);
    (
        pass_rate(passed, failed),
        pass_rate(strict, passed + failed - strict),
    )
}

/// Holds both rates to `baseline`: `Err` names each that fell.
pub fn check_baseline(rate: f64, strict: f64, baseline: &Baseline) -> Result<(), String> {
    let mut fell = Vec::new();
    if !holds_baseline(rate, baseline.rate) {
        fell.push(format!(
            "TCK pass rate {} fell below the baseline {}",
            format_rate(rate),
            format_rate(baseline.rate)
        ));
    }
    if !holds_baseline(strict, baseline.strict) {
        fell.push(format!(
            "TCK strict pass rate {} fell below the baseline {}",
            format_rate(strict),
            format_rate(baseline.strict)
        ));
    }
    if fell.is_empty() {
        Ok(())
    } else {
        Err(format!("{} (tck-baseline.txt)", fell.join("; ")))
    }
}

/// `passed / (passed + failed)`, or 0 when nothing ran.
pub fn pass_rate(passed: usize, failed: usize) -> f64 {
    if passed + failed == 0 {
        0.0
    } else {
        passed as f64 / (passed + failed) as f64
    }
}

/// A rate as the baseline file stores it: four decimals.
pub fn format_rate(rate: f64) -> String {
    format!("{rate:.4}")
}

/// Whether `rate` holds `baseline`. Both are compared at the four decimals
/// the file stores, so a rate equal to its own recorded value always holds.
pub fn holds_baseline(rate: f64, baseline: f64) -> bool {
    format_rate(rate).parse::<f64>().unwrap_or(rate) >= baseline
}

/// The TCK's own worker count: at most four threads, so a run leaves the
/// rest of the machine free.
pub fn default_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, 4)
}

/// Run every scenario, in parallel, each against its own store from
/// `db_factory`. Outcomes are in scenario order.
pub fn run_all(
    scenarios: &[Scenario],
    db_factory: impl Fn() -> TempGraph + Sync,
    threads: usize,
) -> Vec<Outcome> {
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Option<Outcome>>> = Mutex::new(vec![None; scenarios.len()]);
    let factory: &(dyn Fn() -> TempGraph + Sync) = &db_factory;
    std::thread::scope(|scope| {
        for _ in 0..threads.max(1) {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(scenario) = scenarios.get(i) else {
                    break;
                };
                let outcome = run_scenario(scenario, factory);
                results.lock().expect("results lock")[i] = Some(outcome);
            });
        }
    });
    results
        .into_inner()
        .expect("results lock")
        .into_iter()
        .map(|o| o.expect("every scenario ran"))
        .collect()
}

/// `(passed, failed, skipped)` over `outcomes`.
pub fn tally(outcomes: &[Outcome]) -> (usize, usize, usize) {
    outcomes.iter().fold((0, 0, 0), |(p, f, s), o| match o {
        Outcome::Pass(_) => (p + 1, f, s),
        Outcome::Fail(_) => (p, f + 1, s),
        Outcome::Skip(_) => (p, f, s + 1),
    })
}

/// Run `scenarios` and count `(passed, failed, skipped)`.
pub fn run_scenarios(
    scenarios: &[Scenario],
    db_factory: impl Fn() -> TempGraph + Sync,
) -> (usize, usize, usize) {
    tally(&run_all(scenarios, db_factory, default_threads()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scenario `Return6.feature` `[10]` runs: `MATCH (n) RETURN count(n), collect(n)`.
    fn return6() -> Vec<Scenario> {
        parse_features(&features_dir().join("clauses/return"))
            .into_iter()
            .filter(|s| s.feature.ends_with("Return6.feature"))
            .collect()
    }

    #[test]
    fn test_scenario_executing_query() {
        let path = features_dir().join("clauses/match/Match1.feature");
        let feature = gherkin::Feature::parse_path(&path, gherkin::GherkinEnv::default())
            .expect("the vendored Match1.feature parses");
        assert!(!feature.scenarios.is_empty());

        // The step mapping on the simplest query there is, written in the
        // TCK's own form, against a fresh store. The column is aliased: the
        // engine names an unaliased `count(n)` column `COUNT(n)`, where the
        // TCK expects the text as written (a counted failure group of its own).
        let text = "Feature: smoke\n\n  Scenario: count every node\n    Given an empty graph\n    And having executed:\n      \"\"\"\n      CREATE (:A {id: 'a'})-[:T]->(:A {id: 'b'})\n      \"\"\"\n    When executing query:\n      \"\"\"\n      MATCH (n) RETURN count(n) AS count\n      \"\"\"\n    Then the result should be, in any order:\n      | count |\n      | 2     |\n    And no side effects\n";
        let dir = std::env::temp_dir().join(format!("tck-smoke-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Smoke.feature"), text).unwrap();
        let scenarios = parse_features(&dir);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(scenarios.len(), 1);
        let outcome = run_scenario(&scenarios[0], &TempGraph::new);
        assert!(outcome.is_pass(), "{outcome:?}");
    }

    #[test]
    fn test_tck_pass_rate_above_zero() {
        let scenarios = return6();
        assert!(!scenarios.is_empty(), "Return6.feature has scenarios");
        let (passed, _, _) = run_scenarios(&scenarios, TempGraph::new);
        assert!(passed > 0, "at least one Return6 scenario passes");
    }

    #[test]
    fn a_wrong_expectation_fails() {
        let text = "Feature: f\n\n  Scenario: wrong\n    Given an empty graph\n    When executing query:\n      \"\"\"\n      RETURN 1 AS x\n      \"\"\"\n    Then the result should be, in any order:\n      | x |\n      | 2 |\n    And no side effects\n";
        let dir = std::env::temp_dir().join(format!("tck-wrong-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Wrong.feature"), text).unwrap();
        let scenarios = parse_features(&dir);
        std::fs::remove_dir_all(&dir).unwrap();
        let outcome = run_scenario(&scenarios[0], &TempGraph::new);
        assert!(matches!(outcome, Outcome::Fail(_)), "{outcome:?}");
    }

    #[test]
    fn every_vendored_feature_parses() {
        let (scenarios, errors) = parse_features_checked(&features_dir());
        assert!(errors.is_empty(), "{errors:?}");
        assert!(scenarios.len() > 1000, "{} scenarios", scenarios.len());
    }

    #[test]
    fn both_rates_are_read_and_held() {
        let b = parse_baseline(&baseline_text(0.2142, 0.1765)).unwrap();
        assert_eq!(
            b,
            Baseline {
                rate: 0.2142,
                strict: 0.1765
            }
        );
        assert!(
            parse_baseline("0.5\n").is_err(),
            "a file without the strict line"
        );
        assert!(check_baseline(0.2142, 0.1765, &b).is_ok());
        let err = check_baseline(0.2142, 0.1700, &b).unwrap_err();
        assert!(
            err.contains("strict") && !err.contains("TCK pass rate 0.2142"),
            "{err}"
        );
        assert!(check_baseline(0.2100, 0.1765, &b).is_err());
    }

    #[test]
    fn baseline_compares_at_four_decimals() {
        assert!(holds_baseline(0.43214, 0.4321));
        assert!(!holds_baseline(0.43204, 0.4321));
        assert_eq!(pass_rate(0, 0), 0.0);
    }

    /// The gate: the pass rate over every scenario the skip list keeps may
    /// not fall below `tck-baseline.txt`.
    #[test]
    fn test_tck_gate() {
        let scenarios = parse_features(&features_dir());
        let outcomes = run_all(&scenarios, TempGraph::new, default_threads());
        let (passed, failed, skipped) = tally(&outcomes);
        let (rate, strict) = rates(&outcomes);
        let baseline = baselines_from_file(baseline_path()).expect("tck-baseline.txt");
        println!(
            "TCK: {passed} passed, {failed} failed, {skipped} skipped; pass rate {} (baseline {}), strict {} (baseline {})",
            format_rate(rate),
            format_rate(baseline.rate),
            format_rate(strict),
            format_rate(baseline.strict)
        );
        if let Err(e) = check_baseline(rate, strict, &baseline) {
            panic!("{e}");
        }
    }
}
