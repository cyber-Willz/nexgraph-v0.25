//! v0.12 regression tests: `land.*` procedures and the qualified-key / string-column importer options.
//!
//! The small tests use tables whose expected numbers were worked out by hand. `real_registry` loads the 1,000-record
//! Tamil Nadu land registry extract in `tests/fixtures/tn_land_registry_1000.csv` and replays the cases in
//! `tests/fixtures/land_expected.json`, produced by `tests/fixtures/land_expected.py` with pandas / numpy / scipy
//! (regenerate: `python3 tests/fixtures/land_expected.py tests/fixtures/tn_land_registry_1000.csv tests/fixtures/land_expected.json`).
#![cfg(feature = "ingest")]
use nexgraph::ingest::{import_csv_hyperedges_with, Dedup, HyperCsv, HyperOpts};
use nexgraph::timeutil::DateOrder;
use nexgraph::{Database, Value};

fn call(db: &Database, q: &str) -> (Vec<String>, Vec<Vec<Value>>) {
    let r = db.query(q).unwrap_or_else(|e| panic!("{q}: {e}"));
    (r.columns, r.rows)
}

fn cell(cols: &[String], row: &[Value], c: &str) -> Value {
    row[cols.iter().position(|x| x == c).unwrap_or_else(|| panic!("no column {c} in {cols:?}"))].clone()
}

fn close(a: f64, b: f64, what: &str) {
    assert!((a - b).abs() <= 1e-9 * b.abs().max(1.0), "{what}: got {a}, want {b}");
}

// ------------------------------------------------------------ small tables

/// id, district, taluk, village, survey, sub, owner, status, approval, value, area
const SMALL: &str = "id,district,taluk,village,survey,sub,owner,status,approval,value,area\n\
P1,D1,T1,V1,10/1,1,Ann,Active,Approved,1000,2\n\
P2,D1,T1,V1,10/2,3,Bob,Active,Rejected,3000,3\n\
P3,D1,T1,V1,10/2,3,Cid,Under Dispute,Pending,500,1\n\
P4,D2,T1,V1,10/2,3,Dee,Active,Approved,2000,4\n\
P5,D2,T1,V1,11,2,Eve,Active,Pending,1500,1\n\
P6,D2,T1,V2,12A/04,4,Fay,Under Dispute,Approved,900,3\n\
P7,D2,T1,V2,x9/1,1,Gus,Pending Verification,Approved,0,2\n\
P8,D2,T1,V2,13/,1,Hal,Active,Approved,700,\n";

fn small(qualify: bool) -> Database {
    let db = Database::in_memory();
    let m = [("id", "Parcel"), ("district", "District"), ("taluk", "Taluk"), ("village", "Village"), ("owner", "Owner"), ("status", "Status")];
    let o = HyperCsv { edge_label: "PARCEL", members: &m, key_prop: "name", weight_col: None, time_cols: &[] };
    let q: &[(&str, &[&str])] = if qualify { &[("taluk", &["district"]), ("village", &["district", "taluk"])] } else { &[] };
    let r = import_csv_hyperedges_with(&db, &o, SMALL, &HyperOpts { dedup: Dedup::Key("id"), keep_member_props: true, qualify: q, string_cols: &["survey", "sub"], ..Default::default() }).unwrap();
    assert_eq!((r.rows_read, r.rows_skipped, r.edges_created), (8, 0, 8), "{:?}", r.first_error);
    db
}

fn names(db: &Database, label: &str) -> Vec<String> {
    let (_, rows) = call(db, &format!("MATCH (n:{label}) RETURN n.name AS name ORDER BY name"));
    rows.iter().map(|r| r[0].as_str().unwrap().to_string()).collect()
}

#[test]
fn unqualified_members_merge_same_named_places_across_parents() {
    let db = small(false);
    assert_eq!(names(&db, "Village"), ["V1", "V2"], "bare keys: one V1 node joins D1 and D2");
    assert_eq!(names(&db, "Taluk"), ["T1"]);
}

#[test]
fn qualified_members_separate_them() {
    let db = small(true);
    assert_eq!(names(&db, "Taluk"), ["D1/T1", "D2/T1"]);
    assert_eq!(names(&db, "Village"), ["D1/T1/V1", "D2/T1/V1", "D2/T1/V2"]);
    // the raw cell is still the edge property, so a plain filter keeps working
    let (_, rows) = call(&db, "MATCH ()-[e:PARCEL]-() WHERE e.village = 'V1' RETURN DISTINCT e.id AS id ORDER BY id");
    assert_eq!(rows.len(), 5, "P1..P5 are in a village named V1, in two districts");
}

#[test]
fn qualifier_errors_are_reported_before_importing() {
    let db = Database::in_memory();
    let m = [("id", "Parcel"), ("village", "Village")];
    let o = HyperCsv { edge_label: "P", members: &m, key_prop: "name", weight_col: None, time_cols: &[] };
    for (q, want) in [
        (&[("village", &["nope"][..])][..], "no column 'nope'"),
        (&[("district", &["village"][..])][..], "not a member column"),
        (&[("village", &["village"][..])][..], "cannot qualify itself"),
        (&[("owner", &["id"][..])][..], "'owner'"),
    ] {
        let e = import_csv_hyperedges_with(&db, &o, SMALL, &HyperOpts { qualify: q, ..Default::default() }).unwrap_err().to_string();
        assert!(e.contains(want), "{e}");
    }
    assert_eq!(db.query("MATCH (n) RETURN count(*) AS c").unwrap().rows[0][0], Value::Int(0), "nothing was written");
}

#[test]
fn empty_qualifier_cell_skips_the_row() {
    let db = Database::in_memory();
    let csv = "id,district,village\nP1,D1,V1\nP2,,V1\nP3,D2,V1\n";
    let m = [("id", "Parcel"), ("village", "Village")];
    let o = HyperCsv { edge_label: "P", members: &m, key_prop: "name", weight_col: None, time_cols: &[] };
    let q: &[(&str, &[&str])] = &[("village", &["district"])];
    let r = import_csv_hyperedges_with(&db, &o, csv, &HyperOpts { qualify: q, ..Default::default() }).unwrap();
    assert_eq!((r.edges_created, r.rows_skipped), (2, 1));
    assert!(r.first_error.unwrap().contains("qualifier column 'district' is empty"), "reason names the column");
}

#[test]
fn string_columns_stay_text_and_numbers_stay_numbers() {
    let db = small(true);
    let (_, rows) = call(&db, "MATCH ()-[e:PARCEL]-() WHERE e.id = 'P6' RETURN DISTINCT e.survey AS s, e.sub AS u, e.value AS v");
    assert_eq!(rows[0][0].as_str(), Some("12A/04"));
    assert_eq!(rows[0][1].as_str(), Some("4"), "a sub-division code is text");
    assert_eq!(rows[0][2], Value::Int(900), "a value is a number");
}

#[test]
fn conflicts_need_the_qualified_parcel_key() {
    let db = small(true);
    // survey 10/2 + sub 3 appears in D1 (Bob, Cid) and D2 (Dee). Without the district they look like one parcel with three owners
    // (P1 is 10/1 and P5 is 11, so they are not part of it).
    let (c, rows) = call(&db, "CALL land.conflicts('PARCEL','village,survey,sub','owner') YIELD *");
    assert_eq!(rows.len(), 1);
    assert_eq!((cell(&c, &rows[0], "records"), cell(&c, &rows[0], "distinct_owners")), (Value::Int(3), Value::Int(3)));
    assert_eq!(cell(&c, &rows[0], "owners").as_str(), Some("Bob; Cid; Dee"));
    assert_eq!(cell(&c, &rows[0], "parcel").as_str(), Some("V1/10/2/3"));
    assert_eq!(cell(&c, &rows[0], "kind").as_str(), Some("competing_owners"));
    // qualified: only the genuine D1 pair remains
    let (c, rows) = call(&db, "CALL land.conflicts('PARCEL','district,taluk,village,survey,sub','owner') YIELD *");
    assert_eq!(rows.len(), 1);
    assert_eq!(cell(&c, &rows[0], "parcel").as_str(), Some("D1/T1/V1/10/2/3"));
    assert_eq!((cell(&c, &rows[0], "records"), cell(&c, &rows[0], "distinct_owners")), (Value::Int(2), Value::Int(2)));
    assert_eq!(cell(&c, &rows[0], "owners").as_str(), Some("Bob; Cid"));
}

#[test]
fn repeated_record_is_not_a_title_conflict() {
    let db = Database::in_memory();
    let csv = "id,k,owner\nR1,a,Ann\nR2,a,Ann\nR3,b,Bob\n";
    let m = [("id", "Rec"), ("owner", "Owner")];
    let o = HyperCsv { edge_label: "P", members: &m, key_prop: "name", weight_col: None, time_cols: &[] };
    import_csv_hyperedges_with(&db, &o, csv, &HyperOpts { keep_member_props: true, ..Default::default() }).unwrap();
    let (c, rows) = call(&db, "CALL land.conflicts('P','k','owner') YIELD *");
    assert_eq!(rows.len(), 1);
    assert_eq!(cell(&c, &rows[0], "kind").as_str(), Some("repeated_record"));
    assert_eq!(cell(&c, &rows[0], "distinct_owners"), Value::Int(1));
}

#[test]
fn rule_check_counts_applicable_violations_and_samples() {
    let db = small(true);
    // Active: P1 Approved, P2 Rejected, P4 Approved, P5 Pending, P8 Approved -> 5 applicable, 2 violate (P2, P5)
    let (c, rows) = call(&db, "CALL land.rule_check('PARCEL','status=Active','approval=Approved',null,null,'id') YIELD *");
    let r = &rows[0];
    assert_eq!((cell(&c, r, "records"), cell(&c, r, "applicable"), cell(&c, r, "violations"), cell(&c, r, "unknown")), (Value::Int(8), Value::Int(5), Value::Int(2), Value::Int(0)));
    close(cell(&c, r, "violation_rate").as_f64().unwrap(), 0.4, "rate");
    // Wilson 95 % for 2 of 5, scipy.stats.binomtest(2, 5).proportion_ci(method='wilson')
    close(cell(&c, r, "wilson_low").as_f64().unwrap(), 0.11762077423264794, "wilson low");
    close(cell(&c, r, "wilson_high").as_f64().unwrap(), 0.769275718723987, "wilson high");
    assert_eq!(cell(&c, r, "sample_ids").as_str(), Some("P2, P5"));
    // by district: D1 has P1, P2 (1 of 2); D2 has P4, P5, P8 (1 of 3)
    let (c, rows) = call(&db, "CALL land.rule_check('PARCEL','status=Active','approval=Approved','district') YIELD *");
    let d1 = rows.iter().find(|r| cell(&c, r, "group").as_str() == Some("D1")).unwrap();
    let d2 = rows.iter().find(|r| cell(&c, r, "group").as_str() == Some("D2")).unwrap();
    assert_eq!((cell(&c, d1, "applicable"), cell(&c, d1, "violations")), (Value::Int(2), Value::Int(1)));
    assert_eq!((cell(&c, d2, "applicable"), cell(&c, d2, "violations")), (Value::Int(3), Value::Int(1)));
}

#[test]
fn rule_check_refuses_a_missing_column_and_never_calls_unknown_a_violation() {
    let db = small(true);
    let e = db.query("CALL land.rule_check('PARCEL','status=Active','nope=Approved') YIELD *").unwrap_err().to_string();
    assert!(e.contains("thenCond") && e.contains("nope"), "{e}");
    let e = db.query("CALL land.rule_check('PARCEL','status=Active','approval=Approved','nope') YIELD *").unwrap_err().to_string();
    assert!(e.contains("group key") && e.contains("nope"), "{e}");
}

#[test]
fn hotspots_z_and_flag() {
    // 30 records: office A 10/10 disputed, B 2/10, C 0/10 -> overall p0 = 12/30 = 0.4
    let mut csv = String::from("id,office,status\n");
    for i in 0..30 {
        let (o, d) = match i / 10 { 0 => ("A", true), 1 => ("B", i % 10 < 2), _ => ("C", false) };
        csv += &format!("R{i},{o},{}\n", if d { "Disputed" } else { "Ok" });
    }
    let db = Database::in_memory();
    let m = [("id", "Rec"), ("office", "Office")];
    let o = HyperCsv { edge_label: "R", members: &m, key_prop: "name", weight_col: None, time_cols: &[] };
    import_csv_hyperedges_with(&db, &o, &csv, &HyperOpts { keep_member_props: true, ..Default::default() }).unwrap();
    let (c, rows) = call(&db, "CALL land.hotspots('R','office','status=Disputed') YIELD *");
    assert_eq!(rows.len(), 3);
    assert_eq!(cell(&c, &rows[0], "group").as_str(), Some("A"), "highest z first");
    // z = (1 - 0.4) / sqrt(0.4 * 0.6 / 10) = 3.872983346207417; p = 2 * norm.sf(z) = 1.0752e-4
    close(cell(&c, &rows[0], "z").as_f64().unwrap(), 3.872983346207417, "z");
    let p = cell(&c, &rows[0], "p_value").as_f64().unwrap();
    assert!((p - 1.0752e-4).abs() < 1e-8, "{p}");
    assert_eq!(cell(&c, &rows[0], "flagged"), Value::Bool(true), "p < 0.05 / 3");
    assert_eq!(cell(&c, &rows[1], "group").as_str(), Some("B"));
    assert_eq!(cell(&c, &rows[1], "flagged"), Value::Bool(false));
    assert_eq!(cell(&c, &rows[2], "group").as_str(), Some("C"));
    // C: z = -0.4 / 0.1549 = -2.582, p = 0.00982 > 0.05 / 3? 0.0098 < 0.0167: flagged low
    assert_eq!(cell(&c, &rows[2], "flagged"), Value::Bool(true));
    // minN drops small groups
    let (_, rows) = call(&db, "CALL land.hotspots('R','office','status=Disputed',null,11) YIELD *");
    assert!(rows.is_empty());
}

#[test]
fn valuation_excludes_missing_and_nonpositive_and_fences_on_the_log_scale() {
    let db = small(true);
    // P7 has value 0 and P8 has no area: both excluded. Remaining value/area (P1..P6): 500, 1000, 500, 500, 1500, 300
    let (c, rows) = call(&db, "CALL land.valuation('PARCEL','value','area') YIELD *");
    let r = &rows[0];
    assert_eq!((cell(&c, r, "records"), cell(&c, r, "excluded")), (Value::Int(6), Value::Int(2)));
    // total 8900 / 14 area; quartiles of ln(value/area) by numpy.quantile, exponentiated
    close(cell(&c, r, "total_value").as_f64().unwrap(), 8900.0, "total value");
    close(cell(&c, r, "total_area").as_f64().unwrap(), 14.0, "total area");
    close(cell(&c, r, "weighted_value_per_area").as_f64().unwrap(), 635.7142857142857, "weighted");
    close(cell(&c, r, "median_value_per_area").as_f64().unwrap(), 500.0, "median");
        close(cell(&c, r, "p25").as_f64().unwrap(), 499.99999999999983, "p25");
    close(cell(&c, r, "p75").as_f64().unwrap(), 840.8964152537139, "p75");
}

#[test]
fn survey_check_flags_malformed_and_disagreeing_subdivisions() {
    let db = small(true);
    // 10/1 vs 1 same; 10/2 vs 3 differs (P2, P3, P4); 11 no suffix; 12A/04 vs 4 same (numeric compare); x9/1 malformed; 13/ malformed
    let (c, rows) = call(&db, "CALL land.survey_check('PARCEL','survey','sub',null,null,'id') YIELD *");
    let r = &rows[0];
    assert_eq!(
        [cell(&c, r, "records"), cell(&c, r, "malformed"), cell(&c, r, "without_subdivision"), cell(&c, r, "suffix_matches"), cell(&c, r, "suffix_differs")],
        [Value::Int(8), Value::Int(2), Value::Int(1), Value::Int(2), Value::Int(3)]
    );
    close(cell(&c, r, "differs_rate").as_f64().unwrap(), 0.6, "rate");
    assert_eq!(cell(&c, r, "sample_ids").as_str(), Some("P2, P3, P4, P7, P8"));
}

#[test]
fn quoted_identifiers_name_columns_with_spaces() {
    let db = Database::in_memory();
    let csv = "id,Land Area (Acres),Profit ($)\nA,2.5,10\nB,7,-3\n";
    let m = [("id", "Rec"), ("Land Area (Acres)", "Area")];
    let o = HyperCsv { edge_label: "R", members: &m, key_prop: "name", weight_col: None, time_cols: &[] };
    import_csv_hyperedges_with(&db, &o, csv, &HyperOpts { keep_member_props: true, ..Default::default() }).unwrap();
    let (_, rows) = call(&db, "MATCH ()-[e:R]-() WHERE e.`Land Area (Acres)` > 3 RETURN DISTINCT e.id AS id, e.`Profit ($)` AS p");
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0][0].as_str(), rows[0][1].clone()), (Some("B"), Value::Int(-3)));
    let (_, rows) = call(&db, "RETURN 1 AS `a b`");
    assert_eq!(rows[0][0], Value::Int(1));
    for bad in ["MATCH (n) RETURN n.`oops", "MATCH (n) RETURN n.``"] {
        assert!(db.query(bad).is_err(), "{bad}");
    }
    // a doubled backtick is a literal one
    db.query("CREATE (n:T {`we``ird`: 5})").unwrap();
    assert_eq!(call(&db, "MATCH (n:T) RETURN n.`we``ird` AS v").1[0][0], Value::Int(5));
}

// ------------------------------------------------------------- real data

fn registry() -> Database {
    let csv = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tn_land_registry_1000.csv")).unwrap();
    let db = Database::in_memory();
    let m = [
        ("Land Record ID", "Parcel"),
        ("District", "District"),
        ("Taluk", "Taluk"),
        ("Village", "Village"),
        ("Owner Name", "Owner"),
        ("Village Administrative Officer (VAO)", "VAO"),
        ("Revenue Inspector", "RI"),
        ("Record Status", "Status"),
        ("Tahsildar Approval Status", "Approval"),
    ];
    let o = HyperCsv { edge_label: "PARCEL", members: &m, key_prop: "name", weight_col: None, time_cols: &["Last Updated Date"] };
    let q: &[(&str, &[&str])] = &[("Taluk", &["District"]), ("Village", &["District", "Taluk"])];
    let r = import_csv_hyperedges_with(
        &db,
        &o,
        &csv,
        &HyperOpts { dedup: Dedup::Key("Land Record ID"), keep_member_props: true, date_order: DateOrder::DayFirst, qualify: q, string_cols: &["Owner Mobile Number", "Survey Number", "Sub Division"], chunk_rows: 0, delimiter: None, allow_single_member: false },
    )
    .unwrap();
    assert_eq!((r.rows_read, r.rows_skipped, r.edges_created, r.duplicates_skipped), (1000, 0, 1000, 0), "{:?}", r.first_error);
    db
}

#[test]
fn registry_geography_is_not_merged() {
    let db = registry();
    // the extract reuses Taluk-A..D under all 5 districts and Village-1..20 under every taluk: 4 and 20 bare names,
    // but 20 and 367 distinct places
    assert_eq!(names(&db, "Taluk").len(), 20);
    assert_eq!(names(&db, "Village").len(), 367);
    let (_, rows) = call(&db, "MATCH ()-[e:PARCEL]-() WHERE e.`Land Record ID` = 'TN-LAND-0001' RETURN DISTINCT e.`Owner Mobile Number` AS m");
    assert_eq!(rows[0][0].as_str(), Some("9597186965"), "a phone number is text");
    // 1000 timestamps parsed day-first: 02/05/2024 is 2 May, not 5 February
    let (_, rows) = call(&db, "MATCH ()-[e:PARCEL]-() WHERE e.`Land Record ID` = 'TN-LAND-0001' RETURN DISTINCT e.`Last Updated Date` AS t");
    close(rows[0][0].as_f64().unwrap(), 1714608000.0, "2024-05-02T00:00:00Z");
}

#[test]
fn registry_has_no_competing_owners_on_the_qualified_key_but_five_on_the_bare_one() {
    let db = registry();
    let (_, rows) = call(&db, "CALL land.conflicts('PARCEL','District,Taluk,Village,Survey Number,Sub Division','Owner Name') YIELD *");
    assert!(rows.is_empty(), "{rows:?}");
    // the same survey number in the same village *name* but different districts: not conflicts, and the bare key cannot tell
    let (c, rows) = call(&db, "CALL land.conflicts('PARCEL','Village,Survey Number','Owner Name') YIELD *");
    assert_eq!(rows.len(), 5);
    assert!(rows.iter().all(|r| cell(&c, r, "kind").as_str() == Some("competing_owners")));
}

#[test]
fn real_registry() {
    let db = registry();
    let cases: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/land_expected.json")).unwrap()).unwrap();
    let mut n = 0;
    for c in cases.as_array().unwrap() {
        let q = c["nql"].as_str().unwrap();
        let (cols, rows) = call(&db, q);
        let want_where = c["where"].as_object().unwrap();
        let row = rows
            .iter()
            .find(|r| want_where.iter().all(|(k, v)| cell(&cols, r, k).as_str() == v.as_str()))
            .unwrap_or_else(|| panic!("no row {want_where:?} in {q}"));
        for (k, v) in c["expect"].as_object().unwrap() {
            let got = cell(&cols, row, k);
            let want = v.as_f64().unwrap();
            // p-values far from 1 are compared relatively; everything else to 1e-9
            close(got.as_f64().unwrap_or_else(|| panic!("{k} is {got:?} in {q}")), want, &format!("{k} {want_where:?} in {q}"));
        }
        n += 1;
    }
    assert_eq!(n, 125, "every case replayed");
    // the number of rows per call equals the groups python tested (tests dropped by minN, none missing)
    let (_, rows) = call(&db, "CALL land.hotspots('PARCEL','Village Administrative Officer (VAO)','Record Status=Under Dispute',null,10) YIELD *");
    assert_eq!(rows.len(), 49);
    let (_, rows) = call(&db, "CALL land.hotspots('PARCEL','District,Taluk','Record Status=Under Dispute',null,10) YIELD *");
    assert_eq!(rows.len(), 20);
}
