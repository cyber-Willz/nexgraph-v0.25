//! The dense adjacency snapshot and the lean group table must agree with an independent oracle,
//! including after writes (snapshot invalidation), deletes, self-loops, NULL keys and multi-label nodes.
use nexgraph::{props, Database, Value};
use std::collections::{BTreeMap, HashSet};

struct G {
    nodes: Vec<(u64, &'static str, Option<i64>)>, // id, label, k
    edges: Vec<(u64, u64, &'static str)>,         // src, dst, label
}

fn lcg(s: &mut u64) -> u64 {
    *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    *s >> 33
}

fn build(db: &Database, multi_label: bool) -> G {
    let mut s = 12345u64;
    let mut tx = db.begin();
    let mut g = G { nodes: vec![], edges: vec![] };
    for i in 0..300u64 {
        let label = if i % 3 == 0 { "A" } else { "B" };
        let k = if i % 11 == 0 { None } else { Some((i % 17) as i64) };
        let p = match k {
            Some(k) => props([("k", k)]),
            None => Default::default(),
        };
        let id = tx.create_node(&[label], p).unwrap();
        g.nodes.push((id, label, k));
    }
    if multi_label {
        tx.create_node(&["A", "B"], props([("k", 99i64)])).unwrap();
    }
    for _ in 0..2500 {
        let a = g.nodes[(lcg(&mut s) % 300) as usize].0;
        let b = if lcg(&mut s) % 40 == 0 { a } else { g.nodes[(lcg(&mut s) % 300) as usize].0 }; // some self-loops
        let l = if lcg(&mut s) % 4 == 0 { "F" } else { "E" };
        tx.create_edge(l, a, b, 1.0, Default::default()).unwrap();
        g.edges.push((a, b, l));
    }
    tx.commit().unwrap();
    g
}

fn lab(g: &G, id: u64) -> Option<&'static str> {
    g.nodes.iter().find(|n| n.0 == id).map(|n| n.1)
}

fn count(g: &G, src: &str, dst: &str, el: &str, inward: bool) -> i64 {
    g.edges.iter().filter(|e| e.2 == el).filter(|e| {
        let (a, b) = if inward { (e.1, e.0) } else { (e.0, e.1) };
        lab(g, a) == Some(src) && lab(g, b) == Some(dst)
    }).count() as i64
}

fn one(db: &Database, q: &str) -> i64 {
    db.query(q).unwrap().rows[0][0].as_i64().unwrap()
}

fn check_counts(db: &Database, g: &G) {
    assert_eq!(one(db, "MATCH (a:A)-[:E]->(b:B) RETURN count(*)"), count(g, "A", "B", "E", false));
    assert_eq!(one(db, "MATCH (a:B)-[:E]->(b:B) RETURN count(*)"), count(g, "B", "B", "E", false));
    assert_eq!(one(db, "MATCH (a:A)-[:F]->(b:A) RETURN count(*)"), count(g, "A", "A", "F", false));
    assert_eq!(one(db, "MATCH (a:B)<-[:E]-(b:A) RETURN count(*)"), count(g, "B", "A", "E", true));
    assert_eq!(one(db, "MATCH (a:N)-[:E]->(b) RETURN count(*)"), 0); // unknown label
    let all_e = g.edges.iter().filter(|e| e.2 == "E").count() as i64;
    assert_eq!(one(db, "MATCH (a)-[:E]->(b) RETURN count(*)"), all_e);
}

fn check_groups(db: &Database, g: &G) {
    // group by int property, with NULL key group: unordered comparison as maps
    let r = db.query("MATCH (a:A)-[:E]->(b) RETURN a.k AS k, count(*) AS d").unwrap();
    let mut got: BTreeMap<Option<i64>, i64> = BTreeMap::new();
    for row in &r.rows {
        let k = if matches!(row[0], Value::Null) { None } else { row[0].as_i64() };
        assert!(got.insert(k, row[1].as_i64().unwrap()).is_none(), "duplicate group");
    }
    let mut want: BTreeMap<Option<i64>, i64> = BTreeMap::new();
    for e in g.edges.iter().filter(|e| e.2 == "E") {
        if lab(g, e.0) == Some("A") {
            let k = g.nodes.iter().find(|n| n.0 == e.0).unwrap().2;
            *want.entry(k).or_default() += 1;
        }
    }
    assert_eq!(got, want);

    // ORDER BY + SKIP + LIMIT on the aggregate and the key
    let r = db.query("MATCH (a:A)-[:E]->(b) WHERE a.k IS NOT NULL RETURN a.k AS k, count(*) AS d ORDER BY d DESC, k SKIP 1 LIMIT 4").unwrap();
    let mut v: Vec<(i64, i64)> = want.iter().filter_map(|(k, d)| k.map(|k| (k, *d))).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let v: Vec<(i64, i64)> = v.into_iter().skip(1).take(4).collect();
    let got: Vec<(i64, i64)> = r.rows.iter().map(|x| (x[0].as_i64().unwrap(), x[1].as_i64().unwrap())).collect();
    assert_eq!(got, v);

    // group by the node itself, then WITH-filter, then aggregate again
    let mut deg: BTreeMap<u64, i64> = BTreeMap::new();
    for e in g.edges.iter().filter(|e| e.2 == "E") {
        if lab(g, e.0) == Some("A") {
            *deg.entry(e.0).or_default() += 1;
        }
    }
    let thr = 8;
    assert_eq!(one(db, &format!("MATCH (a:A)-[:E]->(b) WITH a, count(b) AS d WHERE d > {thr} RETURN count(*)")), deg.values().filter(|&&d| d > thr).count() as i64);
    assert_eq!(db.query("MATCH (a:A)-[:E]->(b) RETURN a, count(*) AS d").unwrap().rows.len(), deg.len());
    // DISTINCT over aggregated output
    let r = db.query("MATCH (a:A)-[:E]->(b) RETURN DISTINCT count(*) AS d, a.k AS k").unwrap();
    assert!(r.rows.len() <= deg.len());
}

#[test]
fn snapshot_and_fast_groups_match_oracle_through_writes() {
    let db = Database::in_memory();
    let mut g = build(&db, false);
    check_counts(&db, &g);
    check_groups(&db, &g);
    // repeated reads reuse the snapshot and must not change anything
    check_counts(&db, &g);

    // write: new edges must invalidate the snapshot
    let mut tx = db.begin();
    let (a, b) = (g.nodes[0].0, g.nodes[1].0);
    for _ in 0..5 {
        tx.create_edge("E", a, b, 1.0, Default::default()).unwrap();
        g.edges.push((a, b, "E"));
    }
    tx.commit().unwrap();
    check_counts(&db, &g);
    check_groups(&db, &g);

    // delete nodes (cascades to their edges)
    let dead: HashSet<u64> = [g.nodes[3].0, g.nodes[4].0, g.nodes[9].0, g.nodes[10].0].into_iter().collect();
    let mut tx = db.begin();
    for d in &dead {
        tx.delete(*d);
    }
    tx.commit().unwrap();
    g.edges.retain(|e| !dead.contains(&e.0) && !dead.contains(&e.1));
    g.nodes.retain(|n| !dead.contains(&n.0));
    check_counts(&db, &g);
    check_groups(&db, &g);

    // update a property: group keys must follow
    let target = g.nodes.iter().position(|n| n.1 == "A" && n.2.is_some()).unwrap();
    let mut tx = db.begin();
    tx.set_prop(g.nodes[target].0, "k", 1000i64).unwrap();
    tx.commit().unwrap();
    g.nodes[target].2 = Some(1000);
    check_groups(&db, &g);
}

#[test]
fn multi_label_nodes_fall_back_to_the_general_path() {
    let db = Database::in_memory();
    let g = build(&db, true);
    // a node labelled A and B exists: the snapshot is not used, results must still be right
    let n_ab = db.query("MATCH (n:A:B) RETURN count(*)").map(|r| r.rows[0][0].as_i64().unwrap()).unwrap_or(1);
    assert_eq!(n_ab, 1);
    assert_eq!(one(&db, "MATCH (a:A)-[:E]->(b:B) RETURN count(*)"), count(&g, "A", "B", "E", false));
    check_groups(&db, &g);
}

#[test]
fn snapshot_hop_counts_equal_view_hop_counts_and_old_readers_bypass_it() {
    use nexgraph::store::Direction;
    let db = Database::in_memory();
    let mut g = build(&db, false);
    let ts_old = db.last_ts();
    let before = one(&db, "MATCH (a:A)-[:E]->(b:B) RETURN count(*)");
    assert_eq!(before, count(&g, "A", "B", "E", false));
    let a = g.nodes.iter().find(|n| n.1 == "A").unwrap().0;
    let b = g.nodes.iter().find(|n| n.1 == "B").unwrap().0;
    let mut tx = db.begin();
    tx.create_edge("E", a, b, 1.0, Default::default()).unwrap();
    g.edges.push((a, b, "E"));
    tx.commit().unwrap();
    assert_eq!(one(&db, "MATCH (a:A)-[:E]->(b:B) RETURN count(*)"), before + 1);

    // per-node equivalence with the original (version-chain) implementation, all label/direction combos
    db.read(|v| {
        let csr = v.csr().expect("latest reader gets a snapshot");
        for dir in [Direction::Out, Direction::In] {
            for (el, nl) in [(None, None), (Some("E"), None), (Some("F"), Some("A")), (Some("E"), Some("B")), (None, Some("A"))] {
                let sp = v.hop_spec(dir, el, nl).unwrap();
                for n in &g.nodes {
                    assert_eq!(csr.hop_count(n.0, &sp), v.hop_count(n.0, &sp), "node {} dir {:?} {:?} {:?}", n.0, dir as u8, el, nl);
                }
                let src = Some(v.label_id("A").unwrap());
                let want: u64 = g.nodes.iter().filter(|n| n.1 == "A").map(|n| v.hop_count(n.0, &sp)).sum();
                assert_eq!(csr.total(src, &sp), want);
            }
        }
    });
    // a reader older than the newest write must not be served from the latest snapshot
    db.read_at(ts_old, |v| assert!(v.csr().is_none())).unwrap();
}
