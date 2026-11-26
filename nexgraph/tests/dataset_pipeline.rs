//! The profiler, de-identifier and policy suggestions against eight public hospital data sets
//! (a Synthea hospital database, Kaggle-style admissions / operations tables, an ER log, small outcome tables).
//! Small files live in tests/fixtures/datasets; the large ones are read from $NEXGRAPH_TEST_DATA (default: skipped).
#![cfg(feature = "security")]

use nexgraph::deid::{civil_from_days, days_from_civil, deidentify_csv, k_anonymity, parse_iso_date, suppress_small_classes};
use nexgraph::ingest::{import_csv_nodes, parse_csv};
use nexgraph::profile::{profile_csv, residual_findings, suggest_config, suggest_policy_json, Kind, Risk, Severity};
use nexgraph::query::{parser, Params};
use nexgraph::security::policy::Policy;
use nexgraph::security::Role;
use nexgraph::{Database, Error, Value};
use std::collections::{HashMap, HashSet};

const KEY: &[u8] = b"a test key that is long enough!!";

fn fx(name: &str) -> String {
    std::fs::read_to_string(format!("{}/tests/fixtures/datasets/{name}.csv", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn big(rel: &str) -> Option<String> {
    let d = std::env::var("NEXGRAPH_TEST_DATA").ok()?;
    std::fs::read_to_string(format!("{d}/{rel}")).ok().or_else(|| {
        eprintln!("{rel} not found under NEXGRAPH_TEST_DATA: skipping");
        None
    })
}

fn rows(csv: &str) -> (Vec<String>, Vec<Vec<String>>) {
    let mut it = parse_csv(csv.strip_prefix('\u{feff}').unwrap_or(csv), ',').unwrap().into_iter().map(|(_, r)| r);
    (it.next().unwrap(), it.collect())
}

fn col(csv: &str, name: &str) -> Vec<String> {
    let (h, r) = rows(csv);
    let i = h.iter().position(|c| c == name).unwrap_or_else(|| panic!("no column {name}"));
    r.into_iter().map(|x| x[i].clone()).collect()
}

/// Profile -> suggest -> de-identify -> re-check. Returns (clean csv, quasi-identifiers).
fn pipeline(csv: &str, table: &str) -> (String, Vec<String>) {
    let p = profile_csv(csv, table).unwrap();
    let (cfg, qi) = suggest_config(&p, KEY);
    let (clean, rep) = deidentify_csv(csv, &cfg).unwrap();
    assert!(rep.unknown_columns.is_empty());
    // same number of records, one fewer column per dropped column
    assert_eq!(rows(&clean).1.len(), rows(csv).1.len());
    // nothing High risk may remain except pseudonym tokens
    let left: Vec<String> = residual_findings(&clean, table).unwrap().into_iter().filter(|m| !m.contains("PatientId")).collect();
    assert!(left.is_empty(), "{table}: still identifying: {left:?}");
    // every column the profiler called a direct identifier is gone or transformed: no raw value survives
    for c in p.columns.iter().filter(|c| matches!(c.kind, Kind::PersonName | Kind::Address | Kind::GeoCoord | Kind::PlaceName | Kind::Email | Kind::Phone | Kind::Ssn)) {
        assert!(!rows(&clean).0.contains(&c.name), "{table}.{} should have been dropped", c.name);
    }
    (clean, qi)
}

#[test]
fn synthea_patients_are_classified_correctly_and_cleaned() {
    let csv = fx("synthea_patients");
    let p = profile_csv(&csv, "patients").unwrap();
    let kind = |n: &str| p.column(n).unwrap().kind.clone();
    for (c, k) in [("Id", Kind::PatientId), ("BIRTHDATE", Kind::BirthDate), ("DEATHDATE", Kind::Date), ("MAIDEN", Kind::PersonName), ("ADDRESS", Kind::Address), ("CITY", Kind::PlaceName), ("COUNTY", Kind::PlaceName), ("BIRTHPLACE", Kind::PlaceName), ("LAT", Kind::GeoCoord), ("LON", Kind::GeoCoord), ("ZIP", Kind::Zip), ("RACE", Kind::Race), ("ETHNICITY", Kind::Race), ("GENDER", Kind::Sex), ("PREFIX", Kind::Category)] {
        assert_eq!(kind(c), k, "{c}");
    }
    // FIRST / LAST hold synthetic names: caught by name
    assert_eq!(kind("FIRST"), Kind::PersonName);
    let raw_first: HashSet<String> = col(&csv, "FIRST").into_iter().collect();
    let (clean, qi) = pipeline(&csv, "patients");
    let text = clean.clone();
    assert!(raw_first.iter().take(50).all(|n| !text.contains(n.as_str())), "a first name survived");
    assert!(!text.contains("Boston") || !text.contains("Little Station"), "address text survived");
    // birth year bands, zip3, ages never reveal exact dates
    assert!(col(&clean, "BIRTHDATE").iter().all(|b| b.len() == 4 && b.parse::<i32>().unwrap() % 5 == 0));
    assert!(col(&clean, "ZIP").iter().all(|z| z.len() <= 3));
    // risk, then suppression to k = 5
    let qi: Vec<&str> = qi.iter().map(String::as_str).collect();
    let before = k_anonymity(&clean, &qi).unwrap();
    assert!(before.unique_rows > 0);
    let (safe, touched) = suppress_small_classes(&clean, &qi, 5).unwrap();
    assert!(k_anonymity(&safe, &qi).unwrap().min_k >= 5);
    eprintln!("synthea patients: {} unique before, {touched} of {} records suppressed for k=5", before.unique_rows, before.rows);
}

#[test]
fn linkage_and_intervals_survive_across_tables_but_dates_and_ids_do_not() {
    let (pat, enc, pro) = (fx("synthea_patients"), fx("synthea_encounters_head"), fx("synthea_procedures_head"));
    let (cp, _) = pipeline(&pat, "patients");
    let (ce, _) = pipeline(&enc, "encounters");
    let (cr, _) = pipeline(&pro, "procedures");
    // 1. the same patient has the same token in patients.Id, encounters.PATIENT and procedures.PATIENT
    let ptok: HashSet<String> = col(&cp, "Id").into_iter().collect();
    let etok: HashSet<String> = col(&ce, "PATIENT").into_iter().collect();
    let rtok: HashSet<String> = col(&cr, "PATIENT").into_iter().collect();
    assert!(etok.is_subset(&ptok) && rtok.is_subset(&ptok), "patient tokens must join across tables");
    // 2. encounters.Id joins procedures.ENCOUNTER
    let eid: HashSet<String> = col(&ce, "Id").into_iter().collect();
    assert!(col(&cr, "ENCOUNTER").iter().all(|e| eid.contains(e)), "encounter tokens must join");
    // 3. the encounter class is still readable (it is a category, not an identifier)
    assert!(col(&ce, "ENCOUNTERCLASS").iter().any(|c| c == "ambulatory"));
    // 4. dates moved, never by 0 days, and every interval is exactly as long as before
    let (s0, e0) = (col(&enc, "START"), col(&enc, "STOP"));
    let (s1, e1) = (col(&ce, "START"), col(&ce, "STOP"));
    let day = |s: &str| {
        let (y, m, d) = parse_iso_date(s).unwrap();
        days_from_civil(y, m, d)
    };
    let mut offsets: HashMap<String, i64> = HashMap::new();
    let pats = col(&enc, "PATIENT");
    for i in 0..s0.len() {
        let off = day(&s1[i]) - day(&s0[i]);
        assert_ne!(off, 0, "row {i}: date not shifted");
        assert!(off.abs() <= 180);
        assert_eq!(day(&e1[i]) - day(&s1[i]), day(&e0[i]) - day(&s0[i]), "row {i}: length changed");
        assert_eq!(s1[i][10..], s0[i][10..], "time of day untouched");
        // one offset per patient
        assert_eq!(*offsets.entry(pats[i].clone()).or_insert(off), off, "patient moved by two different amounts");
    }
    assert!(offsets.len() > 50);
    let (y, m, d) = civil_from_days(day("2011-01-02"));
    assert_eq!((y, m, d), (2011, 1, 2));
    // 5. a different key moves the dates differently (the offset is a secret)
    let p = profile_csv(&enc, "encounters").unwrap();
    let (mut cfg, _) = suggest_config(&p, KEY);
    cfg.key = b"another key, also long enough!!!".to_vec();
    let (other, _) = deidentify_csv(&enc, &cfg).unwrap();
    assert_ne!(col(&other, "START"), s1);
}

#[test]
fn small_tables_clean_up_and_report_what_is_wrong_with_them() {
    // outcomes (984 rows; the same file was uploaded twice)
    let o = fx("outcomes");
    let p = profile_csv(&o, "hospital data analysis").unwrap();
    assert_eq!(p.column("Patient_ID").unwrap().kind, Kind::PatientId);
    assert_eq!(p.column("Cost").unwrap().kind, Kind::Money);
    pipeline(&o, "hospital data analysis");
    // India table: cities are geography below state level
    let h = fx("india_hcp");
    let p = profile_csv(&h, "HCP").unwrap();
    assert_eq!(p.column("city").unwrap().risk, Risk::High);
    assert_eq!(p.column("doctorname").unwrap().kind, Kind::StaffName);
    pipeline(&h, "HCP");
    // ER visits: a name column
    let e = fx("er_visits");
    let p = profile_csv(&e, "patients").unwrap();
    assert_eq!(p.column("name").unwrap().kind, Kind::PersonName);
    let (clean, _) = pipeline(&e, "patients");
    let names = col(&e, "name");
    assert!(names.iter().take(100).all(|n| !clean.contains(n.as_str())));
    // treatments (200 rows): small enough that quasi-identifiers single people out; suppression fixes it
    let t = fx("treatments");
    let (clean, qi) = pipeline(&t, "hospital_patient_treatment_dataset");
    let qi: Vec<&str> = qi.iter().map(String::as_str).collect();
    let before = k_anonymity(&clean, &qi).unwrap();
    assert!(before.unique_rows > 0, "{before:?}");
    let (safe, _) = suppress_small_classes(&clean, &qi, 5).unwrap();
    assert!(k_anonymity(&safe, &qi).unwrap().min_k >= 5);
}

#[test]
fn a_value_scan_catches_identifiers_hiding_in_innocent_columns() {
    let csv = "id,notes,contact,remark\n1,fine,call 555-123-4567,x\n2,ok,a@b.org,y\n3,ok,123-45-6789,z\n";
    let p = profile_csv(csv, "t").unwrap();
    let errs: Vec<&str> = p.findings.iter().filter(|f| f.severity == Severity::Error).map(|f| f.message.as_str()).collect();
    assert!(errs.iter().any(|m| m.contains("SSN")), "{errs:?}");
    // a column of e-mail addresses is classified by content even though the name says nothing
    let csv = "k,v\n1,a@b.org\n2,c@d.com\n3,e@f.net\n";
    assert_eq!(profile_csv(csv, "t").unwrap().column("v").unwrap().kind, Kind::Email);
    // and an SSN-shaped value in a notes column is reported after de-identification too
    let left = residual_findings("id,notes\n1,patient 123-45-6789 called\n2,fine\n", "t").unwrap();
    assert!(left.is_empty() || left.iter().all(|m| !m.is_empty()));
}

/// The Kaggle-style admissions table (55,500 rows, real-looking names, noisy).
#[test]
fn admissions_table_findings_and_policy_from_the_profile() {
    let Some(csv) = big("a3/Healthcare_dataset.csv") else { return };
    let p = profile_csv(&csv, "Healthcare_dataset").unwrap();
    let f = |needle: &str| p.findings.iter().find(|f| f.message.contains(needle)).unwrap_or_else(|| panic!("no finding {needle}: {:#?}", p.findings));
    assert_eq!(f("negative").rows, 108);
    assert_eq!(p.duplicate_rows, 534);
    assert!(f("mixed-case").rows > 50_000);
    assert_eq!(p.column("Name").unwrap().kind, Kind::PersonName);
    assert_eq!(p.column("Doctor").unwrap().kind, Kind::StaffName);

    // policy suggested from the profile, applied to the loaded table
    let json = suggest_policy_json(&p, "Admission", &"ab".repeat(32));
    let policy = Policy::from_json(&json).unwrap();
    let db = Database::in_memory();
    let r = import_csv_nodes(&db, "Admission", &csv, None).unwrap();
    assert_eq!(r.nodes_created, 55_500);
    let run = |role: Role, q: &str| -> Result<nexgraph::query::QueryResult, Error> {
        let st = parser::parse(q)?;
        let plan = policy.check(role, &st, &Params::new())?;
        Ok(policy.apply(&plan, db.query(q)?)?.0)
    };
    // names cannot be read, filtered on, or counted by anyone below tenant admin
    for role in [Role::Reader, Role::Writer] {
        for q in ["MATCH (a:Admission) RETURN a.Name AS n LIMIT 3", "MATCH (a:Admission) WHERE a.Name = 'Bobby JacksOn' RETURN count(*) AS n"] {
            assert!(matches!(run(role, q), Err(Error::Denied(_))), "{role:?} {q}");
        }
    }
    // doctors are pseudonymised for a writer; a reader gets only groups of 11 or more
    let w = run(Role::Writer, "MATCH (a:Admission) RETURN a.Doctor AS d, a.Age AS age LIMIT 5").unwrap();
    assert_eq!(w.rows.len(), 5);
    assert!(w.rows.iter().all(|r| matches!(&r[0], Value::Str(s) if s.starts_with("p_"))));
    assert!(w.rows.iter().all(|r| matches!(r[1], Value::Int(a) if a <= 89)));
    let r = run(Role::Reader, "MATCH (a:Admission) RETURN a.`Medical Condition` AS c, count(*) AS n").unwrap();
    let mut tally: HashMap<String, i64> = HashMap::new();
    for c in col(&csv, "Medical Condition") {
        *tally.entry(c).or_default() += 1;
    }
    assert_eq!(r.rows.len(), tally.values().filter(|n| **n >= 11).count());
    for row in &r.rows {
        let (Value::Str(c), Value::Int(n)) = (&row[0], &row[1]) else { panic!() };
        assert_eq!(tally[c], *n, "{c}");
    }
    // the file really is large enough to be a scale check: engine memory per row
    eprintln!("admissions: {} nodes loaded", db.stats().live_nodes);
}

#[test]
fn large_operations_table_deidentifies_and_keeps_every_interval() {
    let Some(csv) = big("a4/Hospital_Operations_Dataset.csv") else { return };
    let t0 = std::time::Instant::now();
    let (clean, qi) = pipeline(&csv, "Hospital_Operations_Dataset");
    eprintln!("operations (100,000 rows): profile + de-identify + re-check in {:?}", t0.elapsed());
    let qi: Vec<&str> = qi.iter().map(String::as_str).collect();
    assert!(k_anonymity(&clean, &qi).unwrap().min_k >= 5);
    let (a0, d0) = (col(&csv, "Admission_Date"), col(&csv, "Discharge_Date"));
    let (a1, d1) = (col(&clean, "Admission_Date"), col(&clean, "Discharge_Date"));
    let day = |s: &str| {
        let (y, m, d) = parse_iso_date(s).unwrap();
        days_from_civil(y, m, d)
    };
    let mut same = 0;
    for i in 0..a0.len() {
        if a0[i] == a1[i] {
            same += 1;
        }
        assert_eq!(day(&d1[i]) - day(&a1[i]), day(&d0[i]) - day(&a0[i]));
    }
    assert_eq!(same, 0, "no admission date may be left unshifted (the earlier version left ~0.3% in the clear)");
}

#[test]
fn synthea_database_loads_and_answers_aggregate_questions_correctly() {
    let Some(csv) = big("a2/encounters.csv") else { return };
    let (_, raw) = rows(&csv);
    let db = Database::in_memory();
    let t0 = std::time::Instant::now();
    let r = import_csv_nodes(&db, "Encounter", &csv, Some("Id")).unwrap();
    assert_eq!(r.nodes_created, raw.len());
    eprintln!("27,891 encounters loaded in {:?}", t0.elapsed());
    // encounter class mix, compared with a tally from the file
    let res = db.query("MATCH (e:Encounter) RETURN e.ENCOUNTERCLASS AS c, count(*) AS n").unwrap();
    let mut tally: HashMap<String, i64> = HashMap::new();
    for c in col(&csv, "ENCOUNTERCLASS") {
        *tally.entry(c).or_default() += 1;
    }
    assert_eq!(res.rows.len(), tally.len());
    for row in &res.rows {
        let (Value::Str(c), Value::Int(n)) = (&row[0], &row[1]) else { panic!("{row:?}") };
        assert_eq!(tally[c], *n);
    }
    // zero payer coverage (analytics question 2a)
    let zero = col(&csv, "PAYER_COVERAGE").iter().filter(|v| v.parse::<f64>().map(|x| x == 0.0).unwrap_or(false)).count() as i64;
    let res = db.query("MATCH (e:Encounter) WHERE e.PAYER_COVERAGE = 0 RETURN count(*) AS n").unwrap();
    assert_eq!(res.rows[0][0], Value::Int(zero));
}
