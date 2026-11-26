//! Non-backtracking community detection via the Bethe Hessian.
//!
//! Plain Laplacian spectral clustering degrades on sparse graphs because
//! high-degree nodes create localised eigenvectors. The non-backtracking
//! (Hashimoto) operator `B` does not, but is non-symmetric and `2m`-sized.
//! By the Ihara-Bass identity, `B`'s real eigenvalues outside the bulk are
//! the zeros of `det H(r)` where the symmetric `n x n` Bethe Hessian is
//!
//! ```text
//!   H(r) = (r^2 - 1) I - r A + D
//! ```
//!
//! Taking `r = sqrt(rho_B)`, the number of negative eigenvalues of `H(r)`
//! estimates the number of communities and their eigenvectors cluster them
//! (Saade, Krzakala & Zdeborova 2014). `rho_B` is estimated with `nbsc`
//! (Arnoldi on the matrix-free linearised Hashimoto operator, via
//! `krylov_ds`); small cases use a dense eigensolver, larger ones Lanczos.
//!
//! The graph is the unweighted clique expansion of every edge/hyperedge with
//! at least 2 distinct members (weights and direction are ignored). Assortative
//! structure only; disconnected graphs contribute one extra negative
//! eigenvalue per component.

use crate::error::{Error, Result};
use crate::model::Body;
use crate::store::View;
use crate::value::{Id, Value};
use nalgebra::{DMatrix, DVector};
use nbsc::graph::Graph;
use spectral_hypergraph::spectral::lanczos_smallest;
use spectral_hypergraph::LinearOperator;
use std::collections::HashMap;

pub struct Communities {
    pub ids: Vec<Id>,
    pub assignment: Vec<usize>,
    /// Number of communities found (negative eigenvalues of `H(r)`).
    pub k: usize,
    pub rho_b: f64,
    pub mean_degree: f64,
    pub mean_excess_degree: f64,
    pub eigenvalues: Vec<f64>,
}

/// Build the simple graph used by these procedures. Returns the graph and
/// the node id of each vertex.
pub fn clique_graph(view: &View) -> (Graph, Vec<Id>) {
    let mut members: Vec<Vec<Id>> = Vec::new();
    let mut part: Vec<Id> = Vec::new();
    for e in view.edge_ids() {
        if let Some(b @ Body::Edge { .. }) = view.edge(e) {
            let m = b.members();
            if m.len() >= 2 {
                part.extend(m.iter().copied());
                members.push(m);
            }
        }
    }
    part.sort_unstable();
    part.dedup();
    let idx: HashMap<Id, usize> = part.iter().enumerate().map(|(i, &x)| (x, i)).collect();
    // Build the simple graph directly: `Graph::add_edge` scans the neighbour list to reject duplicates, which is
    // O(degree) per edge and made hub-heavy registries (a node in tens of thousands of hyperedges) take minutes.
    let mut g = Graph::new(part.len());
    let mut seen: std::collections::HashSet<(usize, usize)> = std::collections::HashSet::new();
    for m in &members {
        for (a, &x) in m.iter().enumerate() {
            for &y in &m[a + 1..] {
                let (u, v) = (idx[&x], idx[&y]);
                if u == v {
                    continue;
                }
                let e = if u < v { (u, v) } else { (v, u) };
                if seen.insert(e) {
                    g.neighbors[u].push(v);
                    g.neighbors[v].push(u);
                    g.edges.push(e);
                }
            }
        }
    }
    for l in g.neighbors.iter_mut() {
        l.sort_unstable();
    }
    (g, part)
}

fn excess_degree(g: &Graph) -> (f64, f64) {
    let n = g.n.max(1) as f64;
    let sum_d: f64 = g.neighbors.iter().map(|l| l.len() as f64).sum();
    let sum_dd: f64 = g.neighbors.iter().map(|l| (l.len() * (l.len().saturating_sub(1))) as f64).sum();
    (sum_d / n, if sum_d > 0.0 { sum_dd / sum_d } else { 0.0 })
}

/// `rho_B` via nbsc; falls back to the mean excess degree (its value for
/// locally tree-like random graphs) if the Arnoldi run fails.
pub fn nb_radius(g: &Graph) -> (f64, bool) {
    let dim = 2 * g.n.min(40);
    let r = std::panic::catch_unwind(|| nbsc::spectral::estimate_spectral_radius(g, dim.max(2), 7));
    match r {
        Ok(v) if v.is_finite() => (v, true),
        _ => (excess_degree(g).1, false),
    }
}

struct Hessian<'a> {
    g: &'a Graph,
    r: f64,
}

impl LinearOperator for Hessian<'_> {
    fn dim(&self) -> usize {
        self.g.n
    }
    fn apply(&self, x: &DVector<f64>) -> DVector<f64> {
        let mut y = DVector::zeros(self.g.n);
        for i in 0..self.g.n {
            let d = self.g.neighbors[i].len() as f64;
            let mut s = (self.r * self.r - 1.0 + d) * x[i];
            for &j in &self.g.neighbors[i] {
                s -= self.r * x[j];
            }
            y[i] = s;
        }
        y
    }
}

const DENSE_LIMIT: usize = 1500;
const DENSE_FALLBACK_LIMIT: usize = 4000;

/// Smallest `want` eigenpairs of `H(r)`, ascending.
fn smallest(g: &Graph, r: f64, want: usize) -> Result<(Vec<f64>, DMatrix<f64>)> {
    let n = g.n;
    let want = want.min(n);
    let dense = |g: &Graph| -> (Vec<f64>, DMatrix<f64>) {
        let mut h = DMatrix::<f64>::zeros(n, n);
        for i in 0..n {
            h[(i, i)] = r * r - 1.0 + g.neighbors[i].len() as f64;
            for &j in &g.neighbors[i] {
                h[(i, j)] = -r;
            }
        }
        let e = nalgebra::linalg::SymmetricEigen::new(h);
        let mut ord: Vec<usize> = (0..n).collect();
        ord.sort_by(|&a, &b| e.eigenvalues[a].total_cmp(&e.eigenvalues[b]));
        let vals = ord.iter().take(want).map(|&i| e.eigenvalues[i]).collect();
        let mut vecs = DMatrix::<f64>::zeros(n, want);
        for (c, &i) in ord.iter().take(want).enumerate() {
            vecs.set_column(c, &e.eigenvectors.column(i));
        }
        (vals, vecs)
    };
    if n <= DENSE_LIMIT {
        return Ok(dense(g));
    }
    let op = Hessian { g, r };
    match lanczos_smallest(&op, want, n.min(800), 1e-6, 0xBE7E) {
        Ok(e) => Ok((e.eigenvalues.iter().copied().collect(), e.eigenvectors)),
        Err(_) if n <= DENSE_FALLBACK_LIMIT => Ok(dense(g)),
        Err(e) => Err(Error::Query(format!("Bethe Hessian eigensolver did not converge: {e}"))),
    }
}

/// Detect communities. `k = None` uses the number of negative eigenvalues of
/// `H(sqrt(rho_B))`; `Some(k)` forces `k` clusters.
pub fn communities(view: &View, k: Option<usize>) -> Result<Communities> {
    let (g, ids) = clique_graph(view);
    if g.n < 2 || g.m() == 0 {
        return Err(Error::Query("community detection needs at least one edge with 2+ members".into()));
    }
    let (mean_degree, mean_excess_degree) = excess_degree(&g);
    let (rho, _) = nb_radius(&g);
    let r = if rho > 1.0 + 1e-9 { rho.sqrt() } else { 0.0 };
    if r == 0.0 {
        // tree-like: no non-backtracking structure to detect
        return Ok(Communities { ids, assignment: vec![0; g.n], k: 1, rho_b: rho, mean_degree, mean_excess_degree, eigenvalues: vec![] });
    }
    // Probe more eigenvalues until the count of negative ones is strictly
    // below the number computed (else it is only a lower bound).
    let mut probe = k.map_or(12, |k| k + 2).min(g.n);
    let (mut vals, mut vecs) = smallest(&g, r, probe)?;
    let mut neg = vals.iter().filter(|&&v| v < -1e-9).count();
    while k.is_none() && neg == probe && probe < 96.min(g.n) {
        probe = (probe * 2).min(96).min(g.n);
        (vals, vecs) = smallest(&g, r, probe)?;
        neg = vals.iter().filter(|&&v| v < -1e-9).count();
    }
    let kk = k.unwrap_or(neg.max(1));
    if kk > g.n {
        return Err(Error::Query("k exceeds the number of connected nodes".into()));
    }
    if kk > vecs.ncols() {
        return Err(Error::Query(format!("requested {kk} communities but only {} eigenvectors were computed", vecs.ncols())));
    }
    let assignment = if kk == 1 {
        vec![0; g.n]
    } else {
        // skip the first (trivial, degree-like) eigenvector, as Saade et al. do
        let pts: Vec<Vec<f64>> = (0..g.n).map(|i| (1..kk).map(|c| vecs[(i, c)]).collect()).collect();
        crate::kmeans::kmeans(&pts, kk, 0xC0FFEE)
    };
    Ok(Communities { ids, assignment, k: kk, rho_b: rho, mean_degree, mean_excess_degree, eigenvalues: vals })
}

type Rows = (Vec<&'static str>, Vec<Vec<Value>>);

pub fn call(view: &View, name: &str, args: &[Value]) -> Result<Rows> {
    match name {
        "communities" => {
            let k = match args.first() {
                None | Some(Value::Null) => None,
                Some(v) => Some(v.as_i64().filter(|x| *x >= 1).ok_or_else(|| Error::Query("k must be a positive integer".into()))? as usize),
            };
            let c = communities(view, k)?;
            Ok((vec!["node", "community"], c.ids.iter().zip(&c.assignment).map(|(&i, &a)| vec![Value::Node(i), Value::Int(a as i64)]).collect()))
        }
        "nb_radius" => {
            let (g, _) = clique_graph(view);
            if g.m() == 0 {
                return Err(Error::Query("graph has no edges".into()));
            }
            let (mean_deg, excess) = excess_degree(&g);
            let (rho, from_nbsc) = nb_radius(&g);
            // The community count needs the Bethe Hessian eigensolve, which costs far more than rho_B itself
            // (about 80 s vs 4 s on a 109k-node registry), so it is opt-in: `CALL spectral.nb_radius(true)`.
            let communities = match args.first() {
                Some(Value::Bool(true)) => Value::Int(communities(view, None)?.k as i64),
                _ => Value::Null,
            };
            Ok((
                vec!["nodes", "edges", "mean_degree", "mean_excess_degree", "rho_b", "rho_b_from_arnoldi", "communities"],
                vec![vec![
                    Value::Int(g.n as i64),
                    Value::Int(g.m() as i64),
                    Value::Float(mean_deg),
                    Value::Float(excess),
                    Value::Float(rho),
                    Value::Bool(from_nbsc),
                    communities,
                ]],
            ))
        }
        other => Err(Error::Query(format!("unknown community procedure '{other}'"))),
    }
}
