# nexgraph security (v0.6)

What is implemented, what is tested, and what is **not** covered. Read the limits before exposing this to real data.

## Architecture
```
client --HTTPS (TLS 1.2/1.3, rustls)--> nexgraph-server --TLS--> PostgreSQL
                                          |  Bearer token -> ControlPlane (users, tokens, audit)
                                          |  Principal    -> TenantManager -> Session
                                          '-> one nexgraph Database (= one PostgreSQL schema) per tenant
```
* **Library**: `nexgraph::security::{ControlPlane, TenantManager, Session}` (feature `security`, on by default).
* **Server**: `nexgraph-server` (feature `server`): `cargo build --release --features server`.
* **Transport**: PostgreSQL connections use rustls; certificates are verified (chain + host name) unless the explicit
  `danger_skip_cert_verification` flag is set. `PgOptions::require_tls(true)` refuses plaintext regardless of `sslmode`.
  The control plane and `nexgraph-server` require TLS to PostgreSQL by default (`--pg-allow-plaintext` to opt out).
  The server itself refuses to start without `--cert/--key` (plain HTTP only with `--insecure-http`, loopback only).

## Identity
* Passwords: Argon2id (default parameters of the `argon2` crate), 12-256 characters, not containing the user name, not one repeated character.
* Login failures are indistinguishable (unknown tenant/user, wrong password, disabled, locked) and an unknown user costs one dummy hash verification.
* Lockout: 5 consecutive failures lock the account for 15 minutes (configurable); an admin reset clears it. Per-IP login rate limit in the server (default 20/min).
* Tokens: 256 random bits, `ngt_` + base64url. Only SHA-256 of a token is stored. Default lifetime 8 h; revoked by logout, password change/reset, user or tenant disable.
* Limits on field sizes (user 64, tenant 32, password 256) bound hashing cost.

## Authorization
| role | data in own tenant | users of own tenant | tenants | audit |
|---|---|---|---|---|
| `reader` | read (MATCH/RETURN, `algo.*`, `spectral.*`, `physics.*`, `vector.*`) | - | - | - |
| `writer` | + CREATE / SET / DELETE | - | - | - |
| `tenant_admin` | + indexes, checkpoint, SQL-mirror grants | create (up to tenant_admin), disable, reset password | - | own tenant |
| `platform_admin` | **none** | any | create, disable, purge | all |

Statements are parsed and classified before execution; unknown procedures are refused (fail closed); a read clause cannot hide a write.

## Multi-tenancy
* **Isolation by construction**: each tenant has its own nexgraph database in its own PostgreSQL schema (`<control schema>_t<id>`)
  with its own commit log, checkpoint, mirror and writer lock. A session holds a handle to exactly one tenant database, taken from its token;
  no query or parameter can name another tenant. Whole-graph procedures only see the caller's graph.
* **PostgreSQL-level**: schemas revoke access from PUBLIC; `Session::grant_sql_reader(role)` gives an existing PostgreSQL role `SELECT` on *that tenant's* mirror tables only
  (tested: the role cannot read other tenants, the commit log or the control plane).
* **Quotas** per tenant: `max_entities` (soft: checked before statements that create) and `max_result_rows` (hard: oversized results are refused). Statements over 64 KiB are refused.
* **Offboarding**: disable (instant lock-out, tokens revoked) then purge (drops the graph schema, users, tokens; audit entries stay).

## Audit
Every login (ok/denied), query (reads configurable, writes/denials/errors always), and admin action is appended to a **hash-chained, append-only** table
(triggers refuse UPDATE/DELETE/TRUNCATE; each row's hash covers the previous row). Statement text and data are never logged, only a truncated SHA-256 of the statement.
`verify_audit_chain()` detects edited or removed rows.

## Run it (PowerShell)
```powershell
cargo build --release --features server
$env:NEXGRAPH_PG_URL = "host=db.example.com port=5432 user=nexgraph dbname=graph sslmode=require"
$env:NEXGRAPH_ADMIN_PASSWORD = "a long passphrase"
.\target\release\nexgraph-server.exe init  --pg-ca ca.pem                  # control plane + first platform admin
.\target\release\nexgraph-server.exe serve --pg-ca ca.pem --cert cert.pem --key key.pem --listen 0.0.0.0:8443
```
```powershell
$t = (Invoke-RestMethod https://localhost:8443/v1/login -Method Post -ContentType application/json `
      -Body '{"user":"admin","password":"a long passphrase"}').token
Invoke-RestMethod https://localhost:8443/v1/admin/tenants -Method Post -Headers @{Authorization="Bearer $t"} `
      -ContentType application/json -Body '{"name":"acme"}'
```
Use a dedicated PostgreSQL role for the service (it needs to create schemas in its database); use the `key=value` connection-string form if you need `hostaddr`.

## Tests (81 total; the security-relevant ones)
`tests/tls.rs` (4), `tests/security.rs` (11), `tests/server.rs` (4), unit tests in `src/security/mod.rs` (4). They run against real PostgreSQL 16 servers, one TLS-only,
with test-only certificates in `tests/fixtures` (regenerate with `gen_certs.sh`; never reuse them). They cover: certificate/host-name verification, plaintext refusal, TLS 1.3 on the API,
password/token storage, lockout, expiry, cross-tenant isolation (data, procedures, storage), role matrix, disable/purge, quotas, audit scoping and tamper detection,
PostgreSQL role grants, request size limits, uniform 401s, rate limiting. Deliberately breaking the reader check, the tenant-to-schema mapping and token expiry makes tests fail (see IMPROVEMENTS_v0.6.md).

## Not covered / known limits
* **Not independently audited.** The code was written with AI assistance and tested by its own suite. Get a review and a penetration test before relying on it.
* **Encryption at rest is opt-in (v0.19, see below)**: without `--master-key-file` the commit log, checkpoints and mirror are plaintext and anyone with PostgreSQL superuser access or the disk can read tenant data.
  Isolation between tenants also does not hold against a PostgreSQL superuser or a bug in nexgraph.
* **One process, one PostgreSQL cluster, shared RAM and CPU**: a tenant can consume memory (graphs live in RAM; quotas are soft) and run slow queries (a 30 s response timeout does not cancel the work).
  No per-tenant CPU or memory accounting.
* **Audit chain is tamper-evident, not tamper-proof**: someone able to rewrite the whole table can recompute a consistent chain. Export the latest `row_hash` to a place the database admin cannot write. Reads on the data path are audited best-effort; writes are audited fail-closed (an entry is written before the statement runs).
* **No MFA, SSO/OIDC, client certificates, token scopes or password-breach checks.** Tokens are bearer tokens: protect them like passwords.
* The per-IP login limit is in memory and per process; behind a reverse proxy every client shares the proxy's address (forwarded headers are not trusted).
* TLS certificates are loaded at start-up; rotating them needs a restart. Server TLS is TLS 1.2/1.3 with rustls defaults.
* The control plane uses one PostgreSQL connection behind a mutex (authentication calls are serialised). Throughput under load was not measured.
* Tenant databases stay loaded in memory until `close_all()`; there is no automatic eviction.
* Tested on Linux with PostgreSQL 16.15 only; not on Windows or other PostgreSQL versions.


## BI endpoints (v0.8)

`/v1/bi/*` accept HTTP Basic (`tenant/user:password`) in addition to Bearer, because Power BI refresh can only send static
credentials. Basic logins count toward the per-IP login limit; the issued token is cached in memory for at most 5 minutes
keyed by a SHA-256 of the header (no plaintext stored) and is re-validated on every request. `/v1/bi/query` accepts GET and
therefore executes **read-only statements only**, for every role. Static credentials in a Power BI dataset are only as safe as the
dataset's permissions: use a dedicated `reader` account per report. Not covered: per-table or per-column grants (a reader sees
every table of its tenant, unless a data policy is configured: see v0.19 below) and OAuth/SSO.


## Health-data hardening (v0.19)

Added after assessing the system for electronic health records. Everything below is exercised by `tests/ehr_*.rs` against the eICU demo patient table (1,447 ICU stays).
It narrows the gaps; it does **not** make the system HIPAA-compliant by itself, and it has not been reviewed by a security professional or a privacy officer.

**Data policy** (`--policy FILE.json`, `ManagerOptions::policy`, `src/security/policy.rs`). Rules cover roles up to the one named, so `tenant_admin` can stay unrestricted (and audited).
* `deny` properties / `deny_labels`: a statement that *mentions* one anywhere (pattern, `WHERE`, `RETURN`, `ORDER BY`, `SET`, `CREATE`, index, or as a name inside a procedure argument or `$parameter`) is refused before it runs. Refusing the mention closes the inference channel as well as the read. Matching is case-insensitive and conservative for variables whose label is unknown.
* `mask` properties (redact, keyed pseudonym, bucket, top-code): may only be returned as a plain projection and are masked in the result; they cannot be filtered, sorted, grouped or used in an expression, so a masked identifier cannot be probed by equality. `count(...)` over them is allowed.
* `min_cell`: aggregate-only roles must include `count(*)`; groups smaller than `k` are dropped; `collect`, `min`, `max` and record-level results are refused.
* BI export (`/v1/bi/*`) for restricted roles: no whole-graph `nodes`/`edges` tables (they hold every property as JSON), denied labels and columns removed, masks applied; aggregate-only roles get no table export. Bulk CSV import is closed to restricted roles.

**Record-level audit.** Reads by a restricted role are always audited and **fail closed**: if the audit row cannot be written the result is withheld. Entries carry the properties touched, rows returned, cells masked, groups suppressed and (for results containing node/edge references) the ids and a digest of them.

**Encryption at rest** (`--master-key-file`, `Database::open_encrypted`, `PgOptions::encrypt`, `src/crypto.rs`). XChaCha20-Poly1305, per-tenant keys derived with HKDF from a master secret, random 192-bit nonces, associated data binding each record to its store, kind and position (commit timestamp, checkpoint generation, WAL ordinal). Tampering, reordering, replaying within a log, and moving a record between tenants are detected; wrong key and mixed plaintext/encrypted data are refused. Rotation: add the old master as `--previous-master-key ID:FILE`, run a checkpoint, then drop it. The relational mirror is plaintext SQL, so it is disabled for encrypted tenants.

**De-identification and risk** (`src/deid.rs`): keyed pseudonyms, top-coding, bucketing, hour-only times, drop/redact, k-anonymity measurement and local suppression. On the demo file, 49% of stays were unique on five attributes (gender, age, ethnicity, hospital, discharge year); after generalising and dropping the site 3% were, and k = 5 suppression (131 records) removes the rest. Not scrubbed: free text.

**Fixed in existing code.**
* A statement nested a few thousand levels deep overflowed the stack and **aborted the whole server process** (all tenants) from any logged-in reader. The parser now refuses more than 64 nesting levels, 2,000 operators or 64 path hops.
* CSV exports (`to_csv`, BI CSV) let a stored value such as `=HYPERLINK(...)` run as a spreadsheet formula (CSV injection). Text cells starting with `= + - @`, tab or CR now get a leading `'`; `to_csv_raw` keeps exact text.

**Still not covered (health-relevant).**
* **No MFA/SSO, token scopes or break-glass workflow.** Bearer tokens remain bearer tokens.
* **Differencing**: two allowed aggregate answers can be subtracted to recover a group below `k` (shown in `known_limit_differencing_is_not_prevented`). Needs a query budget or differential-privacy noise.
* **Encryption gives confidentiality and integrity, not freshness**: rollback to an older complete copy, or dropping the newest log records, is not detected (`known_limit_tail_truncation_is_not_detected`). Keys live in process memory; commit timestamps and sizes are visible in PostgreSQL. Existing plaintext tenants must be exported and re-imported into encrypted ones; there is no in-place migration.
* **Masked columns are display-only**: you cannot average a masked column (it would use the raw values). Provide a separately generalised column for analysis.
* Audit rows are still hash-chained in the same database; export the head hash. Audit reads are by property, not by the identity of every projected value.
* No clinical vocabularies (FHIR/OMOP/ICD), survival analysis or clinical validation: see the earlier assessment. Research use on de-identified data only.

## Data profiling and automatic de-identification (v0.20)

Built and validated against eight public hospital data sets (a Synthea hospital database, admissions and operations tables, an ER log, an India provider list, small outcome and treatment tables). Needs features `crypto` + `ingest`.

```
nexgraph-deid profile FILE.csv
nexgraph-deid run FILE.csv --key-file KEY --out OUT.csv [--k 5] [--policy-out policy.json --label Stay]
```

* **profile**: classifies each column by name *and* content (person/staff name, address, ZIP, coordinates, place, birth date, dates, patient/record id, age, sex/race, free text, SSN/e-mail/phone-shaped values even in innocently named columns) and reports data-quality findings (duplicates, negatives, impossible ages, discharge before admission, future dates, constant or mostly-empty columns, inconsistent name case).
* **run**: builds a de-identification config from the profile and applies it: names/addresses/coordinates/places/free text dropped; birth date to a 5-year band; ZIP to 3 digits; ages over 89 top-coded; patient and record ids replaced by keyed pseudonyms that are the *same in every table* (joins keep working); dates shifted by a secret per-patient offset (never 0, interval lengths preserved). It then measures k-anonymity on the quasi-identifiers, suppresses small groups to reach k, and re-scans the output.
* **policy**: `--policy-out` writes a nexgraph data policy (deny/mask/min-cell) matching the profile.

New `deid` rules: `LinkedPseudonym`, `DateShift`, `DateYear`, `DateYearBucket`, `Truncate`, `TopBucket`.

Limits (read these): the classifier is heuristic and can miss an identifier it cannot recognise; free-text content is dropped, not scrubbed; date shifting is an Expert-Determination-style technique, not HIPAA Safe Harbor; k-anonymity covers only the listed quasi-identifiers; the result is not a legal HIPAA determination, and a person must review the report. Keep the key file apart from the data: anyone with the key can recompute the pseudonyms and offsets.

Findings from the data sets: two uploads were byte-identical; the admissions table has 534 duplicate rows, 108 negative amounts and pervasive name-case noise; the Synthea tables join on UUID ids, which the pipeline keeps joinable; 0.3% of admission dates were left unshifted by an early version (fixed, tested).

## openCypher support and a small-group fix (v0.21)

See `docs/opencypher.md` for the new clauses (WITH, OPTIONAL MATCH, UNWIND, MERGE, CASE, REMOVE, SET +=), functions and the measured compatibility (49 of 59 corpus queries), and `bench/README.md` for benchmarks.

**Fixed:** small-group suppression (`min_cell`) could be beaten by multiplying rows: a cross join or `UNWIND range(1,50)` made a group of one person look like a group of 1446 or 50 (reproduced on the eICU demo before the fix). Count-restricted roles are now limited to one MATCH with one pattern and no row-multiplying clauses, and must count `DISTINCT` nodes when the pattern has relationships. Affects v0.19 and v0.20; v0.21 is the fix. Tests: `tests/cypher_semantics.rs`.

Performance work in the same release (WITH fusion, wider factorised fast path, ORDER BY shortcut) is described in `docs/performance.md`; `tests/factorized.rs` checks the fast path against the streaming path.
