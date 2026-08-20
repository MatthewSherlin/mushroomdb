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

#[derive(Debug, Clone, PartialEq)]
pub struct Pattern {
    pub start: NodePat,
    pub chain: Vec<(RelPat, NodePat)>,
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
    Prop { var: String, field: String },
    /// Single aggregate function call. v1 supports only one aggregate per
    /// query (no grouping). Grouped aggregation (`RETURN a, COUNT(*)`) is
    /// rejected at planning time with a clear limitation error.
    Agg { func: AggFunc, arg: AggArg },
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
