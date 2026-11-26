//! Kill-chain / incident-portfolio analytics (v0.10).
//!
//! Built from what the 100,500-row *Enterprise Cyber Kill Chain* file needed: one incident per row, a stage
//! on the kill chain, binary security controls (MFA, EDR, ...), an outcome flag, dollar loss and detection
//! times. Every procedure reads edges (or hyperedges) with a given label, so it works on the output of
//! `ingest::import_csv_hyperedges_with(.., keep_member_props: true)`; a key is looked up as an edge property
//! first and, if absent, as the name of the member node carrying that label (so `'Attack_Vector'` works with
//! or without `keep_member_props`).
//!
//! | procedure | question it answers |
//! |---|---|
//! | `killchain.funnel(label, stageKey, outcomeKey[, order[, lossKey[, detectKey]]])` | how many incidents reach each stage, how often do they succeed, what do they cost |
//! | `killchain.control_effect(label, controlKey, outcomeKey[, groupKey])` | does a binary control reduce the outcome rate? crude risk ratio, per-stratum ratios and a Mantel-Haenszel pooled ratio |
//! | `killchain.exposure(label, groupKey, outcomeKey, lossKey[, minN])` | which segments lose the most money, with Wilson intervals on their success rate |
//! | `killchain.response(label, groupKey, detectKey, respondKey)` | mean time to detect / respond, with medians and p90 |
//! | `killchain.defense_depth(label, [controlKeys], outcomeKey[, lossKey])` | outcome rate as a function of how many controls are in place |
//!
//! Statistical notes (also in `HOW_IT_WORKS.md`): rates carry 95 % Wilson intervals; risk ratios carry 95 %
//! log-normal intervals and two-sided z-test p-values (Greenland-Robins variance for the Mantel-Haenszel
//! estimate). These describe *association in the data you loaded*. **Stratify only by variables that exist
//! before the control acts (vector, size, industry), never by a mediator such as the stage the attack reached:
//! a control that works by stopping attacks early looks useless once you condition on how far they got**
//! (in the kill-chain file MFA lowers success from 37.4 % to 26.8 % overall but shows ~0 effect within stage).

use crate::error::Result;
use crate::model::Body;
use crate::netgraph::*;
use crate::store::View;
use crate::value::Value;
use std::collections::BTreeMap;

pub(crate) fn f(x: f64) -> Value {
    if x.is_finite() {
        Value::Float(x)
    } else {
        Value::Null
    }
}

pub(crate) fn fo(x: Option<f64>) -> Value {
    x.map_or(Value::Null, f)
}

pub(crate) fn call(view: &View, name: &str, a: &[Value]) -> Result<Rows> {
    match name {
        "funnel" => funnel(view, a),
        "control_effect" => control_effect(view, a),
        "exposure" => exposure(view, a),
        "response" => response(view, a),
        "defense_depth" => defense_depth(view, a),
        _ => bad(format!("unknown procedure 'killchain.{name}' (available: killchain.funnel, killchain.control_effect, killchain.exposure, killchain.response, killchain.defense_depth)")),
    }
}

// ------------------------------------------------------------------ helpers

fn truthy(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::Int(0) => Some(false),
        Value::Int(1) => Some(true),
        Value::Float(x) if *x == 0.0 => Some(false),
        Value::Float(x) if *x == 1.0 => Some(true),
        Value::Str(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "y" | "1" | "success" => Some(true),
            "false" | "no" | "n" | "0" | "failure" | "failed" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

pub(crate) fn text(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        o => o.to_string(),
    }
}

/// Property `key` of the event, else the name of the member node labelled `key`.
pub(crate) fn lookup(view: &View, b: &Body, key: &str) -> Option<Value> {
    if let Some(v) = b.props().get(key) {
        return Some(v.clone());
    }
    for m in b.members() {
        if let Some(nb) = view.node(m) {
            if nb.has_label(key) {
                let p = nb.props();
                return p.get("name").cloned().or_else(|| p.values().next().cloned());
            }
        }
    }
    // `Label.prop` (v0.12.1): a property of the member node with that label, so a record reaches the columns of a
    // table joined onto its member nodes (`Customer.Credit_Score`). Only tried when no edge property or label of
    // that exact name exists, so dotted column names such as `int.rate` still resolve as plain properties.
    if let Some((label, prop)) = key.split_once('.') {
        if !label.is_empty() && !prop.is_empty() {
            for m in b.members() {
                if let Some(nb) = view.node(m) {
                    if nb.has_label(label) {
                        if let Some(v) = nb.props().get(prop) {
                            return Some(v.clone());
                        }
                    }
                }
            }
        }
    }
    None
}

pub(crate) fn group_of(view: &View, b: &Body, key: &str) -> Option<String> {
    lookup(view, b, key).map(|v| text(&v))
}

pub(crate) fn num(view: &View, b: &Body, key: &str) -> Option<f64> {
    lookup(view, b, key).and_then(|v| v.as_f64()).filter(|x| x.is_finite())
}

/// `Key=Value` / `Key!=Value`: an outcome or control defined by a category instead of a 0/1 column, e.g.
/// `Category!=Normal` (any attack), `Severity=Critical`, `Zone=Cloud`. Compared as text.
pub(crate) fn split_cmp(key: &str) -> Option<(&str, bool, &str)> {
    let (k, neg, v) = match key.split_once("!=") {
        Some((k, v)) => (k, true, v),
        None => {
            let (k, v) = key.split_once('=')?;
            (k, false, v)
        }
    };
    (!k.is_empty() && !v.is_empty()).then_some((k, neg, v))
}

pub(crate) fn flag(view: &View, b: &Body, key: &str) -> Option<bool> {
    if let Some((k, neg, v)) = split_cmp(key) {
        let got = lookup(view, b, k)?;
        return Some((text(&got) == v) != neg);
    }
    lookup(view, b, key).and_then(|v| truthy(&v))
}

pub(crate) fn events<'v>(view: &'v View, label: &str) -> Result<Vec<&'v Body>> {
    let v: Vec<&Body> = view.edges_with_label(label).into_iter().filter_map(|id| view.edge(id)).collect();
    if v.is_empty() {
        return bad(format!("no edges or hyperedges labelled '{label}' (killchain procedures read events stored as edges)"));
    }
    Ok(v)
}

/// Fail loudly when a key is carried by no event, naming what the first event does carry. (Silent empty
/// output was the failure mode v0.9 fixed for timestamps; the same rule applies here.)
pub(crate) fn require_key(view: &View, ev: &[&Body], key: &str, what: &str) -> Result<()> {
    let key = split_cmp(key).map_or(key, |(k, _, _)| k);
    if ev.iter().any(|b| lookup(view, b, key).is_some()) {
        return Ok(());
    }
    let have: Vec<String> = ev[0].props().keys().take(12).cloned().collect();
    bad(format!("{what} '{key}' is not a property of any event and not a member label; first event has properties: {}", have.join(", ")))
}

/// An outcome or control must be usable (0/1, true/false, or a `Key=Value` test) for at least half of the
/// events. Before v0.10.1 a continuous column such as a 0-1 score was accepted and the rate was silently
/// computed from the few rows that happened to equal exactly 0 or 1.
fn require_binary(view: &View, ev: &[&Body], key: &str, what: &str) -> Result<()> {
    require_key(view, ev, key, what)?;
    // A numeric column that is mostly 0/1 but carries other values (a mostly-zero amount, a 0-1 score) is not a
    // flag: found by the ALM dataset, where a mostly-zero cash-flow column passed the test below.
    if split_cmp(key).is_none() {
        let (mut known, mut other) = (0usize, 0usize);
        for b in ev {
            if let Some(x) = lookup(view, b, key).and_then(|v| v.as_f64()) {
                known += 1;
                if x != 0.0 && x != 1.0 {
                    other += 1;
                }
            }
        }
        if other > 0 && other * 100 > known {
            let usable = ev.iter().filter(|b| flag(view, b, key).is_some()).count();
            return bad(format!(
                "{what} '{key}' is a 0/1 or true/false value for only {usable} of {} events ({other} of its {known} numeric values are neither 0 nor 1, so it looks like an amount or a score); use a binary column such as flag_*, or compare a category with 'Key=Value' or 'Key!=Value' (for example 'Category!=Normal')",
                ev.len()
            ));
        }
    }
    let usable = ev.iter().filter(|b| flag(view, b, key).is_some()).count();
    if usable * 2 < ev.len() {
        return bad(format!(
            "{what} '{key}' is a 0/1 or true/false value for only {usable} of {} events; use a binary column, or compare a category with 'Key=Value' or 'Key!=Value' (for example 'Category!=Normal')",
            ev.len()
        ));
    }
    Ok(())
}

pub(crate) fn list_arg(a: &[Value], i: usize) -> Option<Vec<String>> {
    match a.get(i) {
        Some(Value::List(l)) => Some(l.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect()),
        _ => None,
    }
}

pub(crate) fn opt_str<'a>(a: &'a [Value], i: usize) -> Option<&'a str> {
    match a.get(i) {
        Some(Value::Null) | None => None,
        _ => str_arg(a, i),
    }
}

/// 95 % Wilson score interval for `k` successes out of `n`.
pub(crate) fn wilson(k: f64, n: f64) -> (f64, f64) {
    if n <= 0.0 {
        return (f64::NAN, f64::NAN);
    }
    let z = 1.959963984540054_f64;
    let p = k / n;
    let d = 1.0 + z * z / n;
    let c = (p + z * z / (2.0 * n)) / d;
    let h = z * (p * (1.0 - p) / n + z * z / (4.0 * n * n)).sqrt() / d;
    ((c - h).max(0.0), (c + h).min(1.0))
}

fn norm_cdf(x: f64) -> f64 {
    if x < -8.0 {
        return 0.0;
    }
    if x > 8.0 {
        return 1.0;
    }
    // Marsaglia (2004): Phi(x) = 1/2 + phi(x) (x + x^3/3 + x^5/(3*5) + ...)
    let (mut s, mut t, x2) = (x, x, x * x);
    for i in 1..400 {
        t *= x2 / (2 * i + 1) as f64;
        s += t;
        if t.abs() < 1e-17 * s.abs() {
            break;
        }
    }
    0.5 + s * (-x2 / 2.0).exp() / (2.0 * std::f64::consts::PI).sqrt()
}

/// Upper tail `P(Z > z)` for `z >= 0`, accurate in relative terms far into the tail.
fn norm_sf(z: f64) -> f64 {
    if z < 3.0 {
        return 1.0 - norm_cdf(z);
    }
    // continued fraction  Q(z) = phi(z) / (z + 1/(z + 2/(z + 3/(z + ...))))
    let mut cf = z;
    for k in (1..=80).rev() {
        cf = z + k as f64 / cf;
    }
    (-z * z / 2.0).exp() / (2.0 * std::f64::consts::PI).sqrt() / cf
}

fn p_two_sided(z: f64) -> f64 {
    if !z.is_finite() {
        return f64::NAN;
    }
    (2.0 * norm_sf(z.abs())).min(1.0)
}

#[derive(Default, Clone)]
struct Acc {
    n: usize,
    known: usize,
    pos: usize,
    loss: Vec<f64>,
    det: Vec<f64>,
}

impl Acc {
    fn rate(&self) -> f64 {
        if self.known == 0 {
            f64::NAN
        } else {
            self.pos as f64 / self.known as f64
        }
    }
}

fn describe(x: &[f64]) -> (Option<f64>, Option<f64>, Option<f64>, f64) {
    if x.is_empty() {
        return (None, None, None, 0.0);
    }
    let s = sorted(x);
    (Some(mean(x)), Some(quantile(&s, 0.5)), Some(quantile(&s, 0.9)), x.iter().sum())
}

// ------------------------------------------------------------------- funnel

/// `killchain.funnel(label, stageKey, outcomeKey[, order[, lossKey[, detectKey]]])` ->
/// stage, rank, incidents, share, reached_share, outcome_known, success_rate, mean_detect, mean_loss, total_loss.
/// `order` lists stages from earliest to latest; without it stages are ranked by ascending success rate (a
/// heuristic: later kill-chain stages succeed more often) and `reached_share` should be read with that in mind.
/// `reached_share` is the share of incidents whose stage is this one or any later one.
fn funnel(view: &View, a: &[Value]) -> Result<Rows> {
    let label = need_str(a, 0, "label")?;
    let stage_key = need_str(a, 1, "stageKey")?;
    let out_key = need_str(a, 2, "outcomeKey")?;
    let order = list_arg(a, 3);
    let loss_key = opt_str(a, 4);
    let det_key = opt_str(a, 5);
    let ev = events(view, label)?;
    require_key(view, &ev, stage_key, "stageKey")?;
    require_binary(view, &ev, out_key, "outcomeKey")?;
    let mut by: BTreeMap<String, Acc> = BTreeMap::new();
    for b in &ev {
        let Some(st) = group_of(view, b, stage_key) else { continue };
        let acc = by.entry(st).or_default();
        acc.n += 1;
        if let Some(o) = flag(view, b, out_key) {
            acc.known += 1;
            acc.pos += o as usize;
        }
        if let Some(k) = loss_key {
            acc.loss.extend(num(view, b, k));
        }
        if let Some(k) = det_key {
            acc.det.extend(num(view, b, k));
        }
    }
    let mut stages: Vec<(String, Acc)> = by.into_iter().collect();
    match &order {
        Some(o) => {
            let pos = |s: &str| o.iter().position(|x| x == s).unwrap_or(usize::MAX);
            stages.sort_by(|x, y| pos(&x.0).cmp(&pos(&y.0)).then(x.0.cmp(&y.0)));
        }
        None => stages.sort_by(|x, y| x.1.rate().total_cmp(&y.1.rate()).then(x.0.cmp(&y.0))),
    }
    let total: usize = stages.iter().map(|s| s.1.n).sum();
    let mut later = total;
    let mut rows = Vec::new();
    for (i, (st, acc)) in stages.iter().enumerate() {
        let (md, _, _, _) = describe(&acc.det);
        let (ml, _, _, tl) = describe(&acc.loss);
        rows.push(vec![
            Value::Str(st.clone()),
            Value::Int(i as i64 + 1),
            Value::Int(acc.n as i64),
            f(acc.n as f64 / total as f64),
            f(later as f64 / total as f64),
            Value::Int(acc.known as i64),
            f(acc.rate()),
            fo(md),
            fo(ml),
            f(tl),
        ]);
        later -= acc.n;
    }
    Ok((vec!["stage", "rank", "incidents", "share", "reached_share", "outcome_known", "success_rate", "mean_detect", "mean_loss", "total_loss"], rows))
}

// ----------------------------------------------------------- control effect

#[derive(Default, Clone, Copy)]
struct Two {
    /// control present: events with outcome / total
    a: f64,
    n1: f64,
    /// control absent
    c: f64,
    n0: f64,
    dropped: usize,
}

fn ratio_row(group: &str, method: &str, t: &Two, rr: f64, lo: f64, hi: f64, p: f64) -> Vec<Value> {
    let (r1, r0) = (t.a / t.n1, t.c / t.n0);
    vec![
        Value::Str(group.to_string()),
        Value::Str(method.to_string()),
        Value::Int(t.n1 as i64),
        Value::Int(t.n0 as i64),
        f(r1),
        f(r0),
        f(r0 - r1),
        f(rr),
        f(lo),
        f(hi),
        f(p),
        Value::Int(t.dropped as i64),
    ]
}

/// Crude risk ratio, interval and p-value from one 2x2 table.
fn crude(t: &Two) -> (f64, f64, f64, f64) {
    let rr = (t.a / t.n1) / (t.c / t.n0);
    let se = (1.0 / t.a - 1.0 / t.n1 + 1.0 / t.c - 1.0 / t.n0).sqrt();
    let (lo, hi) = ((rr.ln() - 1.959963984540054 * se).exp(), (rr.ln() + 1.959963984540054 * se).exp());
    // pooled two-proportion z-test
    let p0 = (t.a + t.c) / (t.n1 + t.n0);
    let z = (t.a / t.n1 - t.c / t.n0) / (p0 * (1.0 - p0) * (1.0 / t.n1 + 1.0 / t.n0)).sqrt();
    (rr, lo, hi, p_two_sided(z))
}

/// `killchain.control_effect(label, controlKey, outcomeKey[, groupKey])` ->
/// group, method, n_with, n_without, risk_with, risk_without, abs_reduction, relative_risk, rr_low, rr_high,
/// p_value, n_dropped.
/// `risk_with` is the outcome rate among incidents where the control was in place; `abs_reduction` is
/// `risk_without - risk_with` (positive = the control helps); `relative_risk` < 1 means it helps.
/// Without `groupKey` one `crude` row. With it: one `stratum` row per group value and a final
/// `mantel-haenszel` row pooling them (see the module note on what to stratify by).
fn control_effect(view: &View, a: &[Value]) -> Result<Rows> {
    let label = need_str(a, 0, "label")?;
    let ctl = need_str(a, 1, "controlKey")?;
    let out = need_str(a, 2, "outcomeKey")?;
    let grp = opt_str(a, 3);
    let ev = events(view, label)?;
    require_binary(view, &ev, ctl, "controlKey")?;
    require_binary(view, &ev, out, "outcomeKey")?;
    if let Some(g) = grp {
        require_key(view, &ev, g, "groupKey")?;
    }
    let mut all = Two::default();
    let mut strata: BTreeMap<String, Two> = BTreeMap::new();
    for b in &ev {
        let (c, o) = (flag(view, b, ctl), flag(view, b, out));
        let g = grp.map(|k| group_of(view, b, k));
        let (Some(c), Some(o)) = (c, o) else {
            all.dropped += 1;
            continue;
        };
        let upd = |t: &mut Two| {
            if c {
                t.n1 += 1.0;
                t.a += o as u8 as f64;
            } else {
                t.n0 += 1.0;
                t.c += o as u8 as f64;
            }
        };
        upd(&mut all);
        if let Some(Some(g)) = g {
            upd(strata.entry(g).or_default());
        }
    }
    if all.n1 == 0.0 || all.n0 == 0.0 {
        return bad(format!("control '{ctl}' takes only one value in this data ({} present, {} absent); nothing to compare", all.n1, all.n0));
    }
    let mut rows = Vec::new();
    let (rr, lo, hi, p) = crude(&all);
    rows.push(ratio_row("(all)", "crude", &all, rr, lo, hi, p));
    if grp.is_some() {
        let (mut r_num, mut r_den, mut var_num) = (0.0, 0.0, 0.0);
        for (g, t) in &strata {
            let nn = t.n1 + t.n0;
            if t.n1 > 0.0 && t.n0 > 0.0 {
                let (rr, lo, hi, p) = if t.a > 0.0 && t.c > 0.0 { crude(t) } else { (f64::NAN, f64::NAN, f64::NAN, f64::NAN) };
                rows.push(ratio_row(g, "stratum", t, rr, lo, hi, p));
                r_num += t.a * t.n0 / nn;
                r_den += t.c * t.n1 / nn;
                var_num += (t.n1 * t.n0 * (t.a + t.c) - t.a * t.c * nn) / (nn * nn);
            }
        }
        if r_num > 0.0 && r_den > 0.0 {
            let rr = r_num / r_den;
            let se = (var_num / (r_num * r_den)).sqrt();
            let z = rr.ln() / se;
            let (lo, hi) = ((rr.ln() - 1.959963984540054 * se).exp(), (rr.ln() + 1.959963984540054 * se).exp());
            rows.push(ratio_row("(pooled)", "mantel-haenszel", &all, rr, lo, hi, p_two_sided(z)));
        }
    }
    Ok((vec!["group", "method", "n_with", "n_without", "risk_with", "risk_without", "abs_reduction", "relative_risk", "rr_low", "rr_high", "p_value", "n_dropped"], rows))
}

// ----------------------------------------------------------------- exposure

/// `killchain.exposure(label, groupKey, outcomeKey, lossKey[, minN])` ->
/// group, incidents, successes, outcome_known, success_rate, ci_low, ci_high, loss_n, mean_loss, median_loss, p90_loss,
/// total_loss, loss_share. Groups with fewer than `minN` incidents (default 1) are left out; rows are sorted
/// by `total_loss`, largest first. `loss_n` counts incidents that carry a loss value (the rest are missing,
/// not zero), and `loss_share` is the group's share of the loss summed over the listed groups.
fn exposure(view: &View, a: &[Value]) -> Result<Rows> {
    let label = need_str(a, 0, "label")?;
    let grp = need_str(a, 1, "groupKey")?;
    let out = need_str(a, 2, "outcomeKey")?;
    let loss = need_str(a, 3, "lossKey")?;
    let min_n = usize_arg(a, 4, 1, "minN")?;
    let ev = events(view, label)?;
    for (k, w) in [(grp, "groupKey"), (loss, "lossKey")] {
        require_key(view, &ev, k, w)?;
    }
    require_binary(view, &ev, out, "outcomeKey")?;
    let mut by: BTreeMap<String, Acc> = BTreeMap::new();
    for b in &ev {
        let Some(g) = group_of(view, b, grp) else { continue };
        let acc = by.entry(g).or_default();
        acc.n += 1;
        if let Some(o) = flag(view, b, out) {
            acc.known += 1;
            acc.pos += o as usize;
        }
        acc.loss.extend(num(view, b, loss));
    }
    let mut rows: Vec<(f64, Vec<Value>)> = Vec::new();
    let grand: f64 = by.values().filter(|x| x.n >= min_n).map(|x| x.loss.iter().sum::<f64>()).sum();
    for (g, acc) in by.iter().filter(|(_, x)| x.n >= min_n) {
        let (lo, hi) = wilson(acc.pos as f64, acc.known as f64);
        let (m, med, p90, tot) = describe(&acc.loss);
        rows.push((
            tot,
            vec![
                Value::Str(g.clone()),
                Value::Int(acc.n as i64),
                Value::Int(acc.pos as i64),
                Value::Int(acc.known as i64),
                f(acc.rate()),
                f(lo),
                f(hi),
                Value::Int(acc.loss.len() as i64),
                fo(m),
                fo(med),
                fo(p90),
                f(tot),
                f(if grand > 0.0 { tot / grand } else { f64::NAN }),
            ],
        ));
    }
    rows.sort_by(|x, y| y.0.total_cmp(&x.0));
    Ok((vec!["group", "incidents", "successes", "outcome_known", "success_rate", "ci_low", "ci_high", "loss_n", "mean_loss", "median_loss", "p90_loss", "total_loss", "loss_share"], rows.into_iter().map(|r| r.1).collect()))
}

// ----------------------------------------------------------------- response

/// `killchain.response(label, groupKey, detectKey, respondKey)` ->
/// group, incidents, detect_n, mttd_mean, mttd_p50, mttd_p90, respond_n, mttr_mean, mttr_p50, mttr_p90.
/// Missing values are excluded from each statistic and counted by `detect_n` / `respond_n`.
fn response(view: &View, a: &[Value]) -> Result<Rows> {
    let label = need_str(a, 0, "label")?;
    let grp = need_str(a, 1, "groupKey")?;
    let det = need_str(a, 2, "detectKey")?;
    let rsp = need_str(a, 3, "respondKey")?;
    let ev = events(view, label)?;
    for (k, w) in [(grp, "groupKey"), (det, "detectKey"), (rsp, "respondKey")] {
        require_key(view, &ev, k, w)?;
    }
    let mut by: BTreeMap<String, (usize, Vec<f64>, Vec<f64>)> = BTreeMap::new();
    for b in &ev {
        let Some(g) = group_of(view, b, grp) else { continue };
        let e = by.entry(g).or_default();
        e.0 += 1;
        e.1.extend(num(view, b, det));
        e.2.extend(num(view, b, rsp));
    }
    let rows = by
        .into_iter()
        .map(|(g, (n, d, r))| {
            let (dm, dmed, d90, _) = describe(&d);
            let (rm, rmed, r90, _) = describe(&r);
            vec![Value::Str(g), Value::Int(n as i64), Value::Int(d.len() as i64), fo(dm), fo(dmed), fo(d90), Value::Int(r.len() as i64), fo(rm), fo(rmed), fo(r90)]
        })
        .collect();
    Ok((vec!["group", "incidents", "detect_n", "mttd_mean", "mttd_p50", "mttd_p90", "respond_n", "mttr_mean", "mttr_p50", "mttr_p90"], rows))
}

// ------------------------------------------------------------ defense depth

/// `killchain.defense_depth(label, [controlKeys], outcomeKey[, lossKey])` ->
/// controls_present, incidents, success_rate, ci_low, ci_high, mean_loss. Incidents missing any control or the
/// outcome are left out (the count is the difference between the sum of `incidents` and the event total).
fn defense_depth(view: &View, a: &[Value]) -> Result<Rows> {
    let label = need_str(a, 0, "label")?;
    let Some(ctls) = list_arg(a, 1).filter(|l| !l.is_empty()) else {
        return bad("killchain.defense_depth(label, [controlKeys], outcomeKey[, lossKey]): controlKeys must be a non-empty list of strings");
    };
    let out = need_str(a, 2, "outcomeKey")?;
    let loss = opt_str(a, 3);
    let ev = events(view, label)?;
    for k in &ctls {
        require_binary(view, &ev, k, "control")?;
    }
    require_binary(view, &ev, out, "outcomeKey")?;
    let mut by: BTreeMap<usize, Acc> = BTreeMap::new();
    'ev: for b in &ev {
        let mut k = 0usize;
        for c in &ctls {
            match flag(view, b, c) {
                Some(t) => k += t as usize,
                None => continue 'ev,
            }
        }
        let Some(o) = flag(view, b, out) else { continue };
        let acc = by.entry(k).or_default();
        acc.n += 1;
        acc.known += 1;
        acc.pos += o as usize;
        if let Some(lk) = loss {
            acc.loss.extend(num(view, b, lk));
        }
    }
    let rows = by
        .into_iter()
        .map(|(k, acc)| {
            let (lo, hi) = wilson(acc.pos as f64, acc.known as f64);
            let (m, _, _, _) = describe(&acc.loss);
            vec![Value::Int(k as i64), Value::Int(acc.n as i64), f(acc.rate()), f(lo), f(hi), fo(m)]
        })
        .collect();
    Ok((vec!["controls_present", "incidents", "success_rate", "ci_low", "ci_high", "mean_loss"], rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_tail_matches_reference_values() {
        // reference values from scipy.stats.norm.sf
        for (z, sf) in [(0.0, 0.5), (1.0, 0.15865525393145707), (1.96, 0.024997895148220435), (3.0, 0.0013498980316300933), (5.0, 2.866515718791933e-7), (10.0, 7.61985302416047e-24)] {
            let got = norm_sf(z);
            assert!(((got - sf) / sf).abs() < 1e-9, "sf({z}) = {got}, want {sf}");
        }
        assert!((p_two_sided(1.96) - 0.04999579029644087).abs() < 1e-12);
    }

    #[test]
    fn wilson_interval_reference() {
        // 30 of 100: scipy / statsmodels proportion_confint(30,100,method='wilson') = (0.21895, 0.39585)
        let (lo, hi) = wilson(30.0, 100.0);
        assert!((lo - 0.2189488529).abs() < 1e-9 && (hi - 0.3958485463).abs() < 1e-9, "{lo} {hi}");
        assert!(wilson(0.0, 10.0).0 < 1e-12 && wilson(10.0, 10.0).1 > 1.0 - 1e-12);
    }
}
