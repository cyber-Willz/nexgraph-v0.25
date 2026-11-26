//! v0.10: imports the Enterprise Cyber Kill Chain CSV (dedup by Incident_ID) and times/prints every killchain.* procedure.
//! usage: cargo run --release --example bench_killchain -- FILE.csv
use nexgraph::ingest::{import_csv_hyperedges, import_csv_hyperedges_with, Dedup, HyperCsv, HyperOpts};
use nexgraph::Database;
use std::time::Instant;
fn main() {
    let path = std::env::args().nth(1).expect("usage: bench_killchain FILE.csv");
    let csv = std::fs::read_to_string(path).unwrap();
    let m = [("Industry", "Industry"), ("Country", "Country"), ("Company_Size", "Size"), ("Attack_Vector", "Vector"), ("Threat_Actor", "Actor"), ("Attack_Stage", "Stage"), ("Password_Policy", "PwPolicy"), ("Compliance", "Compliance")];
    let h = HyperCsv { edge_label: "INCIDENT", members: &m, key_prop: "name", weight_col: None, time_cols: &["Timestamp"] };
    let t = Instant::now();
    let raw = Database::in_memory();
    let r0 = import_csv_hyperedges(&raw, &h, &csv).unwrap();
    println!("v0.9-style import (no dedup): {:?}: {} incidents", t.elapsed(), r0.edges_created);
    let t = Instant::now();
    let db = Database::in_memory();
    let r = import_csv_hyperedges_with(&db, &h, &csv, &HyperOpts { dedup: Dedup::Key("Incident_ID"), keep_member_props: true, ..Default::default() }).unwrap();
    println!("v0.10 import (dedup=key:Incident_ID, props): {:?}: {} incidents, {} duplicates skipped, {} nodes\n", t.elapsed(), r.edges_created, r.duplicates_skipped, r.nodes_created);
    let total = |d: &Database| d.query("CALL stats.describe('INCIDENT','Financial_Loss_USD') YIELD count, mean RETURN count, mean").unwrap().rows[0].clone();
    println!("loss records / mean loss, no dedup: {:?}   dedup: {:?}\n", total(&raw), total(&db));
    let order = "['Reconnaissance','Initial Access','Execution','Persistence','Impact']";
    let qs = [
        format!("CALL killchain.funnel('INCIDENT','Attack_Stage','Attack_Success',{order},'Financial_Loss_USD','Detection_Time_Min') YIELD * RETURN stage, rank, incidents, share, reached_share, success_rate, mean_detect, mean_loss, total_loss"),
        "CALL killchain.control_effect('INCIDENT','MFA','Attack_Success') YIELD * RETURN method, n_with, n_without, risk_with, risk_without, abs_reduction, relative_risk, rr_low, rr_high, p_value".into(),
        "CALL killchain.control_effect('INCIDENT','EDR','Attack_Success') YIELD * RETURN method, n_with, n_without, risk_with, risk_without, abs_reduction, relative_risk, rr_low, rr_high, p_value".into(),
        "CALL killchain.control_effect('INCIDENT','IDS','Attack_Success') YIELD * RETURN method, n_with, n_without, risk_with, risk_without, abs_reduction, relative_risk, rr_low, rr_high, p_value".into(),
        "CALL killchain.control_effect('INCIDENT','Security_Training','Attack_Success') YIELD * RETURN method, n_with, n_without, risk_with, risk_without, abs_reduction, relative_risk, rr_low, rr_high, p_value".into(),
        "CALL killchain.control_effect('INCIDENT','Firewall','Attack_Success') YIELD * RETURN method, n_with, n_without, risk_with, risk_without, abs_reduction, relative_risk, rr_low, rr_high, p_value".into(),
        "CALL killchain.control_effect('INCIDENT','MFA','Attack_Success','Attack_Vector') YIELD * RETURN group, method, n_with, n_without, risk_with, risk_without, relative_risk, rr_low, rr_high, p_value".into(),
        "CALL killchain.exposure('INCIDENT','Attack_Vector','Attack_Success','Financial_Loss_USD') YIELD * RETURN group, incidents, success_rate, ci_low, ci_high, mean_loss, p90_loss, total_loss, loss_share".into(),
        "CALL killchain.exposure('INCIDENT','Industry','Attack_Success','Financial_Loss_USD') YIELD * RETURN group, incidents, success_rate, mean_loss, total_loss, loss_share".into(),
        "CALL killchain.exposure('INCIDENT','Threat_Actor','Attack_Success','Financial_Loss_USD') YIELD * RETURN group, incidents, success_rate, mean_loss, total_loss, loss_share".into(),
        "CALL killchain.response('INCIDENT','EDR','Detection_Time_Min','Response_Time_Min') YIELD * RETURN group, incidents, mttd_mean, mttd_p50, mttd_p90, mttr_mean, mttr_p50, mttr_p90".into(),
        "CALL killchain.response('INCIDENT','IDS','Detection_Time_Min','Response_Time_Min') YIELD * RETURN group, incidents, mttd_mean, mttd_p50, mttd_p90, mttr_mean, mttr_p50, mttr_p90".into(),
        "CALL killchain.defense_depth('INCIDENT',['Firewall','MFA','EDR','IDS','Security_Training'],'Attack_Success','Financial_Loss_USD') YIELD * RETURN controls_present, incidents, success_rate, ci_low, ci_high, mean_loss".into(),
        "CALL killchain.exposure('INCIDENT','Password_Policy','Attack_Success','Financial_Loss_USD') YIELD * RETURN group, incidents, success_rate, mean_loss, total_loss, loss_share".into(),
    ];
    for q in &qs {
        let t = Instant::now();
        match db.query(q) {
            Ok(r) => println!("[{:?}] {}\n{}", t.elapsed(), q.split(" YIELD").next().unwrap(), r.to_table()),
            Err(e) => println!("ERR {q}: {e}"),
        }
    }
}
