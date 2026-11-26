//! Spectral geometry of the brain: a replica connectome grown from geometry.
//!
//! The idea (after Pang et al. 2023, "Geometric constraints on human brain function",
//! and Atasoy et al. 2016, connectome harmonics) is that brain structure and dynamics
//! live on a low-dimensional spectral scaffold set by the *shape* of the brain:
//!
//! 1. **Geometric eigenmodes.** Region centroids define a point cloud. The symmetric
//!    normalised Laplacian of its Gaussian-kernel graph (plus weak homotopic left-right
//!    bridges) is a discrete stand-in for the Laplace-Beltrami operator of the cortical
//!    surface. Its eigenvectors `psi_k` (eigenvalues `mu_k`) are smooth, hemisphere-aware
//!    standing waves, ordered from coarse to fine. They depend on positions only.
//! 2. **Spectral connectome model.** The weight between regions `i` and `j` is modelled as
//!    `w_ij ~ sum_{k<K} g_k psi_k(i) psi_k(j)`: a connectome described by `K` spectral
//!    coefficients on top of the geometry (`K` is typically 10 to 30, against thousands
//!    of pairs). The coefficients are fitted by (ridge) least squares.
//! 3. **Replica.** Clip at zero, keep the strongest `m` pairs, take conduction lengths
//!    from geometry with one fitted detour ratio. The result is an ordinary `Connectome`,
//!    so everything downstream (Dale's-law E/I units, conduction delays, BPTT, LSTM, RL,
//!    Arnoldi calibration, harmonic analysis) works on it unchanged.
//!
//! Everything is validated **out of sample**: coefficients are fitted on some region pairs
//! and scored on pairs the model never saw (k-fold over pairs), against a distance-only
//! baseline (exponential distance rule) and a density-matched random graph.
//!
//! What this is not: it does not read a cortical surface mesh (it uses parcel centroids),
//! it does not generate tract directions, and a good fit to one connectome says nothing
//! by itself about biology; see the README for the evidence and limits.

use crate::connectome::{Connectome, Region};
use crate::rng::Rng;
use crate::topology::harmonics;
use nalgebra::{DMatrix, DVector};
use spectral_hypergraph::spectral::dense_eigen;
use std::fs;
use std::path::Path;

fn dist3(a: &[f64; 3], b: &[f64; 3]) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

pub fn pearson(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len().min(b.len());
    if n < 2 {
        return f64::NAN;
    }
    let (ma, mb) = (a.iter().sum::<f64>() / n as f64, b.iter().sum::<f64>() / n as f64);
    let (mut sab, mut saa, mut sbb) = (0.0, 0.0, 0.0);
    for i in 0..n {
        let (x, y) = (a[i] - ma, b[i] - mb);
        sab += x * y;
        saa += x * x;
        sbb += y * y;
    }
    if saa <= 0.0 || sbb <= 0.0 {
        return f64::NAN;
    }
    sab / (saa * sbb).sqrt()
}

/// Area under the ROC curve of `score` for the binary `label` (ties get average rank).
pub fn auc(score: &[f64], label: &[bool]) -> f64 {
    let n = score.len();
    let mut idx: Vec<usize> = (0..n).collect();
    idx.sort_by(|&a, &b| score[a].partial_cmp(&score[b]).unwrap_or(std::cmp::Ordering::Equal));
    let mut rank = vec![0.0; n];
    let mut i = 0;
    while i < n {
        let mut j = i;
        while j + 1 < n && score[idx[j + 1]] == score[idx[i]] {
            j += 1;
        }
        let r = 0.5 * (i + j) as f64 + 1.0;
        for t in i..=j {
            rank[idx[t]] = r;
        }
        i = j + 1;
    }
    let np = label.iter().filter(|&&l| l).count() as f64;
    let nn = n as f64 - np;
    if np == 0.0 || nn == 0.0 {
        return f64::NAN;
    }
    let sum_pos: f64 = (0..n).filter(|&i| label[i]).map(|i| rank[i]).sum();
    (sum_pos - np * (np + 1.0) / 2.0) / (np * nn)
}

/// All region pairs `i < j`, in row-major order.
pub fn region_pairs(n: usize) -> Vec<(usize, usize)> {
    let mut v = Vec::with_capacity(n * (n - 1) / 2);
    for i in 0..n {
        for j in (i + 1)..n {
            v.push((i, j));
        }
    }
    v
}

fn has_positions(conn: &Connectome) -> bool {
    conn.regions.iter().any(|r| r.pos != [0.0; 3])
}

/// Left-right homotopic partners: each region is mirrored through the midline (x -> -x)
/// and matched to the nearest region of the other hemisphere; only mutual matches are kept.
pub fn homotopic_partners(conn: &Connectome) -> Vec<Option<usize>> {
    let n = conn.n();
    let mut best: Vec<Option<usize>> = vec![None; n];
    for i in 0..n {
        let h = conn.regions[i].hemi;
        if h != 'L' && h != 'R' {
            continue;
        }
        let p = conn.regions[i].pos;
        let m = [-p[0], p[1], p[2]];
        let mut bd = f64::INFINITY;
        for j in 0..n {
            let hj = conn.regions[j].hemi;
            if hj == h || (hj != 'L' && hj != 'R') {
                continue;
            }
            let d = dist3(&m, &conn.regions[j].pos);
            if d < bd {
                bd = d;
                best[i] = Some(j);
            }
        }
    }
    (0..n).map(|i| best[i].filter(|&j| best[j] == Some(i))).collect()
}

/// Kernel bandwidth from the data: median distance to the 6th nearest region.
pub fn default_sigma(conn: &Connectome) -> f64 {
    let n = conn.n();
    let mut v: Vec<f64> = (0..n)
        .map(|i| {
            let mut d: Vec<f64> = (0..n).filter(|&j| j != i).map(|j| dist3(&conn.regions[i].pos, &conn.regions[j].pos)).collect();
            d.sort_by(|a, b| a.partial_cmp(b).unwrap());
            d[5.min(d.len() - 1)]
        })
        .collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2].max(1e-6)
}

/// Eigenmodes of the brain's geometry (no connectivity involved).
pub struct GeometricModes {
    /// Ascending eigenvalues of the symmetric normalised Laplacian, in [0, 2].
    pub values: Vec<f64>,
    /// `vectors[(region, k)]` is eigenmode k, unit norm.
    pub vectors: DMatrix<f64>,
    pub sigma: f64,
    pub homotopic: f64,
    pub partners: Vec<Option<usize>>,
}

/// Geometric eigenmodes of the region point cloud: Gaussian affinity
/// `exp(-d^2 / 2 sigma^2)` between all regions, plus `homotopic` extra affinity between
/// mirror-image regions, normalised as `I - D^-1/2 A D^-1/2`.
pub fn geometric_modes(conn: &Connectome, sigma: Option<f64>, homotopic: f64) -> Result<GeometricModes, String> {
    if !has_positions(conn) {
        return Err("geometry needs region positions (x, y, z); this connectome has none".into());
    }
    if homotopic < 0.0 {
        return Err("homotopic coupling must be non-negative".into());
    }
    let n = conn.n();
    let sigma = sigma.unwrap_or_else(|| default_sigma(conn));
    if sigma <= 0.0 {
        return Err("sigma must be positive".into());
    }
    let partners = homotopic_partners(conn);
    let mut a = DMatrix::<f64>::zeros(n, n);
    for i in 0..n {
        for j in 0..n {
            if i != j {
                let d = dist3(&conn.regions[i].pos, &conn.regions[j].pos);
                a[(i, j)] = (-d * d / (2.0 * sigma * sigma)).exp();
            }
        }
        if let Some(j) = partners[i] {
            a[(i, j)] += homotopic;
        }
    }
    let deg: Vec<f64> = (0..n).map(|i| (0..n).map(|j| a[(i, j)]).sum::<f64>().max(1e-12)).collect();
    let l = DMatrix::from_fn(n, n, |i, j| {
        let aij = a[(i, j)] / (deg[i] * deg[j]).sqrt();
        if i == j { 1.0 } else { -aij }
    });
    let l = 0.5 * (&l + l.transpose());
    let e = dense_eigen(&l);
    Ok(GeometricModes { values: e.eigenvalues.iter().copied().collect(), vectors: e.eigenvectors, sigma, homotopic, partners })
}

/// `npairs x k` matrix of products `psi_k(i) psi_k(j)`.
fn feature_matrix(modes: &GeometricModes, k: usize, pairs: &[(usize, usize)]) -> DMatrix<f64> {
    DMatrix::from_fn(pairs.len(), k, |r, c| {
        let (i, j) = pairs[r];
        modes.vectors[(i, c)] * modes.vectors[(j, c)]
    })
}

/// Ridge least squares on the given rows: `min |F g - y|^2 + ridge * mean(diag(F'F)) |g|^2`.
fn ridge_fit(f: &DMatrix<f64>, y: &[f64], rows: &[usize], ridge: f64) -> Result<Vec<f64>, String> {
    let k = f.ncols();
    let mut g = DMatrix::<f64>::zeros(k, k);
    let mut b = DVector::<f64>::zeros(k);
    for &r in rows {
        for p in 0..k {
            let fp = f[(r, p)];
            b[p] += fp * y[r];
            for q in p..k {
                g[(p, q)] += fp * f[(r, q)];
            }
        }
    }
    for p in 0..k {
        for q in 0..p {
            g[(p, q)] = g[(q, p)];
        }
    }
    let lam = ridge * (0..k).map(|p| g[(p, p)]).sum::<f64>() / k as f64;
    for p in 0..k {
        g[(p, p)] += lam.max(1e-12);
    }
    g.lu().solve(&b).map(|v| v.iter().copied().collect()).ok_or_else(|| "singular normal equations".to_string())
}

fn predict(f: &DMatrix<f64>, coef: &[f64], rows: impl Iterator<Item = usize>, out: &mut [f64]) {
    for r in rows {
        out[r] = (0..f.ncols()).map(|c| f[(r, c)] * coef[c]).sum();
    }
}

/// Random fold assignment of `npairs` pairs.
pub fn fold_ids(npairs: usize, folds: usize, seed: u64) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..npairs).collect();
    Rng::new(seed ^ 0xF01D).shuffle(&mut idx);
    let mut f = vec![0; npairs];
    for (pos, &i) in idx.iter().enumerate() {
        f[i] = pos % folds;
    }
    f
}

fn oof_predict(f: &DMatrix<f64>, y: &[f64], fold: &[usize], folds: usize, ridge: f64) -> Result<Vec<f64>, String> {
    let mut out = vec![0.0; y.len()];
    for t in 0..folds {
        let train: Vec<usize> = (0..y.len()).filter(|&r| fold[r] != t).collect();
        let coef = ridge_fit(f, y, &train, ridge)?;
        predict(f, &coef, (0..y.len()).filter(|&r| fold[r] == t), &mut out);
    }
    Ok(out)
}

fn weight_vector(conn: &Connectome, pairs: &[(usize, usize)]) -> Vec<f64> {
    pairs.iter().map(|&(i, j)| conn.weight(i, j)).collect()
}

/// Detour ratio: median of (fibre length / straight-line distance) over the target's edges.
fn detour_ratio(conn: &Connectome) -> f64 {
    let mut r = Vec::new();
    for i in 0..conn.n() {
        for j in (i + 1)..conn.n() {
            if conn.weight(i, j) > 0.0 {
                let d = dist3(&conn.regions[i].pos, &conn.regions[j].pos);
                if d > 1e-9 && conn.length(i, j) > 0.0 {
                    r.push(conn.length(i, j) / d);
                }
            }
        }
    }
    if r.is_empty() {
        return 1.2;
    }
    r.sort_by(|a, b| a.partial_cmp(b).unwrap());
    r[r.len() / 2]
}

/// Turn pair scores into a connectome: clip at zero, keep the top `m`, lengths from geometry.
pub fn replica_from_scores(target: &Connectome, pairs: &[(usize, usize)], scores: &[f64], m: usize, detour: f64, source: &str) -> Result<Connectome, String> {
    let n = target.n();
    let mut order: Vec<usize> = (0..pairs.len()).filter(|&r| scores[r] > 0.0).collect();
    order.sort_by(|&a, &b| scores[b].partial_cmp(&scores[a]).unwrap_or(std::cmp::Ordering::Equal));
    order.truncate(m);
    let mut w = vec![0.0; n * n];
    for &r in &order {
        let (i, j) = pairs[r];
        w[i * n + j] = scores[r];
        w[j * n + i] = scores[r];
    }
    let mut len = vec![0.0; n * n];
    for &(i, j) in pairs {
        let l = (detour * dist3(&target.regions[i].pos, &target.regions[j].pos)).max(1.0);
        len[i * n + j] = l;
        len[j * n + i] = l;
    }
    let regions: Vec<Region> = target.regions.clone();
    Connectome::new(regions, w, len, source)
}

// ---------------------------------------------------------------------------------------
// The spectral brain model
// ---------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct SpectralBrainConfig {
    /// Number of geometric eigenmodes (including the constant mode).
    pub k: usize,
    /// Kernel bandwidth in mm; `None` = data-driven (`default_sigma`).
    pub sigma: Option<f64>,
    /// Extra affinity between mirror-image regions in the geometric graph.
    pub homotopic: f64,
    /// Relative ridge penalty on the spectral coefficients.
    pub ridge: f64,
}

impl Default for SpectralBrainConfig {
    fn default() -> Self {
        SpectralBrainConfig { k: 16, sigma: None, homotopic: 1.0, ridge: 1e-6 }
    }
}

/// A connectome described by geometry plus `k` spectral coefficients, and the replica it generates.
pub struct SpectralBrain {
    pub cfg: SpectralBrainConfig,
    pub modes: GeometricModes,
    /// Spectral filter coefficients `g_k`, one per mode.
    pub coeffs: Vec<f64>,
    pub detour: f64,
    pub n_edges: usize,
    pub pairs: Vec<(usize, usize)>,
    /// In-sample predicted weight for every pair.
    pub scores: Vec<f64>,
    pub replica: Connectome,
}

impl SpectralBrain {
    /// Fit the coefficients on all pairs of `target` and generate the replica.
    pub fn fit(target: &Connectome, cfg: SpectralBrainConfig) -> Result<Self, String> {
        let n = target.n();
        if cfg.k == 0 || cfg.k > n {
            return Err(format!("k must be in 1..={n}"));
        }
        let modes = geometric_modes(target, cfg.sigma, cfg.homotopic)?;
        let pairs = region_pairs(n);
        let y = weight_vector(target, &pairs);
        let f = feature_matrix(&modes, cfg.k, &pairs);
        let rows: Vec<usize> = (0..pairs.len()).collect();
        let coeffs = ridge_fit(&f, &y, &rows, cfg.ridge)?;
        let mut scores = vec![0.0; pairs.len()];
        predict(&f, &coeffs, 0..pairs.len(), &mut scores);
        let n_edges = target.num_edges();
        let detour = detour_ratio(target);
        let replica = replica_from_scores(target, &pairs, &scores, n_edges, detour, &format!("spectral replica (K={}, from geometry of {})", cfg.k, target.source))?;
        Ok(SpectralBrain { cfg, modes, coeffs, detour, n_edges, pairs, scores, replica })
    }

    /// Number of fitted numbers that define the replica beyond the geometry itself:
    /// K coefficients, the edge count and the detour ratio.
    pub fn num_fitted_parameters(&self) -> usize {
        self.cfg.k + 2
    }

    /// Write the replica and the spectral description into `dir`:
    /// `weights.csv`, `lengths.csv`, `labels.csv` (for `--csv/--lengths/--labels`),
    /// TVB-style `weights.txt`, `tract_lengths.txt`, `centres.txt` (for `--tvb`),
    /// `spectral_coefficients.csv` and `geometry_modes.csv`.
    pub fn export(&self, dir: &Path) -> Result<(), String> {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let c = &self.replica;
        let n = c.n();
        let matrix = |m: &[f64], sep: &str| -> String {
            (0..n).map(|i| (0..n).map(|j| format!("{:.8}", m[i * n + j])).collect::<Vec<_>>().join(sep)).collect::<Vec<_>>().join("\n") + "\n"
        };
        let w = fs::write(dir.join("weights.csv"), matrix(c.weights(), ","));
        let l = fs::write(dir.join("lengths.csv"), matrix(c.lengths(), ","));
        let w2 = fs::write(dir.join("weights.txt"), matrix(c.weights(), " "));
        let l2 = fs::write(dir.join("tract_lengths.txt"), matrix(c.lengths(), " "));
        for r in [w, l, w2, l2] {
            r.map_err(|e| e.to_string())?;
        }
        let labels: String = c
            .regions
            .iter()
            .map(|r| format!("{},{},{},{:.4},{:.4},{:.4}\n", r.name, r.hemi, r.system, r.pos[0], r.pos[1], r.pos[2]))
            .collect();
        fs::write(dir.join("labels.csv"), labels).map_err(|e| e.to_string())?;
        let centres: String = c.regions.iter().map(|r| format!("{} {:.4} {:.4} {:.4}\n", r.name, r.pos[0], r.pos[1], r.pos[2])).collect();
        fs::write(dir.join("centres.txt"), centres).map_err(|e| e.to_string())?;
        let mut co = String::from("k,mu_k,g_k\n");
        for k in 0..self.cfg.k {
            co += &format!("{},{:.6},{:.8}\n", k, self.modes.values[k], self.coeffs[k]);
        }
        fs::write(dir.join("spectral_coefficients.csv"), co).map_err(|e| e.to_string())?;
        let mut gm = String::from("region");
        for k in 0..self.cfg.k {
            gm += &format!(",psi_{k}");
        }
        gm.push('\n');
        for i in 0..n {
            gm += &c.regions[i].name;
            for k in 0..self.cfg.k {
                gm += &format!(",{:.6}", self.modes.vectors[(i, k)]);
            }
            gm.push('\n');
        }
        fs::write(dir.join("geometry_modes.csv"), gm).map_err(|e| e.to_string())
    }
}

// ---------------------------------------------------------------------------------------
// Out-of-sample evaluation
// ---------------------------------------------------------------------------------------

/// Held-out predictions of every pair's weight (each pair predicted by a model that never saw it).
pub struct HeldOut {
    pub pairs: Vec<(usize, usize)>,
    pub scores: Vec<f64>,
    pub pearson: f64,
    pub auc: f64,
}

fn held_out_summary(target: &Connectome, pairs: Vec<(usize, usize)>, scores: Vec<f64>) -> HeldOut {
    let y = weight_vector(target, &pairs);
    let lab: Vec<bool> = y.iter().map(|&v| v > 0.0).collect();
    HeldOut { pearson: pearson(&scores, &y), auc: auc(&scores, &lab), pairs, scores }
}

/// k-fold held-out prediction by the spectral model with `cfg.k` modes.
pub fn held_out_spectral(target: &Connectome, cfg: &SpectralBrainConfig, folds: usize, seed: u64) -> Result<HeldOut, String> {
    let modes = geometric_modes(target, cfg.sigma, cfg.homotopic)?;
    held_out_spectral_with(target, &modes, cfg.k, cfg.ridge, folds, seed)
}

fn held_out_spectral_with(target: &Connectome, modes: &GeometricModes, k: usize, ridge: f64, folds: usize, seed: u64) -> Result<HeldOut, String> {
    let pairs = region_pairs(target.n());
    if folds < 2 || folds > pairs.len() {
        return Err("folds must be between 2 and the number of region pairs".into());
    }
    let y = weight_vector(target, &pairs);
    let f = feature_matrix(modes, k, &pairs);
    let fold = fold_ids(pairs.len(), folds, seed);
    let scores = oof_predict(&f, &y, &fold, folds, ridge)?;
    Ok(held_out_summary(target, pairs, scores))
}

/// Distance-only baseline: `w ~ a + b exp(-d / lambda)`, lambda chosen on all pairs
/// (one nonlinear parameter), `a, b` fitted out of sample.
pub fn held_out_distance_rule(target: &Connectome, folds: usize, seed: u64) -> Result<(HeldOut, f64), String> {
    let pairs = region_pairs(target.n());
    let y = weight_vector(target, &pairs);
    let d: Vec<f64> = pairs.iter().map(|&(i, j)| dist3(&target.regions[i].pos, &target.regions[j].pos)).collect();
    let dmax = d.iter().cloned().fold(0.0, f64::max).max(1.0);
    let rows: Vec<usize> = (0..pairs.len()).collect();
    let build = |lam: f64| DMatrix::from_fn(pairs.len(), 2, |r, c| if c == 0 { 1.0 } else { (-d[r] / lam).exp() });
    let (mut best_lam, mut best_r) = (dmax / 4.0, f64::NEG_INFINITY);
    for s in 0..60 {
        // length scales from 2 mm up to several times the brain's extent
        let lam = 2.0 * (dmax / 2.0).powf(s as f64 / 40.0) * 3.0f64.powf(s as f64 / 60.0);
        let f = build(lam);
        if let Ok(c) = ridge_fit(&f, &y, &rows, 1e-9) {
            let mut p = vec![0.0; y.len()];
            predict(&f, &c, 0..y.len(), &mut p);
            let r = pearson(&p, &y);
            if r.is_finite() && r > best_r {
                best_r = r;
                best_lam = lam;
            }
        }
    }
    let f = build(best_lam);
    let fold = fold_ids(pairs.len(), folds, seed);
    let scores = oof_predict(&f, &y, &fold, folds, 1e-9)?;
    Ok((held_out_summary(target, pairs, scores), best_lam))
}

/// Build a replica from held-out (or any) pair scores, matching the target's edge count.
pub fn replica_from_held_out(target: &Connectome, h: &HeldOut, label: &str) -> Result<Connectome, String> {
    replica_from_scores(target, &h.pairs, &h.scores, target.num_edges(), detour_ratio(target), label)
}

#[derive(Clone, Debug)]
pub struct KSweepRow {
    pub k: usize,
    pub mu_max: f64,
    /// Share of the weight matrix's energy inside the span of the first k modes (projection, no fitting).
    pub variance_explained: f64,
    pub heldout_pearson: f64,
    pub heldout_auc: f64,
}

/// Held-out accuracy as a function of the number of geometric eigenmodes.
pub fn k_sweep(target: &Connectome, ks: &[usize], cfg: &SpectralBrainConfig, folds: usize, seed: u64) -> Result<Vec<KSweepRow>, String> {
    let n = target.n();
    let modes = geometric_modes(target, cfg.sigma, cfg.homotopic)?;
    let w = DMatrix::from_fn(n, n, |i, j| target.weight(i, j));
    let total = w.norm_squared().max(1e-300);
    let mut rows = Vec::new();
    for &k in ks.iter().filter(|&&k| k >= 1 && k <= n) {
        let psi = modes.vectors.columns(0, k);
        let proj = psi.transpose() * &w * &psi;
        let h = held_out_spectral_with(target, &modes, k, cfg.ridge, folds, seed)?;
        rows.push(KSweepRow { k, mu_max: modes.values[k - 1], variance_explained: proj.norm_squared() / total, heldout_pearson: h.pearson, heldout_auc: h.auc });
    }
    Ok(rows)
}

/// Smallest K whose held-out correlation is within `tol` of the best in the sweep.
pub fn choose_k(rows: &[KSweepRow], tol: f64) -> Option<usize> {
    let best = rows.iter().map(|r| r.heldout_pearson).filter(|x| x.is_finite()).fold(f64::NEG_INFINITY, f64::max);
    rows.iter().find(|r| r.heldout_pearson.is_finite() && r.heldout_pearson >= best - tol).map(|r| r.k)
}

// ---------------------------------------------------------------------------------------
// Fidelity of a replica to its target
// ---------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Fidelity {
    pub edges: usize,
    /// Pearson correlation of weights over all region pairs.
    pub weight_corr: f64,
    /// |E & E'| / |E | E'|
    pub edge_jaccard: f64,
    pub degree_corr: f64,
    pub strength_corr: f64,
    /// RMS difference between the two sorted normalised-Laplacian spectra.
    pub spectrum_rmse: f64,
    pub fiedler: f64,
    /// Overlap of the lowest-k connectome harmonic subspaces, in [0, 1] (1 = identical).
    pub low_subspace_overlap: f64,
    /// Correlation of fibre lengths over the target's edges.
    pub length_corr: f64,
}

pub fn fidelity(target: &Connectome, rep: &Connectome, k_low: usize) -> Fidelity {
    let n = target.n();
    let pairs = region_pairs(n);
    let (wt, wr) = (weight_vector(target, &pairs), weight_vector(rep, &pairs));
    let (mut inter, mut union) = (0usize, 0usize);
    for r in 0..pairs.len() {
        let (a, b) = (wt[r] > 0.0, wr[r] > 0.0);
        inter += (a && b) as usize;
        union += (a || b) as usize;
    }
    let deg_t: Vec<f64> = (0..n).map(|i| target.degree(i) as f64).collect();
    let deg_r: Vec<f64> = (0..n).map(|i| rep.degree(i) as f64).collect();
    let st: Vec<f64> = (0..n).map(|i| target.strength(i)).collect();
    let sr: Vec<f64> = (0..n).map(|i| rep.strength(i)).collect();
    let (ht, hr) = (harmonics(target), harmonics(rep));
    let spec = (ht.values.iter().zip(&hr.values).map(|(a, b)| (a - b).powi(2)).sum::<f64>() / n as f64).sqrt();
    let k = k_low.clamp(1, n - 1);
    let (a, b) = (ht.vectors.columns(0, k), hr.vectors.columns(0, k));
    let overlap = (a.transpose() * b).norm_squared() / k as f64;
    let (mut lt, mut lr) = (Vec::new(), Vec::new());
    for &(i, j) in &pairs {
        if target.weight(i, j) > 0.0 {
            lt.push(target.length(i, j));
            lr.push(rep.length(i, j));
        }
    }
    Fidelity {
        edges: rep.num_edges(),
        weight_corr: pearson(&wt, &wr),
        edge_jaccard: if union > 0 { inter as f64 / union as f64 } else { f64::NAN },
        degree_corr: pearson(&deg_t, &deg_r),
        strength_corr: pearson(&st, &sr),
        spectrum_rmse: spec,
        fiedler: hr.values.get(1).copied().unwrap_or(0.0),
        low_subspace_overlap: overlap,
        length_corr: pearson(&lt, &lr),
    }
}

/// How much of the 3D layout of the regions is encoded by the wiring alone: regress each
/// spatial axis on the `m` lowest non-constant connectome harmonics (random-walk scaling,
/// with intercept). Returns R^2 for x, y, z and the variance-weighted overall R^2.
pub fn embedding_r2(conn: &Connectome, m: usize) -> [f64; 4] {
    let n = conn.n();
    let m = m.min(n - 2).max(1);
    let h = harmonics(conn);
    let s: Vec<f64> = (0..n).map(|i| conn.strength(i).max(1e-12).sqrt()).collect();
    let x = DMatrix::from_fn(n, m + 1, |i, c| if c == 0 { 1.0 } else { h.vectors[(i, c)] / s[i] });
    let xtx = x.transpose() * &x + DMatrix::identity(m + 1, m + 1) * 1e-12;
    let (mut rss_all, mut tss_all) = (0.0, 0.0);
    let mut out = [f64::NAN; 4];
    for a in 0..3 {
        let y = DVector::from_fn(n, |i, _| conn.regions[i].pos[a]);
        let mean = y.mean();
        let tss: f64 = y.iter().map(|v| (v - mean).powi(2)).sum();
        let rss = match xtx.clone().lu().solve(&(x.transpose() * &y)) {
            Some(beta) => (&y - &x * beta).norm_squared(),
            None => tss,
        };
        out[a] = if tss > 0.0 { 1.0 - rss / tss } else { f64::NAN };
        rss_all += rss;
        tss_all += tss;
    }
    out[3] = if tss_all > 0.0 { 1.0 - rss_all / tss_all } else { f64::NAN };
    out
}
