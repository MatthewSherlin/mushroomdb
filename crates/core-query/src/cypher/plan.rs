//! Cypher logical planner: `Query` → `Vec<PlanOp>`.
//!
//! Pure (no `GraphView`). Never panics. Bound-destination handling lives on
//! `Expand` (see `PlanOp::Expand`); `JoinBound` is only emitted for the
//! *start* node of a MATCH whose variable is already bound.

use super::ast::{
    Expr, NodePat, Operand, OrderItem, OrderTarget, Pattern, Query, RelDir, RelPat, RetItem, RetVal,
};
use std::collections::BTreeSet;

/// One operator in the logical plan. Patterns compile left-to-right into
/// scan / join / expand ops; WHERE is a single `Filter`; then `Project`,
/// rewritten `OrderBy`, `Skip`, `Limit` in that order.
#[derive(Debug, Clone, PartialEq)]
pub enum PlanOp {
    /// Seed rows from all nodes, or those with `label`.
    ScanLabel {
        var: String,
        label: Option<String>,
    },
    /// Retain rows whose `var` node matches the pattern-map props.
    LookupProps {
        var: String,
        props: Vec<(String, Operand)>,
    },
    /// Expand from `from` along `etype`/`dir`, binding `rel_var` and `to`.
    ///
    /// Destination label/prop checks ride on this op (`to_label` / `to_props`).
    /// If `to` is already bound in the row, the executor keeps only edges
    /// that land on that bound id (JoinBound semantics *inside* Expand).
    /// `rel_var` is always `Some` after planning: user name or `_rN`.
    Expand {
        from: String,
        rel_var: Option<String>,
        etype: Option<String>,
        dir: RelDir,
        to: String,
        to_label: Option<String>,
        to_props: Vec<(String, Operand)>,
    },
    /// Pattern-*start* node whose var is already bound: label/prop re-check only.
    JoinBound {
        var: String,
        label: Option<String>,
        props: Vec<(String, Operand)>,
    },
    Filter {
        expr: Expr,
    },
    Project {
        items: Vec<RetItem>,
    },
    /// After `plan`, every item's `target` is `OrderTarget::Alias(column)`
    /// where `column` is a projected column name. The executor resolves
    /// ORDER BY against the post-Project table only.
    OrderBy {
        items: Vec<OrderItem>,
    },
    Skip(u64),
    Limit(u64),
}

/// Compute the effective row bound for LIMIT push-down.
///
/// Returns `Some(SKIP + LIMIT)` when the plan can terminate producers early —
/// that is, when the plan contains a `Limit` op **and no `OrderBy`** op.
/// An `OrderBy` requires full materialisation before slicing, so the bound
/// cannot be pushed past it.
///
/// Returns `None` when:
/// - No `Limit` op is present, or
/// - An `OrderBy` op is present (sorting needs every row).
///
/// # Decision table
///
/// | Plan shape (tail before Project)           | push-down? | note                                         |
/// |--------------------------------------------|------------|----------------------------------------------|
/// | Scan / Expand → Project → Limit            | YES        | pull-based; all stages stop at bound         |
/// | Scan / Expand → Filter → Project → Limit   | YES        | pull-based; Filter + earlier stages all stop |
/// | … → OrderBy → … Limit                      | NO         | sort requires all rows first                 |
///
/// When `row_bound` returns `Some`, the executor uses a demand-driven
/// (pull-based) strategy: **all** producer stages (Scan, Expand, Filter, …)
/// terminate as soon as `bound` final rows have been collected.  No
/// intermediate table is ever fully materialised for the bounded path.
///
/// `SKIP + LIMIT` is used instead of plain `LIMIT` so that there are enough
/// rows to apply the `SKIP` offset and still yield `LIMIT` final rows.
/// Saturating addition is used to guard against pathological large values.
pub fn row_bound(ops: &[PlanOp]) -> Option<usize> {
    // ORDER BY requires full materialisation — bound cannot be pushed.
    if ops.iter().any(|op| matches!(op, PlanOp::OrderBy { .. })) {
        return None;
    }
    let limit_n = ops.iter().rev().find_map(|op| match op {
        PlanOp::Limit(n) => Some(*n),
        _ => None,
    })?;
    let skip_n = ops
        .iter()
        .rev()
        .find_map(|op| match op {
            PlanOp::Skip(n) => Some(*n),
            _ => None,
        })
        .unwrap_or(0);
    Some((skip_n as usize).saturating_add(limit_n as usize))
}

/// Compile `q` into a logical plan. Errors are contextual `String`s; never panics.
pub fn plan(q: &Query) -> Result<Vec<PlanOp>, String> {
    let mut bound = BTreeSet::new();
    let mut rel_bound = BTreeSet::new();
    let mut ops = Vec::new();
    let mut node_anon = 0u32;
    let mut rel_anon = 0u32;

    for pat in &q.matches {
        compile_pattern(
            pat,
            &mut ops,
            &mut bound,
            &mut rel_bound,
            &mut node_anon,
            &mut rel_anon,
        );
    }

    if let Some(expr) = &q.where_expr {
        check_expr_bound(expr, &bound)?;
        ops.push(PlanOp::Filter { expr: expr.clone() });
    }

    check_return_bound(&q.returns, &bound, &rel_bound)?;
    check_duplicate_aliases(&q.returns)?;
    check_duplicate_columns(&q.returns)?;

    ops.push(PlanOp::Project {
        items: q.returns.clone(),
    });

    if !q.order_by.is_empty() {
        let mut items = Vec::with_capacity(q.order_by.len());
        for item in &q.order_by {
            items.push(rewrite_order_item(item, &q.returns, &bound, &rel_bound)?);
        }
        ops.push(PlanOp::OrderBy { items });
    }

    if let Some(n) = q.skip {
        ops.push(PlanOp::Skip(n));
    }
    if let Some(n) = q.limit {
        ops.push(PlanOp::Limit(n));
    }

    Ok(ops)
}

fn compile_pattern(
    pat: &Pattern,
    ops: &mut Vec<PlanOp>,
    bound: &mut BTreeSet<String>,
    rel_bound: &mut BTreeSet<String>,
    node_anon: &mut u32,
    rel_anon: &mut u32,
) {
    let start = name_node(&pat.start, node_anon, bound);
    if bound.contains(&start) {
        ops.push(PlanOp::JoinBound {
            var: start.clone(),
            label: pat.start.label.clone(),
            props: pat.start.props.clone(),
        });
    } else {
        ops.push(PlanOp::ScanLabel {
            var: start.clone(),
            label: pat.start.label.clone(),
        });
        if !pat.start.props.is_empty() {
            ops.push(PlanOp::LookupProps {
                var: start.clone(),
                props: pat.start.props.clone(),
            });
        }
        bound.insert(start.clone());
    }

    let mut from = start;
    for (rel, dest) in &pat.chain {
        let rel_name = name_rel(rel, rel_anon, bound);
        bound.insert(rel_name.clone());
        rel_bound.insert(rel_name.clone());
        let to = name_node(dest, node_anon, bound);
        ops.push(PlanOp::Expand {
            from,
            rel_var: Some(rel_name),
            etype: rel.etype.clone(),
            dir: rel.dir,
            to: to.clone(),
            to_label: dest.label.clone(),
            to_props: dest.props.clone(),
        });
        bound.insert(to.clone());
        from = to;
    }
}

fn name_node(node: &NodePat, counter: &mut u32, bound: &BTreeSet<String>) -> String {
    match &node.var {
        Some(v) => v.clone(),
        None => fresh("_n", counter, bound),
    }
}

fn name_rel(rel: &RelPat, counter: &mut u32, bound: &BTreeSet<String>) -> String {
    match &rel.var {
        Some(v) => v.clone(),
        None => fresh("_r", counter, bound),
    }
}

/// Stable `_nN` / `_rN` in encounter order. Skips names already bound so a
/// user var `_n0` does not collide with the next anonymous node.
fn fresh(prefix: &str, counter: &mut u32, bound: &BTreeSet<String>) -> String {
    for _ in 0..=u32::MAX {
        let name = format!("{prefix}{counter}");
        *counter = counter.wrapping_add(1);
        if !bound.contains(&name) {
            return name;
        }
    }
    format!("{prefix}x")
}

fn check_expr_bound(expr: &Expr, bound: &BTreeSet<String>) -> Result<(), String> {
    match expr {
        Expr::And(lhs, rhs) | Expr::Or(lhs, rhs) => {
            check_expr_bound(lhs, bound)?;
            check_expr_bound(rhs, bound)
        }
        Expr::Not(inner) => check_expr_bound(inner, bound),
        Expr::Cmp { lhs, rhs, .. } => {
            check_operand_bound(lhs, bound, "WHERE")?;
            check_operand_bound(rhs, bound, "WHERE")
        }
    }
}

fn check_operand_bound(
    operand: &Operand,
    bound: &BTreeSet<String>,
    clause: &str,
) -> Result<(), String> {
    match operand {
        Operand::Prop { var, .. } => require_bound(var, bound, clause),
        Operand::Lit(_) | Operand::Param(_) => Ok(()),
    }
}

fn require_bound(var: &str, bound: &BTreeSet<String>, clause: &str) -> Result<(), String> {
    if bound.contains(var) {
        Ok(())
    } else {
        Err(format!("unbound variable `{var}` in {clause}"))
    }
}

fn reject_bare_rel(var: &str, rel_bound: &BTreeSet<String>) -> Result<(), String> {
    if rel_bound.contains(var) {
        Err(format!(
            "cannot return relationship variable '{var}' bare; return its properties ({var}.field) instead"
        ))
    } else {
        Ok(())
    }
}

fn check_return_bound(
    items: &[RetItem],
    bound: &BTreeSet<String>,
    rel_bound: &BTreeSet<String>,
) -> Result<(), String> {
    for item in items {
        match &item.value {
            RetVal::Var(v) => {
                require_bound(v, bound, "RETURN")?;
                reject_bare_rel(v, rel_bound)?;
            }
            RetVal::Prop { var, .. } => {
                require_bound(var, bound, "RETURN")?;
            }
        }
    }
    Ok(())
}

fn check_duplicate_aliases(items: &[RetItem]) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for item in items {
        if let Some(alias) = &item.alias {
            if !seen.insert(alias.clone()) {
                return Err(format!("duplicate RETURN alias `{alias}`"));
            }
        }
    }
    Ok(())
}

fn check_duplicate_columns(items: &[RetItem]) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for item in items {
        let col = column_name(item);
        if !seen.insert(col.clone()) {
            return Err(format!("duplicate RETURN column `{col}`"));
        }
    }
    Ok(())
}

/// Projected column name: alias if given, else the bare var, else `var.field`.
fn column_name(item: &RetItem) -> String {
    if let Some(alias) = &item.alias {
        return alias.clone();
    }
    match &item.value {
        RetVal::Var(v) => v.clone(),
        RetVal::Prop { var, field } => format!("{var}.{field}"),
    }
}

fn rewrite_order_item(
    item: &OrderItem,
    returns: &[RetItem],
    bound: &BTreeSet<String>,
    rel_bound: &BTreeSet<String>,
) -> Result<OrderItem, String> {
    let column = match &item.target {
        OrderTarget::Alias(name) => {
            if returns
                .iter()
                .any(|r| r.alias.as_deref() == Some(name.as_str()))
            {
                name.clone()
            } else {
                return Err(format!("ORDER BY target `{name}` is not present in RETURN"));
            }
        }
        OrderTarget::Var(v) => {
            require_bound(v, bound, "ORDER BY")?;
            reject_bare_rel(v, rel_bound)?;
            match returns
                .iter()
                .find(|r| matches!(&r.value, RetVal::Var(x) if x == v))
            {
                Some(r) => column_name(r),
                None => {
                    return Err(format!("ORDER BY target `{v}` is not present in RETURN"));
                }
            }
        }
        OrderTarget::Prop { var, field } => {
            require_bound(var, bound, "ORDER BY")?;
            match returns.iter().find(
                |r| matches!(&r.value, RetVal::Prop { var: v, field: f } if v == var && f == field),
            ) {
                Some(r) => column_name(r),
                None => {
                    return Err(format!(
                        "ORDER BY target `{var}.{field}` is not present in RETURN"
                    ));
                }
            }
        }
    };
    Ok(OrderItem {
        target: OrderTarget::Alias(column),
        descending: item.descending,
    })
}

#[cfg(test)]
mod tests {
    use super::{plan, PlanOp};
    use crate::cypher::ast::{Expr, Operand, OrderItem, OrderTarget, RetItem, RetVal};
    use crate::cypher::{lex, parse, RelDir};
    use crate::filter::CmpOp;
    use core_storage::Value;

    fn plan_src(src: &str) -> Result<Vec<PlanOp>, String> {
        plan(&parse(&lex(src)?)?)
    }

    fn assert_plan_err(src: &str, needle: &str) -> String {
        let result = std::panic::catch_unwind(|| plan_src(src));
        assert!(result.is_ok(), "plan({src:?}) panicked");
        let err = result
            .unwrap()
            .expect_err(&format!("plan({src:?}) must be Err"));
        assert!(
            err.contains(needle),
            "error must mention {needle:?}, got: {err}"
        );
        err
    }

    /// Dogfood query from T6. Shape:
    /// - MATCH 1: `t` unbound → `ScanLabel` + `LookupProps`.
    /// - MATCH 2: `c` unbound → `ScanLabel`; expand to already-bound `t`.
    ///   Bound dest is *not* a trailing `JoinBound` — Expand carries dest
    ///   checks and the executor filters edges to the bound `to` id.
    /// - MATCH 3: start `c` already bound → `JoinBound`; expand to bound `t`.
    #[test]
    fn dogfood_query_exact_plan() {
        let src = "\
MATCH (t:Talent {id: $tid}) \
MATCH (c:Company)-[i:INDUSTRY_ALIGNMENT]->(t) \
MATCH (c)-[s:SPECIALTY_MATCH]->(t) \
WHERE i.score >= 0.5 AND s.score >= 0.5 \
RETURN c, i.score AS industry, s.score AS specialty \
ORDER BY industry DESC, specialty DESC \
LIMIT 10";
        let got = plan_src(src).expect("dogfood query must plan");
        let expected = vec![
            PlanOp::ScanLabel {
                var: "t".into(),
                label: Some("Talent".into()),
            },
            PlanOp::LookupProps {
                var: "t".into(),
                props: vec![("id".into(), Operand::Param("tid".into()))],
            },
            PlanOp::ScanLabel {
                var: "c".into(),
                label: Some("Company".into()),
            },
            PlanOp::Expand {
                from: "c".into(),
                rel_var: Some("i".into()),
                etype: Some("INDUSTRY_ALIGNMENT".into()),
                dir: RelDir::Right,
                to: "t".into(),
                to_label: None,
                to_props: vec![],
            },
            PlanOp::JoinBound {
                var: "c".into(),
                label: None,
                props: vec![],
            },
            PlanOp::Expand {
                from: "c".into(),
                rel_var: Some("s".into()),
                etype: Some("SPECIALTY_MATCH".into()),
                dir: RelDir::Right,
                to: "t".into(),
                to_label: None,
                to_props: vec![],
            },
            PlanOp::Filter {
                expr: Expr::And(
                    Box::new(Expr::Cmp {
                        lhs: Operand::Prop {
                            var: "i".into(),
                            field: "score".into(),
                        },
                        op: CmpOp::Ge,
                        rhs: Operand::Lit(Value::Float(0.5)),
                    }),
                    Box::new(Expr::Cmp {
                        lhs: Operand::Prop {
                            var: "s".into(),
                            field: "score".into(),
                        },
                        op: CmpOp::Ge,
                        rhs: Operand::Lit(Value::Float(0.5)),
                    }),
                ),
            },
            PlanOp::Project {
                items: vec![
                    RetItem {
                        value: RetVal::Var("c".into()),
                        alias: None,
                    },
                    RetItem {
                        value: RetVal::Prop {
                            var: "i".into(),
                            field: "score".into(),
                        },
                        alias: Some("industry".into()),
                    },
                    RetItem {
                        value: RetVal::Prop {
                            var: "s".into(),
                            field: "score".into(),
                        },
                        alias: Some("specialty".into()),
                    },
                ],
            },
            PlanOp::OrderBy {
                items: vec![
                    OrderItem {
                        target: OrderTarget::Alias("industry".into()),
                        descending: true,
                    },
                    OrderItem {
                        target: OrderTarget::Alias("specialty".into()),
                        descending: true,
                    },
                ],
            },
            PlanOp::Limit(10),
        ];
        assert_eq!(got, expected);
    }

    /// Anonymous names increment in encounter order across the whole query.
    /// MATCH 1: start `_n0`, rel `_r0`, dest `a`.
    /// MATCH 2: start `_n1`, rel `_r1`, dest already-bound `a` (Expand only).
    #[test]
    fn anonymous_node_and_rel_names_are_stable() {
        let got = plan_src("MATCH ()-[]->(a) MATCH ()-[]->(a) RETURN a").unwrap();
        assert_eq!(
            got,
            vec![
                PlanOp::ScanLabel {
                    var: "_n0".into(),
                    label: None,
                },
                PlanOp::Expand {
                    from: "_n0".into(),
                    rel_var: Some("_r0".into()),
                    etype: None,
                    dir: RelDir::Right,
                    to: "a".into(),
                    to_label: None,
                    to_props: vec![],
                },
                PlanOp::ScanLabel {
                    var: "_n1".into(),
                    label: None,
                },
                PlanOp::Expand {
                    from: "_n1".into(),
                    rel_var: Some("_r1".into()),
                    etype: None,
                    dir: RelDir::Right,
                    to: "a".into(),
                    to_label: None,
                    to_props: vec![],
                },
                PlanOp::Project {
                    items: vec![RetItem {
                        value: RetVal::Var("a".into()),
                        alias: None,
                    }],
                },
            ]
        );
    }

    #[test]
    fn props_on_scan_node_emit_scan_then_lookup() {
        let got = plan_src("MATCH (t:Talent {id: $tid}) RETURN t").unwrap();
        assert_eq!(
            got,
            vec![
                PlanOp::ScanLabel {
                    var: "t".into(),
                    label: Some("Talent".into()),
                },
                PlanOp::LookupProps {
                    var: "t".into(),
                    props: vec![("id".into(), Operand::Param("tid".into()))],
                },
                PlanOp::Project {
                    items: vec![RetItem {
                        value: RetVal::Var("t".into()),
                        alias: None,
                    }],
                },
            ]
        );
    }

    #[test]
    fn unbound_var_in_where_is_err() {
        let err = assert_plan_err("MATCH (a) WHERE b.x = 1 RETURN a", "b");
        assert!(
            err.to_ascii_lowercase().contains("unbound")
                && err.to_ascii_lowercase().contains("where"),
            "expected unbound-in-WHERE context, got: {err}"
        );
    }

    #[test]
    fn unbound_var_in_return_is_err() {
        let err = assert_plan_err("MATCH (a) RETURN b", "b");
        assert!(
            err.to_ascii_lowercase().contains("unbound")
                && err.to_ascii_lowercase().contains("return"),
            "expected unbound-in-RETURN context, got: {err}"
        );
    }

    #[test]
    fn unbound_var_in_order_by_is_err() {
        let err = assert_plan_err("MATCH (a) RETURN a ORDER BY b", "b");
        assert!(
            err.to_ascii_lowercase().contains("unbound")
                && (err.to_ascii_lowercase().contains("order")),
            "expected unbound-in-ORDER context, got: {err}"
        );
    }

    #[test]
    fn duplicate_alias_is_err() {
        let err = assert_plan_err("MATCH (a) RETURN a AS x, a.id AS x", "x");
        assert!(
            err.to_ascii_lowercase().contains("duplicate")
                && err.to_ascii_lowercase().contains("alias"),
            "expected duplicate-alias context, got: {err}"
        );
    }

    #[test]
    fn duplicate_column_name_is_err() {
        let err = assert_plan_err("MATCH (a) RETURN a, a", "a");
        assert!(
            err.to_ascii_lowercase().contains("duplicate")
                && err.to_ascii_lowercase().contains("column"),
            "expected duplicate-column context, got: {err}"
        );
    }

    #[test]
    fn order_by_target_absent_from_return_is_err() {
        // `a` is bound, but `a.x` is not a RETURN item.
        let err = assert_plan_err("MATCH (a) RETURN a ORDER BY a.x", "a.x");
        assert!(
            err.to_ascii_lowercase().contains("return"),
            "expected ORDER BY target-not-in-RETURN context, got: {err}"
        );
    }

    /// Alias → that alias's column; bare var → its RETURN column (alias if
    /// given, else the var name); un-aliased prop → `var.field`.
    #[test]
    fn order_by_targets_rewrite_to_projected_column_names() {
        let got = plan_src(
            "MATCH (a)-[r]->(b) \
             RETURN a, a.name AS nm, b.age \
             ORDER BY nm DESC, a ASC, b.age",
        )
        .unwrap();
        let order = got
            .iter()
            .find_map(|op| match op {
                PlanOp::OrderBy { items } => Some(items),
                _ => None,
            })
            .expect("plan must contain OrderBy");
        assert_eq!(
            order,
            &vec![
                OrderItem {
                    target: OrderTarget::Alias("nm".into()),
                    descending: true,
                },
                OrderItem {
                    target: OrderTarget::Alias("a".into()),
                    descending: false,
                },
                OrderItem {
                    target: OrderTarget::Alias("b.age".into()),
                    descending: false,
                },
            ]
        );

        let aliased_var = plan_src("MATCH (a) RETURN a AS person ORDER BY a").unwrap();
        let order = aliased_var
            .iter()
            .find_map(|op| match op {
                PlanOp::OrderBy { items } => Some(items),
                _ => None,
            })
            .expect("plan must contain OrderBy");
        assert_eq!(
            order,
            &vec![OrderItem {
                target: OrderTarget::Alias("person".into()),
                descending: false,
            }]
        );
    }

    #[test]
    fn bound_pattern_start_is_join_bound_then_expand() {
        let got = plan_src("MATCH (a:L) MATCH (a)-[r:T]->(b) RETURN a, b").unwrap();
        assert_eq!(
            got,
            vec![
                PlanOp::ScanLabel {
                    var: "a".into(),
                    label: Some("L".into()),
                },
                PlanOp::JoinBound {
                    var: "a".into(),
                    label: None,
                    props: vec![],
                },
                PlanOp::Expand {
                    from: "a".into(),
                    rel_var: Some("r".into()),
                    etype: Some("T".into()),
                    dir: RelDir::Right,
                    to: "b".into(),
                    to_label: None,
                    to_props: vec![],
                },
                PlanOp::Project {
                    items: vec![
                        RetItem {
                            value: RetVal::Var("a".into()),
                            alias: None,
                        },
                        RetItem {
                            value: RetVal::Var("b".into()),
                            alias: None,
                        },
                    ],
                },
            ]
        );
    }

    #[test]
    fn bound_dest_extra_checks_ride_on_expand() {
        let got = plan_src("MATCH (t:Talent) MATCH (c)-[r]->(t:Talent {id: 1}) RETURN t").unwrap();
        assert_eq!(
            got,
            vec![
                PlanOp::ScanLabel {
                    var: "t".into(),
                    label: Some("Talent".into()),
                },
                PlanOp::ScanLabel {
                    var: "c".into(),
                    label: None,
                },
                PlanOp::Expand {
                    from: "c".into(),
                    rel_var: Some("r".into()),
                    etype: None,
                    dir: RelDir::Right,
                    to: "t".into(),
                    to_label: Some("Talent".into()),
                    to_props: vec![("id".into(), Operand::Lit(Value::Int(1)))],
                },
                PlanOp::Project {
                    items: vec![RetItem {
                        value: RetVal::Var("t".into()),
                        alias: None,
                    }],
                },
            ]
        );
    }

    #[test]
    fn skip_then_limit_follow_project() {
        let got = plan_src("MATCH (a) RETURN a SKIP 2 LIMIT 3").unwrap();
        assert_eq!(
            got,
            vec![
                PlanOp::ScanLabel {
                    var: "a".into(),
                    label: None,
                },
                PlanOp::Project {
                    items: vec![RetItem {
                        value: RetVal::Var("a".into()),
                        alias: None,
                    }],
                },
                PlanOp::Skip(2),
                PlanOp::Limit(3),
            ]
        );
    }

    #[test]
    fn aliased_prop_order_by_rewrites_to_alias_column() {
        let got = plan_src("MATCH (a) RETURN a.name AS nm ORDER BY a.name").unwrap();
        let order = got
            .iter()
            .find_map(|op| match op {
                PlanOp::OrderBy { items } => Some(items),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            order,
            &vec![OrderItem {
                target: OrderTarget::Alias("nm".into()),
                descending: false,
            }]
        );
    }

    #[test]
    fn plan_never_panics_on_hand_built_query() {
        use crate::cypher::ast::{NodePat, Pattern, Query};
        let q = Query {
            matches: vec![],
            where_expr: None,
            returns: vec![],
            order_by: vec![],
            skip: None,
            limit: None,
        };
        let result = std::panic::catch_unwind(|| plan(&q));
        assert!(result.is_ok(), "plan panicked on empty Query");
        let _ = result.unwrap();

        let q = Query {
            matches: vec![Pattern {
                start: NodePat {
                    var: None,
                    label: None,
                    props: vec![],
                },
                chain: vec![],
            }],
            where_expr: Some(Expr::Not(Box::new(Expr::Cmp {
                lhs: Operand::Param("p".into()),
                op: CmpOp::Eq,
                rhs: Operand::Lit(Value::Int(1)),
            }))),
            returns: vec![],
            order_by: vec![OrderItem {
                target: OrderTarget::Alias("missing".into()),
                descending: true,
            }],
            skip: Some(0),
            limit: Some(0),
        };
        let result = std::panic::catch_unwind(|| plan(&q));
        assert!(result.is_ok(), "plan panicked on hand-built Query");
        let _ = result.unwrap();
    }

    #[test]
    fn bare_relationship_var_in_return_is_err() {
        let err = assert_plan_err("MATCH (a)-[r:T]->(b) RETURN r", "r");
        assert!(
            err.to_ascii_lowercase().contains("relationship"),
            "expected bare-rel RETURN guidance, got: {err}"
        );
    }

    #[test]
    fn relationship_prop_in_return_is_ok() {
        plan_src("MATCH (a)-[r:T]->(b) RETURN r.w").expect("rel prop RETURN must plan");
    }

    #[test]
    fn bare_relationship_var_in_order_by_is_err() {
        // RETURN r.w is legal; ORDER BY r is a bare rel var (defense, not just
        // "not in RETURN").
        let err = assert_plan_err("MATCH (a)-[r:T]->(b) RETURN r.w ORDER BY r", "r");
        assert!(
            err.to_ascii_lowercase().contains("relationship"),
            "expected bare-rel ORDER BY guidance, got: {err}"
        );
    }

    #[test]
    fn relationship_prop_in_order_by_is_ok() {
        plan_src("MATCH (a)-[r:T]->(b) RETURN r.w ORDER BY r.w")
            .expect("rel prop ORDER BY must plan");
    }
}
