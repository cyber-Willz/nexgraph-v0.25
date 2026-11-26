//! De-identification and re-identification risk on the eICU demo extract.
#![cfg(feature = "security")]

use nexgraph::deid::{deidentify_csv, k_anonymity, suppress_small_classes, Config, Rule};

const CSV: &str = include_str!("fixtures/eicu_demo_patient.csv");
const QI: [&str; 5] = ["gender", "age", "ethnicity", "hospitalid", "hospitaldischargeyear"];

#[test]
fn quasi_identifiers_single_people_out_and_suppression_fixes_it() {
    let before = k_anonymity(CSV, &QI).unwrap();
    eprintln!("before: {before:?}  unique {:.1}%", 100.0 * before.fraction_unique());
    // The file has no names, yet many stays are unique on five ordinary attributes.
    assert!(before.unique_rows > 100, "expected many unique rows, got {before:?}");
    assert_eq!(before.min_k, 1);

    // generalise first: top-code age at 89 (Safe Harbor) and drop the site, then measure again
    let cfg = Config { key: b"an example key of 32 bytes......".to_vec(), rules: vec![("age".into(), Rule::Bucket(10.0)), ("hospitalid".into(), Rule::Drop), ("hospitaladmittime24".into(), Rule::HourOnly), ("patientunitstayid".into(), Rule::Pseudonym), ("patienthealthsystemstayid".into(), Rule::Pseudonym)] };
    let (gen, rep) = deidentify_csv(CSV, &cfg).unwrap();
    assert!(rep.unknown_columns.is_empty(), "{:?}", rep.unknown_columns);
    let qi2 = ["gender", "age", "ethnicity", "hospitaldischargeyear"];
    let mid = k_anonymity(&gen, &qi2).unwrap();
    eprintln!("after generalising: {mid:?}  unique {:.1}%", 100.0 * mid.fraction_unique());
    assert!(mid.unique_rows < before.unique_rows / 4, "generalising should cut unique rows sharply: {} -> {}", before.unique_rows, mid.unique_rows);
    assert!(!gen.contains("\n210014,") && !gen.contains("hospitalid"), "raw ids and the dropped column are gone");

    // finally enforce k = 5 on what is left
    let (safe, touched) = suppress_small_classes(&gen, &qi2, 5).unwrap();
    let after = k_anonymity(&safe, &qi2).unwrap();
    eprintln!("after k=5 suppression: {after:?} (records touched: {touched})");
    assert!(after.min_k >= 5, "{after:?}");
    assert!(touched < before.rows / 2, "suppression should not destroy most of the data");
}
