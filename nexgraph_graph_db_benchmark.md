# nexgraph 0.25 vs other databases: benchmark report

Date: 2026-11-26. Everything below was measured in this session; nothing is quoted from vendor material.

## Summary

- On **small indexed lookups and multi-hop traversals**, nexgraph is the fastest *graph* engine tested: about 80x faster than Kuzu on point lookups and about 80x on 2-hop counts, with the caveat on call overhead below.
- On **full scans** it is the fastest of all six systems tested (3.3 ms over 1M edges). On **group-by and aggregation** it ties Kuzu and trails DuckDB (about 1.8x slower).
- On **shortest path** it is 10x faster than Kuzu, but FalkorDBLite is about 6x faster than nexgraph.
- On **bulk load** it trails Kuzu, DuckDB and PostgreSQL.
- Versus its own README (v0.4 to v0.21 era): full scan 28 ms to 3.3 ms, aggregate + filter 178 ms to 35 ms. The "Kuzu is 2-5x faster on scans and grouping" statement in `bench/README.md` is **no longer true** for scans and `WITH` aggregation.
- Result correctness: every workload returned results identical to nexgraph's on every engine (checksums compared; PostgreSQL and SQLite on the 10-pair shortest-path subset).

## Method

- **Data:** `bench/gen_data.py`, 100,000 nodes, 1,000,000 directed edges, skewed targets (a few hubs), seed 7. Same query keys for every system.
- **Hardware:** shared sandbox VM, **1 core**, 3 GB RAM. Absolute numbers are indicative; relative ordering is what to read.
- **nexgraph:** v0.25.0, `cargo build --release --features server`, rustc 1.97.1, in-memory `Database::in_memory()`, driver `examples/bench_compare.rs`. Median of 3 full runs.
- **Others:** Python drivers with the same workloads and checksums. Kuzu uses prepared statements; FalkorDBLite is the embedded FalkorDB (Redis module) used through its Python client; DuckDB and SQLite are in-process; PostgreSQL 16 is tuned (`shared_buffers=1GB`, no parallel workers) and accessed over a local Unix socket with prepared statements. SQL engines use B-tree/ART indexes on `edges(a)` and recursive CTEs for reachability and shortest path.
- **Statistic:** median latency per query over the workload's key list (2000 point/1-hop, 500 2-hop, 100 reach-3, 55 paths); scans, group-by and `WITH` are the median of 3 executions.

## Results (median latency; best per row in bold)

| Workload | nexgraph | Kuzu 0.11.3 | FalkorDBLite 0.12.2 | DuckDB 2.0.1 | PostgreSQL 16.15 | SQLite 3.45.1 |
|---|---|---|---|---|---|---|
| Per-call floor (`RETURN 1`) | **0.6 µs** | 26.6 µs | 47.3 µs | 106 µs | 20.2 µs | 1.0 µs |
| Point lookup (indexed) | 3.4 µs | 284 µs | 119 µs | 390 µs | 29.9 µs | **1.6 µs** |
| 1-hop count | 7.5 µs | 740 µs | 228 µs | 505 µs | 37.8 µs | **2.8 µs** |
| 2-hop count | 32.8 µs | 2.6 ms | 172 µs | 1.1 ms | 80.2 µs | **15.4 µs** |
| Reach ≤3 hops, distinct | 1.3 ms | 1.9 ms | **413 µs** | 46.6 ms | 902 µs | 1.2 ms |
| Full scan count (1M edges) | **3.3 ms** | 7.1 ms | 28.2 ms | 7.9 ms | 112 ms | 28.4 ms |
| Group by + top-5 | 32.9 ms | 33.9 ms | 200 ms | **18.3 ms** | 257 ms | 58.8 ms |
| Aggregate + filter (`WITH`) | 34.8 ms | 75.0 ms | 220 ms | **18.6 ms** | 190 ms | 56.1 ms |
| Shortest path (≤12 hops) | 2.0 ms | 20.7 ms | **298 µs** | 457 ms | 4.9 s | 9.5 s |
| Bulk load (1M edges) | 1.2 s | 827 ms | 75.2 s | **332 ms** | 900 ms | 1.5 s |

nexgraph PageRank, 30 iterations on the 1M-edge graph: 266 ms (median of 3). Not run on the other systems.

## How to read this

- **Not like for like on latency.** nexgraph runs in-process from Rust and parses text each call (0.6 µs floor). Kuzu, FalkorDBLite, DuckDB and PostgreSQL are driven through Python bindings or a socket, so their per-call floor is 20-106 µs. The 100x gaps on point and hop queries are mostly call overhead, not engine speed. SQLite, also in-process, shows what a low-overhead engine achieves: it beats nexgraph on point, 1-hop and 2-hop.
- **Where nexgraph genuinely leads:** scans (3.3 ms vs 7.1 ms Kuzu and 7.9 ms DuckDB) and the aggregate-with-filter pipeline against Kuzu (35 ms vs 75 ms). These are engine-level, since the work dominates call overhead.
- **Where it does not:** DuckDB is faster on group-by (18 ms vs 33 ms) and aggregate + filter (19 ms vs 35 ms) and loads 3.6x faster. Kuzu and PostgreSQL load faster too. FalkorDBLite is much faster on 3-hop reach (0.45 ms vs 1.3 ms) and shortest path (0.30 ms vs 1.96 ms), and 6x slower than nexgraph on group-by.
- **SQL baselines:** recursive-CTE shortest path is pathological (PostgreSQL 4.9 s, SQLite 9.5 s per query, 10 pairs each). DuckDB's 457 ms ran all 55 pairs. This shows the value of a native traversal operator for path queries, not a flaw in those databases for their intended use.
- **Durability differs.** nexgraph ran in-memory without WAL; Kuzu, PostgreSQL and FalkorDBLite persist to disk. Persistent-mode nexgraph numbers (file WAL, or PostgreSQL backend) were not measured here; the README reports 430k single-statement commits/s with fsync off and 7.5k/s with fsync on.
- **FalkorDBLite load (75 s)** used batched `UNWIND ... CREATE`, not a bulk loader, and overstates its real load cost.

## Feature comparison (from nexgraph's own docs, not measured)

| | nexgraph 0.25 | Kuzu | FalkorDB | PostgreSQL / SQLite / DuckDB |
|---|---|---|---|---|
| Model | Property **hypergraph** (native hyperedges) | Property graph | Property graph (sparse matrices) | Relational |
| Query language | NQL (Cypher-style subset; no `OPTIONAL MATCH`, `UNWIND`, `MERGE`) | Cypher | openCypher subset | SQL (recursive CTEs for graph work) |
| MVCC / time travel | Yes, `AS OF` | Transactions, no time travel | Limited | PostgreSQL MVCC; no built-in time travel |
| Vector index | HNSW built in | Extension | Yes | pgvector (PostgreSQL) |
| Built-in graph analytics | PageRank, spectral clustering, non-backtracking communities, heat / harmonic PDE solvers | Algorithm extension | PageRank and a few others | None native |
| Scale | Single process, single writer, dataset must fit in RAM | Disk-based, larger than RAM | In-memory (Redis) | Disk-based |
| Security | TLS, Argon2id auth, roles, per-tenant isolation, audit log, encryption at rest | Minimal | Redis ACLs | Mature |

Kuzu, FalkorDB and SQL engine capability cells are from general knowledge and were not verified here.

## Limitations

- **Not tested:** Neo4j, Memgraph, ArangoDB, TigerGraph, JanusGraph (no Docker or download access in the sandbox). `bench/run_bolt.py` exists for Neo4j / Memgraph but remains unrun.
- Single run per competitor on a shared single core; nexgraph spread across its 3 runs was up to 30% on `WITH` aggregation (30-40 ms).
- One 100k-node / 1M-edge graph. No result here describes behavior at 100M+ edges, concurrent writers, or larger-than-RAM data, where nexgraph's design (in-memory, single writer) is the binding constraint.
- PostgreSQL and SQLite shortest path used the first 10 of the 55 pairs.
- Only the workloads in the existing harness were run (no writes under concurrency, no vector or spectral comparison).

## Reproduce

```
python3 bench/gen_data.py DATA 100000 1000000
cargo run --release --features server --example bench_compare -- DATA nexgraph.json
python3 -I bench/run_kuzu.py DATA kuzu.json
python3 run_sql.py {duckdb|pg|sqlite} DATA out.json     # PATHN=10 limits shortest-path pairs
python3 run_falkor.py DATA falkor.json
python3 bench/compare.py nexgraph.json kuzu.json
```

`run_sql.py`, `run_falkor.py` and `common.py` were written for this report (not part of the archive).
