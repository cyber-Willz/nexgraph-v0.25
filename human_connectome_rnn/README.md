# human_connectome_rnn (v0.6)

Recurrent neural networks whose architecture is constrained by a **human structural connectome**: wiring from tractography, conduction delays from fibre length, Dale's-law E/I populations per region, trained with exact BPTT (multi-threaded).

v0.2 integrates your `krylov_ds`, `spectral_hypergraph` and `nbsc` crates (vendored in `crates/`, without datasets) and then **ablates every addition over many seeds on the real connectome**. What helped is on by default; what did not is available but off, with the evidence below.

## What is new in v0.6: a spectral-geometry brain (`brain`)

`src/geometry.rs` grows a **replica connectome from the geometry of the brain** and feeds it to the existing Dale's-law, delayed RNN/LSTM unchanged (the replica is an ordinary `Connectome`, so BPTT, RL, Arnoldi calibration and harmonic analysis all work on it).

1. **Geometric eigenmodes.** Region centroids give a Gaussian-kernel graph (bandwidth from the data, plus a weak homotopic left-right bridge); its symmetric normalised Laplacian is a discrete stand-in for the Laplace-Beltrami operator of the cortical surface. Eigenvectors `psi_k` are smooth, hemisphere-aware standing waves, coarse to fine. They depend on positions only (tested).
2. **Spectral connectome model.** `w_ij ~ sum_{k<K} g_k psi_k(i) psi_k(j)`, with `g_k` fitted by ridge least squares.
3. **Replica.** Clip at zero, keep the strongest `m` pairs, lengths from geometry with one fitted detour ratio. Fitted numbers: `K + 2`, against 2145 region pairs at 66 regions.
4. **Validation out of sample.** k-fold over region pairs, so every pair is predicted by a model that never saw it, against a 3-parameter distance rule (`a + b exp(-d/lambda)`) and a density-matched random graph.

```sh
./target/release/connectome_rnn brain --synthetic 33                       # K sweep, fidelity table, layout-from-wiring R^2
./target/release/connectome_rnn brain --tvb data/tvb66 --k 16 --out replica/   # export weights/lengths/labels/coefficients/modes
./target/release/connectome_rnn brain --synthetic 33 --train --steps 1000 --seeds 4   # also train real vs replica vs controls
```

Options: `--k N|auto`, `--sigma MM`, `--homotopic B`, `--folds N`, `--train`, `--seeds N`, `--out DIR`. Library: `hcrnn::geometry::{SpectralBrain, SpectralBrainConfig, geometric_modes, held_out_spectral, k_sweep, fidelity, embedding_r2}`. 11 new tests (45 in total, all pass), including recovery of a planted spectral connectome out of sample (held-out r > 0.95).

### What it shows (synthetic 66-region connectome; the real TVB-66 download returns HTTP 403 here, as before)

Raw output: `results/brain/`. K = 32 chosen automatically (smallest K within 0.01 of the best held-out correlation). Exported replica: `results/brain/replica_synthetic33/`.

| Wiring vs the real connectome | w-corr | edge Jaccard | degree r | strength r | spectrum RMSE | low-subspace overlap |
|---|---|---|---|---|---|---|
| spectral replica, held-out | 0.598 | 0.423 | 0.534 | 0.445 | 0.074 | 0.679 |
| distance rule, held-out | 0.570 | 0.416 | 0.663 | 0.614 | 0.112 | 0.669 |
| density-matched random | -0.031 | 0.135 | 0.123 | -0.033 | 0.056 | 0.184 |

Held-out edge-prediction AUC: spectral 0.817, distance rule 0.818. Held-out correlation rises with K from 0.18 (K = 1) to 0.60 (K = 32) and then flattens (0.61 at K = 48, 0.58 at K = 64). Wiring alone recovers the 3D layout (R^2 of x, y, z on the 3 lowest connectome harmonics): real 0.81, spectral replica 0.83, distance rule 0.87, random 0.03.

Training (RNN, 1000 steps, 4 seeds, held-out error in degrees, chance 90): real connectome 3.8 +- 0.9, spectral replica 3.6 +- 1.2, distance rule 3.9 +- 0.7, density-matched random **1.8 +- 0.3** (0/4 failures everywhere).

Honest reading:

1. **The geometry carries most of the wiring.** The replica recovers held-out weights (r 0.60), edges (AUC 0.82) and the low-frequency harmonic subspace (overlap 0.68 vs 0.18 for random), and it reproduces the real graph's layout-from-wiring property. Random does none of this.
2. **It is not better than a 3-parameter distance rule.** AUC is identical (0.817 vs 0.818); weight correlation is slightly higher (0.598 vs 0.570, one split, not tested for significance), while degree and strength correlations are lower (0.53 vs 0.66, 0.45 vs 0.61). The synthetic generator is itself an exponential distance rule with within-system clustering and hubs, so geometry should explain the distance part and miss the rest. Whether the spectral model beats a distance rule on **real** tractography is untested here and is the question that matters.
3. **`spectrum RMSE` is not a useful discriminator**: the random graph scores best (0.056) because the bulk of a Laplacian spectrum depends mostly on density. Use the low-subspace overlap.
4. **K = 32 of 66 modes is not strong compression at this size**, because the number of modes is of the order of the number of regions; the modelling claim pays off at finer parcellations, which I did not test.
5. **The replica does not make the network learn better on this task.** All wirings solve it, and the random graph is best (1.8 vs 3.6 to 3.9), the same pattern as the README's v0.5 note on the synthetic graph. The memory-guided task does not separate wirings, so this run supports "the replica is a valid, trainable brain-like wiring", not "geometry improves learning".
6. **Limits:** parcel centroids rather than a cortical surface mesh, so these are region-scale eigenmodes, not vertex-level Laplace-Beltrami modes; undirected weights; K chosen on the same folds that are then reported (mild optimism); lambda of the distance rule chosen on all pairs; the system labels (used for input and output roles) are copied from the target, not generated; one synthetic connectome, one fold split, 4 training seeds.

## What is new in v0.5: reinforcement learning (`--algo rl`)

`src/rl.rs` trains either model as a stochastic policy with **REINFORCE**: the network output `mu_t` is the mean of a Gaussian, the emitted action is `a_t = mu_t + sigma*eps_t`, and the network only ever sees a scalar reward, never the target. The policy-gradient loss `-A_t log pi(a_t)` has gradient `-A_t*eps_t/sigma` at the output, and that is fed through the existing exact BPTT as a *surrogate trial* (pseudo-targets `mu + A*eps*n_out/(2*sigma)`), so there is no change to the models: Dale's law, wiring mask, delays, LSTM gates and radius cap hold by construction, and `check_constraints()` is verified before and after every run.

```sh
./target/release/connectome_rnn train   --synthetic 33 --algo rl --steps 2000                    # dense reward, phase baseline
./target/release/connectome_rnn compare --synthetic 33 --algo rl --reward sparse --seeds 8       # terminal reward only
./target/release/connectome_rnn train   --synthetic 33 --algo rl --pretrain 300 --steps 2000      # supervised warm start, then RL
cargo run --release --example rl_grad_noise -- 500 33                                            # gradient-noise diagnostic
```

Options: `--algo supervised|rl`, `--reward dense|sparse`, `--baseline phase|mean-action`, `--sigma/--sigma-final` (default 0.2 -> 0.05), `--pretrain N`. 3 new tests (35 total, all pass), including a numerical check that the dense-reward policy gradient averaged over 3000 noise draws has cosine 0.9999 with the exact BPTT gradient.

* `dense`: reward `-m_t |a_t - u|^2` per step, with the same mask weights as the supervised loss (0.5 before go, 1 in the scored response window, 0 in the grace period).
* `sparse`: the same fixation penalty before go, then **one** terminal reward `cos(decoded angle - cue)` from the mean action over the last 8 steps.
* `phase` baseline: leave-one-out mean reward over the batch at the same time relative to go (needs no knowledge of the environment). `mean-action` baseline: the reward the environment would have paid for the noise-free action (a counterfactual query, a stand-in for a learned critic; much lower variance).
* Because the emitted action does not change the next observation or the hidden state in this task, an action affects only its own step's reward (or the terminal reward), so the exact policy-gradient return is the immediate or terminal reward. Temporal credit assignment through the recurrent state is done by BPTT, as before.

### Does it improve the system? No, not on this task.

**Setup and one important caveat.** The real TVB-66 connectome could not be downloaded in the environment where this was run (the fetch returned HTTP 403), so everything below uses the built-in synthetic 66-region connectome (`--synthetic 33`, density 0.25, data seed 1). Numbers are therefore **not comparable** with the tables above. Held-out angular error in degrees (chance 90, failure > 45), wiring = connectome unless stated, seeds 1 to 8 (4 where marked), one thread per run. RL hyper-parameters were tuned on seeds 101 and 102 (learning rate 0.006 or 0.012, noise 0.3/0.2/0.5 initial; `results/rl/tune_rl.txt`: lr 0.012 diverges, noise 0.2 -> 0.05 was best), supervised settings are the README defaults. Raw output: `results/rl/`.

| Setting (RNN, delay 20 to 40) | Error (deg) | Failures | s/run |
|---|---|---|---|
| Supervised, 1000 steps | 9.0 +- 14.5 (median 3.5) | 0/8 | 21.6 |
| Supervised, 2000 steps | **2.0 +- 0.7** | 0/8 | 43.4 |
| RL dense, phase baseline, 2000 steps | 5.4 +- 1.8 | 0/8 | 65.3 |
| RL dense, mean-action baseline, 2000 steps | 3.6 +- 1.7 | 0/8 | 64.4 |
| RL sparse (terminal reward), 2000 steps | 44.6 +- 33.4 | **5/8** | 62.9 |
| Supervised 300 steps then RL 1700 | 10.5 +- 14.5 | 1/8 | 61.8 |
| density-matched wiring: supervised 1000 / RL dense 2000 | 2.6 +- 0.4 / 3.6 +- 0.7 | 0/8 | 21.8 / 65.6 |

| Other settings (4 seeds) | Supervised | RL (dense, phase) |
|---|---|---|
| LSTM, delay 20 to 40 (1000 steps sup, 2000 RL) | 4.2 +- 0.9 | 5.8 +- 2.3 |
| RNN, delay 50 to 90 (1500 sup, 3000 RL) | 90.2 +- 1.6 (4/4 fail) | 88.9 +- 2.4 (4/4 fail) |
| LSTM, delay 50 to 90 (1500 sup, 2000 RL) | 36.0 +- 27.3 (2/4 fail) | 42.8 +- 10.1 (2/4 fail) |

Gradient noise (`results/rl/grad_noise.txt`, batch 24, cosine between one RL batch gradient and the exact BPTT gradient of the same batch; the model was trained for 500 supervised steps, error 44 deg): dense mean-action 0.72 +- 0.33, dense phase 0.59 +- 0.44, sparse 0.58 +- 0.35; averaging 8 batches raises these to 0.85, 0.72 and 0.91. At initialisation the dense estimators are better aligned (0.84 to 0.89) and the sparse one worse (0.49).

Honest reading:

1. **RL does not improve accuracy, sample efficiency or cost.** At equal steps, supervised BPTT is better in every paired comparison with seeds 1 to 8 (dense-phase RL is worse than supervised 2000 steps on 8/8 seeds, +3.4 deg, paired t = 5.7; mean-action RL is worse on 7/8, +1.6 deg, t = 2.6), and a step of RL costs about 1.5x because the policy is sampled with an extra forward pass. The only row where RL's mean looks better (RL 5.4 vs supervised-1000 9.0) is one slow supervised seed (44.7 deg at 1000 steps, 2.3 deg at 2000 steps), not a difference in typical behaviour: the median supervised error at 1000 steps was 3.5.
2. **This is expected, and the code shows why.** With the dense reward the expected policy gradient is the supervised MSE gradient (verified numerically, cosine 0.9999), so RL has the same fixed point and only adds sampling variance. Single-batch RL gradients point 0.6 to 0.7 cosine away from the exact gradient, which is why it needs about twice the steps, and why a learning rate of 0.012 diverged.
3. **A sparse terminal reward is clearly worse** (5/8 failures): one scalar per trial spread over a 66-region recurrent state leaves little signal per step.
4. **Warm starting does not help** (the hybrid has a failure seed at 46 deg and is not better than supervised-only), and **RL does not rescue the long-memory regime**: at delays of 50 to 90 the RNN stays at chance under RL exactly as under supervised training, and the LSTM is no different within noise (42.8 +- 10.1 vs 36.0 +- 27.3, 2/4 failures each, 4 seeds). The failure is credit assignment through a long delay, which policy gradient does not fix, because it uses the same BPTT path.
5. **The connectome-wiring advantage reported above is not reproduced here**: on this synthetic graph the density-matched control was as good as or better than the connectome wiring under both algorithms (2.6 vs 9.0 supervised at 1000 steps, 3.6 vs 5.4 under RL). That is a property of the synthetic graph and these budgets, not evidence about real data.
6. **What the RL module is good for**: tasks with no supervised target (sparse or delayed reward, choices, control), or where the output has to influence the next input (closed loop); and as a check on trained solutions, since an RL-trained network satisfies the same constraints and can be compared with the supervised one on the harmonic profile (the share of cue-dependent delay activity in the lowest harmonics was 74% for RL-dense vs 79% supervised-2000 on the connectome, a small difference I did not test for significance). It is **not** a way to improve this memory-guided task.
7. **Limits**: synthetic connectome, one task, 8 seeds (4 for LSTM and long delay), RL tuned over 6 settings on two seeds while supervised kept its README defaults, equal steps rather than a full equal-wall-clock sweep, no closed-loop environment (the actions do not feed back), no learned critic (the mean-action baseline is an oracle-style counterfactual), and no entropy bonus or other RL refinements.

## What is new in v0.4: an LSTM on the same connectome (`--model lstm`)

`ConnectomeLstm` keeps the whole biological scaffold of the RNN (it is built from a `ConnectomeRnn`: same wiring, masks, delays, E/I split, null-wiring modes, calibration) and replaces the unit dynamics with gated cells. Every unit has a cell state `c` and output `h = o * tanh(c)`; the input, forget, output and candidate gates all read the same masked, delayed synapses, each with its own Dale's-law magnitudes `w = sign(pre) * softplus(theta)`. Training, evaluation and the harmonic analysis now go through a small `SequenceModel` trait, so both models share the same code. Gradients are exact BPTT (finite-difference check over 223 parameters from all four gates: worst relative error 5e-5). 6 new tests (31 total); the RNN's results are unchanged to the last digit (the 8-seed table below reproduces the v0.2 numbers exactly).

```sh
./target/release/connectome_rnn train   --tvb data/tvb66 --model lstm
./target/release/connectome_rnn compare --tvb data/tvb66 --model lstm --seeds 8 --modes connectome,density-matched
./target/release/connectome_rnn compare --tvb data/tvb66 --model lstm --delay-min 50 --delay-max 90   # longer memory
```

New options: `--model rnn|lstm`, `--gate-scale`, `--forget-bias`, `--delay-min/--delay-max` (task difficulty), `--seed0` (keep tuning and evaluation seeds apart).

### Does it improve the system? Short answer: only where memory is the bottleneck.

Real 66-region connectome, 400 steps, lr 0.006, held-out error in degrees (chance 90, failure > 45). The LSTM was tuned on seeds 101 to 104 (learning rate 0.003 to 0.02, forget bias 1 to 4, gate scale 0.25 to 1; raw output in `results/lstm/tune_*.txt`), then **all numbers below use seeds 1 to 8**. Raw outputs: `results/lstm/e*.txt`.

| Setting | RNN | LSTM | Reading |
|---|---|---|---|
| Default task (delay 20 to 40), connectome | 5.7 +- 1.7 (0/8) | 6.6 +- 3.8 (0/8) | no gain |
| same, 1000 steps | **2.3 +- 0.4** (0/8) | 3.0 +- 0.4 (0/8) | RNN slightly better (about 3.5 standard errors) |
| same, degree-preserving rewire | 12.3 +- 14.2 (0/8) | 5.7 +- 1.7 (0/8) | LSTM removes the RNN's outlier seeds |
| same, density-matched random | 4.4 +- 3.6 (0/8) | 4.9 +- 0.9 (0/8) | no gain |
| same, dense all-to-all (4 seeds) | 28.3 +- 16.4 (1/4) | **10.5 +- 4.1** (0/4) | LSTM tolerates dense wiring much better |
| **Delay 50 to 90**, connectome | 89.2 +- 2.2 (**8/8**) | **44.4 +- 18.8** (3/8) | **LSTM learns where the RNN stays at chance** |
| Delay 80 to 160, 400 steps | 89.1 +- 2.1 (8/8) | 88.0 +- 4.1 (8/8) | neither learns |
| Delay 80 to 160, 1500 steps (4 seeds) | 90.8 +- 2.0 (4/4) | 90.8 +- 2.2 (4/4) | neither learns |

Cost: 12,222 trainable parameters vs 3,066 (4x, one magnitude per synapse per gate) and about 2.5x the training time (17 s vs 7 s per 400-step run).

Honest reading:

1. **On the task this repository was built around, the LSTM is not an improvement.** The RNN already solves it (5.7 deg at 400 steps, 2.3 deg at 1000), and with enough steps it is slightly more accurate than the LSTM and 2.5x cheaper.
2. **The LSTM helps when the memory must be held longer than the RNN's leak and recurrence can manage in the training budget.** At delays of 50 to 90 steps the RNN never leaves chance, and the LSTM gets below 45 deg in 5 of 8 seeds. This is the one clearly significant gain, but it is partial (44 deg mean) and the effect stops at 80 to 160 steps, where nothing learned in 1500 steps. I did not investigate why; plausible but untested causes are the credit-assignment horizon and the weak pre-go loss.
3. **The forget-gate initialisation decides everything.** With the textbook forget bias of 1 the LSTM is worse than the RNN (about 46 deg on the tuning seeds, with higher learning rates collapsing to chance); with a bias of 2 it matches or beats it (7.1 +- 1.2). The default is now 2. Biases of 3 and 4 were no better, and the gate scale barely mattered.
4. **A capacity confound is not ruled out.** The LSTM has 4x the parameters, and I did not run a parameter-matched or tied-gate control, so "gating helps" and "more synapse-level freedom helps" are not separated. It also received a larger tuning sweep than the RNN's learning-rate sweep.
5. **Gating weakens the case for the connectome as a wiring prior.** For the RNN the connectome was the most consistent wiring (sd 1.7 vs 3.6 and 14.2 for the nulls). With the LSTM that advantage is gone (connectome 6.6 +- 3.8, rewired 5.7 +- 1.7, random 4.9 +- 0.9) and dense wiring becomes trainable. The topology signature is unchanged (86 to 87% of cue-dependent delay activity in the lowest harmonics for the connectome vs 52 to 61% for nulls), so the structure is still used, it just matters less to success.
6. **Limits:** one task, one connectome, 8 seeds (4 for dense and the 1500-step runs), a single tuned configuration per model, Dale's law applied to gate weights by the same presynaptic-sign rule as for the RNN (a modelling choice, not biology), and no radius or energy-regulariser analysis for the LSTM (those hooks are RNN-only).

## What is new in v0.3

Tested on two further datasets that contain **no tractography** (Guo et al. 2025 cortical-curvature templates; Cheng et al. HCP-D T1w/T2w puberty maps). Both runs worked, and the gaps they exposed are now fixed:

| Change | Why |
|---|---|
| `--features FILE --labels FILE [--knn K] [--max-length-mm M]` (`Connectome::from_features`) | Build a region graph from any parcels-by-measures table: correlation of z-scored feature profiles, k strongest positive partners per parcel, symmetrised. Lengths come from label positions. It is a structural-covariance graph, and the source string says "not tractography". |
| `glasser_system()` and automatic system labels | Blank or `auto` system entries in a labels file are filled from Glasser HCP-MMP1.0 names (`L_V1_ROI` is Visual, `R_3b_ROI` is Somatomotor, ...), so the default visual-input / somatomotor-output roles work on a 360-parcel atlas without `--input-regions`. |
| `compare --modes a,b` | Run a subset of wiring modes (unknown names are rejected). The dense control is very slow on large graphs, and a full compare previously hit time limits. |
| `scripts/prepare_pscalar_inputs.py`, `scripts/prepare_gifti_templates.py` | `.pscalar.nii` + border files + inflated surfaces into `features.csv` / `labels.csv`; per-vertex GIFTI templates into `weights.csv` / `lengths.csv` / `labels.csv` (both reproduce the inputs used below). |
| `--max-length-mm` | Surface-derived distances can be a tiny fraction of fibre scale (a 20 mm span makes every delay one step); rescale the longest length to a physical value. |
| `Cargo.lock` regenerated (lockfile v3) | The shipped v4 lockfile fails on cargo 1.75. |
| 3 new tests (25 total) | Planted-cluster recovery by the feature graph, rescaling and input validation, Glasser mapping. |

### Results on the HCP-D parcel maps (360 Glasser parcels x 10 maps, k = 10, 400 steps, 3 seeds)

Defaults: 50 visual input parcels, 18 somatomotor output parcels (Glasser names), geometric lengths 0 to 150 mm.

| Wiring | Error (deg) | Failures | Fiedler | Share of delay activity in lowest harmonics |
|---|---|---|---|---|
| feature graph | 2.8 +- 1.1 | 0/3 | 0.029 | 77% |
| degree-preserving rewire | 3.0 +- 1.1 | 0/3 | 0.495 | 41% |
| density-matched random | 3.3 +- 0.4 | 0/3 | 0.493 | 39% |

Honest reading: all three wirings solve the task, and the feature graph is not significantly better. With 50 input and 18 output parcels the task is far easier than the 6/6 setting used earlier, so these errors are not comparable with the 66-region numbers above. What does separate the feature graph is topology: a much smaller Fiedler value (modular, hemisphere-bridged) and activity concentrated in smooth harmonics. The dense control was not run on this graph (too slow). Real wiring still needs tractography.

### Results on the Guo et al. cortical-curvature templates (97 regions, 4 seeds, 400 steps)

97 MSM-HT templates (31 frontal, 31 parietal, 35 temporal; left hemisphere). Graph: correlation of curvature maps over 38,583 shared vertices, top 8 per node, density 0.144, passed through `--csv`. Lengths rescaled to a 150 mm maximum. Input = 6 temporal nodes, output = 6 frontal nodes (arbitrary).

| Wiring | Fiedler | Error (deg) | Failures (>45) | Share of delay activity in lowest harmonics |
|---|---|---|---|---|
| curvature graph | 0.181 | 41.2 +- 5.7 | 2/4 | 89% |
| degree-preserving rewire | 0.549 | 32.6 +- 13.7 | 0/4 | 73% |
| density-matched random | 0.550 | 28.9 +- 14.6 | 1/4 | 65% |
| dense | not finished (time limit) | | | |

A single 400-step run reached 39.6 deg (held-out 46.8 deg), well above chance (90) but short of the 66-region TVB connectome (5.7 deg above). The curvature graph is slightly worse than both nulls here (inconclusive at 4 seeds).

### What the two external tests show

1. **The pipeline generalises to non-tractography data.** Surface templates (97 regions, 194 units) and parcel maps (360 regions, 720 units) both load, pass the Dale's-law and mask checks, train, and produce topology reports. Training time scales steeply with size: 14 s at 97 regions, 46 s at 360 regions for 400 steps.
2. **Feature-similarity graphs are not wiring.** Neither graph beat a density-matched random graph. The accurate reading is "the code runs on this data", not "folding or myelin similarity constrains the dynamics". Task difficulty also dominates: 6 input / 6 output regions gave 29 to 41 deg on 97 regions, while 50 input / 18 output regions gave under 4 deg on 360. Compare wirings only within one setting.
3. **The topology signature repeats.** In both tests the similarity graph has a much smaller Fiedler value than its nulls (0.18 vs 0.55; 0.03 vs 0.49) and concentrates cue-dependent delay activity in the lowest harmonics (89% vs 65 to 73%; 77% vs 39 to 41%), as the real connectome did in the README's v0.2 table. This is a property of modular graphs with a bottleneck; it does not need to be about brain wiring.
4. **Conduction delays need physical-scale lengths.** Raw template distances were 0.3 to 21 mm, which at the default 5 m/s and 5 ms step collapses every delay to one step. Use `--max-length-mm` (features) or rescale before `--csv`. Delays in these tests are therefore a modelling choice, not data.
5. **Name-based system labels matter.** With no labels the default roles fail. Glasser auto-labelling gives 50 visual and 18 somatomotor parcels, but 276 of 360 parcels remain "Unassigned", so spectral modules are not lobe-pure there.
6. **Limits of the evidence.** 3 to 4 seeds, one task, one budget, dense control not run on either graph, and cue-dependent activity by system is descriptive only. Raw outputs are in `results/external_tests/`.

## Data use

See `DATA_USE_TERMS.md`. HCP-derived inputs must be redistributed under the HCP terms, with the required acknowledgment text.

## What is constrained

Each brain region is an excitatory (E) and an inhibitory (I) population, so R regions give N = 2R units.

| Property | How it is imposed |
|---|---|
| Which regions talk | Inter-regional synapses exist **only** where the connectome has a tract; the trainable set *is* the allowed set. |
| Long-range projections | E units only, targeting both E and I units of the receiving region. |
| Local circuit | Full EE / EI / IE / II block inside every region. |
| Dale's law | Sign fixed by presynaptic unit; `w = sign * softplus(theta)`. |
| Conduction delays | `max(1, round(fibre_mm / (velocity * dt)))` steps per region pair (default 5 m/s, dt 5 ms); gradients flow through delays. |
| Input / output | Visual E units in, somatomotor E units out (defaults, by name heuristic). |

`check_constraints()` re-verifies Dale's law and the mask on the realised weights before and after every CLI training run.

## How the three systems are used

| System | Used for |
|---|---|
| `krylov_ds` (Arnoldi) | `spectral.rs`: exact spectral radius of the **delayed, leaky linearised dynamics** (state stacked over the longest delay, applied matrix-free). Used for optional initial calibration (`--calibration arnoldi`), an optional stability projection (`--radius-cap`) and the `radius` column logged during training. Verified against a dense eigendecomposition and for Krylov convergence (dim 80 vs 400 agree to 1e-6 on the real connectome). |
| `spectral_hypergraph` | `topology.rs`: the connectome as a 2-uniform weighted hypergraph; its normalised Laplacian, **connectome harmonics**, and spectral-clustering **modules** (`inspect`). The Laplacian feeds the optional energy regulariser and `harmonic_profile` (which harmonics carry the memory). |
| `nbsc` | `topology.rs`: non-backtracking radius `rho_B`, adjacency norm `||A||_2` and the expansion ratio `||A||/rho_B` of every wiring graph (`inspect`, `compare`). On the real connectome `||A||/rho_B = 1.05`, i.e. `A/rho_B` is expansive, the same regime your RNBSC paper reports for citation graphs. |

Finding worth knowing: the v0.1 baseline's initial "radius 0.9" was the radius of W alone. Including leak and delays its true dynamical radius was **0.907**, and training itself pushed it to about **1.07** in every variant (the task wants slightly super-critical, persistent dynamics).

## Ablation results (real 66-region connectome, 400 steps, held-out error in degrees, chance 90)

Paired seeds; "wins" = seeds where the variant beats the v0.1 baseline on the same seed.

| Variant | Seeds | Error (mean ± sd) | Failures (>45°) | Verdict |
|---|---|---|---|---|
| v0.1 baseline (lr 0.003) | 12 | 16.8 ± 9.3 | 0 | reference |
| **baseline, lr 0.006** | 12 | **9.2 ± 11.3** | 0 | **better, 11/12 wins: new default** |
| baseline, lr 0.01 | 12 | 12.8 ± 15.4 | 1 | worse than 0.006 |
| baseline, lr 0.015 | 12 | 19.9 ± 27.3 | 3 | worse |
| Arnoldi init, r = 0.90 | 12 | 16.8 ± 15.2 | 2 | no gain |
| Arnoldi init, r = 0.90, lr 0.006 | 12 | 10.4 ± 12.1 | 1 | no gain over lr alone |
| Arnoldi init, r = 0.95 | 12 | 20.5 ± 8.9 | 0 | no gain |
| Arnoldi init, r = 0.975 / 1.0 / 1.03 | 6 | 33.4 / 29.3 / 44.9 | 1 / 1 / 3 | worse: starting super-critical slows learning |
| Radius cap 1.05 | 6 | 35.7 ± 15.4 | 2 | worse (training wants radius > 1) |
| Cosine LR decay | 6 | 45.6 ± 4.5 | 4 | worse at this short budget |
| **Energy regulariser** (λ = 0.005 to 0.3) | 6 | 80.9 to 90.3 | 5 to 6 | **collapses to chance** (activity is silenced) |
| Energy regulariser, delayed start (@150 / @200) | 6 | ~41 ± 40 | 2 | still unreliable |

Conclusions: the one robust improvement is **lr 0.006**. Arnoldi initialisation is correct, delay-aware and well tested but does **not** improve accuracy, so the default stays `--calibration power`. The energy regulariser as formulated (Rayleigh quotient of E rates on the connectome Laplacian) lets the network lower the penalty by going silent; it is kept (`--energy-lambda`, `--energy-start`) but off. The learning-rate sweep and ablations used one task and 400 steps; other tasks or budgets may differ.

## Real wiring vs controls (8 seeds, lr 0.006, 400 steps)

| Wiring | rho_B | Fiedler | Error (deg) | Failures |
|---|---|---|---|---|
| connectome | 22.52 | 0.377 | **5.7 ± 1.7** | 0/8 |
| degree-preserving rewire | 22.26 | 0.644 | 12.3 ± 14.2 | 0/8 |
| density-matched random | 19.50 | 0.648 | 4.4 ± 3.6 | 0/8 |
| dense all-to-all | 64.00 | 1.015 | 29.6 ± 26.6 | 2/8 |

Honest reading: the connectome does **not** beat the density-matched random graph on mean error (5.7 vs 4.4; not distinguishable at 8 seeds), but it trains far **more consistently** than either null (sd 1.7 vs 3.6 and 14.2) and clearly beats dense wiring. Topology separates it from the nulls through the Fiedler value (0.38 vs 0.64: modular structure with a hemispheric bottleneck), not through `rho_B`. The harmonic profile (share of cue-dependent delay activity in the lowest third of harmonics: 86% vs 52 to 61% for the nulls) is descriptive only: each wiring has its own harmonic basis, and for dense wiring the basis is degenerate.

## Usage

```sh
cargo build --release
sh scripts/fetch_tvb66.sh          # real 66-region human connectome (TVB data repo)

./target/release/connectome_rnn inspect --tvb data/tvb66      # topology, harmonics, spectral modules
./target/release/connectome_rnn train   --tvb data/tvb66 --out run1
./target/release/connectome_rnn compare --tvb data/tvb66 --seeds 8   # real vs nulls vs dense
./target/release/connectome_rnn bench   --tvb data/tvb66 --seeds 12 --only R3   # ablation harness
HCRNN_TVB66=data/tvb66 cargo test --release
```

Other data: `--csv weights.csv --lengths lengths_mm.csv --labels labels.csv`, or `--synthetic N` (stand-in generator, not measured data). Region "systems" are a name-based heuristic, not a functional parcellation. Matrices are symmetrised on load.

## Verification

31 tests + doctest (22 original RNN/analysis tests, 3 feature-graph tests, 6 LSTM tests; the LSTM ones check finite-difference gradients through all four gates, thread-count invariance, identical wiring to the RNN, Dale's law and the mask per gate after training in every wiring mode, parameter round-trip, and learning): BPTT gradient vs finite differences (with delays, and with the energy term, relative error about 1e-5), Arnoldi radius vs dense eigendecomposition, calibration hits target in every wiring mode, radius-cap projection, Laplacian equals `L = I - D^-1/2 A D^-1/2` and harmonics are orthonormal, `nbsc` closed forms on complete graphs, spectral modules recover two planted blocks, mask / Dale's law / delay invariants, rewiring preserves degrees, thread-count invariance, learning smoke test.

## Limitations

Macroscale regional E/I populations, not neurons. Streamline counts are not synaptic strengths (they set support and initial magnitudes). One assumed global conduction velocity. One task. 400-step budgets: networks are still improving. Vendored crates are unmodified except for removing their `[profile]` tables (workspace-level profile now).
