//! Land registry and property-title analytics (v0.12).
//!
//! Built from what a 1,000-record Tamil Nadu land registry extract (parcel id, district / taluk / village, survey
//! and sub-division numbers, owner, area, market value, record status, VAO / revenue-inspector / tahsildar
//! workflow) needed from the engine. Like `killchain.*` and `alm.*`, every procedure reads records stored as
//! hyperedges (the output of a hyperedge import with `keep_member_props`); a key is looked up as an edge property
//! first, else as the name of the member node with that label. `filter` is `Key=Value`, `Key!=Value` or a 0/1 key.
//! A group key may name several columns separated by commas (`District,Taluk,Village`); their values are joined
//! with `/`, which is how a village is told apart from the same village name in another district.
//!
//! | procedure | question it answers |
//! |---|---|
//! | `land.conflicts(label, parcelKeys, ownerKey[, limit[, filter[, tenancyKey]]])` | parcels recorded more than once: competing owners (a title conflict) or repeated records |
//! | `land.rule_check(label, ifCond, thenCond[, groupKey[, filter[, idKey]]])` | records that satisfy `ifCond` but not `thenCond` (e.g. `Record Status=Active` but approval not `Approved`), with Wilson interval |
//! | `land.hotspots(label, groupKey, outcome[, filter[, minN]])` | which offices / areas have an unusually high rate of an outcome (disputes, rejections), with a p-value and a Bonferroni flag |
//! | `land.valuation(label, valueKey, areaKey[, groupKey[, filter]])` | value per unit area by segment, with quartiles and Tukey outlier fences on the log scale |
//! | `land.survey_check(label, surveyKey, subKey[, groupKey[, filter[, idKey]]])` | malformed survey numbers, and survey numbers whose `/sub-division` part disagrees with the sub-division column |
//!
//! ## Conventions
//!
//! * A parcel is identified by the **qualified** key you give (`District,Taluk,Village,Survey Number,Sub Division`),
//!   never by a survey number alone: survey numbers repeat across villages.
//! * Statistical flags are screens, not findings. `land.hotspots` tests every group against the overall rate with a
//!   normal approximation and flags those below `0.05 / groups tested` (Bonferroni); a flag means "look at this
//!   office", and with a handful of records per group even that is weak. `minN` (default 10) drops tiny groups.
//! * Outlier fences are Tukey's (`Q1 - 1.5 IQR`, `Q3 + 1.5 IQR`) on `ln(value / area)`, so a 10x error in either
//!   direction is symmetric. Quartiles are linearly interpolated (the pandas / numpy default).
//! * Missing or non-positive value or area are excluded and counted, never treated as zero.

use crate::alm::select;
use crate::error::Result;
use crate::killchain::{f, flag, fo, group_of, num, opt_str, require_key, wilson};
use crate::model::Body;
use crate::netgraph::*;
use crate::store::View;
use crate::value::Value;
use std::collections::BTreeMap;

pub(crate) fn call(view: &View, name: &str, a: &[Value]) -> Result<Rows> {
    match name {
        "conflicts" => conflicts(view, a),
        "rule_check" => rule_check(view, a),
        "hotspots" => hotspots(view, a),
        "valuation" => valuation(view, a),
        "survey_check" => survey_check(view, a),
        _ => bad(format!("unknown procedure 'land.{name}' (available: land.conflicts, land.rule_check, land.hotspots, land.valuation, land.survey_check)")),
    }
}

// ------------------------------------------------------------------ helpers

fn keys_of(s: &str) -> Vec<&str> {
    s.split(',').map(str::trim).filter(|k| !k.is_empty()).collect()
}

/// Values of several keys joined with `/`; `None` when any is missing.
fn joined(view: &View, b: &Body, keys: &[&str]) -> Option<String> {
    let mut parts = Vec::with_capacity(keys.len());
    for k in keys {
        parts.push(group_of(view, b, k)?);
    }
    Some(parts.join("/"))
}

fn group_label(view: &View, b: &Body, keys: &[&str]) -> String {
    if keys.is_empty() {
        return "ALL".into();
    }
    joined(view, b, keys).unwrap_or_else(|| "(missing)".into())
}

fn need_keys(view: &View, ev: &[&Body], keys: &[&str], what: &str) -> Result<()> {
    for k in keys {
        require_key(view, ev, k, what)?;
    }
    Ok(())
}

fn limit_arg(a: &[Value], i: usize, default: usize) -> Result<usize> {
    usize_arg(a, i, default, "limit")
}

/// Complementary error function, accurate to about 1e-12 relative: Taylor series below 3, a continued fraction above.
pub(crate) fn erfc(x: f64) -> f64 {
    if x < 0.0 {
        return 2.0 - erfc(-x);
    }
    if x < 3.0 {
        let (mut term, mut sum, mut n) = (x, x, 0.0f64);
        loop {
            n += 1.0;
            term *= -x * x / n;
            let add = term / (2.0 * n + 1.0);
            sum += add;
            if add.abs() < 1e-17 * sum.abs().max(1e-300) {
                break;
            }
        }
        1.0 - 2.0 / std::f64::consts::PI.sqrt() * sum
    } else {
        let mut t = x;
        for k in (1..=80).rev() {
            t = x + (k as f64 / 2.0) / t;
        }
        (-x * x).exp() / std::f64::consts::PI.sqrt() / t
    }
}

/// Two-sided normal p-value for a z-score.
fn p_two_sided(z: f64) -> f64 {
    erfc(z.abs() / std::f64::consts::SQRT_2)
}

fn sample(ids: &[String]) -> Value {
    Value::Str(ids.iter().take(5).cloned().collect::<Vec<_>>().join(", "))
}

// -------------------------------------------------------------- conflicts

/// `land.conflicts(label, parcelKeys, ownerKey[, limit[, filter[, tenancyKey]]])` -> parcel, records, distinct_owners, owners, kind.
/// Records sharing the same qualified parcel key. `competing_owners` (two or more distinct owners) is a title
/// conflict; `repeated_record` is the same parcel and owner entered more than once. Records missing a key column
/// cannot be placed on a parcel and are reported in the `(unplaced)` row.
fn conflicts(view: &View, a: &[Value]) -> Result<Rows> {
    let label = need_str(a, 0, "label")?;
    let parcel_keys = keys_of(need_str(a, 1, "parcelKeys")?);
    let owner_key = need_str(a, 2, "ownerKey")?;
    if parcel_keys.is_empty() {
        return bad("parcelKeys needs at least one column, e.g. 'District,Taluk,Village,Survey Number,Sub Division'");
    }
    let limit = limit_arg(a, 3, 1000)?;
    let ev = select(view, label, opt_str(a, 4))?;
    need_keys(view, &ev, &parcel_keys, "parcel key")?;
    require_key(view, &ev, owner_key, "owner key")?;
    // Optional 6th argument: a tenancy property (e.g. 'tenancy_type'). Records whose value is not 'SOLE'
    // (case-insensitive) are co-owners; a parcel whose records are all co-ownership is reported as
    // `co_ownership`, not `competing_owners`.
    let tenancy_key = opt_str(a, 5);
    let mut by: BTreeMap<String, Vec<(String, bool)>> = BTreeMap::new();
    let mut unplaced = 0i64;
    for b in &ev {
        match joined(view, b, &parcel_keys) {
            Some(k) => {
                let owner = group_of(view, b, owner_key).unwrap_or_else(|| "(unknown)".into());
                let sole = match tenancy_key {
                    Some(tk) => group_of(view, b, tk).map_or(true, |t| t.eq_ignore_ascii_case("SOLE")),
                    None => true,
                };
                by.entry(k).or_default().push((owner, sole));
            }
            None => unplaced += 1,
        }
    }
    let mut rows: Vec<(String, Vec<(String, bool)>)> = by.into_iter().filter(|(_, o)| o.len() > 1).collect();
    rows.sort_by(|x, y| y.1.len().cmp(&x.1.len()).then_with(|| x.0.cmp(&y.0)));
    let mut out: Vec<Vec<Value>> = rows
        .into_iter()
        .take(limit)
        .map(|(k, recs)| {
            let n = recs.len();
            let any_sole = recs.iter().any(|r| r.1);
            let mut d: Vec<String> = recs.into_iter().map(|r| r.0).collect();
            d.sort();
            d.dedup();
            let kind = if d.len() <= 1 {
                "repeated_record"
            } else if tenancy_key.is_some() && !any_sole {
                "co_ownership"
            } else {
                "competing_owners"
            };
            vec![Value::Str(k), Value::Int(n as i64), Value::Int(d.len() as i64), Value::Str(d.join("; ")), Value::Str(kind.into())]
        })
        .collect();
    if unplaced > 0 {
        out.push(vec![Value::Str("(unplaced)".into()), Value::Int(unplaced), Value::Null, Value::Null, Value::Str("missing_key".into())]);
    }
    Ok((vec!["parcel", "records", "distinct_owners", "owners", "kind"], out))
}

// ------------------------------------------------------------- rule_check

/// `land.rule_check(label, ifCond, thenCond[, groupKey[, filter[, idKey]]])` -> group, records, applicable, violations,
/// unknown, violation_rate, wilson_low, wilson_high, sample_ids.
/// A record is *applicable* when `ifCond` holds; it *violates* the rule when `thenCond` does not hold. A record where
/// either condition cannot be evaluated (the column is missing) is `unknown`, never a violation.
fn rule_check(view: &View, a: &[Value]) -> Result<Rows> {
    let label = need_str(a, 0, "label")?;
    let (cond_if, cond_then) = (need_str(a, 1, "ifCond")?, need_str(a, 2, "thenCond")?);
    let gkeys = opt_str(a, 3).map(keys_of).unwrap_or_default();
    let id_key = opt_str(a, 5);
    let ev = select(view, label, opt_str(a, 4))?;
    require_key(view, &ev, cond_if, "ifCond")?;
    require_key(view, &ev, cond_then, "thenCond")?;
    need_keys(view, &ev, &gkeys, "group key")?;
    #[derive(Default)]
    struct G {
        n: i64,
        applicable: i64,
        violations: i64,
        unknown: i64,
        ids: Vec<String>,
    }
    let mut groups: BTreeMap<String, G> = BTreeMap::new();
    for b in &ev {
        let g = groups.entry(group_label(view, b, &gkeys)).or_default();
        g.n += 1;
        match (flag(view, b, cond_if), flag(view, b, cond_then)) {
            (Some(true), Some(t)) => {
                g.applicable += 1;
                if !t {
                    g.violations += 1;
                    if let Some(k) = id_key {
                        g.ids.push(group_of(view, b, k).unwrap_or_default());
                    }
                }
            }
            (Some(false), _) => {}
            _ => g.unknown += 1,
        }
    }
    let mut rows: Vec<(String, G)> = groups.into_iter().collect();
    rows.sort_by(|x, y| y.1.violations.cmp(&x.1.violations).then_with(|| x.0.cmp(&y.0)));
    let out = rows
        .into_iter()
        .map(|(g, s)| {
            let (lo, hi) = wilson(s.violations as f64, s.applicable as f64);
            let rate = if s.applicable > 0 { s.violations as f64 / s.applicable as f64 } else { f64::NAN };
            vec![Value::Str(g), Value::Int(s.n), Value::Int(s.applicable), Value::Int(s.violations), Value::Int(s.unknown), f(rate), f(lo), f(hi), if id_key.is_some() { sample(&s.ids) } else { Value::Null }]
        })
        .collect();
    Ok((vec!["group", "records", "applicable", "violations", "unknown", "violation_rate", "wilson_low", "wilson_high", "sample_ids"], out))
}

// --------------------------------------------------------------- hotspots

/// `land.hotspots(label, groupKey, outcome[, filter[, minN[, side]]])` -> group, records, hits, rate, overall_rate, wilson_low,
/// wilson_high, z, p_value, flagged. `z = (rate - p0) / sqrt(p0 (1 - p0) / n)` against the overall rate `p0` (which
/// includes the group itself, so the test is slightly conservative). `flagged` is `p < 0.05 / groups_tested`; `side` is
/// `both` (default), `high` (only groups above the overall rate are flagged) or `low`. Use `high` when only an excess matters
/// (fraud, disputes): the default also flags unusually clean groups.
fn hotspots(view: &View, a: &[Value]) -> Result<Rows> {
    let label = need_str(a, 0, "label")?;
    let gkeys = keys_of(need_str(a, 1, "groupKey")?);
    let outcome = need_str(a, 2, "outcome")?;
    let min_n = usize_arg(a, 4, 10, "minN")?.max(1) as i64;
    let side = match opt_str(a, 5).map(|s| s.to_ascii_lowercase()).as_deref() {
        None | Some("both") => 0i8,
        Some("high") => 1,
        Some("low") => -1,
        Some(o) => return bad(format!("side must be both, high or low, not '{o}'")),
    };
    if gkeys.is_empty() {
        return bad("groupKey needs at least one column");
    }
    let ev = select(view, label, opt_str(a, 3))?;
    need_keys(view, &ev, &gkeys, "group key")?;
    require_key(view, &ev, outcome, "outcome")?;
    let mut groups: BTreeMap<String, (i64, i64)> = BTreeMap::new();
    let (mut tn, mut th) = (0i64, 0i64);
    for b in &ev {
        let Some(hit) = flag(view, b, outcome) else { continue };
        let Some(g) = joined(view, b, &gkeys) else { continue };
        let e = groups.entry(g).or_default();
        e.0 += 1;
        e.1 += hit as i64;
        tn += 1;
        th += hit as i64;
    }
    if tn == 0 {
        return bad(format!("outcome '{outcome}' could not be evaluated for any record"));
    }
    let p0 = th as f64 / tn as f64;
    let tested = groups.values().filter(|g| g.0 >= min_n).count().max(1) as f64;
    let mut rows: Vec<(String, i64, i64)> = groups.into_iter().filter(|(_, g)| g.0 >= min_n).map(|(k, g)| (k, g.0, g.1)).collect();
    let stat = |n: i64, h: i64| -> (f64, f64) {
        let sd = (p0 * (1.0 - p0) / n as f64).sqrt();
        if sd > 0.0 {
            let z = (h as f64 / n as f64 - p0) / sd;
            (z, p_two_sided(z))
        } else {
            (f64::NAN, f64::NAN)
        }
    };
    rows.sort_by(|x, y| {
        let (zx, zy) = (stat(x.1, x.2).0, stat(y.1, y.2).0);
        zy.total_cmp(&zx).then_with(|| x.0.cmp(&y.0))
    });
    let out = rows
        .into_iter()
        .map(|(g, n, h)| {
            let (lo, hi) = wilson(h as f64, n as f64);
            let (z, p) = stat(n, h);
            vec![Value::Str(g), Value::Int(n), Value::Int(h), f(h as f64 / n as f64), f(p0), f(lo), f(hi), f(z), f(p), Value::Bool(p.is_finite() && p < 0.05 / tested && (side == 0 || (side > 0) == (z > 0.0)))]
        })
        .collect();
    Ok((vec!["group", "records", "hits", "rate", "overall_rate", "wilson_low", "wilson_high", "z", "p_value", "flagged"], out))
}

// -------------------------------------------------------------- valuation

/// `land.valuation(label, valueKey, areaKey[, groupKey[, filter]])` -> group, records, excluded, total_value, total_area,
/// weighted_value_per_area, median_value_per_area, p25, p75, fence_low, fence_high, outliers_low, outliers_high.
fn valuation(view: &View, a: &[Value]) -> Result<Rows> {
    let label = need_str(a, 0, "label")?;
    let (vk, ak) = (need_str(a, 1, "valueKey")?, need_str(a, 2, "areaKey")?);
    let gkeys = opt_str(a, 3).map(keys_of).unwrap_or_default();
    let ev = select(view, label, opt_str(a, 4))?;
    require_key(view, &ev, vk, "value key")?;
    require_key(view, &ev, ak, "area key")?;
    need_keys(view, &ev, &gkeys, "group key")?;
    #[derive(Default)]
    struct G {
        excluded: i64,
        value: f64,
        area: f64,
        ppa: Vec<f64>,
    }
    let mut groups: BTreeMap<String, G> = BTreeMap::new();
    for b in &ev {
        let g = groups.entry(group_label(view, b, &gkeys)).or_default();
        match (num(view, b, vk), num(view, b, ak)) {
            (Some(v), Some(ar)) if v > 0.0 && ar > 0.0 => {
                g.value += v;
                g.area += ar;
                g.ppa.push(v / ar);
            }
            _ => g.excluded += 1,
        }
    }
    let mut rows: Vec<(String, G)> = groups.into_iter().collect();
    rows.sort_by(|x, y| y.1.value.total_cmp(&x.1.value).then_with(|| x.0.cmp(&y.0)));
    let out = rows
        .into_iter()
        .map(|(g, s)| {
            if s.ppa.is_empty() {
                return vec![Value::Str(g), Value::Int(0), Value::Int(s.excluded), Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Null];
            }
            let logs = sorted(&s.ppa.iter().map(|x| x.ln()).collect::<Vec<_>>());
            let (q1, q3) = (quantile(&logs, 0.25), quantile(&logs, 0.75));
            let iqr = q3 - q1;
            let (lo, hi) = (q1 - 1.5 * iqr, q3 + 1.5 * iqr);
            let (olo, ohi) = (logs.iter().filter(|x| **x < lo).count(), logs.iter().filter(|x| **x > hi).count());
            let ppa = sorted(&s.ppa);
            vec![
                Value::Str(g),
                Value::Int(s.ppa.len() as i64),
                Value::Int(s.excluded),
                f(s.value),
                f(s.area),
                f(s.value / s.area),
                f(quantile(&ppa, 0.5)),
                f(q1.exp()),
                f(q3.exp()),
                f(lo.exp()),
                f(hi.exp()),
                Value::Int(olo as i64),
                Value::Int(ohi as i64),
            ]
        })
        .collect();
    Ok((vec!["group", "records", "excluded", "total_value", "total_area", "weighted_value_per_area", "median_value_per_area", "p25", "p75", "fence_low", "fence_high", "outliers_low", "outliers_high"], out))
}

// ------------------------------------------------------------ survey_check

/// Split a survey number into `(base, Some(sub))`. Well formed: digits with an optional letter, then an optional
/// `/digits` (`774`, `12A`, `774/4`). Anything else is `None`.
fn parse_survey(s: &str) -> Option<(String, Option<String>)> {
    let s = s.trim();
    let (base, sub) = match s.split_once('/') {
        Some((b, u)) => (b, Some(u)),
        None => (s, None),
    };
    let digits = base.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let suffix_ok = base.len() - digits.len() <= 1;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) || !suffix_ok {
        return None;
    }
    match sub {
        Some(u) if u.is_empty() || !u.bytes().all(|b| b.is_ascii_digit()) => None,
        Some(u) => Some((base.to_string(), Some(u.to_string()))),
        None => Some((base.to_string(), None)),
    }
}

/// `land.survey_check(label, surveyKey, subKey[, groupKey[, filter[, idKey]]])` -> group, records, malformed,
/// without_subdivision, suffix_matches, suffix_differs, differs_rate, sample_ids. `differs_rate` is
/// `suffix_differs / (suffix_matches + suffix_differs)`. Sub-divisions compare as integers when both parse, so `04`
/// equals `4`.
fn survey_check(view: &View, a: &[Value]) -> Result<Rows> {
    let label = need_str(a, 0, "label")?;
    let (sk, uk) = (need_str(a, 1, "surveyKey")?, need_str(a, 2, "subKey")?);
    let gkeys = opt_str(a, 3).map(keys_of).unwrap_or_default();
    let id_key = opt_str(a, 5);
    let ev = select(view, label, opt_str(a, 4))?;
    require_key(view, &ev, sk, "survey key")?;
    require_key(view, &ev, uk, "sub-division key")?;
    need_keys(view, &ev, &gkeys, "group key")?;
    #[derive(Default)]
    struct G {
        n: i64,
        malformed: i64,
        none: i64,
        same: i64,
        diff: i64,
        ids: Vec<String>,
    }
    let mut groups: BTreeMap<String, G> = BTreeMap::new();
    for b in &ev {
        let g = groups.entry(group_label(view, b, &gkeys)).or_default();
        g.n += 1;
        let Some(sv) = group_of(view, b, sk) else {
            g.malformed += 1;
            continue;
        };
        let Some((_, sub)) = parse_survey(&sv) else {
            g.malformed += 1;
            if let Some(k) = id_key {
                g.ids.push(group_of(view, b, k).unwrap_or_default());
            }
            continue;
        };
        let Some(sub) = sub else {
            g.none += 1;
            continue;
        };
        let col = group_of(view, b, uk).unwrap_or_default();
        let same = match (sub.parse::<u64>(), col.trim().parse::<u64>()) {
            (Ok(x), Ok(y)) => x == y,
            _ => sub == col.trim(),
        };
        if same {
            g.same += 1;
        } else {
            g.diff += 1;
            if let Some(k) = id_key {
                g.ids.push(group_of(view, b, k).unwrap_or_default());
            }
        }
    }
    let mut rows: Vec<(String, G)> = groups.into_iter().collect();
    rows.sort_by(|x, y| (y.1.diff + y.1.malformed).cmp(&(x.1.diff + x.1.malformed)).then_with(|| x.0.cmp(&y.0)));
    let out = rows
        .into_iter()
        .map(|(g, s)| {
            let denom = s.same + s.diff;
            vec![
                Value::Str(g),
                Value::Int(s.n),
                Value::Int(s.malformed),
                Value::Int(s.none),
                Value::Int(s.same),
                Value::Int(s.diff),
                fo(if denom > 0 { Some(s.diff as f64 / denom as f64) } else { None }),
                if id_key.is_some() { sample(&s.ids) } else { Value::Null },
            ]
        })
        .collect();
    Ok((vec!["group", "records", "malformed", "without_subdivision", "suffix_matches", "suffix_differs", "differs_rate", "sample_ids"], out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn erfc_matches_reference_values() {
        // reference values from mpmath
        let cases = [(0.0, 1.0), (0.5, 0.4795001221869535), (1.0, 0.15729920705028513), (2.0, 0.004677734981047266), (2.9, 4.1097e-5), (3.0, 2.209049699858544e-5), (4.0, 1.541725790028002e-8), (6.0, 2.1519736712498913e-17)];
        for (x, want) in cases {
            let got = erfc(x);
            assert!(((got - want) / want).abs() < 1e-3 || (got - want).abs() < 1e-12, "erfc({x}) = {got}, want {want}");
        }
        assert!(((erfc(1.0) - 0.15729920705028513) / 0.15729920705028513).abs() < 1e-10);
        assert!(((erfc(3.0) - 2.209049699858544e-5) / 2.209049699858544e-5).abs() < 1e-9);
        assert!(((erfc(4.0) - 1.541725790028002e-8) / 1.541725790028002e-8).abs() < 1e-9);
        assert!((erfc(-1.0) - (2.0 - 0.15729920705028513)).abs() < 1e-12);
    }

    #[test]
    fn survey_numbers_parse() {
        assert_eq!(parse_survey("774/4"), Some(("774".into(), Some("4".into()))));
        assert_eq!(parse_survey(" 12A/3 "), Some(("12A".into(), Some("3".into()))));
        assert_eq!(parse_survey("774"), Some(("774".into(), None)));
        for bad in ["", "/4", "774/", "77/4/1", "abc", "12AB/1", "7 74/1", "774/x", "-5/1"] {
            assert_eq!(parse_survey(bad), None, "{bad}");
        }
    }
}
