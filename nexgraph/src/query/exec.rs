//! Statement executor: pattern matching, expression evaluation, projection
//! and write clauses (which feed a `Txn`).

use super::parser::*;
use super::{Params, QueryResult, Stats};
use crate::algo;
use crate::db::Txn;
use crate::error::{Error, Result};
use crate::store::{Direction, EdgeFast, View};
use crate::value::{Id, Props, Value};
use std::cell::Cell;
use std::collections::{HashMap, HashSet};

/// Variable bindings: a tiny association list. With a handful of variables a
/// linear scan beats hashing, clones are cheap, and bindings can be undone by
/// truncation (later entries shadow earlier ones).
#[derive(Clone, Default)]
struct Env(Vec<(std::rc::Rc<str>, Value)>);

impl Env {
    fn new() -> Self {
        Env(Vec::new())
    }
    fn get(&self, k: &str) -> Option<&Value> {
        self.0.iter().rev().find(|(n, _)| &**n == k).map(|(_, v)| v)
    }
    fn contains_key(&self, k: &str) -> bool {
        self.get(k).is_some()
    }
    fn len(&self) -> usize {
        self.0.len()
    }
    fn truncate(&mut self, n: usize) {
        self.0.truncate(n);
    }
    fn push(&mut self, k: &std::rc::Rc<str>, v: Value) {
        self.0.push((k.clone(), v));
    }
    /// Replace an existing binding or add one.
    fn insert(&mut self, k: &str, v: Value) {
        match self.0.iter_mut().rev().find(|(n, _)| **n == *k) {
            Some(slot) => slot.1 = v,
            None => self.0.push((k.into(), v)),
        }
    }
}

type Sink<'s> = &'s mut dyn FnMut(&mut Env) -> Result<bool>;
static NULL: Value = Value::Null;

/// Canonical byte key for grouping / DISTINCT (type-tagged, so Int(1) != Float(1.0)).
fn hkey(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Null => out.push(0),
        Value::Bool(b) => out.extend([1, *b as u8]),
        Value::Int(i) => {
            out.push(2);
            out.extend(i.to_le_bytes());
        }
        Value::Float(f) => {
            out.push(3);
            out.extend(f.to_bits().to_le_bytes());
        }
        Value::Str(s) => {
            out.push(4);
            out.extend((s.len() as u64).to_le_bytes());
            out.extend(s.as_bytes());
        }
        Value::List(l) => {
            out.push(5);
            out.extend((l.len() as u64).to_le_bytes());
            l.iter().for_each(|x| hkey(x, out));
        }
        Value::Vector(x) => {
            out.push(6);
            out.extend((x.len() as u64).to_le_bytes());
            x.iter().for_each(|f| out.extend(f.to_bits().to_le_bytes()));
        }
        Value::Node(i) => {
            out.push(7);
            out.extend(i.to_le_bytes());
        }
        Value::Edge(i) => {
            out.push(8);
            out.extend(i.to_le_bytes());
        }
    }
}
fn hkey_all(vs: &[Value]) -> Vec<u8> {
    let mut b = Vec::with_capacity(vs.len() * 9);
    vs.iter().for_each(|v| hkey(v, &mut b));
    b
}

const TRAVERSAL_BUDGET: u64 = 5_000_000;
const AGGS: [&str; 8] = ["count", "sum", "avg", "min", "max", "collect", "stdev", "median"];

fn qerr<T>(m: impl Into<String>) -> Result<T> {
    Err(Error::Query(m.into()))
}

struct Ctx<'a, 'v> {
    view: &'a View<'v>,
    params: &'a Params,
    budget: Cell<u64>,
}

pub fn exec(view: &View, st: &Statement, params: &Params, mut txn: Option<&mut Txn>) -> Result<QueryResult> {
    let cx = Ctx { view, params, budget: Cell::new(0) };
    let mut stats = Stats::default();
    let mut rows: Vec<Env> = vec![Env::new()];
    let mut out: Option<(Vec<String>, Vec<Vec<Value>>)> = None;
    let mut call_cols: Option<Vec<String>> = None;
    let mut merged: HashMap<Vec<u8>, Vec<(std::rc::Rc<str>, Value)>> = HashMap::new();

    let mut skip_next = false;
    for (ci, clause) in st.clauses.iter().enumerate() {
        if std::mem::take(&mut skip_next) {
            continue;
        }
        match clause {
            Clause::Match { patterns, wher, optional } => {
                let pushed = cx.index_pushdown(patterns, wher.as_ref());
                let patterns = pushed.as_ref().unwrap_or(patterns);
                let fuse = match (*optional, st.clauses.get(ci + 1)) {
                    (false, Some(Clause::Return(rc))) => Some((rc, None, false)),
                    (false, Some(Clause::With { rc, wher })) => Some((rc, wher.as_ref(), true)),
                    _ => None,
                };
                if let Some((rc, with_where, is_with)) = fuse {
                    // Fused MATCH ... RETURN / WITH: stream matches straight into the
                    // projector/aggregator, never materialising the rows.
                    let mut pj = Projector::new(&cx, rc)?;
                    if !(rows.len() == 1 && rows[0].len() == 0 && cx.try_factorized(patterns, wher.as_ref(), &mut pj)?) {
                        for mut env in std::mem::take(&mut rows) {
                            let go = cx.match_all(patterns, wher.as_ref(), &mut env, &mut |e| pj.push(&cx, e))?;
                            if !go {
                                break;
                            }
                        }
                    }
                    let (cols, data) = pj.finish(&cx)?;
                    if is_with {
                        rows = with_rows(&cx, &cols, data, with_where)?;
                        skip_next = true;
                        continue;
                    }
                    out = Some((cols, data));
                    break;
                }
                let mut next = Vec::new();
                for mut env in std::mem::take(&mut rows) {
                    let before = next.len();
                    cx.match_all(patterns, wher.as_ref(), &mut env, &mut |e| {
                        next.push(e.clone());
                        Ok(true)
                    })?;
                    if *optional && next.len() == before {
                        // no match: keep the row, with the pattern's new variables bound to null
                        let mut e2 = env.clone();
                        for pt in patterns.iter() {
                            for v in pattern_vars(pt) {
                                if !e2.contains_key(&v) {
                                    e2.insert(&v, Value::Null);
                                }
                            }
                        }
                        next.push(e2);
                    }
                }
                rows = next;
            }
            Clause::With { rc, wher } => {
                let mut pj = Projector::new(&cx, rc)?;
                for mut env in std::mem::take(&mut rows) {
                    if !pj.push(&cx, &mut env)? {
                        break;
                    }
                }
                let (cols, data) = pj.finish(&cx)?;
                rows = with_rows(&cx, &cols, data, wher.as_ref())?;
            }
            Clause::Unwind { expr, var } => {
                let mut next = Vec::new();
                for env in std::mem::take(&mut rows) {
                    match cx.eval(expr, &env)? {
                        Value::Null => {}
                        Value::List(items) => {
                            for it in items {
                                cx.tick()?;
                                let mut e = env.clone();
                                e.insert(var, it);
                                next.push(e);
                            }
                        }
                        other => return qerr(format!("UNWIND needs a list, got {}", other.type_name())),
                    }
                }
                rows = next;
            }
            Clause::Merge { path, on_create, on_match } => {
                let t = txn.as_deref_mut().ok_or_else(|| Error::Query("write in read-only context".into()))?;
                let pat = [Pattern::Path(path.clone())];
                let mut next = Vec::new();
                for mut env in std::mem::take(&mut rows) {
                    let mut found: Vec<Env> = Vec::new();
                    cx.match_all(&pat, None, &mut env, &mut |e| {
                        found.push(e.clone());
                        Ok(true)
                    })?;
                    if found.is_empty() {
                        // The snapshot cannot see what this statement already created, so remember it:
                        // UNWIND ['a','a'] AS n MERGE (:P {name: n}) must create one node, not two.
                        let key = cx.merge_key(path, &env)?;
                        if let Some(binds) = merged.get(&key) {
                            for (k, v) in binds {
                                env.insert(k, v.clone());
                            }
                            do_set(&cx, t, &env, on_match, &mut stats)?;
                        } else {
                            cx.create_item(&CreateItem::Path(path.clone()), &mut env, t, &mut stats)?;
                            let binds: Vec<(std::rc::Rc<str>, Value)> = path_vars(path).into_iter().filter_map(|v| env.get(&v).cloned().map(|x| (v, x))).collect();
                            merged.insert(key, binds);
                            do_set(&cx, t, &env, on_create, &mut stats)?;
                        }
                        next.push(env);
                    } else {
                        for e in found {
                            do_set(&cx, t, &e, on_match, &mut stats)?;
                            next.push(e);
                        }
                    }
                }
                rows = next;
            }
            Clause::Create(items) => {
                let t = txn.as_deref_mut().ok_or_else(|| Error::Query("write in read-only context".into()))?;
                for env in rows.iter_mut() {
                    for it in items {
                        cx.create_item(it, env, t, &mut stats)?;
                    }
                }
            }
            Clause::Set(items) => {
                let t = txn.as_deref_mut().ok_or_else(|| Error::Query("write in read-only context".into()))?;
                for env in &rows {
                    do_set(&cx, t, env, items, &mut stats)?;
                }
            }
            Clause::Delete(vars) => {
                let t = txn.as_deref_mut().ok_or_else(|| Error::Query("write in read-only context".into()))?;
                let mut seen = HashSet::new();
                for env in &rows {
                    for v in vars {
                        match env.get(v) {
                            Some(Value::Node(id)) | Some(Value::Edge(id)) => {
                                if seen.insert(*id) {
                                    t.delete(*id);
                                    stats.deleted += 1;
                                }
                            }
                            Some(Value::Null) => {}
                            _ => return qerr(format!("cannot delete {v}")),
                        }
                    }
                }
            }
            Clause::CreatePropIndex { label, key } => {
                let t = txn.as_deref_mut().ok_or_else(|| Error::Query("write in read-only context".into()))?;
                t.create_property_index(label, key);
                stats.indexes_created += 1;
            }
            Clause::CreateVectorIndex { label, key, dim, metric } => {
                let t = txn.as_deref_mut().ok_or_else(|| Error::Query("write in read-only context".into()))?;
                t.create_vector_index(label, key, *dim, *metric);
                stats.indexes_created += 1;
            }
            Clause::Call { name, args, yields } => {
                let mut next = Vec::new();
                for env in &rows {
                    let argv = args.iter().map(|a| cx.eval(a, env)).collect::<Result<Vec<_>>>()?;
                    let (cols, res) = algo::call(view, name, &argv)?;
                    let map: Vec<(usize, String)> = if yields.is_empty() {
                        cols.iter().enumerate().map(|(i, c)| (i, c.to_string())).collect()
                    } else {
                        yields
                            .iter()
                            .map(|(c, a)| {
                                cols.iter()
                                    .position(|x| x == c)
                                    .map(|i| (i, a.clone()))
                                    .ok_or_else(|| Error::Query(format!("{name} has no output column '{c}' (has: {})", cols.join(", "))))
                            })
                            .collect::<Result<_>>()?
                    };
                    call_cols = Some(map.iter().map(|m| m.1.clone()).collect());
                    for r in res {
                        let mut e2 = env.clone();
                        for (i, a) in &map {
                            e2.insert(a, r[*i].clone());
                        }
                        next.push(e2);
                    }
                }
                rows = next;
            }
            Clause::Return(rc) => {
                let mut pj = Projector::new(&cx, rc)?;
                for mut env in std::mem::take(&mut rows) {
                    if !pj.push(&cx, &mut env)? {
                        break;
                    }
                }
                out = Some(pj.finish(&cx)?);
                break;
            }
        }
    }

    let (columns, rows_out) = match out {
        Some(o) => o,
        None => match call_cols {
            Some(cols) => {
                let data = rows.iter().map(|e| cols.iter().map(|c| e.get(c).cloned().unwrap_or(Value::Null)).collect()).collect();
                (cols, data)
            }
            None => (vec![], vec![]),
        },
    };
    Ok(QueryResult { columns, rows: rows_out, stats, ts: 0 })
}

/// Apply `SET var.key = expr` items to one row.
fn do_set(cx: &Ctx, t: &mut Txn, env: &Env, items: &[(String, String, Expr)], stats: &mut Stats) -> Result<()> {
    for (var, key, e) in items {
        let val = cx.eval(e, env)?;
        let target = env.get(var).ok_or_else(|| Error::Query(format!("unknown variable {var}")))?;
        match target {
            Value::Edge(id) if key == "weight" => {
                let w = val.as_f64().ok_or_else(|| Error::Query("weight must be numeric".into()))?;
                t.set_weight(*id, w);
            }
            Value::Node(id) | Value::Edge(id) => {
                if val.is_null() {
                    t.set_props(*id, Props::new(), &[key.as_str()])?;
                } else {
                    t.set_prop(*id, key, val)?;
                }
            }
            Value::Null => continue,
            _ => return qerr(format!("{var} is not a node or edge")),
        }
        stats.props_set += 1;
    }
    Ok(())
}

/// The rows after a WITH: each projected row becomes a fresh set of bindings, then WITH's WHERE filters them.
fn with_rows(cx: &Ctx, cols: &[String], data: Vec<Vec<Value>>, wher: Option<&Expr>) -> Result<Vec<Env>> {
    let mut next = Vec::with_capacity(if wher.is_some() { 0 } else { data.len() });
    let rc: Vec<std::rc::Rc<str>> = cols.iter().map(|c| std::rc::Rc::from(c.as_str())).collect();
    let unique = cols.iter().enumerate().all(|(i, c)| !cols[..i].contains(c));
    let mut scratch = Env::new();
    for r in data {
        let mut e = if wher.is_some() { std::mem::take(&mut scratch) } else { Env::new() };
        e.truncate(0);
        if unique {
            for (c, v) in rc.iter().zip(r) {
                e.push(c, v);
            }
        } else {
            for (c, v) in cols.iter().zip(r) {
                e.insert(c, v);
            }
        }
        if let Some(w) = wher {
            if !matches!(cx.eval(w, &e)?, Value::Bool(true)) {
                scratch = e; // reuse the allocation for the next row
                continue;
            }
        }
        next.push(e);
    }
    Ok(next)
}

/// Variables a path pattern introduces.
fn path_vars(p: &PathPat) -> Vec<std::rc::Rc<str>> {
    p.nodes.iter().filter_map(|n| n.var.clone()).chain(p.edges.iter().filter_map(|e| e.var.clone())).collect()
}

fn pattern_vars(p: &Pattern) -> Vec<std::rc::Rc<str>> {
    match p {
        Pattern::Path(pp) => path_vars(pp),
        Pattern::Hyper { var, .. } => vec![std::rc::Rc::from(var.as_str())],
    }
}

/// Bind `val` to `var`; false when `var` is already bound to something else.
fn bind(env: &mut Env, var: &Option<std::rc::Rc<str>>, val: Value) -> bool {
    if let Some(v) = var {
        match env.get(v) {
            Some(existing) => return existing == &val,
            None => env.push(v, val),
        }
    }
    true
}

impl<'a, 'v> Ctx<'a, 'v> {
    fn tick(&self) -> Result<()> {
        let b = self.budget.get() + 1;
        self.budget.set(b);
        if b > TRAVERSAL_BUDGET {
            return qerr("traversal budget exceeded (query touches too many paths)");
        }
        Ok(())
    }

    // ------------------------------------------------------------ matching

    fn props_ok(&self, props: &[(String, Expr)], body: &crate::model::Body, env: &Env) -> Result<bool> {
        for (k, e) in props {
            let want = self.eval(e, env)?;
            let tmp;
            let have: &Value = match body {
                crate::model::Body::Edge { weight, props, .. } if k == "weight" => match props.get("weight") {
                    Some(v) => v,
                    None => {
                        tmp = Value::Float(*weight);
                        &tmp
                    }
                },
                b => b.props().get(k).unwrap_or(&NULL),
            };
            if !have.loose_eq(&want) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn node_ok(&self, np: &NodePat, id: Id, env: &Env) -> Result<bool> {
        let Some(body) = self.view.node(id) else { return Ok(false) };
        if let Some(l) = &np.label {
            if !body.has_label(l) {
                return Ok(false);
            }
        }
        if let Some(v) = &np.var {
            if let Some(b) = env.get(v) {
                if b != &Value::Node(id) {
                    return Ok(false);
                }
            }
        }
        self.props_ok(&np.props, body, env)
    }

    /// `MATCH (a:L) WHERE a.k = <literal or $param> [AND ...]` is answered from the `(L, k)` property index, exactly
    /// like `MATCH (a:L {k: ...})`. Only top-level `AND` conjuncts qualify (an `OR`/`NOT` could widen the match), and the
    /// WHERE clause still runs afterwards, so the result is unchanged; only the candidate set shrinks. Returns the
    /// rewritten patterns, or `None` when there is nothing to push down.
    fn index_pushdown(&self, patterns: &[Pattern], wher: Option<&Expr>) -> Option<Vec<Pattern>> {
        fn conjuncts<'e>(e: &'e Expr, out: &mut Vec<&'e Expr>) {
            match e {
                Expr::Bin(Bin::And, l, r) => {
                    conjuncts(l, out);
                    conjuncts(r, out);
                }
                o => out.push(o),
            }
        }
        let mut eqs: Vec<(&str, &str, &Expr)> = vec![];
        // (var, key, op read as `var.key op bound`, bound)
        let mut rngs: Vec<(&str, &str, Bin, &Expr)> = vec![];
        let mut cs = vec![];
        conjuncts(wher?, &mut cs);
        for c in cs {
            if let Expr::Bin(op @ (Bin::Eq | Bin::Lt | Bin::Le | Bin::Gt | Bin::Ge), l, r) = c {
                for (flip, (a, b)) in [(false, (l, r)), (true, (r, l))] {
                    if let (Expr::Prop(base, k), Expr::Lit(_) | Expr::Param(_)) = (&**a, &**b) {
                        if let Expr::Var(v) = &**base {
                            if *op == Bin::Eq {
                                eqs.push((v.as_str(), k.as_str(), &**b));
                            } else {
                                // `5 < n.k` is `n.k > 5`
                                let op = match (op, flip) {
                                    (o, false) => *o,
                                    (Bin::Lt, true) => Bin::Gt,
                                    (Bin::Le, true) => Bin::Ge,
                                    (Bin::Gt, true) => Bin::Lt,
                                    _ => Bin::Le,
                                };
                                rngs.push((v.as_str(), k.as_str(), op, &**b));
                            }
                        }
                    }
                }
            }
        }
        if eqs.is_empty() && rngs.is_empty() {
            return None;
        }
        let mut out = patterns.to_vec();
        let mut changed = false;
        for pat in out.iter_mut() {
            let Pattern::Path(path) = pat else { continue };
            for np in path.nodes.iter_mut() {
                let (Some(var), Some(label)) = (&np.var, &np.label) else { continue };
                if np.props.iter().any(|(k, _)| self.view.has_prop_index(label, k)) || !np.ranges.is_empty() {
                    continue;
                }
                if let Some((_, k, e)) = eqs.iter().find(|(v, k, _)| *v == &**var && self.view.has_prop_index(label, k)) {
                    np.props.push((k.to_string(), (*e).clone()));
                    changed = true;
                } else if let Some((_, k, _, _)) = rngs.iter().find(|(v, k, _, _)| *v == &**var && self.view.has_prop_index(label, k)) {
                    // one indexed key per node; every conjunct on that key is kept (the tightest wins in `candidates`)
                    for (_, k2, op, e) in rngs.iter().filter(|(v, k2, _, _)| *v == &**var && k2 == k) {
                        np.ranges.push((k2.to_string(), *op, (*e).clone()));
                    }
                    changed = true;
                }
            }
        }
        changed.then_some(out)
    }

    fn estimate(&self, np: &NodePat, env: &Env) -> usize {
        if let Some(v) = &np.var {
            if env.contains_key(v) {
                return 1;
            }
        }
        match &np.label {
            Some(l) => {
                if np.props.iter().any(|(k, _)| self.view.has_prop_index(l, k)) {
                    2
                } else if !np.ranges.is_empty() {
                    // heuristic: a range is assumed to keep about a quarter of the label
                    (self.view.label_estimate(l) / 4).max(2)
                } else {
                    self.view.label_estimate(l).max(1)
                }
            }
            None => self.view.total_estimate().max(1),
        }
    }

    fn candidates(&self, np: &NodePat, env: &Env) -> Result<Vec<Id>> {
        if let Some(v) = &np.var {
            if let Some(b) = env.get(v) {
                return Ok(match b {
                    Value::Node(id) => vec![*id],
                    _ => vec![],
                });
            }
        }
        if let Some(l) = &np.label {
            for (k, e) in &np.props {
                if self.view.has_prop_index(l, k) {
                    let val = self.eval(e, env)?;
                    // Values the index cannot key (null, lists, vectors) must keep the scan semantics
                    // (`x = null` is true for a missing property), so scan the label and let props_ok decide.
                    if matches!(val, Value::Null | Value::List(_) | Value::Vector(_)) {
                        return Ok(self.view.nodes_with_label(l));
                    }
                    return Ok(self.view.lookup(l, k, &val));
                }
            }
            if let Some((k, _, _)) = np.ranges.first() {
                let (mut lo, mut hi): (Option<(Value, bool)>, Option<(Value, bool)>) = (None, None);
                for (_, op, e) in &np.ranges {
                    let v = self.eval(e, env)?;
                    // Null / list / vector bounds keep the scan semantics (WHERE decides), so do not use the index.
                    if matches!(v, Value::Null | Value::List(_) | Value::Vector(_)) {
                        return Ok(self.view.nodes_with_label(l));
                    }
                    let slot = if matches!(op, Bin::Gt | Bin::Ge) { &mut lo } else { &mut hi };
                    if slot.is_none() {
                        *slot = Some((v, matches!(op, Bin::Ge | Bin::Le)));
                    }
                }
                if let Some(ids) = self.view.lookup_range(l, k, lo.as_ref().map(|(v, i)| (v, *i)), hi.as_ref().map(|(v, i)| (v, *i))) {
                    return Ok(ids);
                }
            }
            let found = self.view.nodes_with_label(l);
            if found.is_empty() && !self.view.edges_with_label(l).is_empty() {
                return Err(Error::Query(format!(
                    "'{l}' is an edge/hyperedge label, not a node label; use MATCH HYPEREDGE h:{l} (records) or MATCH ()-[e:{l}]->() (edges)"
                )));
            }
            return Ok(found);
        }
        Ok(self.view.node_ids().collect())
    }

    /// Factorised aggregation for `MATCH (a)-[e]->(b) RETURN <keys of a>, count(*)`:
    /// when nothing but node `a` is referenced, matches are counted per `a`
    /// without binding variables or visiting rows one by one (Kuzu's
    /// factorisation, Polars' projection pushdown), in parallel for big scans.
    /// Returns false when the query does not have that shape.
    fn try_factorized(&self, patterns: &[Pattern], wher: Option<&Expr>, pj: &mut Projector) -> Result<bool> {
        fn only_var(e: &Expr, a: &Option<std::rc::Rc<str>>) -> bool {
            match e {
                Expr::Lit(_) | Expr::Param(_) => true,
                Expr::Var(v) => a.as_deref() == Some(v.as_str()),
                Expr::Prop(b, _) => only_var(b, a),
                Expr::Not(x) | Expr::Neg(x) | Expr::IsNull(x, _) => only_var(x, a),
                Expr::Bin(_, l, r) => only_var(l, a) && only_var(r, a),
                Expr::List(xs) => xs.iter().all(|x| only_var(x, a)),
                Expr::Case { .. } | Expr::Index(..) => false,
                Expr::Call { name, args, .. } => !AGGS.contains(&name.as_str()) && args.iter().all(|x| only_var(x, a)),
            }
        }
        let ([Pattern::Path(path)], w) = (patterns, wher) else { return Ok(false) };
        let [ep] = path.edges.as_slice() else { return Ok(false) };
        let (n0, n1) = (&path.nodes[0], &path.nodes[1]);
        // count(b) and count(r) equal count(*) here: a matched node or edge is never null
        if ep.varlen.is_some() || !ep.props.is_empty() || !n0.props.is_empty() || !n1.props.is_empty() || !pj.all_counts_of(&[&n0.var, &n1.var, &ep.var]) {
            return Ok(false);
        }
        // a WHERE that mentions only the start node just filters the start nodes
        if let Some(w) = w {
            if !only_var(w, &n0.var) {
                return Ok(false);
            }
        }
        if n1.var.is_some() && n1.var == n0.var || ep.var.is_some() && ep.var == n0.var || ep.var.is_some() && ep.var == n1.var {
            return Ok(false);
        }
        if !pj.key_items.iter().all(|e| only_var(e, &n0.var)) || !pj.rc.order.iter().all(|(e, _)| only_var(e, &n0.var) || matches!(e, Expr::Var(v) if pj.cols.contains(v))) {
            return Ok(false);
        }
        let Some(sp) = self.view.hop_spec(ep.dir, ep.label.as_deref(), n1.label.as_deref()) else { return Ok(true) };
        let csr = match self.view.csr() {
            Some(c) if sp.dir != crate::store::Direction::Any => Some(c),
            _ => None,
        };
        // ungrouped count over the whole graph: one sequential pass, no per-start vectors
        if pj.key_items.is_empty() && w.is_none() {
            if let Some(c) = &csr {
                let src = match &n0.label {
                    Some(l) => match self.view.label_id(l) {
                        Some(id) => Some(id),
                        None => return Ok(true),
                    },
                    None => None,
                };
                let total = c.total(src, &sp);
                let mut env = Env::new();
                pj.push_weighted(self, &mut env, total)?;
                return Ok(true);
            }
        }
        let starts: Vec<Id> = match &n0.label {
            Some(l) => self.view.nodes_with_label(l),
            None => self.view.node_ids().collect(),
        };
        let starts = match w {
            Some(w) => {
                let mut keep = Vec::with_capacity(starts.len());
                let mut env = Env::new();
                for a in starts {
                    env.truncate(0);
                    if let Some(v) = &n0.var {
                        env.push(v, Value::Node(a));
                    }
                    if matches!(self.eval(w, &env)?, Value::Bool(true)) {
                        keep.push(a);
                    }
                }
                keep
            }
            None => starts,
        };
        let counts: Vec<u64> = match &csr {
            Some(c) => starts.iter().map(|&a| c.hop_count(a, &sp)).collect(),
            None => parallel_counts(self.view, &starts, &sp),
        };
        if !pj.key_items.is_empty() {
            pj.reserve_groups(starts.len());
        }
        let mut env = Env::new();
        if pj.key_items.is_empty() {
            let total: u64 = counts.iter().sum();
            pj.push_weighted(self, &mut env, total)?;
        } else {
            // key `a.prop` of the start node: read the property directly instead of going through eval
            let direct: Option<&str> = match pj.key_items.as_slice() {
                [Expr::Prop(b, k)] if matches!(&**b, Expr::Var(v) if n0.var.as_deref() == Some(v.as_str())) => Some(k.as_str()),
                _ => None,
            };
            let var_key = matches!(pj.key_items.as_slice(), [Expr::Var(v)] if n0.var.as_deref() == Some(v.as_str()));
            for (&a, &c) in starts.iter().zip(&counts) {
                if c == 0 {
                    continue;
                }
                if let (true, true) = (var_key, pj.fast_ok()) {
                    // grouping by the start node itself: starts are distinct, so every group is new
                    pj.push_unique_key(Value::Node(a), c);
                    continue;
                }
                if let (Some(k), true) = (direct, pj.fast_ok()) {
                    let v = self.view.node(a).and_then(|b| b.props().get(k).cloned()).unwrap_or(Value::Null);
                    pj.push_key_weighted(v, c);
                    continue;
                }
                env.truncate(0);
                if let Some(v) = &n0.var {
                    env.push(v, Value::Node(a));
                }
                pj.push_weighted(self, &mut env, c)?;
            }
        }
        Ok(true)
    }

    /// Match `patterns[0..]` in sequence, then the WHERE filter, calling `sink`
    /// for every complete binding. Returns false if the sink asked to stop.
    fn match_all(&self, patterns: &[Pattern], wher: Option<&Expr>, env: &mut Env, sink: Sink) -> Result<bool> {
        match patterns.split_first() {
            None => {
                if let Some(w) = wher {
                    if !matches!(self.eval(w, env)?, Value::Bool(true)) {
                        return Ok(true);
                    }
                }
                sink(env)
            }
            Some((p, rest)) => self.match_pattern(p, env, &mut |e| self.match_all(rest, wher, e, &mut *sink)),
        }
    }

    fn match_pattern(&self, p: &Pattern, env: &mut Env, sink: Sink) -> Result<bool> {
        match p {
            Pattern::Hyper { var, label, props } => {
                let ids: Vec<Id> = match (env.get(var), label) {
                    (Some(Value::Edge(id)), _) => vec![*id],
                    (Some(_), _) => vec![],
                    (None, Some(l)) => self.view.edges_with_label(l),
                    (None, None) => self.view.edge_ids().collect(),
                };
                for id in ids {
                    let Some(b) = self.view.edge(id) else { continue };
                    if label.as_ref().map_or(false, |l| !b.has_label(l)) || !self.props_ok(props, b, env)? {
                        continue;
                    }
                    let mark = env.len();
                    env.insert(var, Value::Edge(id));
                    let go = sink(env)?;
                    env.truncate(mark);
                    if !go {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            Pattern::Path(path) => {
                let mut path = path.clone();
                let last = path.nodes.len() - 1;
                if last > 0 && self.estimate(&path.nodes[last], env) < self.estimate(&path.nodes[0], env) {
                    path.nodes.reverse();
                    path.edges.reverse();
                    for e in path.edges.iter_mut() {
                        e.dir = match e.dir {
                            Direction::Out => Direction::In,
                            Direction::In => Direction::Out,
                            Direction::Any => Direction::Any,
                        };
                    }
                }
                let mut used = Vec::new();
                for id in self.candidates(&path.nodes[0], env)? {
                    if !self.node_ok(&path.nodes[0], id, env)? {
                        continue;
                    }
                    let mark = env.len();
                    if bind(env, &path.nodes[0].var, Value::Node(id)) {
                        used.clear();
                        let go = self.step(&path, 0, id, env, &mut used, &mut *sink)?;
                        env.truncate(mark);
                        if !go {
                            return Ok(false);
                        }
                    } else {
                        env.truncate(mark);
                    }
                }
                Ok(true)
            }
        }
    }

    fn edge_ok(&self, ep: &EdgePat, body: &crate::model::Body, env: &Env) -> Result<bool> {
        if let Some(l) = &ep.label {
            if !body.has_label(l) {
                return Ok(false);
            }
        }
        self.props_ok(&ep.props, body, env)
    }

    fn step(&self, path: &PathPat, i: usize, cur: Id, env: &mut Env, used: &mut Vec<Id>, sink: Sink) -> Result<bool> {
        if i == path.edges.len() {
            return sink(env);
        }
        let ep = &path.edges[i];
        match ep.varlen {
            None => {
                let np = &path.nodes[i + 1];
                // Dictionary-encoded labels: resolve once per step. A label never
                // used by any entity cannot match.
                let elab = match &ep.label {
                    Some(l) => match self.view.label_id(l) {
                        Some(x) => Some(x),
                        None => return Ok(true),
                    },
                    None => None,
                };
                let nlab = match &np.label {
                    Some(l) => match self.view.label_id(l) {
                        Some(x) => Some(x),
                        None => return Ok(true),
                    },
                    None => None,
                };
                let simple_edge = ep.props.is_empty();
                let simple_node = np.props.is_empty();
                for &eid in self.view.adj_ids(cur, ep.dir) {
                    if used.contains(&eid) {
                        continue;
                    }
                    let mut visit = |v: Id, env: &mut Env, used: &mut Vec<Id>| -> Result<bool> {
                        self.tick()?;
                        let ok = if simple_node {
                            match self.view.node_fast(v, nlab) {
                                Some(b) => b && np.var.as_ref().map_or(true, |x| env.get(x).map_or(true, |b| b == &Value::Node(v))),
                                None => self.node_ok(np, v, env)?,
                            }
                        } else {
                            self.node_ok(np, v, env)?
                        };
                        if !ok {
                            return Ok(true);
                        }
                        let mark = env.len();
                        let mut go = true;
                        if bind(env, &ep.var, Value::Edge(eid)) && bind(env, &np.var, Value::Node(v)) {
                            used.push(eid);
                            go = self.step(path, i + 1, v, env, used, &mut *sink)?;
                            used.pop();
                        }
                        env.truncate(mark);
                        Ok(go)
                    };
                    match (simple_edge, self.view.edge_fast(eid)) {
                        (_, EdgeFast::Gone) => {}
                        (true, EdgeFast::Binary { lab, t, h }) => {
                            if elab.map_or(false, |l| l != lab) {
                                continue;
                            }
                            let v = if t == h {
                                if t == cur {
                                    t
                                } else {
                                    continue;
                                }
                            } else if ep.dir != Direction::In && t == cur {
                                h
                            } else if ep.dir != Direction::Out && h == cur {
                                t
                            } else {
                                continue;
                            };
                            if !visit(v, env, used)? {
                                return Ok(false);
                            }
                        }
                        _ => {
                            let Some(body) = self.view.edge(eid) else { continue };
                            if !self.edge_ok(ep, body, env)? {
                                continue;
                            }
                            let go = self.view.cross_each(cur, body, ep.dir, |v| visit(v, env, used))?;
                            if !go {
                                return Ok(false);
                            }
                        }
                    }
                }
                Ok(true)
            }
            Some((min, max)) => {
                let mut acc = Vec::new();
                self.walk(path, i, cur, 0, min, max, env, used, &mut acc, sink)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn walk(&self, path: &PathPat, i: usize, cur: Id, depth: usize, min: usize, max: usize, env: &mut Env, used: &mut Vec<Id>, acc: &mut Vec<Value>, sink: Sink) -> Result<bool> {
        let ep = &path.edges[i];
        if depth >= min && self.node_ok(&path.nodes[i + 1], cur, env)? {
            let mark = env.len();
            let mut go = true;
            if bind(env, &ep.var, Value::List(acc.clone())) && bind(env, &path.nodes[i + 1].var, Value::Node(cur)) {
                go = self.step(path, i + 1, cur, env, used, &mut *sink)?;
            }
            env.truncate(mark);
            if !go {
                return Ok(false);
            }
        }
        if depth < max {
            for (eid, body) in self.view.incident_dir(cur, ep.dir) {
                if used.contains(&eid) || !self.edge_ok(ep, body, env)? {
                    continue;
                }
                let go = self.view.cross_each(cur, body, ep.dir, |v| -> Result<bool> {
                    self.tick()?;
                    used.push(eid);
                    if ep.var.is_some() {
                        acc.push(Value::Edge(eid));
                    }
                    let go = self.walk(path, i, v, depth + 1, min, max, env, used, acc, &mut *sink);
                    if ep.var.is_some() {
                        acc.pop();
                    }
                    used.pop();
                    go
                })?;
                if !go {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    // ------------------------------------------------------------- create

    /// Identity of a MERGE pattern for this row: bound variables by id, new parts by label and properties.
    fn merge_key(&self, path: &PathPat, env: &Env) -> Result<Vec<u8>> {
        let mut parts: Vec<Value> = Vec::new();
        let props = |ps: &[(String, Expr)], parts: &mut Vec<Value>| -> Result<()> {
            let mut kv: Vec<(&String, Value)> = ps.iter().map(|(k, e)| Ok((k, self.eval(e, env)?))).collect::<Result<_>>()?;
            kv.sort_by(|a, b| a.0.cmp(b.0));
            for (k, v) in kv {
                if v.is_null() {
                    return qerr(format!("MERGE property '{k}' is null"));
                }
                parts.push(Value::Str(k.clone()));
                parts.push(v);
            }
            Ok(())
        };
        for n in &path.nodes {
            match n.var.as_ref().and_then(|v| env.get(v)) {
                Some(v) => parts.push(v.clone()),
                None => {
                    parts.push(Value::Str(n.label.clone().unwrap_or_default()));
                    props(&n.props, &mut parts)?;
                }
            }
        }
        for e in &path.edges {
            parts.push(Value::Str(format!("{:?}{}", e.dir, e.label.clone().unwrap_or_default())));
            props(&e.props, &mut parts)?;
        }
        Ok(hkey_all(&parts))
    }

    fn eval_props(&self, props: &[(String, Expr)], env: &Env) -> Result<(Props, Option<f64>)> {
        let mut p = Props::new();
        let mut weight = None;
        for (k, e) in props {
            let v = self.eval(e, env)?;
            if v.is_null() {
                continue;
            }
            p.insert(k.clone(), v);
        }
        if let Some(w) = p.get("weight") {
            weight = w.as_f64();
        }
        Ok((p, weight))
    }

    fn create_item(&self, it: &CreateItem, env: &mut Env, t: &mut Txn, stats: &mut Stats) -> Result<()> {
        match it {
            CreateItem::Path(path) => {
                let mut ids = Vec::new();
                for np in &path.nodes {
                    let existing = np.var.as_ref().and_then(|v| env.get(v)).cloned();
                    let id = match existing {
                        Some(Value::Node(id)) => id,
                        Some(_) => return qerr("variable is not a node"),
                        None => {
                            let (props, _) = self.eval_props(&np.props, env)?;
                            let labels: Vec<&str> = np.label.as_deref().into_iter().collect();
                            let id = t.create_node(&labels, props)?;
                            stats.nodes_created += 1;
                            if let Some(v) = &np.var {
                                env.insert(v, Value::Node(id));
                            }
                            id
                        }
                    };
                    ids.push(id);
                }
                for (i, ep) in path.edges.iter().enumerate() {
                    if ep.varlen.is_some() {
                        return qerr("variable-length edges cannot be created");
                    }
                    let label = ep.label.as_deref().ok_or_else(|| Error::Query("created edges need a label".into()))?;
                    let (a, b) = match ep.dir {
                        Direction::Out => (ids[i], ids[i + 1]),
                        Direction::In => (ids[i + 1], ids[i]),
                        Direction::Any => return qerr("created edges need a direction (-> or <-)"),
                    };
                    let (mut props, w) = self.eval_props(&ep.props, env)?;
                    if w.is_some() {
                        props.remove("weight");
                    }
                    let id = t.create_edge(label, a, b, w.unwrap_or(1.0), props)?;
                    stats.edges_created += 1;
                    if let Some(v) = &ep.var {
                        env.insert(v, Value::Edge(id));
                    }
                }
            }
            CreateItem::Hyper { var, label, props, tail, head } => {
                let to_ids = |es: &[Expr], env: &Env| -> Result<Vec<Id>> {
                    es.iter()
                        .map(|e| match self.eval(e, env)? {
                            Value::Node(id) => Ok(id),
                            other => qerr(format!("hyperedge members must be nodes, got {}", other.type_name())),
                        })
                        .collect()
                };
                let (tl, hd) = (to_ids(tail, env)?, to_ids(head, env)?);
                let (mut p, w) = self.eval_props(props, env)?;
                if w.is_some() {
                    p.remove("weight");
                }
                let id = t.create_hyperedge(label, &tl, &hd, w.unwrap_or(1.0), p)?;
                stats.edges_created += 1;
                if let Some(v) = var {
                    env.insert(v, Value::Edge(id));
                }
            }
        }
        Ok(())
    }

    // ---------------------------------------------------------- expressions

    fn eval(&self, e: &Expr, env: &Env) -> Result<Value> {
        Ok(match e {
            Expr::Lit(v) => v.clone(),
            Expr::Param(p) => self.params.get(p).cloned().ok_or_else(|| Error::Query(format!("missing parameter ${p}")))?,
            Expr::Var(v) => env.get(v).cloned().ok_or_else(|| Error::Query(format!("unknown variable '{v}'")))?,
            Expr::List(items) => Value::List(items.iter().map(|i| self.eval(i, env)).collect::<Result<_>>()?),
            Expr::Prop(base, key) => match self.eval(base, env)? {
                Value::Null => Value::Null,
                Value::Node(id) => self.view.node(id).and_then(|b| b.props().get(key).cloned()).unwrap_or(Value::Null),
                Value::Edge(id) => match self.view.edge(id) {
                    Some(b) => match (b, key.as_str()) {
                        (crate::model::Body::Edge { props, .. }, "weight") if props.contains_key("weight") => props["weight"].clone(),
                        (crate::model::Body::Edge { weight, .. }, "weight") => Value::Float(*weight),
                        _ => b.props().get(key).cloned().unwrap_or(Value::Null),
                    },
                    None => Value::Null,
                },
                other => return qerr(format!("cannot read property '{key}' of {}", other.type_name())),
            },
            Expr::Not(x) => match self.eval(x, env)? {
                Value::Bool(b) => Value::Bool(!b),
                Value::Null => Value::Null,
                o => return qerr(format!("NOT needs a boolean, got {}", o.type_name())),
            },
            Expr::Neg(x) => match self.eval(x, env)? {
                Value::Int(i) => Value::Int(i.checked_neg().ok_or_else(|| Error::Query("overflow".into()))?),
                Value::Float(f) => Value::Float(-f),
                Value::Null => Value::Null,
                o => return qerr(format!("cannot negate {}", o.type_name())),
            },
            Expr::IsNull(x, neg) => Value::Bool(self.eval(x, env)?.is_null() != *neg),
            Expr::Bin(op, l, r) => self.binop(*op, l, r, env)?,
            Expr::Case { operand, whens, els } => {
                let subject = match operand {
                    Some(o) => Some(self.eval(o, env)?),
                    None => None,
                };
                for (w, t) in whens {
                    let w = self.eval(w, env)?;
                    let hit = match &subject {
                        Some(sv) => !sv.is_null() && !w.is_null() && sv.loose_eq(&w),
                        None => matches!(w, Value::Bool(true)),
                    };
                    if hit {
                        return self.eval(t, env);
                    }
                }
                match els {
                    Some(e) => self.eval(e, env)?,
                    None => Value::Null,
                }
            }
            Expr::Index(base, idx) => match (self.eval(base, env)?, self.eval(idx, env)?) {
                (Value::Null, _) | (_, Value::Null) => Value::Null,
                (Value::List(l), Value::Int(i)) => {
                    let n = l.len() as i64;
                    let k = if i < 0 { n + i } else { i };
                    if (0..n).contains(&k) { l[k as usize].clone() } else { Value::Null }
                }
                (b, i) => return qerr(format!("cannot index {} with {}", b.type_name(), i.type_name())),
            },
            Expr::Call { name, args, .. } => {
                if AGGS.contains(&name.as_str()) {
                    return qerr(format!("aggregate {name}() is only allowed as a top-level RETURN item"));
                }
                let a = args.iter().map(|x| self.eval(x, env)).collect::<Result<Vec<_>>>()?;
                self.scalar_fn(name, &a)?
            }
        })
    }

    fn binop(&self, op: Bin, l: &Expr, r: &Expr, env: &Env) -> Result<Value> {
        use Value::*;
        if op == Bin::And || op == Bin::Or {
            let a = self.eval(l, env)?;
            let b = self.eval(r, env)?;
            let tri = |v: &Value| -> Result<Option<bool>> {
                match v {
                    Bool(b) => Ok(Some(*b)),
                    Null => Ok(None),
                    o => qerr(format!("AND/OR need booleans, got {}", o.type_name())),
                }
            };
            let (x, y) = (tri(&a)?, tri(&b)?);
            return Ok(match (op, x, y) {
                (Bin::And, Some(false), _) | (Bin::And, _, Some(false)) => Bool(false),
                (Bin::And, Some(true), Some(true)) => Bool(true),
                (Bin::Or, Some(true), _) | (Bin::Or, _, Some(true)) => Bool(true),
                (Bin::Or, Some(false), Some(false)) => Bool(false),
                _ => Null,
            });
        }
        let a = self.eval(l, env)?;
        let b = self.eval(r, env)?;
        if a.is_null() || b.is_null() {
            return Ok(Null);
        }
        Ok(match op {
            Bin::Eq => Bool(a.loose_eq(&b)),
            Bin::Ne => Bool(!a.loose_eq(&b)),
            Bin::Lt | Bin::Le | Bin::Gt | Bin::Ge => match a.compare(&b) {
                Some(o) => Bool(match op {
                    Bin::Lt => o.is_lt(),
                    Bin::Le => o.is_le(),
                    Bin::Gt => o.is_gt(),
                    _ => o.is_ge(),
                }),
                None => Null,
            },
            Bin::In => match &b {
                List(items) => Bool(items.iter().any(|x| x.loose_eq(&a))),
                o => return qerr(format!("IN needs a list, got {}", o.type_name())),
            },
            Bin::StartsWith | Bin::EndsWith | Bin::Contains => match (&a, &b) {
                (Str(x), Str(y)) => Bool(match op {
                    Bin::StartsWith => x.starts_with(y.as_str()),
                    Bin::EndsWith => x.ends_with(y.as_str()),
                    _ => x.contains(y.as_str()),
                }),
                _ => Null,
            },
            Bin::Add => match (&a, &b) {
                (Int(x), Int(y)) => Int(x.checked_add(*y).ok_or_else(|| Error::Query("integer overflow".into()))?),
                (Str(x), Str(y)) => Str(format!("{x}{y}")),
                (List(x), List(y)) => List(x.iter().chain(y).cloned().collect()),
                _ => Float(num(&a)? + num(&b)?),
            },
            Bin::Sub => match (&a, &b) {
                (Int(x), Int(y)) => Int(x.checked_sub(*y).ok_or_else(|| Error::Query("integer overflow".into()))?),
                _ => Float(num(&a)? - num(&b)?),
            },
            Bin::Mul => match (&a, &b) {
                (Int(x), Int(y)) => Int(x.checked_mul(*y).ok_or_else(|| Error::Query("integer overflow".into()))?),
                _ => Float(num(&a)? * num(&b)?),
            },
            Bin::Div => match (&a, &b) {
                (Int(_), Int(0)) => return qerr("division by zero"),
                (Int(x), Int(y)) => Int(x / y),
                _ => Float(num(&a)? / num(&b)?),
            },
            Bin::Mod => match (&a, &b) {
                (Int(_), Int(0)) => return qerr("division by zero"),
                (Int(x), Int(y)) => Int(x % y),
                _ => Float(num(&a)? % num(&b)?),
            },
            Bin::Pow => Float(num(&a)?.powf(num(&b)?)),
            Bin::And | Bin::Or => unreachable!(),
        })
    }

    fn scalar_fn(&self, name: &str, a: &[Value]) -> Result<Value> {
        use Value::*;
        let one = || a.first().ok_or_else(|| Error::Query(format!("{name}() needs an argument")));
        Ok(match name {
            "id" => match one()? {
                Node(i) | Edge(i) => Int(*i as i64),
                Null => Null,
                _ => return qerr("id() needs a node or edge"),
            },
            "labels" => match one()? {
                Node(i) => self.view.node(*i).map_or(Null, |b| List(b.label_list().into_iter().map(|s| Str(s.into())).collect())),
                Null => Null,
                _ => return qerr("labels() needs a node"),
            },
            "type" => match one()? {
                Edge(i) => self.view.edge(*i).map_or(Null, |b| Str(b.label_list()[0].into())),
                Null => Null,
                _ => return qerr("type() needs an edge"),
            },
            "weight" => match one()? {
                Edge(i) => match self.view.edge(*i) {
                    Some(crate::model::Body::Edge { weight, .. }) => Float(*weight),
                    _ => Null,
                },
                Null => Null,
                _ => return qerr("weight() needs an edge"),
            },
            "head" | "last" | "tail" if matches!(a.first(), Some(List(_)) | Some(Null)) => match one()? {
                List(l) => match name {
                    "head" => l.first().cloned().unwrap_or(Null),
                    "last" => l.last().cloned().unwrap_or(Null),
                    _ => List(l.iter().skip(1).cloned().collect()),
                },
                _ => Null,
            },
            "members" | "tail" | "head" => match one()? {
                Edge(i) => match self.view.edge(*i) {
                    Some(b @ crate::model::Body::Edge { tail, head, .. }) => List(
                        match name {
                            "members" => b.members(),
                            "tail" => tail.clone(),
                            _ => head.clone(),
                        }
                        .into_iter()
                        .map(Node)
                        .collect(),
                    ),
                    _ => Null,
                },
                Null => Null,
                _ => return qerr("members()/tail()/head() need an edge"),
            },
            "size" | "length" => match one()? {
                List(l) => Int(l.len() as i64),
                Vector(v) => Int(v.len() as i64),
                Str(s) => Int(s.chars().count() as i64),
                Null => Null,
                _ => return qerr("size() needs a list or string"),
            },
            "coalesce" => a.iter().find(|v| !v.is_null()).cloned().unwrap_or(Null),
            "abs" => match one()? {
                Int(i) => Int(i.abs()),
                Float(f) => Float(f.abs()),
                Null => Null,
                _ => return qerr("abs() needs a number"),
            },
            "sqrt" => match one()? {
                Null => Null,
                v => Float(num(v)?.sqrt()),
            },
            "tofloat" => match one()? {
                Null => Null,
                Str(s) => s.trim().parse::<f64>().map(Float).unwrap_or(Null),
                v => Float(num(v)?),
            },
            "toint" | "tointeger" => match one()? {
                Null => Null,
                Str(s) => match s.trim().parse::<i64>() {
                    Ok(i) => Int(i),
                    Err(_) => s.trim().parse::<f64>().ok().filter(|f| f.is_finite()).map_or(Null, |f| Int(f.trunc() as i64)),
                },
                v => Int(num(v)?.trunc() as i64),
            },
            "toboolean" => match one()? {
                Bool(b) => Bool(*b),
                Str(s) if s.eq_ignore_ascii_case("true") => Bool(true),
                Str(s) if s.eq_ignore_ascii_case("false") => Bool(false),
                _ => Null,
            },
            "round" | "floor" | "ceil" | "sign" | "exp" | "log" | "log10" => match one()? {
                Null => Null,
                v => {
                    let x = num(v)?;
                    match name {
                        "round" => Float(x.round()),
                        "floor" => Float(x.floor()),
                        "ceil" => Float(x.ceil()),
                        "sign" => Int(if x > 0.0 { 1 } else if x < 0.0 { -1 } else { 0 }),
                        "exp" => Float(x.exp()),
                        "log" => Float(x.ln()),
                        _ => Float(x.log10()),
                    }
                }
            },
            "exists" => Bool(!one()?.is_null()),
            "timestamp" => Int(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)),
            "trim" | "ltrim" | "rtrim" => match one()? {
                Str(s) => Str(match name {
                    "trim" => s.trim(),
                    "ltrim" => s.trim_start(),
                    _ => s.trim_end(),
                }
                .into()),
                _ => Null,
            },
            "substring" | "left" | "right" => match (a.first(), a.get(1)) {
                (Some(Str(s)), Some(Int(n))) if *n >= 0 => {
                    let cs: Vec<char> = s.chars().collect();
                    let n = *n as usize;
                    let out: String = match name {
                        "left" => cs.iter().take(n).collect(),
                        "right" => cs[cs.len().saturating_sub(n)..].iter().collect(),
                        _ => match a.get(2) {
                            Some(Int(len)) if *len >= 0 => cs.iter().skip(n).take(*len as usize).collect(),
                            Some(Int(_)) => return qerr("substring() length must not be negative"),
                            _ => cs.iter().skip(n).collect(),
                        },
                    };
                    Str(out)
                }
                (Some(Null), _) | (_, Some(Null)) => Null,
                _ => return qerr(format!("{name}() needs a string and a non-negative integer")),
            },
            "replace" => match (a.first(), a.get(1), a.get(2)) {
                (Some(Str(s)), Some(Str(f)), Some(Str(t))) if !f.is_empty() => Str(s.replace(f.as_str(), t)),
                (Some(Null), _, _) | (_, Some(Null), _) | (_, _, Some(Null)) => Null,
                _ => return qerr("replace() needs three strings (the second not empty)"),
            },
            "split" => match (a.first(), a.get(1)) {
                (Some(Str(s)), Some(Str(d))) if !d.is_empty() => List(s.split(d.as_str()).map(|x| Str(x.into())).collect()),
                (Some(Null), _) | (_, Some(Null)) => Null,
                _ => return qerr("split() needs two strings (the delimiter not empty)"),
            },
            "reverse" => match one()? {
                Str(s) => Str(s.chars().rev().collect()),
                List(l) => List(l.iter().rev().cloned().collect()),
                _ => Null,
            },
            "range" => match (a.first(), a.get(1), a.get(2)) {
                (Some(Int(lo)), Some(Int(hi)), step) => {
                    let st = match step {
                        None => 1,
                        Some(Int(0)) => return qerr("range() step must not be 0"),
                        Some(Int(s)) => *s,
                        _ => return qerr("range() needs integers"),
                    };
                    let len = if (st > 0 && hi < lo) || (st < 0 && hi > lo) { 0 } else { ((*hi as i128 - *lo as i128) / st as i128) + 1 };
                    if len > 1_000_000 {
                        return qerr("range() would produce more than 1,000,000 values");
                    }
                    List((0..len as i64).map(|k| Int(lo + k * st)).collect())
                }
                _ => return qerr("range() needs integers"),
            },
            "tostring" => match one()? {
                Null => Null,
                Str(s) => Str(s.clone()),
                v => Str(v.to_string()),
            },
            "lower" | "tolower" => match one()? {
                Str(s) => Str(s.to_lowercase()),
                _ => Null,
            },
            "upper" | "toupper" => match one()? {
                Str(s) => Str(s.to_uppercase()),
                _ => Null,
            },
            "keys" => match one()? {
                Node(i) | Edge(i) => self.view.body(*i).map_or(Null, |b| List(b.props().keys().map(|k| Str(k.clone())).collect())),
                _ => Null,
            },
            "vector" => match one()?.to_vector() {
                Some(v) => Vector(v),
                None => return qerr("vector() needs a list of numbers"),
            },
            "cosine" | "l2" => {
                let (Some(x), Some(y)) = (a.first().and_then(|v| v.to_vector()), a.get(1).and_then(|v| v.to_vector())) else {
                    return qerr("cosine()/l2() need two vectors");
                };
                if x.len() != y.len() {
                    return qerr("vector dimensions differ");
                }
                if name == "l2" {
                    Float(x.iter().zip(&y).map(|(p, q)| ((p - q) as f64).powi(2)).sum::<f64>().sqrt())
                } else {
                    let dot: f64 = x.iter().zip(&y).map(|(p, q)| (*p as f64) * (*q as f64)).sum();
                    let (nx, ny): (f64, f64) = (x.iter().map(|p| (*p as f64).powi(2)).sum::<f64>().sqrt(), y.iter().map(|p| (*p as f64).powi(2)).sum::<f64>().sqrt());
                    if nx == 0.0 || ny == 0.0 {
                        Null
                    } else {
                        Float(dot / (nx * ny))
                    }
                }
            }
            _ => return qerr(format!("unknown function {name}()")),
        })
    }
}

fn num(v: &Value) -> Result<f64> {
    v.as_f64().ok_or_else(|| Error::Query(format!("expected a number, got {}", v.type_name())))
}


// ------------------------------------------------------------------ projection

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Count,
    Sum,
    Avg,
    Min,
    Max,
    Collect,
    Stdev,
    Median,
}

/// Streaming accumulator for one aggregate item of one group.
struct Acc {
    kind: Kind,
    seen: Option<HashSet<Vec<u8>>>,
    n: i64,
    isum: i64,
    overflow: bool,
    all_int: bool,
    fsum: f64,
    bad: bool,
    best: Option<Value>,
    list: Vec<Value>,
}

impl Acc {
    fn new(kind: Kind, _name: &str, distinct: bool) -> Self {
        Acc { kind, seen: distinct.then(HashSet::new), n: 0, isum: 0, overflow: false, all_int: true, fsum: 0.0, bad: false, best: None, list: Vec::new() }
    }

    fn add(&mut self, v: Value) {
        if v.is_null() {
            return;
        }
        if let Some(seen) = &mut self.seen {
            let mut k = Vec::new();
            hkey(&v, &mut k);
            if !seen.insert(k) {
                return;
            }
        }
        self.n += 1;
        match self.kind {
            Kind::Count => {}
            Kind::Collect => self.list.push(v),
            Kind::Stdev | Kind::Median => match v.as_f64() {
                Some(x) => self.list.push(Value::Float(x)),
                None => self.bad = true,
            },
            Kind::Sum | Kind::Avg => {
                match &v {
                    Value::Int(i) => match self.isum.checked_add(*i) {
                        Some(s) => self.isum = s,
                        None => self.overflow = true,
                    },
                    _ => self.all_int = false,
                }
                match v.as_f64() {
                    Some(f) => self.fsum += f,
                    None => self.bad = true,
                }
            }
            Kind::Min | Kind::Max => match &self.best {
                None => self.best = Some(v),
                Some(b) => {
                    let o = v.sort_cmp(b);
                    if (self.kind == Kind::Min && o.is_lt()) || (self.kind == Kind::Max && o.is_gt()) {
                        self.best = Some(v);
                    }
                }
            },
        }
    }

    fn finish(self) -> Result<Value> {
        Ok(match self.kind {
            Kind::Count => Value::Int(self.n),
            Kind::Collect => Value::List(self.list),
            Kind::Stdev | Kind::Median if self.bad => return qerr(format!("{}() needs numbers", if self.kind == Kind::Stdev { "stdev" } else { "median" })),
            Kind::Stdev | Kind::Median if self.list.is_empty() => Value::Null,
            Kind::Stdev => {
                // sample standard deviation (n-1), two-pass for accuracy; null for a single value
                let x: Vec<f64> = self.list.iter().filter_map(|v| v.as_f64()).collect();
                if x.len() < 2 {
                    Value::Null
                } else {
                    let m = x.iter().sum::<f64>() / x.len() as f64;
                    Value::Float((x.iter().map(|v| (v - m) * (v - m)).sum::<f64>() / (x.len() - 1) as f64).sqrt())
                }
            }
            Kind::Median => {
                let mut x: Vec<f64> = self.list.iter().filter_map(|v| v.as_f64()).collect();
                x.sort_by(|a, b| a.total_cmp(b));
                let n = x.len();
                Value::Float(if n % 2 == 1 { x[n / 2] } else { (x[n / 2 - 1] + x[n / 2]) / 2.0 })
            }
            Kind::Sum if self.n == 0 => Value::Int(0),
            Kind::Avg if self.n == 0 => Value::Null,
            Kind::Sum if self.all_int => {
                if self.overflow {
                    return qerr("integer overflow in sum");
                }
                Value::Int(self.isum)
            }
            Kind::Sum | Kind::Avg => {
                if self.bad {
                    return qerr(format!("{}() needs numbers", if self.kind == Kind::Avg { "avg" } else { "sum" }));
                }
                Value::Float(if self.kind == Kind::Avg { self.fsum / self.n as f64 } else { self.fsum })
            }
            Kind::Min | Kind::Max => self.best.unwrap_or(Value::Null),
        })
    }
}

struct Group {
    key: Vec<Value>,
    first: Option<Env>,
    accs: Vec<Acc>,
}

#[derive(Default, Clone, Copy)]
struct Fx(u64);
impl std::hash::Hasher for Fx {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, b: &[u8]) {
        for c in b.chunks(8) {
            let mut w = [0u8; 8];
            w[..c.len()].copy_from_slice(c);
            self.0 = (self.0.rotate_left(5) ^ u64::from_le_bytes(w)).wrapping_mul(0x517c_c1b7_2722_0a95);
        }
    }
}
type FxMap<K, V> = HashMap<K, V, std::hash::BuildHasherDefault<Fx>>;

struct Projector<'r> {
    rc: &'r ReturnClause,
    cols: Vec<String>,
    cols_rc: Vec<std::rc::Rc<str>>,
    agg: Vec<Option<(Kind, &'r Expr, bool, bool, String)>>, // per item: (kind, arg, star, distinct, name)
    any_agg: bool,
    key_items: Vec<&'r Expr>,
    rows: Vec<(Vec<Value>, Vec<Value>)>,
    groups: Vec<Group>,
    index: FxMap<Vec<u8>, usize>,
    skip: usize,
    limit: usize,
    early: bool,
    /// ORDER BY uses something other than output aliases, so each group keeps its first row's bindings.
    needs_first: bool,
    /// ORDER BY items that are just output columns, by position (no expression evaluation needed).
    order_cols: Option<Vec<usize>>,
    /// Lean group table for factorised `count(*)` with one grouping key (no per-group allocation).
    fast: Option<FastG>,
}

#[derive(Default)]
struct FastG {
    keys: Vec<Value>,
    counts: Vec<i64>,
    nmap: FxMap<u64, u32>,
    imap: FxMap<i64, u32>,
    other: FxMap<Vec<u8>, u32>,
}

static STAR_ARG: Expr = Expr::Lit(Value::Null);

impl<'r> Projector<'r> {
    fn new(cx: &Ctx, rc: &'r ReturnClause) -> Result<Self> {
        let cols: Vec<String> = rc.items.iter().map(|i| i.1.clone()).collect();
        let mut agg = Vec::new();
        for (e, _) in &rc.items {
            agg.push(match e {
                Expr::Call { name, args, distinct, star } if AGGS.contains(&name.as_str()) => {
                    let kind = match name.as_str() {
                        "count" => Kind::Count,
                        "sum" => Kind::Sum,
                        "avg" => Kind::Avg,
                        "min" => Kind::Min,
                        "max" => Kind::Max,
                        "stdev" => Kind::Stdev,
                        "median" => Kind::Median,
                        _ => Kind::Collect,
                    };
                    if *star {
                        if kind != Kind::Count {
                            return qerr("only count accepts *");
                        }
                        Some((kind, &STAR_ARG, true, false, name.clone()))
                    } else {
                        let arg = args.first().ok_or_else(|| Error::Query(format!("{name}() needs an argument")))?;
                        Some((kind, arg, false, *distinct, name.clone()))
                    }
                }
                _ => None,
            });
        }
        let any_agg = agg.iter().any(|a| a.is_some());
        let key_items = rc.items.iter().zip(&agg).filter(|(_, a)| a.is_none()).map(|((e, _), _)| e).collect();
        let env0 = Env::new();
        let skip = match &rc.skip {
            Some(e) => cx.eval(e, &env0)?.as_i64().filter(|x| *x >= 0).ok_or_else(|| Error::Query("SKIP must be a non-negative integer".into()))? as usize,
            None => 0,
        };
        let limit = match &rc.limit {
            Some(e) => cx.eval(e, &env0)?.as_i64().filter(|x| *x >= 0).ok_or_else(|| Error::Query("LIMIT must be a non-negative integer".into()))? as usize,
            None => usize::MAX,
        };
        let early = !any_agg && !rc.distinct && rc.order.is_empty() && rc.limit.is_some();
        let cols_rc = cols.iter().map(|c| std::rc::Rc::from(c.as_str())).collect();
        let needs_first = rc.order.iter().any(|(e, _)| !matches!(e, Expr::Var(v) if cols.contains(v)));
        let order_cols = rc.order.iter().map(|(e, _)| match e {
            Expr::Var(v) => cols.iter().position(|c| c == v),
            _ => None,
        }).collect::<Option<Vec<usize>>>();
        Ok(Projector { rc, cols, cols_rc, agg, any_agg, key_items, rows: Vec::new(), groups: Vec::new(), index: FxMap::default(), skip, limit, early, needs_first, order_cols, fast: None })
    }

    /// Order keys for a row: evaluated with the output aliases layered over the row's bindings.
    fn order_keys(&self, cx: &Ctx, env: &mut Env, vals: &[Value]) -> Result<Vec<Value>> {
        if self.rc.order.is_empty() {
            return Ok(Vec::new());
        }
        if let Some(ix) = &self.order_cols {
            // an output alias shadows any row binding of the same name, so this equals evaluating the expression
            return Ok(ix.iter().map(|&i| vals[i].clone()).collect());
        }
        let mark = env.len();
        for (c, v) in self.cols_rc.iter().zip(vals) {
            env.push(c, v.clone());
        }
        let r = self.rc.order.iter().map(|(e, _)| cx.eval(e, env)).collect::<Result<Vec<_>>>();
        env.truncate(mark);
        r
    }

    /// Feed one binding; returns false once no more rows are needed.
    fn push(&mut self, cx: &Ctx, env: &mut Env) -> Result<bool> {
        if !self.any_agg {
            let vals = self.rc.items.iter().map(|(e, _)| cx.eval(e, env)).collect::<Result<Vec<_>>>()?;
            let keys = self.order_keys(cx, env, &vals)?;
            self.rows.push((keys, vals));
            if self.early && self.rows.len() >= self.skip.saturating_add(self.limit) {
                return Ok(false);
            }
            return Ok(true);
        }
        let key = self.key_items.iter().map(|e| cx.eval(e, env)).collect::<Result<Vec<_>>>()?;
        let hk = if key.is_empty() { Vec::new() } else { hkey_all(&key) };
        let gi = match self.index.get(&hk) {
            Some(&g) => g,
            None => {
                let accs = self.agg.iter().flatten().map(|(k, _, _, d, n)| Acc::new(*k, n, *d)).collect();
                let first = self.needs_first.then(|| env.clone());
                self.groups.push(Group { key, first, accs });
                self.index.insert(hk, self.groups.len() - 1);
                self.groups.len() - 1
            }
        };
        let g = &mut self.groups[gi];
        for (acc, (_, arg, star, _, _)) in g.accs.iter_mut().zip(self.agg.iter().flatten()) {
            if *star {
                acc.n += 1;
            } else {
                acc.add(cx.eval(arg, env)?);
            }
        }
        Ok(true)
    }

    /// Like `push`, for `w` identical rows at once (factorised aggregation:
    /// every aggregate must be `count(*)`).
    fn push_weighted(&mut self, cx: &Ctx, env: &mut Env, w: u64) -> Result<()> {
        if w == 0 {
            return Ok(());
        }
        if self.fast_ok() {
            let k = cx.eval(self.key_items[0], env)?;
            self.push_key_weighted(k, w);
            return Ok(());
        }
        let key = self.key_items.iter().map(|e| cx.eval(e, env)).collect::<Result<Vec<_>>>()?;
        self.push_weighted_slow(key, env, w)
    }

    fn fast_ok(&self) -> bool {
        self.key_items.len() == 1 && !self.needs_first && self.groups.is_empty()
    }

    /// Append a group whose key is known to be new (caller guarantees distinct keys, and uses no other push).
    fn push_unique_key(&mut self, k: Value, w: u64) {
        let f = self.fast.get_or_insert_with(FastG::default);
        f.keys.push(k);
        f.counts.push(w as i64);
    }

    fn push_key_weighted(&mut self, k: Value, w: u64) {
        {
            let f = self.fast.get_or_insert_with(FastG::default);
            let slot = match &k {
                Value::Node(i) => f.nmap.get(i).copied(),
                Value::Int(i) => f.imap.get(i).copied(),
                _ => f.other.get(&hkey_all(std::slice::from_ref(&k))).copied(),
            };
            let gi = match slot {
                Some(g) => g,
                None => {
                    let g = f.keys.len() as u32;
                    match &k {
                        Value::Node(i) => {
                            f.nmap.insert(*i, g);
                        }
                        Value::Int(i) => {
                            f.imap.insert(*i, g);
                        }
                        _ => {
                            f.other.insert(hkey_all(std::slice::from_ref(&k)), g);
                        }
                    }
                    f.keys.push(k);
                    f.counts.push(0);
                    g
                }
            };
            f.counts[gi as usize] += w as i64;
        }
    }

    fn push_weighted_slow(&mut self, key: Vec<Value>, env: &mut Env, w: u64) -> Result<()> {
        let hk = if key.is_empty() { Vec::new() } else { hkey_all(&key) };
        let gi = match self.index.get(&hk) {
            Some(&g) => g,
            None => {
                let accs = self.agg.iter().flatten().map(|(k, _, _, d, n)| Acc::new(*k, n, *d)).collect();
                let first = self.needs_first.then(|| env.clone());
                self.groups.push(Group { key, first, accs });
                self.index.insert(hk, self.groups.len() - 1);
                self.groups.len() - 1
            }
        };
        for acc in self.groups[gi].accs.iter_mut() {
            acc.n += w as i64;
        }
        Ok(())
    }

    /// Expect up to `n` groups (bounded), so the group table is not regrown and rehashed while filling.
    fn reserve_groups(&mut self, n: usize) {
        let n = n.min(1 << 20);
        if self.key_items.len() == 1 && !self.needs_first {
            let f = self.fast.get_or_insert_with(FastG::default);
            f.keys.reserve(n);
            f.counts.reserve(n);
            return;
        }
        self.groups.reserve(n);
        self.index.reserve(n);
    }

    /// Every aggregate is `count(*)` or a plain `count(v)` of one of `vars` (variables the pattern always binds).
    fn all_counts_of(&self, vars: &[&Option<std::rc::Rc<str>>]) -> bool {
        self.any_agg
            && self.agg.iter().flatten().all(|(k, arg, star, d, _)| {
                *k == Kind::Count && !*d && (*star || matches!(arg, Expr::Var(v) if vars.iter().any(|x| x.as_deref() == Some(v.as_str()))))
            })
    }

    fn finish(mut self, cx: &Ctx) -> Result<(Vec<String>, Vec<Vec<Value>>)> {
        let mut produced = std::mem::take(&mut self.rows);
        let mut presorted = false;
        if let Some(f) = self.fast.take() {
            let n = f.keys.len();
            let is_agg: Vec<bool> = self.agg.iter().map(|a| a.is_some()).collect();
            let row = |i: usize| -> Vec<Value> { is_agg.iter().map(|&a| if a { Value::Int(f.counts[i]) } else { f.keys[i].clone() }).collect() };
            let mut idx: Vec<u32> = (0..n as u32).collect();
            let need = self.skip.saturating_add(self.limit);
            if self.rc.distinct {
                for &i in &idx {
                    let vals = row(i as usize);
                    let keys = self.order_keys(cx, &mut Env::new(), &vals)?;
                    produced.push((keys, vals));
                }
            } else {
                if let Some(ix) = &self.order_cols {
                    let order = &self.rc.order;
                    let cmp = |a: &u32, b: &u32| {
                        let (ta, tb) = (Value::Int(f.counts[*a as usize]), Value::Int(f.counts[*b as usize]));
                        for (o, &c) in order.iter().zip(ix) {
                            let (x, y) = if is_agg[c] { (&ta, &tb) } else { (&f.keys[*a as usize], &f.keys[*b as usize]) };
                            let r = x.sort_cmp(y);
                            let r = if o.1 { r.reverse() } else { r };
                            if r != std::cmp::Ordering::Equal {
                                return r;
                            }
                        }
                        a.cmp(b)
                    };
                    if need < idx.len() {
                        if need > 0 {
                            idx.select_nth_unstable_by(need - 1, cmp);
                        }
                        idx.truncate(need);
                    }
                    idx.sort_by(cmp);
                } else {
                    idx.truncate(need);
                }
                for &i in &idx {
                    produced.push((Vec::new(), row(i as usize)));
                }
                presorted = true;
            }
        } else if self.any_agg {
            if self.groups.is_empty() && self.key_items.is_empty() {
                let accs = self.agg.iter().flatten().map(|(k, _, _, d, n)| Acc::new(*k, n, *d)).collect();
                self.groups.push(Group { key: vec![], first: None, accs });
            }
            for g in std::mem::take(&mut self.groups) {
                let mut ki = g.key.into_iter();
                let mut ai = g.accs.into_iter();
                let mut vals = Vec::new();
                for a in &self.agg {
                    vals.push(match a {
                        Some(_) => ai.next().unwrap().finish()?,
                        None => ki.next().unwrap(),
                    });
                }
                let mut env = g.first.unwrap_or_default();
                let keys = self.order_keys(cx, &mut env, &vals)?;
                produced.push((keys, vals));
            }
        }
        if self.rc.distinct {
            let mut seen = HashSet::new();
            produced.retain(|(_, v)| seen.insert(hkey_all(v)));
        }
        if !self.rc.order.is_empty() && !presorted {
            // Slice pushdown: with a LIMIT only the first skip+limit rows are
            // needed, so select them first and sort just those. The original
            // position is the last tiebreak, which keeps the result identical
            // to a stable full sort.
            let need = self.skip.saturating_add(self.limit);
            if need < produced.len() {
                let order = &self.rc.order;
                let cmp = |a: &(usize, (Vec<Value>, Vec<Value>)), b: &(usize, (Vec<Value>, Vec<Value>))| {
                    for (i, (_, desc)) in order.iter().enumerate() {
                        let o = a.1 .0[i].sort_cmp(&b.1 .0[i]);
                        let o = if *desc { o.reverse() } else { o };
                        if o != std::cmp::Ordering::Equal {
                            return o;
                        }
                    }
                    a.0.cmp(&b.0)
                };
                let mut idx: Vec<_> = produced.into_iter().enumerate().collect();
                if need > 0 {
                    idx.select_nth_unstable_by(need - 1, cmp);
                }
                idx.truncate(need);
                idx.sort_by(cmp);
                produced = idx.into_iter().map(|(_, r)| r).collect();
            }
            produced.sort_by(|a, b| {
                for (i, (_, desc)) in self.rc.order.iter().enumerate() {
                    let o = a.0[i].sort_cmp(&b.0[i]);
                    let o = if *desc { o.reverse() } else { o };
                    if o != std::cmp::Ordering::Equal {
                        return o;
                    }
                }
                std::cmp::Ordering::Equal
            });
        }
        let rows_out = produced.into_iter().skip(self.skip).take(self.limit).map(|(_, v)| v).collect();
        Ok((self.cols, rows_out))
    }
}


/// Per-start-node hop counts, computed on scoped threads for large scans.
fn parallel_counts(view: &View, starts: &[Id], sp: &crate::store::HopSpec) -> Vec<u64> {
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(8);
    if starts.len() < 20_000 || threads < 2 {
        return starts.iter().map(|&a| view.hop_count(a, sp)).collect();
    }
    let mut out = vec![0u64; starts.len()];
    let chunk = starts.len().div_ceil(threads);
    std::thread::scope(|sc| {
        for (ids, res) in starts.chunks(chunk).zip(out.chunks_mut(chunk)) {
            sc.spawn(move || {
                for (r, &a) in res.iter_mut().zip(ids) {
                    *r = view.hop_count(a, sp);
                }
            });
        }
    });
    out
}
