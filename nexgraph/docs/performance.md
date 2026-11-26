# Analytics performance: where the gap to Kuzu comes from, and the plan

Measured on 100k nodes / 1M edges, 2 vCPU (`examples/prof_group.rs`, `bench/`). nexgraph already beats Kuzu on small indexed lookups and shortest paths (partly because it runs in-process); Kuzu is 2–5x faster on scans, grouping and loading.

## What the profile shows
| query | time | reading |
|---|---|---|
| `count(*)` over edges, fast path | 20–28 ms | ~20–28 ns/row |
| same + a WHERE that cannot use the fast path | ~285 ms | ~285 ns/row of interpretive overhead |
| `sum(a.k)`, `a` sequential | +14 ns/row | property read is cheap when memory is warm |
| `sum(b.k)`, `b` scattered | **+545 ns/row** | cache misses: entity → version list → `Arc<Body>` → `BTreeMap<String,Value>` → key |
| group by node, 100k groups | ~1 µs per group | several heap allocations per new group (key, hash key, accumulators) |

## Done in v0.21
* `MATCH … WITH` is fused with its MATCH and streamed, like `MATCH … RETURN`.
* The factorised fast path now accepts `count(b)` / `count(r)` and a WHERE that mentions only the start node (tests: `tests/factorized.rs` checks it against the streaming path).
* ORDER BY on output columns no longer rebuilds variable bindings per group; group tables are pre-sized.
* Effect: the WITH workload 500 → ~180 ms.

## Remaining plan (estimates, not measurements), in order
1. **Columnar storage for the latest version**: typed columns indexed by id (null bitmap, dictionary-encoded strings); keep the version chain for time travel. Targets the ~500 ns scattered read. Largest change; touches the write path.
2. **Batched execution**: ~1,000-row vectors with selection vectors; morsel-driven parallel streaming (today only the count fast path is multi-threaded); typed hash aggregation (no per-group heap allocation).
3. **Dense adjacency arrays (CSR)** for traversals; the per-node `outs`/`ins` lists approximate this today.
4. **Bulk load path** (load is 3x slower; not profiled — per-operation transaction and index overhead is a guess).

Sources: [Kuzu technical report](https://cs.uwaterloo.ca/~ssalihog/papers/kuzu-tr.pdf) (columnar properties, CSR, factorised vectors, morsel parallelism); [Kuzu, CIDR 2023](https://vldb.org/cidrdb/2023/kuzu-graph-database-management-system.html); [Leis et al., morsel-driven parallelism](https://15721.courses.cs.cmu.edu/spring2016/papers/p743-leis.pdf); [Kersten et al., compiled vs vectorized](https://ir.cwi.nl/pub/28470/28470.pdf).
