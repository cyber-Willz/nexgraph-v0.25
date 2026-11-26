//! Where does per-row time go? cargo run --release --features server --example prof_group -- DATA_DIR
use nexgraph::{props, Database};
use std::time::Instant;
fn main() {
    let dir = std::env::args().nth(1).expect("data dir");
    let db = Database::in_memory();
    let mut tx = db.begin();
    tx.create_property_index("N", "k");
    let nodes: Vec<u64> = std::fs::read_to_string(format!("{dir}/nodes.csv")).unwrap().lines().skip(1).map(|l| tx.create_node(&["N"], props([("k", l.parse::<i64>().unwrap())])).unwrap()).collect();
    for l in std::fs::read_to_string(format!("{dir}/edges.csv")).unwrap().lines().skip(1) {
        let (a, b) = l.split_once(',').unwrap();
        tx.create_edge("E", nodes[a.parse::<usize>().unwrap()], nodes[b.parse::<usize>().unwrap()], 1.0, Default::default()).unwrap();
    }
    tx.commit().unwrap();
    let qs = [
        ("edges: count(*)                         factorised, 1 group", "MATCH (a:N)-[:E]->(b) RETURN count(*)"),
        ("edges: group by a, LIMIT 1, no ORDER BY factorised, 100k groups", "MATCH (a:N)-[:E]->(b) RETURN a, count(*) AS d LIMIT 1"),
        ("edges: group by a, ORDER BY d LIMIT 1   + order keys + top-k", "MATCH (a:N)-[:E]->(b) RETURN a, count(*) AS d ORDER BY d DESC LIMIT 1"),
        ("edges: group by a.k, ORDER BY d, k      + property read", "MATCH (a:N)-[:E]->(b) RETURN a.k AS k, count(*) AS d ORDER BY d DESC, k LIMIT 5"),
        ("nodes: RETURN a.k LIMIT 1               early exit", "MATCH (a:N) RETURN a.k LIMIT 1"),
        ("nodes: count(*)                         100k nodes", "MATCH (a:N) RETURN count(*)"),
    ];
    for (name, q) in qs {
        db.query(q).unwrap();
        let mut best = f64::MAX;
        for _ in 0..3 {
            let t = Instant::now();
            db.query(q).unwrap();
            best = best.min(t.elapsed().as_secs_f64() * 1e3);
        }
        println!("{best:9.1} ms  {name}");
    }
}
