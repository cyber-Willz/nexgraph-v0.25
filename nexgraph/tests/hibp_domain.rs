//! Domain checks on the HIBP breach dataset: NQL aggregates vs values computed independently in Python.
//! Needs NG_HIBP_NODES, NG_HIBP_EDGES (graph CSVs) and NG_HIBP_EXPECTED (json); skipped otherwise.
#![cfg(feature = "ingest")]
use nexgraph::ingest::{import_csv_edges, import_csv_nodes, EdgeCsv};
use nexgraph::{Database, Value};

fn env(k: &str) -> Option<String> { std::env::var(k).ok() }

#[test]
fn hibp_aggregates_match_independent_computation() {
    let (Some(n), Some(e), Some(x)) = (env("NG_HIBP_NODES"), env("NG_HIBP_EDGES"), env("NG_HIBP_EXPECTED")) else { eprintln!("skipped"); return };
    let exp: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(x).unwrap()).unwrap();
    let db = Database::in_memory();
    import_csv_nodes(&db, "Entity", &std::fs::read_to_string(n).unwrap(), Some("id")).unwrap();
    import_csv_edges(&db, &EdgeCsv { edge_label: "EXPOSES", src_col: "breach", dst_col: "dataclass", node_label: "Entity", node_key: "id", weight_col: None }, &std::fs::read_to_string(e).unwrap()).unwrap();
    let q = |t: &str| db.query(t).unwrap_or_else(|e| panic!("{t}: {e}")).rows;
    let i = |t: &str| q(t)[0][0].as_i64().unwrap();
    let ex = |k: &str| exp[k].as_i64().unwrap();
    assert_eq!(i("MATCH (b:Entity) WHERE b.kind = 'breach' RETURN sum(b.pwn_count)"), ex("pwn_sum"));
    assert_eq!(i("MATCH (b:Entity) WHERE b.kind = 'breach' RETURN max(b.pwn_count)"), ex("max_pwn"));
    assert_eq!(i("MATCH (b:Entity) WHERE b.verified = true RETURN count(*)"), ex("verified"));
    assert_eq!(i("MATCH (b:Entity) WHERE b.sensitive = true RETURN count(*)"), ex("sensitive"));
    assert_eq!(i("MATCH (b:Entity) WHERE b.breach_date >= '2016-01-01' AND b.breach_date <= '2016-12-31' RETURN count(*)"), ex("y2016"));
    // co-exposure: breaches exposing both Passwords and Email addresses
    assert_eq!(i("MATCH (b:Entity)-[:EXPOSES]->(p:Entity), (b)-[:EXPOSES]->(m:Entity) WHERE p.id = 'D:Passwords' AND m.id = 'D:Email addresses' RETURN count(DISTINCT b)"), ex("pw_and_email"));
    // top data classes by number of breaches
    let top = q("MATCH (b:Entity)-[:EXPOSES]->(d:Entity) RETURN d.id AS d, count(*) AS n ORDER BY n DESC, d ASC LIMIT 5");
    for (row, want) in top.iter().zip(exp["top5"].as_array().unwrap()) {
        assert_eq!(row[0], Value::Str(format!("D:{}", want[0].as_str().unwrap())));
        assert_eq!(row[1].as_i64().unwrap(), want[1].as_i64().unwrap());
    }
    // every breach exposes at least one class (NQL has no pattern predicates, so count the complement)
    assert_eq!(i("MATCH (b:Entity)-[:EXPOSES]->(d:Entity) WHERE b.kind = 'breach' RETURN count(DISTINCT b)"), 777 - ex("no_class"));
    println!("HIBP aggregates OK; top5 = {:?}", top.iter().map(|r| (r[0].clone(), r[1].clone())).collect::<Vec<_>>());
}
