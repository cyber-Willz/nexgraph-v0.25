//! Runs a corpus of standard openCypher queries against a small movie/people graph and reports which work.
//! Corpus: tests/cypher/corpus.txt. Set NEXGRAPH_GAP_REPORT=1 to print failures.
use nexgraph::Database;

fn graph() -> Database {
    let db = Database::in_memory();
    for q in [
        "CREATE (:Person {name:'Ada', age:41}), (:Person {name:'Bo', age:29}), (:Person {name:'Cy', age:35}), (:Person {name:'Di', age:52})",
        "CREATE (:Movie {title:'Matrix'}), (:Movie {title:'Speed'})",
        "MATCH (a:Person {name:'Ada'}), (m:Movie {title:'Matrix'}) CREATE (a)-[:ACTED_IN {role:'Neo'}]->(m)",
        "MATCH (a:Person {name:'Bo'}), (m:Movie {title:'Matrix'}) CREATE (a)-[:ACTED_IN {role:'Trin'}]->(m)",
        "MATCH (a:Person {name:'Cy'}), (m:Movie {title:'Speed'}) CREATE (a)-[:ACTED_IN {role:'Jack'}]->(m)",
        "MATCH (a:Person {name:'Ada'}), (b:Person {name:'Bo'}) CREATE (a)-[:KNOWS]->(b)",
        "MATCH (a:Person {name:'Bo'}), (b:Person {name:'Cy'}) CREATE (a)-[:KNOWS]->(b)",
        "MATCH (a:Person {name:'Cy'}), (b:Person {name:'Di'}) CREATE (a)-[:KNOWS]->(b)",
    ] {
        db.query(q).unwrap();
    }
    db
}

#[test]
fn opencypher_corpus() {
    let corpus = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cypher/corpus.txt")).unwrap();
    let (mut ok, mut bad) = (0, vec![]);
    for q in corpus.lines().filter(|l| !l.trim().is_empty()) {
        let db = graph(); // fresh graph: write queries must not affect the next one
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| db.query(q))) {
            Ok(Ok(_)) => ok += 1,
            Ok(Err(e)) => bad.push(format!("{q}\n    -> {e}")),
            Err(_) => bad.push(format!("{q}\n    -> PANIC")),
        }
    }
    let total = ok + bad.len();
    eprintln!("openCypher corpus: {ok}/{total} run");
    if std::env::var("NEXGRAPH_GAP_REPORT").is_ok() {
        for b in &bad {
            eprintln!("FAIL {b}");
        }
    }
    let min: usize = std::env::var("NEXGRAPH_GAP_MIN").ok().and_then(|v| v.parse().ok()).unwrap_or(49);
    assert!(ok >= min, "only {ok} of {total} queries run (need {min})");
}
