//! Illustrative community-bank balance sheet (USD millions). Fictional numbers.

use crate::curve::{ShockSizes, YieldCurve};
use crate::instrument::{BalanceSheet, HqlaLevel, Instrument, Kind, Side};
use crate::report::{AlmConfig, AlmInput, Limits};

#[allow(clippy::too_many_arguments)]
fn inst(id: &str, side: Side, kind: Kind, balance: f64, rate: f64, term: u32, category: &str, cpty: &str) -> Instrument {
    Instrument {
        id: id.into(),
        side,
        kind,
        balance,
        rate,
        term_months: term,
        reinvest_term_months: None,
        category: category.into(),
        counterparty: cpty.into(),
        hqla: None,
        runoff_rate: 0.0,
        inflow_factor: 0.5,
        tradable: false,
        trade_cost: None,
        min_balance: None,
        max_balance: None,
    }
}

pub fn sample_curve() -> YieldCurve {
    YieldCurve::new(
        vec![1.0 / 12.0, 0.25, 0.5, 1.0, 2.0, 3.0, 5.0, 7.0, 10.0, 20.0, 30.0],
        vec![0.0405, 0.0410, 0.0412, 0.0400, 0.0385, 0.0382, 0.0390, 0.0405, 0.0420, 0.0450, 0.0445],
    )
    .expect("valid sample curve")
}

pub fn sample_balance_sheet() -> BalanceSheet {
    use Kind::*;
    use Side::*;
    let mut v = Vec::new();

    let mut cash = inst("cash_reserves", Asset, Floating { reset_months: 1, spread: -0.003 }, 80.0, 0.041, 1, "cash", "central_bank");
    cash.hqla = Some(HqlaLevel::L1);
    cash.tradable = true;
    cash.min_balance = Some(30.0);
    cash.max_balance = Some(200.0);
    v.push(cash);

    let mut g3 = inst("gov_bond_3y", Asset, FixedBullet, 120.0, 0.0405, 36, "securities", "sovereign");
    g3.hqla = Some(HqlaLevel::L1);
    g3.tradable = true;
    g3.min_balance = Some(0.0);
    g3.max_balance = Some(300.0);
    v.push(g3);

    let mut g10 = inst("gov_bond_10y", Asset, FixedBullet, 60.0, 0.0430, 120, "securities", "sovereign");
    g10.hqla = Some(HqlaLevel::L1);
    g10.tradable = true;
    g10.min_balance = Some(0.0);
    g10.max_balance = Some(200.0);
    v.push(g10);

    let mut corp = inst("corp_bonds_5y", Asset, FixedBullet, 40.0, 0.0520, 60, "securities", "corporate");
    corp.hqla = Some(HqlaLevel::L2A);
    v.push(corp);

    v.push(inst("resi_mortgages", Asset, Amortizing, 330.0, 0.0615, 300, "mortgage", "household"));
    v.push(inst("cre_loans_floating", Asset, Floating { reset_months: 6, spread: 0.03 }, 200.0, 0.0725, 84, "cre", "corporate_re"));
    v.push(inst("consumer_loans", Asset, Amortizing, 90.0, 0.095, 48, "consumer", "household"));
    v.push(inst("sme_loans_fixed", Asset, FixedBullet, 80.0, 0.078, 36, "sme", "corporate"));

    let mut dd = inst("demand_deposits", Liability, NonMaturity { decay_monthly: 0.015, beta: 0.10, floor: 0.0 }, 250.0, 0.005, 0, "deposit_demand", "");
    dd.runoff_rate = 0.10;
    v.push(dd);
    let mut sv = inst("savings_deposits", Liability, NonMaturity { decay_monthly: 0.020, beta: 0.45, floor: 0.002 }, 240.0, 0.022, 0, "deposit_savings", "");
    sv.runoff_rate = 0.10;
    v.push(sv);
    let mut t12 = inst("term_deposits_12m", Liability, FixedBullet, 170.0, 0.0385, 12, "deposit_term", "");
    t12.runoff_rate = 0.05;
    v.push(t12);
    v.push(inst("term_deposits_24m", Liability, FixedBullet, 70.0, 0.041, 24, "deposit_term", ""));
    let mut cd = inst("corporate_deposits", Liability, NonMaturity { decay_monthly: 0.030, beta: 0.60, floor: 0.0 }, 90.0, 0.018, 0, "deposit_corporate", "corporate");
    cd.runoff_rate = 0.40;
    v.push(cd);
    let mut wh = inst("wholesale_6m", Liability, FixedBullet, 60.0, 0.045, 6, "wholesale", "interbank");
    wh.runoff_rate = 0.25;
    wh.tradable = true;
    wh.min_balance = Some(0.0);
    wh.max_balance = Some(150.0);
    v.push(wh);
    v.push(inst("sub_debt_5y", Liability, FixedBullet, 30.0, 0.075, 60, "capital_debt", "investors"));

    BalanceSheet { name: "Sample community bank (USD m, fictional)".into(), currency: "USD".into(), instruments: v }
}

pub fn sample_input() -> AlmInput {
    AlmInput {
        curve: sample_curve(),
        balance_sheet: sample_balance_sheet(),
        limits: Limits::default(),
        shocks: ShockSizes::default(),
        config: AlmConfig::default(),
    }
}
