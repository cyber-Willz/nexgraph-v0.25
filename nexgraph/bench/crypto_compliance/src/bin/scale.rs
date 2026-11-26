use nexgraph::ingest::import_csv_nodes;
use nexgraph::{Database, SyncMode};
use std::time::Instant;
fn rss_mb() -> u64 { std::fs::read_to_string("/proc/self/status").ok().and_then(|s| s.lines().find(|l| l.starts_with("VmHWM")).and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())).unwrap_or(0) / 1024 }
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let csv = std::fs::read_to_string(&a[1]).unwrap();
    let db = Database::open_with(&a[2], SyncMode::Never).unwrap();
    let r0 = rss_mb();
    let t = Instant::now();
    let r = import_csv_nodes(&db, "Heist", &csv, None).unwrap();
    println!("import rows={} secs={:.1} rss_before={}MB rss_after={}MB", r.rows_read, t.elapsed().as_secs_f64(), r0, rss_mb());
    let t = Instant::now();
    db.checkpoint().unwrap();
    println!("checkpoint secs={:.1} rss_peak={}MB", t.elapsed().as_secs_f64(), rss_mb());
}
