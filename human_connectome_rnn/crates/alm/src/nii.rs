//! Net interest income simulation under an instantaneous curve shock.
//!
//! Static-curve convention: after the shock the curve stays where it is (no
//! forward path). Floating items reset to the shocked index; with a constant
//! balance sheet, run-off is replaced by the same product priced at
//! `shocked zero(reinvest term) + original margin over the base curve`.
//! Non-maturity deposits re-price with `beta` on the 3m rate.

use crate::cashflow::amort_schedule;
use crate::curve::{nominal_from_cont, YieldCurve};
use crate::error::Result;
use crate::instrument::{BalanceSheet, Instrument, Kind, Side};
use serde::Serialize;

#[derive(Debug, Clone, Copy)]
pub struct NiiConfig {
    pub horizon_months: u32,
    pub constant_balance_sheet: bool,
}

impl Default for NiiConfig {
    fn default() -> Self {
        Self { horizon_months: 12, constant_balance_sheet: true }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct NiiResult {
    pub interest_income: f64,
    pub interest_expense: f64,
    pub nii: f64,
}

/// Monthly interest amounts (positive magnitudes) for months 1..=horizon.
pub fn instrument_interest(inst: &Instrument, base: &YieldCurve, scen: &YieldCurve, cfg: &NiiConfig) -> Vec<f64> {
    let h = cfg.horizon_months as usize;
    let mut out = vec![0.0; h];
    let rt = inst.reinvest_term() as f64;
    let margin = inst.rate - nominal_from_cont(base.zero(rt / 12.0));
    let mkt = nominal_from_cont(scen.zero(rt / 12.0)) + margin;
    let bal = inst.balance;
    let term = inst.term_months as usize;

    match &inst.kind {
        Kind::FixedBullet => {
            for m in 1..=h {
                if m <= term {
                    out[m - 1] = bal * inst.rate / 12.0;
                } else if cfg.constant_balance_sheet {
                    out[m - 1] = bal * mkt / 12.0;
                }
            }
        }
        Kind::Amortizing => {
            let sched = amort_schedule(bal, inst.rate, term);
            let mut reinvested = 0.0;
            for m in 1..=h {
                let mut interest = reinvested * mkt / 12.0;
                let mut pending = 0.0;
                if m <= term {
                    interest += sched[m - 1].1;
                    if cfg.constant_balance_sheet {
                        pending = sched[m - 1].0;
                    }
                }
                out[m - 1] = interest;
                reinvested += pending;
            }
        }
        Kind::Floating { reset_months, spread } => {
            let rs = *reset_months as usize;
            let idx = nominal_from_cont(scen.zero(rs as f64 / 12.0)) + spread;
            for m in 1..=h {
                if m > term && !cfg.constant_balance_sheet {
                    continue;
                }
                let r = if m <= rs && m <= term { inst.rate } else { idx };
                out[m - 1] = bal * r / 12.0;
            }
        }
        Kind::NonMaturity { decay_monthly, beta, floor } => {
            let r = (inst.rate + beta * (scen.zero(0.25) - base.zero(0.25))).max(*floor);
            for m in 1..=h {
                let b = if cfg.constant_balance_sheet { bal } else { bal * (1.0 - decay_monthly).powi(m as i32 - 1) };
                out[m - 1] = b * r / 12.0;
            }
        }
    }
    out
}

pub fn simulate(bs: &BalanceSheet, base: &YieldCurve, scen: &YieldCurve, cfg: &NiiConfig) -> Result<NiiResult> {
    let (mut inc, mut exp) = (0.0, 0.0);
    for i in &bs.instruments {
        let s: f64 = instrument_interest(i, base, scen, cfg).iter().sum();
        match i.side {
            Side::Asset => inc += s,
            Side::Liability => exp += s,
        }
    }
    Ok(NiiResult { interest_income: inc, interest_expense: exp, nii: inc - exp })
}
