//! Loads the full 9,700-path stochastic file (1.17M events) and times `alm.scenario_risk` on it.
//! usage: cargo run --release --example alm_full_paths -- extended/scenarios_stochastic.csv
use nexgraph::ingest::{import_csv_hyperedges_with, Dedup, HyperCsv, HyperOpts};
use nexgraph::Database;
use std::time::Instant;

fn rss_mb() -> u64 {
    std::fs::read_to_string("/proc/self/status").ok().and_then(|s| s.lines().find(|l| l.starts_with("VmRSS")).and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())).unwrap_or(0) / 1024
}

fn main() {
    let path = std::env::args().nth(1).expect("path to scenarios_stochastic.csv");
    let csv = std::fs::read_to_string(&path).unwrap();
    let db = Database::in_memory();
    let t = Instant::now();
    let o = HyperCsv { edge_label: "PATH", members: &[("scenario_id", "Scenario"), ("month", "Month")], key_prop: "name", weight_col: None, time_cols: &[] };
    let r = import_csv_hyperedges_with(&db, &o, &csv, &HyperOpts { dedup: Dedup::None, keep_member_props: true, ..Default::default() }).unwrap();
    println!("imported {} events in {:.1}s, RSS {} MB", r.edges_created, t.elapsed().as_secs_f64(), rss_mb());
    for q in [
        "CALL alm.scenario_risk('PATH','scenario_id','month','short_rate',60,0,'change') YIELD *",
        "CALL alm.scenario_risk('PATH','scenario_id','month','equity_index',60,0,'relative') YIELD *",
        "CALL alm.scenario_risk('PATH','scenario_id','month','short_rate',120,0,'change',-1000000000,50000000000,0.995) YIELD *",
    ] {
        let t = Instant::now();
        let r = db.query(q).unwrap();
        let row = &r.rows[0];
        let get = |c: &str| row[r.columns.iter().position(|x| x == c).unwrap()].as_f64();
        println!("{:>6.0} ms  n={:?} mean={:.6?} p005={:.6?} p995={:.6?} pnl_var={:?}  {}", t.elapsed().as_secs_f64() * 1e3, get("scenarios"), get("change_mean"), get("change_p005"), get("change_p995"), get("pnl_var"), &q[..70]);
    }
}
