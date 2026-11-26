//! Exposure network: spectral hypergraph + Arnoldi contagion radius.
//!
//! Instruments are hypergraph vertices. Hyperedges group instruments that share an
//! exposure factor: product category, counterparty group, repricing window. A weak
//! global hyperedge keeps the hypergraph connected. This reuses `spectral_hypergraph`
//! (normalized Laplacian, spectral clustering) and `krylov_ds` (Arnoldi).
//!
//! Contagion is a stylised linear propagation model, `x' = M x + s` with
//! `M = alpha * W * diag(f)`, W the row-normalised clique-expansion adjacency and `f`
//! a per-instrument fragility (liability run-off rate, asset illiquidity). The
//! coefficients are assumptions, not calibrated parameters. `rho(M) < 1` means a
//! shock decays; `1 / (1 - rho)` is the order of magnitude of its amplification.

use crate::error::{AlmError, Result};
use crate::instrument::{BalanceSheet, HqlaLevel, Instrument, Side};
use krylov_ds::eig::arnoldi_ritz_values;
use krylov_ds::{Arnoldi, DenseMatrix};
use nalgebra::{DMatrix, DVector};
use serde::{Deserialize, Serialize};
use spectral_hypergraph::hypergraph::{HypergraphBuilder, SpectralHypergraph, VertexId};
use spectral_hypergraph::laplacian::{clique_expansion_adjacency, dense_normalized_laplacian};
use spectral_hypergraph::spectral::{dense_eigen, spectral_cluster};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SpectralConfig {
    pub clusters: usize,
    /// Weight of the global hyperedge that connects everything.
    pub connect_weight: f64,
    /// Propagation strength alpha in (0, 1].
    pub coupling: f64,
    pub arnoldi_dim: usize,
    pub seed: u64,
}

impl Default for SpectralConfig {
    fn default() -> Self {
        Self { clusters: 3, connect_weight: 0.01, coupling: 0.6, arnoldi_dim: 40, seed: 7 }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ClusterSummary {
    pub id: usize,
    pub members: Vec<String>,
    pub assets: f64,
    pub liabilities: f64,
    /// (assets - liabilities) / gross balance of the cluster, in [-1, 1].
    pub imbalance: f64,
    pub dominant_category: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SpectralReport {
    pub vertices: usize,
    pub hyperedges: usize,
    /// Second-smallest eigenvalue of the normalized hypergraph Laplacian.
    pub algebraic_connectivity: f64,
    pub clusters: Vec<ClusterSummary>,
    pub contagion_radius: f64,
    pub contagion_stable: bool,
    /// ||(I-M)^-1 s||_1 / ||s||_1 for a run-off-weighted funding shock; `None` if rho >= 1.
    pub amplification: Option<f64>,
    pub hhi_funding: f64,
    pub hhi_assets: f64,
}

fn spec_err<E: std::fmt::Display>(e: E) -> AlmError {
    AlmError::Spectral(e.to_string())
}

fn repricing_bucket(m: f64) -> &'static str {
    if m <= 3.0 {
        "<=3m"
    } else if m <= 12.0 {
        "3-12m"
    } else if m <= 60.0 {
        "1-5y"
    } else {
        ">5y"
    }
}

pub fn build_hypergraph(bs: &BalanceSheet, cfg: &SpectralConfig) -> Result<SpectralHypergraph> {
    let mut b = HypergraphBuilder::new();
    let mut ids: Vec<VertexId> = Vec::new();
    for i in &bs.instruments {
        ids.push(b.add_vertex(i.id.clone()).map_err(spec_err)?);
    }
    let gross: f64 = bs.instruments.iter().map(|i| i.balance).sum::<f64>().max(1e-12);

    let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (k, i) in bs.instruments.iter().enumerate() {
        groups.entry(format!("category:{}", i.category)).or_default().push(k);
        if !i.counterparty.is_empty() {
            groups.entry(format!("cpty:{}", i.counterparty)).or_default().push(k);
        }
        groups.entry(format!("repricing:{}", repricing_bucket(i.repricing_months()))).or_default().push(k);
    }
    for members in groups.values().filter(|m| m.len() >= 2) {
        let w = members.iter().map(|&k| bs.instruments[k].balance).sum::<f64>() / gross;
        let vs: Vec<VertexId> = members.iter().map(|&k| ids[k]).collect();
        b.add_hyperedge(&vs, w.max(1e-6)).map_err(spec_err)?;
    }
    b.add_hyperedge(&ids, cfg.connect_weight.max(1e-6)).map_err(spec_err)?;
    b.build().map_err(spec_err)
}

fn fragility(i: &Instrument) -> f64 {
    match i.side {
        Side::Liability => i.runoff_rate.clamp(0.0, 1.0),
        Side::Asset => {
            let liquidity = match i.hqla {
                Some(HqlaLevel::L1) => 1.0,
                Some(HqlaLevel::L2A) => 0.85,
                Some(HqlaLevel::L2B) => 0.5,
                None => 0.2,
            };
            1.0 - liquidity
        }
    }
}

/// `M = alpha * W * diag(f)`, W row-stochastic from the clique expansion.
pub fn contagion_matrix(bs: &BalanceSheet, hg: &SpectralHypergraph, cfg: &SpectralConfig) -> Result<DMatrix<f64>> {
    let a = clique_expansion_adjacency(hg).map_err(spec_err)?;
    let n = a.nrows();
    let mut m = DMatrix::<f64>::zeros(n, n);
    for r in 0..n {
        let s: f64 = a.row(r).sum();
        if s <= 0.0 {
            continue;
        }
        for c in 0..n {
            m[(r, c)] = cfg.coupling * (a[(r, c)] / s) * fragility(&bs.instruments[c]);
        }
    }
    Ok(m)
}

/// Dominant |eigenvalue| of a general (non-symmetric) matrix via `krylov_ds::Arnoldi`.
pub fn spectral_radius_arnoldi(m: &DMatrix<f64>, dim: usize) -> Result<f64> {
    let n = m.nrows();
    let mut data = Vec::with_capacity(n * n);
    for r in 0..n {
        for c in 0..n {
            data.push(m[(r, c)]);
        }
    }
    let op = DenseMatrix::new(n, data);
    let mut v0: Vec<f64> = (0..n).map(|i| 1.0 + 0.1 * (i as f64 * 1.7).sin()).collect();
    let norm = v0.iter().map(|x| x * x).sum::<f64>().sqrt();
    v0.iter_mut().for_each(|x| *x /= norm);
    let res = Arnoldi::<f64>::new(dim.min(n).max(1), 1e-12).run(&op, &v0).map_err(spec_err)?;
    if res.steps == 0 {
        return Err(AlmError::Spectral("Arnoldi produced no steps".into()));
    }
    Ok(arnoldi_ritz_values(&res).iter().map(|z| z.re.hypot(z.im)).fold(0.0, f64::max))
}

fn hhi(items: impl Iterator<Item = (String, f64)>) -> f64 {
    let mut map: BTreeMap<String, f64> = BTreeMap::new();
    for (k, v) in items {
        *map.entry(k).or_default() += v;
    }
    let total: f64 = map.values().sum();
    if total <= 0.0 {
        return 0.0;
    }
    map.values().map(|v| (v / total).powi(2)).sum()
}

pub fn analyse(bs: &BalanceSheet, cfg: &SpectralConfig) -> Result<SpectralReport> {
    let hg = build_hypergraph(bs, cfg)?;
    let n = hg.num_vertices();

    let lap = dense_normalized_laplacian(&hg).map_err(spec_err)?;
    let eig = dense_eigen(&lap);
    let algebraic_connectivity = if n >= 2 { eig.eigenvalues[1] } else { 0.0 };

    let k = cfg.clusters.clamp(1, n);
    let assign = spectral_cluster(&hg, k, false, cfg.seed).map_err(spec_err)?.assignments;
    let mut clusters = Vec::new();
    for c in 0..k {
        let idx: Vec<usize> = (0..n).filter(|&v| assign[v] == c).collect();
        if idx.is_empty() {
            continue;
        }
        let (mut a, mut l) = (0.0, 0.0);
        let mut cats: BTreeMap<&str, f64> = BTreeMap::new();
        for &v in &idx {
            let i = &bs.instruments[v];
            match i.side {
                Side::Asset => a += i.balance,
                Side::Liability => l += i.balance,
            }
            *cats.entry(i.category.as_str()).or_default() += i.balance;
        }
        let dominant = cats
            .iter()
            .max_by(|x, y| x.1.partial_cmp(y.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(k, _)| k.to_string())
            .unwrap_or_default();
        clusters.push(ClusterSummary {
            id: c,
            members: idx.iter().map(|&v| bs.instruments[v].id.clone()).collect(),
            assets: a,
            liabilities: l,
            imbalance: if a + l > 0.0 { (a - l) / (a + l) } else { 0.0 },
            dominant_category: dominant,
        });
    }

    let m = contagion_matrix(bs, &hg, cfg)?;
    let rho = spectral_radius_arnoldi(&m, cfg.arnoldi_dim)?;
    let stable = rho < 1.0;
    let amplification = if stable {
        let s = DVector::from_iterator(
            n,
            bs.instruments.iter().map(|i| if i.side == Side::Liability { i.balance * i.runoff_rate } else { 0.0 }),
        );
        let s1: f64 = s.iter().sum();
        if s1 > 0.0 {
            let a = DMatrix::<f64>::identity(n, n) - &m;
            a.lu().solve(&s).map(|x| x.iter().map(|v| v.abs()).sum::<f64>() / s1)
        } else {
            None
        }
    } else {
        None
    };

    Ok(SpectralReport {
        vertices: n,
        hyperedges: hg.num_hyperedges(),
        algebraic_connectivity,
        clusters,
        contagion_radius: rho,
        contagion_stable: stable,
        amplification,
        hhi_funding: hhi(bs.instruments.iter().filter(|i| i.side == Side::Liability).map(|i| {
            (if i.counterparty.is_empty() { i.id.clone() } else { i.counterparty.clone() }, i.balance)
        })),
        hhi_assets: hhi(bs.instruments.iter().filter(|i| i.side == Side::Asset).map(|i| (i.category.clone(), i.balance))),
    })
}
