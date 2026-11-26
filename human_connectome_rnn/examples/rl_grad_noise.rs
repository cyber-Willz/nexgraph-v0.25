//! How noisy is the policy gradient compared with the exact BPTT gradient?
//!
//! Trains a model supervised for a number of steps, then, at that fixed point, draws many batches and
//! compares the single-batch REINFORCE gradient with the supervised gradient of the same batch:
//! cosine similarity, and the batch size an RL estimate needs to reach a given alignment (the
//! averaged-gradient cosine as the number of independent batches grows).
//!
//! cargo run --release --example rl_grad_noise -- [steps_before_probe] [per_hemi]

use hcrnn::network::default_roles;
use hcrnn::rl::{policy_gradient, Baseline, Reward, RlConfig};
use hcrnn::rng::Rng;
use hcrnn::train::{make_trials, train};
use hcrnn::{Connectome, ConnectomeRnn, MaskMode, MemoryGuidedTask, RnnConfig, SequenceModel, TrainConfig};

fn cosine(a: &[f64], b: &[f64]) -> f64 {
    let dot: f64 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    dot / (a.iter().map(|x| x * x).sum::<f64>().sqrt() * b.iter().map(|x| x * x).sum::<f64>().sqrt())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let probe_after: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let per_hemi: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(33);
    let conn = Connectome::synthetic_human(per_hemi, 0.25, 1).unwrap().log_compressed();
    let (i, o) = default_roles(&conn).unwrap();
    let mut net = ConnectomeRnn::new(&conn, RnnConfig { mode: MaskMode::Connectome, input_regions: i, output_regions: o, seed: 1, ..Default::default() }).unwrap();
    let task = MemoryGuidedTask::default();
    if probe_after > 0 {
        let cfg = TrainConfig { steps: probe_after, log_every: probe_after, threads: 2, ..Default::default() };
        let rep = train(&mut net, &task, &cfg, |_| {});
        println!("supervised pre-training: {} steps, held-out error {:.1} deg", probe_after, rep.final_eval.angular_error_deg);
    } else {
        println!("probing at initialisation");
    }
    let n = net.params().len();
    let batch = 24;
    let draws = 40;
    let mut rng = Rng::new(123);
    let mut noise = Rng::new(456);
    println!("{:<34} {:>16} {:>22}", "estimator (batch 24)", "cos to BPTT (m+-sd)", "cos after averaging 8 batches");
    for (name, reward, baseline) in [
        ("dense, mean-action baseline", Reward::Dense, Baseline::MeanAction),
        ("dense, phase baseline", Reward::Dense, Baseline::Phase),
        ("sparse, phase baseline", Reward::Sparse, Baseline::Phase),
    ] {
        let rl = RlConfig { reward, baseline, normalize_adv: false, ..Default::default() };
        let mut cs = Vec::new();
        let mut avg_cs = Vec::new();
        let mut g_sup_acc = vec![0.0; n];
        let mut g_rl_acc = vec![0.0; n];
        for d in 0..draws {
            let trials = make_trials(&task, batch, rng.next_u64());
            let mut g_sup = vec![0.0; n];
            net.batch_loss_and_grad(&trials, &mut g_sup, 2);
            let mut g = vec![0.0; n];
            policy_gradient(&net, &trials, &rl, 0.2, &mut noise, &mut g, 2);
            cs.push(cosine(&g, &g_sup));
            for k in 0..n {
                g_sup_acc[k] += g_sup[k];
                g_rl_acc[k] += g[k];
            }
            if (d + 1) % 8 == 0 {
                avg_cs.push(cosine(&g_rl_acc, &g_sup_acc));
                g_sup_acc.iter_mut().for_each(|x| *x = 0.0);
                g_rl_acc.iter_mut().for_each(|x| *x = 0.0);
            }
        }
        let m = cs.iter().sum::<f64>() / cs.len() as f64;
        let sd = (cs.iter().map(|c| (c - m).powi(2)).sum::<f64>() / (cs.len() as f64 - 1.0)).sqrt();
        let am = avg_cs.iter().sum::<f64>() / avg_cs.len() as f64;
        println!("{:<34} {:>9.3} +- {:<5.3} {:>16.3}", name, m, sd, am);
    }
}
