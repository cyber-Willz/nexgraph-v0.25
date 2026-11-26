//! Profiling driver: random graph, full-scan aggregate queries.
use nexgraph::{props, Database, Value};
fn main() {
    let n = 20_000usize;
    let db = Database::in_memory();
    let mut tx = db.begin();
    let ids: Vec<_> = (0..n).map(|i| tx.create_node(&["N"], props([("k", Value::Int(i as i64))])).unwrap()).collect();
    let mut x = 12345u64;
    let mut r = || { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x };
    for _ in 0..160_000 {
        let (a, b) = ((r() % n as u64) as usize, (r() % n as u64) as usize);
        tx.create_edge("E", ids[a], ids[b], 1.0, Default::default()).unwrap();
    }
    tx.commit().unwrap();
    let t = std::time::Instant::now();
    let q = db.query("MATCH (a:N)-[:E]->(b) RETURN count(*)").unwrap();
    println!("{:?} {:?}", q.rows, t.elapsed());
    let t = std::time::Instant::now();
    let q = db.query("MATCH (a:N)-[:E]->(b) RETURN a.k AS k, count(*) AS d ORDER BY d DESC, k LIMIT 5").unwrap();
    println!("{:?} {:?}", q.rows.len(), t.elapsed());
}
