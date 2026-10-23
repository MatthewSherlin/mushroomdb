//! Reading the TCK's `.feature` files into runnable scenarios.
//!
//! A `Scenario Outline` is expanded here, one [`Scenario`] per row of each of
//! its `Examples` tables, with every `<name>` placeholder in its steps,
//! doc strings and tables replaced by that row's value. A feature's
//! `Background` steps are put in front of each of its scenarios.

use std::path::{Path, PathBuf};

/// One runnable TCK scenario (an outline row is one scenario).
#[derive(Debug, Clone)]
pub struct Scenario {
    /// The feature file, relative to the features directory, with `/`
    /// separators (`clauses/match/Match1.feature`).
    pub feature: String,
    /// The scenario's name, with ` (example N)` for an outline row.
    pub name: String,
    /// Tags of the feature, the scenario and the examples table, in that order.
    pub tags: Vec<String>,
    pub steps: Vec<Step>,
}

impl Scenario {
    /// The TCK area the scenario belongs to: the first two directories of its
    /// feature (`clauses/match`, `expressions/list`).
    pub fn area(&self) -> String {
        let parts: Vec<&str> = self.feature.split('/').collect();
        match parts.len() {
            0 | 1 => self.feature.clone(),
            2 => parts[0].to_string(),
            _ => format!("{}/{}", parts[0], parts[1]),
        }
    }

    /// `feature: name`, the scenario's unique label in reports.
    pub fn label(&self) -> String {
        format!("{}: {}", self.feature, self.name)
    }

    /// Every query text the scenario runs (setup, the query under test and
    /// control queries).
    pub fn queries(&self) -> impl Iterator<Item = &str> {
        self.steps.iter().filter_map(|s| s.docstring.as_deref())
    }
}

/// One step: its text without the keyword, and its doc string or table.
#[derive(Debug, Clone)]
pub struct Step {
    pub text: String,
    pub docstring: Option<String>,
    pub table: Option<Vec<Vec<String>>>,
}

/// A feature file that would not parse.
#[derive(Debug, Clone)]
pub struct FeatureError {
    pub feature: String,
    pub message: String,
}

/// Every scenario under `dir`, in path order, plus the files that failed to
/// parse.
pub fn parse_features_checked(dir: &Path) -> (Vec<Scenario>, Vec<FeatureError>) {
    let mut files = Vec::new();
    collect_feature_files(dir, &mut files);
    files.sort();
    let mut scenarios = Vec::new();
    let mut errors = Vec::new();
    for file in files {
        let rel = file
            .strip_prefix(dir)
            .unwrap_or(&file)
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        let parsed = std::fs::read_to_string(&file)
            .map_err(|e| e.to_string())
            .and_then(|text| {
                gherkin::Feature::parse(normalize(&text), gherkin::GherkinEnv::default())
                    .map_err(|e| format!("{e:?}"))
            });
        match parsed {
            Ok(feature) => expand_feature(&rel, &feature, &mut scenarios),
            Err(message) => errors.push(FeatureError {
                feature: rel,
                message,
            }),
        }
    }
    (scenarios, errors)
}

/// Two spellings the TCK's own runner reads and the `gherkin` crate's grammar
/// does not, rewritten in memory (the vendored files are never edited):
///
/// - a scenario whose first step is `And` (Match5 `[25]`) gets `Given`;
/// - a backslash in a table cell that starts no Gherkin escape (`\'` in
///   Literals6) is doubled, so the cell reads back with the one backslash
///   the TCK wrote. Gherkin's own escapes (`\\`, `\|`, `\n`) are kept.
pub fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut after_scenario_header = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        let indent = &line[..line.len() - trimmed.len()];
        if trimmed.starts_with("Scenario") {
            after_scenario_header = true;
            out.push_str(line);
        } else if after_scenario_header && trimmed.starts_with("And ") {
            after_scenario_header = false;
            out.push_str(indent);
            out.push_str("Given ");
            out.push_str(&trimmed[4..]);
        } else if trimmed.starts_with('|') {
            out.push_str(indent);
            let mut chars = trimmed.chars().peekable();
            while let Some(c) = chars.next() {
                if c == '\\' {
                    match chars.peek() {
                        Some('\\') | Some('|') | Some('n') => {
                            out.push(c);
                            out.push(chars.next().unwrap_or_default());
                        }
                        _ => out.push_str("\\\\"),
                    }
                } else {
                    out.push(c);
                }
            }
        } else {
            if !trimmed.is_empty() && !trimmed.starts_with('@') && !trimmed.starts_with('#') {
                after_scenario_header = false;
            }
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// Every scenario under `dir`; files that fail to parse are left out (see
/// [`parse_features_checked`] to see them).
pub fn parse_features(dir: &Path) -> Vec<Scenario> {
    parse_features_checked(dir).0
}

fn collect_feature_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_feature_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "feature") {
            out.push(path);
        }
    }
}

fn expand_feature(rel: &str, feature: &gherkin::Feature, out: &mut Vec<Scenario>) {
    let background: Vec<Step> = feature
        .background
        .iter()
        .flat_map(|b| b.steps.iter().map(step_of))
        .collect();
    let all = feature
        .scenarios
        .iter()
        .chain(feature.rules.iter().flat_map(|r| r.scenarios.iter()));
    for scenario in all {
        let mut tags = feature.tags.clone();
        tags.extend(scenario.tags.iter().cloned());
        let steps: Vec<Step> = background
            .iter()
            .cloned()
            .chain(scenario.steps.iter().map(step_of))
            .collect();
        if scenario.examples.is_empty() {
            out.push(Scenario {
                feature: rel.to_string(),
                name: scenario.name.clone(),
                tags,
                steps,
            });
            continue;
        }
        let mut n = 0;
        for examples in &scenario.examples {
            let Some(table) = &examples.table else {
                continue;
            };
            let Some((header, rows)) = table.rows.split_first() else {
                continue;
            };
            for row in rows {
                n += 1;
                let mut row_tags = tags.clone();
                row_tags.extend(examples.tags.iter().cloned());
                let fill = |s: &str| substitute(s, header, row);
                out.push(Scenario {
                    feature: rel.to_string(),
                    name: format!("{} (example {n})", scenario.name),
                    tags: row_tags,
                    steps: steps
                        .iter()
                        .map(|st| Step {
                            text: fill(&st.text),
                            docstring: st.docstring.as_deref().map(fill),
                            table: st.table.as_ref().map(|t| {
                                t.iter()
                                    .map(|r| r.iter().map(|c| fill(c)).collect())
                                    .collect()
                            }),
                        })
                        .collect(),
                });
            }
        }
    }
}

fn step_of(s: &gherkin::Step) -> Step {
    Step {
        text: s.value.clone(),
        docstring: s.docstring.clone(),
        table: s.table.as_ref().map(|t| t.rows.clone()),
    }
}

fn substitute(text: &str, header: &[String], row: &[String]) -> String {
    let mut out = text.to_string();
    for (name, value) in header.iter().zip(row) {
        out = out.replace(&format!("<{name}>"), value);
    }
    out
}
