//! Reinforcement learning (policy gradient) on the connectome models.
//!
//! The network is a stochastic *policy*: its output `mu_t` (2 numbers, the reported direction) is the
//! mean of a Gaussian, and the action actually emitted is `a_t = mu_t + sigma * eps_t`. The network is
//! never shown the target; it only receives a scalar reward, and it is trained with REINFORCE
//! (score-function gradient), not by differentiating the reward.
//!
//! Reuse of the exact BPTT. The policy-gradient loss is `-sum_t A_t log pi(a_t | h_t)`, whose gradient
//! with respect to the network output is `dL/dmu_t = -A_t * eps_t / sigma`. The models already
//! back-propagate `dL/dy_t = 2 m_t (y_t - target_t) / (sum m * n_out)` through time (and through the
//! conduction delays, the Dale's-law weight map and the LSTM gates). We therefore feed them a
//! *surrogate trial* whose targets are chosen so that this expression equals the policy gradient:
//! `target_t = mu_t + A_t * eps_t * n_out / (2 sigma)` with `m_t = 1` on the steps that carry a
//! learning signal and 0 elsewhere. No model code changes, so Dale's law, the wiring mask and the
//! radius cap hold by construction and `check_constraints()` stays valid.
//!
//! Credit assignment. In this task the emitted action does not influence the next observation or the
//! hidden state, so an action only affects the reward at its own step (dense reward) or the terminal
//! decoded-direction reward (sparse reward, which only depends on the last `decode_window` actions).
//! The exact policy-gradient return is therefore the immediate / terminal reward, not a discounted sum
//! over the future. Temporal credit assignment through the recurrent state is carried by BPTT.
//!
//! Consequence, stated up front: with the dense reward `r_t = -m_t |a_t - u|^2` the expected policy
//! gradient equals the supervised MSE gradient, so RL can only match supervised learning here and
//! pays for it in variance. `tests/rl.rs` checks that equivalence numerically.

use crate::model::SequenceModel;
use crate::optim::Adam;
use crate::rng::Rng;
use crate::task::{MemoryGuidedTask, Trial};
use crate::train::{evaluate, lr_at, make_trials, LogLine, TrainConfig, TrainReport};
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Reward {
    /// Per-step reward `-m_t |a_t - u|^2` (same weighting as the supervised mask). Dense.
    Dense,
    /// Fixation penalty before go, then one terminal reward `cos(angle(sum of last window actions) - cue)`.
    Sparse,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Baseline {
    /// Leave-one-out mean of the reward over the batch at the same time relative to go. Needs no
    /// knowledge of the environment, which is what a real RL setting provides.
    Phase,
    /// Reward the environment would have paid for the noise-free mean action. Far lower variance, but it
    /// needs a counterfactual environment query (a learned critic would approximate it).
    MeanAction,
}

#[derive(Clone, Debug)]
pub struct RlConfig {
    pub reward: Reward,
    pub baseline: Baseline,
    /// Exploration noise (std of the Gaussian policy) at the first and the last step (linear anneal).
    pub sigma: f64,
    pub sigma_final: f64,
    /// Weight of the fixation penalty (keeps the readout silent before go), as in the supervised mask.
    pub fix_weight: f64,
    /// Number of final steps whose mean action is decoded in the sparse reward (matches `evaluate`).
    pub decode_window: usize,
    /// Divide advantages by their batch std (makes the update scale independent of the reward scale).
    pub normalize_adv: bool,
    /// First `pretrain` steps use the supervised gradient (hybrid), the rest use RL.
    pub pretrain: usize,
}

impl Default for RlConfig {
    fn default() -> Self {
        RlConfig {
            reward: Reward::Dense,
            baseline: Baseline::Phase,
            sigma: 0.2,
            sigma_final: 0.05,
            fix_weight: 0.5,
            decode_window: 8,
            normalize_adv: true,
            pretrain: 0,
        }
    }
}

fn wrap(a: f64) -> f64 {
    let two_pi = 2.0 * std::f64::consts::PI;
    (a + std::f64::consts::PI).rem_euclid(two_pi) - std::f64::consts::PI
}

/// One policy-gradient estimate over `trials`. Writes the gradient of the surrogate loss into `grad`
/// (descend it) and returns the mean total reward of the sampled actions.
pub fn policy_gradient<M: SequenceModel>(
    net: &M,
    trials: &[Trial],
    cfg: &RlConfig,
    sigma: f64,
    rng: &mut Rng,
    grad: &mut [f64],
    threads: usize,
) -> f64 {
    let b = trials.len();
    let nout = trials[0].n_out;
    // Per trial: mean actions, noise, and for every step (reward, baseline-reward, acts?, key).
    struct Step {
        t: usize,
        r: f64,
        r_mean: f64,
        key: i64,
        adv: f64,
    }
    let mut mus: Vec<Vec<f64>> = Vec::with_capacity(b);
    let mut eps: Vec<Vec<f64>> = Vec::with_capacity(b);
    let mut steps: Vec<Vec<Step>> = Vec::with_capacity(b);
    let mut total_return = 0.0;
    const TERMINAL_KEY: i64 = i64::MIN;

    for tr in trials {
        let mu = net.outputs(tr);
        let e: Vec<f64> = (0..tr.t * nout).map(|_| rng.normal()).collect();
        let a: Vec<f64> = mu.iter().zip(&e).map(|(m, z)| m + sigma * z).collect();
        let (c, s) = (tr.angle.cos(), tr.angle.sin());
        let u = [c, s];
        let mut st: Vec<Step> = Vec::new();
        for t in 0..tr.t {
            let key = t as i64 - tr.go as i64;
            if t < tr.go {
                let w = cfg.fix_weight;
                let (mut ra, mut rm) = (0.0, 0.0);
                for k in 0..nout {
                    ra -= w * a[t * nout + k].powi(2);
                    rm -= w * mu[t * nout + k].powi(2);
                }
                st.push(Step { t, r: ra, r_mean: rm, key, adv: 0.0 });
            } else if cfg.reward == Reward::Dense && tr.mask[t] > 0.0 {
                // scored response steps (the trial's mask excludes the grace period)
                let (mut ra, mut rm) = (0.0, 0.0);
                for k in 0..nout {
                    ra -= tr.mask[t] * (a[t * nout + k] - u[k]).powi(2);
                    rm -= tr.mask[t] * (mu[t * nout + k] - u[k]).powi(2);
                }
                st.push(Step { t, r: ra, r_mean: rm, key, adv: 0.0 });
            }
        }
        if cfg.reward == Reward::Sparse {
            let win = cfg.decode_window.min(tr.t - tr.go).max(1);
            let (mut ax, mut ay, mut mx, mut my) = (0.0, 0.0, 0.0, 0.0);
            for t in tr.t - win..tr.t {
                ax += a[t * nout];
                ay += a[t * nout + 1];
                mx += mu[t * nout];
                my += mu[t * nout + 1];
            }
            let ra = wrap(ay.atan2(ax) - tr.angle).cos();
            let rm = wrap(my.atan2(mx) - tr.angle).cos();
            // one entry per window step, all sharing the terminal reward
            for t in tr.t - win..tr.t {
                st.push(Step { t, r: ra, r_mean: rm, key: TERMINAL_KEY, adv: 0.0 });
            }
        }
        // logged return: sum of per-step rewards (dense) or fixation + terminal (sparse, counted once)
        let mut seen_terminal = false;
        for s in &st {
            if s.key == TERMINAL_KEY {
                if !seen_terminal {
                    total_return += s.r;
                    seen_terminal = true;
                }
            } else {
                total_return += s.r;
            }
        }
        mus.push(mu);
        eps.push(e);
        steps.push(st);
    }

    // ---- baselines -> advantages ----
    match cfg.baseline {
        Baseline::MeanAction => {
            for st in steps.iter_mut() {
                for s in st.iter_mut() {
                    s.adv = s.r - s.r_mean;
                }
            }
        }
        Baseline::Phase => {
            use std::collections::HashMap;
            // (sum, count) per key; for the terminal key each trial contributes once
            let mut agg: HashMap<i64, (f64, usize)> = HashMap::new();
            for st in &steps {
                let mut counted_terminal = false;
                for s in st {
                    if s.key == TERMINAL_KEY {
                        if counted_terminal {
                            continue;
                        }
                        counted_terminal = true;
                    }
                    let e = agg.entry(s.key).or_insert((0.0, 0));
                    e.0 += s.r;
                    e.1 += 1;
                }
            }
            for st in steps.iter_mut() {
                for s in st.iter_mut() {
                    let (sum, cnt) = agg[&s.key];
                    s.adv = if cnt >= 2 { s.r - (sum - s.r) / (cnt as f64 - 1.0) } else { 0.0 };
                }
            }
        }
    }
    // The terminal reward of a trial is counted once in the baseline sum; the leave-one-out above used
    // `s.r` of that trial, which is the same value, so the estimate stays exact.

    if cfg.normalize_adv {
        let (mut ss, mut n) = (0.0, 0usize);
        for st in &steps {
            for s in st {
                ss += s.adv * s.adv;
                n += 1;
            }
        }
        let sd = (ss / n.max(1) as f64).sqrt();
        if sd > 1e-12 {
            for st in steps.iter_mut() {
                for s in st.iter_mut() {
                    s.adv /= sd;
                }
            }
        }
    }

    // ---- surrogate trials ----
    let mut pts: Vec<Trial> = Vec::with_capacity(b);
    for (i, tr) in trials.iter().enumerate() {
        let mut pt = tr.clone();
        pt.targets = vec![0.0; tr.t * nout];
        pt.mask = vec![0.0; tr.t];
        for s in &steps[i] {
            pt.mask[s.t] = 1.0;
            for k in 0..nout {
                let idx = s.t * nout + k;
                pt.targets[idx] = mus[i][idx] + s.adv * eps[i][idx] * nout as f64 / (2.0 * sigma);
            }
        }
        pts.push(pt);
    }
    let _ = net.batch_loss_and_grad(&pts, grad, threads);
    total_return / b as f64
}

/// Train with policy gradient (optionally after `cfg.pretrain` supervised steps). Same logging, evaluation
/// and report types as `train::train`; `train_loss` in the log is the negative mean reward (RL steps) or the
/// supervised loss (pretrain steps).
pub fn train_rl<M: SequenceModel>(
    net: &mut M,
    task: &MemoryGuidedTask,
    tcfg: &TrainConfig,
    cfg: &RlConfig,
    mut on_log: impl FnMut(&LogLine),
) -> TrainReport {
    let mut rng = Rng::new(tcfg.seed);
    let mut noise_rng = Rng::new(tcfg.seed ^ 0x5EED_0F_A110);
    let eval_set = make_trials(task, tcfg.eval_trials, tcfg.seed ^ 0xE7A1_5EED);
    let mut opt = Adam::new(net.params().len(), tcfg.lr, tcfg.clip);
    let mut grad = vec![0.0; net.params().len()];
    let start = Instant::now();
    let mut log = Vec::new();
    let (mut acc_loss, mut acc_n, mut last_gn) = (0.0, 0usize, 0.0);
    for step in 1..=tcfg.steps {
        opt.lr = lr_at(tcfg, step);
        let batch: Vec<Trial> = (0..tcfg.batch).map(|_| task.sample(&mut rng)).collect();
        let loss = if step <= cfg.pretrain {
            net.batch_loss_and_grad(&batch, &mut grad, tcfg.threads)
        } else {
            let p = step as f64 / tcfg.steps as f64;
            let sigma = cfg.sigma + (cfg.sigma_final - cfg.sigma) * p;
            -policy_gradient(net, &batch, cfg, sigma, &mut noise_rng, &mut grad, tcfg.threads)
        };
        last_gn = opt.step(net.params_mut(), &grad);
        if let Some(cap) = tcfg.radius_cap {
            if step % tcfg.project_every.max(1) == 0 {
                net.project_radius(cap);
            }
        }
        acc_loss += loss;
        acc_n += 1;
        if step % tcfg.log_every == 0 || step == tcfg.steps {
            let line = LogLine {
                step,
                train_loss: acc_loss / acc_n as f64,
                grad_norm: last_gn,
                radius: net.radius_for_log(),
                eval: evaluate(net, &eval_set),
                seconds: start.elapsed().as_secs_f64(),
            };
            on_log(&line);
            log.push(line);
            acc_loss = 0.0;
            acc_n = 0;
        }
    }
    let _ = last_gn;
    let final_eval = evaluate(net, &eval_set);
    TrainReport { log, final_eval, seconds: start.elapsed().as_secs_f64() }
}
