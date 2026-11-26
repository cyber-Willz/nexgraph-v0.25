//! The factorised aggregation fast path must give exactly the answers of the streaming path.
//! Each pair of queries below means the same thing; the second carries an extra always-true predicate on `b`
//! that keeps it off the fast path.
use nexgraph::{Database, Value};

fn graph() -> Database {
    let db = Database::in_memory();
    let mut tx = db.begin();
    let mut x = 99u64;
    let mut r = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let ids: Vec<u64> = (0..400)
        .map(|i| {
            let l = if i % 5 == 0 { "M" } else { "N" };
            tx.create_node(&[l], nexgraph::props([("k", Value::Int(i)), ("g", Value::Int(i % 7))])).unwrap()
        })
        .collect();
    for _ in 0..3000 {
        let (a, b) = (r() as usize % 400, r() as usize % 400);
        let lab = if r() % 3 == 0 { "F" } else { "E" };
        tx.create_edge(lab, ids[a], ids[b], 1.0, Default::default()).unwrap();
    }
    tx.commit().unwrap();
    db
}

fn rows(db: &Database, q: &str) -> Vec<Vec<Value>> {
    let mut r = db.query(q).unwrap_or_else(|e| panic!("{q}: {e}")).rows;
    r.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
    r
}

#[test]
fn fast_and_streaming_paths_agree() {
    let db = graph();
    let pairs = [
        ("MATCH (a:N)-[:E]->(b) RETURN count(b)", "MATCH (a:N)-[:E]->(b) WHERE b.k >= 0 RETURN count(b)"),
        ("MATCH (a:N)-[:E]->(b) RETURN count(*)", "MATCH (a:N)-[:E]->(b) WHERE b.k >= 0 RETURN count(*)"),
        ("MATCH (a:N)-[r:E]->(b:M) RETURN count(r)", "MATCH (a:N)-[r:E]->(b:M) WHERE b.k >= 0 RETURN count(r)"),
        ("MATCH (a:N)-[:E]->(b) RETURN a.k AS k, count(b) AS d", "MATCH (a:N)-[:E]->(b) WHERE b.k >= 0 RETURN a.k AS k, count(b) AS d"),
        ("MATCH (a:N)<-[:E]-(b) RETURN a.g AS g, count(*) AS d", "MATCH (a:N)<-[:E]-(b) WHERE b.k >= 0 RETURN a.g AS g, count(*) AS d"),
        // WHERE on the start node only
        ("MATCH (a:N)-[:E]->(b) WHERE a.g = 3 RETURN count(*)", "MATCH (a:N)-[:E]->(b) WHERE a.g = 3 AND b.k >= 0 RETURN count(*)"),
        ("MATCH (a)-[:F]->(b) WHERE a.k < 100 RETURN a.g AS g, count(b) AS d", "MATCH (a)-[:F]->(b) WHERE a.k < 100 AND b.k >= 0 RETURN a.g AS g, count(b) AS d"),
        // a WHERE that is null for some rows
        ("MATCH (a:N)-[:E]->(b) WHERE a.missing > 1 RETURN count(*)", "MATCH (a:N)-[:E]->(b) WHERE a.missing > 1 AND b.k >= 0 RETURN count(*)"),
        // through WITH
        ("MATCH (a:N)-[:E]->(b) WITH a, count(b) AS d WHERE d > 6 RETURN count(*) AS n", "MATCH (a:N)-[:E]->(b) WHERE b.k >= 0 WITH a, count(b) AS d WHERE d > 6 RETURN count(*) AS n"),
        ("MATCH (a:N)-[:E]->(b) WITH a.g AS g, count(b) AS d RETURN g, d", "MATCH (a:N)-[:E]->(b) WHERE b.k >= 0 WITH a.g AS g, count(b) AS d RETURN g, d"),
    ];
    for (fast, slow) in pairs {
        let (f, s) = (rows(&db, fast), rows(&db, slow));
        assert_eq!(f, s, "\n  {fast}\n  {slow}");
        assert!(!f.is_empty(), "{fast}: test graph gives no rows");
    }
    // count(DISTINCT b) is not the same as count(*) and must keep working
    let d = rows(&db, "MATCH (a:N)-[:E]->(b) RETURN count(DISTINCT b) AS n");
    let c = rows(&db, "MATCH (a:N)-[:E]->(b) RETURN count(*) AS n");
    assert_ne!(d, c);
}
