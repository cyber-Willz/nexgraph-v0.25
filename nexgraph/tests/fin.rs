//! v0.13 `fin.identity`, tab-separated import, and the scorecard separation warning.
//! `real_sec_*` replay `tests/fixtures/fin_expected.json` (pandas, `fin_expected.py`) on the balance-sheet facts of the
//! real SEC Financial Statement Data Set 2010 Q1 (`num.txt`, tab-separated).
#![cfg(feature = "ingest")]
use nexgraph::ingest::{import_csv_hyperedges_with, Dedup, HyperCsv, HyperOpts};
use nexgraph::{Database, Value};
use serde_json::Value as J;
use std::collections::HashMap;

fn load(data: &str, members: &[(&str, &str)], delim: Option<char>, strs: &[&str]) -> Database {
    let db = Database::in_memory();
    let o = HyperCsv { edge_label: "FACT", members, key_prop: "name", weight_col: None, time_cols: &[] };
    import_csv_hyperedges_with(&db, &o, data, &HyperOpts { dedup: Dedup::None, keep_member_props: true, string_cols: strs, delimiter: delim, ..Default::default() }).unwrap();
    db
}
fn sec() -> Database {
    let p = format!("{}/tests/fixtures/sec_2010q1_balance_facts.txt", env!("CARGO_MANIFEST_DIR"));
    load(&std::fs::read_to_string(p).unwrap(), &[("adsh", "Filing"), ("tag", "Tag")], Some('\t'), &["adsh"])
}
fn expected() -> J {
    let p = format!("{}/tests/fixtures/fin_expected.json", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}
fn n(v: &Value) -> f64 {
    match v {
        Value::Int(i) => *i as f64,
        Value::Float(f) => *f,
        o => panic!("{o:?}"),
    }
}
fn s(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        o => format!("{o:?}"),
    }
}
fn summary(rows: &[Vec<Value>]) -> HashMap<String, i64> {
    rows.iter().filter(|r| s(&r[0]) == "summary").map(|r| (s(&r[1]), n(&r[3]) as i64)).collect()
}
fn check(db: &Database, q: &str, want: &J) {
    let rows = db.query(q).unwrap_or_else(|e| panic!("{q}: {e}")).rows;
    let m = summary(&rows);
    assert_eq!(m["groups_with_total"], want["with_total"].as_i64().unwrap(), "{q}");
    assert_eq!(m["checked"], want["checked"].as_i64().unwrap(), "{q}");
    assert_eq!(m["violations"], want["violations"].as_i64().unwrap(), "{q}");
    assert_eq!(m["incomplete"], want["incomplete"].as_i64().unwrap(), "{q}");
    assert_eq!(m["checked"], m["pass"] + m["violations"]);
    let worst: Vec<&Vec<Value>> = rows.iter().filter(|r| s(&r[0]) == "violation").collect();
    for (i, w) in want["worst"].as_array().unwrap().iter().enumerate().take(worst.len()) {
        // equal gaps can swap places, so compare the gap percentages, not the order of the ids
        let got = n(&worst[i][6]);
        assert!((got - w[2].as_f64().unwrap()).abs() <= 1e-9 * got.max(1.0), "{q}: worst[{i}] {got} vs {:?}", w);
    }
}

#[test]
fn real_sec_balance_identities_match_pandas() {
    let (db, ex) = (sec(), expected());
    assert_eq!(ex["ambiguous_groups"].as_i64().unwrap(), 0);
    check(&db, "CALL fin.identity('FACT','adsh','ddate','tag','value','Assets','LiabilitiesAndStockholdersEquity',0.01,'segments,coreg','uom=USD',3) YIELD *", &ex["lse"]);
    check(&db, "CALL fin.identity('FACT','adsh','ddate','tag','value','Assets','Liabilities,StockholdersEquity',1,'segments,coreg','uom=USD',3) YIELD *", &ex["le"]);
    check(&db, "CALL fin.identity('FACT','adsh','ddate','tag','value','Assets','Liabilities,StockholdersEquity,?MinorityInterest,?RedeemableNoncontrollingInterestEquityCarryingAmount,?TemporaryEquityCarryingAmountAttributableToParent',1,'segments,coreg','uom=USD',3) YIELD *", &ex["le_opt"]);
}

#[test]
fn tab_import_keeps_numbers_numeric_and_ids_text() {
    let db = sec();
    let rows = db.query("CALL fin.identity('FACT','adsh','ddate','tag','value','Assets','Liabilities',1,null,null,1) YIELD *").unwrap().rows;
    assert!(summary(&rows)["facts_used"] > 6000, "values were read as numbers");
    // a comma-separated read of the same file yields one wide column, which the importer refuses
    let p = format!("{}/tests/fixtures/sec_2010q1_balance_facts.txt", env!("CARGO_MANIFEST_DIR"));
    let data = std::fs::read_to_string(p).unwrap();
    let o = HyperCsv { edge_label: "X", members: &[("adsh", "Filing"), ("tag", "Tag")], key_prop: "name", weight_col: None, time_cols: &[] };
    assert!(import_csv_hyperedges_with(&Database::in_memory(), &o, &data, &HyperOpts::default()).is_err());
}

#[test]
fn identity_by_hand_incomplete_ambiguous_tolerance_and_blank_keys() {
    // E1: 100 = 60 + 40 exactly. E2: 100 vs 60 + 39.9 -> 0.1 % off. E3: no liabilities (incomplete).
    // E4: two different Assets values (ambiguous). E5: consolidated 100 = 100, but a segment fact (segments set) says 7.
    let data = "e,p,t,v,segments\\nE1,2020,Assets,100,\\nE1,2020,L,60,\\nE1,2020,Q,40,\\nE2,2020,Assets,100,\\nE2,2020,L,60,\\nE2,2020,Q,39.9,\\nE3,2020,Assets,100,\\nE3,2020,Q,40,\\nE4,2020,Assets,100,\\nE4,2020,Assets,101,\\nE4,2020,L,60,\\nE4,2020,Q,40,\\nE5,2020,Assets,100,\\nE5,2020,L,100,\\nE5,2020,Assets,7,Seg=A;\\nE5,2020,Q,0,\\nE1,2019,Assets,50,\\nE1,2019,L,50,\\n".replace("\\n", "\n");
    let db = load(&data, &[("e", "Entity"), ("t", "Tag")], None, &[]);
    let run = |tol: &str, blank: &str, parts: &str| {
        let q = format!("CALL fin.identity('FACT','e','p','t','v','Assets','{parts}',{tol},{blank},null,10) YIELD *");
        db.query(&q).unwrap_or_else(|e| panic!("{q}: {e}")).rows
    };
    let m = summary(&run("0.01", "'segments'", "L,Q"));
    assert_eq!((m["groups_with_total"], m["checked"], m["pass"], m["violations"], m["incomplete"], m["ambiguous"]), (6, 3, 2, 1, 2, 1), "{m:?}");
    // E2 passes at a 0.2 % tolerance, fails at 0.01 %
    assert_eq!(summary(&run("0.2", "'segments'", "L,Q"))["violations"], 0);
    // with the segment fact counted, E5's Assets is ambiguous (100 vs 7)
    let m = summary(&run("0.01", "null", "L,Q"));
    assert_eq!(m["ambiguous"], 2, "{m:?}");
    // the one violation names E2 and its gap
    let rows = run("0.01", "'segments'", "L,Q");
    let v: Vec<_> = rows.iter().filter(|r| s(&r[0]) == "violation").collect();
    assert_eq!(v.len(), 1);
    assert_eq!(s(&v[0][1]), "E2");
    assert!((n(&v[0][5]) - 0.1).abs() < 1e-9 && (n(&v[0][6]) - 0.1).abs() < 1e-9);
    // optional part: E3 becomes checkable when Liabilities is optional (100 vs 0 + 40 -> violation)
    let m = summary(&run("0.01", "'segments'", "?L,Q"));
    assert_eq!(m["incomplete"], 1, "only E1 2019 still lacks Q: {m:?}");
    assert_eq!(m["violations"], 2, "E2 (0.1 %) and E3 (100 vs 0 + 40): {m:?}");
    assert!(db.query("CALL fin.identity('FACT','e','p','t','v','Assets','',0.01,null,null,10) YIELD *").is_err());
    assert!(db.query("CALL fin.identity('FACT','e','p','t','v','Assets','L,Q','0.01',null,null,10) YIELD *").is_err(), "tolerance must be numeric");
}

#[test]
fn scorecard_warns_on_a_column_that_is_the_answer() {
    // `leak` equals the outcome; `noise` does not
    let mut csv = String::from("id,leak,noise,bad,grp\n");
    for i in 0..400 {
        let bad = i % 5 == 0;
        csv.push_str(&format!("{i},{},{},{},{}\n", if bad { 900 + i % 7 } else { 100 + i % 7 }, (i * 37) % 11, i32::from(bad), i % 3));
    }
    let db = Database::in_memory();
    let o = HyperCsv { edge_label: "L", members: &[("id", "Id"), ("grp", "Grp")], key_prop: "name", weight_col: None, time_cols: &[] };
    import_csv_hyperedges_with(&db, &o, &csv, &HyperOpts::default()).unwrap_or_else(|e| panic!("import: {e}"));
    let rows = db.query("CALL uw.scorecard('L','bad=1','leak,noise','Id') YIELD *").unwrap().rows;
    assert_eq!(s(&rows[0][0]), "warning");
    assert!(s(&rows[0][2]).contains("uw.audit"));
    let rows = db.query("CALL uw.scorecard('L','bad=1','noise','Id') YIELD *").unwrap().rows;
    assert!(rows.iter().all(|r| s(&r[0]) != "warning"), "an honest weak model must not be warned");
}
