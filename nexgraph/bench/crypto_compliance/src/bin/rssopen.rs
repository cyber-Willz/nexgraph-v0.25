use nexgraph::{Database, SyncMode};
fn rss() -> u64 { std::fs::read_to_string("/proc/self/status").ok().and_then(|s| s.lines().find(|l| l.starts_with("VmRSS")).and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())).unwrap_or(0) / 1024 }
fn main() { let d = std::env::args().nth(1).unwrap(); let t = std::time::Instant::now(); let db = Database::open_with(&d, SyncMode::Never).unwrap(); println!("open {:.1}s rss={}MB stats={:?}", t.elapsed().as_secs_f64(), rss(), db.stats()); }
