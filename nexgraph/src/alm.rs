//! Asset-liability management (ALM) analytics (v0.11).
//!
//! Built from what the *ALM insurance dataset v2.1* (3,501 holdings, 47,880 liability cash-flow rows, key-rate
//! durations, 9,700 stochastic paths) needed from the engine. Like `killchain.*`, every procedure reads events
//! stored as edges (the output of a hyperedge import with `keep_member_props`), and a key is looked up as an edge
//! property first, else as the name of the member node with that label. Where a procedure takes a `filter`, it is
//! a `Key=Value`, `Key!=Value` or 0/1 key and keeps only the matching events, which is how one picks a book
//! (`book=UNIT_LINKED`) or an asset class (`asset_class!=EQUITY`).
//!
//! | procedure | question it answers |
//! |---|---|
//! | `alm.position(label, mvKey, durKey[, convKey[, groupKey[, filter]]])` | market value, duration, convexity, DV01 and concentration by segment |
//! | `alm.liability_profile(label, cfKey, dfKey, monthKey[, groupKey[, filter]])` | present value, duration, convexity, DV01 and run-off of liability cash flows |
//! | `alm.duration_gap(aLabel, mvKey, durKey, convKey, lLabel, cfKey, dfKey, monthKey[, aFilter[, lFilter]])` | funding ratio, leverage-adjusted duration gap, net DV01 and convexity |
//! | `alm.rate_shock(.. same .., shocksBp[, aFilter[, lFilter[, bondLabel, bondCfKeys, bondDfKey, bondMonthKey[, bondFilter]]]])` | surplus under parallel yield shifts (liabilities repriced exactly; assets by duration + convexity, or exactly from bond cash flows when given) |
//! | `alm.cashflow_ladder(inLabel, inMonthKey, inKeys, outLabel, outMonthKey, outKey[, bucketMonths[, inFilter[, outFilter]]])` | cash-flow matching by time bucket, cumulative gap and shortfalls |
//! | `alm.krd_gap(label, sideKey, pvKey, krdKeys[, assetSide[, liabSide[, filter]]])` | key-rate DV01 of assets versus liabilities by tenor |
//! | `alm.scenario_risk(label, scenarioKey, monthKey, valueKey, horizon[, baseMonth[, mode[, exposure[, exposure2[, alpha]]]]])` | distribution, VaR and CTE across stochastic paths |
//!
//! ## Conventions (also in `HOW_IT_WORKS.md`)
//!
//! * **Yields are annual-effective.** A liability cash flow `cf` at `t = month/12` years with discount factor `df`
//!   has the implied zero yield `y = df^(-1/t) - 1`. A parallel shift of `d` reprices it as `cf (1+y+d)^(-t)`, exact
//!   for that shift. Modified duration is `sum(t pv/(1+y)) / PV` and convexity `sum(t(t+1) pv/(1+y)^2) / PV`, the
//!   same convention as the `mod_duration` and `convexity` columns of a bond holding, so the two sides are
//!   comparable.
//! * **DV01** is the loss in value per +1 bp parallel shift: `value * duration * 1e-4`. `net_dv01 = asset_dv01 -
//!   liability_dv01`; a positive number means surplus falls when rates rise.
//! * **Liabilities are exact; assets are exact only when you give their cash flows.** By default holdings are revalued
//!   with `-D dy + C dy^2 / 2`, which on the ALM dataset's 2,000 bonds is within 0.6 % of the exact bond P&L at +-100 bp,
//!   2.3 % at +-200 bp and 5.2 % at +-300 bp (the approximation understates large gains and large losses alike). Pass the
//!   bonds' cash-flow events (`bondLabel`, `bondCfKeys`, ...) to reprice them exactly; `d_assets_approx` then shows what the
//!   shortcut would have said.
//! * **A liability is a positive number** (an outflow). Net cash flows that are inflows (premiums) reduce it. A segment whose
//!   net present value is zero or negative (term cover and whole life in the ALM dataset: premiums exceed claims) has no
//!   duration or convexity (NULL, never a sign-flipped ratio), but `liability_profile` still reports its signed DV01.
//! * **Both sides must describe the same obligations.** `duration_gap` and `rate_shock` flag a funding ratio below
//!   0.5 or above 2 in a `warning` column. The ALM dataset's own `books_reconciliation.csv` shows why: its asset
//!   portfolio backs the unit-linked book, not the traditional one, and dividing one by the other gives 0.26.

use crate::error::Result;
use crate::killchain::{events, f, flag, fo, group_of, list_arg, num, opt_str, require_key};
use crate::model::Body;
use crate::netgraph::*;
use crate::store::View;
use crate::value::Value;
use std::collections::BTreeMap;

pub(crate) fn call(view: &View, name: &str, a: &[Value]) -> Result<Rows> {
    match name {
        "position" => position(view, a),
        "liability_profile" => liability_profile(view, a),
        "duration_gap" => duration_gap(view, a),
        "rate_shock" => rate_shock(view, a),
        "cashflow_ladder" => cashflow_ladder(view, a),
        "krd_gap" => krd_gap(view, a),
        "scenario_risk" => scenario_risk(view, a),
        _ => bad(format!("unknown procedure 'alm.{name}' (available: alm.position, alm.liability_profile, alm.duration_gap, alm.rate_shock, alm.cashflow_ladder, alm.krd_gap, alm.scenario_risk)")),
    }
}

// ------------------------------------------------------------------ helpers

const BP: f64 = 1e-4;

fn opt_f64(a: &[Value], i: usize, what: &str) -> Result<Option<f64>> {
    match a.get(i) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_f64().filter(|x| x.is_finite()).map(Some).ok_or_else(|| crate::error::Error::Query(format!("{what} must be a finite number"))),
    }
}

fn num_list(a: &[Value], i: usize, what: &str) -> Result<Option<Vec<f64>>> {
    match a.get(i) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::List(l)) => l
            .iter()
            .map(|v| v.as_f64().filter(|x| x.is_finite()).ok_or_else(|| crate::error::Error::Query(format!("{what} must be a list of numbers"))))
            .collect::<Result<Vec<_>>>()
            .map(Some),
        _ => bad(format!("{what} must be a list of numbers")),
    }
}

/// Events with `label`, optionally restricted to those matching `filter`.
pub(crate) fn select<'v>(view: &'v View, label: &str, filter: Option<&str>) -> Result<Vec<&'v Body>> {
    let ev = events(view, label)?;
    let Some(fl) = filter else { return Ok(ev) };
    require_key(view, &ev, fl, "filter")?;
    let kept: Vec<&Body> = ev.into_iter().filter(|b| flag(view, b, fl) == Some(true)).collect();
    if kept.is_empty() {
        return bad(format!("filter '{fl}' matches none of the '{label}' events"));
    }
    Ok(kept)
}

// ---------------------------------------------------------------- assets

#[derive(Default, Clone)]
struct Book {
    n: usize,
    mv: f64,
    /// sum of mv * duration over holdings that have a duration
    d1: f64,
    c1: f64,
    mv_known_dur: f64,
    max_mv: f64,
    sq: f64,
}

impl Book {
    fn add(&mut self, mv: f64, dur: Option<f64>, conv: Option<f64>) {
        self.n += 1;
        self.mv += mv;
        self.sq += mv * mv;
        if mv > self.max_mv {
            self.max_mv = mv;
        }
        if let Some(d) = dur {
            self.d1 += mv * d;
            self.mv_known_dur += mv;
            self.c1 += mv * conv.unwrap_or(0.0);
        }
    }
    fn dur(&self) -> Option<f64> {
        (self.mv_known_dur > 0.0).then(|| self.d1 / self.mv_known_dur)
    }
    fn conv(&self) -> Option<f64> {
        (self.mv_known_dur > 0.0).then(|| self.c1 / self.mv_known_dur)
    }
}

fn book_of(view: &View, ev: &[&Body], mv_key: &str, dur_key: &str, conv_key: Option<&str>) -> Result<(Book, usize)> {
    require_key(view, ev, mv_key, "market value key")?;
    require_key(view, ev, dur_key, "duration key")?;
    if let Some(c) = conv_key {
        require_key(view, ev, c, "convexity key")?;
    }
    let mut bk = Book::default();
    let mut missing = 0usize;
    for b in ev {
        match num(view, b, mv_key) {
            Some(mv) => bk.add(mv, num(view, b, dur_key), conv_key.and_then(|c| num(view, b, c))),
            None => missing += 1,
        }
    }
    if bk.n == 0 {
        return bad(format!("no event has a numeric '{mv_key}'"));
    }
    Ok((bk, missing))
}

fn position(view: &View, a: &[Value]) -> Result<Rows> {
    let label = need_str(a, 0, "label")?;
    let (mv_key, dur_key) = (need_str(a, 1, "mvKey")?, need_str(a, 2, "durKey")?);
    let conv_key = opt_str(a, 3);
    let group_key = opt_str(a, 4);
    let ev = select(view, label, opt_str(a, 5))?;
    require_key(view, &ev, mv_key, "market value key")?;
    require_key(view, &ev, dur_key, "duration key")?;
    if let Some(c) = conv_key {
        require_key(view, &ev, c, "convexity key")?;
    }
    if let Some(g) = group_key {
        require_key(view, &ev, g, "group key")?;
    }
    let mut groups: BTreeMap<String, Book> = BTreeMap::new();
    let mut total = 0.0;
    for b in &ev {
        let Some(mv) = num(view, b, mv_key) else { continue };
        let g = group_key.and_then(|k| group_of(view, b, k)).unwrap_or_else(|| if group_key.is_some() { "(missing)".into() } else { "ALL".into() });
        groups.entry(g).or_default().add(mv, num(view, b, dur_key), conv_key.and_then(|c| num(view, b, c)));
        total += mv;
    }
    let mut rows: Vec<(String, Book)> = groups.into_iter().collect();
    rows.sort_by(|x, y| y.1.mv.total_cmp(&x.1.mv));
    let out = rows
        .into_iter()
        .map(|(g, b)| {
            vec![
                Value::Str(g),
                Value::Int(b.n as i64),
                f(b.mv),
                f(if total != 0.0 { b.mv / total } else { f64::NAN }),
                fo(b.dur()),
                fo(b.conv()),
                f(b.d1 * BP),
                f(if b.mv != 0.0 { b.mv_known_dur / b.mv } else { f64::NAN }),
                f(if b.mv != 0.0 { b.max_mv / b.mv } else { f64::NAN }),
                f(if b.mv != 0.0 { b.sq / (b.mv * b.mv) } else { f64::NAN }),
            ]
        })
        .collect();
    Ok((vec!["group", "holdings", "market_value", "share", "mod_duration", "convexity", "dv01", "dur_coverage", "largest_holding_share", "hhi"], out))
}

// ------------------------------------------------------------ liabilities

/// One liability cash flow: time in years, amount (outflow positive), implied annual-effective zero yield, df.
#[derive(Clone, Copy)]
struct Lc {
    t: f64,
    cf: f64,
    y: f64,
    df: f64,
}

struct Liab {
    flows: Vec<Lc>,
    undiscounted: f64,
    dropped: usize,
}

fn liab_of(view: &View, ev: &[&Body], cf_keys: &[&str], df_key: &str, month_key: &str) -> Result<Liab> {
    for k in cf_keys {
        require_key(view, ev, k, "cash-flow key")?;
    }
    for (k, w) in [(df_key, "discount-factor key"), (month_key, "month key")] {
        require_key(view, ev, k, w)?;
    }
    let mut flows = Vec::with_capacity(ev.len());
    let (mut dropped, mut und) = (0usize, 0.0);
    for b in ev {
        // several keys (coupon + principal) are summed; a row with none of them is dropped
        let parts: Vec<f64> = cf_keys.iter().filter_map(|k| num(view, b, k)).collect();
        let (Some(df), Some(m)) = (num(view, b, df_key), num(view, b, month_key)) else {
            dropped += 1;
            continue;
        };
        if parts.is_empty() || df <= 0.0 || m < 0.0 {
            dropped += 1;
            continue;
        }
        let cf: f64 = parts.iter().sum();
        let t = m / 12.0;
        let y = if t > 0.0 { df.powf(-1.0 / t) - 1.0 } else { 0.0 };
        und += cf;
        flows.push(Lc { t, cf, y, df });
    }
    if flows.is_empty() {
        return bad(format!("no event has numeric {cf_keys:?}, '{df_key}' and '{month_key}' (with a positive discount factor)"));
    }
    Ok(Liab { flows, undiscounted: und, dropped })
}

impl Liab {
    fn pv(&self) -> f64 {
        self.flows.iter().map(|l| l.cf * l.df).sum()
    }
    /// Present value after a parallel shift `d` of the annual-effective zero curve; `None` if a yield would reach -100 %.
    fn reprice(&self, d: f64) -> Option<f64> {
        let mut s = 0.0;
        for l in &self.flows {
            // paid within the first month (t = 0): no shift applies, and the stated discount factor is kept so that
            // repricing at zero shift equals `pv()` (the ALM dataset has 93 such bond flows with df ~ 0.999)
            if l.t == 0.0 {
                s += l.cf * l.df;
                continue;
            }
            let base = 1.0 + l.y + d;
            if base <= 0.0 {
                return None;
            }
            s += l.cf * base.powf(-l.t);
        }
        Some(s)
    }
    /// (modified duration, convexity); `None` unless PV is positive.
    fn dur_conv(&self) -> Option<(f64, f64)> {
        let pv = self.pv();
        if pv <= 0.0 {
            return None;
        }
        let d: f64 = self.flows.iter().map(|l| l.cf * l.df * l.t / (1.0 + l.y)).sum();
        let c: f64 = self.flows.iter().map(|l| l.cf * l.df * l.t * (l.t + 1.0) / ((1.0 + l.y) * (1.0 + l.y))).sum();
        Some((d / pv, c / pv))
    }
}

fn liability_profile(view: &View, a: &[Value]) -> Result<Rows> {
    let label = need_str(a, 0, "label")?;
    let (cf_key, df_key, month_key) = (need_str(a, 1, "cfKey")?, need_str(a, 2, "dfKey")?, need_str(a, 3, "monthKey")?);
    let group_key = opt_str(a, 4);
    let ev = select(view, label, opt_str(a, 5))?;
    if let Some(g) = group_key {
        require_key(view, &ev, g, "group key")?;
    }
    let mut by: BTreeMap<String, Vec<&Body>> = BTreeMap::new();
    for b in &ev {
        let g = group_key.and_then(|k| group_of(view, b, k)).unwrap_or_else(|| if group_key.is_some() { "(missing)".into() } else { "ALL".into() });
        by.entry(g).or_default().push(b);
    }
    let mut out = Vec::new();
    for (g, evs) in by {
        let l = liab_of(view, &evs, &[cf_key], df_key, month_key)?;
        let pv = l.pv();
        let dc = l.dur_conv();
        // run-off: the horizon by which half of the present value has been paid, and the shares within 5 and 10 years
        let mut sched: Vec<(f64, f64)> = l.flows.iter().map(|x| (x.t, x.cf * x.df)).collect();
        sched.sort_by(|p, q| p.0.total_cmp(&q.0));
        let half = if pv > 0.0 {
            let mut cum = 0.0;
            sched.iter().find(|(_, v)| {
                cum += v;
                cum >= 0.5 * pv
            }).map(|x| x.0)
        } else {
            None
        };
        let within = |years: f64| if pv > 0.0 { Some(sched.iter().filter(|x| x.0 <= years + 1e-9).map(|x| x.1).sum::<f64>() / pv) } else { None };
        // implied yield: PV-weighted mean of the cash-flow zero yields
        let yld = if pv > 0.0 { Some(l.flows.iter().map(|x| x.cf * x.df * x.y).sum::<f64>() / pv) } else { None };
        out.push(vec![
            Value::Str(g),
            Value::Int(l.flows.len() as i64),
            Value::Int(l.dropped as i64),
            f(l.undiscounted),
            f(pv),
            fo(dc.map(|x| x.0)),
            fo(dc.map(|x| x.1)),
            // DV01 is defined for any sign of PV (a book of net inflows has a negative one); duration and convexity are not
            f(l.flows.iter().map(|x| x.cf * x.df * x.t / (1.0 + x.y)).sum::<f64>() * BP),
            fo(yld),
            fo(half),
            fo(within(5.0)),
            fo(within(10.0)),
        ]);
    }
    out.sort_by(|x, y| y[4].as_f64().unwrap_or(f64::MIN).total_cmp(&x[4].as_f64().unwrap_or(f64::MIN)));
    Ok((vec!["group", "cashflows", "dropped", "undiscounted", "pv", "mod_duration", "convexity", "dv01", "implied_yield", "half_life_years", "pv_share_5y", "pv_share_10y"], out))
}

// ------------------------------------------------- asset vs liability

struct Pair {
    a: Book,
    a_missing: usize,
    l: Liab,
}

/// Arguments 0..=7 are shared by `duration_gap` and `rate_shock`; the two filters sit at `fa` and `fl`.
fn pair_args(view: &View, a: &[Value], fa: usize, fl: usize) -> Result<Pair> {
    let a_label = need_str(a, 0, "assetLabel")?;
    let (mv_key, dur_key) = (need_str(a, 1, "mvKey")?, need_str(a, 2, "durKey")?);
    let conv_key = opt_str(a, 3);
    let l_label = need_str(a, 4, "liabLabel")?;
    let (cf_key, df_key, month_key) = (need_str(a, 5, "cfKey")?, need_str(a, 6, "dfKey")?, need_str(a, 7, "monthKey")?);
    let aev = select(view, a_label, opt_str(a, fa))?;
    let lev = select(view, l_label, opt_str(a, fl))?;
    let (book, a_missing) = book_of(view, &aev, mv_key, dur_key, conv_key)?;
    let l = liab_of(view, &lev, &[cf_key], df_key, month_key)?;
    Ok(Pair { a: book, a_missing, l })
}

fn warning(a: f64, l: f64) -> Value {
    if !(l > 0.0) {
        return Value::Str("liabilities are not positive: check the cash-flow sign (outflows positive) and the liability filter".into());
    }
    let fr = a / l;
    if !(0.5..=2.0).contains(&fr) {
        Value::Str(format!("assets are {:.0}% of liabilities: check that both selections describe the same obligations (use the filter arguments to pick a book)", fr * 100.0))
    } else {
        Value::Null
    }
}

fn duration_gap(view: &View, a: &[Value]) -> Result<Rows> {
    let p = pair_args(view, a, 8, 9)?;
    let (assets, liab) = (p.a.mv, p.l.pv());
    let (da, ca) = (p.a.dur().unwrap_or(0.0), p.a.conv().unwrap_or(0.0));
    let lc = p.l.dur_conv();
    let (dl, cl) = lc.unwrap_or((f64::NAN, f64::NAN));
    let asset_dv01 = p.a.d1 * BP;
    let liab_dv01 = if lc.is_some() { liab * dl * BP } else { f64::NAN };
    // leverage-adjusted gap: the duration by which assets would have to change to immunise the liabilities
    let gap = if assets > 0.0 { da - liab / assets * dl } else { f64::NAN };
    let row = vec![
        f(assets),
        f(liab),
        f(assets - liab),
        f(if liab > 0.0 { assets / liab } else { f64::NAN }),
        f(da),
        f(dl),
        f(gap),
        f(asset_dv01),
        f(liab_dv01),
        f(asset_dv01 - liab_dv01),
        f(ca),
        f(cl),
        f(p.a.c1 - liab * cl),
        f(if assets > 0.0 { p.a.mv_known_dur / assets } else { f64::NAN }),
        Value::Int(p.a_missing as i64),
        Value::Int(p.l.dropped as i64),
        warning(assets, liab),
    ];
    Ok((
        vec![
            "assets", "liabilities", "surplus", "funding_ratio", "asset_duration", "liability_duration", "duration_gap", "asset_dv01", "liability_dv01", "net_dv01", "asset_convexity", "liability_convexity",
            "net_dollar_convexity", "dur_coverage", "assets_missing_mv", "liabilities_dropped", "warning",
        ],
        vec![row],
    ))
}

fn rate_shock(view: &View, a: &[Value]) -> Result<Rows> {
    let p = pair_args(view, a, 9, 10)?;
    let shocks = num_list(a, 8, "shocksBp")?.unwrap_or_else(|| vec![-200.0, -100.0, -50.0, 0.0, 50.0, 100.0, 200.0]);
    if shocks.is_empty() || shocks.len() > 64 {
        return bad("shocksBp must list between 1 and 64 shifts in basis points");
    }
    // Optional exact revaluation of the bonds from their own cash flows (arguments 11..=15). Holdings that are
    // not in that selection (equities, cash) keep a zero rate sensitivity.
    let bonds = match opt_str(a, 11) {
        None => None,
        Some(lbl) => {
            let keys = list_arg(a, 12).filter(|k| !k.is_empty()).ok_or_else(|| crate::error::Error::Query("bondCfKeys must be a non-empty list of cash-flow keys".into()))?;
            let (dfk, mk) = (need_str(a, 13, "bondDfKey")?, need_str(a, 14, "bondMonthKey")?);
            let ev = select(view, lbl, opt_str(a, 15))?;
            let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
            let l = liab_of(view, &ev, &keys, dfk, mk)?;
            let base = l.reprice(0.0).unwrap_or(f64::NAN);
            Some((l, base))
        }
    };
    let (a0, d1, c1) = (p.a.mv, p.a.d1, p.a.c1);
    let l0 = p.l.reprice(0.0).unwrap_or(f64::NAN);
    let mut out = Vec::with_capacity(shocks.len());
    for bp in shocks {
        let d = bp * BP;
        let approx = -d1 * d + 0.5 * c1 * d * d;
        let exact = bonds.as_ref().map(|(l, base)| l.reprice(d).map(|v| v - base));
        let da = match &exact {
            Some(Some(x)) => *x,
            Some(None) => f64::NAN,
            None => approx,
        };
        let lq = p.l.reprice(d);
        let (assets, liab) = (a0 + da, lq.unwrap_or(f64::NAN));
        out.push(vec![
            f(bp),
            f(assets),
            f(liab),
            f(assets - liab),
            f(if liab > 0.0 { assets / liab } else { f64::NAN }),
            f(da),
            f(approx),
            f(liab - l0),
            f((assets - liab) - (a0 - l0)),
            f(((assets - liab) - (a0 - l0)) / a0),
            Value::Str(if bonds.is_some() { "cash-flow repricing" } else { "duration+convexity" }.into()),
            if lq.is_none() || matches!(exact, Some(None)) { Value::Str("shift takes a yield to -100% or below".into()) } else { warning(assets, liab) },
        ]);
    }
    Ok((vec!["shock_bp", "assets", "liabilities", "surplus", "funding_ratio", "d_assets", "d_assets_approx", "d_liabilities", "d_surplus", "d_surplus_pct_assets", "asset_method", "warning"], out))
}

// ----------------------------------------------------------- cash flows

fn cashflow_ladder(view: &View, a: &[Value]) -> Result<Rows> {
    let in_label = need_str(a, 0, "inLabel")?;
    let in_month = need_str(a, 1, "inMonthKey")?;
    let Some(in_keys) = list_arg(a, 2).filter(|k| !k.is_empty()) else { return bad("inKeys must be a non-empty list of cash-flow keys") };
    let out_label = need_str(a, 3, "outLabel")?;
    let (out_month, out_key) = (need_str(a, 4, "outMonthKey")?, need_str(a, 5, "outKey")?);
    let bk = match opt_f64(a, 6, "bucketMonths")? {
        None => 12usize,
        Some(x) if x >= 1.0 && x <= 360.0 && x.fract() == 0.0 => x as usize,
        _ => return bad("bucketMonths must be a whole number from 1 to 360"),
    };
    let iev = select(view, in_label, opt_str(a, 7))?;
    let oev = select(view, out_label, opt_str(a, 8))?;
    require_key(view, &iev, in_month, "inflow month key")?;
    for k in &in_keys {
        require_key(view, &iev, k, "inflow key")?;
    }
    require_key(view, &oev, out_month, "outflow month key")?;
    require_key(view, &oev, out_key, "outflow key")?;
    let mut ladder: BTreeMap<usize, (f64, f64)> = BTreeMap::new();
    for b in &iev {
        let Some(m) = num(view, b, in_month).filter(|m| *m >= 0.0) else { continue };
        let v: f64 = in_keys.iter().filter_map(|k| num(view, b, k)).sum();
        ladder.entry(m as usize / bk).or_default().0 += v;
    }
    for b in &oev {
        let (Some(m), Some(v)) = (num(view, b, out_month).filter(|m| *m >= 0.0), num(view, b, out_key)) else { continue };
        ladder.entry(m as usize / bk).or_default().1 += v;
    }
    let mut cum = 0.0;
    let mut out = Vec::with_capacity(ladder.len());
    for (i, (inflow, outflow)) in ladder {
        let net = inflow - outflow;
        cum += net;
        let (start, end) = (i * bk, i * bk + bk - 1);
        out.push(vec![
            Value::Str(format!("m{start}-{end}")),
            Value::Int(start as i64),
            Value::Int(end as i64),
            f(inflow),
            f(outflow),
            f(net),
            f(cum),
            f(if outflow > 0.0 { inflow / outflow } else { f64::NAN }),
            Value::Int((net < 0.0) as i64),
            Value::Int((cum < 0.0) as i64),
        ]);
    }
    Ok((vec!["bucket", "start_month", "end_month", "inflow", "outflow", "net", "cum_net", "cover_ratio", "shortfall", "cum_shortfall"], out))
}

/// `krd_0p25y` -> 0.25, `y_10y` -> 10: the tenor written in a key-rate column name.
fn tenor_of(key: &str) -> Option<f64> {
    let t = key.split_once('_').map_or(key, |x| x.1);
    t.strip_suffix('y').unwrap_or(t).replace('p', ".").parse().ok()
}

fn krd_gap(view: &View, a: &[Value]) -> Result<Rows> {
    let label = need_str(a, 0, "label")?;
    let (side_key, pv_key) = (need_str(a, 1, "sideKey")?, need_str(a, 2, "pvKey")?);
    let Some(keys) = list_arg(a, 3).filter(|k| !k.is_empty()) else { return bad("krdKeys must be a non-empty list of key-rate duration columns") };
    let asset_side = opt_str(a, 4).unwrap_or("ASSET");
    let liab_side = opt_str(a, 5).unwrap_or("LIABILITY");
    let ev = select(view, label, opt_str(a, 6))?;
    require_key(view, &ev, side_key, "side key")?;
    require_key(view, &ev, pv_key, "value key")?;
    for k in &keys {
        require_key(view, &ev, k, "key-rate duration")?;
    }
    let mut tot = vec![(0.0f64, 0.0f64); keys.len()];
    let (mut seen_a, mut seen_l) = (false, false);
    for b in &ev {
        let Some(side) = group_of(view, b, side_key) else { continue };
        let Some(pv) = num(view, b, pv_key) else { continue };
        let is_a = side == asset_side;
        if !is_a && side != liab_side {
            continue;
        }
        if is_a { seen_a = true } else { seen_l = true }
        for (i, k) in keys.iter().enumerate() {
            if let Some(d) = num(view, b, k) {
                let v = pv.abs() * d * BP;
                if is_a { tot[i].0 += v } else { tot[i].1 += v }
            }
        }
    }
    if !seen_a || !seen_l {
        return bad(format!("need events with {side_key} = '{asset_side}' and '{liab_side}' (found assets: {seen_a}, liabilities: {seen_l}); pass assetSide and liabSide to name them"));
    }
    let mut out: Vec<Vec<Value>> = keys
        .iter()
        .zip(tot)
        .map(|(k, (av, lv))| vec![Value::Str(k.clone()), fo(tenor_of(k)), f(av), f(lv), f(av - lv), f(if lv > 0.0 { av / lv } else { f64::NAN })])
        .collect();
    out.sort_by(|x, y| x[1].as_f64().unwrap_or(f64::MAX).total_cmp(&y[1].as_f64().unwrap_or(f64::MAX)));
    Ok((vec!["key", "tenor_years", "asset_dv01", "liability_dv01", "net_dv01", "cover_ratio"], out))
}

// ------------------------------------------------------------ scenarios

fn scenario_risk(view: &View, a: &[Value]) -> Result<Rows> {
    let label = need_str(a, 0, "label")?;
    let (sc_key, month_key, val_key) = (need_str(a, 1, "scenarioKey")?, need_str(a, 2, "monthKey")?, need_str(a, 3, "valueKey")?);
    let horizon = match opt_f64(a, 4, "horizon")? {
        Some(h) if h >= 0.0 && h.fract() == 0.0 => h,
        _ => return bad("horizon must be a whole month number"),
    };
    let base_m = opt_f64(a, 5, "baseMonth")?.unwrap_or(0.0);
    let relative = match opt_str(a, 6).unwrap_or("change") {
        "change" => false,
        "relative" => true,
        o => return bad(format!("mode must be 'change' or 'relative', got '{o}'")),
    };
    let (e1, e2) = (opt_f64(a, 7, "exposure")?, opt_f64(a, 8, "exposure2")?);
    let alpha = opt_f64(a, 9, "alpha")?.unwrap_or(0.995);
    if !(0.5..1.0).contains(&alpha) {
        return bad("alpha must be in [0.5, 1)");
    }
    if horizon == base_m {
        return bad("horizon and baseMonth are the same month");
    }
    let ev = events(view, label)?;
    for (k, w) in [(sc_key, "scenario key"), (month_key, "month key"), (val_key, "value key")] {
        require_key(view, &ev, k, w)?;
    }
    let mut by: BTreeMap<String, (Option<f64>, Option<f64>)> = BTreeMap::new();
    for b in &ev {
        let (Some(m), Some(v)) = (num(view, b, month_key), num(view, b, val_key)) else { continue };
        let is_base = m == base_m;
        if !is_base && m != horizon {
            continue;
        }
        let Some(id) = group_of(view, b, sc_key) else { continue };
        let e = by.entry(id).or_default();
        if is_base { e.0 = Some(v) } else { e.1 = Some(v) }
    }
    let (mut changes, mut bases, mut hors) = (vec![], vec![], vec![]);
    let mut incomplete = 0usize;
    for (_, (b0, h)) in by {
        match (b0, h) {
            (Some(b0), Some(h)) => {
                if relative && b0 == 0.0 {
                    incomplete += 1;
                    continue;
                }
                changes.push(if relative { h / b0 - 1.0 } else { h - b0 });
                bases.push(b0);
                hors.push(h);
            }
            _ => incomplete += 1,
        }
    }
    if changes.is_empty() {
        return bad(format!("no scenario has '{val_key}' at both month {base_m} and month {horizon}"));
    }
    let s = sorted(&changes);
    let pnl: Option<Vec<f64>> = (e1.is_some() || e2.is_some()).then(|| changes.iter().map(|c| e1.unwrap_or(0.0) * c + 0.5 * e2.unwrap_or(0.0) * c * c).collect());
    let (mut pm, mut pv, mut pc, mut pl) = (f64::NAN, f64::NAN, f64::NAN, f64::NAN);
    if let Some(p) = &pnl {
        let sp = sorted(p);
        let k = (((1.0 - alpha) * sp.len() as f64).ceil() as usize).clamp(1, sp.len());
        pm = mean(p);
        pv = -quantile(&sp, 1.0 - alpha);
        pc = -mean(&sp[..k]);
        pl = p.iter().filter(|x| **x < 0.0).count() as f64 / p.len() as f64;
    }
    let row = vec![
        Value::Int(changes.len() as i64),
        Value::Int(incomplete as i64),
        f(mean(&bases)),
        f(mean(&hors)),
        f(mean(&changes)),
        f(stdev(&changes)),
        f(s[0]),
        f(quantile(&s, 0.005)),
        f(quantile(&s, 0.05)),
        f(quantile(&s, 0.5)),
        f(quantile(&s, 0.95)),
        f(quantile(&s, 0.995)),
        f(s[s.len() - 1]),
        f(pm),
        f(pv),
        f(pc),
        f(pl),
    ];
    Ok((
        vec![
            "scenarios", "incomplete", "base_mean", "horizon_mean", "change_mean", "change_stdev", "change_min", "change_p005", "change_p05", "change_p50", "change_p95", "change_p995", "change_max", "pnl_mean", "pnl_var", "pnl_cte",
            "prob_loss",
        ],
        vec![row],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenor_is_read_from_the_column_name() {
        assert_eq!(tenor_of("krd_0p25y"), Some(0.25));
        assert_eq!(tenor_of("krd_10p0y"), Some(10.0));
        assert_eq!(tenor_of("y_30y"), Some(30.0));
        assert_eq!(tenor_of("pv"), None);
    }
}
