# How nexgraph works

nexgraph is an embedded, in-memory property-hypergraph database with a write-ahead log for durability. This note follows a piece of data from the query text to disk and back.

## 1. Data model

* **Node**: labels plus a property map.
* **Edge = directed hyperedge**: a label, a weight (`f64`, default 1.0), properties, and two member sets, `tail` and `head`.
  * Binary edge: `tail=[a]`, `head=[b]`.
  * Directed hyperedge: `tail=[a,b]`, `head=[c]`. Every tail member reaches every head member.
  * Undirected hyperedge: all members in `tail`, `head` empty. It is crossed only by undirected patterns `-[]-` and connects every member to every other.
* Nodes and edges share one `u64` id space.
* Property values: null, bool, int, float, string, list, f32 vector. Node and edge references are query-time values and cannot be stored.
* Deleting a node deletes every edge and hyperedge it belongs to (the whole hyperedge, not just the membership).

## 2. Storage: MVCC version chains

Each entity holds a list of immutable versions `{from_ts, to_ts, body}`. A write never changes a body. It closes the current version (`to_ts = commit ts`) and appends a new one. A reader at timestamp `T` sees the version where `from <= T < to`.

This gives you:

* **Snapshot isolation.** A reader pins `T = last committed ts` and is unaffected by later commits.
* **Time travel.** `AS OF n` or `db.read_at(n, ..)` simply reads at an older `T`.
* **Cheap deletes.** A delete just closes the last version.

Node adjacency is a list of incident edge ids per node, filtered by visibility at read time.

## 3. Indexes

| Index | Structure | Notes |
|---|---|---|
| Label | label → set of ids | always on |
| Property equality | (label, key) → value → set of ids | `CREATE INDEX ON :L(k)`; ints and floats share a key space |
| Vector | HNSW graph per (label, key) | `CREATE VECTOR INDEX ... DIM n METRIC cosine\|l2` |

Indexes only ever **add** entries. Every lookup re-checks the candidate against the reader's snapshot, so stale entries from old versions are harmless. For vectors each HNSW point remembers its `(id, version)`; hits whose version is not visible, or whose vector no longer matches, are dropped, and the search over-fetches to compensate. `exact = true` brute-forces instead.

## 4. Writes and durability

```
begin()  ──►  buffer Ops  ──►  commit():
   1. take the writer mutex (one writer at a time)
   2. validate ops against current state   (nothing applied yet)
   3. append CRC-framed record to wal.log, fsync   ◄── durability point
   4. apply ops under the store write lock
   5. publish new last_ts
```

* Ops are logical and deterministic: `CreateNode`, `CreateEdge`, `Update`, `Delete`, `CreatePropIndex`, `CreateVectorIndex`.
* A failed validation applies nothing. A dropped or rolled-back transaction leaves no trace.
* **Recovery on open**: load `checkpoint.bin` if present, then replay `wal.log`, skipping commits already in the checkpoint. A damaged frame at the very end is a torn write and is truncated. A bad checksum with data after it is reported as corruption instead of being skipped.
* **Checkpoint** writes the live state to a temp file, fsyncs, renames atomically, truncates the WAL and compacts history in memory. After it, `AS OF` earlier than the checkpoint is refused.
* Sync mode: `Always` (fsync per commit) or `Never` (OS flushes).

## 5. Concurrency

The store sits behind a read-write lock. Readers share it and never block each other. A commit takes the write lock only for step 4, which is short. Each query holds one read guard for its whole run, so it sees one consistent snapshot. A long read query delays commits.

## 6. Query pipeline (NQL)

```
text ─► lexer ─► parser ─► Statement ─► executor ─► QueryResult
```

* **Read statements** run under one snapshot (`AS OF` picks an old one).
* **Write statements** open a transaction, execute against a snapshot, collect ops, release the read lock, then commit. Writes are not visible to later clauses in the same statement.
* **Execution (v0.3)**: `MATCH ... RETURN` streams matches into a projector with hash aggregation and no row materialisation; bindings are added and undone during the depth-first walk; directed hops use per-node `outs`/`ins` lists.
* **Flat metadata (v0.4)**: `Inner.cm` is a dense array with one small record per entity (latest-version timestamps, dictionary-encoded label, binary-edge endpoints). Hops, label checks and counting use it when the reader's timestamp is at or after the record's `from`; otherwise the version chain is used. `MATCH (a)-[e]->(b) RETURN <a keys>, count(*)` is counted per start node (in parallel for large scans) instead of row by row.
* **Matching**: a path pattern picks the cheaper end to start from (bound variable, then an indexed property, then a label, then all nodes) and expands by backtracking. Edges are not reused within one path. Variable-length hops (`*1..3`) are depth-first. A 5M-step budget aborts runaway queries.
* **Projection**: `RETURN` supports `DISTINCT`, aggregates (`count`, `sum`, `avg`, `min`, `max`, `collect`) with implicit grouping, `ORDER BY` (it may use grouping expressions), `SKIP` and `LIMIT`.
* **Procedures**: `CALL name(args) YIELD cols` joins rows with the procedure output.

Example:

```
MATCH (a:Person)-[:KNOWS*1..3]->(b) WHERE b.age > 30
RETURN a.name, count(*) AS n ORDER BY n DESC LIMIT 10

CREATE HYPEREDGE :Reaction {weight:2.5} FROM (a, b) TO (c)

CALL spectral.cluster(3) YIELD node, cluster RETURN cluster, count(*)
```

## 7. Analytics

* **Plain algorithms** (`algo.*`) work on a weighted graph projected from the snapshot. Directed edges go tail to head; an undirected hyperedge becomes a weighted clique. They are PageRank, connected components, BFS shortest path and Dijkstra.
* **Spectral procedures** (`spectral.*`) build a `spectral_hypergraph::SpectralHypergraph` from every edge with at least 2 distinct members and positive weight. Nodes in no such edge are left out.
  * They use the normalised hypergraph Laplacian (Zhou, Huang and Schoelkopf).
  * Up to 1,500 vertices it uses a dense eigensolver, and above that matrix-free Lanczos.
  * Procedures: `fiedler()`, `cluster(k)` (k-means on the embedding), `eigenvalues(k)` and `embedding(k)`.
  * Every hyperedge in the database takes part, including ones added for other purposes.
  * On a graph with several components the zero eigenvalue repeats, so the Fiedler vector is not meaningful.

### 7b. Physics and non-backtracking procedures (v0.2)

* **`physics.*`** works on the walk `P = Dv^-1 H W De^-1 H^T`, applied matrix-free (edge mean, then weighted scatter back to members). Heat is `e^-t sum_k t^k P^k / k!` (uniformisation, long times split into steps), the `'sym'` mode conjugates by `Dv^1/2` to give the normalised Laplacian used by SHPINN. Harmonic extension clamps the known nodes and iterates `u <- P u` on the rest until the change falls below the tolerance; nodes that no known node can reach are `null`.
* **`spectral.communities`** builds the unweighted clique-expansion graph, estimates `rho_B` with `nbsc` (Arnoldi on the matrix-free Hashimoto linearisation), forms the Bethe Hessian `H(r) = (r^2-1)I - rA + D` at `r = sqrt(rho_B)`, counts its negative eigenvalues for k, and k-means-clusters the eigenvectors (first, trivial one skipped). Dense solver up to 1500 nodes, Lanczos above with dense fallback.

## 8. Source map

| File | Role |
|---|---|
| `value.rs` | values, varint codec, CRC-32 |
| `model.rs` | `Body`, `Op`, `Commit` and their binary encoding |
| `store.rs` | version chains, indexes, `View` (snapshot reads, traversal, vector search) |
| `db.rs` | `Database`, `Txn`, WAL, checkpoint, recovery |
| `hnsw.rs` | vector index |
| `query/parser.rs`, `query/exec.rs` | NQL |
| `algo.rs` | graph algorithms and spectral procedures |
| `physics.rs` | heat diffusion and harmonic extension |
| `community.rs`, `kmeans.rs` | Bethe Hessian communities, clustering |

## 9. Known limits

* Everything is held in memory.
* Only one process per database directory is supported (not enforced), and only one writer at a time.
* The planner is a heuristic, not cost-based.
* Equality indexes only; no `WITH`, `OPTIONAL MATCH` or `MERGE`.
* General multi-row pattern scans are row-at-a-time; one-hop count aggregates are factorised (v0.4).
* Only rustc 1.97 was compiled, so the 1.75 minimum is unchecked.


## ALM procedures (v0.11)

Per cash flow `cf` at `t = month/12` with discount factor `df`: implied annual-effective yield `y = df^(-1/t) - 1`; a parallel shift `d` gives
`cf (1+y+d)^(-t)`; modified duration `sum(t cf df/(1+y))/PV`; convexity `sum(t(t+1) cf df/(1+y)^2)/PV`; DV01 = value x duration x 1e-4.
Holdings are revalued as `-D d + C d^2/2` unless their cash flows are supplied to `alm.rate_shock`. Leverage-adjusted duration gap =
`D_A - (L/A) D_L`. VaR(alpha) = minus the (1-alpha) quantile of P&L; CTE = minus the mean of the worst ceil((1-alpha)n) paths.
Flows in month 0 keep their stated discount factor and take no shift. Segments with non-positive PV have no duration or convexity.
