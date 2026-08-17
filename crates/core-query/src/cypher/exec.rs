//! Cypher executor: `PlanOp` sequence → `ResultSet` over a binding table.

use crate::cypher::ast::{Expr, Operand, OrderItem, OrderTarget, RetItem, RetVal};
use crate::cypher::plan::PlanOp;
use crate::cypher::RelDir;
use crate::filter::eval_cmp;
use crate::result::ResultSet;
use crate::traverse::{expand, Dir, EdgeRef};
use crate::value_ops::{cmp_optional, values_equal};
use crate::view::GraphView;
use core_storage::Value;
use std::collections::{BTreeMap, BTreeSet};

/// Query parameters. Missing names anywhere in the plan are an error at
/// execution start (the plan is walked before any rows are produced).
pub struct Params<'a>(pub &'a BTreeMap<String, Value>);

#[derive(Clone, Debug)]
enum Cell {
    Node(u32),
    Rel(EdgeRef),
}

type Row = BTreeMap<String, Cell>;

struct Projected {
    columns: Vec<String>,
    rows: Vec<Vec<Option<Value>>>,
}

/// Execute a plan against a view. Row order before OrderBy is deterministic
/// (scan order = dense ids; expand order = expand()'s sorted order).
pub fn execute(view: &GraphView, plan: &[PlanOp], params: &Params) -> Result<ResultSet, String> {
    check_params(plan, params)?;

    let mut rows: Vec<Row> = vec![BTreeMap::new()];
    let mut projected: Option<Projected> = None;

    for op in plan {
        match op {
            PlanOp::ScanLabel { var, label } => {
                rows = scan_label(view, &rows, var, label.as_deref());
            }
            PlanOp::LookupProps { var, props } => {
                rows = retain_node(view, &rows, var, None, props, params)?;
            }
            PlanOp::JoinBound { var, label, props } => {
                rows = retain_node(view, &rows, var, label.as_deref(), props, params)?;
            }
            PlanOp::Expand { .. } => {
                rows = exec_expand(view, &rows, op, params)?;
            }
            PlanOp::Filter { expr } => {
                rows = exec_filter(view, &rows, expr, params)?;
            }
            PlanOp::Project { items } => {
                projected = Some(exec_project(view, &rows, items)?);
            }
            PlanOp::OrderBy { items } => {
                let table = projected
                    .as_mut()
                    .ok_or_else(|| "ORDER BY before PROJECT".to_string())?;
                exec_order_by(table, items)?;
            }
            PlanOp::Skip(n) => {
                if let Some(table) = projected.as_mut() {
                    apply_skip(&mut table.rows, *n);
                } else {
                    apply_skip(&mut rows, *n);
                }
            }
            PlanOp::Limit(n) => {
                if let Some(table) = projected.as_mut() {
                    apply_limit(&mut table.rows, *n);
                } else {
                    apply_limit(&mut rows, *n);
                }
            }
        }
    }

    Ok(match projected {
        Some(table) => finish(table),
        None => ResultSet::new(vec![]),
    })
}

fn finish(table: Projected) -> ResultSet {
    let mut rs = ResultSet::new(table.columns);
    for row in table.rows {
        rs.push_row(row);
    }
    rs
}

fn check_params(plan: &[PlanOp], params: &Params) -> Result<(), String> {
    let mut names = Vec::new();
    let mut seen = BTreeSet::new();
    for op in plan {
        match op {
            PlanOp::LookupProps { props, .. }
            | PlanOp::JoinBound { props, .. }
            | PlanOp::Expand {
                to_props: props, ..
            } => {
                for (_, operand) in props {
                    collect_operand(operand, &mut names, &mut seen);
                }
            }
            PlanOp::Filter { expr } => collect_expr(expr, &mut names, &mut seen, 0)?,
            _ => {}
        }
    }
    for name in names {
        if !params.0.contains_key(&name) {
            return Err(format!("missing parameter `{name}`"));
        }
    }
    Ok(())
}

fn collect_operand(op: &Operand, names: &mut Vec<String>, seen: &mut BTreeSet<String>) {
    if let Operand::Param(n) = op {
        if seen.insert(n.clone()) {
            names.push(n.clone());
        }
    }
}

fn collect_expr(
    expr: &Expr,
    names: &mut Vec<String>,
    seen: &mut BTreeSet<String>,
    depth: u32,
) -> Result<(), String> {
    if depth > 256 {
        return Err("expression nesting too deep".into());
    }
    match expr {
        Expr::And(lhs, rhs) | Expr::Or(lhs, rhs) => {
            collect_expr(lhs, names, seen, depth + 1)?;
            collect_expr(rhs, names, seen, depth + 1)
        }
        Expr::Not(inner) => collect_expr(inner, names, seen, depth + 1),
        Expr::Cmp { lhs, rhs, .. } => {
            collect_operand(lhs, names, seen);
            collect_operand(rhs, names, seen);
            Ok(())
        }
    }
}

fn scan_ids(view: &GraphView, label: Option<&str>) -> Vec<u32> {
    match label {
        Some(label) => view.nodes_with_label(label),
        None => (0..view.ids.len() as u32).collect(),
    }
}

fn scan_label(view: &GraphView, rows: &[Row], var: &str, label: Option<&str>) -> Vec<Row> {
    let ids = scan_ids(view, label);
    let mut out = Vec::new();
    for row in rows {
        for &id in &ids {
            let mut next = row.clone();
            next.insert(var.to_string(), Cell::Node(id));
            out.push(next);
        }
    }
    out
}

fn require_cell<'a>(row: &'a Row, var: &str) -> Result<&'a Cell, String> {
    row.get(var)
        .ok_or_else(|| format!("unbound variable `{var}`"))
}

fn require_node(row: &Row, var: &str) -> Result<u32, String> {
    match require_cell(row, var)? {
        Cell::Node(id) => Ok(*id),
        Cell::Rel(_) => Err(format!("variable `{var}` is not a node")),
    }
}

fn resolve_operand(
    view: &GraphView,
    row: &Row,
    operand: &Operand,
    params: &Params,
) -> Result<Option<Value>, String> {
    match operand {
        Operand::Lit(v) => Ok(Some(v.clone())),
        Operand::Param(name) => match params.0.get(name) {
            Some(v) => Ok(Some(v.clone())),
            None => Err(format!("missing parameter `{name}`")),
        },
        Operand::Prop { var, field } => resolve_prop(view, row, var, field),
    }
}

fn resolve_prop(
    view: &GraphView,
    row: &Row,
    var: &str,
    field: &str,
) -> Result<Option<Value>, String> {
    match require_cell(row, var)? {
        Cell::Node(id) => Ok(view.prop(*id, field).cloned()),
        Cell::Rel(e) => Ok(view.edge_props.get(e.etype, e.src, e.dst, field).cloned()),
    }
}

fn node_matches(
    view: &GraphView,
    row: &Row,
    id: u32,
    label: Option<&str>,
    props: &[(String, Operand)],
    params: &Params,
) -> Result<bool, String> {
    if let Some(want) = label {
        match view.label_of(id) {
            Some(got) if got == want => {}
            _ => return Ok(false),
        }
    }
    for (field, operand) in props {
        let Some(expected) = resolve_operand(view, row, operand, params)? else {
            return Ok(false);
        };
        match view.prop(id, field) {
            Some(got) if values_equal(got, &expected) => {}
            _ => return Ok(false),
        }
    }
    Ok(true)
}

fn retain_node(
    view: &GraphView,
    rows: &[Row],
    var: &str,
    label: Option<&str>,
    props: &[(String, Operand)],
    params: &Params,
) -> Result<Vec<Row>, String> {
    let mut out = Vec::new();
    for row in rows {
        let id = require_node(row, var)?;
        if node_matches(view, row, id, label, props, params)? {
            out.push(row.clone());
        }
    }
    Ok(out)
}

fn map_dir(dir: RelDir) -> Dir {
    match dir {
        RelDir::Right => Dir::Out,
        RelDir::Left => Dir::In,
        RelDir::Undirected => Dir::Both,
    }
}

fn neighbor(from: u32, e: &EdgeRef, dir: RelDir) -> u32 {
    match dir {
        RelDir::Right => e.dst,
        RelDir::Left => e.src,
        RelDir::Undirected => {
            if e.src == from {
                e.dst
            } else {
                e.src
            }
        }
    }
}

fn row_has_edge(row: &Row, e: &EdgeRef) -> bool {
    row.values()
        .any(|c| matches!(c, Cell::Rel(existing) if existing == e))
}

fn resolve_etypes(view: &GraphView, etype: Option<&str>) -> Option<Vec<u32>> {
    etype.map(|name| view.syms.get(name).into_iter().collect())
}

fn exec_expand(
    view: &GraphView,
    rows: &[Row],
    op: &PlanOp,
    params: &Params,
) -> Result<Vec<Row>, String> {
    let PlanOp::Expand {
        from,
        rel_var,
        etype,
        dir,
        to,
        to_label,
        to_props,
    } = op
    else {
        return Err("internal: expected Expand".into());
    };
    let etypes = resolve_etypes(view, etype.as_deref());
    let exp_dir = map_dir(*dir);
    let mut out = Vec::new();
    for row in rows {
        let from_id = require_node(row, from)?;
        let bound_to = match row.get(to) {
            Some(Cell::Node(id)) => Some(*id),
            Some(Cell::Rel(_)) => return Err(format!("variable `{to}` is not a node")),
            None => None,
        };
        for e in expand(view, from_id, etypes.as_deref(), exp_dir) {
            if row_has_edge(row, &e) {
                continue;
            }
            let nbr = neighbor(from_id, &e, *dir);
            if let Some(want) = bound_to {
                if nbr != want {
                    continue;
                }
            }
            if !node_matches(view, row, nbr, to_label.as_deref(), to_props, params)? {
                continue;
            }
            let mut next = row.clone();
            if let Some(rv) = rel_var {
                next.insert(rv.clone(), Cell::Rel(e));
            }
            if bound_to.is_none() {
                next.insert(to.clone(), Cell::Node(nbr));
            }
            out.push(next);
        }
    }
    Ok(out)
}

fn exec_filter(
    view: &GraphView,
    rows: &[Row],
    expr: &Expr,
    params: &Params,
) -> Result<Vec<Row>, String> {
    let mut out = Vec::new();
    for row in rows {
        if eval_expr(view, row, expr, params, 0)? {
            out.push(row.clone());
        }
    }
    Ok(out)
}

fn eval_expr(
    view: &GraphView,
    row: &Row,
    expr: &Expr,
    params: &Params,
    depth: u32,
) -> Result<bool, String> {
    if depth > 256 {
        return Err("expression nesting too deep".into());
    }
    match expr {
        Expr::And(lhs, rhs) => {
            let l = eval_expr(view, row, lhs, params, depth + 1)?;
            let r = eval_expr(view, row, rhs, params, depth + 1)?;
            Ok(l && r)
        }
        Expr::Or(lhs, rhs) => {
            let l = eval_expr(view, row, lhs, params, depth + 1)?;
            let r = eval_expr(view, row, rhs, params, depth + 1)?;
            Ok(l || r)
        }
        Expr::Not(inner) => Ok(!eval_expr(view, row, inner, params, depth + 1)?),
        Expr::Cmp { lhs, op, rhs } => {
            let l = resolve_operand(view, row, lhs, params)?;
            let r = resolve_operand(view, row, rhs, params)?;
            match (l, r) {
                (Some(a), Some(b)) => Ok(eval_cmp(op, &a, &b)),
                _ => Ok(false),
            }
        }
    }
}

fn column_name(item: &RetItem) -> String {
    if let Some(alias) = &item.alias {
        return alias.clone();
    }
    match &item.value {
        RetVal::Var(v) => v.clone(),
        RetVal::Prop { var, field } => format!("{var}.{field}"),
    }
}

fn exec_project(view: &GraphView, rows: &[Row], items: &[RetItem]) -> Result<Projected, String> {
    let columns: Vec<String> = items.iter().map(column_name).collect();
    let mut out_rows = Vec::with_capacity(rows.len());
    for row in rows {
        let mut cells = Vec::with_capacity(items.len());
        for item in items {
            cells.push(project_item(view, row, item)?);
        }
        out_rows.push(cells);
    }
    Ok(Projected {
        columns,
        rows: out_rows,
    })
}

fn project_item(view: &GraphView, row: &Row, item: &RetItem) -> Result<Option<Value>, String> {
    match &item.value {
        RetVal::Var(v) => {
            let id = require_node(row, v)?;
            match view.ids.key_of(id) {
                Some(key) => Ok(Some(Value::Str(key.to_owned()))),
                None => Err(format!("unknown node id {id}")),
            }
        }
        RetVal::Prop { var, field } => resolve_prop(view, row, var, field),
    }
}

fn order_column(item: &OrderItem) -> String {
    match &item.target {
        OrderTarget::Alias(name) | OrderTarget::Var(name) => name.clone(),
        OrderTarget::Prop { var, field } => format!("{var}.{field}"),
    }
}

fn exec_order_by(table: &mut Projected, items: &[OrderItem]) -> Result<(), String> {
    let mut keys = Vec::with_capacity(items.len());
    for item in items {
        let name = order_column(item);
        let idx = table
            .columns
            .iter()
            .position(|c| c == &name)
            .ok_or_else(|| format!("ORDER BY target `{name}` is not a projected column"))?;
        keys.push((idx, item.descending));
    }
    table.rows.sort_by(|a, b| {
        for &(idx, desc) in &keys {
            let c = cmp_optional(
                a.get(idx).and_then(|x| x.as_ref()),
                b.get(idx).and_then(|x| x.as_ref()),
                desc,
            );
            if c != std::cmp::Ordering::Equal {
                return c;
            }
        }
        std::cmp::Ordering::Equal
    });
    Ok(())
}

fn apply_skip<T>(rows: &mut Vec<T>, n: u64) {
    let n = usize::try_from(n).unwrap_or(usize::MAX);
    if n >= rows.len() {
        rows.clear();
    } else {
        rows.drain(0..n);
    }
}

fn apply_limit<T>(rows: &mut Vec<T>, n: u64) {
    let n = usize::try_from(n).unwrap_or(usize::MAX);
    rows.truncate(n);
}

#[cfg(test)]
mod tests {
    use super::{execute, Params};
    use crate::cypher::ast::{Operand, OrderItem, OrderTarget, RetItem, RetVal};
    use crate::cypher::plan::{plan, PlanOp};
    use crate::cypher::{lex, parse, RelDir};
    use crate::result::ResultSet;
    use crate::view::GraphView;
    use core_storage::{ColumnStore, EdgeProps, IdMap, Interner, Topology, Value};
    use std::collections::BTreeMap;

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

        fn edge(&mut self, etype: &str, src: u32, dst: u32, props: Vec<(&str, Value)>) {
            let et = self.syms.intern(etype);
            self.topo.add_edge(et, src, dst);
            for (f, v) in props {
                self.eprops.set(et, src, dst, f, v);
            }
        }

        fn view(&self) -> GraphView<'_> {
            GraphView {
                ids: &self.ids,
                syms: &self.syms,
                labels: &self.labels,
                props: &self.props,
                topo: &self.topo,
                edge_props: &self.eprops,
            }
        }
    }

    fn compile(src: &str) -> Vec<PlanOp> {
        plan(&parse(&lex(src).expect("lex")).expect("parse")).expect("plan")
    }

    fn run(
        view: &GraphView,
        src: &str,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResultSet, String> {
        execute(view, &compile(src), &Params(params))
    }

    fn s(v: &str) -> Value {
        Value::Str(v.into())
    }

    fn f(v: f64) -> Value {
        Value::Float(v)
    }

    fn i(v: i64) -> Value {
        Value::Int(v)
    }

    fn rows_of(rs: &ResultSet) -> Vec<Vec<Option<Value>>> {
        (0..rs.len()).map(|i| rs.row(i).to_vec()).collect()
    }

    fn col(rs: &ResultSet, name: &str) -> Vec<Option<Value>> {
        (0..rs.len()).map(|i| rs.get(i, name).cloned()).collect()
    }

    fn hop_graph() -> Fx {
        let mut fx = Fx::new();
        let ada = fx.add("Person", "ada", vec![]);
        let bob = fx.add("Person", "bob", vec![]);
        let cam = fx.add("Person", "cam", vec![]);
        let acme = fx.add("Company", "acme", vec![]);
        fx.edge("KNOWS", ada, bob, vec![]);
        fx.edge("KNOWS", ada, cam, vec![]);
        fx.edge("KNOWS", bob, cam, vec![]);
        fx.edge("LIKES", ada, acme, vec![]);
        fx
    }

    fn undirected_graph() -> Fx {
        let mut fx = Fx::new();
        let a = fx.add("N", "a", vec![]);
        let b = fx.add("N", "b", vec![]);
        fx.edge("T", a, b, vec![("w", i(42))]);
        fx
    }

    fn triangle() -> Fx {
        let mut fx = Fx::new();
        let a = fx.add("N", "a", vec![]);
        let b = fx.add("N", "b", vec![]);
        let c = fx.add("N", "c", vec![]);
        fx.edge("T", a, b, vec![("eid", i(1))]);
        fx.edge("T", b, c, vec![("eid", i(2))]);
        fx.edge("T", c, a, vec![("eid", i(3))]);
        fx
    }

    fn single_edge() -> (Fx, u32, u32) {
        let mut fx = Fx::new();
        let a = fx.add("N", "a", vec![]);
        let b = fx.add("N", "b", vec![]);
        fx.edge("T", a, b, vec![]);
        (fx, a, b)
    }

    /// Companies with scored INDUSTRY_ALIGNMENT / SPECIALTY_MATCH edges to t1.
    fn dogfood_graph() -> Fx {
        let mut fx = Fx::new();
        let t1 = fx.add("Talent", "t1", vec![("id", s("t1"))]);
        let acme = fx.add("Company", "acme", vec![]);
        let beta = fx.add("Company", "beta", vec![]);
        let gamma = fx.add("Company", "gamma", vec![]);
        let delta = fx.add("Company", "delta", vec![]);
        let echo = fx.add("Company", "echo", vec![]);
        let foxtrot = fx.add("Company", "foxtrot", vec![]);
        let zeta = fx.add("Company", "zeta", vec![]);
        fx.edge("INDUSTRY_ALIGNMENT", acme, t1, vec![("score", f(0.9))]);
        fx.edge("SPECIALTY_MATCH", acme, t1, vec![("score", f(0.8))]);
        fx.edge("INDUSTRY_ALIGNMENT", beta, t1, vec![("score", f(0.6))]);
        fx.edge("SPECIALTY_MATCH", beta, t1, vec![("score", f(0.7))]);
        fx.edge("INDUSTRY_ALIGNMENT", gamma, t1, vec![("score", f(0.4))]);
        fx.edge("SPECIALTY_MATCH", gamma, t1, vec![("score", f(0.9))]);
        fx.edge("INDUSTRY_ALIGNMENT", delta, t1, vec![("score", f(0.8))]);
        fx.edge("SPECIALTY_MATCH", delta, t1, vec![("score", f(0.3))]);
        fx.edge("INDUSTRY_ALIGNMENT", echo, t1, vec![("score", f(0.5))]);
        fx.edge("SPECIALTY_MATCH", echo, t1, vec![("score", f(0.5))]);
        fx.edge("INDUSTRY_ALIGNMENT", foxtrot, t1, vec![("score", f(0.95))]);
        fx.edge("INDUSTRY_ALIGNMENT", zeta, t1, vec![("score", f(0.9))]);
        fx.edge("SPECIALTY_MATCH", zeta, t1, vec![("score", f(0.6))]);
        fx
    }

    const DOGFOOD: &str = "\
MATCH (t:Talent {id: $tid}) \
MATCH (c:Company)-[i:INDUSTRY_ALIGNMENT]->(t) \
MATCH (c)-[s:SPECIALTY_MATCH]->(t) \
WHERE i.score >= 0.5 AND s.score >= 0.5 \
RETURN c, i.score AS industry, s.score AS specialty \
ORDER BY industry DESC, specialty DESC \
LIMIT 10";

    fn tid_params() -> BTreeMap<String, Value> {
        let mut p = BTreeMap::new();
        p.insert("tid".into(), s("t1"));
        p
    }

    #[test]
    fn single_hop_match_label_and_etype_filters() {
        let fx = hop_graph();
        let v = fx.view();
        let rs = run(
            &v,
            "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a, b",
            &BTreeMap::new(),
        )
        .expect("single-hop");
        assert_eq!(rs.columns(), &["a".to_string(), "b".to_string()]);
        assert_eq!(
            rows_of(&rs),
            vec![
                vec![Some(s("ada")), Some(s("bob"))],
                vec![Some(s("ada")), Some(s("cam"))],
                vec![Some(s("bob")), Some(s("cam"))],
            ]
        );
        // dest label filters out ada -LIKES-> acme; etype filters it too
        let likes = run(
            &v,
            "MATCH (a:Person)-[:LIKES]->(b:Company) RETURN a, b",
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(rows_of(&likes), vec![vec![Some(s("ada")), Some(s("acme"))]]);
        let no_combo = run(
            &v,
            "MATCH (a:Person)-[:KNOWS]->(b:Company) RETURN a, b",
            &BTreeMap::new(),
        )
        .unwrap();
        assert!(no_combo.is_empty());
    }

    #[test]
    fn undirected_match_finds_both_orientations_and_binds_true_triple() {
        let fx = undirected_graph();
        let v = fx.view();
        // w lives only on the true (T, a, b) triple. A reversed (T, b, a)
        // binding would project None.
        let rs =
            run(&v, "MATCH (x)-[r:T]-(y) RETURN x, y, r.w", &BTreeMap::new()).expect("undirected");
        assert_eq!(
            rows_of(&rs),
            vec![
                vec![Some(s("a")), Some(s("b")), Some(i(42))],
                vec![Some(s("b")), Some(s("a")), Some(i(42))],
            ]
        );

        let left = run(
            &v,
            "MATCH (y)<-[r:T]-(x) RETURN x, y, r.w",
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(
            rows_of(&left),
            vec![vec![Some(s("a")), Some(s("b")), Some(i(42))]]
        );
    }

    #[test]
    fn relationship_uniqueness_triangle_and_two_hop_cycle() {
        let tri = triangle();
        let v = tri.view();
        let rs = run(
            &v,
            "MATCH (x)-[r1:T]->(y)-[r2:T]->(z) RETURN x, y, z, r1.eid, r2.eid",
            &BTreeMap::new(),
        )
        .expect("triangle 2-hop");
        assert_eq!(
            rows_of(&rs),
            vec![
                vec![
                    Some(s("a")),
                    Some(s("b")),
                    Some(s("c")),
                    Some(i(1)),
                    Some(i(2))
                ],
                vec![
                    Some(s("b")),
                    Some(s("c")),
                    Some(s("a")),
                    Some(i(2)),
                    Some(i(3))
                ],
                vec![
                    Some(s("c")),
                    Some(s("a")),
                    Some(s("b")),
                    Some(i(3)),
                    Some(i(1))
                ],
            ]
        );
        for row in rows_of(&rs) {
            assert_ne!(row[3], row[4], "r1 must never bind the same edge as r2");
        }

        let (mut one, a, b) = single_edge();
        let v = one.view();
        let cycle = run(
            &v,
            "MATCH (x)-[:T]->(y)-[:T]->(x) RETURN x",
            &BTreeMap::new(),
        )
        .expect("2-hop cycle");
        assert!(
            cycle.is_empty(),
            "single directed edge cannot close a 2-hop cycle"
        );

        // Undirected 2-hop back would reuse the only EdgeRef without uniqueness.
        let undirected_cycle = run(&v, "MATCH (x)-[:T]-(y)-[:T]-(x) RETURN x", &BTreeMap::new())
            .expect("undirected uniqueness");
        assert!(
            undirected_cycle.is_empty(),
            "relationship uniqueness must reject walking the same triple back"
        );

        one.edge("T", b, a, vec![]);
        let v = one.view();
        let with_recip = run(
            &v,
            "MATCH (x)-[:T]->(y)-[:T]->(x) RETURN x",
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(col(&with_recip, "x"), vec![Some(s("a")), Some(s("b"))]);
    }

    #[test]
    fn multi_match_join_bound_and_bound_destination_expand() {
        let fx = dogfood_graph();
        let v = fx.view();
        // No WHERE: any company with *both* edge types into the bound talent.
        // foxtrot has industry only → dropped by the second (bound-dest) expand.
        let rs = run(
            &v,
            "MATCH (t:Talent {id: $tid}) \
             MATCH (c:Company)-[i:INDUSTRY_ALIGNMENT]->(t) \
             MATCH (c)-[s:SPECIALTY_MATCH]->(t) \
             RETURN c",
            &tid_params(),
        )
        .expect("join + bound dest");
        assert_eq!(
            col(&rs, "c"),
            vec![
                Some(s("acme")),
                Some(s("beta")),
                Some(s("gamma")),
                Some(s("delta")),
                Some(s("echo")),
                Some(s("zeta")),
            ]
        );

        // Empty JoinBound (MATCH 2 start already bound, no label/props) keeps all.
        let keep = run(&v, "MATCH (c:Company) MATCH (c) RETURN c", &BTreeMap::new()).unwrap();
        assert_eq!(keep.len(), 7);
        // Label re-check on JoinBound drops everything.
        let drop = run(
            &v,
            "MATCH (c:Company) MATCH (c:Talent) RETURN c",
            &BTreeMap::new(),
        )
        .unwrap();
        assert!(drop.is_empty());
    }

    #[test]
    fn rel_var_edge_prop_filter() {
        let mut fx = Fx::new();
        let a = fx.add("N", "a", vec![]);
        let b = fx.add("N", "b", vec![]);
        let c = fx.add("N", "c", vec![]);
        fx.edge("T", a, b, vec![("w", f(0.7))]);
        fx.edge("T", a, c, vec![("w", f(0.3))]);
        let v = fx.view();
        let rs = run(
            &v,
            "MATCH (x)-[r:T]->(y) WHERE r.w >= 0.5 RETURN y, r.w",
            &BTreeMap::new(),
        )
        .expect("edge-prop filter");
        assert_eq!(rows_of(&rs), vec![vec![Some(s("b")), Some(f(0.7))]]);
        let fail = run(
            &v,
            "MATCH (x)-[r:T]->(y) WHERE r.w >= 0.8 RETURN y",
            &BTreeMap::new(),
        )
        .unwrap();
        assert!(fail.is_empty());
    }

    #[test]
    fn params_present_resolve_missing_is_err_before_rows() {
        let fx = dogfood_graph();
        let v = fx.view();
        let hit =
            run(&v, "MATCH (t:Talent {id: $tid}) RETURN t", &tid_params()).expect("present param");
        assert_eq!(col(&hit, "t"), vec![Some(s("t1"))]);

        // Unknown label would yield Ok(empty) if params were not walked first.
        let err = run(
            &v,
            "MATCH (t:NoSuchLabel {id: $tid}) RETURN t",
            &BTreeMap::new(),
        )
        .expect_err("missing param must be Err, not Ok(empty)");
        assert!(
            err.contains("tid")
                && (err.contains("param") || err.contains("Param") || err.contains("missing")),
            "missing-param error must name the parameter, got: {err}"
        );

        let err = run(
            &v,
            "MATCH (t:Talent) WHERE t.id = $tid RETURN t",
            &BTreeMap::new(),
        )
        .expect_err("missing WHERE param");
        assert!(err.contains("tid"), "got: {err}");
    }

    #[test]
    fn order_by_none_last_then_skip_limit() {
        let mut fx = Fx::new();
        fx.add("Person", "ada", vec![("age", i(30))]);
        fx.add("Person", "bob", vec![]); // missing age → None
        fx.add("Person", "cam", vec![("age", i(10))]);
        fx.add("Person", "dan", vec![("age", i(20))]);
        let v = fx.view();

        let asc = run(
            &v,
            "MATCH (p:Person) RETURN p, p.age AS age ORDER BY age",
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(
            rows_of(&asc),
            vec![
                vec![Some(s("cam")), Some(i(10))],
                vec![Some(s("dan")), Some(i(20))],
                vec![Some(s("ada")), Some(i(30))],
                vec![Some(s("bob")), None],
            ]
        );

        let desc = run(
            &v,
            "MATCH (p:Person) RETURN p, p.age AS age ORDER BY age DESC",
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(
            rows_of(&desc),
            vec![
                vec![Some(s("ada")), Some(i(30))],
                vec![Some(s("dan")), Some(i(20))],
                vec![Some(s("cam")), Some(i(10))],
                vec![Some(s("bob")), None],
            ]
        );

        let skip_lim = run(
            &v,
            "MATCH (p:Person) RETURN p, p.age AS age ORDER BY age SKIP 1 LIMIT 2",
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(
            rows_of(&skip_lim),
            vec![
                vec![Some(s("dan")), Some(i(20))],
                vec![Some(s("ada")), Some(i(30))],
            ]
        );

        let desc_sl = run(
            &v,
            "MATCH (p:Person) RETURN p, p.age AS age ORDER BY age DESC SKIP 1 LIMIT 2",
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(
            rows_of(&desc_sl),
            vec![
                vec![Some(s("dan")), Some(i(20))],
                vec![Some(s("cam")), Some(i(10))],
            ]
        );
    }

    #[test]
    fn unknown_label_and_etype_are_ok_empty() {
        let fx = hop_graph();
        let v = fx.view();
        let lab = run(&v, "MATCH (x:Nope) RETURN x", &BTreeMap::new()).expect("unknown label");
        assert!(lab.is_empty());
        let et = run(
            &v,
            "MATCH (a)-[:NO_SUCH_ETYPE]->(b) RETURN a",
            &BTreeMap::new(),
        )
        .expect("unknown etype");
        assert!(et.is_empty());
    }

    #[test]
    fn execute_is_deterministic() {
        let fx = dogfood_graph();
        let v = fx.view();
        let p = tid_params();
        let a = run(&v, DOGFOOD, &p).expect("first");
        let b = run(&v, DOGFOOD, &p).expect("second");
        assert_eq!(a, b);
        let hop = hop_graph();
        let hv = hop.view();
        let q = "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a, b";
        assert_eq!(run(&hv, q, &BTreeMap::new()), run(&hv, q, &BTreeMap::new()));
    }

    #[test]
    fn dogfood_pipeline_exact_rows() {
        let fx = dogfood_graph();
        let v = fx.view();
        let rs = run(&v, DOGFOOD, &tid_params()).expect("dogfood");
        assert_eq!(
            rs.columns(),
            &[
                "c".to_string(),
                "industry".to_string(),
                "specialty".to_string()
            ]
        );
        // industry DESC, specialty DESC; gamma (0.4) / delta (0.3) / foxtrot (no s) out.
        assert_eq!(
            rows_of(&rs),
            vec![
                vec![Some(s("acme")), Some(f(0.9)), Some(f(0.8))],
                vec![Some(s("zeta")), Some(f(0.9)), Some(f(0.6))],
                vec![Some(s("beta")), Some(f(0.6)), Some(f(0.7))],
                vec![Some(s("echo")), Some(f(0.5)), Some(f(0.5))],
            ]
        );
    }

    #[test]
    fn unknown_var_in_op_is_err_not_panic() {
        let fx = hop_graph();
        let v = fx.view();
        let plan = vec![
            PlanOp::ScanLabel {
                var: "a".into(),
                label: None,
            },
            PlanOp::Expand {
                from: "zzz".into(),
                rel_var: Some("r".into()),
                etype: None,
                dir: RelDir::Right,
                to: "b".into(),
                to_label: None,
                to_props: vec![],
            },
            PlanOp::Project {
                items: vec![RetItem {
                    value: RetVal::Var("a".into()),
                    alias: None,
                }],
            },
        ];
        let params = BTreeMap::new();
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            execute(&v, &plan, &Params(&params))
        }));
        assert!(caught.is_ok(), "execute panicked on unknown var");
        let err = caught.unwrap().expect_err("unknown var must be Err");
        assert!(
            err.contains("zzz") && err.to_ascii_lowercase().contains("unbound"),
            "got: {err}"
        );

        let join = vec![
            PlanOp::JoinBound {
                var: "ghost".into(),
                label: None,
                props: vec![],
            },
            PlanOp::Project {
                items: vec![RetItem {
                    value: RetVal::Var("ghost".into()),
                    alias: None,
                }],
            },
        ];
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            execute(&v, &join, &Params(&params))
        }));
        assert!(caught.is_ok(), "execute panicked on JoinBound unknown var");
        assert!(caught.unwrap().is_err());
    }

    #[test]
    fn dest_props_on_bound_expand_are_applied() {
        let fx = dogfood_graph();
        let v = fx.view();
        let rs = run(
            &v,
            "MATCH (t:Talent {id: $tid}) \
             MATCH (c:Company)-[r:INDUSTRY_ALIGNMENT]->(t:Talent {id: $tid}) \
             RETURN c",
            &tid_params(),
        )
        .unwrap();
        // foxtrot included (has industry); companies without the edge are not.
        assert_eq!(
            col(&rs, "c"),
            vec![
                Some(s("acme")),
                Some(s("beta")),
                Some(s("gamma")),
                Some(s("delta")),
                Some(s("echo")),
                Some(s("foxtrot")),
                Some(s("zeta")),
            ]
        );
        let miss = run(
            &v,
            "MATCH (t:Talent {id: $tid}) \
             MATCH (c:Company)-[r:INDUSTRY_ALIGNMENT]->(t {id: 'nope'}) \
             RETURN c",
            &tid_params(),
        )
        .unwrap();
        assert!(miss.is_empty());
    }

    #[test]
    fn missing_node_prop_projects_none_and_pattern_misses() {
        let mut fx = Fx::new();
        fx.add("Person", "ada", vec![("age", i(30))]);
        fx.add("Person", "bob", vec![]);
        let v = fx.view();
        let rs = run(&v, "MATCH (p:Person) RETURN p.age", &BTreeMap::new()).unwrap();
        assert_eq!(col(&rs, "p.age"), vec![Some(i(30)), None]);
        let pat = run(&v, "MATCH (p:Person {age: 30}) RETURN p", &BTreeMap::new()).unwrap();
        assert_eq!(col(&pat, "p"), vec![Some(s("ada"))]);
    }

    #[test]
    fn execute_never_panics_on_hostile_plans() {
        let fx = hop_graph();
        let v = fx.view();
        let params = BTreeMap::new();
        let hostile = vec![
            vec![],
            vec![PlanOp::Project { items: vec![] }],
            vec![PlanOp::OrderBy {
                items: vec![OrderItem {
                    target: OrderTarget::Alias("nope".into()),
                    descending: false,
                }],
            }],
            vec![PlanOp::Filter {
                expr: crate::cypher::ast::Expr::Cmp {
                    lhs: Operand::Prop {
                        var: "missing".into(),
                        field: "x".into(),
                    },
                    op: crate::filter::CmpOp::Eq,
                    rhs: Operand::Lit(i(1)),
                },
            }],
            vec![
                PlanOp::ScanLabel {
                    var: "a".into(),
                    label: None,
                },
                PlanOp::LookupProps {
                    var: "zzz".into(),
                    props: vec![("k".into(), Operand::Lit(i(1)))],
                },
            ],
            vec![PlanOp::Skip(99), PlanOp::Limit(0)],
        ];
        for plan in hostile {
            let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                execute(&v, &plan, &Params(&params))
            }));
            assert!(caught.is_ok(), "execute panicked on hostile plan: {plan:?}");
        }
    }
}
