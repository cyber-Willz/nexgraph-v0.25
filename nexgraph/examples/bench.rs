//! cargo run --release --example bench
use nexgraph::{props, Database, Metric, SyncMode, Value};
use std::time::Instant;

fn rng(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

fn main() {
    let n = 100_000usize;
    let deg = 8usize;
    let mut s = 0x1234_5678_9abc_def1u64;
    let db = Database::in_memory();
    let t = Instant::now();
    let mut tx = db.begin();
    tx.create_property_index("N", "k");
    let ids: Vec<_> = (0..n).map(|i| tx.create_node(&["N"], props([("k", i as i64)])).unwrap()).collect();
    for &a in &ids {
        for _ in 0..deg {
            let b = ids[(rng(&mut s) % n as u64) as usize];
            tx.create_edge("E", a, b, 1.0, Default::default()).unwrap();
        }
    }
    tx.commit().unwrap();
    let el = t.elapsed();
    println!("bulk load {n} nodes + {} edges in one txn: {:.2}s ({:.0} entities/s)", n * deg, el.as_secs_f64(), (n * (deg + 1)) as f64 / el.as_secs_f64());

    let q = 20_000;
    let t = Instant::now();
    for i in 0..q {
        let r = db.query(&format!("MATCH (a:N {{k: {}}}) RETURN a.k", (i * 7) % n)).unwrap();
        assert_eq!(r.rows.len(), 1);
    }
    println!("indexed point query (parse+exec): {:.1} us/query", t.elapsed().as_secs_f64() * 1e6 / q as f64);

    let t = Instant::now();
    let mut total = 0;
    for i in 0..2000 {
        total += db.query(&format!("MATCH (a:N {{k: {}}})-[:E]->(b)-[:E]->(c) RETURN count(*)", (i * 13) % n)).unwrap().rows[0][0].as_i64().unwrap();
    }
    println!("2-hop traversal from indexed node: {:.1} us/query (avg {} paths)", t.elapsed().as_secs_f64() * 1e6 / 2000.0, total / 2000);

    let t = Instant::now();
    let r = db.query("MATCH (a:N)-[:E]->(b) RETURN count(*)").unwrap();
    println!("full edge scan count = {} in {:.0} ms", r.rows[0][0], t.elapsed().as_secs_f64() * 1e3);

    let t = Instant::now();
    db.query("CALL algo.pagerank(0.85, 30) YIELD node, score RETURN max(score)").unwrap();
    println!("pagerank 30 iters on {n} nodes: {:.0} ms", t.elapsed().as_secs_f64() * 1e3);

    // vector index
    let dim = 32;
    let vdb = Database::in_memory();
    vdb.query(&format!("CREATE VECTOR INDEX ON :D(e) DIM {dim} METRIC cosine")).unwrap();
    let vn = 20_000;
    let t = Instant::now();
    let mut tx = vdb.begin();
    for _ in 0..vn {
        let v: Vec<f32> = (0..dim).map(|_| (rng(&mut s) % 10_000) as f32 / 10_000.0 - 0.5).collect();
        tx.create_node(&["D"], props([("e", Value::Vector(v))])).unwrap();
    }
    tx.commit().unwrap();
    println!("vector insert {vn} x {dim}d (HNSW build): {:.2}s", t.elapsed().as_secs_f64());
    let qs: Vec<Vec<f32>> = (0..200).map(|_| (0..dim).map(|_| (rng(&mut s) % 10_000) as f32 / 10_000.0 - 0.5).collect()).collect();
    let (mut ta, mut te, mut hit) = (0.0, 0.0, 0);
    for q in &qs {
        let t = Instant::now();
        let a = vdb.read(|v| v.vector_search("D", "e", q, 10, 64, false)).unwrap();
        ta += t.elapsed().as_secs_f64();
        let t = Instant::now();
        let e = vdb.read(|v| v.vector_search("D", "e", q, 10, 64, true)).unwrap();
        te += t.elapsed().as_secs_f64();
        hit += a.iter().filter(|x| e.iter().any(|y| y.0 == x.0)).count();
    }
    println!("vector top-10: HNSW {:.0} us vs exact {:.0} us per query, recall@10 = {:.3}", ta * 1e6 / 200.0, te * 1e6 / 200.0, hit as f64 / 2000.0);

    // durability cost
    for mode in [SyncMode::Never, SyncMode::Always] {
        let dir = std::env::temp_dir().join(format!("nexgraph-bench-{}-{:?}", std::process::id(), mode));
        let _ = std::fs::remove_dir_all(&dir);
        let d = Database::open_with(&dir, mode).unwrap();
        let k = if mode == SyncMode::Always { 300 } else { 3000 };
        let t = Instant::now();
        for i in 0..k {
            d.query(&format!("CREATE (a:T {{i:{i}}})")).unwrap();
        }
        println!("single-statement commits, sync={:?}: {:.0} commits/s", mode, k as f64 / t.elapsed().as_secs_f64());
        drop(d);
        let t = Instant::now();
        let d = Database::open(&dir).unwrap();
        println!("  recovery of {k} commits: {:.0} ms (nodes {})", t.elapsed().as_secs_f64() * 1e3, d.stats().live_nodes);
        let _ = std::fs::remove_dir_all(&dir);
    }
    let _ = Metric::L2;
}
