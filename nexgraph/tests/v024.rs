//! v0.24 regressions from the Belize land-registry run: hyperedge-label hint, tenancy-aware `land.conflicts`,
//! the opt-in community count of `spectral.nb_radius`, and the CLI-visible `allow_single_member` importer path.
#![cfg(all(feature = "ingest", feature = "nonbacktracking"))]
use nexgraph::ingest::{import_csv_hyperedges_with, Dedup, HyperCsv, HyperOpts};
use nexgraph::{Database, Value};

const OWN: &str = "parcel_id,owner,tenancy_type\nA,P1,SOLE\nA,P2,JOINT_TENANCY\nB,P3,JOINT_TENANCY\nB,P4,TENANCY_IN_COMMON\nC,P5,SOLE\nC,P6,SOLE\nD,P7,SOLE\n";

fn db() -> Database {
    let db = Database::in_memory();
    let m = [("parcel_id", "Parcel"), ("owner", "Party")];
    let o = HyperCsv { edge_label: "Ownership", members: &m, key_prop: "id", weight_col: None, time_cols: &[] };
    let opts = HyperOpts { dedup: Dedup::None, keep_member_props: true, string_cols: &["tenancy_type"], ..Default::default() };
    let r = import_csv_hyperedges_with(&db, &o, OWN, &opts).unwrap();
    assert_eq!(r.rows_skipped, 0, "{:?}", r.first_error);
    db
}

#[test]
fn hyperedge_label_used_as_node_label_is_explained() {
    let d = db();
    let e = d.query("MATCH (r:Ownership) RETURN count(*) AS n").unwrap_err().to_string();
    assert!(e.contains("MATCH HYPEREDGE"), "{e}");
    // a genuinely empty node label is still just empty
    assert_eq!(d.query("MATCH (r:NoSuchLabel) RETURN count(*) AS n").unwrap().rows[0][0], Value::Int(0));
    // and real node labels / the hyperedge form are unaffected
    assert_eq!(d.query("MATCH (p:Parcel) RETURN count(*) AS n").unwrap().rows[0][0], Value::Int(4));
    assert_eq!(d.query("MATCH HYPEREDGE h:Ownership RETURN count(*) AS n").unwrap().rows[0][0], Value::Int(7));
}

#[test]
fn conflicts_separates_co_ownership_from_competing_claims() {
    let d = db();
    let kinds = |q: &str| -> Vec<(String, String)> {
        let r = d.query(q).unwrap();
        let (pi, ki) = (r.columns.iter().position(|c| c == "parcel").unwrap(), r.columns.iter().position(|c| c == "kind").unwrap());
        let mut v: Vec<_> = r.rows.iter().map(|x| (format!("{}", x[pi]).trim_matches('\'').to_string(), format!("{}", x[ki]).trim_matches('\'').to_string())).collect();
        v.sort();
        v
    };
    // without the tenancy argument the behaviour is unchanged: every multi-owner parcel is "competing"
    let plain = kinds("CALL land.conflicts('Ownership','parcel_id','owner')");
    assert!(plain.iter().all(|(_, k)| k == "competing_owners") && plain.len() == 3, "{plain:?}");
    let t = kinds("CALL land.conflicts('Ownership','parcel_id','owner',100,null,'tenancy_type')");
    let get = |p: &str| t.iter().find(|(a, _)| a == p).map(|(_, k)| k.clone());
    assert_eq!(get("B").as_deref(), Some("co_ownership")); // two co-owners, no sole title
    assert_eq!(get("A").as_deref(), Some("competing_owners")); // a sole title next to another claimant
    assert_eq!(get("C").as_deref(), Some("competing_owners")); // two sole titles
    assert_eq!(get("D"), None);
}

#[test]
fn nb_radius_community_count_is_opt_in() {
    let d = Database::in_memory();
    d.query("CREATE (a:N {id:1}),(b:N {id:2}),(c:N {id:3}),(e:N {id:4}),(a)-[:K]->(b),(b)-[:K]->(c),(c)-[:K]->(a),(c)-[:K]->(e),(e)-[:K]->(a)").unwrap();
    let r = d.query("CALL spectral.nb_radius() YIELD rho_b, communities RETURN rho_b, communities").unwrap();
    assert!(matches!(r.rows[0][0], Value::Float(x) if x > 1.0));
    assert_eq!(r.rows[0][1], Value::Null);
    let r = d.query("CALL spectral.nb_radius(true) YIELD communities RETURN communities").unwrap();
    assert!(matches!(r.rows[0][0], Value::Int(k) if k >= 1), "{:?}", r.rows[0][0]);
}

#[test]
fn single_member_rows_load_only_when_allowed() {
    let d = Database::in_memory();
    let m = [("parcel", "Parcel"), ("holder", "Party")];
    let o = HyperCsv { edge_label: "Enc", members: &m, key_prop: "id", weight_col: None, time_cols: &[] };
    let csv = "parcel,holder\nA,H1\nB,\n";
    let r = import_csv_hyperedges_with(&d, &o, csv, &HyperOpts::default()).unwrap();
    assert_eq!(r.rows_skipped, 1);
    assert!(r.first_error.is_some());
    let d = Database::in_memory();
    let r = import_csv_hyperedges_with(&d, &o, csv, &HyperOpts { allow_single_member: true, ..Default::default() }).unwrap();
    assert_eq!(r.rows_skipped, 0);
}
