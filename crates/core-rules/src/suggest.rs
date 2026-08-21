/// Rule suggestion: profile the data and propose linking rules with previewed edge counts.
///
/// The database proposes its own schema: call [`suggest_rules`] to get a ranked list of
/// candidate [`RuleDef`]s with estimated edge counts and example pairs. No rule is
/// created automatically — the caller must call `db.create_rule(suggestion.def)` explicitly.
use crate::def::{evaluate, NodeView, Predicate, RuleDef};
use core_storage::{list_tokens, Value, ValueKey};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Public constants and config
// ---------------------------------------------------------------------------

/// Default seed for seeded sampling (hex encoding of "Mushroom").
pub const DEFAULT_SEED: u64 = 0x4d75_7368_726f_6f6d;

/// Maximum cardinality to propose a [`Predicate::FieldEqual`] suggestion.
pub const LOW_CARDINALITY_MAX: usize = 20;

/// Default minimum cosine similarity for [`Predicate::VectorSimilar`] suggestions.
pub const VECTOR_SIMILAR_MIN: f64 = 0.8;

/// Suggest `approximate: true` when dst label has more than this many nodes.
pub const VECTOR_APPROX_THRESHOLD: usize = 2_000;

/// Tuning parameters for [`suggest_rules`].
#[derive(Debug, Clone)]
pub struct SuggestConfig {
    /// Max nodes per label sampled during profiling.
    pub max_sample_nodes: usize,
    /// Max source nodes per candidate during preview evaluation.
    pub max_sample_sources: usize,
    /// Max example pairs returned per suggestion.
    pub max_examples: usize,
    /// Per-candidate preview time budget in milliseconds.
    pub budget_ms: u64,
}

impl Default for SuggestConfig {
    fn default() -> Self {
        Self {
            max_sample_nodes: 10_000,
            max_sample_sources: 200,
            max_examples: 3,
            budget_ms: 250,
        }
    }
}

/// One suggested rule with estimated edge count, example pairs, and rationale.
///
/// NO auto-accept: call `db.create_rule(suggestion.def)` explicitly to apply.
#[derive(Debug, Clone, Serialize)]
pub struct RuleSuggestion {
    /// The proposed rule definition (not yet created in the database).
    pub def: RuleDef,
    /// Estimated edge count if the rule were applied. Labeled as an estimate —
    /// derived by extrapolating from a sample of source nodes.
    pub est_edges: u64,
    /// Up to [`SuggestConfig::max_examples`] example `(src_key, dst_key, score)` pairs
    /// drawn from the sample evaluation.
    pub examples: Vec<(String, String, f64)>,
    /// Human-readable explanation of why this rule was suggested.
    pub rationale: String,
}

// ---------------------------------------------------------------------------
// Seeded LCG sampler
// ---------------------------------------------------------------------------

#[inline]
fn lcg_step(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    *state
}

/// Seeded Fisher-Yates partial shuffle returning `k` selected indices from `0..n`.
/// Deterministic for the same `(n, k, seed)`.
fn sample_indices(n: usize, k: usize, seed: u64) -> Vec<usize> {
    if n == 0 {
        return Vec::new();
    }
    let take = k.min(n);
    let mut indices: Vec<usize> = (0..n).collect();
    let mut rng = seed;
    for i in 0..take {
        let r = lcg_step(&mut rng);
        let j = i + (r as usize % (n - i));
        indices.swap(i, j);
    }
    indices[..take].to_vec()
}

// ---------------------------------------------------------------------------
// Value helpers
// ---------------------------------------------------------------------------

fn as_float_val(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) if f.is_finite() => Some(*f),
        _ => None,
    }
}

/// Returns Some(Vec<f64>) if all items in a List are numeric (Int/Float), None otherwise.
fn as_float_list(v: &Value) -> Option<Vec<f64>> {
    let Value::List(items) = v else {
        return None;
    };
    if items.is_empty() {
        return None;
    }
    items.iter().map(as_float_val).collect()
}

// ---------------------------------------------------------------------------
// Per-label field profile
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FieldProfile {
    /// Count of sampled nodes that carry this field.
    present: usize,
    /// Distinct string values (for cardinality and KeyMatch).
    str_distinct: BTreeSet<String>,
    /// Sampled numeric values (for NumericWithin).
    numeric_vals: Vec<f64>,
    /// Token sets per node (for Overlap).
    list_tokens: Vec<(u32, BTreeSet<ValueKey>)>,
    /// (node_id, dimension) for float-array fields (for VectorSimilar).
    vec_entries: Vec<(u32, usize)>,
}

/// Profile all fields for a sampled subset of `nodes`.
fn profile_label(
    nodes: &[(u32, String)],
    get_prop: &dyn Fn(u32, &str) -> Option<Value>,
    all_fields: &[String],
    max_sample: usize,
    seed: u64,
) -> BTreeMap<String, FieldProfile> {
    let sample = sample_indices(nodes.len(), max_sample, seed);
    let mut profiles: BTreeMap<String, FieldProfile> = BTreeMap::new();

    for si in sample {
        let (node_id, _) = &nodes[si];
        for field in all_fields {
            let Some(val) = get_prop(*node_id, field) else {
                continue;
            };
            let p = profiles.entry(field.clone()).or_default();
            p.present += 1;

            match &val {
                Value::Str(s) => {
                    p.str_distinct.insert(s.clone());
                }
                Value::Int(_) | Value::Float(_) => {
                    if let Some(f) = as_float_val(&val) {
                        p.numeric_vals.push(f);
                    }
                }
                Value::List(_) => {
                    if let Some(fvec) = as_float_list(&val) {
                        // Float-array: candidate for VectorSimilar.
                        p.vec_entries.push((*node_id, fvec.len()));
                    } else if let Some(toks) = list_tokens(&val) {
                        // Token list: candidate for Overlap.
                        p.list_tokens.push((*node_id, toks));
                    }
                }
                _ => {}
            }
        }
    }

    profiles
}

/// Returns the dimension that ≥ 80 % of `entries` agree on, or `None`.
fn dominant_dim(entries: &[(u32, usize)]) -> Option<usize> {
    if entries.is_empty() {
        return None;
    }
    let mut counts: BTreeMap<usize, usize> = BTreeMap::new();
    for (_, dim) in entries {
        *counts.entry(*dim).or_default() += 1;
    }
    let total = entries.len();
    counts
        .into_iter()
        .find(|&(_, count)| count * 10 >= total * 8)
        .map(|(dim, _)| dim)
}

// ---------------------------------------------------------------------------
// Dedup against existing rules
// ---------------------------------------------------------------------------

fn is_covered(
    existing: &[RuleDef],
    src_label: &str,
    dst_label: &str,
    pred: &Predicate,
) -> bool {
    existing.iter().any(|r| {
        r.src_label == src_label
            && r.dst_label == dst_label
            && same_pred_kind_field(&r.predicate, pred)
    })
}

fn same_pred_kind_field(a: &Predicate, b: &Predicate) -> bool {
    match (a, b) {
        (Predicate::KeyMatch { field: fa }, Predicate::KeyMatch { field: fb }) => fa == fb,
        (Predicate::FieldEqual { field: fa }, Predicate::FieldEqual { field: fb }) => fa == fb,
        (Predicate::Overlap { field: fa, .. }, Predicate::Overlap { field: fb, .. }) => fa == fb,
        (
            Predicate::NumericWithin { field: fa, .. },
            Predicate::NumericWithin { field: fb, .. },
        ) => fa == fb,
        (
            Predicate::VectorSimilar { field: fa, .. },
            Predicate::VectorSimilar { field: fb, .. },
        ) => fa == fb,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Per-candidate preview evaluation
// ---------------------------------------------------------------------------

struct Preview {
    est_edges: u64,
    examples: Vec<(String, String, f64)>,
}

fn run_preview(
    def: &RuleDef,
    src_nodes: &[(u32, String)],
    dst_nodes: &[(u32, String)],
    get_prop: &dyn Fn(u32, &str) -> Option<Value>,
    config: &SuggestConfig,
) -> Preview {
    let src_n = src_nodes.len();
    let dst_n = dst_nodes.len();
    if src_n == 0 || dst_n == 0 {
        return Preview {
            est_edges: 0,
            examples: Vec::new(),
        };
    }

    // We do NOT use a seed here — the preview seed was baked into the def index
    // before this call. Use a fixed offset from the def name for reproducibility.
    let seed = def
        .name
        .bytes()
        .fold(DEFAULT_SEED, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
    let src_sample = sample_indices(src_n, config.max_sample_sources, seed);
    let deadline = Instant::now() + Duration::from_millis(config.budget_ms);

    let mut hit_edges = 0u64;
    let mut examples: Vec<(String, String, f64)> = Vec::new();
    let mut processed = 0usize;

    'outer: for &si in &src_sample {
        // Structural time-budget enforcement: check between source iterations.
        if Instant::now() >= deadline {
            break;
        }
        let (src_id, src_key) = &src_nodes[si];
        let sp = |f: &str| get_prop(*src_id, f);
        let src_view = NodeView {
            key: src_key.as_str(),
            props: &sp,
        };

        for (dst_id, dst_key) in dst_nodes {
            if src_key == dst_key {
                continue; // skip self-loops
            }
            let dp = |f: &str| get_prop(*dst_id, f);
            let dst_view = NodeView {
                key: dst_key.as_str(),
                props: &dp,
            };
            if let Some(score) = evaluate(&def.predicate, &src_view, &dst_view) {
                hit_edges += 1;
                if examples.len() < config.max_examples {
                    examples.push((src_key.clone(), dst_key.clone(), score));
                }
            }
        }
        processed += 1;

        // Second time-budget check: bail after each source completes.
        if Instant::now() >= deadline {
            break 'outer;
        }
    }

    let est_edges = if processed == 0 {
        0
    } else {
        let hit_rate = hit_edges as f64 / (processed as f64 * dst_n as f64);
        (hit_rate * src_n as f64 * dst_n as f64).round() as u64
    };

    Preview { est_edges, examples }
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

/// Suggest linking rules by profiling the database and generating candidates.
///
/// # Arguments
/// - `label_nodes` — maps each label name to its `(node_id, key)` pairs.
/// - `get_prop` — returns `Some(Value)` for `(node_id, field_name)`, or `None` if absent.
/// - `all_fields` — all field names present anywhere in the store.
/// - `existing` — currently registered rules. Suggestions identical to an existing rule
///   (same `src_label`, `dst_label`, predicate kind, and field) are suppressed.
/// - `config` — tuning parameters. Use [`SuggestConfig::default()`] for the standard settings.
/// - `seed` — seed for deterministic sampling. Use [`DEFAULT_SEED`] for the stable default.
///
/// Returns a `Vec` sorted by `est_edges` descending. Never panics on an empty or
/// degenerate database — returns an empty `Vec` instead.
pub fn suggest_rules(
    label_nodes: &BTreeMap<String, Vec<(u32, String)>>,
    get_prop: &dyn Fn(u32, &str) -> Option<Value>,
    all_fields: &[String],
    existing: &[RuleDef],
    config: &SuggestConfig,
    seed: u64,
) -> Vec<RuleSuggestion> {
    if label_nodes.is_empty() || all_fields.is_empty() {
        return Vec::new();
    }

    // Build key sets per label for KeyMatch detection.
    let label_keys: BTreeMap<&str, BTreeSet<&str>> = label_nodes
        .iter()
        .map(|(label, nodes)| {
            let keys: BTreeSet<&str> = nodes.iter().map(|(_, k)| k.as_str()).collect();
            (label.as_str(), keys)
        })
        .collect();

    // Profile each label.
    let profiles: BTreeMap<String, BTreeMap<String, FieldProfile>> = label_nodes
        .iter()
        .enumerate()
        .map(|(i, (label, nodes))| {
            let label_seed = seed.wrapping_add(i as u64 ^ 0x9e37_79b9_7f4a_7c15);
            let p = profile_label(nodes, get_prop, all_fields, config.max_sample_nodes, label_seed);
            (label.clone(), p)
        })
        .collect();

    let labels: Vec<&str> = label_nodes.keys().map(String::as_str).collect();
    let mut results: Vec<RuleSuggestion> = Vec::new();

    // -----------------------------------------------------------------------
    // (a) KeyMatch: _id-suffix fields matching another label's keys
    // -----------------------------------------------------------------------
    for src_label in &labels {
        let Some(src_profile) = profiles.get(*src_label) else {
            continue;
        };
        let src_nodes = &label_nodes[*src_label];

        for (field, fp) in src_profile {
            if !field.ends_with("_id") || fp.str_distinct.is_empty() {
                continue;
            }
            for dst_label in &labels {
                let Some(dst_keys) = label_keys.get(dst_label) else {
                    continue;
                };
                let match_count = fp.str_distinct.iter().filter(|v| dst_keys.contains(v.as_str())).count();
                if match_count == 0 {
                    continue;
                }
                let pred = Predicate::KeyMatch {
                    field: field.clone(),
                };
                if is_covered(existing, src_label, dst_label, &pred) {
                    continue;
                }
                let base = field.trim_end_matches("_id").to_uppercase();
                let name = format!(
                    "suggest_km_{}_{}_{field}",
                    src_label.to_lowercase(),
                    dst_label.to_lowercase(),
                );
                let def = RuleDef {
                    name,
                    src_label: src_label.to_string(),
                    dst_label: dst_label.to_string(),
                    predicate: pred,
                    edge_type: format!("{base}_OF"),
                    weight_prop: None,
                    max_edges: None,
                    approximate: false,
                };
                let examples_preview: Vec<String> =
                    fp.str_distinct.iter().filter(|v| dst_keys.contains(v.as_str())).take(3).cloned().collect();
                let rationale = format!(
                    "Field '{field}' in {src_label} ends with '_id' and {match_count} \
                     sampled value(s) match keys in {dst_label} \
                     (e.g. {}). Suggests a foreign-key relationship.",
                    examples_preview.join(", ")
                );
                let preview =
                    run_preview(&def, src_nodes, &label_nodes[*dst_label], get_prop, config);
                results.push(RuleSuggestion {
                    def,
                    est_edges: preview.est_edges,
                    examples: preview.examples,
                    rationale,
                });
            }
        }
    }

    // -----------------------------------------------------------------------
    // (b) Overlap: list-field cross-label Jaccard ≥ p50
    // -----------------------------------------------------------------------
    for (si, src_label) in labels.iter().enumerate() {
        let Some(src_profile) = profiles.get(*src_label) else {
            continue;
        };
        let src_nodes = &label_nodes[*src_label];

        for (di, dst_label) in labels.iter().enumerate() {
            if di < si {
                continue; // process each (unordered) pair once
            }
            let Some(dst_profile) = profiles.get(*dst_label) else {
                continue;
            };
            let dst_nodes = &label_nodes[*dst_label];

            for field in all_fields {
                let Some(src_fp) = src_profile.get(field) else {
                    continue;
                };
                let Some(dst_fp) = dst_profile.get(field) else {
                    continue;
                };
                if src_fp.list_tokens.is_empty() || dst_fp.list_tokens.is_empty() {
                    continue;
                }

                // Sample Jaccard values from the profiled token sets.
                let n_src_toks = src_fp.list_tokens.len();
                let n_dst_toks = dst_fp.list_tokens.len();
                let n_pairs = 200.min(n_src_toks * n_dst_toks);
                let mut rng = seed
                    .wrapping_add(0xAB_CD_EF_01u64)
                    .wrapping_add(si as u64 * 0x1111)
                    .wrapping_add(di as u64 * 0x2222)
                    .wrapping_add(field.len() as u64 * 0x3333);

                let mut jaccards: Vec<f64> = Vec::with_capacity(n_pairs);
                for _ in 0..n_pairs {
                    let si2 = lcg_step(&mut rng) as usize % n_src_toks;
                    let di2 = lcg_step(&mut rng) as usize % n_dst_toks;
                    let (_, src_toks) = &src_fp.list_tokens[si2];
                    let (_, dst_toks) = &dst_fp.list_tokens[di2];
                    let inter = src_toks.intersection(dst_toks).count();
                    let union = src_toks.union(dst_toks).count();
                    if union > 0 {
                        jaccards.push(inter as f64 / union as f64);
                    }
                }

                if jaccards.is_empty() {
                    continue;
                }
                jaccards.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let p50 = jaccards[jaccards.len() / 2];
                if p50 <= 0.0 {
                    continue;
                }

                let min_val = ((p50 * 100.0).round() / 100.0).clamp(0.01, 1.0);
                let pred = Predicate::Overlap {
                    field: field.clone(),
                    min: min_val,
                };
                if is_covered(existing, src_label, dst_label, &pred) {
                    continue;
                }

                let name = format!(
                    "suggest_ov_{}_{}_{field}",
                    src_label.to_lowercase(),
                    dst_label.to_lowercase(),
                );
                let def = RuleDef {
                    name,
                    src_label: src_label.to_string(),
                    dst_label: dst_label.to_string(),
                    predicate: pred,
                    edge_type: format!("OVERLAPS_{}", field.to_uppercase()),
                    weight_prop: Some("score".into()),
                    max_edges: None,
                    approximate: false,
                };
                let rationale = format!(
                    "Field '{field}' is a token list in both {src_label} and {dst_label}. \
                     Sampled Jaccard p50={p50:.2}; using that as the minimum threshold \
                     (min={min_val:.2}). Lists share common tokens suggesting semantic affinity."
                );
                let preview = run_preview(&def, src_nodes, dst_nodes, get_prop, config);
                results.push(RuleSuggestion {
                    def,
                    est_edges: preview.est_edges,
                    examples: preview.examples,
                    rationale,
                });
            }
        }
    }

    // -----------------------------------------------------------------------
    // (c) FieldEqual: low-cardinality string fields with shared values
    // -----------------------------------------------------------------------
    for (si, src_label) in labels.iter().enumerate() {
        let Some(src_profile) = profiles.get(*src_label) else {
            continue;
        };
        let src_nodes = &label_nodes[*src_label];

        for (di, dst_label) in labels.iter().enumerate() {
            if di < si {
                continue;
            }
            let Some(dst_profile) = profiles.get(*dst_label) else {
                continue;
            };
            let dst_nodes = &label_nodes[*dst_label];

            for field in all_fields {
                let Some(src_fp) = src_profile.get(field) else {
                    continue;
                };
                let Some(dst_fp) = dst_profile.get(field) else {
                    continue;
                };
                if src_fp.str_distinct.is_empty() || dst_fp.str_distinct.is_empty() {
                    continue;
                }
                if src_fp.str_distinct.len() > LOW_CARDINALITY_MAX
                    || dst_fp.str_distinct.len() > LOW_CARDINALITY_MAX
                {
                    continue;
                }
                let shared = src_fp.str_distinct.intersection(&dst_fp.str_distinct).count();
                if shared == 0 {
                    continue;
                }

                let pred = Predicate::FieldEqual {
                    field: field.clone(),
                };
                if is_covered(existing, src_label, dst_label, &pred) {
                    continue;
                }

                let name = format!(
                    "suggest_fe_{}_{}_{field}",
                    src_label.to_lowercase(),
                    dst_label.to_lowercase(),
                );
                let def = RuleDef {
                    name,
                    src_label: src_label.to_string(),
                    dst_label: dst_label.to_string(),
                    predicate: pred,
                    edge_type: format!("SAME_{}", field.to_uppercase()),
                    weight_prop: None,
                    max_edges: None,
                    approximate: false,
                };
                let rationale = format!(
                    "Field '{field}' has low cardinality in {src_label} \
                     ({} distinct value(s)) and {dst_label} ({} distinct value(s)), \
                     with {shared} shared value(s). Suggests a categorical grouping predicate.",
                    src_fp.str_distinct.len(),
                    dst_fp.str_distinct.len(),
                );
                let preview = run_preview(&def, src_nodes, dst_nodes, get_prop, config);
                results.push(RuleSuggestion {
                    def,
                    est_edges: preview.est_edges,
                    examples: preview.examples,
                    rationale,
                });
            }
        }
    }

    // -----------------------------------------------------------------------
    // (d) NumericWithin: overlapping numeric ranges → tolerance from spread
    // -----------------------------------------------------------------------
    for (si, src_label) in labels.iter().enumerate() {
        let Some(src_profile) = profiles.get(*src_label) else {
            continue;
        };
        let src_nodes = &label_nodes[*src_label];

        for (di, dst_label) in labels.iter().enumerate() {
            if di < si {
                continue;
            }
            let Some(dst_profile) = profiles.get(*dst_label) else {
                continue;
            };
            let dst_nodes = &label_nodes[*dst_label];

            for field in all_fields {
                let Some(src_fp) = src_profile.get(field) else {
                    continue;
                };
                let Some(dst_fp) = dst_profile.get(field) else {
                    continue;
                };
                if src_fp.numeric_vals.is_empty() || dst_fp.numeric_vals.is_empty() {
                    continue;
                }

                let src_min = src_fp
                    .numeric_vals
                    .iter()
                    .cloned()
                    .fold(f64::INFINITY, f64::min);
                let src_max = src_fp
                    .numeric_vals
                    .iter()
                    .cloned()
                    .fold(f64::NEG_INFINITY, f64::max);
                let dst_min = dst_fp
                    .numeric_vals
                    .iter()
                    .cloned()
                    .fold(f64::INFINITY, f64::min);
                let dst_max = dst_fp
                    .numeric_vals
                    .iter()
                    .cloned()
                    .fold(f64::NEG_INFINITY, f64::max);

                // Check range overlap.
                if src_max < dst_min || dst_max < src_min {
                    continue;
                }

                let combined_min = src_min.min(dst_min);
                let combined_max = src_max.max(dst_max);
                let spread = combined_max - combined_min;
                if !spread.is_finite() || spread <= 0.0 {
                    continue;
                }
                // Tolerance = spread / 4, minimum 1.0 so exact-match rules are avoided.
                let tolerance = (spread / 4.0).max(1.0);

                let pred = Predicate::NumericWithin {
                    field: field.clone(),
                    tolerance,
                };
                if is_covered(existing, src_label, dst_label, &pred) {
                    continue;
                }

                let name = format!(
                    "suggest_nw_{}_{}_{field}",
                    src_label.to_lowercase(),
                    dst_label.to_lowercase(),
                );
                let def = RuleDef {
                    name,
                    src_label: src_label.to_string(),
                    dst_label: dst_label.to_string(),
                    predicate: pred,
                    edge_type: format!("NEAR_{}", field.to_uppercase()),
                    weight_prop: Some("score".into()),
                    max_edges: None,
                    approximate: false,
                };
                let rationale = format!(
                    "Field '{field}' is numeric in {src_label} (range [{src_min:.2}, {src_max:.2}]) \
                     and {dst_label} (range [{dst_min:.2}, {dst_max:.2}]); ranges overlap. \
                     Tolerance {tolerance:.2} derived from combined spread {spread:.2}."
                );
                let preview = run_preview(&def, src_nodes, dst_nodes, get_prop, config);
                results.push(RuleSuggestion {
                    def,
                    est_edges: preview.est_edges,
                    examples: preview.examples,
                    rationale,
                });
            }
        }
    }

    // -----------------------------------------------------------------------
    // (e) VectorSimilar: equal-dim float arrays → cosine similarity
    // -----------------------------------------------------------------------
    for (si, src_label) in labels.iter().enumerate() {
        let Some(src_profile) = profiles.get(*src_label) else {
            continue;
        };
        let src_nodes = &label_nodes[*src_label];

        for (di, dst_label) in labels.iter().enumerate() {
            if di < si {
                continue;
            }
            let Some(dst_profile) = profiles.get(*dst_label) else {
                continue;
            };
            let dst_nodes = &label_nodes[*dst_label];

            for field in all_fields {
                let Some(src_fp) = src_profile.get(field) else {
                    continue;
                };
                let Some(dst_fp) = dst_profile.get(field) else {
                    continue;
                };
                if src_fp.vec_entries.is_empty() || dst_fp.vec_entries.is_empty() {
                    continue;
                }

                let src_dim = dominant_dim(&src_fp.vec_entries);
                let dst_dim = dominant_dim(&dst_fp.vec_entries);
                let (Some(sdim), Some(ddim)) = (src_dim, dst_dim) else {
                    continue;
                };
                if sdim != ddim || sdim == 0 {
                    continue;
                }

                let approximate = dst_nodes.len() > VECTOR_APPROX_THRESHOLD;
                let pred = Predicate::VectorSimilar {
                    field: field.clone(),
                    min: VECTOR_SIMILAR_MIN,
                };
                if is_covered(existing, src_label, dst_label, &pred) {
                    continue;
                }

                let name = format!(
                    "suggest_vs_{}_{}_{field}",
                    src_label.to_lowercase(),
                    dst_label.to_lowercase(),
                );
                let def = RuleDef {
                    name,
                    src_label: src_label.to_string(),
                    dst_label: dst_label.to_string(),
                    predicate: pred,
                    edge_type: format!("SIMILAR_{}", field.to_uppercase()),
                    weight_prop: Some("score".into()),
                    max_edges: None,
                    approximate,
                };
                let rationale = format!(
                    "Field '{field}' is a float-array of dim {sdim} in both {src_label} \
                     and {dst_label}. Suggests embedding-based similarity (min={VECTOR_SIMILAR_MIN}){}.",
                    if approximate {
                        ", approximate=true suggested (n>2000)"
                    } else {
                        ""
                    }
                );
                let preview = run_preview(&def, src_nodes, dst_nodes, get_prop, config);
                results.push(RuleSuggestion {
                    def,
                    est_edges: preview.est_edges,
                    examples: preview.examples,
                    rationale,
                });
            }
        }
    }

    // Sort by estimated edge count descending so the highest-value suggestions come first.
    results.sort_by(|a, b| b.est_edges.cmp(&a.est_edges).then(a.def.name.cmp(&b.def.name)));
    results
}
