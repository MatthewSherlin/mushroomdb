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
use crate::result::{
    Entity, EntityKind, NodeEntity, PathEntity, RelationshipEntity, ResultSet, ValueEntityTree,
    ValueKindTag,
};
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
///
/// Also fills value entity trees for columns that have a [`ValueKindTag`]
/// (embedded entities inside maps and mixed lists).
pub fn attach_entities(view: &GraphView, rs: &mut ResultSet) {
    let kinds = rs.column_kinds().to_vec();
    let has_flat_kinds = kinds.iter().any(Option::is_some);
    let has_kind_trees = rs.has_value_kind_trees();
    if !has_flat_kinds && !has_kind_trees {
        return;
    }
    let width = rs.columns().len();
    let mut cache = NodeCache::default();
    // Collect kind trees once (to avoid re-borrowing rs during the loops).
    let kind_trees: Vec<Option<ValueKindTag>> = (0..width)
        .map(|col| rs.value_kind_tree(col).cloned())
        .collect();
    let mut entities = Vec::with_capacity(rs.len() * width);
    let mut value_trees: Vec<Option<ValueEntityTree>> = if has_kind_trees {
        Vec::with_capacity(rs.len() * width)
    } else {
        Vec::new()
    };
    for row in 0..rs.len() {
        for (col, cell) in rs.row(row).iter().enumerate() {
            let entity = match (kinds.get(col).copied().flatten(), cell) {
                (Some(kind), Some(value)) => resolve(view, kind, value, &mut cache),
                _ => None,
            };
            entities.push(entity);
            if has_kind_trees {
                let tree = match (kind_trees.get(col).and_then(|t| t.as_ref()), cell) {
                    (Some(tag), Some(value)) => {
                        resolve_value_entity_tree(view, tag, value, &mut cache)
                    }
                    _ => None,
                };
                value_trees.push(tree);
            }
        }
    }
    rs.set_entities(entities);
    if has_kind_trees {
        rs.set_value_entity_trees(value_trees);
    }
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

/// Resolve a [`ValueEntityTree`] from a cell value using its [`ValueKindTag`].
/// Returns `None` when the value does not match the expected shape (null cell,
/// type mismatch). The cache is shared with flat entity resolution so nodes
/// read once per result.
fn resolve_value_entity_tree(
    view: &GraphView,
    tag: &ValueKindTag,
    value: &Value,
    cache: &mut NodeCache,
) -> Option<ValueEntityTree> {
    match tag {
        ValueKindTag::Node => match value {
            Value::Str(key) => cache.node(view, key).map(ValueEntityTree::Node),
            _ => None,
        },
        ValueKindTag::Relationship => {
            relationship(view, value, cache).map(ValueEntityTree::Relationship)
        }
        ValueKindTag::Path => path(view, value, cache).map(ValueEntityTree::Path),
        ValueKindTag::UniformList(elem_kind) => {
            let Value::List(items) = value else {
                return None;
            };
            let resolved: Vec<Option<ValueEntityTree>> = items
                .iter()
                .map(|item| {
                    resolve_value_entity_tree(view, &elem_kind_to_tag(*elem_kind), item, cache)
                })
                .collect();
            Some(ValueEntityTree::List(resolved))
        }
        ValueKindTag::MixedList(element_tags) => {
            let Value::List(items) = value else {
                return None;
            };
            let resolved: Vec<Option<ValueEntityTree>> = element_tags
                .iter()
                .zip(items.iter())
                .map(|(tag, item)| {
                    tag.as_ref()
                        .and_then(|t| resolve_value_entity_tree(view, t, item, cache))
                })
                .collect();
            Some(ValueEntityTree::List(resolved))
        }
        ValueKindTag::Map(field_tags) => {
            let Value::Map(map) = value else {
                return None;
            };
            let resolved: Vec<(String, ValueEntityTree)> = field_tags
                .iter()
                .filter_map(|(key, tag)| {
                    let v = map.get(key)?;
                    resolve_value_entity_tree(view, tag, v, cache).map(|tree| (key.clone(), tree))
                })
                .collect();
            if resolved.is_empty() {
                None
            } else {
                Some(ValueEntityTree::Map(resolved))
            }
        }
    }
}

/// Convert a flat `EntityKind` to its matching `ValueKindTag` atom or uniform
/// list variant (for use inside `resolve_value_entity_tree`).
fn elem_kind_to_tag(kind: EntityKind) -> ValueKindTag {
    match kind {
        EntityKind::Node => ValueKindTag::Node,
        EntityKind::Relationship => ValueKindTag::Relationship,
        EntityKind::Path => ValueKindTag::Path,
        EntityKind::NodeList => ValueKindTag::UniformList(EntityKind::Node),
        EntityKind::RelationshipList => ValueKindTag::UniformList(EntityKind::Relationship),
        EntityKind::PathList => ValueKindTag::UniformList(EntityKind::Path),
    }
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
        RetVal::FuncCall { name, args } => Some(Operand::FuncCall {
            name: name.clone(),
            args: args.clone(),
        }),
        _ => None,
    }
}

pub(crate) fn operand_kind(
    op: &Operand,
    scope: &HashMap<String, EntityKind>,
) -> Option<EntityKind> {
    match op {
        Operand::Var(v) => scope.get(v).copied(),
        Operand::Prop { var, field } => path_part_kind(var, field, scope),
        Operand::FuncCall { name, args } => func_kind(name, args, scope),
        // `[a, b]` or `[startNode(r), endNode(r)]`: a list of expressions all of
        // the same entity kind produces a uniform list of that kind.
        Operand::ListLit(items) if !items.is_empty() => {
            let mut kinds = items.iter().map(|item| operand_kind(item, scope));
            let first = kinds.next()??;
            kinds
                .all(|k| k == Some(first))
                .then(|| first.list_of())
                .flatten()
        }
        // `nodes(p)[0]`, `collect(n)[i]`: subscript of a list of one entity kind
        // yields one element of that kind.
        Operand::Index { base, .. } => operand_kind(base, scope)?.element(),
        // `CASE WHEN cond THEN n ELSE m END`: entity kind when every non-null
        // branch returns the same entity kind. A null-literal branch
        // (`ELSE NULL`) is a wildcard and does not disqualify; a branch with an
        // unknown/scalar kind (not a null literal) does.
        Operand::Case { branches, default } => {
            let is_null_lit = |op: &Operand| matches!(op, Operand::Lit(Value::Null));
            let branch_kinds: Vec<Option<EntityKind>> = branches
                .iter()
                .filter(|(_, op)| !is_null_lit(op))
                .map(|(_, op)| operand_kind(op, scope))
                .chain(
                    default
                        .iter()
                        .filter(|d| !is_null_lit(d))
                        .map(|d| operand_kind(d, scope)),
                )
                .collect();
            let first = branch_kinds.iter().flatten().next().copied()?;
            branch_kinds
                .iter()
                .all(|k| *k == Some(first))
                .then_some(first)
        }
        _ => None,
    }
}

/// `nodes(p)` and `relationships(p)` of a path variable are what `p.nodes` and
/// `p.relationships` are; `startNode(r)` and `endNode(r)` of a relationship
/// variable are nodes; `head`/`last` of a list of entities yield one element;
/// `coalesce` of arguments that are all the same entity kind yields that kind.
pub(crate) fn func_kind(
    name: &str,
    args: &[Operand],
    scope: &HashMap<String, EntityKind>,
) -> Option<EntityKind> {
    let norm = name.to_ascii_lowercase();
    // `head(x)` / `last(x)` where x is any expression with a list entity kind.
    if (norm == "head" || norm == "last") && args.len() == 1 {
        return operand_kind(&args[0], scope)?.element();
    }
    // `coalesce(a, b, …)`: entity kind when all non-null args are the same kind.
    if norm == "coalesce" && !args.is_empty() {
        let non_null_kinds: Vec<Option<EntityKind>> = args
            .iter()
            .filter(|a| !matches!(a, Operand::Lit(Value::Null)))
            .map(|a| operand_kind(a, scope))
            .collect();
        if !non_null_kinds.is_empty() {
            let first = non_null_kinds.iter().flatten().next().copied()?;
            if non_null_kinds.iter().all(|k| *k == Some(first)) {
                return Some(first);
            }
        }
        return None;
    }
    // Single-variable forms.
    let [Operand::Var(var)] = args else {
        return None;
    };
    match (norm.as_str(), scope.get(var)) {
        ("nodes", Some(EntityKind::Path)) => Some(EntityKind::NodeList),
        ("relationships", Some(EntityKind::Path)) => Some(EntityKind::RelationshipList),
        ("startnode" | "endnode", Some(EntityKind::Relationship)) => Some(EntityKind::Node),
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

/// The [`ValueKindTag`] trees of two `UNION` parts, merged like
/// [`merge_kinds`]: a column keeps its tree only when both parts agree, so
/// one part's plain value is never read as the other part's graph element.
pub(crate) fn merge_value_kind_trees(
    acc: &ResultSet,
    next: &ResultSet,
) -> Vec<Option<ValueKindTag>> {
    (0..acc.columns().len())
        .map(
            |col| match (acc.value_kind_tree(col), next.value_kind_tree(col)) {
                (Some(a), Some(b)) if a == b => Some(a.clone()),
                _ => None,
            },
        )
        .collect()
}

/// The entity kinds of ALL variables that are live after the plan finishes,
/// following `WITH` projections. Used by the executor to annotate its variable
/// table so `elementId()` can resolve element ids for aliased node/relationship
/// expressions (e.g. `WITH startNode(r) AS a RETURN elementId(a)`).
pub(crate) fn all_var_kinds(plan: &[PlanOp]) -> HashMap<String, EntityKind> {
    let mut scope: HashMap<String, EntityKind> = HashMap::new();
    let mut ignored = Vec::new();
    bind_ops(plan, &mut scope, &mut ignored);
    scope
}

/// Build a [`ValueKindTag`] tree for an operand that may contain graph
/// elements nested inside a map literal or a heterogeneous list literal.
/// Returns `None` for operands whose entity content is already fully described
/// by a flat [`EntityKind`] (uniform lists, bare variables, functions that
/// return a single entity), since those are handled by the existing mechanism.
fn operand_kind_tree(op: &Operand, scope: &HashMap<String, EntityKind>) -> Option<ValueKindTag> {
    match op {
        // A map literal is a Tier-2 case whenever at least one field holds an entity.
        Operand::MapLit(fields) => {
            let entity_fields: Vec<(String, ValueKindTag)> = fields
                .iter()
                .filter_map(|(k, v)| operand_kind_tree(v, scope).map(|t| (k.clone(), t)))
                .collect();
            if entity_fields.is_empty() {
                None
            } else {
                Some(ValueKindTag::Map(entity_fields))
            }
        }
        // A list literal with heterogeneous or partial entity content.
        Operand::ListLit(items) if !items.is_empty() => {
            // If the flat mechanism already handles this (uniform list of one entity
            // kind), skip it — no Tier-2 tag needed.
            if operand_kind(op, scope).is_some() {
                return None;
            }
            let element_tags: Vec<Option<ValueKindTag>> = items
                .iter()
                .map(|item| operand_kind_tree(item, scope))
                .collect();
            if element_tags.iter().any(Option::is_some) {
                Some(ValueKindTag::MixedList(element_tags))
            } else {
                None
            }
        }
        // Other operands: convert a flat entity kind to its ValueKindTag atom,
        // but only for single-entity or uniform-list kinds that need to appear
        // *nested* inside a larger structure (e.g. a list of node-lists).
        other => {
            let kind = operand_kind(other, scope)?;
            match kind {
                EntityKind::Node => Some(ValueKindTag::Node),
                EntityKind::Relationship => Some(ValueKindTag::Relationship),
                EntityKind::Path => Some(ValueKindTag::Path),
                EntityKind::NodeList => Some(ValueKindTag::UniformList(EntityKind::Node)),
                EntityKind::RelationshipList => {
                    Some(ValueKindTag::UniformList(EntityKind::Relationship))
                }
                EntityKind::PathList => Some(ValueKindTag::UniformList(EntityKind::Path)),
            }
        }
    }
}

/// Set on `rs` the [`ValueKindTag`] trees of a write statement's `RETURN`
/// (`MATCH … SET … RETURN`), which has no read plan to work them out from:
/// `scope` holds the kinds of the variables its `MATCH` bound. A column `rs`
/// already gives a flat kind gets no tree, as in a read.
pub fn set_write_return_kind_trees(
    rs: &mut ResultSet,
    returns: &[RetItem],
    scope: &HashMap<String, EntityKind>,
) {
    let trees = returns
        .iter()
        .enumerate()
        .map(|(col, item)| {
            if rs.column_kind(col).is_some() {
                return None;
            }
            match &item.value {
                RetVal::ScalarExpr(op) => operand_kind_tree(op, scope),
                RetVal::FuncCall { name, args } => operand_kind_tree(
                    &Operand::FuncCall {
                        name: name.clone(),
                        args: args.clone(),
                    },
                    scope,
                ),
                _ => None,
            }
        })
        .collect();
    rs.set_value_kind_trees(trees);
}

/// Per-column [`ValueKindTag`] trees for `plan`'s output: only filled for
/// columns that have embedded graph elements in maps or heterogeneous lists
/// (i.e. where the flat [`EntityKind`] mechanism does not apply). Columns that
/// already have a flat [`EntityKind`] get `None` here.
pub(crate) fn output_value_kind_trees(plan: &[PlanOp]) -> Vec<Option<ValueKindTag>> {
    let mut scope: HashMap<String, EntityKind> = HashMap::new();
    let mut flat_kinds: Vec<Option<EntityKind>> = Vec::new();
    bind_ops(plan, &mut scope, &mut flat_kinds);
    // Walk the plan to find the Project op that produces the output columns,
    // using the final scope for entity kind analysis.
    for op in plan.iter().rev() {
        if let PlanOp::Project { items } = op {
            return items
                .iter()
                .zip(flat_kinds.iter().chain(std::iter::repeat(&None)))
                .map(|(item, flat)| {
                    // Skip columns already covered by the flat mechanism.
                    if flat.is_some() {
                        return None;
                    }
                    match &item.value {
                        RetVal::ScalarExpr(op) => operand_kind_tree(op, &scope),
                        RetVal::FuncCall { name, args } => {
                            let op = Operand::FuncCall {
                                name: name.clone(),
                                args: args.clone(),
                            };
                            operand_kind_tree(&op, &scope)
                        }
                        _ => None,
                    }
                })
                .collect();
        }
    }
    vec![None; flat_kinds.len()]
}
