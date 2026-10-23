//! Graph elements behind result columns (TX-NS).
//!
//! A RETURN of a bare node, relationship or path variable renders, as it
//! always has, a value every surface can carry: a node is its key, a path is
//! a `{nodes, relationships, length}` map, and a relationship is the
//! `{type, start, end}` map a path's relationship already is. Two things ride
//! beside those values on the [`ResultSet`]:
//!
//! - the [`EntityKind`] of each such column, worked out from the plan by
//!   [`output_kinds`] and always set (it costs one pass over the plan); and
//! - the [`Entity`] behind each of its cells — label and properties of a
//!   node, type, ends and properties of a relationship, the elements of a
//!   path — read from the same view the query ran on. These are built only
//!   when the caller asked, by running the query inside
//!   [`with_entity_detail`]; the engine's query entry points call
//!   [`attach_requested_entities`] on the result they return.
//!
//! The switch is a scoped, per-thread flag rather than a parameter because
//! every query entry point (a read, a masked read, a write's `RETURN`, a
//! statement inside a transaction, a reader snapshot) runs synchronously on
//! its caller's thread and builds its view inside the call; one flag reaches
//! all of them without a second copy of each signature. A caller that never
//! sets it — HTTP, Python, the CLI — gets results byte-identical to before.

use crate::cypher::ast::{ret_val_label, AggArg, AggFunc, Operand, RetItem, RetVal};
use crate::cypher::plan::PlanOp;
use crate::result::{Entity, EntityKind, NodeEntity, PathEntity, RelationshipEntity, ResultSet};
use crate::view::GraphView;
use core_storage::Value;
use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};

thread_local! {
    static ENTITY_DETAIL: Cell<bool> = const { Cell::new(false) };
}

/// Run `f` with entity details requested on this thread: every query result
/// an engine entry point returns inside `f` carries the [`Entity`] behind each
/// bare node, relationship and path cell. The previous setting is restored
/// when `f` returns or panics.
pub fn with_entity_detail<R>(f: impl FnOnce() -> R) -> R {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            ENTITY_DETAIL.with(|c| c.set(self.0));
        }
    }
    let _restore = Restore(ENTITY_DETAIL.with(|c| c.replace(true)));
    f()
}

/// Whether the current thread is inside [`with_entity_detail`].
pub fn entity_detail_requested() -> bool {
    ENTITY_DETAIL.with(|c| c.get())
}

/// [`attach_entities`] when the current thread asked for details, else
/// nothing.
pub fn attach_requested_entities(view: &GraphView, rs: &mut ResultSet) {
    if entity_detail_requested() {
        attach_entities(view, rs);
    }
}

/// Attach the graph element behind every cell of `rs`'s node, relationship
/// and path columns, read from `view` (the view the query ran on, so a
/// role's mask applies). A result with no such column is left alone. A cell
/// that does not resolve — a null, or a value that is not what its column's
/// kind says — gets no entity and is sent as its value.
pub fn attach_entities(view: &GraphView, rs: &mut ResultSet) {
    let kinds = rs.column_kinds().to_vec();
    if kinds.iter().all(Option::is_none) {
        return;
    }
    let width = rs.columns().len();
    let mut cache = NodeCache::default();
    let mut entities = Vec::with_capacity(rs.len() * width);
    for row in 0..rs.len() {
        for (col, cell) in rs.row(row).iter().enumerate() {
            let entity = match (kinds.get(col).copied().flatten(), cell) {
                (Some(kind), Some(value)) => resolve(view, kind, value, &mut cache),
                _ => None,
            };
            entities.push(entity);
        }
    }
    rs.set_entities(entities);
}

/// Nodes already read for this result: a node that appears in many rows (a
/// path's shared end, a grouped key) is read once.
#[derive(Default)]
struct NodeCache {
    nodes: HashMap<String, Option<NodeEntity>>,
}

impl NodeCache {
    fn node(&mut self, view: &GraphView, key: &str) -> Option<NodeEntity> {
        if let Some(hit) = self.nodes.get(key) {
            return hit.clone();
        }
        let found = view
            .node_id(key)
            .filter(|id| view.visible(*id))
            .map(|id| NodeEntity {
                id,
                key: key.to_owned(),
                label: view.label_of(id).unwrap_or_default().to_owned(),
                properties: view.props.props_of(id),
            });
        self.nodes.insert(key.to_owned(), found.clone());
        found
    }
}

fn resolve(
    view: &GraphView,
    kind: EntityKind,
    value: &Value,
    cache: &mut NodeCache,
) -> Option<Entity> {
    match kind {
        EntityKind::Node => match value {
            Value::Str(key) => cache.node(view, key).map(Entity::Node),
            _ => None,
        },
        EntityKind::Relationship => relationship(view, value, cache).map(Entity::Relationship),
        EntityKind::Path => path(view, value, cache).map(Entity::Path),
        EntityKind::NodeList | EntityKind::RelationshipList | EntityKind::PathList => {
            let (Value::List(items), Some(element)) = (value, kind.element()) else {
                return None;
            };
            Some(Entity::List(
                items
                    .iter()
                    .map(|item| resolve(view, element, item, cache))
                    .collect(),
            ))
        }
    }
}

fn text<'a>(map: &'a BTreeMap<String, Value>, field: &str) -> Option<&'a str> {
    match map.get(field) {
        Some(Value::Str(s)) => Some(s),
        _ => None,
    }
}

/// A `{type, start, end}` map, as a relationship column and a path render
/// it, read back into the relationship it names. Both ends must be visible.
fn relationship(
    view: &GraphView,
    value: &Value,
    cache: &mut NodeCache,
) -> Option<RelationshipEntity> {
    let Value::Map(map) = value else {
        return None;
    };
    let (rel_type, start, end) = (text(map, "type")?, text(map, "start")?, text(map, "end")?);
    cache.node(view, start)?;
    cache.node(view, end)?;
    let etype = view.sym(rel_type)?;
    let (src, dst) = (view.node_id(start)?, view.node_id(end)?);
    Some(RelationshipEntity {
        rel_type: rel_type.to_owned(),
        start: start.to_owned(),
        end: end.to_owned(),
        start_id: src,
        end_id: dst,
        properties: view.edge_props.props_of(etype, src, dst),
    })
}

/// A `{nodes, relationships, length}` map read back into its elements.
fn path(view: &GraphView, value: &Value, cache: &mut NodeCache) -> Option<PathEntity> {
    let Value::Map(map) = value else {
        return None;
    };
    let (Some(Value::List(nodes)), Some(Value::List(rels))) =
        (map.get("nodes"), map.get("relationships"))
    else {
        return None;
    };
    if nodes.len() != rels.len() + 1 {
        return None;
    }
    let nodes = nodes
        .iter()
        .map(|n| match n {
            Value::Str(key) => cache.node(view, key),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    let relationships = rels
        .iter()
        .map(|r| relationship(view, r, cache))
        .collect::<Option<Vec<_>>>()?;
    Some(PathEntity {
        nodes,
        relationships,
    })
}

/// The [`EntityKind`] of each column `plan` returns: a RETURN item (or a
/// grouping key) that is a bare variable bound to a node, a relationship or a
/// named path, or a list of them: `collect(v)` of such a variable, a list
/// literal of variables of one kind (`[a, b]`), and a path's `.nodes` and
/// `.relationships`. Variables are followed through every clause that rebinds the
/// scope: a `WITH` keeps only its items (an alias of a bare variable keeps the
/// variable's kind), a grouping keeps its keys, an `UNWIND` alias is a value.
/// A variable-length relationship variable is no relationship (it renders as
/// its hop count).
pub(crate) fn output_kinds(plan: &[PlanOp]) -> Vec<Option<EntityKind>> {
    let mut scope: HashMap<String, EntityKind> = HashMap::new();
    let mut out: Vec<Option<EntityKind>> = Vec::new();
    bind_ops(plan, &mut scope, &mut out);
    out
}

fn bind_ops(
    plan: &[PlanOp],
    scope: &mut HashMap<String, EntityKind>,
    out: &mut Vec<Option<EntityKind>>,
) {
    let node = |scope: &mut HashMap<String, EntityKind>, var: &str| {
        scope.insert(var.to_owned(), EntityKind::Node);
    };
    for op in plan {
        match op {
            PlanOp::ScanLabel { var, .. }
            | PlanOp::ScanKey { var, .. }
            | PlanOp::IndexScan { var, .. }
            | PlanOp::IndexRangeScan { var, .. }
            | PlanOp::IndexIntersect { var, .. }
            | PlanOp::LookupProps { var, .. }
            | PlanOp::JoinBound { var, .. } => node(scope, var),
            PlanOp::Expand {
                from, rel_var, to, ..
            } => {
                node(scope, from);
                node(scope, to);
                if let Some(r) = rel_var {
                    scope.insert(r.clone(), EntityKind::Relationship);
                }
            }
            PlanOp::VarExpand {
                from,
                rel_var,
                to,
                trail_var,
                ..
            }
            | PlanOp::ShortestPath {
                from,
                rel_var,
                to,
                trail_var,
                ..
            } => {
                node(scope, from);
                node(scope, to);
                for hidden in rel_var.iter().chain(trail_var) {
                    scope.remove(hidden);
                }
            }
            PlanOp::BindPath { path_var, .. } => {
                scope.insert(path_var.clone(), EntityKind::Path);
            }
            PlanOp::With { items, .. } => {
                *scope = carried(items.iter(), scope);
            }
            PlanOp::GroupAggregate { keys, aggs } => {
                let key_kinds: Vec<Option<EntityKind>> = keys
                    .iter()
                    .map(|(_, item)| item_kind(item, scope))
                    .collect();
                let agg_kinds: Vec<Option<EntityKind>> = aggs
                    .iter()
                    .map(|(func, arg, _)| agg_kind(func, arg, scope))
                    .collect();
                let mut next: HashMap<String, EntityKind> = keys
                    .iter()
                    .map(|(col, _)| col)
                    .chain(aggs.iter().map(|(_, _, col)| col))
                    .zip(key_kinds.iter().chain(&agg_kinds))
                    .filter_map(|(col, kind)| kind.map(|k| (col.clone(), k)))
                    .collect();
                std::mem::swap(scope, &mut next);
                *out = key_kinds;
                out.extend(agg_kinds);
            }
            PlanOp::Unwind { alias, .. } => {
                scope.remove(alias);
            }
            PlanOp::LeftOuterApply { inner, .. } => {
                let mut ignored = Vec::new();
                bind_ops(inner, scope, &mut ignored);
            }
            PlanOp::Project { items } => {
                *out = items.iter().map(|item| item_kind(item, scope)).collect();
            }
            PlanOp::Aggregate { func, arg, .. } => *out = vec![agg_kind(func, arg, scope)],
            PlanOp::Filter { .. }
            | PlanOp::Distinct
            | PlanOp::OrderBy { .. }
            | PlanOp::Skip(_)
            | PlanOp::Limit(_) => {}
        }
    }
}

/// The scope a `WITH` leaves: its items, under their column names, each with
/// the kind of the bare variable it carries.
fn carried<'a>(
    items: impl Iterator<Item = &'a RetItem>,
    scope: &HashMap<String, EntityKind>,
) -> HashMap<String, EntityKind> {
    items
        .filter_map(|item| {
            let kind = item_kind(item, scope)?;
            let col = item.alias.clone().or_else(|| ret_val_label(&item.value))?;
            Some((col, kind))
        })
        .collect()
}

fn item_kind(item: &RetItem, scope: &HashMap<String, EntityKind>) -> Option<EntityKind> {
    match &item.value {
        RetVal::Prop { var, field } => path_part_kind(var, field, scope),
        RetVal::Agg { func, arg } => agg_kind(func, arg, scope),
        other => operand_kind(&ret_val_operand(other)?, scope),
    }
}

/// `RetVal` as the operand it computes, for the forms that have a kind.
fn ret_val_operand(value: &RetVal) -> Option<Operand> {
    match value {
        RetVal::Var(v) => Some(Operand::Var(v.clone())),
        RetVal::ScalarExpr(op) => Some(op.clone()),
        _ => None,
    }
}

fn operand_kind(op: &Operand, scope: &HashMap<String, EntityKind>) -> Option<EntityKind> {
    match op {
        Operand::Var(v) => scope.get(v).copied(),
        Operand::Prop { var, field } => path_part_kind(var, field, scope),
        // `[a, b]`: a list of variables that all hold elements of one kind.
        Operand::ListLit(items) if !items.is_empty() => {
            let mut kinds = items.iter().map(|item| match item {
                Operand::Var(v) => scope.get(v).copied(),
                _ => None,
            });
            let first = kinds.next()??;
            kinds
                .all(|k| k == Some(first))
                .then(|| first.list_of())
                .flatten()
        }
        _ => None,
    }
}

/// `p.nodes` and `p.relationships` of a path variable.
fn path_part_kind(
    var: &str,
    field: &str,
    scope: &HashMap<String, EntityKind>,
) -> Option<EntityKind> {
    match (scope.get(var), field) {
        (Some(EntityKind::Path), "nodes") => Some(EntityKind::NodeList),
        (Some(EntityKind::Path), "relationships") => Some(EntityKind::RelationshipList),
        _ => None,
    }
}

/// `collect(v)` / `collect(DISTINCT v)` of a node, relationship or path
/// variable is a list of them; no other aggregate has a kind.
fn agg_kind(
    func: &AggFunc,
    arg: &AggArg,
    scope: &HashMap<String, EntityKind>,
) -> Option<EntityKind> {
    if !matches!(func, AggFunc::Collect) {
        return None;
    }
    match arg {
        AggArg::Var(v) => scope.get(v)?.list_of(),
        AggArg::Distinct(inner) => agg_kind(func, inner, scope),
        _ => None,
    }
}

/// The kinds of a UNION's columns: a column keeps a kind only when every part
/// gave it the same one.
pub(crate) fn merge_kinds(
    acc: &[Option<EntityKind>],
    next: &[Option<EntityKind>],
) -> Vec<Option<EntityKind>> {
    acc.iter()
        .zip(next)
        .map(|(a, b)| if a == b { *a } else { None })
        .collect()
}
