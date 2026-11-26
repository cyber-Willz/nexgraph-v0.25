//! The web UI's launcher catalogue (`frontend/src/catalog.rs`, compiled here via `#[path]`) against the real engine:
//! every entry must resolve to a procedure that accepts the arguments the UI sends. The frontend crate cannot depend
//! on nexgraph (spectral_hypergraph pins `indexmap = =2.2.6`, which conflicts with the wasm toolchain), so the
//! check lives here.
#![cfg(all(feature = "spectral", feature = "nonbacktracking"))]

#[path = "../frontend/src/catalog.rs"]
#[allow(dead_code)]
mod catalog;

use catalog::*;
use nexgraph::{Database, Value as V};
use serde_json::Value;


fn to_nx(v: &Value) -> V {
    match v {
        Value::Null => V::Null,
        Value::Bool(b) => V::Bool(*b),
        Value::Number(n) => n.as_i64().map(V::Int).unwrap_or_else(|| V::Float(n.as_f64().unwrap())),
        Value::String(s) => V::Str(s.clone()),
        _ => unreachable!(),
    }
}

fn graph() -> Database {
    let db = Database::in_memory();
    db.query("CREATE (a:P {name:'a', w:1.0}), (b:P {name:'b', w:0.0}), (c:P {name:'c', w:0.0}), (d:P {name:'d', w:0.0}), (a)-[:K]->(b), (b)-[:K]->(c), (c)-[:K]->(a), (c)-[:K]->(d), (d)-[:K]->(a)").unwrap();
    db
}

/// Every launcher entry runs on the real engine: it must parse, resolve a procedure and accept its arguments.
/// GAAP procedures need a ledger, so for those only "unknown procedure / bad arity" would be a failure.
#[test]
fn every_entry_runs_on_the_engine() {
    let db = graph();
    for q in PROCS {
        let mut vals = default_values(q);
        match q.name {
            "physics.heat" => vals[0] = "w".into(),
            n if n.starts_with("gaap.") => {
                for (i, pr) in q.params.iter().enumerate() {
                    if vals[i].is_empty() {
                        vals[i] = if pr.name == "company" { "acme".into() } else if pr.name == "account" { "1100".into() } else { "2025-12-31".into() };
                    }
                }
            }
            _ => {}
        }
        let (stmt, params) = build(q, &vals).unwrap();
        let params: nexgraph::Params = params.iter().map(|(k, v)| (k.clone(), to_nx(v))).collect();
        match db.query_with(&stmt, &params) {
            Ok(r) => assert!(!r.columns.is_empty(), "{}: no columns", q.name),
            Err(e) if q.group == "gaap" => {
                let m = e.to_string();
                assert!(!m.contains("unknown procedure") && !m.to_lowercase().contains("parse"), "{}: {m}", q.name);
            }
            Err(e) => panic!("{} failed: {e}", q.name),
        }
    }
}
