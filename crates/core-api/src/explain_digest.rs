//! Why are these two related — as lines.
//!
//! One renderer, because there are two callers: the `explain_association` MCP
//! tool and `mushroomdb why`. The CLI had no door to `GraphDb::explain` at
//! all until 0.7, and the code-graph `why` it borrowed the name from answered
//! a different question out of a code graph.

use crate::digest::{cap_lines, sanitize, MAX_TOOL_LINES};
use crate::{Explanation, GraphDb, NodeInfo, PredicateSummary, Result, Value};
use core_storage::fs::Fs;
use serde_json::{json, Value as Js};
use std::collections::BTreeSet;

/// Every rule-derived edge between `a` and `b`, each with the values that made
/// its predicate true.
///
/// The matched values are read here, off the same handle the edges came from,
/// so the evidence cannot describe a graph that has since moved. An error from
/// `explain` (an unknown key, for one) is the engine's answer and is returned
/// as it came.
pub fn explain_with_evidence<F: Fs>(
    db: &GraphDb<F>,
    a: &str,
    b: &str,
) -> Result<Vec<ExplainedEdge>> {
    let found = db.explain(a, b)?;
    Ok(found
        .into_iter()
        .map(|e| {
            // A via-hop rule evaluates its predicate between the *via*
            // node and the destination, not between the two keys the
            // caller asked about, so there is no pair of nodes here whose
            // values would be the evidence — the line still names the hop.
            let evidence = if e.via_edge.is_some() {
                None
            } else {
                match (db.node_info(&e.src_key), db.node_info(&e.dst_key)) {
                    (Some(src), Some(dst)) => {
                        predicate_evidence(&e.predicate, &src, &dst, e.weight)
                    }
                    _ => None,
                }
            };
            ExplainedEdge { edge: e, evidence }
        })
        .collect())
}

fn value_to_json(v: &Value) -> Js {
    match v {
        Value::Int(i) => json!(i),
        Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(Js::Number)
            .unwrap_or(Js::Null),
        Value::Str(s) => json!(s),
        Value::Bool(b) => json!(b),
        Value::List(xs) => Js::Array(xs.iter().map(value_to_json).collect()),
        Value::Map(m) => {
            let obj: serde_json::Map<String, Js> = m
                .iter()
                .map(|(k, v)| (k.clone(), value_to_json(v)))
                .collect();
            Js::Object(obj)
        }
    }
}

/// One explained edge, with the values that made the predicate true.
///
/// The `Explanation` fields are flattened, so `json: true` hands back the
/// array it always did with one `evidence` object added per relationship.
#[derive(serde::Serialize)]
pub struct ExplainedEdge {
    #[serde(flatten)]
    pub edge: Explanation,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Evidence>,
}

/// What the two nodes actually had in common, per predicate kind.
///
/// Naming the rule and the threshold was never the answer to "why are these
/// two related" — the shared values are. Without them an assistant fetches
/// both nodes' raw property lists and reads them out, which names every
/// value either node holds rather than the ones they share.
#[derive(serde::Serialize)]
#[serde(untagged)]
pub enum Evidence {
    /// `overlap` — the intersection of the two lists, sorted.
    Shared { field: String, shared: Vec<Js> },
    /// `field_equal` / `key_match` — the one value both carry.
    Value { field: String, value: Js },
    /// `geo_radius` — both points and the distance between them.
    Geo {
        field: String,
        a: Js,
        b: Js,
        km: f64,
    },
    /// `numeric_within` / `vector_similar` — the two sides. For
    /// `vector_similar` the vectors themselves are useless to read, so `a`
    /// and `b` are omitted and only the score stands.
    Pair {
        field: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        a: Option<Js>,
        #[serde(skip_serializing_if = "Option::is_none")]
        b: Option<Js>,
        #[serde(skip_serializing_if = "Option::is_none")]
        similarity: Option<f64>,
    },
    /// `all` / `any` — one entry per branch that contributed.
    Parts { parts: Vec<Evidence> },
}

/// Mean Earth radius, as the rules engine uses for `geo_radius`.
const EARTH_RADIUS_KM: f64 = 6371.0088;

/// Evidence for one predicate, recursing through `all` / `any`.
///
/// `score` is the edge's weight and is passed only at the top level: `all`
/// takes the minimum of its branches and `any` the maximum, so a branch's own
/// score is not recoverable from the edge and a nested `vector_similar` has
/// no similarity to report.
///
/// # Only what matched
///
/// A branch reports evidence **only when that branch is itself satisfied**,
/// thresholds applied: an `overlap` under its `min`, a `numeric_within` past
/// its `tolerance`, a `geo_radius` past its `km`, a `key_match` whose field
/// does not name the other node. Under `any` that is the whole point — one
/// branch carries the edge and the others did not — and printing an unmatched
/// branch stated a reason the engine had rejected (`size_bucket: 1 vs 9` on a
/// `±2` tolerance). Under `all` every branch matched by construction, so the
/// checks change nothing there.
fn predicate_evidence(
    p: &PredicateSummary,
    src: &NodeInfo,
    dst: &NodeInfo,
    score: Option<f64>,
) -> Option<Evidence> {
    if let Some(parts) = &p.parts {
        let parts: Vec<Evidence> = parts
            .iter()
            .filter_map(|q| predicate_evidence(q, src, dst, None))
            .collect();
        return (!parts.is_empty()).then_some(Evidence::Parts { parts });
    }
    let field = p.fields.first()?.clone();
    match p.kind.as_str() {
        "overlap" => {
            let (Some(Value::List(a)), Some(Value::List(b))) =
                (src.props.get(&field), dst.props.get(&field))
            else {
                return None;
            };
            // Compared as the rules engine compares them: only a scalar
            // element is a token, and its type is part of its identity, so a
            // `1` and a `1.0` in two lists are not an overlap.
            let left: BTreeSet<(u8, String)> = a.iter().filter_map(scalar_token).collect();
            let right: BTreeSet<(u8, String)> = b.iter().filter_map(scalar_token).collect();
            let union = left.union(&right).count();
            let mut shared: Vec<String> = left
                .intersection(&right)
                .map(|(_, text)| text.clone())
                .collect();
            shared.sort();
            shared.dedup();
            if shared.is_empty() || union == 0 {
                return None;
            }
            // The rule's own test: the Jaccard ratio, against the `min` the
            // predicate declares. Inside an `any`, a list that overlaps but
            // not enough is a branch the engine rejected.
            let jaccard = shared.len() as f64 / union as f64;
            if p.min.is_some_and(|min| jaccard < min) {
                return None;
            }
            Some(Evidence::Shared {
                field,
                shared: shared.into_iter().map(Js::String).collect(),
            })
        }
        "field_equal" => {
            let v = src.props.get(&field)?;
            (dst.props.get(&field) == Some(v)).then(|| Evidence::Value {
                field,
                value: value_to_json(v),
            })
        }
        // A key-match rule reads a foreign key off the source; the value they
        // share is the destination's own key — when the field really does name
        // it, directly or as one element of a list of foreign keys.
        "key_match" => {
            let names_dst = match src.props.get(&field)? {
                Value::Str(s) => s == &dst.key,
                Value::List(items) => items
                    .iter()
                    .any(|v| matches!(v, Value::Str(s) if s == &dst.key)),
                _ => false,
            };
            names_dst.then(|| Evidence::Value {
                field,
                value: Js::String(dst.key.clone()),
            })
        }
        "numeric_within" => {
            let (a, b) = (src.props.get(&field)?, dst.props.get(&field)?);
            let (x, y) = (numeric(a)?, numeric(b)?);
            let delta = (x - y).abs();
            // The rule's own test. A zero tolerance asks for equality.
            let within = match p.tolerance {
                Some(0.0) => delta == 0.0,
                Some(t) => delta <= t,
                None => true,
            };
            within.then(|| Evidence::Pair {
                field,
                a: Some(value_to_json(a)),
                b: Some(value_to_json(b)),
                similarity: None,
            })
        }
        "geo_radius" => {
            let (alat, alon) = lat_lon(src.props.get(&field)?)?;
            let (blat, blon) = lat_lon(dst.props.get(&field)?)?;
            let km = haversine_km(alat, alon, blat, blon);
            if p.km.is_some_and(|radius| km > radius) {
                return None;
            }
            Some(Evidence::Geo {
                field,
                a: Js::String(format_lat_lon(alat, alon)),
                b: Js::String(format_lat_lon(blat, blon)),
                km: round2(km),
            })
        }
        // The two vectors say nothing a reader can use; the cosine the rule
        // scored does, and that is the edge's weight.
        "vector_similar" => score.map(|sim| Evidence::Pair {
            field,
            a: None,
            b: None,
            similarity: Some(sim),
        }),
        _ => None,
    }
}

/// The comparable token of one list element, as `(type tag, text)`.
///
/// Mirrors `ValueKey::from_value`: a nested list or map is not a token and
/// cannot overlap, and two tokens of different types never match however
/// alike they read.
fn scalar_token(v: &Value) -> Option<(u8, String)> {
    match v {
        Value::Str(s) => Some((0, s.clone())),
        Value::Int(i) => Some((1, i.to_string())),
        Value::Float(f) => Some((2, format!("{f}"))),
        Value::Bool(b) => Some((3, b.to_string())),
        Value::List(_) | Value::Map(_) => None,
    }
}

/// A `[lat, lon]` pair, as `geo_radius` reads it.
fn lat_lon(v: &Value) -> Option<(f64, f64)> {
    let Value::List(items) = v else {
        return None;
    };
    if items.len() != 2 {
        return None;
    }
    Some((numeric(&items[0])?, numeric(&items[1])?))
}

/// A finite number, as the rules engine reads one: an integer or a finite
/// float, and nothing else.
fn numeric(v: &Value) -> Option<f64> {
    match v {
        #[allow(clippy::cast_precision_loss)]
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) if f.is_finite() => Some(*f),
        _ => None,
    }
}

fn format_lat_lon(lat: f64, lon: f64) -> String {
    format!("{:.4},{:.4}", lat, lon)
}

fn round2(km: f64) -> f64 {
    (km * 100.0).round() / 100.0
}

/// Great-circle distance in km — the same formula `geo_radius` scores with,
/// so the printed distance and the edge's score agree.
fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let phi1 = lat1.to_radians();
    let phi2 = lat2.to_radians();
    let dphi = (lat2 - lat1).to_radians();
    let dlam = (lon2 - lon1).to_radians();
    let a = ((dphi / 2.0).sin().powi(2) + phi1.cos() * phi2.cos() * (dlam / 2.0).sin().powi(2))
        .clamp(0.0, 1.0);
    EARTH_RADIUS_KM * 2.0 * a.sqrt().atan2((1.0 - a).sqrt())
}

/// One evidence clause, rendered for the digest line.
fn evidence_summary(e: &Evidence) -> String {
    match e {
        Evidence::Shared { field, shared } => {
            let vals: Vec<String> = shared.iter().map(json_scalar_text).collect();
            format!("{}: {}", sanitize(field), vals.join(", "))
        }
        Evidence::Value { field, value } => {
            format!("{}: {}", sanitize(field), json_scalar_text(value))
        }
        Evidence::Geo { field, a, b, km } => format!(
            "{}: {} vs {}, {km} km apart",
            sanitize(field),
            json_scalar_text(a),
            json_scalar_text(b)
        ),
        Evidence::Pair {
            field,
            a: Some(a),
            b: Some(b),
            ..
        } => format!(
            "{}: {} vs {}",
            sanitize(field),
            json_scalar_text(a),
            json_scalar_text(b)
        ),
        Evidence::Pair {
            field,
            similarity: Some(sim),
            ..
        } => format!("{}: similarity {sim:.2}", sanitize(field)),
        Evidence::Pair { field, .. } => sanitize(field),
        Evidence::Parts { parts } => parts
            .iter()
            .map(evidence_summary)
            .collect::<Vec<_>>()
            .join("; "),
    }
}

/// A JSON scalar as the digest prints it: a string without its quotes,
/// anything else as-is. Property values are graph content, so every string
/// goes through [`sanitize`].
fn json_scalar_text(v: &Js) -> String {
    match v {
        Js::String(s) => sanitize(s),
        other => other.to_string(),
    }
}

/// One header, then one line per rule-derived edge, capped like every other
/// task digest.
///
/// Rule names, edge types and predicate fields are all graph content — a rule
/// is named by whoever created it — so each goes through
/// [`sanitize`] before it reaches a line-structured digest.
pub fn render_explain(a: &str, b: &str, found: &[ExplainedEdge]) -> String {
    let mut out = format!(
        "mushroomdb explain — {} ↔ {}: {} relationship(s)\n",
        sanitize(a),
        sanitize(b),
        found.len()
    );
    if found.is_empty() {
        out.push_str("  none\n");
        return out;
    }
    for ExplainedEdge { edge: e, evidence } in found {
        out.push_str(&format!(
            "  {} via rule {}",
            sanitize(&e.edge_type),
            sanitize(&e.rule)
        ));
        if let Some(weight) = e.weight {
            out.push_str(&format!(" (score {weight:.2})"));
        }
        if let Some(via) = &e.via_edge {
            out.push_str(&format!(" via {}", sanitize(via)));
        }
        out.push_str(&format!(" — {}", predicate_summary(&e.predicate)));
        // The matched values, in brackets, after the threshold that admitted
        // them: "overlap on specialties >= 0.2 [specialties: hospitality,
        // residential]". This is the line that stops an assistant fetching
        // both nodes' raw lists and reading out everything either one holds.
        if let Some(ev) = evidence {
            out.push_str(&format!(" [{}]", evidence_summary(ev)));
        }
        out.push('\n');
    }
    cap_lines(&out, MAX_TOOL_LINES)
}

/// A predicate in one clause: what it compares, on which fields, and the
/// threshold it had to clear.
pub fn predicate_summary(p: &PredicateSummary) -> String {
    let mut out = sanitize(&p.kind);
    if !p.fields.is_empty() {
        let fields: Vec<String> = p.fields.iter().map(|f| sanitize(f)).collect();
        out.push_str(&format!(" on {}", fields.join(", ")));
    }
    if let Some(min) = p.min {
        out.push_str(&format!(" >= {min}"));
    }
    if let Some(tolerance) = p.tolerance {
        out.push_str(&format!(" +/- {tolerance}"));
    }
    if let Some(km) = p.km {
        out.push_str(&format!(" within {km} km"));
    }
    if let Some(parts) = &p.parts {
        let inner: Vec<String> = parts.iter().map(predicate_summary).collect();
        out.push_str(&format!(" ({})", inner.join("; ")));
    }
    if p.approximate {
        out.push_str(" (approximate)");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Binding: an explanation's line carries the score and the hop a via-rule
    /// went over, and the digest never runs past the line budget.
    ///
    /// `crates/server/tests/mcp.rs` covers the plain rule and the empty case end to end; what
    /// is only reachable from here is a via-hop rule and a report longer than
    /// [`MAX_TOOL_LINES`], neither of which a two-node fixture
    /// produces.
    #[test]
    fn an_explanation_line_names_the_score_the_hop_and_the_predicate() {
        let one = |rule: &str, via: Option<&str>| ExplainedEdge {
            edge: Explanation {
                rule: rule.to_string(),
                edge_type: "SIMILAR".to_string(),
                src_key: "a".to_string(),
                dst_key: "b".to_string(),
                weight: Some(0.9625),
                predicate: PredicateSummary {
                    kind: "vector_similar".to_string(),
                    fields: vec!["emb".to_string()],
                    min: Some(0.85),
                    tolerance: None,
                    km: None,
                    parts: None,
                    approximate: false,
                },
                via_edge: via.map(str::to_string),
            },
            // A via-hop rule matched between the via node and the
            // destination, so there is no pair here to show evidence from.
            evidence: None,
        };

        let text = render_explain("a", "b", &[one("close", Some("WORKS_AT"))]);
        assert_eq!(
            text,
            "mushroomdb explain — a ↔ b: 1 relationship(s)\n  SIMILAR via rule close (score 0.96) \
             via WORKS_AT — vector_similar on emb >= 0.85\n"
        );

        let many: Vec<ExplainedEdge> = (0..40).map(|i| one(&format!("r{i}"), None)).collect();
        let capped = render_explain("a", "b", &many);
        assert_eq!(
            capped.lines().count(),
            MAX_TOOL_LINES,
            "the digest is capped like every other one"
        );
        assert!(
            capped.starts_with("mushroomdb explain — a ↔ b: 40 relationship(s)"),
            "and the header still says how many there were: {capped}"
        );
    }
}
