//! v0.13 regression tests: `uw.*`, `land.hotspots(side)`, keyed node import with `Label.prop` joins, chunked commits.
//!
//! `reference_*` tests load real fixtures (LendingClub loans, a 3,000-row sample of a synthetic loan-approval set, a
//! synthetic insurance claims graph) and compare the engine with `tests/fixtures/uw_expected.json`, produced by
//! `tests/fixtures/uw_expected.py` with numpy / scipy / sklearn / statsmodels only (regenerate:
//! `python3 -I tests/fixtures/uw_expected.py tests/fixtures > tests/fixtures/uw_expected.json`).
#![cfg(feature = "ingest")]
use nexgraph::ingest::{import_csv_hyperedges_with, import_csv_nodes_keyed, Dedup, HyperCsv, HyperOpts};
use nexgraph::{Database, Value};
use serde_json::Value as J;
use std::collections::HashMap;

fn call(db: &Database, q: &str) -> (Vec<String>, Vec<Vec<Value>>) {
    let r = db.query(q).unwrap_or_else(|e| panic!("{q}: {e}"));
    (r.columns, r.rows)
}
fn text(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Bool(b) => b.to_string(),
        o => format!("{o:?}"),
    }
}
fn num(v: &Value) -> f64 {
    match v {
        Value::Int(i) => *i as f64,
        Value::Float(f) => *f,
        o => panic!("not a number: {o:?}"),
    }
}
fn col(cols: &[String], c: &str) -> usize {
    cols.iter().position(|x| x == c).unwrap_or_else(|| panic!("no column {c} in {cols:?}"))
}
fn close(a: f64, b: f64, tol: f64, what: &str) {
    assert!((a - b).abs() <= tol * b.abs().max(1.0), "{what}: got {a}, want {b}");
}
fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}
fn expected() -> J {
    serde_json::from_str(&fixture("uw_expected.json")).unwrap()
}
fn load(csv: &str, label: &str, members: &[(&str, &str)], dedup: Dedup, chunk: usize) -> Database {
    let db = Database::in_memory();
    let o = HyperCsv { edge_label: label, members, key_prop: "name", weight_col: None, time_cols: &[] };
    import_csv_hyperedges_with(&db, &o, csv, &HyperOpts { dedup, keep_member_props: true, chunk_rows: chunk, ..Default::default() }).unwrap();
    db
}
fn lending() -> Database {
    load(&fixture("lending_club_loans.csv"), "LOAN", &[("purpose", "Purpose"), ("credit.policy", "Policy")], Dedup::None, 0)
}

#[test]
fn reference_scorecard_matches_numpy_sklearn_statsmodels() {
    let (db, ex) = (lending(), expected());
    let sc = &ex["scorecard"];
    let feats = sc["features"].as_str().unwrap();
    for case in sc["cases"].as_array().unwrap() {
        let l2 = case["l2"].as_f64().unwrap();
        let (c, rows) = call(&db, &format!("CALL uw.scorecard('LOAN','not.fully.paid=1','{feats}','','l2={l2}') YIELD *"));
        let mut got: HashMap<String, Vec<Value>> = HashMap::new();
        for r in rows {
            got.insert(format!("{}|{}|{}", text(&r[col(&c, "section")]), text(&r[col(&c, "name")]), text(&r[col(&c, "level")])), r);
        }
        let mut n = 0;
        for (k, want) in case["rows"].as_object().unwrap() {
            // the engine puts the model metrics under train / test and the coefficients under coef
            let r = got.get(k).unwrap_or_else(|| panic!("l2={l2}: engine has no row {k}; has {:?}", got.keys().collect::<Vec<_>>()));
            for (field, w) in want.as_object().unwrap() {
                let tol = if k.contains("p_value") || field == "p_value" { 1e-6 } else { 1e-7 };
                let g = num(&r[col(&c, field)]);
                close(g, w.as_f64().unwrap(), tol, &format!("l2={l2} {k}.{field}"));
                n += 1;
            }
        }
        assert!(n > 60, "only {n} values compared");
    }
}

#[test]
fn reference_audit_auc_matches_delong() {
    let (db, ex) = (lending(), expected());
    let feats: Vec<&String> = ex["audit"].as_object().unwrap().keys().collect();
    let list = feats.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(",");
    let (c, rows) = call(&db, &format!("CALL uw.audit('LOAN','not.fully.paid=1','{list}') YIELD *"));
    assert_eq!(rows.len(), feats.len());
    for r in rows {
        let w = &ex["audit"][text(&r[col(&c, "feature")])];
        close(num(&r[col(&c, "auc")]), w["auc"].as_f64().unwrap(), 1e-9, "auc");
        close(num(&r[col(&c, "auc_low")]), w["low"].as_f64().unwrap(), 1e-9, "auc_low");
        close(num(&r[col(&c, "auc_high")]), w["high"].as_f64().unwrap(), 1e-9, "auc_high");
    }
}

#[test]
fn reference_psi_matches_numpy() {
    let (db, ex) = (lending(), expected());
    let (c, rows) = call(&db, "CALL uw.psi('LOAN','fico','int.rate<0.12','int.rate>=0.12') YIELD *");
    let total = rows.last().unwrap();
    assert_eq!(text(&total[0]), "(total)");
    close(num(&total[col(&c, "psi_component")]), ex["psi"]["total"].as_f64().unwrap(), 1e-9, "psi total");
    let want_b: Vec<i64> = ex["psi"]["base"].as_array().unwrap().iter().map(|v| v.as_i64().unwrap()).collect();
    let want_c: Vec<i64> = ex["psi"]["compare"].as_array().unwrap().iter().map(|v| v.as_i64().unwrap()).collect();
    let body = &rows[..rows.len() - 1];
    assert_eq!(body.iter().map(|r| num(&r[col(&c, "base_records")]) as i64).collect::<Vec<_>>(), want_b);
    assert_eq!(body.iter().map(|r| num(&r[col(&c, "compare_records")]) as i64).collect::<Vec<_>>(), want_c);
}

#[test]
fn reference_disparity_matches_scipy_chi2() {
    let ex = expected();
    let db = load(&fixture("loan_approval_3000.csv"), "LOAN", &[("Application_ID", "Application"), ("Marital_Status", "Marital")], Dedup::Key("Application_ID"), 0);
    let (c, rows) = call(&db, "CALL uw.disparity('LOAN','Marital_Status','Loan_Approved=Approved') YIELD *");
    let w = &ex["disparity"];
    for r in &rows {
        let g = &w["groups"][text(&r[col(&c, "group")])];
        assert_eq!(num(&r[col(&c, "records")]) as i64, g["n"].as_i64().unwrap());
        assert_eq!(num(&r[col(&c, "favourable")]) as i64, g["fav"].as_i64().unwrap());
        close(num(&r[col(&c, "rate")]), g["rate"].as_f64().unwrap(), 1e-12, "rate");
        close(num(&r[col(&c, "air")]), g["air"].as_f64().unwrap(), 1e-12, "air");
        close(num(&r[col(&c, "chi2")]), w["chi2"].as_f64().unwrap(), 1e-9, "chi2");
        close(num(&r[col(&c, "chi2_p")]), w["p"].as_f64().unwrap(), 1e-7, "chi2 p");
        assert_eq!(text(&r[col(&c, "ref_group")]), w["ref"].as_str().unwrap());
    }
}

#[test]
fn reference_hotspots_one_sided_matches_scipy() {
    let ex = expected();
    let m = [("claim_id", "Claim"), ("claimant_id", "Claimant"), ("clinic_provider_id", "Clinic"), ("repair_provider_id", "Repair"), ("attorney_provider_id", "Attorney")];
    let db = load(&fixture("insurance_claims_graph.csv"), "CLAIM", &m, Dedup::Key("claim_id"), 0);
    for (key, w) in ex["hotspots"].as_object().unwrap() {
        for (side, field) in [("high", "high"), ("both", "both")] {
            let (c, rows) = call(&db, &format!("CALL land.hotspots('CLAIM','{key}','is_fraud=1',null,10,'{side}') YIELD *"));
            let mut got: Vec<String> = rows.iter().filter(|r| matches!(r[col(&c, "flagged")], Value::Bool(true))).map(|r| text(&r[0])).collect();
            got.sort();
            let want: Vec<String> = w[field].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
            assert_eq!(got, want, "{key} side={side}");
        }
    }
    assert!(db.query("CALL land.hotspots('CLAIM','clinic_provider_id','is_fraud=1',null,10,'up') YIELD *").is_err());
    // the screen finds a share of the planted rings and, one-sided, never a provider outside them
    for (_, w) in ex["hotspots"].as_object().unwrap() {
        assert_eq!(w["false_pos_high"].as_i64().unwrap(), 0);
        assert!(w["ring_hits"].as_i64().unwrap() >= 6);
    }
}

#[test]
fn audit_flags_post_decision_columns_but_not_a_strong_honest_one() {
    let db = load(&fixture("loan_approval_3000.csv"), "LOAN", &[("Application_ID", "Application"), ("Marital_Status", "Marital")], Dedup::Key("Application_ID"), 0);
    let (c, rows) = call(&db, "CALL uw.audit('LOAN','Loan_Approved=Rejected','Credit_Score,Approved_Amount,Risk_Score,Default_Risk,Age') YIELD *");
    let flags: HashMap<String, String> = rows.iter().map(|r| (text(&r[col(&c, "feature")]), text(&r[col(&c, "flags")]))).collect();
    for leaky in ["Approved_Amount", "Risk_Score", "Default_Risk"] {
        assert!(flags[leaky].contains("leak?"), "{leaky}: {:?}", flags[leaky]);
    }
    assert!(!flags["Credit_Score"].contains("leak?"), "Credit_Score is strong but legitimate");
    assert!(!flags["Age"].contains("leak?"));
}

#[test]
fn out_of_time_split_and_unknown_options_are_handled() {
    let db = lending();
    let (c, rows) = call(&db, "CALL uw.scorecard('LOAN','not.fully.paid=1','fico,int.rate','','test=credit.policy=0') YIELD *");
    let n = |name: &str| rows.iter().find(|r| text(&r[0]) == "meta" && text(&r[1]) == name).map(|r| num(&r[col(&c, "value")])).unwrap();
    assert_eq!(n("test_records") as i64, 1868); // credit.policy = 0 in LendingClub
    assert_eq!(n("train_records") as i64 + n("test_records") as i64, 9578);
    assert!(db.query("CALL uw.scorecard('LOAN','not.fully.paid=1','fico','','bogus=1') YIELD *").is_err());
    assert!(db.query("CALL uw.scorecard('LOAN','not.fully.paid=1','nope','') YIELD *").is_err());
}

#[test]
fn affordability_by_hand() {
    // loan 1: income 5000/month, existing debt 1000, new loan 12000 at 0% over 12 months -> payment 1000
    //   now 20 %, pro-forma 40 %; stressed (+200 bp = 2 %): payment 12000 r / (1 - (1+r)^-12), r = 0.02/12
    // loan 2: income 4000, debt 500, loan 10000 at 12 % over 24 months -> payment 470.7347..., pro-forma 24.3 %
    let csv = "id,inc,debt,amt,rate,term\nA,5000,1000,12000,0,12 months\nB,4000,500,10000,12,2 years\nC,0,100,5000,10,12\n";
    let db = load(csv, "L", &[("id", "Loan"), ("term", "Term")], Dedup::Key("id"), 0);
    let (c, rows) = call(&db, "CALL uw.affordability('L','inc','debt','amt','rate','term') YIELD *");
    let r = &rows[0];
    assert_eq!(num(&r[col(&c, "evaluated")]) as i64, 2);
    assert_eq!(num(&r[col(&c, "excluded")]) as i64, 1); // zero income
    let dti_pf: [f64; 2] = [40.0, (500.0 + 10000.0 * 0.01 / (1.0 - 1.01f64.powi(-24))) / 4000.0 * 100.0];
    let mut v = [dti_pf[0], dti_pf[1]];
    v.sort_by(f64::total_cmp);
    close(num(&r[col(&c, "median_dti_proforma")]), (v[0] + v[1]) / 2.0, 1e-9, "median pro-forma DTI");
    close(num(&r[col(&c, "median_dti_now")]), (20.0 + 12.5) / 2.0, 1e-9, "median current DTI");
    assert_eq!(num(&r[col(&c, "over_limit_proforma")]) as i64, 0);
    let rs: f64 = 0.02 / 12.0;
    let stressed_a = (1000.0 + 12000.0 * rs / (1.0 - (1.0 + rs).powi(-12))) / 5000.0 * 100.0;
    assert!(stressed_a < 43.0);
    let (_, rows) = call(&db, "CALL uw.affordability('L','inc','debt','amt','rate','term','limit=30') YIELD *");
    close(num(&rows[0][col(&c, "over_limit_proforma")]), 0.5, 1e-12, "over limit"); // a share: A is at 40 % > 30 %; B at 24.3 % is not
}

#[test]
fn keyed_node_import_joins_through_label_dot_prop() {
    let loans = "loan,cust,amount\nL1,C1,100\nL2,C1,200\nL3,C2,300\nL4,C9,400\n";
    let custs = "cust,score,age\nC1,700,30\nC2,500,50\nC3,600,40\n";
    let defaults = "loan,default\nL1,0\nL2,1\nL3,1\nL4,0\n";
    let db = load(loans, "LOAN", &[("loan", "Loan"), ("cust", "Customer")], Dedup::Key("loan"), 0);
    let r = import_csv_nodes_keyed(&db, "Customer", custs, "cust", "name").unwrap();
    assert_eq!((r.nodes_updated, r.nodes_created), (2, 1), "C1 and C2 exist, C3 is new");
    let r = import_csv_nodes_keyed(&db, "Loan", defaults, "loan", "name").unwrap();
    assert_eq!((r.nodes_updated, r.nodes_created), (4, 0));
    // the join: Customer.score is read from the member node, Loan.default from the member loan node
    let (c, rows) = call(&db, "CALL uw.audit('LOAN','Loan.default=1','Customer.score,amount') YIELD *");
    let miss = |f: &str| num(&rows.iter().find(|r| text(&r[0]) == f).unwrap()[col(&c, "missing_rate")]);
    close(miss("amount"), 0.0, 1e-12, "amount missing");
    close(miss("Customer.score"), 0.25, 1e-12, "L4's customer C9 has no row, so one of four is missing");
}

#[test]
fn chunked_commits_import_the_same_graph() {
    let csv = fixture("lending_club_loans.csv");
    let m = [("purpose", "Purpose"), ("credit.policy", "Policy")];
    let a = load(&csv, "LOAN", &m, Dedup::None, 0);
    let b = load(&csv, "LOAN", &m, Dedup::None, 1000);
    for q in ["MATCH (n) RETURN count(n)", "MATCH ()-[e]->() RETURN count(e)"] {
        let (x, y) = (call(&a, q).1, call(&b, q).1);
        assert_eq!(format!("{x:?}"), format!("{y:?}"), "{q}");
    }
    let q = "CALL uw.scorecard('LOAN','not.fully.paid=1','fico,int.rate','') YIELD *";
    assert_eq!(format!("{:?}", call(&a, q).1), format!("{:?}", call(&b, q).1));
}
