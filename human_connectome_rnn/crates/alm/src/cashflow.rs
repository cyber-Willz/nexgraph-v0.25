//! Monthly cash-flow projection (dense: one entry per month 1..=horizon).

use crate::curve::{nominal_from_cont, YieldCurve};
use crate::error::{AlmError, Result};
use crate::instrument::{Instrument, Kind};

#[derive(Debug, Clone, Copy, Default)]
pub struct CashFlow {
    pub month: u32,
    pub principal: f64,
    pub interest: f64,
}

impl CashFlow {
    pub fn total(&self) -> f64 {
        self.principal + self.interest
    }
}

/// Level-payment schedule: `(principal, interest)` for months 1..=n.
pub fn amort_schedule(balance: f64, annual_rate: f64, n: usize) -> Vec<(f64, f64)> {
    let r = annual_rate / 12.0;
    let pmt = if r.abs() < 1e-12 {
        balance / n as f64
    } else {
        balance * r / (1.0 - (1.0 + r).powi(-(n as i32)))
    };
    let mut out = Vec::with_capacity(n);
    let mut bal = balance;
    for _ in 0..n {
        let interest = bal * r;
        let principal = (pmt - interest).min(bal).max(0.0);
        bal -= principal;
        out.push((principal, interest));
    }
    out
}

/// Project cash flows. Floating coupons beyond the first reset use the forward
/// curve of `scen`; non-maturity deposit rates move by `beta` times the change
/// of the 3m zero rate between `base` and `scen`.
pub fn project(inst: &Instrument, base: &YieldCurve, scen: &YieldCurve, horizon: u32) -> Result<Vec<CashFlow>> {
    if horizon == 0 {
        return Err(AlmError::Invalid("horizon must be >= 1".into()));
    }
    let h = horizon as usize;
    let mut cf: Vec<CashFlow> = (1..=horizon).map(|m| CashFlow { month: m, ..Default::default() }).collect();
    let bal = inst.balance;

    if let Kind::NonMaturity { decay_monthly, beta, floor } = &inst.kind {
        let r = (inst.rate + beta * (scen.zero(0.25) - base.zero(0.25))).max(*floor);
        let mut b = bal;
        for c in cf.iter_mut() {
            let p = b * decay_monthly;
            c.interest = b * r / 12.0;
            c.principal = p;
            b -= p;
        }
        cf[h - 1].principal += b; // residual paid out at the horizon
        return Ok(cf);
    }

    let term = inst.term_months as usize;
    if term > h {
        return Err(AlmError::Invalid(format!(
            "instrument '{}': term {} months exceeds projection horizon {}",
            inst.id, term, h
        )));
    }
    match &inst.kind {
        Kind::FixedBullet => {
            for c in cf.iter_mut().take(term) {
                c.interest = bal * inst.rate / 12.0;
            }
            cf[term - 1].principal = bal;
        }
        Kind::Amortizing => {
            let s = amort_schedule(bal, inst.rate, term);
            for (c, (p, i)) in cf.iter_mut().zip(s) {
                c.principal = p;
                c.interest = i;
            }
        }
        Kind::Floating { reset_months, spread } => {
            let rs = *reset_months as usize;
            let mut rate = inst.rate;
            for m in 0..term {
                if m > 0 && m % rs == 0 {
                    let f = scen.forward(m as f64 / 12.0, (m + rs) as f64 / 12.0);
                    rate = nominal_from_cont(f) + spread;
                }
                cf[m].interest = bal * rate / 12.0;
            }
            cf[term - 1].principal = bal;
        }
        Kind::NonMaturity { .. } => unreachable!(),
    }
    Ok(cf)
}
