//! Load the ten crypto-compliance CSVs into a directory database; one JSON line per step.
use nexgraph::ingest::{import_csv_hyperedges_with, import_csv_nodes, HyperCsv, HyperOpts};
use nexgraph::{Database, SyncMode};
use std::time::Instant;
fn rss_mb() -> u64 { std::fs::read_to_string("/proc/self/status").ok().and_then(|s| s.lines().find(|l| l.starts_with("VmHWM")).and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())).unwrap_or(0) / 1024 }
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (src, dir) = (&a[1], &a[2]);
    let db = Database::open_with(dir, SyncMode::Never).expect("open");
    let rd = |f: &str| std::fs::read_to_string(format!("{src}/{f}.csv")).unwrap();
    let t0 = Instant::now();
    // (file, label, key column or none)
    let nodes: &[(&str, &str, Option<&str>)] = &[
        ("bitcoin_heist_ransomware", "Heist", None),
        ("bitcoin_heist_ransomware", "HeistByAddr", Some("address")),
        ("bitcoin_scam_messages", "Msg", None),
        ("bitcoin_daily_network", "Daily", None),
        ("crypto_scam_tx", "ScamTx", None),
        ("etfd_ethereum_fraud", "Etfd", None),
        ("ethereum_fraud_demo", "EthDemo", None),
        ("eth_fraud_cleaned", "Account", Some("address")),
        ("eth_illicit_features", "Account", Some("address")),
    ];
    for (f, l, k) in nodes {
        let csv = rd(f);
        let t = Instant::now();
        match import_csv_nodes(&db, l, &csv, *k) {
            Ok(r) => println!("{{\"step\":\"{f}->{l}\",\"keyed\":{},\"secs\":{:.2},\"rows_read\":{},\"skipped\":{},\"created\":{},\"updated\":{},\"duplicate_keys\":{},\"first_error\":{:?},\"rss_mb\":{}}}", k.is_some(), t.elapsed().as_secs_f64(), r.rows_read, r.rows_skipped, r.nodes_created, r.nodes_updated, r.duplicate_keys, r.first_error, rss_mb()),
            Err(e) => println!("{{\"step\":\"{f}->{l}\",\"error\":{:?}}}", e.to_string()),
        }
    }
    let hyp: &[(&str, &str, &[(&str, &str)], &[&str])] = &[
        ("blockchain_tx_risk", "TxRisk", &[("transaction_id", "Transaction"), ("wallet_id", "Wallet")], &["transaction_id", "wallet_id", "split"]),
        ("digital_payment_fraud", "Payment", &[("transaction_id", "PayTxn"), ("user_id", "User"), ("device_location", "Location")], &["transaction_id", "user_id", "device_location", "transaction_type", "payment_mode", "device_type", "split"]),
    ];
    for (f, l, m, strs) in hyp {
        let csv = rd(f);
        let o = HyperCsv { edge_label: l, members: m, key_prop: "id", weight_col: None, time_cols: &[] };
        let opts = HyperOpts { keep_member_props: false, string_cols: strs, chunk_rows: 20_000, ..Default::default() };
        let t = Instant::now();
        match import_csv_hyperedges_with(&db, &o, &csv, &opts) {
            Ok(r) => println!("{{\"step\":\"{f}->{l}\",\"keyed\":false,\"secs\":{:.2},\"rows_read\":{},\"skipped\":{},\"created\":{},\"edges\":{},\"first_error\":{:?},\"rss_mb\":{}}}", t.elapsed().as_secs_f64(), r.rows_read, r.rows_skipped, r.nodes_created, r.edges_created, r.first_error, rss_mb()),
            Err(e) => println!("{{\"step\":\"{f}->{l}\",\"error\":{:?}}}", e.to_string()),
        }
    }
    let t = Instant::now();
    db.checkpoint().expect("checkpoint");
    println!("{{\"checkpoint_secs\":{:.2},\"total_secs\":{:.2},\"stats\":\"{:?}\",\"rss_mb\":{}}}", t.elapsed().as_secs_f64(), t0.elapsed().as_secs_f64(), db.stats(), rss_mb());
}
