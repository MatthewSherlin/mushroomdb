//! Cypher subset AST. Types match the Plan 3 Task 6 interface block.

use crate::filter::CmpOp;
use core_storage::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub matches: Vec<Pattern>,
    pub where_expr: Option<Expr>,
    pub returns: Vec<RetItem>,
    pub order_by: Vec<OrderItem>,
    pub skip: Option<u64>,
    pub limit: Option<u64>,
}

/// Aggregate function in a RETURN clause.
#[derive(Debug, Clone, PartialEq)]
pub enum AggFunc {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

/// Argument to an aggregate function.
#[derive(Debug, Clone, PartialEq)]
pub enum AggArg {
    /// `COUNT(*)` — every matched row counts regardless of binding.
    Star,
    /// `COUNT(var)` — counts rows where `var` is bound (non-null).
    Var(String),
    /// `SUM(var.field)`, `AVG(var.field)`, etc.
    Prop { var: String, field: String },
}

/// Hop-count range for variable-length relationship patterns (`*min..max`).
/// Both bounds are inclusive.  `min = max` is a fixed-hop pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HopRange {
    pub min: u8,
    pub max: u8,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pattern {
    pub start: NodePat,
    pub chain: Vec<(RelPat, NodePat)>,
    /// True when parsed as `MATCH shortestPath(...)`.
    pub shortest: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NodePat {
    pub var: Option<String>,
    pub label: Option<String>,
    pub props: Vec<(String, Operand)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelDir {
    Right,
    Left,
    Undirected,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RelPat {
    pub var: Option<String>,
    pub etype: Option<String>,
    pub dir: RelDir,
    /// `None` = single-hop (normal `Expand`).  `Some(r)` = variable-length
    /// (`VarExpand`) with the given min/max hop bounds.
    pub hops: Option<HopRange>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    Cmp {
        lhs: Operand,
        op: CmpOp,
        rhs: Operand,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Operand {
    Prop { var: String, field: String },
    Lit(Value),
    Param(String),
}

/// RETURN item value: bare variable, `var.field`, or an aggregate call.
#[derive(Debug, Clone, PartialEq)]
pub enum RetVal {
    Var(String),
    Prop {
        var: String,
        field: String,
    },
    /// Single aggregate function call.  When combined with non-aggregate items
    /// in the same RETURN clause, the planner routes to `GroupAggregate`.
    /// Multiple aggregate items are also supported via `GroupAggregate`.
    Agg {
        func: AggFunc,
        arg: AggArg,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct RetItem {
    pub value: RetVal,
    pub alias: Option<String>,
}

/// ORDER BY target. Bare identifiers that match a RETURN alias become `Alias`;
/// otherwise they stay `Var`. `var.field` is always `Prop`.
#[derive(Debug, Clone, PartialEq)]
pub enum OrderTarget {
    Alias(String),
    Var(String),
    Prop { var: String, field: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderItem {
    pub target: OrderTarget,
    pub descending: bool,
}

// ─── Write statement AST ──────────────────────────────────────────────────────

/// Top-level write statement (CREATE / MATCH…SET / MATCH…DELETE / MERGE).
/// Produced by `parse_write`; executed by `GraphDb::query_write`.
#[derive(Debug, Clone, PartialEq)]
pub enum WriteStatement {
    Create(CreateStmt),
    MatchSet(MatchSetStmt),
    MatchDelete(MatchDeleteStmt),
    Merge(MergeStmt),
}

/// `CREATE (a:L {id: 'x', ...})[-[:T]->(b:L2 {id: 'y', ...})]`
///
/// `nodes` is in encounter order. `edges` reference node vars that appear in
/// `nodes`. Each node must have a string-valued `id` property (used as key).
#[derive(Debug, Clone, PartialEq)]
pub struct CreateStmt {
    pub nodes: Vec<CreateNode>,
    pub edges: Vec<CreateEdge>,
}

/// One node in a CREATE pattern.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateNode {
    /// Optional binding variable (`a` in `(a:Label {…})`).
    pub var: Option<String>,
    pub label: String,
    /// Literal property pairs.  Must include a string-valued `id` field.
    pub props: Vec<(String, core_storage::Value)>,
}

/// One edge in a CREATE pattern, referencing vars from `CreateStmt.nodes`.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateEdge {
    pub src_var: String,
    pub etype: String,
    pub dst_var: String,
}

/// `MATCH patterns [WHERE expr] SET var.field = literal [, …]`
#[derive(Debug, Clone, PartialEq)]
pub struct MatchSetStmt {
    pub matches: Vec<Pattern>,
    pub where_expr: Option<Expr>,
    pub sets: Vec<SetClause>,
}

/// One `var.field = literal` assignment in a SET clause.
#[derive(Debug, Clone, PartialEq)]
pub struct SetClause {
    pub var: String,
    pub field: String,
    pub value: core_storage::Value,
}

/// `MATCH patterns [WHERE expr] DELETE rel_var [, …]`
///
/// Each `EdgeDelete` carries the etype, src node var, and dst node var resolved
/// from the MATCH patterns at parse time so the executor can emit a read query
/// returning node keys without extra pattern-scanning.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchDeleteStmt {
    pub matches: Vec<Pattern>,
    pub where_expr: Option<Expr>,
    pub deletes: Vec<EdgeDelete>,
}

/// One edge variable to delete, with resolved topology.
#[derive(Debug, Clone, PartialEq)]
pub struct EdgeDelete {
    pub rel_var: String,
    pub etype: String,
    pub src_var: String,
    pub dst_var: String,
}

/// `MERGE (n:Label {id: 'x'})` — match-or-create by a single key property.
///
/// Exactly one property is allowed (the key).  More properties, or `ON CREATE
/// SET` / `ON MATCH SET` → named error at parse time.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeStmt {
    pub label: String,
    /// The single property that identifies the node. Value must be a string.
    pub key_field: String,
    pub key_value: core_storage::Value,
}
