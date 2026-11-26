**New in v0.23:** a web UI (`frontend/`, Yew + Stylist; query workbench, analytics launcher, data explorer, audit log, tenant/user admin, Power BI) served by `nexgraph-server --frontend DIR`, plus `/v1/stats` and tenant/user list endpoints. See `frontend/README.md` and `IMPROVEMENTS_v0.23.md`.

**New in v0.22:** `WHERE n.k > x` (and `<`, `<=`, `>=`, either side, `AND`ed) is answered from the ordered property index (130 ms → 43 µs on 200k nodes in `examples/bench_range.rs`), and opening a database directory twice is refused by an enforced lock. See `IMPROVEMENTS_v0.22.md`.

**New in v0.18:** GAAP ledger extensions: atomic `Ledger::post_many`, journal CSV import, `gaap.ratios` (17 ratios) and `gaap.open_items` / `gaap.aging` (reference-based receivable/payable aging tied to the ledger). v0.17 added the double-entry GAAP ledger itself (`gaap.*`, `GAAP.md`). See `IMPROVEMENTS_v0.18.md`.

**New in v0.15:** `swarm.detect` (persistent co-movement groups in track data, with random-heading and background significance tests and a `coordinated / whole scene / co-located` verdict), one-entity observation imports (`allow_single`). Detection and characterisation only. See `IMPROVEMENTS_v0.15.md` for findings on the supplied drone datasets and the limits.

**New in v0.14:** tab-separated import (`delimiter`), `fin.identity` statement-consistency checks on SEC / XBRL fact tables, and a separation warning in `uw.scorecard`. Findings and what could not be tested (Excel, GBK files, a corrupt Q3 zip): `IMPROVEMENTS_v0.14.md`.

**New in v0.13:** underwriting analytics (`uw.audit / scorecard / gains / scores / disparity / group_gap / psi / affordability`) with leakage detection, keyed node import and `Label.prop` joins across tables, chunked import commits (`chunk=`), one-sided `land.hotspots`, and loan / fraud presets; verified against numpy, scipy, sklearn and statsmodels. Finding: one of the supplied datasets (A8) carries no predictive signal. Not built: a bank-statement `cf.*` module. See `IMPROVEMENTS_v0.13.md`.

**New in v0.12:** land-registry analytics (`land.conflicts / rule_check / hotspots / valuation / survey_check`), qualified member keys so same-named places under different parents are not merged, text-preserving identifier columns, and backtick-quoted identifiers (``e.`Land Area (Acres)` ``) in NQL; verified on a 1,000-record land registry extract against pandas and scipy. See `IMPROVEMENTS_v0.12.md`.

# nexgraph 0.10

**New in v0.10:** de-duplicating hyperedge import (`Dedup`, `HyperOpts`) and kill-chain analytics (`killchain.funnel / control_effect / exposure / response / defense_depth`), verified on a 100k-incident dataset against pandas, scipy and statsmodels. See `IMPROVEMENTS_v0.10.md`.

An embedded **property-hypergraph database** in Rust, built next to `human_connectome_rnn` and reusing its `spectral_hypergraph` crate (unmodified, referenced by path). The database core has no third-party dependencies; the optional `spectral` feature pulls in `spectral_hypergraph` and `nalgebra`, and `nonbacktracking` adds `nbsc` (built without Burn) for its Hashimoto operator.

## What makes it different from a conventional graph database

| Capability | How it works here |
|---|---|
| **Native hyperedges** | Every edge is a directed hyperedge `tail -> head` (sets). Binary edge = 1-to-1; empty head = undirected hyperedge over all members. No reification needed. |
| **MVCC + time travel** | Immutable version chains, snapshot isolation, `AS OF <ts>` queries. Readers never block readers and never see torn commits. |
| **Durability** | CRC-32 framed write-ahead log, optional fsync per commit, atomic checkpoint (tmp + rename + dir fsync), torn-tail truncation, and mid-log corruption reported instead of skipped. |
| **Indexes** | Label (always), property equality (`CREATE INDEX ON :L(k)`), HNSW vector (`CREATE VECTOR INDEX ON :L(k) DIM n METRIC cosine\|l2`). All are MVCC-safe: candidates are re-verified at the reader's snapshot. |
| **Spectral analytics in the query language** | `CALL spectral.cluster(k)`, `spectral.fiedler()`, `spectral.eigenvalues(k)`, `spectral.embedding(k)` run on the live hypergraph (normalised hypergraph Laplacian, Zhou et al.; dense solver to 1500 vertices, Lanczos above). Also `algo.pagerank/components/shortestPath/dijkstra`. |
| **Exact PDE procedures** *(v0.2, from pde_shpinn)* | `CALL physics.heat(key, t [,'rw'\|'sym'])`, `physics.heat_from([nodes], t)` solve the heat equation `du/dt = -(I-P)u` on the hypergraph by uniformisation (no eigensolver; stable); `physics.harmonic(key)` solves the Dirichlet problem (fixes nodes that have a numeric `key`, fills in the rest). `'sym'` is exactly the normalised hypergraph Laplacian SHPINN uses. |
| **Non-backtracking communities** *(v0.2, from pde_shpinn/nbsc)* | `CALL spectral.communities([k])` (Bethe Hessian at `r = sqrt(rho_B)`, `rho_B` from nbsc's Arnoldi run; negative-eigenvalue count gives k automatically) and `spectral.nb_radius()`. |
| **NQL** | Cypher-style: `MATCH` (incl. variable-length `*1..3`, `HYPEREDGE` patterns), `WHERE`, `CREATE`, `SET`, `DELETE`, `RETURN` with `DISTINCT/ORDER BY/SKIP/LIMIT` and aggregates, `CALL ... YIELD`, parameters, `AS OF`. |

**v0.4** adds Polars-inspired execution (flat dictionary-encoded entity metadata, factorised one-hop counting, top-K): full scan 577 ms → ~25 ms, 2-hop 67 µs → 22 µs, group-by 776 ms → ~190 ms; see `IMPROVEMENTS_v0.4.md`. **v0.3** was a performance release driven by the cross-system benchmark; see `IMPROVEMENTS_v0.3.md` for what changed and `bench/BENCHMARK_REPORT.md` for before/after numbers (full scan 1.29 s → 0.58 s, group-by 1.62 s → 0.78 s, shortest path 323 ms → 1.2 ms, 2-hop 104 µs → 67 µs; scans are still far slower than Kuzu/DuckDB).

## Quick start

```rust
use nexgraph::Database;
let db = Database::open("./mydb")?;           // or Database::in_memory()
db.query("CREATE (a:Person {name:'Ada'}), (b:Person {name:'Bob'}), (a)-[:KNOWS]->(b)")?;
db.query("MATCH (a),(b) WHERE a.name='Ada' AND b.name='Bob' CREATE HYPEREDGE m:Meeting {t:'x'} OVER (a,b)")?;
let r = db.query("MATCH (a)-[:KNOWS*1..3]->(b) RETURN a.name, count(*) AS n ORDER BY n DESC")?;
println!("{}", r.to_table());
```

Programmatic transactions: `db.begin()` → `create_node / create_edge / create_hyperedge / set_prop / delete / commit` (drop or `rollback()` discards). Reads: `db.read(|view| ...)`, `db.read_at(ts, ...)`.

Shell: `cargo run --release --bin nexgraph -- ./mydb` (reads statements from stdin, `;`-terminated; `.checkpoint`, `.stats`).

## Semantics worth knowing

* **Traversal over hyperedges**: a directed edge leads from every tail member to every head member; an undirected hyperedge (empty head) is crossed only by undirected patterns `-[]-` and connects all members pairwise.
* **Deleting a node deletes every edge/hyperedge it belongs to** (the whole hyperedge, not just the membership).
* **Edge weight** is a first-class `f64` (default 1.0): `CREATE ... {weight: 2.5}` / `SET e.weight = ...`.
* **Isolation**: snapshot isolation, single writer at a time (writers serialise on a mutex). Within one statement, writes are not visible to later clauses of the same statement.
* **Checkpoint = vacuum**: `checkpoint()` writes the live state, truncates the WAL and discards version history older than the latest commit (`AS OF` before that returns an error).
* **Edge uniqueness within a path** (Cypher relationship-isomorphism); a 5M-step traversal budget aborts runaway variable-length queries.
* **Vector search**: HNSW over-fetches, then drops hits not visible at the snapshot; `exact = true` brute-forces.
* Spectral procedures consider nodes that belong to at least one edge with >= 2 distinct members and positive weight. **Every such edge participates, including any hyperedges you add for other purposes.**

## What came from pde_shpinn (v0.2) and what did not

`pde_shpinn` is a Cargo workspace of `krylov_ds`, `spectral_hypergraph`, `nbsc` (these three are byte-identical to the copies already in `human_connectome_rnn`, so nothing new there) plus the `shpinn` crate and the Cora/Citeseer/PubMed data.

* **Used**: the *equations* SHPINN trains a network to approximate (Poisson/Dirichlet and heat on the hypergraph Laplacian), solved exactly; `nbsc`'s Hashimoto operator and `rho_B`; the Cora Planetoid files as a real-data test.
* **Not used**: the Burn-based PINN training, Fourier features, Burgers / Navier-Stokes / finance modules, GCN/GAT/SAGE layers. They need Burn 0.13 plus pinned transitive crates and have no role in a database, where a direct solve of the same equation is available. SHPINN's own demos validate against exactly such direct solves.
* **Bug found by the real-data run**: v0.1's `spectral.cluster` failed (Lanczos non-convergence) on graphs above 1500 nodes. Fixed: Lanczos now gets up to `max(300, 20k)` steps and falls back to the dense solver up to 4000 vertices; clustering uses nexgraph's own k-means. Regression-tested.

### Measured results (v0.2)

**Synthetic sparse planted partition** (2 blocks of 500, accuracy over 6 seeds, k = 2 for both):

| mean degree / margin over the detectability gap | Bethe Hessian | Laplacian `spectral.cluster` |
|---|---|---|
| 3 / 1.5 | 0.988 | 0.991 |
| 2 / 1.2 | 0.958 | 0.812 |
| 2 / 1.0 | 0.871 | 0.689 |
| 1.5 / 1.1 | 0.943 | 0.503 |

So the non-backtracking method ties at moderate sparsity and wins clearly near the threshold. Auto-k recovered 3 of 3 planted blocks in the test.

**Real data, Cora (Planetoid split, `examples/cora_real_data.rs`)**:

* `physics.harmonic` as label propagation, 7 one-vs-rest solves in 1.4 s: **71.5 % strict test accuracy** (715/1000; 59 test nodes cannot be reached from any of the 140 labelled nodes and count as wrong), 76.0 % on the 941 reachable nodes. Chance is 14.3 %. This is plain label propagation with no node features.
* Community detection on the 2485-node largest component vs the 7 classes (NMI): Laplacian `spectral.cluster(7)` **0.390**; Bethe Hessian with k = 7 **0.227**; Bethe Hessian with automatic k **0.273 at 37 communities**. **On this real citation graph the Bethe Hessian is worse than the existing Laplacian clustering, and its automatic k over-counts** (Cora is not a stochastic block model: hubs, many small groups). Use it for very sparse, block-like graphs; do not expect it to beat the Laplacian generally.

**Exactness checks**: `physics.heat(..., 'sym')` matches the dense `V exp(-t Lambda) V^T` built from `spectral_hypergraph`'s own Laplacian to < 1e-9 (t from 0 to 40); `physics.harmonic` matches a direct dense LU solve to < 1e-7 on a random hypergraph and is exactly linear on a path; constants are preserved and the maximum principle holds; nbsc's `rho_B` on a random near-4-regular graph is 3.000 (theory: 3).

## Verified here (rustc 1.97, Linux)

* 42 tests pass (release; v0.3/v0.4 add 4 in `tests/v03.rs`); of these in v0.2: 9 unit (codec, CRC vector, parser, HNSW recall > 0.95), 17 integration (v0.1), 11 integration for the v0.2 additions, 1 doctest. Integration covers query semantics, hyperedge traversal and cascade, time travel, rollback/validation atomicity, index equivalence, restart recovery, checkpoint, torn-tail truncation, mid-log corruption detection, 4 concurrent readers against a writer (invariant never torn), HNSW vs exact including deletes and updated vectors, recovery of a planted two-clique partition by `spectral.cluster/fiedler/eigenvalues` (also on hyperedges), and the traversal budget.
* `examples/import_connectome.rs` loads the attached crate's synthetic 66-region connectome (548 tracts); degrees match `Connectome::degree` for all 66 regions and strengths match exactly; hubs, spectral clustering, Laplacian spectrum, HNSW position search and time travel all run.

### Benchmarks (release build, one core, this sandbox; indicative only)

| | |
|---|---|
| Bulk load, 100k nodes + 800k edges, one txn | 1.31 s |
| Indexed point query incl. parse | 2.4 us |
| 2-hop traversal from indexed node (~64 paths) | 104 us |
| Full scan `MATCH (a)-[]->(b) count` over 800k edges | ~25 ms in v0.4 (0.58 s v0.3, 1.2 s v0.2) |
| PageRank, 30 iterations, 100k nodes / 800k edges | 209 ms |
| HNSW build, 20k x 32-d | 4.65 s |
| Top-10 vector query: HNSW vs exact | 364 us vs 6148 us, recall@10 = 0.954 |
| Single-statement commits: fsync off / on | 430k/s / 7.5k/s |

## Limits (honest list)

* **Not verified on MSRV 1.75** (the toolchain could not be downloaded here); I avoided APIs newer than that, but only 1.97 was compiled.
* Single process only (enforced since v0.22 by an exclusive lock on `<dir>/LOCK`; a second open fails); single writer; whole dataset in memory (no paging/larger-than-RAM storage); no replication or distributed execution.
* Query executor is a straightforward nested-loop matcher with one heuristic (start from the more selective end of a path). No cost-based planner, no `WITH`, `OPTIONAL MATCH`, `UNWIND`, `MERGE`, label-set update in NQL (available via the Rust API), property indexes serve equality and, since v0.22, range predicates (`<`, `<=`, `>`, `>=`) on top-level `WHERE` conjuncts; no composite or multi-key range indexes.
* Full-graph pattern scans are bound by pointer chasing through version chains and are the weakest benchmark above (see `IMPROVEMENTS_v0.3.md`).
* HNSW is append-only and in memory (rebuilt on open), and heavy churn of vectors increases dead entries until checkpoint.
* `physics.*` and `spectral.communities` ignore edge direction; communities also ignore weights and use the clique expansion (a hyperedge of size s adds s(s-1)/2 pairs). Heat and harmonic use every edge of 2+ members with positive weight.
* `physics.harmonic` iterates to a tolerance (default 1e-10, 100k iterations) and errors rather than returning an unconverged answer; nodes unreachable from any known node get `null`.
* Spectral results on graphs with several components are degenerate (zero eigenvalue repeats) — the Fiedler vector is then not meaningful.
* In the connectome example, spectral clustering with k = 2 did not split by hemisphere (25/8 in each hemisphere); the `SYSTEM` hyperedges added there also tie regions together. That is a property of that synthetic data, not a defect check either way.

## Layout

```
src/value.rs   values, varint/CRC codec        src/store.rs   MVCC store, indexes, View
src/model.rs   Body/Op/Commit                  src/db.rs      WAL, checkpoint, Txn, Database
src/hnsw.rs    vector index                    src/algo.rs    algorithms + spectral procedures
src/query/     lexer/parser/executor           tests/db.rs    integration tests
examples/      import_connectome.rs, cora_real_data.rs, bench.rs
data/          cora_planetoid/ (Cora, Planetoid split, from pde_shpinn; for the example)
src/physics.rs, src/community.rs, src/kmeans.rs  (v0.2)
```

Expected sibling directory: `../human_connectome_rnn` (included in the archive, unmodified).


## PostgreSQL storage (v0.5)

`Database::open_postgres(url)` stores the commit log and checkpoints in a PostgreSQL schema and keeps SQL-readable
mirror tables (`nodes`, `edges`, `edge_members`, `binary_edges`) in step, atomically. See `POSTGRES.md` for usage, guarantees
and limits (notably: no TLS yet) and `IMPROVEMENTS_v0.5.md` for measurements.

## TLS, authentication and multi-tenancy (v0.6)

PostgreSQL connections use verified TLS (rustls). `nexgraph::security` adds users with Argon2id passwords, hashed expiring tokens,
roles (reader / writer / tenant_admin / platform_admin), one isolated database per tenant, quotas and a hash-chained audit log;
`nexgraph-server` exposes it over HTTPS. See `SECURITY.md` (including what is **not** covered) and `IMPROVEMENTS_v0.6.md`.

## Data analytics and cyber-security (v0.7)

`CALL algo.degree/betweenness/closeness/kcore/triangles/scc/articulation/louvain/similarity`, `CALL stats.describe/outliers/histogram/correlation/timeseries/spikes`,
`stdev()`/`median()` aggregates, `CALL cyber.reachable/attack_paths/chokepoints/propagate_risk/fanout/beaconing/volume/ioc`, `nexgraph::drift`, and loaders for CSV,
Suricata `eve.json` and Zeek JSON (`nexgraph::ingest`). Export with `QueryResult::to_csv()/to_json()` or shell `.mode csv|json`.

```text
.import events zeek conn.json
CALL cyber.beaconing('FLOW','ts',20,0.25) YIELD src, dst, events, cv RETURN src.ip, dst.ip, events, cv;
CALL cyber.chokepoints('Entry','Crown') YIELD node, score RETURN node.ip, score ORDER BY score DESC LIMIT 5;
```
See `IMPROVEMENTS_v0.7.md` for semantics, measurements and limits.


## Event tables and hyperedges (v0.9)

`ingest::import_csv_hyperedges` / `.import hyper` turn each CSV row into one hyperedge over its entity columns; `WHERE a.k = literal` now uses property indexes;
`cyber.beaconing`, `stats.timeseries`, `stats.spikes` accept ISO 8601 timestamp strings; spectral procedures scale to 100k parallel hyperedges and no longer panic
on graphs with huge numbers of components; `YIELD *`. See `IMPROVEMENTS_v0.9.md` (found by running a 100k-row cyber-attack dataset).

## Power BI / BI tools (v0.8)

`nexgraph::bi` flattens a snapshot into typed tables (`nodes`, `edges`, `edge_members`, `node_<Label>`, `edge_<Label>`); `nexgraph-server`
serves them at `/v1/bi/*` (CSV/JSON, Bearer or Basic auth, read-only NQL, `as_of` time travel, ready-made Power Query script),
PostgreSQL gets `bi_nodes/bi_edges/bi_edge_members` views, and the shell has `.powerbi DIR` for a CSV folder + Power Query script.
See `POWERBI.md` (including what was **not** verified: the M scripts were not run in Power BI Desktop).


## ALM (v0.11)

`alm.position`, `alm.liability_profile`, `alm.duration_gap`, `alm.rate_shock`, `alm.cashflow_ladder`, `alm.krd_gap` and `alm.scenario_risk`
cover asset-liability management: funding ratio, duration gap, exact liability repricing under yield shifts, cash-flow matching, key-rate DV01 and
VaR across stochastic paths. See `IMPROVEMENTS_v0.11.md` and the module header of `src/alm.rs`.

> **v0.24:** hyperedge-label hint on `MATCH (n:Label)`, `single` import option, tenancy-aware `land.conflicts`, fast `spectral.nb_radius()` (community count now opt-in). See `IMPROVEMENTS_v0.24.md`.

> **v0.25:** checkpoint and open use much less memory (peak 4.4 GB -> 1.8 GB on a 508k-node load), profiler no longer mistakes address *counts* for street addresses and recognises wallet addresses, keyed node imports report duplicate keys. See `IMPROVEMENTS_v0.25.md`.
