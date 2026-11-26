//! Zero curve (continuously compounded) and BCBS-style interest-rate shocks.

use crate::error::{AlmError, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct YieldCurve {
    /// Node tenors in years, strictly increasing, > 0.
    pub tenors: Vec<f64>,
    /// Continuously compounded zero rates (decimal) at those tenors.
    pub zeros: Vec<f64>,
}

/// Nominal annual rate with monthly compounding equivalent to a continuously
/// compounded zero rate `z`. Used so that a coupon of `nominal_from_cont(z)/12`
/// per month discounts exactly to par on a flat curve at `z`.
pub fn nominal_from_cont(z: f64) -> f64 {
    12.0 * ((z / 12.0).exp() - 1.0)
}

impl YieldCurve {
    pub fn new(tenors: Vec<f64>, zeros: Vec<f64>) -> Result<Self> {
        let c = Self { tenors, zeros };
        c.validate()?;
        Ok(c)
    }

    pub fn flat(z: f64) -> Self {
        Self { tenors: vec![1.0], zeros: vec![z] }
    }

    pub fn validate(&self) -> Result<()> {
        if self.tenors.is_empty() || self.tenors.len() != self.zeros.len() {
            return Err(AlmError::Invalid("curve: tenors/zeros must be non-empty and equal length".into()));
        }
        if self.tenors.iter().chain(self.zeros.iter()).any(|x| !x.is_finite()) {
            return Err(AlmError::Invalid("curve: non-finite value".into()));
        }
        if self.tenors[0] <= 0.0 || self.tenors.windows(2).any(|w| w[1] <= w[0]) {
            return Err(AlmError::Invalid("curve: tenors must be > 0 and strictly increasing".into()));
        }
        Ok(())
    }

    /// Linear interpolation of zero rates; flat extrapolation.
    pub fn zero(&self, t: f64) -> f64 {
        let n = self.tenors.len();
        if t <= self.tenors[0] {
            return self.zeros[0];
        }
        if t >= self.tenors[n - 1] {
            return self.zeros[n - 1];
        }
        let k = self.tenors.partition_point(|&x| x <= t);
        let (t0, t1, z0, z1) = (self.tenors[k - 1], self.tenors[k], self.zeros[k - 1], self.zeros[k]);
        z0 + (z1 - z0) * (t - t0) / (t1 - t0)
    }

    pub fn df(&self, t: f64) -> f64 {
        (-self.zero(t) * t).exp()
    }

    /// Continuously compounded forward rate over [t1, t2].
    pub fn forward(&self, t1: f64, t2: f64) -> f64 {
        if t2 <= t1 {
            return self.zero(t1);
        }
        (self.zero(t2) * t2 - self.zero(t1) * t1) / (t2 - t1)
    }

    /// Apply a tenor-dependent shift (decimal) at the nodes, flooring zeros.
    pub fn shifted(&self, f: impl Fn(f64) -> f64, floor: f64) -> YieldCurve {
        YieldCurve {
            tenors: self.tenors.clone(),
            zeros: self.tenors.iter().zip(&self.zeros).map(|(&t, &z)| (z + f(t)).max(floor)).collect(),
        }
    }

    pub fn bumped_node(&self, idx: usize, dz: f64) -> YieldCurve {
        let mut c = self.clone();
        c.zeros[idx] += dz;
        c
    }

    pub fn parallel(&self, dz: f64) -> YieldCurve {
        YieldCurve { tenors: self.tenors.clone(), zeros: self.zeros.iter().map(|z| z + dz).collect() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShockSizes {
    pub parallel_bp: f64,
    pub short_bp: f64,
    pub long_bp: f64,
}

impl Default for ShockSizes {
    /// BCBS d368 USD calibration. Belize's dollar is pegged to USD, so these are
    /// a reasonable default; set your own via the input file.
    fn default() -> Self {
        Self { parallel_bp: 200.0, short_bp: 250.0, long_bp: 100.0 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Shock {
    Base,
    ParallelUp,
    ParallelDown,
    Steepener,
    Flattener,
    ShortUp,
    ShortDown,
}

impl Shock {
    pub const ALL: [Shock; 7] = [
        Shock::Base,
        Shock::ParallelUp,
        Shock::ParallelDown,
        Shock::Steepener,
        Shock::Flattener,
        Shock::ShortUp,
        Shock::ShortDown,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Shock::Base => "base",
            Shock::ParallelUp => "parallel up",
            Shock::ParallelDown => "parallel down",
            Shock::Steepener => "steepener",
            Shock::Flattener => "flattener",
            Shock::ShortUp => "short up",
            Shock::ShortDown => "short down",
        }
    }

    /// Shift (decimal) at tenor `t` years.
    pub fn shift(self, t: f64, s: &ShockSizes) -> f64 {
        let short = (-t / 4.0).exp();
        let long = 1.0 - short;
        let (p, sh, lg) = (s.parallel_bp / 1e4, s.short_bp / 1e4, s.long_bp / 1e4);
        match self {
            Shock::Base => 0.0,
            Shock::ParallelUp => p,
            Shock::ParallelDown => -p,
            Shock::Steepener => -0.65 * sh * short + 0.9 * lg * long,
            Shock::Flattener => 0.8 * sh * short - 0.6 * lg * long,
            Shock::ShortUp => sh * short,
            Shock::ShortDown => -sh * short,
        }
    }

    pub fn apply(self, curve: &YieldCurve, sizes: &ShockSizes, floor: f64) -> YieldCurve {
        curve.shifted(|t| self.shift(t, sizes), floor)
    }
}
