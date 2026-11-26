//! Tests for the pde_shpinn-derived additions: physics.* (exact heat /
//! Dirichlet solves), non-backtracking community detection, nb_radius.

use nexgraph::{props, Database, Id, Value};
use spectral_hypergraph::hypergraph::HypergraphBuilder;
use spectral_hypergraph::laplacian::dense_normalized_laplacian;
use spectral_hypergraph::spectral::dense_eigen;

fn lcg(s: &mut u64) -> f64 {
    *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    (*s >> 11) as f64 / (1u64 << 53) as f64
}

/// Random connected hypergraph with 2-4 member edges, built both in the DB
/// and in `spectral_hypergraph` (identical vertex order and weights).
fn random_hypergraph(n: usize, m: usize, seed: u64) -> (Database, Vec<Id>, spectral_hypergraph::SpectralHypergraph, Vec<(Vec<usize>, f64)>) {
    let mut s = seed;
    let db = Database::in_memory();
    let mut tx = db.begin();
    let ids: Vec<Id> = (0..n).map(|i| tx.create_node(&["V"], props([("i", i as i64)])).unwrap()).collect();
    let mut edges: Vec<(Vec<usize>, f64)> = Vec::new();
    for i in 1..n {
        edges.push((vec![i - 1, i], 1.0)); // spine keeps it connected
    }
    while edges.len() < m {
        let k = 2 + (lcg(&mut s) * 3.0) as usize;
        let mut mem: Vec<usize> = (0..k).map(|_| (lcg(&mut s) * n as f64) as usize % n).collect();
        mem.sort_unstable();
        mem.dedup();
        if mem.len() >= 2 {
            edges.push((mem, 0.5 + lcg(&mut s)));
        }
    }
    let mut b = HypergraphBuilder::new();
    let vs: Vec<_> = (0..n).map(|i| b.add_vertex(i.to_string()).unwrap()).collect();
    for (mem, w) in &edges {
        let nodes: Vec<Id> = mem.iter().map(|&i| ids[i]).collect();
        tx.create_hyperedge("H", &nodes, &[], *w, Default::default()).unwrap();
        b.add_hyperedge(&mem.iter().map(|&i| vs[i]).collect::<Vec<_>>(), *w).unwrap();
    }
    tx.commit().unwrap();
    (db, ids, b.build().unwrap(), edges)
}

#[test]
fn heat_sym_matches_dense_matrix_exponential_of_the_crates_laplacian() {
    let (db, ids, hg, _) = random_hypergraph(40, 70, 11);
    let l = dense_normalized_laplacian(&hg).unwrap();
    let eig = dense_eigen(&l);
    let n = ids.len();
    for (t, seed) in [(0.0, 1u64), (0.3, 2), (2.5, 3), (40.0, 4)] {
        let mut s = seed;
        let u0: Vec<f64> = (0..n).map(|_| lcg(&mut s) - 0.5).collect();
        // reference: V exp(-t Lambda) V^T u0
        let v = &eig.eigenvectors;
        let mut want = vec![0.0; n];
        for k in 0..n {
            let c: f64 = (0..n).map(|i| v[(i, k)] * u0[i]).sum::<f64>() * (-t * eig.eigenvalues[k]).exp();
            for i in 0..n {
                want[i] += c * v[(i, k)];
            }
        }
        // DB: set the property, call the procedure
        let mut tx = db.begin();
        for (i, &id) in ids.iter().enumerate() {
            tx.set_prop(id, "u0", Value::Float(u0[i])).unwrap();
        }
        tx.commit().unwrap();
        let r = db.query(&format!("CALL physics.heat('u0', {t}, 'sym') YIELD node, value RETURN id(node), value ORDER BY id(node)")).unwrap();
        let got: Vec<f64> = r.rows.iter().map(|x| x[1].as_f64().unwrap()).collect();
        let err = got.iter().zip(&want).map(|(a, b)| (a - b).abs()).fold(0.0, f64::max);
        assert!(err < 1e-9, "t={t}: max error {err:e}");
    }
}

#[test]
fn heat_rw_preserves_constants_and_is_an_averaging_operator() {
    let (db, ids, _, _) = random_hypergraph(30, 50, 5);
    let mut tx = db.begin();
    for &id in &ids {
        tx.set_prop(id, "c", Value::Float(3.25)).unwrap();
    }
    tx.commit().unwrap();
    let r = db.query("CALL physics.heat('c', 7.0) YIELD value RETURN min(value), max(value)").unwrap();
    assert!((r.rows[0][0].as_f64().unwrap() - 3.25).abs() < 1e-12);
    assert!((r.rows[0][1].as_f64().unwrap() - 3.25).abs() < 1e-12);
    // maximum principle: values stay inside [min, max] of the initial field
    let mut tx = db.begin();
    for (i, &id) in ids.iter().enumerate() {
        tx.set_prop(id, "x", Value::Float((i % 7) as f64)).unwrap();
    }
    tx.commit().unwrap();
    let r = db.query("CALL physics.heat('x', 1.3) YIELD value RETURN min(value), max(value)").unwrap();
    assert!(r.rows[0][0].as_f64().unwrap() >= -1e-12 && r.rows[0][1].as_f64().unwrap() <= 6.0 + 1e-12);
    // heat_from: seed mass spreads but stays largest at the seed for small t
    let r = db.query("MATCH (s:V {i:0}) CALL physics.heat_from([s], 0.2) YIELD node, value RETURN id(node), value ORDER BY value DESC LIMIT 1").unwrap();
    assert_eq!(r.rows[0][0], Value::Int(ids[0] as i64));
}

#[test]
fn harmonic_on_a_path_is_linear() {
    let db = Database::in_memory();
    db.query("CREATE (a:P {i:0, v:0.0})").unwrap();
    for i in 1..10 {
        db.query(&format!("MATCH (p:P {{i:{}}}) CREATE (p)-[:E]->(q:P {{i:{i}}})", i - 1)).unwrap();
    }
    db.query("MATCH (p:P {i:9}) SET p.v = 9.0").unwrap();
    let r = db.query("CALL physics.harmonic('v') YIELD node, value, observed RETURN node.i, value, observed ORDER BY node.i").unwrap();
    for row in &r.rows {
        let i = row[0].as_i64().unwrap() as f64;
        assert!((row[1].as_f64().unwrap() - i).abs() < 1e-7, "node {i}: {:?}", row);
        assert_eq!(row[2], Value::Bool(i == 0.0 || i == 9.0));
    }
}

#[test]
fn harmonic_matches_direct_linear_solve_on_hypergraph() {
    let (db, ids, _, edges) = random_hypergraph(35, 60, 21);
    let n = ids.len();
    // P = Dv^-1 H W De^-1 H^T
    let mut dv = vec![0.0; n];
    let mut p = nalgebra::DMatrix::<f64>::zeros(n, n);
    for (m, w) in &edges {
        for &i in m {
            dv[i] += w;
            for &j in m {
                p[(i, j)] += w / m.len() as f64;
            }
        }
    }
    for i in 0..n {
        for j in 0..n {
            p[(i, j)] /= dv[i];
        }
    }
    let known: Vec<usize> = vec![0, 7, 19, 30];
    let vals = [1.0, -2.0, 0.5, 4.0];
    let unk: Vec<usize> = (0..n).filter(|i| !known.contains(i)).collect();
    let a = nalgebra::DMatrix::from_fn(unk.len(), unk.len(), |r, c| (if r == c { 1.0 } else { 0.0 }) - p[(unk[r], unk[c])]);
    let rhs = nalgebra::DVector::from_fn(unk.len(), |r, _| known.iter().zip(vals).map(|(&k, v)| p[(unk[r], k)] * v).sum::<f64>());
    let sol = a.lu().solve(&rhs).unwrap();
    let mut tx = db.begin();
    for (&k, v) in known.iter().zip(vals) {
        tx.set_prop(ids[k], "v", Value::Float(v)).unwrap();
    }
    tx.commit().unwrap();
    let r = db.query("CALL physics.harmonic('v') YIELD node, value, observed RETURN node.i, value ORDER BY node.i").unwrap();
    for (row, i) in unk.iter().enumerate().map(|(r, &i)| (r, i)) {
        let got = r.rows[i][1].as_f64().unwrap();
        assert!((got - sol[row]).abs() < 1e-7, "node {i}: {got} vs {}", sol[row]);
    }
}

#[test]
fn harmonic_marks_unreachable_nodes_null_and_errors_without_data() {
    let db = Database::in_memory();
    db.query("CREATE (a:V {v:1.0}), (b:V), (a)-[:E]->(b), (c:V), (d:V), (c)-[:E]->(d), (z:V)").unwrap();
    let r = db.query("CALL physics.harmonic('v') YIELD node, value RETURN count(value), count(*)").unwrap();
    assert_eq!(r.rows[0], vec![Value::Int(2), Value::Int(5)]);
    assert!(db.query("CALL physics.harmonic('nope') YIELD node RETURN node").is_err());
    assert!(db.query("CALL physics.heat('v', -1) YIELD node RETURN node").is_err());
    assert!(db.query("CALL physics.heat('v', 1, 'bogus') YIELD node RETURN node").is_err());
}

fn sbm(db: &Database, blocks: usize, size: usize, c_in: f64, c_out: f64, seed: u64) -> Vec<usize> {
    let n = blocks * size;
    let (p_in, p_out) = (c_in / size as f64, c_out / ((blocks - 1) * size) as f64);
    let mut s = seed;
    let mut tx = db.begin();
    let ids: Vec<Id> = (0..n).map(|i| tx.create_node(&["V"], props([("block", (i / size) as i64)])).unwrap()).collect();
    for u in 0..n {
        for v in (u + 1)..n {
            let p = if u / size == v / size { p_in } else { p_out };
            if lcg(&mut s) < p {
                tx.create_edge("E", ids[u], ids[v], 1.0, Default::default()).unwrap();
            }
        }
    }
    tx.commit().unwrap();
    (0..n).map(|i| i / size).collect()
}

fn accuracy(pred: &[(Id, i64)], truth_of: impl Fn(Id) -> usize, k: usize) -> f64 {
    // best accuracy over the k! label permutations (k <= 4 here)
    fn perms(k: usize) -> Vec<Vec<usize>> {
        if k == 1 {
            return vec![vec![0]];
        }
        let mut out = Vec::new();
        for p in perms(k - 1) {
            for pos in 0..k {
                let mut q = p.clone();
                q.insert(pos, k - 1);
                out.push(q);
            }
        }
        out
    }
    perms(k)
        .iter()
        .map(|perm| pred.iter().filter(|(id, c)| perm[*c as usize % k] == truth_of(*id)).count() as f64 / pred.len() as f64)
        .fold(0.0, f64::max)
}

#[test]
fn bethe_hessian_recovers_sparse_planted_partition_and_reports_laplacian_for_comparison() {
    // two blocks, mean degree 3, c_in - c_out = 4 > 2*sqrt(3): detectable but sparse
    let mut bh = Vec::new();
    let mut lap = Vec::new();
    for seed in 1..=5u64 {
        let db = Database::in_memory();
        let truth = sbm(&db, 2, 400, 5.0, 1.0, seed);
        let node_block = |id: Id| truth[(id - 1) as usize];
        let rows = |q: &str| -> Vec<(Id, i64)> {
            db.query(q).unwrap().rows.iter().map(|r| (if let Value::Node(i) = r[0] { i } else { unreachable!() }, r[1].as_i64().unwrap())).collect()
        };
        let a = rows("CALL spectral.communities(2) YIELD node, community RETURN node, community");
        let b = rows("CALL spectral.cluster(2) YIELD node, cluster RETURN node, cluster");
        bh.push(accuracy(&a, &node_block, 2));
        lap.push(accuracy(&b, &node_block, 2));
    }
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
    println!("sparse SBM (n=800, mean degree 3): Bethe Hessian accuracy {:?} (mean {:.3}); hypergraph-Laplacian cluster {:?} (mean {:.3})", bh, mean(&bh), lap, mean(&lap));
    assert!(mean(&bh) > 0.8, "Bethe Hessian should detect this partition: {bh:?}");
}

#[test]
fn communities_auto_k_counts_blocks_and_trees_have_none() {
    let db = Database::in_memory();
    let truth = sbm(&db, 3, 300, 9.0, 1.0, 3);
    let r = db.query("CALL spectral.nb_radius(true) YIELD communities, rho_b, mean_excess_degree RETURN communities, rho_b, mean_excess_degree").unwrap();
    let k = r.rows[0][0].as_i64().unwrap();
    println!("auto k = {k} (3 planted), rho_b = {}, excess degree = {}", r.rows[0][1], r.rows[0][2]);
    assert_eq!(k, 3);
    let _ = truth;
    // a path (tree) has rho_B = 0: reported as a single community, not an error
    let db = Database::in_memory();
    db.query("CREATE (a:V)-[:E]->(b:V)-[:E]->(c:V)-[:E]->(d:V)").unwrap();
    let r = db.query("CALL spectral.communities() YIELD community RETURN count(DISTINCT community)").unwrap();
    assert_eq!(r.rows[0][0], Value::Int(1));
}

#[test]
fn nb_radius_of_a_random_regular_graph_is_degree_minus_one() {
    let g = nbsc::graph::random_near_regular(400, 4, 9);
    let db = Database::in_memory();
    let mut tx = db.begin();
    let ids: Vec<Id> = (0..g.n).map(|_| tx.create_node(&["V"], Default::default()).unwrap()).collect();
    for &(u, v) in &g.edges {
        tx.create_edge("E", ids[u], ids[v], 1.0, Default::default()).unwrap();
    }
    tx.commit().unwrap();
    let r = db.query("CALL spectral.nb_radius() YIELD rho_b, rho_b_from_arnoldi RETURN rho_b, rho_b_from_arnoldi").unwrap();
    let rho = r.rows[0][0].as_f64().unwrap();
    println!("random near-4-regular: rho_B = {rho:.4} (theory for exactly 4-regular: 3)");
    assert!((rho - 3.0).abs() < 0.25, "rho_B {rho}");
    assert_eq!(r.rows[0][1], Value::Bool(true));
}

#[test]
fn hyperedges_participate_in_communities() {
    // two groups of hyperedges (triples) joined by a single bridge hyperedge
    let db = Database::in_memory();
    let mut tx = db.begin();
    let ids: Vec<Id> = (0..60).map(|i| tx.create_node(&["V"], props([("g", (i / 30) as i64)])).unwrap()).collect();
    let mut s = 99u64;
    for g in 0..2 {
        for _ in 0..70 {
            let mut m: Vec<Id> = (0..3).map(|_| ids[g * 30 + (lcg(&mut s) * 30.0) as usize % 30]).collect();
            m.sort_unstable();
            m.dedup();
            if m.len() >= 2 {
                tx.create_hyperedge("H", &m, &[], 1.0, Default::default()).unwrap();
            }
        }
    }
    tx.create_hyperedge("H", &[ids[0], ids[30]], &[], 1.0, Default::default()).unwrap();
    tx.commit().unwrap();
    let r = db.query("CALL spectral.communities(2) YIELD node, community RETURN node.g, community, count(*) AS n").unwrap();
    // each planted group falls (almost) entirely into one community
    let mut by_group = std::collections::HashMap::<i64, std::collections::HashMap<i64, i64>>::new();
    for row in &r.rows {
        *by_group.entry(row[0].as_i64().unwrap()).or_default().entry(row[1].as_i64().unwrap()).or_default() += row[2].as_i64().unwrap();
    }
    for (g, m) in &by_group {
        let total: i64 = m.values().sum();
        let top = m.values().max().unwrap();
        assert!(*top as f64 / total as f64 > 0.9, "group {g} split: {m:?}");
    }
}

/// Informational sweep toward the detectability threshold (gap = 2*sqrt(c)*margin).
/// Prints mean accuracy over 6 seeds for both methods; asserts nothing about who wins.
#[test]
fn sparse_sweep_bethe_hessian_vs_laplacian_near_threshold() {
    for (c, margin) in [(3.0, 1.5), (2.0, 1.2), (2.0, 1.0), (1.5, 1.1)] {
        let gap = 2.0 * f64::sqrt(c) * margin;
        let (c_in, c_out) = (c + gap / 2.0, c - gap / 2.0);
        if c_out < 0.0 {
            continue;
        }
        let (mut bh, mut lap) = (0.0, 0.0);
        let seeds = 6;
        for seed in 1..=seeds {
            let db = Database::in_memory();
            let truth = sbm(&db, 2, 500, c_in, c_out, 100 + seed);
            let node_block = |id: Id| truth[(id - 1) as usize];
            let rows = |q: &str| -> Vec<(Id, i64)> {
                db.query(q).unwrap().rows.iter().map(|r| (if let Value::Node(i) = r[0] { i } else { unreachable!() }, r[1].as_i64().unwrap())).collect()
            };
            // restrict both to the giant component for a fair comparison
            bh += accuracy(&rows("CALL spectral.communities(2) YIELD node, community RETURN node, community"), &node_block, 2);
            lap += accuracy(&rows("CALL spectral.cluster(2) YIELD node, cluster RETURN node, cluster"), &node_block, 2);
        }
        println!("SWEEP c={c} c_in={c_in:.2} c_out={c_out:.2} margin={margin}: Bethe {:.3}  Laplacian {:.3}", bh / seeds as f64, lap / seeds as f64);
        if c == 2.0 && margin == 1.2 {
            assert!(bh / seeds as f64 > 0.9, "Bethe Hessian should still detect at margin 1.2");
        }
    }
}

/// Regression: found on real data (Cora, 2485-node component). v0.1's
/// spectral.cluster handed the crate's 30-step Lanczos run to graphs above
/// the dense limit and failed to converge. Now Lanczos gets more steps and
/// falls back to the dense solver.
#[test]
fn spectral_cluster_works_above_the_dense_limit() {
    let db = Database::in_memory();
    let truth = sbm(&db, 2, 800, 10.0, 1.0, 7); // 1600 nodes > 1500
    let r = db.query("CALL spectral.cluster(2) YIELD node, cluster RETURN node, cluster").unwrap();
    let pred: Vec<(Id, i64)> = r.rows.iter().map(|x| (if let Value::Node(i) = x[0] { i } else { 0 }, x[1].as_i64().unwrap())).collect();
    assert!(pred.len() > 1500);
    let acc = accuracy(&pred, |id| truth[(id - 1) as usize], 2);
    assert!(acc > 0.9, "accuracy {acc}");
    let r = db.query("CALL spectral.eigenvalues(3) YIELD eigenvalue RETURN eigenvalue").unwrap();
    assert!(r.rows[0][0].as_f64().unwrap().abs() < 1e-5);
}
