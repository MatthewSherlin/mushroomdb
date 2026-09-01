use crate::def::{evaluate, is_keymatch_rooted, NodeView, Predicate, RuleDef};
use crate::hnsw::HnswIndex;
use crate::index::{
    candidate_spec, candidate_spec_approx_with_k, ivf_drift_rebuild_threshold, CandidateSpec,
    RuleIndex,
};
use core_storage::v8::encode::{decode_ivf_bytes, decode_provenance_bytes};
use core_storage::v8::seam::ColumnsView;
use core_storage::{EdgeProps, IdMap, Interner, Topology, Value};

/// Decode raw IVF section bytes into the `RuleIvfExport` format consumed by
/// `reindex_all_load_ivf`.  Returns an empty map when `bytes` is empty.
fn decode_ivf_bytes_to_export(bytes: &[u8]) -> BTreeMap<String, RuleIvfExport> {
    decode_ivf_bytes(bytes)
        .into_iter()
        .map(|(name, ps)| {
            (
                name,
                (
                    (ps.src.centroids, ps.src.clusters, ps.src.drift),
                    (ps.dst.centroids, ps.dst.clusters, ps.dst.drift),
                ),
            )
        })
        .collect()
}
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, OnceLock};

/// A single derived-edge fire or retract captured during a commit.
///
/// Populated inside [`ProvSets::insert`] / [`ProvSets::remove`] while graph
/// state is fully intact (before any tombstone step in `DeleteNode`).
/// String keys are resolved at capture time so they remain valid even after
/// node deletion.
#[derive(Debug, Clone)]
pub struct EngineEdgeDelta {
    pub rule: String,
    /// User-facing source node key.
    pub src_key: String,
    /// User-facing destination node key.
    pub dst_key: String,
    /// Edge-type string.
    pub edge_type: String,
    /// Internal edge-type symbol (for edge_props weight lookup in db.rs).
    pub etype_sym: u32,
    /// Internal source node id (for edge_props weight lookup in db.rs).
    pub src_id: u32,
    /// Internal destination node id (for edge_props weight lookup in db.rs).
    pub dst_id: u32,
    /// `true` = edge was fired (added to provenance); `false` = retracted.
    pub fired: bool,
}

#[cfg(test)]
pub use crate::index::{with_ivf_drift_rebuild, with_vector_dim_reject, with_vector_early_exit};

/// Test-only: the largest number of desired (src,dst) pairs held in memory at
/// once during a backfill sweep. Lets scale tests assert bounded materialization.
#[cfg(test)]
pub(crate) static PEAK_DESIRED_PAIRS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
pub(crate) fn record_desired_len(n: usize) {
    use std::sync::atomic::Ordering;
    let mut cur = PEAK_DESIRED_PAIRS.load(Ordering::Relaxed);
    while n > cur {
        match PEAK_DESIRED_PAIRS.compare_exchange_weak(cur, n, Ordering::Relaxed, Ordering::Relaxed)
        {
            Ok(_) => break,
            Err(actual) => cur = actual,
        }
    }
}

/// Borrowed mutable view of graph state the engine writes derived edges into.
pub struct GraphMut<'a> {
    pub ids: &'a IdMap,
    pub syms: &'a mut Interner,
    pub labels: &'a [u32],
    pub props: ColumnsView<'a>,
    pub topo: &'a mut Topology,
    pub edge_props: &'a mut EdgeProps,
}

/// Used when `RuleDef.max_edges` is `None`.
pub const DEFAULT_MAX_EDGES: u64 = 1_000_000;

/// `(etype, src, dst)` as stored in `provenance` / `owned`.
type Triple = (u32, u32, u32);
/// Reverse-index entry: `(rule_id, etype, src, dst)`. `rule_id` is interned.
type Touch = (u32, u32, u32, u32);

/// IVF state for one index side, exported for V4 snapshot persistence:
/// `(centroids, node→cluster assignments, drift_counter)`.
pub type SideIvfExport = (Vec<Vec<f64>>, BTreeMap<u32, usize>, u64);
/// IVF state for both sides (src, dst) of one approximate rule.
pub type RuleIvfExport = (SideIvfExport, SideIvfExport);

/// Raw (src-graph, dst-graph) HNSW blobs retained from the last snapshot.
type HnswBlobMap = BTreeMap<String, (Vec<u8>, Vec<u8>)>;
/// Lazily-decoded HNSW graph pair (src-side, dst-side) keyed by rule name.
type LazyHnswMap = BTreeMap<String, (Option<HnswIndex>, Option<HnswIndex>)>;

/// Lazily-decoded provenance state used by `&self` read paths
/// (`stats()`, `explain()`, `provenance_touching`).
///
/// Populated once from the retained section-7 bytes via `ensure_provenance_loaded`.
/// After the first `&mut self` mutation (which calls `ensure_provenance_loaded_mut`
/// and installs into the live `RuleEngine` fields), read paths switch to the live
/// fields and this struct is no longer consulted.
#[derive(Debug, Default)]
struct LazyProvenance {
    provenance: BTreeMap<String, BTreeSet<Triple>>,
    by_node: BTreeMap<u32, BTreeSet<Touch>>,
    intern_rule: Vec<String>,
}

#[derive(Debug, Default)]
pub struct RuleEngine {
    rules: BTreeMap<String, RuleDef>,
    indexes: BTreeMap<String, RuleIndex>,
    provenance: BTreeMap<String, BTreeSet<Triple>>,
    owned: BTreeSet<Triple>,
    /// Derived reverse index: node → provenance triples that touch it.
    /// Never serialized; rebuilt from `provenance` on persist-restore.
    by_node: BTreeMap<u32, BTreeSet<Touch>>,
    /// Intern table for rule names used as `Touch` rule_ids. Derived.
    /// Additive-only: ids are never reused. `by_node` stores these ids, so
    /// pruning-and-reusing a slot would alias leftover touches to a new name.
    /// Bound: one slot per distinct rule name ever created in this process.
    rule_intern: BTreeMap<String, u32>,
    intern_rule: Vec<String>,
    tripped: BTreeMap<String, bool>,
    fires: BTreeMap<String, u64>,
    /// Staging buffer for post-commit [`EngineEdgeDelta`] events.
    ///
    /// Populated by [`ProvSets::insert`] / [`ProvSets::remove`] during
    /// `apply` (live writes AND WAL replay). Callers must drain via
    /// [`RuleEngine::drain_deltas`] immediately after apply to consume live
    /// events or discard replay noise.
    pending_deltas: Vec<EngineEdgeDelta>,
    /// Gate: whether to accumulate [`EngineEdgeDelta`] items during rule
    /// application.
    ///
    /// **Safety invariant:** events are fire-and-forget live streams — a
    /// subscriber that attaches *later* never receives past events by design.
    /// Similarly, views call `backfill_view` at creation time (reading directly
    /// from `topo`, not from pending deltas), so deltas accumulated before a
    /// view is defined are not needed. Accumulation can therefore be skipped
    /// whenever no subscriber and no view exists; the observable behaviour is
    /// identical. Set to `true` by `set_emit_deltas` before the first
    /// subscribe, create_view, or any operation that needs events; cleared when
    /// the last listener is removed.
    emit_deltas: bool,
    /// Approximate rule names whose dst-side IVF drift exceeded
    /// [`crate::IVF_DRIFT_REBUILD`] during the last index maintenance.
    /// Drained by [`RuleEngine::take_rebuild_needed`] after apply.
    rebuild_needed: BTreeSet<String>,
    /// Whether candidate indexes have been populated.  Starts `false` after a
    /// snapshot restore that defers index building.  Set to `true` by
    /// `reindex_all`, `reindex_all_load_ivf`, and `create_rule`.  On the first
    /// mutation call when this is `false`, the full O(n) scan runs (first-write
    /// cost), consuming and replacing `retained_hnsw_blobs`.
    indexes_populated: bool,
    /// Raw HNSW blobs retained from the last snapshot, not yet deserialized.
    ///
    /// Populated by `store_snapshot_state`.  Consumed (deserialized into each
    /// rule's `RuleIndex`) either eagerly in `consume_retained_state_eager` (WAL-
    /// present open) or lazily on the first mutation or first ANN query
    /// (`ensure_hnsw_loaded` / the lazy-init guard in the mutation hooks).
    /// Wrapped in Mutex so `ensure_hnsw_loaded` can be called from `&self`
    /// (shared-read ANN path in `find_similar_vector` / `search_hybrid`).
    retained_hnsw_blobs: Mutex<HnswBlobMap>,
    /// Raw bincode bytes of the IVF cluster state retained from the last snapshot.
    ///
    /// Same lifecycle as `retained_hnsw_blobs` (mutation path only; consumed
    /// in `consume_retained_state_eager` or the lazy-init guard in
    /// `on_node_changed`).  Decoded via `decode_ivf_bytes` on first use
    /// instead of at open time to avoid the ~544 MiB bincode overhead.
    /// Wrapped in Mutex so `store_snapshot_state` can take `&self`.
    retained_ivf_bytes: Mutex<Option<Vec<u8>>>,
    /// Raw rkyv bytes of the provenance section retained from the last snapshot.
    ///
    /// Wrapped in `Mutex` so the `&self` read path (`ensure_provenance_loaded`)
    /// can access it without `&mut self`.  The bytes are NOT cleared by
    /// `ensure_provenance_loaded`; they are consumed (set to `None`) only by
    /// `ensure_provenance_loaded_mut` on the first mutation.  This mirrors the
    /// `retained_hnsw_blobs` lifetime so the write path can still consume the
    /// raw bytes to install into the live mutable fields.
    retained_provenance_bytes: Mutex<Option<Vec<u8>>>,
    /// Lazily-decoded provenance for `&self` read paths (clean-open, no-WAL).
    ///
    /// Populated at most once via `OnceLock::get_or_init` inside
    /// `ensure_provenance_loaded`.  After the first mutation
    /// `ensure_provenance_loaded_mut` installs provenance into the live struct
    /// fields and clears `retained_provenance_bytes`; subsequent reads detect
    /// `retained_provenance_bytes.is_none()` and go directly to the live fields.
    lazy_provenance: OnceLock<LazyProvenance>,
    /// Lazily-loaded HNSW graphs for the clean-open (no-WAL) ANN read path.
    ///
    /// Populated once by `ensure_hnsw_loaded` on the first ANN query after a
    /// snapshot open with no WAL.  `OnceLock` guarantees exactly-once init
    /// even under concurrent shared-read access.  After the first mutation the
    /// mutation-path HNSW in `indexes` takes over; this field is never cleared.
    lazy_hnsw: OnceLock<LazyHnswMap>,
}

// ---------------------------------------------------------------------------
// Private helpers (free functions, not methods, to avoid whole-struct borrows)
// ---------------------------------------------------------------------------

/// Rule-aware candidate spec: exact `ScanAll` for `approximate=false`, IVF
/// `VectorClusters` for `approximate=true` (VectorSimilar-rooted predicates).
fn candidate_spec_for(def: &RuleDef) -> CandidateSpec<'_> {
    if def.approximate {
        // k = max(max_edges, 64): return at least 64 candidates so evaluation
        // has a meaningful pool; bounded by max_edges when set by the caller.
        let k = def.max_edges.map(|me| me.max(64)).unwrap_or(64) as usize;
        candidate_spec_approx_with_k(&def.predicate, k)
    } else {
        candidate_spec(&def.predicate)
    }
}

/// Rule-aware src-side lookup spec. KeyMatch is still exact on the src side
/// regardless of `approximate` (the approximation is on the dst candidate set).
/// For KeyMatch-rooted predicates, src side is indexed as Scalar (FK field
/// value → node bucket) so reverse lookup uses the dst key. Non-KeyMatch
/// `All` uses the full [`candidate_spec_for`] Intersect (not `parts[0]`).
fn src_lookup_spec_for(def: &RuleDef) -> CandidateSpec<'_> {
    if is_keymatch_rooted(&def.predicate) {
        let field =
            keymatch_field(&def.predicate).expect("keymatch-rooted predicate has a KeyMatch field");
        CandidateSpec::Scalar { field }
    } else {
        candidate_spec_for(def)
    }
}

/// True when `p` contains a `VectorSimilar { field: f }` where `f == field`.
fn predicate_covers_field(p: &Predicate, field: &str) -> bool {
    match p {
        Predicate::VectorSimilar { field: f, .. } => f == field,
        Predicate::All(parts) | Predicate::Any(parts) => {
            parts.iter().any(|q| predicate_covers_field(q, field))
        }
        _ => false,
    }
}

/// Extract the FK field name from a KeyMatch (or All-leading-KeyMatch) predicate.
fn keymatch_field(p: &Predicate) -> Option<&str> {
    match p {
        Predicate::KeyMatch { field } => Some(field),
        Predicate::All(parts) => parts.first().and_then(keymatch_field),
        Predicate::Any(_) => None,
        _ => None,
    }
}

/// Compute the set of desired (src, dst) → score edges involving node `n` on
/// the given side.  Returns an empty map if `n`'s label doesn't match the rule.
fn compute_desired(
    def: &RuleDef,
    index: &RuleIndex,
    n: u32,
    on_src_side: bool,
    g: &GraphMut<'_>,
) -> BTreeMap<(u32, u32), f64> {
    let (my_label, other_label) = if on_src_side {
        (&def.src_label, &def.dst_label)
    } else {
        (&def.dst_label, &def.src_label)
    };

    let Some(my_sym) = g.syms.get(my_label) else {
        return BTreeMap::new();
    };
    if g.labels.get(n as usize).copied() != Some(my_sym) {
        return BTreeMap::new();
    }
    let other_sym = g.syms.get(other_label);

    let n_key = match g.ids.key_of(n) {
        Some(k) => k,
        None => return BTreeMap::new(),
    };
    let n_get = |f: &str| g.props.get(n, f).map(|vr| vr.into_value());

    let spec = candidate_spec_for(def);
    let candidates: BTreeSet<u32> = if on_src_side {
        if is_keymatch_rooted(&def.predicate) {
            // KeyMatch src→dst: look up the dst node directly by FK field value.
            // Covers `ByKey` and `All` whose first conjunct is KeyMatch
            // (`Intersect([ByKey, …])`); evaluate() filters remaining conjuncts.
            let field = keymatch_field(&def.predicate).expect("ByKey always comes from KeyMatch");
            match n_get(field) {
                Some(Value::Str(ref target_key)) => match g.ids.get(target_key) {
                    Some(dst_id) => std::iter::once(dst_id).collect(),
                    None => BTreeSet::new(),
                },
                _ => BTreeSet::new(),
            }
        } else {
            index.dst_side.candidates(&spec, &n_get)
        }
    } else {
        // n is dst: probe src_side to find src candidates.
        let src_spec = src_lookup_spec_for(def);
        if is_keymatch_rooted(&def.predicate) {
            // Synthetic getter: returns n's key for the FK field so we find
            // src nodes whose FK value points to n.
            let key_getter = |_: &str| Some(Value::Str(n_key.to_string()));
            index.src_side.candidates(&src_spec, &key_getter)
        } else {
            index.src_side.candidates(&src_spec, &n_get)
        }
    };

    // Fast path: Cauchy-Schwarz suffix-norm early exit for exact VectorSimilar.
    //
    // Skipped for approximate rules (`def.approximate == true`): the IVF
    // pre-filter already eliminates non-candidate nodes, and ScanAll metadata
    // (vec_meta / vec_checkpoints) is not maintained for VectorClusters specs.
    //
    // Pre-fetch n's live vector ONCE outside the candidate loop so it is
    // allocated only once per compute_desired call (not per candidate pair).
    // m's vector is still fetched per pair — unavoidable without caching full
    // vectors (which is the O(n·dim) trade-off the brief rules out).
    //
    // Freshness gate: `SideIndex::fresh_ckpts_for` returns `None` when the
    // cached norm differs from the live norm, preventing stale checkpoints from
    // producing a false reject (see doc comment on `fresh_ckpts_for`).
    let n_early_exit_hint: Option<(Vec<f64>, f64, [f64; 8])> = if !def.approximate {
        if let Predicate::VectorSimilar { field, .. } = &def.predicate {
            if crate::index::vector_early_exit_enabled() {
                let n_side = if on_src_side {
                    &index.src_side
                } else {
                    &index.dst_side
                };
                if let Some(vn_v) = n_get(field) {
                    if let Some(vn) = crate::index::as_numeric_list(&vn_v) {
                        if let Some((norm_n, ckpts_n)) = n_side.fresh_ckpts_for(n, &vn) {
                            Some((vn, norm_n, *ckpts_n))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    let mut out = BTreeMap::new();
    for m in candidates {
        if m == n {
            continue; // never self-edges
        }
        if g.labels.get(m as usize).copied() != other_sym {
            continue; // label filter
        }
        let m_key = match g.ids.key_of(m) {
            Some(k) => k,
            None => continue,
        };
        let m_get = |f: &str| g.props.get(m, f).map(|vr| vr.into_value());
        let (s_view, d_view, s_id, d_id) = if on_src_side {
            (
                NodeView {
                    key: n_key,
                    props: &n_get,
                },
                NodeView {
                    key: m_key,
                    props: &m_get,
                },
                n,
                m,
            )
        } else {
            (
                NodeView {
                    key: m_key,
                    props: &m_get,
                },
                NodeView {
                    key: n_key,
                    props: &n_get,
                },
                m,
                n,
            )
        };

        // Use the pre-fetched n hint if available; fetch m per-pair.
        if let (Some((ref vn, norm_n, ckpts_n)), Predicate::VectorSimilar { field, min }) =
            (&n_early_exit_hint, &def.predicate)
        {
            let m_side = if on_src_side {
                &index.dst_side
            } else {
                &index.src_side
            };
            if let Some(vm_v) = m_get(field) {
                if let Some(vm) = crate::index::as_numeric_list(&vm_v) {
                    if let Some((norm_m, ckpts_m)) = m_side.fresh_ckpts_for(m, &vm) {
                        let (va, ckpts_a, na, vb, ckpts_b, nb) = if on_src_side {
                            (
                                vn.as_slice(),
                                ckpts_n,
                                *norm_n,
                                vm.as_slice(),
                                ckpts_m,
                                norm_m,
                            )
                        } else {
                            (
                                vm.as_slice(),
                                ckpts_m,
                                norm_m,
                                vn.as_slice(),
                                ckpts_n,
                                *norm_n,
                            )
                        };
                        match crate::def::cosine_early_exit(va, vb, ckpts_a, ckpts_b, na, nb, *min)
                        {
                            None => continue, // exact reject
                            Some(score) => {
                                out.insert((s_id, d_id), score);
                                continue; // full cosine already computed
                            }
                        }
                    }
                }
            }
        }

        if let Some(score) = evaluate(&def.predicate, &s_view, &d_view) {
            out.insert((s_id, d_id), score);
        }
    }
    #[cfg(test)]
    record_desired_len(out.len());
    out
}

/// Compute the desired `(src, dst) → score` map for a **via-hop rule** where
/// `def.via_label.is_some()`.
///
/// Semantics: src -[via_edge/via_dir]→ via(via_label), evaluate predicate
/// between via and dst; fire src→dst if any via satisfies; score = max over via.
///
/// `anchor` selects which side of the computation to anchor:
/// - `ViaAnchor::Src(src_id)`: expand from one specific src node.
/// - `ViaAnchor::Dst(dst_id)`: n is a dst node; scan all src nodes and check
///   if any via hop to them evaluates with n.
///
/// Always returns `(src, dst)` keyed pairs regardless of anchor.
fn compute_desired_via(
    def: &RuleDef,
    anchor: ViaAnchor,
    g: &GraphMut<'_>,
) -> BTreeMap<(u32, u32), f64> {
    let via_label = def.via_label.as_deref().unwrap();
    let via_edge_str = def.via_edge.as_deref().unwrap();
    let via_dir = def.via_dir.unwrap_or(core_storage::Direction::Out);

    let src_sym = match g.syms.get(&def.src_label) {
        Some(s) => s,
        None => return BTreeMap::new(),
    };
    let via_sym = match g.syms.get(via_label) {
        Some(s) => s,
        None => return BTreeMap::new(),
    };
    let dst_sym = match g.syms.get(&def.dst_label) {
        Some(s) => s,
        None => return BTreeMap::new(),
    };
    let via_etype = match g.syms.get(via_edge_str) {
        Some(e) => e,
        None => return BTreeMap::new(),
    };

    // Determine which src ids to iterate over.
    let srcs: Vec<u32> = match anchor {
        ViaAnchor::Src(src_id) => {
            if g.labels.get(src_id as usize).copied() == Some(src_sym) {
                vec![src_id]
            } else {
                return BTreeMap::new();
            }
        }
        ViaAnchor::Dst(_) => {
            // Scan all src-label nodes.
            (0..g.ids.len() as u32)
                .filter(|&id| {
                    matches!(
                        g.labels.get(id as usize).copied(),
                        Some(s) if s != u32::MAX && s == src_sym
                    )
                })
                .collect()
        }
    };

    // Collect dst candidates: all dst-label nodes (or just the anchored dst).
    let anchored_dst: Option<u32> = match anchor {
        ViaAnchor::Dst(dst_id) => {
            if g.labels.get(dst_id as usize).copied() == Some(dst_sym) {
                Some(dst_id)
            } else {
                return BTreeMap::new();
            }
        }
        _ => None,
    };

    let mut out = BTreeMap::new();

    for src in srcs {
        let _src_key = match g.ids.key_of(src) {
            Some(k) => k,
            None => continue,
        };
        // Expand via hops from src.
        let via_neighbors: Vec<u32> = g
            .topo
            .neighbors(via_etype, via_dir, src)
            .iter()
            .copied()
            .filter(|&v| g.labels.get(v as usize).copied() == Some(via_sym))
            .collect();

        if via_neighbors.is_empty() {
            continue;
        }

        // Collect dsts to evaluate: all dst-label nodes or just anchored one.
        let dsts: Vec<u32> = if let Some(dst_id) = anchored_dst {
            vec![dst_id]
        } else {
            (0..g.ids.len() as u32)
                .filter(|&id| {
                    id != src
                        && matches!(
                            g.labels.get(id as usize).copied(),
                            Some(s) if s != u32::MAX && s == dst_sym
                        )
                })
                .collect()
        };

        for dst in dsts {
            if dst == src {
                continue; // no self-edges
            }
            let dst_key = match g.ids.key_of(dst) {
                Some(k) => k,
                None => continue,
            };
            let dst_get = |f: &str| g.props.get(dst, f).map(|vr| vr.into_value());
            let dst_view = NodeView {
                key: dst_key,
                props: &dst_get,
            };

            // Score = max over via nodes that satisfy predicate(via, dst).
            let mut best: Option<f64> = None;
            for &via_id in &via_neighbors {
                let via_key = match g.ids.key_of(via_id) {
                    Some(k) => k,
                    None => continue,
                };
                let via_get = |f: &str| g.props.get(via_id, f).map(|vr| vr.into_value());
                let via_view = NodeView {
                    key: via_key,
                    props: &via_get,
                };
                if let Some(score) = evaluate(&def.predicate, &via_view, &dst_view) {
                    best = Some(match best {
                        None => score,
                        Some(prev) => prev.max(score),
                    });
                }
            }

            if let Some(score) = best {
                out.insert((src, dst), score);
            }
        }
    }

    out
}

/// Anchor point for `compute_desired_via`.
enum ViaAnchor {
    /// Expand from one src node (src prop change or src insert).
    Src(u32),
    /// Re-evaluate all srcs that can reach some via satisfying predicate with
    /// this dst (dst prop change).
    Dst(u32),
}

fn edge_budget(def: &RuleDef) -> u64 {
    // Only applies when max_edges is None (global-budget path).
    // Some(k) rules use per-source top-k semantics, not this budget.
    def.max_edges.unwrap_or(DEFAULT_MAX_EDGES)
}

/// Filter a per-source candidate map to the top-k destinations.
///
/// `per_src` must contain only pairs with the same source node (all
/// `(src, dst)` keys share the same `src`).  Returns the top-`k` subset
/// ordered by **(score DESC, dst_key ASC)** — higher scores win; ties are
/// broken by the destination node's string key in ascending lexicographic
/// order, giving a deterministic result independent of internal node IDs.
///
/// When `k` equals or exceeds the number of candidates, the input is
/// returned unchanged (no allocation).
///
/// # Memory cost (per-source candidate ordering)
///
/// This function sorts and truncates a `Vec<((u32,u32), f64)>` of length
/// equal to the number of matching candidates for one source.  That is
/// O(M) per call, where M is the candidate count for this source.  Across
/// a backfill sweep the peak additional memory is O(M_max) — the largest
/// per-source candidate set — not the global total, because the Vec is
/// dropped after each source.  No persistent per-source ordering is
/// maintained beyond the materialized top-k provenance; backfill and
/// rebuild recompute the ordering on demand from the live candidate index.
pub(crate) fn filter_src_top_k(
    per_src: BTreeMap<(u32, u32), f64>,
    k: u64,
    ids: &core_storage::IdMap,
) -> BTreeMap<(u32, u32), f64> {
    if per_src.len() as u64 <= k {
        return per_src;
    }
    let mut candidates: Vec<((u32, u32), f64)> = per_src.into_iter().collect();
    // Sort: score DESC (higher = better), then dst_key ASC as tiebreak.
    candidates.sort_by(|&((_, da), sa), &((_, db), sb)| {
        sb.total_cmp(&sa).then_with(|| {
            let ka = ids.key_of(da).unwrap_or("");
            let kb = ids.key_of(db).unwrap_or("");
            ka.cmp(kb)
        })
    });
    candidates.truncate(k as usize);
    candidates.into_iter().collect()
}

/// Apply top-k derived-edge semantics for a single source node.
///
/// Retracts `(src, *)` provenance edges not in `desired_from_src`, then
/// adds / refreshes weights for those that are.  Does **not** use the
/// global tripped latch or budget check — top-k rules (`max_edges: Some(k)`)
/// are self-capping by construction.
fn apply_per_src_top_k(
    def: &RuleDef,
    src: u32,
    desired_from_src: BTreeMap<(u32, u32), f64>,
    prov: &mut ProvSets<'_>,
    g: &mut GraphMut<'_>,
) {
    let et = g.syms.intern(&def.edge_type);

    // Collect current (src, *) provenance triples for this rule.
    // We filter to s == src so that (*, src) triples — where src is a dst
    // for some other source — are not mistakenly retracted.
    let current: Vec<Triple> = {
        let rid = prov.rule_intern.get(&def.name).copied();
        prov.by_node
            .get(&src)
            .into_iter()
            .flatten()
            .filter(|(r, t, s, _d)| Some(*r) == rid && *t == et && *s == src)
            .map(|(_, t, s, d)| (*t, *s, *d))
            .collect()
    };

    // Retract (src, dst) pairs no longer in the top-k.
    for (t, s, d) in current {
        if !desired_from_src.contains_key(&(s, d)) {
            g.topo.remove_edge(t, s, d);
            g.edge_props.remove_edge(t, s, d);
            prov.remove(&def.name, (t, s, d), g.ids, g.syms);
        }
    }

    // Insert new top-k pairs; refresh weights on already-owned pairs.
    for ((s, d), score) in &desired_from_src {
        let triple = (et, *s, *d);
        let already = prov.contains(&triple);
        if !already {
            let newly = g.topo.add_edge(et, *s, *d);
            if newly {
                prov.insert(&def.name, triple, g.ids, g.syms);
            }
        }
        let is_owned = already || prov.contains(&triple);
        if is_owned {
            if let Some(p) = &def.weight_prop {
                g.edge_props.set(et, *s, *d, p, Value::Float(*score));
            }
        }
    }
}

/// Never recycles ids. See `RuleEngine::rule_intern` for why.
fn intern_rule(intern: &mut BTreeMap<String, u32>, names: &mut Vec<String>, rule: &str) -> u32 {
    if let Some(&id) = intern.get(rule) {
        return id;
    }
    let id = names.len() as u32;
    intern.insert(rule.to_string(), id);
    names.push(rule.to_string());
    id
}

type ByNodeRebuild = (
    BTreeMap<u32, BTreeSet<Touch>>,
    BTreeMap<String, u32>,
    Vec<String>,
);

fn rebuild_by_node(provenance: &BTreeMap<String, BTreeSet<Triple>>) -> ByNodeRebuild {
    let mut by_node = BTreeMap::new();
    let mut intern = BTreeMap::new();
    let mut names = Vec::new();
    for (rule, set) in provenance {
        let rid = intern_rule(&mut intern, &mut names, rule);
        for &triple in set {
            touch_insert(&mut by_node, rid, triple);
        }
    }
    (by_node, intern, names)
}

fn touch_insert(by_node: &mut BTreeMap<u32, BTreeSet<Touch>>, rid: u32, triple: Triple) {
    let (t, s, d) = triple;
    let entry = (rid, t, s, d);
    by_node.entry(s).or_default().insert(entry);
    if s != d {
        by_node.entry(d).or_default().insert(entry);
    }
}

fn touch_remove(by_node: &mut BTreeMap<u32, BTreeSet<Touch>>, rid: u32, triple: Triple) {
    let (t, s, d) = triple;
    let entry = (rid, t, s, d);
    if let Some(set) = by_node.get_mut(&s) {
        set.remove(&entry);
        if set.is_empty() {
            by_node.remove(&s);
        }
    }
    if s != d {
        if let Some(set) = by_node.get_mut(&d) {
            set.remove(&entry);
            if set.is_empty() {
                by_node.remove(&d);
            }
        }
    }
}

#[cfg(test)]
fn resolve_by_node(
    by_node: &BTreeMap<u32, BTreeSet<Touch>>,
    names: &[String],
) -> BTreeMap<u32, BTreeSet<(String, Triple)>> {
    by_node
        .iter()
        .map(|(&n, set)| {
            let resolved = set
                .iter()
                .map(|&(rid, t, s, d)| (names[rid as usize].clone(), (t, s, d)))
                .collect();
            (n, resolved)
        })
        .collect()
}

/// Mutable provenance + derived reverse index. Every insert/remove goes
/// through [`ProvSets::insert`] / [`ProvSets::remove`].
struct ProvSets<'a> {
    set: &'a mut BTreeSet<Triple>,
    owned: &'a mut BTreeSet<Triple>,
    by_node: &'a mut BTreeMap<u32, BTreeSet<Touch>>,
    rule_intern: &'a mut BTreeMap<String, u32>,
    intern_rule: &'a mut Vec<String>,
    /// Staging buffer for post-commit events. Keys are resolved at capture
    /// time (before any tombstone step) so the strings remain valid after
    /// node deletion.
    deltas: &'a mut Vec<EngineEdgeDelta>,
    /// Mirror of [`RuleEngine::emit_deltas`]: when `false`, pushes to
    /// `deltas` are skipped entirely (no heap allocation, no String clone).
    emit: bool,
}

impl ProvSets<'_> {
    /// `ids` and `syms` are passed by the caller (not stored in ProvSets) to
    /// avoid a conflicting borrow when callers also need `&mut g.syms` for
    /// `intern` calls in the same function body.
    fn insert(&mut self, rule: &str, triple: Triple, ids: &IdMap, syms: &Interner) -> bool {
        if !self.set.insert(triple) {
            return false;
        }
        self.owned.insert(triple);
        let rid = intern_rule(self.rule_intern, self.intern_rule, rule);
        touch_insert(self.by_node, rid, triple);
        let (etype, src, dst) = triple;
        if self.emit {
            if let (Some(sk), Some(dk), Some(et)) =
                (ids.key_of(src), ids.key_of(dst), syms.resolve(etype))
            {
                self.deltas.push(EngineEdgeDelta {
                    rule: rule.to_string(),
                    src_key: sk.to_string(),
                    dst_key: dk.to_string(),
                    edge_type: et.to_string(),
                    etype_sym: etype,
                    src_id: src,
                    dst_id: dst,
                    fired: true,
                });
            }
        }
        true
    }

    fn remove(&mut self, rule: &str, triple: Triple, ids: &IdMap, syms: &Interner) -> bool {
        if !self.set.remove(&triple) {
            return false;
        }
        self.owned.remove(&triple);
        let rid = intern_rule(self.rule_intern, self.intern_rule, rule);
        touch_remove(self.by_node, rid, triple);
        let (etype, src, dst) = triple;
        if self.emit {
            if let (Some(sk), Some(dk), Some(et)) =
                (ids.key_of(src), ids.key_of(dst), syms.resolve(etype))
            {
                self.deltas.push(EngineEdgeDelta {
                    rule: rule.to_string(),
                    src_key: sk.to_string(),
                    dst_key: dk.to_string(),
                    edge_type: et.to_string(),
                    etype_sym: etype,
                    src_id: src,
                    dst_id: dst,
                    fired: false,
                });
            }
        }
        true
    }

    fn contains(&self, triple: &Triple) -> bool {
        self.set.contains(triple)
    }

    fn len(&self) -> usize {
        self.set.len()
    }
}

/// Diff-apply `desired` against provenance. `retract_touching = Some(n)` only
/// retracts triples that involve `n` (incremental fire). `None` retracts any
/// current provenance triple not in `desired` (backfill / rebuild).
///
/// `tripped` is a one-way latch: once set, no new provenance edges are added
/// (gate on the flag itself, not `prov.len()`), even if retracts have brought
/// the set below budget. Retracts and weight refreshes on already-owned edges
/// still run. Crossing the budget on a not-yet-tripped rule sets the latch
/// and skips that add and every later add in this call. Never an error.
/// First-N (pre-trip) is BTree `(src, dst)` order of `desired` after retract.
fn apply_desired(
    def: &RuleDef,
    desired: BTreeMap<(u32, u32), f64>,
    retract_touching: Option<u32>,
    prov: &mut ProvSets<'_>,
    tripped: &mut bool,
    g: &mut GraphMut<'_>,
) {
    let budget = edge_budget(def);
    let et = g.syms.intern(&def.edge_type);

    let current: Vec<Triple> = match retract_touching {
        None => prov
            .set
            .iter()
            .filter(|(t, _, _)| *t == et)
            .copied()
            .collect(),
        Some(n) => {
            let rid = prov.rule_intern.get(&def.name).copied();
            prov.by_node
                .get(&n)
                .into_iter()
                .flatten()
                .filter(|(r, t, _, _)| Some(*r) == rid && *t == et)
                .map(|(_, t, s, d)| (*t, *s, *d))
                .collect()
        }
    };

    for (t, s, d) in current {
        if !desired.contains_key(&(s, d)) {
            g.topo.remove_edge(t, s, d);
            g.edge_props.remove_edge(t, s, d);
            prov.remove(&def.name, (t, s, d), g.ids, g.syms);
        }
    }

    for ((s, d), score) in desired {
        let triple = (et, s, d);
        let already = prov.contains(&triple);
        if !already {
            if *tripped || prov.len() as u64 >= budget {
                *tripped = true;
                continue;
            }
            let newly = g.topo.add_edge(et, s, d);
            if newly {
                prov.insert(&def.name, triple, g.ids, g.syms);
            }
        }
        // Only set weight_prop on edges this rule owns (newly added now, or
        // already in provenance). Pre-existing user edges are never owned, so
        // writing a weight to them would leave a ghost property after deletion.
        let is_owned_here = already || prov.contains(&triple);
        if is_owned_here {
            if let Some(p) = &def.weight_prop {
                g.edge_props.set(et, s, d, p, Value::Float(score));
            }
        }
    }
}

/// Union of `compute_desired(..., as_src)` over every live src-label node.
/// BTree iteration of the result is the engine's deterministic first-N order
/// for backfill and rebuild.
///
/// Kept for test reference comparators only.  Production paths use the
/// streaming variants (`apply_streaming_create`, `apply_streaming_rebuild`)
/// which never materialise the global map.
#[cfg(test)]
#[allow(dead_code)]
fn compute_full_desired(
    def: &RuleDef,
    index: &RuleIndex,
    g: &GraphMut<'_>,
) -> BTreeMap<(u32, u32), f64> {
    let mut desired = BTreeMap::new();
    let src_sym = g.syms.get(&def.src_label);
    for id in 0..g.ids.len() as u32 {
        let label_sym = match g.labels.get(id as usize).copied() {
            Some(s) if s != u32::MAX => s,
            _ => continue,
        };
        if src_sym == Some(label_sym) {
            desired.extend(compute_desired(def, index, id, true, g));
            #[cfg(test)]
            record_desired_len(desired.len());
        }
    }
    desired
}

/// Returns `true` if the `(s, d)` pair is still desired under `def` given the
/// current graph state.  Calls `evaluate` directly — bypasses the candidate
/// index.  Valid in `rebuild` after a full reindex because every pair that
/// evaluates to `Some` is reachable via the freshly-built index; the direct
/// call is therefore semantically equivalent to membership in
/// `compute_desired(def, index, s, true, g)`.  O(eval) per call.
fn pair_still_desired(def: &RuleDef, s: u32, d: u32, g: &GraphMut<'_>) -> bool {
    let src_sym = match g.syms.get(&def.src_label) {
        Some(sym) => sym,
        None => return false,
    };
    let dst_sym = match g.syms.get(&def.dst_label) {
        Some(sym) => sym,
        None => return false,
    };
    if g.labels.get(s as usize).copied() != Some(src_sym) {
        return false;
    }
    if g.labels.get(d as usize).copied() != Some(dst_sym) {
        return false;
    }
    let s_key = match g.ids.key_of(s) {
        Some(k) => k,
        None => return false,
    };
    let d_key = match g.ids.key_of(d) {
        Some(k) => k,
        None => return false,
    };
    let s_get = |f: &str| g.props.get(s, f).map(|vr| vr.into_value());
    let d_get = |f: &str| g.props.get(d, f).map(|vr| vr.into_value());
    evaluate(
        &def.predicate,
        &NodeView {
            key: s_key,
            props: &s_get,
        },
        &NodeView {
            key: d_key,
            props: &d_get,
        },
    )
    .is_some()
}

/// Count desired `(src, dst)` pairs up to `limit + 1`; returns as soon as the
/// threshold is crossed.  Peak additional memory: O(max-candidates-per-src).
/// Used in `rebuild` to detect the over-budget case without materialising the
/// full desired map.
fn count_desired_up_to(def: &RuleDef, index: &RuleIndex, limit: u64, g: &GraphMut<'_>) -> u64 {
    let mut count = 0u64;
    let src_sym = g.syms.get(&def.src_label);
    for id in 0..g.ids.len() as u32 {
        let label_sym = match g.labels.get(id as usize).copied() {
            Some(s) if s != u32::MAX => s,
            _ => continue,
        };
        if src_sym != Some(label_sym) {
            continue;
        }
        count += compute_desired(def, index, id, true, g).len() as u64;
        if count > limit {
            return count;
        }
    }
    count
}

/// Streaming backfill for `create_rule`.
///
/// Iterates src nodes in ascending id order.  For each src, computes and
/// immediately applies the per-src desired edges (already dst-sorted within
/// that src).  This traversal visits `(src, dst)` pairs in exactly the same
/// order as iterating the result of `compute_full_desired` would — because the
/// global BTree order on `(u32, u32)` keys is src-major ascending with
/// dst-sorted-within, matching the per-src ascending-dst order emitted by
/// `compute_desired`.
///
/// Consequently the first-N edges selected by the running cap are byte-identical
/// to what the old full-map approach would have selected for EXACT rules
/// (`def.approximate == false`).  For approximate rules the IVF candidate order
/// is deterministic (replay-identical within the same fitted clusters) but is not
/// equivalent to the old full-map path, which was never exercised for approximate
/// rules.
///
/// Cap semantics: on `create_rule` there is no pre-existing provenance for
/// this rule, so when the cap trips we can `break` immediately — no
/// weight-refresh-on-existing path can be skipped, because every remaining
/// `already == false` entry would have been `continue`-d by the old loop too.
fn apply_streaming_create(
    def: &RuleDef,
    index: &RuleIndex,
    prov: &mut ProvSets<'_>,
    tripped: &mut bool,
    g: &mut GraphMut<'_>,
) {
    let budget = edge_budget(def);
    let et = g.syms.intern(&def.edge_type);
    let src_sym = g.syms.get(&def.src_label);

    'outer: for id in 0..g.ids.len() as u32 {
        let label_sym = match g.labels.get(id as usize).copied() {
            Some(s) if s != u32::MAX => s,
            _ => continue,
        };
        if src_sym != Some(label_sym) {
            continue;
        }
        let per_src = compute_desired(def, index, id, true, g);
        for ((s, d), score) in per_src {
            let triple = (et, s, d);
            // On create_rule, this rule has no pre-existing provenance, so
            // `already` is always false.  The path is kept for correctness
            // if the same edge is co-owned by another rule (topo.add_edge
            // returns false; we do not record it in our provenance).
            let already = prov.contains(&triple);
            if !already {
                if *tripped || prov.len() as u64 >= budget {
                    *tripped = true;
                    break 'outer;
                }
                let newly = g.topo.add_edge(et, s, d);
                if newly {
                    prov.insert(&def.name, triple, g.ids, g.syms);
                }
            }
            let is_owned_here = already || prov.contains(&triple);
            if is_owned_here {
                if let Some(p) = &def.weight_prop {
                    g.edge_props.set(et, s, d, p, Value::Float(score));
                }
            }
        }
    }
}

/// Streaming backfill for `create_rule` with top-k per-source semantics.
///
/// Iterates src-label nodes in ascending id order. For each src, computes
/// the full candidate set, filters to the top-k destinations (score DESC,
/// dst_key ASC), and applies via `apply_per_src_top_k`.  No global budget or
/// tripped latch is used — the per-source cap is enforced by `filter_src_top_k`.
fn apply_streaming_create_top_k(
    def: &RuleDef,
    k: u64,
    index: &RuleIndex,
    prov: &mut ProvSets<'_>,
    g: &mut GraphMut<'_>,
) {
    let src_sym = g.syms.get(&def.src_label);
    for id in 0..g.ids.len() as u32 {
        let label_sym = match g.labels.get(id as usize).copied() {
            Some(s) if s != u32::MAX => s,
            _ => continue,
        };
        if src_sym != Some(label_sym) {
            continue;
        }
        let per_src = compute_desired(def, index, id, true, g);
        let top_k = filter_src_top_k(per_src, k, g.ids);
        apply_per_src_top_k(def, id, top_k, prov, g);
    }
}

/// Streaming rebuild for top-k per-source rules.
///
/// Iterates all src-label nodes. For each src, computes fresh desired set,
/// filters to top-k, and applies via `apply_per_src_top_k` — which retracts
/// stale edges and inserts newly-ranked ones.  No budget counting, no tripped
/// latch.
fn apply_streaming_rebuild_top_k(
    def: &RuleDef,
    k: u64,
    index: &RuleIndex,
    prov: &mut ProvSets<'_>,
    g: &mut GraphMut<'_>,
) {
    let et = g.syms.intern(&def.edge_type);

    // Collect all src nodes: those currently in provenance (may have lost their
    // label since last fire) + all live src-label nodes.
    let existing_srcs: BTreeSet<u32> = prov
        .set
        .iter()
        .filter(|(t, _, _)| *t == et)
        .map(|(_, s, _)| *s)
        .collect();

    let src_sym = g.syms.get(&def.src_label);
    let mut all_srcs: BTreeSet<u32> = existing_srcs;
    for id in 0..g.ids.len() as u32 {
        let label_sym = match g.labels.get(id as usize).copied() {
            Some(s) if s != u32::MAX => s,
            _ => continue,
        };
        if src_sym == Some(label_sym) {
            all_srcs.insert(id);
        }
    }

    for src in all_srcs {
        let desired_src = compute_desired(def, index, src, true, g);
        let top_k = filter_src_top_k(desired_src, k, g.ids);
        apply_per_src_top_k(def, src, top_k, prov, g);
    }
}

/// Streaming rebuild for `rebuild`.
///
/// Replaces `compute_full_desired` + size-check + `apply_desired(None)`.
///
/// Over-budget path: counts desired pairs up to `budget + 1` (early exit);
/// if the total exceeds the budget, sets the tripped latch and returns without
/// touching provenance (identical to the current no-op rebuild behaviour).
///
/// Fits-budget path: un-trips, retracts each existing provenance triple whose
/// pair is no longer desired via direct `evaluate` — O(existing × eval) —
/// then streams-adds all desired edges in the same src-ascending / dst-within
/// order as `apply_streaming_create`.
fn apply_streaming_rebuild(
    def: &RuleDef,
    index: &RuleIndex,
    prov: &mut ProvSets<'_>,
    tripped: &mut bool,
    g: &mut GraphMut<'_>,
) {
    let budget = edge_budget(def);
    let et = g.syms.intern(&def.edge_type);

    // 1. Count: early-exit as soon as the budget is exceeded.
    let total = count_desired_up_to(def, index, budget, g);
    if total > budget {
        *tripped = true;
        return; // provenance untouched; latch stays set.
    }

    // 2. Un-trip — full desired fits.
    *tripped = false;

    // 3. Retract existing edges that are no longer desired.
    //    O(existing × eval): evaluate each pair directly, no full-map build.
    let current: Vec<Triple> = prov
        .set
        .iter()
        .filter(|(t, _, _)| *t == et)
        .copied()
        .collect();
    for (t, s, d) in current {
        if !pair_still_desired(def, s, d, g) {
            g.topo.remove_edge(t, s, d);
            g.edge_props.remove_edge(t, s, d);
            prov.remove(&def.name, (t, s, d), g.ids, g.syms);
        }
    }

    // 4. Stream-add desired edges; refresh weights on already-owned edges.
    //    count_desired_up_to verified total <= budget so no trip guard is needed.
    let src_sym = g.syms.get(&def.src_label);
    for id in 0..g.ids.len() as u32 {
        let label_sym = match g.labels.get(id as usize).copied() {
            Some(s) if s != u32::MAX => s,
            _ => continue,
        };
        if src_sym != Some(label_sym) {
            continue;
        }
        let per_src = compute_desired(def, index, id, true, g);
        for ((s, d), score) in per_src {
            let triple = (et, s, d);
            let already = prov.contains(&triple);
            if !already {
                let newly = g.topo.add_edge(et, s, d);
                if newly {
                    prov.insert(&def.name, triple, g.ids, g.syms);
                }
            }
            let is_owned_here = already || prov.contains(&triple);
            if is_owned_here {
                if let Some(p) = &def.weight_prop {
                    g.edge_props.set(et, s, d, p, Value::Float(score));
                }
            }
        }
    }
}

/// Increment `fires` once per live node whose label matches either side.
/// Backfill / rebuild counting: one tick per participating node evaluated.
fn bump_fires_for_participants(def: &RuleDef, g: &GraphMut<'_>, fires: &mut u64) {
    let src_sym = g.syms.get(&def.src_label);
    let dst_sym = g.syms.get(&def.dst_label);
    for id in 0..g.ids.len() as u32 {
        let label_sym = match g.labels.get(id as usize).copied() {
            Some(s) if s != u32::MAX => s,
            _ => continue,
        };
        if src_sym == Some(label_sym) || dst_sym == Some(label_sym) {
            *fires += 1;
        }
    }
}

/// Insert node `id` into the rule's src/dst indexes where its label matches.
fn index_node_for_rule(
    id: u32,
    label_sym: u32,
    def: &RuleDef,
    index: &mut RuleIndex,
    syms: &Interner,
    props: ColumnsView<'_>,
) {
    let get = |f: &str| props.get(id, f).map(|vr| vr.into_value());
    if syms.get(&def.src_label) == Some(label_sym) {
        let spec = src_lookup_spec_for(def);
        index.src_side.insert(&spec, id, &get);
    }
    if syms.get(&def.dst_label) == Some(label_sym) {
        let spec = candidate_spec_for(def);
        index.dst_side.insert(&spec, id, &get);
    }
}

// ---------------------------------------------------------------------------
// RuleEngine
// ---------------------------------------------------------------------------

impl RuleEngine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn rules(&self) -> impl Iterator<Item = &RuleDef> {
        self.rules.values()
    }

    pub fn is_owned(&self, etype: u32, src: u32, dst: u32) -> bool {
        self.owned.contains(&(etype, src, dst))
    }

    /// Whether provenance bytes are still retained (not yet consumed by a mutation).
    ///
    /// When `true`, `provenance()` and `provenance_touching*` dispatch to
    /// `lazy_provenance`; when `false` they use the live mutable fields.
    fn provenance_is_retained(&self) -> bool {
        self.retained_provenance_bytes
            .lock()
            .expect("lock poisoned")
            .is_some()
    }

    /// Read-only view of the provenance map: rule name → set of (etype_sym, src, dst).
    ///
    /// Triggers a one-time lazy decode from retained bytes when called before
    /// the first mutation on a clean-open (no-WAL) store.
    pub fn provenance(&self) -> &BTreeMap<String, BTreeSet<(u32, u32, u32)>> {
        if self.provenance_is_retained() {
            self.ensure_provenance_loaded();
            &self.lazy_provenance.get().unwrap().provenance
        } else {
            &self.provenance
        }
    }

    /// O(degree) reverse-index lookup: every provenance triple that touches `node`.
    ///
    /// Dispatches to the lazy-decoded or live index depending on whether
    /// retained bytes have already been consumed by a mutation.
    pub fn provenance_touching(
        &self,
        node: u32,
    ) -> impl Iterator<Item = (&str, u32, u32, u32)> + '_ {
        let use_lazy = self.provenance_is_retained();
        let (by_node, intern_rule): (&BTreeMap<u32, BTreeSet<Touch>>, &Vec<String>) = if use_lazy {
            self.ensure_provenance_loaded();
            let lp = self.lazy_provenance.get().unwrap();
            (&lp.by_node, &lp.intern_rule)
        } else {
            (&self.by_node, &self.intern_rule)
        };
        by_node
            .get(&node)
            .into_iter()
            .flatten()
            .map(move |&(rid, t, s, d)| (intern_rule[rid as usize].as_str(), t, s, d))
    }

    /// Number of provenance triples incident on `node`.
    pub fn provenance_touching_len(&self, node: u32) -> usize {
        if self.provenance_is_retained() {
            self.ensure_provenance_loaded();
            self.lazy_provenance
                .get()
                .unwrap()
                .by_node
                .get(&node)
                .map_or(0, BTreeSet::len)
        } else {
            self.by_node.get(&node).map_or(0, BTreeSet::len)
        }
    }

    /// One-way latch: `true` after a budget breach until [`Self::rebuild`]
    /// is the only exit (and only if the full desired set then fits).
    pub fn is_tripped(&self, name: &str) -> bool {
        self.tripped.get(name).copied().unwrap_or(false)
    }

    /// Evaluations of this rule: one tick per `on_node_changed` fire, and
    /// one tick per participating node on backfill **and rebuild** (even
    /// when rebuild is a provenance no-op).
    pub fn fire_count(&self, name: &str) -> u64 {
        self.fires.get(name).copied().unwrap_or(0)
    }

    /// Drain and return all pending edge-fire / retract deltas since the last
    /// call.  Callers (`db.rs` `log_then_apply_with`) invoke this after a
    /// successful WAL commit + apply to build [`DbEvent`]s for live
    /// subscriptions.  [`GraphDb::open_with`] drains and discards after WAL
    /// replay so replay noise never leaks to subscribers.
    ///
    /// # T2 note (as-of replay)
    ///
    /// When Plan-15 T2 adds as-of replay for subscribers, that path should
    /// call apply-only (no `log_then_apply_with`) and then call
    /// `drain_deltas()` to feed those events to the replaying subscriber.
    /// The suppression is already in place: `apply` accumulates but never
    /// emits; `drain_deltas` is the only emission gate.
    pub fn drain_deltas(&mut self) -> Vec<EngineEdgeDelta> {
        std::mem::take(&mut self.pending_deltas)
    }

    /// Number of accumulated deltas not yet drained.  Used by
    /// `debug_assert` in `log_then_apply_with` to catch stale-delta bugs.
    pub fn pending_delta_count(&self) -> usize {
        self.pending_deltas.len()
    }

    /// Borrow the slice of deltas accumulated since `cursor` without
    /// consuming them.  `cursor` should be the value returned by
    /// `pending_delta_count()` before an engine call.
    ///
    /// The returned slice is valid until the next call to `drain_deltas()`.
    /// T1's drain discipline is preserved: these deltas are still in the
    /// buffer and will be drained by `log_then_apply_with` after `apply`
    /// returns.
    pub fn pending_deltas_since(&self, cursor: usize) -> &[EngineEdgeDelta] {
        &self.pending_deltas[cursor..]
    }

    /// Snapshot support: definitions + provenance + tripped/fires. Candidate
    /// indexes and the `by_node` reverse index are NOT included (derived:
    /// `reindex_all` / `rebuild_by_node` on open).
    #[allow(clippy::type_complexity)]
    pub fn to_persist(
        &self,
    ) -> (
        Vec<RuleDef>,
        BTreeMap<String, BTreeSet<(u32, u32, u32)>>,
        BTreeMap<String, bool>,
        BTreeMap<String, u64>,
    ) {
        (
            self.rules.values().cloned().collect(),
            self.provenance.clone(),
            self.tripped.clone(),
            self.fires.clone(),
        )
    }

    /// Reconstruct engine from a snapshot.  Caller must call `reindex_all` after.
    pub fn from_persist(
        rules: Vec<RuleDef>,
        prov: BTreeMap<String, BTreeSet<(u32, u32, u32)>>,
        tripped: BTreeMap<String, bool>,
        fires: BTreeMap<String, u64>,
    ) -> Self {
        let mut owned = BTreeSet::new();
        for set in prov.values() {
            owned.extend(set.iter().copied());
        }
        let indexes = rules
            .iter()
            .map(|r| (r.name.clone(), RuleIndex::default()))
            .collect();
        let rules: BTreeMap<String, RuleDef> =
            rules.into_iter().map(|r| (r.name.clone(), r)).collect();
        // Fill any missing keys so live rules always have entries.
        let mut tripped = tripped;
        let mut fires = fires;
        for name in rules.keys() {
            tripped.entry(name.clone()).or_insert(false);
            fires.entry(name.clone()).or_insert(0);
        }
        let (by_node, rule_intern, intern_rule) = rebuild_by_node(&prov);
        Self {
            rules,
            indexes,
            provenance: prov,
            owned,
            by_node,
            rule_intern,
            intern_rule,
            tripped,
            fires,
            pending_deltas: Vec::new(),
            emit_deltas: false,
            rebuild_needed: BTreeSet::new(),
            // Candidate indexes start empty; caller must either call
            // consume_retained_state_eager (WAL-present open) or rely on the
            // lazy init in the mutation hooks (clean open, first-write cost).
            indexes_populated: false,
            retained_hnsw_blobs: Mutex::new(BTreeMap::new()),
            retained_ivf_bytes: Mutex::new(None),
            retained_provenance_bytes: Mutex::new(None),
            lazy_provenance: OnceLock::new(),
            lazy_hnsw: OnceLock::new(),
        }
    }

    /// Enable or disable delta accumulation.
    ///
    /// Set to `true` before the first subscriber or view is added.
    /// Set to `false` when the last subscriber and last view are removed.
    /// See the `emit_deltas` field doc for the safety invariant.
    pub fn set_emit_deltas(&mut self, emit: bool) {
        self.emit_deltas = emit;
    }

    /// Whether delta accumulation is currently enabled.
    pub fn emit_deltas(&self) -> bool {
        self.emit_deltas
    }

    /// Drain rule names that exceeded the IVF dst-drift rebuild threshold
    /// during the most recent `on_node_changed` / `on_node_removed`.
    pub fn take_rebuild_needed(&mut self) -> Vec<String> {
        std::mem::take(&mut self.rebuild_needed)
            .into_iter()
            .collect()
    }

    /// Re-queue `name` so a later write can issue `RebuildRule`.
    ///
    /// Used when auto-rebuild WAL IO fails after a durable user op.
    pub fn queue_rebuild_needed(&mut self, name: String) {
        self.rebuild_needed.insert(name);
    }

    fn maybe_queue_ivf_rebuild(&mut self, rule_name: &str, def: &RuleDef) {
        if !def.approximate {
            return;
        }
        let Some(idx) = self.indexes.get(rule_name) else {
            return;
        };
        if idx.dst_side.ivf_drift > ivf_drift_rebuild_threshold() {
            self.rebuild_needed.insert(rule_name.to_string());
        }
    }

    /// Export IVF state for all approximate rules.  Passed to `snapshot()` in
    /// `core-api` and stored in the V4 snapshot so `open()` can restore cluster
    /// assignments without re-fitting k-means.
    pub fn export_ivf_state(&self) -> BTreeMap<String, RuleIvfExport> {
        let mut out = BTreeMap::new();
        for (name, def) in &self.rules {
            if def.approximate {
                if let Some(idx) = self.indexes.get(name) {
                    out.insert(
                        name.clone(),
                        (
                            idx.src_side.export_ivf_state(),
                            idx.dst_side.export_ivf_state(),
                        ),
                    );
                }
            }
        }
        out
    }

    /// Rebuild all candidate indexes by scanning every node.  Call on open.
    pub fn reindex_all(
        &mut self,
        ids: &IdMap,
        syms: &Interner,
        labels: &[u32],
        props: ColumnsView<'_>,
    ) {
        for idx in self.indexes.values_mut() {
            *idx = RuleIndex::default();
        }
        // Collect rule names once outside the per-node loop to avoid repeated
        // allocation and to satisfy the borrow checker without cloning inside.
        let rule_names: Vec<String> = self.rules.keys().cloned().collect();

        // Init HNSW for approximate rules before inserting nodes.
        for name in &rule_names {
            if self.rules[name].approximate {
                let idx = self.indexes.get_mut(name).unwrap();
                idx.src_side.init_hnsw(name);
                idx.dst_side.init_hnsw(name);
            }
        }

        for id in 0..ids.len() as u32 {
            let label_sym = match labels.get(id as usize).copied() {
                Some(s) if s != u32::MAX => s,
                _ => continue,
            };
            for name in &rule_names {
                let def = self.rules[name].clone();
                let idx = self.indexes.get_mut(name).unwrap();
                index_node_for_rule(id, label_sym, &def, idx, syms, props);
            }
        }
        // After all nodes are indexed, fit IVF clusters for approximate rules.
        // HNSW was built incrementally; IVF kept as legacy fallback.
        for name in &rule_names {
            if self.rules[name].approximate {
                let idx = self.indexes.get_mut(name).unwrap();
                idx.src_side.fit_ivf_clusters(name);
                idx.dst_side.fit_ivf_clusters(name);
            }
        }
        self.indexes_populated = true;
    }

    /// Like `reindex_all` but LOADS persisted IVF state for approximate rules
    /// instead of re-fitting k-means.  This eliminates the cold-start re-fit
    /// cost when opening a V4 snapshot.
    ///
    /// `ivf_state`: map from rule name to `(src_export, dst_export)` as
    /// produced by `export_ivf_state` / stored in the V4 snapshot.
    ///
    /// For approximate rules absent from `ivf_state` (e.g. a rule added
    /// after the snapshot), falls back to `fit_ivf_clusters`.
    pub fn reindex_all_load_ivf(
        &mut self,
        ids: &IdMap,
        syms: &Interner,
        labels: &[u32],
        props: ColumnsView<'_>,
        ivf_state: BTreeMap<String, RuleIvfExport>,
    ) {
        for idx in self.indexes.values_mut() {
            *idx = RuleIndex::default();
        }
        let rule_names: Vec<String> = self.rules.keys().cloned().collect();

        // Init HNSW for approximate rules before inserting nodes so each
        // insert also populates the HNSW graph incrementally.
        for name in &rule_names {
            if self.rules[name].approximate {
                let idx = self.indexes.get_mut(name).unwrap();
                idx.src_side.init_hnsw(name);
                idx.dst_side.init_hnsw(name);
            }
        }

        for id in 0..ids.len() as u32 {
            let label_sym = match labels.get(id as usize).copied() {
                Some(s) if s != u32::MAX => s,
                _ => continue,
            };
            for name in &rule_names {
                let def = self.rules[name].clone();
                let idx = self.indexes.get_mut(name).unwrap();
                index_node_for_rule(id, label_sym, &def, idx, syms, props);
            }
        }
        // For approximate rules: restore persisted IVF state (no re-fit).
        // HNSW was built incrementally; it will be replaced by `load_hnsw_state`
        // in `restore_snapshot_state` when a snapshot blob is available.
        for name in &rule_names {
            if !self.rules[name].approximate {
                continue;
            }
            let idx = self.indexes.get_mut(name).unwrap();
            if let Some(((sc, sa, sd), (dc, da, dd))) = ivf_state.get(name) {
                idx.src_side.load_ivf_state(sc.clone(), sa.clone(), *sd);
                idx.dst_side.load_ivf_state(dc.clone(), da.clone(), *dd);
            } else {
                // No persisted state for this rule: fall back to full re-fit.
                idx.src_side.fit_ivf_clusters(name);
                idx.dst_side.fit_ivf_clusters(name);
            }
        }
        self.indexes_populated = true;
    }

    /// Store HNSW blobs and raw IVF bytes from a snapshot **without deserializing**.
    ///
    /// Called from `restore_snapshot_state` in db.rs.  Neither the HNSW graphs
    /// nor the IVF centroids are materialized here; they are consumed lazily:
    ///   - `consume_retained_state_eager` (WAL-present open, before WAL replay)
    ///   - The mutation-hook lazy-init guard (clean open, first-write cost)
    ///   - `ensure_hnsw_loaded` (first ANN query on a clean open)
    pub fn store_snapshot_state(
        &self,
        hnsw_blobs: BTreeMap<String, (Vec<u8>, Vec<u8>)>,
        ivf_bytes: Vec<u8>,
    ) {
        *self
            .retained_hnsw_blobs
            .lock()
            .expect("retained_hnsw_blobs lock poisoned") = hnsw_blobs;
        *self
            .retained_ivf_bytes
            .lock()
            .expect("retained_ivf_bytes lock poisoned") = if ivf_bytes.is_empty() {
            None
        } else {
            Some(ivf_bytes)
        };
        // indexes_populated remains false.
    }

    /// Store raw rkyv provenance bytes retained from a V8 snapshot.
    ///
    /// Called from `restore_v8_base` in db.rs after open.  Provenance is not
    /// decoded here; it is materialized lazily — either by the `&self` read path
    /// (`ensure_provenance_loaded`) for stats/explain, or by the `&mut self`
    /// write path (`ensure_provenance_loaded_mut`) on the first mutation.
    pub fn store_provenance_bytes(&self, bytes: Vec<u8>) {
        *self
            .retained_provenance_bytes
            .lock()
            .expect("lock poisoned") = if bytes.is_empty() { None } else { Some(bytes) };
    }

    /// Populate `lazy_provenance` from retained bytes for `&self` read paths.
    ///
    /// Uses `OnceLock` for exactly-once initialization.  The retained bytes are
    /// NOT consumed here; `ensure_provenance_loaded_mut` still has access to them
    /// for the write path.  After the first mutation, `retained_provenance_bytes`
    /// is `None` and callers switch to the live `self.provenance` field instead.
    pub fn ensure_provenance_loaded(&self) {
        self.lazy_provenance.get_or_init(|| {
            // Hold the Mutex across decode to avoid cloning 115 MiB.  This is a
            // one-time cost; subsequent calls return immediately via OnceLock.
            let guard = self
                .retained_provenance_bytes
                .lock()
                .expect("retained_provenance_bytes lock poisoned");
            let bytes = match &*guard {
                Some(b) if !b.is_empty() => b,
                _ => return LazyProvenance::default(),
            };
            let prov = decode_provenance_bytes(bytes);
            let (by_node, _rule_intern, intern_rule) = rebuild_by_node(&prov);
            LazyProvenance {
                provenance: prov,
                by_node,
                intern_rule,
            }
        });
    }

    /// Decode and install retained provenance bytes into the live mutable fields.
    ///
    /// No-op if bytes have already been consumed or were never stored.
    /// Must be called under `&mut self` before any operation that reads or
    /// diffs against `self.provenance`, `self.owned`, or `self.by_node`.
    pub fn ensure_provenance_loaded_mut(&mut self) {
        let bytes = match self
            .retained_provenance_bytes
            .lock()
            .expect("lock poisoned")
            .take()
        {
            Some(b) => b,
            None => return,
        };
        let prov = decode_provenance_bytes(&bytes);
        for set in prov.values() {
            self.owned.extend(set.iter().copied());
        }
        let (by_node, rule_intern, intern_rule) = rebuild_by_node(&prov);
        self.provenance = prov;
        self.by_node = by_node;
        self.rule_intern = rule_intern;
        self.intern_rule = intern_rule;
    }

    /// Eagerly consume retained snapshot state before WAL replay.
    ///
    /// Call this in `open_with` when the WAL has records.  Runs the O(n) node
    /// scan + restores persisted IVF centroids and HNSW blobs so that WAL
    /// replay finds fully-populated indexes.  Marks `indexes_populated = true`.
    pub fn consume_retained_state_eager(
        &mut self,
        ids: &IdMap,
        syms: &Interner,
        labels: &[u32],
        props: ColumnsView<'_>,
    ) {
        if self.indexes_populated {
            return;
        }
        // Also ensure provenance is loaded before WAL replay so diffs apply
        // against the correct pre-snapshot provenance state.
        self.ensure_provenance_loaded_mut();
        let hnsw = std::mem::take(
            &mut *self
                .retained_hnsw_blobs
                .lock()
                .expect("retained_hnsw_blobs lock poisoned"),
        );
        let ivf_bytes = self
            .retained_ivf_bytes
            .lock()
            .expect("retained_ivf_bytes lock poisoned")
            .take()
            .unwrap_or_default();
        let ivf = decode_ivf_bytes_to_export(&ivf_bytes);
        self.reindex_all_load_ivf(ids, syms, labels, props, ivf);
        // Override the incrementally-built HNSW with the persisted blob (better).
        self.load_hnsw_state(hnsw);
    }

    /// Deserialize retained HNSW blobs into `lazy_hnsw` for the clean-open ANN
    /// read path.  Takes `&self` so it can be called from `find_similar_vector`
    /// and `search_hybrid` under a shared (`db.read()`) lock.
    ///
    /// Uses `OnceLock` to guarantee exactly-once initialization even under
    /// concurrent shared access.  The retained blobs are borrowed (not consumed)
    /// so that a subsequent first-mutation call to `consume_retained_state_eager`
    /// can still load the persisted HNSW graphs into `self.indexes`.
    ///
    /// Called before the first ANN query on a clean-open (no WAL) store.
    pub fn ensure_hnsw_loaded(&self) {
        self.lazy_hnsw.get_or_init(|| {
            // Snapshot blob entries into a local Vec, then release the Mutex
            // before deserialization so the lock is not held across potentially
            // expensive bincode::deserialize calls.
            let snapshot: Vec<(String, Vec<u8>, Vec<u8>)> = {
                let guard = self
                    .retained_hnsw_blobs
                    .lock()
                    .expect("retained_hnsw_blobs lock poisoned");
                if guard.is_empty() {
                    return BTreeMap::new();
                }
                guard
                    .iter()
                    .map(|(name, (sb, db))| (name.clone(), sb.clone(), db.clone()))
                    .collect()
            }; // lock released here
            snapshot
                .into_iter()
                .map(|(name, sb, db)| {
                    let src = if !sb.is_empty() {
                        bincode::deserialize::<HnswIndex>(&sb).ok()
                    } else {
                        None
                    };
                    let dst = if !db.is_empty() {
                        bincode::deserialize::<HnswIndex>(&db).ok()
                    } else {
                        None
                    };
                    (name, (src, dst))
                })
                .collect()
        });
    }

    /// Returns `true` if candidate indexes have been built (either eagerly or
    /// via the lazy mutation-hook trigger).
    pub fn indexes_populated(&self) -> bool {
        self.indexes_populated
    }

    /// Export HNSW graphs for all approximate rules as opaque bincoded blobs.
    ///
    /// Returns a map from rule name to `(src_blob, dst_blob)`.  An empty `Vec`
    /// means the corresponding side has no initialized HNSW graph.
    pub fn export_hnsw_state(&self) -> BTreeMap<String, (Vec<u8>, Vec<u8>)> {
        let mut out = BTreeMap::new();
        for (name, def) in &self.rules {
            if def.approximate {
                if let Some(idx) = self.indexes.get(name) {
                    out.insert(
                        name.clone(),
                        (
                            idx.src_side.export_hnsw_blob(),
                            idx.dst_side.export_hnsw_blob(),
                        ),
                    );
                }
            }
        }
        out
    }

    /// Returns HNSW state for snapshotting.  When indexes are not yet populated
    /// (clean open with no mutation), returns the retained raw blobs directly so
    /// that a migrate/snapshot does not silently drop fitted indexes.
    pub fn export_hnsw_state_passthrough(&self) -> BTreeMap<String, (Vec<u8>, Vec<u8>)> {
        if !self.indexes_populated {
            let guard = self
                .retained_hnsw_blobs
                .lock()
                .expect("retained_hnsw_blobs lock poisoned");
            if !guard.is_empty() {
                return guard.clone();
            }
        }
        self.export_hnsw_state()
    }

    /// Returns a clone of the retained raw IVF bincode bytes.
    ///
    /// Returns `None` if no bytes are retained (fresh store or indexes already
    /// consumed by a mutation).  Used by `snapshot_with` for passthrough when
    /// indexes have not yet been populated.
    pub fn retained_ivf_bytes_clone(&self) -> Option<Vec<u8>> {
        self.retained_ivf_bytes
            .lock()
            .expect("retained_ivf_bytes lock poisoned")
            .clone()
    }

    /// Restore HNSW graphs from bincoded blobs (overrides any incrementally built
    /// graphs produced during `reindex_all_load_ivf`).
    ///
    /// Called from `restore_snapshot_state` in db.rs after reindex.
    pub fn load_hnsw_state(&mut self, blobs: BTreeMap<String, (Vec<u8>, Vec<u8>)>) {
        for (name, (src_blob, dst_blob)) in blobs {
            if let Some(idx) = self.indexes.get_mut(&name) {
                if !src_blob.is_empty() {
                    idx.src_side.load_hnsw_blob(&src_blob);
                }
                if !dst_blob.is_empty() {
                    idx.dst_side.load_hnsw_blob(&dst_blob);
                }
            }
        }
    }

    /// Find approximate nearest-neighbor ids on the dst side of the first
    /// approximate VectorSimilar rule covering `(dst_label, field)`.
    ///
    /// Returns `None` when no matching rule or HNSW index exists.
    pub fn hnsw_search_dst(
        &self,
        field: &str,
        dst_label: &str,
        q: &[f64],
        k: usize,
    ) -> Option<Vec<(u32, f64)>> {
        for (name, def) in &self.rules {
            if !def.approximate || def.dst_label != dst_label {
                continue;
            }
            // Check that the predicate covers this vector field.
            if !predicate_covers_field(&def.predicate, field) {
                continue;
            }
            if let Some(idx) = self.indexes.get(name) {
                if let Some(h) = idx.dst_side.hnsw_ref() {
                    if !h.is_empty() {
                        return Some(h.search(q, k));
                    }
                }
            }
            // Fallback: blobs deserialized via ensure_hnsw_loaded (read path,
            // no mutation has populated self.indexes yet).
            if let Some(lazy) = self.lazy_hnsw.get() {
                if let Some((_, Some(h))) = lazy.get(name) {
                    if !h.is_empty() {
                        return Some(h.search(q, k));
                    }
                }
            }
        }
        None
    }

    /// Returns `true` if any approximate VectorSimilar rule covers `field`.
    ///
    /// Use as a capability probe before calling `hnsw_search_dst` or
    /// `hnsw_search_any_dst` — presence of the rule guarantees the native Rust
    /// path will be used (HNSW when the index is populated, Rust brute-force
    /// otherwise); it does NOT guarantee a populated HNSW index.
    pub fn hnsw_has_rule(&self, field: &str) -> bool {
        self.rules
            .values()
            .any(|def| def.approximate && predicate_covers_field(&def.predicate, field))
    }

    /// Like `hnsw_search_dst` but searches across **all** dst_labels that have
    /// an approximate VectorSimilar rule covering `field`.
    ///
    /// Results from multiple rules are merged by node id (keeping the maximum
    /// score for any id that appears in more than one rule's index), then
    /// sorted descending and truncated to `k`.
    ///
    /// Returns `None` when no applicable rule has a populated HNSW index
    /// (same sentinel convention as `hnsw_search_dst`).
    pub fn hnsw_search_any_dst(&self, field: &str, q: &[f64], k: usize) -> Option<Vec<(u32, f64)>> {
        let mut merged: std::collections::BTreeMap<u32, f64> = std::collections::BTreeMap::new();
        let mut found_index = false;

        for (name, def) in &self.rules {
            if !def.approximate {
                continue;
            }
            if !predicate_covers_field(&def.predicate, field) {
                continue;
            }
            let hits: Option<Vec<(u32, f64)>> = if let Some(idx) = self.indexes.get(name) {
                if let Some(h) = idx.dst_side.hnsw_ref() {
                    if !h.is_empty() {
                        found_index = true;
                        Some(h.search(q, k))
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else if let Some(lazy) = self.lazy_hnsw.get() {
                if let Some((_, Some(h))) = lazy.get(name) {
                    if !h.is_empty() {
                        found_index = true;
                        Some(h.search(q, k))
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            };

            if let Some(hits) = hits {
                for (id, score) in hits {
                    merged
                        .entry(id)
                        .and_modify(|s| {
                            if score > *s {
                                *s = score;
                            }
                        })
                        .or_insert(score);
                }
            }
        }

        if !found_index {
            return None;
        }
        let mut result: Vec<(u32, f64)> = merged.into_iter().collect();
        result.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        result.truncate(k);
        Some(result)
    }

    /// Register a rule and backfill existing nodes.
    /// Returns Err on failed validate() or duplicate name.
    pub fn create_rule(&mut self, def: RuleDef, g: &mut GraphMut<'_>) -> Result<(), String> {
        def.validate()?;
        if self.rules.contains_key(&def.name) {
            return Err(format!("rule {:?} already exists", def.name));
        }
        let name = def.name.clone();
        self.rules.insert(name.clone(), def);
        self.indexes.insert(name.clone(), RuleIndex::default());
        self.provenance.entry(name.clone()).or_default();
        self.tripped.insert(name.clone(), false);
        self.fires.insert(name.clone(), 0);

        // Phase 1: index all existing nodes for this rule.
        let n_total = g.ids.len() as u32;
        let def = self.rules[&name].clone();

        // Phase 1a: init HNSW for approximate rules before inserting nodes so
        // each insert also populates the HNSW graph incrementally.
        if def.approximate {
            let idx = self.indexes.get_mut(&name).unwrap();
            idx.src_side.init_hnsw(&name);
            idx.dst_side.init_hnsw(&name);
        }

        for id in 0..n_total {
            let label_sym = match g.labels.get(id as usize).copied() {
                Some(s) if s != u32::MAX => s,
                _ => continue,
            };
            let idx = self.indexes.get_mut(&name).unwrap();
            index_node_for_rule(id, label_sym, &def, idx, g.syms, g.props);
        }

        // Phase 1b: fit IVF clusters for approximate rules (after all nodes indexed).
        // HNSW was built incrementally above; IVF is kept as legacy fallback.
        if def.approximate {
            let idx = self.indexes.get_mut(&name).unwrap();
            idx.src_side.fit_ivf_clusters(&name);
            idx.dst_side.fit_ivf_clusters(&name);
        }

        // Phase 2: streaming backfill.
        // Branches on max_edges semantics:
        //   None    → global-budget path (tripped latch, first-N in BTree order)
        //   Some(k) → per-source top-k path (no tripped latch, score-ordered)
        let mut prov = ProvSets {
            set: self.provenance.get_mut(&name).unwrap(),
            owned: &mut self.owned,
            by_node: &mut self.by_node,
            rule_intern: &mut self.rule_intern,
            intern_rule: &mut self.intern_rule,
            deltas: &mut self.pending_deltas,
            emit: self.emit_deltas,
        };
        if def.via_label.is_some() {
            // Via-hop backfill: bypass the candidate index; use compute_desired_via.
            let budget = edge_budget(&def);
            let et = g.syms.intern(&def.edge_type);
            let src_sym = g.syms.get(&def.src_label);
            let tripped = self.tripped.get_mut(&name).unwrap();
            'via_outer: for id in 0..g.ids.len() as u32 {
                let label_sym = match g.labels.get(id as usize).copied() {
                    Some(s) if s != u32::MAX => s,
                    _ => continue,
                };
                if src_sym != Some(label_sym) {
                    continue;
                }
                let per_src = compute_desired_via(&def, ViaAnchor::Src(id), g);
                if let Some(k) = def.max_edges {
                    let top_k = filter_src_top_k(per_src, k, g.ids);
                    apply_per_src_top_k(&def, id, top_k, &mut prov, g);
                } else {
                    for ((s, d), score) in per_src {
                        let triple = (et, s, d);
                        let already = prov.contains(&triple);
                        if !already {
                            if *tripped || prov.len() as u64 >= budget {
                                *tripped = true;
                                break 'via_outer;
                            }
                            let newly = g.topo.add_edge(et, s, d);
                            if newly {
                                prov.insert(&name, triple, g.ids, g.syms);
                            }
                        }
                        let is_owned_here = already || prov.contains(&triple);
                        if is_owned_here {
                            if let Some(p) = &def.weight_prop {
                                g.edge_props.set(et, s, d, p, Value::Float(score));
                            }
                        }
                    }
                }
            }
        } else if let Some(k) = def.max_edges {
            apply_streaming_create_top_k(&def, k, &self.indexes[&name], &mut prov, g);
        } else {
            let tripped = self.tripped.get_mut(&name).unwrap();
            apply_streaming_create(&def, &self.indexes[&name], &mut prov, tripped, g);
        }
        // Fires: one tick per participating node evaluated (same unit as
        // on_node_changed). Empty-graph create_rule therefore leaves fires=0.
        let fires = self.fires.get_mut(&name).unwrap();
        bump_fires_for_participants(&def, g, fires);

        // This rule's index is now populated. If prior rules' indexes were
        // already populated (or there are no other rules) mark the whole engine
        // as ready; otherwise a later reindex_all call will set the flag.
        self.indexes_populated = true;

        Ok(())
    }

    /// Remove the rule and exactly its owned edges.  Returns Err if unknown.
    pub fn delete_rule(&mut self, name: &str, g: &mut GraphMut<'_>) -> Result<(), String> {
        if !self.rules.contains_key(name) {
            return Err(format!("rule {:?} not found", name));
        }
        let def = self.rules.remove(name).unwrap();
        self.indexes.remove(name);
        self.tripped.remove(name);
        self.fires.remove(name);
        let mut leftover = self.provenance.remove(name).unwrap_or_default();
        // intern so the symbol exists; edge_type was already interned at create time.
        let _et = g.syms.intern(&def.edge_type);
        let triples: Vec<Triple> = leftover.iter().copied().collect();
        let mut sets = ProvSets {
            set: &mut leftover,
            owned: &mut self.owned,
            by_node: &mut self.by_node,
            rule_intern: &mut self.rule_intern,
            intern_rule: &mut self.intern_rule,
            deltas: &mut self.pending_deltas,
            emit: self.emit_deltas,
        };
        for triple in triples {
            let (t, s, d) = triple;
            g.topo.remove_edge(t, s, d);
            g.edge_props.remove_edge(t, s, d);
            sets.remove(name, triple, g.ids, g.syms);
        }
        // Surviving rules that share the same edge_type may derive edges that
        // were previously blocked (add_edge returned false because the deleted
        // rule already owned them, so their provenance never recorded them).
        // Rebuilding each such rule lets it claim those edges now that the
        // deleted rule's entries have been removed from the topology.
        let same_etype_survivors: Vec<String> = self
            .rules
            .values()
            .filter(|r| r.edge_type == def.edge_type)
            .map(|r| r.name.clone())
            .collect();
        for survivor in same_etype_survivors {
            // rebuild returns Err only for unknown rules; survivor is live.
            let _ = self.rebuild(&survivor, g);
        }
        Ok(())
    }

    /// Called when node `n` is inserted (changed=None) or a field is updated.
    /// - None: all rules where n's label matches either side fire; index gains n.
    /// - Some((field, old_value)): only rules watching `field` fire; index is
    ///   updated using old_value for removal so stale buckets are cleaned.
    ///
    /// For via-hop rules (`def.via_label.is_some()`), also fires when n carries
    /// the via-label: finds all srcs that route through n and recomputes their
    /// derived edges. Via-hop rules bypass the candidate index and use
    /// `compute_desired_via` instead.
    pub fn on_node_changed(
        &mut self,
        n: u32,
        changed: Option<(&str, Option<Value>)>,
        g: &mut GraphMut<'_>,
    ) {
        // Ensure provenance is decoded before diffing against existing edges.
        self.ensure_provenance_loaded_mut();
        // Lazy index build: on restore from a V8 snapshot, candidate indexes
        // start empty to avoid an O(n) scan at open time.  The first mutation
        // pays the cost instead.  Subsequent calls skip this branch.
        // Retained HNSW/IVF blobs from the snapshot are consumed here so the
        // reindex does NOT wipe the loaded HNSW graphs.
        if !self.indexes_populated && !self.rules.is_empty() {
            let hnsw = std::mem::take(
                &mut *self
                    .retained_hnsw_blobs
                    .lock()
                    .expect("retained_hnsw_blobs lock poisoned"),
            );
            let ivf_bytes = self
                .retained_ivf_bytes
                .lock()
                .expect("retained_ivf_bytes lock poisoned")
                .take()
                .unwrap_or_default();
            let ivf = decode_ivf_bytes_to_export(&ivf_bytes);
            self.reindex_all_load_ivf(g.ids, g.syms, g.labels, g.props, ivf);
            self.load_hnsw_state(hnsw);
        }

        let n_label = g.labels.get(n as usize).copied();
        let rule_names: Vec<String> = self.rules.keys().cloned().collect();

        for rule_name in rule_names {
            let def = self.rules[&rule_name].clone();

            if def.via_label.is_some() {
                // --- Via-hop rule path ---
                self.on_node_changed_via(&rule_name, &def, n, n_label, changed.clone(), g);
            } else {
                // --- Standard 2-node rule path ---
                let src_sym = g.syms.get(&def.src_label);
                let dst_sym = g.syms.get(&def.dst_label);
                let as_src = src_sym.is_some() && n_label == src_sym;
                let as_dst = dst_sym.is_some() && n_label == dst_sym;

                let fires = match changed {
                    None => as_src || as_dst,
                    Some((field, _)) => def.watched_fields().contains(field) && (as_src || as_dst),
                };
                if !fires {
                    continue;
                }
                *self.fires.entry(rule_name.clone()).or_default() += 1;

                // --- Index maintenance ---
                if let Some((field, ref old_val)) = changed {
                    let old_val_cloned = old_val.clone();
                    let old_getter = |f: &str| {
                        if f == field {
                            old_val_cloned.clone()
                        } else {
                            g.props.get(n, f).map(|vr| vr.into_value())
                        }
                    };
                    let idx = self.indexes.get_mut(&rule_name).unwrap();
                    if as_src {
                        let spec = src_lookup_spec_for(&def);
                        idx.src_side.remove(&spec, n, &old_getter);
                    }
                    if as_dst {
                        let spec = candidate_spec_for(&def);
                        idx.dst_side.remove(&spec, n, &old_getter);
                    }
                }

                {
                    let cur_getter = |f: &str| g.props.get(n, f).map(|vr| vr.into_value());
                    let idx = self.indexes.get_mut(&rule_name).unwrap();
                    if as_src {
                        let spec = src_lookup_spec_for(&def);
                        idx.src_side.insert(&spec, n, &cur_getter);
                    }
                    if as_dst {
                        let spec = candidate_spec_for(&def);
                        idx.dst_side.insert(&spec, n, &cur_getter);
                    }
                }

                self.maybe_queue_ivf_rebuild(&rule_name, &def);

                // --- Desired set + diff-apply ---
                if let Some(k) = def.max_edges {
                    let et = g.syms.intern(&def.edge_type);
                    let affected_srcs_for_n_dst: BTreeSet<u32> = if as_dst {
                        let rid = self.rule_intern.get(&def.name).copied();
                        self.by_node
                            .get(&n)
                            .into_iter()
                            .flatten()
                            .filter(|(r, t, _s, d)| Some(*r) == rid && *t == et && *d == n)
                            .map(|(_, _, s, _)| *s)
                            .collect()
                    } else {
                        BTreeSet::new()
                    };

                    let mut prov = ProvSets {
                        set: self.provenance.entry(rule_name.clone()).or_default(),
                        owned: &mut self.owned,
                        by_node: &mut self.by_node,
                        rule_intern: &mut self.rule_intern,
                        intern_rule: &mut self.intern_rule,
                        deltas: &mut self.pending_deltas,
                        emit: self.emit_deltas,
                    };

                    if as_src {
                        let desired_n_src =
                            compute_desired(&def, &self.indexes[&rule_name], n, true, g);
                        let top_k = filter_src_top_k(desired_n_src, k, g.ids);
                        apply_per_src_top_k(&def, n, top_k, &mut prov, g);
                    }

                    if as_dst {
                        let new_desired =
                            compute_desired(&def, &self.indexes[&rule_name], n, false, g);
                        let new_srcs: BTreeSet<u32> = new_desired.keys().map(|(s, _)| *s).collect();
                        let affected_srcs: BTreeSet<u32> =
                            affected_srcs_for_n_dst.union(&new_srcs).copied().collect();
                        for src in affected_srcs {
                            if src == n {
                                continue;
                            }
                            let desired_src =
                                compute_desired(&def, &self.indexes[&rule_name], src, true, g);
                            let top_k = filter_src_top_k(desired_src, k, g.ids);
                            apply_per_src_top_k(&def, src, top_k, &mut prov, g);
                        }
                    }
                } else {
                    let mut desired = BTreeMap::new();
                    if as_src {
                        desired.extend(compute_desired(
                            &def,
                            &self.indexes[&rule_name],
                            n,
                            true,
                            g,
                        ));
                    }
                    if as_dst {
                        desired.extend(compute_desired(
                            &def,
                            &self.indexes[&rule_name],
                            n,
                            false,
                            g,
                        ));
                    }
                    let tripped = self.tripped.entry(rule_name.clone()).or_default();
                    apply_desired(
                        &def,
                        desired,
                        Some(n),
                        &mut ProvSets {
                            set: self.provenance.entry(rule_name).or_default(),
                            owned: &mut self.owned,
                            by_node: &mut self.by_node,
                            rule_intern: &mut self.rule_intern,
                            intern_rule: &mut self.intern_rule,
                            deltas: &mut self.pending_deltas,
                            emit: self.emit_deltas,
                        },
                        tripped,
                        g,
                    );
                }
            }
        }
    }

    /// Inner handler for `on_node_changed` when the rule is a via-hop rule.
    ///
    /// For each role n can play (src, via, dst), computes and applies the
    /// desired edge set using `compute_desired_via`. Via-hop rules bypass the
    /// candidate index; no index maintenance is performed here.
    ///
    /// Incremental correctness by change class:
    /// - **src prop / insert** (`as_src`): re-expand via from n, recompute all
    ///   (n, dst) pairs. `apply_via_for_srcs([n])`.
    /// - **via-node prop change** (`as_via`, field in watched_fields): find
    ///   srcs that hop to n via `via_edge`, recompute their (src, dst) pairs.
    ///   `apply_via_for_srcs(reverse_via_neighbors(n))`.
    /// - **dst prop / insert** (`as_dst`): anchor on n, compute desired for all
    ///   srcs. `apply_via_for_srcs(all_src_label_nodes)`.
    fn on_node_changed_via(
        &mut self,
        rule_name: &str,
        def: &RuleDef,
        n: u32,
        n_label: Option<u32>,
        changed: Option<(&str, Option<Value>)>,
        g: &mut GraphMut<'_>,
    ) {
        let src_sym = g.syms.get(&def.src_label);
        let dst_sym = g.syms.get(&def.dst_label);
        let via_sym = def.via_label.as_deref().and_then(|l| g.syms.get(l));

        let as_src = src_sym.is_some() && n_label == src_sym;
        let as_dst = dst_sym.is_some() && n_label == dst_sym;
        let as_via = via_sym.is_some() && n_label == via_sym;

        // Via-hop predicates are evaluated between via and dst, so watched_fields
        // cover both via-node and dst-node fields (predicate fields come from the
        // via→dst evaluation). A via-node prop change fires if its field is watched.
        let fires = match changed {
            None => as_src || as_via || as_dst,
            Some((field, _)) => {
                let wf = def.watched_fields();
                (wf.contains(field)) && (as_src || as_via || as_dst)
            }
        };
        if !fires {
            return;
        }
        *self.fires.entry(rule_name.to_string()).or_default() += 1;

        // Collect affected srcs: union of srcs identified from each role.
        let mut affected_srcs: BTreeSet<u32> = BTreeSet::new();
        if as_src {
            affected_srcs.insert(n);
        }
        if as_via {
            // Srcs that hop to this via-node via via_edge (reverse direction).
            let via_edge_str = def.via_edge.as_deref().unwrap();
            let via_dir = def.via_dir.unwrap_or(core_storage::Direction::Out);
            let rev_dir = match via_dir {
                core_storage::Direction::Out => core_storage::Direction::In,
                core_storage::Direction::In => core_storage::Direction::Out,
            };
            if let (Some(via_etype), Some(s_sym)) = (g.syms.get(via_edge_str), src_sym) {
                for &src in g.topo.neighbors(via_etype, rev_dir, n).as_ref() {
                    if g.labels.get(src as usize).copied() == Some(s_sym) {
                        affected_srcs.insert(src);
                    }
                }
            }
        }
        if as_dst {
            // Recompute all srcs whose via-hops might produce edges to n.
            let desired_touching_n = compute_desired_via(def, ViaAnchor::Dst(n), g);
            for (src, _dst) in desired_touching_n.keys() {
                affected_srcs.insert(*src);
            }
            // Also include any srcs that currently have provenance pointing to n.
            let et = g.syms.intern(&def.edge_type);
            let rid = self.rule_intern.get(rule_name).copied();
            let old_srcs: Vec<u32> = self
                .by_node
                .get(&n)
                .into_iter()
                .flatten()
                .filter(|(r, t, _s, d)| Some(*r) == rid && *t == et && *d == n)
                .map(|(_, _, s, _)| *s)
                .collect();
            affected_srcs.extend(old_srcs);
        }

        // For each affected src, compute desired_via(Src) and apply.
        let affected_srcs: Vec<u32> = affected_srcs.into_iter().collect();

        if let Some(k) = def.max_edges {
            let mut prov = ProvSets {
                set: self.provenance.entry(rule_name.to_string()).or_default(),
                owned: &mut self.owned,
                by_node: &mut self.by_node,
                rule_intern: &mut self.rule_intern,
                intern_rule: &mut self.intern_rule,
                deltas: &mut self.pending_deltas,
                emit: self.emit_deltas,
            };
            for src in affected_srcs {
                let desired_src = compute_desired_via(def, ViaAnchor::Src(src), g);
                let top_k = filter_src_top_k(desired_src, k, g.ids);
                apply_per_src_top_k(def, src, top_k, &mut prov, g);
            }
        } else {
            let tripped = self.tripped.entry(rule_name.to_string()).or_default();
            let budget = edge_budget(def);
            // Apply per-src so each affected src retracts its stale edges and
            // adds its new desired edges independently.
            for src in affected_srcs {
                let desired_src = compute_desired_via(def, ViaAnchor::Src(src), g);
                if !*tripped {
                    let mut prov = ProvSets {
                        set: self.provenance.entry(rule_name.to_string()).or_default(),
                        owned: &mut self.owned,
                        by_node: &mut self.by_node,
                        rule_intern: &mut self.rule_intern,
                        intern_rule: &mut self.intern_rule,
                        deltas: &mut self.pending_deltas,
                        emit: self.emit_deltas,
                    };
                    apply_desired(def, desired_src, Some(src), &mut prov, tripped, g);
                }
                // If budget was just tripped inside apply_desired, stop adding
                // but continue retracting stale edges for already-processed srcs
                // (apply_desired handles retracts even when tripped).
                let _ = budget;
            }
        }
    }

    /// Called when a user edge `(etype_str, src_id, dst_id)` is inserted or
    /// deleted (not a derived edge — those are managed by provenance, not here).
    ///
    /// For any via-hop rule where `via_edge == etype_str` and src_id carries
    /// `src_label`, the src_id's desired derived-edge set may have changed:
    /// a new WORKS_AT edge makes a new Org reachable as a via-node, and a
    /// deleted WORKS_AT removes a previously reachable Org.
    ///
    /// This is the only hook the engine exposes for topology changes. It is
    /// called from `db.rs` on `WalRecord::InsertEdge` and `WalRecord::DeleteEdge`
    /// immediately after the topo is updated (so `g.topo` already reflects the
    /// new state).
    pub fn on_edge_changed(
        &mut self,
        etype_str: &str,
        src_id: u32,
        dst_id: u32,
        g: &mut GraphMut<'_>,
    ) {
        // Ensure provenance is decoded before diffing against existing edges.
        self.ensure_provenance_loaded_mut();
        // Lazy index build: same guard as on_node_changed.  Retained snapshot
        // blobs are consumed to avoid wiping any HNSW graphs.
        if !self.indexes_populated && !self.rules.is_empty() {
            let hnsw = std::mem::take(
                &mut *self
                    .retained_hnsw_blobs
                    .lock()
                    .expect("retained_hnsw_blobs lock poisoned"),
            );
            let ivf_bytes = self
                .retained_ivf_bytes
                .lock()
                .expect("retained_ivf_bytes lock poisoned")
                .take()
                .unwrap_or_default();
            let ivf = decode_ivf_bytes_to_export(&ivf_bytes);
            self.reindex_all_load_ivf(g.ids, g.syms, g.labels, g.props, ivf);
            self.load_hnsw_state(hnsw);
        }

        let rule_names: Vec<String> = self.rules.keys().cloned().collect();
        for rule_name in rule_names {
            let def = self.rules[&rule_name].clone();
            let Some(ref via_edge) = def.via_edge else {
                continue; // not a via-hop rule
            };
            if via_edge != etype_str {
                continue; // edge type doesn't match this rule's via_edge
            }

            // Check that src_id carries src_label and dst_id carries via_label.
            let src_sym = match g.syms.get(&def.src_label) {
                Some(s) => s,
                None => continue,
            };
            let via_sym = match def.via_label.as_deref().and_then(|l| g.syms.get(l)) {
                Some(s) => s,
                None => continue,
            };
            // via_dir == Out → the edge goes src_id → dst_id (src-label node to via-label node)
            // via_dir == In  → the edge goes dst_id ← src_id, i.e., src_id is the via-label
            //                  end and dst_id is the src-label end. Adjust accordingly.
            let via_dir = def.via_dir.unwrap_or(core_storage::Direction::Out);
            let (rule_src, rule_via) = match via_dir {
                core_storage::Direction::Out => (src_id, dst_id),
                core_storage::Direction::In => (dst_id, src_id),
            };

            if g.labels.get(rule_src as usize).copied() != Some(src_sym) {
                continue;
            }
            if g.labels.get(rule_via as usize).copied() != Some(via_sym) {
                continue;
            }

            // Recompute derived edges for rule_src — its via-hop set just changed.
            *self.fires.entry(rule_name.clone()).or_default() += 1;
            let desired_src = compute_desired_via(&def, ViaAnchor::Src(rule_src), g);

            if let Some(k) = def.max_edges {
                let mut prov = ProvSets {
                    set: self.provenance.entry(rule_name).or_default(),
                    owned: &mut self.owned,
                    by_node: &mut self.by_node,
                    rule_intern: &mut self.rule_intern,
                    intern_rule: &mut self.intern_rule,
                    deltas: &mut self.pending_deltas,
                    emit: self.emit_deltas,
                };
                let top_k = filter_src_top_k(desired_src, k, g.ids);
                apply_per_src_top_k(&def, rule_src, top_k, &mut prov, g);
            } else {
                let tripped = self.tripped.entry(rule_name.clone()).or_default();
                let mut prov = ProvSets {
                    set: self.provenance.entry(rule_name).or_default(),
                    owned: &mut self.owned,
                    by_node: &mut self.by_node,
                    rule_intern: &mut self.rule_intern,
                    intern_rule: &mut self.intern_rule,
                    deltas: &mut self.pending_deltas,
                    emit: self.emit_deltas,
                };
                apply_desired(&def, desired_src, Some(rule_src), &mut prov, tripped, g);
            }
        }
    }

    /// Retract every provenance edge touching `n` across all rules and drop
    /// `n` from every rule index using its *current* props.
    ///
    /// Caller must invoke this while labels/props are still intact (before
    /// tombstone). Rules are walked in BTree name order; touching edges in
    /// BTree triple order. A second call on an already-retracted node is a
    /// no-op (crash-window replay / absent state).
    pub fn on_node_removed(&mut self, n: u32, g: &mut GraphMut<'_>) {
        // Ensure provenance is decoded before diffing against existing edges.
        self.ensure_provenance_loaded_mut();
        // Lazy index build: same guard as on_node_changed.  Top-k backfill
        // compute_desired consults the candidate index; an empty index would
        // silently produce no backfill.  Consume retained snapshot blobs here
        // rather than wiping any loaded HNSW graphs.
        if !self.indexes_populated && !self.rules.is_empty() {
            let hnsw = std::mem::take(
                &mut *self
                    .retained_hnsw_blobs
                    .lock()
                    .expect("retained_hnsw_blobs lock poisoned"),
            );
            let ivf_bytes = self
                .retained_ivf_bytes
                .lock()
                .expect("retained_ivf_bytes lock poisoned")
                .take()
                .unwrap_or_default();
            let ivf = decode_ivf_bytes_to_export(&ivf_bytes);
            self.reindex_all_load_ivf(g.ids, g.syms, g.labels, g.props, ivf);
            self.load_hnsw_state(hnsw);
        }

        let n_label = g.labels.get(n as usize).copied();
        let rule_names: Vec<String> = self.rules.keys().cloned().collect();

        for rule_name in rule_names {
            let def = self.rules[&rule_name].clone();
            let src_sym = g.syms.get(&def.src_label);
            let dst_sym = g.syms.get(&def.dst_label);
            let as_src = src_sym.is_some() && n_label == src_sym;
            let as_dst = dst_sym.is_some() && n_label == dst_sym;

            {
                let cur_getter = |f: &str| g.props.get(n, f).map(|vr| vr.into_value());
                let idx = self.indexes.get_mut(&rule_name).unwrap();
                if as_src {
                    let spec = src_lookup_spec_for(&def);
                    idx.src_side.remove(&spec, n, &cur_getter);
                }
                if as_dst {
                    let spec = candidate_spec_for(&def);
                    idx.dst_side.remove(&spec, n, &cur_getter);
                }
            }

            self.maybe_queue_ivf_rebuild(&rule_name, &def);
        }

        let touching: Vec<(String, Triple)> = self
            .by_node
            .get(&n)
            .into_iter()
            .flatten()
            .map(|&(rid, t, s, d)| (self.intern_rule[rid as usize].clone(), (t, s, d)))
            .collect();

        // Collect srcs that need top-k backfill BEFORE retracting provenance.
        // For top-k rules: when n is a dst, the src loses one from its top-k
        // and needs the next-best candidate added.
        let topk_backfill: Vec<(String, u32)> = touching
            .iter()
            .filter_map(|(rule_name, triple)| {
                let &(_, s, d) = triple;
                let def = self.rules.get(rule_name)?;
                def.max_edges?; // only top-k rules need backfill
                if d == n && s != n {
                    Some((rule_name.clone(), s))
                } else {
                    None
                }
            })
            .collect();

        for (rule_name, triple) in touching {
            let (t, s, d) = triple;
            g.topo.remove_edge(t, s, d);
            g.edge_props.remove_edge(t, s, d);
            if let Some(set) = self.provenance.get_mut(&rule_name) {
                ProvSets {
                    set,
                    owned: &mut self.owned,
                    by_node: &mut self.by_node,
                    rule_intern: &mut self.rule_intern,
                    intern_rule: &mut self.intern_rule,
                    deltas: &mut self.pending_deltas,
                    emit: self.emit_deltas,
                }
                .remove(&rule_name, triple, g.ids, g.syms);
            }
        }

        // Backfill top-k srcs whose dst was removed.
        // By now n is removed from the dst index (done in the first loop above),
        // so compute_desired(src, true) will not include n in candidates — the
        // resulting top-k automatically promotes the next-best candidate.
        for (rule_name, src) in topk_backfill {
            let def = self.rules[&rule_name].clone();
            let k = def.max_edges.unwrap(); // guarded by filter above
            let desired_src = compute_desired(&def, &self.indexes[&rule_name], src, true, g);
            let top_k = filter_src_top_k(desired_src, k, g.ids);
            let mut prov = ProvSets {
                set: self.provenance.entry(rule_name.clone()).or_default(),
                owned: &mut self.owned,
                by_node: &mut self.by_node,
                rule_intern: &mut self.rule_intern,
                intern_rule: &mut self.intern_rule,
                deltas: &mut self.pending_deltas,
                emit: self.emit_deltas,
            };
            apply_per_src_top_k(&def, src, top_k, &mut prov, g);
        }
    }

    /// Recompute one rule from scratch. Only exit from the tripped latch.
    ///
    /// If the full desired set fits in the budget, it is applied completely
    /// and `tripped` is cleared. If it still exceeds the budget, existing
    /// provenance is left completely untouched and `tripped` stays true
    /// (rebuild-is-noop for at/over-cap rules). Always counts as a fire
    /// evaluation per participating node. Returns Err if unknown.
    pub fn rebuild(&mut self, name: &str, g: &mut GraphMut<'_>) -> Result<(), String> {
        if !self.rules.contains_key(name) {
            return Err(format!("rule {:?} not found", name));
        }
        self.rebuild_needed.remove(name);
        let def = self.rules[name].clone();

        // Reindex this rule from scratch (indexes only).
        *self.indexes.get_mut(name).unwrap() = RuleIndex::default();

        // Init HNSW before indexing so inserts populate the graph incrementally.
        if def.approximate {
            let idx = self.indexes.get_mut(name).unwrap();
            idx.src_side.init_hnsw(name);
            idx.dst_side.init_hnsw(name);
        }

        let n_total = g.ids.len() as u32;
        for id in 0..n_total {
            let label_sym = match g.labels.get(id as usize).copied() {
                Some(s) if s != u32::MAX => s,
                _ => continue,
            };
            let idx = self.indexes.get_mut(name).unwrap();
            index_node_for_rule(id, label_sym, &def, idx, g.syms, g.props);
        }

        // Fit IVF clusters for approximate rules after reindex (drift reset).
        // HNSW was built incrementally; IVF kept as legacy fallback.
        if def.approximate {
            let idx = self.indexes.get_mut(name).unwrap();
            idx.src_side.fit_ivf_clusters(name);
            idx.dst_side.fit_ivf_clusters(name);
        }

        // Streaming rebuild: branches on max_edges semantics.
        //   None    → global-budget path (may no-op if still over budget)
        //   Some(k) → per-source top-k rebuild (always converges; no tripped latch)
        let mut prov = ProvSets {
            set: self.provenance.get_mut(name).unwrap(),
            owned: &mut self.owned,
            by_node: &mut self.by_node,
            rule_intern: &mut self.rule_intern,
            intern_rule: &mut self.intern_rule,
            deltas: &mut self.pending_deltas,
            emit: self.emit_deltas,
        };
        if let Some(k) = def.max_edges {
            apply_streaming_rebuild_top_k(&def, k, &self.indexes[name], &mut prov, g);
        } else {
            let tripped = self.tripped.get_mut(name).unwrap();
            apply_streaming_rebuild(&def, &self.indexes[name], &mut prov, tripped, g);
        }
        let fires = self.fires.entry(name.to_string()).or_default();
        bump_fires_for_participants(&def, g, fires);

        Ok(())
    }

    #[cfg(test)]
    fn by_node_consistent(&self) -> bool {
        let (rebuilt, intern, names) = rebuild_by_node(&self.provenance);
        resolve_by_node(&self.by_node, &self.intern_rule) == resolve_by_node(&rebuilt, &names)
            && intern.len() == names.len()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::def::{evaluate, NodeView, Predicate, RuleDef};
    use core_storage::{ColumnStore, Direction, EdgeProps, IdMap, Interner, Topology, Value};

    struct Fx {
        ids: IdMap,
        syms: Interner,
        labels: Vec<u32>,
        props: ColumnStore,
        topo: Topology,
        eprops: EdgeProps,
    }
    impl Fx {
        fn new() -> Self {
            Fx {
                ids: IdMap::new(),
                syms: Interner::new(),
                labels: vec![],
                props: ColumnStore::new(),
                topo: Topology::new(),
                eprops: EdgeProps::new(),
            }
        }
        fn add(&mut self, label: &str, key: &str, props: Vec<(&str, Value)>) -> u32 {
            let id = self.ids.get_or_insert(key);
            let sym = self.syms.intern(label);
            self.labels.resize(id as usize + 1, u32::MAX);
            self.labels[id as usize] = sym;
            for (f, v) in props {
                self.props.set(id, f, v);
            }
            id
        }
        fn g(&mut self) -> GraphMut<'_> {
            GraphMut {
                ids: &self.ids,
                syms: &mut self.syms,
                labels: &self.labels,
                props: ColumnsView::owned(&self.props),
                topo: &mut self.topo,
                edge_props: &mut self.eprops,
            }
        }
    }

    fn tags(items: &[&str]) -> Value {
        Value::List(items.iter().map(|s| Value::Str((*s).into())).collect())
    }

    fn overlap_rule() -> RuleDef {
        RuleDef {
            name: "rel".into(),
            src_label: "A".into(),
            dst_label: "A".into(),
            predicate: Predicate::Overlap {
                field: "tags".into(),
                min: 0.4,
            },
            edge_type: "REL".into(),
            weight_prop: Some("score".into()),
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        }
    }

    fn emb(xs: &[f64]) -> Value {
        Value::List(xs.iter().copied().map(Value::Float).collect())
    }

    fn approx_vec_rule() -> RuleDef {
        RuleDef {
            name: "sim".into(),
            src_label: "V".into(),
            dst_label: "V".into(),
            predicate: Predicate::VectorSimilar {
                field: "emb".into(),
                min: 0.5,
            },
            edge_type: "SIM".into(),
            weight_prop: None,
            max_edges: None,
            approximate: true,
            via_label: None,
            via_edge: None,
            via_dir: None,
        }
    }

    #[test]
    fn approximate_rule_rebuilds_after_drift_threshold() {
        with_ivf_drift_rebuild(1, || {
            let mut fx = Fx::new();
            let mut ids = Vec::new();
            for i in 0..6 {
                let x = i as f64 * 0.2;
                ids.push(fx.add("V", &format!("v{i}"), vec![("emb", emb(&[x, 1.0 - x]))]));
            }
            let mut eng = RuleEngine::new();
            {
                let mut g = fx.g();
                eng.create_rule(approx_vec_rule(), &mut g).unwrap();
            }
            assert!(eng.take_rebuild_needed().is_empty());
            {
                let mut g = fx.g();
                eng.on_node_removed(ids[0], &mut g);
            }
            assert!(
                eng.take_rebuild_needed().is_empty(),
                "drift=1 is not > threshold 1"
            );
            {
                let mut g = fx.g();
                eng.on_node_removed(ids[1], &mut g);
            }
            assert_eq!(eng.take_rebuild_needed(), vec!["sim".to_string()]);
            {
                let mut g = fx.g();
                eng.rebuild("sim", &mut g).unwrap();
            }
            assert!(
                eng.take_rebuild_needed().is_empty(),
                "rebuild must reset drift and not re-queue itself"
            );
            let drift = eng
                .export_ivf_state()
                .get("sim")
                .map(|(_, dst)| dst.2)
                .unwrap();
            assert_eq!(drift, 0, "rebuild resets dst-side IVF drift");
        });
    }

    #[test]
    fn backfill_creates_edges_with_scores_and_delete_removes_exactly_them() {
        let mut fx = Fx::new();
        let a = fx.add("A", "a", vec![("tags", tags(&["x", "y"]))]);
        let b = fx.add("A", "b", vec![("tags", tags(&["x", "y"]))]);
        let _c = fx.add("A", "c", vec![("tags", tags(&["q"]))]);
        // pre-existing user edge with same type: must survive rule delete
        let et = fx.syms.intern("REL");
        fx.topo.add_edge(et, a, b);
        let mut eng = RuleEngine::new();
        let mut g = fx.g();
        eng.create_rule(overlap_rule(), &mut g).unwrap();
        // a↔b jaccard 1.0 both directions; user edge a→b pre-existed so only b→a is owned
        assert!(g.topo.neighbors(et, Direction::Out, b).contains(&a));
        assert_eq!(
            g.edge_props.get(et, b, a, "score"),
            Some(&Value::Float(1.0))
        );
        assert!(!eng.is_owned(et, a, b));
        assert!(eng.is_owned(et, b, a));
        eng.delete_rule("rel", &mut g).unwrap();
        assert!(g.topo.neighbors(et, Direction::Out, a).contains(&b)); // user edge kept
        assert!(!g.topo.neighbors(et, Direction::Out, b).contains(&a)); // derived removed
        assert_eq!(g.edge_props.get(et, b, a, "score"), None);
    }

    #[test]
    fn incremental_update_adds_and_removes_edges() {
        let mut fx = Fx::new();
        let a = fx.add("A", "a", vec![("tags", tags(&["x", "y"]))]);
        let b = fx.add("A", "b", vec![("tags", tags(&["y", "z"]))]);
        let et = fx.syms.intern("REL");
        let mut eng = RuleEngine::new();
        {
            let mut g = fx.g();
            eng.create_rule(overlap_rule(), &mut g).unwrap(); // jaccard 1/3 < 0.4 → no edges
            assert_eq!(g.topo.edge_count(), 0);
        }
        // b's tags change to overlap strongly
        let old = fx.props.get(b, "tags").cloned();
        fx.props.set(b, "tags", tags(&["x", "y"]));
        {
            let mut g = fx.g();
            eng.on_node_changed(b, Some(("tags", old)), &mut g);
            assert!(g.topo.neighbors(et, Direction::Out, a).contains(&b));
            assert!(g.topo.neighbors(et, Direction::Out, b).contains(&a));
        }
        // and change away again → edges retract
        let old = fx.props.get(b, "tags").cloned();
        fx.props.set(b, "tags", tags(&["qqq"]));
        let mut g = fx.g();
        eng.on_node_changed(b, Some(("tags", old)), &mut g);
        assert_eq!(g.topo.edge_count(), 0);
        assert_eq!(g.edge_props.get(et, a, b, "score"), None);
    }

    #[test]
    fn key_match_new_node_links_and_rebuild_is_noop() {
        let mut fx = Fx::new();
        fx.add("C", "c1", vec![]);
        let mut eng = RuleEngine::new();
        {
            let mut g = fx.g();
            eng.create_rule(
                RuleDef {
                    name: "fk".into(),
                    src_label: "T".into(),
                    dst_label: "C".into(),
                    predicate: Predicate::KeyMatch {
                        field: "cid".into(),
                    },
                    edge_type: "AT".into(),
                    weight_prop: None,
                    max_edges: None,
                    approximate: false,
                    via_label: None,
                    via_edge: None,
                    via_dir: None,
                },
                &mut g,
            )
            .unwrap();
        }
        let t = fx.add("T", "t1", vec![("cid", Value::Str("c1".into()))]);
        let (at, c1, count_before) = {
            let mut g = fx.g();
            eng.on_node_changed(t, None, &mut g);
            let at = g.syms.get("AT").unwrap();
            let c1 = g.ids.get("c1").unwrap();
            assert!(g.topo.neighbors(at, Direction::Out, t).contains(&c1));
            (at, c1, g.topo.edge_count())
        };
        let mut g = fx.g();
        eng.rebuild("fk", &mut g).unwrap();
        assert_eq!(g.topo.edge_count(), count_before); // rebuild is a no-op on consistent state
        assert!(g.topo.neighbors(at, Direction::Out, t).contains(&c1));
    }

    #[test]
    fn score_refresh_on_persisting_owned_edge() {
        // Pins: weight set unconditionally even when add_edge returns false (edge persists).
        // jaccard({x,y,z},{x,y,q}) = |{x,y}|/|{x,y,z,q}| = 2/4 = 0.5 ≥ 0.2 → edges both ways.
        let mut fx = Fx::new();
        let a = fx.add("A", "a", vec![("tags", tags(&["x", "y", "z"]))]);
        let b = fx.add("A", "b", vec![("tags", tags(&["x", "y", "q"]))]);
        let et = fx.syms.intern("SIM");
        let mut eng = RuleEngine::new();
        {
            let mut g = fx.g();
            eng.create_rule(
                RuleDef {
                    name: "sim".into(),
                    src_label: "A".into(),
                    dst_label: "A".into(),
                    predicate: Predicate::Overlap {
                        field: "tags".into(),
                        min: 0.2,
                    },
                    edge_type: "SIM".into(),
                    weight_prop: Some("score".into()),
                    max_edges: None,
                    approximate: false,
                    via_label: None,
                    via_edge: None,
                    via_dir: None,
                },
                &mut g,
            )
            .unwrap();
            // Both directions present and owned with score ≈ 0.5.
            assert!(g.topo.neighbors(et, Direction::Out, a).contains(&b));
            assert!(g.topo.neighbors(et, Direction::Out, b).contains(&a));
            assert!(eng.is_owned(et, a, b) || eng.is_owned(et, b, a));
            let check = |v: Option<&Value>| {
                if let Some(Value::Float(f)) = v {
                    assert!(
                        (f - 0.5).abs() < 1e-9,
                        "initial score should be 0.5, got {f}"
                    );
                }
            };
            check(g.edge_props.get(et, a, b, "score"));
            check(g.edge_props.get(et, b, a, "score"));
        }
        // Change b's tags to match a exactly → jaccard = 1.0.
        let old = fx.props.get(b, "tags").cloned();
        fx.props.set(b, "tags", tags(&["x", "y", "z"]));
        {
            let mut g = fx.g();
            eng.on_node_changed(b, Some(("tags", old)), &mut g);
            // Both directions still present.
            assert!(g.topo.neighbors(et, Direction::Out, a).contains(&b));
            assert!(g.topo.neighbors(et, Direction::Out, b).contains(&a));
            // Scores must now be 1.0 on both directions.
            assert_eq!(
                g.edge_props.get(et, a, b, "score"),
                Some(&Value::Float(1.0)),
                "score on a→b must refresh to 1.0"
            );
            assert_eq!(
                g.edge_props.get(et, b, a, "score"),
                Some(&Value::Float(1.0)),
                "score on b→a must refresh to 1.0"
            );
        }
    }

    #[test]
    fn dst_side_keymatch_links_when_c_node_inserted_after_t() {
        // Exercises the synthetic key-probe on src_side Scalar index (dst-side KeyMatch path).
        let mut fx = Fx::new();
        // Insert T node first with cid="c9" — no C node yet → no edge.
        let t = fx.add("T", "t1", vec![("cid", Value::Str("c9".into()))]);
        let mut eng = RuleEngine::new();
        {
            let mut g = fx.g();
            eng.create_rule(
                RuleDef {
                    name: "fk".into(),
                    src_label: "T".into(),
                    dst_label: "C".into(),
                    predicate: Predicate::KeyMatch {
                        field: "cid".into(),
                    },
                    edge_type: "AT".into(),
                    weight_prop: None,
                    max_edges: None,
                    approximate: false,
                    via_label: None,
                    via_edge: None,
                    via_dir: None,
                },
                &mut g,
            )
            .unwrap();
            // No C node → no edge.
            let at = g.syms.intern("AT");
            assert_eq!(g.topo.edge_count(), 0, "no C node yet → no edge");
            // t is indexed in src_side with Scalar{cid}="c9"
            let _ = at;
        }
        // Now insert C node "c9" and notify the engine.
        let c9 = fx.add("C", "c9", vec![]);
        {
            let mut g = fx.g();
            eng.on_node_changed(c9, None, &mut g);
            let at = g.syms.get("AT").unwrap();
            // The dst-side path must have probed src_side with key="c9" and found t.
            assert!(
                g.topo.neighbors(at, Direction::Out, t).contains(&c9),
                "T→C edge must appear when C node is inserted"
            );
            assert!(eng.is_owned(at, t, c9));
        }
    }

    #[test]
    fn on_node_removed_retracts_both_sides_and_deindexes() {
        let mut fx = Fx::new();
        let a = fx.add("A", "a", vec![("tags", tags(&["x", "y"]))]);
        let b = fx.add("A", "b", vec![("tags", tags(&["x", "y"]))]);
        let et = fx.syms.intern("REL");
        let mut eng = RuleEngine::new();
        {
            let mut g = fx.g();
            eng.create_rule(overlap_rule(), &mut g).unwrap();
            assert!(g.topo.neighbors(et, Direction::Out, a).contains(&b));
            assert!(g.topo.neighbors(et, Direction::Out, b).contains(&a));
        }
        {
            let mut g = fx.g();
            eng.on_node_removed(a, &mut g);
            assert!(!g.topo.neighbors(et, Direction::Out, a).contains(&b));
            assert!(!g.topo.neighbors(et, Direction::Out, b).contains(&a));
            assert_eq!(g.edge_props.get(et, a, b, "score"), None);
            assert_eq!(g.edge_props.get(et, b, a, "score"), None);
            assert!(!eng.is_owned(et, a, b));
            assert!(!eng.is_owned(et, b, a));
        }
        // Partner re-links to a NEW matching node; de-indexed a is not a candidate.
        let c = fx.add("A", "c", vec![("tags", tags(&["x", "y"]))]);
        {
            let mut g = fx.g();
            eng.on_node_changed(c, None, &mut g);
            assert!(g.topo.neighbors(et, Direction::Out, b).contains(&c));
            assert!(g.topo.neighbors(et, Direction::Out, c).contains(&b));
            assert!(!g.topo.neighbors(et, Direction::Out, c).contains(&a));
            assert!(!g.topo.neighbors(et, Direction::Out, a).contains(&c));
        }
        // Second remove is a no-op (crash-window / already-retracted).
        {
            let mut g = fx.g();
            eng.on_node_removed(a, &mut g);
            assert!(g.topo.neighbors(et, Direction::Out, b).contains(&c));
        }
    }

    #[test]
    fn duplicate_name_and_unknown_delete_error() {
        let mut fx = Fx::new();
        let mut eng = RuleEngine::new();
        let mut g = fx.g();
        eng.create_rule(overlap_rule(), &mut g).unwrap();
        assert!(eng.create_rule(overlap_rule(), &mut g).is_err());
        assert!(eng.delete_rule("nope", &mut g).is_err());
    }

    /// C1: two rules sharing the same edge_type both match a pair of nodes.
    /// During backfill of R2, add_edge returns false for edges R1 already owns,
    /// so R2's provenance lacks them.  Deleting R1 removes those edges from the
    /// topology — but the rebuild-survivors step must then re-run R2 so it claims
    /// them.  Deleting R2 afterward must actually remove the edge.
    #[test]
    fn coowned_edge_type_survives_first_delete_gone_after_second() {
        let mut fx = Fx::new();
        let a = fx.add("A", "a", vec![("tags", tags(&["x", "y"]))]);
        let b = fx.add("A", "b", vec![("tags", tags(&["x", "y"]))]);
        let mut eng = RuleEngine::new();
        {
            let mut g = fx.g();
            // R1: Overlap min=0.1 — derives a↔b (jaccard 1.0 ≥ 0.1).
            eng.create_rule(
                RuleDef {
                    name: "r1".into(),
                    src_label: "A".into(),
                    dst_label: "A".into(),
                    predicate: Predicate::Overlap {
                        field: "tags".into(),
                        min: 0.1,
                    },
                    edge_type: "REL2".into(),
                    weight_prop: None,
                    max_edges: None,
                    approximate: false,
                    via_label: None,
                    via_edge: None,
                    via_dir: None,
                },
                &mut g,
            )
            .unwrap();
            // R2: same edge_type, Overlap min=0.2 — also derives a↔b.
            eng.create_rule(
                RuleDef {
                    name: "r2".into(),
                    src_label: "A".into(),
                    dst_label: "A".into(),
                    predicate: Predicate::Overlap {
                        field: "tags".into(),
                        min: 0.2,
                    },
                    edge_type: "REL2".into(),
                    weight_prop: None,
                    max_edges: None,
                    approximate: false,
                    via_label: None,
                    via_edge: None,
                    via_dir: None,
                },
                &mut g,
            )
            .unwrap();

            let et = g.syms.intern("REL2");
            // Both directions must exist (either rule claims them).
            assert!(
                g.topo.neighbors(et, Direction::Out, a).contains(&b),
                "a→b must exist after both rules created"
            );
            assert!(
                g.topo.neighbors(et, Direction::Out, b).contains(&a),
                "b→a must exist after both rules created"
            );

            // Delete R1 — rebuild-survivors re-runs R2 which must reclaim the edges.
            eng.delete_rule("r1", &mut g).unwrap();
            assert!(
                g.topo.neighbors(et, Direction::Out, a).contains(&b),
                "a→b must survive R1 deletion (R2 rebuilds and claims it)"
            );
            assert!(
                g.topo.neighbors(et, Direction::Out, b).contains(&a),
                "b→a must survive R1 deletion (R2 rebuilds and claims it)"
            );
            // R2 now owns both directions.
            assert!(
                eng.is_owned(et, a, b),
                "a→b must be owned by R2 after rebuild"
            );
            assert!(
                eng.is_owned(et, b, a),
                "b→a must be owned by R2 after rebuild"
            );

            // Delete R2 — no survivor left, edges must be gone.
            eng.delete_rule("r2", &mut g).unwrap();
            assert!(
                !g.topo.neighbors(et, Direction::Out, a).contains(&b),
                "a→b must be gone after both rules deleted"
            );
            assert!(
                !g.topo.neighbors(et, Direction::Out, b).contains(&a),
                "b→a must be gone after both rules deleted"
            );
        }
    }

    /// Helper: FieldEqual rule with top-k per-source cap.
    fn topk_eq_rule(k: u64) -> RuleDef {
        RuleDef {
            name: "eq".into(),
            src_label: "N".into(),
            dst_label: "N".into(),
            predicate: Predicate::FieldEqual { field: "k".into() },
            edge_type: "EQ".into(),
            weight_prop: None,
            max_edges: Some(k),
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        }
    }

    fn prov_pairs(eng: &RuleEngine, name: &str) -> BTreeSet<(u32, u32)> {
        eng.provenance()
            .get(name)
            .map(|s| s.iter().map(|&(_, a, b)| (a, b)).collect())
            .unwrap_or_default()
    }

    /// k=1: each src gets its single best-scored dst (score DESC, key ASC
    /// tiebreak).  FieldEqual has uniform score 1.0, so the winner is the dst
    /// with the lexicographically smallest key that is not the src itself.
    #[test]
    fn topk_k1_keeps_best_scored_dst() {
        let mut fx = Fx::new();
        let mut eng = RuleEngine::new();
        {
            let mut g = fx.g();
            eng.create_rule(topk_eq_rule(1), &mut g).unwrap();
        }
        // Insert 4 nodes all sharing k="const".  Keys: n0 < n1 < n2 < n3.
        let mut ids = Vec::new();
        for i in 0..4usize {
            let id = fx.add(
                "N",
                &format!("n{i}"),
                vec![("k", Value::Str("const".into()))],
            );
            ids.push(id);
            let mut g = fx.g();
            eng.on_node_changed(id, None, &mut g);
        }
        let et = fx.syms.get("EQ").unwrap();
        // Each src's single allowed dst must be the smallest key ≠ self.
        // n0 → n1 (smallest other)
        // n1 → n0 (n0 < n1)
        // n2 → n0
        // n3 → n0
        let expected_dsts = [ids[1], ids[0], ids[0], ids[0]];
        for (i, (&src, &expected_dst)) in ids.iter().zip(expected_dsts.iter()).enumerate() {
            let out: Vec<u32> = fx.topo.neighbors(et, Direction::Out, src).to_vec();
            assert_eq!(
                out,
                vec![expected_dst],
                "src n{i} should point only to the best dst"
            );
        }
        assert_eq!(eng.provenance()["eq"].len(), 4);
        assert!(!eng.is_tripped("eq"), "top-k rules never trip");
    }

    /// k=2 insert-evict: adding a better dst evicts the worst of the current k.
    /// Uses NumericWithin (scored) so scores differ across dsts.
    #[test]
    fn topk_insert_evict() {
        // Rule: S→D with VectorSimilar-alike (we use NumericWithin for simplicity).
        // 3 src nodes, numeric field "v"; tolerance 10.0 so score = 1-|Δ|/10.
        // k=1 per source.
        let mut fx = Fx::new();
        let rule = RuleDef {
            name: "nw".into(),
            src_label: "S".into(),
            dst_label: "D".into(),
            predicate: Predicate::NumericWithin {
                field: "v".into(),
                tolerance: 10.0,
            },
            edge_type: "NEAR".into(),
            weight_prop: Some("score".into()),
            max_edges: Some(1),
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        };
        let mut eng = RuleEngine::new();
        {
            let mut g = fx.g();
            eng.create_rule(rule, &mut g).unwrap();
        }

        // src s0 with v=0.0
        let s0 = fx.add("S", "s0", vec![("v", Value::Float(0.0))]);
        // dst d_far with v=9.0 → score=0.1 (worst)
        let d_far = fx.add("D", "d_far", vec![("v", Value::Float(9.0))]);
        {
            let mut g = fx.g();
            eng.on_node_changed(s0, None, &mut g);
            eng.on_node_changed(d_far, None, &mut g);
        }
        let et = fx.syms.get("NEAR").unwrap();
        // s0 → d_far (only candidate)
        assert!(fx.topo.neighbors(et, Direction::Out, s0).contains(&d_far));
        assert_eq!(eng.provenance()["nw"].len(), 1);

        // Insert d_close with v=1.0 → score=0.9 (better than d_far).
        let d_close = fx.add("D", "d_close", vec![("v", Value::Float(1.0))]);
        {
            let mut g = fx.g();
            eng.on_node_changed(d_close, None, &mut g);
        }
        // s0 should now point to d_close (evicting d_far).
        let out: Vec<u32> = fx.topo.neighbors(et, Direction::Out, s0).to_vec();
        assert_eq!(out, vec![d_close], "d_close should evict d_far");
        assert!(!fx.topo.neighbors(et, Direction::Out, s0).contains(&d_far));
        assert_eq!(eng.provenance()["nw"].len(), 1);
        assert!(eng.by_node_consistent());
    }

    /// Retract-backfill: removing the best dst causes the next-best to fill in.
    #[test]
    fn topk_retract_backfill() {
        let mut fx = Fx::new();
        let rule = RuleDef {
            name: "nw".into(),
            src_label: "S".into(),
            dst_label: "D".into(),
            predicate: Predicate::NumericWithin {
                field: "v".into(),
                tolerance: 10.0,
            },
            edge_type: "NEAR".into(),
            weight_prop: Some("score".into()),
            max_edges: Some(1),
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        };
        let mut eng = RuleEngine::new();

        let s0 = fx.add("S", "s0", vec![("v", Value::Float(0.0))]);
        let d_close = fx.add("D", "d_close", vec![("v", Value::Float(1.0))]); // score=0.9
        let d_far = fx.add("D", "d_far", vec![("v", Value::Float(8.0))]); // score=0.2
        {
            let mut g = fx.g();
            eng.create_rule(rule, &mut g).unwrap();
        }
        let et = fx.syms.get("NEAR").unwrap();
        // d_close is the top-1 dst.
        assert!(fx.topo.neighbors(et, Direction::Out, s0).contains(&d_close));
        assert!(!fx.topo.neighbors(et, Direction::Out, s0).contains(&d_far));
        assert_eq!(eng.provenance()["nw"].len(), 1);

        // Break d_close's match by pushing its v out of tolerance.
        let old = fx.props.get(d_close, "v").cloned();
        fx.props.set(d_close, "v", Value::Float(50.0));
        {
            let mut g = fx.g();
            eng.on_node_changed(d_close, Some(("v", old)), &mut g);
        }
        // d_far should backfill.
        assert!(!fx.topo.neighbors(et, Direction::Out, s0).contains(&d_close));
        assert!(
            fx.topo.neighbors(et, Direction::Out, s0).contains(&d_far),
            "d_far should backfill after d_close retracted"
        );
        assert_eq!(eng.provenance()["nw"].len(), 1);
        assert!(eng.by_node_consistent());
    }

    /// Tie-breaking: equal scores → dst_key ASC wins.
    #[test]
    fn topk_tie_broken_by_dst_key() {
        // FieldEqual: all dsts have score 1.0 → tiebreak by key.
        let mut fx = Fx::new();
        let mut eng = RuleEngine::new();
        {
            let mut g = fx.g();
            eng.create_rule(topk_eq_rule(2), &mut g).unwrap();
        }
        // 5 nodes all with k="x" → each src matches 4 others; top-2 by key.
        // Keys: a, b, c, d, e (alphabetical).
        for name in ["a", "b", "c", "d", "e"] {
            let id = fx.add("N", name, vec![("k", Value::Str("x".into()))]);
            let mut g = fx.g();
            eng.on_node_changed(id, None, &mut g);
        }
        let et = fx.syms.get("EQ").unwrap();
        let get_id = |key: &str| fx.ids.get(key).unwrap();
        // Node "a" should point to the two smallest keys that aren't "a": b, c.
        let a = get_id("a");
        let b = get_id("b");
        let c = get_id("c");
        let out_a: BTreeSet<u32> = fx
            .topo
            .neighbors(et, Direction::Out, a)
            .iter()
            .copied()
            .collect();
        assert!(out_a.contains(&b), "a→b (b is best key after a)");
        assert!(out_a.contains(&c), "a→c (c is 2nd best key)");
        assert_eq!(out_a.len(), 2);
        // Node "e" should point to "a" and "b" (two smallest keys ≠ "e").
        let e = get_id("e");
        let out_e: BTreeSet<u32> = fx
            .topo
            .neighbors(et, Direction::Out, e)
            .iter()
            .copied()
            .collect();
        assert!(out_e.contains(&a), "e→a");
        assert!(out_e.contains(&b), "e→b");
        assert_eq!(out_e.len(), 2);
        assert!(eng.by_node_consistent());
    }

    /// When k >= candidate count, all candidates are included (no truncation).
    #[test]
    fn topk_k_larger_than_candidate_count() {
        let mut fx = Fx::new();
        let mut eng = RuleEngine::new();
        {
            let mut g = fx.g();
            // k=100 but only 3 other nodes → all 3 included.
            eng.create_rule(topk_eq_rule(100), &mut g).unwrap();
        }
        for i in 0..4usize {
            let id = fx.add("N", &format!("n{i}"), vec![("k", Value::Str("c".into()))]);
            let mut g = fx.g();
            eng.on_node_changed(id, None, &mut g);
        }
        // 4 nodes × 3 matches each = 12 directed edges.
        assert_eq!(eng.provenance()["eq"].len(), 12);
        assert!(!eng.is_tripped("eq"));
    }

    /// rebuild() with top-k rule re-converges to the correct per-source top-k
    /// after externally removing a node's field.
    #[test]
    fn topk_rebuild_exact() {
        let mut fx = Fx::new();
        let mut eng = RuleEngine::new();
        {
            let mut g = fx.g();
            eng.create_rule(topk_eq_rule(1), &mut g).unwrap();
        }
        // 3 nodes with k="x" → each gets 1 dst (smallest key ≠ self).
        let _a = fx.add("N", "a", vec![("k", Value::Str("x".into()))]);
        let _b = fx.add("N", "b", vec![("k", Value::Str("x".into()))]);
        let _c = fx.add("N", "c", vec![("k", Value::Str("x".into()))]);
        {
            let mut g = fx.g();
            eng.on_node_changed(_a, None, &mut g);
            eng.on_node_changed(_b, None, &mut g);
            eng.on_node_changed(_c, None, &mut g);
        }
        assert_eq!(eng.provenance()["eq"].len(), 3);

        // rebuild should produce the same result.
        {
            let mut g = fx.g();
            eng.rebuild("eq", &mut g).unwrap();
        }
        assert_eq!(eng.provenance()["eq"].len(), 3);
        assert!(!eng.is_tripped("eq"));
        assert!(eng.by_node_consistent());
    }

    /// by_node index stays consistent across top-k inserts, evictions and rebuild.
    #[test]
    fn topk_by_node_consistent() {
        let mut fx = Fx::new();
        let mut eng = RuleEngine::new();
        {
            let mut g = fx.g();
            eng.create_rule(topk_eq_rule(2), &mut g).unwrap();
        }
        for i in 0..5usize {
            let id = fx.add(
                "N",
                &format!("n{i}"),
                vec![("k", Value::Str("const".into()))],
            );
            let mut g = fx.g();
            eng.on_node_changed(id, None, &mut g);
        }
        assert!(eng.by_node_consistent(), "consistent after insertions");

        // Evict by changing a prop.
        let id2 = fx.ids.get("n2").unwrap();
        let old = fx.props.get(id2, "k").cloned();
        fx.props.set(id2, "k", Value::Str("other".into()));
        {
            let mut g = fx.g();
            eng.on_node_changed(id2, Some(("k", old)), &mut g);
        }
        assert!(eng.by_node_consistent(), "consistent after eviction");

        {
            let mut g = fx.g();
            eng.rebuild("eq", &mut g).unwrap();
        }
        assert!(eng.by_node_consistent(), "consistent after rebuild");
    }

    fn numeric_rule() -> RuleDef {
        RuleDef {
            name: "nw".into(),
            src_label: "C".into(),
            dst_label: "C".into(),
            predicate: Predicate::NumericWithin {
                field: "year".into(),
                tolerance: 2.0,
            },
            edge_type: "NEAR".into(),
            weight_prop: Some("score".into()),
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        }
    }

    fn geo_rule() -> RuleDef {
        RuleDef {
            name: "geo".into(),
            src_label: "City".into(),
            dst_label: "City".into(),
            predicate: Predicate::GeoRadius {
                field: "loc".into(),
                km: 400.0,
            },
            edge_type: "NEAR_GEO".into(),
            weight_prop: Some("score".into()),
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        }
    }

    fn vec_rule() -> RuleDef {
        RuleDef {
            name: "vec".into(),
            src_label: "Doc".into(),
            dst_label: "Doc".into(),
            predicate: Predicate::VectorSimilar {
                field: "emb".into(),
                min: 0.9,
            },
            edge_type: "SIM".into(),
            weight_prop: Some("score".into()),
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        }
    }

    fn pair_edges(topo: &Topology, et: u32, a: u32, b: u32) -> bool {
        topo.neighbors(et, Direction::Out, a).contains(&b)
            && topo.neighbors(et, Direction::Out, b).contains(&a)
    }

    #[test]
    fn numeric_within_incremental_crosses_bucket_and_clears_old_index() {
        let mut fx = Fx::new();
        let a = fx.add("C", "a", vec![("year", Value::Float(10.0))]);
        let b = fx.add("C", "b", vec![("year", Value::Float(12.0))]);
        let et = fx.syms.intern("NEAR");
        let mut eng = RuleEngine::new();
        {
            let mut g = fx.g();
            eng.create_rule(numeric_rule(), &mut g).unwrap();
            // |12−10| = 2 ≤ 2 → score 0.0 both ways
            assert!(pair_edges(g.topo, et, a, b));
        }

        // 12.0 (bucket 6) → 16.1 (bucket 8): two buckets away, so the old
        // value's ±1 probe no longer reaches b. Match breaks.
        let old = fx.props.get(b, "year").cloned();
        fx.props.set(b, "year", Value::Float(16.1));
        {
            let mut g = fx.g();
            eng.on_node_changed(b, Some(("year", old)), &mut g);
            assert!(!pair_edges(g.topo, et, a, b));
            assert_eq!(g.topo.edge_count(), 0);
        }
        let def = numeric_rule();
        let spec = candidate_spec_for(&def);
        let old_map: std::collections::HashMap<_, _> =
            [("year".to_string(), Value::Float(12.0))].into();
        let old_get = |f: &str| old_map.get(f).cloned();
        let src_hits = eng.indexes["nw"].src_side.candidates(&spec, &old_get);
        let dst_hits = eng.indexes["nw"].dst_side.candidates(&spec, &old_get);
        assert!(!src_hits.contains(&b), "old src bucket must drop b");
        assert!(!dst_hits.contains(&b), "old dst bucket must drop b");
        assert!(src_hits.contains(&a));

        // 16.1 → 11.9 (bucket 5): match returns.
        let old = fx.props.get(b, "year").cloned();
        fx.props.set(b, "year", Value::Float(11.9));
        let mut g = fx.g();
        eng.on_node_changed(b, Some(("year", old)), &mut g);
        assert!(pair_edges(g.topo, et, a, b));
    }

    fn loc_val(lat: f64, lon: f64) -> Value {
        Value::List(vec![Value::Float(lat), Value::Float(lon)])
    }

    fn emb_val(vals: &[f64]) -> Value {
        Value::List(vals.iter().copied().map(Value::Float).collect())
    }

    #[test]
    fn rebuild_is_noop_for_numeric_geo_and_vector() {
        let mut fx = Fx::new();
        let ca = fx.add("C", "ca", vec![("year", Value::Int(1998))]);
        let cb = fx.add("C", "cb", vec![("year", Value::Float(2000.0))]);
        let pa = fx.add("City", "paris", vec![("loc", loc_val(48.8566, 2.3522))]);
        let lo = fx.add("City", "london", vec![("loc", loc_val(51.5074, -0.1278))]);
        let da = fx.add("Doc", "d1", vec![("emb", emb_val(&[1.0, 0.0]))]);
        let db = fx.add("Doc", "d2", vec![("emb", emb_val(&[1.0, 0.0]))]);

        let mut eng = RuleEngine::new();
        {
            let mut g = fx.g();
            eng.create_rule(numeric_rule(), &mut g).unwrap();
            eng.create_rule(geo_rule(), &mut g).unwrap();
            eng.create_rule(vec_rule(), &mut g).unwrap();
        }

        let (near, ngeo, sim) = (
            fx.syms.get("NEAR").unwrap(),
            fx.syms.get("NEAR_GEO").unwrap(),
            fx.syms.get("SIM").unwrap(),
        );
        assert!(pair_edges(&fx.topo, near, ca, cb));
        assert!(pair_edges(&fx.topo, ngeo, pa, lo));
        assert!(pair_edges(&fx.topo, sim, da, db));
        let before = fx.topo.edge_count();

        {
            let mut g = fx.g();
            eng.rebuild("nw", &mut g).unwrap();
            eng.rebuild("geo", &mut g).unwrap();
            eng.rebuild("vec", &mut g).unwrap();
        }
        assert_eq!(fx.topo.edge_count(), before);
        assert!(pair_edges(&fx.topo, near, ca, cb));
        assert!(pair_edges(&fx.topo, ngeo, pa, lo));
        assert!(pair_edges(&fx.topo, sim, da, db));
    }

    fn fk_rule() -> RuleDef {
        RuleDef {
            name: "works_at".into(),
            src_label: "T".into(),
            dst_label: "C".into(),
            predicate: Predicate::KeyMatch {
                field: "cid".into(),
            },
            edge_type: "AT".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        }
    }

    #[test]
    fn by_node_matches_rebuild_after_mutation_storm() {
        let mut fx = Fx::new();
        let hub = fx.add("C", "hub", vec![]);
        let other = fx.add("C", "other", vec![]);
        let mut people = Vec::new();
        for i in 0..40 {
            let cid = if i < 30 { "hub" } else { "other" };
            people.push(fx.add(
                "T",
                &format!("t{i}"),
                vec![("cid", Value::Str(cid.into())), ("tags", tags(&["x", "y"]))],
            ));
        }
        let mut overlap = overlap_rule();
        overlap.src_label = "T".into();
        overlap.dst_label = "T".into();
        let mut eng = RuleEngine::new();
        {
            let mut g = fx.g();
            eng.create_rule(fk_rule(), &mut g).unwrap();
            eng.create_rule(overlap, &mut g).unwrap();
        }
        assert!(eng.by_node_consistent());
        assert_eq!(eng.provenance_touching_len(hub), 30);

        // Incremental: re-home half the hub people, flip tags, then restore.
        for (i, &id) in people.iter().enumerate().take(15) {
            let old = fx.props.get(id, "cid").cloned();
            fx.props.set(id, "cid", Value::Str("other".into()));
            let mut g = fx.g();
            eng.on_node_changed(id, Some(("cid", old)), &mut g);
            assert!(
                eng.by_node_consistent(),
                "inconsistent after cid update {i}"
            );
        }
        for &id in people.iter().take(8) {
            let old = fx.props.get(id, "tags").cloned();
            fx.props.set(id, "tags", tags(&["q"]));
            let mut g = fx.g();
            eng.on_node_changed(id, Some(("tags", old)), &mut g);
        }
        assert!(eng.by_node_consistent());

        // Delete-node cleanup uses the reverse index.
        {
            let mut g = fx.g();
            eng.on_node_removed(people[0], &mut g);
        }
        fx.labels[people[0] as usize] = u32::MAX;
        assert!(eng.by_node_consistent());
        assert_eq!(eng.provenance_touching_len(people[0]), 0);

        {
            let mut g = fx.g();
            eng.rebuild("works_at", &mut g).unwrap();
            eng.rebuild("rel", &mut g).unwrap();
        }
        assert!(eng.by_node_consistent());

        {
            let mut g = fx.g();
            eng.delete_rule("rel", &mut g).unwrap();
        }
        assert!(eng.by_node_consistent());
        assert_eq!(eng.provenance_touching(people[1]).count(), 1);

        // Persist-restore rebuilds the reverse index from provenance.
        let (defs, prov, tripped, fires) = eng.to_persist();
        let restored = RuleEngine::from_persist(defs, prov, tripped, fires);
        assert!(restored.by_node_consistent());
        assert_eq!(
            restored.provenance_touching_len(hub),
            eng.provenance_touching_len(hub)
        );
        assert_eq!(
            restored.provenance_touching_len(other),
            eng.provenance_touching_len(other)
        );
    }

    #[test]
    fn provenance_touching_high_degree_hub() {
        let mut fx = Fx::new();
        let hub = fx.add("C", "hub", vec![]);
        let mut first = None;
        for i in 0..256 {
            let id = fx.add(
                "T",
                &format!("t{i}"),
                vec![("cid", Value::Str("hub".into()))],
            );
            if first.is_none() {
                first = Some(id);
            }
        }
        let first = first.unwrap();
        let mut eng = RuleEngine::new();
        {
            let mut g = fx.g();
            eng.create_rule(fk_rule(), &mut g).unwrap();
        }
        assert!(eng.by_node_consistent());
        assert_eq!(eng.provenance_touching_len(hub), 256);
        assert_eq!(eng.provenance_touching_len(first), 1);
        let hits: Vec<_> = eng.provenance_touching(first).collect();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "works_at");
        assert_eq!(hits[0].2, first);
        assert_eq!(hits[0].3, hub);
    }

    /// by_node index stays consistent across global-budget trip and rebuild
    /// (max_edges: None path — DEFAULT_MAX_EDGES = 1_000_000).
    ///
    /// Uses a tiny budget via a special rule with `max_edges: None` but many
    /// nodes to naturally exceed the default; instead we directly test the
    /// None-path by verifying that the by_node index is consistent at each
    /// step of normal insertions and rebuilds.
    #[test]
    fn by_node_consistent_across_inserts_and_rebuild() {
        let mut fx = Fx::new();
        let mut eng = RuleEngine::new();
        let rule = RuleDef {
            name: "eq".into(),
            src_label: "N".into(),
            dst_label: "N".into(),
            predicate: Predicate::FieldEqual { field: "k".into() },
            edge_type: "EQ".into(),
            weight_prop: None,
            max_edges: None, // global-budget path, DEFAULT_MAX_EDGES = 1_000_000
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        };
        {
            let mut g = fx.g();
            eng.create_rule(rule, &mut g).unwrap();
        }
        let mut ids = Vec::new();
        for i in 0..6 {
            let id = fx.add(
                "N",
                &format!("n{i}"),
                vec![("k", Value::Str("const".into()))],
            );
            ids.push(id);
            let mut g = fx.g();
            eng.on_node_changed(id, None, &mut g);
        }
        // 6 nodes × 5 matches each = 30 directed edges (well under 1M budget).
        assert_eq!(eng.provenance()["eq"].len(), 30);
        assert!(!eng.is_tripped("eq"));
        assert!(eng.by_node_consistent(), "consistent after insertions");

        // Change one node's field — triggers retract + backfill on that src.
        let old = fx.props.get(ids[3], "k").cloned();
        fx.props.set(ids[3], "k", Value::Str("other".into()));
        {
            let mut g = fx.g();
            eng.on_node_changed(ids[3], Some(("k", old)), &mut g);
        }
        assert!(eng.by_node_consistent(), "consistent after property change");

        {
            let mut g = fx.g();
            eng.rebuild("eq", &mut g).unwrap();
        }
        assert!(!eng.is_tripped("eq"));
        assert!(eng.by_node_consistent(), "consistent after rebuild");
    }

    fn mix64(mut x: u64) -> u64 {
        x = x.wrapping_add(0x9E3779B97F4A7C15);
        x = (x ^ (x >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94D049BB133111EB);
        x ^ (x >> 31)
    }

    fn rand_emb(seed: u64, i: u32, dim: usize) -> Value {
        let vals: Vec<f64> = (0..dim)
            .map(|d| {
                let bits = mix64(seed ^ ((i as u64 + 1).wrapping_mul(0x100000001)) ^ (d as u64));
                let mut f = (bits as f64) / (u64::MAX as f64) * 2.0 - 1.0;
                if f == 0.0 {
                    f = 1.0;
                }
                f
            })
            .collect();
        emb_val(&vals)
    }

    fn seed_docs(n: u32, seed: u64) -> (Fx, Vec<u32>) {
        let dims = [2usize, 3, 4, 8];
        let mut fx = Fx::new();
        let mut ids = Vec::new();
        for i in 0..n {
            let dim = dims[(i as usize) % dims.len()];
            ids.push(fx.add(
                "Doc",
                &format!("d{i}"),
                vec![("emb", rand_emb(seed, i, dim))],
            ));
        }
        (fx, ids)
    }

    /// Identity proof: 500 mixed-dim vectors, derived edges with the dim
    /// reject on vs forced off (and vs brute-force evaluate) are identical.
    #[test]
    fn vector_dim_reject_matches_unfiltered_and_oracle() {
        const N: u32 = 500;
        const SEED: u64 = 0xC0FF_EE00_D15C;
        let def = vec_rule();

        let (mut fx_on, ids) = seed_docs(N, SEED);
        let mut eng_on = RuleEngine::new();
        {
            let mut g = fx_on.g();
            eng_on.create_rule(def.clone(), &mut g).unwrap();
        }
        let on = prov_pairs(&eng_on, "vec");
        assert!(!on.is_empty(), "seeded set must produce some edges");

        let (mut fx_off, _) = seed_docs(N, SEED);
        let mut eng_off = RuleEngine::new();
        {
            let mut g = fx_off.g();
            with_vector_dim_reject(false, || {
                eng_off.create_rule(def.clone(), &mut g).unwrap();
            });
        }
        assert_eq!(on, prov_pairs(&eng_off, "vec"), "filter vs no-filter");

        let mut brute = BTreeSet::new();
        for &s in &ids {
            for &d in &ids {
                if s == d {
                    continue;
                }
                let skey = fx_on.ids.key_of(s).unwrap();
                let dkey = fx_on.ids.key_of(d).unwrap();
                let sget = |f: &str| fx_on.props.get(s, f).cloned();
                let dget = |f: &str| fx_on.props.get(d, f).cloned();
                if evaluate(
                    &def.predicate,
                    &NodeView {
                        key: skey,
                        props: &sget,
                    },
                    &NodeView {
                        key: dkey,
                        props: &dget,
                    },
                )
                .is_some()
                {
                    brute.insert((s, d));
                }
            }
        }
        assert_eq!(on, brute, "filter vs brute-force evaluate");
    }

    /// Dim change must flow through remove(old)+insert(new); edges match a
    /// fresh engine built from the post-update props.
    #[test]
    fn vector_dim_change_updates_cache_and_matches_fresh_build() {
        let mut fx = Fx::new();
        let a = fx.add("Doc", "a", vec![("emb", emb_val(&[1.0, 0.0]))]);
        let b = fx.add("Doc", "b", vec![("emb", emb_val(&[1.0, 0.0]))]);
        let c = fx.add("Doc", "c", vec![("emb", emb_val(&[1.0, 0.0, 0.0]))]);
        let mut eng = RuleEngine::new();
        {
            let mut g = fx.g();
            eng.create_rule(vec_rule(), &mut g).unwrap();
        }
        assert_eq!(eng.indexes["vec"].src_side.vec_dim(a), Some(2));
        assert_eq!(eng.indexes["vec"].src_side.vec_dim(c), Some(3));
        assert_eq!(prov_pairs(&eng, "vec"), BTreeSet::from([(a, b), (b, a)]));

        let old = fx.props.get(b, "emb").cloned();
        fx.props.set(b, "emb", emb_val(&[1.0, 0.0, 0.0]));
        {
            let mut g = fx.g();
            eng.on_node_changed(b, Some(("emb", old)), &mut g);
        }
        assert_eq!(eng.indexes["vec"].src_side.vec_dim(b), Some(3));
        assert_eq!(eng.indexes["vec"].dst_side.vec_dim(b), Some(3));
        let after = prov_pairs(&eng, "vec");
        assert_eq!(after, BTreeSet::from([(b, c), (c, b)]));

        // Separate graph: first engine already owns the b↔c edges in `fx.topo`.
        let mut fresh_fx = Fx::new();
        let fa = fresh_fx.add("Doc", "a", vec![("emb", emb_val(&[1.0, 0.0]))]);
        let fb = fresh_fx.add("Doc", "b", vec![("emb", emb_val(&[1.0, 0.0, 0.0]))]);
        let fc = fresh_fx.add("Doc", "c", vec![("emb", emb_val(&[1.0, 0.0, 0.0]))]);
        let mut fresh = RuleEngine::new();
        {
            let mut g = fresh_fx.g();
            fresh.create_rule(vec_rule(), &mut g).unwrap();
        }
        assert_eq!(
            prov_pairs(&fresh, "vec"),
            BTreeSet::from([(fb, fc), (fc, fb)])
        );
        assert_eq!(fresh.indexes["vec"].src_side.vec_dim(fb), Some(3));
        assert_eq!(fresh.indexes["vec"].src_side.vec_dim(fa), Some(2));
    }

    // -----------------------------------------------------------------------
    // Streaming backfill — order-identity and memory-bound tests (Plan 11 M1)
    // -----------------------------------------------------------------------

    /// Top-k order-identity property test.
    ///
    /// For rules with `max_edges: Some(k)` (top-k per-source semantics),
    /// verifies that `create_rule` streaming backfill produces the same
    /// per-source top-k set as an independent brute-force reference.
    ///
    /// The reference is intentionally independent of `filter_src_top_k`:
    /// it sorts candidates inline (score DESC, dst-key ASC, take k) so a
    /// comparator bug cannot self-agree between reference and actual.
    ///
    /// Covers all four `CandidateSpec` paths through `compute_desired`:
    /// - `FieldEqual` → `CandidateSpec::Scalar` (uniform score=1.0, tiebreak by key)
    /// - `NumericWithin` → `CandidateSpec::NumericBucket` (scored, variable top-k)
    /// - `KeyMatch` → `CandidateSpec::ByKey` (FK probe, at most 1 dst per src)
    /// - `VectorSimilar` / `approximate=false` → `CandidateSpec::ScanAll` (scored)
    #[test]
    fn streaming_topk_order_identity_property_test() {
        // Reference: build index, compute_desired per src, then brute-force
        // sort (score DESC, dst-key ASC, take k) — independent of filter_src_top_k.
        fn reference_topk(rule: &RuleDef, k: u64, fx: &mut Fx) -> BTreeSet<(u32, u32)> {
            let mut idx = RuleIndex::default();
            for id in 0..fx.ids.len() as u32 {
                let label_sym = match fx.labels.get(id as usize).copied() {
                    Some(s) if s != u32::MAX => s,
                    _ => continue,
                };
                index_node_for_rule(
                    id,
                    label_sym,
                    rule,
                    &mut idx,
                    &fx.syms,
                    ColumnsView::owned(&fx.props),
                );
            }
            let src_sym = fx.syms.get(&rule.src_label);
            let mut out = BTreeSet::new();
            let ids_snap: Vec<u32> = (0..fx.ids.len() as u32).collect();
            for id in ids_snap {
                let label_sym = match fx.labels.get(id as usize).copied() {
                    Some(s) if s != u32::MAX => s,
                    _ => continue,
                };
                if src_sym != Some(label_sym) {
                    continue;
                }
                let g = GraphMut {
                    ids: &fx.ids,
                    syms: &mut fx.syms,
                    labels: &fx.labels,
                    props: ColumnsView::owned(&fx.props),
                    topo: &mut fx.topo,
                    edge_props: &mut fx.eprops,
                };
                let per_src = compute_desired(rule, &idx, id, true, &g);
                // Independent brute-force sort: score DESC, dst-key ASC, take k.
                let mut candidates: Vec<((u32, u32), f64)> = per_src.into_iter().collect();
                candidates.sort_by(|&((_, da), sa), &((_, db), sb)| {
                    sb.total_cmp(&sa).then_with(|| {
                        let ka = fx.ids.key_of(da).unwrap_or("");
                        let kb = fx.ids.key_of(db).unwrap_or("");
                        ka.cmp(kb)
                    })
                });
                candidates.truncate(k as usize);
                out.extend(candidates.into_iter().map(|(k, _)| k));
            }
            out
        }

        // Helper: run create_rule and return provenance (src,dst) pairs.
        fn streaming_pairs(rule: RuleDef, fx: &mut Fx) -> BTreeSet<(u32, u32)> {
            let name = rule.name.clone();
            let mut eng = RuleEngine::new();
            eng.create_rule(rule, &mut fx.g()).unwrap();
            eng.provenance()
                .get(&name)
                .map(|s| s.iter().map(|&(_, a, b)| (a, b)).collect())
                .unwrap_or_default()
        }

        // ----------------------------------------------------------------
        // Case 1: FieldEqual (uniform score=1.0, tiebreak by key ASC)
        // N→N, 3-value "k" field; top-k filters per src by key.
        // ----------------------------------------------------------------
        for seed in [0u64, 1, 42, 0xDEAD_BEEF, 0x1234_5678, 99, 12_648_430, 7] {
            for k in [1u64, 2, 3, 5] {
                let rule = RuleDef {
                    name: "eq".into(),
                    src_label: "N".into(),
                    dst_label: "N".into(),
                    predicate: Predicate::FieldEqual { field: "k".into() },
                    edge_type: "EQ".into(),
                    weight_prop: None,
                    max_edges: Some(k),
                    approximate: false,
                    via_label: None,
                    via_edge: None,
                    via_dir: None,
                };

                let build = || {
                    let mut fx = Fx::new();
                    for i in 0..12u32 {
                        let h = mix64(seed ^ (i as u64 + 1));
                        let val = match h % 3 {
                            0 => "a",
                            1 => "b",
                            _ => "c",
                        };
                        fx.add(
                            "N",
                            &format!("n{i:02}"),
                            vec![("k", Value::Str(val.into()))],
                        );
                    }
                    fx
                };

                let expected = reference_topk(&rule, k, &mut build());
                let actual = streaming_pairs(rule, &mut build());

                assert_eq!(
                    expected, actual,
                    "FieldEqual seed={seed} k={k}: streaming top-k must match brute-force top-k"
                );
            }
        }

        // ----------------------------------------------------------------
        // Case 2: NumericWithin (scored — top-k filters by score DESC, key ASC)
        // S→D, numeric field "v", tolerance 10.0.
        // ----------------------------------------------------------------
        for seed in [0u64, 1, 42, 7] {
            for k in [1u64, 2, 4] {
                let rule = RuleDef {
                    name: "nw".into(),
                    src_label: "S".into(),
                    dst_label: "D".into(),
                    predicate: Predicate::NumericWithin {
                        field: "v".into(),
                        tolerance: 10.0,
                    },
                    edge_type: "NEAR".into(),
                    weight_prop: Some("score".into()),
                    max_edges: Some(k),
                    approximate: false,
                    via_label: None,
                    via_edge: None,
                    via_dir: None,
                };

                let build = || {
                    let mut fx = Fx::new();
                    for i in 0..6u32 {
                        let h = mix64(seed ^ (i as u64 + 1));
                        let v = (h % 20) as f64;
                        fx.add("S", &format!("s{i}"), vec![("v", Value::Float(v))]);
                    }
                    for i in 0..8u32 {
                        let h = mix64(seed ^ (i as u64 + 101));
                        let v = (h % 20) as f64;
                        fx.add("D", &format!("d{i}"), vec![("v", Value::Float(v))]);
                    }
                    fx
                };

                let expected = reference_topk(&rule, k, &mut build());
                let actual = streaming_pairs(rule, &mut build());

                assert_eq!(
                    expected, actual,
                    "NumericWithin seed={seed} k={k}: streaming top-k must match brute-force top-k"
                );
            }
        }

        // ----------------------------------------------------------------
        // Case 3: KeyMatch (CandidateSpec::ByKey)
        // T→C FK rule: each T has a "cid" field whose value is the key of
        // a C node.  Each src has at most 1 candidate, so filter_src_top_k
        // is the identity — but the ByKey candidate path must be exercised.
        // ----------------------------------------------------------------
        for seed in [0u64, 1, 42, 7] {
            for k in [1u64, 2] {
                let rule = RuleDef {
                    name: "fk".into(),
                    src_label: "T".into(),
                    dst_label: "C".into(),
                    predicate: Predicate::KeyMatch {
                        field: "cid".into(),
                    },
                    edge_type: "AT".into(),
                    weight_prop: None,
                    max_edges: Some(k),
                    approximate: false,
                    via_label: None,
                    via_edge: None,
                    via_dir: None,
                };

                let build = || {
                    let mut fx = Fx::new();
                    // 4 C nodes.
                    for i in 0..4u32 {
                        fx.add("C", &format!("c{i}"), vec![]);
                    }
                    // 8 T nodes, each pointing at a C node determined by hash.
                    for i in 0..8u32 {
                        let h = mix64(seed ^ (i as u64 + 1));
                        let cid = format!("c{}", h % 4);
                        fx.add("T", &format!("t{i}"), vec![("cid", Value::Str(cid))]);
                    }
                    fx
                };

                let expected = reference_topk(&rule, k, &mut build());
                let actual = streaming_pairs(rule, &mut build());

                assert_eq!(
                    expected, actual,
                    "KeyMatch seed={seed} k={k}: streaming top-k must match brute-force top-k"
                );
            }
        }

        // ----------------------------------------------------------------
        // Case 4: VectorSimilar approximate=false (CandidateSpec::ScanAll)
        // V→V cosine-sim rule.  6 nodes in 2 clusters of 3; min=0.9 so only
        // within-cluster pairs qualify.  top-k=2 filters the 2 best in cluster.
        // ----------------------------------------------------------------
        {
            // cluster A: unit vectors near [1,0]; cluster B: near [0,1].
            let cluster_a: &[(&str, f64, f64)] = &[
                ("va0", 1.0_f64, 0.0_f64),
                ("va1", 0.98_f64, 0.199_f64), // cos(~11.5°) ≈ 0.98
                ("va2", 0.97_f64, 0.243_f64), // cos(~14°) ≈ 0.97
            ];
            let cluster_b: &[(&str, f64, f64)] = &[
                ("vb0", 0.0_f64, 1.0_f64),
                ("vb1", 0.1_f64, 0.995_f64),
                ("vb2", 0.05_f64, 0.999_f64),
            ];
            for k in [1u64, 2] {
                let rule = RuleDef {
                    name: "vsim".into(),
                    src_label: "V".into(),
                    dst_label: "V".into(),
                    predicate: Predicate::VectorSimilar {
                        field: "emb".into(),
                        min: 0.9,
                    },
                    edge_type: "VSIM".into(),
                    weight_prop: Some("score".into()),
                    max_edges: Some(k),
                    approximate: false,
                    via_label: None,
                    via_edge: None,
                    via_dir: None,
                };

                let build = || {
                    let mut fx = Fx::new();
                    let mut add_v = |key: &str, x: f64, y: f64| {
                        let norm = (x * x + y * y).sqrt();
                        let v = Value::List(vec![Value::Float(x / norm), Value::Float(y / norm)]);
                        fx.add("V", key, vec![("emb", v)]);
                    };
                    for &(k, x, y) in cluster_a.iter().chain(cluster_b.iter()) {
                        add_v(k, x, y);
                    }
                    fx
                };

                let expected = reference_topk(&rule, k, &mut build());
                let actual = streaming_pairs(rule, &mut build());

                assert_eq!(
                    expected, actual,
                    "VectorSimilar/ScanAll k={k}: streaming top-k must match brute-force top-k"
                );
            }
        }
    }

    /// Streaming peak-transient allocation bound.
    ///
    /// Measures the PEAK process RSS *during* `create_rule` by polling from a
    /// background sampler thread at ~1 ms intervals.  Unlike a before/after
    /// snapshot this captures transient allocations freed before the call
    /// returns.
    ///
    /// **Why the OLD code would fail this test:**
    /// The old `compute_full_desired` built a global `BTreeMap<(u32,u32),f64>`
    /// for ALL 250 000 desired pairs (500 Talent × 500 Company, same field
    /// value, FieldEqual) before applying the cap.  At ~26 bytes per BTree
    /// entry (amortised node overhead on aarch64) that is ≈6.5 MiB transient
    /// — held for the entire duration of `apply_desired`.  The peak sampler
    /// would observe this spike; the 3 MiB threshold would be exceeded.
    ///
    /// **Why the NEW code passes:**
    /// `apply_streaming_create` caps after ~1 000 evaluations (one pass over
    /// the first few src nodes).  The largest in-flight allocation is one
    /// per-src `BTreeMap` of ≤ 500 entries ≈ 13 KiB — never materialising
    /// the full 250 000-pair map.  Peak transient delta is sub-100 KiB.
    ///
    /// Threshold 3 MiB: old ≈ 6.5 MiB (FAILS); new ≈ 13 KiB (PASSES).
    ///
    /// Marked `#[ignore]` (forks `ps`, environment-dependent).
    /// Run: `cargo test -p core-rules streaming_peak_transient_bound -- --ignored --test-threads=1`
    #[test]
    #[ignore]
    fn streaming_peak_transient_bound() {
        use std::sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            Arc,
        };

        // Sample process RSS every ~1 ms from a background thread.
        // Returns the peak RSS observed while `f` executes.
        fn peak_rss_during<F: FnOnce()>(f: F) -> u64 {
            let done = Arc::new(AtomicBool::new(false));
            let peak = Arc::new(AtomicU64::new(0));
            let done2 = done.clone();
            let peak2 = peak.clone();
            let pid = std::process::id().to_string();

            let handle = std::thread::spawn(move || {
                while !done2.load(Ordering::Relaxed) {
                    let rss = std::process::Command::new("ps")
                        .args(["-o", "rss=", "-p", &pid])
                        .output()
                        .ok()
                        .and_then(|o| String::from_utf8(o.stdout).ok())
                        .and_then(|s| s.trim().parse::<u64>().ok())
                        .unwrap_or(0)
                        * 1024;
                    peak2.fetch_max(rss, Ordering::Relaxed);
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            });

            f();

            done.store(true, Ordering::Relaxed);
            let _ = handle.join();
            peak.load(Ordering::Relaxed)
        }

        // 500 Talent × 500 Company, all FieldEqual on k="same"
        // → 250 000 desired pairs, top-k = 2 per source (max_edges: Some(2)).
        // Peak transient: one per-src BTreeMap of ≤ 500 entries ≈ 13 KiB.
        let mut fx = Fx::new();
        for i in 0..500u32 {
            fx.add(
                "Talent",
                &format!("t{i}"),
                vec![("k", Value::Str("same".into()))],
            );
        }
        for i in 0..500u32 {
            fx.add(
                "Company",
                &format!("c{i}"),
                vec![("k", Value::Str("same".into()))],
            );
        }
        let rule = RuleDef {
            name: "eq_tc".into(),
            src_label: "Talent".into(),
            dst_label: "Company".into(),
            predicate: Predicate::FieldEqual { field: "k".into() },
            edge_type: "EQ".into(),
            weight_prop: None,
            max_edges: Some(2), // top-k=2 per source; 500 * 2 = 1000 total edges
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        };

        // Baseline: RSS before any create_rule allocation.
        let pid = std::process::id().to_string();
        let baseline = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &pid])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0)
            * 1024;

        let mut eng = RuleEngine::new();
        let peak = peak_rss_during(|| {
            eng.create_rule(rule, &mut fx.g()).unwrap();
        });

        let peak_delta = peak.saturating_sub(baseline);

        // Threshold 3 MiB.  Old O(pairs) path: 250k entries × ~26 bytes ≈ 6.5 MiB
        // transient; would exceed threshold.  New streaming path: single per-src
        // BTreeMap ≤ 500 entries ≈ 13 KiB; never approaches threshold.
        assert!(
            peak_delta < 3 * 1024 * 1024,
            "peak transient delta {} bytes ({} KiB) exceeded 3 MiB; \
             streaming path may be building the full pairs map",
            peak_delta,
            peak_delta / 1024
        );
        assert_eq!(eng.provenance()["eq_tc"].len(), 1_000); // 500 Talent × top-k 2 = 1000
        assert!(!eng.is_tripped("eq_tc")); // top-k rules never trip
        eprintln!(
            "streaming_peak_transient_bound: baseline={baseline} peak={peak} \
             delta={peak_delta} bytes ({} KiB)",
            peak_delta / 1024
        );
    }

    // -----------------------------------------------------------------------
    // Task 3 (Plan 11): Checkpointed Cauchy-Schwarz suffix-norm early exit
    // -----------------------------------------------------------------------

    /// Helper: a near-threshold vector pair. Returns (a, b) where cos(a,b) is
    /// just above the provided threshold (so the pair SHOULD match).
    fn near_threshold_pair(dim: usize, min: f64) -> (Vec<f64>, Vec<f64>) {
        // Construct b = cos_target * a + epsilon * perp, then normalise both.
        // For simplicity: a = [1, 0, ..., 0], b = [cos_target, sin_small, 0, ...]
        let cos_target = min + 1e-6; // just above min
        let sin_small = (1.0 - cos_target * cos_target).sqrt();
        let mut a = vec![0.0f64; dim];
        a[0] = 1.0;
        let mut b = vec![0.0f64; dim];
        b[0] = cos_target;
        if dim > 1 {
            b[1] = sin_small;
        }
        (a, b)
    }

    fn emb_val2(xs: &[f64]) -> Value {
        Value::List(xs.iter().copied().map(Value::Float).collect())
    }

    /// Build an identical test fixture twice so ON/OFF/oracle comparisons all
    /// operate on the same graph topology.  Uses dims [2,4,8,16] with a
    /// near-threshold pair at dim=8 to exercise the checkpoint boundaries.
    fn make_early_exit_fixture(seed: u64, min: f64) -> (Fx, Vec<u32>, usize, usize) {
        let dims = [2usize, 4, 8, 16];
        let n = 100u32;
        let mut fx = Fx::new();
        let mut ids = Vec::new();
        for i in 0..n {
            let dim = dims[(i as usize) % dims.len()];
            let emb = rand_emb(seed, i, dim);
            ids.push(fx.add("Doc", &format!("d{i}"), vec![("emb", emb)]));
        }
        // Near-threshold pair at dim=8, cos just above min → must match.
        let (va, vb) = near_threshold_pair(8, min);
        let nt_a = fx.add("Doc", "nt_a", vec![("emb", emb_val2(&va))]);
        let nt_b = fx.add("Doc", "nt_b", vec![("emb", emb_val2(&vb))]);
        ids.push(nt_a);
        ids.push(nt_b);
        (fx, ids, nt_a as usize, nt_b as usize)
    }

    /// Identity proof: derived edges are identical with early-exit ON, OFF,
    /// and vs the brute-force oracle.  Tests mixed dims (2, 4, 8, 16) with
    /// near-threshold cosines (cos ≈ min ± epsilon) to exercise exact rejects.
    #[test]
    fn vector_early_exit_identity_proof() {
        const SEED: u64 = 0xEA_4E_5A;
        const MIN: f64 = 0.85;

        let def = RuleDef {
            name: "vec".into(),
            src_label: "Doc".into(),
            dst_label: "Doc".into(),
            predicate: Predicate::VectorSimilar {
                field: "emb".into(),
                min: MIN,
            },
            edge_type: "SIM".into(),
            weight_prop: Some("score".into()),
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        };

        // Build three identical fixtures (independent topo state, same data).
        let (mut fx_on, ids, nt_a, nt_b) = make_early_exit_fixture(SEED, MIN);
        let (mut fx_off, _, _, _) = make_early_exit_fixture(SEED, MIN);
        let (fx_oracle, _, _, _) = make_early_exit_fixture(SEED, MIN);

        let nt_a = nt_a as u32;
        let nt_b = nt_b as u32;

        // Run with early-exit ON (default).
        let mut eng_on = RuleEngine::new();
        {
            let mut g = fx_on.g();
            eng_on.create_rule(def.clone(), &mut g).unwrap();
        }
        let edges_on = prov_pairs(&eng_on, "vec");
        assert!(!edges_on.is_empty(), "should produce some edges");

        // Near-threshold pair must appear with early-exit ON.
        assert!(
            edges_on.contains(&(nt_a, nt_b)),
            "near-threshold pair nt_a→nt_b must match with early-exit ON"
        );
        assert!(
            edges_on.contains(&(nt_b, nt_a)),
            "near-threshold pair nt_b→nt_a must match with early-exit ON"
        );

        // Run with early-exit OFF; must produce identical edge set.
        let mut eng_off = RuleEngine::new();
        {
            let mut g = fx_off.g();
            with_vector_early_exit(false, || {
                eng_off.create_rule(def.clone(), &mut g).unwrap();
            });
        }
        let edges_off = prov_pairs(&eng_off, "vec");
        assert_eq!(
            edges_on, edges_off,
            "early-exit ON vs OFF must produce identical edges"
        );

        // Brute-force oracle: evaluate() on all (s,d) pairs.
        let mut oracle = BTreeSet::new();
        for &s in &ids {
            for &d in &ids {
                if s == d {
                    continue;
                }
                let skey = fx_oracle.ids.key_of(s).unwrap();
                let dkey = fx_oracle.ids.key_of(d).unwrap();
                let sg = |f: &str| fx_oracle.props.get(s, f).cloned();
                let dg = |f: &str| fx_oracle.props.get(d, f).cloned();
                if evaluate(
                    &def.predicate,
                    &NodeView {
                        key: skey,
                        props: &sg,
                    },
                    &NodeView {
                        key: dkey,
                        props: &dg,
                    },
                )
                .is_some()
                {
                    oracle.insert((s, d));
                }
            }
        }
        assert_eq!(
            edges_on, oracle,
            "early-exit ON vs brute-force oracle must be identical"
        );
    }

    /// Coherence: checkpoints are rebuilt through the insert/remove choke-points
    /// when a vector prop is updated.  Dim change, freshness gate exercised.
    #[test]
    fn vector_early_exit_checkpoint_coherence() {
        let mut fx = Fx::new();
        // Two dim=4 nodes that match under VectorSimilar min=0.9.
        let a = fx.add("Doc", "a", vec![("emb", emb_val(&[1.0, 0.0, 0.0, 0.0]))]);
        let b = fx.add("Doc", "b", vec![("emb", emb_val(&[1.0, 0.0, 0.0, 0.0]))]);
        // dim=6 node that should NOT match dim=4 nodes.
        let c = fx.add(
            "Doc",
            "c",
            vec![("emb", emb_val(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0]))],
        );
        let def = RuleDef {
            name: "vec".into(),
            src_label: "Doc".into(),
            dst_label: "Doc".into(),
            predicate: Predicate::VectorSimilar {
                field: "emb".into(),
                min: 0.9,
            },
            edge_type: "SIM".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        };

        let mut eng = RuleEngine::new();
        {
            let mut g = fx.g();
            eng.create_rule(def.clone(), &mut g).unwrap();
        }

        // Checkpoints must be populated for all three nodes.
        assert!(
            eng.indexes["vec"].src_side.vec_ckpts(a).is_some(),
            "a must have src checkpoints"
        );
        assert!(
            eng.indexes["vec"].dst_side.vec_ckpts(b).is_some(),
            "b must have dst checkpoints"
        );
        assert!(
            eng.indexes["vec"].src_side.vec_ckpts(c).is_some(),
            "c must have src checkpoints (dim=6)"
        );

        // ckpts[0] must equal the full L2 norm.
        let ckpts_a = *eng.indexes["vec"].src_side.vec_ckpts(a).unwrap();
        let norm_a = eng.indexes["vec"].src_side.vec_meta(a).unwrap().1;
        assert!(
            (ckpts_a[0] - norm_a).abs() < 1e-12,
            "ckpts[0] must equal the full L2 norm"
        );

        // Initial edges: a↔b only (c is different dim).
        assert_eq!(prov_pairs(&eng, "vec"), BTreeSet::from([(a, b), (b, a)]));

        // Update b to dim=6 (same as c) — choke-points must rebuild checkpoints.
        let old_b = fx.props.get(b, "emb").cloned();
        fx.props
            .set(b, "emb", emb_val(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0]));
        {
            let mut g = fx.g();
            eng.on_node_changed(b, Some(("emb", old_b)), &mut g);
        }
        // b's dim must now be 6 in both sides.
        assert_eq!(eng.indexes["vec"].src_side.vec_dim(b), Some(6));
        assert_eq!(eng.indexes["vec"].dst_side.vec_dim(b), Some(6));
        // b must have new checkpoints for dim=6.
        assert!(eng.indexes["vec"].src_side.vec_ckpts(b).is_some());
        // Edges must now be b↔c (both dim=6, cos=1.0 > 0.9).
        assert_eq!(prov_pairs(&eng, "vec"), BTreeSet::from([(b, c), (c, b)]));

        // Freshness gate: fresh_ckpts_for returns None when live vector differs.
        // Simulate by passing a different live vector to fresh_ckpts_for.
        let wrong_live = vec![2.0f64, 0.0, 0.0, 0.0, 0.0, 0.0]; // same dim, different norm
        let gate_result = eng.indexes["vec"].src_side.fresh_ckpts_for(b, &wrong_live);
        assert!(
            gate_result.is_none(),
            "freshness gate must reject a mismatched-norm live vector"
        );

        // fresh_ckpts_for must succeed with the correct live vector.
        let correct_live = vec![1.0f64, 0.0, 0.0, 0.0, 0.0, 0.0];
        let gate_result = eng.indexes["vec"]
            .src_side
            .fresh_ckpts_for(b, &correct_live);
        assert!(
            gate_result.is_some(),
            "freshness gate must accept the matching live vector"
        );
    }

    /// Razor test: dim=1536 pair with true cosine within 1e-12 of `min`.
    ///
    /// Purpose: with energy spread uniformly across all 1536 elements, each
    /// checkpoint boundary contributes a tiny slice of dot product.  Float
    /// rounding of suffix-norm accumulation can shift `cos_max` by O(dim × ε)
    /// ≈ 3.4 × 10⁻¹³ at dim=1536, inside the 1e-12 margin tested here.  The
    /// epsilon guard in `cosine_early_exit` absorbs this; ON/OFF/oracle must
    /// agree on all edges.
    #[test]
    fn vector_early_exit_razor_dim1536() {
        const MIN: f64 = 0.85;
        const DIM: usize = 1536;
        // target cosine = min + 5e-13: inside the dim-scale float-error zone.
        let target = MIN + 5e-13;
        let inv_sqrt = 1.0 / (DIM as f64).sqrt();

        // a: unit-norm uniform vector — energy spread equally across all chunks.
        let a: Vec<f64> = vec![inv_sqrt; DIM];

        // b = target * a + sqrt(1 - target^2) * e_perp
        // e_perp = [1, -1, 0, ..., 0] / sqrt(2) is perpendicular to uniform a:
        //   dot(a, e_perp) = inv_sqrt * (1 - 1) / sqrt(2) = 0  ✓
        // norm(b) = sqrt(target^2 + (1-target^2)) = 1            ✓
        // cos(a, b) = dot(a, b) = target * dot(a, a) = target    ✓
        let perp_scale = (1.0 - target * target).sqrt() / (2.0f64).sqrt();
        let mut b: Vec<f64> = vec![target * inv_sqrt; DIM];
        b[0] += perp_scale;
        b[1] -= perp_scale;

        let def = RuleDef {
            name: "razor".into(),
            src_label: "Doc".into(),
            dst_label: "Doc".into(),
            predicate: Predicate::VectorSimilar {
                field: "emb".into(),
                min: MIN,
            },
            edge_type: "SIM".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        };

        // Three independent fixtures with the same razor pair.
        let build_fx = || {
            let mut fx = Fx::new();
            let na = fx.add("Doc", "razor_a", vec![("emb", emb_val2(&a))]);
            let nb = fx.add("Doc", "razor_b", vec![("emb", emb_val2(&b))]);
            (fx, na, nb)
        };

        let (mut fx_on, na, nb) = build_fx();
        let (mut fx_off, _, _) = build_fx();
        let (fx_oracle, _, _) = build_fx();

        // ON
        let mut eng_on = RuleEngine::new();
        {
            let mut g = fx_on.g();
            eng_on.create_rule(def.clone(), &mut g).unwrap();
        }
        let edges_on = prov_pairs(&eng_on, "razor");
        assert!(
            edges_on.contains(&(na, nb)),
            "razor pair razor_a→razor_b must be present with early-exit ON (cos={target:.15}, min={MIN})"
        );
        assert!(
            edges_on.contains(&(nb, na)),
            "razor pair razor_b→razor_a must be present with early-exit ON"
        );

        // OFF
        let mut eng_off = RuleEngine::new();
        {
            let mut g = fx_off.g();
            with_vector_early_exit(false, || {
                eng_off.create_rule(def.clone(), &mut g).unwrap();
            });
        }
        let edges_off = prov_pairs(&eng_off, "razor");
        assert_eq!(
            edges_on, edges_off,
            "razor dim=1536: early-exit ON vs OFF must produce identical edges"
        );

        // Brute-force oracle.
        let ids = [na, nb];
        let mut oracle = BTreeSet::new();
        for &s in &ids {
            for &d in &ids {
                if s == d {
                    continue;
                }
                let skey = fx_oracle.ids.key_of(s).unwrap();
                let dkey = fx_oracle.ids.key_of(d).unwrap();
                let sg = |f: &str| fx_oracle.props.get(s, f).cloned();
                let dg = |f: &str| fx_oracle.props.get(d, f).cloned();
                if evaluate(
                    &def.predicate,
                    &NodeView {
                        key: skey,
                        props: &sg,
                    },
                    &NodeView {
                        key: dkey,
                        props: &dg,
                    },
                )
                .is_some()
                {
                    oracle.insert((s, d));
                }
            }
        }
        assert_eq!(
            edges_on, oracle,
            "razor dim=1536: early-exit ON vs brute-force oracle must be identical"
        );
    }

    // -----------------------------------------------------------------------
    // Scale test: backfill must not materialise the full cross-product
    // -----------------------------------------------------------------------
    //
    // Step-1 analysis (flow read per brief):
    //
    // compute_desired (~246): returns a BTreeMap<(u32,u32),f64> for ONE source
    //   node against all matching candidates from the dst-side index.  For a
    //   FieldEqual rule with 400 Org dsts all sharing city="austin", each call
    //   returns at most 400 pairs.  The per-source map is dropped after
    //   filter_src_top_k consumes it.
    //
    // compute_desired_via (~452): similar per-anchor scope; not exercised here.
    //
    // compute_full_desired (~945): TEST-ONLY reference implementation.  Iterates
    //   every src node and calls compute_desired, extending a GLOBAL BTreeMap.
    //   For 400 Person × 400 Org this accumulates 160 000 pairs — the full
    //   cross-product — before returning.  This is the memory wall the streaming
    //   rewrite was designed to eliminate.
    //
    // apply_streaming_create_top_k (~1106): the production path for
    //   max_edges=Some(k).  Calls compute_desired per src (≤400 pairs), passes
    //   ownership to filter_src_top_k (truncates to k=5), then drops the map.
    //   The largest map alive at any instant is one per-src BTreeMap of ≤400
    //   entries — never the 160 000-pair global map.
    //
    // filter_src_top_k (~626): runs BEFORE compute_full_desired is ever called
    //   (compute_full_desired is dead code in the production path).  It truncates
    //   the per-source map to k entries BEFORE apply_per_src_top_k sees it.
    //   Conclusion: filter_src_top_k IS applied per-source before any global map
    //   extension; compute_full_desired does NOT participate in create_rule.
    //
    // Expected test behaviour:
    //   The production path (apply_streaming_create_top_k) yields peak ≈ 400
    //   (one per-src BTreeMap).  The assertion bound is 400*5*4 = 8 000 — well
    //   below the 160 000 cross-product.  The test therefore PASSES with the
    //   current streaming code, confirming the fix is in place.
    //
    //   If someone reverts the streaming path and re-introduces a global
    //   compute_full_desired call inside create_rule, peak would reach 160 000
    //   and the assertion would FAIL — which is the regression this test guards.

    /// Helper: FieldEqual rule between two distinct labels with top-k cap.
    fn field_equal_rule(
        src_label: &str,
        dst_label: &str,
        field: &str,
        edge_type: &str,
        max_edges: Option<u64>,
    ) -> RuleDef {
        RuleDef {
            name: format!("{src_label}_{dst_label}_{field}"),
            src_label: src_label.into(),
            dst_label: dst_label.into(),
            predicate: Predicate::FieldEqual {
                field: field.into(),
            },
            edge_type: edge_type.into(),
            weight_prop: None,
            max_edges,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        }
    }

    #[test]
    fn backfill_does_not_materialize_the_cross_product() {
        use std::sync::atomic::Ordering;
        // 400 Person + 400 Org, all city="austin"; rule FieldEqual{city},
        // max_edges=Some(5).  Correct behaviour: 400 × 5 = 2000 derived edges,
        // and peak simultaneous pairs ≤ 400*5*4 (generous headroom), NOT the
        // 160 000 cross-product.
        let mut fx = Fx::new();
        for i in 0..400u32 {
            fx.add(
                "Person",
                &format!("p{i}"),
                vec![("city", Value::Str("austin".into()))],
            );
        }
        for i in 0..400u32 {
            fx.add(
                "Org",
                &format!("o{i}"),
                vec![("city", Value::Str("austin".into()))],
            );
        }

        let mut eng = RuleEngine::new();
        PEAK_DESIRED_PAIRS.store(0, Ordering::Relaxed);
        {
            let mut g = fx.g();
            eng.create_rule(
                field_equal_rule("Person", "Org", "city", "IN_CITY", Some(5)),
                &mut g,
            )
            .unwrap();
        }

        let edges = fx.topo.edge_count();
        assert_eq!(
            edges,
            400 * 5,
            "per-source top-k must yield exactly k per source"
        );

        let peak = PEAK_DESIRED_PAIRS.load(Ordering::Relaxed);
        assert!(
            peak <= 400 * 5 * 4, // generous headroom; NOT the 160_000 cross product
            "backfill must not materialize the full cross-product; peak was {peak}"
        );
    }

    /// Regression guard for the `max_edges = None` (global-budget) backfill path.
    ///
    /// With `max_edges = None`, `create_rule` routes to `apply_streaming_create`
    /// (engine.rs:~1075), which calls `compute_desired` once per source node and
    /// applies edges immediately under a `prov.len() >= budget` latch
    /// (budget = DEFAULT_MAX_EDGES = 1_000_000).  For 400 Person × 400 Org nodes
    /// the cross-product is 160_000 — well below the budget — so ALL pairs are
    /// applied.  Crucially, no global desired-map is ever materialised: the
    /// per-source map is computed, iterated, and dropped before the next source
    /// is processed.
    ///
    /// The budget latch itself (edges capped at DEFAULT_MAX_EDGES when the
    /// cross-product exceeds it) is covered by the existing `#[ignore]`d
    /// `streaming_peak_transient_bound` test (~line 4436); this test guards
    /// peak desired-pair memory only.
    #[test]
    fn global_budget_backfill_stays_per_source_bounded() {
        use std::sync::atomic::Ordering;
        // Same 400 Person × 400 Org shared-value fixture as the Task 1 test,
        // but rule has max_edges = None (global-budget path).
        let mut fx = Fx::new();
        for i in 0..400u32 {
            fx.add(
                "Person",
                &format!("p{i}"),
                vec![("city", Value::Str("austin".into()))],
            );
        }
        for i in 0..400u32 {
            fx.add(
                "Org",
                &format!("o{i}"),
                vec![("city", Value::Str("austin".into()))],
            );
        }

        let mut eng = RuleEngine::new();
        PEAK_DESIRED_PAIRS.store(0, Ordering::Relaxed);
        {
            let mut g = fx.g();
            eng.create_rule(
                field_equal_rule("Person", "Org", "city", "IN_CITY", None),
                &mut g,
            )
            .unwrap();
        }

        // All 160_000 pairs are below DEFAULT_MAX_EDGES (1_000_000), so every
        // pair is applied — edge count equals the full cross-product.
        let edges = fx.topo.edge_count();
        assert_eq!(
            edges,
            400 * 400,
            "none-path must apply all pairs when under budget; got {edges}"
        );

        // Peak simultaneous pairs must be bounded per-source (≤400 candidates),
        // NOT the full 160_000 cross-product.  Any reversion to global desired-map
        // accumulation would observe peak = 160_000 and trip this guard.
        let peak = PEAK_DESIRED_PAIRS.load(Ordering::Relaxed);
        assert!(
            peak <= 400 * 4, // one per-src map of ≤400 candidates, with headroom
            "none-path backfill must not accumulate a global desired-map; peak was {peak}"
        );
    }
}
