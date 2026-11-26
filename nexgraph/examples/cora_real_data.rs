//! Real-data check on Cora (Planetoid split, 2708 papers, 7 classes), loaded
//! with the `nbsc` dataset loader and imported into nexgraph.
//!
//!   cargo run --release --example cora_real_data
//!
//! 1. physics.harmonic as label propagation: test accuracy on the standard
//!    Planetoid split (140 labelled training nodes).
//! 2. Community detection on the largest connected component vs the 7 classes
//!    (NMI): spectral.communities (Bethe Hessian) vs spectral.cluster.

use nbsc::dataset::Dataset;
use nexgraph::{props, Database, Value};
use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

fn nmi(a: &[usize], b: &[usize]) -> f64 {
    let n = a.len() as f64;
    let mut ja: HashMap<usize, f64> = HashMap::new();
    let mut jb: HashMap<usize, f64> = HashMap::new();
    let mut jab: HashMap<(usize, usize), f64> = HashMap::new();
    for (&x, &y) in a.iter().zip(b) {
        *ja.entry(x).or_default() += 1.0;
        *jb.entry(y).or_default() += 1.0;
        *jab.entry((x, y)).or_default() += 1.0;
    }
    let h = |m: &HashMap<usize, f64>| -m.values().map(|c| c / n * (c / n).ln()).sum::<f64>();
    let mi: f64 = jab.iter().map(|(&(x, y), c)| c / n * ((c * n) / (ja[&x] * jb[&y])).ln()).sum();
    2.0 * mi / (h(&ja) + h(&jb))
}

fn main() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("data/cora_planetoid");
    let ds = Dataset::load_planetoid_canonical(
        &dir.join("cora_planetoid.content"),
        &dir.join("cora_planetoid.cites"),
        &dir.join("cora_planetoid.train.idx"),
        &dir.join("cora_planetoid.val.idx"),
        &dir.join("cora_planetoid.test.idx"),
    )
    .expect("load Cora");
    let n = ds.graph.n;
    let k = ds.num_classes();
    println!("Cora: {n} nodes, {} edges, {k} classes, train {} / val {} / test {}", ds.graph.m(), ds.train_indices().len(), ds.val_indices().len(), ds.test_indices().len());

    // ---- import
    let db = Database::in_memory();
    let t = Instant::now();
    let mut tx = db.begin();
    tx.create_property_index("Paper", "idx");
    let ids: Vec<_> = (0..n)
        .map(|i| {
            let mut p = props([("idx", Value::Int(i as i64)), ("class", Value::Int(ds.labels[i] as i64))]);
            if ds.train_mask[i] {
                for c in 0..k {
                    p.insert(format!("y{c}"), Value::Float(if ds.labels[i] == c { 1.0 } else { 0.0 }));
                }
            }
            tx.create_node(&["Paper"], p).unwrap()
        })
        .collect();
    for &(u, v) in &ds.graph.edges {
        tx.create_hyperedge("CITES", &[ids[u], ids[v]], &[], 1.0, Default::default()).unwrap();
    }
    tx.commit().unwrap();
    println!("imported in {:.0} ms", t.elapsed().as_secs_f64() * 1e3);

    // ---- 1. label propagation via physics.harmonic
    let t = Instant::now();
    let mut score = vec![vec![f64::NAN; k]; n];
    for c in 0..k {
        let r = db.query(&format!("CALL physics.harmonic('y{c}') YIELD node, value RETURN node.idx, value")).unwrap();
        for row in &r.rows {
            if let Value::Float(v) = row[1] {
                score[row[0].as_i64().unwrap() as usize][c] = v;
            }
        }
    }
    let el = t.elapsed();
    // Unreachable test nodes (no path to a labelled node) have no score and
    // are counted as wrong in the strict figure.
    let (mut right, mut unreachable, mut reach_right) = (0, 0, 0);
    let test = ds.test_indices();
    for &i in &test {
        let s = &score[i];
        if s.iter().any(|v| v.is_nan()) {
            unreachable += 1;
            continue;
        }
        let pred = (0..k).max_by(|&a, &b| s[a].total_cmp(&s[b])).unwrap();
        if pred == ds.labels[i] {
            right += 1;
            reach_right += 1;
        }
    }
    println!(
        "\n[1] harmonic label propagation (7 one-vs-rest solves, {:.0} ms): strict test accuracy {:.1}% ({right}/{}); {unreachable} test nodes are unreachable from any labelled node (counted wrong); accuracy on the {} reachable ones {:.1}%; chance = {:.1}%",
        el.as_secs_f64() * 1e3,
        100.0 * right as f64 / test.len() as f64,
        test.len(),
        test.len() - unreachable,
        100.0 * reach_right as f64 / (test.len() - unreachable) as f64,
        100.0 / k as f64
    );

    // ---- 2. communities on the largest connected component
    let comp = db.query("CALL algo.components() YIELD node, component RETURN node.idx, component").unwrap();
    let mut count: HashMap<i64, usize> = HashMap::new();
    comp.rows.iter().for_each(|r| *count.entry(r[1].as_i64().unwrap()).or_default() += 1);
    let (lcc, size) = count.iter().max_by_key(|(_, &c)| c).map(|(&k, &c)| (k, c)).unwrap();
    println!("\n[2] {} connected components; largest has {size} nodes", count.len());
    let keep: Vec<usize> = comp.rows.iter().filter(|r| r[1].as_i64().unwrap() == lcc).map(|r| r[0].as_i64().unwrap() as usize).collect();
    let mut index_in_lcc = vec![usize::MAX; n];
    keep.iter().enumerate().for_each(|(j, &i)| index_in_lcc[i] = j);
    let db2 = Database::in_memory();
    let mut tx = db2.begin();
    let ids2: Vec<_> = keep.iter().map(|&i| tx.create_node(&["Paper"], props([("idx", Value::Int(i as i64)), ("class", Value::Int(ds.labels[i] as i64))])).unwrap()).collect();
    for &(u, v) in &ds.graph.edges {
        if index_in_lcc[u] != usize::MAX {
            tx.create_hyperedge("CITES", &[ids2[index_in_lcc[u]], ids2[index_in_lcc[v]]], &[], 1.0, Default::default()).unwrap();
        }
    }
    tx.commit().unwrap();
    let truth: Vec<usize> = keep.iter().map(|&i| ds.labels[i]).collect();

    let r = db2.query("CALL spectral.nb_radius() YIELD nodes, edges, mean_degree, mean_excess_degree, rho_b, communities RETURN nodes, edges, mean_degree, mean_excess_degree, rho_b, communities").unwrap();
    println!("    nb_radius: {:?}", r.rows[0].iter().map(|v| v.to_string()).collect::<Vec<_>>());
    for (label, q) in [
        ("Bethe Hessian, auto k  ", "CALL spectral.communities() YIELD node, community RETURN node.idx, community"),
        ("Bethe Hessian, k=7     ", "CALL spectral.communities(7) YIELD node, community RETURN node.idx, community"),
        ("hypergraph Laplacian k=7", "CALL spectral.cluster(7) YIELD node, cluster RETURN node.idx, cluster"),
    ] {
        let t = Instant::now();
        match db2.query(q) {
            Ok(r) => {
                let mut pred = vec![0usize; keep.len()];
                for row in &r.rows {
                    pred[index_in_lcc[row[0].as_i64().unwrap() as usize]] = row[1].as_i64().unwrap() as usize;
                }
                let kk = pred.iter().max().unwrap() + 1;
                println!("    {label}: {kk} communities, NMI vs 7 classes = {:.3}  ({:.1}s)", nmi(&pred, &truth), t.elapsed().as_secs_f64());
            }
            Err(e) => println!("    {label}: {e}"),
        }
    }
}
