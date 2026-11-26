# nexgraph on PostgreSQL (v0.5, TLS added in v0.6)

The query engine is unchanged: it still runs in memory. PostgreSQL now replaces the WAL file and
`checkpoint.bin` as the **durable store**, and can additionally keep a **relational mirror** of the live graph that
plain SQL can read.

## Use

```rust
use nexgraph::{Database, PgOptions};
let db = Database::open_postgres("postgres://user:pw@localhost/mydb")?;          // schema "nexgraph"
let db = Database::open_postgres_with(url, PgOptions::default().schema("crm").mirror(true))?;
db.query("CREATE (:Client {name:'Ada'})")?;      // one PostgreSQL transaction: commit row + mirror rows
```

CLI (PowerShell):

```powershell
cargo run --release --bin nexgraph -- "postgres://user:pw@localhost/mydb" --schema crm
# meta commands: .checkpoint .stats .mirror .verify .rebuild .refresh      flags: --no-mirror --read-only --async-commit
```

Run the PostgreSQL tests and benchmark:

```powershell
$env:NEXGRAPH_TEST_PG_URL = "postgres://user:pw@localhost/scratchdb"   # tests create/drop schemas nxt_*
cargo test --release --test pg -- --test-threads=1
$env:NEXGRAPH_PG_URL = $env:NEXGRAPH_TEST_PG_URL; cargo run --release --example bench_pg
```
Without the variable the PG tests print a notice and return. Use a scratch database.

## What is stored (schema = one nexgraph database)
| table | content |
|---|---|
| `meta` | format version, checkpoint position, mirror position |
| `commits` | one row per commit: `ts` (PK), CRC-32, binary payload (same codec as the file WAL, byte-exact) |
| `checkpoint_chunks` | latest checkpoint, split into rows of <= 8 MiB |
| `nodes`, `edges`, `edge_members`, view `binary_edges` | the mirror (live state only) |

Mirror columns: `nodes(id, labels text[], props jsonb, created_ts, updated_ts)`,
`edges(id, label, weight, props, tail bigint[], head bigint[], ...)`, `edge_members(edge_id, node_id, role)`.
```sql
SELECT s.props->>'name', d.props->>'name'
FROM crm.binary_edges e JOIN crm.nodes s ON s.id=e.src JOIN crm.nodes d ON d.id=e.dst WHERE e.label='KNOWS';
```

## Behaviour
* **Atomic**: commit row and mirror changes are one PG transaction; memory is updated only after it commits.
  A failed commit changes nothing (tested, including a foreign writer inserting the next `ts`).
* **Single writer**: a session advisory lock per schema; a second writer fails at open. Any number of
  `read_only(true)` replicas; call `db.refresh()` to follow (reloads automatically after a writer checkpoint).
* **Lost connection**: the writer reconnects, re-takes the lock (waits up to 10 s for the old backend to die) and
  resolves the in-flight commit exactly (landed -> accepted by CRC match; not landed -> retried once). Tested with `pg_terminate_backend`.
* **Integrity**: CRC per commit, contiguous-`ts` check, checkpoint chunk count/CRC verified on open; corruption is an error, never silent.
* **Mirror is derived**: `verify_mirror()` compares every row, `rebuild_mirror()` recreates it. If it was off (or is
  out of date) when the database is opened, it is rebuilt automatically.
* Time travel and `checkpoint()` semantics are the same as the file backend (history before a checkpoint is compacted;
  nothing auto-checkpoints, so `commits` grows until you call it).

## Limits (read these)
* **TLS** (v0.6): connections use rustls with certificate and host-name verification. `sslmode=require` in the URL, or
  `PgOptions::require_tls(true)`, refuses plaintext; `tls_ca_file(..)` trusts a private CA. Client-certificate (mTLS) authentication to PostgreSQL is not supported.
  The `postgres://` URL form rejects `hostaddr`; use the `key=value` form when you need it.
* **Poolers**: the writer lock is session-level; PgBouncer in transaction mode breaks it. Connect directly or use session pooling.
* The dataset still has to fit in RAM; PostgreSQL is storage, not a query backend. SQL over the mirror is far slower than NQL for traversals.
* The mirror is one-way. Edits made to mirror tables with SQL do not reach the graph (`verify_mirror` flags them, `rebuild_mirror` removes them);
  grant clients `SELECT` only.
* Mirror is lossy by design: `jsonb` cannot hold NUL (replaced by U+2400 in the mirror only), keeps no int/float or vector/list distinction,
  and non-finite float properties appear as strings. The commit log is exact.
* Bulk commits build a transient overlay of touched entities (extra RAM ~ commit size).
* Replicas poll (no push), and have no mirror handle.
* Tested only on PostgreSQL 16.15, Linux, one machine. **Not tested on Windows**, other PG versions, or networked servers.
* MSRV is now 1.85 (the `postgres` crates require it); `--no-default-features` removes the dependency.
