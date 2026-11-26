//! # alm — Asset-Liability Management
//!
//! Built on the workspace's `krylov_ds` (Arnoldi) and `spectral_hypergraph`
//! crates.
//!
//! * [`curve`]      zero curve, BCBS-style rate shocks
//! * [`instrument`] balance-sheet items (fixed, amortizing, floating, non-maturity)
//! * [`cashflow`]   monthly cash-flow projection
//! * [`valuation`]  PV, EVE, DV01, duration/convexity, key-rate DV01
//! * [`nii`]        12-month net interest income simulation
//! * [`liquidity`]  LCR (with Level-2 caps), maturity ladder, survival horizon
//! * [`network`]    exposure hypergraph, spectral clustering, Arnoldi contagion radius
//! * [`optimizer`]  minimum-cost DV01 rebalancing
//! * [`report`]     full run, limit checks, text/JSON output

pub mod cashflow;
pub mod curve;
pub mod error;
pub mod instrument;
pub mod liquidity;
pub mod network;
pub mod nii;
pub mod optimizer;
pub mod report;
pub mod sample;
pub mod valuation;

pub use curve::{Shock, ShockSizes, YieldCurve};
pub use error::{AlmError, Result};
pub use instrument::{BalanceSheet, HqlaLevel, Instrument, Kind, Side};
pub use report::{run_alm, AlmConfig, AlmInput, AlmReport, Limits};
