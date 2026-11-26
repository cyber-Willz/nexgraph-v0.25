# nexgraph v0.5: summary

**New in v0.5:** optional PostgreSQL durability + relational mirror (see `POSTGRES.md`, `IMPROVEMENTS_v0.5.md`). The text below describes the engine, unchanged from v0.4 apart from the durability layer.

**What it is.** An embedded, in-memory property-hypergraph database written in Rust, with a write-ahead log for durability, a Cypher-style query language (NQL), and spectral/physics analytics that run inside queries. Built beside `human_connectome_rnn`; it reuses `spectral_hypergraph` and `nbsc` from that tree without modifying them.

## Data model
* **Nodes** carry labels and properties.
* **Edges are directed hyperedges**: a `tail` set and a `head` set, plus a label, an `f64` weight and properties.
  * Binary edge: one tail, one head.
  * Undirected hyperedge: all members in `tail`, empty `head`.
* Deleting a node deletes every edge and hyperedge it belongs to.

## Core engine
| Area | How it works |
|---|---|
| Storage | Immutable version chains per entity, `{from_ts, to_ts, body}` |
| Isolation | MVCC snapshot isolation; one writer at a time, many concurrent readers |
| Time travel | `AS OF <ts>` queries and `db.read_at(ts, ..)`, until a checkpoint compacts history |
| Durability | CRC-32 framed WAL (optional fsync per commit), atomic checkpoint, torn-tail truncation, mid-log corruption reported |
| Indexes | Label (always), property equality, HNSW vector (cosine or L2); all re-verified at the reader's snapshot |
| Writes | Validate, append to the log, apply, publish timestamp; a failed commit applies nothing |

## Query language (NQL)
`MATCH` (including variable-length paths `*1..3` and `HYPEREDGE` patterns), `WHERE`, `CREATE` (nodes, edges, hyperedges), `SET`, `DELETE`, `RETURN` with `DISTINCT`, aggregates, `ORDER BY`, `SKIP`, `LIMIT`, `CALL ... YIELD`, parameters, and `AS OF`. A traversal budget stops runaway queries.

## Analytics procedures
| Procedure | Purpose |
|---|---|
| `algo.pagerank`, `algo.components`, `algo.shortestPath`, `algo.dijkstra` | classic graph algorithms (hyperedges projected to weighted pairs) |
| `spectral.fiedler`, `cluster(k)`, `eigenvalues(k)`, `embedding(k)` | normalised hypergraph Laplacian |
| `spectral.communities([k])`, `nb_radius()` | non-backtracking (Bethe Hessian) communities; v0.2, from `nbsc` |
| `physics.heat`, `heat_from`, `harmonic` | exact heat diffusion and Dirichlet extension; v0.2, from the equations `pde_shpinn` approximates |
| `vector.search` | k-nearest-neighbour over a vector index |

## Evidence (rustc 1.97, one Linux sandbox)
* **42 tests pass**: unit, integration (queries, hyperedges, time travel, rollback, recovery, corruption, concurrency, HNSW, spectral, physics) and a doctest.
* **Exactness**: `physics.heat('sym')` matches the dense matrix exponential of the crate's Laplacian to below 1e-9; `physics.harmonic` matches a direct solve to below 1e-7.
* **Connectome import**: 66 regions and 548 tracts from `human_connectome_rnn`; degrees and strengths match the source.
* **Cora (real data)**: label propagation 71.5% strict test accuracy (76.0% on reachable nodes; chance 14.3%).
* **Sparse planted partitions**: the Bethe Hessian ties the Laplacian at moderate sparsity and wins near the detectability threshold (0.943 vs 0.503 at mean degree 1.5).
* **Benchmarks**: bulk load of 100k nodes and 800k edges in 1.3 s; indexed point query 2.4 µs; HNSW top-10 in 364 µs against 6.1 ms exact (recall 0.954); 7.5k commits/s with fsync.

## Known limits
* **Real-data weakness**: on Cora the Bethe Hessian clustering is worse than the Laplacian (NMI 0.227 against 0.390 at k=7) and its automatic k over-counts (37 communities).
* **Scale**: everything lives in memory; one process and one writer (a second open of a directory is refused since v0.22); no replication.
* **Query engine**: heuristic planner only; no `WITH`, `OPTIONAL MATCH` or `MERGE`; equality and range (`<`,`<=`,`>`,`>=`) predicates use property indexes, no composite indexes; complex pattern scans are row-at-a-time; the one-hop count shape is factorised (800k edges in ~25 ms, 1.2 s in v0.2).
* **Spectral caveats**: results are degenerate on disconnected graphs; `physics.*` and communities ignore edge direction.
* **Toolchain**: only rustc 1.97 was compiled, so the 1.75 minimum is unverified.
* **Provenance**: the code was largely AI-generated; check your institution's or employer's disclosure rules before presenting it as unaided work.

## Files
Run `cargo test --release` in `nexgraph/`. See `README.md` for usage and measured tables, and `HOW_IT_WORKS.md` for the internals.
