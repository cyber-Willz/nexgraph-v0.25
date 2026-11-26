# nexgraph and Power BI (v0.8)

Power BI wants **flat, typed tables**; nexgraph stores a property hypergraph. v0.8 defines one documented relational
projection (`src/bi.rs`) and offers four ways to get it into Power BI. Pick by where your data lives.

| Route | Use when | Refresh |
|---|---|---|
| **A. HTTPS API** `/v1/bi/*` (`nexgraph-server`) | multi-tenant / remote; Power BI Service scheduled refresh | live, per request |
| **B. PostgreSQL views** `bi_nodes`, `bi_edges`, `bi_edge_members` | you already run nexgraph on PostgreSQL; want DirectQuery or the native PostgreSQL connector | live (DirectQuery) or scheduled |
| **C. CSV folder** `.powerbi DIR` / `bi::export_dir` | embedded / file-WAL database, quick start, offline | re-run the export |
| **D. Ad-hoc NQL** `/v1/bi/query` | you want an analytic result (PageRank, chokepoints, beaconing...) as a table | live |

## The table model

| table | one row per | columns |
|---|---|---|
| `nodes` | node | `id, label, labels, props` |
| `edges` | edge / hyperedge | `id, label, weight, directed, tail_count, head_count, src, dst, tail, head, props` |
| `edge_members` | (edge, member) | `edge_id, node_id, role, label, weight` |
| `node_<Label>` | node with that label | `id, labels,` + one typed column per property |
| `edge_<Label>` | edge with that label | `id, weight, src, dst,` + one typed column per property |

* `props` is the whole property map as JSON text (`Json.Document`); the per-label tables are the typed view of it.
* `src`/`dst` exist only for binary edges. **Hyperedges are modelled by `edge_members`**: relate
  `nodes[id] 1—* edge_members[node_id]`, `edge_members[edge_id] *—1 edges[id]`. Use `edges[src]→nodes[id]` and
  `edges[dst]→nodes[id]` for ordinary directed graphs (Power BI allows only one *active* relationship; make `dst` inactive
  and use `USERELATIONSHIP`).
* Column types are inferred from all values: all-int → `int64`, int+float → `double`, all-bool → `boolean`, anything else
  (strings, lists, mixed) → `text`. A property called like a fixed column (`id`, `weight`...) is exposed as `prop_<name>`.
* Table names are `[A-Za-z0-9_]` only; odd labels are mapped to `_` and made unique.
* Timestamps are whatever you stored; the cyber loaders use epoch seconds. Convert with
  `#datetime(1970,1,1,0,0,0) + #duration(0,0,0,[ts])` (the generated script includes an `Epoch` helper).

## A. HTTPS API

```text
GET  /v1/bi/tables                      [?as_of=TS]   catalog: tables, columns, types, row counts
GET  /v1/bi/tables/{name}               [?format=csv|json&as_of=TS]
GET  /v1/bi/query?q=NQL                 [&format=csv|json]    read-only statements only
POST /v1/bi/query  {"query":..,"params":{..}}  [?format=..]
GET  /v1/bi/powerquery                  [?base=https://host:8443]   ready-made Power Query (M) script, no auth
```

**Authentication.** Power BI refresh can only send static credentials, so these endpoints accept either
`Authorization: Bearer <token>` or **HTTP Basic** with user `tenant/user` and the account password. Use a dedicated
`reader` account per report. Basic logins are cached server-side for at most 5 minutes (hashed key, so no Argon2id per
request) and share the per-IP login limit; the underlying token is still checked on every request, so disabling the user
or tenant takes effect immediately, and lockout rules apply. **Always serve over TLS** (the server refuses to start without it).

**Safety.** `/v1/bi/query` runs on GET, so it only executes read-only statements (a writer's `CREATE` is refused with 403).
Tenant row quotas apply to table and query results; reads are audited when the server runs with `ManagerOptions::audit_reads`.
`as_of=TS` time-travels to an earlier commit (trend reports over graph state); `x-nexgraph-ts` on each response is the snapshot used.

**In Power BI Desktop** — Get data → Blank query → Advanced editor, paste `GET /v1/bi/powerquery`, fill `User`/`Password`
(better: make them Parameters), set the data source credentials to *Anonymous* (the Authorization header authenticates).
Then `NexGraph[Table]("node_Host")`, `NexGraph[Query]("CALL algo.degree() YIELD node, degree RETURN node.ip AS ip, degree")`.

Gotchas: Power BI Service needs a *publicly resolvable* HTTPS endpoint with a certificate it trusts, or an on-premises
data gateway; a private CA works only through the gateway. Very long NQL in a GET hits URL limits: use views you
maintain server-side (not available yet), shorter statements, or the PostgreSQL route.

## B. PostgreSQL route

With `Database::open_postgres`, the mirror already has `nodes/edges/edge_members/binary_edges`; v0.8 adds flat views with the
same names and columns as the BI model (`bi_nodes`, `bi_edges`, `bi_edge_members`; `props` stays `jsonb`). In Power BI:
Get data → PostgreSQL database, pick the schema's `bi_*` views, expand `props` with *Parse JSON* → expand to columns.
Give the report a dedicated role: `Session::grant_sql_reader(role)` (tenant_admin) now grants these views too.
`created_ts/updated_ts` (commit timestamps) are included. DirectQuery works, but the mirror is only as fresh as the last commit.

## C. CSV folder

```text
nexgraph ./mydb
> .powerbi C:\reports\graph
```
writes `<table>.csv` (UTF-8), `catalog.json` and `nexgraph_folder.pq` (typed Power Query for the folder; set `Folder`).
All files come from one snapshot. Rust: `nexgraph::bi::export_dir(&db, dir)`.

## Verified here / not verified

* Verified: 13 unit-level tests (`tests/bi.rs`: projection against the graph, type inference, CSV quoting, naming safety, time
  travel, export, M bracket balance); an end-to-end HTTPS test with real TLS and PostgreSQL (`tests/server.rs`: Basic and
  Bearer, CSV/JSON, `as_of`, read-only enforcement, tenant isolation, disabled-account cut-off, error codes); a PostgreSQL
  test of the views incl. reopen and `rebuild_mirror`. The existing suites still pass. Mutation check: removing the read-only
  guard fails the end-to-end test.
* **Not verified: the Power Query (M) scripts were never run in Power BI Desktop** (none available here). They are small and
  syntactically checked only for bracket balance. Treat them as a template and report any error text.
* No OData endpoint (Power BI's OData connector would need `$filter/$select/$top` support; the Web connector with CSV is used instead).
* Table construction is a full scan per request and the catalog scans everything; no caching. Fine for 10^5–10^6 entities, not tuned beyond that.
* Per-label tables are rebuilt from the snapshot each time; very wide property sets make wide tables.
