//! Exact cosine kernel: pack L2-normalised rows, GEMV / gram via one `dgemm`.
//!
//! Used by brute `find_similar` and `pairwise_similar`. Not used by HNSW.

use core_query::GraphView;
use core_storage::Value;
use std::borrow::Cow;
use std::cell::Cell;

/// Above this packed n, `pairwise_similar` uses n `gemv`s instead of `gram`.
pub const PAIRWISE_GRAM_MAX: usize = 4_096;
/// Hard refuse for `pairwise_similar` on resolved unique key count.
pub const PAIRWISE_MAX_N: usize = 8_192;

thread_local! {
    static PAIRWISE_GRAM_MAX_OVERRIDE: Cell<Option<usize>> = const { Cell::new(None) };
    static PAIRWISE_MAX_N_OVERRIDE: Cell<Option<usize>> = const { Cell::new(None) };
}

pub(crate) fn pairwise_gram_max() -> usize {
    PAIRWISE_GRAM_MAX_OVERRIDE.with(|c| c.get().unwrap_or(PAIRWISE_GRAM_MAX))
}

pub(crate) fn pairwise_max_n() -> usize {
    PAIRWISE_MAX_N_OVERRIDE.with(|c| c.get().unwrap_or(PAIRWISE_MAX_N))
}

/// Run `f` with temporary pairwise n caps. Restores the previous overrides
/// (including across panics). Thread-local, same shape as `HNSW_BUILD_BATCH`.
pub fn with_pairwise_caps<R>(gram_max: usize, max_n: usize, f: impl FnOnce() -> R) -> R {
    let prev_gram = PAIRWISE_GRAM_MAX_OVERRIDE.with(|c| c.replace(Some(gram_max)));
    let prev_n = PAIRWISE_MAX_N_OVERRIDE.with(|c| c.replace(Some(max_n)));
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    PAIRWISE_GRAM_MAX_OVERRIDE.with(|c| c.set(prev_gram));
    PAIRWISE_MAX_N_OVERRIDE.with(|c| c.set(prev_n));
    match out {
        Ok(v) => v,
        Err(p) => std::panic::resume_unwind(p),
    }
}

/// Row-major packed, L2-normalised f64 matrix. Not persisted.
pub struct PackedVectors {
    pub ids: Vec<u32>, // row i is node ids[i]
    pub dim: usize,
    pub data: Vec<f64>, // len == ids.len() * dim, each row unit-length
}

/// Cosine of two already-unit vectors (dot). Test helper only — mixed-dim
/// candidates are skipped, not tailed through this.
#[allow(dead_code)]
pub fn cosine_unit(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

/// Pack candidates whose `len() == dim` and whose L2 norm is non-zero.
/// Other candidates are omitted (not scored).
pub fn pack<'a, I>(rows: I, dim: usize) -> PackedVectors
where
    I: IntoIterator<Item = (u32, &'a [f64])>,
{
    let mut ids = Vec::new();
    let mut data = Vec::new();
    for (id, row) in rows {
        if row.len() != dim {
            continue;
        }
        let norm: f64 = row.iter().map(|x| x * x).sum::<f64>().sqrt();
        if norm == 0.0 {
            continue;
        }
        ids.push(id);
        data.extend(row.iter().map(|x| x / norm));
    }
    PackedVectors { ids, dim, data }
}

/// `out[i] = row(i) · q_unit`. `q_unit.len() == packed.dim`.
pub fn gemv(packed: &PackedVectors, q_unit: &[f64]) -> Vec<f64> {
    debug_assert_eq!(q_unit.len(), packed.dim);
    let n = packed.ids.len();
    let dim = packed.dim;
    let mut out = vec![0.0; n];
    if q_unit.len() != dim {
        return out;
    }
    dgemm_f64(
        n,
        dim,
        1,
        &packed.data,
        dim as isize,
        1,
        q_unit,
        1,
        1,
        &mut out,
        1,
        1,
    );
    out
}

/// `out` is n×n row-major `A Aᵀ`. Diagonal is ~1 for unit rows.
/// Callers must not invoke this for `n > PAIRWISE_GRAM_MAX`.
pub fn gram(packed: &PackedVectors) -> Vec<f64> {
    let n = packed.ids.len();
    let dim = packed.dim;
    let mut out = vec![0.0; n.saturating_mul(n)];
    dgemm_f64(
        n,
        dim,
        n,
        &packed.data,
        dim as isize,
        1,
        &packed.data,
        1,
        dim as isize,
        &mut out,
        n as isize,
        1,
    );
    out
}

/// Overlay-aware f64 view of node `id`'s `field`.
/// Base `ColumnData::Vector` and no overlay/tombstone → `ColumnsView::vector`
/// (`Cow::Borrowed` when aligned). Overlay `Value::List` → owned f64s via
/// `value_as_float_list`. Missing / non-list → `None`.
pub fn vector_f64<'a>(view: &'a GraphView<'_>, id: u32, field: &str) -> Option<Cow<'a, [f64]>> {
    if let Some(v) = view.props.vector(id, field) {
        return Some(v);
    }
    let vr = view.prop(id, field)?;
    let xs = value_as_float_list(vr.as_value())?;
    Some(Cow::Owned(xs))
}

fn value_as_float_list(v: &Value) -> Option<Vec<f64>> {
    match v {
        Value::List(items) => items
            .iter()
            .map(|item| match item {
                Value::Float(f) => Some(*f),
                Value::Int(i) => Some(*i as f64),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

/// C ← A B. Empty `m`/`k`/`n` returns without calling dgemm (`c` left as-is).
#[allow(clippy::too_many_arguments)]
fn dgemm_f64(
    m: usize,
    k: usize,
    n: usize,
    a: &[f64],
    rsa: isize,
    csa: isize,
    b: &[f64],
    rsb: isize,
    csb: isize,
    c: &mut [f64],
    rsc: isize,
    csc: isize,
) {
    if m == 0 || k == 0 || n == 0 {
        return;
    }
    debug_assert!(c.len() >= m.saturating_mul(n));
    // SAFETY: m, k, n are non-zero. `a` is the m×k matrix at (`rsa`, `csa`);
    // `b` is the k×n matrix at (`rsb`, `csb`); `c` is the m×n output at
    // (`rsc`, `csc`) and does not alias `a` or `b`. `rsc`/`csc` are non-zero
    // at every call site, so C elements do not alias each other. β = 0 so C
    // need not be initialized; the Vec is zeroed anyway.
    unsafe {
        matrixmultiply::dgemm(
            m,
            k,
            n,
            1.0,
            a.as_ptr(),
            rsa,
            csa,
            b.as_ptr(),
            rsb,
            csb,
            0.0,
            c.as_mut_ptr(),
            rsc,
            csc,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gram_2x2_orthonormal() {
        let r0: [f64; 2] = [1.0, 0.0];
        let r1: [f64; 2] = [0.0, 1.0];
        let packed = pack([(0, r0.as_slice()), (1, r1.as_slice())], 2);
        let g = gram(&packed);
        assert_eq!(g.len(), 4);
        assert!((g[0] - 1.0).abs() < 1e-12, "g00={}", g[0]);
        assert!(g[1].abs() < 1e-12, "g01={}", g[1]);
        assert!(g[2].abs() < 1e-12, "g10={}", g[2]);
        assert!((g[3] - 1.0).abs() < 1e-12, "g11={}", g[3]);
    }

    #[test]
    fn gemv_matches_cosine_unit() {
        let a = [3.0, 4.0];
        let b = [1.0, 0.0];
        let c = [0.0, 2.0];
        let packed = pack([(0, a.as_slice()), (1, b.as_slice()), (2, c.as_slice())], 2);
        let q = [1.0, 1.0];
        let qn = q.iter().map(|x| x * x).sum::<f64>().sqrt();
        let q_unit = [q[0] / qn, q[1] / qn];
        let scores = gemv(&packed, &q_unit);
        assert_eq!(scores.len(), packed.ids.len());
        for (i, row) in packed.data.chunks(packed.dim).enumerate() {
            let expected = cosine_unit(row, &q_unit);
            assert!(
                (scores[i] - expected).abs() < 1e-12,
                "row {i}: gemv {} vs cosine_unit {expected}",
                scores[i]
            );
        }
    }
}
