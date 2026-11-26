//! v0.3 regression tests: streaming executor, directed adjacency, bidirectional BFS.
use nexgraph::{props, Database, Value};
use std::collections::VecDeque;

fn rng(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

#[test]
fn bidirectional_bfs_matches_reference_bfs() {
    let n = 400usize;
    let mut s = 88172645463325252u64;
    let db = Database::in_memory();
    let mut tx = db.begin();
    let ids: Vec<_> = (0..n).map(|i| tx.create_node(&["N"], props([("k", Value::Int(i as i64))])).unwrap()).collect();
    let mut adj = vec![Vec::new(); n]; // reference adjacency (directed)
    for _ in 0..700 {
        let (a, b) = ((rng(&mut s) % n as u64) as usize, (rng(&mut s) % n as u64) as usize);
        tx.create_edge("E", ids[a], ids[b], 1.0, Default::default()).unwrap();
        adj[a].push(b);
    }
    // an undirected hyperedge: members reach each other
    let hm = [3usize, 77, 150, 301];
    tx.create_hyperedge("H", &hm.iter().map(|&i| ids[i]).collect::<Vec<_>>(), &[], 1.0, Default::default()).unwrap();
    for &a in &hm {
        for &b in &hm {
            if a != b {
                adj[a].push(b);
            }
        }
    }
    tx.commit().unwrap();
    db.query("CREATE INDEX ON :N(k)").unwrap();
    let mut checked = 0;
    for _ in 0..120 {
        let (a, b) = ((rng(&mut s) % n as u64) as usize, (rng(&mut s) % n as u64) as usize);
        let mut dist = vec![usize::MAX; n];
        dist[a] = 0;
        let mut q = VecDeque::from([a]);
        while let Some(u) = q.pop_front() {
            for &v in &adj[u] {
                if dist[v] == usize::MAX {
                    dist[v] = dist[u] + 1;
                    q.push_back(v);
                }
            }
        }
        let r = db.query(&format!("MATCH (a:N {{k:{a}}}), (b:N {{k:{b}}}) CALL algo.shortestPath(a, b) YIELD length RETURN length")).unwrap();
        match r.rows.first() {
            Some(row) => {
                assert_eq!(row[0].as_i64().unwrap() as usize, dist[b], "{a}->{b}");
                checked += 1;
            }
            None => assert_eq!(dist[b], usize::MAX, "{a}->{b} should be unreachable"),
        }
    }
    assert!(checked > 20);
}

#[test]
fn streaming_aggregates_match_expected_semantics() {
    let db = Database::in_memory();
    db.query("CREATE (a:P {n:'a', g:1, v:1}), (b:P {n:'b', g:1, v:2.5}), (c:P {n:'c', g:2, v:4}), (d:P {n:'d', g:2})").unwrap();
    let t = |q: &str| db.query(q).unwrap().rows;
    // mixed int/float sum -> float; null skipped
    assert_eq!(t("MATCH (p:P) WHERE p.g = 1 RETURN sum(p.v)"), vec![vec![Value::Float(3.5)]]);
    assert_eq!(t("MATCH (p:P) WHERE p.g = 2 RETURN sum(p.v), count(p.v), count(*), avg(p.v)"), vec![vec![Value::Float(4.0).clone(), Value::Int(1), Value::Int(2), Value::Float(4.0)]].into_iter().map(|mut r| { r[0] = Value::Int(4); r }).collect::<Vec<_>>());
    // group + ORDER BY a grouping expression + alias, LIMIT
    let rows = t("MATCH (p:P) RETURN p.g AS g, count(*) AS c ORDER BY g DESC LIMIT 1");
    assert_eq!(rows, vec![vec![Value::Int(2), Value::Int(2)]]);
    // empty input: global aggregates give one row, grouped gives none
    assert_eq!(t("MATCH (p:P) WHERE p.g = 99 RETURN count(*), sum(p.v), min(p.v), collect(p.n)"), vec![vec![Value::Int(0), Value::Int(0), Value::Null, Value::List(vec![])]]);
    assert!(t("MATCH (p:P) WHERE p.g = 99 RETURN p.g, count(*)").is_empty());
    // DISTINCT, collect distinct, and Int(1) vs Float(1.0) stay distinct keys
    assert_eq!(t("MATCH (p:P) RETURN DISTINCT p.g ORDER BY p.g").len(), 2);
    assert_eq!(t("MATCH (p:P) RETURN count(DISTINCT p.g)"), vec![vec![Value::Int(2)]]);
    // LIMIT without ORDER stops early but still returns the right number of rows; SKIP works
    assert_eq!(t("MATCH (p:P) RETURN p.n LIMIT 2").len(), 2);
    assert_eq!(t("MATCH (p:P) RETURN p.n ORDER BY p.n SKIP 1 LIMIT 2"), vec![vec![Value::Str("b".into())], vec![Value::Str("c".into())]]);
}

#[test]
fn directed_adjacency_self_loops_and_directions() {
    let db = Database::in_memory();
    db.query("CREATE (a:X {n:'a'}), (b:X {n:'b'}), (c:X {n:'c'}), (a)-[:R]->(b), (b)-[:R]->(c), (c)-[:R]->(c), (b)-[:R]->(a)").unwrap();
    let t = |q: &str| db.query(q).unwrap().rows;
    let n = |q: &str| t(q)[0][0].as_i64().unwrap();
    assert_eq!(n("MATCH (a:X)-[:R]->(b) RETURN count(*)"), 4);
    assert_eq!(n("MATCH (a:X)<-[:R]-(b) RETURN count(*)"), 4);
    assert_eq!(n("MATCH (a:X)-[:R]-(b) RETURN count(*)"), 8 - 1); // the self loop is crossed once
    assert_eq!(n("MATCH (a:X {n:'c'})-[:R]->(b) RETURN count(*)"), 1);
    assert_eq!(n("MATCH (a:X {n:'b'})<-[:R]-(b) RETURN count(*)"), 1);
    // a node in both tail and head of one hyperedge must not be double counted
    db.query("MATCH (a:X {n:'a'}), (b:X {n:'b'}) CREATE HYPEREDGE :H FROM (a, b) TO (b)").unwrap();
    assert_eq!(n("MATCH (a:X {n:'a'})-[]->(b) RETURN count(*)"), 2);
}

// ---- v0.4: flat metadata, factorised counting ----

#[test]
fn factorised_counts_equal_generic_path_and_reference() {
    let n = 300usize;
    let mut s = 0x9E3779B97F4A7C15u64;
    let db = Database::in_memory();
    let mut tx = db.begin();
    let lab = |i: usize| if i % 3 == 0 { "A" } else { "B" };
    let ids: Vec<_> = (0..n).map(|i| tx.create_node(&[lab(i)], props([("k", Value::Int(i as i64)), ("g", Value::Int((i % 5) as i64))])).unwrap()).collect();
    let mut edges = Vec::new(); // (from, to, label)
    for _ in 0..900 {
        let (a, b) = ((rng(&mut s) % n as u64) as usize, (rng(&mut s) % n as u64) as usize);
        let l = if rng(&mut s) % 2 == 0 { "X" } else { "Y" };
        tx.create_edge(l, ids[a], ids[b], 1.0, Default::default()).unwrap();
        edges.push((a, b, l));
    }
    tx.commit().unwrap();
    let ts_before = db.query("MATCH (a:A) RETURN count(*)").unwrap().ts;
    // delete some nodes (cascades to their edges)
    let dead: Vec<usize> = (0..n).filter(|i| i % 11 == 0).collect();
    db.query("MATCH (a) WHERE a.k % 11 = 0 DELETE a").unwrap();
    let alive = |i: usize| !dead.contains(&i);
    let live_edges: Vec<_> = edges.iter().filter(|(a, b, _)| alive(*a) && alive(*b)).collect();

    for (pat, el, sl, tl, dir) in [
        ("(a:A)-[:X]->(b:B)", Some("X"), Some("A"), Some("B"), 0),
        ("(a)-[:Y]->(b)", Some("Y"), None, None, 0),
        ("(a:B)<-[:X]-(b:A)", Some("X"), Some("B"), Some("A"), 1),
        ("(a)-[]-(b:A)", None, None, Some("A"), 2),
        ("(a:A)-[:Z]->(b)", Some("Z"), Some("A"), None, 0),
    ] {
        // reference: per start node count
        let mut want = std::collections::BTreeMap::<i64, i64>::new();
        for &&(f, t, l) in &live_edges {
            let ends: Vec<(usize, usize)> = match dir {
                0 => vec![(f, t)],
                1 => vec![(t, f)],
                _ => if f == t { vec![(f, t)] } else { vec![(f, t), (t, f)] },
            };
            for (a, b) in ends {
                if el.map_or(true, |x| x == l) && sl.map_or(true, |x| x == lab(a)) && tl.map_or(true, |x| x == lab(b)) {
                    *want.entry(a as i64).or_default() += 1;
                }
            }
        }
        let fast = db.query(&format!("MATCH {pat} RETURN a.k AS k, count(*) AS c ORDER BY k")).unwrap().rows;
        let slow = db.query(&format!("MATCH {pat} WHERE true RETURN a.k AS k, count(*) AS c ORDER BY k")).unwrap().rows;
        let got: Vec<(i64, i64)> = fast.iter().map(|r| (r[0].as_i64().unwrap(), r[1].as_i64().unwrap())).collect();
        let exp: Vec<(i64, i64)> = want.into_iter().collect();
        assert_eq!(got, exp, "{pat}: fast vs reference");
        assert_eq!(fast, slow, "{pat}: fast vs generic path");
        let total = db.query(&format!("MATCH {pat} RETURN count(*)")).unwrap().rows[0][0].as_i64().unwrap();
        assert_eq!(total, exp.iter().map(|x| x.1).sum::<i64>(), "{pat}: total");
    }
    // time travel: the deleted nodes are still there before the delete
    let old = db.query(&format!("AS OF {ts_before} MATCH (a:A)-[:X]->(b) RETURN count(*)")).unwrap().rows[0][0].as_i64().unwrap();
    let want_old = edges.iter().filter(|(f, _, l)| *l == "X" && lab(*f) == "A").count() as i64;
    assert_eq!(old, want_old);
    // top-K with ties is identical to the stable full sort
    let full = db.query("MATCH (a)-[]->(b) RETURN a.g AS g, count(*) AS c ORDER BY g").unwrap().rows;
    let top = db.query("MATCH (a)-[]->(b) RETURN a.g AS g, count(*) AS c ORDER BY g SKIP 1 LIMIT 2").unwrap().rows;
    assert_eq!(top, full[1..3].to_vec());
    let top = db.query("MATCH (a)-[]->(b) RETURN a.k AS k ORDER BY a.g DESC LIMIT 7").unwrap().rows;
    let all = db.query("MATCH (a)-[]->(b) RETURN a.k AS k ORDER BY a.g DESC").unwrap().rows;
    assert_eq!(top, all[..7].to_vec());
}
