# Benchmarks

Same graph, same query text, same checksums for every system. A system only counts as having run a workload if its result matches the others (`compare.py` prints `match` / `DIFFER`).

```
python3 bench/gen_data.py DATA 100000 1000000          # 100k nodes, 1M edges, skewed targets, seed 7
cargo run --release --features server --example bench_compare -- DATA nexgraph.json
python3 -I bench/run_kuzu.py DATA kuzu.json            # pip install kuzu
python3 bench/run_bolt.py DATA neo4j.json bolt://localhost:7687 neo4j USER PASS     # Neo4j or Memgraph
python3 bench/compare.py nexgraph.json kuzu.json
```

## Results (2 vCPU, 7 GB; nexgraph = median of 3 runs, Kuzu = 1 run; medians)

| workload | nexgraph | Kuzu 0.11.3 | results |
|---|---|---|---|
| noop (`RETURN 1`) | 0.6 µs | 98.7 µs | – |
| point lookup | 3.7 µs | 403 µs | match |
| 1-hop count | 8.3 µs | 790 µs | match |
| 2-hop count | 32 µs | 3.1 ms | match |
| reach ≤3 hops, distinct | 1.2 ms | 2.3 ms | match |
| shortest path | 1.6 ms | 21 ms | match |
| full scan count (1M edges) | 28 ms | 8.4 ms | match |
| group by + top-5 | 182 ms | 41.7 ms | match |
| `WITH` aggregate + filter | 178 ms (was 500 before v0.21 fusion and fast-path work) | 77 ms | match |
| bulk load (1M edges) | 2.0 s | 0.66 s | – |

Run-to-run spread on this shared VM is large for the full-scan row (19–28 ms across runs).

## How to read this
* **Not like for like on latency.** nexgraph runs in-process from Rust. Kuzu is driven from Python (its own `RETURN 1` costs ~99 µs through the binding, even prepared), so the 100x gaps on point and hop queries are mostly a call-overhead difference. Do not quote them as engine speed.
* **Where Kuzu is simply faster:** full scans, grouping and aggregation (2–5x), and bulk load (3x). It is a columnar, vectorised engine; nexgraph is row-at-a-time.
* **Where nexgraph holds up:** small, indexed, selective traversals and shortest paths, which is the shape of many application queries. Even allowing ~100 µs per Kuzu call, hop2 and path are still faster.
* The graph is small (100k nodes). Nothing here says how either system behaves at 100M+ edges.
* **Neo4j and Memgraph were not measured.** The build sandbox had no Docker daemon and could not reach their download hosts. `run_bolt.py` is written for them but has never been run; expect to fix small things (it passes a syntax check only).
* One run on a shared 2-core VM. Repeat before relying on any single number.
