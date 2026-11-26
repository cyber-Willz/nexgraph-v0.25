//! PostgreSQL storage backend (feature `postgres`).
//!
//! The engine itself is unchanged: queries still run against the in-memory
//! MVCC store. What changes is *where commits are made durable*. Instead of a
//! WAL file plus `checkpoint.bin`, every commit is one row in
//! `<schema>.commits` and checkpoints live in `<schema>.checkpoint_chunks`,
//! all inside a PostgreSQL schema. On top of that the backend can maintain a
//! **relational mirror** (`nodes`, `edges`, `edge_members`, views `binary_edges` and the Power BI views `bi_nodes`, `bi_edges`, `bi_edge_members`)
//! updated *in the same PostgreSQL transaction* as the commit record, so plain
//! SQL, BI tools and `pg_dump` see the live graph with no ETL.
//!
//! ```text
//!   Txn::commit
//!     validate (read lock)
//!     BEGIN  ── INSERT commits(ts, crc, payload)
//!            ── upsert/delete mirror rows            (one PG transaction)
//!            ── UPDATE meta.mirror_ts
//!     COMMIT                                          <- durable here
//!     apply to the in-memory store, publish ts
//! ```
//!
//! Guarantees and limits are listed in `POSTGRES.md`. In short: one writer per
//! schema (enforced with a session advisory lock), any number of read-only
//! replicas that `refresh()`, a lost connection is re-established and the
//! in-flight commit is resolved exactly (landed or not), and the mirror is
//! derived data that can be rebuilt or verified at any time.

use crate::db::{frame, parse_frames, Database, Dur, Writer};
use crate::error::{Error, Result};
use crate::model::{Body, Commit, Op};
use crate::store::Inner;
use crate::value::{crc32, Dec, Enc, Id, Props, Value};
use postgres::error::SqlState;
use postgres::{Client, Config, GenericClient, IsolationLevel, Statement};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};

/// Rows per mirror statement.
const BATCH: usize = 2000;
const FORMAT_VERSION: &str = "1";

/// Options for opening a PostgreSQL-backed database.
#[derive(Clone, Debug)]
pub struct PgOptions {
    /// Schema that holds this database (created if missing). One schema is one
    /// nexgraph database; several can share a PostgreSQL database.
    pub schema: String,
    /// Maintain the relational mirror (`nodes`, `edges`, `edge_members`).
    pub mirror: bool,
    /// Open without taking the writer lock and without writing anything. Use
    /// [`Database::refresh`] to follow the writer.
    pub read_only: bool,
    /// `false` sets `synchronous_commit = off` for this session: much faster
    /// commits, but the last few commits can be lost if the *server* crashes
    /// (never corrupted, and a client crash loses nothing).
    pub synchronous_commit: bool,
    /// Reconnect (and resolve the in-flight commit) after a lost connection.
    pub reconnect: bool,
    /// Checkpoint payloads are split into rows of at most this many bytes.
    pub checkpoint_chunk_bytes: usize,
    /// Force TLS regardless of `sslmode` in the URL (refuses to connect in plaintext).
    pub require_tls: bool,
    /// Trust settings for TLS (certificates are always verified unless the `danger_` flag is set).
    pub tls: crate::tls::PgTls,
    /// Encrypt commit and checkpoint payloads at rest (see [`crate::crypto`]). Requires `mirror(false)`:
    /// the relational mirror is plaintext SQL by design.
    pub keys: Option<std::sync::Arc<crate::crypto::Keyring>>,
}

impl Default for PgOptions {
    fn default() -> Self {
        PgOptions {
            schema: "nexgraph".into(),
            mirror: true,
            read_only: false,
            synchronous_commit: true,
            reconnect: true,
            checkpoint_chunk_bytes: 8 << 20,
            require_tls: false,
            tls: Default::default(),
            keys: None,
        }
    }
}

impl PgOptions {
    pub fn schema(mut self, s: impl Into<String>) -> Self {
        self.schema = s.into();
        self
    }
    pub fn mirror(mut self, on: bool) -> Self {
        self.mirror = on;
        self
    }
    pub fn read_only(mut self, on: bool) -> Self {
        self.read_only = on;
        self
    }
    pub fn synchronous_commit(mut self, on: bool) -> Self {
        self.synchronous_commit = on;
        self
    }
    pub fn require_tls(mut self, on: bool) -> Self {
        self.require_tls = on;
        self
    }
    /// Encrypt this database at rest with `keys`. The schema is marked encrypted: it can no longer be opened
    /// without a key ring that holds the key it was sealed with, and existing plaintext data is refused.
    pub fn encrypt(mut self, keys: crate::crypto::Keyring) -> Self {
        self.keys = Some(std::sync::Arc::new(keys));
        self
    }
    /// CA bundle (PEM) that must have signed the server certificate.
    pub fn tls_ca_file(mut self, p: impl Into<std::path::PathBuf>) -> Self {
        self.tls.ca_file = Some(p.into());
        self
    }
}

/// Facts about the writer's PostgreSQL session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PgSessionInfo {
    pub tls: bool,
    pub tls_version: Option<String>,
    pub cipher: Option<String>,
}

/// Row counts of the relational mirror.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MirrorStats {
    pub nodes: u64,
    pub edges: u64,
    pub edge_members: u64,
    /// Commit timestamp the mirror reflects (`None` if never written).
    pub mirror_ts: Option<u64>,
}

/// Result of comparing the mirror with the live in-memory state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MirrorReport {
    pub nodes_checked: u64,
    pub edges_checked: u64,
    /// Entities missing from, or different in, the mirror (first 20 ids).
    pub mismatched: Vec<Id>,
    pub mismatched_total: u64,
    /// Mirror rows that no live entity corresponds to.
    pub extra_rows: u64,
    /// `edge_members` rows that disagree with the edges' tail/head arrays.
    pub member_mismatches: u64,
}

impl MirrorReport {
    pub fn is_clean(&self) -> bool {
        self.mismatched_total == 0 && self.extra_rows == 0 && self.member_mismatches == 0
    }
}

/// What [`Database::refresh`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refresh {
    pub applied_commits: usize,
    /// The primary checkpointed past this replica; state was reloaded.
    pub reloaded: bool,
    pub last_ts: u64,
}

pub(crate) struct Loaded {
    pub last_ts: u64,
    pub next_id: Id,
    pub horizon: u64,
}

// ------------------------------------------------------------------ store

struct MirrorStmts {
    up_nodes: Statement,
    new_edges: Statement,
    mod_edges: Statement,
    del_edges: Statement,
    del_nodes: Statement,
    set_ts: Statement,
}

struct Stmts {
    ins_commit: Statement,
    mirror: Option<MirrorStmts>,
}

pub(crate) struct PgStore {
    cfg: Config,
    opts: PgOptions,
    client: std::mem::ManuallyDrop<Client>,
    st: Option<Stmts>,
}

/// `postgres::Client` runs its own runtime and panics when dropped inside an async context
/// (for example when a `Database` is released in a tokio task). Dropping it on a helper thread
/// is always safe.
pub(crate) fn drop_client(c: Client) {
    let _ = std::thread::spawn(move || drop(c)).join();
}

impl Drop for PgStore {
    fn drop(&mut self) {
        // SAFETY: `client` is never used after this point.
        let c = unsafe { std::mem::ManuallyDrop::take(&mut self.client) };
        drop_client(c);
    }
}

fn q(schema: &str) -> String {
    format!("\"{schema}\"")
}

fn check_schema(s: &str) -> Result<()> {
    let ok = !s.is_empty()
        && s.len() <= 63
        && s.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(format!("schema name {s:?} must match [A-Za-z_][A-Za-z0-9_]{{0,62}}")))
    }
}

fn lock_key(schema: &str) -> (i32, i32) {
    let mut h: u32 = 0x811c9dc5;
    for b in schema.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    (0x6e78_6772, h as i32) // "nxgr"
}

fn connect(cfg: &Config, opts: &PgOptions, take_lock: bool, lock_wait: std::time::Duration) -> Result<Client> {
    let mut cfg = cfg.clone();
    cfg.application_name(&format!("nexgraph-{}", opts.schema));
    if opts.require_tls {
        cfg.ssl_mode(postgres::config::SslMode::Require);
    }
    let mut c = cfg.connect(crate::tls::connector(&opts.tls)?)?;
    if !opts.synchronous_commit {
        c.batch_execute("SET synchronous_commit = off")?;
    }
    if take_lock {
        let (a, b) = lock_key(&opts.schema);
        // After a dropped connection the old backend can linger (holding the lock)
        // until the server notices; wait for it when reconnecting.
        let deadline = std::time::Instant::now() + lock_wait;
        let mut ok: bool = c.query_one("SELECT pg_try_advisory_lock($1, $2)", &[&a, &b])?.get(0);
        while !ok && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(100));
            ok = c.query_one("SELECT pg_try_advisory_lock($1, $2)", &[&a, &b])?.get(0);
        }
        if !ok {
            return Err(Error::Backend(format!(
                "schema \"{}\" is already open for writing by another process (single-writer lock); \
                 open it read-only or close the other writer",
                opts.schema
            )));
        }
    }
    Ok(c)
}

fn get_meta<C: GenericClient>(c: &mut C, schema: &str, key: &str) -> Result<Option<String>> {
    let sql = format!("SELECT value FROM {}.meta WHERE key = $1", q(schema));
    Ok(c.query_opt(sql.as_str(), &[&key])?.map(|r| r.get::<_, String>(0)))
}

fn set_meta<C: GenericClient>(c: &mut C, schema: &str, key: &str, val: &str) -> Result<()> {
    let sql = format!(
        "INSERT INTO {}.meta(key, value) VALUES ($1, $2) ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        q(schema)
    );
    c.execute(sql.as_str(), &[&key, &val])?;
    Ok(())
}

fn meta_u64<C: GenericClient>(c: &mut C, schema: &str, key: &str) -> Result<Option<u64>> {
    match get_meta(c, schema, key)? {
        None => Ok(None),
        Some(s) => s.parse::<u64>().map(Some).map_err(|_| Error::Corrupt(format!("meta {key} is not a number: {s:?}"))),
    }
}

impl PgStore {
    /// Connect, take the writer lock (unless read-only), create tables, load
    /// the durable state into `inner` and reconcile the mirror.
    pub(crate) fn open(url: &str, opts: PgOptions, inner: &mut Inner) -> Result<(PgStore, Loaded)> {
        check_schema(&opts.schema)?;
        if opts.keys.is_some() && opts.mirror {
            return Err(Error::Invalid("encryption at rest needs PgOptions::mirror(false): the relational mirror stores every property in plaintext".into()));
        }
        let cfg = Config::from_str(url).map_err(|e| Error::Invalid(format!("bad PostgreSQL connection string: {e}")))?;
        let read_only = opts.read_only;
        let client = connect(&cfg, &opts, !read_only, std::time::Duration::ZERO)?;
        let mut store = PgStore { cfg, opts, client: std::mem::ManuallyDrop::new(client), st: None };
        if !read_only {
            store.init_schema()?;
            store.prepare()?;
        } else if store.table_missing("meta")? {
            return Err(Error::Backend(format!("schema \"{}\" has no nexgraph tables; open it once read-write first", store.opts.schema)));
        }
        store.check_encryption_marker()?;
        let loaded = store.load(inner)?;
        if !read_only && store.opts.mirror {
            let ts = loaded.last_ts;
            let have = meta_u64(&mut *store.client, &store.opts.schema, "mirror_ts")?;
            if have != Some(ts) {
                store.rebuild_mirror(inner, ts)?;
            }
        }
        Ok((store, loaded))
    }

    fn table_missing(&mut self, t: &str) -> Result<bool> {
        let r = self.client.query_one(
            "SELECT to_regclass($1) IS NULL",
            &[&format!("{}.{}", q(&self.opts.schema), t)],
        )?;
        Ok(r.get(0))
    }

    fn init_schema(&mut self) -> Result<()> {
        let s = q(&self.opts.schema);
        let mut ddl = format!(
            "CREATE SCHEMA IF NOT EXISTS {s};
             REVOKE ALL ON SCHEMA {s} FROM PUBLIC;
             CREATE TABLE IF NOT EXISTS {s}.meta (key text PRIMARY KEY, value text NOT NULL);
             CREATE TABLE IF NOT EXISTS {s}.commits (
                 ts bigint PRIMARY KEY, crc bigint NOT NULL, payload bytea NOT NULL,
                 committed_at timestamptz NOT NULL DEFAULT now());
             CREATE TABLE IF NOT EXISTS {s}.checkpoint_chunks (
                 gen bigint NOT NULL, seq integer NOT NULL, payload bytea NOT NULL, PRIMARY KEY (gen, seq));"
        );
        if self.opts.mirror {
            ddl.push_str(&format!(
                "CREATE TABLE IF NOT EXISTS {s}.nodes (
                     id bigint PRIMARY KEY, labels text[] NOT NULL, props jsonb NOT NULL DEFAULT '{{}}',
                     created_ts bigint NOT NULL, updated_ts bigint NOT NULL);
                 CREATE INDEX IF NOT EXISTS nodes_labels_gin ON {s}.nodes USING gin (labels);
                 CREATE INDEX IF NOT EXISTS nodes_props_gin ON {s}.nodes USING gin (props jsonb_path_ops);
                 CREATE TABLE IF NOT EXISTS {s}.edges (
                     id bigint PRIMARY KEY, label text NOT NULL, weight double precision NOT NULL,
                     props jsonb NOT NULL DEFAULT '{{}}', tail bigint[] NOT NULL, head bigint[] NOT NULL,
                     created_ts bigint NOT NULL, updated_ts bigint NOT NULL);
                 CREATE INDEX IF NOT EXISTS edges_label_idx ON {s}.edges (label);
                 CREATE TABLE IF NOT EXISTS {s}.edge_members (
                     edge_id bigint NOT NULL REFERENCES {s}.edges(id) ON DELETE CASCADE,
                     node_id bigint NOT NULL,
                     role text NOT NULL CHECK (role IN ('tail', 'head')),
                     PRIMARY KEY (edge_id, node_id, role));
                 CREATE INDEX IF NOT EXISTS edge_members_node_idx ON {s}.edge_members (node_id);
                 CREATE OR REPLACE VIEW {s}.binary_edges AS
                     SELECT id, label, tail[1] AS src, head[1] AS dst, weight, props
                     FROM {s}.edges WHERE cardinality(tail) = 1 AND cardinality(head) = 1;
                 CREATE OR REPLACE VIEW {s}.bi_nodes AS
                     SELECT id, labels[1] AS label, array_to_string(labels, '|') AS labels, props, created_ts, updated_ts
                     FROM {s}.nodes;
                 CREATE OR REPLACE VIEW {s}.bi_edges AS
                     SELECT id, label, weight, cardinality(head) > 0 AS directed,
                            cardinality(tail) AS tail_count, cardinality(head) AS head_count,
                            CASE WHEN cardinality(tail) = 1 AND cardinality(head) = 1 THEN tail[1] END AS src,
                            CASE WHEN cardinality(tail) = 1 AND cardinality(head) = 1 THEN head[1] END AS dst,
                            props, created_ts, updated_ts
                     FROM {s}.edges;
                 CREATE OR REPLACE VIEW {s}.bi_edge_members AS
                     SELECT m.edge_id, m.node_id, m.role, e.label, e.weight
                     FROM {s}.edge_members m JOIN {s}.edges e ON e.id = m.edge_id;"
            ));
        }
        ddl.push_str(&format!("REVOKE ALL ON ALL TABLES IN SCHEMA {s} FROM PUBLIC;"));
        self.client.batch_execute(&ddl)?;
        let schema = self.opts.schema.clone();
        self.client.execute(
            format!("INSERT INTO {s}.meta(key, value) VALUES ('format_version', $1) ON CONFLICT (key) DO NOTHING").as_str(),
            &[&FORMAT_VERSION],
        )?;
        let v = get_meta(&mut *self.client, &schema, "format_version")?.unwrap_or_default();
        if v != FORMAT_VERSION {
            return Err(Error::Backend(format!("schema \"{schema}\" has storage format {v:?}; this build understands {FORMAT_VERSION:?}")));
        }
        Ok(())
    }

    fn prepare(&mut self) -> Result<()> {
        let s = q(&self.opts.schema);
        let ins_commit = self.client.prepare(&format!("INSERT INTO {s}.commits (ts, crc, payload) VALUES ($1, $2, $3)"))?;
        let mirror = if self.opts.mirror {
            let nl = |col: &str| format!("ARRAY(SELECT e::bigint FROM jsonb_array_elements_text({col}) WITH ORDINALITY a(e, o) ORDER BY o)");
            Some(MirrorStmts {
                up_nodes: self.client.prepare(&format!(
                    "INSERT INTO {s}.nodes (id, labels, props, created_ts, updated_ts)
                     SELECT id,
                            ARRAY(SELECT e FROM jsonb_array_elements_text(labels) WITH ORDINALITY a(e, o) ORDER BY o),
                            props, $2, $2
                     FROM jsonb_to_recordset($1::text::jsonb) AS t(id bigint, labels jsonb, props jsonb)
                     ON CONFLICT (id) DO UPDATE
                        SET labels = EXCLUDED.labels, props = EXCLUDED.props, updated_ts = EXCLUDED.updated_ts"
                ))?,
                new_edges: self.client.prepare(&format!(
                    "WITH t AS (
                         SELECT id, label, weight::float8 AS weight, props, tail, head
                         FROM jsonb_to_recordset($1::text::jsonb)
                              AS x(id bigint, label text, weight text, props jsonb, tail jsonb, head jsonb)
                     ), ins AS (
                         INSERT INTO {s}.edges (id, label, weight, props, tail, head, created_ts, updated_ts)
                         SELECT id, label, weight, props, {}, {}, $2, $2 FROM t
                         ON CONFLICT (id) DO UPDATE
                            SET label = EXCLUDED.label, weight = EXCLUDED.weight, props = EXCLUDED.props,
                                tail = EXCLUDED.tail, head = EXCLUDED.head, updated_ts = EXCLUDED.updated_ts
                         RETURNING id, tail, head
                     )
                     INSERT INTO {s}.edge_members (edge_id, node_id, role)
                     SELECT id, n, 'tail' FROM ins, unnest(tail) AS n
                     UNION
                     SELECT id, n, 'head' FROM ins, unnest(head) AS n
                     ON CONFLICT DO NOTHING",
                    nl("tail"),
                    nl("head")
                ))?,
                mod_edges: self.client.prepare(&format!(
                    "UPDATE {s}.edges e SET props = t.props, weight = t.weight::float8, updated_ts = $2
                     FROM jsonb_to_recordset($1::text::jsonb) AS t(id bigint, weight text, props jsonb)
                     WHERE e.id = t.id"
                ))?,
                del_edges: self.client.prepare(&format!("DELETE FROM {s}.edges WHERE id = ANY($1)"))?,
                del_nodes: self.client.prepare(&format!("DELETE FROM {s}.nodes WHERE id = ANY($1)"))?,
                set_ts: self.client.prepare(&format!(
                    "INSERT INTO {s}.meta(key, value) VALUES ('mirror_ts', $1) ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value"
                ))?,
            })
        } else {
            None
        };
        self.st = Some(Stmts { ins_commit, mirror });
        Ok(())
    }

    // -------------------------------------------------------------- load

    /// Replay checkpoint + commit log into `inner` inside one consistent snapshot.
    /// A schema is either encrypted or not. Opening an encrypted one without a key, or one holding plaintext
    /// records with a key, is refused rather than silently mixing the two.
    fn check_encryption_marker(&mut self) -> Result<()> {
        let schema = self.opts.schema.clone();
        let s = q(&schema);
        let marker = get_meta(&mut *self.client, &schema, "encryption")?;
        match (&marker, &self.opts.keys) {
            (Some(_), None) => Err(Error::Invalid(format!("schema \"{schema}\" is encrypted at rest: open it with PgOptions::encrypt and its key"))),
            (None, Some(_)) => {
                let used: i64 = self.client.query_one(format!("SELECT (SELECT count(*) FROM {s}.commits) + (SELECT count(*) FROM {s}.checkpoint_chunks)").as_str(), &[])?.get(0);
                if used > 0 {
                    return Err(Error::Invalid(format!("schema \"{schema}\" holds unencrypted records; refusing to mix them with encrypted ones (export and re-import into a new schema)")));
                }
                if !self.opts.read_only {
                    set_meta(&mut *self.client, &schema, "encryption", ENC_MARKER)?;
                }
                Ok(())
            }
            (Some(m), Some(_)) if m != ENC_MARKER => Err(Error::Invalid(format!("schema \"{schema}\" uses encryption format {m:?}; this build understands {ENC_MARKER:?}"))),
            _ => Ok(()),
        }
    }

    pub(crate) fn load(&mut self, inner: &mut Inner) -> Result<Loaded> {
        let schema = self.opts.schema.clone();
        let keys = self.opts.keys.clone();
        let mut tx = self.client.build_transaction().isolation_level(IsolationLevel::RepeatableRead).read_only(true).start()?;
        let l = load_into(&mut tx, &schema, inner, &keys)?;
        tx.commit()?;
        Ok(l)
    }

    // ------------------------------------------------------------ commit

    /// Make `c` durable (and mirror it) in one PostgreSQL transaction.
    /// `inner` is the state *before* `c`; `prev_ts` is its timestamp.
    pub(crate) fn append(&mut self, c: &Commit, inner: &Inner, prev_ts: u64) -> Result<()> {
        if self.opts.read_only {
            return Err(Error::Unsupported("database is open read-only".into()));
        }
        if self.client.is_closed() {
            self.reconnect()?;
        }
        let payload = seal_commit(&self.opts.keys, &self.opts.schema, c.ts, c.encode())?;
        let crc = crc32(&payload) as i64;
        let delta = if self.opts.mirror { Some(Delta::build(inner, c)) } else { None };
        match self.append_once(c.ts, crc, &payload, delta.as_ref()) {
            Ok(()) => Ok(()),
            Err(e) if self.client.is_closed() && self.opts.reconnect => {
                // The connection died mid-commit: the transaction either landed or not.
                self.reconnect()?;
                let schema = self.opts.schema.clone();
                let s = q(&schema);
                let latest: i64 = self
                    .client
                    .query_one(
                        format!(
                            "SELECT COALESCE((SELECT max(ts) FROM {s}.commits),
                                             (SELECT value::bigint FROM {s}.meta WHERE key = 'ckpt_ts'), 0)"
                        )
                        .as_str(),
                        &[],
                    )?
                    .get(0);
                if latest as u64 == c.ts {
                    let stored: Option<i64> = self
                        .client
                        .query_opt(format!("SELECT crc FROM {s}.commits WHERE ts = $1").as_str(), &[&(c.ts as i64)])?
                        .map(|r| r.get(0));
                    if stored == Some(crc) {
                        return Ok(()); // it did land
                    }
                    return Err(Error::Backend(format!("commit {} was written by another process after the connection dropped", c.ts)));
                }
                if latest as u64 == prev_ts {
                    return self.append_once(c.ts, crc, &payload, delta.as_ref()); // it did not; retry once
                }
                Err(e)
            }
            Err(e) => Err(e),
        }
    }

    fn append_once(&mut self, ts: u64, crc: i64, payload: &[u8], delta: Option<&Delta>) -> Result<()> {
        let st = self.st.as_ref().ok_or_else(|| Error::Backend("statements not prepared".into()))?;
        let mut tx = self.client.transaction()?;
        if let Err(e) = tx.execute(&st.ins_commit, &[&(ts as i64), &crc, &payload]) {
            if e.code() == Some(&SqlState::UNIQUE_VIOLATION) {
                return Err(Error::Backend(format!(
                    "commit ts {ts} already exists in schema \"{}\": another writer has written to it",
                    self.opts.schema
                )));
            }
            return Err(e.into());
        }
        if let (Some(m), Some(d)) = (&st.mirror, delta) {
            d.write(&mut tx, m, ts)?;
            tx.execute(&m.set_ts, &[&ts.to_string()])?;
        }
        tx.commit()?;
        Ok(())
    }

    fn reconnect(&mut self) -> Result<()> {
        if !self.opts.reconnect {
            return Err(Error::Backend("connection to PostgreSQL was lost and reconnect is disabled".into()));
        }
        // The advisory lock died with the old session; take it again. If someone
        // else grabbed it meanwhile this fails and the database stays unusable.
        let client = connect(&self.cfg, &self.opts, !self.opts.read_only, std::time::Duration::from_secs(10))?;
        drop_client(std::mem::replace(&mut *self.client, client));
        if !self.opts.read_only {
            self.prepare()?;
        }
        Ok(())
    }

    // -------------------------------------------------------- checkpoint

    /// Replace the checkpoint with `c` (the live state) and drop the commits it covers.
    pub(crate) fn checkpoint(&mut self, next_id: Id, c: &Commit) -> Result<()> {
        if self.client.is_closed() {
            self.reconnect()?;
        }
        let schema = self.opts.schema.clone();
        let s = q(&schema);
        let mut e = Enc::new();
        e.varint(next_id);
        e.buf.extend_from_slice(&c.encode());
        let chunk = self.opts.checkpoint_chunk_bytes.max(1024);
        let mut tx = self.client.transaction()?;
        let gen = meta_u64(&mut tx, &schema, "ckpt_gen")?.unwrap_or(0) + 1;
        let buf = frame(&match &self.opts.keys {
            Some(k) => k.seal(&ckpt_aad(&schema, gen), &e.buf)?,
            None => e.buf,
        });
        let ins = format!("INSERT INTO {s}.checkpoint_chunks (gen, seq, payload) VALUES ($1, $2, $3)");
        let mut n = 0i32;
        for part in buf.chunks(chunk) {
            tx.execute(ins.as_str(), &[&(gen as i64), &n, &part])?;
            n += 1;
        }
        set_meta(&mut tx, &schema, "ckpt_gen", &gen.to_string())?;
        set_meta(&mut tx, &schema, "ckpt_chunks", &n.to_string())?;
        set_meta(&mut tx, &schema, "ckpt_ts", &c.ts.to_string())?;
        set_meta(&mut tx, &schema, "next_id", &next_id.to_string())?;
        tx.execute(format!("DELETE FROM {s}.checkpoint_chunks WHERE gen < $1").as_str(), &[&(gen as i64)])?;
        tx.execute(format!("DELETE FROM {s}.commits WHERE ts <= $1").as_str(), &[&(c.ts as i64)])?;
        tx.commit()?;
        Ok(())
    }

    // ------------------------------------------------------------ mirror

    fn mirror_stmts(&self) -> Result<&MirrorStmts> {
        self.st
            .as_ref()
            .and_then(|s| s.mirror.as_ref())
            .ok_or_else(|| Error::Unsupported("the relational mirror is disabled for this connection".into()))
    }

    /// Recreate the mirror from the live in-memory state (one transaction).
    pub(crate) fn rebuild_mirror(&mut self, inner: &Inner, ts: u64) -> Result<()> {
        self.mirror_stmts()?;
        let s = q(&self.opts.schema);
        let st = self.st.as_ref().unwrap();
        let m = st.mirror.as_ref().unwrap();
        let mut tx = self.client.transaction()?;
        tx.batch_execute(&format!("TRUNCATE {s}.edge_members, {s}.edges, {s}.nodes"))?;
        let mut delta = Delta::default();
        for op in inner.snapshot_ops() {
            match op {
                Op::CreateNode { id, labels, props } => {
                    delta.nodes.push((id, labels, props));
                    if delta.nodes.len() >= BATCH {
                        delta.flush_nodes(&mut tx, m, ts)?;
                    }
                }
                Op::CreateEdge { id, label, tail, head, weight, props } => {
                    delta.new_edges.push((id, EdgeRow { label, tail, head, weight, props }));
                    if delta.new_edges.len() >= BATCH {
                        delta.flush_new_edges(&mut tx, m, ts)?;
                    }
                }
                _ => {}
            }
        }
        // Nodes before edges is only a convention here (no FK to nodes), but keep it tidy.
        delta.flush_nodes(&mut tx, m, ts)?;
        delta.flush_new_edges(&mut tx, m, ts)?;
        tx.execute(&m.set_ts, &[&ts.to_string()])?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn mirror_stats(&mut self) -> Result<MirrorStats> {
        self.mirror_stmts()?;
        let schema = self.opts.schema.clone();
        let s = q(&schema);
        let mut tx = self.client.build_transaction().isolation_level(IsolationLevel::RepeatableRead).read_only(true).start()?;
        let count = |tx: &mut postgres::Transaction<'_>, t: &str| -> Result<u64> {
            let n: i64 = tx.query_one(format!("SELECT count(*) FROM {s}.{t}").as_str(), &[])?.get(0);
            Ok(n as u64)
        };
        let out = MirrorStats {
            nodes: count(&mut tx, "nodes")?,
            edges: count(&mut tx, "edges")?,
            edge_members: count(&mut tx, "edge_members")?,
            mirror_ts: meta_u64(&mut tx, &schema, "mirror_ts")?,
        };
        tx.commit()?;
        Ok(out)
    }

    /// Compare the mirror with the live state `inner` row by row.
    pub(crate) fn verify_mirror(&mut self, inner: &Inner) -> Result<MirrorReport> {
        self.mirror_stmts()?;
        let s = q(&self.opts.schema);
        let mut tx = self.client.build_transaction().isolation_level(IsolationLevel::RepeatableRead).read_only(true).start()?;
        let mut rep = MirrorReport::default();
        let bad = |rep: &mut MirrorReport, ids: Vec<i64>| {
            rep.mismatched_total += ids.len() as u64;
            for id in ids {
                if rep.mismatched.len() < 20 {
                    rep.mismatched.push(id as Id);
                }
            }
        };
        let (mut nodes, mut edges): (Vec<(Id, Vec<String>, Props)>, Vec<(Id, EdgeRow)>) = (Vec::new(), Vec::new());
        let node_sql = format!(
            "SELECT t.id FROM jsonb_to_recordset($1::text::jsonb) AS t(id bigint, labels jsonb, props jsonb)
             LEFT JOIN {s}.nodes n ON n.id = t.id
             WHERE n.id IS NULL OR n.props IS DISTINCT FROM t.props
                OR n.labels IS DISTINCT FROM ARRAY(SELECT e FROM jsonb_array_elements_text(t.labels) WITH ORDINALITY a(e, o) ORDER BY o)"
        );
        let edge_sql = format!(
            "SELECT t.id FROM jsonb_to_recordset($1::text::jsonb)
                    AS t(id bigint, label text, weight text, props jsonb, tail jsonb, head jsonb)
             LEFT JOIN {s}.edges e ON e.id = t.id
             WHERE e.id IS NULL OR e.label IS DISTINCT FROM t.label OR e.props IS DISTINCT FROM t.props
                OR e.weight IS DISTINCT FROM t.weight::float8
                OR e.tail IS DISTINCT FROM ARRAY(SELECT x::bigint FROM jsonb_array_elements_text(t.tail) WITH ORDINALITY a(x, o) ORDER BY o)
                OR e.head IS DISTINCT FROM ARRAY(SELECT x::bigint FROM jsonb_array_elements_text(t.head) WITH ORDINALITY a(x, o) ORDER BY o)"
        );
        for op in inner.snapshot_ops() {
            match op {
                Op::CreateNode { id, labels, props } => {
                    nodes.push((id, labels, props));
                    rep.nodes_checked += 1;
                    if nodes.len() >= BATCH {
                        let json = nodes_json(&nodes);
                        let ids: Vec<i64> = tx.query(node_sql.as_str(), &[&json])?.iter().map(|r| r.get(0)).collect();
                        bad(&mut rep, ids);
                        nodes.clear();
                    }
                }
                Op::CreateEdge { id, label, tail, head, weight, props } => {
                    edges.push((id, EdgeRow { label, tail, head, weight, props }));
                    rep.edges_checked += 1;
                    if edges.len() >= BATCH {
                        let json = edges_json(&edges, true);
                        let ids: Vec<i64> = tx.query(edge_sql.as_str(), &[&json])?.iter().map(|r| r.get(0)).collect();
                        bad(&mut rep, ids);
                        edges.clear();
                    }
                }
                _ => {}
            }
        }
        if !nodes.is_empty() {
            let json = nodes_json(&nodes);
            let ids: Vec<i64> = tx.query(node_sql.as_str(), &[&json])?.iter().map(|r| r.get(0)).collect();
            bad(&mut rep, ids);
        }
        if !edges.is_empty() {
            let json = edges_json(&edges, true);
            let ids: Vec<i64> = tx.query(edge_sql.as_str(), &[&json])?.iter().map(|r| r.get(0)).collect();
            bad(&mut rep, ids);
        }
        // Rows in the mirror that no live entity of the right kind corresponds to
        // (an exact check, not a count comparison: a missing row plus a planted one must not cancel out).
        for (table, want_edge) in [("nodes", false), ("edges", true)] {
            for r in tx.query(format!("SELECT id FROM {s}.{table}").as_str(), &[])? {
                let id: i64 = r.get(0);
                let ok = id >= 0 && inner.live_body(id as Id).is_some_and(|b| b.is_edge() == want_edge);
                if !ok {
                    rep.extra_rows += 1;
                }
            }
        }
        // edge_members must equal the distinct (edge, node, role) triples of tail/head.
        let derived = format!(
            "SELECT id AS edge_id, n AS node_id, 'tail'::text AS role FROM {s}.edges, unnest(tail) AS n
             UNION SELECT id, n, 'head' FROM {s}.edges, unnest(head) AS n"
        );
        let n: i64 = tx
            .query_one(
                format!(
                    "SELECT (SELECT count(*) FROM (({derived}) EXCEPT SELECT edge_id, node_id, role FROM {s}.edge_members) a)
                          + (SELECT count(*) FROM (SELECT edge_id, node_id, role FROM {s}.edge_members EXCEPT ({derived})) b)"
                )
                .as_str(),
                &[],
            )?
            .get(0);
        rep.member_mismatches = n as u64;
        tx.commit()?;
        Ok(rep)
    }
}

/// Replay a snapshot of the durable state into `inner`.
const ENC_MARKER: &str = "xchacha20poly1305-v1";

fn commit_aad(schema: &str, ts: u64) -> Vec<u8> {
    let mut a = format!("nexgraph/pg/commit/v1|{schema}|").into_bytes();
    a.extend_from_slice(&ts.to_le_bytes());
    a
}

fn ckpt_aad(schema: &str, gen: u64) -> Vec<u8> {
    let mut a = format!("nexgraph/pg/checkpoint/v1|{schema}|").into_bytes();
    a.extend_from_slice(&gen.to_le_bytes());
    a
}

fn seal_commit(keys: &Option<std::sync::Arc<crate::crypto::Keyring>>, schema: &str, ts: u64, plain: Vec<u8>) -> Result<Vec<u8>> {
    match keys {
        Some(k) => k.seal(&commit_aad(schema, ts), &plain),
        None => Ok(plain),
    }
}

fn open_commit(keys: &Option<std::sync::Arc<crate::crypto::Keyring>>, schema: &str, ts: u64, stored: &[u8]) -> Result<Commit> {
    match keys {
        Some(k) => Commit::decode(&k.open(&commit_aad(schema, ts), stored)?),
        None => Commit::decode(stored),
    }
}

fn load_into<C: GenericClient>(c: &mut C, schema: &str, inner: &mut Inner, keys: &Option<std::sync::Arc<crate::crypto::Keyring>>) -> Result<Loaded> {
    let s = q(schema);
    let v = get_meta(c, schema, "format_version")?;
    if v.as_deref() != Some(FORMAT_VERSION) {
        return Err(Error::Backend(format!("schema \"{schema}\" has storage format {v:?}; this build understands {FORMAT_VERSION:?}")));
    }
    let mut last_ts = 0u64;
    let mut next_id: Id = 1;
    if let (Some(gen), Some(ts), Some(n)) = (meta_u64(c, schema, "ckpt_gen")?, meta_u64(c, schema, "ckpt_ts")?, meta_u64(c, schema, "ckpt_chunks")?) {
        let rows = c.query(format!("SELECT payload FROM {s}.checkpoint_chunks WHERE gen = $1 ORDER BY seq").as_str(), &[&(gen as i64)])?;
        if rows.len() as u64 != n {
            return Err(Error::Corrupt(format!("checkpoint has {} chunks, expected {n}", rows.len())));
        }
        let mut buf = Vec::new();
        for r in &rows {
            buf.extend_from_slice(r.get::<_, &[u8]>(0));
        }
        let (p, good) = parse_frames(&buf)?;
        if p.len() != 1 || good != buf.len() {
            return Err(Error::Corrupt("checkpoint payload is damaged".into()));
        }
        let plain = match keys {
            Some(k) => k.open(&ckpt_aad(schema, gen), p[0])?,
            None => p[0].to_vec(),
        };
        let mut d = Dec::new(&plain);
        next_id = d.varint()?;
        let ck = Commit::decode(d.rest())?;
        if ck.ts != ts {
            return Err(Error::Corrupt(format!("checkpoint ts {} disagrees with meta ts {ts}", ck.ts)));
        }
        inner.apply(ck.ts, &ck.ops)?;
        last_ts = ck.ts;
    }
    let horizon = last_ts;
    let page = format!("SELECT ts, crc, payload FROM {s}.commits WHERE ts > $1 ORDER BY ts LIMIT 200");
    loop {
        let rows = c.query(page.as_str(), &[&(last_ts as i64)])?;
        if rows.is_empty() {
            break;
        }
        for r in &rows {
            let (ts, crc, payload): (i64, i64, &[u8]) = (r.get(0), r.get(1), r.get(2));
            if crc32(payload) as i64 != crc {
                return Err(Error::Corrupt(format!("commit {ts}: checksum mismatch")));
            }
            let cm = open_commit(keys, schema, ts as u64, payload)?;
            if cm.ts != ts as u64 || cm.ts != last_ts + 1 {
                return Err(Error::Corrupt(format!("commit log gap or reorder: expected ts {}, found {ts}", last_ts + 1)));
            }
            inner.apply(cm.ts, &cm.ops)?;
            last_ts = cm.ts;
        }
    }
    Ok(Loaded { last_ts, next_id: next_id.max(inner.max_id() + 1), horizon })
}

// ------------------------------------------------------------- mirror delta

struct EdgeRow {
    label: String,
    tail: Vec<Id>,
    head: Vec<Id>,
    weight: f64,
    props: Props,
}

enum Ov {
    New(Body),
    Mod(Body),
    Gone,
}

/// Final per-entity effect of one commit on the mirror, computed against the
/// pre-commit state by replaying the ops on an overlay of touched entities.
#[derive(Default)]
struct Delta {
    nodes: Vec<(Id, Vec<String>, Props)>,
    new_edges: Vec<(Id, EdgeRow)>,
    mod_edges: Vec<(Id, f64, Props)>,
    gone: Vec<i64>,
}

impl Delta {
    fn build(inner: &Inner, c: &Commit) -> Delta {
        let mut ov: HashMap<Id, Ov> = HashMap::new();
        fn cur(inner: &Inner, ov: &HashMap<Id, Ov>, id: Id) -> Option<Body> {
            match ov.get(&id) {
                Some(Ov::New(b)) | Some(Ov::Mod(b)) => Some(b.clone()),
                Some(Ov::Gone) => None,
                None => inner.live_body(id).map(|b| (**b).clone()),
            }
        }
        for op in &c.ops {
            match op {
                Op::CreateNode { id, labels, props } => {
                    ov.insert(*id, Ov::New(Body::Node { labels: labels.clone(), props: props.clone() }));
                }
                Op::CreateEdge { id, label, tail, head, weight, props } => {
                    ov.insert(*id, Ov::New(Body::Edge { label: label.clone(), tail: tail.clone(), head: head.clone(), weight: *weight, props: props.clone() }));
                }
                Op::Update { id, set, remove, add_labels, remove_labels, weight } => {
                    let Some(mut b) = cur(inner, &ov, *id) else { continue };
                    match &mut b {
                        Body::Node { labels, props } => {
                            for k in remove {
                                props.remove(k);
                            }
                            for (k, v) in set {
                                props.insert(k.clone(), v.clone());
                            }
                            labels.retain(|l| !remove_labels.contains(l));
                            for l in add_labels {
                                if !labels.contains(l) {
                                    labels.push(l.clone());
                                }
                            }
                        }
                        Body::Edge { props, weight: w, .. } => {
                            for k in remove {
                                props.remove(k);
                            }
                            for (k, v) in set {
                                props.insert(k.clone(), v.clone());
                            }
                            if let Some(nw) = weight {
                                *w = *nw;
                            }
                        }
                    }
                    let was_new = matches!(ov.get(id), Some(Ov::New(_)));
                    ov.insert(*id, if was_new { Ov::New(b) } else { Ov::Mod(b) });
                }
                Op::Delete { id } => match cur(inner, &ov, *id) {
                    None => {}
                    Some(Body::Edge { .. }) => {
                        ov.insert(*id, Ov::Gone);
                    }
                    Some(Body::Node { .. }) => {
                        for e in inner.live_incident(*id) {
                            if !matches!(ov.get(&e), Some(Ov::Gone)) {
                                ov.insert(e, Ov::Gone);
                            }
                        }
                        let touching: Vec<Id> = ov
                            .iter()
                            .filter_map(|(eid, o)| match o {
                                Ov::New(b @ Body::Edge { .. }) | Ov::Mod(b @ Body::Edge { .. }) if b.members().contains(id) => Some(*eid),
                                _ => None,
                            })
                            .collect();
                        for e in touching {
                            ov.insert(e, Ov::Gone);
                        }
                        ov.insert(*id, Ov::Gone);
                    }
                },
                Op::CreatePropIndex { .. } | Op::CreateVectorIndex { .. } => {}
            }
        }
        let mut d = Delta::default();
        let mut ids: Vec<Id> = ov.keys().copied().collect();
        ids.sort_unstable();
        for id in ids {
            match ov.remove(&id).unwrap() {
                Ov::Gone => d.gone.push(id as i64),
                Ov::New(Body::Node { labels, props }) | Ov::Mod(Body::Node { labels, props }) => d.nodes.push((id, labels, props)),
                Ov::New(Body::Edge { label, tail, head, weight, props }) => d.new_edges.push((id, EdgeRow { label, tail, head, weight, props })),
                Ov::Mod(Body::Edge { weight, props, .. }) => d.mod_edges.push((id, weight, props)),
            }
        }
        d
    }

    fn flush_nodes<C: GenericClient>(&mut self, c: &mut C, m: &MirrorStmts, ts: u64) -> Result<()> {
        for part in self.nodes.chunks(BATCH) {
            c.execute(&m.up_nodes, &[&nodes_json(part), &(ts as i64)])?;
        }
        self.nodes.clear();
        Ok(())
    }

    fn flush_new_edges<C: GenericClient>(&mut self, c: &mut C, m: &MirrorStmts, ts: u64) -> Result<()> {
        for part in self.new_edges.chunks(BATCH) {
            c.execute(&m.new_edges, &[&edges_json(part, true), &(ts as i64)])?;
        }
        self.new_edges.clear();
        Ok(())
    }

    fn write<C: GenericClient>(&self, c: &mut C, m: &MirrorStmts, ts: u64) -> Result<()> {
        let ts64 = ts as i64;
        for part in self.nodes.chunks(BATCH) {
            c.execute(&m.up_nodes, &[&nodes_json(part), &ts64])?;
        }
        for part in self.new_edges.chunks(BATCH) {
            c.execute(&m.new_edges, &[&edges_json(part, true), &ts64])?;
        }
        for part in self.mod_edges.chunks(BATCH) {
            let mut o = String::from("[");
            for (i, (id, w, props)) in part.iter().enumerate() {
                if i > 0 {
                    o.push(',');
                }
                let _ = write!(o, "{{\"id\":{id},\"weight\":");
                json_weight(&mut o, *w);
                o.push_str(",\"props\":");
                json_props(&mut o, props);
                o.push('}');
            }
            o.push(']');
            c.execute(&m.mod_edges, &[&o, &ts64])?;
        }
        if !self.gone.is_empty() {
            c.execute(&m.del_edges, &[&self.gone])?;
            c.execute(&m.del_nodes, &[&self.gone])?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- JSON text

/// Strings go to `jsonb`, which cannot hold U+0000; it is replaced by U+2400 in
/// the *mirror only* (the commit log is byte-exact).
fn json_str(o: &mut String, s: &str) {
    o.push('"');
    for ch in s.chars() {
        match ch {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            '\0' => o.push('\u{2400}'),
            c if (c as u32) < 0x20 => {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c => o.push(c),
        }
    }
    o.push('"');
}

fn json_f64(o: &mut String, f: f64) {
    if f.is_finite() {
        let _ = write!(o, "{f}");
    } else if f.is_nan() {
        o.push_str("\"NaN\"");
    } else if f > 0.0 {
        o.push_str("\"Infinity\"");
    } else {
        o.push_str("\"-Infinity\"");
    }
}

/// Weights travel as JSON strings and are cast to float8 in SQL (keeps NaN/Infinity).
fn json_weight(o: &mut String, f: f64) {
    if f.is_finite() {
        let _ = write!(o, "\"{f}\"");
    } else if f.is_nan() {
        o.push_str("\"NaN\"");
    } else if f > 0.0 {
        o.push_str("\"Infinity\"");
    } else {
        o.push_str("\"-Infinity\"");
    }
}

fn json_value(o: &mut String, v: &Value) {
    match v {
        Value::Null | Value::Node(_) | Value::Edge(_) => o.push_str("null"),
        Value::Bool(b) => o.push_str(if *b { "true" } else { "false" }),
        Value::Int(i) => {
            let _ = write!(o, "{i}");
        }
        Value::Float(f) => json_f64(o, *f),
        Value::Str(s) => json_str(o, s),
        Value::List(l) => {
            o.push('[');
            for (i, x) in l.iter().enumerate() {
                if i > 0 {
                    o.push(',');
                }
                json_value(o, x);
            }
            o.push(']');
        }
        Value::Vector(v) => {
            o.push('[');
            for (i, x) in v.iter().enumerate() {
                if i > 0 {
                    o.push(',');
                }
                json_f64(o, *x as f64);
            }
            o.push(']');
        }
    }
}

fn json_props(o: &mut String, p: &Props) {
    o.push('{');
    for (i, (k, v)) in p.iter().enumerate() {
        if i > 0 {
            o.push(',');
        }
        json_str(o, k);
        o.push(':');
        json_value(o, v);
    }
    o.push('}');
}

fn json_strs(o: &mut String, l: &[String]) {
    o.push('[');
    for (i, x) in l.iter().enumerate() {
        if i > 0 {
            o.push(',');
        }
        json_str(o, x);
    }
    o.push(']');
}

fn json_ids(o: &mut String, l: &[Id]) {
    o.push('[');
    for (i, x) in l.iter().enumerate() {
        if i > 0 {
            o.push(',');
        }
        let _ = write!(o, "{x}");
    }
    o.push(']');
}

fn nodes_json(rows: &[(Id, Vec<String>, Props)]) -> String {
    let mut o = String::with_capacity(rows.len() * 64);
    o.push('[');
    for (i, (id, labels, props)) in rows.iter().enumerate() {
        if i > 0 {
            o.push(',');
        }
        let _ = write!(o, "{{\"id\":{id},\"labels\":");
        json_strs(&mut o, labels);
        o.push_str(",\"props\":");
        json_props(&mut o, props);
        o.push('}');
    }
    o.push(']');
    o
}

fn edges_json(rows: &[(Id, EdgeRow)], with_members: bool) -> String {
    let mut o = String::with_capacity(rows.len() * 96);
    o.push('[');
    for (i, (id, e)) in rows.iter().enumerate() {
        if i > 0 {
            o.push(',');
        }
        let _ = write!(o, "{{\"id\":{id},\"label\":");
        json_str(&mut o, &e.label);
        o.push_str(",\"weight\":");
        json_weight(&mut o, e.weight);
        o.push_str(",\"props\":");
        json_props(&mut o, &e.props);
        if with_members {
            o.push_str(",\"tail\":");
            json_ids(&mut o, &e.tail);
            o.push_str(",\"head\":");
            json_ids(&mut o, &e.head);
        }
        o.push('}');
    }
    o.push(']');
    o
}

// ------------------------------------------------------------ Database API

impl Database {
    /// Open (or create) a database stored in the default `nexgraph` schema of
    /// the PostgreSQL database at `url` (URL or `key=value` connection string).
    ///
    /// ```no_run
    /// let db = nexgraph::Database::open_postgres("postgres://user:pw@localhost/mydb").unwrap();
    /// db.query("CREATE (:City {name:'Belize City'})").unwrap();
    /// ```
    pub fn open_postgres(url: &str) -> Result<Database> {
        Self::open_postgres_with(url, PgOptions::default())
    }

    pub fn open_postgres_with(url: &str, opts: PgOptions) -> Result<Database> {
        let read_only = opts.read_only;
        let mut inner = Inner::new();
        let (store, loaded) = PgStore::open(url, opts, &mut inner)?;
        Ok(Database {
            inner: RwLock::new(inner),
            writer: Mutex::new(Writer { dur: Dur::Pg(Box::new(store)), next_id: loaded.next_id, dir: None, read_only, _lock: None }),
            last_ts: AtomicU64::new(loaded.last_ts),
            horizon: AtomicU64::new(loaded.horizon),
        })
    }

    fn with_pg<R>(&self, f: impl FnOnce(&mut PgStore, &Inner) -> Result<R>) -> Result<R> {
        let mut w = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let g = self.inner.read().unwrap();
        match &mut w.dur {
            Dur::Pg(p) => f(p, &g),
            _ => Err(Error::Unsupported("this database is not stored in PostgreSQL".into())),
        }
    }

    /// Whether the writer's connection is encrypted, and with what.
    pub fn pg_session_info(&self) -> Result<PgSessionInfo> {
        self.with_pg(|p, _| {
            let r = p.client.query_one("SELECT ssl, version, cipher FROM pg_stat_ssl WHERE pid = pg_backend_pid()", &[])?;
            Ok(PgSessionInfo { tls: r.get(0), tls_version: r.get(1), cipher: r.get(2) })
        })
    }

    /// Let an existing PostgreSQL role read the mirror tables (SELECT only; the
    /// graph itself stays writable only through nexgraph).
    pub fn grant_mirror_select(&self, role: &str) -> Result<()> {
        check_schema(role).map_err(|_| Error::Invalid(format!("role name {role:?} must match [A-Za-z_][A-Za-z0-9_]{{0,62}}")))?;
        self.with_pg(|p, _| {
            if !p.opts.mirror {
                return Err(Error::Unsupported("the relational mirror is disabled for this connection".into()));
            }
            let s = q(&p.opts.schema);
            p.client.batch_execute(&format!(
                "GRANT USAGE ON SCHEMA {s} TO \"{role}\";
                 GRANT SELECT ON {s}.nodes, {s}.edges, {s}.edge_members, {s}.binary_edges, {s}.bi_nodes, {s}.bi_edges, {s}.bi_edge_members TO \"{role}\""
            ))?;
            Ok(())
        })
    }

    /// Row counts of the relational mirror.
    pub fn mirror_stats(&self) -> Result<MirrorStats> {
        self.with_pg(|p, _| p.mirror_stats())
    }

    /// Compare every live node and edge with its mirror row.
    pub fn verify_mirror(&self) -> Result<MirrorReport> {
        self.with_pg(|p, g| p.verify_mirror(g))
    }

    /// Recreate the mirror from the in-memory state.
    pub fn rebuild_mirror(&self) -> Result<MirrorStats> {
        let ts = self.last_ts();
        self.with_pg(|p, g| p.rebuild_mirror(g, ts))?;
        self.mirror_stats()
    }

    /// Follow the writer: apply commits made since this handle's last
    /// timestamp (or reload everything if the writer checkpointed past it).
    /// Only for databases opened with `PgOptions::read_only(true)`.
    pub fn refresh(&self) -> Result<Refresh> {
        let mut guard = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let w = &mut *guard;
        if !w.read_only {
            return Err(Error::Unsupported("refresh() is for read-only replicas; this handle is the writer".into()));
        }
        let Dur::Pg(p) = &mut w.dur else {
            return Err(Error::Unsupported("this database is not stored in PostgreSQL".into()));
        };
        if p.client.is_closed() {
            p.reconnect()?;
        }
        let schema = p.opts.schema.clone();
        let keys = p.opts.keys.clone();
        let last = self.last_ts();
        let s = q(&schema);
        // One consistent snapshot: checkpoint position and the commits after `last`.
        let mut tx = p.client.build_transaction().isolation_level(IsolationLevel::RepeatableRead).read_only(true).start()?;
        let ck = meta_u64(&mut tx, &schema, "ckpt_ts")?.unwrap_or(0);
        if ck > last {
            let mut fresh = Inner::new();
            let l = load_into(&mut tx, &schema, &mut fresh, &keys)?;
            tx.commit()?;
            let mut g = self.inner.write().unwrap();
            *g = fresh;
            self.horizon.store(l.horizon, Ordering::Release);
            self.last_ts.store(l.last_ts, Ordering::Release);
            w.next_id = l.next_id;
            return Ok(Refresh { applied_commits: 0, reloaded: true, last_ts: l.last_ts });
        }
        let mut fetched: Vec<Commit> = Vec::new();
        let mut at = last;
        loop {
            let rows = tx.query(
                format!("SELECT ts, crc, payload FROM {s}.commits WHERE ts > $1 ORDER BY ts LIMIT 200").as_str(),
                &[&(at as i64)],
            )?;
            if rows.is_empty() {
                break;
            }
            for r in &rows {
                let (ts, crc, payload): (i64, i64, &[u8]) = (r.get(0), r.get(1), r.get(2));
                if crc32(payload) as i64 != crc {
                    return Err(Error::Corrupt(format!("commit {ts}: checksum mismatch")));
                }
                let cm = open_commit(&keys, &schema, ts as u64, payload)?;
                if cm.ts != at + 1 {
                    return Err(Error::Corrupt(format!("commit log gap: expected ts {}, found {}", at + 1, cm.ts)));
                }
                at = cm.ts;
                fetched.push(cm);
            }
        }
        tx.commit()?;
        if !fetched.is_empty() {
            let mut g = self.inner.write().unwrap();
            for c in &fetched {
                g.apply(c.ts, &c.ops)?;
            }
            self.last_ts.store(at, Ordering::Release);
            let m = g.max_id() + 1;
            w.next_id = w.next_id.max(m);
        }
        Ok(Refresh { applied_commits: fetched.len(), reloaded: false, last_ts: at })
    }
}
