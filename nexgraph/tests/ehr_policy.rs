//! Data-policy tests on a real critical-care extract (the eICU demo patient table, 1,446 ICU stays).
//! No database server needed: the policy engine is exercised against an in-memory nexgraph database and
//! every answer is compared with a tally taken straight from the CSV.
#![cfg(feature = "security")]

use nexgraph::ingest::{import_csv_nodes, parse_csv};
use nexgraph::query::parser;
use nexgraph::query::{Params, QueryResult};
use nexgraph::security::policy::{Applied, Mask, Policy, Rule};
use nexgraph::security::Role;
use nexgraph::{Database, Error, Value};
use std::collections::{BTreeMap, HashSet};

const CSV: &str = include_str!("fixtures/eicu_demo_patient.csv");

fn db() -> Database {
    let db = Database::in_memory();
    let r = import_csv_nodes(&db, "Stay", CSV, Some("patientunitstayid")).unwrap();
    assert_eq!(r.nodes_created, table().1.len());
    db
}

fn table() -> (Vec<String>, Vec<Vec<String>>) {
    let mut it = parse_csv(CSV, ',').unwrap().into_iter().map(|(_, r)| r);
    let h = it.next().unwrap();
    (h, it.collect())
}

fn sv(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn col(name: &str) -> Vec<String> {
    let (h, rows) = table();
    let i = h.iter().position(|c| c == name).unwrap();
    rows.into_iter().map(|r| r[i].clone()).collect()
}

fn policy() -> Policy {
    Policy {
        rules: vec![Rule {
            up_to: Some(Role::Writer),
            deny: vec!["Stay.hospitalid".into(), "wardid".into()],
            mask: vec![
                ("uniquepid".into(), Mask::Pseudonym),
                ("patienthealthsystemstayid".into(), Mask::Redact),
                ("age".into(), Mask::TopCode(80.0)),
                ("admissionweight".into(), Mask::Bucket(10.0)),
            ],
            deny_labels: vec!["Note".into()],
        }],
        min_cell: 11,
        min_cell_up_to: Some(Role::Reader),
        pseudonym_key: b"0123456789abcdef0123456789abcdef".to_vec(),
        summary_procedures: vec![],
    }
}

fn run(db: &Database, p: &Policy, role: Role, q: &str) -> Result<(QueryResult, Applied), Error> {
    run_with(db, p, role, q, &Params::new())
}

fn run_with(db: &Database, p: &Policy, role: Role, q: &str, params: &Params) -> Result<(QueryResult, Applied), Error> {
    let st = parser::parse(q)?;
    let plan = p.check(role, &st, params)?;
    let r = db.query_with(q, params)?;
    p.apply(&plan, r)
}

fn denied(r: Result<(QueryResult, Applied), Error>) -> String {
    match r {
        Err(Error::Denied(m)) => m,
        Err(e) => panic!("expected Denied, got error {e}"),
        Ok((r, _)) => panic!("expected Denied, but the query ran and returned {} rows: {:?}", r.rows.len(), r.rows.iter().take(3).collect::<Vec<_>>()),
    }
}

#[test]
fn aggregate_only_role_gets_groups_of_at_least_k_and_the_counts_are_right() {
    let (db, p) = (db(), policy());
    let (r, a) = run(&db, &p, Role::Reader, "MATCH (s:Stay) RETURN s.gender AS g, count(*) AS n ORDER BY n DESC").unwrap();
    // independent tally from the file (an empty cell is a missing property, i.e. null, in the graph)
    let mut tally: BTreeMap<String, i64> = BTreeMap::new();
    for g in col("gender") {
        *tally.entry(g).or_default() += 1;
    }
    let expect: BTreeMap<String, i64> = tally.iter().filter(|(_, n)| **n >= 11).map(|(k, n)| (k.clone(), *n)).collect();
    let got: BTreeMap<String, i64> = r.rows.iter().map(|row| (sv(&row[0]), match &row[1] { Value::Int(n) => *n, v => panic!("{v:?}") })).collect();
    assert_eq!(got, expect);
    assert_eq!(a.suppressed_rows, tally.len() - expect.len());
    assert!(a.suppressed_rows >= 2, "'Unknown' (2 stays) and missing (3) are below 11 and must be suppressed: {tally:?}");
    assert!(r.rows.iter().all(|row| matches!(row[1], Value::Int(n) if n >= 11)));
}

#[test]
fn record_level_and_individual_value_queries_are_refused_for_aggregate_only_roles() {
    let (db, p) = (db(), policy());
    for q in [
        "MATCH (s:Stay) RETURN s.age",
        "MATCH (s:Stay) RETURN s",
        "MATCH (s:Stay) RETURN s.gender, max(s.unitvisitnumber) AS m, count(*) AS n",
        "MATCH (s:Stay) RETURN s.gender, min(s.unitvisitnumber) AS m, count(*) AS n",
        "MATCH (s:Stay) RETURN collect(s.gender) AS g, count(*) AS n",
        "MATCH (s:Stay) RETURN s.gender, avg(s.unitvisitnumber) AS a", // aggregate but no count(*) to suppress small groups
        "MATCH (s:Stay) RETURN s.gender, count(*) + max(s.unitvisitnumber) AS n",
        // a masked column is for display only: averaging it would compute on the raw, un-top-coded values
        "MATCH (s:Stay) RETURN s.gender, avg(s.age) AS a, count(*) AS n",
    ] {
        denied(run(&db, &p, Role::Reader, q));
    }
    // a reader may still ask a legitimate aggregate question about an unmasked column
    let (r, _) = run(&db, &p, Role::Reader, "MATCH (s:Stay) RETURN s.gender AS g, avg(s.unitvisitnumber) AS a, count(*) AS n").unwrap();
    assert_eq!(r.rows.len(), 2, "Female and Male only; the groups of 2 and 3 are suppressed");
}

#[test]
fn denied_properties_cannot_be_read_or_used_to_filter_group_or_sort() {
    let (db, p) = (db(), policy());
    for role in [Role::Reader, Role::Writer] {
        for q in [
            "MATCH (s:Stay) RETURN s.hospitalid AS h, count(*) AS n",
            "MATCH (s:Stay) WHERE s.hospitalid = 73 RETURN count(*) AS n",
            "MATCH (s:Stay {hospitalid: 73}) RETURN count(*) AS n",
            "MATCH (s:Stay) WHERE s.unitvisitnumber > 0 AND s.wardid < 90 RETURN count(*) AS n",
            "MATCH (s:Stay) RETURN s.gender AS g, count(*) AS n ORDER BY s.hospitalid",
            "MATCH (s:Stay) RETURN count(s.hospitalid) AS n",
            "MATCH (s:Stay) WHERE s.HospitalID = 73 RETURN count(*) AS n", // case
            "MATCH (s:Stay) WHERE s.`hospitalid` = 73 RETURN count(*) AS n", // quoted identifier
            "MATCH (a:Stay), (b) WHERE b.wardid = 87 RETURN count(*) AS n", // unlabelled variable: conservative match
            "AS OF 1 MATCH (s:Stay) RETURN s.hospitalid, count(*) AS n", // time travel does not bypass
        ] {
            let m = denied(run(&db, &p, role, q));
            assert!(m.contains("not available") || m.contains("hospitalid") || m.contains("wardid"), "{q} -> {m}");
        }
    }
}

#[test]
fn procedure_arguments_that_name_protected_columns_are_refused() {
    let (db, p) = (db(), policy());
    let mut params = Params::new();
    params.insert("p".into(), Value::Str("hospitalid".into()));
    params.insert("many".into(), Value::List(vec![Value::Str("age".into()), Value::Str("Stay.hospitalid".into())]));
    for q in [
        "CALL stats.describe('Stay', 'hospitalid') YIELD count RETURN count",
        "CALL stats.describe('Stay', $p) YIELD count RETURN count",
        "CALL uw.audit('Stay', 'x', 'age,wardid') YIELD feature RETURN feature",
        "CALL uw.audit('Stay', 'x', $many) YIELD feature RETURN feature",
        "CALL uw.scorecard('Stay', 'x', 'age;Stay.hospitalid', 'patientunitstayid') YIELD term RETURN term",
        "CALL stats.describe('Note', 'text') YIELD count RETURN count", // denied label
    ] {
        denied(run_with(&db, &p, Role::Writer, q, &params));
    }
}

#[test]
fn masked_properties_are_returned_masked_and_cannot_be_probed() {
    let (db, p) = (db(), policy());
    let (r, a) = run(&db, &p, Role::Writer, "MATCH (s:Stay) RETURN s.uniquepid AS pid, s.age AS age, s.admissionweight AS w, s.patienthealthsystemstayid AS hs").unwrap();
    assert_eq!(r.rows.len(), table().1.len());
    let raw_pids: HashSet<String> = col("uniquepid").into_iter().collect();
    let text = r.to_csv_raw();
    for pid in raw_pids.iter().take(2000) {
        assert!(!text.contains(pid.as_str()), "raw patient id {pid} leaked");
    }
    assert!(a.masked_cells > 0);
    // pseudonyms: stable, joinable, one per real patient (1091 distinct patients in the file)
    let tokens: HashSet<String> = r.rows.iter().map(|row| sv(&row[0])).collect();
    assert_eq!(tokens.len(), raw_pids.len(), "pseudonymisation must be one-to-one on this data");
    assert!(r.rows.iter().all(|row| sv(&row[0]).starts_with("p_")));
    // top-code at 80 and bucket(10)
    let max_age = r.rows.iter().filter_map(|row| if let Value::Int(i) = row[1] { Some(i) } else { None }).max().unwrap();
    let raw_max = col("age").iter().filter_map(|a| a.parse::<i64>().ok()).max().unwrap();
    assert!(raw_max > 80 && max_age == 80, "raw max {raw_max}, masked max {max_age}");
    // "> 89" is text, not a number: a numeric mask must not pass it through
    assert!(col("age").iter().any(|a| a == "> 89"));
    assert!(r.rows.iter().all(|row| !matches!(&row[1], Value::Str(s) if s != "[redacted]")), "no raw age text may survive");
    assert!(r.rows.iter().all(|row| match row[2] { Value::Float(f) => f % 10.0 == 0.0, Value::Null => true, Value::Int(i) => i % 10 == 0, _ => false }));
    assert!(r.rows.iter().all(|row| matches!(&row[3], Value::Str(s) if s == "[redacted]") || row[3] == Value::Null));
    // the masked value cannot be used to look a person up, sort by, or hide inside an expression
    for q in [
        "MATCH (s:Stay) WHERE s.uniquepid = '002-10665' RETURN count(*) AS n",
        "MATCH (s:Stay {uniquepid: '002-10665'}) RETURN s.age",
        "MATCH (s:Stay) WHERE s.age > 85 RETURN s.gender",
        "MATCH (s:Stay) RETURN s.gender ORDER BY s.uniquepid",
        "MATCH (s:Stay) RETURN lower(s.uniquepid) AS x",
        "MATCH (s:Stay) RETURN collect(s.uniquepid) AS x",
        "MATCH (s:Stay) SET s.copy = s.uniquepid",
    ] {
        denied(run(&db, &p, Role::Writer, q));
    }
    // counting distinct patients is fine and reveals no value
    let (r, _) = run(&db, &p, Role::Writer, "MATCH (s:Stay) RETURN count(DISTINCT s.uniquepid) AS patients").unwrap();
    assert_eq!(r.rows[0][0], Value::Int(raw_pids.len() as i64));
}

#[test]
fn tenant_admin_is_outside_the_rules_and_an_empty_policy_changes_nothing() {
    let (db, p) = (db(), policy());
    let (r, a) = run(&db, &p, Role::TenantAdmin, "MATCH (s:Stay) WHERE s.hospitalid = 73 RETURN s.uniquepid AS pid LIMIT 3").unwrap();
    assert_eq!(a, Applied::default());
    assert_eq!(r.rows.len(), 3);
    assert!(r.rows.iter().all(|row| col("uniquepid").contains(&sv(&row[0]))));
    let empty = Policy::default();
    assert!(!empty.restricts(Role::Reader));
    run(&db, &empty, Role::Reader, "MATCH (s:Stay) RETURN s.hospitalid LIMIT 1").unwrap();
}

#[test]
fn writes_cannot_launder_protected_values_into_new_properties() {
    let (db, p) = (db(), policy());
    for q in [
        "MATCH (s:Stay) SET s.h = s.hospitalid",
        "MATCH (s:Stay) SET s.hospitalid = 1",
        "CREATE (:Stay {wardid: 5})",
        "CREATE INDEX ON :Stay(hospitalid)",
        "CREATE (:Note {text: 'x'})",
    ] {
        denied(run(&db, &p, Role::Writer, q));
    }
}

#[test]
fn pseudonym_masks_without_a_key_fail_closed() {
    let db = db();
    let mut p = policy();
    p.pseudonym_key.clear();
    let e = run(&db, &p, Role::Writer, "MATCH (s:Stay) RETURN s.uniquepid AS pid LIMIT 1").unwrap_err();
    assert!(matches!(e, Error::Denied(_)), "{e}");
}

/// Documents a limit rather than a guarantee: allowed aggregate answers can be subtracted to reveal a group
/// smaller than k. Closing it needs a query budget or noise (differential privacy), which this policy does not
/// provide. If that is ever added, this test should start failing and be inverted.
#[test]
fn known_limit_differencing_is_not_prevented() {
    let (db, p) = (db(), policy());
    let one = |q: &str| match &run(&db, &p, Role::Reader, q).unwrap().0.rows[..] {
        [row] => match row[0] {
            Value::Int(n) => n,
            _ => panic!(),
        },
        other => panic!("one row expected, got {other:?}"),
    };
    // asked directly, the 2-stay group 'Unknown' is suppressed ...
    let (direct, _) = run(&db, &p, Role::Reader, "MATCH (s:Stay) WHERE s.gender = 'Unknown' RETURN count(*) AS n").unwrap();
    assert!(direct.rows.is_empty());
    // ... but three allowed questions give its size away
    let nonnull = one("MATCH (s:Stay) RETURN count(s.gender) AS n");
    let f = one("MATCH (s:Stay) WHERE s.gender = 'Female' RETURN count(*) AS n");
    let m = one("MATCH (s:Stay) WHERE s.gender = 'Male' RETURN count(*) AS n");
    assert_eq!(nonnull - f - m, 2);
}

#[test]
fn json_policy_round_trip_and_validation() {
    let p = Policy::from_json(
        r#"{"min_cell": 11, "min_cell_up_to": "reader", "pseudonym_key_hex": "00112233445566778899aabbccddeeff",
            "rules": [{"up_to": "writer", "deny": ["Stay.hospitalid"], "deny_labels": ["Note"],
                       "mask": {"uniquepid": "pseudonym", "age": "top_code:89", "weight": "bucket:5", "x": "redact"}}]}"#,
    )
    .unwrap();
    assert_eq!(p.min_cell, 11);
    assert_eq!(p.rules[0].mask.len(), 4);
    assert!(p.restricts(Role::Reader) && p.restricts(Role::Writer) && !p.restricts(Role::TenantAdmin));
    for bad in [
        r#"{"rules":[{"mask":{"a":"shuffle"}}]}"#,
        r#"{"rules":[{"mask":{"a":"pseudonym"}}]}"#, // no key
        r#"{"rules":[{"mask":{"a":"bucket:x"}}]}"#,
        r#"{"rules":[{"up_to":"root"}]}"#,
        r#"{"pseudonym_key_hex":"abc"}"#,
        "not json",
    ] {
        assert!(Policy::from_json(bad).is_err(), "{bad}");
    }
}
