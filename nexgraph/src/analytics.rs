//! Data-analytics procedures (v0.7).
//!
//! Graph metrics (`algo.degree`, `algo.betweenness`, `algo.closeness`,
//! `algo.kcore`, `algo.triangles`, `algo.scc`, `algo.articulation`,
//! `algo.louvain`, `algo.similarity`) and descriptive statistics over
//! node/edge properties (`stats.describe`, `stats.outliers`, `stats.histogram`,
//! `stats.correlation`, `stats.timeseries`, `stats.spikes`).
//!
//! All procedures read the caller's MVCC snapshot, so they combine with
//! `AS OF` and never block writers. Every one is deterministic: ties are
//! broken by node id and sampling picks evenly spaced sources.

use crate::error::Result;
use crate::netgraph::*;
use crate::store::View;
use crate::value::{Id, Value};
use std::collections::{BTreeMap, HashMap, VecDeque};

fn f(x: f64) -> Value {
    if x.is_finite() {
        Value::Float(x)
    } else {
        Value::Null
    }
}

pub(crate) fn call_algo(view: &View, name: &str, a: &[Value]) -> Option<Result<Rows>> {
    Some(match name {
        "degree" => degree(view, a),
        "betweenness" => betweenness(view, a),
        "closeness" => closeness(view, a),
        "kcore" => kcore(view, a),
        "triangles" => triangles(view, a),
        "scc" => scc(view, a),
        "articulation" => articulation(view, a),
        "louvain" => louvain(view, a),
        "similarity" => similarity(view, a),
        _ => return None,
    })
}

pub(crate) fn call_stats(view: &View, name: &str, a: &[Value]) -> Result<Rows> {
    match name {
        "describe" => describe(view, a),
        "outliers" => outliers(view, a),
        "histogram" => histogram(view, a),
        "correlation" => correlation(view, a),
        "timeseries" => timeseries(view, a),
        "spikes" => spikes(view, a),
        _ => bad(format!("unknown procedure 'stats.{name}' (available: stats.describe, stats.outliers, stats.histogram, stats.correlation, stats.timeseries, stats.spikes)")),
    }
}

// ------------------------------------------------------------ graph metrics

/// `algo.degree([edgeLabel])` -> node, in_degree, out_degree, neighbours, in_weight, out_weight
/// (degrees count edge incidences; `neighbours` counts distinct adjacent nodes).
fn degree(view: &View, a: &[Value]) -> Result<Rows> {
    let net = Net::build(view, str_arg(a, 0));
    let und = net.simple_undirected();
    let rows = (0..net.n())
        .map(|u| {
            let ow: f64 = net.out[u].iter().map(|x| x.1).sum();
            let iw: f64 = net.inn[u].iter().map(|x| x.1).sum();
            vec![Value::Node(net.ids[u]), Value::Int(net.inn[u].len() as i64), Value::Int(net.out[u].len() as i64), Value::Int(und[u].len() as i64), f(iw), f(ow)]
        })
        .collect();
    Ok((vec!["node", "in_degree", "out_degree", "neighbours", "in_weight", "out_weight"], rows))
}

/// Refuse all-sources traversals whose cost (sources x (nodes + arcs)) is unreasonable; the caller
/// should sample or narrow the entry set. Keeps one request from monopolising a shared server.
pub(crate) fn work_guard(sources: usize, n: usize, arcs: usize, what: &str) -> Result<()> {
    const BUDGET: u128 = 4_000_000_000;
    if (sources as u128) * ((n + arcs) as u128) > BUDGET {
        return bad(format!("{what}: {sources} sources over {n} nodes / {arcs} arcs exceeds the work budget; pass a sample size (e.g. 500) or narrow the entry set"));
    }
    Ok(())
}

fn pick_sources(n: usize, sample: usize) -> Vec<usize> {
    if sample == 0 || sample >= n {
        (0..n).collect()
    } else {
        (0..sample).map(|i| i * n / sample).collect()
    }
}

/// Brandes dependency accumulation from `sources`. With `targets`, only paths
/// ending in a target node are counted. Unweighted, directed, counts shortest
/// paths between distinct node pairs.
pub(crate) fn brandes(out: &[Vec<u32>], inn: &[Vec<u32>], sources: &[usize], targets: Option<&[bool]>) -> Vec<f64> {
    let n = out.len();
    let mut cb = vec![0.0f64; n];
    let (mut sigma, mut dist, mut delta) = (vec![0.0f64; n], vec![-1i32; n], vec![0.0f64; n]);
    let mut order: Vec<u32> = Vec::with_capacity(n);
    let mut q = VecDeque::new();
    for &s in sources {
        order.clear();
        sigma.iter_mut().for_each(|x| *x = 0.0);
        dist.iter_mut().for_each(|x| *x = -1);
        delta.iter_mut().for_each(|x| *x = 0.0);
        sigma[s] = 1.0;
        dist[s] = 0;
        q.push_back(s as u32);
        while let Some(u) = q.pop_front() {
            order.push(u);
            for &v in &out[u as usize] {
                if dist[v as usize] < 0 {
                    dist[v as usize] = dist[u as usize] + 1;
                    q.push_back(v);
                }
                if dist[v as usize] == dist[u as usize] + 1 {
                    sigma[v as usize] += sigma[u as usize];
                }
            }
        }
        for &w in order.iter().rev() {
            let w = w as usize;
            let coef = match targets {
                None => 1.0,
                Some(t) => t[w] as u8 as f64,
            };
            for &v in &inn[w] {
                let v = v as usize;
                if dist[v] >= 0 && dist[v] + 1 == dist[w] {
                    delta[v] += sigma[v] / sigma[w] * (coef + delta[w]);
                }
            }
            if w != s {
                cb[w] += delta[w];
            }
        }
    }
    cb
}

/// `algo.betweenness([sample[, edgeLabel]])` -> node, score. Exact (Brandes) by default;
/// with `sample = k` it uses k evenly spaced sources and scales by n/k (an estimate).
fn betweenness(view: &View, a: &[Value]) -> Result<Rows> {
    let sample = usize_arg(a, 0, 0, "sample")?;
    let net = Net::build(view, str_arg(a, 1));
    let (out, inn) = net.simple_directed();
    let src = pick_sources(net.n(), sample);
    work_guard(src.len(), net.n(), out.iter().map(|l| l.len()).sum(), "algo.betweenness")?;
    let scale = if src.is_empty() { 1.0 } else { net.n() as f64 / src.len() as f64 };
    let cb = brandes(&out, &inn, &src, None);
    Ok((vec!["node", "score"], (0..net.n()).map(|u| vec![Value::Node(net.ids[u]), f(cb[u] * scale)]).collect()))
}

/// `algo.closeness([sample[, edgeLabel]])` -> node, harmonic. Inbound harmonic centrality
/// `sum over sources s of 1/d(s,t)` (well defined on disconnected graphs); sampled like betweenness.
fn closeness(view: &View, a: &[Value]) -> Result<Rows> {
    let sample = usize_arg(a, 0, 0, "sample")?;
    let net = Net::build(view, str_arg(a, 1));
    let (out, _) = net.simple_directed();
    let n = net.n();
    let src = pick_sources(n, sample);
    work_guard(src.len(), n, out.iter().map(|l| l.len()).sum(), "algo.closeness")?;
    let scale = if src.is_empty() { 1.0 } else { n as f64 / src.len() as f64 };
    let mut h = vec![0.0f64; n];
    let (mut dist, mut q) = (vec![-1i32; n], VecDeque::new());
    for &s in &src {
        dist.iter_mut().for_each(|x| *x = -1);
        dist[s] = 0;
        q.push_back(s as u32);
        while let Some(u) = q.pop_front() {
            for &v in &out[u as usize] {
                if dist[v as usize] < 0 {
                    dist[v as usize] = dist[u as usize] + 1;
                    h[v as usize] += 1.0 / dist[v as usize] as f64;
                    q.push_back(v);
                }
            }
        }
    }
    Ok((vec!["node", "harmonic"], (0..n).map(|u| vec![Value::Node(net.ids[u]), f(h[u] * scale)]).collect()))
}

/// `algo.kcore([edgeLabel])` -> node, core (Batagelj-Zaversnik peeling, undirected).
fn kcore(view: &View, a: &[Value]) -> Result<Rows> {
    let net = Net::build(view, str_arg(a, 0));
    let adj = net.simple_undirected();
    let n = net.n();
    let mut deg: Vec<usize> = adj.iter().map(|l| l.len()).collect();
    let md = deg.iter().copied().max().unwrap_or(0);
    let mut bin = vec![0usize; md + 2];
    deg.iter().for_each(|&d| bin[d] += 1);
    let mut start = 0;
    for b in bin.iter_mut().take(md + 1) {
        let c = *b;
        *b = start;
        start += c;
    }
    let (mut pos, mut vert) = (vec![0usize; n], vec![0usize; n]);
    for v in 0..n {
        pos[v] = bin[deg[v]];
        vert[pos[v]] = v;
        bin[deg[v]] += 1;
    }
    for d in (1..=md).rev() {
        bin[d] = bin[d - 1];
    }
    bin[0] = 0;
    for i in 0..n {
        let v = vert[i];
        for &u in &adj[v] {
            let u = u as usize;
            if deg[u] > deg[v] {
                let (du, pu) = (deg[u], pos[u]);
                let pw = bin[du];
                let w = vert[pw];
                if u != w {
                    pos[u] = pw;
                    vert[pu] = w;
                    pos[w] = pu;
                    vert[pw] = u;
                }
                bin[du] += 1;
                deg[u] -= 1;
            }
        }
    }
    Ok((vec!["node", "core"], (0..n).map(|u| vec![Value::Node(net.ids[u]), Value::Int(deg[u] as i64)]).collect()))
}

/// `algo.triangles([edgeLabel])` -> node, triangles, clustering (local clustering coefficient).
fn triangles(view: &View, a: &[Value]) -> Result<Rows> {
    let net = Net::build(view, str_arg(a, 0));
    let adj = net.simple_undirected();
    let n = net.n();
    let mut tri = vec![0i64; n];
    for u in 0..n {
        for &v in adj[u].iter().filter(|&&v| v as usize > u) {
            let (x, y) = (&adj[u], &adj[v as usize]);
            let (mut i, mut j) = (0, 0);
            while i < x.len() && j < y.len() {
                match x[i].cmp(&y[j]) {
                    std::cmp::Ordering::Less => i += 1,
                    std::cmp::Ordering::Greater => j += 1,
                    std::cmp::Ordering::Equal => {
                        if x[i] > v {
                            tri[u] += 1;
                            tri[v as usize] += 1;
                            tri[x[i] as usize] += 1;
                        }
                        i += 1;
                        j += 1;
                    }
                }
            }
        }
    }
    let rows = (0..n)
        .map(|u| {
            let d = adj[u].len() as f64;
            let cc = if d >= 2.0 { 2.0 * tri[u] as f64 / (d * (d - 1.0)) } else { 0.0 };
            vec![Value::Node(net.ids[u]), Value::Int(tri[u]), f(cc)]
        })
        .collect();
    Ok((vec!["node", "triangles", "clustering"], rows))
}

/// `algo.scc([edgeLabel])` -> node, component, size. Strongly connected components
/// (iterative Tarjan). In an attack graph a component of size > 1 is a set of mutually reachable assets.
fn scc(view: &View, a: &[Value]) -> Result<Rows> {
    let net = Net::build(view, str_arg(a, 0));
    let (out, _) = net.simple_directed();
    let n = net.n();
    let (mut index, mut low) = (vec![u32::MAX; n], vec![0u32; n]);
    let (mut on, mut comp) = (vec![false; n], vec![usize::MAX; n]);
    let (mut stack, mut next, mut nc) = (Vec::new(), 0u32, 0usize);
    for root in 0..n {
        if index[root] != u32::MAX {
            continue;
        }
        let mut call: Vec<(usize, usize)> = vec![(root, 0)];
        index[root] = next;
        low[root] = next;
        next += 1;
        stack.push(root);
        on[root] = true;
        while let Some(&mut (u, ref mut i)) = call.last_mut() {
            if *i < out[u].len() {
                let v = out[u][*i] as usize;
                *i += 1;
                if index[v] == u32::MAX {
                    index[v] = next;
                    low[v] = next;
                    next += 1;
                    stack.push(v);
                    on[v] = true;
                    call.push((v, 0));
                } else if on[v] {
                    low[u] = low[u].min(index[v]);
                }
            } else {
                call.pop();
                if let Some(&(p, _)) = call.last() {
                    low[p] = low[p].min(low[u]);
                }
                if low[u] == index[u] {
                    while let Some(w) = stack.pop() {
                        on[w] = false;
                        comp[w] = nc;
                        if w == u {
                            break;
                        }
                    }
                    nc += 1;
                }
            }
        }
    }
    let mut size = vec![0i64; nc];
    comp.iter().for_each(|&c| size[c] += 1);
    Ok((vec!["node", "component", "size"], (0..n).map(|u| vec![Value::Node(net.ids[u]), Value::Int(comp[u] as i64), Value::Int(size[comp[u]])]).collect()))
}

/// `algo.articulation([edgeLabel])` -> node, components_after_removal. Cut vertices of the
/// undirected graph: assets whose loss splits the network (single points of failure).
fn articulation(view: &View, a: &[Value]) -> Result<Rows> {
    let net = Net::build(view, str_arg(a, 0));
    let adj = net.simple_undirected();
    let n = net.n();
    let (mut disc, mut low) = (vec![u32::MAX; n], vec![0u32; n]);
    let mut extra = vec![0i64; n]; // child subtrees cut off from the parent side
    let mut is_cut = vec![false; n];
    let mut t = 0u32;
    for root in 0..n {
        if disc[root] != u32::MAX {
            continue;
        }
        let mut root_children = 0;
        let mut call: Vec<(usize, usize, usize)> = vec![(root, usize::MAX, 0)]; // node, parent, next idx
        disc[root] = t;
        low[root] = t;
        t += 1;
        while let Some(&mut (u, p, ref mut i)) = call.last_mut() {
            if *i < adj[u].len() {
                let v = adj[u][*i] as usize;
                *i += 1;
                if disc[v] == u32::MAX {
                    disc[v] = t;
                    low[v] = t;
                    t += 1;
                    if u == root {
                        root_children += 1;
                    }
                    call.push((v, u, 0));
                } else if v != p {
                    low[u] = low[u].min(disc[v]);
                }
            } else {
                call.pop();
                if p != usize::MAX {
                    low[p] = low[p].min(low[u]);
                    if p != root && low[u] >= disc[p] {
                        is_cut[p] = true;
                        extra[p] += 1;
                    }
                }
            }
        }
        if root_children > 1 {
            is_cut[root] = true;
            extra[root] = root_children as i64 - 1;
        }
    }
    // Components left after removing a cut vertex = extra + 1: a non-root vertex cuts off `extra`
    // child subtrees and leaves the parent side as one more; the root has (children - 1) = extra.
    let rows = (0..n)
        .filter(|&u| is_cut[u])
        .map(|u| {
            vec![Value::Node(net.ids[u]), Value::Int(extra[u] + 1)]
        })
        .collect();
    Ok((vec!["node", "components_after_removal"], rows))
}

/// `algo.louvain([resolution[, edgeLabel]])` -> node, community. Louvain modularity maximisation on the
/// undirected, weighted projection (parallel edges add their weights; weights must be non-negative).
/// Deterministic: nodes are visited in id order and a move needs a strictly positive gain. Communities are
/// numbered by first appearance. `resolution` (default 1.0) > 1 gives smaller communities.
fn louvain(view: &View, a: &[Value]) -> Result<Rows> {
    let res = f64_arg(a, 0, 1.0)?;
    if res <= 0.0 {
        return bad("resolution must be positive");
    }
    let net = Net::build(view, str_arg(a, 1));
    let n0 = net.n();
    let mut adj: Vec<BTreeMap<usize, f64>> = vec![BTreeMap::new(); n0];
    let mut selfw = vec![0.0f64; n0];
    for u in 0..n0 {
        for &(v, w, _) in &net.out[u] {
            let v = v as usize;
            if !(w >= 0.0) || !w.is_finite() {
                return bad("algo.louvain needs non-negative finite edge weights");
            }
            if u != v && w > 0.0 {
                *adj[u].entry(v).or_default() += w;
                *adj[v].entry(u).or_default() += w;
            }
        }
    }
    let mut member: Vec<usize> = (0..n0).collect(); // original node -> current super-node
    loop {
        let n = adj.len();
        let k: Vec<f64> = (0..n).map(|u| adj[u].values().sum::<f64>() + 2.0 * selfw[u]).collect();
        let m2: f64 = k.iter().sum();
        if m2 <= 0.0 {
            break;
        }
        let mut comm: Vec<usize> = (0..n).collect();
        let mut tot = k.clone();
        let mut moved_any = false;
        for _pass in 0..100 {
            let mut moved = false;
            for u in 0..n {
                let cu = comm[u];
                let mut to: BTreeMap<usize, f64> = BTreeMap::new();
                for (&v, &w) in &adj[u] {
                    *to.entry(comm[v]).or_default() += w;
                }
                tot[cu] -= k[u];
                let gain = |c: usize, w: f64| w - res * tot[c] * k[u] / m2;
                let (mut best, mut best_g) = (cu, gain(cu, to.get(&cu).copied().unwrap_or(0.0)));
                for (&c, &w) in &to {
                    let g = gain(c, w);
                    if g > best_g + 1e-12 {
                        best = c;
                        best_g = g;
                    }
                }
                tot[best] += k[u];
                if best != cu {
                    comm[u] = best;
                    moved = true;
                }
            }
            moved_any |= moved;
            if !moved {
                break;
            }
        }
        if !moved_any {
            break;
        }
        // renumber and aggregate
        let mut remap: HashMap<usize, usize> = HashMap::new();
        for u in 0..n {
            let nxt = remap.len();
            remap.entry(comm[u]).or_insert(nxt);
        }
        let nc = remap.len();
        let c: Vec<usize> = comm.iter().map(|x| remap[x]).collect();
        let mut nadj: Vec<BTreeMap<usize, f64>> = vec![BTreeMap::new(); nc];
        let mut nself = vec![0.0f64; nc];
        for u in 0..n {
            nself[c[u]] += selfw[u];
            for (&v, &w) in &adj[u] {
                if c[u] == c[v] {
                    nself[c[u]] += w / 2.0;
                } else {
                    *nadj[c[u]].entry(c[v]).or_default() += w;
                }
            }
        }
        member.iter_mut().for_each(|m| *m = c[*m]);
        adj = nadj;
        selfw = nself;
        if nc == n {
            break;
        }
    }
    let mut remap: HashMap<usize, i64> = HashMap::new();
    let rows = (0..n0)
        .map(|u| {
            let nxt = remap.len() as i64;
            let c = *remap.entry(member[u]).or_insert(nxt);
            vec![Value::Node(net.ids[u]), Value::Int(c)]
        })
        .collect();
    Ok((vec!["node", "community"], rows))
}

/// `algo.similarity(node[, k[, edgeLabel]])` -> node, jaccard, common. Neighbourhood
/// Jaccard similarity to `node` over the undirected graph, top-k by score then id.
fn similarity(view: &View, a: &[Value]) -> Result<Rows> {
    let id = node_arg(a, 0, "node")?;
    let k = usize_arg(a, 1, 10, "k")?;
    let net = Net::build(view, str_arg(a, 2));
    let adj = net.simple_undirected();
    let Some(s) = net.idx(id) else { return Ok((vec!["node", "jaccard", "common"], vec![])) };
    let mut common: BTreeMap<u32, u32> = BTreeMap::new();
    for &m in &adj[s] {
        for &v in &adj[m as usize] {
            if v as usize != s {
                *common.entry(v).or_default() += 1;
            }
        }
    }
    let mut rows: Vec<(f64, u32, u32)> = common
        .into_iter()
        .map(|(v, c)| {
            let un = adj[s].len() + adj[v as usize].len() - c as usize;
            (c as f64 / un as f64, v, c)
        })
        .collect();
    rows.sort_by(|x, y| y.0.total_cmp(&x.0).then(x.1.cmp(&y.1)));
    rows.truncate(k);
    Ok((vec!["node", "jaccard", "common"], rows.into_iter().map(|(j, v, c)| vec![Value::Node(net.ids[v as usize]), f(j), Value::Int(c as i64)]).collect()))
}

// ------------------------------------------------------------- statistics

fn label_key<'a>(a: &'a [Value], what: &str) -> Result<(Option<&'a str>, &'a str)> {
    Ok((str_arg(a, 0), need_str(a, 1, what)?))
}

/// `stats.describe(label, key)` -> count, min, p25, median, p75, p95, max, mean, stdev, missing.
/// `label` may be null (all nodes). Considers nodes and edges that carry the label.
fn describe(view: &View, a: &[Value]) -> Result<Rows> {
    let (label, key) = label_key(a, "key")?;
    let vals = numeric_prop(view, label, key);
    let total = match label {
        Some(l) => view.nodes_with_label(l).len() + view.edges_with_label(l).len(),
        None => view.node_ids().count(),
    };
    let x: Vec<f64> = vals.iter().map(|v| v.2).collect();
    let s = sorted(&x);
    let missing = Value::Int((total - x.len()) as i64);
    let row = if x.is_empty() {
        vec![Value::Int(0), Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, missing]
    } else {
        vec![Value::Int(x.len() as i64), f(s[0]), f(quantile(&s, 0.25)), f(quantile(&s, 0.5)), f(quantile(&s, 0.75)), f(quantile(&s, 0.95)), f(s[s.len() - 1]), f(mean(&x)), f(stdev(&x)), missing]
    };
    Ok((vec!["count", "min", "p25", "median", "p75", "p95", "max", "mean", "stdev", "missing"], vec![row]))
}

/// `stats.outliers(label, key[, threshold])` -> entity, value, z, robust_z. Rows whose
/// |robust z| (median/MAD) reaches `threshold` (default 3.5), most extreme first.
fn outliers(view: &View, a: &[Value]) -> Result<Rows> {
    let (label, key) = label_key(a, "key")?;
    let thr = f64_arg(a, 2, 3.5)?;
    let vals = numeric_prop(view, label, key);
    let x: Vec<f64> = vals.iter().map(|v| v.2).collect();
    let rz = robust_z(&x);
    let (m, sd) = if x.is_empty() { (0.0, 0.0) } else { (mean(&x), stdev(&x)) };
    let mut rows: Vec<(f64, Vec<Value>)> = vals
        .iter()
        .zip(&rz)
        .filter(|(_, z)| z.abs() >= thr)
        .map(|(v, z)| {
            let zs = if sd > 0.0 { (v.2 - m) / sd } else { 0.0 };
            (z.abs(), vec![ent(v.0, v.1), f(v.2), f(zs), f(*z)])
        })
        .collect();
    rows.sort_by(|p, q| q.0.total_cmp(&p.0));
    Ok((vec!["entity", "value", "z", "robust_z"], rows.into_iter().map(|r| r.1).collect()))
}

/// `stats.histogram(label, key[, bins])` -> bin, lo, hi, count (equal-width, default 10 bins).
fn histogram(view: &View, a: &[Value]) -> Result<Rows> {
    let (label, key) = label_key(a, "key")?;
    let bins = usize_arg(a, 2, 10, "bins")?;
    if bins == 0 || bins > 10_000 {
        return bad("bins must be between 1 and 10000");
    }
    let x: Vec<f64> = numeric_prop(view, label, key).iter().map(|v| v.2).collect();
    if x.is_empty() {
        return Ok((vec!["bin", "lo", "hi", "count"], vec![]));
    }
    let (lo, hi) = (x.iter().cloned().fold(f64::INFINITY, f64::min), x.iter().cloned().fold(f64::NEG_INFINITY, f64::max));
    let w = if hi > lo { (hi - lo) / bins as f64 } else { 1.0 };
    let mut c = vec![0i64; bins];
    for v in &x {
        c[(((v - lo) / w) as usize).min(bins - 1)] += 1;
    }
    Ok((vec!["bin", "lo", "hi", "count"], (0..bins).map(|i| vec![Value::Int(i as i64), f(lo + w * i as f64), f(lo + w * (i + 1) as f64), Value::Int(c[i])]).collect()))
}

fn ranks(x: &[f64]) -> Vec<f64> {
    let mut idx: Vec<usize> = (0..x.len()).collect();
    idx.sort_by(|&a, &b| x[a].total_cmp(&x[b]));
    let mut r = vec![0.0; x.len()];
    let mut i = 0;
    while i < idx.len() {
        let mut j = i;
        while j + 1 < idx.len() && x[idx[j + 1]] == x[idx[i]] {
            j += 1;
        }
        let avg = (i + j) as f64 / 2.0 + 1.0;
        (i..=j).for_each(|k| r[idx[k]] = avg);
        i = j + 1;
    }
    r
}

fn pearson(x: &[f64], y: &[f64]) -> f64 {
    let (mx, my) = (mean(x), mean(y));
    let (mut sxy, mut sxx, mut syy) = (0.0, 0.0, 0.0);
    for (a, b) in x.iter().zip(y) {
        sxy += (a - mx) * (b - my);
        sxx += (a - mx) * (a - mx);
        syy += (b - my) * (b - my);
    }
    if sxx == 0.0 || syy == 0.0 {
        f64::NAN
    } else {
        sxy / (sxx * syy).sqrt()
    }
}

/// `stats.correlation(label, keyA, keyB)` -> n, pearson, spearman over entities that have both keys.
fn correlation(view: &View, a: &[Value]) -> Result<Rows> {
    let label = str_arg(a, 0);
    let (ka, kb) = (need_str(a, 1, "keyA")?, need_str(a, 2, "keyB")?);
    let xa: HashMap<(Id, bool), f64> = numeric_prop(view, label, ka).into_iter().map(|v| ((v.0, v.1), v.2)).collect();
    let mut pairs: Vec<((Id, bool), f64, f64)> = numeric_prop(view, label, kb).into_iter().filter_map(|v| xa.get(&(v.0, v.1)).map(|&x| ((v.0, v.1), x, v.2))).collect();
    pairs.sort_by_key(|p| p.0);
    let (x, y): (Vec<f64>, Vec<f64>) = pairs.iter().map(|p| (p.1, p.2)).unzip();
    let row = if x.len() < 2 { vec![Value::Int(x.len() as i64), Value::Null, Value::Null] } else { vec![Value::Int(x.len() as i64), f(pearson(&x, &y)), f(pearson(&ranks(&x), &ranks(&y)))] };
    Ok((vec!["n", "pearson", "spearman"], vec![row]))
}

/// Buckets of `bucket` seconds: (bucket start -> (count, sum)).
fn bucketize(view: &View, label: Option<&str>, ts_key: &str, bucket: f64, val_key: Option<&str>) -> Result<BTreeMap<i64, (i64, f64)>> {
    if !(bucket > 0.0) {
        return bad("bucket must be positive");
    }
    let mut m: BTreeMap<i64, (i64, f64)> = BTreeMap::new();
    let vals: HashMap<(Id, bool), f64> = match val_key {
        Some(k) => numeric_prop(view, label, k).into_iter().map(|v| ((v.0, v.1), v.2)).collect(),
        None => HashMap::new(),
    };
    for (id, is_edge, ts) in time_prop(view, label, ts_key)? {
        let b = (ts / bucket).floor() as i64;
        let e = m.entry(b).or_default();
        e.0 += 1;
        e.1 += vals.get(&(id, is_edge)).copied().unwrap_or(0.0);
    }
    if let (Some(lo), Some(hi)) = (m.keys().next(), m.keys().next_back()) {
        if hi - lo > 5_000_000 {
            return bad("time range spans more than 5,000,000 buckets; use a larger bucket");
        }
    }
    Ok(m)
}

/// `stats.timeseries(label, tsKey, bucketSeconds[, valueKey])` -> bucket_start, count, sum, mean.
/// Empty buckets inside the range are reported with count 0.
fn timeseries(view: &View, a: &[Value]) -> Result<Rows> {
    let bucket = f64_arg(a, 2, 0.0)?;
    let m = bucketize(view, str_arg(a, 0), need_str(a, 1, "tsKey")?, bucket, str_arg(a, 3))?;
    let (Some(&lo), Some(&hi)) = (m.keys().next(), m.keys().next_back()) else { return Ok((vec!["bucket_start", "count", "sum", "mean"], vec![])) };
    let rows = (lo..=hi)
        .map(|b| {
            let (c, s) = m.get(&b).copied().unwrap_or((0, 0.0));
            vec![f(b as f64 * bucket), Value::Int(c), f(s), if c > 0 { f(s / c as f64) } else { Value::Null }]
        })
        .collect();
    Ok((vec!["bucket_start", "count", "sum", "mean"], rows))
}

/// `stats.spikes(label, tsKey, bucketSeconds[, threshold])` -> bucket_start, count, robust_z.
/// Time buckets whose event count is an upward outlier (robust z >= threshold, default 3.5).
fn spikes(view: &View, a: &[Value]) -> Result<Rows> {
    let bucket = f64_arg(a, 2, 0.0)?;
    let thr = f64_arg(a, 3, 3.5)?;
    let m = bucketize(view, str_arg(a, 0), need_str(a, 1, "tsKey")?, bucket, None)?;
    let (Some(&lo), Some(&hi)) = (m.keys().next(), m.keys().next_back()) else { return Ok((vec!["bucket_start", "count", "robust_z"], vec![])) };
    let counts: Vec<f64> = (lo..=hi).map(|b| m.get(&b).map_or(0.0, |x| x.0 as f64)).collect();
    let z = robust_z(&counts);
    let rows = (lo..=hi).zip(counts.iter().zip(&z)).filter(|(_, (_, z))| **z >= thr).map(|(b, (c, z))| vec![f(b as f64 * bucket), Value::Int(*c as i64), f(*z)]).collect();
    Ok((vec!["bucket_start", "count", "robust_z"], rows))
}
