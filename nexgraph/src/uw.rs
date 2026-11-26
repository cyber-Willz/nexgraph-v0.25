//! Credit underwriting analytics (v0.13).
//!
//! Built from three public loan datasets (a 30,000 application approval set, the 9,578 loan LendingClub extract, a
//! 100,000 loan / 50,000 customer / default three-table set). Like `land.*` these procedures read records stored as
//! hyperedges (a hyperedge import with `keep_member_props`); a column is an edge property, a member node name, or
//! `Label.prop` for a property of a member node (a table joined with `import_csv_nodes_keyed`). Every procedure is
//! stateless: a model is refitted on each call (a 100,000 row fit with 20 features takes about a second).
//!
//! | procedure | question it answers |
//! |---|---|
//! | `uw.audit(label, target, features[, filter])` | which fields carry signal (AUC with DeLong CI, information value) and which are **leaks** (decided after the outcome) |
//! | `uw.scorecard(label, target, features, idKey[, opts])` | regularised logistic model: coefficients with Wald intervals, and out-of-sample AUC / Gini / KS / log-loss / Brier |
//! | `uw.gains(label, target, features, idKey[, opts])` | decile gains table: does the ranking separate good from bad, and where would a cut-off sit |
//! | `uw.scores(label, target, features, idKey[, opts])` | per-record probability of default with the three largest risk drivers (reason codes) |
//! | `uw.disparity(label, groupKey, favourable[, filter[, minN]])` | approval-rate parity across a protected or sensitive attribute (adverse impact ratio, chi-square) |
//! | `uw.group_gap(label, target, features, idKey, groupKey[, opts])` | does a group fare differently from what a model **without** that attribute predicts (the conditional follow-up to `uw.disparity`) |
//! | `uw.psi(label, key, baseCond, compareCond[, bins])` | population stability of a feature or score between two periods or books |
//! | `uw.affordability(label, incomeKey, debtKey, loanKey, rateKey, termKey[, opts])` | recomputed debt-to-income, the new payment, pro-forma and stressed ratios, and stored-versus-recomputed mismatches |
//!
//! ## Conventions
//!
//! * `target` is a condition for the **bad / event** class: `not.fully.paid=1`, `Default=1`, `Loan_Approved=Rejected`.
//!   Operators `= != > >= < <=` compare numbers numerically (dates are epoch seconds) and otherwise as text.
//!   Records where the target cannot be evaluated are excluded and counted, never treated as good.
//! * `features` is a comma-separated list. A field whose values are all numbers is numeric: median-imputed (with a
//!   missing-indicator column when any training value is missing) and standardised with the training mean and
//!   population standard deviation. Anything else is categorical: one-hot against the most frequent training level,
//!   levels with fewer than `min_level` training records pooled as `(other)`, a missing value is its own level.
//! * The train / test split is deterministic: FNV-1a 64 of the `idKey` value, modulo 100, below `test_pct` (default 30)
//!   is test. Or give `test=<condition>` for an out-of-time split. With neither an id nor a condition the record's
//!   position is hashed. The split never looks at the outcome.
//! * The model minimises `sum(log-loss) + l2/2 * sum(w^2)` over the non-intercept coefficients, solved by Newton's
//!   method (default `l2=1`; `l2=0` is the plain maximum likelihood fit, which diverges under perfect separation and
//!   says so with `converged = 0`). Standard errors are from the inverse penalised Hessian.
//! * `opts` is `key=value;key=value`: `l2`, `test_pct`, `min_level`, `test`, `filter`, `limit`, `set` (`test` or
//!   `train`, which rows `uw.gains` and `uw.scores` report), `max_iter`.
//! * Metrics are honest: the test AUC comes with a DeLong standard error and 95 % interval, so a model with no
//!   signal reports an interval that contains 0.5 rather than a number that looks like skill.
//! * A model, a disparity ratio or a leak flag is a screen for a human or a validation process, not a lending
//!   decision. The four-fifths rule in `uw.disparity` is a US employment-testing convention; whether it is the
//!   right standard for a given lender and jurisdiction is a legal question.

use crate::alm::select;
use crate::error::Result;
use crate::killchain::{f, flag, fo, group_of, lookup, opt_str as opt_str_raw, require_key, wilson};
use crate::land::erfc;
use crate::model::Body;
use crate::netgraph::*;
use crate::store::View;
use crate::value::Value;
use std::collections::{BTreeMap, HashMap, HashSet};

pub(crate) fn call(view: &View, name: &str, a: &[Value]) -> Result<Rows> {
    match name {
        "audit" => audit(view, a),
        "scorecard" => scorecard(view, a),
        "gains" => gains(view, a),
        "scores" => scores(view, a),
        "disparity" => disparity(view, a),
        "group_gap" => group_gap(view, a),
        "psi" => psi(view, a),
        "affordability" => affordability(view, a),
        _ => bad(format!("unknown procedure 'uw.{name}' (available: uw.audit, uw.scorecard, uw.gains, uw.scores, uw.disparity, uw.group_gap, uw.psi, uw.affordability)")),
    }
}

// ------------------------------------------------------------------ small helpers

/// An optional text argument: null, absent and the empty string all mean "not given".
fn opt_str<'a>(a: &'a [Value], i: usize) -> Option<&'a str> {
    opt_str_raw(a, i).map(str::trim).filter(|s| !s.is_empty())
}

fn keys_of(s: &str) -> Vec<&str> {
    s.split(',').map(str::trim).filter(|k| !k.is_empty()).collect()
}

/// Two-sided 95 % normal quantile (same constant as the Wilson intervals in `killchain`).
const Z95: f64 = 1.959963984540054;

fn parse_opts(s: Option<&str>, allowed: &[&str]) -> Result<HashMap<String, String>> {
    let mut m = HashMap::new();
    for piece in s.unwrap_or("").split(';').map(str::trim).filter(|p| !p.is_empty()) {
        let Some((k, v)) = piece.split_once('=') else {
            return bad(format!("option '{piece}' is not key=value"));
        };
        let k = k.trim();
        if !allowed.contains(&k) {
            return bad(format!("unknown option '{k}' (allowed: {})", allowed.join(", ")));
        }
        m.insert(k.to_string(), v.trim().to_string());
    }
    Ok(m)
}

fn opt_f64(o: &HashMap<String, String>, k: &str, default: f64) -> Result<f64> {
    match o.get(k) {
        None => Ok(default),
        Some(v) => v.parse::<f64>().ok().filter(|x| x.is_finite()).map_or_else(|| bad(format!("option {k}='{v}' is not a number")), Ok),
    }
}

fn fnum(v: &Value) -> Option<f64> {
    match v {
        Value::Int(_) | Value::Float(_) => v.as_f64().filter(|x| x.is_finite()),
        _ => None,
    }
}

fn text_of(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        Value::Int(i) => i.to_string(),
        Value::Float(x) => format!("{x}"),
        Value::Bool(b) => b.to_string(),
        other => format!("{other:?}"),
    }
}

/// Evaluate `Key op literal` (op one of `>= <= != > < =`) or a plain 0/1 key; `None` when the key is missing.
fn cond(view: &View, b: &Body, c: &str) -> Option<bool> {
    for op in [">=", "<=", "!=", ">", "<", "="] {
        if let Some((k, lit)) = c.split_once(op) {
            let (k, lit) = (k.trim(), lit.trim());
            if k.is_empty() || lit.is_empty() {
                continue;
            }
            let got = lookup(view, b, k)?;
            if let Some(x) = fnum(&got) {
                let rhs = lit.parse::<f64>().ok().or_else(|| crate::timeutil::parse_timestamp(lit));
                if let Some(r) = rhs {
                    return Some(match op {
                        ">=" => x >= r,
                        "<=" => x <= r,
                        ">" => x > r,
                        "<" => x < r,
                        "!=" => x != r,
                        _ => x == r,
                    });
                }
            }
            let t = text_of(&got);
            return Some(match op {
                "=" => t == lit,
                "!=" => t != lit,
                ">=" => t.as_str() >= lit,
                "<=" => t.as_str() <= lit,
                ">" => t.as_str() > lit,
                _ => t.as_str() < lit,
            });
        }
    }
    flag(view, b, c)
}

fn cond_key(c: &str) -> &str {
    for op in [">=", "<=", "!=", ">", "<", "="] {
        if let Some((k, _)) = c.split_once(op) {
            return k.trim();
        }
    }
    c
}

/// FNV-1a 64-bit.
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn midranks(x: &[f64]) -> Vec<f64> {
    let mut idx: Vec<usize> = (0..x.len()).collect();
    idx.sort_by(|&a, &b| x[a].total_cmp(&x[b]));
    let mut r = vec![0.0; x.len()];
    let mut i = 0;
    while i < idx.len() {
        let mut j = i;
        while j + 1 < idx.len() && x[idx[j + 1]] == x[idx[i]] {
            j += 1;
        }
        let mid = (i + j) as f64 / 2.0 + 1.0;
        for k in i..=j {
            r[idx[k]] = mid;
        }
        i = j + 1;
    }
    r
}

fn var1(x: &[f64]) -> f64 {
    if x.len() < 2 {
        return 0.0;
    }
    let m = x.iter().sum::<f64>() / x.len() as f64;
    x.iter().map(|v| (v - m) * (v - m)).sum::<f64>() / (x.len() - 1) as f64
}

/// AUC (probability a bad record outscores a good one, ties half) and its DeLong standard error.
fn auc_delong(score: &[f64], bad_: &[bool]) -> Option<(f64, f64)> {
    let pos: Vec<f64> = score.iter().zip(bad_).filter(|(_, b)| **b).map(|(s, _)| *s).collect();
    let neg: Vec<f64> = score.iter().zip(bad_).filter(|(_, b)| !**b).map(|(s, _)| *s).collect();
    let (m, n) = (pos.len(), neg.len());
    if m == 0 || n == 0 {
        return None;
    }
    let all: Vec<f64> = pos.iter().chain(neg.iter()).copied().collect();
    let (tx, ty, tz) = (midranks(&pos), midranks(&neg), midranks(&all));
    let auc = (tz[..m].iter().sum::<f64>() / m as f64 - (m as f64 + 1.0) / 2.0) / n as f64;
    let v01: Vec<f64> = (0..m).map(|i| (tz[i] - tx[i]) / n as f64).collect();
    let v10: Vec<f64> = (0..n).map(|j| 1.0 - (tz[m + j] - ty[j]) / m as f64).collect();
    Some((auc, (var1(&v01) / m as f64 + var1(&v10) / n as f64).sqrt()))
}

fn ks_stat(score: &[f64], bad_: &[bool]) -> Option<f64> {
    let (nb, ng) = (bad_.iter().filter(|b| **b).count() as f64, bad_.iter().filter(|b| !**b).count() as f64);
    if nb == 0.0 || ng == 0.0 {
        return None;
    }
    let mut idx: Vec<usize> = (0..score.len()).collect();
    idx.sort_by(|&a, &b| score[a].total_cmp(&score[b]));
    let (mut cb, mut cg, mut best) = (0.0, 0.0, 0.0f64);
    let mut i = 0;
    while i < idx.len() {
        let mut j = i;
        while j < idx.len() && score[idx[j]] == score[idx[i]] {
            if bad_[idx[j]] {
                cb += 1.0;
            } else {
                cg += 1.0;
            }
            j += 1;
        }
        best = best.max((cb / nb - cg / ng).abs());
        i = j;
    }
    Some(best)
}

/// Regularised upper incomplete gamma `Q(a, x)` (Numerical Recipes series / continued fraction).
pub(crate) fn gamma_q(a: f64, x: f64) -> f64 {
    if x <= 0.0 {
        return 1.0;
    }
    let lg = ln_gamma(a);
    if x < a + 1.0 {
        let (mut ap, mut del, mut sum) = (a, 1.0 / a, 1.0 / a);
        for _ in 0..1000 {
            ap += 1.0;
            del *= x / ap;
            sum += del;
            if del.abs() < sum.abs() * 1e-16 {
                break;
            }
        }
        1.0 - sum * (-x + a * x.ln() - lg).exp()
    } else {
        let tiny = 1e-300;
        let (mut b, mut c, mut d) = (x + 1.0 - a, 1.0 / tiny, 1.0 / (x + 1.0 - a));
        let mut h = d;
        for i in 1..1000 {
            let an = -(i as f64) * (i as f64 - a);
            b += 2.0;
            d = an * d + b;
            if d.abs() < tiny {
                d = tiny;
            }
            c = b + an / c;
            if c.abs() < tiny {
                c = tiny;
            }
            d = 1.0 / d;
            let del = d * c;
            h *= del;
            if (del - 1.0).abs() < 1e-16 {
                break;
            }
        }
        (-x + a * x.ln() - lg).exp() * h
    }
}

fn ln_gamma(x: f64) -> f64 {
    // Lanczos approximation (g = 7, n = 9), accurate to ~1e-15
    const C: [f64; 9] = [0.99999999999980993, 676.5203681218851, -1259.1392167224028, 771.32342877765313, -176.61502916214059, 12.507343278686905, -0.13857109526572012, 9.9843695780195716e-6, 1.5056327351493116e-7];
    if x < 0.5 {
        return (std::f64::consts::PI / (std::f64::consts::PI * x).sin()).ln() - ln_gamma(1.0 - x);
    }
    let x = x - 1.0;
    let mut a = C[0];
    let t = x + 7.5;
    for (i, c) in C.iter().enumerate().skip(1) {
        a += c / (x + i as f64);
    }
    0.5 * (2.0 * std::f64::consts::PI).ln() + (x + 0.5) * t.ln() - t + a.ln()
}

fn p_two_sided(z: f64) -> f64 {
    erfc(z.abs() / std::f64::consts::SQRT_2)
}

// ------------------------------------------------------------------ linear algebra (small, dense, SPD)

/// Cholesky factor `L` (row-major, lower) of a symmetric positive definite `n x n` matrix.
fn cholesky(a: &[f64], n: usize) -> Option<Vec<f64>> {
    let mut l = vec![0.0; n * n];
    for i in 0..n {
        for j in 0..=i {
            let mut s = a[i * n + j];
            for k in 0..j {
                s -= l[i * n + k] * l[j * n + k];
            }
            if i == j {
                if s <= 1e-12 {
                    return None;
                }
                l[i * n + i] = s.sqrt();
            } else {
                l[i * n + j] = s / l[j * n + j];
            }
        }
    }
    Some(l)
}

fn chol_solve(l: &[f64], n: usize, b: &[f64]) -> Vec<f64> {
    let mut y = vec![0.0; n];
    for i in 0..n {
        let mut s = b[i];
        for k in 0..i {
            s -= l[i * n + k] * y[k];
        }
        y[i] = s / l[i * n + i];
    }
    let mut x = vec![0.0; n];
    for i in (0..n).rev() {
        let mut s = y[i];
        for k in i + 1..n {
            s -= l[k * n + i] * x[k];
        }
        x[i] = s / l[i * n + i];
    }
    x
}

fn inv_diag(l: &[f64], n: usize) -> Vec<f64> {
    (0..n)
        .map(|j| {
            let mut e = vec![0.0; n];
            e[j] = 1.0;
            chol_solve(l, n, &e)[j]
        })
        .collect()
}

// ------------------------------------------------------------------ frame: the records a model sees

struct Frame {
    names: Vec<String>,
    cols: Vec<Vec<Option<Value>>>,
    y: Vec<bool>,
    id: Vec<String>,
    test: Vec<bool>,
    excluded: usize,
}

fn build_frame(view: &View, ev: &[&Body], target: &str, features: &[&str], id_key: Option<&str>, o: &HashMap<String, String>) -> Result<Frame> {
    require_key(view, ev, cond_key(target), "target")?;
    for k in features {
        require_key(view, ev, k, "feature")?;
    }
    if let Some(k) = id_key {
        require_key(view, ev, k, "id key")?;
    }
    let test_pct = opt_f64(o, "test_pct", 30.0)?;
    if !(0.0..=100.0).contains(&test_pct) {
        return bad("test_pct must be between 0 and 100");
    }
    let test_cond = o.get("test").map(String::as_str);
    if let Some(c) = test_cond {
        require_key(view, ev, cond_key(c), "test condition")?;
    }
    let mut fr = Frame { names: features.iter().map(|s| s.to_string()).collect(), cols: vec![Vec::new(); features.len()], y: vec![], id: vec![], test: vec![], excluded: 0 };
    for (pos, b) in ev.iter().enumerate() {
        let Some(y) = cond(view, b, target) else {
            fr.excluded += 1;
            continue;
        };
        let id = id_key.and_then(|k| group_of(view, b, k)).unwrap_or_else(|| pos.to_string());
        let is_test = match test_cond {
            Some(c) => cond(view, b, c).unwrap_or(false),
            None => ((fnv1a(&id) % 100) as f64) < test_pct,
        };
        for (j, k) in features.iter().enumerate() {
            fr.cols[j].push(lookup(view, b, k).filter(|v| !matches!(v, Value::Null)));
        }
        fr.y.push(y);
        fr.id.push(id);
        fr.test.push(is_test);
    }
    if fr.y.is_empty() {
        return bad(format!("target '{target}' could not be evaluated for any record"));
    }
    Ok(fr)
}

#[derive(Debug)]
enum Kind {
    Num { med: f64, mean: f64, sd: f64 },
    Miss,
    Level(String),
    Other(Vec<String>),
}

struct DCol {
    feat: usize,
    kind: Kind,
    name: String,
    level: String,
}

struct Design {
    cols: Vec<DCol>,
    mean: Vec<f64>,
    dropped: Vec<String>,
    reference: Vec<(usize, String)>,
}

impl Design {
    fn fit(fr: &Frame, train: &[usize], min_level: usize) -> Design {
        let mut cols = Vec::new();
        let mut dropped = Vec::new();
        let mut reference = Vec::new();
        for (j, name) in fr.names.iter().enumerate() {
            let vals: Vec<&Option<Value>> = train.iter().map(|&i| &fr.cols[j][i]).collect();
            let numeric = vals.iter().all(|v| v.as_ref().map_or(true, |v| fnum(v).is_some())) && vals.iter().any(|v| v.is_some());
            if numeric {
                let present: Vec<f64> = vals.iter().filter_map(|v| v.as_ref().and_then(fnum)).collect();
                let missing = vals.len() - present.len();
                let med = quantile(&sorted(&present), 0.5);
                let filled: Vec<f64> = vals.iter().map(|v| v.as_ref().and_then(fnum).unwrap_or(med)).collect();
                let mean = filled.iter().sum::<f64>() / filled.len() as f64;
                let sd = (filled.iter().map(|x| (x - mean) * (x - mean)).sum::<f64>() / filled.len() as f64).sqrt();
                if sd < 1e-12 {
                    dropped.push(format!("{name} (constant in the training set)"));
                    continue;
                }
                cols.push(DCol { feat: j, kind: Kind::Num { med, mean, sd }, name: name.clone(), level: String::new() });
                if missing > 0 {
                    cols.push(DCol { feat: j, kind: Kind::Miss, name: name.clone(), level: "(missing)".into() });
                }
            } else {
                let mut counts: BTreeMap<String, usize> = BTreeMap::new();
                for v in &vals {
                    *counts.entry(v.as_ref().map_or("(missing)".to_string(), text_of)).or_default() += 1;
                }
                let kept: Vec<(&String, &usize)> = counts.iter().filter(|(_, c)| **c >= min_level).collect();
                let Some(refl) = kept.iter().max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0))).map(|x| x.0.clone()) else {
                    dropped.push(format!("{name} (no level has {min_level} training records)"));
                    continue;
                };
                let rare: Vec<String> = counts.iter().filter(|(_, c)| **c < min_level).map(|(l, _)| l.clone()).collect();
                if kept.len() + usize::from(!rare.is_empty()) < 2 {
                    dropped.push(format!("{name} (a single level in the training set)"));
                    continue;
                }
                for (l, _) in &kept {
                    if **l != refl {
                        cols.push(DCol { feat: j, kind: Kind::Level((*l).clone()), name: name.clone(), level: (*l).clone() });
                    }
                }
                if !rare.is_empty() {
                    cols.push(DCol { feat: j, kind: Kind::Other(kept.iter().map(|x| x.0.clone()).collect()), name: name.clone(), level: "(other)".into() });
                }
                reference.push((j, refl));
            }
        }
        let mut d = Design { cols, mean: vec![], dropped, reference };
        let mut sums = vec![0.0; d.cols.len()];
        let mut row = vec![0.0; d.cols.len()];
        for &i in train {
            d.encode(fr, i, &mut row);
            for (s, r) in sums.iter_mut().zip(&row) {
                *s += r;
            }
        }
        d.mean = sums.iter().map(|s| s / train.len().max(1) as f64).collect();
        d
    }

    fn encode(&self, fr: &Frame, i: usize, out: &mut [f64]) {
        for (k, c) in self.cols.iter().enumerate() {
            let v = &fr.cols[c.feat][i];
            out[k] = match &c.kind {
                Kind::Num { med, mean, sd } => (v.as_ref().and_then(fnum).unwrap_or(*med) - mean) / sd,
                Kind::Miss => f64::from(v.as_ref().and_then(fnum).is_none()),
                Kind::Level(l) => f64::from(v.as_ref().map_or("(missing)".to_string(), text_of) == *l),
                Kind::Other(kept) => f64::from(!kept.contains(&v.as_ref().map_or("(missing)".to_string(), text_of))),
            };
        }
    }
}

struct Model {
    design: Design,
    beta: Vec<f64>, // [intercept, design columns...]
    se: Vec<f64>,
    converged: bool,
    iters: usize,
    l2: f64,
}

fn sigmoid(z: f64) -> f64 {
    if z >= 0.0 {
        1.0 / (1.0 + (-z).exp())
    } else {
        let e = z.exp();
        e / (1.0 + e)
    }
}

fn fit_model(fr: &Frame, train: &[usize], l2: f64, min_level: usize, max_iter: usize) -> Result<Model> {
    let design = Design::fit(fr, train, min_level);
    let p = design.cols.len() + 1;
    let n = train.len();
    let bads = train.iter().filter(|&&i| fr.y[i]).count();
    if bads == 0 || bads == n {
        return bad(format!("the training set has {bads} bad records out of {n}: a model needs both classes (check the target, filter and test split)"));
    }
    let mut x = vec![0.0; n * p];
    for (r, &i) in train.iter().enumerate() {
        x[r * p] = 1.0;
        design.encode(fr, i, &mut x[r * p + 1..(r + 1) * p]);
    }
    let y: Vec<f64> = train.iter().map(|&i| f64::from(fr.y[i])).collect();
    let mut beta = vec![0.0; p];
    let base = bads as f64 / n as f64;
    beta[0] = (base / (1.0 - base)).ln();
    let (mut converged, mut iters) = (false, 0);
    let mut h = vec![0.0; p * p];
    for it in 1..=max_iter {
        iters = it;
        h.iter_mut().for_each(|v| *v = 0.0);
        let mut g = vec![0.0; p];
        for r in 0..n {
            let row = &x[r * p..(r + 1) * p];
            let z: f64 = row.iter().zip(&beta).map(|(a, b)| a * b).sum();
            let pr = sigmoid(z);
            let w = (pr * (1.0 - pr)).max(1e-12);
            let e = pr - y[r];
            for a in 0..p {
                g[a] += e * row[a];
                let wa = w * row[a];
                for b in 0..=a {
                    h[a * p + b] += wa * row[b];
                }
            }
        }
        for a in 0..p {
            for b in 0..a {
                h[b * p + a] = h[a * p + b];
            }
            if a > 0 {
                h[a * p + a] += l2;
                g[a] += l2 * beta[a];
            }
        }
        let l = cholesky(&h, p).ok_or_else(|| crate::error::Error::Invalid("the design matrix is singular: two features are exact copies or combinations of each other (drop one) or use l2 > 0".into()))?;
        let step = chol_solve(&l, p, &g);
        let mut max_step = 0.0f64;
        for a in 0..p {
            beta[a] -= step[a];
            max_step = max_step.max(step[a].abs());
        }
        if max_step < 1e-10 {
            converged = true;
            break;
        }
        if beta.iter().any(|b| !b.is_finite() || b.abs() > 1e3) {
            break; // perfect separation with l2 = 0: the coefficients run away
        }
    }
    // standard errors: inverse penalised Hessian at the solution
    h.iter_mut().for_each(|v| *v = 0.0);
    for r in 0..n {
        let row = &x[r * p..(r + 1) * p];
        let pr = sigmoid(row.iter().zip(&beta).map(|(a, b)| a * b).sum());
        let w = (pr * (1.0 - pr)).max(1e-12);
        for a in 0..p {
            for b in 0..=a {
                h[a * p + b] += w * row[a] * row[b];
            }
        }
    }
    for a in 0..p {
        for b in 0..a {
            h[b * p + a] = h[a * p + b];
        }
        if a > 0 {
            h[a * p + a] += l2;
        }
    }
    let se = match cholesky(&h, p) {
        Some(l) => inv_diag(&l, p).iter().map(|v| v.max(0.0).sqrt()).collect(),
        None => vec![f64::NAN; p],
    };
    Ok(Model { design, beta, se, converged, iters, l2 })
}

impl Model {
    fn predict(&self, fr: &Frame, i: usize, buf: &mut [f64]) -> f64 {
        self.design.encode(fr, i, buf);
        sigmoid(self.beta[0] + buf.iter().zip(&self.beta[1..]).map(|(a, b)| a * b).sum::<f64>())
    }
}

struct Fitted {
    fr: Frame,
    train: Vec<usize>,
    test: Vec<usize>,
    model: Model,
    pd: Vec<f64>,
}

fn fit_from_args(view: &View, a: &[Value], allowed: &[&str], opts_at: usize) -> Result<Fitted> {
    let label = need_str(a, 0, "label")?;
    let target = need_str(a, 1, "target")?;
    let features = keys_of(need_str(a, 2, "features")?);
    if features.is_empty() {
        return bad("features: name at least one column");
    }
    let id_key = opt_str(a, 3);
    let o = parse_opts(opt_str(a, opts_at), allowed)?;
    let ev = select(view, label, o.get("filter").map(String::as_str))?;
    let fr = build_frame(view, &ev, target, &features, id_key, &o)?;
    let l2 = opt_f64(&o, "l2", 1.0)?;
    if l2 < 0.0 {
        return bad("l2 must be 0 or more");
    }
    let min_level = opt_f64(&o, "min_level", 30.0)?.max(1.0) as usize;
    let max_iter = opt_f64(&o, "max_iter", 50.0)?.max(1.0) as usize;
    let train: Vec<usize> = (0..fr.y.len()).filter(|&i| !fr.test[i]).collect();
    let test: Vec<usize> = (0..fr.y.len()).filter(|&i| fr.test[i]).collect();
    let model = fit_model(&fr, &train, l2, min_level, max_iter)?;
    let mut buf = vec![0.0; model.design.cols.len()];
    let pd: Vec<f64> = (0..fr.y.len()).map(|i| model.predict(&fr, i, &mut buf)).collect();
    Ok(Fitted { fr, train, test, model, pd })
}

const MODEL_OPTS: [&str; 7] = ["l2", "test_pct", "min_level", "test", "filter", "limit", "max_iter"];
const MODEL_OPTS_SET: [&str; 8] = ["l2", "test_pct", "min_level", "test", "filter", "limit", "max_iter", "set"];

// ------------------------------------------------------------------ uw.scorecard

fn metrics(fr: &Frame, pd: &[f64], idx: &[usize], set: &str, rows: &mut Vec<Vec<Value>>) {
    if idx.is_empty() {
        return;
    }
    let p: Vec<f64> = idx.iter().map(|&i| pd[i]).collect();
    let y: Vec<bool> = idx.iter().map(|&i| fr.y[i]).collect();
    let n = idx.len() as f64;
    let mut m = |name: &str, v: f64, se: Option<f64>, lo: Option<f64>, hi: Option<f64>| {
        rows.push(vec![Value::Str(set.into()), Value::Str(name.into()), Value::Str(String::new()), f(v), fo(se), fo(lo), fo(hi), Value::Null, Value::Null]);
    };
    m("records", n, None, None, None);
    m("bads", y.iter().filter(|b| **b).count() as f64, None, None, None);
    m("base_rate", y.iter().filter(|b| **b).count() as f64 / n, None, None, None);
    m("mean_pd", p.iter().sum::<f64>() / n, None, None, None);
    if let Some((auc, se)) = auc_delong(&p, &y) {
        m("auc", auc, Some(se), Some((auc - Z95 * se).max(0.0)), Some((auc + Z95 * se).min(1.0)));
        m("gini", 2.0 * auc - 1.0, Some(2.0 * se), Some(2.0 * (auc - Z95 * se).max(0.0) - 1.0), Some(2.0 * (auc + Z95 * se).min(1.0) - 1.0));
    }
    if let Some(ks) = ks_stat(&p, &y) {
        m("ks", ks, None, None, None);
    }
    let ll = -p.iter().zip(&y).map(|(pr, b)| if *b { pr.clamp(1e-15, 1.0).ln() } else { (1.0 - pr).clamp(1e-15, 1.0).ln() }).sum::<f64>() / n;
    m("logloss", ll, None, None, None);
    m("brier", p.iter().zip(&y).map(|(pr, b)| (pr - f64::from(*b)).powi(2)).sum::<f64>() / n, None, None, None);
}

/// `uw.scorecard(label, target, features, idKey[, opts])` -> section, name, level, value, std_err, ci_low, ci_high, p_value, odds_ratio.
/// Sections: `meta` (counts, convergence), `coef` (one row per design column, on the standardised scale for numeric
/// fields), `train` and `test` (metrics; the test rows are the out-of-sample evidence).
fn scorecard(view: &View, a: &[Value]) -> Result<Rows> {
    let fit = fit_from_args(view, a, &MODEL_OPTS, 4)?;
    let (fr, m) = (&fit.fr, &fit.model);
    let mut rows: Vec<Vec<Value>> = Vec::new();
    let mut meta = |name: &str, v: f64, level: &str| rows.push(vec![Value::Str("meta".into()), Value::Str(name.into()), Value::Str(level.into()), f(v), Value::Null, Value::Null, Value::Null, Value::Null, Value::Null]);
    meta("records", fr.y.len() as f64, "");
    meta("excluded_no_target", fr.excluded as f64, "");
    meta("train_records", fit.train.len() as f64, "");
    meta("test_records", fit.test.len() as f64, "");
    meta("converged", f64::from(u8::from(m.converged)), "");
    meta("iterations", m.iters as f64, "");
    meta("l2", m.l2, "");
    for d in &m.design.dropped {
        meta("dropped_feature", f64::NAN, d);
    }
    for (j, l) in &m.design.reference {
        meta("reference_level", f64::NAN, &format!("{}={l}", fr.names[*j]));
    }
    let mut coef = |name: &str, level: &str, b: f64, se: f64| {
        let z = b / se;
        rows.push(vec![Value::Str("coef".into()), Value::Str(name.into()), Value::Str(level.into()), f(b), f(se), f(b - Z95 * se), f(b + Z95 * se), f(p_two_sided(z)), f(b.exp())]);
    };
    coef("(intercept)", "", m.beta[0], m.se[0]);
    for (k, c) in m.design.cols.iter().enumerate() {
        coef(&c.name, &c.level, m.beta[k + 1], m.se[k + 1]);
    }
    metrics(fr, &fit.pd, &fit.train, "train", &mut rows);
    metrics(fr, &fit.pd, &fit.test, "test", &mut rows);
    // Near-perfect separation is almost never skill: a column carries the outcome (a score computed after the decision,
    // a flag derived from the label). Say so in the output instead of leaving a 33-billion odds ratio to speak for itself.
    let big = m.beta.iter().skip(1).any(|b| b.abs() > 8.0);
    let perfect = rows.iter().any(|r| matches!((&r[0], &r[1], &r[3]), (Value::Str(a), Value::Str(b), Value::Float(v)) if a == "test" && b == "auc" && *v >= 0.995));
    if big || perfect {
        rows.insert(
            0,
            vec![Value::Str("warning".into()), Value::Str("separation".into()), Value::Str("near-perfect separation (test AUC >= 0.995 or a standardised coefficient beyond 8): a feature probably encodes the outcome; run uw.audit and remove it before trusting this model".into()), Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Null],
        );
    }
    Ok((vec!["section", "name", "level", "value", "std_err", "ci_low", "ci_high", "p_value", "odds_ratio"], rows))
}

/// `uw.gains(label, target, features, idKey[, opts])` -> decile, records, bads, bad_rate, wilson_low, wilson_high,
/// avg_pd, cum_bad_share, lift, ks_gap. Decile 1 holds the highest predicted risk. Reports the test set unless
/// `set=train`.
fn gains(view: &View, a: &[Value]) -> Result<Rows> {
    let fit = fit_from_args(view, a, &MODEL_OPTS_SET, 4)?;
    let o = parse_opts(opt_str(a, 4), &MODEL_OPTS_SET)?;
    let idx: &Vec<usize> = match o.get("set").map(String::as_str) {
        None | Some("test") => &fit.test,
        Some("train") => &fit.train,
        Some(s) => return bad(format!("set must be test or train, not '{s}'")),
    };
    if idx.is_empty() {
        return bad("the selected set is empty: lower test_pct or use set=train");
    }
    let mut order: Vec<usize> = idx.clone();
    order.sort_by(|&x, &y| fit.pd[y].total_cmp(&fit.pd[x]).then_with(|| x.cmp(&y)));
    let n = order.len();
    let total_bad = order.iter().filter(|&&i| fit.fr.y[i]).count() as f64;
    let total_good = n as f64 - total_bad;
    let base = total_bad / n as f64;
    let (mut cum_bad, mut cum_good) = (0.0, 0.0);
    let mut rows = Vec::new();
    for d in 0..10 {
        let (lo, hi) = (d * n / 10, (d + 1) * n / 10);
        if hi == lo {
            continue;
        }
        let part = &order[lo..hi];
        let bads = part.iter().filter(|&&i| fit.fr.y[i]).count() as f64;
        let k = part.len() as f64;
        cum_bad += bads;
        cum_good += k - bads;
        let (wl, wh) = wilson(bads, k);
        rows.push(vec![
            Value::Int(d as i64 + 1),
            Value::Int(part.len() as i64),
            Value::Int(bads as i64),
            f(bads / k),
            f(wl),
            f(wh),
            f(part.iter().map(|&i| fit.pd[i]).sum::<f64>() / k),
            f(cum_bad / total_bad.max(1.0)),
            f(bads / k / base),
            f((cum_bad / total_bad.max(1.0) - cum_good / total_good.max(1.0)).abs()),
        ]);
    }
    Ok((vec!["decile", "records", "bads", "bad_rate", "wilson_low", "wilson_high", "avg_pd", "cum_bad_share", "lift", "ks_gap"], rows))
}

/// `uw.scores(label, target, features, idKey[, opts])` -> id, set, pd, outcome, reason_1, reason_2, reason_3.
/// Reasons name the features pushing this record's risk up most, relative to the training average:
/// `feature (value)`. Highest predicted risk first; `limit` (default 1000) rows; `set` as for `uw.gains`.
fn scores(view: &View, a: &[Value]) -> Result<Rows> {
    let fit = fit_from_args(view, a, &MODEL_OPTS_SET, 4)?;
    let o = parse_opts(opt_str(a, 4), &MODEL_OPTS_SET)?;
    let limit = opt_f64(&o, "limit", 1000.0)?.max(1.0) as usize;
    let (idx, set): (&Vec<usize>, &str) = match o.get("set").map(String::as_str) {
        None | Some("test") => (&fit.test, "test"),
        Some("train") => (&fit.train, "train"),
        Some(s) => return bad(format!("set must be test or train, not '{s}'")),
    };
    let mut order: Vec<usize> = idx.clone();
    order.sort_by(|&x, &y| fit.pd[y].total_cmp(&fit.pd[x]).then_with(|| x.cmp(&y)));
    let m = &fit.model;
    let mut buf = vec![0.0; m.design.cols.len()];
    let rows = order
        .into_iter()
        .take(limit)
        .map(|i| {
            m.design.encode(&fit.fr, i, &mut buf);
            let mut by_feat: BTreeMap<usize, f64> = BTreeMap::new();
            for (k, c) in m.design.cols.iter().enumerate() {
                *by_feat.entry(c.feat).or_default() += m.beta[k + 1] * (buf[k] - m.design.mean[k]);
            }
            let mut drivers: Vec<(usize, f64)> = by_feat.into_iter().filter(|(_, v)| *v > 0.0).collect();
            drivers.sort_by(|x, y| y.1.total_cmp(&x.1).then_with(|| x.0.cmp(&y.0)));
            let reason = |r: usize| match drivers.get(r) {
                Some((j, _)) => Value::Str(format!("{} ({})", fit.fr.names[*j], fit.fr.cols[*j][i].as_ref().map_or("missing".to_string(), text_of))),
                None => Value::Null,
            };
            vec![Value::Str(fit.fr.id[i].clone()), Value::Str(set.into()), f(fit.pd[i]), Value::Int(i64::from(fit.fr.y[i])), reason(0), reason(1), reason(2)]
        })
        .collect();
    Ok((vec!["id", "set", "pd", "outcome", "reason_1", "reason_2", "reason_3"], rows))
}

// ------------------------------------------------------------------ uw.audit

/// Information value of a field: numeric fields in up to ten equal-frequency bins (cut points are the linearly
/// interpolated deciles, bins are right-inclusive, duplicates collapsed), categorical fields by level, missing as
/// its own bin. `WoE = ln(share of goods / share of bads)` with 0.5 added to each cell.
fn information_value(vals: &[Option<Value>], y: &[bool]) -> f64 {
    let numeric = vals.iter().all(|v| v.as_ref().map_or(true, |v| fnum(v).is_some())) && vals.iter().any(|v| v.is_some());
    let mut cells: BTreeMap<String, (f64, f64)> = BTreeMap::new();
    if numeric {
        let present = sorted(&vals.iter().filter_map(|v| v.as_ref().and_then(fnum)).collect::<Vec<_>>());
        let mut cuts: Vec<f64> = (1..10).map(|k| quantile(&present, k as f64 / 10.0)).collect();
        cuts.dedup();
        for (v, b) in vals.iter().zip(y) {
            let key = match v.as_ref().and_then(fnum) {
                None => "~missing".to_string(),
                Some(x) => format!("{:03}", cuts.partition_point(|c| *c < x)),
            };
            let e = cells.entry(key).or_default();
            if *b {
                e.1 += 1.0;
            } else {
                e.0 += 1.0;
            }
        }
    } else {
        for (v, b) in vals.iter().zip(y) {
            let e = cells.entry(v.as_ref().map_or("(missing)".to_string(), text_of)).or_default();
            if *b {
                e.1 += 1.0;
            } else {
                e.0 += 1.0;
            }
        }
    }
    let k = cells.len() as f64;
    let (g, b): (f64, f64) = cells.values().fold((0.0, 0.0), |a, c| (a.0 + c.0, a.1 + c.1));
    cells
        .values()
        .map(|(good, bd)| {
            let (dg, db) = ((good + 0.5) / (g + 0.5 * k), (bd + 0.5) / (b + 0.5 * k));
            (dg - db) * (dg / db).ln()
        })
        .sum()
}

/// `uw.audit(label, target, features[, filter])` -> feature, kind, records, missing_rate, distinct, auc, auc_low,
/// auc_high, gini_abs, iv, strength, flags. `auc` is for numeric fields only (the probability a bad record has the
/// higher value; 0.5 is no signal) with a DeLong interval; `gini_abs = |2 auc - 1|`. Flags: `leak?` (AUC or
/// 1 - AUC of at least 0.97, or IV of at least 2: the field almost certainly records the outcome or a decision
/// made after it), `suspicious` (IV of at least 0.5: strong enough that you should confirm the field is known at
/// decision time, e.g. a price set after underwriting), `constant`, `high_missing` (over 30 %), `id_like` (categorical
/// with more than half the records distinct). `strength` is the usual IV scale: under 0.02 useless, under 0.1
/// weak, under 0.3 medium, under 0.5 strong, otherwise suspicious.
fn audit(view: &View, a: &[Value]) -> Result<Rows> {
    let label = need_str(a, 0, "label")?;
    let target = need_str(a, 1, "target")?;
    let features = keys_of(need_str(a, 2, "features")?);
    if features.is_empty() {
        return bad("features: name at least one column");
    }
    let ev = select(view, label, opt_str(a, 3))?;
    let fr = build_frame(view, &ev, target, &features, None, &HashMap::new())?;
    let mut rows = Vec::new();
    for (j, name) in features.iter().enumerate() {
        let col = &fr.cols[j];
        let n = col.len() as f64;
        let present = col.iter().filter(|v| v.is_some()).count();
        let numeric = present > 0 && col.iter().all(|v| v.as_ref().map_or(true, |v| fnum(v).is_some()));
        let distinct = col.iter().flatten().map(text_of).collect::<HashSet<_>>().len();
        let (mut auc, mut lo, mut hi, mut gini) = (None, None, None, None);
        if numeric {
            let (s, y): (Vec<f64>, Vec<bool>) = col.iter().zip(&fr.y).filter_map(|(v, y)| v.as_ref().and_then(fnum).map(|x| (x, *y))).unzip();
            if let Some((au, se)) = auc_delong(&s, &y) {
                auc = Some(au);
                lo = Some((au - Z95 * se).max(0.0));
                hi = Some((au + Z95 * se).min(1.0));
                gini = Some((2.0 * au - 1.0).abs());
            }
        }
        let iv = if distinct > 1 || present < col.len() { Some(information_value(col, &fr.y)) } else { Some(0.0) };
        let ivv = iv.unwrap_or(0.0);
        let strength = match ivv {
            x if x < 0.02 => "useless",
            x if x < 0.1 => "weak",
            x if x < 0.3 => "medium",
            x if x < 0.5 => "strong",
            _ => "suspicious",
        };
        let mut flags: Vec<&str> = Vec::new();
        if auc.map_or(false, |x| x >= 0.97 || x <= 0.03) || ivv >= 2.0 {
            flags.push("leak?");
        } else if ivv >= 0.5 {
            flags.push("suspicious");
        }
        if distinct <= 1 && present == col.len() {
            flags.push("constant");
        }
        if (n - present as f64) / n > 0.3 {
            flags.push("high_missing");
        }
        if !numeric && distinct as f64 > n / 2.0 {
            flags.push("id_like");
        }
        rows.push(vec![
            Value::Str(name.to_string()),
            Value::Str(if numeric { "numeric" } else { "categorical" }.into()),
            Value::Int(col.len() as i64),
            f((n - present as f64) / n),
            Value::Int(distinct as i64),
            fo(auc),
            fo(lo),
            fo(hi),
            fo(gini),
            fo(iv),
            Value::Str(strength.into()),
            Value::Str(flags.join(", ")),
        ]);
    }
    rows.sort_by(|x, y| {
        let g = |r: &Vec<Value>| r[9].as_f64().unwrap_or(0.0);
        g(y).total_cmp(&g(x))
    });
    Ok((vec!["feature", "kind", "records", "missing_rate", "distinct", "auc", "auc_low", "auc_high", "gini_abs", "iv", "strength", "flags"], rows))
}

// ------------------------------------------------------------------ uw.disparity

/// `uw.disparity(label, groupKey, favourable[, filter[, minN]])` -> group, records, favourable, rate, ref_group,
/// air, wilson_low, wilson_high, z_vs_ref, p_vs_ref, flag, chi2, df, chi2_p.
/// `favourable` is the good outcome (`Loan_Approved=Approved`). The reference group is the one with the highest
/// rate among groups of at least `minN` (default 30) records. `air` (adverse impact ratio) is the group's rate over
/// the reference rate; `flag` is `adverse_impact` when `air < 0.8` and the two-proportion z-test against the
/// reference gives p < 0.05, `below_minN` for a group too small to judge. `chi2` tests independence of the outcome and
/// the group over all groups of at least `minN`. This is a screen on **raw** rates: use `filter` to compare within a
/// stratum (a score band, a product) before concluding anything.
fn disparity(view: &View, a: &[Value]) -> Result<Rows> {
    let label = need_str(a, 0, "label")?;
    let gkey = need_str(a, 1, "groupKey")?;
    let fav = need_str(a, 2, "favourable")?;
    let min_n = usize_arg(a, 4, 30, "minN")?.max(1) as f64;
    let ev = select(view, label, opt_str(a, 3))?;
    require_key(view, &ev, gkey, "group key")?;
    require_key(view, &ev, cond_key(fav), "favourable outcome")?;
    let mut groups: BTreeMap<String, (f64, f64)> = BTreeMap::new();
    for b in &ev {
        let (Some(g), Some(y)) = (group_of(view, b, gkey), cond(view, b, fav)) else { continue };
        let e = groups.entry(g).or_default();
        e.0 += 1.0;
        e.1 += f64::from(y);
    }
    if groups.is_empty() {
        return bad("no record has both the group key and the outcome");
    }
    let big: Vec<(&String, &(f64, f64))> = groups.iter().filter(|(_, v)| v.0 >= min_n).collect();
    let Some(refg) = big.iter().max_by(|x, y| (x.1 .1 / x.1 .0).total_cmp(&(y.1 .1 / y.1 .0)).then_with(|| y.0.cmp(x.0))).map(|x| (x.0.clone(), *x.1)) else {
        return bad(format!("no group has {min_n} records: lower minN"));
    };
    let (tn, tf): (f64, f64) = big.iter().fold((0.0, 0.0), |a, g| (a.0 + g.1 .0, a.1 + g.1 .1));
    let p0 = tf / tn;
    let (chi2, df) = if big.len() > 1 && p0 > 0.0 && p0 < 1.0 {
        (big.iter().map(|(_, (n, k))| (k - n * p0).powi(2) / (n * p0 * (1.0 - p0))).sum::<f64>(), (big.len() - 1) as f64)
    } else {
        (f64::NAN, 0.0)
    };
    let chi2_p = if chi2.is_finite() { gamma_q(df / 2.0, chi2 / 2.0) } else { f64::NAN };
    let refrate = refg.1 .1 / refg.1 .0;
    let mut rows = Vec::new();
    for (g, (n, k)) in &groups {
        let rate = k / n;
        let (wl, wh) = wilson(*k, *n);
        let air = if refrate > 0.0 { rate / refrate } else { f64::NAN };
        let (z, p) = if *g == refg.0 {
            (f64::NAN, f64::NAN)
        } else {
            let pooled = (k + refg.1 .1) / (n + refg.1 .0);
            let se = (pooled * (1.0 - pooled) * (1.0 / n + 1.0 / refg.1 .0)).sqrt();
            if se > 0.0 {
                let z = (rate - refrate) / se;
                (z, p_two_sided(z))
            } else {
                (f64::NAN, f64::NAN)
            }
        };
        let flag = if *n < min_n {
            "below_minN"
        } else if air < 0.8 && p < 0.05 {
            "adverse_impact"
        } else {
            ""
        };
        rows.push(vec![Value::Str(g.clone()), f(*n), f(*k), f(rate), Value::Str(refg.0.clone()), f(air), f(wl), f(wh), f(z), f(p), Value::Str(flag.into()), f(chi2), f(df), f(chi2_p)]);
    }
    rows.sort_by(|x, y| y[3].as_f64().unwrap_or(0.0).total_cmp(&x[3].as_f64().unwrap_or(0.0)));
    for r in &mut rows {
        for c in [1usize, 2] {
            if let Some(v) = r[c].as_f64() {
                r[c] = Value::Int(v as i64);
            }
        }
    }
    Ok((vec!["group", "records", "favourable", "rate", "ref_group", "air", "wilson_low", "wilson_high", "z_vs_ref", "p_vs_ref", "flag", "chi2", "df", "chi2_p"], rows))
}

// ------------------------------------------------------------------ uw.group_gap

/// `uw.group_gap(label, target, features, idKey, groupKey[, opts])` -> group, records, bads, observed_rate,
/// expected_rate, gap, z, p_value, flag. A model is fitted on `features` (which must **not** contain `groupKey`),
/// then for each group the observed bad count is compared with the sum of the model's predicted probabilities:
/// `z = (observed - expected) / sqrt(sum p (1 - p))`. A positive gap means the group does worse than the legitimate
/// factors explain, a negative one better. `flag` is `gap` when `p < 0.05 / groups` (Bonferroni). Reports the test
/// set; `set=all` or `set=train` change that (in-sample rows are optimistic). The raw difference in rates that
/// `uw.disparity` reports can be entirely explained by the other columns; this says whether it is. It is still
/// a screen: a gap is evidence to investigate, not a finding of discrimination, and an absent gap does not clear a
/// feature that is itself a proxy for the group.
fn group_gap(view: &View, a: &[Value]) -> Result<Rows> {
    let gkey = need_str(a, 4, "groupKey")?;
    let features = keys_of(need_str(a, 2, "features")?);
    if features.contains(&gkey) {
        return bad(format!("groupKey '{gkey}' is also in features: remove it, the point is to model without it"));
    }
    let fit = fit_from_args(view, a, &MODEL_OPTS_SET, 5)?;
    let o = parse_opts(opt_str(a, 5), &MODEL_OPTS_SET)?;
    let idx: Vec<usize> = match o.get("set").map(String::as_str) {
        None | Some("test") => fit.test.clone(),
        Some("train") => fit.train.clone(),
        Some("all") => (0..fit.fr.y.len()).collect(),
        Some(s) => return bad(format!("set must be test, train or all, not '{s}'")),
    };
    if idx.is_empty() {
        return bad("the selected set is empty: lower test_pct or use set=all");
    }
    // the group of each record in the frame: re-read it from the records in the same order the frame was built
    let label = need_str(a, 0, "label")?;
    let target = need_str(a, 1, "target")?;
    let ev = select(view, label, o.get("filter").map(String::as_str))?;
    require_key(view, &ev, gkey, "group key")?;
    let groups: Vec<String> = ev.iter().filter(|b| cond(view, b, target).is_some()).map(|b| group_of(view, b, gkey).unwrap_or_else(|| "(missing)".into())).collect();
    let mut acc: BTreeMap<String, (f64, f64, f64, f64)> = BTreeMap::new(); // n, bads, expected, variance
    for &i in &idx {
        let e = acc.entry(groups[i].clone()).or_default();
        let p = fit.pd[i];
        e.0 += 1.0;
        e.1 += f64::from(fit.fr.y[i]);
        e.2 += p;
        e.3 += p * (1.0 - p);
    }
    let k = acc.len() as f64;
    let rows = acc
        .into_iter()
        .map(|(g, (n, bads, exp, var))| {
            let z = if var > 0.0 { (bads - exp) / var.sqrt() } else { f64::NAN };
            let p = if z.is_finite() { p_two_sided(z) } else { f64::NAN };
            vec![Value::Str(g), Value::Int(n as i64), Value::Int(bads as i64), f(bads / n), f(exp / n), f((bads - exp) / n), f(z), f(p), Value::Str(if p.is_finite() && p < 0.05 / k { "gap" } else { "" }.into())]
        })
        .collect();
    Ok((vec!["group", "records", "bads", "observed_rate", "expected_rate", "gap", "z", "p_value", "flag"], rows))
}

// ------------------------------------------------------------------ uw.psi

/// `uw.psi(label, key, baseCond, compareCond[, bins])` -> bin, base_records, compare_records, base_share,
/// compare_share, psi_component, plus a final `(total)` row. Numeric fields are cut at the base population's
/// quantiles (`bins`, default 10, duplicates collapsed, right-inclusive); anything else by level. Shares are
/// floored at 1e-4 so an empty bin does not give an infinite contribution. Rule of thumb: under 0.10 stable, 0.10 to
/// 0.25 some shift, over 0.25 a major shift.
fn psi(view: &View, a: &[Value]) -> Result<Rows> {
    let label = need_str(a, 0, "label")?;
    let key = need_str(a, 1, "key")?;
    let (base_c, comp_c) = (need_str(a, 2, "baseCond")?, need_str(a, 3, "compareCond")?);
    let bins = usize_arg(a, 4, 10, "bins")?.clamp(2, 50);
    let ev = select(view, label, None)?;
    require_key(view, &ev, key, "key")?;
    require_key(view, &ev, cond_key(base_c), "baseCond")?;
    require_key(view, &ev, cond_key(comp_c), "compareCond")?;
    let (mut base, mut comp): (Vec<Option<Value>>, Vec<Option<Value>>) = (vec![], vec![]);
    for b in &ev {
        let v = lookup(view, b, key).filter(|v| !matches!(v, Value::Null));
        match (cond(view, b, base_c), cond(view, b, comp_c)) {
            (Some(true), Some(true)) => return bad("a record is in both the base and the compare population: the two conditions must not overlap"),
            (Some(true), _) => base.push(v),
            (_, Some(true)) => comp.push(v),
            _ => {}
        }
    }
    if base.is_empty() || comp.is_empty() {
        return bad(format!("the base has {} records and the compare population {}: both must be non-empty", base.len(), comp.len()));
    }
    let numeric = base.iter().chain(comp.iter()).all(|v| v.as_ref().map_or(true, |v| fnum(v).is_some())) && base.iter().chain(comp.iter()).any(|v| v.is_some());
    let mut cells: BTreeMap<String, (f64, f64)> = BTreeMap::new();
    if numeric {
        let present = sorted(&base.iter().filter_map(|v| v.as_ref().and_then(fnum)).collect::<Vec<_>>());
        if present.is_empty() {
            return bad("the base population has no values for this key");
        }
        let mut cuts: Vec<f64> = (1..bins).map(|k| quantile(&present, k as f64 / bins as f64)).collect();
        cuts.dedup();
        let name = |i: usize| -> String {
            let lo = if i == 0 { "-inf".to_string() } else { format!("{}", cuts[i - 1]) };
            let hi = if i == cuts.len() { "+inf".to_string() } else { format!("{}", cuts[i]) };
            format!("{i:03} ({lo}, {hi}]")
        };
        for (set, side) in [(&base, 0), (&comp, 1)] {
            for v in set.iter() {
                let k = match v.as_ref().and_then(fnum) {
                    Some(x) => name(cuts.partition_point(|c| *c < x)),
                    None => "~ missing".into(),
                };
                let e = cells.entry(k).or_default();
                if side == 0 {
                    e.0 += 1.0;
                } else {
                    e.1 += 1.0;
                }
            }
        }
    } else {
        for (set, side) in [(&base, 0), (&comp, 1)] {
            for v in set.iter() {
                let e = cells.entry(v.as_ref().map_or("(missing)".to_string(), text_of)).or_default();
                if side == 0 {
                    e.0 += 1.0;
                } else {
                    e.1 += 1.0;
                }
            }
        }
    }
    let (nb, nc) = (base.len() as f64, comp.len() as f64);
    let mut rows = Vec::new();
    let mut total = 0.0;
    for (k, (b, c)) in &cells {
        let (sb, sc) = ((b / nb).max(1e-4), (c / nc).max(1e-4));
        let comp_ = (sc - sb) * (sc / sb).ln();
        total += comp_;
        rows.push(vec![Value::Str(k.clone()), Value::Int(*b as i64), Value::Int(*c as i64), f(b / nb), f(c / nc), f(comp_)]);
    }
    rows.push(vec![Value::Str("(total)".into()), Value::Int(base.len() as i64), Value::Int(comp.len() as i64), f(1.0), f(1.0), f(total)]);
    Ok((vec!["bin", "base_records", "compare_records", "base_share", "compare_share", "psi_component"], rows))
}

// ------------------------------------------------------------------ uw.affordability

fn term_months(v: &Value) -> Option<f64> {
    match v {
        Value::Str(s) => {
            let digits: String = s.trim().chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
            let n: f64 = digits.parse().ok()?;
            let unit = s.trim()[digits.len()..].trim().to_ascii_lowercase();
            Some(if unit.starts_with("year") || unit.starts_with("yr") { n * 12.0 } else { n })
        }
        _ => fnum(v),
    }
}

/// Level payment of an amortising loan: `P r / (1 - (1 + r)^-n)`, `r = annual % / 1200`, `P / n` at zero interest.
pub(crate) fn annuity(principal: f64, annual_pct: f64, months: f64) -> f64 {
    let r = annual_pct / 1200.0;
    if r.abs() < 1e-12 {
        principal / months
    } else {
        principal * r / (1.0 - (1.0 + r).powf(-months))
    }
}

/// `uw.affordability(label, incomeKey, debtKey, loanKey, rateKey, termKey[, opts])` -> group, records, evaluated,
/// excluded, median_new_payment, median_dti_now, median_dti_proforma, over_limit_now, over_limit_proforma,
/// over_limit_stressed, debt_exceeds_income, stored_mismatch, stored_capped.
/// `incomeKey` is income per month, or per year with `basis=annual`; `debtKey` is the existing monthly debt service;
/// `rateKey` is the annual rate in percent; `termKey` is months, or text such as `48 months` / `5 years`. DTI is in
/// percent. `opts` (`key=value;...`): `basis`, `limit` (default 43), `stress_bps` (default 200: the new payment is
/// recomputed at the rate plus this many basis points), `stored` (a column holding the DTI the source system
/// recorded; compared with the recomputed current DTI, tolerance `tol` default 1 point), `group`, `filter`.
/// Records with income at or below zero, or a missing input, are excluded and counted. `stored_capped` counts stored
/// values equal to the largest stored value while the recomputed DTI is higher, which is what a ceiling in the
/// source system looks like.
fn affordability(view: &View, a: &[Value]) -> Result<Rows> {
    let label = need_str(a, 0, "label")?;
    let (ik, dk, lk, rk, tk) = (need_str(a, 1, "incomeKey")?, need_str(a, 2, "debtKey")?, need_str(a, 3, "loanKey")?, need_str(a, 4, "rateKey")?, need_str(a, 5, "termKey")?);
    let o = parse_opts(opt_str(a, 6), &["basis", "limit", "stress_bps", "stored", "tol", "group", "filter"])?;
    let annual = match o.get("basis").map(String::as_str) {
        None | Some("monthly") => false,
        Some("annual") => true,
        Some(s) => return bad(format!("basis must be monthly or annual, not '{s}'")),
    };
    let (limit, stress, tol) = (opt_f64(&o, "limit", 43.0)?, opt_f64(&o, "stress_bps", 200.0)?, opt_f64(&o, "tol", 1.0)?);
    let gkeys = o.get("group").map(|g| keys_of(g)).unwrap_or_default();
    let ev = select(view, label, o.get("filter").map(String::as_str))?;
    for k in [ik, dk, lk, rk, tk] {
        require_key(view, &ev, k, "column")?;
    }
    let stored = o.get("stored").map(String::as_str);
    if let Some(s) = stored {
        require_key(view, &ev, s, "stored DTI column")?;
    }
    for g in &gkeys {
        require_key(view, &ev, g, "group key")?;
    }
    let stored_max = stored.map(|s| ev.iter().filter_map(|b| lookup(view, b, s).and_then(|v| fnum(&v))).fold(f64::NEG_INFINITY, f64::max));
    #[derive(Default)]
    struct G {
        n: i64,
        excl: i64,
        pay: Vec<f64>,
        now: Vec<f64>,
        pro: Vec<f64>,
        over_now: i64,
        over_pro: i64,
        over_str: i64,
        exceeds: i64,
        mismatch: i64,
        capped: i64,
    }
    let mut groups: BTreeMap<String, G> = BTreeMap::new();
    for b in &ev {
        let label_g = if gkeys.is_empty() { "ALL".to_string() } else { gkeys.iter().map(|g| group_of(view, b, g).unwrap_or_else(|| "(missing)".into())).collect::<Vec<_>>().join("/") };
        let g = groups.entry(label_g).or_default();
        g.n += 1;
        let inc = lookup(view, b, ik).and_then(|v| fnum(&v));
        let (debt, loan, rate) = (lookup(view, b, dk).and_then(|v| fnum(&v)), lookup(view, b, lk).and_then(|v| fnum(&v)), lookup(view, b, rk).and_then(|v| fnum(&v)));
        let term = lookup(view, b, tk).and_then(|v| term_months(&v));
        let (Some(inc), Some(debt), Some(loan), Some(rate), Some(term)) = (inc, debt, loan, rate, term) else {
            g.excl += 1;
            continue;
        };
        let monthly = if annual { inc / 12.0 } else { inc };
        if monthly <= 0.0 || term <= 0.0 || loan < 0.0 || debt < 0.0 {
            g.excl += 1;
            continue;
        }
        let pay = annuity(loan, rate, term);
        let (now, pro, str_) = (debt / monthly * 100.0, (debt + pay) / monthly * 100.0, (debt + annuity(loan, rate + stress / 100.0, term)) / monthly * 100.0);
        g.pay.push(pay);
        g.now.push(now);
        g.pro.push(pro);
        g.over_now += i64::from(now > limit);
        g.over_pro += i64::from(pro > limit);
        g.over_str += i64::from(str_ > limit);
        g.exceeds += i64::from(debt > monthly);
        if let Some(s) = stored.and_then(|s| lookup(view, b, s)).and_then(|v| fnum(&v)) {
            g.mismatch += i64::from((s - now).abs() > tol);
            if let Some(mx) = stored_max {
                g.capped += i64::from(s == mx && now > s + tol);
            }
        }
    }
    let rows = groups
        .into_iter()
        .map(|(name, g)| {
            let ev_n = g.pay.len();
            let med = |v: &Vec<f64>| if v.is_empty() { None } else { Some(quantile(&sorted(v), 0.5)) };
            let share = |c: i64| if ev_n == 0 { None } else { Some(c as f64 / ev_n as f64) };
            vec![
                Value::Str(name),
                Value::Int(g.n),
                Value::Int(ev_n as i64),
                Value::Int(g.excl),
                fo(med(&g.pay)),
                fo(med(&g.now)),
                fo(med(&g.pro)),
                fo(share(g.over_now)),
                fo(share(g.over_pro)),
                fo(share(g.over_str)),
                Value::Int(g.exceeds),
                if stored.is_some() { Value::Int(g.mismatch) } else { Value::Null },
                if stored.is_some() { Value::Int(g.capped) } else { Value::Null },
            ]
        })
        .collect();
    Ok((vec!["group", "records", "evaluated", "excluded", "median_new_payment", "median_dti_now", "median_dti_proforma", "over_limit_now", "over_limit_proforma", "over_limit_stressed", "debt_exceeds_income", "stored_mismatch", "stored_capped"], rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv_matches_reference_vectors() {
        // reference vectors from the FNV spec
        assert_eq!(fnv1a(""), 0xcbf29ce484222325);
        assert_eq!(fnv1a("a"), 0xaf63dc4c8601ec8c);
        assert_eq!(fnv1a("foobar"), 0x85944171f73967e8);
    }

    #[test]
    fn auc_with_ties_and_delong() {
        // scores bad: 3,2,2 ; good: 2,1,1,0 -> P(bad > good) + 0.5 P(tie) = (3 + 2*(1 + .5... ) computed by hand below
        let s = [3.0, 2.0, 2.0, 2.0, 1.0, 1.0, 0.0];
        let y = [true, true, true, false, false, false, false];
        // pairs: bad3 beats 4 goods = 4; bad2 beats 3 (1,1,0) and ties 1 -> 3.5 (twice) => 4 + 3.5 + 3.5 = 11 of 12
        let (auc, se) = auc_delong(&s, &y).unwrap();
        assert!((auc - 11.0 / 12.0).abs() < 1e-15, "{auc}");
        assert!(se > 0.0 && se < 0.2, "{se}");
        assert!(auc_delong(&s, &[true; 7]).is_none());
    }

    #[test]
    fn ks_handles_ties() {
        let s = [1.0, 1.0, 2.0, 2.0];
        assert_eq!(ks_stat(&s, &[false, true, true, false]).unwrap(), 0.0);
        assert_eq!(ks_stat(&[1.0, 2.0, 3.0, 4.0], &[false, false, true, true]).unwrap(), 1.0);
    }

    #[test]
    fn gamma_q_matches_chi_square_tail() {
        // scipy.stats.chi2.sf: (x, df, sf)
        for (x, df, want) in [(3.841458820694124, 1.0, 0.05), (5.991464547107979, 2.0, 0.05), (1.0, 4.0, 0.9097959895689501), (30.0, 5.0, 1.4748581038443073e-5), (0.2, 3.0, 0.9775892977616494)] {
            let got = gamma_q(df / 2.0, x / 2.0);
            assert!(((got - want) / want).abs() < 1e-9, "chi2.sf({x}, {df}) = {got}, want {want}");
        }
        assert!((gamma_q(0.5, 3.841458820694124 / 2.0) - 0.05).abs() < 1e-12);
    }

    #[test]
    fn annuity_formula() {
        // 100,000 at 6 % over 360 months is the textbook 599.55; zero interest is straight-line
        assert!((annuity(100_000.0, 6.0, 360.0) - 599.5505252).abs() < 1e-6);
        assert!((annuity(12_000.0, 0.0, 12.0) - 1000.0).abs() < 1e-12);
    }

    #[test]
    fn terms_parse() {
        assert_eq!(term_months(&Value::Str("48 months".into())), Some(48.0));
        assert_eq!(term_months(&Value::Str("5 years".into())), Some(60.0));
        assert_eq!(term_months(&Value::Int(36)), Some(36.0));
        assert_eq!(term_months(&Value::Str("soon".into())), None);
    }
}
