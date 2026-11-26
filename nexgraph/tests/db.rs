use nexgraph::{props, Database, Direction, Error, Metric, Params, Value};
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::Arc;

fn tmpdir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("nexgraph-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn social() -> Database {
    let db = Database::in_memory();
    db.query(
        "CREATE (a:Person {name:'Ada', age:36}), (b:Person {name:'Bob', age:41}), (c:Person {name:'Cy', age:29}), (d:Person {name:'Di', age:52}),
         (a)-[:KNOWS {since:2001}]->(b), (b)-[:KNOWS {since:2005}]->(c), (c)-[:KNOWS {since:2010}]->(d)",
    )
    .unwrap();
    db
}

fn col(r: &nexgraph::QueryResult, i: usize) -> Vec<Value> {
    r.rows.iter().map(|x| x[i].clone()).collect()
}

#[test]
fn basic_match_where_order_limit() {
    let db = social();
    let r = db.query("MATCH (p:Person) WHERE p.age > 30 RETURN p.name AS n ORDER BY p.age DESC LIMIT 2").unwrap();
    assert_eq!(col(&r, 0), vec![Value::Str("Di".into()), Value::Str("Bob".into())]);
    let r = db.query("MATCH (a)-[k:KNOWS]->(b) WHERE k.since >= 2005 RETURN a.name, b.name ORDER BY a.name").unwrap();
    assert_eq!(r.rows.len(), 2);
    assert_eq!(r.rows[0][0], Value::Str("Bob".into()));
}

#[test]
fn variable_length_and_direction() {
    let db = social();
    let r = db.query("MATCH (a:Person {name:'Ada'})-[:KNOWS*1..3]->(x) RETURN x.name ORDER BY x.name").unwrap();
    assert_eq!(col(&r, 0), vec!["Bob".into(), "Cy".into(), "Di".into()]);
    let r = db.query("MATCH (a:Person {name:'Di'})<-[:KNOWS*2]-(x) RETURN x.name").unwrap();
    assert_eq!(col(&r, 0), vec![Value::Str("Bob".into())]);
    // undirected
    let r = db.query("MATCH (a:Person {name:'Bob'})-[:KNOWS]-(x) RETURN x.name ORDER BY x.name").unwrap();
    assert_eq!(col(&r, 0), vec!["Ada".into(), "Cy".into()]);
    // wrong direction yields nothing
    let r = db.query("MATCH (a:Person {name:'Ada'})<-[:KNOWS]-(x) RETURN x").unwrap();
    assert!(r.rows.is_empty());
}

#[test]
fn aggregation_and_distinct() {
    let db = social();
    let r = db.query("MATCH (p:Person) RETURN count(*) AS n, avg(p.age) AS a, min(p.age), max(p.age), sum(p.age)").unwrap();
    assert_eq!(r.rows[0][0], Value::Int(4));
    assert_eq!(r.rows[0][1], Value::Float(39.5));
    assert_eq!(r.rows[0][4], Value::Int(158));
    let r = db.query("MATCH (a)-[:KNOWS]->(b) RETURN a.name, count(b) AS c ORDER BY c DESC, a.name").unwrap();
    assert_eq!(r.rows.len(), 3);
    let r = db.query("MATCH (p:Person) RETURN DISTINCT p.age > 40 AS old").unwrap();
    assert_eq!(r.rows.len(), 2);
    let r = db.query("MATCH (x:Nothing) RETURN count(*)").unwrap();
    assert_eq!(r.rows[0][0], Value::Int(0));
}

#[test]
fn set_delete_and_cascade() {
    let db = social();
    db.query("MATCH (p:Person {name:'Ada'}) SET p.age = 37, p.title = 'Countess'").unwrap();
    let r = db.query("MATCH (p:Person {name:'Ada'}) RETURN p.age, p.title").unwrap();
    assert_eq!(r.rows[0], vec![Value::Int(37), Value::Str("Countess".into())]);
    db.query("MATCH (p:Person {name:'Ada'}) SET p.title = null").unwrap();
    let r = db.query("MATCH (p:Person {name:'Ada'}) RETURN p.title").unwrap();
    assert_eq!(r.rows[0][0], Value::Null);
    let r = db.query("MATCH (p:Person {name:'Bob'}) DELETE p").unwrap();
    assert_eq!(r.stats.deleted, 1);
    let r = db.query("MATCH (a)-[:KNOWS]->(b) RETURN a.name, b.name").unwrap();
    assert_eq!(r.rows.len(), 1, "Bob's two edges must be gone: {:?}", r.rows);
}

#[test]
fn hyperedges() {
    let db = social();
    db.query("MATCH (a:Person {name:'Ada'}), (b:Person {name:'Bob'}), (c:Person {name:'Cy'}) CREATE HYPEREDGE m:Meeting {topic:'plan'} OVER (a, b, c)").unwrap();
    db.query("MATCH (a:Person {name:'Ada'}), (b:Person {name:'Bob'}), (d:Person {name:'Di'}) CREATE HYPEREDGE :Reaction {rate:0.5, weight:2.5} FROM (a, b) TO (d)").unwrap();
    let r = db.query("MATCH HYPEREDGE h:Meeting RETURN size(members(h)), h.topic").unwrap();
    assert_eq!(r.rows[0], vec![Value::Int(3), Value::Str("plan".into())]);
    // undirected hyperedge is crossed only by undirected patterns
    let r = db.query("MATCH (a:Person {name:'Ada'})-[:Meeting]-(x) RETURN x.name ORDER BY x.name").unwrap();
    assert_eq!(col(&r, 0), vec!["Bob".into(), "Cy".into()]);
    let r = db.query("MATCH (a:Person {name:'Ada'})-[:Meeting]->(x) RETURN x.name").unwrap();
    assert!(r.rows.is_empty());
    // directed hyperedge: every tail member reaches the head
    let r = db.query("MATCH (a:Person {name:'Bob'})-[r:Reaction]->(x) RETURN x.name, r.weight, r.rate").unwrap();
    assert_eq!(r.rows[0], vec![Value::Str("Di".into()), Value::Float(2.5), Value::Float(0.5)]);
    let r = db.query("MATCH HYPEREDGE h:Reaction RETURN size(tail(h)), size(head(h))").unwrap();
    assert_eq!(r.rows[0], vec![Value::Int(2), Value::Int(1)]);
    // deleting a member removes the whole hyperedge
    db.query("MATCH (a:Person {name:'Cy'}) DELETE a").unwrap();
    let r = db.query("MATCH HYPEREDGE h:Meeting RETURN h").unwrap();
    assert!(r.rows.is_empty());
}

#[test]
fn time_travel_and_snapshot_isolation() {
    let db = Database::in_memory();
    let t1 = db.query("CREATE (a:T {v:1})").unwrap().ts;
    let t2 = db.query("MATCH (a:T) SET a.v = 2").unwrap().ts;
    let t3 = db.query("MATCH (a:T) DELETE a").unwrap().ts;
    assert!(t1 < t2 && t2 < t3);
    let v = |ts: u64| db.query(&format!("AS OF {ts} MATCH (a:T) RETURN a.v")).unwrap().rows;
    assert_eq!(v(t1), vec![vec![Value::Int(1)]]);
    assert_eq!(v(t2), vec![vec![Value::Int(2)]]);
    assert!(v(t3).is_empty());
    assert!(db.query("AS OF 999 MATCH (a) RETURN a").is_err());
    assert!(db.query("AS OF 1 CREATE (a)").is_err());
}

#[test]
fn rollback_and_validation() {
    let db = Database::in_memory();
    let mut tx = db.begin();
    let a = tx.create_node(&["X"], props([("k", 1i64)])).unwrap();
    tx.rollback();
    assert_eq!(db.stats().live_nodes, 0);
    // edge to missing node is rejected atomically
    let mut tx = db.begin();
    let n = tx.create_node(&["X"], Default::default()).unwrap();
    tx.create_edge("E", n, 9999, 1.0, Default::default()).unwrap();
    assert!(matches!(tx.commit(), Err(Error::NotFound(9999))));
    assert_eq!(db.stats().live_nodes, 0, "failed commit must apply nothing");
    let _ = a;
    // node refs are not storable
    {
        let mut tx = db.begin();
        assert!(tx.create_node(&["X"], props([("r", Value::Node(1))])).is_err());
    }
    // create + delete in one txn is allowed
    let mut tx = db.begin();
    let n = tx.create_node(&["X"], Default::default()).unwrap();
    tx.delete(n);
    tx.commit().unwrap();
    assert_eq!(db.stats().live_nodes, 0);
}

#[test]
fn property_index_equivalence() {
    let db = Database::in_memory();
    let mut tx = db.begin();
    for i in 0..500i64 {
        tx.create_node(&["U"], props([("k", i % 50), ("s", i)])).unwrap();
    }
    tx.commit().unwrap();
    let before = db.read(|v| v.lookup("U", "k", &Value::Int(7)));
    db.query("CREATE INDEX ON :U(k)").unwrap();
    let after = db.read(|v| v.lookup("U", "k", &Value::Int(7)));
    assert_eq!(before.len(), 10);
    assert_eq!(before, after);
    // updates keep the index correct (stale entries are re-verified)
    db.query("MATCH (u:U {s: 7}) SET u.k = 1000").unwrap();
    assert_eq!(db.read(|v| v.lookup("U", "k", &Value::Int(7))).len(), 9);
    assert_eq!(db.read(|v| v.lookup("U", "k", &Value::Int(1000))).len(), 1);
    // float/int equivalence
    assert_eq!(db.read(|v| v.lookup("U", "k", &Value::Float(1000.0))).len(), 1);
    let r = db.query("MATCH (u:U {k: 1000}) RETURN u.s").unwrap();
    assert_eq!(r.rows[0][0], Value::Int(7));
}

#[test]
fn persistence_recovery_and_checkpoint() {
    let dir = tmpdir("persist");
    {
        let db = Database::open(&dir).unwrap();
        db.query("CREATE (a:P {n:'a'}), (b:P {n:'b'}), (a)-[:R {weight:3.5}]->(b)").unwrap();
        db.query("CREATE INDEX ON :P(n)").unwrap();
        db.query("MATCH (a:P {n:'a'}) SET a.n = 'a2'").unwrap();
    }
    {
        let db = Database::open(&dir).unwrap();
        assert_eq!(db.last_ts(), 3);
        let r = db.query("MATCH (a:P {n:'a2'})-[r:R]->(b) RETURN b.n, r.weight").unwrap();
        assert_eq!(r.rows[0], vec![Value::Str("b".into()), Value::Float(3.5)]);
        assert_eq!(db.stats().prop_indexes, 1);
        // history survives a plain restart
        let r = db.query("AS OF 1 MATCH (a:P {n:'a'}) RETURN a").unwrap();
        assert_eq!(r.rows.len(), 1);
        db.checkpoint().unwrap();
        assert!(db.query("AS OF 1 MATCH (a) RETURN a").is_err(), "history compacted");
        db.query("CREATE (c:P {n:'c'})").unwrap();
    }
    {
        let db = Database::open(&dir).unwrap();
        assert_eq!(db.stats().live_nodes, 3);
        assert_eq!(db.last_ts(), 4);
        // ids keep increasing after checkpoint + restart
        let before = db.read(|v| v.node_ids().max().unwrap());
        db.query("CREATE (z:P {n:'z'})").unwrap();
        assert!(db.read(|v| v.node_ids().max().unwrap()) > before);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn torn_tail_is_truncated_and_midlog_corruption_detected() {
    let dir = tmpdir("torn");
    {
        let db = Database::open(&dir).unwrap();
        for i in 0..5 {
            db.query(&format!("CREATE (a:N {{i:{i}}})")).unwrap();
        }
    }
    let wal = dir.join("wal.log");
    let good = std::fs::read(&wal).unwrap();
    // simulate a crash mid-append: garbage partial frame at the end
    OpenOptions::new().append(true).open(&wal).unwrap().write_all(&[0xFF, 0x00, 0x00, 0x00, 1, 2, 3]).unwrap();
    {
        let db = Database::open(&dir).unwrap();
        assert_eq!(db.stats().live_nodes, 5);
        db.query("CREATE (a:N {i:5})").unwrap(); // log still appendable
    }
    {
        let db = Database::open(&dir).unwrap();
        assert_eq!(db.stats().live_nodes, 6);
    }
    // flip a byte inside the first record: must be reported, not skipped
    let mut bytes = std::fs::read(&wal).unwrap();
    assert!(bytes.len() > good.len() - 10);
    bytes[10] ^= 0xFF;
    std::fs::write(&wal, &bytes).unwrap();
    assert!(matches!(Database::open(&dir), Err(Error::Corrupt(_))));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn concurrent_readers_see_consistent_snapshots() {
    let db = Arc::new(Database::in_memory());
    db.query("CREATE (a:Acct {id:1, bal:100}), (b:Acct {id:2, bal:100})").unwrap();
    let writer = {
        let db = db.clone();
        std::thread::spawn(move || {
            for i in 0..200 {
                // transfer keeps the total invariant at 200
                let mut tx = db.begin();
                let (a, b) = db.read(|v| (v.lookup("Acct", "id", &Value::Int(1))[0], v.lookup("Acct", "id", &Value::Int(2))[0]));
                tx.set_prop(a, "bal", Value::Int(100 - i)).unwrap();
                tx.set_prop(b, "bal", Value::Int(100 + i)).unwrap();
                tx.commit().unwrap();
            }
        })
    };
    let readers: Vec<_> = (0..4)
        .map(|_| {
            let db = db.clone();
            std::thread::spawn(move || {
                for _ in 0..300 {
                    let r = db.query("MATCH (a:Acct) RETURN sum(a.bal)").unwrap();
                    assert_eq!(r.rows[0][0], Value::Int(200), "torn read");
                }
            })
        })
        .collect();
    writer.join().unwrap();
    readers.into_iter().for_each(|r| r.join().unwrap());
}

#[test]
fn vector_index_search_matches_exact() {
    let db = Database::in_memory();
    db.query("CREATE VECTOR INDEX ON :Doc(emb) DIM 8 METRIC cosine").unwrap();
    let mut tx = db.begin();
    let mut s = 12345u64;
    let mut rnd = || {
        s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((s >> 33) as f32) / (1u64 << 31) as f32 - 0.5
    };
    for i in 0..600i64 {
        let v: Vec<f32> = (0..8).map(|_| rnd()).collect();
        tx.create_node(&["Doc"], props([("i", Value::Int(i)), ("emb", Value::Vector(v))])).unwrap();
    }
    tx.commit().unwrap();
    let q: Vec<f32> = (0..8).map(|_| rnd()).collect();
    let approx = db.read(|v| v.vector_search("Doc", "emb", &q, 10, 100, false)).unwrap();
    let exact = db.read(|v| v.vector_search("Doc", "emb", &q, 10, 100, true)).unwrap();
    let a: HashSet<_> = approx.iter().map(|x| x.0).collect();
    let e: HashSet<_> = exact.iter().map(|x| x.0).collect();
    assert!(a.intersection(&e).count() >= 9, "recall too low");
    assert!(exact.windows(2).all(|w| w[0].1 <= w[1].1));
    // procedure form, with a parameter
    let mut p = Params::new();
    p.insert("q".into(), Value::Vector(q.clone()));
    let r = db.query_with("CALL vector.search('Doc', 'emb', $q, 3, true) YIELD node, distance RETURN node.i, distance", &p).unwrap();
    assert_eq!(r.rows.len(), 3);
    // deleted nodes disappear from results; updated vectors use the new value
    let top = exact[0].0;
    let mut tx = db.begin();
    tx.delete(top);
    tx.commit().unwrap();
    let after = db.read(|v| v.vector_search("Doc", "emb", &q, 10, 100, false)).unwrap();
    assert!(after.iter().all(|x| x.0 != top));
    let nn = after[0].0;
    let mut tx = db.begin();
    tx.set_prop(nn, "emb", Value::Vector(q.iter().map(|x| -x).collect::<Vec<_>>())).unwrap();
    tx.commit().unwrap();
    let after2 = db.read(|v| v.vector_search("Doc", "emb", &q, 5, 100, true)).unwrap();
    assert!(after2.iter().all(|x| x.0 != nn || x.1 > 1.5), "stale vector leaked");
    let _ = Metric::L2;
}

fn two_cliques() -> Database {
    // two 6-cliques joined by one weak bridge
    let db = Database::in_memory();
    let mut tx = db.begin();
    let ids: Vec<_> = (0..12).map(|i| tx.create_node(&["V"], props([("i", i as i64), ("side", (i / 6) as i64)])).unwrap()).collect();
    for side in 0..2 {
        for i in 0..6 {
            for j in (i + 1)..6 {
                tx.create_edge("E", ids[side * 6 + i], ids[side * 6 + j], 1.0, Default::default()).unwrap();
            }
        }
    }
    tx.create_edge("E", ids[5], ids[6], 0.1, Default::default()).unwrap();
    tx.commit().unwrap();
    db
}

#[test]
fn spectral_procedures_recover_planted_partition() {
    let db = two_cliques();
    let r = db.query("CALL spectral.cluster(2) YIELD node, cluster RETURN node.side AS side, cluster, count(*) AS n").unwrap();
    // each side must map to exactly one cluster
    let mut map = std::collections::HashMap::new();
    for row in &r.rows {
        if let (Value::Int(s), Value::Int(c)) = (&row[0], &row[1]) {
            assert_eq!(*map.entry(*s).or_insert(*c), *c, "side split across clusters: {:?}", r.rows);
        }
    }
    assert_eq!(map.len(), 2);
    assert_ne!(map[&0], map[&1]);

    let r = db.query("CALL spectral.eigenvalues(3) YIELD eigenvalue RETURN eigenvalue").unwrap();
    let ev: Vec<f64> = r.rows.iter().map(|x| x[0].as_f64().unwrap()).collect();
    assert!(ev[0].abs() < 1e-8, "lambda_0 should be 0, got {}", ev[0]);
    assert!(ev[1] < 0.1 && ev[2] > 5.0 * ev[1], "spectral gap after the planted cut: {ev:?}");

    // Fiedler vector has opposite signs on the two cliques
    let r = db.query("CALL spectral.fiedler() YIELD node, value RETURN node.side, value").unwrap();
    let sign = |side: i64| -> Vec<bool> { r.rows.iter().filter(|x| x[0] == Value::Int(side)).map(|x| x[1].as_f64().unwrap() > 0.0).collect() };
    for side in 0..2 {
        let s = sign(side);
        assert!(s.iter().all(|x| *x == s[0]));
    }
    assert_ne!(sign(0)[0], sign(1)[0]);

    let r = db.query("CALL spectral.embedding(2) YIELD node, embedding RETURN size(embedding)").unwrap();
    assert_eq!(r.rows[0][0], Value::Int(2));
}

#[test]
fn spectral_works_on_hyperedges() {
    let db = Database::in_memory();
    db.query("CREATE (a:V),(b:V),(c:V),(d:V),(e:V),(f:V)").unwrap();
    db.query("MATCH (a:V),(b:V),(c:V) WHERE id(a) < id(b) AND id(b) < id(c) AND id(c) <= 3 CREATE HYPEREDGE :H OVER (a,b,c)").unwrap();
    db.query("MATCH (d:V),(e:V),(f:V) WHERE id(d) = 4 AND id(e) = 5 AND id(f) = 6 CREATE HYPEREDGE :H OVER (d,e,f)").unwrap();
    db.query("MATCH (c:V),(d:V) WHERE id(c) = 3 AND id(d) = 4 CREATE HYPEREDGE :H {weight:0.05} OVER (c,d)").unwrap();
    let r = db.query("CALL spectral.cluster(2) YIELD node, cluster RETURN id(node), cluster ORDER BY id(node)").unwrap();
    let c: Vec<_> = r.rows.iter().map(|x| x[1].clone()).collect();
    assert_eq!(c[0], c[1]);
    assert_eq!(c[1], c[2]);
    assert_eq!(c[3], c[4]);
    assert_eq!(c[4], c[5]);
    assert_ne!(c[0], c[3]);
}

#[test]
fn graph_algorithms() {
    let db = social();
    let r = db.query("MATCH (a:Person {name:'Ada'}), (d:Person {name:'Di'}) CALL algo.shortestPath(a, d) YIELD path, length RETURN length").unwrap();
    assert_eq!(r.rows[0][0], Value::Int(3));
    let r = db.query("MATCH (a:Person {name:'Di'}), (d:Person {name:'Ada'}) CALL algo.shortestPath(a, d) YIELD length RETURN length").unwrap();
    assert!(r.rows.is_empty(), "edges are directed");
    let r = db.query("CALL algo.pagerank() YIELD node, score RETURN sum(score)").unwrap();
    assert!((r.rows[0][0].as_f64().unwrap() - 1.0).abs() < 1e-9);
    let r = db.query("CALL algo.components() YIELD component RETURN count(DISTINCT component)").unwrap();
    assert_eq!(r.rows[0][0], Value::Int(1));
    db.query("MATCH (a:Person {name:'Ada'}), (c:Person {name:'Cy'}) CREATE (a)-[:KNOWS {weight:0.5}]->(c)").unwrap();
    let r = db.query("MATCH (a:Person {name:'Ada'}), (d:Person {name:'Di'}) CALL algo.dijkstra(a, d) YIELD distance RETURN distance").unwrap();
    assert_eq!(r.rows[0][0], Value::Float(1.5)); // Ada->Cy (0.5) + Cy->Di (default 1.0)
}

#[test]
fn query_errors_are_reported_not_panicked() {
    let db = social();
    for bad in [
        "MATCH (a RETURN a",
        "MATCH (a) RETURN nosuch",
        "MATCH (a:Person) RETURN a.age + 'x'",
        "MATCH (a:Person) RETURN a.name / 2",
        "CALL no.such()",
        "CREATE (a)-[:X]-(b)",
        "MATCH (a:Person) RETURN count(a) + 1",
        "",
    ] {
        assert!(db.query(bad).is_err(), "should fail: {bad}");
    }
    // a failed write statement leaves no trace
    let n = db.stats().live_nodes;
    assert!(db.query("CREATE (z:Z), (q:Q) RETURN nosuch").is_err());
    assert_eq!(db.stats().live_nodes, n);
}

#[test]
fn traversal_budget_stops_runaway_queries() {
    let db = Database::in_memory();
    // dense complete digraph: unbounded path enumeration explodes
    let mut tx = db.begin();
    let ids: Vec<_> = (0..14).map(|_| tx.create_node(&["K"], Default::default()).unwrap()).collect();
    for &a in &ids {
        for &b in &ids {
            if a != b {
                tx.create_edge("E", a, b, 1.0, Default::default()).unwrap();
            }
        }
    }
    tx.commit().unwrap();
    let r = db.query("MATCH (a:K)-[:E*1..14]->(b:K) RETURN count(*)");
    assert!(matches!(r, Err(Error::Query(m)) if m.contains("budget")));
    let _ = Direction::Out;
}
