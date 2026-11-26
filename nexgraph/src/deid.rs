//! De-identification and re-identification-risk checks for tabular (CSV) extracts, run *before* the data is
//! loaded into the database.
//!
//! Two parts:
//!
//! * [`deidentify_csv`] applies per-column rules (drop, keyed pseudonym, top-code, bucket, redact, reduce a
//!   time of day to the hour). The pseudonym is `HMAC-SHA256(key, "column|value")`, so the same patient maps
//!   to the same token in every table (linkage survives) but the token cannot be reversed or guessed without
//!   the key. Keep the key out of the data store.
//! * [`k_anonymity`] measures how many records share each combination of quasi-identifiers (age, sex,
//!   site, year ...). A record that is alone in its group (k = 1) can be singled out by anyone who knows
//!   those attributes about a person, whatever the direct identifiers look like. [`suppress_small_classes`]
//!   removes the quasi-identifier values of records in groups smaller than `k`.
//!
//! This is a technical aid. It does **not** establish that a data set meets HIPAA Safe Harbor or the Expert
//! Determination standard, or any other legal test: that depends on the full data set, the recipients and
//! outside data, and needs a qualified person. Free text (notes, diagnoses typed by hand) is not scrubbed.

use crate::crypto::hmac_sha256;
use crate::error::{Error, Result};
use crate::ingest::parse_csv;
use std::collections::{BTreeMap, HashMap};

#[derive(Clone, Debug, PartialEq)]
pub enum Rule {
    /// Remove the column.
    Drop,
    /// Keyed, stable, irreversible token (`p_` + 12 hex digits). Empty cells stay empty.
    Pseudonym,
    /// Numbers above `n` become `n` (HIPAA Safe Harbor: ages over 89 are aggregated into one category).
    /// A text such as `> 89` counts as a number above 89.
    TopCode(f64),
    /// Round numbers down to a multiple of `w`.
    Bucket(f64),
    /// `13:08:59` becomes `13:00:00`.
    HourOnly,
    /// Replace every non-empty cell with `[redacted]`.
    Redact,
    /// Like `Pseudonym` but the token depends only on the value and this domain, not on the column name, so the
    /// same patient id gets the same token in every table and column (joins keep working).
    LinkedPseudonym(String),
    /// `2024-03-09` / `2024-03-09T10:00:00Z` becomes `2024`.
    DateYear,
    /// Move the date part by a secret, per-subject offset of up to `max_days` either way. Every date of one
    /// subject (taken from the `subject` column) moves by the same amount, so intervals such as length of stay
    /// survive; the time of day and everything after the date is left as it was. This is the usual
    /// Expert-Determination technique, not Safe Harbor (which only allows the year).
    DateShift { subject: String, max_days: i64 },
    /// `2024-03-09` becomes `2020-2024`-style band start: the year rounded down to a multiple of `width`.
    DateYearBucket(u32),
    /// Keep the first `n` characters (ZIP 3).
    Truncate(usize),
    /// Top-code at `top`, then round down to a multiple of `width` (ages: 89 and 5).
    TopBucket { top: f64, width: f64 },
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                29
            } else {
                28
            }
        }
    }
}

/// The ISO date at the start of `s` (`YYYY-MM-DD`, optionally followed by `T` or a space and anything), if valid.
pub fn parse_iso_date(s: &str) -> Option<(i64, u32, u32)> {
    let b = s.as_bytes();
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' || !b[..4].iter().chain(&b[5..7]).chain(&b[8..10]).all(u8::is_ascii_digit) {
        return None;
    }
    if b.len() > 10 && !matches!(b[10], b'T' | b' ') {
        return None;
    }
    let y: i64 = s[..4].parse().ok()?;
    let m: u32 = s[5..7].parse().ok()?;
    let d: u32 = s[8..10].parse().ok()?;
    if !(1..=12).contains(&m) || d == 0 || d > days_in_month(y, m) {
        return None;
    }
    Some((y, m, d))
}

#[derive(Clone, Debug, Default)]
pub struct Config {
    /// Secret for pseudonyms, at least 16 bytes.
    pub key: Vec<u8>,
    pub rules: Vec<(String, Rule)>,
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub rows: usize,
    pub columns_dropped: Vec<String>,
    /// Column -> cells changed.
    pub cells_changed: BTreeMap<String, usize>,
    /// Rules that named a column the file does not have (a typo would otherwise leave data in the clear).
    pub unknown_columns: Vec<String>,
}

fn num_text(f: f64) -> String {
    if f.fract() == 0.0 && f.abs() < 1e15 {
        format!("{}", f as i64)
    } else {
        format!("{f}")
    }
}

/// A number, treating `> 89` as 90 (so it is top-coded) and `< 1` as 0.
pub(crate) fn lenient_number(s: &str) -> Option<f64> {
    let t = s.trim();
    if let Ok(v) = t.parse::<f64>() {
        return v.is_finite().then_some(v);
    }
    if let Some(r) = t.strip_prefix('>') {
        return r.trim().parse::<f64>().ok().map(|v| v + 1.0);
    }
    if let Some(r) = t.strip_prefix('<') {
        return r.trim().parse::<f64>().ok().map(|v| v - 1.0);
    }
    None
}

fn csv_cell(s: &str, out: &mut String) {
    if s.contains([',', '"', '\n', '\r']) {
        out.push('"');
        out.push_str(&s.replace('"', "\"\""));
        out.push('"');
    } else {
        out.push_str(s);
    }
}

pub(crate) fn write_csv(header: &[String], rows: &[Vec<String>]) -> String {
    let mut out = String::new();
    let line = |cells: &[String], out: &mut String| {
        for (i, c) in cells.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            csv_cell(c, out);
        }
        out.push('\n');
    };
    line(header, &mut out);
    for r in rows {
        line(r, &mut out);
    }
    out
}

pub(crate) fn read(csv: &str) -> Result<(Vec<String>, Vec<Vec<String>>)> {
    let csv = csv.strip_prefix('\u{feff}').unwrap_or(csv);
    let mut recs = parse_csv(csv, ',')?.into_iter().map(|(_, r)| r);
    let header = recs.next().ok_or_else(|| Error::Invalid("empty CSV".into()))?;
    let w = header.len();
    let mut rows = Vec::new();
    for (i, r) in recs.enumerate() {
        if r.len() != w {
            return Err(Error::Invalid(format!("row {} has {} fields, the header has {w}", i + 2, r.len())));
        }
        rows.push(r);
    }
    Ok((header, rows))
}

/// Apply `cfg.rules` to a CSV. Columns without a rule are copied unchanged, so list every column that
/// identifies or helps to identify a person.
pub fn deidentify_csv(csv: &str, cfg: &Config) -> Result<(String, Report)> {
    if cfg.rules.iter().any(|(_, r)| matches!(r, Rule::Pseudonym | Rule::LinkedPseudonym(_) | Rule::DateShift { .. })) && cfg.key.len() < 16 {
        return Err(Error::Invalid("pseudonym key must be at least 16 bytes".into()));
    }
    let (header, mut rows) = read(csv)?;
    let mut rep = Report { rows: rows.len(), ..Default::default() };
    let mut plan: Vec<Option<(&str, &Rule)>> = vec![None; header.len()];
    // DateShift needs the subject's column
    let mut subject_ix: Vec<Option<usize>> = vec![None; header.len()];
    for (name, rule) in &cfg.rules {
        match header.iter().position(|h| h.eq_ignore_ascii_case(name)) {
            Some(i) => {
                plan[i] = Some((header[i].as_str(), rule));
                if let Rule::DateShift { subject, .. } = rule {
                    subject_ix[i] = Some(header.iter().position(|h| h.eq_ignore_ascii_case(subject)).ok_or_else(|| Error::Invalid(format!("DateShift subject column {subject:?} not found")))?);
                }
            }
            None => rep.unknown_columns.push(name.clone()),
        }
    }
    let mut out_rows: Vec<Vec<String>> = Vec::with_capacity(rows.len());
    for row in rows.iter_mut() {
        let mut o = Vec::with_capacity(row.len());
        for (i, cell) in row.iter().enumerate() {
            let Some((name, rule)) = plan[i] else {
                o.push(cell.clone());
                continue;
            };
            let new = match rule {
                Rule::Drop => continue,
                _ if cell.is_empty() => String::new(),
                Rule::Pseudonym => {
                    let mac = hmac_sha256(&cfg.key, format!("{}|{}", name.to_ascii_lowercase(), cell).as_bytes());
                    format!("p_{}", crate::crypto::hex(&mac[..6]))
                }
                Rule::TopCode(n) => match lenient_number(cell) {
                    Some(v) if v > *n => num_text(*n),
                    Some(_) => cell.clone(),
                    None => "[redacted]".into(), // never pass an unparseable value through a numeric rule
                },
                Rule::Bucket(w) => match lenient_number(cell) {
                    Some(v) if *w > 0.0 => num_text((v / w).floor() * w),
                    _ => "[redacted]".into(),
                },
                Rule::HourOnly => match cell.split_once(':') {
                    Some((h, _)) if h.len() == 2 && h.bytes().all(|b| b.is_ascii_digit()) => format!("{h}:00:00"),
                    _ => "[redacted]".into(),
                },
                Rule::Redact => "[redacted]".into(),
                Rule::LinkedPseudonym(domain) => {
                    let mac = hmac_sha256(&cfg.key, format!("link:{}|{}", domain.to_ascii_lowercase(), cell).as_bytes());
                    format!("p_{}", crate::crypto::hex(&mac[..6]))
                }
                Rule::DateYear => match parse_iso_date(cell) {
                    Some((y, _, _)) => format!("{y:04}"),
                    None => "[redacted]".into(),
                },
                Rule::DateYearBucket(w) => match parse_iso_date(cell) {
                    Some((y, _, _)) if *w > 0 => format!("{}", (y / *w as i64) * *w as i64),
                    _ => "[redacted]".into(),
                },
                Rule::Truncate(n) => cell.chars().take(*n).collect(),
                Rule::TopBucket { top, width } => match lenient_number(cell) {
                    Some(v) if *width > 0.0 => num_text((v.min(*top) / width).floor() * width),
                    _ => "[redacted]".into(),
                },
                Rule::DateShift { max_days, .. } => {
                    let subject = subject_ix[i].map(|j| row[j].as_str()).unwrap_or("");
                    match (parse_iso_date(cell), subject.is_empty()) {
                        (Some((y, m, d)), false) => {
                            // offsets are -max..-1 and 1..max: zero would leave a subject's real dates in the file
                            let max = (*max_days).max(1);
                            let mac = hmac_sha256(&cfg.key, format!("shift|{subject}").as_bytes());
                            let v = (u64::from_le_bytes(mac[..8].try_into().unwrap()) % (2 * max as u64)) as i64;
                            let off = if v < max { v - max } else { v - max + 1 };
                            let (ny, nm, nd) = civil_from_days(days_from_civil(y, m, d) + off);
                            format!("{ny:04}-{nm:02}-{nd:02}{}", &cell[10..])
                        }
                        _ => "[redacted]".into(),
                    }
                }
            };
            if new != *cell {
                *rep.cells_changed.entry(name.to_string()).or_default() += 1;
            }
            o.push(new);
        }
        out_rows.push(o);
    }
    let mut out_header = Vec::new();
    for (i, h) in header.iter().enumerate() {
        if matches!(plan[i], Some((_, Rule::Drop))) {
            rep.columns_dropped.push(h.clone());
        } else {
            out_header.push(h.clone());
        }
    }
    Ok((write_csv(&out_header, &out_rows), rep))
}

#[derive(Clone, Debug, PartialEq)]
pub struct KReport {
    pub rows: usize,
    /// Distinct quasi-identifier combinations.
    pub classes: usize,
    /// Size of the smallest group.
    pub min_k: usize,
    /// Records that are alone in their group (k = 1).
    pub unique_rows: usize,
    /// Records in groups smaller than 5 / 11 (11 is the usual cell-size rule for public tables).
    pub rows_below_5: usize,
    pub rows_below_11: usize,
}

impl KReport {
    pub fn fraction_unique(&self) -> f64 {
        if self.rows == 0 {
            0.0
        } else {
            self.unique_rows as f64 / self.rows as f64
        }
    }
}

fn key_columns(header: &[String], quasi: &[&str]) -> Result<Vec<usize>> {
    quasi
        .iter()
        .map(|q| header.iter().position(|h| h.eq_ignore_ascii_case(q)).ok_or_else(|| Error::Invalid(format!("no column {q:?}"))))
        .collect()
}

fn groups(rows: &[Vec<String>], cols: &[usize]) -> HashMap<Vec<String>, usize> {
    let mut m: HashMap<Vec<String>, usize> = HashMap::new();
    for r in rows {
        *m.entry(cols.iter().map(|&c| r[c].clone()).collect()).or_default() += 1;
    }
    m
}

/// How many records share each combination of the `quasi` columns.
pub fn k_anonymity(csv: &str, quasi: &[&str]) -> Result<KReport> {
    let (header, rows) = read(csv)?;
    let cols = key_columns(&header, quasi)?;
    if cols.is_empty() {
        return Err(Error::Invalid("name at least one quasi-identifier column".into()));
    }
    let g = groups(&rows, &cols);
    let below = |k: usize| g.values().filter(|&&n| n < k).sum::<usize>();
    Ok(KReport {
        rows: rows.len(),
        classes: g.len(),
        min_k: g.values().copied().min().unwrap_or(0),
        unique_rows: g.values().filter(|&&n| n == 1).count(),
        rows_below_5: below(5),
        rows_below_11: below(11),
    })
}

/// Local suppression: in every record whose quasi-identifier group has fewer than `k` members, replace the
/// quasi-identifier cells with `*`. If the starred records are themselves fewer than `k` they are removed, so
/// the result is k-anonymous on `quasi`. Returns the new CSV and the number of records touched.
pub fn suppress_small_classes(csv: &str, quasi: &[&str], k: usize) -> Result<(String, usize)> {
    if k < 2 {
        return Err(Error::Invalid("k must be at least 2".into()));
    }
    let (header, mut rows) = read(csv)?;
    let cols = key_columns(&header, quasi)?;
    if cols.is_empty() {
        return Err(Error::Invalid("name at least one quasi-identifier column".into()));
    }
    let g = groups(&rows, &cols);
    let mut touched = 0usize;
    for r in rows.iter_mut() {
        let key: Vec<String> = cols.iter().map(|&c| r[c].clone()).collect();
        if g[&key] < k {
            for &c in &cols {
                r[c] = "*".into();
            }
            touched += 1;
        }
    }
    if touched > 0 && touched < k {
        rows.retain(|r| !cols.iter().all(|&c| r[c] == "*"));
    }
    Ok((write_csv(&header, &rows), touched))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CSV: &str = "pid,age,sex,site,t\n1,45,M,a,13:08:59\n2,45,M,a,09:00:00\n3,93,F,b,23:59:59\n4,> 89,F,b,01:02:03\n5,30,M,c,\n";

    #[test]
    fn rules_apply_and_pseudonyms_are_stable_and_keyed() {
        let cfg = Config {
            key: b"0123456789abcdef".to_vec(),
            rules: vec![("pid".into(), Rule::Pseudonym), ("age".into(), Rule::TopCode(89.0)), ("t".into(), Rule::HourOnly), ("site".into(), Rule::Drop), ("nope".into(), Rule::Drop)],
        };
        let (out, rep) = deidentify_csv(CSV, &cfg).unwrap();
        assert_eq!(rep.columns_dropped, vec!["site"]);
        assert_eq!(rep.unknown_columns, vec!["nope"], "a rule naming a missing column is reported, not ignored");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "pid,age,sex,t");
        assert!(lines[1].starts_with("p_") && lines[1].ends_with(",45,M,13:00:00"));
        assert!(lines[3].contains(",89,F,"), "93 top-coded: {}", lines[3]);
        assert!(lines[4].contains(",89,F,"), "'> 89' top-coded: {}", lines[4]);
        assert!(lines[5].ends_with(",30,M,"), "empty stays empty: {}", lines[5]);
        assert!(!out.contains("\n1,") && !out.contains("\n3,"), "raw ids must be gone");
        let (again, _) = deidentify_csv(CSV, &cfg).unwrap();
        assert_eq!(out, again, "stable");
        let other = Config { key: b"fedcba9876543210".to_vec(), ..cfg.clone() };
        assert_ne!(out, deidentify_csv(CSV, &other).unwrap().0, "a different key gives different tokens");
        assert!(deidentify_csv(CSV, &Config { key: vec![1; 4], ..cfg }).is_err(), "short key refused");
    }

    #[test]
    fn numeric_rules_never_pass_unparseable_values_through() {
        let cfg = Config { key: vec![], rules: vec![("age".into(), Rule::Bucket(10.0))] };
        let (out, _) = deidentify_csv("age\n47\n3.9\nabc\n", &cfg).unwrap();
        assert_eq!(out, "age\n40\n0\n[redacted]\n");
    }

    #[test]
    fn k_anonymity_and_suppression() {
        let r = k_anonymity(CSV, &["age", "sex"]).unwrap();
        assert_eq!((r.rows, r.classes, r.min_k, r.unique_rows), (5, 4, 1, 3));
        assert_eq!(r.rows_below_5, 5);
        let (out, touched) = suppress_small_classes(CSV, &["age", "sex"], 2).unwrap();
        assert_eq!(touched, 3);
        // the three starred records are >= k, so they stay, and the table is now 2-anonymous
        let r2 = k_anonymity(&out, &["age", "sex"]).unwrap();
        assert!(r2.min_k >= 2, "{r2:?}");
        // fewer starred records than k: they are removed
        let (out, touched) = suppress_small_classes("a,b\n1,x\n1,x\n2,y\n", &["a", "b"], 2).unwrap();
        assert_eq!(touched, 1);
        assert_eq!(out, "a,b\n1,x\n1,x\n");
        assert!(suppress_small_classes(CSV, &["age"], 1).is_err());
        assert!(k_anonymity(CSV, &["missing"]).is_err());
    }

    fn cfg(rules: Vec<(&str, Rule)>) -> Config {
        Config { key: b"0123456789abcdef".to_vec(), rules: rules.into_iter().map(|(c, r)| (c.to_string(), r)).collect() }
    }

    #[test]
    fn date_shift_is_per_subject_never_zero_and_keeps_time() {
        let mut csv = String::from("p,d\n");
        for i in 0..400 {
            csv.push_str(&format!("s{i},2020-06-15 08:30:00\ns{i},2020-07-01\n"));
        }
        let (out, _) = deidentify_csv(&csv, &cfg(vec![("d", Rule::DateShift { subject: "p".into(), max_days: 30 })])).unwrap();
        let l: Vec<Vec<&str>> = out.lines().skip(1).map(|x| x.split(',').collect()).collect();
        let day = |s: &str| {
            let (y, m, d) = parse_iso_date(s).unwrap();
            days_from_civil(y, m, d)
        };
        for pair in l.chunks(2) {
            let o1 = day(pair[0][1]) - day("2020-06-15");
            let o2 = day(pair[1][1]) - day("2020-07-01");
            assert_eq!(o1, o2, "one offset per subject");
            assert!(o1 != 0 && o1.abs() <= 30);
            assert!(pair[0][1].ends_with(" 08:30:00"));
        }
    }

    #[test]
    fn year_bucket_top_bucket_and_linked_pseudonyms() {
        let csv = "id,born,age,other\nA,1977-03-19,93,A\nB,1982-12-31,7,B\n";
        let (out, _) = deidentify_csv(csv, &cfg(vec![("id", Rule::LinkedPseudonym("x".into())), ("other", Rule::LinkedPseudonym("x".into())), ("born", Rule::DateYearBucket(5)), ("age", Rule::TopBucket { top: 89.0, width: 5.0 })])).unwrap();
        let rows: Vec<Vec<&str>> = out.lines().skip(1).map(|x| x.split(',').collect()).collect();
        assert_eq!(rows[0][0], rows[0][3], "same value, same domain: same token whatever the column");
        assert_ne!(rows[0][0], rows[1][0]);
        assert_eq!((rows[0][1], rows[1][1]), ("1975", "1980"));
        assert_eq!(rows[0][2], "85", "93 is top-coded to 89 then banded: no one over 89 is distinguishable");
        assert_eq!(rows[1][2], "5");
        // a different domain gives a different token
        let (o2, _) = deidentify_csv(csv, &cfg(vec![("id", Rule::LinkedPseudonym("y".into()))])).unwrap();
        assert_ne!(o2.lines().nth(1).unwrap().split(',').next(), Some(rows[0][0]));
    }
}
