//! Minimum-cost DV01 rebalancing.
//!
//! Choose trades `x_i` on tradable instruments minimising `sum w_i x_i^2` subject to
//!   (1) `sum sign_i * unit_dv01_i * x_i = target_dv01 - current_dv01`  (hit the DV01 target)
//!   (2) `sum sign_i * x_i = 0`                                          (balance-sheet size unchanged)
//!   (3) `lo_i <= x_i <= hi_i`                                           (balance bounds)
//! Solved through the 2-dimensional dual (see `rebalance_dv01`); any plan returned is
//! verified against the target, the size constraint and the bounds.

use crate::curve::YieldCurve;
use crate::error::{AlmError, Result};
use crate::instrument::BalanceSheet;
use crate::valuation::{discount_spreads, risk_measures, unit_dv01};
use nalgebra::{Matrix2, Vector2};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Trade {
    pub id: String,
    pub delta: f64,
    pub new_balance: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RebalancePlan {
    pub trades: Vec<Trade>,
    pub dv01_before: f64,
    pub dv01_after: f64,
    pub target_dv01: f64,
    pub gross_turnover: f64,
}

pub fn rebalance_dv01(bs: &BalanceSheet, base: &YieldCurve, horizon: u32, calibrate: bool, target_dv01: f64) -> Result<RebalancePlan> {
    let sp = discount_spreads(bs, base, horizon, calibrate)?;
    let current = risk_measures(bs, base, horizon, &sp)?.dv01_eve;
    let t: Vec<usize> = (0..bs.instruments.len()).filter(|&k| bs.instruments[k].tradable).collect();
    if t.len() < 2 {
        return Err(AlmError::Infeasible("need at least 2 tradable instruments (set \"tradable\": true)".into()));
    }
    let m = t.len();
    let (mut a, mut s, mut w, mut lo, mut hi) = (vec![0.0; m], vec![0.0; m], vec![1.0; m], vec![0.0; m], vec![0.0; m]);
    for (j, &k) in t.iter().enumerate() {
        let i = &bs.instruments[k];
        a[j] = i.sign() * unit_dv01(i, base, horizon, sp[k])?;
        s[j] = i.sign();
        w[j] = i.trade_cost.unwrap_or(1.0).max(1e-9);
        lo[j] = i.min_balance.unwrap_or(0.0) - i.balance;
        hi[j] = i.max_balance.map(|b| b - i.balance).unwrap_or(f64::INFINITY);
    }

    let need = target_dv01 - current;

    // Dual solve. For multipliers lam, the minimiser of the Lagrangian under the bounds is
    //   x_j(lam) = clamp((a_j lam_0 + s_j lam_1) / w_j, lo_j, hi_j),
    // and the plan is the lam with A x(lam) = (need, 0). That residual is monotone in lam,
    // so a Newton ascent on the concave dual (backtracking on the dual value) converges; if it
    // stalls above tolerance the target is not reachable within the bounds. Constraint 1 is rescaled (DV01 per unit is ~1e-4).
    let a_scale = a.iter().fold(0.0_f64, |m, v| m.max(v.abs()));
    if a_scale <= 0.0 {
        return Err(AlmError::Infeasible("tradable instruments have no rate sensitivity".into()));
    }
    let an: Vec<f64> = a.iter().map(|v| v / a_scale).collect();
    let need_n = need / a_scale;
    let primal = |lam: &Vector2<f64>| -> Vec<f64> {
        (0..m).map(|j| ((an[j] * lam[0] + s[j] * lam[1]) / w[j]).clamp(lo[j], hi[j])).collect()
    };
    let resid = |x: &[f64]| -> Vector2<f64> {
        Vector2::new(an.iter().zip(x).map(|(p, q)| p * q).sum::<f64>() - need_n, s.iter().zip(x).map(|(p, q)| p * q).sum())
    };
    let mut g = Matrix2::<f64>::zeros();
    for j in 0..m {
        let wi = 1.0 / w[j];
        g[(0, 0)] += an[j] * an[j] * wi;
        g[(0, 1)] += an[j] * s[j] * wi;
        g[(1, 0)] += an[j] * s[j] * wi;
        g[(1, 1)] += s[j] * s[j] * wi;
    }
    let mut lam = g
        .try_inverse()
        .ok_or_else(|| AlmError::Infeasible("tradable instruments are collinear in (DV01, size); add a different tenor/side".into()))?
        * Vector2::new(need_n, 0.0);
    // Concave dual objective; line searches ascend on it (its gradient is -resid).
    let dual = |lam: &Vector2<f64>, x: &[f64]| -> f64 {
        lam[0] * need_n
            + (0..m).map(|j| 0.5 * w[j] * x[j] * x[j] - (an[j] * lam[0] + s[j] * lam[1]) * x[j]).sum::<f64>()
    };
    let mut x = primal(&lam);
    let mut r = resid(&x);
    let mut gval = dual(&lam, &x);
    for _ in 0..500 {
        if r.norm() < 1e-10 {
            break;
        }
        let mut jac = Matrix2::<f64>::zeros();
        for j in 0..m {
            let u = (an[j] * lam[0] + s[j] * lam[1]) / w[j];
            if u > lo[j] && u < hi[j] {
                let wi = 1.0 / w[j];
                jac[(0, 0)] += an[j] * an[j] * wi;
                jac[(0, 1)] += an[j] * s[j] * wi;
                jac[(1, 0)] += an[j] * s[j] * wi;
                jac[(1, 1)] += s[j] * s[j] * wi;
            }
        }
        let mut improved = false;
        for reg in [1e-12, 1e-8, 1e-4, 1e-1] {
            let jr = jac + Matrix2::identity() * (reg * (1.0 + jac.trace()));
            let Some(inv) = jr.try_inverse() else { continue };
            let step = inv * r;
            let mut t = 1.0;
            for _ in 0..40 {
                let cand = lam - step * t;
                let xc = primal(&cand);
                let gc = dual(&cand, &xc);
                if gc > gval + 1e-14 * (1.0 + gval.abs()) {
                    r = resid(&xc);
                    lam = cand;
                    x = xc;
                    gval = gc;
                    improved = true;
                    break;
                }
                t *= 0.5;
            }
            if improved {
                break;
            }
        }
        if !improved {
            break;
        }
    }

    let after = current + a.iter().zip(&x).map(|(p, q)| p * q).sum::<f64>();
    let size_err: f64 = s.iter().zip(&x).map(|(p, q)| p * q).sum();
    let tol = 1e-6 * (1.0 + current.abs() + target_dv01.abs());
    let bounds_ok = (0..m).all(|j| x[j] >= lo[j] - 1e-7 && x[j] <= hi[j] + 1e-7);
    if (after - target_dv01).abs() > tol || size_err.abs() > 1e-6 || !bounds_ok {
        return Err(AlmError::Infeasible(format!(
            "EVE DV01 target {:+.4} is not reachable with the tradable instruments and their bounds (closest found {:+.4}, current {:+.4}); relax bounds, mark more instruments tradable, or use a less ambitious target",
            target_dv01, after, current
        )));
    }

    let trades = t
        .iter()
        .zip(&x)
        .map(|(&k, &d)| Trade { id: bs.instruments[k].id.clone(), delta: d, new_balance: bs.instruments[k].balance + d })
        .collect::<Vec<_>>();
    let turnover = trades.iter().map(|t| t.delta.abs()).sum();
    Ok(RebalancePlan { trades, dv01_before: current, dv01_after: after, target_dv01, gross_turnover: turnover })
}

pub fn apply(bs: &BalanceSheet, plan: &RebalancePlan) -> BalanceSheet {
    let mut out = bs.clone();
    for t in &plan.trades {
        if let Some(i) = out.instruments.iter_mut().find(|i| i.id == t.id) {
            i.balance = t.new_balance.max(0.0);
        }
    }
    out
}
