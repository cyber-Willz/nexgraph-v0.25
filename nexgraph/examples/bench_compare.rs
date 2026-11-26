//! Cross-system benchmark driver: cargo run --release --example bench_compare -- <data-dir> <out.json>
//! Loads edges.csv / nodes.csv and runs the same workloads as the Python drivers in ../bench.
use nexgraph::{props, Database, Value};
use std::fmt::Write as _;
use std::time::Instant;

fn ids(dir: &str, name: &str) -> Vec<i64> {
    std::fs::read_to_string(format!("{dir}/{name}.txt")).unwrap().split_whitespace().map(|x| x.parse().unwrap()).collect()
}

fn summarize(name: &str, mut lat: Vec<f64>, check: String, out: &mut String) {
    let total: f64 = lat.iter().sum::<f64>() / 1e3;
    lat.sort_by(|a, b| a.total_cmp(b));
    let med = lat[lat.len() / 2];
    let p95 = lat[((lat.len() - 1) as f64 * 0.95) as usize];
    println!("nexgraph   {name:8} n={:5} median={med:12.1}us p95={p95:12.1}us", lat.len());
    if !out.is_empty() {
        out.push_str(",\n");
    }
    write!(out, "{{\"workload\":\"{name}\",\"n\":{},\"median_us\":{med},\"p95_us\":{p95},\"total_ms\":{total},\"checksum\":{check}}}", lat.len()).unwrap();
}

fn run(db: &Database, name: &str, args: &[i64], q: impl Fn(i64) -> String, warm: usize, out: &mut String) {
    for &a in args.iter().take(warm) {
        db.query(&q(a)).unwrap();
    }
    let (mut lat, mut res) = (vec![], vec![]);
    for &a in args {
        let s = q(a);
        let t = Instant::now();
        let r = db.query(&s).unwrap();
        lat.push(t.elapsed().as_secs_f64() * 1e6);
        res.push(match r.rows.first().and_then(|x| x.first()) {
            Some(Value::Int(i)) => *i,
            _ => -1,
        });
    }
    summarize(name, lat, format!("{res:?}"), out);
}

fn main() {
    let dir = std::env::args().nth(1).expect("data dir");
    let outp = std::env::args().nth(2).expect("out json");
    let mut rows = String::new();
    let db = Database::in_memory();
    let t = Instant::now();
    let mut tx = db.begin();
    tx.create_property_index("N", "k");
    let nodes: Vec<u64> = std::fs::read_to_string(format!("{dir}/nodes.csv")).unwrap().lines().skip(1).map(|l| tx.create_node(&["N"], props([("k", l.parse::<i64>().unwrap())])).unwrap()).collect();
    for l in std::fs::read_to_string(format!("{dir}/edges.csv")).unwrap().lines().skip(1) {
        let (a, b) = l.split_once(',').unwrap();
        tx.create_edge("E", nodes[a.parse::<usize>().unwrap()], nodes[b.parse::<usize>().unwrap()], 1.0, Default::default()).unwrap();
    }
    tx.commit().unwrap();
    let load_ms = t.elapsed().as_secs_f64() * 1e3;
    println!("nexgraph   load {load_ms:.0} ms");

    // per-call floor: parsing + planning + executing a constant
    let mut lat = vec![];
    for _ in 0..2000 {
        let t = Instant::now();
        db.query("RETURN 1").unwrap();
        lat.push(t.elapsed().as_secs_f64() * 1e6);
    }
    summarize("noop", lat, "[]".into(), &mut rows);
    run(&db, "point", &ids(&dir, "point"), |k| format!("MATCH (a:N {{k: {k}}}) RETURN a.k"), 30, &mut rows);
    run(&db, "hop1", &ids(&dir, "hop1"), |k| format!("MATCH (a:N {{k: {k}}})-[:E]->(b) RETURN count(b)"), 30, &mut rows);
    run(&db, "hop2", &ids(&dir, "hop2"), |k| format!("MATCH (a:N {{k: {k}}})-[:E]->(b)-[:E]->(c) RETURN count(*)"), 30, &mut rows);
    run(&db, "reach3", &ids(&dir, "reach3"), |k| format!("MATCH (a:N {{k: {k}}})-[:E*1..3]->(b) RETURN count(DISTINCT b)"), 10, &mut rows);

    // full scans (3 runs)
    let mut lat = vec![];
    let mut res = vec![];
    for _ in 0..3 {
        let t = Instant::now();
        let r = db.query("MATCH (a:N)-[:E]->(b) RETURN count(*)").unwrap();
        lat.push(t.elapsed().as_secs_f64() * 1e6);
        res.push(r.rows[0][0].as_i64().unwrap());
    }
    summarize("scan", lat, format!("{:?}", &res[..1]), &mut rows);
    let mut lat = vec![];
    let mut top = String::new();
    for _ in 0..3 {
        let t = Instant::now();
        let r = db.query("MATCH (a:N)-[:E]->(b) RETURN a.k AS k, count(*) AS d ORDER BY d DESC, k LIMIT 5").unwrap();
        lat.push(t.elapsed().as_secs_f64() * 1e6);
        top = format!("{:?}", r.rows.iter().map(|x| vec![x[0].as_i64().unwrap(), x[1].as_i64().unwrap()]).collect::<Vec<_>>());
    }
    summarize("degree", lat, top, &mut rows);

    // WITH + aggregate + filter (openCypher pipeline)
    let mut lat = vec![];
    let mut res = vec![];
    for _ in 0..3 {
        let t = Instant::now();
        let r = db.query("MATCH (a:N)-[:E]->(b) WITH a, count(b) AS d WHERE d > 12 RETURN count(*)").unwrap();
        lat.push(t.elapsed().as_secs_f64() * 1e6);
        res.push(r.rows[0][0].as_i64().unwrap());
    }
    summarize("withagg", lat, format!("{:?}", &res[..1]), &mut rows);

    // shortest paths (BFS)
    let pairs: Vec<(i64, i64)> = std::fs::read_to_string(format!("{dir}/pairs.txt")).unwrap().lines().map(|l| { let (a, b) = l.split_once(' ').unwrap(); (a.parse().unwrap(), b.parse().unwrap()) }).collect();
    let (mut lat, mut res) = (vec![], vec![]);
    for (i, &(a, b)) in pairs.iter().enumerate() {
        let q = format!("MATCH (a:N {{k:{a}}}), (b:N {{k:{b}}}) CALL algo.shortestPath(a, b) YIELD length RETURN length");
        let t = Instant::now();
        let r = db.query(&q).unwrap();
        if i >= 5 {
            lat.push(t.elapsed().as_secs_f64() * 1e6);
            res.push(r.rows.first().map_or(-1, |x| x[0].as_i64().unwrap()));
        }
    }
    summarize("path", lat, format!("{res:?}"), &mut rows);

    // pagerank: 30 iterations, damping .85 -> top-10 node keys
    let t = Instant::now();
    let r = db.query("CALL algo.pagerank(0.85, 30) YIELD node, score RETURN node.k AS k, score ORDER BY score DESC, k LIMIT 10").unwrap();
    let pr_ms = t.elapsed().as_secs_f64() * 1e3;
    println!("nexgraph   pagerank {pr_ms:.0} ms");
    let top: Vec<i64> = r.rows.iter().map(|x| x[0].as_i64().unwrap()).collect();
    std::fs::write(&outp, format!("{{\"system\":\"nexgraph\",\"load_ms\":{load_ms},\"pagerank_ms\":{pr_ms},\"pagerank_top10\":{top:?},\"rows\":[\n{rows}\n]}}")).unwrap();
}
