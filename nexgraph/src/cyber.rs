//! Cyber-security analytics (v0.7).
//!
//! These procedures treat the database as a security graph: hosts, users,
//! services and alerts are nodes; connections, logins and trust relations are
//! edges (hyperedges work too, e.g. one hyperedge per multi-party session).
//! They are conventions-light: you name the edge label and property keys.
//!
//! | procedure | question it answers |
//! |---|---|
//! | `cyber.reachable(src[, maxHops[, label]])` | blast radius: what can an attacker reach from `src`? |
//! | `cyber.attack_paths(src, dst, k[, maxHops[, label]])` | the k cheapest simple paths (Yen); edge `weight` = attacker effort |
//! | `cyber.chokepoints(entryLabel, targetLabel[, label])` | which assets sit on the most entry-to-crown-jewel shortest paths? |
//! | `cyber.propagate_risk(seedKey[, label])` | likelihood that each asset is compromised from seeded ones; edge `weight` = success probability |
//! | `cyber.fanout(label[, minDistinct])` | scanners / password sprayers: sources with unusually many distinct targets |
//! | `cyber.beaconing(label, tsKey[, minEvents[, maxCv]])` | C2 beacons: pairs with near-periodic connection times |
//! | `cyber.volume(label, bytesKey[, threshold])` | exfiltration: sources with unusually large outbound volume |
//! | `cyber.ioc(label, key, [values][, hops])` | indicator-of-compromise matches and everything within `hops` of them |
//!
//! The Rust function [`drift`] compares two MVCC snapshots (`ts_old`, `ts_new`)
//! and reports new and removed nodes and links, with *novel links between
//! pre-existing nodes* separated out: the classic lateral-movement signal.

use crate::db::Database;
use crate::error::Result;
use crate::model::Body;
use crate::netgraph::*;
use crate::store::View;
use crate::value::{Id, Value};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet, VecDeque};

fn f(x: f64) -> Value {
    if x.is_finite() {
        Value::Float(x)
    } else {
        Value::Null
    }
}

pub(crate) fn call(view: &View, name: &str, a: &[Value]) -> Result<Rows> {
    match name {
        "reachable" => reachable(view, a),
        "attack_paths" => attack_paths(view, a),
        "chokepoints" => chokepoints(view, a),
        "propagate_risk" => propagate_risk(view, a),
        "fanout" => fanout(view, a),
        "beaconing" => beaconing(view, a),
        "volume" => volume(view, a),
        "ioc" => ioc(view, a),
        _ => bad(format!("unknown procedure 'cyber.{name}' (available: cyber.reachable, cyber.attack_paths, cyber.chokepoints, cyber.propagate_risk, cyber.fanout, cyber.beaconing, cyber.volume, cyber.ioc)")),
    }
}

/// `cyber.reachable(src[, maxHops[, edgeLabel]])` -> node, hops. Directed BFS (default 10 hops).
fn reachable(view: &View, a: &[Value]) -> Result<Rows> {
    let src = node_arg(a, 0, "src")?;
    let max = usize_arg(a, 1, 10, "maxHops")?;
    let net = Net::build(view, str_arg(a, 2));
    let (out, _) = net.simple_directed();
    let Some(s) = net.idx(src) else { return Ok((vec!["node", "hops"], vec![])) };
    let mut dist = vec![usize::MAX; net.n()];
    dist[s] = 0;
    let mut q = VecDeque::from([s]);
    let mut rows = vec![];
    while let Some(u) = q.pop_front() {
        rows.push(vec![Value::Node(net.ids[u]), Value::Int(dist[u] as i64)]);
        if dist[u] >= max {
            continue;
        }
        for &v in &out[u] {
            if dist[v as usize] == usize::MAX {
                dist[v as usize] = dist[u] + 1;
                q.push_back(v as usize);
            }
        }
    }
    Ok((vec!["node", "hops"], rows))
}

// ---- k cheapest simple paths (Yen) ----------------------------------------------------------

#[derive(PartialEq)]
struct St(f64, u32);
impl Eq for St {}
impl PartialOrd for St {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for St {
    fn cmp(&self, o: &Self) -> Ordering {
        o.0.total_cmp(&self.0).then(o.1.cmp(&self.1)) // min-heap
    }
}

struct Costs {
    out: Vec<Vec<(u32, f64)>>, // cheapest parallel edge only
    best: HashMap<(u32, u32), f64>,
}

fn costs(net: &Net) -> Result<Costs> {
    let mut best: HashMap<(u32, u32), f64> = HashMap::new();
    for u in 0..net.n() {
        for &(v, w, _) in &net.out[u] {
            if !(w > 0.0) || !w.is_finite() {
                return bad("cyber.attack_paths needs every edge weight to be positive and finite (weight = attacker effort)");
            }
            let e = best.entry((u as u32, v)).or_insert(f64::INFINITY);
            if w < *e {
                *e = w;
            }
        }
    }
    let mut out = vec![Vec::new(); net.n()];
    let mut keys: Vec<_> = best.iter().map(|(&k, &w)| (k, w)).collect();
    keys.sort_by_key(|x| x.0);
    for ((u, v), w) in keys {
        out[u as usize].push((v, w));
    }
    Ok(Costs { out, best })
}

fn dijkstra(c: &Costs, s: u32, t: u32, ban_nodes: &HashSet<u32>, ban_edges: &HashSet<(u32, u32)>) -> Option<(Vec<u32>, f64)> {
    let n = c.out.len();
    let (mut dist, mut prev) = (vec![f64::INFINITY; n], vec![u32::MAX; n]);
    dist[s as usize] = 0.0;
    let mut h = BinaryHeap::from([St(0.0, s)]);
    while let Some(St(d, u)) = h.pop() {
        if d > dist[u as usize] {
            continue;
        }
        if u == t {
            let mut p = vec![t];
            let mut x = t;
            while x != s {
                x = prev[x as usize];
                p.push(x);
            }
            p.reverse();
            return Some((p, d));
        }
        for &(v, w) in &c.out[u as usize] {
            if ban_nodes.contains(&v) || ban_edges.contains(&(u, v)) {
                continue;
            }
            let nd = d + w;
            if nd < dist[v as usize] {
                dist[v as usize] = nd;
                prev[v as usize] = u;
                h.push(St(nd, v));
            }
        }
    }
    None
}

/// `cyber.attack_paths(src, dst, k[, maxHops[, edgeLabel]])` -> rank, cost, hops, path.
/// The k cheapest *simple* paths by total edge weight (Yen's algorithm, exact). Paths with more than
/// `maxHops` edges are skipped (the search still ranks by cost, so a long cheap path can crowd out a short one).
fn attack_paths(view: &View, a: &[Value]) -> Result<Rows> {
    let (src, dst) = (node_arg(a, 0, "src")?, node_arg(a, 1, "dst")?);
    let k = usize_arg(a, 2, 3, "k")?.clamp(1, 1000);
    let max_hops = usize_arg(a, 3, usize::MAX, "maxHops")?;
    let net = Net::build(view, str_arg(a, 4));
    let cols = vec!["rank", "cost", "hops", "path"];
    let (Some(s), Some(t)) = (net.idx(src), net.idx(dst)) else { return Ok((cols, vec![])) };
    let c = costs(&net)?;
    let (s, t) = (s as u32, t as u32);
    let none_n = HashSet::new();
    let none_e = HashSet::new();
    let Some(first) = dijkstra(&c, s, t, &none_n, &none_e) else { return Ok((cols, vec![])) };
    let path_cost = |p: &[u32]| -> f64 { p.windows(2).map(|w| c.best[&(w[0], w[1])]).sum() };
    let mut accepted: Vec<(Vec<u32>, f64)> = vec![first];
    let mut cand: Vec<(Vec<u32>, f64)> = vec![];
    let mut seen: HashSet<Vec<u32>> = HashSet::new();
    seen.insert(accepted[0].0.clone());
    while accepted.len() < k {
        let prev = accepted.last().unwrap().0.clone();
        for i in 0..prev.len() - 1 {
            let root = &prev[..=i];
            let ban_e: HashSet<(u32, u32)> = accepted.iter().filter(|(p, _)| p.len() > i + 1 && &p[..=i] == root).map(|(p, _)| (p[i], p[i + 1])).collect();
            let ban_n: HashSet<u32> = root[..i].iter().copied().collect();
            if let Some((spur, _)) = dijkstra(&c, prev[i], t, &ban_n, &ban_e) {
                let mut total = root[..i].to_vec();
                total.extend(spur);
                if seen.insert(total.clone()) {
                    let cost = path_cost(&total);
                    cand.push((total, cost));
                }
            }
        }
        if cand.is_empty() {
            break;
        }
        let bi = (0..cand.len()).min_by(|&x, &y| cand[x].1.total_cmp(&cand[y].1).then_with(|| cand[x].0.cmp(&cand[y].0))).unwrap();
        accepted.push(cand.swap_remove(bi));
    }
    let rows = accepted
        .iter()
        .filter(|(p, _)| p.len() - 1 <= max_hops)
        .enumerate()
        .map(|(i, (p, cost))| vec![Value::Int(i as i64 + 1), f(*cost), Value::Int(p.len() as i64 - 1), Value::List(p.iter().map(|&x| Value::Node(net.ids[x as usize])).collect())])
        .collect();
    Ok((cols, rows))
}

/// `cyber.chokepoints(entryLabel, targetLabel[, edgeLabel])` -> node, score. Brandes accumulation
/// restricted to shortest paths from nodes labelled `entryLabel` to nodes labelled `targetLabel`:
/// the expected number of such (entry, target) pairs whose shortest paths cross the node (ties split evenly).
/// Entries and targets themselves are not reported. Fixing the top node cuts the most exposure.
fn chokepoints(view: &View, a: &[Value]) -> Result<Rows> {
    let (el, tl) = (need_str(a, 0, "entryLabel")?, need_str(a, 1, "targetLabel")?);
    let net = Net::build(view, str_arg(a, 2));
    let (out, inn) = net.simple_directed();
    let entries: Vec<usize> = view.nodes_with_label(el).iter().filter_map(|&i| net.idx(i)).collect();
    let mut is_t = vec![false; net.n()];
    let targets: Vec<usize> = view.nodes_with_label(tl).iter().filter_map(|&i| net.idx(i)).collect();
    targets.iter().for_each(|&t| is_t[t] = true);
    crate::analytics::work_guard(entries.len(), net.n(), out.iter().map(|l| l.len()).sum(), "cyber.chokepoints")?;
    let cb = crate::analytics::brandes(&out, &inn, &entries, Some(&is_t));
    let ends: HashSet<usize> = entries.iter().chain(targets.iter()).copied().collect();
    let mut rows: Vec<(f64, usize)> = (0..net.n()).filter(|u| !ends.contains(u) && cb[*u] > 1e-12).map(|u| (cb[u], u)).collect();
    rows.sort_by(|x, y| y.0.total_cmp(&x.0).then(x.1.cmp(&y.1)));
    Ok((vec!["node", "score"], rows.into_iter().map(|(s, u)| vec![Value::Node(net.ids[u]), f(s)]).collect()))
}

/// `cyber.propagate_risk(seedKey[, edgeLabel])` -> node, risk, seed. Seeds are nodes whose `seedKey`
/// property is a probability in (0, 1] (e.g. 1.0 for a confirmed-compromised host). Each edge's `weight`
/// (clamped to [0, 1]) is the probability an attacker crosses it. A node's risk is the maximum, over all
/// paths from any seed, of seed probability times the product of edge probabilities (the single most likely
/// attack chain; exact, via a max-product Dijkstra). `seed` names the seed that achieves it.
fn propagate_risk(view: &View, a: &[Value]) -> Result<Rows> {
    let key = need_str(a, 0, "seedKey")?;
    let net = Net::build(view, str_arg(a, 1));
    let n = net.n();
    let mut risk = vec![0.0f64; n];
    let mut origin = vec![u32::MAX; n];
    let mut heap: BinaryHeap<(OrdF, u32)> = BinaryHeap::new();
    for (i, &id) in net.ids.iter().enumerate() {
        if let Some(p) = view.node(id).and_then(|b| b.props().get(key)).and_then(|v| v.as_f64()) {
            if p > 0.0 && p <= 1.0 {
                risk[i] = p;
                origin[i] = i as u32;
                heap.push((OrdF(p), i as u32));
            } else if p.is_finite() && p != 0.0 {
                return bad(format!("seed property '{key}' must be a probability in (0, 1], got {p}"));
            }
        }
    }
    while let Some((OrdF(r), u)) = heap.pop() {
        if r < risk[u as usize] {
            continue;
        }
        for &(v, w, _) in &net.out[u as usize] {
            let nr = r * w.clamp(0.0, 1.0);
            if nr > risk[v as usize] {
                risk[v as usize] = nr;
                origin[v as usize] = origin[u as usize];
                heap.push((OrdF(nr), v));
            }
        }
    }
    let mut rows: Vec<(f64, usize)> = (0..n).filter(|&u| risk[u] > 0.0).map(|u| (risk[u], u)).collect();
    rows.sort_by(|x, y| y.0.total_cmp(&x.0).then(x.1.cmp(&y.1)));
    Ok((vec!["node", "risk", "seed"], rows.into_iter().map(|(r, u)| vec![Value::Node(net.ids[u]), f(r), Value::Node(net.ids[origin[u] as usize])]).collect()))
}

#[derive(PartialEq, Clone, Copy)]
struct OrdF(f64);
impl Eq for OrdF {}
impl PartialOrd for OrdF {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for OrdF {
    fn cmp(&self, o: &Self) -> Ordering {
        self.0.total_cmp(&o.0)
    }
}

/// Iterate (src, dst, edge body) over binary-ish edges of one label.
fn pairs<'a>(view: &View<'a>, label: &str) -> Vec<(Id, Id, &'a Body)> {
    let mut v = vec![];
    for e in view.edges_with_label(label) {
        if let Some(b @ Body::Edge { tail, head, .. }) = view.edge(e) {
            for t in tail {
                for h in head {
                    v.push((*t, *h, b));
                }
            }
        }
    }
    v
}

/// `cyber.fanout(edgeLabel[, minDistinct])` -> node, distinct_targets, events, robust_z.
/// Sources ranked by how unusual their number of distinct targets is (robust z on ln(1+n) across all
/// sources of that edge label). High values: port/host scans, password spraying, worm-like spread.
fn fanout(view: &View, a: &[Value]) -> Result<Rows> {
    let label = need_str(a, 0, "edgeLabel")?;
    let min = usize_arg(a, 1, 2, "minDistinct")?;
    let mut m: BTreeMap<Id, (BTreeSet<Id>, i64)> = BTreeMap::new();
    for (s, d, _) in pairs(view, label) {
        let e = m.entry(s).or_default();
        e.0.insert(d);
        e.1 += 1;
    }
    let keys: Vec<Id> = m.keys().copied().collect();
    let x: Vec<f64> = keys.iter().map(|k| (1.0 + m[k].0.len() as f64).ln()).collect();
    let z = robust_z(&x);
    let mut rows: Vec<(f64, Vec<Value>)> = keys
        .iter()
        .zip(&z)
        .filter(|(k, _)| m[*k].0.len() >= min)
        .map(|(k, z)| (*z, vec![Value::Node(*k), Value::Int(m[k].0.len() as i64), Value::Int(m[k].1), f(*z)]))
        .collect();
    rows.sort_by(|p, q| q.0.total_cmp(&p.0));
    Ok((vec!["node", "distinct_targets", "events", "robust_z"], rows.into_iter().map(|r| r.1).collect()))
}

/// `cyber.beaconing(edgeLabel, tsKey[, minEvents[, maxCv]])` -> src, dst, events, mean_interval, cv, score.
/// Groups connections per (src, dst); a pair with at least `minEvents` (default 8) timestamps (seconds,
/// numeric property `tsKey`) whose inter-arrival coefficient of variation is at most `maxCv` (default 0.3)
/// looks automated. `score = 1/(1+cv)`; 1.0 is perfectly periodic. Jittered beacons keep cv small;
/// human browsing is bursty (cv >= 1). A flagged pair is a lead, not proof: update checks and monitoring agents also beat.
fn beaconing(view: &View, a: &[Value]) -> Result<Rows> {
    let label = need_str(a, 0, "edgeLabel")?;
    let key = need_str(a, 1, "tsKey")?;
    let min_ev = usize_arg(a, 2, 8, "minEvents")?.max(3);
    let max_cv = f64_arg(a, 3, 0.3)?;
    let mut g: BTreeMap<(Id, Id), Vec<f64>> = BTreeMap::new();
    let (mut present, mut sample) = (0usize, None);
    for (s, d, b) in pairs(view, label) {
        if let Some(raw) = b.props().get(key) {
            present += 1;
            match crate::timeutil::epoch_seconds(raw) {
                Some(t) => g.entry((s, d)).or_default().push(t),
                None if sample.is_none() => sample = Some(format!("{raw:?}")),
                None => {}
            }
        }
    }
    if present > 0 && g.is_empty() {
        return Err(crate::error::Error::Query(format!("property '{key}' is present on {present} edges but none is a number of seconds or an ISO 8601 timestamp (first value: {})", sample.unwrap_or_default())));
    }
    let mut rows: Vec<(f64, usize, Vec<Value>)> = vec![];
    for ((s, d), mut ts) in g {
        if ts.len() < min_ev {
            continue;
        }
        ts.sort_by(|x, y| x.total_cmp(y));
        let iv: Vec<f64> = ts.windows(2).map(|w| w[1] - w[0]).collect();
        let m = mean(&iv);
        if m <= 0.0 {
            continue;
        }
        let cv = stdev(&iv) / m;
        if cv <= max_cv {
            let score = 1.0 / (1.0 + cv);
            rows.push((score, ts.len(), vec![Value::Node(s), Value::Node(d), Value::Int(ts.len() as i64), f(m), f(cv), f(score)]));
        }
    }
    rows.sort_by(|p, q| q.0.total_cmp(&p.0).then(q.1.cmp(&p.1)));
    Ok((vec!["src", "dst", "events", "mean_interval", "cv", "score"], rows.into_iter().map(|r| r.2).collect()))
}

/// `cyber.volume(edgeLabel, bytesKey[, threshold])` -> node, total, events, robust_z.
/// Outbound byte totals per source (sum of numeric property `bytesKey`), flagging sources whose
/// ln(1+total) has robust z >= `threshold` (default 3.5): bulk exfiltration candidates.
fn volume(view: &View, a: &[Value]) -> Result<Rows> {
    let label = need_str(a, 0, "edgeLabel")?;
    let key = need_str(a, 1, "bytesKey")?;
    let thr = f64_arg(a, 2, 3.5)?;
    let mut m: BTreeMap<Id, (f64, i64)> = BTreeMap::new();
    for e in view.edges_with_label(label) {
        if let Some(Body::Edge { tail, props, .. }) = view.edge(e) {
            if let Some(b) = props.get(key).and_then(|v| v.as_f64()).filter(|b| b.is_finite() && *b >= 0.0) {
                for t in tail {
                    let x = m.entry(*t).or_default();
                    x.0 += b;
                    x.1 += 1;
                }
            }
        }
    }
    let keys: Vec<Id> = m.keys().copied().collect();
    let z = robust_z(&keys.iter().map(|k| (1.0 + m[k].0).ln()).collect::<Vec<_>>());
    let mut rows: Vec<(f64, Vec<Value>)> = keys.iter().zip(&z).filter(|(_, z)| **z >= thr).map(|(k, z)| (*z, vec![Value::Node(*k), f(m[k].0), Value::Int(m[k].1), f(*z)])).collect();
    rows.sort_by(|p, q| q.0.total_cmp(&p.0));
    Ok((vec!["node", "total", "events", "robust_z"], rows.into_iter().map(|r| r.1).collect()))
}

/// `cyber.ioc(label, key, values[, hops])` -> node, hops, ioc, via. Finds nodes (of `label`, or any node
/// if null) whose `key` property equals one of `values` (strings or numbers; e.g. IPs, domains, hashes),
/// then walks `hops` (default 1) undirected steps from them. `via` is the matched node nearest to `node`
/// and `ioc` its matching value, so every row says which indicator put the node in scope.
fn ioc(view: &View, a: &[Value]) -> Result<Rows> {
    let label = str_arg(a, 0);
    let key = need_str(a, 1, "key")?;
    let Some(Value::List(vals)) = a.get(2) else { return bad("values must be a list of indicator values") };
    let hops = usize_arg(a, 3, 1, "hops")?;
    let net = Net::build(view, None);
    let adj = net.simple_undirected();
    let pool: Vec<Id> = match label {
        Some(l) => view.nodes_with_label(l),
        None => view.node_ids().collect(),
    };
    let mut origin: Vec<Option<(usize, usize, Value)>> = vec![None; net.n()]; // (hops, via, ioc)
    let mut q = VecDeque::new();
    let mut seeds: Vec<(usize, Value)> = vec![];
    for id in pool {
        if let (Some(i), Some(b)) = (net.idx(id), view.node(id)) {
            if let Some(pv) = b.props().get(key) {
                if let Some(hit) = vals.iter().find(|v| v.loose_eq(pv)) {
                    seeds.push((i, hit.clone()));
                }
            }
        }
    }
    seeds.sort_by_key(|s| s.0);
    for (i, hit) in seeds {
        origin[i] = Some((0, i, hit));
        q.push_back(i);
    }
    while let Some(u) = q.pop_front() {
        let (d, via, hit) = origin[u].clone().unwrap();
        if d >= hops {
            continue;
        }
        for &v in &adj[u] {
            if origin[v as usize].is_none() {
                origin[v as usize] = Some((d + 1, via, hit.clone()));
                q.push_back(v as usize);
            }
        }
    }
    let mut rows: Vec<(usize, usize, Vec<Value>)> = origin
        .into_iter()
        .enumerate()
        .filter_map(|(i, o)| o.map(|(d, via, hit)| (d, i, vec![Value::Node(net.ids[i]), Value::Int(d as i64), hit, Value::Node(net.ids[via])])))
        .collect();
    rows.sort_by_key(|r| (r.0, r.1));
    Ok((vec!["node", "hops", "ioc", "via"], rows.into_iter().map(|r| r.2).collect()))
}

// ------------------------------------------------------------------ drift

/// Differences between two snapshots of the database.
#[derive(Debug, Clone, Default)]
pub struct Drift {
    pub ts_old: u64,
    pub ts_new: u64,
    pub new_nodes: Vec<Id>,
    pub removed_nodes: Vec<Id>,
    /// Links `(label, tail, head)` present now but not before, between nodes that both already existed.
    pub novel_links: Vec<(String, Vec<Id>, Vec<Id>)>,
    /// Links involving at least one node created since `ts_old`.
    pub links_to_new_nodes: usize,
    pub removed_links: usize,
    /// Jaccard similarity of the two link sets (1.0 = unchanged).
    pub link_jaccard: f64,
}

type LinkKey = (String, Vec<Id>, Vec<Id>);

fn links(view: &View) -> BTreeSet<LinkKey> {
    view.edge_ids()
        .filter_map(|e| match view.edge(e) {
            Some(Body::Edge { label, tail, head, .. }) => {
                let (mut t, mut h) = (tail.clone(), head.clone());
                t.sort_unstable();
                h.sort_unstable();
                Some((label.clone(), t, h))
            }
            _ => None,
        })
        .collect()
}

/// Compare the database at `ts_old` and `ts_new` (both must be within retained history, i.e. no
/// checkpoint since). Parallel duplicate links count once. Use a stable-baseline `ts_old` (for
/// example the commit at the end of last week) and alert on `novel_links` that cross trust zones.
pub fn drift(db: &Database, ts_old: u64, ts_new: u64) -> Result<Drift> {
    let (nodes_a, links_a) = db.read_at(ts_old, |v| (v.node_ids().collect::<BTreeSet<Id>>(), links(v)))?;
    let (nodes_b, links_b) = db.read_at(ts_new, |v| (v.node_ids().collect::<BTreeSet<Id>>(), links(v)))?;
    let new_nodes: Vec<Id> = nodes_b.difference(&nodes_a).copied().collect();
    let removed_nodes: Vec<Id> = nodes_a.difference(&nodes_b).copied().collect();
    let mut novel = vec![];
    let mut to_new = 0;
    for l in links_b.difference(&links_a) {
        let members = l.1.iter().chain(l.2.iter());
        if members.clone().any(|m| !nodes_a.contains(m)) {
            to_new += 1;
        } else {
            novel.push(l.clone());
        }
    }
    let inter = links_a.intersection(&links_b).count();
    let uni = links_a.union(&links_b).count();
    Ok(Drift {
        ts_old,
        ts_new,
        new_nodes,
        removed_nodes,
        novel_links: novel,
        links_to_new_nodes: to_new,
        removed_links: links_a.difference(&links_b).count(),
        link_jaccard: if uni == 0 { 1.0 } else { inter as f64 / uni as f64 },
    })
}
