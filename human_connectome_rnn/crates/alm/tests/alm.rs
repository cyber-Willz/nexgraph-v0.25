use alm::cashflow::{amort_schedule, project};
use alm::curve::{nominal_from_cont, Shock, ShockSizes, YieldCurve};
use alm::instrument::{BalanceSheet, HqlaLevel, Instrument, Kind, Side};
use alm::liquidity::cap_hqla;
use alm::network::{build_hypergraph, contagion_matrix, spectral_radius_arnoldi, SpectralConfig};
use alm::nii::{simulate, NiiConfig};
use alm::optimizer::{apply, rebalance_dv01};
use alm::report::{run_alm, AlmInput};
use alm::sample::{sample_balance_sheet, sample_curve, sample_input};
use alm::valuation::{calibrate_spread, discount_spreads, instrument_pv, risk_measures, value};
use alm::AlmError;

fn mk(id: &str, side: Side, kind: Kind, bal: f64, rate: f64, term: u32) -> Instrument {
    Instrument {
        id: id.into(), side, kind, balance: bal, rate, term_months: term, reinvest_term_months: None,
        category: id.into(), counterparty: String::new(), hqla: None, runoff_rate: 0.0, inflow_factor: 0.5,
        tradable: false, trade_cost: None, min_balance: None, max_balance: None,
    }
}

#[test]
fn curve_interpolation_and_shocks() {
    let c = YieldCurve::new(vec![1.0, 5.0], vec![0.02, 0.04]).unwrap();
    assert!((c.zero(3.0) - 0.03).abs() < 1e-12);
    assert_eq!(c.zero(0.1), 0.02);
    assert_eq!(c.zero(50.0), 0.04);
    assert!(YieldCurve::new(vec![2.0, 1.0], vec![0.0, 0.0]).is_err());
    let s = ShockSizes::default();
    assert!((Shock::ParallelUp.shift(7.0, &s) - 0.02).abs() < 1e-15);
    assert!((Shock::ShortUp.shift(0.0, &s) - 0.025).abs() < 1e-15);
    // steepener: short end down, long end up
    assert!(Shock::Steepener.shift(0.25, &s) < 0.0 && Shock::Steepener.shift(30.0, &s) > 0.0);
    assert!(Shock::Flattener.shift(0.25, &s) > 0.0 && Shock::Flattener.shift(30.0, &s) < 0.0);
    // floor
    let down = Shock::ParallelDown.apply(&YieldCurve::flat(0.01), &s, 0.0);
    assert_eq!(down.zeros[0], 0.0);
}

#[test]
fn par_bond_prices_at_par_on_flat_curve() {
    let z = 0.04;
    let c = YieldCurve::flat(z);
    let b = mk("b", Side::Asset, Kind::FixedBullet, 100.0, nominal_from_cont(z), 60);
    let pv = instrument_pv(&b, &c, &c, 120, 0.0).unwrap();
    assert!((pv - 100.0).abs() < 1e-9, "pv {pv}");
}

#[test]
fn amortizing_principal_sums_to_balance_and_prices_at_par() {
    let s = amort_schedule(250.0, 0.06, 180);
    let p: f64 = s.iter().map(|x| x.0).sum();
    assert!((p - 250.0).abs() < 1e-8);
    let z = 0.035;
    let c = YieldCurve::flat(z);
    let l = mk("m", Side::Asset, Kind::Amortizing, 250.0, nominal_from_cont(z), 180);
    assert!((instrument_pv(&l, &c, &c, 240, 0.0).unwrap() - 250.0).abs() < 1e-8);
}

#[test]
fn floating_prices_near_par_when_coupon_matches_index() {
    let z = 0.04;
    let c = YieldCurve::flat(z);
    let f = mk("f", Side::Asset, Kind::Floating { reset_months: 3, spread: 0.0 }, 100.0, nominal_from_cont(z), 36);
    let pv = instrument_pv(&f, &c, &c, 60, 0.0).unwrap();
    assert!((pv - 100.0).abs() < 1e-8, "pv {pv}");
}

#[test]
fn deposit_rate_follows_beta_and_floor() {
    let base = YieldCurve::flat(0.03);
    let up = base.parallel(0.01);
    let d = mk("d", Side::Liability, Kind::NonMaturity { decay_monthly: 0.02, beta: 0.5, floor: 0.0 }, 100.0, 0.01, 0);
    let b = project(&d, &base, &base, 240).unwrap();
    let u = project(&d, &base, &up, 240).unwrap();
    assert!((u[0].interest / b[0].interest - 1.5).abs() < 1e-9); // 1% -> 1.5%
    let principal: f64 = b.iter().map(|c| c.principal).sum();
    assert!((principal - 100.0).abs() < 1e-9);
}

#[test]
fn calibration_makes_pv_equal_book() {
    let bs = sample_balance_sheet();
    let c = sample_curve();
    let sp = discount_spreads(&bs, &c, 480, true).unwrap();
    for (i, s) in bs.instruments.iter().zip(&sp) {
        let pv = instrument_pv(i, &c, &c, 480, *s).unwrap();
        assert!((pv - i.balance).abs() < 1e-6, "{} {} {}", i.id, pv, i.balance);
    }
    let v = value(&bs, &c, &c, 480, &sp).unwrap();
    assert!((v.eve - bs.book_equity()).abs() < 1e-5);
    assert!(calibrate_spread(&bs.instruments[0], &c, 480).is_ok());
}

#[test]
fn positive_duration_gap_loses_when_rates_rise() {
    let bs = sample_balance_sheet();
    let c = sample_curve();
    let sp = discount_spreads(&bs, &c, 480, true).unwrap();
    let r = risk_measures(&bs, &c, 480, &sp).unwrap();
    assert!(r.duration_gap > 0.0 && r.dv01_eve > 0.0);
    let up = value(&bs, &c, &c.parallel(0.02), 480, &sp).unwrap().eve;
    let dn = value(&bs, &c, &c.parallel(-0.02), 480, &sp).unwrap().eve;
    assert!(up < r.eve && dn > r.eve);
    // first-order check: dEVE ~ -dv01 * 200bp (+ convexity)
    assert!(((up - r.eve) + r.dv01_eve * 200.0).abs() < 0.25 * r.dv01_eve * 200.0);
}

#[test]
fn nii_is_asset_sensitive_with_floating_assets_and_fixed_funding() {
    let bs = BalanceSheet {
        name: "t".into(),
        currency: "USD".into(),
        instruments: vec![
            mk("fl", Side::Asset, Kind::Floating { reset_months: 1, spread: 0.02 }, 100.0, 0.06, 60),
            mk("fx", Side::Liability, Kind::FixedBullet, 90.0, 0.04, 60),
            mk("fx2", Side::Liability, Kind::FixedBullet, 5.0, 0.04, 60),
        ],
    };
    let c = YieldCurve::flat(0.04);
    let cfg = NiiConfig::default();
    let base = simulate(&bs, &c, &c, &cfg).unwrap().nii;
    let up = simulate(&bs, &c, &c.parallel(0.01), &cfg).unwrap().nii;
    let dn = simulate(&bs, &c, &c.parallel(-0.01), &cfg).unwrap().nii;
    assert!(up > base && dn < base);
    // 100 * 1% over ~11 of 12 months (first month at the old rate)
    assert!((up - base - 100.0 * 0.01 * 11.0 / 12.0).abs() < 0.05, "{}", up - base);
}

#[test]
fn hqla_caps_match_bcbs_formula() {
    assert!((cap_hqla(100.0, 0.0, 100.0) - 100.0 / 0.85).abs() < 1e-9);
    assert!((cap_hqla(100.0, 100.0, 0.0) - 100.0 / 0.6).abs() < 1e-9);
    assert_eq!(cap_hqla(50.0, 10.0, 5.0), 65.0);
}

#[test]
fn sample_lcr_and_ladder_are_consistent() {
    let rep = run_alm(&sample_input()).unwrap();
    let l = &rep.liquidity;
    assert!((l.lcr.hqla - (260.0 + 34.0)).abs() < 1e-9);
    assert!((l.lcr.ratio - l.lcr.hqla / l.lcr.net_outflows_30d).abs() < 1e-12);
    let net: f64 = l.ladder.iter().map(|b| b.net).sum();
    assert!((l.ladder.last().unwrap().cumulative - net).abs() < 1e-8);
    assert_eq!(l.ladder.len(), 7);
}

#[test]
fn arnoldi_radius_matches_dense_eigenvalues() {
    let bs = sample_balance_sheet();
    let cfg = SpectralConfig::default();
    let hg = build_hypergraph(&bs, &cfg).unwrap();
    let m = contagion_matrix(&bs, &hg, &cfg).unwrap();
    let rho = spectral_radius_arnoldi(&m, 40).unwrap();
    let dense = m.complex_eigenvalues().iter().map(|z| z.re.hypot(z.im)).fold(0.0, f64::max);
    assert!((rho - dense).abs() < 1e-8, "{rho} vs {dense}");
    assert!(rho > 0.0 && rho < 1.0);
}

#[test]
fn hypergraph_is_connected_and_clusters_cover_everything() {
    let rep = run_alm(&sample_input()).unwrap();
    let s = &rep.spectral;
    assert_eq!(s.vertices, 15);
    assert!(s.algebraic_connectivity > 1e-6);
    let covered: usize = s.clusters.iter().map(|c| c.members.len()).sum();
    assert_eq!(covered, 15);
    assert!(s.amplification.unwrap() >= 1.0);
    // stronger coupling raises the radius
    let mut inp = sample_input();
    inp.config.spectral.coupling = 0.9;
    let hi = run_alm(&inp).unwrap().spectral.contagion_radius;
    assert!(hi > s.contagion_radius);
}

#[test]
fn rebalance_hits_target_preserves_size_and_bounds() {
    let inp = sample_input();
    let (bs, c) = (&inp.balance_sheet, &inp.curve);
    let plan = rebalance_dv01(bs, c, 480, true, 0.18).unwrap();
    assert!((plan.dv01_after - 0.18).abs() < 1e-6);
    let size: f64 = plan.trades.iter().map(|t| bs.instruments.iter().find(|i| i.id == t.id).unwrap().sign() * t.delta).sum();
    assert!(size.abs() < 1e-6);
    for t in &plan.trades {
        let i = bs.instruments.iter().find(|i| i.id == t.id).unwrap();
        assert!(t.new_balance >= i.min_balance.unwrap_or(0.0) - 1e-6);
        assert!(t.new_balance <= i.max_balance.unwrap_or(f64::INFINITY) + 1e-6);
    }
    // re-measure on the rebalanced sheet (spreads are per-instrument, so recompute them from the original)
    let new_bs = apply(bs, &plan);
    let sp = discount_spreads(bs, c, 480, true).unwrap();
    let r = risk_measures(&new_bs, c, 480, &sp).unwrap();
    assert!((r.dv01_eve - 0.18).abs() < 1e-6, "{}", r.dv01_eve);
    assert!((new_bs.book_equity() - bs.book_equity()).abs() < 1e-6);
}

#[test]
fn rebalance_reports_unreachable_targets() {
    let inp = sample_input();
    match rebalance_dv01(&inp.balance_sheet, &inp.curve, 480, true, 0.0) {
        Err(AlmError::Infeasible(m)) => assert!(m.contains("not reachable")),
        other => panic!("expected infeasible, got {other:?}"),
    }
    let mut bs = inp.balance_sheet.clone();
    bs.instruments.iter_mut().for_each(|i| i.tradable = false);
    assert!(matches!(rebalance_dv01(&bs, &inp.curve, 480, true, 0.1), Err(AlmError::Infeasible(_))));
}

#[test]
fn json_round_trip_and_validation() {
    let inp = sample_input();
    let back = AlmInput::from_json(&inp.to_json().unwrap()).unwrap();
    let (a, b) = (run_alm(&inp).unwrap(), run_alm(&back).unwrap());
    assert!((a.risk.eve - b.risk.eve).abs() < 1e-12 && (a.spectral.contagion_radius - b.spectral.contagion_radius).abs() < 1e-12);
    assert!(a.to_string().contains("ALM REPORT"));
    assert!(a.to_json().is_ok());

    let mut dup = sample_input();
    dup.balance_sheet.instruments[1].id = dup.balance_sheet.instruments[0].id.clone();
    assert!(matches!(run_alm(&dup), Err(AlmError::Invalid(_))));

    let mut long = sample_input();
    long.config.horizon_months = 100; // mortgage term is 300
    assert!(matches!(run_alm(&long), Err(AlmError::Invalid(_))));

    let mut bad = sample_input();
    bad.balance_sheet.instruments[0].hqla = Some(HqlaLevel::L1);
    bad.balance_sheet.instruments[0].balance = -1.0;
    assert!(run_alm(&bad).is_err());
}
