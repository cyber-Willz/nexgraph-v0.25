//! v0.25 regressions from the crypto-compliance run: profiler address classification, duplicate-key warning on keyed
//! node imports, and checkpoint/reopen round trips through the streaming snapshot path.
#![cfg(all(feature = "ingest", feature = "nonbacktracking"))]
use nexgraph::ingest::import_csv_nodes;
use nexgraph::profile::{profile_csv, Kind};
use nexgraph::{Database, SyncMode, Value};

#[test]
fn profiler_tells_wallets_from_counts_and_flags() {
    let mut csv = String::from("address,unique_sent_to_addresses,btc_address_present,street_address,note\n");
    for i in 0..60 {
        csv += &format!("0x{:040x},{},{},{} Main St,x\n", i * 7919, i % 9, i % 2, i);
    }
    let p = profile_csv(&csv, "eth").unwrap();
    let kind = |n: &str| p.columns.iter().find(|c| c.name == n).unwrap().kind.clone();
    assert_eq!(kind("address"), Kind::WalletAddress);
    assert_eq!(kind("unique_sent_to_addresses"), Kind::Number, "a count is not an address");
    assert_eq!(kind("btc_address_present"), Kind::Number, "a 0/1 flag is not an address");
    assert_eq!(kind("street_address"), Kind::Address, "real street addresses are still caught");
    // a bitcoin base58 address column
    let mut b = String::from("addr\n");
    for i in 0..40 {
        b += &format!("1{}\n", "BoYZ".repeat(8).replace('o', "a").chars().cycle().skip(i).take(30).collect::<String>());
    }
    let pb = profile_csv(&b, "btc").unwrap();
    assert_eq!(pb.columns[0].kind, Kind::WalletAddress);
}

#[test]
fn keyed_import_reports_duplicate_keys_within_one_file() {
    let db = Database::in_memory();
    let csv = "address,v\nA,1\nB,2\nA,3\nA,4\n";
    let r = import_csv_nodes(&db, "X", csv, Some("address")).unwrap();
    assert_eq!((r.nodes_created, r.nodes_updated, r.duplicate_keys), (2, 2, 2));
    // re-importing a file with unique keys updates existing nodes but is not a duplicate-key problem
    let r = import_csv_nodes(&db, "X", "address,v\nA,9\nB,8\n", Some("address")).unwrap();
    assert_eq!((r.nodes_created, r.nodes_updated, r.duplicate_keys), (0, 2, 0));
    let q = db.query("MATCH (n:X {address:'A'}) RETURN n.v AS v").unwrap();
    assert_eq!(q.rows[0][0], Value::Int(9));
}

#[test]
fn checkpoint_streaming_round_trips_nodes_edges_and_indexes() {
    let dir = std::env::temp_dir().join(format!("nexgraph_v025_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Database::open_with(&dir, SyncMode::Never).unwrap();
        db.query("CREATE INDEX ON :P(id)").ok();
        for i in 0..200 {
            db.query(&format!("CREATE (:P {{id:{i}, name:'n{i}', w:{}.5}})", i)).unwrap();
        }
        db.query("MATCH (a:P {id:1}),(b:P {id:2}) CREATE (a)-[:K {w:2.0}]->(b)").unwrap();
        db.query("MATCH (n:P {id:5}) SET n.name = 'changed'").unwrap();
        db.query("MATCH (n:P {id:7}) DELETE n").unwrap();
        let before = db.stats();
        db.checkpoint().unwrap();
        let after = db.stats();
        assert_eq!((before.live_nodes, before.live_edges), (after.live_nodes, after.live_edges));
        assert_eq!(after.versions, after.live_nodes + after.live_edges, "history is compacted");
        db.query("CREATE (:P {id:1000, name:'post'})").unwrap(); // lands in the WAL after the checkpoint
    }
    let db = Database::open_with(&dir, SyncMode::Never).unwrap();
    let n = |q: &str| db.query(q).unwrap().rows[0][0].clone();
    assert_eq!(n("MATCH (n:P) RETURN count(*) AS c"), Value::Int(200)); // 200 - 1 deleted + 1 post
    assert_eq!(n("MATCH (n:P {id:5}) RETURN n.name AS v"), Value::Str("changed".into()));
    assert_eq!(n("MATCH (n:P {id:1000}) RETURN n.name AS v"), Value::Str("post".into()));
    assert_eq!(n("MATCH ()-[e:K]->() RETURN count(*) AS c"), Value::Int(1));
    assert_eq!(db.stats().live_nodes, 200);
    let _ = std::fs::remove_dir_all(&dir);
}
