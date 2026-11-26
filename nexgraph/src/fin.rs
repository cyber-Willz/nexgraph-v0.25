//! `fin.*`: consistency checks on financial-statement facts stored as one edge per fact (the SEC Financial Statement Data
//! Sets, XBRL exports, any long `entity, period, item, value` table).
//!
//! `fin.identity(label, entityKey, periodKey, tagKey, valueKey, totalTag, partTags[, tolPct[, blankKeys[, filter[, limit]]]])`
//! checks, for every (entity, period) that reports `totalTag`, that `totalTag = sum(partTags)`. `partTags` is a comma list;
//! a part prefixed with `?` is optional (missing counts as 0), the others must all be present or the group is `incomplete`
//! rather than a violation. `blankKeys` lists properties that must be **absent** for a fact to count: in the SEC files an
//! empty `segments` / `coreg` cell means the consolidated figure, so `segments,coreg` selects consolidated facts. A group
//! with two different values for one tag is `ambiguous` and is reported, not guessed at.
//!
//! Output: `kind`, `entity`, `period`, `total`, `parts_sum`, `gap`, `gap_pct`, `detail`. `summary` rows first (counts in
//! `total`), then up to `limit` (default 50) `violation` rows, largest relative gap first. `tolPct` (default 0.01) is the
//! relative gap, in percent of the total, that still counts as a match.
use crate::alm::select;
use crate::error::Result;
use crate::killchain::{f, group_of, lookup, num, opt_str, require_key};
use crate::netgraph::*;
use crate::store::View;
use crate::value::Value;
use std::collections::BTreeMap;

pub fn call(view: &View, name: &str, a: &[Value]) -> Result<Rows> {
    match name {
        "fin.identity" => identity(view, a),
        o => bad(format!("unknown procedure {o}; the fin procedures are fin.identity")),
    }
}

fn identity(view: &View, a: &[Value]) -> Result<Rows> {
    let label = need_str(a, 0, "label")?;
    let (ent, per, tagk, valk) = (need_str(a, 1, "entityKey")?, need_str(a, 2, "periodKey")?, need_str(a, 3, "tagKey")?, need_str(a, 4, "valueKey")?);
    let total_tag = need_str(a, 5, "totalTag")?;
    let parts: Vec<(&str, bool)> = need_str(a, 6, "partTags")?.split(',').map(str::trim).filter(|s| !s.is_empty()).map(|s| match s.strip_prefix('?') {
        Some(t) => (t, true),
        None => (s, false),
    }).collect();
    if parts.is_empty() {
        return bad("partTags: give at least one tag");
    }
    let tol = match a.get(7) {
        None | Some(Value::Null) => 0.01,
        Some(v) => v.as_f64().filter(|x| *x >= 0.0).ok_or_else(|| crate::error::Error::Query("tolPct must be a number >= 0".into()))?,
    };
    let blanks: Vec<&str> = opt_str(a, 8).map(|s| s.split(',').map(str::trim).filter(|s| !s.is_empty()).collect()).unwrap_or_default();
    let limit = usize_arg(a, 10, 50, "limit")?;
    let evs = select(view, label, opt_str(a, 9))?;
    for k in [ent, per, tagk, valk] {
        require_key(view, &evs, k, "key")?;
    }
    #[derive(Default)]
    struct G {
        total: Vec<f64>,
        parts: BTreeMap<String, Vec<f64>>,
    }
    let mut groups: BTreeMap<(String, String), G> = BTreeMap::new();
    let mut facts = 0i64;
    for b in &evs {
        if blanks.iter().any(|k| lookup(view, b, k).is_some()) {
            continue;
        }
        let (Some(e), Some(p), Some(t), Some(v)) = (group_of(view, b, ent), group_of(view, b, per), group_of(view, b, tagk), num(view, b, valk)) else { continue };
        facts += 1;
        let g = groups.entry((e, p)).or_default();
        if t == total_tag {
            g.total.push(v);
        } else if parts.iter().any(|(pt, _)| *pt == t) {
            g.parts.entry(t).or_default().push(v);
        }
    }
    let (mut with_total, mut checked, mut pass, mut incomplete, mut ambiguous) = (0i64, 0i64, 0i64, 0i64, 0i64);
    let mut bad_rows: Vec<(f64, Vec<Value>)> = Vec::new();
    let same = |v: &[f64]| v.iter().all(|x| (x - v[0]).abs() <= 1e-9 * v[0].abs().max(1.0));
    for ((e, p), g) in &groups {
        if g.total.is_empty() {
            continue;
        }
        with_total += 1;
        if !same(&g.total) || g.parts.values().any(|v| !same(v)) {
            ambiguous += 1;
            continue;
        }
        let missing: Vec<&str> = parts.iter().filter(|(t, opt)| !opt && !g.parts.contains_key(*t)).map(|(t, _)| *t).collect();
        if !missing.is_empty() {
            incomplete += 1;
            continue;
        }
        checked += 1;
        let total = g.total[0];
        let sum: f64 = g.parts.values().map(|v| v[0]).sum();
        let gap = total - sum;
        let pct = if total != 0.0 { gap.abs() / total.abs() * 100.0 } else if gap == 0.0 { 0.0 } else { f64::INFINITY };
        if pct <= tol {
            pass += 1;
        } else {
            let detail = parts.iter().map(|(t, _)| format!("{t}={}", g.parts.get(*t).map_or("absent".to_string(), |v| v[0].to_string()))).collect::<Vec<_>>().join("; ");
            bad_rows.push((pct, vec![Value::Str("violation".into()), Value::Str(e.clone()), Value::Str(p.clone()), f(total), f(sum), f(gap), f(pct), Value::Str(detail)]));
        }
    }
    bad_rows.sort_by(|x, y| y.0.partial_cmp(&x.0).unwrap_or(std::cmp::Ordering::Equal));
    let s = |name: &str, n: i64, d: &str| vec![Value::Str("summary".into()), Value::Str(name.into()), Value::Null, Value::Int(n), Value::Null, Value::Null, Value::Null, Value::Str(d.into())];
    let mut rows = vec![
        s("facts_used", facts, "facts that passed the filter and blank-key rules and have a numeric value"),
        s("groups_with_total", with_total, "(entity, period) groups that report the total tag"),
        s("checked", checked, "groups with the total and every required part, one value per tag"),
        s("pass", pass, "within tolerance"),
        s("violations", checked - pass, "outside tolerance"),
        s("incomplete", incomplete, "a required part is missing: not judged"),
        s("ambiguous", ambiguous, "two different values for one tag in the same group: not judged"),
    ];
    rows.extend(bad_rows.into_iter().take(limit).map(|(_, r)| r));
    Ok((vec!["kind", "entity", "period", "total", "parts_sum", "gap", "gap_pct", "detail"], rows))
}
