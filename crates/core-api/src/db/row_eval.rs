//! Evaluating a write's values row by row, as the statement goes.
//!
//! A `SET n.count = n.count + 1` over several rows must read what the rows
//! before it wrote, and a `SET n.w = n.v` after `SET n.v = 10` must read 10. The
//! read part of a statement runs once, over the state before it, so a value that
//! reads a node's property cannot be taken from that result. Such a value is
//! evaluated here, at the moment its row is written, with every read of a node
//! property answered by [`Overlay`], the statement's own effects so far, before
//! the store. Everything else in the value (parameters, row variables,
//! functions, arithmetic) goes through the executor's own evaluator, so a
//! function means here what it means in a `RETURN`.
//!
//! The overlay is hash-keyed, so a pass over the rows is linear.

use super::*;
use core_query::cypher::{eval_operand, Binding};

/// What the statement has done to one node so far.
pub(super) struct NodeState {
    /// The node exists at this point of the statement.
    pub(super) exists: bool,
    /// The statement created the node (its edges are not in the store).
    pub(super) created_here: bool,
    /// The node's properties at this point, once known.
    pub(super) props: Option<BTreeMap<String, Value>>,
    /// Writes made before `props` was loaded from the store: a value, or
    /// `None` for a removal. Applied over the stored properties when they load.
    pub(super) delta: BTreeMap<String, Option<Value>>,
}

impl NodeState {
    pub(super) fn stored() -> Self {
        NodeState {
            exists: true,
            created_here: false,
            props: None,
            delta: BTreeMap::new(),
        }
    }
}

type EdgeKey = (String, String, String);

/// The statement's own effects, read back by later rows.
#[derive(Default)]
pub(super) struct Overlay {
    pub(super) nodes: HashMap<String, NodeState>,
    edges: HashSet<EdgeKey>,
    /// The edges the statement created, by each endpoint. An entry can name an
    /// edge that is gone from `edges` (its other end was deleted).
    touching: HashMap<String, Vec<EdgeKey>>,
}

impl Overlay {
    pub(super) fn node_created(&mut self, key: &str, props: &[(String, Value)]) {
        self.nodes.insert(
            key.to_string(),
            NodeState {
                exists: true,
                created_here: true,
                props: Some(props.iter().cloned().collect()),
                delta: BTreeMap::new(),
            },
        );
    }

    pub(super) fn note_set(&mut self, key: &str, field: &str, value: &Value) {
        let state = self
            .nodes
            .entry(key.to_string())
            .or_insert_with(NodeState::stored);
        match state.props.as_mut() {
            Some(props) => {
                props.insert(field.to_string(), value.clone());
            }
            None => {
                state.delta.insert(field.to_string(), Some(value.clone()));
            }
        }
    }

    pub(super) fn note_remove(&mut self, key: &str, field: &str) {
        let state = self
            .nodes
            .entry(key.to_string())
            .or_insert_with(NodeState::stored);
        match state.props.as_mut() {
            Some(props) => {
                props.remove(field);
            }
            None => {
                state.delta.insert(field.to_string(), None);
            }
        }
    }

    pub(super) fn edge_created(&mut self, etype: &str, src: &str, dst: &str) {
        let edge: EdgeKey = (etype.to_string(), src.to_string(), dst.to_string());
        if self.edges.insert(edge.clone()) {
            self.touching
                .entry(src.to_string())
                .or_default()
                .push(edge.clone());
            if src != dst {
                self.touching.entry(dst.to_string()).or_default().push(edge);
            }
        }
    }

    pub(super) fn node_deleted(&mut self, key: &str) {
        for edge in self.touching.remove(key).unwrap_or_default() {
            self.edges.remove(&edge);
        }
        self.nodes.insert(
            key.to_string(),
            NodeState {
                exists: false,
                created_here: false,
                props: None,
                delta: BTreeMap::new(),
            },
        );
    }

    fn has_pending_edge(&self, key: &str) -> bool {
        self.touching
            .get(key)
            .is_some_and(|edges| edges.iter().any(|e| self.edges.contains(e)))
    }
}

impl<F: Fs> GraphDb<F> {
    /// Whether the node `key` exists at this point of the statement.
    pub(super) fn uw_exists(&self, ov: &Overlay, key: &str) -> bool {
        match ov.nodes.get(key) {
            Some(state) => state.exists,
            None => self.has_node(key),
        }
    }

    /// The properties of `key` at this point of the statement.
    pub(super) fn uw_props<'o>(
        &self,
        ov: &'o mut Overlay,
        key: &str,
    ) -> &'o mut BTreeMap<String, Value> {
        let state = ov
            .nodes
            .entry(key.to_string())
            .or_insert_with(NodeState::stored);
        let delta = std::mem::take(&mut state.delta);
        state.props.get_or_insert_with(|| {
            let mut props = self.node_info(key).map(|n| n.props).unwrap_or_default();
            for (field, value) in delta {
                match value {
                    Some(v) => props.insert(field, v),
                    None => props.remove(&field),
                };
            }
            props
        })
    }

    /// Whether the edge exists at this point of the statement.
    pub(super) fn uw_edge_exists(&self, ov: &Overlay, etype: &str, src: &str, dst: &str) -> bool {
        if ov
            .edges
            .contains(&(etype.to_string(), src.to_string(), dst.to_string()))
        {
            return true;
        }
        let fresh = |key: &str| ov.nodes.get(key).is_some_and(|n| n.created_here);
        if fresh(src) || fresh(dst) {
            return false;
        }
        self.uw_exists(ov, src)
            && self.uw_exists(ov, dst)
            && self.merge_edge_present(etype, src, dst)
    }

    /// Whether any edge touches `key` at this point of the statement.
    pub(super) fn uw_has_edges(&self, ov: &Overlay, key: &str) -> bool {
        if ov.has_pending_edge(key) {
            return true;
        }
        if ov.nodes.get(key).is_some_and(|n| n.created_here) {
            return false;
        }
        let Some(id) = self.ids.get(key) else {
            return false;
        };
        let tv = self.topo_view();
        tv.etypes().any(|et| {
            !tv.neighbors(et, Direction::Out, id).is_empty()
                || !tv.neighbors(et, Direction::In, id).is_empty()
        })
    }

    /// Record in the overlay what `SET n = {…}` / `SET n += {…}` does to `key`.
    pub(super) fn uw_apply_map(
        &self,
        ov: &mut Overlay,
        key: &str,
        merge: bool,
        entries: &MapEntries,
    ) {
        let existing = self.uw_props(ov, key).clone();
        let writes = plan_map_write(key, &existing, merge, entries, &|field| {
            self.view_store.view_for_prop(field).is_some()
        });
        for write in writes {
            match write {
                MapWrite::Set { field, value } => ov.note_set(key, &field, &value),
                MapWrite::Remove { field } => ov.note_remove(key, &field),
            }
        }
    }

    /// A property of a node as a read of it would see it at this point of the
    /// statement: the stored or written value, else the identity the engine
    /// exposes as a property (`key`, `id`, `label`).
    fn uw_read_prop(&self, ov: &mut Overlay, key: &str, field: &str) -> Option<Value> {
        if !self.uw_exists(ov, key) {
            return None;
        }
        if let Some(v) = self.uw_props(ov, key).get(field) {
            return Some(v.clone());
        }
        match field {
            "key" | "id" => Some(Value::Str(key.to_string())),
            "label" => self.node_info(key).map(|n| Value::Str(n.label)),
            _ => None,
        }
    }

    /// Evaluate `operand` for one row at this point of the statement.
    ///
    /// `node_key` answers for a name: `Some(Some(key))` a node, `Some(None)` a
    /// node variable that is null in this row (an `OPTIONAL MATCH` that found
    /// nothing), `None` not a node. `values` are the row's other variables.
    pub(super) fn uw_eval(
        &self,
        ov: &mut Overlay,
        operand: &Operand,
        node_key: &dyn Fn(&str) -> Option<Option<String>>,
        values: &[(&str, Value)],
        params: &BTreeMap<String, Value>,
    ) -> Result<Option<Value>> {
        let mut stored: Vec<(String, u32)> = Vec::new();
        let rewritten = map_leaves(operand, &mut |leaf| match leaf {
            Operand::Prop { var, field } => {
                let key = node_key(var)?;
                Some(match key {
                    Some(key) => match self.uw_read_prop(ov, &key, field) {
                        Some(v) => Operand::Lit(v),
                        None => Operand::Lit(Value::Null),
                    },
                    None => Operand::Lit(Value::Null),
                })
            }
            Operand::Var(var) => {
                let key = node_key(var)?;
                Some(match key {
                    Some(key) => match self.ids.get(&key).filter(|_| self.uw_exists(ov, &key)) {
                        Some(id) if !ov.nodes.get(&key).is_some_and(|n| n.created_here) => {
                            stored.push((var.clone(), id));
                            Operand::Var(var.clone())
                        }
                        _ => Operand::Lit(Value::Str(key)),
                    },
                    None => Operand::Lit(Value::Null),
                })
            }
            _ => None,
        });
        let mut bindings: Vec<(&str, Binding)> = values
            .iter()
            .map(|(name, v)| (*name, Binding::Value(v.clone())))
            .collect();
        for (name, id) in &stored {
            bindings.push((name.as_str(), Binding::Node(*id)));
        }
        let mask_opt = self.pending_write_authz.as_ref().map(|a| a.mask.clone());
        let evaluated = match mask_opt {
            Some(ref mask) => eval_operand(
                &self.view_masked(mask),
                &rewritten,
                &Params(params),
                &bindings,
            ),
            None => eval_operand(&self.view(), &rewritten, &Params(params), &bindings),
        };
        evaluated.map_err(|e| GraphError::QueryError {
            detail: format!("execute: {e}"),
        })
    }
}

/// Whether `operand` reads a node variable in `nodes`: a property of it, or the
/// bare name.
pub(super) fn reads_node(operand: &Operand, nodes: &[String]) -> bool {
    let mut found = false;
    map_leaves(operand, &mut |leaf| {
        let name = match leaf {
            Operand::Prop { var, .. } | Operand::Var(var) => var,
            _ => return None,
        };
        if nodes.iter().any(|n| n == name) {
            found = true;
        }
        None
    });
    found
}

/// A copy of `operand` in which every `Prop` and `Var` leaf for which `leaf`
/// returns `Some(replacement)` is replaced. The walk covers the conditions of a
/// `CASE` as well.
pub(super) fn map_leaves(
    operand: &Operand,
    leaf: &mut dyn FnMut(&Operand) -> Option<Operand>,
) -> Operand {
    match operand {
        Operand::Prop { .. } | Operand::Var(_) => leaf(operand).unwrap_or_else(|| operand.clone()),
        Operand::Lit(_) | Operand::Param(_) => operand.clone(),
        Operand::FuncCall { name, args } => Operand::FuncCall {
            name: name.clone(),
            args: args.iter().map(|a| map_leaves(a, leaf)).collect(),
        },
        Operand::BinArith { op, left, right } => Operand::BinArith {
            op: op.clone(),
            left: Box::new(map_leaves(left, leaf)),
            right: Box::new(map_leaves(right, leaf)),
        },
        Operand::Index { base, index } => Operand::Index {
            base: Box::new(map_leaves(base, leaf)),
            index: Box::new(map_leaves(index, leaf)),
        },
        Operand::ListLit(items) => {
            Operand::ListLit(items.iter().map(|i| map_leaves(i, leaf)).collect())
        }
        Operand::MapLit(entries) => Operand::MapLit(
            entries
                .iter()
                .map(|(k, v)| (k.clone(), map_leaves(v, leaf)))
                .collect(),
        ),
        Operand::Case { branches, default } => Operand::Case {
            branches: branches
                .iter()
                .map(|(cond, value)| (map_expr(cond, leaf), map_leaves(value, leaf)))
                .collect(),
            default: default.as_ref().map(|d| Box::new(map_leaves(d, leaf))),
        },
        // The list (and `reduce`'s initial value) is read in the enclosing
        // scope; the body is read with the comprehension's own variables
        // bound, so a leaf naming one of them is left alone.
        Operand::ListComprehension {
            var,
            list,
            filter,
            map,
        } => {
            let list = Box::new(map_leaves(list, leaf));
            let locals = [var];
            let mut inner = shadowing(&locals, leaf);
            Operand::ListComprehension {
                var: var.clone(),
                list,
                filter: filter.as_ref().map(|f| Box::new(map_expr(f, &mut inner))),
                map: Box::new(map_leaves(map, &mut inner)),
            }
        }
        Operand::Reduce {
            acc_var,
            init,
            iter_var,
            list,
            body,
        } => {
            let init = Box::new(map_leaves(init, leaf));
            let list = Box::new(map_leaves(list, leaf));
            let locals = [acc_var, iter_var];
            let mut inner = shadowing(&locals, leaf);
            Operand::Reduce {
                acc_var: acc_var.clone(),
                init,
                iter_var: iter_var.clone(),
                list,
                body: Box::new(map_leaves(body, &mut inner)),
            }
        }
    }
}

/// `leaf`, except that a `Prop` or `Var` leaf naming one of `locals` is kept:
/// inside a list comprehension or `reduce` that name is the local variable.
fn shadowing<'a>(
    locals: &'a [&'a String],
    leaf: &'a mut dyn FnMut(&Operand) -> Option<Operand>,
) -> impl FnMut(&Operand) -> Option<Operand> + 'a {
    move |operand| {
        let name = match operand {
            Operand::Prop { var, .. } | Operand::Var(var) => var,
            _ => return leaf(operand),
        };
        if locals.contains(&name) {
            None
        } else {
            leaf(operand)
        }
    }
}

fn map_expr(expr: &Expr, leaf: &mut dyn FnMut(&Operand) -> Option<Operand>) -> Expr {
    match expr {
        Expr::And(a, b) => Expr::And(Box::new(map_expr(a, leaf)), Box::new(map_expr(b, leaf))),
        Expr::Or(a, b) => Expr::Or(Box::new(map_expr(a, leaf)), Box::new(map_expr(b, leaf))),
        Expr::Not(a) => Expr::Not(Box::new(map_expr(a, leaf))),
        Expr::Cmp { lhs, op, rhs } => Expr::Cmp {
            lhs: map_leaves(lhs, leaf),
            op: op.clone(),
            rhs: map_leaves(rhs, leaf),
        },
        Expr::Truthy(o) => Expr::Truthy(map_leaves(o, leaf)),
        Expr::IsNull(o) => Expr::IsNull(map_leaves(o, leaf)),
        Expr::IsNotNull(o) => Expr::IsNotNull(map_leaves(o, leaf)),
        Expr::In { expr, list } => Expr::In {
            expr: map_leaves(expr, leaf),
            list: list.iter().map(|o| map_leaves(o, leaf)).collect(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pending-edge check of a plain `DELETE` is one lookup, not a scan of
    /// every edge the statement made: 100,000 edges and 100,000 checks finish
    /// at once (a scan would be 10^10 comparisons).
    #[test]
    fn the_pending_edge_check_is_a_lookup() {
        let mut ov = Overlay::default();
        for i in 0..100_000 {
            ov.edge_created("T", &format!("a{i}"), &format!("b{i}"));
        }
        let started = std::time::Instant::now();
        let touched = (0..100_000)
            .filter(|i| ov.has_pending_edge(&format!("a{i}")))
            .count();
        assert_eq!(touched, 100_000);
        assert!(!ov.has_pending_edge("nobody"));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn deleting_a_node_drops_the_pending_edges_it_held() {
        let mut ov = Overlay::default();
        ov.edge_created("T", "a", "b");
        assert!(ov.has_pending_edge("b"));
        ov.node_deleted("a");
        assert!(!ov.has_pending_edge("a"));
        assert!(!ov.has_pending_edge("b"), "the edge is gone with its end");
    }
}
