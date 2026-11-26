# nexgraph User Guide

**Version covered: nexgraph 0.25.0** (Rust, edition 2021, MSRV 1.85)

nexgraph is an embedded **property-hypergraph database** with a Cypher-style query language (NQL), built-in graph/spectral/statistical analytics, optional PostgreSQL durability, and an optional multi-tenant HTTPS server with a web UI. This guide shows how to install it, load data, query it, run the analytics, and operate the server.

> **Read this first: status and honesty notes.** The code base is largely AI-generated and verified by its own test suite. It has **not** been independently audited or penetration-tested, and it is **not** HIPAA/GDPR/PCI certified. The PostgreSQL, HTTPS-server and web-UI paths were tested on Linux with PostgreSQL 16 only. Everything lives in RAM, so your data set must fit in memory. See [Limits](#18-limits-and-honest-caveats) before putting real or regulated data into it.

---

## Contents

1. [Concepts in five minutes](#1-concepts-in-five-minutes)
2. [Install and build](#2-install-and-build)
3. [Quick start](#3-quick-start)
4. [Data model](#4-data-model)
5. [Storage, durability and time travel](#5-storage-durability-and-time-travel)
6. [The `nexgraph` shell](#6-the-nexgraph-shell)
7. [NQL query language](#7-nql-query-language)
8. [Loading data](#8-loading-data)
9. [Graph, spectral and physics procedures](#9-graph-spectral-and-physics-procedures)
10. [Statistics and cyber-security procedures](#10-statistics-and-cyber-security-procedures)
11. [Domain analytics modules](#11-domain-analytics-modules)
12. [GAAP ledger](#12-gaap-ledger)
13. [Using nexgraph from Rust](#13-using-nexgraph-from-rust)
14. [PostgreSQL backend](#14-postgresql-backend)
15. [Server, tenants, roles and HTTP API](#15-server-tenants-roles-and-http-api)
16. [Web UI, Power BI and BI export](#16-web-ui-power-bi-and-bi-export)
17. [Privacy tooling: data policy, encryption, de-identification](#17-privacy-tooling-data-policy-encryption-de-identification)
18. [Limits and honest caveats](#18-limits-and-honest-caveats)
19. [Troubleshooting](#19-troubleshooting)
20. [Appendix: layout, tests, further reading](#20-appendix-layout-tests-further-reading)

---

## 1. Concepts in five minutes

| Idea | What it means in nexgraph |
|---|---|
| **Hypergraph** | An edge can connect *many* nodes to *many* nodes. A normal "A knows B" edge is just the 1-to-1 case. A meeting of five people is **one** hyperedge, not ten pairwise edges. |
| **Property graph** | Nodes carry labels and key/value properties. Edges carry a label, a numeric `weight` (default `1.0`) and properties. |
| **NQL** | The query language. It is a Cypher dialect (about 49 of 59 queries in the project's own compatibility corpus run; it is not GQL and not the openCypher TCK). |
| **Embedded** | You use it as a Rust library (`Database::open("./mydb")`) or through the `nexgraph` shell. There is no server unless you build and run `nexgraph-server`. |
| **MVCC + time travel** | Every commit has a timestamp. Readers see a consistent snapshot and never block each other. `AS OF <ts>` reads an older state. |
| **Analytics inside queries** | `CALL spectral.cluster(3)`, `CALL algo.pagerank()`, `CALL cyber.beaconing(...)`, `CALL gaap.balance_sheet(...)` and many more run directly on the live graph and return rows you can filter, aggregate and join in NQL. |

---

## 2. Install and build

### Requirements

* Rust **1.85 or newer** (`rustup update`). Only rustc 1.97 on Linux was actually compiled by the author.
* The project directory must sit **beside** `human_connectome_rnn/` because `Cargo.toml` references its crates by path (`spectral_hypergraph`, `nbsc`, `krylov_ds`):

```
workspace/
├── nexgraph/
└── human_connectome_rnn/      # included in the archive, used unmodified
```

* PostgreSQL (16 tested) only if you want the PostgreSQL backend or the server.
* For the web UI: the `wasm32-unknown-unknown` target and `trunk`.

### Build

```bash
cd nexgraph
cargo build --release                         # shell: target/release/nexgraph
cargo build --release --features server       # also builds nexgraph-server
cargo test  --release                         # full test suite (PostgreSQL tests are skipped without a URL)
```

On WSL2/Windows the same commands work from a WSL shell. Executables get the `.exe` suffix when built natively on Windows.

### Cargo features

| Feature | Default | Adds |
|---|---|---|
| `spectral` | yes | `spectral.*` and `physics.*` procedures (`spectral_hypergraph`, `nalgebra`) |
| `nonbacktracking` | yes | `spectral.communities`, `spectral.nb_radius` (`nbsc`, no Burn dependency) |
| `ingest` | yes | CSV / JSON-lines / Suricata / Zeek loaders, `.import` in the shell |
| `postgres` | yes | PostgreSQL durability with verified TLS (implies `crypto`) |
| `security` | yes | Users, Argon2id passwords, tokens, roles, tenants, audit log |
| `crypto` | via `postgres` | Encryption at rest (XChaCha20-Poly1305), HMAC/HKDF, de-identification helpers |
| `server` | **no** | `nexgraph-server` HTTPS API |
| `connectome` | no | Imports a `human_connectome_rnn` connectome (`examples/import_connectome.rs`) |

`--no-default-features` gives a dependency-free core (storage, NQL, `algo.*`, `stats.*`, `cyber.*`, domain modules). A build without `postgres` refuses `postgres://` targets with a clear message.

---

## 3. Quick start

### From the shell

```bash
# the directory is created on first open
printf '%s\n' \
  "CREATE (a:Person {name:'Ada'}), (b:Person {name:'Bob'}), (a)-[:KNOWS {since:1843}]->(b);" \
  "MATCH (a)-[:KNOWS]->(b) RETURN a.name, b.name;" \
  | ./target/release/nexgraph ./mydb
```

Without a target argument the database lives in memory and disappears on exit.

### From Rust

```rust
use nexgraph::Database;

let db = Database::open("./mydb")?;               // or Database::in_memory()
db.query("CREATE (a:Person {name:'Ada'}), (b:Person {name:'Bob'}), (a)-[:KNOWS]->(b)")?;
db.query("MATCH (a),(b) WHERE a.name='Ada' AND b.name='Bob' \
          CREATE HYPEREDGE m:Meeting {topic:'plan'} OVER (a,b)")?;
let r = db.query("MATCH (a)-[:KNOWS*1..3]->(b) RETURN a.name, count(*) AS n ORDER BY n DESC")?;
println!("{}", r.to_table());
```

Add it as a path dependency: `nexgraph = { path = "../nexgraph" }`.

---

## 4. Data model

* **Node**: zero or more labels plus a property map.
* **Edge = directed hyperedge**: a label, a `weight` (`f64`, default `1.0`), properties, and two member sets, **tail** and **head**.

| Kind | Shape | Meaning |
|---|---|---|
| Binary edge | `tail=[a]`, `head=[b]` | ordinary directed edge |
| Directed hyperedge | `tail=[a,b]`, `head=[c]` | every tail member reaches every head member |
| Undirected hyperedge | all members in `tail`, `head` empty | crossed only by undirected patterns `-[]-`; connects every member to every other |

* Nodes and edges share one `u64` id space.
* Property values: `null`, bool, int, float, string, list, `f32` vector. Node/edge references exist only at query time and cannot be stored as properties.
* **Deleting a node deletes every edge and hyperedge it belongs to** (the whole hyperedge, not just its membership).
* **Edge uniqueness**: within one path an edge is never reused (Cypher relationship-isomorphism).
* **Weight** is first-class: `CREATE ... {weight: 2.5}` or `SET e.weight = 2.5`. Analytics that need strengths or probabilities (PageRank, Dijkstra, `cyber.propagate_risk`, spectral) use it.

---

## 5. Storage, durability and time travel

### Three ways to open a database

| Target | How | Durability |
|---|---|---|
| In memory | `Database::in_memory()` / shell with no target | none |
| Directory | `Database::open(dir)` / `nexgraph ./mydb` | CRC-32 framed write-ahead log (`wal.log`) plus atomic `checkpoint.bin`; fsync per commit by default |
| PostgreSQL | `Database::open_postgres(url)` / `nexgraph "postgres://..."` | commit log and checkpoints in a PostgreSQL schema, optional SQL-readable mirror tables |

Directory databases:

* `Database::open_with(dir, SyncMode::Never)` skips the per-commit fsync (durable against a process crash, not power loss). Roughly 430k commits/s vs 7.5k/s for single-statement commits in the author's benchmark (indicative only).
* A directory has an **enforced exclusive lock** (`<dir>/LOCK`). A second open, from this or another process, fails with an "already open" error. The lock disappears when the process exits.
* **Recovery on open** loads `checkpoint.bin`, then replays `wal.log`. A damaged final frame is treated as a torn write and truncated. A bad checksum with data after it is reported as corruption, never silently skipped.

### Checkpoint = vacuum

`.checkpoint` (shell) or `db.checkpoint()`:

1. writes the live state to a temp file, fsyncs, atomically renames it,
2. truncates the WAL,
3. **discards version history older than the latest commit**.

After a checkpoint, `AS OF` an earlier timestamp returns an error. Nothing checkpoints automatically, so on long-lived databases run it periodically (on PostgreSQL the `commits` table keeps growing until you do).

### Time travel

Every result carries `ts`, the snapshot (reads) or commit (writes) timestamp. Prefix any read statement with `AS OF <ts>`:

```
MATCH (n:Host) RETURN count(*);               -- note the ts printed in the footer, e.g. ts 12
...later changes...
AS OF 12 MATCH (n:Host) RETURN count(*);      -- the graph as it was at commit 12
AS OF 12 CALL gaap.income_statement('Acme','2024-01-01','2024-12-31');
```

`.drift OLD_TS NEW_TS` (shell) compares two snapshots: new/removed nodes, links to new nodes, **novel links between pre-existing nodes** (a lateral-movement signal) and link Jaccard.

---

## 6. The `nexgraph` shell

```
nexgraph [TARGET] [--schema NAME] [--no-mirror] [--read-only] [--async-commit]
```

| Argument | Meaning |
|---|---|
| `TARGET` | directory, `postgres://` / `postgresql://` URL, or omitted for in-memory. With `--pg` the URL comes from `$NEXGRAPH_PG_URL`. |
| `--schema NAME` | PostgreSQL schema holding the database (default `nexgraph`) |
| `--no-mirror` | don't maintain the relational mirror tables |
| `--read-only` | open as a replica; `.refresh` follows the writer |
| `--async-commit` | `synchronous_commit=off` for the session (faster; last commits may be lost on a server crash) |

**Input rules**

* Statements end with `;` or a blank line. Multi-line statements are fine.
* The shell reads from stdin, so piping files works: `nexgraph ./mydb < script.nql`. The repository's `one.nql` is a small example.
* It prints no banner or prompt unless `NEXGRAPH_INTERACTIVE` is set (`NEXGRAPH_INTERACTIVE=1 nexgraph ./mydb`).
* Exit status is `1` if any statement failed.

**Meta commands** (must be at the start of a line, outside a statement)

| Command | Effect |
|---|---|
| `.quit`, `.exit` | leave |
| `.mode table\|csv\|json` | output format (default `table`) |
| `.stats` | engine counters and the last timestamp |
| `.checkpoint` | checkpoint (see above) |
| `.drift OLD_TS NEW_TS` | snapshot comparison |
| `.powerbi DIR` | export typed CSV tables, `catalog.json` and a Power Query script (see [section 16](#16-web-ui-power-bi-and-bi-export)) |
| `.import ...` | load CSV / events (see [section 8](#8-loading-data)) |
| `.mirror`, `.verify`, `.rebuild`, `.refresh` | PostgreSQL only: mirror stats, compare mirror to graph, recreate it, follow the writer (replicas) |

Write statements print `ok <stats> (ts N)`. Read statements print a table (or CSV/JSON) and `(N rows, ts T)`. CSV output neutralises spreadsheet-formula injection by prefixing a `'` to text cells starting with `= + - @`, tab or CR (`QueryResult::to_csv_raw` keeps exact text).

---

## 7. NQL query language

### 7.1 Creating data

```
CREATE (a:Person {name:'Ada', age:36}), (b:Person {name:'Bob'}), (a)-[:KNOWS {since:1843, weight:0.5}]->(b)

-- undirected hyperedge over existing nodes
MATCH (a:Person {name:'Ada'}), (b:Person {name:'Bob'}), (c:Person {name:'Cy'})
CREATE HYPEREDGE m:Meeting {topic:'plan'} OVER (a, b, c)

-- directed hyperedge
MATCH (a:Person {name:'Ada'}), (b:Person {name:'Bob'}), (d:Person {name:'Di'})
CREATE HYPEREDGE :Reaction {rate:0.5, weight:2.5} FROM (a, b) TO (d)
```

### 7.2 Reading

```
MATCH (p:Person) WHERE p.age > 30 RETURN p.name AS n ORDER BY p.age DESC LIMIT 2

MATCH (a:Person)-[:KNOWS*1..3]->(b) WHERE b.name STARTS WITH 'B'
RETURN a.name, count(*) AS n ORDER BY n DESC LIMIT 10

MATCH (a:Person {name:'Ada'})<-[:KNOWS]-(x) RETURN x          -- incoming
MATCH (a:Person {name:'Bob'})-[:KNOWS]-(x) RETURN x.name      -- either direction
MATCH (a:Person {name:'Ada'})-[:Meeting]-(x) RETURN x.name    -- across an undirected hyperedge

MATCH HYPEREDGE h:Meeting WHERE size(members(h)) > 2 RETURN h.topic, members(h)
MATCH HYPEREDGE h:Reaction RETURN size(tail(h)), size(head(h))
```

* Directed edges lead from every tail member to every head member. An undirected hyperedge (empty head) is crossed only by undirected patterns `-[]-`.
* **`MATCH (n:Label)` where `Label` is only an edge/hyperedge label is an error** that tells you to use `MATCH HYPEREDGE h:Label ...`. A label that is unused everywhere returns 0 rows.
* A 5-million-step traversal budget aborts runaway variable-length queries.

### 7.3 Clauses and expressions

| Area | Supported |
|---|---|
| Clauses | `MATCH`, `OPTIONAL MATCH`, `WHERE`, `WITH` (incl. aggregation, `ORDER BY`/`SKIP`/`LIMIT`/`WHERE`), `UNWIND`, `MERGE` (+ `ON CREATE SET` / `ON MATCH SET`), `CREATE`, `SET`, `SET v += {...}`, `REMOVE v.k`, `DELETE`, `RETURN`, `CALL ... YIELD` |
| `RETURN` | `DISTINCT`, `AS`, `ORDER BY` (may use grouping expressions), `SKIP`, `LIMIT`, implicit grouping |
| Aggregates | `count`, `sum`, `avg`, `min`, `max`, `collect`, `stdev`, `median` |
| Operators | `= <> < <= > >=`, `AND OR NOT`, `IN`, `IS NULL`, `STARTS WITH`, `+ - * / ^`, list indexing `l[i]`, `CASE` (simple and searched) |
| Functions | `id`, `size`, `members`, `tail`, `head`, `toLower toUpper toInteger toBoolean round floor ceil sign exp log log10 exists timestamp trim ltrim rtrim substring left right replace split reverse range` |
| Parameters | `$name`, passed via `Database::query_with(text, &Params)` or the HTTP `params` object |
| Time travel | `AS OF <ts>` before a read statement |
| Odd names | backtick-quote them: ``e.`Land Area (Acres)` `` |
| `YIELD *` | returns every column of a procedure; the quickest way to discover a procedure's output |

Semantics worth knowing:

* `/` on two integers is integer division; `^` always returns a float; `range()` is capped at 1,000,000 values.
* `OPTIONAL MATCH ... WHERE` filters the optional match, not the row.
* `MERGE` reads from the statement's start snapshot; it remembers what it created itself. A `null` property in a `MERGE` pattern is an error.
* Within one statement, writes are not visible to later clauses of that statement.
* `SET v = null` removes the property.

### 7.4 Not supported (fails with a clear error)

`UNION`, `EXISTS { ... }` and pattern predicates, path variables (`p = (...)`) and `length(p)`, list comprehensions, aggregates nested in expressions (`head(collect(x))` – use `WITH`), `SET v:Label` (use the Rust API's `add_label`), `properties(v)`, temporal functions such as `date()`, `CREATE CONSTRAINT`, `SET v = {...}`, `WITH *`. There is no cost-based planner.

### 7.5 Indexes

```
CREATE INDEX ON :Host(ip)                         -- property equality index
CREATE INDEX FOR (v:Host) ON (v.ip)               -- openCypher spelling
CREATE VECTOR INDEX ON :Doc(emb) DIM 8 METRIC cosine      -- or METRIC l2
```

* The label index is always on.
* Equality predicates (`{ip:'1.2.3.4'}` or top-level `WHERE n.ip = 'x'`) and range predicates (`<`, `<=`, `>`, `>=`, either side, `AND`-ed) use property indexes.
* Indexes are MVCC-safe: candidates are re-verified at the reader's snapshot.
* Keyed CSV imports create the property index for you.

### 7.6 Vector search

```
CALL vector.search('Doc', 'emb', [0.1, 0.2, ...], 10)          YIELD node, distance RETURN node.title, distance
CALL vector.search('Doc', 'emb', [0.1, 0.2, ...], 10, true)    -- exact brute force
```

HNSW is approximate (recall@10 about 0.95 in the author's 20k × 32-d test), in memory and rebuilt on open.

---

## 8. Loading data

### 8.1 Shell `.import`

```
.import nodes  LABEL FILE.csv [KEYCOL]
.import edges  EDGELABEL FILE.csv SRCCOL DSTCOL NODELABEL NODEKEY [WEIGHTCOL]
.import events suricata|zeek FILE.jsonl
.import hyper  EDGELABEL FILE.csv KEYPROP COL:Label[,COL:Label...] [TIMECOL[,TIMECOL...]|-] [options]
```

| Command | What it does |
|---|---|
| `nodes` | one node per row, cells become typed properties. With `KEYCOL` the import is keyed (rows with the same key update one node; a property index is created; `duplicate_keys` in the report counts repeats within the file). |
| `edges` | one binary edge per row between nodes found or created by `NODEKEY` value in `SRCCOL`/`DSTCOL`; other cells become edge properties. |
| `events` | JSON-lines network telemetry. Suricata `eve.json` (flow, alert, dns, http, tls) and Zeek JSON (`conn.log`, `dns.log`). Produces `(:Host {ip})-[:FLOW {...}]->(:Host)` and `(:Host)-[:ALERT {...}]->(:Host)`. |
| `hyper` | **one hyperedge per row** over the entity columns you list. Each `COL:Label` cell becomes (or reuses) a member node of that label, stored under property `KEYPROP`. Other cells become typed edge properties. Time columns are stored as epoch seconds. |

`hyper` options (after the optional time columns, or `-` for none):

| Option | Effect |
|---|---|
| `dedup=row` | drop exact duplicate rows |
| `dedup=key:COL` | drop rows whose `COL` value was already imported (first wins; conflicting content counted as a key conflict) |
| `props` | also store each member column as an **edge property**. Needed by `killchain.*`, `alm.*`, `land.*`, `uw.*` and for `WHERE e.Attack_Vector = 'Email'`. Costs memory. |
| `single` | keep rows with only one member (default skips rows with fewer than two) |
| `dates=iso\|mdy\|dmy` | how time columns are written (`11/8/2016` is ambiguous, so nexgraph never guesses) |

Examples:

```
.import nodes Host hosts.csv ip
.import edges CONNECTS flows.csv src_ip dst_ip Host ip bytes
.import events zeek conn.json
.import hyper ATTACK attacks.csv name attack_type:AttackType,industry:Industry,attacker_ip:Ip,target_ip:Ip timestamp dedup=row props
```

Each import prints an `ImportReport` (`rows_read`, `rows_skipped`, `nodes_created`, `nodes_updated`, `edges_created`, `duplicates_skipped`, `key_conflicts`, `duplicate_keys`, `first_error`, `ts`). **Check `rows_skipped` and `first_error`** – bad rows are skipped and counted rather than aborting the load. Each import is one atomic transaction, so very large files need memory roughly proportional to the file.

Choosing between binary edges and hyperedges: if every row is an *event* with several entities (attack, incident, loan, land record), one hyperedge per row keeps the co-occurrence structure that a star of binary edges destroys, and is what the domain modules expect.

### 8.2 Rust API (`nexgraph::ingest`)

`import_csv_nodes`, `import_csv_nodes_keyed(db, label, csv, key_col, key_prop)`, `import_csv_edges(&EdgeCsv)`, `import_csv_hyperedges(&HyperCsv)`, `import_csv_hyperedges_with(&HyperCsv, &HyperOpts)`, `import_events(db, jsonl, Preset)`. `HyperOpts` also carries qualified member keys (so `Taluk-A` under two districts stays two nodes), chunked commits and tab-separated input; see the rustdoc on `HyperOpts`.

Timestamps in CSVs stay text unless named in the time columns. `cyber.beaconing`, `stats.timeseries` and `stats.spikes` accept either epoch seconds or ISO 8601 strings and raise an error (rather than returning nothing) if a property exists but nothing parses.

### 8.3 Programmatic transactions

```rust
use nexgraph::props;

let mut tx = db.begin();
let a = tx.create_node(&["Person"], props([("name", "Ada")]))?;
let b = tx.create_node(&["Person"], props([("name", "Bob")]))?;
tx.create_edge("KNOWS", a, b, 1.0, Default::default())?;
tx.create_hyperedge("Meeting", &[a, b], &[], 1.0, Default::default())?;   // empty head = undirected
tx.commit()?;            // dropping the Txn or tx.rollback() discards everything
```

Other `Txn` methods: `set_prop`, `set_props`, `set_weight`, `add_label`, `remove_label`, `delete`, `create_property_index`, `create_vector_index`.

---

## 9. Graph, spectral and physics procedures

Call with `CALL name(args) YIELD cols`, then continue the query with `RETURN`, `WHERE`, etc. All procedures read the caller's snapshot and combine with `AS OF`. Unknown procedure names produce an error that lists every available procedure.

```
CALL algo.pagerank() YIELD node, score RETURN node.name, score ORDER BY score DESC LIMIT 10
MATCH (a:Person {name:'Ada'}), (d:Person {name:'Di'}) CALL algo.dijkstra(a, d) YIELD path, distance RETURN distance
CALL spectral.cluster(3) YIELD node, cluster RETURN cluster, count(*)
```

### 9.1 `algo.*` (hyperedges are projected to weighted pairs; directed edges go tail → head, undirected hyperedges become weighted cliques)

| Procedure | Yields |
|---|---|
| `algo.pagerank([damping=0.85[, iters=50]])` | `node, score` |
| `algo.components()` | `node, component` |
| `algo.shortestPath(source, target)` | `path, length` (BFS) |
| `algo.dijkstra(source, target)` | `path, distance` (uses `weight`) |
| `algo.degree([edgeLabel])` | `node, in_degree, out_degree, neighbours, in_weight, out_weight` |
| `algo.betweenness([sample[, edgeLabel]])` | `node, score` (exact Brandes by default; `sample` for large graphs) |
| `algo.closeness([sample[, edgeLabel]])` | `node, harmonic` |
| `algo.kcore([edgeLabel])` | `node, core` |
| `algo.triangles([edgeLabel])` | `node, triangles, clustering` |
| `algo.scc([edgeLabel])` | `node, component, size` |
| `algo.articulation([edgeLabel])` | `node, components_after_removal` (cut vertices) |
| `algo.louvain([resolution[, edgeLabel]])` | `node, community` |
| `algo.similarity(node[, k[, edgeLabel]])` | `node, jaccard, common` |

`source`/`target`/`node` arguments are node variables bound by an earlier `MATCH`. `db.stats()` (`CALL db.stats()`) yields `nodes, edges, versions, ts`.

### 9.2 `spectral.*` (normalised hypergraph Laplacian, Zhou–Huang–Schölkopf)

| Procedure | Yields | Notes |
|---|---|---|
| `spectral.fiedler()` | `node, value` | second eigenvector; meaningful only on a connected graph |
| `spectral.cluster(k)` | `node, cluster` | k-means on the spectral embedding |
| `spectral.eigenvalues(k)` | `eigenvalue` | smallest k |
| `spectral.embedding(k)` | `node, embedding` | `k` must be below the number of connected nodes |
| `spectral.communities([k])` | see `YIELD *` | non-backtracking (Bethe Hessian) communities; k is found automatically if omitted |
| `spectral.nb_radius([true])` | see `YIELD *` | non-backtracking spectral radius `rho_B`; pass `true` to also compute the (slow) community count |

Rules that surprise people:

* **Every** edge with at least two distinct members and positive weight participates, **including hyperedges you added for other purposes**. Nodes in no such edge are left out.
* Results on graphs with several components are degenerate (repeated zero eigenvalue). With very many components (e.g. 100k disconnected pairs) you get an explanatory error instead of a result.
* Dense solver up to 1,500 vertices (up to 4,000 for the fallback), matrix-free Lanczos above that.
* `spectral.communities` ignores direction and weights and uses the clique expansion (a hyperedge of size *s* adds *s(s−1)/2* pairs). On real, non-block-like graphs (e.g. Cora) it can be **worse** than `spectral.cluster` and over-count communities; use it for very sparse, block-like graphs.

### 9.3 `physics.*` (exact solves of the equations SHPINN trains a network to approximate)

| Procedure | Yields | Meaning |
|---|---|---|
| `physics.heat(key, t[, 'rw'\|'sym'])` | `node, value` | heat diffusion `du/dt = −(I−P)u` starting from numeric property `key` (missing = 0); `'sym'` = normalised Laplacian |
| `physics.heat_from([nodes], t[, mode])` | `node, value` | unit heat placed on the listed nodes |
| `physics.harmonic(key[, tol=1e-10[, max_iter=100000]])` | `node, value, observed` | Dirichlet problem: nodes with a numeric `key` are fixed, the rest are filled in; unreachable nodes get `null`; errors instead of returning an unconverged answer |

Use cases: label propagation (Cora: 71.5 % strict test accuracy with no node features), "where does influence/risk spread".

---

## 10. Statistics and cyber-security procedures

### 10.1 `stats.*` (descriptive statistics over node/edge properties)

| Procedure | Yields |
|---|---|
| `stats.describe(label, key)` | `count, min, p25, median, p75, p95, max, mean, stdev, missing` (`label` may be `null` for all) |
| `stats.outliers(label, key[, threshold])` | `entity, value, z, robust_z` |
| `stats.histogram(label, key[, bins=10])` | `bin, lo, hi, count` |
| `stats.correlation(label, keyA, keyB)` | `n, pearson, spearman` |
| `stats.timeseries(label, tsKey, bucketSeconds[, valueKey])` | `bucket_start, count, sum, mean` |
| `stats.spikes(label, tsKey, bucketSeconds[, threshold])` | `bucket_start, count, robust_z` |

`label` can be a node **or edge** label. `stats.timeseries` returns every bucket in the range, including empty ones (`count 0`, `mean null`), so a short burst over a long span produces many rows; filter with `WHERE count > 0`.

### 10.2 `cyber.*` (hosts/users/services as nodes; connections and logins as edges or hyperedges)

You pass the edge label and property keys; there is no fixed schema.

| Procedure | Question it answers |
|---|---|
| `cyber.reachable(src[, maxHops[, label]])` | blast radius from `src` |
| `cyber.attack_paths(src, dst, k[, maxHops[, label]])` | k cheapest simple paths (Yen); edge `weight` = attacker effort |
| `cyber.chokepoints(entryLabel, targetLabel[, label])` | assets on the most entry→crown-jewel shortest paths |
| `cyber.propagate_risk(seedKey[, label])` | compromise likelihood from seeded nodes; edge `weight` = success probability |
| `cyber.fanout(label[, minDistinct])` | scanners / password sprayers |
| `cyber.beaconing(label, tsKey[, minEvents[, maxCv]])` | near-periodic connections (C2 beacons); yields `src, dst, events, cv` |
| `cyber.volume(label, bytesKey[, threshold])` | unusually large outbound volume (exfiltration) |
| `cyber.ioc(label, key, [values][, hops])` | indicator-of-compromise matches and everything within `hops`; yields `node, hops, ioc, via` |

```
.import events zeek conn.json
CALL cyber.beaconing('FLOW','ts',20,0.25) YIELD src, dst, events, cv RETURN src.ip, dst.ip, events, cv;
CALL cyber.chokepoints('Entry','Crown') YIELD node, score RETURN node.ip, score ORDER BY score DESC LIMIT 5;
CALL cyber.ioc('Host','ip',['203.0.113.9','198.51.100.7'],2) YIELD node, hops, ioc, via RETURN node, hops, ioc, via;
```

### 10.3 `killchain.*` (incident portfolios: one incident per hyperedge)

| Procedure | Question |
|---|---|
| `killchain.funnel(label, stageKey, outcomeKey[, order[, lossKey[, detectKey]]])` | how many incidents reach each stage, success rate, cost |
| `killchain.control_effect(label, controlKey, outcomeKey[, groupKey])` | does a binary control (MFA, EDR…) reduce the outcome rate? Crude and stratified risk ratios plus a Mantel–Haenszel pooled ratio |
| `killchain.exposure(label, groupKey, outcomeKey, lossKey[, minN])` | which segments lose the most, with Wilson intervals |
| `killchain.response(label, groupKey, detectKey, respondKey)` | mean/median/p90 time to detect and respond |
| `killchain.defense_depth(label, [controlKeys], outcomeKey[, lossKey])` | outcome rate vs. number of controls in place |

Import with `.import hyper ... props` so the keys are edge properties. An **outcome key** must be a 0/1 or true/false column, or a condition written as `Key=Value` / `Key!=Value` (for example `'Outcome=Success'` or `'Category!=Normal'`); passing a plain category column is rejected with an explanatory error. Statistical note from the source: **stratify only by variables that exist before the control acts**; conditioning on a mediator (such as the stage an attack reached) can make a working control look useless.

---

## 11. Domain analytics modules

These modules read records stored as **hyperedges imported with `props`** (a "column" is an edge property, a member-node name, or – in `uw.*` – `Label.prop` for a property of a member node joined via a keyed node import). Common conventions:

* `filter` / conditions: `Key=Value`, `Key!=Value`, or a 0/1 key; `uw.*` also supports `> >= < <=` (numeric when both sides are numbers; dates are epoch seconds).
* `opts` strings are `key=value;key=value`.
* Rows that cannot be evaluated are **excluded and counted**, never treated as zero or "good".
* Statistical flags are screens for a human reviewer, not findings or decisions.

### 11.1 `alm.*` (asset-liability management)

| Procedure | Purpose |
|---|---|
| `alm.position(label, mvKey, durKey[, convKey[, groupKey[, filter]]])` | market value, duration, convexity, DV01, concentration |
| `alm.liability_profile(label, cfKey, dfKey, monthKey[, groupKey[, filter]])` | PV, duration, convexity, DV01, run-off of liability cash flows |
| `alm.duration_gap(aLabel, mvKey, durKey, convKey, lLabel, cfKey, dfKey, monthKey[, aFilter[, lFilter]])` | funding ratio, leverage-adjusted duration gap, net DV01 |
| `alm.rate_shock(<same>, shocksBp[, aFilter[, lFilter[, bondLabel, bondCfKeys, bondDfKey, bondMonthKey[, bondFilter]]]])` | surplus under parallel yield shifts; liabilities repriced exactly, assets by duration+convexity or exactly from bond cash flows |
| `alm.cashflow_ladder(inLabel, inMonthKey, inKeys, outLabel, outMonthKey, outKey[, bucketMonths[, inFilter[, outFilter]]])` | cash-flow matching by bucket |
| `alm.krd_gap(label, sideKey, pvKey, krdKeys[, assetSide[, liabSide[, filter]]])` | key-rate DV01, assets vs liabilities |
| `alm.scenario_risk(label, scenarioKey, monthKey, valueKey, horizon[, baseMonth[, mode[, exposure[, exposure2[, alpha]]]]])` | distribution, VaR and CTE across stochastic paths |

Conventions: yields are annual-effective; DV01 = value × duration × 1e-4 (positive net DV01 means surplus falls when rates rise); a liability is a positive number; a segment with non-positive PV has `NULL` duration; a funding ratio below 0.5 or above 2 raises a `warning` (the two sides probably describe different obligations).

### 11.2 `land.*` (land registry and title records)

| Procedure | Purpose |
|---|---|
| `land.conflicts(label, parcelKeys, ownerKey[, limit[, filter[, tenancyKey]]])` | parcels recorded more than once: competing owners vs repeated records; `tenancyKey` separates `co_ownership` |
| `land.rule_check(label, ifCond, thenCond[, groupKey[, filter[, idKey]]])` | records meeting `ifCond` but not `thenCond`, with Wilson interval |
| `land.hotspots(label, groupKey, outcome[, filter[, minN]])` | offices/areas with unusually high outcome rates (p-value, Bonferroni flag) |
| `land.valuation(label, valueKey, areaKey[, groupKey[, filter]])` | value per unit area, quartiles, Tukey fences on log scale |
| `land.survey_check(label, surveyKey, subKey[, groupKey[, filter[, idKey]]])` | malformed survey numbers and `/sub-division` mismatches |

Always identify a parcel by a **qualified** key (`District,Taluk,Village,Survey Number,Sub Division`); survey numbers repeat across villages. Multiple group columns are comma-separated and joined with `/`.

### 11.3 `uw.*` (credit underwriting analytics)

| Procedure | Purpose |
|---|---|
| `uw.audit(label, target, features[, filter])` | which fields carry signal (AUC with DeLong CI, information value) and which are **leaks** |
| `uw.scorecard(label, target, features, idKey[, opts])` | regularised logistic model: coefficients with Wald intervals; out-of-sample AUC, Gini, KS, log-loss, Brier |
| `uw.gains(label, target, features, idKey[, opts])` | decile gains table |
| `uw.scores(label, target, features, idKey[, opts])` | per-record probability of default with top three risk drivers |
| `uw.disparity(label, groupKey, favourable[, filter[, minN]])` | approval-rate parity (adverse impact ratio, chi-square) |
| `uw.group_gap(label, target, features, idKey, groupKey[, opts])` | does a group fare differently from a model *without* that attribute |
| `uw.psi(label, key, baseCond, compareCond[, bins])` | population stability between two books/periods |
| `uw.affordability(label, incomeKey, debtKey, loanKey, rateKey, termKey[, opts])` | recomputed DTI, new payment, stressed ratios, stored-vs-recomputed mismatches |

* `target` is the **bad/event** condition, e.g. `Default=1` or `not.fully.paid=1`. `features` is a **comma**-separated list (`'age,income,Loan.grade'`); the semicolon separates `key=value` pairs only inside `opts`.
* Train/test split is deterministic (hash of `idKey`; default 30 % test) or `test=<condition>` for an out-of-time split.
* Options: `l2` (default 1), `test_pct`, `min_level`, `test`, `filter`, `limit`, `set` (`test`/`train`), `max_iter`.
* A model with no signal reports an AUC interval that contains 0.5; a model, disparity ratio or leak flag is a screen for validation, **not a lending decision** (the four-fifths rule is a US employment-testing convention).

### 11.4 `fin.identity` (financial-statement consistency)

`fin.identity(label, entityKey, periodKey, tagKey, valueKey, totalTag, partTags[, tolPct[, blankKeys[, filter[, limit]]]])` checks `totalTag = sum(partTags)` per entity and period on long-format fact tables (SEC Financial Statement Data Sets, XBRL exports). Prefix an optional part with `?`; `blankKeys` lists properties that must be empty for a fact to count (`segments,coreg` selects consolidated facts). Output: summary rows then `violation` rows ordered by relative gap; groups with two values for one tag are `ambiguous`, missing required parts are `incomplete`.

### 11.5 `swarm.*` (coordinated-group detection in track data; defensive analytics only)

`swarm.detect(label, idKey, timeKey, xKey, yKey, zKey, win[, opts])` finds tracks that persistently move together (one edge per observation). It reports per group `size, members, windows, persistence, heading_coherence, speed_mean, speed_cv, p_coherence, p_background` and a `verdict` of `coordinated`, `coordinated (whole scene)` or `co-located`. `swarm.scan(..., wins[, opts])` repeats the detection at several window lengths (`'2,5,10,20'`). Key options: `eps` 20, `cos_min` 0.8, `persist` 0.6, `min_windows` 3, `min_size` 3, `dim` 3 (2 ignores z), `perm` 1000, `seed` 1, `p_max` 0.01. Cost is O(tracks²) per window. It detects and characterises motion; it does not infer intent.

---

## 12. GAAP ledger

A double-entry general ledger with US-GAAP statements stored in the graph (`nexgraph::gaap`, `CALL gaap.*`). Journal entries are `:JournalEntry` nodes with `:LINE` edges plus one `:POSTING` hyperedge each, so spectral and community procedures can analyse the flow of money between accounts.

```rust
use nexgraph::{Database, gaap::*};
let db = Database::open("books")?;
let l = Ledger::new(&db)?;
l.create_company("Acme", "Corporation", "USD", 1)?;
l.seed_reference_chart("Acme")?;                      // reference 41-account chart
l.add_period("Acme", "FY2024", "Annual", "2024-01-01", "2024-12-31")?;
l.post("Acme", &Entry::new("2024-01-02", "Issue stock").dr("101", 100000.0).cr("401", 100000.0))?;
l.post("Acme", &Entry::adjusting("2024-12-31", "Depreciation", "630", "161", 2200.0))?;
l.close_period("Acme", "FY2024", ClosingAccounts::default())?;
```

Posting is **library-only**; the procedures and the UI are read-only. Other write methods: `post_many` (atomic batch, all-or-nothing, error names the failing entry), `import_journal_csv` (columns `entry, date, account`, `debit` and/or `credit`; optional `journal, reference, description, period, memo`), `reverse`, `set_cash_flow`.

| Procedure | Arguments |
|---|---|
| `gaap.trial_balance(company, type, start, end)` | `type` = `unadjusted`, `adjusted` or `post-closing` |
| `gaap.income_statement(company, from, to)` | multi-step |
| `gaap.retained_earnings(company, from, to)` | roll-forward |
| `gaap.balance_sheet(company, asof)` | classified; last row is the equation check |
| `gaap.cash_flow(company, from, to)` | indirect; last row reconciles to the real cash change |
| `gaap.ledger(company, account, from, to)` | T-account with running balance |
| `gaap.audit(company)` | balanced entries, gapless numbers, hyperedges match lines, nothing posted into closed periods, abnormal balances |
| `gaap.ratios(company, from, to)` | 17 ratios: profitability, liquidity, solvency, efficiency (zero denominator → `NULL`) |
| `gaap.open_items(company, account, asof)` | reference-matched open items with age bucket |
| `gaap.aging(company, account, asof)` | 0-30 / 31-60 / 61-90 / 91+ buckets, ledger balance and difference (must be 0) |

Enforced rules: entries balance to the cent, at least two lines, known active accounts, real dates, a period covers the date and **no closed period covers it**, entries are immutable (corrections are reversing entries), gapless entry numbers. Use the invoice/bill number as the entry `reference` so aging works. This covers the accounting core only: no subledgers, inventory costing, fixed-asset registers, payroll, leases, tax, multi-currency or approval workflow. `cargo run --release --example gaap_demo` prints a worked year.

---

## 13. Using nexgraph from Rust

| Task | API |
|---|---|
| Open | `Database::in_memory()`, `open(dir)`, `open_with(dir, SyncMode)`, `open_encrypted(dir, sync, Keyring)`, `open_postgres(url)`, `open_postgres_with(url, PgOptions)` |
| Query | `db.query(text)`, `db.query_with(text, &Params)` → `QueryResult { columns, rows, stats, ts }` |
| Render | `r.to_table()`, `r.to_csv()`, `r.to_csv_raw()`, `r.to_json()` |
| Transactions | `db.begin()` → `Txn` (see 8.3) |
| Direct reads | `db.read(\|view\| ...)`, `db.read_at(ts, \|view\| ...)` |
| Maintenance | `db.checkpoint()`, `db.stats()`, `db.last_ts()` |
| Compare snapshots | `nexgraph::drift(&db, old_ts, new_ts)` |
| BI export | `nexgraph::bi::export_dir(&db, dir)` |

Concurrency: any number of readers plus **one writer at a time** (writers serialise on a mutex). A query holds one read guard for its whole run, so a very long read can delay commits.

---

## 14. PostgreSQL backend

PostgreSQL replaces the WAL file and `checkpoint.bin` as the durable store. The query engine still runs in memory; PostgreSQL is storage, not a query backend.

```bash
nexgraph "postgres://user:pw@localhost/mydb" --schema crm
```

```rust
let db = Database::open_postgres("postgres://user:pw@localhost/mydb")?;
let db = Database::open_postgres_with(url, PgOptions::default().schema("crm").mirror(true))?;
```

**What is stored** (one schema = one nexgraph database): `meta`, `commits` (one row per commit with CRC), `checkpoint_chunks` (latest checkpoint in ≤ 8 MiB rows), and the **mirror** tables `nodes(id, labels, props jsonb, created_ts, updated_ts)`, `edges(id, label, weight, props, tail, head, ...)`, `edge_members(edge_id, node_id, role)` and view `binary_edges`. Plain SQL can read the mirror:

```sql
SELECT s.props->>'name', d.props->>'name'
FROM crm.binary_edges e
JOIN crm.nodes s ON s.id = e.src JOIN crm.nodes d ON d.id = e.dst
WHERE e.label = 'KNOWS';
```

**Behaviour**

* Commit row and mirror changes are one PostgreSQL transaction; memory is updated only after it commits.
* **Single writer** via a session advisory lock per schema. Any number of `--read-only` replicas; call `.refresh` to follow the writer.
* A lost connection is handled: the writer reconnects, re-takes the lock and resolves the in-flight commit exactly.
* The mirror is **derived, one-way and lossy** (jsonb has no NUL, no int/float or vector/list distinction). Edits made with SQL do not reach the graph; `.verify` flags them and `.rebuild` removes them. Grant clients `SELECT` only.
* TLS (rustls) with certificate and host-name verification. `sslmode=require` or `PgOptions::require_tls(true)` refuses plaintext; `tls_ca_file(..)` trusts a private CA. Client-certificate authentication to PostgreSQL is not supported. The `postgres://` URL form rejects `hostaddr`; use the `key=value` form for that.
* **PgBouncer in transaction mode breaks the writer lock.** Connect directly or use session pooling.

Run the PostgreSQL tests against a scratch database only:

```bash
export NEXGRAPH_TEST_PG_URL="postgres://user:pw@localhost/scratchdb"   # tests create/drop schemas nxt_*
cargo test --release --test pg -- --test-threads=1
```

---

## 15. Server, tenants, roles and HTTP API

`nexgraph-server` is a multi-tenant HTTPS JSON API over PostgreSQL. **One tenant = one nexgraph database = one PostgreSQL schema**, with its own commit log, checkpoint, mirror and writer lock.

### 15.1 Start it

```bash
cargo build --release --features server

export NEXGRAPH_PG_URL="host=db.example.com port=5432 user=nexgraph dbname=graph sslmode=require"
export NEXGRAPH_ADMIN_PASSWORD="a long passphrase"            # 12-256 chars; or first line of stdin

# once: create the control plane and the first platform admin
./target/release/nexgraph-server init --pg-ca ca.pem [--admin NAME]

# serve
./target/release/nexgraph-server serve --pg-ca ca.pem --cert cert.pem --key key.pem --listen 0.0.0.0:8443
```

PowerShell uses `$env:NEXGRAPH_PG_URL = "..."` instead of `export`.

| Option | Meaning |
|---|---|
| `--pg URL` | PostgreSQL URL (or `$NEXGRAPH_PG_URL`) |
| `--pg-ca FILE` | private CA for the PostgreSQL server |
| `--pg-allow-plaintext` | allow non-TLS PostgreSQL (**not recommended**) |
| `--control-schema NAME` | schema for users/tokens/audit |
| `--cert FILE --key FILE` | TLS certificate and key for the API (required) |
| `--listen ADDR` | default `127.0.0.1:8443` |
| `--insecure-http` | plain HTTP, **loopback addresses only**, for development |
| `--frontend DIR` | serve the web UI (also `$NEXGRAPH_FRONTEND_DIR`) |
| `--policy FILE.json` | data policy (section 17) |
| `--master-key-file FILE` | enable encryption at rest (section 17) |
| `--master-key-id N`, `--previous-master-key ID:FILE` | key id and rotation |

The server refuses to start without `--cert/--key` unless `--insecure-http` is given on a loopback address. Use a dedicated PostgreSQL role that can create schemas in its database. Ctrl-C shuts down gracefully (10 s). Health check: `GET /healthz`.

### 15.2 Roles

| Role | Data in own tenant | Users of own tenant | Tenants | Audit |
|---|---|---|---|---|
| `reader` | read (`MATCH`/`RETURN`, `algo.*`, `spectral.*`, `physics.*`, `vector.*`, and the analytics families) | – | – | – |
| `writer` | + `CREATE`/`SET`/`DELETE`/`MERGE` | – | – | – |
| `tenant_admin` | + indexes, checkpoint, SQL-mirror grants | create (up to tenant_admin), disable, reset password | – | own tenant |
| `platform_admin` | **none** | any | create, disable, purge | all |

Statements are parsed and classified before execution; unknown procedures are refused; a read clause cannot hide a write.

### 15.3 Accounts and limits

* Passwords: Argon2id, 12–256 characters, must not contain the user name or be one repeated character.
* Tokens: `ngt_…`, 256 random bits, only the SHA-256 is stored, default lifetime 8 h, revoked by logout, password change/reset, or user/tenant disable.
* Lockout: 5 consecutive failures lock an account for 15 minutes; per-IP login rate limit (default 20/min, in memory, per process).
* Quotas per tenant: `max_entities` (soft; checked before statements that create) and `max_result_rows` (hard). Statements over 64 KiB are refused. Responses time out after 30 s (the engine call itself is not cancelled).
* Offboarding: **disable** (instant lock-out) then **purge** (drops the tenant's schema, users and tokens; audit entries stay).

### 15.4 HTTP API

All bodies are JSON; errors are `{"error": "..."}`.

| Method and path | Role / auth | Body or query |
|---|---|---|
| `POST /v1/login` | none | `{"tenant"?, "user", "password"}` → `{"token", "expires_in"}` (omit/empty `tenant` for platform admins) |
| `POST /v1/logout` | Bearer | – |
| `GET /v1/me` | Bearer | → `{user, tenant, role}` |
| `POST /v1/me/password` | Bearer | `{"old", "new"}` |
| `POST /v1/query` | Bearer | `{"query", "params"?}` → `{columns, rows, stats, ts}` |
| `POST /v1/checkpoint` | tenant_admin | – |
| `GET /v1/stats` | tenant account | → nodes, edges, versions, index counts |
| `GET /v1/audit?limit=&tenant=` | admins | audit rows |
| `POST /v1/admin/tenants` | platform_admin | `{"name", "max_entities"?, "max_result_rows"?}` |
| `GET /v1/admin/tenants` | platform_admin | list |
| `POST /v1/admin/tenants/{name}/disable` and `/enable` | platform_admin | – |
| `DELETE /v1/admin/tenants/{name}` | platform_admin | tenant must be disabled |
| `POST /v1/admin/users` | tenant_admin / platform_admin | `{"tenant"?, "name", "password", "role"}` |
| `GET /v1/admin/users[?tenant=]` | tenant_admin / platform_admin | list |
| `POST /v1/admin/users/disable` | same | `{"tenant"?, "name", "disabled"}` |
| `POST /v1/admin/users/reset-password` | same | `{"tenant"?, "name", "new_password"}` |
| `/v1/bi/*` | Bearer **or** Basic | see section 16 |

Example session:

```bash
TOKEN=$(curl -s https://localhost:8443/v1/login -H 'content-type: application/json' \
        -d '{"user":"admin","password":"a long passphrase"}' | jq -r .token)

curl -s https://localhost:8443/v1/admin/tenants -H "Authorization: Bearer $TOKEN" \
     -H 'content-type: application/json' -d '{"name":"acme"}'

curl -s https://localhost:8443/v1/admin/users -H "Authorization: Bearer $TOKEN" \
     -H 'content-type: application/json' \
     -d '{"tenant":"acme","name":"alice","password":"another long passphrase","role":"writer"}'

# as alice
curl -s https://localhost:8443/v1/query -H "Authorization: Bearer $ALICE" \
     -H 'content-type: application/json' \
     -d '{"query":"MATCH (p:Person) WHERE p.age > $min RETURN p.name","params":{"min":30}}'
```

Use `--cacert ca.pem` with curl for a private CA. Tenant names are limited to 32 characters and user names to 64.

### 15.5 Audit log

Logins, queries (reads configurable; writes, denials and errors always) and admin actions go into a **hash-chained, append-only** table; statement text and data are never logged, only a truncated SHA-256 of the statement. The chain is tamper-*evident*, not tamper-proof: export the latest `row_hash` somewhere the database admin cannot write.

---

## 16. Web UI, Power BI and BI export

### 16.1 Web UI

A WebAssembly single-page app (Yew) served by the API server from the same origin.

```bash
rustup target add wasm32-unknown-unknown
cargo install trunk
cd frontend && trunk build --release                      # -> frontend/dist

nexgraph-server serve --pg URL --cert cert.pem --key key.pem --frontend frontend/dist
```

For development: run the server with `--insecure-http --listen 127.0.0.1:8443`, then `cd frontend && trunk serve --open`.

| Page | What it does |
|---|---|
| Sign in / My account | tenant (empty for platform admins), user, password; change password; checkpoint (tenant_admin); theme |
| Dashboard | engine counters, nodes/edges by label, recent audit |
| Query workbench | NQL editor, `$param` JSON, history, sortable/paged results, CSV |
| Analytics | launcher for 22 `CALL` procedures with typed parameters, chart + table, "open in workbench" |
| Data explorer | BI tables with `AS OF` time travel, search, sort, CSV |
| Power BI | generated Power Query script and connection steps |
| Audit log, Tenants, Users | administration (role-aware; platform admins only see administration) |

The server still enforces every permission; the UI only hides what a role cannot use.

### 16.2 BI tables

`nexgraph::bi` flattens a snapshot into typed tables:

| Table | One row per | Columns |
|---|---|---|
| `nodes` | node | `id, label, labels, props` (JSON text) |
| `edges` | edge/hyperedge | `id, label, weight, directed, tail_count, head_count, src, dst, tail, head, props` |
| `edge_members` | (edge, member) | `edge_id, node_id, role, label, weight` |
| `node_<Label>` | node with that label | `id, labels,` one typed column per property |
| `edge_<Label>` | edge with that label | `id, weight, src, dst,` one typed column per property |

Types are inferred (all-int → `int64`, int+float → `double`, all-bool → `boolean`, else `text`). `src`/`dst` exist only for binary edges; model hyperedges through `edge_members`. A property named like a fixed column is exposed as `prop_<name>`.

### 16.3 Four routes into Power BI

| Route | Use when |
|---|---|
| **A. HTTPS API** `/v1/bi/*` | multi-tenant/remote, Power BI Service scheduled refresh |
| **B. PostgreSQL views** `bi_nodes`, `bi_edges`, `bi_edge_members` | already on PostgreSQL; DirectQuery or native connector |
| **C. CSV folder** `.powerbi DIR` / `bi::export_dir` | embedded database, quick start, offline |
| **D. Ad-hoc NQL** `/v1/bi/query` | an analytic result (PageRank, chokepoints, beaconing…) as a table |

Route A endpoints:

```
GET  /v1/bi/tables                      [?as_of=TS]   catalog
GET  /v1/bi/tables/{name}               [?format=csv|json&as_of=TS]
GET  /v1/bi/query?q=NQL                 [&format=csv|json]    read-only statements only
POST /v1/bi/query  {"query":..,"params":{..}}  [?format=..]
GET  /v1/bi/powerquery                  [?base=https://host:8443]   Power Query (M) script, no auth
```

* Power BI refresh can only send static credentials, so these endpoints also accept **HTTP Basic** with user `tenant/user` and the account password. Create a dedicated `reader` account per report.
* In Power BI Desktop: *Get data → Blank query → Advanced editor*, paste the script from `/v1/bi/powerquery`, fill `User`/`Password` (better as Parameters) and set the data-source credential to *Anonymous* (the Authorization header authenticates).
* The `x-nexgraph-ts` response header is the snapshot used. Power BI Service needs a publicly resolvable HTTPS endpoint with a trusted certificate, or an on-premises data gateway.
* **The generated M scripts were never run in Power BI Desktop** by the author (syntax-checked only). Treat them as templates.

---

## 17. Privacy tooling: data policy, encryption, de-identification

These narrow gaps for sensitive data (written for health records). They do **not** make the system HIPAA-compliant and have not been reviewed by a security professional or privacy officer.

### 17.1 Data policy (`--policy FILE.json`)

```json
{
  "min_cell": 11,
  "min_cell_up_to": "reader",
  "pseudonym_key_hex": "<64 random hex digits, kept outside the database>",
  "summary_procedures": [],
  "rules": [
    {
      "up_to": "writer",
      "deny": ["Stay.hospitalid", "Stay.wardid"],
      "deny_labels": ["Note"],
      "mask": {
        "uniquepid": "pseudonym",
        "age": "top_code:89",
        "admissionweight": "bucket:5",
        "hospitaladmittime24": "redact"
      }
    }
  ]
}
```

* A rule applies to roles **up to** the one named, so `tenant_admin` can stay unrestricted (and audited).
* `deny` / `deny_labels`: any statement that *mentions* the property or label anywhere (pattern, `WHERE`, `RETURN`, parameters, procedure arguments) is refused before it runs.
* `mask`: the property may only be returned as a plain projection and is masked in the result (`pseudonym`, `top_code:N`, `bucket:N`, `redact`); it cannot be filtered, sorted, grouped or used in an expression. Masked columns are display-only.
* `min_cell`: aggregate-only roles must include `count(*)`, groups smaller than *k* are dropped, `collect`/`min`/`max` and record-level results are refused. Count-restricted roles are limited to one `MATCH` with one pattern and no row-multiplying clauses and must count `DISTINCT` nodes when the pattern has relationships.
* Restricted roles are also blocked from whole-graph BI tables and bulk CSV import; their reads are always audited and **fail closed**.
* Known gap: differencing two allowed aggregate answers can reveal a group smaller than *k*.

### 17.2 Encryption at rest

* Server: `--master-key-file FILE` (a file with at least 64 hexadecimal digits = 32 bytes), `--master-key-id N`. Library: `Database::open_encrypted(dir, sync, keys)`, `PgOptions::encrypt`.
* XChaCha20-Poly1305 with per-tenant keys derived by HKDF; records are bound to their store, kind and position, so tampering, reordering and moving records between tenants are detected. Wrong key or mixed plaintext/encrypted data is refused.
* **Rotation**: start with `--previous-master-key ID:FILE` alongside the new master, run a checkpoint, then drop the old key.
* The SQL mirror is plaintext, so it is **disabled for encrypted tenants**. There is no in-place migration: export and re-import into an encrypted tenant.
* Not covered: freshness (rollback to an older complete copy or dropped newest records is not detected); keys live in process memory.

### 17.3 `nexgraph-deid` (profile and de-identify CSVs)

Needs features `crypto` + `ingest` (both on by default).

```bash
nexgraph-deid profile FILE.csv
nexgraph-deid run FILE.csv --key-file KEY --out OUT.csv [--k 5] [--policy-out policy.json --label Stay]
```

* `profile` classifies each column by name *and* content (names, addresses, ZIP, coordinates, dates, ids, ages, sex/race, free text, SSN/e-mail/phone-shaped values, wallet addresses) and reports data-quality findings (duplicates, negative values, impossible ages, discharge before admission, constant columns…).
* `run` applies rules: names/addresses/coordinates/free text dropped; birth dates to 5-year bands; ZIP to 3 digits; ages over 89 top-coded; ids replaced by keyed pseudonyms that are **identical across tables** (joins keep working); dates shifted by a secret per-patient offset. It then measures k-anonymity on the quasi-identifiers, suppresses small groups to reach *k*, and re-scans the output. `KEY` is a file of at least 32 hex digits; keep it away from the data.
* The classifier is heuristic and can miss identifiers; free text is dropped, not scrubbed; the result is not a legal determination and a person must review the report.

---

## 18. Limits and honest caveats

**Scale and architecture**

* Everything is held in memory (about 2.9 KB per node with 14 properties; a 2.97M-row wide table needs roughly 8.6 GB to hold and about 12 GB to import). There is no paging, replication or distributed execution.
* One process per database (enforced lock) and one writer at a time. `import_csv_nodes` is one atomic transaction.
* Bulk commits build a transient overlay roughly proportional to commit size.
* HNSW is in memory, append-only and rebuilt on open; heavy vector churn leaves dead entries until a checkpoint.

**Query engine**

* Heuristic planner (starts from the more selective end of a path), no cost model. Scans that touch properties of scattered nodes are the weakest area; the author measured Kuzu and DuckDB as 2–5× faster on scans, grouping and loading, while nexgraph wins small indexed lookups and shortest paths.
* No composite or multi-key range indexes. See [7.4](#74-not-supported-fails-with-a-clear-error) for unsupported syntax.
* Procedures on disconnected graphs give degenerate spectral results.

**Security and compliance**

* Not independently audited. No MFA/SSO/OIDC, client certificates, token scopes or breach-password checks; tokens are plain bearer tokens.
* Tenants share one process, RAM and CPU; quotas are soft and there is no per-tenant CPU or memory accounting; tenant databases stay loaded until shutdown.
* TLS certificates are loaded at start-up; rotating them needs a restart. The login rate limit ignores forwarded headers, so behind a reverse proxy all clients share the proxy's address.
* Isolation between tenants does not hold against a PostgreSQL superuser.

**Verification scope**

* Tested on Linux, PostgreSQL 16.15, one machine. Not tested on Windows, other PostgreSQL versions or networked servers. Only rustc 1.97 was compiled (MSRV 1.85 comes from the `postgres` crates).
* Statistical outputs (hotspots, disparity, risk ratios, leak flags) are screens that describe association in the data you loaded; they are not causal findings, legal determinations or decisions.

---

## 19. Troubleshooting

| Symptom | Cause and fix |
|---|---|
| `... is already open ...` when opening a directory | Another process (or handle) holds `<dir>/LOCK`. Close it; the lock disappears when the process dies. |
| `AS OF n` fails | History before the latest checkpoint was discarded, or `n` is beyond the last commit. Use a timestamp from after the last checkpoint. |
| `'Label' is an edge/hyperedge label, not a node label` | You wrote `MATCH (n:Label)` for a hyperedge label. Use `MATCH HYPEREDGE h:Label ...`. |
| `unknown procedure 'x'` | The message lists all valid names. Procedure names are case-insensitive. |
| A procedure's output columns are unclear | Add `YIELD *`. |
| Spectral error mentioning many components, or a meaningless Fiedler vector | The graph is (nearly) disconnected. Restrict to one component or import events as hyperedges so entities connect. |
| `physics.harmonic` error about convergence | It refuses to return an unconverged answer. Loosen `tol` or raise `max_iter`; unreachable nodes return `null`. |
| `no node has a numeric 'key' property` | `physics.heat/harmonic` need a numeric property to start from. |
| Import reports skipped rows | Read `first_error` (includes the 1-based line number). Rows with an unparsable timestamp or bad weight are skipped *before* any node is created. |
| Keyed import merged rows | `duplicate_keys > 0` means the key column is not unique. |
| `cyber.beaconing` / `stats.timeseries` raise a parse error | The timestamp column exists but isn't epoch seconds or ISO 8601. Re-import with `dates=` and the time column named. |
| `killchain.*` / `alm.*` / `land.*` / `uw.*` say a key is missing | Re-import with `props` so member columns become edge properties. |
| Server refuses to start | Needs `--cert/--key` (or `--insecure-http` on a loopback address), and TLS to PostgreSQL unless `--pg-allow-plaintext`. |
| Writer lock lost or second writer fails | One writer per PostgreSQL schema. Check for PgBouncer transaction pooling; use `--read-only` for extra readers. |
| `504` from the server | The 30 s response timeout elapsed (the query may still be running server-side). Narrow the query or use sampling arguments. |
| Quota error | `max_entities` or `max_result_rows` for the tenant; a platform admin can raise them when creating the tenant. |
| Out of memory on a big load | Everything must fit in RAM; split loads, reduce properties, or use `props` only when the domain procedures need it. |
| `to_csv` cell starts with `'` | Intentional spreadsheet-injection protection; use `to_csv_raw` for exact text. |

---

## 20. Appendix: layout, tests, further reading

### Source layout

```
src/value.rs, model.rs        values, codec, ops/commits
src/store.rs                  MVCC store, indexes, View
src/db.rs                     Database, Txn, WAL, checkpoint, recovery
src/query/                    lexer/parser/executor (NQL)
src/algo.rs, analytics.rs     graph algorithms, spectral entry points, stats.*
src/physics.rs, community.rs  heat/harmonic, non-backtracking communities
src/cyber.rs, killchain.rs, alm.rs, land.rs, uw.rs, fin.rs, swarm.rs, gaap.rs   domain modules
src/ingest.rs, timeutil.rs    loaders and date parsing
src/pg.rs, tls.rs             PostgreSQL backend
src/security/                 control plane, tenants, policy
src/crypto.rs, deid.rs, profile.rs   encryption, de-identification, profiling
src/bi.rs, server.rs          BI export, HTTPS API
src/bin/                      nexgraph, nexgraph-server, nexgraph-deid
frontend/                     Yew web UI
examples/                     benchmarks, Cora, connectome import, GAAP demo, swarm eval
tests/                        integration tests and fixtures
bench/, docs/                 cross-system benchmarks, performance and openCypher notes
```

### Tests and examples

```bash
cargo test --release                               # full suite
cargo run --release --example gaap_demo            # worked GAAP year
cargo run --release --example bench                # core benchmark
cargo run --release --features nonbacktracking --example cora_real_data
```

Several end-to-end tests are dataset-driven through `NG_*` environment variables (for example `tests/lifecycle.rs` and `tests/cyber_events.rs`); they print a notice and return when the variables are absent.

### Further reading (in this repository)

| File | Topic |
|---|---|
| `README.md` | measured results and long-form feature notes |
| `HOW_IT_WORKS.md` | internals: MVCC, WAL, indexes, executor, spectral math, ALM formulas |
| `SECURITY.md` | what is and is not covered, health-data hardening |
| `POSTGRES.md`, `POWERBI.md`, `GAAP.md` | backend, BI and ledger details |
| `docs/opencypher.md`, `docs/performance.md` | dialect coverage and performance plan |
| `IMPROVEMENTS_v0.N.md` | per-release findings, fixes and limits |
| `CRYPTO_COMPLIANCE_TEST_REPORT.md` | large-dataset measurements behind v0.25 |
