//! `UNWIND … <write clauses>` (QL-09b): the write clauses run once per row the
//! read part produces, and the whole statement commits as one batch.
//!
//! The statement runs in two phases, like every Cypher write here:
//!
//! 1. **Read.** The read part (UNWIND, MATCH, WITH, …) runs once, through the
//!    ordinary query engine, over the state before the statement. It projects
//!    the node and value variables of each row, and one column for every
//!    expression a write clause evaluates that does not read a node's
//!    properties (`r.id`, `$tenant`, `toUpper(r.name)`).
//! 2. **Write.** One pass over the rows turns the write clauses into batch ops,
//!    in order. A small overlay of the statement's own effects (nodes created,
//!    deleted, properties changed, edges created) lets a later row, and a later
//!    clause, see an earlier one: `UNWIND [1, 1] AS x MERGE (:N {id: x})`
//!    creates one node, and `SET n.count = n.count + 1` over two rows that match
//!    the same node adds two. An expression that reads a node's properties is
//!    evaluated here, at its turn, against the overlay (see [`super::row_eval`]).
//!    The ops commit as a single `Batch` frame, so a failure on any row writes
//!    nothing, and every write meets the same commit-time checks as `CREATE` or
//!    `SET` (constraints, the `id` rule, namespace and view-owned refusals,
//!    rules).
//! 3. **Return.** A `RETURN` projects the rows the pass captured (each row's
//!    node keys and values) after the commit, reading node properties from the
//!    written store; a node the statement deleted is null.
//!
//! The overlay is hash-keyed, so the pass is linear in the rows.
//!
//! Phases 1 and 2 read through a [`RowSource`]: the store for an auto-commit
//! statement ([`GraphDb::exec_unwind_write`]), or a transaction's state, where
//! the ops are buffered instead of committed and the `RETURN` reads the
//! transaction with them laid over it ([`unwind_rows`], [`RowPass::result`]).

use super::row_eval::{
    reads_node, reads_rel_prop, uw_edge_exists, uw_edge_props, uw_edges_removed, uw_eval,
    uw_exists, uw_has_edges, uw_props, EdgeKey, Overlay, RowSource,
};
use super::*;
use core_query::cypher::{
    ast::OptionalClause, CreateEdge, MergeSet, RelDir, RelPat, RowCreateNode, RowMerge,
    RowMergeNode, RowReturn, RowWrite, SetClause, UnwindClause, UnwindExpr, UnwindWriteStmt,
    REL_ENDS_FN,
};

/// The parameter that carries the captured rows to the `RETURN` query.
const ROWS_PARAM: &str = "__uw_rows";
/// The name the `RETURN` query gives one captured row.
const ROW_VAR: &str = "__uw_row";

/// Where each write operand's value sits in the read part's result. An operand
/// with no entry reads a node's properties and is evaluated as the rows are
/// written.
struct Columns {
    by_operand: HashMap<*const Operand, String>,
    items: Vec<RetItem>,
    /// Some write operand is evaluated per row at write time.
    dynamic: bool,
}

/// The refusal for a `CREATE` with a property map, or a `MERGE` whose map
/// differs, on a pair that already has its relationship: one property map per
/// (type, start, end) (QL-RP).
pub(crate) const REL_EXISTS: &str = "a relationship between these nodes with this type already \
     exists; use MERGE … SET to change its properties";

/// A relationship variable's binding in a row's environment: the edge's
/// type and end keys, length-prefixed so any key round-trips.
fn rel_binding((t, a, b): &EdgeKey) -> String {
    format!("{}:{}:{}:{t}{a}{b}", t.len(), a.len(), b.len())
}

/// The edge a [`rel_binding`] names.
fn rel_of(binding: &str) -> Option<EdgeKey> {
    let mut parts = binding.splitn(4, ':');
    let lt: usize = parts.next()?.parse().ok()?;
    let la: usize = parts.next()?.parse().ok()?;
    let lb: usize = parts.next()?.parse().ok()?;
    let rest = parts.next()?;
    if rest.len() != lt + la + lb {
        return None;
    }
    Some((
        rest.get(..lt)?.to_string(),
        rest.get(lt..lt + la)?.to_string(),
        rest.get(lt + la..)?.to_string(),
    ))
}

/// The column a read-part relationship's identity is projected under.
fn rel_ends_col(var: &str) -> String {
    format!("__uwe_{var}")
}

impl Columns {
    fn operand(&mut self, op: &Operand, node_vars: &[String], rel_vars: &[String]) {
        if reads_node(op, node_vars) || reads_rel_prop(op, rel_vars) {
            self.dynamic = true;
            return;
        }
        let name = format!("__uw{}", self.items.len());
        self.by_operand.insert(op as *const Operand, name.clone());
        self.items.push(RetItem {
            value: RetVal::ScalarExpr(op.clone()),
            alias: Some(name),
        });
    }

    /// The column that holds `op`, or `None` when it is evaluated at write time.
    fn column_of(&self, op: &Operand) -> Option<&str> {
        self.by_operand
            .get(&(op as *const Operand))
            .map(String::as_str)
    }
}

/// Counts the statement reports.
#[derive(Default)]
struct Tally {
    created: i64,
    props_set: i64,
    deleted: i64,
    relationships_created: i64,
    /// The keys every `MERGE` looked up, found or created: a transaction
    /// guards its commit with them, as its plain `MERGE` does.
    merge_keys: Vec<String>,
}

fn err(detail: String) -> GraphError {
    GraphError::QueryError { detail }
}

/// The text a `RETURN` clause's names are looked for in. An extra match only
/// costs a lookup or a captured value, and every name the clause uses appears
/// in it as a quoted string, so a name that is used is never missed.
fn used_text(ret: &RowReturn) -> String {
    format!("{:?}{:?}", ret.items, ret.order_by)
}

fn names_used(used: &str, name: &str) -> bool {
    used.contains(&format!("{name:?}"))
}

/// A relationship variable the `RETURN` names, and what the write pass captures
/// to find the relationship again after the commit: its two end nodes' keys and
/// its type. The end nodes are the pattern's own (`left` and `right` as
/// written, with `dir` between them).
struct RelCapture {
    var: String,
    left: String,
    right: String,
    dir: RelDir,
    etypes: Vec<String>,
    /// Bound by a `MERGE` of the statement (its ends and type come from the
    /// merge), not by the read part.
    merged: bool,
}

impl RelCapture {
    /// The key of a captured field in the row map; `tag` is `l`, `r` or `t`.
    fn field(&self, tag: &str) -> String {
        format!("__uwr{tag}_{}", self.var)
    }
}

/// Name the anonymous end nodes of every pattern that binds a relationship
/// variable in `wanted`, so their keys can be projected, and describe each such
/// relationship. A variable-length relationship has no single end pair and is
/// refused by name.
fn read_rel_captures(read: &mut Query, wanted: &[String]) -> Result<Vec<RelCapture>> {
    let mut found: Vec<RelCapture> = Vec::new();
    let mut anonymous = 0usize;
    let mut refused: Option<String> = None;
    let mut visit = |pattern: &mut Pattern| {
        if pattern.shortest {
            return;
        }
        let mut name = |node: &mut NodePat| -> String {
            node.var
                .get_or_insert_with(|| {
                    anonymous += 1;
                    format!("_uwn{anonymous}")
                })
                .clone()
        };
        let mut left = name(&mut pattern.start);
        for (rel, dest) in &mut pattern.chain {
            let right = name(dest);
            if let Some(var) = rel.var.as_ref().filter(|v| wanted.contains(v)) {
                if rel.hops.is_some() {
                    refused = Some(var.clone());
                } else {
                    found.push(RelCapture {
                        var: var.clone(),
                        left: left.clone(),
                        right: right.clone(),
                        dir: rel.dir,
                        etypes: rel.etypes.clone(),
                        merged: false,
                    });
                }
            }
            left = right;
        }
    };
    for p in &mut read.matches {
        visit(p);
    }
    for oc in &mut read.optional_clauses {
        oc.patterns.iter_mut().for_each(&mut visit);
    }
    for stage in &mut read.stages {
        stage.matches.iter_mut().for_each(&mut visit);
        for oc in &mut stage.optional_clauses {
            oc.patterns.iter_mut().for_each(&mut visit);
        }
    }
    if let Some(var) = refused {
        return Err(err(format!(
            "not supported in mushroomdb 0.10: the variable-length relationship `{var}` in the \
             RETURN of a write statement that begins with UNWIND"
        )));
    }
    Ok(found)
}

/// The relationships the `RETURN` names: the read part's (the clone of the read
/// part it returns has its anonymous end nodes named) and the `MERGE`s'.
fn rel_captures(stmt: &UnwindWriteStmt, used: &str) -> Result<(Query, Vec<RelCapture>)> {
    let wanted: Vec<String> = stmt
        .rel_vars
        .iter()
        .filter(|v| names_used(used, v))
        .cloned()
        .collect();
    let mut read = stmt.read.clone();
    let mut rels = read_rel_captures(&mut read, &wanted)?;
    for write in &stmt.writes {
        if let RowWrite::Create { edges, .. } = write {
            for edge in edges {
                if let Some(var) = edge.rel_var.as_ref().filter(|v| wanted.contains(v)) {
                    rels.retain(|r| &r.var != var);
                    rels.push(RelCapture {
                        var: var.clone(),
                        left: edge.src_var.clone(),
                        right: edge.dst_var.clone(),
                        dir: RelDir::Right,
                        etypes: vec![edge.etype.clone()],
                        merged: true,
                    });
                }
            }
        }
        if let RowWrite::Merge(merge) = write {
            if let Some(edge) = &merge.edge {
                if let Some(var) = edge.rel_var.as_ref().filter(|v| wanted.contains(v)) {
                    rels.retain(|r| &r.var != var);
                    rels.push(RelCapture {
                        var: var.clone(),
                        left: merge.start.var.clone(),
                        right: edge.end.var.clone(),
                        dir: edge.dir,
                        etypes: vec![edge.etype.clone()],
                        merged: true,
                    });
                }
            }
        }
    }
    Ok((read, rels))
}

/// The columns the read part projects: every variable the write clauses or the
/// `RETURN` need, and one per write operand that does not read a node.
fn plan_columns(stmt: &UnwindWriteStmt, used: &str, captures: &[RelCapture]) -> Columns {
    let mut cols = Columns {
        by_operand: HashMap::new(),
        items: Vec::new(),
        dynamic: false,
    };
    // A constant column, so the read part always yields its row count.
    cols.items.push(RetItem {
        value: RetVal::ScalarExpr(Operand::Lit(Value::Int(1))),
        alias: Some("__uw_row".into()),
    });
    let nodes = &stmt.node_vars;
    let rels = &stmt.rel_vars;
    let mut var_names: Vec<String> = Vec::new();
    let mut note_var = |v: &str| {
        if stmt.scope.iter().any(|s| s == v) && !var_names.iter().any(|n| n == v) {
            var_names.push(v.to_string());
        }
    };
    for write in &stmt.writes {
        match write {
            RowWrite::Create { nodes: made, edges } => {
                for node in made {
                    for (_, op) in &node.props {
                        cols.operand(op, nodes, rels);
                    }
                    if let Some(op) = &node.props_map {
                        cols.operand(op, nodes, rels);
                    }
                }
                for edge in edges {
                    note_var(&edge.src_var);
                    note_var(&edge.dst_var);
                    for (_, op) in &edge.props {
                        cols.operand(op, nodes, rels);
                    }
                    if let Some(op) = &edge.props_map {
                        cols.operand(op, nodes, rels);
                    }
                }
            }
            RowWrite::Merge(merge) => {
                let ends = std::iter::once(&merge.start).chain(merge.edge.as_ref().map(|e| &e.end));
                for end in ends {
                    if end.bound {
                        note_var(&end.var);
                    } else {
                        merge_operands(end, &mut cols, nodes, rels);
                    }
                }
                if let Some(edge) = &merge.edge {
                    for (_, op) in &edge.props {
                        cols.operand(op, nodes, rels);
                    }
                }
                for item in merge.on_create.iter().chain(&merge.on_match) {
                    match item {
                        MergeSet::Prop(sc) => cols.operand(&sc.value, nodes, rels),
                        MergeSet::Map { value, .. } => cols.operand(value, nodes, rels),
                    }
                }
            }
            RowWrite::Set(sets) => {
                for sc in sets {
                    note_var(&sc.var);
                    cols.operand(&sc.value, nodes, rels);
                }
            }
            RowWrite::SetMap { var, value, .. } => {
                note_var(var);
                cols.operand(value, nodes, rels);
            }
            RowWrite::Remove(items) => {
                for (var, _) in items {
                    note_var(var);
                }
            }
            RowWrite::Delete { vars, .. } => {
                for var in vars {
                    note_var(var);
                }
            }
        }
    }
    // A write evaluated at its turn, or a RETURN, needs every variable of the
    // row, not only the nodes a write clause names.
    // A write evaluated at its turn needs every variable of the row; a `RETURN`
    // only the ones it names.
    let wanted: Vec<String> = if cols.dynamic {
        stmt.scope.clone()
    } else {
        let mut names = var_names;
        if stmt.returns.is_some() {
            for name in &stmt.scope {
                if names_used(used, name) && !names.contains(name) {
                    names.push(name.clone());
                }
            }
        }
        names
    };
    let mut projected: Vec<String> = wanted.clone();
    for name in wanted {
        cols.items.push(RetItem {
            value: RetVal::Var(name),
            alias: None,
        });
    }
    // The identity of each read-part relationship a write clause names, in
    // the edge's own direction whatever the pattern's.
    for var in read_rels_written(stmt) {
        cols.items.push(RetItem {
            value: RetVal::FuncCall {
                name: REL_ENDS_FN.into(),
                args: vec![Operand::Var(var.clone())],
            },
            alias: Some(rel_ends_col(&var)),
        });
    }
    // The ends and the type of each relationship the RETURN names.
    for rel in captures.iter().filter(|r| !r.merged) {
        for end in [&rel.left, &rel.right] {
            if !projected.contains(end) {
                projected.push(end.clone());
                cols.items.push(RetItem {
                    value: RetVal::Var(end.clone()),
                    alias: None,
                });
            }
        }
        cols.items.push(RetItem {
            value: RetVal::FuncCall {
                name: "type".into(),
                args: vec![Operand::Var(rel.var.clone())],
            },
            alias: Some(rel.field("t")),
        });
    }
    cols
}

fn merge_operands(node: &RowMergeNode, cols: &mut Columns, nodes: &[String], rels: &[String]) {
    cols.operand(&node.key, nodes, rels);
    if let Some(ns) = &node.ns {
        cols.operand(ns, nodes, rels);
    }
}

/// The relationship variables the read part binds that a write clause names:
/// each is projected as its identity so a write can find the edge.
fn read_rels_written(stmt: &UnwindWriteStmt) -> Vec<String> {
    let made: HashSet<&str> = stmt
        .writes
        .iter()
        .flat_map(|w| match w {
            RowWrite::Create { edges, .. } => edges
                .iter()
                .filter_map(|e| e.rel_var.as_deref())
                .collect::<Vec<_>>(),
            RowWrite::Merge(m) => m
                .edge
                .as_ref()
                .and_then(|e| e.rel_var.as_deref())
                .into_iter()
                .collect(),
            _ => Vec::new(),
        })
        .collect();
    let named = names_in_writes(&stmt.writes);
    stmt.rel_vars
        .iter()
        .filter(|v| !made.contains(v.as_str()) && named.contains(v.as_str()))
        .cloned()
        .collect()
}

/// Every variable the write clauses name: a `SET` / `REMOVE` / `DELETE`
/// target, and every variable or property an operand of theirs reads.
fn names_in_writes(writes: &[RowWrite]) -> HashSet<String> {
    let mut names: HashSet<String> = HashSet::new();
    let mut operands: Vec<&Operand> = Vec::new();
    for write in writes {
        match write {
            RowWrite::Create { nodes, edges } => {
                for node in nodes {
                    operands.extend(node.props.iter().map(|(_, op)| op));
                    operands.extend(node.props_map.as_ref());
                }
                for edge in edges {
                    operands.extend(edge.props.iter().map(|(_, op)| op));
                    operands.extend(edge.props_map.as_ref());
                }
            }
            RowWrite::Merge(merge) => {
                if let Some(edge) = &merge.edge {
                    operands.extend(edge.props.iter().map(|(_, op)| op));
                }
                for item in merge.on_create.iter().chain(&merge.on_match) {
                    names.insert(item.var().to_string());
                    match item {
                        MergeSet::Prop(sc) => operands.push(&sc.value),
                        MergeSet::Map { value, .. } => operands.push(value),
                    }
                }
            }
            RowWrite::Set(sets) => {
                for sc in sets {
                    names.insert(sc.var.clone());
                    operands.push(&sc.value);
                }
            }
            RowWrite::SetMap { var, value, .. } => {
                names.insert(var.clone());
                operands.push(value);
            }
            RowWrite::Remove(items) => names.extend(items.iter().map(|(v, _)| v.clone())),
            RowWrite::Delete { vars, .. } => names.extend(vars.iter().cloned()),
        }
    }
    for op in operands {
        super::row_eval::map_leaves(op, &mut |leaf| {
            if let Operand::Prop { var, .. } | Operand::Var(var) = leaf {
                names.insert(var.clone());
            }
            None
        });
    }
    names
}

/// The read query a `RETURN` runs after the commit. It unwinds the rows the
/// write pass captured (each row's node keys and values), restores the value
/// variables, and finds each node variable again by its key with an
/// `OPTIONAL MATCH`, so a node the statement deleted is null rather than a
/// dropped row. The read part is not run again.
fn return_query(stmt: &UnwindWriteStmt, ret: &RowReturn, rels: &[RelCapture]) -> Query {
    let row_field = |name: &str| Operand::Prop {
        var: ROW_VAR.to_string(),
        field: name.to_string(),
    };
    let mut items: Vec<RetItem> = vec![RetItem {
        value: RetVal::Var(ROW_VAR.to_string()),
        alias: None,
    }];
    let mut optional_clauses: Vec<OptionalClause> = Vec::new();
    let mut names: Vec<&String> = stmt.scope.iter().collect();
    for node in &stmt.node_vars {
        if !names.contains(&node) {
            names.push(node);
        }
    }
    // A variable the RETURN never names is neither captured nor looked up
    // (`RETURN count(*)` over a bulk load reads nothing but the row count).
    let used = used_text(ret);
    for name in names {
        if !names_used(&used, name) {
            continue;
        }
        if stmt.node_vars.contains(name) {
            optional_clauses.push(OptionalClause {
                patterns: vec![Pattern {
                    start: NodePat {
                        var: Some(name.clone()),
                        label: None,
                        // `id` is the planner's key lookup (a `ScanKey` per row, not a scan).
                        props: vec![("id".to_string(), row_field(name))],
                    },
                    chain: vec![],
                    shortest: false,
                    path_var: None,
                    continues_match: false,
                }],
                where_expr: None,
            });
        } else {
            items.push(RetItem {
                value: RetVal::ScalarExpr(row_field(name)),
                alias: Some(name.clone()),
            });
        }
    }
    // Each relationship the RETURN names is found again between its captured
    // ends, of its captured type.
    for rel in rels {
        let end = |tag: &str, side: &str| NodePat {
            var: Some(format!("__uw_{side}_{}", rel.var)),
            label: None,
            props: vec![("id".to_string(), row_field(&rel.field(tag)))],
        };
        optional_clauses.push(OptionalClause {
            patterns: vec![Pattern {
                start: end("l", "l"),
                chain: vec![(
                    RelPat {
                        var: Some(rel.var.clone()),
                        etypes: rel.etypes.clone(),
                        dir: rel.dir,
                        hops: None,
                    },
                    end("r", "r"),
                )],
                shortest: false,
                path_var: None,
                continues_match: false,
            }],
            where_expr: Some(core_query::cypher::Expr::Cmp {
                lhs: Operand::FuncCall {
                    name: "type".into(),
                    args: vec![Operand::Var(rel.var.clone())],
                },
                op: core_query::CmpOp::Eq,
                rhs: row_field(&rel.field("t")),
            }),
        });
    }
    Query {
        matches: Vec::new(),
        optional_clauses: Vec::new(),
        where_expr: None,
        unwinds: vec![UnwindClause {
            list: UnwindExpr::Expr(Operand::Param(ROWS_PARAM.to_string())),
            alias: ROW_VAR.to_string(),
        }],
        post_unwind_where: None,
        stages: vec![core_query::cypher::WithStage {
            items,
            where_expr: None,
            order_by: Vec::new(),
            skip: None,
            limit: None,
            matches: Vec::new(),
            optional_clauses,
            unwinds: Vec::new(),
            post_where: None,
        }],
        returns: ret.items.clone(),
        distinct: ret.distinct,
        order_by: ret.order_by.clone(),
        skip: ret.skip.clone(),
        limit: ret.limit.clone(),
    }
}

/// One row of the pass: how a write clause reads the row's values.
struct RowCtx<'a, S: RowSource + ?Sized> {
    src: &'a S,
    rows: &'a ResultSet,
    row: usize,
    cols: &'a Columns,
    node_vars: &'a HashSet<&'a str>,
    /// Every relationship variable of the statement.
    rel_vars: &'a HashSet<&'a str>,
    /// The row's non-node variables.
    values: Vec<(&'a str, Value)>,
    params: &'a BTreeMap<String, Value>,
}

impl<S: RowSource + ?Sized> RowCtx<'_, S> {
    /// The value of `op` for this row at this point of the statement; `None`
    /// is null.
    fn eval(
        &self,
        op: &Operand,
        env: &HashMap<&str, String>,
        ov: &mut Overlay,
    ) -> Result<Option<Value>> {
        if let Some(col) = self.cols.column_of(op) {
            return Ok(match self.rows.get(self.row, col) {
                None | Some(Value::Null) => None,
                Some(v) => Some(v.clone()),
            });
        }
        let node_key = |name: &str| -> Option<Option<String>> {
            self.node_vars
                .contains(name)
                .then(|| env.get(name).cloned())
        };
        let rel_key = |name: &str| -> Option<Option<EdgeKey>> {
            self.rel_vars
                .contains(name)
                .then(|| env.get(name).and_then(|b| rel_of(b)))
        };
        uw_eval(
            self.src,
            ov,
            op,
            &node_key,
            &rel_key,
            &self.values,
            self.params,
        )
    }
}

/// What the read and write phases of one statement produced: the batch ops to
/// commit (or buffer), the counts, and what a `RETURN` projects afterwards.
pub(crate) struct RowPass {
    /// The statement's writes, in order.
    pub(crate) ops: Vec<BatchOp>,
    tally: Tally,
    /// One entry per row, for the `RETURN`.
    captured: Vec<Value>,
    /// The `RETURN` query, planned before anything was written.
    return_plan: Option<Query>,
}

impl RowPass {
    /// Nodes the statement created.
    pub(crate) fn created(&self) -> i64 {
        self.tally.created
    }

    /// The keys the statement's `MERGE`s looked up, found or created.
    pub(crate) fn merge_keys(&self) -> &[String] {
        &self.tally.merge_keys
    }

    /// The statement's result once its ops are applied: the write counts, or
    /// the `RETURN` projected over `view` (the state with the ops applied).
    pub(crate) fn result(
        self,
        view: &GraphView<'_>,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResultSet> {
        let Some(query) = self.return_plan else {
            return Ok(write_counts(
                self.tally.created,
                self.tally.props_set,
                self.tally.deleted,
                self.tally.relationships_created,
            ));
        };
        let ops = plan(&query).map_err(|e| err(format!("plan: {e}")))?;
        // Only the parameters the RETURN names go along: the statement's own
        // (a bulk load's rows) are not copied again.
        let used = format!("{:?}", query);
        let mut with_rows: BTreeMap<String, Value> = params
            .iter()
            .filter(|(name, _)| used.contains(&format!("{name:?}")))
            .map(|(name, v)| (name.clone(), v.clone()))
            .collect();
        with_rows.insert(ROWS_PARAM.to_string(), Value::List(self.captured));
        execute(view, &ops, &Params(&with_rows)).map_err(|e| err(format!("execute: {e}")))
    }
}

/// Phases 1 and 2 of `UNWIND … <write clauses> [RETURN …]` over `src`: the
/// read part, then the write clauses per row, as batch ops. Nothing is
/// written.
pub(crate) fn unwind_rows<S: RowSource + ?Sized>(
    src: &S,
    stmt: &UnwindWriteStmt,
    params: &BTreeMap<String, Value>,
) -> Result<RowPass> {
    // Plan the RETURN first: a statement that cannot be projected must not
    // commit.
    let used = stmt.returns.as_ref().map(used_text).unwrap_or_default();
    let (mut read, rels) = rel_captures(stmt, &used)?;
    let return_plan = match &stmt.returns {
        Some(ret) => {
            let query = return_query(stmt, ret, &rels);
            plan(&query).map_err(|e| err(format!("plan: {e}")))?;
            Some(query)
        }
        None => None,
    };

    // Phase 1: the read part, one row per iteration.
    let mut cols = plan_columns(stmt, &used, &rels);
    let rows = match one_row(src, &read, &cols, params) {
        Some(rows) => rows?,
        None => {
            read.returns = std::mem::take(&mut cols.items);
            let ops = plan(&read).map_err(|e| err(format!("plan: {e}")))?;
            cols.items = read.returns;
            src.row_view(|view| execute(view, &ops, &Params(params)))
                .map_err(|e| err(format!("execute: {e}")))?
        }
    };

    // Phase 2: the write clauses, per row.
    let var_cols: Vec<&str> = cols
        .items
        .iter()
        .filter(|i| i.alias.is_none())
        .filter_map(|i| match &i.value {
            RetVal::Var(v) => Some(v.as_str()),
            _ => None,
        })
        .collect();
    let node_vars: HashSet<&str> = stmt.node_vars.iter().map(String::as_str).collect();
    let rel_vars: HashSet<&str> = stmt.rel_vars.iter().map(String::as_str).collect();
    let read_rels = read_rels_written(stmt);
    let capture = stmt.returns.is_some();
    let mut captured: Vec<Value> = Vec::new();
    let mut ov = Overlay::default();
    let mut ops: Vec<BatchOp> = Vec::new();
    let mut tally = Tally::default();
    for row in 0..rows.len() {
        let mut env: HashMap<&str, String> = HashMap::new();
        let mut values: Vec<(&str, Value)> = Vec::new();
        for var in &var_cols {
            match rows.get(row, var) {
                Some(Value::Null) | None => {}
                Some(Value::Str(key)) if node_vars.contains(var) => {
                    env.insert(var, key.clone());
                }
                Some(v) if !node_vars.contains(var) => values.push((var, v.clone())),
                Some(_) => {}
            }
        }
        for var in &read_rels {
            if let Some(Value::List(ends)) = rows.get(row, &rel_ends_col(var)) {
                if let [Value::Str(t), Value::Str(a), Value::Str(b)] = ends.as_slice() {
                    env.insert(
                        var.as_str(),
                        rel_binding(&(t.clone(), a.clone(), b.clone())),
                    );
                }
            }
        }
        let ctx = RowCtx {
            src,
            rows: &rows,
            row,
            cols: &cols,
            node_vars: &node_vars,
            rel_vars: &rel_vars,
            values,
            params,
        };
        for write in &stmt.writes {
            uw_write(write, &ctx, &mut env, &mut ov, &mut ops, &mut tally)?;
        }
        if capture {
            // Only what the RETURN names: a bare `count(*)` captures an
            // empty row.
            let mut entry: BTreeMap<String, Value> = BTreeMap::new();
            for (name, v) in &ctx.values {
                if names_used(&used, name) {
                    entry.insert((*name).to_string(), v.clone());
                }
            }
            for (name, key) in &env {
                if names_used(&used, name) && !rel_vars.contains(name) {
                    entry.insert((*name).to_string(), Value::Str(key.clone()));
                }
            }
            for rel in &rels {
                let (left, right, etype) = if rel.merged {
                    (
                        env.get(rel.left.as_str()).cloned(),
                        env.get(rel.right.as_str()).cloned(),
                        rel.etypes.first().cloned(),
                    )
                } else {
                    let text = |col: &str| match rows.get(row, col) {
                        Some(Value::Str(k)) => Some(k.clone()),
                        _ => None,
                    };
                    (text(&rel.left), text(&rel.right), text(&rel.field("t")))
                };
                for (tag, value) in [("l", left), ("r", right), ("t", etype)] {
                    if let Some(v) = value {
                        entry.insert(rel.field(tag), Value::Str(v));
                    }
                }
            }
            captured.push(Value::Map(entry));
        }
    }
    Ok(RowPass {
        ops,
        tally,
        captured,
        return_plan,
    })
}

/// The read part's one row, evaluated directly, when the read part is only
/// the `UNWIND [1] AS <ONE_ROW_VAR>` a `CREATE` / `MERGE` that opens the
/// statement is given (`CREATE (:P {id: $id})`): every column is then a
/// literal or reads only `$parameter`s, so planning and running a query for
/// it would only add cost. `None` for any other read part, or a column this
/// cannot evaluate, which then runs through the query engine as usual.
fn one_row<S: RowSource + ?Sized>(
    src: &S,
    read: &Query,
    cols: &Columns,
    params: &BTreeMap<String, Value>,
) -> Option<Result<ResultSet>> {
    let only_one_row = read.matches.is_empty()
        && read.optional_clauses.is_empty()
        && read.where_expr.is_none()
        && read.post_unwind_where.is_none()
        && read.stages.is_empty()
        && matches!(
            read.unwinds.as_slice(),
            [UnwindClause { list: UnwindExpr::Lit(items), alias }]
                if alias == core_query::cypher::ONE_ROW_VAR && items.len() == 1
        );
    if !only_one_row {
        return None;
    }
    let columns: Vec<String> = cols
        .items
        .iter()
        .map(|item| match (&item.alias, &item.value) {
            (Some(alias), _) => Some(alias.clone()),
            (None, RetVal::Var(v)) => Some(v.clone()),
            _ => None,
        })
        .collect::<Option<_>>()?;
    let values = src.row_view(|view| -> Option<Result<Vec<Option<Value>>>> {
        let mut values = Vec::with_capacity(cols.items.len());
        for item in &cols.items {
            let value = match &item.value {
                RetVal::ScalarExpr(op) => {
                    match core_query::cypher::eval_operand(view, op, &Params(params), &[]) {
                        Ok(v) => v,
                        Err(e) => return Some(Err(err(format!("execute: {e}")))),
                    }
                }
                RetVal::Var(v) if v == core_query::cypher::ONE_ROW_VAR => Some(Value::Int(1)),
                _ => return None,
            };
            values.push(value);
        }
        Some(Ok(values))
    })?;
    Some(values.map(|values| {
        let mut rows = ResultSet::new(columns);
        rows.push_row(values);
        rows
    }))
}

impl<F: Fs> GraphDb<F> {
    /// Run `UNWIND … <write clauses> [RETURN …]`.
    pub(super) fn exec_unwind_write(
        &mut self,
        stmt: UnwindWriteStmt,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResultSet> {
        let mut pass = unwind_rows(&*self, &stmt, params)?;
        let ops = std::mem::take(&mut pass.ops);
        if !ops.is_empty() {
            self.commit_batch(ops)?;
        }
        // Refresh the role mask so a created node is visible to the RETURN
        // (read-after-write); see `exec_merge`.
        if pass.created() > 0 {
            if let Some(role) = self.pending_write_authz.as_ref().map(|a| a.role.clone()) {
                let new_mask = self.mask_for_role(&role)?;
                if let Some(a) = self.pending_write_authz.as_mut() {
                    a.mask = new_mask;
                }
            }
        }
        self.row_view(|view| pass.result(view, params))
    }
}

/// One write clause for one row.
fn uw_write<'v, S: RowSource + ?Sized>(
    write: &'v RowWrite,
    ctx: &RowCtx<'_, S>,
    env: &mut HashMap<&'v str, String>,
    ov: &mut Overlay,
    ops: &mut Vec<BatchOp>,
    tally: &mut Tally,
) -> Result<()> {
    let src = ctx.src;
    match write {
        RowWrite::Create { nodes, edges } => {
            for node in nodes {
                uw_create_node(node, ctx, env, ov, ops)?;
                tally.created += 1;
            }
            for edge in edges {
                uw_create_edge(edge, ctx, env, ov, ops, tally)?;
            }
        }
        RowWrite::Merge(merge) => {
            uw_merge(merge, ctx, env, ov, ops, tally)?;
        }
        RowWrite::Set(sets) => {
            for sc in sets {
                if ctx.rel_vars.contains(sc.var.as_str()) {
                    uw_set_rel_prop(sc, ctx, env, ov, ops, tally)?;
                    continue;
                }
                let Some(key) = env.get(sc.var.as_str()).cloned() else {
                    continue; // a null node (OPTIONAL MATCH): nothing to set
                };
                let Some(v) = ctx.eval(&sc.value, env, ov)? else {
                    return Err(err(format!(
                        "SET value for {}.{} evaluated to null",
                        sc.var, sc.field
                    )));
                };
                ov.note_set(&key, &sc.field, &v);
                ops.push(BatchOp::SetProp {
                    key,
                    field: sc.field.clone(),
                    value: v,
                });
                tally.props_set += 1;
            }
        }
        RowWrite::SetMap {
            var,
            merge,
            value: operand,
        } => {
            let Some(key) = env.get(var.as_str()).cloned() else {
                return Ok(());
            };
            let set = MapSet {
                var,
                merge: *merge,
                value: operand,
            };
            if ctx.rel_vars.contains(var.as_str()) {
                uw_set_rel_map(&set, &key, ctx, env, ov, ops, tally)?;
            } else {
                uw_set_map(&set, key, ctx, env, ov, ops, tally)?;
            }
        }
        RowWrite::Remove(items) => {
            for (var, field) in items {
                let Some(key) = env.get(var.as_str()).cloned() else {
                    continue;
                };
                if ctx.rel_vars.contains(var.as_str()) {
                    let edge = bound_rel(var, &key)?;
                    uw_remove_rel_prop(var, &edge, field, ctx, ov, ops, tally)?;
                    continue;
                }
                if uw_props(src, ov, &key).contains_key(field) {
                    tally.props_set += 1;
                }
                ov.note_remove(&key, field);
                ops.push(BatchOp::RemoveProp {
                    key,
                    field: field.clone(),
                });
            }
        }
        RowWrite::Delete { vars, detach } => {
            for var in vars {
                let Some(key) = env.get(var.as_str()).cloned() else {
                    continue;
                };
                if !uw_exists(src, ov, &key) {
                    continue; // already deleted by an earlier row
                }
                if !*detach && uw_has_edges(src, ov, &key) {
                    return Err(err(format!(
                        "Cannot delete node `{key}` because it still has incident edges. \
                         Use DETACH DELETE to remove the node and all its edges."
                    )));
                }
                // A node and the relationships it takes with it, as
                // `MATCH … DETACH DELETE` counts them.
                if *detach {
                    tally.deleted += uw_edges_removed(src, ov, &key);
                }
                ov.node_deleted(&key);
                ops.push(BatchOp::DeleteNode { key });
                tally.deleted += 1;
            }
        }
    }
    Ok(())
}

/// A `SET n = <map>` / `SET n += <map>`, as a clause of its own or as an item
/// of a MERGE's `ON CREATE SET` / `ON MATCH SET`.
struct MapSet<'a> {
    var: &'a str,
    merge: bool,
    value: &'a Operand,
}

/// Apply a [`MapSet`] to the node `key`: the value must evaluate to a map; a
/// null entry removes the key under `+=` and is an absent key under `=` (see
/// [`plan_map_write`]).
fn uw_set_map<S: RowSource + ?Sized>(
    set: &MapSet<'_>,
    key: String,
    ctx: &RowCtx<'_, S>,
    env: &HashMap<&str, String>,
    ov: &mut Overlay,
    ops: &mut Vec<BatchOp>,
    tally: &mut Tally,
) -> Result<()> {
    let src = ctx.src;
    let entries: MapEntries = match ctx.eval(set.value, env, ov)? {
        Some(Value::Map(m)) => m
            .into_iter()
            .map(|(k, v)| {
                (
                    k,
                    if matches!(v, Value::Null) {
                        None
                    } else {
                        Some(v)
                    },
                )
            })
            .collect(),
        other => {
            return Err(err(format!(
                "SET {} {}= <map>: the value must be a map, got {}",
                set.var,
                if set.merge { "+" } else { "" },
                other.map_or("null".to_string(), |v| format!("{v:?}")),
            )))
        }
    };
    let existing = uw_props(src, ov, &key).clone();
    let writes = plan_map_write(&key, &existing, set.merge, &entries, &|field| {
        src.row_view_owns(field)
    });
    for w in writes {
        match w {
            MapWrite::Set { field, value } => {
                tally.props_set += 1;
                ov.note_set(&key, &field, &value);
                ops.push(BatchOp::SetProp {
                    key: key.clone(),
                    field,
                    value,
                });
            }
            MapWrite::Remove { field } => {
                if existing.contains_key(&field) {
                    tally.props_set += 1;
                }
                ov.note_remove(&key, &field);
                ops.push(BatchOp::RemoveProp {
                    key: key.clone(),
                    field,
                });
            }
        }
    }
    Ok(())
}

/// The properties `(n:L $props)` writes: the map's entries, a null one left
/// out. A value that is not a map is an error naming the parameter.
fn uw_props_map<S: RowSource + ?Sized>(
    node: &RowCreateNode,
    operand: &Operand,
    ctx: &RowCtx<'_, S>,
    env: &HashMap<&str, String>,
    ov: &mut Overlay,
) -> Result<Vec<(String, Value)>> {
    match ctx.eval(operand, env, ov)? {
        Some(Value::Map(m)) => Ok(m
            .into_iter()
            .filter(|(_, v)| !matches!(v, Value::Null))
            .collect()),
        other => {
            let name = match operand {
                Operand::Param(name) => format!("the parameter `{name}`"),
                _ => "the property map".to_string(),
            };
            Err(err(format!(
                "CREATE (:{} …): {name} must be a map of properties, got {}",
                node.label,
                other.map_or("null".to_string(), |v| format!("{v:?}")),
            )))
        }
    }
}

fn uw_create_node<'v, S: RowSource + ?Sized>(
    node: &'v RowCreateNode,
    ctx: &RowCtx<'_, S>,
    env: &mut HashMap<&'v str, String>,
    ov: &mut Overlay,
    ops: &mut Vec<BatchOp>,
) -> Result<()> {
    let mut props: Vec<(String, Value)> = match &node.props_map {
        Some(operand) => uw_props_map(node, operand, ctx, env, ov)?,
        None => Vec::with_capacity(node.props.len()),
    };
    for (field, operand) in &node.props {
        // A null value is an absent property.
        if let Some(v) = ctx.eval(operand, env, ov)? {
            props.push((field.clone(), v));
        }
    }
    let key = match props.iter().find(|(f, _)| f == "id") {
        Some((_, Value::Str(s))) => s.clone(),
        _ => {
            return Err(err(format!(
                "CREATE node ({}:{}) requires a string 'id' property",
                node.var, node.label
            )))
        }
    };
    if uw_exists(ctx.src, ov, &key) {
        return Err(GraphError::DuplicateKey { key });
    }
    ov.node_created(&key, &props);
    ops.push(BatchOp::InsertNode {
        label: node.label.clone(),
        key: key.clone(),
        props,
    });
    env.insert(node.var.as_str(), key);
    Ok(())
}

/// The key a MERGE node pattern names for this row. A bound node's key is
/// the one its variable holds.
fn uw_merge_key<S: RowSource + ?Sized>(
    node: &RowMergeNode,
    ctx: &RowCtx<'_, S>,
    env: &HashMap<&str, String>,
    ov: &mut Overlay,
) -> Result<(String, Value)> {
    if node.bound {
        return match env.get(node.var.as_str()) {
            Some(key) => Ok((key.clone(), Value::Str(key.clone()))),
            None => Err(err(format!(
                "MERGE ({0}): `{0}` is null in this row, so there is no node to merge from",
                node.var
            ))),
        };
    }
    match ctx.eval(&node.key, env, ov)? {
        Some(Value::Str(s)) => Ok((s.clone(), Value::Str(s))),
        None => Err(err(format!(
            "MERGE (:{} {{{}: null}}): a MERGE cannot match on a null property value",
            node.label, node.key_field
        ))),
        other => Err(err(format!(
            "MERGE key value must be a string (got {:?})",
            other.unwrap_or(Value::Null)
        ))),
    }
}

/// Whether a MERGE node exists at this point, after the role-scope check
/// (made once per key, against the store). A bound node must still exist.
fn uw_merge_node_existed<S: RowSource + ?Sized>(
    src: &S,
    ov: &Overlay,
    node: &RowMergeNode,
    key: &str,
) -> Result<bool> {
    if !node.bound {
        return uw_merge_existed(src, ov, &node.label, key);
    }
    if uw_exists(src, ov, key) {
        Ok(true)
    } else {
        Err(err(format!(
            "MERGE ({}): the node `{key}` was deleted earlier in the statement",
            node.var
        )))
    }
}

fn uw_merge_existed<S: RowSource + ?Sized>(
    src: &S,
    ov: &Overlay,
    label: &str,
    key: &str,
) -> Result<bool> {
    match ov.nodes.get(key) {
        // Created or deleted by the statement: its own answer.
        Some(state) if state.created_here || !state.exists => Ok(state.exists),
        _ => src.row_merge_existed(label, key),
    }
}

fn uw_merge<'v, S: RowSource + ?Sized>(
    merge: &'v RowMerge,
    ctx: &RowCtx<'_, S>,
    env: &mut HashMap<&'v str, String>,
    ov: &mut Overlay,
    ops: &mut Vec<BatchOp>,
    tally: &mut Tally,
) -> Result<()> {
    let src = ctx.src;
    let (start_key, start_value) = uw_merge_key(&merge.start, ctx, env, ov)?;
    tally.merge_keys.push(start_key.clone());
    let start_ns = match &merge.start.ns {
        Some(op) => ctx.eval(op, env, ov)?,
        None => None,
    };
    let Some(edge) = &merge.edge else {
        let existed = uw_merge_node_existed(src, ov, &merge.start, &start_key)?;
        if !existed {
            let props =
                src.row_merge_props(&merge.start.key_field, &start_value, start_ns.as_ref())?;
            ov.node_created(&start_key, &props);
            ops.push(BatchOp::InsertNode {
                label: merge.start.label.clone(),
                key: start_key.clone(),
                props,
            });
            tally.created += 1;
        }
        let sets = if existed {
            &merge.on_match
        } else {
            &merge.on_create
        };
        env.insert(merge.start.var.as_str(), start_key);
        uw_merge_sets(sets, ctx, env, ov, ops, tally)?;
        return Ok(());
    };

    let (end_key, end_value) = uw_merge_key(&edge.end, ctx, env, ov)?;
    tally.merge_keys.push(end_key.clone());
    let end_ns = match &edge.end.ns {
        Some(op) => ctx.eval(op, env, ov)?,
        None => None,
    };
    if start_key == end_key
        && !merge.start.bound
        && !edge.end.bound
        && merge.start.label != edge.end.label
    {
        return Err(err(format!(
            "MERGE: key '{start_key}' is given both label '{}' and label '{}'",
            merge.start.label, edge.end.label
        )));
    }
    let start_existed = uw_merge_node_existed(src, ov, &merge.start, &start_key)?;
    let end_existed = if end_key == start_key {
        start_existed
    } else {
        uw_merge_node_existed(src, ov, &edge.end, &end_key)?
    };
    let (src_key, dst_key) = match edge.dir {
        core_query::cypher::RelDir::Left => (end_key.clone(), start_key.clone()),
        _ => (start_key.clone(), end_key.clone()),
    };
    let edge_existed =
        start_existed && end_existed && uw_edge_exists(src, ov, &edge.etype, &src_key, &dst_key);
    let rel: EdgeKey = (edge.etype.clone(), src_key.clone(), dst_key.clone());
    let rel_var = edge.rel_var.as_deref().unwrap_or("r");
    let rel_props = uw_rel_map(rel_var, &edge.props, None, ctx, env, ov)?;
    if !rel_props.is_empty() {
        refuse_rule_owned(src, &edge.etype)?;
    }
    // The map is part of what MERGE matches: a pair whose relationship
    // carries other values is not created again beside it (one property map
    // per pair), it is refused.
    if edge_existed {
        let existing = uw_edge_props(src, ov, &rel);
        let matches = rel_props.iter().all(|(field, value)| {
            existing
                .get(field)
                .is_some_and(|v| core_query::values_equal(v, value))
        });
        if !matches {
            return Err(err(format!(
                "MERGE (…)-[:{}]->(…) from '{src_key}' to '{dst_key}': {REL_EXISTS}",
                edge.etype
            )));
        }
    }
    if !start_existed {
        let props = src.row_merge_props(&merge.start.key_field, &start_value, start_ns.as_ref())?;
        ov.node_created(&start_key, &props);
        ops.push(BatchOp::InsertNode {
            label: merge.start.label.clone(),
            key: start_key.clone(),
            props,
        });
        tally.created += 1;
    }
    if !end_existed && end_key != start_key {
        let props = src.row_merge_props(&edge.end.key_field, &end_value, end_ns.as_ref())?;
        ov.node_created(&end_key, &props);
        ops.push(BatchOp::InsertNode {
            label: edge.end.label.clone(),
            key: end_key.clone(),
            props,
        });
        tally.created += 1;
    }
    let sets = if edge_existed {
        &merge.on_match
    } else {
        ops.push(BatchOp::InsertEdge {
            edge_type: edge.etype.clone(),
            src_key: src_key.clone(),
            dst_key: dst_key.clone(),
        });
        ov.edge_created(&edge.etype, &src_key, &dst_key);
        tally.relationships_created += 1;
        write_fresh_rel_props(&rel, rel_props, ov, ops);
        &merge.on_create
    };
    env.insert(merge.start.var.as_str(), start_key);
    env.insert(edge.end.var.as_str(), end_key);
    if let Some(var) = edge.rel_var.as_deref() {
        env.insert(var, rel_binding(&rel));
    }
    uw_merge_sets(sets, ctx, env, ov, ops, tally)
}

/// `ON CREATE SET` / `ON MATCH SET` items of a MERGE; each names a node
/// the MERGE bound.
fn uw_merge_sets<S: RowSource + ?Sized>(
    sets: &[MergeSet],
    ctx: &RowCtx<'_, S>,
    env: &HashMap<&str, String>,
    ov: &mut Overlay,
    ops: &mut Vec<BatchOp>,
    tally: &mut Tally,
) -> Result<()> {
    for item in sets {
        let sc = match item {
            MergeSet::Prop(sc) if ctx.rel_vars.contains(sc.var.as_str()) => {
                uw_set_rel_prop(sc, ctx, env, ov, ops, tally)?;
                continue;
            }
            MergeSet::Prop(sc) => sc,
            MergeSet::Map { var, merge, value } => {
                let Some(key) = env.get(var.as_str()).cloned() else {
                    return Err(err(format!("SET variable '{var}' is not bound")));
                };
                let set = MapSet {
                    var,
                    merge: *merge,
                    value,
                };
                if ctx.rel_vars.contains(var.as_str()) {
                    uw_set_rel_map(&set, &key, ctx, env, ov, ops, tally)?;
                } else {
                    uw_set_map(&set, key, ctx, env, ov, ops, tally)?;
                }
                continue;
            }
        };
        let Some(v) = ctx.eval(&sc.value, env, ov)? else {
            return Err(err(format!(
                "SET value for {}.{} evaluated to null",
                sc.var, sc.field
            )));
        };
        let Some(key) = env.get(sc.var.as_str()).cloned() else {
            return Err(err(format!("SET variable '{}' is not bound", sc.var)));
        };
        ov.note_set(&key, &sc.field, &v);
        ops.push(BatchOp::SetProp {
            key,
            field: sc.field.clone(),
            value: v,
        });
        tally.props_set += 1;
    }
    Ok(())
}

// ── Relationship properties (QL-RP) ─────────────────────────────────────────

/// The edge a relationship variable's binding names.
fn bound_rel(var: &str, binding: &str) -> Result<EdgeKey> {
    rel_of(binding).ok_or_else(|| err(format!("internal: `{var}` holds no relationship")))
}

/// Refuse a user property write to an edge type a rule derives.
fn refuse_rule_owned<S: RowSource + ?Sized>(src: &S, edge_type: &str) -> Result<()> {
    match src.row_rule_owning(edge_type) {
        Some(rule) => Err(rule_owned_edge_type_error(edge_type, &rule)),
        None => Ok(()),
    }
}

/// Refuse a user write to the reserved `count`.
fn refuse_reserved(var: &str, field: &str) -> Result<()> {
    if field == EDGE_COUNT_PROP {
        return Err(edge_count_reserved_error(var));
    }
    Ok(())
}

/// The properties a relationship pattern's map gives for this row: its
/// entries (a null one left out), or the `$parameter` map's.
fn uw_rel_map<S: RowSource + ?Sized>(
    var: &str,
    props: &[(String, Operand)],
    props_map: Option<&Operand>,
    ctx: &RowCtx<'_, S>,
    env: &HashMap<&str, String>,
    ov: &mut Overlay,
) -> Result<BTreeMap<String, Value>> {
    let mut out: BTreeMap<String, Value> = BTreeMap::new();
    if let Some(operand) = props_map {
        match ctx.eval(operand, env, ov)? {
            Some(Value::Map(m)) => {
                out.extend(m.into_iter().filter(|(_, v)| !matches!(v, Value::Null)))
            }
            other => {
                return Err(err(format!(
                    "relationship property map: {} must be a map, got {}",
                    match operand {
                        Operand::Param(name) => format!("the parameter `{name}`"),
                        _ => "the value".to_string(),
                    },
                    other.map_or("null".to_string(), |v| format!("{v:?}")),
                )))
            }
        }
    }
    for (field, operand) in props {
        if let Some(v) = ctx.eval(operand, env, ov)? {
            out.insert(field.clone(), v);
        }
    }
    for field in out.keys() {
        refuse_reserved(var, field)?;
    }
    Ok(out)
}

/// The properties of a relationship the statement has just created. Like a
/// created node's, they are part of the creation and not counted in
/// `properties_set`.
fn write_fresh_rel_props(
    rel: &EdgeKey,
    props: BTreeMap<String, Value>,
    ov: &mut Overlay,
    ops: &mut Vec<BatchOp>,
) {
    for (field, value) in &props {
        ops.push(BatchOp::SetEdgeProp {
            edge_type: rel.0.clone(),
            src_key: rel.1.clone(),
            dst_key: rel.2.clone(),
            field: field.clone(),
            value: value.clone(),
        });
    }
    ov.edge_props_fresh(rel, props);
}

/// One relationship of a `CREATE`. A pair that already has its relationship
/// keeps the multiplicity behaviour when the pattern carries no map, and is
/// refused when it carries one (one property map per pair).
fn uw_create_edge<'v, S: RowSource + ?Sized>(
    edge: &'v CreateEdge,
    ctx: &RowCtx<'_, S>,
    env: &mut HashMap<&'v str, String>,
    ov: &mut Overlay,
    ops: &mut Vec<BatchOp>,
    tally: &mut Tally,
) -> Result<()> {
    let end = |var: &str, side: &str| -> Result<String> {
        env.get(var)
            .cloned()
            .ok_or_else(|| err(format!("CREATE edge {side} variable '{var}' is not bound")))
    };
    let from = end(&edge.src_var, "src")?;
    let to = end(&edge.dst_var, "dst")?;
    let rel: EdgeKey = (edge.etype.clone(), from.clone(), to.clone());
    let var = edge.rel_var.as_deref().unwrap_or("r");
    let props = uw_rel_map(var, &edge.props, edge.props_map.as_ref(), ctx, env, ov)?;
    if !props.is_empty() {
        refuse_rule_owned(ctx.src, &edge.etype)?;
    }
    let existed = uw_edge_exists(ctx.src, ov, &edge.etype, &from, &to);
    if existed && !props.is_empty() {
        return Err(err(format!(
            "CREATE ({from})-[:{}]->({to}): {REL_EXISTS}",
            edge.etype
        )));
    }
    ops.push(BatchOp::InsertEdge {
        edge_type: edge.etype.clone(),
        src_key: from.clone(),
        dst_key: to.clone(),
    });
    ov.edge_created(&edge.etype, &from, &to);
    tally.relationships_created += 1;
    if !existed {
        write_fresh_rel_props(&rel, props, ov, ops);
    }
    if let Some(var) = edge.rel_var.as_deref() {
        env.insert(var, rel_binding(&rel));
    }
    Ok(())
}

/// `SET r.f = expr` on a relationship.
fn uw_set_rel_prop<S: RowSource + ?Sized>(
    sc: &SetClause,
    ctx: &RowCtx<'_, S>,
    env: &HashMap<&str, String>,
    ov: &mut Overlay,
    ops: &mut Vec<BatchOp>,
    tally: &mut Tally,
) -> Result<()> {
    refuse_reserved(&sc.var, &sc.field)?;
    let Some(binding) = env.get(sc.var.as_str()) else {
        return Ok(()); // a null relationship (OPTIONAL MATCH): nothing to set
    };
    let rel = bound_rel(&sc.var, binding)?;
    refuse_rule_owned(ctx.src, &rel.0)?;
    let Some(value) = ctx.eval(&sc.value, env, ov)? else {
        return Err(err(format!(
            "SET value for {}.{} evaluated to null",
            sc.var, sc.field
        )));
    };
    set_rel_prop(&rel, &sc.field, value, ov, ops, tally);
    Ok(())
}

fn set_rel_prop(
    rel: &EdgeKey,
    field: &str,
    value: Value,
    ov: &mut Overlay,
    ops: &mut Vec<BatchOp>,
    tally: &mut Tally,
) {
    ov.note_edge_write(rel, field, Some(&value));
    ops.push(BatchOp::SetEdgeProp {
        edge_type: rel.0.clone(),
        src_key: rel.1.clone(),
        dst_key: rel.2.clone(),
        field: field.to_string(),
        value,
    });
    tally.props_set += 1;
}

/// `REMOVE r.f`: a property the relationship lacks is a no-op, not counted.
fn uw_remove_rel_prop<S: RowSource + ?Sized>(
    var: &str,
    rel: &EdgeKey,
    field: &str,
    ctx: &RowCtx<'_, S>,
    ov: &mut Overlay,
    ops: &mut Vec<BatchOp>,
    tally: &mut Tally,
) -> Result<()> {
    refuse_reserved(var, field)?;
    refuse_rule_owned(ctx.src, &rel.0)?;
    if !uw_edge_props(ctx.src, ov, rel).contains_key(field) {
        return Ok(());
    }
    ov.note_edge_write(rel, field, None);
    ops.push(BatchOp::RemoveEdgeProp {
        edge_type: rel.0.clone(),
        src_key: rel.1.clone(),
        dst_key: rel.2.clone(),
        field: field.to_string(),
    });
    tally.props_set += 1;
    Ok(())
}

/// `SET r = <map>` / `SET r += <map>` on a relationship: `+=` sets the listed
/// keys and a null removes; `=` makes the user's properties exactly the map's
/// non-null entries. The reserved `count` is never removed, and naming it is
/// refused.
fn uw_set_rel_map<S: RowSource + ?Sized>(
    set: &MapSet<'_>,
    binding: &str,
    ctx: &RowCtx<'_, S>,
    env: &HashMap<&str, String>,
    ov: &mut Overlay,
    ops: &mut Vec<BatchOp>,
    tally: &mut Tally,
) -> Result<()> {
    let rel = bound_rel(set.var, binding)?;
    refuse_rule_owned(ctx.src, &rel.0)?;
    let entries: Vec<(String, Option<Value>)> = match ctx.eval(set.value, env, ov)? {
        Some(Value::Map(m)) => m
            .into_iter()
            .map(|(k, v)| (k, (!matches!(v, Value::Null)).then_some(v)))
            .collect(),
        other => {
            return Err(err(format!(
                "SET {} {}= <map>: the value must be a map, got {}",
                set.var,
                if set.merge { "+" } else { "" },
                other.map_or("null".to_string(), |v| format!("{v:?}")),
            )))
        }
    };
    for (field, _) in &entries {
        refuse_reserved(set.var, field)?;
    }
    let existing = uw_edge_props(ctx.src, ov, &rel).clone();
    if !set.merge {
        let kept: HashSet<&str> = entries
            .iter()
            .filter(|(_, v)| v.is_some())
            .map(|(f, _)| f.as_str())
            .collect();
        for field in existing.keys() {
            if field != EDGE_COUNT_PROP && !kept.contains(field.as_str()) {
                uw_remove_rel_prop(set.var, &rel, field, ctx, ov, ops, tally)?;
            }
        }
    }
    for (field, value) in entries {
        match value {
            Some(v) => set_rel_prop(&rel, &field, v, ov, ops, tally),
            None => uw_remove_rel_prop(set.var, &rel, &field, ctx, ov, ops, tally)?,
        }
    }
    Ok(())
}
