//! v0.10 regression tests: de-duplicating hyperedge import and the `killchain.*` procedures.
//!
//! The first group is self-contained: a 10-row incident table whose every expected number was computed
//! independently in pandas / scipy / statsmodels (Wilson intervals, `Table2x2` risk ratios, pooled z-test,
//! `StratifiedTable.riskratio_pooled`). The last test replays the same checks on the real 100,500-row
//! Enterprise Cyber Kill Chain file and needs `NG_KC_CSV` and `NG_KC_EXPECTED` (skipped otherwise).
#![cfg(feature = "ingest")]
use nexgraph::ingest::{import_csv_hyperedges, import_csv_hyperedges_with, Dedup, HyperCsv, HyperOpts};
use nexgraph::{Database, Value};

const SMALL: &str = "id,stage,vec,size,mfa,ok,loss,det
1,Recon,Email,S,1,0,100,10
2,Recon,Email,S,1,0,200,20
3,Recon,Email,S,0,1,300,30
4,Recon,Web,S,0,0,,40
5,Impact,Email,L,1,1,1000,50
6,Impact,Web,L,0,1,2000,60
7,Impact,Web,L,0,1,3000,
8,Impact,Web,L,1,0,500,80
9,Impact,Email,L,0,1,4000,90
10,Recon,Web,S,1,0,50,5
";

const M: [(&str, &str); 3] = [("stage", "Stage"), ("vec", "Vector"), ("size", "Size")];

fn load(csv: &str, opts: &HyperOpts) -> (Database, nexgraph::ingest::ImportReport) {
    let db = Database::in_memory();
    let r = import_csv_hyperedges_with(&db, &HyperCsv { edge_label: "INC", members: &M, key_prop: "name", weight_col: None, time_cols: &[] }, csv, opts).unwrap();
    (db, r)
}

fn small(keep: bool) -> Database {
    load(SMALL, &HyperOpts { dedup: Dedup::None, keep_member_props: keep, ..Default::default() }).0
}

fn call(db: &Database, q: &str) -> (Vec<String>, Vec<Vec<Value>>) {
    let r = db.query(q).unwrap_or_else(|e| panic!("{q}: {e}"));
    (r.columns, r.rows)
}

/// Row whose first cell is `key`, as (column, value) lookups.
struct Row(Vec<String>, Vec<Value>);
impl Row {
    fn f(&self, c: &str) -> f64 {
        let i = self.0.iter().position(|x| x == c).unwrap_or_else(|| panic!("no column {c} in {:?}", self.0));
        self.1[i].as_f64().unwrap_or_else(|| panic!("{c} is {:?}", self.1[i]))
    }
    fn null(&self, c: &str) -> bool {
        let i = self.0.iter().position(|x| x == c).unwrap();
        self.1[i] == Value::Null
    }
}
fn row(db: &Database, q: &str, key: &str) -> Row {
    let (cols, rows) = call(db, q);
    let r = rows.into_iter().find(|r| r[0].as_str() == Some(key)).unwrap_or_else(|| panic!("no row '{key}' in {q}"));
    Row(cols, r)
}

fn close(a: f64, b: f64, tol: f64, what: &str) {
    assert!((a - b).abs() <= tol * b.abs().max(1.0), "{what}: got {a}, want {b}");
}

// ------------------------------------------------------------------ import

#[test]
fn dedup_exact_row_skips_only_identical_repeats() {
    let csv = format!("{SMALL}2,Recon,Email,S,1,0,200,20\n5,Impact,Email,L,1,1,1000,50\n");
    let (db, r) = load(&csv, &HyperOpts { dedup: Dedup::ExactRow, keep_member_props: false, ..Default::default() });
    assert_eq!((r.rows_read, r.edges_created, r.duplicates_skipped, r.rows_skipped, r.key_conflicts), (12, 10, 2, 0, 0), "{r:?}");
    assert_eq!(db.query("MATCH (a:Stage)-[e:INC]-(b:Vector) RETURN count(DISTINCT e)").unwrap().rows[0][0], Value::Int(10));
    // default import keeps the repeats (back-compatible)
    let db2 = Database::in_memory();
    let r2 = import_csv_hyperedges(&db2, &HyperCsv { edge_label: "INC", members: &M, key_prop: "name", weight_col: None, time_cols: &[] }, &csv).unwrap();
    assert_eq!((r2.edges_created, r2.duplicates_skipped), (12, 0));
}

#[test]
fn dedup_by_key_counts_conflicts_and_keeps_the_first_record() {
    // id 2 repeated identically (duplicate), id 3 repeated with a different loss (conflict), id 11 whitespace-only variant of id 1
    let csv = format!("{SMALL}2,Recon,Email,S,1,0,200,20\n3,Recon,Email,S,0,1,999,30\n 1 ,Recon,Email,S,1,0,100,10\n");
    let (db, r) = load(&csv, &HyperOpts { dedup: Dedup::Key("id"), keep_member_props: true, ..Default::default() });
    assert_eq!((r.edges_created, r.duplicates_skipped, r.key_conflicts, r.rows_skipped), (10, 2, 1, 1), "{r:?}");
    assert!(r.first_error.as_deref().unwrap().contains("key '3'"), "{:?}", r.first_error);
    // the first record for id 3 won
    let (_, rows) = call(&db, "CALL stats.describe('INC','loss') YIELD * RETURN count, mean");
    assert_eq!(rows[0][0], Value::Int(9));
    close(rows[0][1].as_f64().unwrap(), (100.0 + 200.0 + 300.0 + 1000.0 + 2000.0 + 3000.0 + 500.0 + 4000.0 + 50.0) / 9.0, 1e-12, "mean loss");
}

#[test]
fn dedup_needs_an_existing_key_column() {
    let db = Database::in_memory();
    let e = import_csv_hyperedges_with(&db, &HyperCsv { edge_label: "INC", members: &M, key_prop: "name", weight_col: None, time_cols: &[] }, SMALL, &HyperOpts { dedup: Dedup::Key("nope"), keep_member_props: false, ..Default::default() }).unwrap_err();
    assert!(e.to_string().contains("nope"), "{e}");
    assert_eq!(db.stats().live_nodes, 0, "nothing written on a bad option");
}

#[test]
fn member_props_are_optional_and_both_forms_give_the_same_answers() {
    let (a, b) = (small(false), small(true));
    // without keep_member_props the member column is not an edge property...
    assert_eq!(call(&a, "MATCH (x:Vector)-[e:INC]-(y:Stage) WHERE e.vec = 'Email' RETURN count(DISTINCT e)").1[0][0], Value::Int(0));
    // ...with it, it is
    assert_eq!(call(&b, "MATCH (x:Vector)-[e:INC]-(y:Stage) WHERE e.vec = 'Email' RETURN count(DISTINCT e)").1[0][0], Value::Int(5));
    // and the kill-chain procedures resolve the key either way
    let by_label = "CALL killchain.exposure('INC','Vector','ok','loss') YIELD * RETURN group, incidents, total_loss";
    let by_prop = "CALL killchain.exposure('INC','vec','ok','loss') YIELD * RETURN group, incidents, total_loss";
    assert_eq!(call(&a, by_label).1, call(&b, by_prop).1);
    assert_eq!(call(&b, by_label).1, call(&b, by_prop).1);
    // a column name that exists only as a member column is reported, not silently empty
    let e = a.query(by_prop).unwrap_err().to_string();
    assert!(e.contains("groupKey 'vec'") && e.contains("member label"), "{e}");
}

// ------------------------------------------------------------------- procedures

#[test]
fn funnel_matches_pandas() {
    let db = small(false);
    let q = "CALL killchain.funnel('INC','Stage','ok',['Recon','Impact'],'loss','det') YIELD * RETURN stage, rank, incidents, share, reached_share, success_rate, mean_detect, mean_loss, total_loss";
    let r = row(&db, q, "Recon");
    assert_eq!((r.f("rank"), r.f("incidents")), (1.0, 5.0));
    close(r.f("reached_share"), 1.0, 1e-12, "reached recon");
    close(r.f("success_rate"), 0.2, 1e-12, "recon rate");
    close(r.f("mean_detect"), 21.0, 1e-12, "recon det");
    close(r.f("mean_loss"), 162.5, 1e-12, "recon loss"); // 4 of 5 rows carry a loss
    close(r.f("total_loss"), 650.0, 1e-12, "recon total");
    let r = row(&db, q, "Impact");
    close(r.f("reached_share"), 0.5, 1e-12, "reached impact");
    close(r.f("success_rate"), 0.8, 1e-12, "impact rate");
    close(r.f("mean_detect"), 70.0, 1e-12, "impact det"); // one missing detect value is excluded, not zeroed
    close(r.f("mean_loss"), 2100.0, 1e-12, "impact loss");
    // no order: ranked by ascending success rate -> Recon first
    let (_, rows) = call(&db, "CALL killchain.funnel('INC','Stage','ok') YIELD * RETURN stage, rank");
    assert_eq!((rows[0][0].as_str(), &rows[0][1]), (Some("Recon"), &Value::Int(1)));
}

#[test]
fn control_effect_matches_statsmodels() {
    let db = small(false);
    let q = "CALL killchain.control_effect('INC','mfa','ok') YIELD * RETURN group, method, n_with, n_without, risk_with, risk_without, abs_reduction, relative_risk, rr_low, rr_high, p_value, n_dropped";
    let r = row(&db, q, "(all)");
    assert_eq!((r.f("n_with"), r.f("n_without"), r.f("n_dropped")), (5.0, 5.0, 0.0));
    close(r.f("risk_with"), 0.2, 1e-12, "risk_with");
    close(r.f("risk_without"), 0.8, 1e-12, "risk_without");
    close(r.f("abs_reduction"), 0.6, 1e-12, "abs_reduction");
    close(r.f("relative_risk"), 0.25, 1e-12, "rr");
    close(r.f("rr_low"), 0.04103656131972161, 1e-9, "rr_low");
    close(r.f("rr_high"), 1.5230320960144235, 1e-9, "rr_high");
    close(r.f("p_value"), 0.057779571123597204, 1e-9, "p");
    // stratified by vector: per-stratum rows and a Mantel-Haenszel row
    let q = "CALL killchain.control_effect('INC','mfa','ok','Vector') YIELD * RETURN group, method, n_with, n_without, risk_with, risk_without, abs_reduction, relative_risk, rr_low, rr_high, p_value, n_dropped";
    let (_, rows) = call(&db, q);
    assert_eq!(rows.len(), 4, "crude + Email + Web + pooled");
    let email = row(&db, q, "Email");
    assert_eq!((email.f("n_with"), email.f("n_without")), (3.0, 2.0));
    let web = row(&db, q, "Web");
    assert_eq!((web.f("n_with"), web.f("n_without"), web.f("risk_with")), (2.0, 3.0, 0.0));
    assert!(web.null("relative_risk"), "no events among the treated: ratio undefined, not infinite or zero");
    let mh = row(&db, q, "(pooled)");
    close(mh.f("relative_risk"), 0.2, 1e-12, "MH rr");
    close(mh.f("rr_low"), 0.028172698818643496, 1e-9, "MH low");
    close(mh.f("rr_high"), 1.4198142768462672, 1e-9, "MH high");
    close(mh.f("p_value"), 0.10752062090332627, 1e-9, "MH p");
}

#[test]
fn control_effect_refuses_a_control_with_no_variation_and_names_missing_keys() {
    let db = small(false);
    let (_, rows) = call(&db, "CALL killchain.exposure('INC','Vector','ok','loss') YIELD * RETURN group");
    assert_eq!(rows.len(), 2);
    let e = db.query("CALL killchain.control_effect('INC','nope','ok') YIELD * RETURN group").unwrap_err().to_string();
    assert!(e.contains("controlKey 'nope'") && e.contains("first event has properties"), "{e}");
    let e = db.query("CALL killchain.funnel('NOPE','stage','ok') YIELD * RETURN stage").unwrap_err().to_string();
    assert!(e.contains("no edges or hyperedges labelled 'NOPE'"), "{e}");
    // a constant control
    let csv = "id,stage,vec,mfa,ok\n1,A,X,1,0\n2,A,Y,1,1\n3,B,X,1,0\n";
    let db2 = Database::in_memory();
    import_csv_hyperedges(&db2, &HyperCsv { edge_label: "INC", members: &[("stage", "Stage"), ("vec", "Vector")], key_prop: "name", weight_col: None, time_cols: &[] }, csv).unwrap();
    let e = db2.query("CALL killchain.control_effect('INC','mfa','ok') YIELD * RETURN group").unwrap_err().to_string();
    assert!(e.contains("takes only one value"), "{e}");
}

#[test]
fn exposure_matches_pandas() {
    let db = small(false);
    let q = "CALL killchain.exposure('INC','Vector','ok','loss') YIELD * RETURN group, incidents, successes, success_rate, ci_low, ci_high, loss_n, mean_loss, median_loss, p90_loss, total_loss, loss_share";
    let r = row(&db, q, "Email");
    assert_eq!((r.f("incidents"), r.f("successes"), r.f("loss_n")), (5.0, 3.0, 5.0));
    close(r.f("success_rate"), 0.6, 1e-12, "rate");
    close(r.f("ci_low"), 0.2307242812760129, 1e-9, "wilson low");
    close(r.f("ci_high"), 0.8823792257673522, 1e-9, "wilson high");
    close(r.f("mean_loss"), 1120.0, 1e-12, "mean");
    close(r.f("median_loss"), 300.0, 1e-12, "median");
    close(r.f("p90_loss"), 2800.0, 1e-12, "p90 (linear interpolation, as numpy)");
    close(r.f("total_loss"), 5600.0, 1e-12, "total");
    close(r.f("loss_share"), 0.5022421524663677, 1e-12, "share");
    let w = row(&db, q, "Web");
    assert_eq!(w.f("loss_n"), 4.0, "one incident has no loss recorded");
    close(w.f("mean_loss"), 1387.5, 1e-12, "web mean over recorded losses only");
    // ordered by total loss, largest first; minN filters
    assert_eq!(call(&db, q).1[0][0].as_str(), Some("Email"));
    assert_eq!(call(&db, "CALL killchain.exposure('INC','Vector','ok','loss',6) YIELD * RETURN group").1.len(), 0);
}

#[test]
fn response_and_defense_depth_match_pandas() {
    let db = small(false);
    let q = "CALL killchain.response('INC','Vector','det','det') YIELD * RETURN group, incidents, detect_n, mttd_mean, mttd_p50, mttd_p90, respond_n, mttr_mean, mttr_p50, mttr_p90";
    let w = row(&db, q, "Web");
    assert_eq!((w.f("incidents"), w.f("detect_n")), (5.0, 4.0));
    close(w.f("mttd_mean"), 46.25, 1e-12, "mean");
    close(w.f("mttd_p50"), 50.0, 1e-12, "p50");
    close(w.f("mttd_p90"), 74.0, 1e-12, "p90");
    let q = "CALL killchain.defense_depth('INC',['mfa'],'ok','loss') YIELD * RETURN controls_present, incidents, success_rate, ci_low, ci_high, mean_loss";
    let (cols, rows) = call(&db, q);
    let none = Row(cols.clone(), rows[0].clone());
    let one = Row(cols, rows[1].clone());
    assert_eq!((none.f("controls_present"), none.f("incidents")), (0.0, 5.0));
    close(none.f("success_rate"), 0.8, 1e-12, "no control");
    close(none.f("ci_low"), 0.3755346297625252, 1e-9, "wilson low");
    close(none.f("mean_loss"), 2325.0, 1e-12, "mean loss no control");
    close(one.f("success_rate"), 0.2, 1e-12, "with control");
    close(one.f("ci_high"), 0.6244653702374748, 1e-9, "wilson high");
    let e = db.query("CALL killchain.defense_depth('INC',[],'ok') YIELD * RETURN controls_present").unwrap_err().to_string();
    assert!(e.contains("non-empty list"), "{e}");
}

#[test]
fn procedures_follow_time_travel() {
    let db = small(false);
    let ts = db.last_ts();
    import_csv_hyperedges(&db, &HyperCsv { edge_label: "INC", members: &M, key_prop: "name", weight_col: None, time_cols: &[] }, "id,stage,vec,size,mfa,ok,loss,det\n11,Impact,Email,L,0,1,9000,10\n").unwrap();
    let now = call(&db, "CALL killchain.exposure('INC','Vector','ok','loss') YIELD * RETURN group, incidents");
    let then = call(&db, &format!("AS OF {ts} CALL killchain.exposure('INC','Vector','ok','loss') YIELD * RETURN group, incidents"));
    let n = |r: &(Vec<String>, Vec<Vec<Value>>)| r.1.iter().find(|x| x[0].as_str() == Some("Email")).unwrap()[1].clone();
    assert_eq!((n(&then), n(&now)), (Value::Int(5), Value::Int(6)));
}

// ------------------------------------------------------------------- real file

#[test]
fn enterprise_kill_chain_file_matches_independent_computation() {
    let (Ok(csv), Ok(exp)) = (std::env::var("NG_KC_CSV"), std::env::var("NG_KC_EXPECTED")) else { eprintln!("skipped"); return };
    let e: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(exp).unwrap()).unwrap();
    let csv = std::fs::read_to_string(csv).unwrap();
    let g = |v: &serde_json::Value| v.as_f64().unwrap();
    let m = [("Industry", "Industry"), ("Country", "Country"), ("Company_Size", "Size"), ("Attack_Vector", "Vector"), ("Threat_Actor", "Actor"), ("Attack_Stage", "Stage"), ("Password_Policy", "PwPolicy"), ("Compliance", "Compliance")];
    let h = HyperCsv { edge_label: "INCIDENT", members: &m, key_prop: "name", weight_col: None, time_cols: &["Timestamp"] };

    // 1. both dedup modes find exactly the 500 repeats, none conflicting
    let t = std::time::Instant::now();
    let by_key = Database::in_memory();
    let r = import_csv_hyperedges_with(&by_key, &h, &csv, &HyperOpts { dedup: Dedup::Key("Incident_ID"), keep_member_props: true, ..Default::default() }).unwrap();
    println!("import (dedup by Incident_ID, member props): {:?} -> {r:?}", t.elapsed());
    assert_eq!((r.rows_read as f64, r.edges_created as f64, r.duplicates_skipped as f64), (g(&e["raw_rows"]), g(&e["rows"]), g(&e["dups"])));
    assert_eq!((r.key_conflicts, r.rows_skipped), (0, 0));
    let by_row = Database::in_memory();
    let r2 = import_csv_hyperedges_with(&by_row, &h, &csv, &HyperOpts { dedup: Dedup::ExactRow, keep_member_props: false, ..Default::default() }).unwrap();
    assert_eq!((r2.edges_created, r2.duplicates_skipped), (r.edges_created, r.duplicates_skipped));
    // without dedup the repeats are double counted (the v0.9 behaviour)
    let raw = Database::in_memory();
    assert_eq!(import_csv_hyperedges(&raw, &h, &csv).unwrap().edges_created as f64, g(&e["raw_rows"]));
    let db = by_key;

    // 2. funnel
    let order = "['Reconnaissance','Initial Access','Execution','Persistence','Impact']";
    let fq = format!("CALL killchain.funnel('INCIDENT','Attack_Stage','Attack_Success',{order},'Financial_Loss_USD','Detection_Time_Min') YIELD * RETURN stage, rank, incidents, share, reached_share, success_rate, mean_detect, mean_loss, total_loss");
    for s in e["funnel"].as_array().unwrap() {
        let r = row(&db, &fq, s["stage"].as_str().unwrap());
        assert_eq!(r.f("incidents"), g(&s["incidents"]));
        for (c, k) in [("share", "share"), ("reached_share", "reached"), ("success_rate", "rate"), ("mean_detect", "mdet"), ("mean_loss", "mloss"), ("total_loss", "tloss")] {
            close(r.f(c), g(&s[k]), 1e-9, &format!("funnel {} {c}", s["stage"]));
        }
    }

    // 3. crude control effects for five controls, p-values compared on the log scale (they reach 1e-220)
    for (c, x) in e["crude"].as_object().unwrap() {
        let r = row(&db, &format!("CALL killchain.control_effect('INCIDENT','{c}','Attack_Success') YIELD * RETURN group, method, n_with, n_without, risk_with, risk_without, abs_reduction, relative_risk, rr_low, rr_high, p_value, n_dropped"), "(all)");
        assert_eq!((r.f("n_with"), r.f("n_without")), (g(&x["n1"]), g(&x["n0"])), "{c}");
        for (k, kk) in [("risk_with", "r1"), ("risk_without", "r0"), ("relative_risk", "rr"), ("rr_low", "lo"), ("rr_high", "hi")] {
            close(r.f(k), g(&x[kk]), 1e-9, &format!("{c} {k}"));
        }
        let (p, want) = (r.f("p_value"), g(&x["p"]));
        assert!(p.log10() - want.log10() < 1e-6 && want.log10() - p.log10() < 1e-6 * want.log10().abs().max(1.0), "{c} p: {p:e} vs {want:e}");
    }

    // 4. Mantel-Haenszel for MFA stratified by vector (a pre-treatment variable), and per-stratum rows
    let mq = "CALL killchain.control_effect('INCIDENT','MFA','Attack_Success','Attack_Vector') YIELD * RETURN group, method, n_with, n_without, risk_with, risk_without, abs_reduction, relative_risk, rr_low, rr_high, p_value, n_dropped";
    let mh = row(&db, mq, "(pooled)");
    let x = &e["mh_vector_mfa"];
    for (k, kk) in [("relative_risk", "rr"), ("rr_low", "lo"), ("rr_high", "hi")] {
        close(mh.f(k), g(&x[kk]), 1e-9, &format!("MH {k}"));
    }
    assert!((mh.f("p_value").log10() - g(&x["p"]).log10()).abs() < 1e-6 * g(&x["p"]).log10().abs());
    for (v, s) in x["strata"].as_object().unwrap() {
        let r = row(&db, mq, v);
        close(r.f("relative_risk"), g(&s["rr"]), 1e-9, &format!("stratum {v}"));
    }
    // the documented trap: stratifying by the stage the attack reached (a mediator) erases the effect
    let (_, rows) = call(&db, "CALL killchain.control_effect('INCIDENT','MFA','Attack_Success','Attack_Stage') YIELD * RETURN group, relative_risk");
    let pooled = rows.iter().find(|r| r[0].as_str() == Some("(pooled)")).unwrap()[1].as_f64().unwrap();
    close(pooled, g(&e["mh_stage_mfa_rr"]), 1e-9, "MH by stage");
    assert!(pooled > 0.95, "within-stage MH ratio should be near 1, got {pooled}");

    // 5. exposure by vector, response by EDR, defense depth
    let xq = "CALL killchain.exposure('INCIDENT','Attack_Vector','Attack_Success','Financial_Loss_USD') YIELD * RETURN group, incidents, successes, success_rate, ci_low, ci_high, loss_n, mean_loss, median_loss, p90_loss, total_loss, loss_share";
    for (v, s) in e["exposure_vector"].as_object().unwrap() {
        let r = row(&db, xq, v);
        assert_eq!((r.f("incidents"), r.f("successes"), r.f("loss_n")), (g(&s["n"]), g(&s["k"]), g(&s["ln"])), "{v}");
        for (k, kk) in [("success_rate", "rate"), ("ci_low", "lo"), ("ci_high", "hi"), ("mean_loss", "mean"), ("median_loss", "med"), ("p90_loss", "p90"), ("total_loss", "tot"), ("loss_share", "share")] {
            close(r.f(k), g(&s[kk]), 1e-9, &format!("exposure {v} {k}"));
        }
    }
    let rq = "CALL killchain.response('INCIDENT','EDR','Detection_Time_Min','Response_Time_Min') YIELD * RETURN group, incidents, detect_n, mttd_mean, mttd_p50, mttd_p90, respond_n, mttr_mean, mttr_p50, mttr_p90";
    for (v, s) in e["response_edr"].as_object().unwrap() {
        let r = row(&db, rq, v);
        assert_eq!((r.f("incidents"), r.f("detect_n"), r.f("respond_n")), (g(&s["n"]), g(&s["dn"]), g(&s["rn"])), "EDR={v}");
        for (k, kk) in [("mttd_mean", "dm"), ("mttd_p50", "dmed"), ("mttd_p90", "d90"), ("mttr_mean", "rm"), ("mttr_p50", "rmed"), ("mttr_p90", "r90")] {
            close(r.f(k), g(&s[kk]), 1e-9, &format!("response EDR={v} {k}"));
        }
    }
    let (cols, rows) = call(&db, "CALL killchain.defense_depth('INCIDENT',['Firewall','MFA','EDR','IDS','Security_Training'],'Attack_Success','Financial_Loss_USD') YIELD * RETURN controls_present, incidents, success_rate, ci_low, ci_high, mean_loss");
    assert_eq!(rows.len(), e["depth"].as_object().unwrap().len());
    for r in rows {
        let r = Row(cols.clone(), r);
        let s = &e["depth"][format!("{}", r.f("controls_present") as i64)];
        assert_eq!(r.f("incidents"), g(&s["n"]));
        for (k, kk) in [("success_rate", "rate"), ("ci_low", "lo"), ("ci_high", "hi"), ("mean_loss", "mloss")] {
            close(r.f(k), g(&s[kk]), 1e-9, &format!("depth {k}"));
        }
    }
    println!("all kill-chain aggregates match pandas / scipy / statsmodels");
}

#[test]
fn outcomes_may_be_category_tests() {
    let db = small(false);
    // 'stage=Impact' is true for 2 of 5 Email incidents and 3 of 5 Web incidents
    for expr in ["Stage=Impact", "Stage!=Recon"] {
        let q = format!("CALL killchain.exposure('INC','Vector','{expr}','loss') YIELD * RETURN group, incidents, successes, outcome_known, success_rate");
        let (email, web) = (row(&db, &q, "Email"), row(&db, &q, "Web"));
        assert_eq!((email.f("successes"), email.f("outcome_known")), (2.0, 5.0), "{expr}");
        assert_eq!((web.f("successes"), web.f("outcome_known")), (3.0, 5.0), "{expr}");
        close(web.f("success_rate"), 0.6, 1e-12, "web rate");
    }
    // the same expression works as a control (member label 'Stage')
    let q = "CALL killchain.control_effect('INC','Stage=Impact','ok') YIELD * RETURN group, method, n_with, n_without, risk_with, risk_without";
    let r = row(&db, q, "(all)");
    assert_eq!((r.f("n_with"), r.f("n_without")), (5.0, 5.0));
    close(r.f("risk_with"), 0.8, 1e-12, "impact success");
    close(r.f("risk_without"), 0.2, 1e-12, "recon success");
}

#[test]
fn a_continuous_column_is_refused_as_an_outcome_instead_of_silently_subsetting() {
    let db = small(false);
    let e = db.query("CALL killchain.exposure('INC','Vector','loss','loss') YIELD * RETURN group").unwrap_err().to_string();
    assert!(e.contains("outcomeKey 'loss'") && e.contains("only") && e.contains("Key!=Value"), "{e}");
    let e = db.query("CALL killchain.defense_depth('INC',['loss'],'ok') YIELD * RETURN controls_present").unwrap_err().to_string();
    assert!(e.contains("control 'loss'"), "{e}");
    let e = db.query("CALL killchain.funnel('INC','Stage','det') YIELD * RETURN stage").unwrap_err().to_string();
    assert!(e.contains("outcomeKey 'det'"), "{e}");
    // an unknown key behind a test expression still names the missing key
    let e = db.query("CALL killchain.exposure('INC','Vector','nope!=x','loss') YIELD * RETURN group").unwrap_err().to_string();
    assert!(e.contains("'nope'") && e.contains("not a property"), "{e}");
}
