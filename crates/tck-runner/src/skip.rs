//! Which TCK scenarios are not run, and why.
//!
//! A scenario is skipped only when it exercises a feature 0.10 deliberately
//! does not have. Each rule names that feature in its reason, and the
//! conformance page counts the scenarios each rule took out. No rule may be
//! added because a scenario fails: a failing scenario of a feature 0.10 has
//! is a failure, and it is counted as one.
//!
//! Every rule reads the scenario's text (its tags, step texts, queries and
//! tables), never the engine's answer, so the skip list is the same whatever
//! the engine does.

use crate::feature::Scenario;

/// One skip rule: a short id, the reason printed on the conformance page, and
/// the test that selects the scenarios it covers.
pub struct SkipRule {
    pub id: &'static str,
    pub reason: &'static str,
    pub applies: fn(&Scenario) -> bool,
}

impl std::fmt::Debug for SkipRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SkipRule({})", self.id)
    }
}

/// The rules, in the order they are tried; a scenario is counted under the
/// first that applies.
pub const SKIP_RULES: &[SkipRule] = &[
    SkipRule {
        id: "ignore",
        reason: "tagged @ignore by the TCK itself",
        applies: tagged_ignore,
    },
    SkipRule {
        id: "user-procedures",
        reason: "registers a test procedure (`there exists a procedure`); 0.10 has built-in procedures only and no way to declare one",
        applies: declares_procedure,
    },
    SkipRule {
        id: "call-subquery",
        reason: "CALL { … } subqueries are not in 0.10",
        applies: uses_call_subquery,
    },
    SkipRule {
        id: "foreach",
        reason: "FOREACH is not in 0.10",
        applies: uses_foreach,
    },
    SkipRule {
        id: "subquery-expression",
        reason: "EXISTS { … }, COUNT { … } and COLLECT { … } subqueries are not in 0.10",
        applies: uses_subquery_expression,
    },
    SkipRule {
        id: "pattern-predicate",
        reason: "pattern predicates (a relationship pattern used as an expression) are not in 0.10",
        applies: uses_pattern_predicate,
    },
    SkipRule {
        id: "multi-label",
        reason: "a node has one label in 0.10; more than one label (n:A:B, n:A|B, n:A&B) is not supported",
        applies: uses_multiple_labels,
    },
    SkipRule {
        id: "gql-label-expression",
        reason: "GQL-only label expressions (:!A, :%) are not in 0.10",
        applies: uses_gql_label_expression,
    },
    SkipRule {
        id: "spatial",
        reason: "spatial values (point, distance) are not in 0.10",
        applies: uses_spatial,
    },
    SkipRule {
        id: "time-types",
        reason: "Time and LocalTime values, and DateTime with a named time zone, are not in 0.10 (Date, LocalDateTime, offset DateTime and Duration are, and run)",
        applies: uses_time_or_zone,
    },
    SkipRule {
        id: "profile",
        reason: "PROFILE output is not in 0.10",
        applies: uses_profile,
    },
];

/// The first rule that skips `scenario`, if any.
pub fn skip_rule_for(scenario: &Scenario) -> Option<&'static SkipRule> {
    SKIP_RULES.iter().find(|r| (r.applies)(scenario))
}

// ---------------------------------------------------------------------------
// A small Cypher tokenizer, enough to find keywords and labels outside string
// literals and comments.

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word(String),
    Str(String),
    Punct(char),
}

fn tokenize(text: &str) -> Vec<Tok> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i += 2;
        } else if c == '\'' || c == '"' {
            let quote = c;
            i += 1;
            let mut s = String::new();
            while i < chars.len() && chars[i] != quote {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    i += 1;
                }
                s.push(chars[i]);
                i += 1;
            }
            i += 1;
            out.push(Tok::Str(s));
        } else if c == '`' {
            i += 1;
            let mut s = String::new();
            while i < chars.len() && chars[i] != '`' {
                s.push(chars[i]);
                i += 1;
            }
            i += 1;
            out.push(Tok::Word(s));
        } else if c.is_alphanumeric() || c == '_' || c == '$' {
            let mut s = String::new();
            while i < chars.len()
                && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$')
            {
                s.push(chars[i]);
                i += 1;
            }
            out.push(Tok::Word(s));
        } else {
            out.push(Tok::Punct(c));
            i += 1;
        }
    }
    out
}

fn is_word(t: Option<&Tok>, w: &str) -> bool {
    matches!(t, Some(Tok::Word(x)) if x.eq_ignore_ascii_case(w))
}

fn is_punct(t: Option<&Tok>, c: char) -> bool {
    matches!(t, Some(Tok::Punct(x)) if *x == c)
}

/// Every text of the scenario a skip rule reads: queries, step texts and
/// table cells.
fn texts(s: &Scenario) -> Vec<&str> {
    let mut out: Vec<&str> = s.queries().collect();
    for step in &s.steps {
        out.push(&step.text);
        if let Some(t) = &step.table {
            out.extend(t.iter().flatten().map(String::as_str));
        }
    }
    out
}

fn any_query_tokens(s: &Scenario, f: impl Fn(&[Tok]) -> bool) -> bool {
    s.queries().any(|q| f(&tokenize(q)))
}

// ---------------------------------------------------------------------------
// The rules

fn tagged_ignore(s: &Scenario) -> bool {
    s.tags.iter().any(|t| t.trim_start_matches('@') == "ignore")
}

fn declares_procedure(s: &Scenario) -> bool {
    s.steps
        .iter()
        .any(|st| st.text.starts_with("there exists a procedure"))
}

fn uses_call_subquery(s: &Scenario) -> bool {
    any_query_tokens(s, |t| {
        t.windows(2).any(|w| {
            is_word(Some(&w[0]), "CALL") && matches!(&w[1], Tok::Punct('{') | Tok::Punct('('))
        })
    })
}

fn uses_foreach(s: &Scenario) -> bool {
    any_query_tokens(s, |t| t.iter().any(|x| is_word(Some(x), "FOREACH")))
}

fn uses_subquery_expression(s: &Scenario) -> bool {
    any_query_tokens(s, |t| {
        t.windows(2).any(|w| {
            ["EXISTS", "COUNT", "COLLECT"]
                .iter()
                .any(|k| is_word(Some(&w[0]), k))
                && is_punct(Some(&w[1]), '{')
        })
    })
}

/// `( [var] [:Label…] [{…}] )` starting at `i`; the index after it.
fn node_pattern_end(t: &[Tok], mut i: usize) -> Option<usize> {
    if !is_punct(t.get(i), '(') {
        return None;
    }
    i += 1;
    if matches!(t.get(i), Some(Tok::Word(_))) {
        i += 1;
    }
    while is_punct(t.get(i), ':') {
        i += 1;
        if !matches!(t.get(i), Some(Tok::Word(_))) {
            return None;
        }
        i += 1;
    }
    if is_punct(t.get(i), '{') {
        let mut depth = 0;
        while let Some(x) = t.get(i) {
            match x {
                Tok::Punct('{') => depth += 1,
                Tok::Punct('}') => {
                    depth -= 1;
                    if depth == 0 {
                        i += 1;
                        break;
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }
    is_punct(t.get(i), ')').then_some(i + 1)
}

/// A relationship begins at `i`: `--`, `-[`, `->`, or `<-`.
fn relationship_starts(t: &[Tok], i: usize) -> bool {
    (is_punct(t.get(i), '-')
        && (is_punct(t.get(i + 1), '-')
            || is_punct(t.get(i + 1), '[')
            || is_punct(t.get(i + 1), '>')))
        || (is_punct(t.get(i), '<') && is_punct(t.get(i + 1), '-'))
}

fn uses_pattern_predicate(s: &Scenario) -> bool {
    const EXPR_KEYWORDS: [&str; 7] = ["WHERE", "NOT", "AND", "OR", "XOR", "RETURN", "WITH"];
    any_query_tokens(s, |t| {
        (0..t.len()).any(|i| {
            let start = if EXPR_KEYWORDS.iter().any(|k| is_word(t.get(i), k)) {
                i + 1
            } else if (is_word(t.get(i), "exists") || is_word(t.get(i), "size"))
                && is_punct(t.get(i + 1), '(')
            {
                i + 2
            } else {
                return false;
            };
            node_pattern_end(t, start).is_some_and(|end| relationship_starts(t, end))
        })
    })
}

fn multi_label_in(t: &[Tok]) -> bool {
    let mut stack: Vec<char> = Vec::new();
    for (i, tok) in t.iter().enumerate() {
        match tok {
            Tok::Punct(c @ ('(' | '[' | '{')) => stack.push(*c),
            Tok::Punct(')' | ']' | '}') => {
                stack.pop();
            }
            Tok::Punct(':') => {
                if matches!(stack.last(), Some('[') | Some('{')) {
                    continue;
                }
                let label = matches!(t.get(i + 1), Some(Tok::Word(_)));
                let joiner = is_punct(t.get(i + 2), ':')
                    || is_punct(t.get(i + 2), '|')
                    || is_punct(t.get(i + 2), '&');
                let next = if is_punct(t.get(i + 3), ':') {
                    i + 4
                } else {
                    i + 3
                };
                if label && joiner && matches!(t.get(next), Some(Tok::Word(_))) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

fn uses_multiple_labels(s: &Scenario) -> bool {
    texts(s).iter().any(|text| multi_label_in(&tokenize(text)))
}

fn uses_gql_label_expression(s: &Scenario) -> bool {
    any_query_tokens(s, |t| {
        t.windows(2).any(|w| {
            is_punct(Some(&w[0]), ':') && (is_punct(Some(&w[1]), '!') || is_punct(Some(&w[1]), '%'))
        })
    })
}

fn uses_spatial(s: &Scenario) -> bool {
    any_query_tokens(s, |t| {
        t.windows(2).any(|w| {
            (is_word(Some(&w[0]), "point") || is_word(Some(&w[0]), "distance"))
                && (is_punct(Some(&w[1]), '(') || is_punct(Some(&w[1]), '.'))
        })
    })
}

/// The areas of the IANA time-zone names (`Europe/Stockholm`).
const ZONE_AREAS: [&str; 10] = [
    "Africa",
    "America",
    "Antarctica",
    "Asia",
    "Atlantic",
    "Australia",
    "Europe",
    "Indian",
    "Pacific",
    "Etc",
];

/// A string that names a time zone: `Area/City` (`Europe/Stockholm`), or a
/// date-time with one in brackets (`…+01:00[Europe/London]`).
fn names_a_zone(s: &str) -> bool {
    let zone = match (s.find('['), s.rfind(']')) {
        (Some(open), Some(close)) if open < close => &s[open + 1..close],
        _ => s,
    };
    zone.split_once('/').is_some_and(|(area, city)| {
        ZONE_AREAS.contains(&area)
            && !city.is_empty()
            && city
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '/' | '+'))
    })
}

fn uses_time_or_zone(s: &Scenario) -> bool {
    let time_function = any_query_tokens(s, |t| {
        t.windows(2).enumerate().any(|(i, w)| {
            let named = is_word(Some(&w[0]), "time") || is_word(Some(&w[0]), "localtime");
            // `n.time` is a property, not the function.
            let property = i > 0 && is_punct(t.get(i - 1), '.');
            named && !property && (is_punct(Some(&w[1]), '(') || is_punct(Some(&w[1]), '.'))
        })
    });
    let zone_literal = any_query_tokens(s, |t| {
        t.iter()
            .any(|x| matches!(x, Tok::Str(s) if names_a_zone(s)))
    });
    let zone_in_table = s.steps.iter().any(|st| {
        st.table
            .iter()
            .flatten()
            .flatten()
            .any(|cell| names_a_zone(cell.trim_matches('\'')))
    });
    time_function || zone_literal || zone_in_table
}

fn uses_profile(s: &Scenario) -> bool {
    s.queries().any(|q| is_word(tokenize(q).first(), "PROFILE"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feature::Step;

    fn scenario(query: &str) -> Scenario {
        Scenario {
            feature: "x/y/Z.feature".into(),
            name: "n".into(),
            tags: vec![],
            steps: vec![Step {
                text: "executing query:".into(),
                docstring: Some(query.into()),
                table: None,
            }],
        }
    }

    fn rule(query: &str) -> Option<&'static str> {
        skip_rule_for(&scenario(query)).map(|r| r.id)
    }

    #[test]
    fn each_rule_selects_its_feature_and_nothing_else() {
        assert_eq!(rule("MATCH (n:A:B) RETURN n"), Some("multi-label"));
        assert_eq!(rule("CREATE (:A {name: 'a:b'})"), None);
        assert_eq!(rule("MATCH (a)-[:T|:S]->(b) RETURN a"), None);
        assert_eq!(
            rule("MATCH (n) WHERE (n)-->() RETURN n"),
            Some("pattern-predicate")
        );
        assert_eq!(
            rule("MATCH (n) WHERE NOT (n)-[:T]->(:B) RETURN n"),
            Some("pattern-predicate")
        );
        assert_eq!(rule("MATCH (n) WHERE (n.x) - 1 > 0 RETURN n"), None);
        assert_eq!(rule("RETURN time('12:00') AS t"), Some("time-types"));
        assert_eq!(rule("RETURN localtime() AS t"), Some("time-types"));
        assert_eq!(
            rule("RETURN datetime('2015-07-21T21:40:32.142+0100') AS t"),
            None
        );
        assert_eq!(rule("MATCH (n) RETURN n.time"), None);
        assert_eq!(
            rule("RETURN datetime({year: 1984, timezone: 'Europe/Stockholm'})"),
            Some("time-types")
        );
        assert_eq!(
            rule("CALL { RETURN 1 AS x } RETURN x"),
            Some("call-subquery")
        );
        assert_eq!(rule("CALL db.labels()"), None);
        assert_eq!(
            rule("MATCH (n) WHERE EXISTS { (n)-->() } RETURN n"),
            Some("subquery-expression")
        );
        assert_eq!(rule("RETURN count(*)"), None);
        assert_eq!(rule("FOREACH (x IN [1] | CREATE ())"), Some("foreach"));
        assert_eq!(rule("RETURN point({x: 1, y: 2})"), Some("spatial"));
        assert_eq!(rule("PROFILE MATCH (n) RETURN n"), Some("profile"));
        assert_eq!(rule("MATCH (n) RETURN 'a|b' AS s"), None);
        assert_eq!(rule("CREATE (:Movie {title: 'Frost/Nixon'})"), None);
        assert_eq!(
            rule("RETURN datetime('2015-07-21T21:40:32.142+01:00[Europe/London]') AS d"),
            Some("time-types")
        );
    }
}
