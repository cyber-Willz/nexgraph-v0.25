//! Balance-sheet items.

use crate::error::{AlmError, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    Asset,
    Liability,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HqlaLevel {
    L1,
    L2A,
    L2B,
}

impl HqlaLevel {
    pub fn haircut(self) -> f64 {
        match self {
            HqlaLevel::L1 => 0.0,
            HqlaLevel::L2A => 0.15,
            HqlaLevel::L2B => 0.50,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Kind {
    /// Monthly coupon, principal at `term_months`.
    FixedBullet,
    /// Level-payment amortizing loan over `term_months` (mortgage, consumer loan).
    Amortizing,
    /// Resets every `reset_months` to the index (zero rate at the reset tenor) + `spread`.
    Floating { reset_months: u32, spread: f64 },
    /// Non-maturity deposit with behavioural run-off and a rate pass-through `beta`
    /// to the 3-month zero rate. `decay_monthly` is the fraction of balance leaving per month.
    NonMaturity { decay_monthly: f64, beta: f64, floor: f64 },
}

fn d_category() -> String {
    "other".into()
}
fn d_inflow() -> f64 {
    0.5
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Instrument {
    pub id: String,
    pub side: Side,
    pub kind: Kind,
    pub balance: f64,
    /// Current annual nominal rate (coupon, loan rate or deposit rate), decimal.
    pub rate: f64,
    /// Remaining contractual term (ignored for non-maturity deposits).
    #[serde(default)]
    pub term_months: u32,
    /// Term of the product that replaces run-off in the NII simulation (default `term_months`).
    #[serde(default)]
    pub reinvest_term_months: Option<u32>,
    #[serde(default = "d_category")]
    pub category: String,
    /// Counterparty / sector group for concentration and the exposure hypergraph.
    /// Empty = granular pool (e.g. retail deposits): counted as its own group, no shared hyperedge.
    #[serde(default)]
    pub counterparty: String,
    #[serde(default)]
    pub hqla: Option<HqlaLevel>,
    /// 30-day stressed outflow factor for liabilities (share of balance).
    #[serde(default)]
    pub runoff_rate: f64,
    /// Share of 30-day contractual inflows counted in the LCR for assets.
    #[serde(default = "d_inflow")]
    pub inflow_factor: f64,
    /// May the rebalancer resize this item?
    #[serde(default)]
    pub tradable: bool,
    /// Relative cost weight of trading this item (default 1).
    #[serde(default)]
    pub trade_cost: Option<f64>,
    #[serde(default)]
    pub min_balance: Option<f64>,
    #[serde(default)]
    pub max_balance: Option<f64>,
}

impl Instrument {
    pub fn sign(&self) -> f64 {
        match self.side {
            Side::Asset => 1.0,
            Side::Liability => -1.0,
        }
    }

    pub fn reinvest_term(&self) -> u32 {
        match self.kind {
            Kind::NonMaturity { .. } => 1,
            _ => self.reinvest_term_months.unwrap_or(self.term_months).max(1),
        }
    }

    /// Rough repricing horizon in months, used only to bucket instruments in the hypergraph.
    pub fn repricing_months(&self) -> f64 {
        match &self.kind {
            Kind::Floating { reset_months, .. } => *reset_months as f64,
            Kind::FixedBullet => self.term_months as f64,
            Kind::Amortizing => self.term_months as f64 / 2.0,
            Kind::NonMaturity { decay_monthly, .. } => 1.0 / decay_monthly.max(1e-6),
        }
    }

    pub fn validate(&self) -> Result<()> {
        let bad = |m: &str| Err(AlmError::Invalid(format!("instrument '{}': {}", self.id, m)));
        if self.id.trim().is_empty() {
            return Err(AlmError::Invalid("instrument with empty id".into()));
        }
        if !(self.balance.is_finite() && self.balance >= 0.0) {
            return bad("balance must be finite and >= 0");
        }
        if !self.rate.is_finite() {
            return bad("rate must be finite");
        }
        if !(0.0..=1.0).contains(&self.runoff_rate) || !(0.0..=1.0).contains(&self.inflow_factor) {
            return bad("runoff_rate and inflow_factor must be in [0,1]");
        }
        match &self.kind {
            Kind::NonMaturity { decay_monthly, beta, floor } => {
                if !(*decay_monthly > 0.0 && *decay_monthly <= 1.0) {
                    return bad("decay_monthly must be in (0,1]");
                }
                if !(0.0..=1.0).contains(beta) || !floor.is_finite() {
                    return bad("beta must be in [0,1] and floor finite");
                }
            }
            Kind::Floating { reset_months, spread } => {
                if *reset_months == 0 || self.term_months == 0 || !spread.is_finite() {
                    return bad("floating needs reset_months >= 1, term_months >= 1, finite spread");
                }
            }
            _ => {
                if self.term_months == 0 {
                    return bad("term_months must be >= 1");
                }
            }
        }
        if let (Some(lo), Some(hi)) = (self.min_balance, self.max_balance) {
            if lo > hi {
                return bad("min_balance > max_balance");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BalanceSheet {
    pub name: String,
    #[serde(default)]
    pub currency: String,
    pub instruments: Vec<Instrument>,
}

impl BalanceSheet {
    pub fn validate(&self) -> Result<()> {
        let mut seen = HashSet::new();
        for i in &self.instruments {
            i.validate()?;
            if !seen.insert(i.id.clone()) {
                return Err(AlmError::Invalid(format!("duplicate instrument id '{}'", i.id)));
            }
        }
        if self.total(Side::Asset) <= 0.0 || self.total(Side::Liability) <= 0.0 {
            return Err(AlmError::Invalid("balance sheet needs positive assets and liabilities".into()));
        }
        if self.instruments.len() < 3 {
            return Err(AlmError::Invalid("need at least 3 instruments".into()));
        }
        Ok(())
    }

    pub fn total(&self, side: Side) -> f64 {
        self.instruments.iter().filter(|i| i.side == side).map(|i| i.balance).sum()
    }

    /// Book equity = assets - liabilities (residual).
    pub fn book_equity(&self) -> f64 {
        self.total(Side::Asset) - self.total(Side::Liability)
    }
}
