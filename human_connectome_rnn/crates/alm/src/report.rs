//! End-to-end run, limit checks, text/JSON report.

use crate::curve::{Shock, ShockSizes, YieldCurve};
use crate::error::{AlmError, Result};
use crate::instrument::BalanceSheet;
use crate::liquidity::{self, LiquidityReport};
use crate::network::{self, SpectralConfig, SpectralReport};
use crate::nii::{self, NiiConfig};
use crate::valuation::{self, RiskMeasures};
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Limits {
    /// Worst-scenario EVE loss as a share of book equity.
    pub max_eve_loss_pct_equity: f64,
    /// Worst-scenario 12m NII loss as a share of base NII.
    pub max_nii_loss_pct: f64,
    pub min_lcr: f64,
    pub max_abs_duration_gap_years: f64,
    pub max_contagion_radius: f64,
    pub max_funding_hhi: f64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_eve_loss_pct_equity: 0.15,
            max_nii_loss_pct: 0.10,
            min_lcr: 1.0,
            max_abs_duration_gap_years: 2.0,
            max_contagion_radius: 0.85,
            max_funding_hhi: 0.35,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AlmConfig {
    pub horizon_months: u32,
    pub nii_horizon_months: u32,
    pub constant_balance_sheet: bool,
    /// Calibrate per-instrument discount spreads so PV = book on the base curve.
    pub calibrate_to_book: bool,
    /// Floor applied to shocked zero rates.
    pub rate_floor: f64,
    pub spectral: SpectralConfig,
}

impl Default for AlmConfig {
    fn default() -> Self {
        Self {
            horizon_months: 480,
            nii_horizon_months: 12,
            constant_balance_sheet: true,
            calibrate_to_book: true,
            rate_floor: 0.0,
            spectral: SpectralConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlmInput {
    pub curve: YieldCurve,
    pub balance_sheet: BalanceSheet,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub shocks: ShockSizes,
    #[serde(default)]
    pub config: AlmConfig,
}

impl AlmInput {
    pub fn from_json(s: &str) -> Result<Self> {
        serde_json::from_str(s).map_err(|e| AlmError::Io(e.to_string()))
    }
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string_pretty(self).map_err(|e| AlmError::Io(e.to_string()))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ScenarioResult {
    pub name: String,
    pub eve: f64,
    pub delta_eve: f64,
    pub delta_eve_pct_equity: f64,
    pub nii: f64,
    pub delta_nii: f64,
    pub delta_nii_pct: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LimitCheck {
    pub name: String,
    pub value: f64,
    pub limit: f64,
    pub ok: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AlmReport {
    pub name: String,
    pub book_assets: f64,
    pub book_liabilities: f64,
    pub book_equity: f64,
    pub risk: RiskMeasures,
    pub key_rate_dv01: Vec<(f64, f64)>,
    pub scenarios: Vec<ScenarioResult>,
    pub liquidity: LiquidityReport,
    pub spectral: SpectralReport,
    pub checks: Vec<LimitCheck>,
}

pub fn run_alm(input: &AlmInput) -> Result<AlmReport> {
    input.curve.validate()?;
    input.balance_sheet.validate()?;
    let bs = &input.balance_sheet;
    let cfg = &input.config;
    let h = cfg.horizon_months;
    let nii_cfg = NiiConfig { horizon_months: cfg.nii_horizon_months, constant_balance_sheet: cfg.constant_balance_sheet };

    let sp = valuation::discount_spreads(bs, &input.curve, h, cfg.calibrate_to_book)?;
    let risk = valuation::risk_measures(bs, &input.curve, h, &sp)?;
    let krd = valuation::key_rate_dv01(bs, &input.curve, h, &sp)?;
    let book_equity = bs.book_equity();

    let base_nii = nii::simulate(bs, &input.curve, &input.curve, &nii_cfg)?.nii;
    let mut scenarios = Vec::new();
    for shock in Shock::ALL {
        let c = shock.apply(&input.curve, &input.shocks, cfg.rate_floor);
        let eve = valuation::value(bs, &input.curve, &c, h, &sp)?.eve;
        let n = nii::simulate(bs, &input.curve, &c, &nii_cfg)?.nii;
        scenarios.push(ScenarioResult {
            name: shock.name().into(),
            eve,
            delta_eve: eve - risk.eve,
            delta_eve_pct_equity: if book_equity.abs() > 1e-12 { (eve - risk.eve) / book_equity } else { f64::NAN },
            nii: n,
            delta_nii: n - base_nii,
            delta_nii_pct: if base_nii.abs() > 1e-12 { (n - base_nii) / base_nii.abs() } else { f64::NAN },
        });
    }

    let liq = liquidity::analyse(bs, &input.curve, h)?;
    let spec = network::analyse(bs, &cfg.spectral)?;

    let lim = &input.limits;
    let worst_eve = scenarios.iter().map(|s| s.delta_eve).fold(0.0_f64, f64::min);
    let worst_nii = scenarios.iter().map(|s| s.delta_nii).fold(0.0_f64, f64::min);
    let eve_loss = if book_equity > 0.0 { -worst_eve / book_equity } else { f64::INFINITY };
    let nii_loss = if base_nii.abs() > 1e-12 { -worst_nii / base_nii.abs() } else { 0.0 };
    let chk = |name: &str, value: f64, limit: f64, ok: bool| LimitCheck { name: name.into(), value, limit, ok };
    let checks = vec![
        chk("EVE loss, worst scenario (share of book equity)", eve_loss, lim.max_eve_loss_pct_equity, eve_loss <= lim.max_eve_loss_pct_equity),
        chk("NII loss, worst scenario (share of base NII)", nii_loss, lim.max_nii_loss_pct, nii_loss <= lim.max_nii_loss_pct),
        chk("LCR (minimum)", liq.lcr.ratio, lim.min_lcr, liq.lcr.ratio >= lim.min_lcr),
        chk("|duration gap| (years)", risk.duration_gap.abs(), lim.max_abs_duration_gap_years, risk.duration_gap.abs() <= lim.max_abs_duration_gap_years),
        chk("contagion spectral radius", spec.contagion_radius, lim.max_contagion_radius, spec.contagion_radius <= lim.max_contagion_radius),
        chk("funding concentration (HHI)", spec.hhi_funding, lim.max_funding_hhi, spec.hhi_funding <= lim.max_funding_hhi),
    ];

    Ok(AlmReport {
        name: bs.name.clone(),
        book_assets: bs.total(crate::instrument::Side::Asset),
        book_liabilities: bs.total(crate::instrument::Side::Liability),
        book_equity,
        risk,
        key_rate_dv01: krd,
        scenarios,
        liquidity: liq,
        spectral: spec,
        checks,
    })
}

impl AlmReport {
    pub fn all_limits_ok(&self) -> bool {
        self.checks.iter().all(|c| c.ok)
    }
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string_pretty(self).map_err(|e| AlmError::Io(e.to_string()))
    }
}

impl fmt::Display for AlmReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "ALM REPORT: {}", self.name)?;
        writeln!(f, "{}", "=".repeat(78))?;
        writeln!(
            f,
            "Book: assets {:.1}  liabilities {:.1}  equity {:.1}   |  PV: assets {:.1}  liabilities {:.1}  EVE {:.1}",
            self.book_assets, self.book_liabilities, self.book_equity, self.risk.pv_assets, self.risk.pv_liabilities, self.risk.eve
        )?;
        let r = &self.risk;
        writeln!(f, "\nRATE RISK")?;
        writeln!(f, "  duration  assets {:.2}y  liabilities {:.2}y  gap {:+.2}y", r.duration_assets, r.duration_liabilities, r.duration_gap)?;
        writeln!(f, "  DV01      assets {:.3}  liabilities {:.3}  EVE {:+.3}  (value change per -1bp)", r.dv01_assets, r.dv01_liabilities, r.dv01_eve)?;
        writeln!(f, "  convexity assets {:.1}  liabilities {:.1}", r.convexity_assets, r.convexity_liabilities)?;
        write!(f, "  key-rate EVE DV01:")?;
        for (t, d) in &self.key_rate_dv01 {
            write!(f, " {:.2}y:{:+.3}", t, d)?;
        }
        writeln!(f)?;
        writeln!(f, "\n{:<14}{:>10}{:>10}{:>10}{:>10}{:>10}{:>9}", "SCENARIO", "EVE", "dEVE", "%equity", "NII", "dNII", "dNII%")?;
        for s in &self.scenarios {
            writeln!(
                f,
                "{:<14}{:>10.1}{:>+10.1}{:>9.1}%{:>10.2}{:>+10.2}{:>8.1}%",
                s.name, s.eve, s.delta_eve, s.delta_eve_pct_equity * 100.0, s.nii, s.delta_nii, s.delta_nii_pct * 100.0
            )?;
        }
        let l = &self.liquidity;
        writeln!(f, "\nLIQUIDITY")?;
        writeln!(
            f,
            "  LCR {:.0}%  (HQLA {:.1} = L1 {:.1} + L2A {:.1} + L2B {:.1} before caps; net 30d outflow {:.1})",
            l.lcr.ratio * 100.0, l.lcr.hqla, l.lcr.hqla_l1, l.lcr.hqla_l2a, l.lcr.hqla_l2b, l.lcr.net_outflows_30d
        )?;
        match l.survival_months {
            Some(m) => writeln!(f, "  survival horizon (contractual run-off, no roll-over): {} months", m)?,
            None => writeln!(f, "  survival horizon (contractual run-off, no roll-over): beyond projection horizon")?,
        }
        writeln!(f, "  {:<8}{:>10}{:>10}{:>10}{:>11}", "bucket", "inflow", "outflow", "net", "cumulative")?;
        for b in &l.ladder {
            writeln!(f, "  {:<8}{:>10.1}{:>10.1}{:>+10.1}{:>+11.1}", b.label, b.inflow, b.outflow, b.net, b.cumulative)?;
        }
        let s = &self.spectral;
        writeln!(f, "\nEXPOSURE NETWORK (spectral hypergraph + Arnoldi)")?;
        writeln!(f, "  {} instruments, {} hyperedges, algebraic connectivity {:.4}", s.vertices, s.hyperedges, s.algebraic_connectivity)?;
        write!(f, "  contagion radius {:.3} ({})", s.contagion_radius, if s.contagion_stable { "shocks decay" } else { "UNSTABLE" })?;
        match s.amplification {
            Some(a) => writeln!(f, ", funding-shock amplification x{:.2}", a)?,
            None => writeln!(f)?,
        }
        writeln!(f, "  HHI funding {:.3}  assets {:.3}", s.hhi_funding, s.hhi_assets)?;
        for c in &s.clusters {
            writeln!(
                f,
                "  cluster {}: assets {:.1} / liabilities {:.1} (imbalance {:+.2}, mostly {}): {}",
                c.id, c.assets, c.liabilities, c.imbalance, c.dominant_category, c.members.join(", ")
            )?;
        }
        writeln!(f, "\nLIMITS")?;
        for c in &self.checks {
            writeln!(f, "  [{}] {:<48} {:>9.3}  (limit {:.3})", if c.ok { "ok" } else { "BREACH" }, c.name, c.value, c.limit)?;
        }
        Ok(())
    }
}
