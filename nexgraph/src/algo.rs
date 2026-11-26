//! Graph algorithms and procedures callable from NQL via `CALL`.
//!
//! Hyperedge semantics for algorithms: a directed hyperedge sends weight from
//! every tail member to every head member; an undirected hyperedge connects
//! all its members pairwise. Spectral procedures use the normalised
//! hypergraph Laplacian of Zhou-Huang-Schoelkopf via the `spectral_hypergraph`
//! crate, over the nodes that belong to at least one edge of >= 2 distinct
//! members with positive weight.

use crate::error::{Error, Result};
use crate::model::Body;
use crate::store::{Direction, View};
use crate::value::{Id, Value};
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};

type Rows = (Vec<&'static str>, Vec<Vec<Value>>);

fn bad<T>(m: impl Into<String>) -> Result<T> {
    Err(Error::Query(m.into()))
}

fn node_arg(a: &[Value], i: usize, what: &str) -> Result<Id> {
    match a.get(i) {
        Some(Value::Node(id)) => Ok(*id),
        _ => bad(format!("{what} must be a node")),
    }
}

fn int_arg(a: &[Value], i: usize, default: i64) -> Result<i64> {
    match a.get(i) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v.as_i64().ok_or_else(|| Error::Query("expected an integer argument".into())),
    }
}

pub fn call(view: &View, name: &str, args: &[Value]) -> Result<Rows> {
    if let Some(r) = name.to_ascii_lowercase().strip_prefix("algo.").and_then(|rest| crate::analytics::call_algo(view, rest, args)) {
        return r;
    }
    match name.to_ascii_lowercase().as_str() {
        "db.stats" => {
            let s = view.stats();
            Ok((
                vec!["nodes", "edges", "versions", "ts"],
                vec![vec![Value::Int(s.live_nodes as i64), Value::Int(s.live_edges as i64), Value::Int(s.versions as i64), Value::Int(view.ts as i64)]],
            ))
        }
        "algo.pagerank" => {
            let damping = args.first().and_then(|v| v.as_f64()).unwrap_or(0.85);
            let iters = int_arg(args, 1, 50)?.clamp(1, 1000) as usize;
            let (ids, pr) = pagerank(view, damping, iters);
            Ok((vec!["node", "score"], ids.into_iter().zip(pr).map(|(i, s)| vec![Value::Node(i), Value::Float(s)]).collect()))
        }
        "algo.components" => {
            let (ids, comp) = components(view);
            Ok((vec!["node", "component"], ids.into_iter().zip(comp).map(|(i, c)| vec![Value::Node(i), Value::Int(c as i64)]).collect()))
        }
        "algo.shortestpath" => {
            let (a, b) = (node_arg(args, 0, "source")?, node_arg(args, 1, "target")?);
            Ok((vec!["path", "length"], shortest_path(view, a, b).map(|p| vec![vec![Value::List(p.iter().map(|x| Value::Node(*x)).collect()), Value::Int(p.len() as i64 - 1)]]).unwrap_or_default()))
        }
        "algo.dijkstra" => {
            let (a, b) = (node_arg(args, 0, "source")?, node_arg(args, 1, "target")?);
            Ok((
                vec!["path", "distance"],
                dijkstra(view, a, b)?.map(|(p, d)| vec![vec![Value::List(p.iter().map(|x| Value::Node(*x)).collect()), Value::Float(d)]]).unwrap_or_default(),
            ))
        }
        "vector.search" => {
            let label = args.first().and_then(|v| v.as_str()).ok_or_else(|| Error::Query("vector.search(label, key, query, k[, exact])".into()))?;
            let key = args.get(1).and_then(|v| v.as_str()).ok_or_else(|| Error::Query("key must be a string".into()))?;
            let q = args.get(2).and_then(|v| v.to_vector()).ok_or_else(|| Error::Query("query must be a vector or list of numbers".into()))?;
            let k = int_arg(args, 3, 10)?.max(0) as usize;
            let exact = matches!(args.get(4), Some(Value::Bool(true)));
            let hits = view.vector_search(label, key, &q, k, 64, exact)?;
            Ok((vec!["node", "distance"], hits.into_iter().map(|(i, d)| vec![Value::Node(i), Value::Float(d as f64)]).collect()))
        }
        n if n.starts_with("stats.") => crate::analytics::call_stats(view, &n[6..], args),
        n if n.starts_with("cyber.") => crate::cyber::call(view, &n[6..], args),
        n if n.starts_with("killchain.") => crate::killchain::call(view, &n[10..], args),
        n if n.starts_with("alm.") => crate::alm::call(view, &n[4..], args),
        n if n.starts_with("land.") => crate::land::call(view, &n[5..], args),
        n if n.starts_with("swarm.") => crate::swarm::call(view, n, args),
        n if n.starts_with("fin.") => crate::fin::call(view, n, args),
        n if n.starts_with("gaap.") => crate::gaap::call(view, n, args),
        n if n.starts_with("uw.") => crate::uw::call(view, &n[3..], args),
        n if n.starts_with("physics.") => crate::physics::call(view, &n[8..], args),
        n if n.starts_with("spectral.") => spectral_call(view, &n[9..], args),
        _ => bad(format!(
            "unknown procedure '{name}' (available: db.stats, algo.pagerank, algo.components, algo.shortestPath, algo.dijkstra, algo.degree, algo.betweenness, algo.closeness, algo.kcore, algo.triangles, algo.scc, algo.articulation, algo.louvain, algo.similarity, stats.describe, stats.outliers, stats.histogram, stats.correlation, stats.timeseries, stats.spikes, cyber.reachable, cyber.attack_paths, cyber.chokepoints, cyber.propagate_risk, cyber.fanout, cyber.beaconing, cyber.volume, cyber.ioc, killchain.funnel, killchain.control_effect, killchain.exposure, killchain.response, killchain.defense_depth, alm.position, alm.liability_profile, alm.duration_gap, alm.rate_shock, alm.cashflow_ladder, alm.krd_gap, alm.scenario_risk, land.conflicts, land.rule_check, land.hotspots, land.valuation, land.survey_check, uw.audit, uw.scorecard, uw.gains, uw.scores, uw.disparity, uw.group_gap, uw.psi, uw.affordability, fin.identity, gaap.trial_balance, gaap.income_statement, gaap.retained_earnings, gaap.balance_sheet, gaap.cash_flow, gaap.ledger, gaap.audit, gaap.ratios, gaap.open_items, gaap.aging, swarm.detect, vector.search, spectral.fiedler, spectral.cluster, spectral.eigenvalues, spectral.embedding, spectral.communities, spectral.nb_radius, physics.heat, physics.heat_from, physics.harmonic)"
        )),
    }
}

// ------------------------------------------------------- plain algorithms

fn weighted_graph(view: &View) -> (Vec<Id>, HashMap<Id, usize>, Vec<Vec<(usize, f64)>>) {
    // dense id -> index table (no hashing on the edge loop)
    const NO: u32 = u32::MAX;
    let ids: Vec<Id> = view.node_ids().collect();
    let mut dense = vec![NO; view.id_bound()];
    ids.iter().enumerate().for_each(|(i, &x)| dense[x as usize] = i as u32);
    let lookup = |m: &Id| -> Option<usize> { dense.get(*m as usize).copied().filter(|&d| d != NO).map(|d| d as usize) };
    let mut adj = vec![Vec::new(); ids.len()];
    for e in view.edge_ids() {
        if let Some(Body::Edge { tail, head, weight, .. }) = view.edge(e) {
            if head.is_empty() {
                let mem: Vec<usize> = tail.iter().filter_map(|m| lookup(m)).collect();
                let k = mem.len();
                if k >= 2 {
                    let w = weight / (k - 1) as f64;
                    for &a in &mem {
                        for &b in &mem {
                            if a != b {
                                adj[a].push((b, w));
                            }
                        }
                    }
                }
            } else {
                for t in tail {
                    for h in head {
                        if let (Some(a), Some(b)) = (lookup(t), lookup(h)) {
                            adj[a].push((b, *weight));
                        }
                    }
                }
            }
        }
    }
    (ids, HashMap::new(), adj)
}

pub fn pagerank(view: &View, damping: f64, iters: usize) -> (Vec<Id>, Vec<f64>) {
    let (ids, _, adj) = weighted_graph(view);
    let n = ids.len();
    if n == 0 {
        return (ids, vec![]);
    }
    // CSR with the per-edge transition weight precomputed.
    let out_w: Vec<f64> = adj.iter().map(|a| a.iter().map(|x| x.1.max(0.0)).sum()).collect();
    let mut off = Vec::with_capacity(n + 1);
    let mut tgt: Vec<u32> = Vec::new();
    let mut wt: Vec<f64> = Vec::new();
    off.push(0usize);
    for u in 0..n {
        if out_w[u] > 0.0 {
            for &(v, w) in &adj[u] {
                tgt.push(v as u32);
                wt.push(damping * w.max(0.0) / out_w[u]);
            }
        }
        off.push(tgt.len());
    }
    drop(adj);
    let mut pr = vec![1.0 / n as f64; n];
    let mut next = vec![0.0; n];
    for _ in 0..iters {
        next.iter_mut().for_each(|x| *x = (1.0 - damping) / n as f64);
        let mut dangling = 0.0;
        for u in 0..n {
            if out_w[u] <= 0.0 {
                dangling += pr[u];
            } else {
                let pu = pr[u];
                for k in off[u]..off[u + 1] {
                    next[tgt[k] as usize] += pu * wt[k];
                }
            }
        }
        let add = damping * dangling / n as f64;
        let mut delta = 0.0;
        for i in 0..n {
            next[i] += add;
            delta += (next[i] - pr[i]).abs();
        }
        std::mem::swap(&mut pr, &mut next);
        if delta < 1e-12 {
            break;
        }
    }
    (ids, pr)
}

pub fn components(view: &View) -> (Vec<Id>, Vec<usize>) {
    let (ids, _, adj) = weighted_graph(view);
    let n = ids.len();
    let mut und = vec![Vec::new(); n];
    for (u, l) in adj.iter().enumerate() {
        for &(v, _) in l {
            und[u].push(v);
            und[v].push(u);
        }
    }
    let mut comp = vec![usize::MAX; n];
    let mut c = 0;
    for s in 0..n {
        if comp[s] != usize::MAX {
            continue;
        }
        let mut stack = vec![s];
        comp[s] = c;
        while let Some(u) = stack.pop() {
            for &v in &und[u] {
                if comp[v] == usize::MAX {
                    comp[v] = c;
                    stack.push(v);
                }
            }
        }
        c += 1;
    }
    (ids, comp)
}

/// Unweighted shortest path following edge direction (undirected hyperedges
/// are crossed freely).
pub fn shortest_path(view: &View, a: Id, b: Id) -> Option<Vec<Id>> {
    view.node(a)?;
    view.node(b)?;
    if a == b {
        return Some(vec![a]);
    }
    // Bidirectional BFS over dense per-id arrays: always expand the smaller
    // frontier, one whole level at a time, so the first meeting is optimal.
    let n = view.id_bound();
    const NONE: u64 = u64::MAX;
    let mut fwd = vec![NONE; n]; // predecessor towards `a`
    let mut bwd = vec![NONE; n]; // successor towards `b`
    let mut df = vec![u32::MAX; n];
    let mut db = vec![u32::MAX; n];
    fwd[a as usize] = a;
    bwd[b as usize] = b;
    df[a as usize] = 0;
    db[b as usize] = 0;
    let (mut ff, mut bf) = (vec![a], vec![b]);
    let (mut lf, mut lb) = (0u32, 0u32);
    while !ff.is_empty() && !bf.is_empty() {
        let mut best: Option<(u32, Id)> = None; // (total length, meeting node)
        if ff.len() <= bf.len() {
            let mut next = Vec::new();
            for &u in &ff {
                for (_, body) in view.incident_iter(u) {
                    let dir = if matches!(body, Body::Edge { head, .. } if head.is_empty()) { Direction::Any } else { Direction::Out };
                    let _ = view.cross_each::<()>(u, body, dir, |v| {
                        let vi = v as usize;
                        if fwd[vi] == NONE {
                            fwd[vi] = u;
                            df[vi] = lf + 1;
                            next.push(v);
                            if db[vi] != u32::MAX {
                                let t = lf + 1 + db[vi];
                                if best.map_or(true, |(bt, _)| t < bt) {
                                    best = Some((t, v));
                                }
                            }
                        }
                        Ok(true)
                    });
                }
            }
            ff = next;
            lf += 1;
        } else {
            let mut next = Vec::new();
            for &v in &bf {
                for (_, body) in view.incident_iter(v) {
                    let dir = if matches!(body, Body::Edge { head, .. } if head.is_empty()) { Direction::Any } else { Direction::In };
                    let _ = view.cross_each::<()>(v, body, dir, |u| {
                        let ui = u as usize;
                        if bwd[ui] == NONE {
                            bwd[ui] = v;
                            db[ui] = lb + 1;
                            next.push(u);
                            if df[ui] != u32::MAX {
                                let t = lb + 1 + df[ui];
                                if best.map_or(true, |(bt, _)| t < bt) {
                                    best = Some((t, u));
                                }
                            }
                        }
                        Ok(true)
                    });
                }
            }
            bf = next;
            lb += 1;
        }
        if let Some((_, m)) = best {
            let mut p = vec![m];
            let mut c = m;
            while c != a {
                c = fwd[c as usize];
                p.push(c);
            }
            p.reverse();
            let mut c = m;
            while c != b {
                c = bwd[c as usize];
                p.push(c);
            }
            return Some(p);
        }
    }
    None
}

#[derive(PartialEq)]
struct Item(f64, Id);
impl Eq for Item {}
impl PartialOrd for Item {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Item {
    fn cmp(&self, o: &Self) -> Ordering {
        o.0.total_cmp(&self.0).then(o.1.cmp(&self.1)) // min-heap
    }
}

/// Dijkstra over edge weights (must be non-negative).
pub fn dijkstra(view: &View, a: Id, b: Id) -> Result<Option<(Vec<Id>, f64)>> {
    if view.node(a).is_none() || view.node(b).is_none() {
        return Ok(None);
    }
    let mut dist: HashMap<Id, f64> = HashMap::from([(a, 0.0)]);
    let mut prev: HashMap<Id, Id> = HashMap::new();
    let mut heap = BinaryHeap::from([Item(0.0, a)]);
    while let Some(Item(d, u)) = heap.pop() {
        if d > *dist.get(&u).unwrap_or(&f64::INFINITY) {
            continue;
        }
        if u == b {
            let mut p = vec![b];
            let mut c = b;
            while c != a {
                c = prev[&c];
                p.push(c);
            }
            p.reverse();
            return Ok(Some((p, d)));
        }
        for (_, body) in view.incident(u) {
            let Body::Edge { weight, head, .. } = body else { continue };
            if *weight < 0.0 {
                return bad("dijkstra requires non-negative edge weights");
            }
            let dir = if head.is_empty() { Direction::Any } else { Direction::Out };
            for v in view.cross(u, body, dir) {
                let nd = d + weight;
                if nd < *dist.get(&v).unwrap_or(&f64::INFINITY) {
                    dist.insert(v, nd);
                    prev.insert(v, u);
                    heap.push(Item(nd, v));
                }
            }
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------- spectral

#[cfg(not(feature = "spectral"))]
fn spectral_call(_: &View, _: &str, _: &[Value]) -> Result<Rows> {
    Err(Error::Unsupported("built without the `spectral` feature".into()))
}

#[cfg(feature = "spectral")]
mod spec {
    use super::*;
    use nalgebra::DMatrix;
    use spectral_hypergraph::hypergraph::HypergraphBuilder;
    use spectral_hypergraph::laplacian::HypergraphOperator;
    use spectral_hypergraph::spectral::{dense_eigen, lanczos_smallest};
    use spectral_hypergraph::SpectralHypergraph;

    pub const DENSE_LIMIT: usize = 1500;

    fn herr(e: spectral_hypergraph::HypergraphError) -> Error {
        Error::Query(format!("spectral: {e}"))
    }

    /// Build the hypergraph of the snapshot. Returns it with the node id of
    /// each vertex (vertex `i` is `ids[i]`).
    pub fn build(view: &View) -> Result<(SpectralHypergraph, Vec<Id>)> {
        let mut edges: Vec<(Vec<Id>, f64)> = Vec::new();
        let mut participating: Vec<Id> = Vec::new();
        for e in view.edge_ids() {
            if let Some(b @ Body::Edge { weight, .. }) = view.edge(e) {
                let m = b.members();
                if m.len() >= 2 && *weight > 0.0 && weight.is_finite() {
                    participating.extend(m.iter().copied());
                    edges.push((m, *weight));
                }
            }
        }
        participating.sort_unstable();
        participating.dedup();
        if participating.len() < 2 {
            return bad("spectral analysis needs at least 2 connected nodes");
        }
        let mut b = HypergraphBuilder::with_capacity(participating.len(), edges.len(), 3);
        let mut vid = HashMap::new();
        for &id in &participating {
            vid.insert(id, b.add_vertex(id.to_string()).map_err(herr)?);
        }
        for (m, w) in &edges {
            let vs: Vec<_> = m.iter().map(|x| vid[x]).collect();
            b.add_hyperedge(&vs, *w).map_err(herr)?;
        }
        Ok((b.build().map_err(herr)?, participating))
    }

    /// Dense solve of the normalised Laplacian (`k` smallest eigenpairs).
    /// `I - Dv^{-1/2} H W De^{-1} H^T Dv^{-1/2}` built directly in `O(n^2 + sum |e|^2)` time and `O(n^2)`
    /// memory. `spectral_hypergraph::dense_normalized_laplacian` forms `m x m` diagonal matrices (`m` =
    /// number of hyperedges), which needs 8 m^2 bytes: 80 GB for 100,000 parallel events over 47 vertices.
    /// Same matrix (checked against it in the unit tests), without that term.
    pub(super) fn dense_laplacian(hg: &SpectralHypergraph) -> std::result::Result<DMatrix<f64>, spectral_hypergraph::HypergraphError> {
        if hg.num_hyperedges() == 0 {
            return Err(spectral_hypergraph::HypergraphError::EmptyHyperEdgeSet);
        }
        let n = hg.num_vertices();
        let mut dv = vec![0.0f64; n];
        for v in hg.vertex_ids() {
            let d = hg.vertex_degree(v)?;
            if d <= 0.0 {
                return Err(spectral_hypergraph::HypergraphError::IsolatedVertex(hg.vertex_label(v)?.to_string()));
            }
            dv[v.0] = d;
        }
        let mut core = DMatrix::<f64>::zeros(n, n);
        for e in hg.hyperedge_ids() {
            let scale = hg.hyperedge_weight(e)? / hg.hyperedge_degree(e)? as f64;
            // Incidence weights are always 1 (`HypergraphBuilder::add_hyperedge`); `incidence_weight` itself is
            // O(degree of the vertex) per call, which is quadratic in the number of parallel events.
            let mem: Vec<(usize, f64)> = hg.hyperedge_members(e)?.into_iter().map(|v| (v.0, 1.0 / dv[v.0].sqrt())).collect();
            for &(i, hi) in &mem {
                for &(j, hj) in &mem {
                    core[(i, j)] += scale * hi * hj;
                }
            }
        }
        Ok(DMatrix::<f64>::identity(n, n) - core)
    }

    fn dense_k(hg: &SpectralHypergraph, k: usize) -> Result<(Vec<f64>, DMatrix<f64>)> {
        let l = dense_laplacian(hg).map_err(herr)?;
        let e = dense_eigen(&l);
        Ok((e.eigenvalues.iter().take(k).copied().collect(), e.eigenvectors.columns(0, k).into_owned()))
    }

    /// Above this size a failed Lanczos run is retried with the dense solver
    /// (memory ~ 8 n^2 bytes); beyond it the failure is reported.
    pub const DENSE_FALLBACK_LIMIT: usize = 4000;

    /// Smallest `k` eigenpairs of the normalised hypergraph Laplacian
    /// (ascending): dense solver up to `DENSE_LIMIT` vertices; above that
    /// Lanczos with up to `max(300, 20k)` steps, falling back to the dense
    /// solver (up to `DENSE_FALLBACK_LIMIT` vertices) if it does not converge.
    pub fn decompose(hg: &SpectralHypergraph, k: usize) -> Result<(Vec<f64>, DMatrix<f64>)> {
        let n = hg.num_vertices();
        if k == 0 || k > n {
            return bad(format!("k must be in 1..={n}"));
        }
        if n <= DENSE_LIMIT {
            return dense_k(hg, k);
        }
        let op = HypergraphOperator::new(hg).map_err(herr)?;
        match lanczos_smallest(&op, k, n.min((20 * k).max(300)), 1e-6, 0x5EC7) {
            // A Krylov space can collapse early (invariant subspace) when the Laplacian has a huge
            // null space, e.g. a graph made of many disconnected components. Never hand back fewer than
            // `k` eigenpairs: callers index `k` columns.
            Ok(e) if e.eigenvectors.ncols() >= k && e.eigenvalues.len() >= k && e.eigenvectors.nrows() == n => Ok((e.eigenvalues.iter().copied().collect(), e.eigenvectors)),
            Ok(_) if n <= DENSE_FALLBACK_LIMIT => dense_k(hg, k),
            Ok(e) => bad(format!(
                "spectral: only {} of the {k} requested eigenpairs could be computed on {n} vertices. This happens when the graph is made of many disconnected components \
                 (the Laplacian null space is then huge). Restrict the analysis to one component (see algo.components) or ask for fewer eigenvectors",
                e.eigenvectors.ncols()
            )),
            Err(_) if n <= DENSE_FALLBACK_LIMIT => dense_k(hg, k),
            Err(e) => bad(format!("spectral: Lanczos did not converge on {n} vertices: {e}")),
        }
    }

    /// Spectral clustering: embed with the `k` smallest eigenvectors,
    /// row-normalise, k-means (Ng-Jordan-Weiss for hypergraphs).
    pub fn cluster(hg: &SpectralHypergraph, k: usize) -> Result<Vec<usize>> {
        let n = hg.num_vertices();
        if k == 0 || k > n {
            return bad(format!("k must be in 1..={n}"));
        }
        let (_, v) = decompose(hg, k)?;
        let pts: Vec<Vec<f64>> = (0..n)
            .map(|i| {
                let row: Vec<f64> = (0..k).map(|j| v[(i, j)]).collect();
                let norm = row.iter().map(|x| x * x).sum::<f64>().sqrt();
                if norm > 1e-12 { row.iter().map(|x| x / norm).collect() } else { row }
            })
            .collect();
        Ok(if k == 1 { vec![0; n] } else { crate::kmeans::kmeans(&pts, k, 42) })
    }
}

#[cfg(feature = "spectral")]
fn spectral_call(view: &View, name: &str, args: &[Value]) -> Result<Rows> {
    if matches!(name, "communities" | "nb_radius") {
        #[cfg(feature = "nonbacktracking")]
        return crate::community::call(view, name, args);
        #[cfg(not(feature = "nonbacktracking"))]
        return Err(Error::Unsupported("built without the `nonbacktracking` feature".into()));
    }
    let (hg, ids) = spec::build(view)?;
    match name {
        "fiedler" => {
            let (_, v) = spec::decompose(&hg, 2)?;
            Ok((vec!["node", "value"], ids.iter().enumerate().map(|(i, &id)| vec![Value::Node(id), Value::Float(v[(i, 1)])]).collect()))
        }
        "cluster" => {
            let k = int_arg(args, 0, 2)?.max(1) as usize;
            let a = spec::cluster(&hg, k)?;
            Ok((vec!["node", "cluster"], ids.iter().zip(a).map(|(&id, c)| vec![Value::Node(id), Value::Int(c as i64)]).collect()))
        }
        "eigenvalues" => {
            let k = int_arg(args, 0, 6)?.max(1) as usize;
            let (vals, _) = spec::decompose(&hg, k.min(ids.len()))?;
            Ok((vec!["index", "eigenvalue"], vals.into_iter().enumerate().map(|(i, v)| vec![Value::Int(i as i64), Value::Float(v)]).collect()))
        }
        "embedding" => {
            let k = int_arg(args, 0, 3)?.max(1) as usize;
            if k + 1 > ids.len() {
                return bad("embedding dimension must be < number of connected nodes");
            }
            let (_, v) = spec::decompose(&hg, k + 1)?;
            Ok((
                vec!["node", "embedding"],
                ids.iter().enumerate().map(|(i, &id)| vec![Value::Node(id), Value::Vector((1..=k).map(|j| v[(i, j)] as f32).collect())]).collect(),
            ))
        }
        other => bad(format!("unknown spectral procedure '{other}' (fiedler, cluster, eigenvalues, embedding)")),
    }
}

/// Smallest eigenvalues of the normalised hypergraph Laplacian for a snapshot
/// (library entry point; used by tests and examples).
#[cfg(feature = "spectral")]
pub fn laplacian_spectrum(view: &View, k: usize) -> Result<Vec<f64>> {
    let (hg, _) = spec::build(view)?;
    Ok(spec::decompose(&hg, k.min(hg.num_vertices()))?.0)
}


#[cfg(all(test, feature = "spectral"))]
mod spec_tests {
    use nalgebra::DMatrix;
    use spectral_hypergraph::hypergraph::HypergraphBuilder;
    use spectral_hypergraph::laplacian::dense_normalized_laplacian;

    /// The O(n^2) construction is the same matrix as the reference m x m one, on random weighted hypergraphs
    /// with parallel edges and mixed cardinalities.
    #[test]
    fn direct_dense_laplacian_equals_reference() {
        let mut seed = 0x9E3779B97F4A7C15u64;
        let mut rnd = move |m: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % m
        };
        for trial in 0..20 {
            let n = 5 + rnd(20) as usize;
            let mut b = HypergraphBuilder::new();
            let vs: Vec<_> = (0..n).map(|i| b.add_vertex(format!("v{i}")).unwrap()).collect();
            let mut covered = vec![false; n];
            for _ in 0..(10 + rnd(40)) {
                let k = 2 + rnd(4) as usize;
                let mut m: Vec<usize> = (0..k).map(|_| rnd(n as u64) as usize).collect();
                m.sort();
                m.dedup();
                if m.len() < 2 {
                    continue;
                }
                m.iter().for_each(|&i| covered[i] = true);
                let w = 0.5 + rnd(100) as f64 / 25.0;
                let ids: Vec<_> = m.iter().map(|&i| vs[i]).collect();
                b.add_hyperedge(&ids, w).unwrap();
                if rnd(3) == 0 {
                    b.add_hyperedge(&ids, w).unwrap(); // parallel duplicate
                }
            }
            for i in 0..n {
                if !covered[i] {
                    b.add_hyperedge(&[vs[i], vs[(i + 1) % n]], 1.0).unwrap();
                }
            }
            let hg = b.build().unwrap();
            let a: DMatrix<f64> = dense_normalized_laplacian(&hg).unwrap();
            let c = super::spec::dense_laplacian(&hg).unwrap();
            assert!((a - c).amax() < 1e-12, "trial {trial}");
        }
    }
}
