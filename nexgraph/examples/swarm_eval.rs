//! swarm_eval <csv> <id> <time> <x> <y> <z> <win> [opts]  -> one line per group: members|size|coh|p_coh|p_bg|verdict
use nexgraph::ingest::{import_csv_hyperedges_with, Dedup, HyperCsv, HyperOpts};
use nexgraph::{Database, Value};
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let csv = std::fs::read_to_string(&a[1]).unwrap();
    let id = a[2].as_str();
    let db = Database::in_memory();
    let o = HyperCsv { edge_label: "OBS", members: &[(id, "Track")], key_prop: "name", weight_col: None, time_cols: &[] };
    import_csv_hyperedges_with(&db, &o, &csv, &HyperOpts { dedup: Dedup::None, keep_member_props: true, string_cols: &[id], allow_single_member: true, ..Default::default() }).unwrap();
    let opts = a.get(8).cloned().unwrap_or_default();
    let q = format!("CALL swarm.detect('OBS','Track','{}','{}','{}','{}',{},'{}') YIELD *", a[2 + 1], a[4], a[5], a[6], a[7], opts);
    let r = db.query(&q).unwrap();
    let col = |n: &str| r.columns.iter().position(|x| x == n).unwrap();
    let s = |v: &Value| match v { Value::Str(s) => s.clone(), Value::Int(i) => i.to_string(), Value::Float(f) => format!("{f:.4}"), Value::Null => "-".into(), o => format!("{o:?}") };
    for row in &r.rows {
        println!("{}|{}|{}|{}|{}|{}", s(&row[col("members")]), s(&row[col("size")]), s(&row[col("heading_coherence")]), s(&row[col("p_coherence")]), s(&row[col("p_background")]), s(&row[col("verdict")]));
    }
}
