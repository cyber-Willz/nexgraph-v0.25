//! Shared helpers for the analytics (`algo.*`, `stats.*`) and cyber (`cyber.*`)
//! procedures: a dense directed projection of the hypergraph, optionally
//! restricted to one edge label, plus argument and robust-statistics helpers.
//!
//! Projection rules (same as the older algorithms): a directed hyperedge sends
//! weight from every tail member to every head member; an undirected hyperedge
//! (empty head) connects all its members pairwise in both directions. Note that
//! a very large undirected hyperedge of `s` members expands to `s(s-1)` arcs.

use crate::error::{Error, Result};
use crate::model::Body;
use crate::store::View;
use crate::value::{Id, Value};

pub(crate) type Rows = (Vec<&'static str>, Vec<Vec<Value>>);

pub(crate) fn bad<T>(m: impl Into<String>) -> Result<T> {
    Err(Error::Query(m.into()))
}

const NO: u32 = u32::MAX;

/// Directed multigraph over the live nodes, with dense indices.
pub(crate) struct Net {
    pub ids: Vec<Id>,
    dense: Vec<u32>,
    /// `out[u]` = (v, weight, edge id)
    pub out: Vec<Vec<(u32, f64, Id)>>,
    pub inn: Vec<Vec<(u32, f64)>>,
}

impl Net {
    pub fn build(view: &View, label: Option<&str>) -> Net {
        let ids: Vec<Id> = view.node_ids().collect();
        let mut dense = vec![NO; view.id_bound()];
        for (i, &x) in ids.iter().enumerate() {
            dense[x as usize] = i as u32;
        }
        let n = ids.len();
        let mut net = Net { ids, dense, out: vec![Vec::new(); n], inn: vec![Vec::new(); n] };
        let edges: Vec<Id> = match label {
            Some(l) => view.edges_with_label(l),
            None => view.edge_ids().collect(),
        };
        for e in edges {
            if let Some(Body::Edge { tail, head, weight, .. }) = view.edge(e) {
                if head.is_empty() {
                    let mem: Vec<u32> = tail.iter().filter_map(|m| net.idx(*m)).map(|i| i as u32).collect();
                    for &a in &mem {
                        for &b in &mem {
                            if a != b {
                                net.add(a, b, *weight, e);
                            }
                        }
                    }
                } else {
                    for t in tail {
                        for h in head {
                            if let (Some(a), Some(b)) = (net.idx(*t), net.idx(*h)) {
                                net.add(a as u32, b as u32, *weight, e);
                            }
                        }
                    }
                }
            }
        }
        net
    }

    fn add(&mut self, a: u32, b: u32, w: f64, e: Id) {
        self.out[a as usize].push((b, w, e));
        self.inn[b as usize].push((a, w));
    }

    pub fn n(&self) -> usize {
        self.ids.len()
    }

    pub fn idx(&self, id: Id) -> Option<usize> {
        self.dense.get(id as usize).copied().filter(|&d| d != NO).map(|d| d as usize)
    }

    /// Deduplicated directed adjacency without self-loops (out, in).
    pub fn simple_directed(&self) -> (Vec<Vec<u32>>, Vec<Vec<u32>>) {
        let n = self.n();
        let mut out: Vec<Vec<u32>> = vec![Vec::new(); n];
        let mut inn: Vec<Vec<u32>> = vec![Vec::new(); n];
        for u in 0..n {
            let mut v: Vec<u32> = self.out[u].iter().map(|x| x.0).filter(|&x| x as usize != u).collect();
            v.sort_unstable();
            v.dedup();
            for &w in &v {
                inn[w as usize].push(u as u32);
            }
            out[u] = v;
        }
        for l in inn.iter_mut() {
            l.sort_unstable();
        }
        (out, inn)
    }

    /// Deduplicated undirected adjacency without self-loops, sorted.
    pub fn simple_undirected(&self) -> Vec<Vec<u32>> {
        let n = self.n();
        let mut adj: Vec<Vec<u32>> = vec![Vec::new(); n];
        for u in 0..n {
            for &(v, _, _) in &self.out[u] {
                if v as usize != u {
                    adj[u].push(v);
                    adj[v as usize].push(u as u32);
                }
            }
        }
        for l in adj.iter_mut() {
            l.sort_unstable();
            l.dedup();
        }
        adj
    }
}

// ------------------------------------------------------------- arguments

pub(crate) fn str_arg<'a>(a: &'a [Value], i: usize) -> Option<&'a str> {
    a.get(i).and_then(|v| v.as_str())
}

pub(crate) fn need_str<'a>(a: &'a [Value], i: usize, what: &str) -> Result<&'a str> {
    str_arg(a, i).ok_or_else(|| Error::Query(format!("{what} must be a string")))
}

pub(crate) fn f64_arg(a: &[Value], i: usize, default: f64) -> Result<f64> {
    match a.get(i) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v.as_f64().filter(|x| x.is_finite()).ok_or_else(|| Error::Query("expected a finite number argument".into())),
    }
}

pub(crate) fn usize_arg(a: &[Value], i: usize, default: usize, what: &str) -> Result<usize> {
    match a.get(i) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => match v.as_i64() {
            Some(x) if x >= 0 => Ok(x as usize),
            _ => bad(format!("{what} must be a non-negative integer")),
        },
    }
}

pub(crate) fn node_arg(a: &[Value], i: usize, what: &str) -> Result<Id> {
    match a.get(i) {
        Some(Value::Node(id)) => Ok(*id),
        _ => bad(format!("{what} must be a node")),
    }
}

// ------------------------------------------------------------ statistics

pub(crate) fn mean(x: &[f64]) -> f64 {
    x.iter().sum::<f64>() / x.len() as f64
}

/// Sample standard deviation (n-1); 0 for fewer than two values.
pub(crate) fn stdev(x: &[f64]) -> f64 {
    if x.len() < 2 {
        return 0.0;
    }
    let m = mean(x);
    (x.iter().map(|v| (v - m) * (v - m)).sum::<f64>() / (x.len() - 1) as f64).sqrt()
}

/// Linear-interpolated quantile of an ascending-sorted slice.
pub(crate) fn quantile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let p = q.clamp(0.0, 1.0) * (sorted.len() - 1) as f64;
    let (lo, hi) = (p.floor() as usize, p.ceil() as usize);
    sorted[lo] + (sorted[hi] - sorted[lo]) * (p - lo as f64)
}

pub(crate) fn sorted(x: &[f64]) -> Vec<f64> {
    let mut s = x.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    s
}

/// Robust z-scores: modified z (median/MAD, Iglewicz-Hoaglin) and, when the
/// MAD is zero (more than half the values identical), the mean-absolute-
/// deviation form; if that is zero too every score is 0.
pub(crate) fn robust_z(x: &[f64]) -> Vec<f64> {
    if x.is_empty() {
        return vec![];
    }
    let s = sorted(x);
    let med = quantile(&s, 0.5);
    let dev: Vec<f64> = sorted(&x.iter().map(|v| (v - med).abs()).collect::<Vec<_>>());
    let mad = quantile(&dev, 0.5);
    if mad > 0.0 {
        return x.iter().map(|v| 0.6745 * (v - med) / mad).collect();
    }
    let mean_ad = mean(&dev);
    if mean_ad > 0.0 {
        return x.iter().map(|v| (v - med) / (1.253314 * mean_ad)).collect();
    }
    vec![0.0; x.len()]
}

/// Numeric property values of every node (or edge) with `label` (all nodes if
/// `label` is `None`), as `(id, is_edge, value)`.
pub(crate) fn numeric_prop(view: &View, label: Option<&str>, key: &str) -> Vec<(Id, bool, f64)> {
    prop_as(view, label, key, |v| v.as_f64().filter(|x| x.is_finite())).0
}

/// Timestamp property as epoch seconds: numbers are seconds, strings are parsed as ISO 8601.
/// Fails (instead of silently returning nothing) when the property exists but *no* value can be read as a
/// time, which is what happens when a CSV with `2024-04-03 11:13:15` columns is imported as text.
pub(crate) fn time_prop(view: &View, label: Option<&str>, key: &str) -> crate::error::Result<Vec<(Id, bool, f64)>> {
    let (rows, present, sample) = prop_as(view, label, key, crate::timeutil::epoch_seconds);
    if present > 0 && rows.is_empty() {
        return Err(crate::error::Error::Query(format!("property '{key}' is present on {present} entities but none is a number of seconds or an ISO 8601 timestamp (first value: {})", sample.unwrap_or_default())));
    }
    Ok(rows)
}

/// `(values, how many entities carry the key, first non-convertible value)`.
fn prop_as(view: &View, label: Option<&str>, key: &str, conv: impl Fn(&Value) -> Option<f64>) -> (Vec<(Id, bool, f64)>, usize, Option<String>) {
    let mut out = Vec::new();
    let (mut present, mut sample) = (0usize, None);
    let mut take = |id: Id, b: &Body, is_edge: bool| {
        if let Some(raw) = b.props().get(key) {
            present += 1;
            match conv(raw) {
                Some(v) => out.push((id, is_edge, v)),
                None if sample.is_none() => sample = Some(format!("{raw:?}")),
                None => {}
            }
        }
    };
    match label {
        Some(l) => {
            for id in view.nodes_with_label(l) {
                if let Some(b) = view.node(id) {
                    take(id, b, false);
                }
            }
            for id in view.edges_with_label(l) {
                if let Some(b) = view.edge(id) {
                    take(id, b, true);
                }
            }
        }
        None => {
            for id in view.node_ids() {
                if let Some(b) = view.node(id) {
                    take(id, b, false);
                }
            }
        }
    }
    (out, present, sample)
}

pub(crate) fn ent(id: Id, is_edge: bool) -> Value {
    if is_edge {
        Value::Edge(id)
    } else {
        Value::Node(id)
    }
}
