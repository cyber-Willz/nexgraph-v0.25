//! Tests for the spectral-geometry brain (src/geometry.rs).

use hcrnn::geometry::{self, SpectralBrain, SpectralBrainConfig};
use hcrnn::network::default_roles;
use hcrnn::{Connectome, ConnectomeRnn, RnnConfig};
use std::path::PathBuf;

fn conn() -> Connectome {
    Connectome::synthetic_human(20, 0.25, 3).unwrap().log_compressed()
}

fn scratch(name: &str) -> PathBuf {
    let p = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&p);
    p
}

#[test]
fn geometric_modes_are_an_orthonormal_ascending_eigenbasis() {
    let c = conn();
    let m = geometry::geometric_modes(&c, None, 1.0).unwrap();
    let n = c.n();
    assert!(m.values[0].abs() < 1e-9, "lowest eigenvalue {}", m.values[0]);
    assert!(m.values.windows(2).all(|w| w[0] <= w[1] + 1e-12));
    assert!(m.values.iter().all(|&v| v > -1e-9 && v < 2.0 + 1e-9));
    let gram = m.vectors.transpose() * &m.vectors;
    for i in 0..n {
        for j in 0..n {
            let want = if i == j { 1.0 } else { 0.0 };
            assert!((gram[(i, j)] - want).abs() < 1e-8);
        }
    }
    // the constant mode of the normalised Laplacian is positive everywhere
    let s0 = m.vectors[(0, 0)].signum();
    assert!((0..n).all(|i| m.vectors[(i, 0)] * s0 > 0.0));
}

#[test]
fn geometry_depends_on_positions_only() {
    // Two connectomes with the same positions but different wiring give identical modes.
    let a = conn();
    let b = a.density_matched_random(&mut hcrnn::rng::Rng::new(9));
    let (ma, mb) = (geometry::geometric_modes(&a, None, 1.0).unwrap(), geometry::geometric_modes(&b, None, 1.0).unwrap());
    for k in 0..a.n() {
        assert!((ma.values[k] - mb.values[k]).abs() < 1e-12);
    }
}

#[test]
fn homotopic_partners_are_mutual_mirror_pairs() {
    let c = conn();
    let p = geometry::homotopic_partners(&c);
    let per = c.n() / 2;
    for i in 0..c.n() {
        let j = p[i].expect("synthetic brain is mirror symmetric");
        assert_eq!(p[j], Some(i));
        assert_ne!(c.regions[i].hemi, c.regions[j].hemi);
        assert_eq!(j, (i + per) % c.n());
    }
}

#[test]
fn recovers_a_planted_spectral_connectome_out_of_sample() {
    // Plant w_ij = relu(sum_k g_k psi_k(i) psi_k(j)) on the real geometry and check that
    // the held-out fit finds it again.
    let base = conn();
    let n = base.n();
    let modes = geometry::geometric_modes(&base, None, 1.0).unwrap();
    let mut g = vec![0.0; 10];
    g[0] = 6.0 * n as f64;
    g[1] = 3.0 * n as f64;
    g[3] = -2.0 * n as f64;
    g[6] = 1.5 * n as f64;
    let mut w = vec![0.0; n * n];
    for i in 0..n {
        for j in 0..n {
            if i != j {
                let v: f64 = (0..g.len()).map(|k| g[k] * modes.vectors[(i, k)] * modes.vectors[(j, k)]).sum();
                w[i * n + j] = v.max(0.0);
            }
        }
    }
    let target = Connectome::new(base.regions.clone(), w, base.lengths().to_vec(), "planted").unwrap();
    let cfg = SpectralBrainConfig { k: 10, ..Default::default() };
    let h = geometry::held_out_spectral(&target, &cfg, 5, 1).unwrap();
    assert!(h.pearson > 0.95, "held-out r {}", h.pearson);
    assert!(h.auc > 0.95, "held-out auc {}", h.auc);
    // far fewer modes than needed must do clearly worse
    let h1 = geometry::held_out_spectral(&target, &SpectralBrainConfig { k: 1, ..Default::default() }, 5, 1).unwrap();
    assert!(h1.pearson < h.pearson - 0.1);
}

#[test]
fn replica_beats_random_out_of_sample_and_matches_edge_count() {
    let c = conn();
    let cfg = SpectralBrainConfig { k: 16, ..Default::default() };
    let b = SpectralBrain::fit(&c, cfg.clone()).unwrap();
    assert_eq!(b.replica.num_edges(), c.num_edges());
    let ho = geometry::held_out_spectral(&c, &cfg, 5, 1).unwrap();
    let rep = geometry::replica_from_held_out(&c, &ho, "ho").unwrap();
    let rnd = c.density_matched_random(&mut hcrnn::rng::Rng::new(5));
    let (fs, fr) = (geometry::fidelity(&c, &rep, 6), geometry::fidelity(&c, &rnd, 6));
    assert!(fs.weight_corr > fr.weight_corr + 0.3, "{} vs {}", fs.weight_corr, fr.weight_corr);
    assert!(fs.low_subspace_overlap > fr.low_subspace_overlap + 0.2);
    assert!(ho.auc > 0.7, "auc {}", ho.auc);
}

#[test]
fn fidelity_of_a_connectome_to_itself_is_perfect() {
    let c = conn();
    let f = geometry::fidelity(&c, &c, 6);
    assert!((f.weight_corr - 1.0).abs() < 1e-9);
    assert!((f.edge_jaccard - 1.0).abs() < 1e-12);
    assert!(f.spectrum_rmse < 1e-9);
    assert!((f.low_subspace_overlap - 1.0).abs() < 1e-6);
}

#[test]
fn auc_and_pearson_basics() {
    assert!((geometry::auc(&[0.1, 0.2, 0.8, 0.9], &[false, false, true, true]) - 1.0).abs() < 1e-12);
    assert!((geometry::auc(&[0.9, 0.8, 0.2, 0.1], &[false, false, true, true])).abs() < 1e-12);
    assert!((geometry::auc(&[1.0, 1.0, 1.0, 1.0], &[false, true, false, true]) - 0.5).abs() < 1e-12);
    assert!((geometry::pearson(&[1.0, 2.0, 3.0], &[2.0, 4.0, 6.0]) - 1.0).abs() < 1e-12);
}

#[test]
fn wiring_encodes_layout_for_geometric_graphs_not_random_ones() {
    let c = conn();
    let real = geometry::embedding_r2(&c, 3);
    let rnd = geometry::embedding_r2(&c.density_matched_random(&mut hcrnn::rng::Rng::new(2)), 3);
    assert!(real[3] > 0.5, "real {:?}", real);
    assert!(rnd[3] < 0.2, "random {:?}", rnd);
}

#[test]
fn replica_drives_the_dales_law_delayed_rnn_unchanged() {
    let c = conn();
    let b = SpectralBrain::fit(&c, SpectralBrainConfig { k: 12, ..Default::default() }).unwrap();
    let (inp, out) = default_roles(&b.replica).unwrap();
    let cfg = RnnConfig { input_regions: inp, output_regions: out, seed: 1, ..Default::default() };
    let net = ConnectomeRnn::new(&b.replica, cfg).unwrap();
    net.check_constraints().unwrap();
    assert!(net.max_delay() >= 1);
    assert!(net.weight_radius().is_finite());
}

#[test]
fn export_round_trips_through_both_loaders() {
    let c = conn();
    let b = SpectralBrain::fit(&c, SpectralBrainConfig { k: 12, ..Default::default() }).unwrap();
    let dir = scratch("spectral_replica_export");
    b.export(&dir).unwrap();
    let t = Connectome::from_tvb_dir(&dir).unwrap();
    assert_eq!(t.n(), c.n());
    assert_eq!(t.num_edges(), b.replica.num_edges());
    let cs = Connectome::from_csv(&dir.join("weights.csv"), Some(&dir.join("lengths.csv")), Some(&dir.join("labels.csv"))).unwrap();
    assert_eq!(cs.n(), c.n());
    assert_eq!(cs.num_edges(), b.replica.num_edges());
    assert_eq!(cs.regions[0].system, b.replica.regions[0].system);
    let coef = std::fs::read_to_string(dir.join("spectral_coefficients.csv")).unwrap();
    assert_eq!(coef.lines().count(), 1 + 12);
}

#[test]
fn rejects_missing_geometry_and_bad_arguments() {
    let c = conn();
    let flat: Vec<_> = c.regions.iter().map(|r| hcrnn::Region { pos: [0.0; 3], ..r.clone() }).collect();
    let nogeo = Connectome::new(flat, c.weights().to_vec(), c.lengths().to_vec(), "x").unwrap();
    assert!(geometry::geometric_modes(&nogeo, None, 1.0).is_err());
    assert!(SpectralBrain::fit(&c, SpectralBrainConfig { k: 0, ..Default::default() }).is_err());
    assert!(SpectralBrain::fit(&c, SpectralBrainConfig { k: c.n() + 1, ..Default::default() }).is_err());
    assert!(geometry::held_out_spectral(&c, &SpectralBrainConfig::default(), 1, 1).is_err());
}
