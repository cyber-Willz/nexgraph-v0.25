//! Load a human_connectome_rnn `Connectome` into nexgraph and query it.
//!
//!   cargo run --release --features connectome --example import_connectome [-- <tvb66-dir>]
//!
//! Without an argument the crate's synthetic human-like connectome is used
//! (the real TVB-66 download is not bundled).

use hcrnn::Connectome;
use nexgraph::{props, Database, Metric, Value};
use std::collections::HashMap;

fn main() {
    let conn = match std::env::args().nth(1) {
        Some(dir) => Connectome::from_tvb_dir(std::path::Path::new(&dir)).expect("load TVB dir"),
        None => Connectome::synthetic_human(33, 0.25, 1).expect("synthetic"),
    };
    println!("{}", conn.summary());
    let db = Database::in_memory();

    // ---- import: regions -> nodes (position as a vector), tracts -> edges
    let mut tx = db.begin();
    tx.create_property_index("Region", "name");
    tx.create_vector_index("Region", "pos", 3, Metric::L2);
    let ids: Vec<_> = conn
        .regions
        .iter()
        .map(|r| {
            tx.create_node(
                &["Region"],
                props([
                    ("name", Value::Str(r.name.clone())),
                    ("hemi", Value::Str(r.hemi.to_string())),
                    ("system", Value::Str(r.system.clone())),
                    ("pos", Value::Vector(r.pos.iter().map(|x| *x as f32).collect())),
                ]),
            )
            .unwrap()
        })
        .collect();
    let n = conn.n();
    let mut edges = 0;
    for i in 0..n {
        for j in (i + 1)..n {
            let w = conn.weight(i, j);
            if w > 0.0 {
                // undirected tract: a 2-member hyperedge with an empty head
                tx.create_hyperedge("TRACT", &[ids[i], ids[j]], &[], w, props([("length_mm", conn.length(i, j))])).unwrap();
                edges += 1;
            }
        }
    }
    // functional systems as genuine hyperedges (impossible in a plain graph)
    let mut by_sys: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, r) in conn.regions.iter().enumerate() {
        by_sys.entry(r.system.as_str()).or_default().push(i);
    }
    for (sys, members) in &by_sys {
        if members.len() >= 2 && *sys != "Unassigned" {
            let m: Vec<_> = members.iter().map(|&i| ids[i]).collect();
            tx.create_hyperedge("SYSTEM", &m, &[], 0.0001, props([("name", Value::Str(sys.to_string()))])).unwrap();
        }
    }
    tx.commit().unwrap();
    println!("imported {n} regions, {edges} tracts");
    assert_eq!(db.stats().live_nodes, n);

    // ---- cross-check degree and strength against the source crate
    let r = db.query("MATCH (a:Region)-[t:TRACT]-(b) RETURN a.name AS name, count(b) AS deg, sum(t.weight) AS strength").unwrap();
    let mut max_err: f64 = 0.0;
    for row in &r.rows {
        let name = row[0].as_str().unwrap();
        let i = conn.regions.iter().position(|x| x.name == name).unwrap();
        assert_eq!(row[1], Value::Int(conn.degree(i) as i64), "degree mismatch for {name}");
        max_err = max_err.max((row[2].as_f64().unwrap() - conn.strength(i)).abs());
    }
    println!("degree matches source for all {n} regions; max strength error {max_err:.2e}");

    // ---- hubs
    let r = db.query("MATCH (a:Region)-[t:TRACT]-(b) RETURN a.name, a.system, count(b) AS degree, sum(t.weight) AS strength ORDER BY strength DESC LIMIT 5").unwrap();
    println!("\ntop-5 hubs by strength\n{}", r.to_table());

    // ---- spectral clustering (k = 2) vs hemisphere labels
    let r = db.query("CALL spectral.cluster(2) YIELD node, cluster RETURN node.hemi AS hemi, cluster, count(*) AS n ORDER BY hemi, cluster").unwrap();
    println!("\nspectral.cluster(2) vs hemisphere\n{}", r.to_table());

    // ---- Laplacian spectrum
    let r = db.query("CALL spectral.eigenvalues(5) YIELD index, eigenvalue RETURN index, eigenvalue").unwrap();
    println!("\nsmallest normalised-Laplacian eigenvalues\n{}", r.to_table());

    // ---- vector index vs exact on region positions
    let q = &conn.regions[0].pos;
    let q: Vec<f32> = q.iter().map(|x| *x as f32).collect();
    let (approx, exact) = db.read(|v| (v.vector_search("Region", "pos", &q, 5, 64, false).unwrap(), v.vector_search("Region", "pos", &q, 5, 64, true).unwrap()));
    println!("\n5 nearest regions to {} (HNSW == exact: {})", conn.regions[0].name, approx.iter().map(|x| x.0).collect::<Vec<_>>() == exact.iter().map(|x| x.0).collect::<Vec<_>>());

    // ---- time travel: prune weak tracts, then look back
    let before = db.last_ts();
    db.query("MATCH ()-[t:TRACT]-() WHERE t.weight < 0.05 SET t.weight = 0.0").unwrap();
    let now = db.query("MATCH ()-[t:TRACT]-() WHERE t.weight > 0.0 RETURN count(*)").unwrap().rows[0][0].clone();
    let then = db.query(&format!("AS OF {before} MATCH ()-[t:TRACT]-() WHERE t.weight > 0.0 RETURN count(*)")).unwrap().rows[0][0].clone();
    println!("\nnon-zero tract endpoints now: {now}, as of ts {before}: {then}");
}
