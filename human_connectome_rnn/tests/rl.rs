use hcrnn::network::default_roles;
use hcrnn::rl::{policy_gradient, train_rl, Baseline, Reward, RlConfig};
use hcrnn::rng::Rng;
use hcrnn::train::{evaluate, make_trials};
use hcrnn::{Connectome, ConnectomeLstm, ConnectomeRnn, LstmConfig, MaskMode, MemoryGuidedTask, RnnConfig, SequenceModel, TrainConfig};

fn conn() -> Connectome {
    Connectome::synthetic_human(10, 0.3, 3).unwrap().log_compressed()
}
fn cfg(c: &Connectome) -> RnnConfig {
    let (i, o) = default_roles(c).unwrap();
    RnnConfig { mode: MaskMode::Connectome, input_regions: i, output_regions: o, seed: 5, ..Default::default() }
}
fn small_task() -> MemoryGuidedTask {
    MemoryGuidedTask { fix_steps: 4, cue_steps: 6, delay_min: 8, delay_max: 12, resp_steps: 10, grace: 2, cue_amp: 1.0 }
}

fn cosine(a: &[f64], b: &[f64]) -> f64 {
    let dot: f64 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na = a.iter().map(|x| x * x).sum::<f64>().sqrt();
    let nb = b.iter().map(|x| x * x).sum::<f64>().sqrt();
    dot / (na * nb)
}

/// With the dense reward -m|a-u|^2 the expected REINFORCE gradient is the supervised MSE gradient.
/// Averaging many noise draws on a fixed batch must therefore align with the exact BPTT gradient.
#[test]
fn dense_policy_gradient_matches_supervised_gradient_in_expectation() {
    let c = conn();
    let net = ConnectomeRnn::new(&c, cfg(&c)).unwrap();
    let trials = make_trials(&small_task(), 6, 11);
    let n = net.params().len();
    let mut g_sup = vec![0.0; n];
    net.batch_loss_and_grad(&trials, &mut g_sup, 1);
    for baseline in [Baseline::MeanAction, Baseline::Phase] {
        let rl = RlConfig { reward: Reward::Dense, baseline, normalize_adv: false, ..Default::default() };
        let mut rng = Rng::new(7);
        let mut acc = vec![0.0; n];
        let mut g = vec![0.0; n];
        let draws = 3000;
        for _ in 0..draws {
            policy_gradient(&net, &trials, &rl, 0.3, &mut rng, &mut g, 1);
            for i in 0..n {
                acc[i] += g[i] / draws as f64;
            }
        }
        let cs = cosine(&acc, &g_sup);
        let ratio = acc.iter().map(|x| x * x).sum::<f64>().sqrt() / g_sup.iter().map(|x| x * x).sum::<f64>().sqrt();
        eprintln!("{baseline:?}: cosine {cs:.4} norm ratio {ratio:.3}");
        let floor = if baseline == Baseline::MeanAction { 0.97 } else { 0.9 };
        assert!(cs > floor, "{baseline:?}: policy gradient not aligned with supervised gradient (cos {cs})");
    }
}

#[test]
fn rl_training_learns_and_keeps_constraints_rnn() {
    let c = conn();
    let mut net = ConnectomeRnn::new(&c, cfg(&c)).unwrap();
    let task = small_task();
    let eval = make_trials(&task, 48, 99);
    let before = evaluate(&net, &eval);
    let tcfg = TrainConfig { steps: 400, batch: 24, lr: 5e-3, log_every: 200, eval_trials: 16, threads: 2, ..Default::default() };
    let rl = RlConfig { baseline: Baseline::MeanAction, ..Default::default() };
    train_rl(&mut net, &task, &tcfg, &rl, |_| {});
    net.check_constraints().unwrap();
    let after = evaluate(&net, &eval);
    eprintln!("rnn dense/mean-action: err {:.1} -> {:.1}", before.angular_error_deg, after.angular_error_deg);
    assert!(after.loss < before.loss * 0.7, "RL did not reduce loss: {} -> {}", before.loss, after.loss);
}

#[test]
fn rl_training_runs_on_lstm_sparse_and_hybrid() {
    let c = conn();
    let task = small_task();
    let mut net = ConnectomeLstm::new(&c, cfg(&c), LstmConfig::default()).unwrap();
    let tcfg = TrainConfig { steps: 30, batch: 8, lr: 5e-3, log_every: 15, eval_trials: 8, threads: 2, ..Default::default() };
    let rl = RlConfig { reward: Reward::Sparse, pretrain: 10, ..Default::default() };
    let rep = train_rl(&mut net, &task, &tcfg, &rl, |_| {});
    net.check_constraints().unwrap();
    assert!(rep.final_eval.loss.is_finite());
    assert!(net.params().iter().all(|p| p.is_finite()));
}
