//! Automatic profiling of a CSV extract before it is loaded: find the columns that identify people, find data
//! quality problems, and turn both into a suggested de-identification [`Config`], a list of quasi-identifiers
//! for [`crate::deid::k_anonymity`], and a data [`Policy`](crate::security::Policy) for the loaded label.
//!
//! Classification uses the column name *and* the values. Value scans (SSN, e-mail, phone shapes, person-like
//! text) run on every column, so an identifier hiding in a column called `notes` is still reported, which is
//! also how [`residual_findings`] checks a de-identified file.
//!
//! This is a heuristic aid. A clean report does not prove a file is de-identified, and a column it labels
//! harmless may still identify someone in combination with others or with outside data.

use crate::deid::{days_from_civil, parse_iso_date, read, Config, Rule};
use crate::error::Result;
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Risk {
    None,
    Low,
    Medium,
    High,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    PersonName,
    StaffName,
    Email,
    Phone,
    Ssn,
    Address,
    /// A ledger address (Ethereum `0x` + 40 hex digits, or a Bitcoin base58/bech32 string). Pseudonymous, but it links
    /// one holder's activity across datasets, so it is pseudonymised (joins preserved), not dropped.
    WalletAddress,
    Zip,
    GeoCoord,
    PlaceName,
    BirthDate,
    Date,
    PatientId,
    RecordId,
    Age,
    Sex,
    Race,
    FreeText,
    Money,
    Number,
    Category,
    Other,
}

#[derive(Clone, Debug)]
pub struct ColumnProfile {
    pub name: String,
    pub kind: Kind,
    pub risk: Risk,
    pub why: String,
    pub distinct: usize,
    pub missing: usize,
    pub min: Option<f64>,
    pub max: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug)]
pub struct Finding {
    pub severity: Severity,
    pub column: Option<String>,
    pub rows: usize,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct Profile {
    pub table: String,
    pub rows: usize,
    pub columns: Vec<ColumnProfile>,
    pub findings: Vec<Finding>,
    /// Rows that are exact copies of an earlier row.
    pub duplicate_rows: usize,
}

impl Profile {
    pub fn column(&self, name: &str) -> Option<&ColumnProfile> {
        self.columns.iter().find(|c| c.name.eq_ignore_ascii_case(name))
    }
    pub fn high_risk(&self) -> Vec<&ColumnProfile> {
        self.columns.iter().filter(|c| c.risk == Risk::High).collect()
    }
}

fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_lowercase()).collect()
}

fn has(n: &str, words: &[&str]) -> bool {
    words.iter().any(|w| n.contains(w))
}

fn looks_ssn(v: &str) -> bool {
    let b = v.as_bytes();
    b.len() == 11 && b[3] == b'-' && b[6] == b'-' && b.iter().enumerate().all(|(i, c)| i == 3 || i == 6 || c.is_ascii_digit())
}

fn looks_email(v: &str) -> bool {
    match v.split_once('@') {
        Some((l, r)) => !l.is_empty() && r.contains('.') && !r.starts_with('.') && !v.contains(' '),
        None => false,
    }
}

fn looks_phone(v: &str) -> bool {
    let digits = v.chars().filter(|c| c.is_ascii_digit()).count();
    (10..=15).contains(&digits) && v.chars().all(|c| c.is_ascii_digit() || " -+().".contains(c)) && v.chars().any(|c| "-() .+".contains(c))
}

fn looks_uuid(v: &str) -> bool {
    v.len() == 36 && v.bytes().enumerate().all(|(i, c)| if matches!(i, 8 | 13 | 18 | 23) { c == b'-' } else { c.is_ascii_hexdigit() })
}

/// Ethereum (`0x` + 40 hex) or Bitcoin (base58 starting 1/3, or bech32 `bc1`/`tb1`) address shape.
fn looks_wallet(v: &str) -> bool {
    let v = v.trim();
    if let Some(h) = v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
        return h.len() == 40 && h.bytes().all(|c| c.is_ascii_hexdigit());
    }
    let b58 = |s: &str| s.bytes().all(|c| c.is_ascii_alphanumeric() && !matches!(c, b'0' | b'O' | b'I' | b'l'));
    if (v.starts_with('1') || v.starts_with('3')) && (26..=35).contains(&v.len()) {
        return b58(v);
    }
    let l = v.to_ascii_lowercase();
    (l.starts_with("bc1") || l.starts_with("tb1")) && (14..=90).contains(&v.len()) && l.bytes().all(|c| c.is_ascii_alphanumeric())
}

/// Two or more capitalised words and nothing else: "Matthew Smith", "Dr. L. Khan" (a person-name shape).
fn looks_person_name(v: &str) -> bool {
    let words: Vec<&str> = v.split_whitespace().collect();
    if !(2..=4).contains(&words.len()) {
        return false;
    }
    words.iter().all(|w| {
        let w = w.trim_end_matches('.');
        let mut ch = w.chars();
        matches!(ch.next(), Some(c) if c.is_alphabetic() && c.is_uppercase() || c.is_alphabetic() && c.is_lowercase()) && w.chars().all(|c| c.is_alphabetic() || c == '\'' || c == '-')
    })
}

/// Mixed-case noise inside a word ("LesLie", "TErRy"): an upper-case letter after a lower-case one.
fn odd_case(v: &str) -> bool {
    v.split_whitespace().any(|w| {
        let cs: Vec<char> = w.chars().collect();
        cs.windows(2).any(|p| p[0].is_lowercase() && p[1].is_uppercase())
    })
}

/// Profile `csv`. `table` is the file or table name (a column `Id` in a table called `patients` is a patient id).
pub fn profile_csv(csv: &str, table: &str) -> Result<Profile> {
    let (header, rows) = read(csv)?;
    let n = rows.len();
    let tnorm = norm(table);
    let mut cols: Vec<ColumnProfile> = vec![];
    let mut findings: Vec<Finding> = vec![];
    let mut push = |sev: Severity, col: Option<&str>, rows: usize, m: String| findings.push(Finding { severity: sev, column: col.map(String::from), rows, message: m });

    for (ci, h) in header.iter().enumerate() {
        let name = norm(h);
        let vals: Vec<&str> = rows.iter().map(|r| r[ci].as_str()).collect();
        let present: Vec<&str> = vals.iter().copied().filter(|v| !v.trim().is_empty()).collect();
        let missing = n - present.len();
        let distinct = present.iter().collect::<HashSet<_>>().len();
        let frac = |c: usize| if present.is_empty() { 0.0 } else { c as f64 / present.len() as f64 };
        let nums: Vec<f64> = present.iter().filter_map(|v| v.trim().parse::<f64>().ok().filter(|x| x.is_finite())).collect();
        let numeric = frac(nums.len()) >= 0.95;
        let dates = present.iter().filter(|v| parse_iso_date(v).is_some()).count();
        let is_date = frac(dates) >= 0.9;
        let (min, max) = if numeric && !nums.is_empty() { (nums.iter().cloned().reduce(f64::min), nums.iter().cloned().reduce(f64::max)) } else { (None, None) };
        let avg_len = if present.is_empty() { 0.0 } else { present.iter().map(|v| v.chars().count()).sum::<usize>() as f64 / present.len() as f64 };

        // content scans on every column
        let ssn = present.iter().filter(|v| looks_ssn(v)).count();
        let email = present.iter().filter(|v| looks_email(v)).count();
        let phone = present.iter().filter(|v| !numeric && looks_phone(v)).count();

        let (kind, risk, why): (Kind, Risk, String) = if ssn > 0 || has(&name, &["ssn", "socialsecurity"]) {
            (Kind::Ssn, Risk::High, "social security number".into())
        } else if frac(email) >= 0.5 || has(&name, &["email"]) {
            (Kind::Email, Risk::High, "e-mail address".into())
        } else if frac(phone) >= 0.5 || has(&name, &["phone", "mobile", "fax"]) {
            (Kind::Phone, Risk::High, "telephone number".into())
        } else if frac(present.iter().filter(|v| looks_wallet(v)).count()) >= 0.9 {
            (Kind::WalletAddress, Risk::High, "ledger address (pseudonymous; links a holder's activity across datasets)".into())
        } else if has(&name, &["address", "street"]) && !numeric {
            // numeric columns named like "unique_sent_to_addresses" or "btc_address_present" are counts/flags, not addresses
            (Kind::Address, Risk::High, "street address".into())
        } else if has(&name, &["zip", "postal"]) {
            (Kind::Zip, Risk::Medium, "postal code: more than 3 digits is finer than HIPAA Safe Harbor allows".into())
        } else if matches!(name.as_str(), "lat" | "lon" | "lng" | "latitude" | "longitude") {
            (Kind::GeoCoord, Risk::High, "coordinates are finer than a state".into())
        } else if ((has(&name, &["birth"]) && has(&name, &["date"])) || name == "dob" || name == "birthdate") && is_date {
            (Kind::BirthDate, Risk::High, "date of birth".into())
        } else if has(&name, &["race", "ethnic"]) {
            (Kind::Race, Risk::Low, "quasi-identifier".into())
        } else if matches!(name.as_str(), "prefix" | "suffix" | "title" | "salutation") {
            (Kind::Category, Risk::Low, "honorific".into())
        } else if has(&name, &["doctor", "physician", "provider", "nurse", "clinician"]) && !has(&name, &["id", "code", "insurance"]) && !numeric && frac(present.iter().filter(|v| looks_person_name(v)).count()) >= 0.5 {
            (Kind::StaffName, Risk::Medium, "clinician name (identifies staff, and with dates can identify patients)".into())
        } else if name == "birthplace" || name == "city" || name == "county" || name == "town" || name == "village" || name == "district" || name.ends_with("city") && !name.ends_with("icity") || name.ends_with("county") {
            (Kind::PlaceName, Risk::High, "geographic area smaller than a state".into())
        } else if has(&name, &["doctor", "physician", "provider", "nurse", "clinician"]) && has(&name, &["name"]) {
            (Kind::StaffName, Risk::Medium, "clinician name (identifies staff, and with dates can identify patients)".into())
        } else if matches!(name.as_str(), "first" | "last" | "middle" | "given" | "fname" | "lname" | "forename") && !numeric {
            (Kind::PersonName, Risk::High, "person name".into())
        } else if has(&name, &["name", "surname", "maiden", "prefix", "suffix"]) && !has(&name, &["hospital", "organization", "organisation", "company", "insurance", "payer", "department", "filename"]) {
            (Kind::PersonName, Risk::High, "person name".into())
        } else if matches!(name.as_str(), "patient" | "patientid" | "mrn" | "uniquepid" | "patientuid" | "medicalrecordnumber") || (name == "id" && has(&tnorm, &["patient"])) {
            (Kind::PatientId, Risk::High, "patient identifier (a record number is an identifier under HIPAA)".into())
        } else if name == "id" || name.ends_with("id") || matches!(name.as_str(), "encounter" | "accountnumber" | "licensenumber") {
            (Kind::RecordId, Risk::Medium, "record identifier".into())
        } else if is_date {
            (Kind::Date, Risk::Medium, "calendar date: everything finer than the year is an identifier under Safe Harbor".into())
        } else if name == "age" || name.starts_with("age") && numeric && max.map(|m| m <= 130.0).unwrap_or(false) {
            (Kind::Age, Risk::Medium, "age (over 89 must be aggregated)".into())
        } else if matches!(name.as_str(), "gender" | "sex") {
            (Kind::Sex, Risk::Low, "quasi-identifier".into())
        } else if !numeric && avg_len > 40.0 && distinct as f64 > 0.5 * present.len().max(1) as f64 {
            (Kind::FreeText, Risk::High, "free text can hold names and dates that no rule here removes".into())
        } else if has(&name, &["cost", "amount", "billing", "charge", "price", "claim", "fee", "coverage", "payment"]) && numeric {
            (Kind::Money, Risk::None, "amount".into())
        } else if numeric {
            (Kind::Number, Risk::None, "number".into())
        } else if distinct <= 60 || (distinct as f64) < 0.05 * n.max(1) as f64 {
            (Kind::Category, Risk::Low, "category".into())
        } else if present.iter().filter(|v| looks_uuid(v)).count() as f64 >= 0.9 * present.len() as f64 {
            (Kind::RecordId, Risk::Medium, "uuid".into())
        } else {
            (Kind::Other, Risk::Low, "unclassified text".into())
        };

        // content findings that apply whatever the column is called
        if ssn > 0 && !matches!(kind, Kind::Ssn) || email > 0 && !matches!(kind, Kind::Email) || phone > 0 && !matches!(kind, Kind::Phone) {
            push(Severity::Error, Some(h), ssn + email + phone, format!("{h}: values shaped like SSN/e-mail/phone found in a column that looks like {kind:?}"));
        }
        if matches!(kind, Kind::Category | Kind::Other | Kind::FreeText) {
            let names = present.iter().filter(|v| looks_person_name(v)).count();
            if frac(names) >= 0.5 && distinct as f64 > 0.3 * present.len() as f64 {
                push(Severity::Error, Some(h), names, format!("{h}: {names} values look like person names but the column is not named like one"));
            }
        }
        // quality
        if missing > 0 && missing as f64 > 0.2 * n as f64 {
            push(Severity::Warning, Some(h), missing, format!("{h}: {missing} of {n} values missing"));
        }
        if distinct == 1 && n > 1 {
            push(Severity::Info, Some(h), n, format!("{h}: constant column"));
        }
        if numeric && has(&name, &["cost", "amount", "billing", "charge", "price", "claim", "stay", "days", "length", "wait", "age"]) {
            let neg = nums.iter().filter(|x| **x < 0.0).count();
            if neg > 0 {
                push(Severity::Error, Some(h), neg, format!("{h}: {neg} negative values in a column that cannot be negative"));
            }
        }
        if matches!(kind, Kind::Age) {
            let bad = nums.iter().filter(|x| **x < 0.0 || **x > 120.0).count();
            if bad > 0 {
                push(Severity::Error, Some(h), bad, format!("{h}: {bad} ages outside 0-120"));
            }
        }
        if numeric && !nums.is_empty() && nums.iter().any(|x| x.fract() != 0.0) {
            let long = present.iter().filter(|v| v.split_once('.').map(|(_, f)| f.len() > 6).unwrap_or(false)).count();
            if long as f64 > 0.5 * present.len() as f64 {
                push(Severity::Info, Some(h), long, format!("{h}: values carry 7+ decimal places (false precision; also a fingerprint that survives rounding elsewhere)"));
            }
        }
        if matches!(kind, Kind::PersonName | Kind::StaffName) && !present.is_empty() {
            let odd = present.iter().filter(|v| odd_case(v)).count();
            if odd > 0 && frac(odd) >= 0.05 {
                push(Severity::Warning, Some(h), odd, format!("{h}: {odd} values have mixed-case noise (e.g. {:?}); the same entity may be spelled several ways", present.iter().find(|v| odd_case(v)).unwrap()));
            }
            // same text, different case/space => groups that should be one
            let mut groups: HashMap<String, HashSet<&str>> = HashMap::new();
            for v in &present {
                groups.entry(v.to_ascii_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")).or_default().insert(v);
            }
            let split = groups.values().filter(|g| g.len() > 1).count();
            if split > 0 {
                push(Severity::Warning, Some(h), split, format!("{h}: {split} names appear in more than one spelling/case"));
            }
        }
        if matches!(kind, Kind::PatientId | Kind::RecordId) && distinct < present.len() && matches!(kind, Kind::PatientId) && has(&tnorm, &["patient", "hospital", "treatment"]) {
            let dups = present.len() - distinct;
            push(Severity::Warning, Some(h), dups, format!("{h}: {dups} repeated values (one row per stay, or duplicated records?)"));
        }
        cols.push(ColumnProfile { name: h.clone(), kind, risk, why, distinct, missing, min, max });
    }

    // exact duplicate rows
    let mut seen: HashSet<&Vec<String>> = HashSet::new();
    let duplicate_rows = rows.iter().filter(|r| !seen.insert(r)).count();
    if duplicate_rows > 0 {
        push(Severity::Warning, None, duplicate_rows, format!("{duplicate_rows} rows are exact copies of an earlier row"));
    }

    // dates: order, future, length-of-stay consistency
    let ix = |pred: &dyn Fn(&str) -> bool| -> Vec<usize> { header.iter().enumerate().filter(|(i, h)| pred(&norm(h)) && matches!(cols[*i].kind, Kind::Date)).map(|(i, _)| i).collect() };
    let starts = ix(&|n| has(n, &["admission", "admit", "start", "arrival", "admitted"]) && !has(n, &["time24"]));
    let ends = ix(&|n| has(n, &["discharge", "stop", "departure", "end"]));
    let today = days_from_civil(2026, 11, 22);
    for (i, h) in header.iter().enumerate() {
        if matches!(cols[i].kind, Kind::Date) {
            let fut = rows.iter().filter(|r| parse_iso_date(&r[i]).map(|(y, m, d)| days_from_civil(y, m, d) > today).unwrap_or(false)).count();
            if fut > 0 {
                push(Severity::Warning, Some(h), fut, format!("{h}: {fut} dates are after 2026-11-22"));
            }
        }
    }
    let los_cols: Vec<usize> = header.iter().enumerate().filter(|(i, h)| matches!(cols[*i].kind, Kind::Number) && has(&norm(h), &["lengthofstay", "los", "stay"])).map(|(i, _)| i).collect();
    for &s in starts.iter().take(1) {
        for &e in ends.iter().take(1) {
            let (mut inverted, mut mism, mut both) = (0usize, 0usize, 0usize);
            for r in &rows {
                if let (Some((y1, m1, d1)), Some((y2, m2, d2))) = (parse_iso_date(&r[s]), parse_iso_date(&r[e])) {
                    both += 1;
                    let span = days_from_civil(y2, m2, d2) - days_from_civil(y1, m1, d1);
                    if span < 0 {
                        inverted += 1;
                    }
                    if let Some(&l) = los_cols.first() {
                        if let Ok(v) = r[l].trim().parse::<f64>() {
                            if (v - span as f64).abs() > 0.5 {
                                mism += 1;
                            }
                        }
                    }
                }
            }
            if inverted > 0 {
                push(Severity::Error, Some(&header[e]), inverted, format!("{}: {inverted} of {both} rows end before they start ({})", header[e], header[s]));
            }
            if mism > 0 {
                push(Severity::Warning, Some(&header[los_cols[0]]), mism, format!("{}: {mism} of {both} rows disagree with the number of days between {} and {}", header[los_cols[0]], header[s], header[e]));
            }
        }
    }
    findings.sort_by(|a, b| b.severity.cmp(&a.severity));
    Ok(Profile { table: table.to_string(), rows: n, columns: cols, findings, duplicate_rows })
}

/// The pseudonym domain for an identifier column, so the same entity gets the same token across tables:
/// `encounters.Id` and `procedures.ENCOUNTER` both become `id:encounter`; `Doctor_ID` becomes `id:doctor`.
fn id_domain(table: &str, col: &str) -> String {
    let c = norm(col);
    let base = if c == "id" { norm(table) } else { c.strip_suffix("id").filter(|b| !b.is_empty()).map(String::from).unwrap_or(c) };
    format!("id:{}", base.strip_suffix('s').filter(|b| b.len() > 2).unwrap_or(&base))
}

/// Choose rules for every risky column. `key` is the pseudonymisation secret (at least 16 bytes).
/// Returns the config and the quasi-identifier columns to feed to `k_anonymity` on the *de-identified* file.
pub fn suggest_config(p: &Profile, key: &[u8]) -> (Config, Vec<String>) {
    let subject = p.columns.iter().find(|c| c.kind == Kind::PatientId).map(|c| c.name.clone());
    let mut rules: Vec<(String, Rule)> = vec![];
    let mut qi: Vec<String> = vec![];
    for c in &p.columns {
        let r = match c.kind {
            Kind::PersonName | Kind::Email | Kind::Phone | Kind::Ssn | Kind::Address | Kind::GeoCoord | Kind::PlaceName | Kind::FreeText => Some(Rule::Drop),
            Kind::BirthDate => Some(Rule::DateYearBucket(5)),
            Kind::StaffName => Some(Rule::LinkedPseudonym("staff".into())),
            Kind::PatientId => Some(Rule::LinkedPseudonym("patient".into())),
            Kind::RecordId | Kind::WalletAddress => Some(Rule::LinkedPseudonym(id_domain(&p.table, &c.name))),
            Kind::Zip => {
                qi.push(c.name.clone());
                Some(Rule::Truncate(3))
            }
            Kind::Date => Some(match &subject {
                Some(s) => Rule::DateShift { subject: s.clone(), max_days: 180 },
                None => Rule::DateYear,
            }),
            Kind::Age => {
                qi.push(c.name.clone());
                Some(Rule::TopBucket { top: 89.0, width: 5.0 })
            }
            Kind::Sex | Kind::Race => {
                qi.push(c.name.clone());
                None
            }
            _ => None,
        };
        if let Some(r) = r {
            rules.push((c.name.clone(), r));
        }
    }
    // a birth-year band is a quasi-identifier
    for (name, r) in &rules {
        if matches!(r, Rule::DateYearBucket(_)) && p.column(name).map(|c| c.kind == Kind::BirthDate).unwrap_or(false) {
            qi.push(name.clone());
        }
    }
    (Config { key: key.to_vec(), rules }, qi)
}

/// What is still identifying in a (de-identified) file: every High-risk column and every content finding.
pub fn residual_findings(csv: &str, table: &str) -> Result<Vec<String>> {
    let p = profile_csv(csv, table)?;
    let mut out: Vec<String> = p.high_risk().iter().map(|c| format!("{}: {:?} ({})", c.name, c.kind, c.why)).collect();
    out.extend(p.findings.iter().filter(|f| f.severity == Severity::Error && f.message.contains("shaped like")).map(|f| f.message.clone()));
    Ok(out)
}

/// A JSON policy (for `--policy`) that hides what `p` found: High-risk columns are denied, ids are
/// pseudonymised, ages top-coded. `label` is the graph label the file will be loaded under.
pub fn suggest_policy_json(p: &Profile, label: &str, key_hex: &str) -> String {
    let mut deny: Vec<String> = vec![];
    let mut mask: BTreeMap<String, String> = BTreeMap::new();
    for c in &p.columns {
        let q = format!("{label}.{}", c.name);
        match c.kind {
            Kind::PersonName | Kind::Email | Kind::Phone | Kind::Ssn | Kind::Address | Kind::GeoCoord | Kind::PlaceName | Kind::FreeText | Kind::BirthDate => deny.push(q),
            Kind::PatientId | Kind::StaffName | Kind::RecordId | Kind::WalletAddress => {
                mask.insert(c.name.clone(), "pseudonym".into());
            }
            Kind::Age => {
                mask.insert(c.name.clone(), "top_code:89".into());
            }
            Kind::Zip => {
                mask.insert(c.name.clone(), "redact".into());
            }
            _ => {}
        }
    }
    let list = |v: &[String]| v.iter().map(|s| format!("{s:?}")).collect::<Vec<_>>().join(", ");
    let masks = mask.iter().map(|(k, v)| format!("{k:?}: {v:?}")).collect::<Vec<_>>().join(", ");
    format!("{{\n  \"min_cell\": 11,\n  \"min_cell_up_to\": \"reader\",\n  \"pseudonym_key_hex\": {key_hex:?},\n  \"rules\": [{{\"up_to\": \"writer\", \"deny\": [{}], \"mask\": {{{masks}}}}}]\n}}\n", list(&deny))
}

impl Profile {
    /// A readable report.
    pub fn report(&self) -> String {
        let mut s = format!("## {} — {} rows, {} columns\n\n| column | kind | risk | distinct | missing | note |\n|---|---|---|---|---|---|\n", self.table, self.rows, self.columns.len());
        for c in &self.columns {
            s.push_str(&format!("| {} | {:?} | {:?} | {} | {} | {} |\n", c.name, c.kind, c.risk, c.distinct, c.missing, c.why));
        }
        if !self.findings.is_empty() {
            s.push_str("\nFindings:\n");
            for f in &self.findings {
                s.push_str(&format!("- [{:?}] {}\n", f.severity, f.message));
            }
        }
        s
    }
}
