//! Run NQL statements from a file against a directory database; one JSON line per statement.
//! File format: `### name` line, then the statement (until the next `###`).
use nexgraph::{Database, SyncMode};
use std::time::Instant;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let t0 = Instant::now();
    let db = Database::open_with(&a[1], SyncMode::Never).expect("open");
    println!("{{\"name\":\"_open\",\"ms\":{:.1}}}", t0.elapsed().as_secs_f64() * 1e3);
    let text = std::fs::read_to_string(&a[2]).expect("read");
    let mut cur: Option<(String, String)> = None;
    let mut all: Vec<(String, String)> = vec![];
    for l in text.lines() {
        if let Some(n) = l.strip_prefix("### ") {
            if let Some(c) = cur.take() { all.push(c); }
            cur = Some((n.trim().to_string(), String::new()));
        } else if let Some((_, s)) = cur.as_mut() { s.push_str(l); s.push('\n'); }
    }
    if let Some(c) = cur { all.push(c); }
    for (name, q) in all {
        let t = Instant::now();
        match db.query(q.trim()) {
            Ok(r) => println!("{{\"name\":{:?},\"ms\":{:.1},\"rows\":{},\"data\":{}}}", name, t.elapsed().as_secs_f64() * 1e3, r.rows.len(), r.to_json()),
            Err(e) => println!("{{\"name\":{:?},\"ms\":{:.1},\"error\":{:?}}}", name, t.elapsed().as_secs_f64() * 1e3, e.to_string()),
        }
    }
}
