//! Liquidity: LCR (BCBS Level-2 caps), contractual maturity ladder, survival horizon.
//!
//! Non-maturity deposits use their behavioural decay in the ladder. The ladder and
//! survival horizon assume run-off with no new business or roll-over: a deliberately
//! harsh view, not a forecast.

use crate::cashflow::project;
use crate::curve::YieldCurve;
use crate::error::Result;
use crate::instrument::{BalanceSheet, HqlaLevel, Side};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Lcr {
    pub hqla_l1: f64,
    pub hqla_l2a: f64,
    pub hqla_l2b: f64,
    /// Stock of HQLA after haircuts and the 15% / 40% caps.
    pub hqla: f64,
    pub outflows_30d: f64,
    pub inflows_30d_capped: f64,
    pub net_outflows_30d: f64,
    /// `null` in JSON when net outflows are zero.
    pub ratio: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LadderBucket {
    pub label: String,
    pub inflow: f64,
    pub outflow: f64,
    pub net: f64,
    pub cumulative: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LiquidityReport {
    pub lcr: Lcr,
    pub ladder: Vec<LadderBucket>,
    /// First month in which HQLA + cumulative contractual net flow turns negative.
    pub survival_months: Option<u32>,
}

/// BCBS Level-2 caps on post-haircut amounts.
pub fn cap_hqla(l1: f64, l2a: f64, l2b: f64) -> f64 {
    let adj15 = (l2b - 15.0 / 85.0 * (l1 + l2a)).max(l2b - 15.0 / 60.0 * l1).max(0.0);
    let adj40 = (l2a + l2b - adj15 - 2.0 / 3.0 * l1).max(0.0);
    l1 + l2a + l2b - adj15 - adj40
}

const BUCKETS: [(&str, u32, u32); 7] = [
    ("<=1m", 1, 1),
    ("1-3m", 2, 3),
    ("3-6m", 4, 6),
    ("6-12m", 7, 12),
    ("1-3y", 13, 36),
    ("3-5y", 37, 60),
    (">5y", 61, u32::MAX),
];

pub fn analyse(bs: &BalanceSheet, base: &YieldCurve, horizon: u32) -> Result<LiquidityReport> {
    let h = horizon as usize;
    let (mut inflow, mut outflow) = (vec![0.0; h], vec![0.0; h]);
    let (mut l1, mut l2a, mut l2b) = (0.0, 0.0, 0.0);
    let (mut out30, mut in30) = (0.0, 0.0);

    for i in &bs.instruments {
        let flows = project(i, base, base, horizon)?;
        for c in &flows {
            let k = c.month as usize - 1;
            match i.side {
                Side::Asset => inflow[k] += c.total(),
                Side::Liability => outflow[k] += c.total(),
            }
        }
        match i.side {
            Side::Asset => match i.hqla {
                Some(level) => {
                    let v = i.balance * (1.0 - level.haircut());
                    match level {
                        HqlaLevel::L1 => l1 += v,
                        HqlaLevel::L2A => l2a += v,
                        HqlaLevel::L2B => l2b += v,
                    }
                }
                None => in30 += flows[0].total() * i.inflow_factor,
            },
            Side::Liability => out30 += i.balance * i.runoff_rate,
        }
    }

    let hqla = cap_hqla(l1, l2a, l2b);
    let in_capped = in30.min(0.75 * out30);
    let net = out30 - in_capped;
    let lcr = Lcr {
        hqla_l1: l1,
        hqla_l2a: l2a,
        hqla_l2b: l2b,
        hqla,
        outflows_30d: out30,
        inflows_30d_capped: in_capped,
        net_outflows_30d: net,
        ratio: if net > 1e-12 { hqla / net } else { f64::INFINITY },
    };

    let mut ladder = Vec::new();
    let mut cum = 0.0;
    for (label, from, to) in BUCKETS {
        let (a, b) = (from as usize - 1, (to as usize).min(h));
        if a >= h {
            break;
        }
        let inf: f64 = inflow[a..b].iter().sum();
        let outf: f64 = outflow[a..b].iter().sum();
        cum += inf - outf;
        ladder.push(LadderBucket { label: label.into(), inflow: inf, outflow: outf, net: inf - outf, cumulative: cum });
    }

    let mut survival = None;
    let mut c = 0.0;
    for m in 0..h {
        c += inflow[m] - outflow[m];
        if hqla + c < 0.0 {
            survival = Some(m as u32 + 1);
            break;
        }
    }

    Ok(LiquidityReport { lcr, ladder, survival_months: survival })
}
