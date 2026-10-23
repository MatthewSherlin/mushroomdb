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

/// The pass rate recorded in `path`: the first line that is not blank or a
/// `#` comment, as a fraction (`0.4321`). A missing file is an error.
pub fn baseline_from_file(path: impl AsRef<Path>) -> Result<f64, String> {
    let path = path.as_ref();
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .ok_or_else(|| format!("{}: no pass rate in the file", path.display()))?;
    line.parse::<f64>()
        .map_err(|e| format!("{}: {line:?} is not a pass rate: {e}", path.display()))
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
        let (passed, failed, skipped) = run_scenarios(&scenarios, TempGraph::new);
        let rate = pass_rate(passed, failed);
        let baseline = baseline_from_file(baseline_path()).expect("tck-baseline.txt");
        println!(
            "TCK: {passed} passed, {failed} failed, {skipped} skipped; pass rate {} (baseline {})",
            format_rate(rate),
            format_rate(baseline)
        );
        assert!(
            holds_baseline(rate, baseline),
            "TCK pass rate {} fell below the baseline {} in tck-baseline.txt",
            format_rate(rate),
            format_rate(baseline)
        );
    }
}
