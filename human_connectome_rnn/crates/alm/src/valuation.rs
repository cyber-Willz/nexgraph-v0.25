//! PV, economic value of equity (EVE), DV01, duration, convexity, key-rate DV01.

use crate::cashflow::{project, CashFlow};
use crate::curve::YieldCurve;
use crate::error::{AlmError, Result};
use crate::instrument::{BalanceSheet, Instrument, Side};
use serde::Serialize;

const BP: f64 = 1e-4;

/// PV with a constant discount spread `s` (decimal, continuously compounded) over the curve.
pub fn pv_flows(flows: &[CashFlow], curve: &YieldCurve, s: f64) -> f64 {
    flows
        .iter()
        .map(|c| {
            let t = c.month as f64 / 12.0;
            c.total() * (-(curve.zero(t) + s) * t).exp()
        })
        .sum()
}

pub fn instrument_pv(inst: &Instrument, base: &YieldCurve, scen: &YieldCurve, horizon: u32, spread: f64) -> Result<f64> {
    Ok(pv_flows(&project(inst, base, scen, horizon)?, scen, spread))
}

/// Discount spread that makes PV(base curve) equal the book balance (an option-adjusted
/// spread). Without it, loan margins and deposit franchise value are capitalised and EVE
/// is far above book equity; with it, EVE equals book equity on the base curve and
/// scenario changes measure rate sensitivity only.
pub fn calibrate_spread(inst: &Instrument, base: &YieldCurve, horizon: u32) -> Result<f64> {
    if inst.balance <= 0.0 {
        return Ok(0.0);
    }
    let flows = project(inst, base, base, horizon)?;
    let f = |s: f64| pv_flows(&flows, base, s) - inst.balance;
    let (mut lo, mut hi) = (-0.10, 1.0);
    if f(lo) < 0.0 || f(hi) > 0.0 {
        return Err(AlmError::Invalid(format!("instrument '{}': cannot calibrate discount spread", inst.id)));
    }
    for _ in 0..100 {
        let mid = 0.5 * (lo + hi);
        if f(mid) > 0.0 {
            lo = mid
        } else {
            hi = mid
        }
    }
    Ok(0.5 * (lo + hi))
}

/// Per-instrument discount spreads (all zero when `calibrate` is false).
pub fn discount_spreads(bs: &BalanceSheet, base: &YieldCurve, horizon: u32, calibrate: bool) -> Result<Vec<f64>> {
    bs.instruments.iter().map(|i| if calibrate { calibrate_spread(i, base, horizon) } else { Ok(0.0) }).collect()
}

#[derive(Debug, Clone, Serialize)]
pub struct Valuation {
    pub pv_assets: f64,
    pub pv_liabilities: f64,
    /// Economic value of equity = PV(assets) - PV(liabilities).
    pub eve: f64,
}

pub fn value(bs: &BalanceSheet, base: &YieldCurve, scen: &YieldCurve, horizon: u32, spreads: &[f64]) -> Result<Valuation> {
    let (mut a, mut l) = (0.0, 0.0);
    for (i, &sp) in bs.instruments.iter().zip(spreads) {
        let pv = instrument_pv(i, base, scen, horizon, sp)?;
        match i.side {
            Side::Asset => a += pv,
            Side::Liability => l += pv,
        }
    }
    Ok(Valuation { pv_assets: a, pv_liabilities: l, eve: a - l })
}

#[derive(Debug, Clone, Serialize)]
pub struct RiskMeasures {
    pub pv_assets: f64,
    pub pv_liabilities: f64,
    pub eve: f64,
    /// Value gained per -1bp parallel move (positive = long duration).
    pub dv01_assets: f64,
    pub dv01_liabilities: f64,
    pub dv01_eve: f64,
    pub duration_assets: f64,
    pub duration_liabilities: f64,
    /// D_A - (PV_L / PV_A) * D_L, in years.
    pub duration_gap: f64,
    pub convexity_assets: f64,
    pub convexity_liabilities: f64,
}

pub fn risk_measures(bs: &BalanceSheet, base: &YieldCurve, horizon: u32, sp: &[f64]) -> Result<RiskMeasures> {
    let v0 = value(bs, base, base, horizon, sp)?;
    let up = value(bs, base, &base.parallel(BP), horizon, sp)?;
    let dn = value(bs, base, &base.parallel(-BP), horizon, sp)?;
    let dv_a = (dn.pv_assets - up.pv_assets) / 2.0;
    let dv_l = (dn.pv_liabilities - up.pv_liabilities) / 2.0;
    let dur = |dv: f64, pv: f64| dv / (pv * BP);
    let cvx = |u: f64, d: f64, p: f64| (u + d - 2.0 * p) / (p * BP * BP);
    let (da, dl) = (dur(dv_a, v0.pv_assets), dur(dv_l, v0.pv_liabilities));
    Ok(RiskMeasures {
        pv_assets: v0.pv_assets,
        pv_liabilities: v0.pv_liabilities,
        eve: v0.eve,
        dv01_assets: dv_a,
        dv01_liabilities: dv_l,
        dv01_eve: dv_a - dv_l,
        duration_assets: da,
        duration_liabilities: dl,
        duration_gap: da - v0.pv_liabilities / v0.pv_assets * dl,
        convexity_assets: cvx(up.pv_assets, dn.pv_assets, v0.pv_assets),
        convexity_liabilities: cvx(up.pv_liabilities, dn.pv_liabilities, v0.pv_liabilities),
    })
}

/// EVE DV01 attributed to each curve node (bump one node by +-1bp).
pub fn key_rate_dv01(bs: &BalanceSheet, base: &YieldCurve, horizon: u32, sp: &[f64]) -> Result<Vec<(f64, f64)>> {
    let mut out = Vec::with_capacity(base.tenors.len());
    for k in 0..base.tenors.len() {
        let up = value(bs, base, &base.bumped_node(k, BP), horizon, sp)?.eve;
        let dn = value(bs, base, &base.bumped_node(k, -BP), horizon, sp)?.eve;
        out.push((base.tenors[k], (dn - up) / 2.0));
    }
    Ok(out)
}

/// Value gained per -1bp parallel move, per unit of balance (side-agnostic, positive = long duration).
pub fn unit_dv01(inst: &Instrument, base: &YieldCurve, horizon: u32, spread: f64) -> Result<f64> {
    let mut unit = inst.clone();
    unit.balance = 1.0;
    let up = instrument_pv(&unit, base, &base.parallel(BP), horizon, spread)?;
    let dn = instrument_pv(&unit, base, &base.parallel(-BP), horizon, spread)?;
    Ok((dn - up) / 2.0)
}
