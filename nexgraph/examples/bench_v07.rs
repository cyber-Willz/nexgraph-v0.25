//! v0.7 benchmark: analytics and cyber procedures on a 100k-node / 800k-edge random graph.
use nexgraph::{props, Database, Value};
use std::time::Instant;
fn main() {
    let (n, m) = (100_000usize, 800_000usize);
    let mut s = 88172645463325252u64;
    let mut r = move || { s ^= s << 13; s ^= s >> 7; s ^= s << 17; s };
    let db = Database::in_memory();
    let mut tx = db.begin();
    let ids: Vec<_> = (0..n).map(|i| tx.create_node(&[if i % 100 == 0 { "Entry" } else if i % 1000 == 1 { "Crown" } else { "Host" }], props([("k", Value::Int(i as i64)), ("v", Value::Float((r() % 1000) as f64))])).unwrap()).collect();
    for _ in 0..m {
        let (a, b) = ((r() % n as u64) as usize, (r() % n as u64) as usize);
        tx.create_edge("FLOW", ids[a], ids[b], 1.0, props([("ts", Value::Float((r() % 86400) as f64)), ("bytes", Value::Int((r() % 100000) as i64))])).unwrap();
    }
    tx.commit().unwrap();
    for q in [
        "CALL algo.degree() YIELD node, in_degree RETURN count(*)",
        "CALL algo.kcore() YIELD node, core RETURN max(core)",
        "CALL algo.triangles() YIELD node, triangles RETURN sum(triangles)",
        "CALL algo.scc() YIELD node, component RETURN count(DISTINCT component)",
        "CALL algo.articulation() YIELD node RETURN count(*)",
        "CALL algo.louvain() YIELD node, community RETURN count(DISTINCT community)",
        "CALL algo.betweenness(200) YIELD node, score RETURN max(score)",
        "CALL cyber.chokepoints('Entry','Crown') YIELD node, score RETURN count(*)",
        "CALL cyber.fanout('FLOW') YIELD node, robust_z RETURN count(*)",
        "CALL cyber.beaconing('FLOW','ts') YIELD src RETURN count(*)",
        "CALL cyber.volume('FLOW','bytes') YIELD node RETURN count(*)",
        "CALL stats.describe('Host','v') YIELD count RETURN count",
        "CALL stats.spikes('FLOW','ts',60) YIELD bucket_start RETURN count(*)",
    ] {
        let t = Instant::now();
        let res = db.query(q);
        println!("{:>8.0} ms  {}  -> {}", t.elapsed().as_secs_f64() * 1e3, q, match res { Ok(r) => r.rows.first().map(|x| x[0].to_string()).unwrap_or_default(), Err(e) => format!("ERR {e}") });
    }
}
