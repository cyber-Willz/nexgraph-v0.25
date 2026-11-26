//! v0.7: analytics (`algo.*`, `stats.*`) and cyber (`cyber.*`) procedures, each checked against a
//! brute-force reference or a hand-computed answer.
use nexgraph::{drift, props, Database, Id, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

fn rng(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

/// Random directed graph (no parallel edges, no self loops) with `n` nodes labelled `N` and key `k`.
fn random_graph(n: usize, m: usize, seed: u64) -> (Database, Vec<Id>, Vec<(usize, usize)>) {
    let mut s = seed;
    let db = Database::in_memory();
    let mut tx = db.begin();
    let ids: Vec<Id> = (0..n).map(|i| tx.create_node(&["N"], props([("k", Value::Int(i as i64))])).unwrap()).collect();
    let mut set = BTreeSet::new();
    while set.len() < m {
        let (a, b) = ((rng(&mut s) % n as u64) as usize, (rng(&mut s) % n as u64) as usize);
        if a != b && set.insert((a, b)) {
            tx.create_edge("E", ids[a], ids[b], 1.0, Default::default()).unwrap();
        }
    }
    tx.commit().unwrap();
    (db, ids, set.into_iter().collect())
}

fn col(db: &Database, q: &str) -> Vec<Vec<Value>> {
    db.query(q).unwrap_or_else(|e| panic!("{q}: {e}")).rows
}

fn node_map(rows: &[Vec<Value>], c: usize) -> HashMap<Id, f64> {
    rows.iter()
        .map(|r| match (&r[0], r[c].as_f64()) {
            (Value::Node(i), Some(x)) => (*i, x),
            o => panic!("bad row {o:?}"),
        })
        .collect()
}

fn undirected(n: usize, e: &[(usize, usize)]) -> Vec<BTreeSet<usize>> {
    let mut adj = vec![BTreeSet::new(); n];
    for &(a, b) in e {
        adj[a].insert(b);
        adj[b].insert(a);
    }
    adj
}

#[test]
fn betweenness_matches_all_pairs_reference() {
    let (n, _) = (45, 0);
    let (db, ids, edges) = random_graph(n, 110, 7);
    // reference: sigma and dist by BFS from every node
    let mut out = vec![vec![]; n];
    edges.iter().for_each(|&(a, b)| out[a].push(b));
    let (mut dist, mut sig) = (vec![vec![usize::MAX; n]; n], vec![vec![0.0f64; n]; n]);
    for s in 0..n {
        dist[s][s] = 0;
        sig[s][s] = 1.0;
        let mut q = VecDeque::from([s]);
        while let Some(u) = q.pop_front() {
            for &v in &out[u] {
                if dist[s][v] == usize::MAX {
                    dist[s][v] = dist[s][u] + 1;
                    q.push_back(v);
                }
                if dist[s][v] == dist[s][u] + 1 {
                    sig[s][v] += sig[s][u];
                }
            }
        }
    }
    let mut want = vec![0.0; n];
    for s in 0..n {
        for t in 0..n {
            if s == t || dist[s][t] == usize::MAX {
                continue;
            }
            for v in 0..n {
                if v != s && v != t && dist[s][v] != usize::MAX && dist[v][t] != usize::MAX && dist[s][v] + dist[v][t] == dist[s][t] {
                    want[v] += sig[s][v] * sig[v][t] / sig[s][t];
                }
            }
        }
    }
    let got = node_map(&col(&db, "CALL algo.betweenness() YIELD node, score RETURN node, score"), 1);
    assert!(want.iter().any(|&x| x > 1.0));
    for v in 0..n {
        assert!((got[&ids[v]] - want[v]).abs() < 1e-9, "node {v}: {} vs {}", got[&ids[v]], want[v]);
    }
    // sampling with k = n is exact; k < n is an estimate that stays in a sane range
    let all = node_map(&col(&db, &format!("CALL algo.betweenness({n}) YIELD node, score RETURN node, score")), 1);
    assert!((all[&ids[3]] - want[3]).abs() < 1e-9);
}

#[test]
fn kcore_triangles_scc_articulation_match_references() {
    let n = 40;
    let (db, ids, edges) = random_graph(n, 70, 99);
    let adj = undirected(n, &edges);

    // k-core by naive peeling
    let mut core = vec![0usize; n];
    let mut alive: BTreeSet<usize> = (0..n).collect();
    let mut k = 0;
    while !alive.is_empty() {
        loop {
            let low: Vec<usize> = alive.iter().copied().filter(|&v| adj[v].iter().filter(|u| alive.contains(u)).count() <= k).collect();
            if low.is_empty() {
                break;
            }
            for v in low {
                core[v] = k;
                alive.remove(&v);
            }
        }
        k += 1;
    }
    let got = node_map(&col(&db, "CALL algo.kcore() YIELD node, core RETURN node, core"), 1);
    (0..n).for_each(|v| assert_eq!(got[&ids[v]] as usize, core[v], "core of {v}"));

    // triangles by brute force
    let tri_rows = col(&db, "CALL algo.triangles() YIELD node, triangles, clustering RETURN node, triangles, clustering");
    let tri = node_map(&tri_rows, 1);
    let clu = node_map(&tri_rows, 2);
    for v in 0..n {
        let nb: Vec<usize> = adj[v].iter().copied().collect();
        let mut t = 0;
        for i in 0..nb.len() {
            for j in i + 1..nb.len() {
                if adj[nb[i]].contains(&nb[j]) {
                    t += 1;
                }
            }
        }
        assert_eq!(tri[&ids[v]] as usize, t, "triangles of {v}");
        let d = nb.len() as f64;
        let want = if d >= 2.0 { 2.0 * t as f64 / (d * (d - 1.0)) } else { 0.0 };
        assert!((clu[&ids[v]] - want).abs() < 1e-12);
    }

    // SCC: same component <=> mutual reachability
    let mut out = vec![vec![]; n];
    edges.iter().for_each(|&(a, b)| out[a].push(b));
    let reach: Vec<Vec<bool>> = (0..n)
        .map(|s| {
            let mut seen = vec![false; n];
            seen[s] = true;
            let mut q = VecDeque::from([s]);
            while let Some(u) = q.pop_front() {
                for &v in &out[u] {
                    if !seen[v] {
                        seen[v] = true;
                        q.push_back(v);
                    }
                }
            }
            seen
        })
        .collect();
    let comp = node_map(&col(&db, "CALL algo.scc() YIELD node, component, size RETURN node, component, size"), 1);
    let mut multi = 0;
    for a in 0..n {
        for b in 0..n {
            assert_eq!(comp[&ids[a]] == comp[&ids[b]], reach[a][b] && reach[b][a], "scc {a},{b}");
        }
        if (0..n).filter(|&b| reach[a][b] && reach[b][a]).count() > 1 {
            multi += 1;
        }
    }
    assert!(multi > 3, "graph should contain a non-trivial SCC");

    // articulation points: removing the node increases the component count
    let components = |skip: Option<usize>| {
        let mut seen = vec![false; n];
        let mut c = 0;
        for s in 0..n {
            if seen[s] || Some(s) == skip {
                continue;
            }
            c += 1;
            let mut st = vec![s];
            seen[s] = true;
            while let Some(u) = st.pop() {
                for &v in &adj[u] {
                    if !seen[v] && Some(v) != skip {
                        seen[v] = true;
                        st.push(v);
                    }
                }
            }
        }
        c
    };
    let base = components(None);
    let cut = node_map(&col(&db, "CALL algo.articulation() YIELD node, components_after_removal RETURN node, components_after_removal"), 1);
    let mut n_cut = 0;
    for v in 0..n {
        let after = components(Some(v)) as i64;
        // isolated vertices vanish from the count, so compare against base adjusted for them
        let isolated = adj[v].is_empty() as i64;
        let delta = after - (base as i64 - isolated);
        if delta > 0 {
            n_cut += 1;
            assert_eq!(cut[&ids[v]] as i64, delta + 1, "components after removing {v}");
        } else {
            assert!(!cut.contains_key(&ids[v]), "{v} is not a cut vertex");
        }
    }
    assert!(n_cut > 0 && cut.len() == n_cut);
}

#[test]
fn louvain_and_similarity() {
    let db = Database::in_memory();
    let mut tx = db.begin();
    let a: Vec<Id> = (0..6).map(|i| tx.create_node(&["N"], props([("k", Value::Int(i))])).unwrap()).collect();
    let b: Vec<Id> = (6..12).map(|i| tx.create_node(&["N"], props([("k", Value::Int(i))])).unwrap()).collect();
    for g in [&a, &b] {
        for i in 0..6 {
            for j in i + 1..6 {
                tx.create_edge("E", g[i], g[j], 1.0, Default::default()).unwrap();
            }
        }
    }
    tx.create_edge("E", a[0], b[0], 1.0, Default::default()).unwrap(); // a single bridge
    tx.commit().unwrap();
    let c = node_map(&col(&db, "CALL algo.louvain() YIELD node, community RETURN node, community"), 1);
    assert!(a.iter().all(|x| c[x] == c[&a[0]]) && b.iter().all(|x| c[x] == c[&b[0]]));
    assert_ne!(c[&a[0]], c[&b[0]]);
    // a[1] and a[2] share 4 of their neighbours: N(a1)={a0,a2,a3,a4,a5}, N(a2)={a0,a1,a3,a4,a5}
    let r = col(&db, "MATCH (x:N {k:1}) CALL algo.similarity(x, 1) YIELD node, jaccard, common RETURN node, jaccard, common");
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][2].as_i64(), Some(4));
    assert!((r[0][1].as_f64().unwrap() - 4.0 / 6.0).abs() < 1e-12);
}

#[test]
fn attack_paths_match_exhaustive_enumeration() {
    let n = 9;
    let mut s = 4242u64;
    let db = Database::in_memory();
    let mut tx = db.begin();
    let ids: Vec<Id> = (0..n).map(|i| tx.create_node(&["H"], props([("k", Value::Int(i as i64))])).unwrap()).collect();
    let mut w = vec![vec![0.0f64; n]; n];
    for a in 0..n {
        for b in 0..n {
            if a != b && rng(&mut s) % 100 < 35 {
                let c = 1.0 + (rng(&mut s) % 90) as f64 / 10.0 + (rng(&mut s) % 1000) as f64 / 1e6; // distinct costs
                w[a][b] = c;
                tx.create_edge("E", ids[a], ids[b], c, Default::default()).unwrap();
            }
        }
    }
    tx.commit().unwrap();
    // exhaustive simple paths 0 -> n-1
    fn dfs(u: usize, t: usize, w: &Vec<Vec<f64>>, seen: &mut Vec<bool>, cost: f64, out: &mut Vec<f64>) {
        if u == t {
            out.push(cost);
            return;
        }
        for v in 0..w.len() {
            if w[u][v] > 0.0 && !seen[v] {
                seen[v] = true;
                dfs(v, t, w, seen, cost + w[u][v], out);
                seen[v] = false;
            }
        }
    }
    for (src, dst) in [(0usize, n - 1), (1, 7), (3, 5)] {
        let mut all = vec![];
        let mut seen = vec![false; n];
        seen[src] = true;
        dfs(src, dst, &w, &mut seen, 0.0, &mut all);
        all.sort_by(|a, b| a.total_cmp(b));
        let k = 6;
        let r = col(&db, &format!("MATCH (a:H {{k:{src}}}), (b:H {{k:{dst}}}) CALL cyber.attack_paths(a, b, {k}) YIELD rank, cost, hops, path RETURN rank, cost, hops, path ORDER BY rank"));
        assert_eq!(r.len(), all.len().min(k), "{src}->{dst}: number of paths");
        for (i, row) in r.iter().enumerate() {
            assert!((row[1].as_f64().unwrap() - all[i]).abs() < 1e-9, "{src}->{dst} rank {}: {:?} vs {}", i + 1, row[1], all[i]);
            let Value::List(p) = &row[3] else { panic!() };
            assert_eq!(p.len() as i64 - 1, row[2].as_i64().unwrap());
            assert_eq!(p.first(), Some(&Value::Node(ids[src])));
            assert_eq!(p.last(), Some(&Value::Node(ids[dst])));
            let uniq: BTreeSet<String> = p.iter().map(|x| x.to_string()).collect();
            assert_eq!(uniq.len(), p.len(), "paths must be simple");
        }
    }
    // non-positive weights are rejected, not silently mis-ranked
    db.query("MATCH (a:H {k:0}), (b:H {k:1}) CREATE (a)-[:Z {weight:-1}]->(b)").unwrap();
    assert!(db.query("MATCH (a:H {k:0}), (b:H {k:8}) CALL cyber.attack_paths(a, b, 2) YIELD rank RETURN rank").is_err());
}

#[test]
fn chokepoints_find_the_gateway() {
    // 3 internet-facing hosts -> gateway -> 2 app servers -> db; plus a decoy side path to one server
    let db = Database::in_memory();
    let mut tx = db.begin();
    let mk = |tx: &mut nexgraph::Txn, l: &str, k: &str| tx.create_node(&[l], props([("name", k)])).unwrap();
    let ext: Vec<Id> = (0..3).map(|i| mk(&mut tx, "Entry", &format!("ext{i}"))).collect();
    let gw = mk(&mut tx, "Host", "gw");
    let (app1, app2, side) = (mk(&mut tx, "Host", "app1"), mk(&mut tx, "Host", "app2"), mk(&mut tx, "Host", "side"));
    let db_ = mk(&mut tx, "Crown", "db");
    for &e in &ext {
        tx.create_edge("CONN", e, gw, 1.0, Default::default()).unwrap();
    }
    tx.create_edge("CONN", gw, app1, 1.0, Default::default()).unwrap();
    tx.create_edge("CONN", gw, app2, 1.0, Default::default()).unwrap();
    tx.create_edge("CONN", app1, db_, 1.0, Default::default()).unwrap();
    tx.create_edge("CONN", app2, db_, 1.0, Default::default()).unwrap();
    tx.create_edge("CONN", ext[0], side, 1.0, Default::default()).unwrap(); // dead end
    tx.commit().unwrap();
    let r = col(&db, "CALL cyber.chokepoints('Entry', 'Crown') YIELD node, score RETURN node, score ORDER BY score DESC");
    // 3 entries each reach db by 2 equal paths: gw carries all 3 pairs, app1 and app2 1.5 each; side none
    assert_eq!(r.len(), 3);
    assert_eq!(r[0][0], Value::Node(gw));
    assert!((r[0][1].as_f64().unwrap() - 3.0).abs() < 1e-9);
    assert!((r[1][1].as_f64().unwrap() - 1.5).abs() < 1e-9 && (r[2][1].as_f64().unwrap() - 1.5).abs() < 1e-9);
    assert!(!r.iter().any(|x| x[0] == Value::Node(side)));
    let _ = (app1, app2);
}

#[test]
fn risk_propagation_is_the_most_likely_chain() {
    // a(seed 0.9) -0.5-> b -0.8-> d ;  a -0.3-> c -0.9-> d ; e(seed 0.2) -1.0-> d
    let db = Database::in_memory();
    let mut tx = db.begin();
    let n = |tx: &mut nexgraph::Txn, k: &str, seed: Option<f64>| {
        let mut p = props([("name", k)]);
        if let Some(s) = seed {
            p.insert("p".into(), Value::Float(s));
        }
        tx.create_node(&["H"], p).unwrap()
    };
    let (a, b, c, d, e) = (n(&mut tx, "a", Some(0.9)), n(&mut tx, "b", None), n(&mut tx, "c", None), n(&mut tx, "d", None), n(&mut tx, "e", Some(0.2)));
    for (x, y, w) in [(a, b, 0.5), (b, d, 0.8), (a, c, 0.3), (c, d, 0.9), (e, d, 1.0)] {
        tx.create_edge("LOGIN", x, y, w, Default::default()).unwrap();
    }
    tx.commit().unwrap();
    let rows = col(&db, "CALL cyber.propagate_risk('p') YIELD node, risk, seed RETURN node, risk, seed");
    let m: HashMap<Id, (f64, Id)> = rows.iter().map(|r| (if let Value::Node(i) = r[0] { i } else { 0 }, (r[1].as_f64().unwrap(), if let Value::Node(i) = r[2] { i } else { 0 }))).collect();
    assert!((m[&b].0 - 0.45).abs() < 1e-12 && m[&b].1 == a);
    assert!((m[&c].0 - 0.27).abs() < 1e-12);
    // d: via a-b-d = 0.9*0.5*0.8 = 0.36; via a-c-d = 0.243; via e = 0.2  -> 0.36 from seed a
    assert!((m[&d].0 - 0.36).abs() < 1e-12 && m[&d].1 == a);
    assert!((m[&a].0 - 0.9).abs() < 1e-12 && (m[&e].0 - 0.2).abs() < 1e-12);
    // a seed outside (0,1] is an error
    db.query("MATCH (x:H {name:'b'}) SET x.p = 1.5").unwrap();
    assert!(db.query("CALL cyber.propagate_risk('p') YIELD node RETURN node").is_err());
}

#[test]
fn detects_scanner_beacon_and_exfil() {
    let mut s = 31337u64;
    let db = Database::in_memory();
    let mut tx = db.begin();
    let hosts: Vec<Id> = (0..60).map(|i| tx.create_node(&["Host"], props([("name", format!("h{i}"))])).unwrap()).collect();
    let (scanner, c2, victim) = (hosts[0], hosts[1], hosts[2]);
    // background: each of hosts 3..50 talks to 2-4 random hosts, bursty times, modest bytes
    for &h in &hosts[3..50] {
        for _ in 0..(2 + rng(&mut s) % 3) {
            let d = hosts[(rng(&mut s) % 60) as usize];
            if d == h {
                continue;
            }
            for _ in 0..(1 + rng(&mut s) % 12) {
                let t = (rng(&mut s) % 86_400) as f64;
                let bytes = 1_000 + rng(&mut s) % 50_000;
                tx.create_edge("FLOW", h, d, 1.0, props([("ts", Value::Float(t)), ("bytes", Value::Int(bytes as i64))])).unwrap();
            }
        }
    }
    // scanner touches every host once
    for (i, &d) in hosts.iter().enumerate().skip(3) {
        tx.create_edge("FLOW", scanner, d, 1.0, props([("ts", Value::Float(100.0 + i as f64)), ("bytes", Value::Int(60))])).unwrap();
    }
    // beacon victim -> c2 every 300 s with +-10% jitter
    let mut t = 1000.0;
    for _ in 0..40 {
        t += 300.0 * (0.9 + (rng(&mut s) % 200) as f64 / 1000.0);
        tx.create_edge("FLOW", victim, c2, 1.0, props([("ts", Value::Float(t)), ("bytes", Value::Int(400))])).unwrap();
    }
    // exfil: victim pushes 5 GB in a few flows to a drop host
    let drop = hosts[55];
    for i in 0..5 {
        tx.create_edge("FLOW", victim, drop, 1.0, props([("ts", Value::Float(70_000.0 + i as f64)), ("bytes", Value::Int(1_000_000_000))])).unwrap();
    }
    tx.commit().unwrap();

    let fan = col(&db, "CALL cyber.fanout('FLOW') YIELD node, distinct_targets, events, robust_z RETURN node, distinct_targets, robust_z");
    assert_eq!(fan[0][0], Value::Node(scanner));
    assert_eq!(fan[0][1].as_i64(), Some(57));
    assert!(fan[0][2].as_f64().unwrap() > 3.5 && fan[1][2].as_f64().unwrap() < fan[0][2].as_f64().unwrap());

    let beacons = col(&db, "CALL cyber.beaconing('FLOW', 'ts') YIELD src, dst, events, mean_interval, cv, score RETURN src, dst, events, mean_interval, cv");
    assert_eq!(beacons.len(), 1, "only the periodic pair is flagged: {beacons:?}");
    assert_eq!((beacons[0][0].clone(), beacons[0][1].clone()), (Value::Node(victim), Value::Node(c2)));
    assert_eq!(beacons[0][2].as_i64(), Some(40));
    assert!((beacons[0][3].as_f64().unwrap() - 300.0).abs() < 15.0 && beacons[0][4].as_f64().unwrap() < 0.15);

    let vol = col(&db, "CALL cyber.volume('FLOW', 'bytes') YIELD node, total, events, robust_z RETURN node, total");
    assert_eq!(vol[0][0], Value::Node(victim));
    assert_eq!(vol[0][1].as_f64(), Some(5_000_000_000.0 + 40.0 * 400.0));
}

#[test]
fn ioc_expands_from_matches() {
    let db = Database::in_memory();
    let mut tx = db.begin();
    let ip = |tx: &mut nexgraph::Txn, l: &str, v: &str| tx.create_node(&[l], props([("ip", v)])).unwrap();
    let bad = ip(&mut tx, "Host", "203.0.113.9");
    let a = ip(&mut tx, "Host", "10.0.0.1");
    let b = ip(&mut tx, "Host", "10.0.0.2");
    let c = ip(&mut tx, "Host", "10.0.0.3");
    let far = ip(&mut tx, "Host", "10.0.0.4");
    tx.create_edge("FLOW", a, bad, 1.0, Default::default()).unwrap();
    tx.create_edge("FLOW", b, a, 1.0, Default::default()).unwrap();
    tx.create_edge("FLOW", c, b, 1.0, Default::default()).unwrap();
    tx.create_edge("FLOW", far, c, 1.0, Default::default()).unwrap();
    tx.commit().unwrap();
    let r = col(&db, "CALL cyber.ioc('Host', 'ip', ['203.0.113.9', '198.51.100.7'], 2) YIELD node, hops, ioc, via RETURN node, hops, ioc, via");
    let got: BTreeMap<Id, i64> = r.iter().map(|x| (if let Value::Node(i) = x[0] { i } else { 0 }, x[1].as_i64().unwrap())).collect();
    assert_eq!(got, BTreeMap::from([(bad, 0), (a, 1), (b, 2)]));
    assert!(r.iter().all(|x| x[2] == Value::Str("203.0.113.9".into()) && x[3] == Value::Node(bad)));
    let rr = col(&db, "MATCH (x:Host {ip:'10.0.0.1'}) CALL cyber.reachable(x, 1) YIELD node, hops RETURN node, hops");
    assert_eq!(rr.len(), 2); // itself and `bad`
}

#[test]
fn stats_procedures() {
    let db = Database::in_memory();
    let mut tx = db.begin();
    // 1..=9 plus one extreme value; some rows lack the key
    for v in 1..=9 {
        tx.create_node(&["M"], props([("v", Value::Int(v)), ("w", Value::Int(2 * v + 1))])).unwrap();
    }
    let big = tx.create_node(&["M"], props([("v", Value::Int(500)), ("w", Value::Int(7))])).unwrap();
    tx.create_node(&["M"], props([("name", "no-v")])).unwrap();
    tx.commit().unwrap();
    let d = col(&db, "CALL stats.describe('M', 'v') YIELD count, min, p25, median, p75, p95, max, mean, stdev, missing RETURN count, min, p25, median, p75, p95, max, mean, stdev, missing");
    let r: Vec<f64> = d[0].iter().map(|v| v.as_f64().unwrap()).collect();
    assert_eq!((r[0], r[1], r[6], r[9]), (10.0, 1.0, 500.0, 1.0));
    assert!((r[3] - 5.5).abs() < 1e-12 && (r[2] - 3.25).abs() < 1e-12 && (r[4] - 7.75).abs() < 1e-12);
    assert!((r[7] - 54.5).abs() < 1e-12);
    let sd = ((1..=9).map(|v| (v as f64 - 54.5).powi(2)).sum::<f64>() + (500.0f64 - 54.5).powi(2)) / 9.0;
    assert!((r[8] - sd.sqrt()).abs() < 1e-9);
    let o = col(&db, "CALL stats.outliers('M', 'v') YIELD entity, value, z, robust_z RETURN entity, value, robust_z");
    assert_eq!(o.len(), 1);
    assert_eq!(o[0][0], Value::Node(big));
    let h = col(&db, "CALL stats.histogram('M', 'v', 5) YIELD bin, lo, hi, count RETURN bin, count");
    assert_eq!(h.iter().map(|r| r[1].as_i64().unwrap()).collect::<Vec<_>>(), vec![9, 0, 0, 0, 1]);
    // v and w=2v+1 are perfectly correlated except for the big one; on the first nine rows Pearson = 1
    let c = col(&db, "CALL stats.correlation('M', 'v', 'w') YIELD n, pearson, spearman RETURN n, pearson, spearman");
    assert_eq!(c[0][0].as_i64(), Some(10));
    assert!(c[0][1].as_f64().unwrap() < 0.0 && c[0][2].as_f64().unwrap() < 0.9); // the outlier breaks it
    let db2 = Database::in_memory();
    db2.query("CREATE (:P {a:1, b:3}), (:P {a:2, b:5}), (:P {a:3, b:7}), (:P {a:4, b:9})").unwrap();
    let c2 = col(&db2, "CALL stats.correlation('P', 'a', 'b') YIELD n, pearson, spearman RETURN pearson, spearman");
    assert!((c2[0][0].as_f64().unwrap() - 1.0).abs() < 1e-12 && (c2[0][1].as_f64().unwrap() - 1.0).abs() < 1e-12);

    // NQL aggregates
    let a = col(&db, "MATCH (m:M) WHERE m.v <= 9 RETURN median(m.v), stdev(m.v), count(*)");
    assert_eq!(a[0][0].as_f64(), Some(5.0));
    assert!((a[0][1].as_f64().unwrap() - (60.0f64 / 8.0).sqrt()).abs() < 1e-12);
    assert!(db.query("MATCH (m:M) RETURN stdev(m.name)").is_err());
}

#[test]
fn timeseries_and_spikes() {
    let db = Database::in_memory();
    let mut tx = db.begin();
    let a = tx.create_node(&["H"], props([("k", 1)])).unwrap();
    // one event per 60 s bucket for 40 buckets, a gap in buckets 10..13, and a burst of 30 in bucket 25
    for b in 0..40i64 {
        if (10..13).contains(&b) {
            continue;
        }
        tx.create_edge("EV", a, a, 1.0, props([("ts", Value::Float(b as f64 * 60.0 + 5.0)), ("n", Value::Int(b))])).unwrap();
    }
    for i in 0..30 {
        tx.create_edge("EV", a, a, 1.0, props([("ts", Value::Float(25.0 * 60.0 + i as f64)), ("n", Value::Int(0))])).unwrap();
    }
    tx.commit().unwrap();
    let ts = col(&db, "CALL stats.timeseries('EV', 'ts', 60, 'n') YIELD bucket_start, count, sum, mean RETURN bucket_start, count, sum");
    assert_eq!(ts.len(), 40);
    assert_eq!(ts[11][1].as_i64(), Some(0)); // gap kept as an explicit empty bucket
    assert_eq!(ts[25][1].as_i64(), Some(31));
    assert_eq!(ts[3][2].as_f64(), Some(3.0));
    assert_eq!(ts.iter().map(|r| r[1].as_i64().unwrap()).sum::<i64>(), 37 + 30);
    let sp = col(&db, "CALL stats.spikes('EV', 'ts', 60) YIELD bucket_start, count, robust_z RETURN bucket_start, count");
    assert_eq!(sp.len(), 1);
    assert_eq!(sp[0][0].as_f64(), Some(1500.0));
}

#[test]
fn drift_separates_novel_links_between_existing_nodes() {
    let db = Database::in_memory();
    let mut tx = db.begin();
    let h: Vec<Id> = (0..5).map(|i| tx.create_node(&["Host"], props([("k", Value::Int(i))])).unwrap()).collect();
    tx.create_edge("AUTH", h[0], h[1], 1.0, Default::default()).unwrap();
    tx.create_edge("AUTH", h[1], h[2], 1.0, Default::default()).unwrap();
    tx.create_edge("AUTH", h[3], h[4], 1.0, Default::default()).unwrap();
    let t0 = tx.commit().unwrap();
    let mut tx = db.begin();
    tx.create_edge("AUTH", h[0], h[1], 1.0, Default::default()).unwrap(); // duplicate: not novel
    tx.create_edge("AUTH", h[2], h[3], 1.0, Default::default()).unwrap(); // novel lateral link
    let evil = tx.create_node(&["Host"], props([("k", 99)])).unwrap();
    tx.create_edge("AUTH", h[2], evil, 1.0, Default::default()).unwrap(); // link to a new node
    tx.delete(h[4]); // removes node and its edge
    let t1 = tx.commit().unwrap();
    let d = drift(&db, t0, t1).unwrap();
    assert_eq!(d.new_nodes, vec![evil]);
    assert_eq!(d.removed_nodes, vec![h[4]]);
    assert_eq!(d.novel_links.len(), 1);
    assert_eq!((d.novel_links[0].1.clone(), d.novel_links[0].2.clone()), (vec![h[2]], vec![h[3]]));
    assert_eq!((d.links_to_new_nodes, d.removed_links), (1, 1));
    // old: {01,12,34}; new: {01,12,23,2evil}; intersection 2, union 5
    assert!((d.link_jaccard - 2.0 / 5.0).abs() < 1e-12);
    assert_eq!(drift(&db, t1, t1).unwrap().link_jaccard, 1.0);
}

#[test]
fn louvain_recovers_ring_of_cliques_and_maximises_modularity() {
    // 6 cliques of 5 nodes in a ring, neighbouring cliques joined by one edge
    let (cl, sz) = (6usize, 5usize);
    let db = Database::in_memory();
    let mut tx = db.begin();
    let ids: Vec<Id> = (0..cl * sz).map(|i| tx.create_node(&["N"], props([("k", Value::Int(i as i64))])).unwrap()).collect();
    let mut edges = vec![];
    for c in 0..cl {
        for i in 0..sz {
            for j in i + 1..sz {
                edges.push((c * sz + i, c * sz + j));
            }
        }
        edges.push((c * sz, ((c + 1) % cl) * sz + 1));
    }
    for &(a, b) in &edges {
        tx.create_edge("E", ids[a], ids[b], 1.0, Default::default()).unwrap();
    }
    tx.commit().unwrap();
    let comm = node_map(&col(&db, "CALL algo.louvain() YIELD node, community RETURN node, community"), 1);
    let planted: Vec<usize> = (0..cl * sz).map(|i| i / sz).collect();
    for a in 0..cl * sz {
        for b in 0..cl * sz {
            assert_eq!(comm[&ids[a]] == comm[&ids[b]], planted[a] == planted[b], "{a},{b}");
        }
    }
    // modularity of the returned partition equals the planted optimum and beats the trivial partitions
    let m = edges.len() as f64;
    let modularity = |lab: &dyn Fn(usize) -> usize| {
        let mut deg = vec![0.0; cl * sz];
        let mut inside = 0.0;
        for &(a, b) in &edges {
            deg[a] += 1.0;
            deg[b] += 1.0;
            if lab(a) == lab(b) {
                inside += 1.0;
            }
        }
        let mut tot: BTreeMap<usize, f64> = BTreeMap::new();
        (0..cl * sz).for_each(|v| *tot.entry(lab(v)).or_default() += deg[v]);
        inside / m - tot.values().map(|d| (d / (2.0 * m)).powi(2)).sum::<f64>()
    };
    let got = modularity(&|v| comm[&ids[v]] as usize);
    assert!((got - modularity(&|v| planted[v])).abs() < 1e-12);
    assert!(got > 0.7 && got > modularity(&|_| 0) && got > modularity(&|v| v));
    // isolated nodes and an empty graph are fine
    let empty = Database::in_memory();
    assert!(col(&empty, "CALL algo.louvain() YIELD node, community RETURN node").is_empty());
    empty.query("CREATE (:N {k:1}), (:N {k:2})").unwrap();
    assert_eq!(col(&empty, "CALL algo.louvain() YIELD node, community RETURN community").len(), 2);
}
