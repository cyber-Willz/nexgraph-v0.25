//! Exact PDE-style procedures on the hypergraph: heat diffusion and Dirichlet
//! (harmonic) extension.
//!
//! These are the governing equations that `pde_shpinn` approximates with a
//! trained network (its Poisson and heat demos are validated against exactly
//! these direct solves). A database can skip the network and solve them
//! directly, with no Burn dependency, no training and no sampling error.
//!
//! Operator: the Zhou-Huang-Schoelkopf hypergraph walk
//! `P = Dv^-1 H W De^-1 H^T`, where `H` is node-edge incidence, `W` the edge
//! weights, `De` the edge sizes and `Dv` the weighted node degrees. `I - P` is
//! the random-walk Laplacian and `I - Dv^1/2 P Dv^-1/2` is exactly the
//! normalised hypergraph Laplacian that `spectral_hypergraph` (and SHPINN)
//! use, so `mode = 'sym'` reproduces that operator and `mode = 'rw'`
//! (default) diffuses a field by neighbourhood averaging.
//!
//! Every edge with at least 2 distinct members and positive finite weight
//! participates (direction is ignored). Nodes in no such edge are inert:
//! heat leaves them unchanged and harmonic extension reports `null`.

use crate::error::{Error, Result};
use crate::model::Body;
use crate::store::View;
use crate::value::{Id, Value};
use std::collections::HashMap;

pub(crate) struct Walk {
    pub ids: Vec<Id>,
    edges: Vec<(Vec<usize>, f64)>,
    dv: Vec<f64>,
}

impl Walk {
    pub fn build(view: &View) -> Walk {
        let ids: Vec<Id> = view.node_ids().collect();
        let idx: HashMap<Id, usize> = ids.iter().enumerate().map(|(i, &x)| (x, i)).collect();
        let mut edges = Vec::new();
        let mut dv = vec![0.0; ids.len()];
        for e in view.edge_ids() {
            if let Some(b @ Body::Edge { weight, .. }) = view.edge(e) {
                let m: Vec<usize> = b.members().iter().filter_map(|x| idx.get(x).copied()).collect();
                if m.len() >= 2 && *weight > 0.0 && weight.is_finite() {
                    for &i in &m {
                        dv[i] += weight;
                    }
                    edges.push((m, *weight));
                }
            }
        }
        Walk { ids, edges, dv }
    }

    pub fn n(&self) -> usize {
        self.ids.len()
    }

    /// `y = P x` (rows of inert nodes are the identity).
    pub fn apply_p(&self, x: &[f64], y: &mut [f64]) {
        y.iter_mut().for_each(|v| *v = 0.0);
        for (m, w) in &self.edges {
            let mean = m.iter().map(|&j| x[j]).sum::<f64>() / m.len() as f64;
            for &i in m {
                y[i] += w * mean;
            }
        }
        for i in 0..y.len() {
            y[i] = if self.dv[i] > 0.0 { y[i] / self.dv[i] } else { x[i] };
        }
    }

    /// `exp(-t (I - P)) x` by uniformisation: `e^-t sum_k t^k/k! P^k x`.
    /// All terms are non-negative combinations, so it is unconditionally
    /// stable; long times are split into steps of at most 200.
    pub fn heat_rw(&self, x0: &[f64], t: f64) -> Vec<f64> {
        let n = self.n();
        let steps = (t / 200.0).ceil().max(1.0) as usize;
        let dt = t / steps as f64;
        let mut u = x0.to_vec();
        let mut term = vec![0.0; n];
        let mut next = vec![0.0; n];
        for _ in 0..steps {
            let mut w = (-dt).exp();
            let mut acc: Vec<f64> = u.iter().map(|v| v * w).collect();
            term.copy_from_slice(&u);
            let mut mass = w;
            let kmax = (dt + 12.0 * dt.sqrt() + 40.0) as usize;
            for k in 1..=kmax {
                self.apply_p(&term, &mut next);
                std::mem::swap(&mut term, &mut next);
                w *= dt / k as f64;
                mass += w;
                for i in 0..n {
                    acc[i] += w * term[i];
                }
                if 1.0 - mass < 1e-15 {
                    break;
                }
            }
            u = acc;
        }
        u
    }

    pub fn heat(&self, x0: &[f64], t: f64, sym: bool) -> Vec<f64> {
        if !sym {
            return self.heat_rw(x0, t);
        }
        let s = |i: usize| if self.dv[i] > 0.0 { self.dv[i].sqrt() } else { 1.0 };
        let scaled: Vec<f64> = x0.iter().enumerate().map(|(i, v)| v / s(i)).collect();
        let r = self.heat_rw(&scaled, t);
        r.iter().enumerate().map(|(i, v)| v * s(i)).collect()
    }

    /// Dirichlet problem: `known[i] = Some(v)` fixes node `i`; the rest solve
    /// `u = P u`. Returns values (None for nodes that no known node can reach)
    /// and the iteration count.
    pub fn harmonic(&self, known: &[Option<f64>], tol: f64, max_iter: usize) -> Result<(Vec<Option<f64>>, usize)> {
        let n = self.n();
        // nodes reachable from a known node through shared edges
        let mut reach: Vec<bool> = known.iter().map(|k| k.is_some()).collect();
        loop {
            let mut changed = false;
            for (m, _) in &self.edges {
                if m.iter().any(|&i| reach[i]) {
                    for &i in m {
                        if !reach[i] {
                            reach[i] = true;
                            changed = true;
                        }
                    }
                }
            }
            if !changed {
                break;
            }
        }
        let mut u: Vec<f64> = known.iter().map(|k| k.unwrap_or(0.0)).collect();
        let mut y = vec![0.0; n];
        let mut delta = f64::INFINITY;
        let mut iters = 0;
        while iters < max_iter {
            self.apply_p(&u, &mut y);
            delta = 0.0;
            for i in 0..n {
                if known[i].is_none() && reach[i] {
                    delta = f64::max(delta, (y[i] - u[i]).abs());
                    u[i] = y[i];
                }
            }
            iters += 1;
            if delta < tol {
                break;
            }
        }
        if delta >= tol {
            return Err(Error::Query(format!("harmonic extension did not converge in {max_iter} iterations (change {delta:.2e}); raise max_iter")));
        }
        Ok((u.into_iter().enumerate().map(|(i, v)| if reach[i] { Some(v) } else { None }).collect(), iters))
    }
}

fn prop_f64(view: &View, id: Id, key: &str) -> Option<f64> {
    view.node(id).and_then(|b| b.props().get(key)).and_then(|v| v.as_f64()).filter(|x| x.is_finite())
}

type Rows = (Vec<&'static str>, Vec<Vec<Value>>);

fn bad<T>(m: impl Into<String>) -> Result<T> {
    Err(Error::Query(m.into()))
}

fn sym_arg(a: &[Value], i: usize) -> Result<bool> {
    match a.get(i) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Str(s)) if s == "rw" => Ok(false),
        Some(Value::Str(s)) if s == "sym" => Ok(true),
        _ => bad("mode must be 'rw' or 'sym'"),
    }
}

fn time_arg(a: &[Value], i: usize) -> Result<f64> {
    match a.get(i).and_then(|v| v.as_f64()) {
        Some(t) if t >= 0.0 && t <= 1e4 && t.is_finite() => Ok(t),
        _ => bad("t must be a number in [0, 10000]"),
    }
}

/// `physics.heat(key, t [, mode])`, `physics.heat_from(nodes, t [, mode])`,
/// `physics.harmonic(key [, tol, max_iter])`.
pub fn call(view: &View, name: &str, args: &[Value]) -> Result<Rows> {
    let w = Walk::build(view);
    match name {
        "heat" => {
            let key = args.first().and_then(|v| v.as_str()).ok_or_else(|| Error::Query("physics.heat(key, t [, mode])".into()))?;
            let (t, sym) = (time_arg(args, 1)?, sym_arg(args, 2)?);
            let x0: Vec<f64> = w.ids.iter().map(|&i| prop_f64(view, i, key).unwrap_or(0.0)).collect();
            let u = w.heat(&x0, t, sym);
            Ok((vec!["node", "value"], w.ids.iter().zip(u).map(|(&i, v)| vec![Value::Node(i), Value::Float(v)]).collect()))
        }
        "heat_from" => {
            let Some(Value::List(src)) = args.first() else { return bad("physics.heat_from(sources, t [, mode]): sources must be a list of nodes") };
            let (t, sym) = (time_arg(args, 1)?, sym_arg(args, 2)?);
            let mut x0 = vec![0.0; w.n()];
            let pos: HashMap<Id, usize> = w.ids.iter().enumerate().map(|(i, &x)| (x, i)).collect();
            for s in src {
                match s {
                    Value::Node(id) => match pos.get(id) {
                        Some(&i) => x0[i] = 1.0,
                        None => return bad("source node does not exist in this snapshot"),
                    },
                    _ => return bad("sources must be nodes"),
                }
            }
            let u = w.heat(&x0, t, sym);
            Ok((vec!["node", "value"], w.ids.iter().zip(u).map(|(&i, v)| vec![Value::Node(i), Value::Float(v)]).collect()))
        }
        "harmonic" => {
            let key = args.first().and_then(|v| v.as_str()).ok_or_else(|| Error::Query("physics.harmonic(key [, tol, max_iter])".into()))?;
            let tol = args.get(1).and_then(|v| v.as_f64()).unwrap_or(1e-10);
            let max_iter = args.get(2).and_then(|v| v.as_i64()).unwrap_or(100_000).max(1) as usize;
            let known: Vec<Option<f64>> = w.ids.iter().map(|&i| prop_f64(view, i, key)).collect();
            if known.iter().all(|k| k.is_none()) {
                return bad(format!("no node has a numeric '{key}' property to propagate"));
            }
            let (u, _) = w.harmonic(&known, tol, max_iter)?;
            Ok((
                vec!["node", "value", "observed"],
                w.ids
                    .iter()
                    .zip(u.into_iter().zip(known))
                    .map(|(&i, (v, k))| vec![Value::Node(i), v.map_or(Value::Null, Value::Float), Value::Bool(k.is_some())])
                    .collect(),
            ))
        }
        other => bad(format!("unknown physics procedure '{other}' (heat, heat_from, harmonic)")),
    }
}
