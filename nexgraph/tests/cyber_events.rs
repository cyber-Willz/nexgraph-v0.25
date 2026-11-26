//! Real-file check: the 100k-row synthetic cyber-attack CSV imported as one hyperedge per event, with every
//! aggregate compared against values computed independently in pandas.
//! Needs NG_CYBER_CSV and NG_CYBER_EXPECTED (json); skipped otherwise.
#![cfg(feature = "ingest")]
use nexgraph::ingest::{import_csv_hyperedges, HyperCsv};
use nexgraph::{Database, Value};
use std::time::Instant;

#[test]
fn cyber_events_as_hyperedges_match_pandas() {
    let (Ok(csv), Ok(exp)) = (std::env::var("NG_CYBER_CSV"), std::env::var("NG_CYBER_EXPECTED")) else { eprintln!("skipped"); return };
    let exp: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(exp).unwrap()).unwrap();
    let csv = std::fs::read_to_string(csv).unwrap();
    let db = Database::in_memory();
    let m = [("attack_type", "AttackType"), ("target_system", "System"), ("industry", "Industry"), ("location", "Country"), ("security_tools_used", "Tool"), ("user_role", "Role"), ("mitigation_method", "Mitigation"), ("attacker_ip", "Ip"), ("target_ip", "Ip")];
    let t = Instant::now();
    let r = import_csv_hyperedges(&db, &HyperCsv { edge_label: "ATTACK", members: &m, key_prop: "name", weight_col: None, time_cols: &["timestamp"] }, &csv).unwrap();
    println!("import: {:?} -> {} events, {} nodes, skipped {}", t.elapsed(), r.edges_created, r.nodes_created, r.rows_skipped);
    let n = |k: &str| exp[k].as_i64().unwrap();
    assert_eq!((r.edges_created as i64, r.rows_skipped), (n("rows"), 0));
    let q = |t: &str| -> Vec<Vec<Value>> {
        let s = Instant::now();
        let r = db.query(t).unwrap_or_else(|e| panic!("{t}: {e}"));
        println!("  {:>8.1?}  {}", s.elapsed(), t.split(" RETURN").next().unwrap());
        r.rows
    };
    // the Ip label is shared by attacker and target columns -> distinct targets + attackers - overlap; check targets via a join
    assert_eq!(q("MATCH (a:AttackType)-[e:ATTACK]-(b:Industry) WHERE a.name = 'Ransomware' AND b.name = 'Finance' RETURN count(e), sum(e.data_compromised_GB)")[0][0].as_i64().unwrap(), n("ransom_fin"));
    let gb = q("MATCH (a:AttackType)-[e:ATTACK]-(b:Industry) WHERE a.name = 'Ransomware' AND b.name = 'Finance' RETURN sum(e.data_compromised_GB)")[0][0].as_f64().unwrap();
    assert!((gb - exp["ransom_fin_gb"].as_f64().unwrap()).abs() < 1e-6, "{gb}");
    let mean = q("MATCH (a:AttackType)-[e:ATTACK]-(b:Country) WHERE a.name = 'Phishing' RETURN avg(e.response_time_min)")[0][0].as_f64().unwrap();
    assert!((mean - exp["phish_resp_mean"].as_f64().unwrap()).abs() < 1e-9);
    assert_eq!(q("MATCH (a:AttackType)-[e:ATTACK]-(b:Tool) WHERE a.name = 'DDoS' AND e.outcome = 'Success' AND e.attack_severity >= 9 RETURN count(e)")[0][0].as_i64().unwrap(), n("ddos_success_sev9"));
    assert_eq!(q("MATCH (a:Country)-[e:ATTACK]-(b:Tool) WHERE a.name = 'Russia' AND b.name = 'Firewall' RETURN count(e)")[0][0].as_i64().unwrap(), n("by_loc_fw"));
    for (k, v) in exp["by_type"].as_object().unwrap() {
        assert_eq!(q(&format!("MATCH (a:AttackType)-[e:ATTACK]-(b:Role) WHERE a.name = '{k}' RETURN count(e)"))[0][0].as_i64().unwrap(), v.as_i64().unwrap(), "{k}");
    }
    // epoch-second timestamps: January 2024 via the time procedures and via a range predicate
    let jan = 1704067200.0;
    assert_eq!(q(&format!("MATCH (a:AttackType)-[e:ATTACK]-(b:Role) WHERE e.timestamp >= {jan} AND e.timestamp < {} RETURN count(DISTINCT e)", jan + 31.0 * 86400.0))[0][0].as_i64().unwrap(), n("jan2024"));
    let total = q("CALL stats.describe('ATTACK','data_compromised_GB') YIELD count, mean RETURN count, mean")[0].clone();
    assert_eq!(total[0].as_i64().unwrap(), n("rows"));
    assert!((total[1].as_f64().unwrap() * n("rows") as f64 - exp["sum_gb"].as_f64().unwrap()).abs() < 0.5);
    // spectral co-occurrence over the categorical dimensions only (IPs would make 200k components)
    let dim = Database::in_memory();
    let md = &m[..7];
    import_csv_hyperedges(&dim, &HyperCsv { edge_label: "ATTACK", members: md, key_prop: "name", weight_col: None, time_cols: &["timestamp"] }, &csv).unwrap();
    let s = Instant::now();
    let c = dim.query("CALL spectral.cluster(3) YIELD node, cluster RETURN count(*), count(DISTINCT cluster)").unwrap();
    println!("  spectral.cluster over {} dimension values: {:?} {:?}", c.rows[0][0], c.rows[0][1], s.elapsed());
    let ev = dim.query("CALL spectral.eigenvalues(4) YIELD * RETURN count(*)").unwrap();
    assert_eq!(ev.rows[0][0], Value::Int(4));
}
