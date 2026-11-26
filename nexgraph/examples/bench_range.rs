//! Range predicate on an indexed property vs the label scan (v0.22).
//! `cargo run --release --example bench_range [nodes]`
use nexgraph::{props, Database};
use std::time::Instant;

fn main() {
    let n: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(200_000);
    let db = Database::in_memory();
    let mut x = 12345u64;
    let mut tx = db.begin();
    for i in 0..n {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let v = (x >> 33) % 1_000_000;
        tx.create_node(&["P"], props([("id", i as i64), ("v", v as i64)])).unwrap();
    }
    tx.commit().unwrap();
    // 0.01% of the nodes per query
    let qs: Vec<String> = (0..50).map(|k| format!("MATCH (p:P) WHERE p.v >= {} AND p.v < {} RETURN count(*)", k * 10_000, k * 10_000 + 100)).collect();
    let run = |label: &str| -> Vec<i64> {
        let t = Instant::now();
        let mut out = vec![];
        for q in &qs {
            if let nexgraph::Value::Int(c) = db.query(q).unwrap().rows[0][0] {
                out.push(c);
            }
        }
        println!("{label:>10}: {:>9.1} us/query ({} queries, {} nodes)", t.elapsed().as_secs_f64() * 1e6 / qs.len() as f64, qs.len(), n);
        out
    };
    let a = run("scan");
    db.query("CREATE INDEX ON :P(v)").unwrap();
    let b = run("index");
    assert_eq!(a, b, "indexed and scanned counts must agree");
    println!("matched per query: {:?}...", &a[..5]);
}
