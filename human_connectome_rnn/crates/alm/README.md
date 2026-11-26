# alm: Asset-Liability Management

Rust ALM engine inside the `human_connectome_rnn` workspace. It reuses two vendored crates:

* `krylov_ds`: Arnoldi iteration, used for the dominant eigenvalue (spectral radius) of the contagion operator.
* `spectral_hypergraph`: exposure hypergraph, normalized Laplacian, algebraic connectivity, spectral clustering.

The connectome RNN code itself is not used; it is neuroscience-specific.

## Quick start
```sh
cargo build --release -p alm
./target/release/alm sample --out input.json        # illustrative balance sheet (USD m, fictional)
./target/release/alm report input.json              # full analysis; exit code 3 if any limit is breached
./target/release/alm report input.json --json
./target/release/alm rebalance input.json --target-dv01 0.18 --apply rebalanced.json
cargo test --release -p alm
```
Library: `alm::run_alm(&AlmInput) -> AlmReport`.

## What it computes
| Area | Method |
|---|---|
| Cash flows | Monthly, per instrument: fixed bullet, level-payment amortizing, floating (forward-curve resets), non-maturity deposits (behavioural decay + rate beta/floor) |
| Valuation | PV on a zero curve with a per-instrument discount spread calibrated so PV = book on the base curve (`calibrate_to_book`); EVE; DV01, duration, convexity, duration gap; key-rate DV01 |
| Scenarios | BCBS-style parallel up/down, steepener, flattener, short up/down (sizes configurable; default USD 200/250/100bp, floored at 0) |
| NII | 12-month instantaneous-shock simulation, static curve afterwards; constant or run-off balance sheet; run-off replaced at shocked rate + original margin |
| Liquidity | LCR with BCBS Level-2 caps (15%/40%), 75% inflow cap; contractual maturity ladder; survival horizon |
| Network | Hypergraph of instruments (hyperedges: category, counterparty, repricing window, weak global edge); algebraic connectivity; spectral clusters with asset/liability imbalance; funding/asset HHI |
| Contagion | `x' = M x + s`, `M = alpha * W * diag(fragility)`; radius via Arnoldi (cross-checked against a dense eigensolver in tests); funding-shock amplification `(I-M)^-1` |
| Rebalancing | Minimum-cost trades on `tradable` items to hit an EVE DV01 target, size-neutral, within balance bounds (dual Newton solver, verified; reports unreachable targets) |
| Limits | EVE loss % equity, NII loss %, LCR, duration gap, contagion radius, funding HHI (all configurable) |

## Input file
JSON: `curve {tenors, zeros}`, `balance_sheet {name, currency, instruments[]}`, optional `limits`, `shocks`, `config`. See `examples/sample_input.json` and `examples/sample_report.txt`.
Instrument kinds: `{"type":"FixedBullet"}`, `{"type":"Amortizing"}`, `{"type":"Floating","reset_months":6,"spread":0.03}`, `{"type":"NonMaturity","decay_monthly":0.02,"beta":0.45,"floor":0.002}`.
An empty `counterparty` means a granular pool (e.g. retail deposits): it gets no shared hyperedge and counts as its own group in the HHI.

## Assumptions and limits (read before relying on numbers)
* Single curve, no credit/option risk: prepayment, deposit withdrawal behaviour and caps/floors are only crudely modelled via `decay_monthly`, `beta`, `floor`. Behavioural parameters drive the results; calibrate them to your own data.
* The contagion model is stylised: its coupling and fragility inputs are assumptions, not estimated parameters. Treat the radius as a relative diagnostic across balance sheets/scenarios, not a probability.
* Calibrating spreads to book means EVE equals book equity on the base curve, so scenario ΔEVE is a rate-sensitivity measure. It is not a fair-value estimate.
* LCR inflow factors and run-off rates are per-instrument inputs; check them against your regulator's rules. This tool does not implement NSFR, IRRBB supervisory add-ons, or hedging instruments (swaps).
* The sample data is fictional.
