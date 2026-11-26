//! One nexgraph database per tenant, reachable only through an authenticated [`Session`].

use super::control::{ControlPlane, TenantInfo};
use super::policy::Policy;
use crate::crypto::{Keyring, Secret};
use crate::query::parser;
use super::{classify_full, role_allows, Need, Principal, Role};
use crate::db::Database;
use crate::error::{Error, Result};
use crate::pg::PgOptions;
use crate::query::{Params, QueryResult};
use crate::store::StoreStats;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct ManagerOptions {
    /// Base options for tenant databases (the schema is chosen per tenant). TLS is required by default.
    pub pg: PgOptions,
    /// Longer statements are refused before parsing.
    pub max_statement_bytes: usize,
    /// Write an audit entry for read queries too (writes, denials and admin actions are always audited).
    /// Reads by a role the [`Policy`] restricts are *always* audited, and fail closed: if the entry cannot be
    /// written, the result is withheld.
    pub audit_reads: bool,
    /// Column / label access rules, masking and small-cell suppression (empty = none).
    pub policy: Policy,
    /// Master secret for encryption at rest (at least 32 random bytes; keep it in a KMS or a protected file,
    /// not in source). A separate key is derived per tenant. Setting it turns the relational mirror off for
    /// tenant databases, because the mirror is plaintext SQL.
    pub master_key: Option<Secret>,
    pub master_key_id: u32,
    /// Earlier master secrets still needed to read data sealed before a rotation.
    pub previous_master_keys: Vec<(u32, Secret)>,
}

impl Default for ManagerOptions {
    fn default() -> Self {
        ManagerOptions { pg: PgOptions::default().require_tls(true), max_statement_bytes: 64 * 1024, audit_reads: true, policy: Policy::default(), master_key: None, master_key_id: 1, previous_master_keys: vec![] }
    }
}

pub struct TenantManager {
    cp: Arc<ControlPlane>,
    url: String,
    opts: ManagerOptions,
    dbs: Mutex<HashMap<i64, Arc<Database>>>,
}

impl TenantManager {
    pub fn new(cp: Arc<ControlPlane>, pg_url: &str, opts: ManagerOptions) -> TenantManager {
        TenantManager { cp, url: pg_url.to_string(), opts, dbs: Mutex::new(HashMap::new()) }
    }

    pub fn control_plane(&self) -> &Arc<ControlPlane> {
        &self.cp
    }

    fn db_for(&self, t: &TenantInfo) -> Result<Arc<Database>> {
        let mut g = self.dbs.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(d) = g.get(&t.id) {
            return Ok(d.clone());
        }
        let mut pg = self.opts.pg.clone().schema(t.schema.clone());
        if let Some(m) = &self.opts.master_key {
            let prev: Vec<(u32, &[u8])> = self.opts.previous_master_keys.iter().map(|(i, k)| (*i, k.0.as_slice())).collect();
            let ring = Keyring::from_masters((self.opts.master_key_id, &m.0), &prev, &t.schema)?;
            pg = pg.mirror(false).encrypt(ring);
        }
        let db = Arc::new(Database::open_postgres_with(&self.url, pg)?);
        g.insert(t.id, db.clone());
        Ok(db)
    }

    /// Close every tenant database this manager has open (connections and writer locks are
    /// released once no session still uses them). Tenants are reopened on demand.
    pub fn close_all(&self) {
        self.dbs.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// Validate `token` and bind a session to its owner's tenant (and only that tenant).
    pub fn session(&self, token: &str) -> Result<Session> {
        let principal = self.cp.authenticate(token)?;
        let (tenant, db) = match principal.tenant_id {
            Some(id) => {
                let t = self.cp.tenant_by_id(id)?;
                if t.disabled {
                    return Err(Error::Auth("invalid credentials".into()));
                }
                let db = self.db_for(&t)?;
                (Some(t), Some(db))
            }
            None => (None, None),
        };
        Ok(Session { principal, tenant, db, cp: self.cp.clone(), opts: self.opts.clone() })
    }

    /// Irreversibly delete a (disabled) tenant: its graph schema, users and tokens. Audit entries stay.
    pub fn purge_tenant(&self, actor: &Principal, name: &str) -> Result<()> {
        if actor.role != Role::PlatformAdmin {
            let _ = self.cp.audit(actor.tenant_id, &actor.user, Some(actor.role), "purge_tenant", "denied", "requires platform_admin");
            return Err(Error::Denied("purge_tenant requires platform_admin".into()));
        }
        let t = self.cp.tenant(name)?;
        if !t.disabled {
            return Err(Error::Invalid("disable the tenant before purging it".into()));
        }
        let taken = self.dbs.lock().unwrap_or_else(|e| e.into_inner()).remove(&t.id);
        if let Some(mut arc) = taken {
            let t0 = Instant::now();
            // wait for in-flight sessions (new ones cannot start: the tenant is disabled)
            let db = loop {
                match Arc::try_unwrap(arc) {
                    Ok(db) => break db,
                    Err(a) => {
                        if t0.elapsed() > Duration::from_secs(10) {
                            self.dbs.lock().unwrap_or_else(|e| e.into_inner()).insert(t.id, a);
                            return Err(Error::Invalid("tenant still has active sessions; try again".into()));
                        }
                        arc = a;
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
            };
            drop(db); // closes the connection and releases the writer lock
        }
        self.cp.drop_tenant_schema(&t.schema)?;
        self.cp.delete_tenant_record(actor, &t)
    }
}

/// ` nodes=[ids...] node_count=N` for the node/edge references in a result (at most 100 listed), so the audit
/// trail can say *which records* a restricted role was shown, not only how many.
fn node_ids_detail(r: &QueryResult) -> String {
    let mut ids: Vec<u64> = vec![];
    for row in &r.rows {
        for v in row {
            if let crate::value::Value::Node(i) | crate::value::Value::Edge(i) = v {
                ids.push(*i);
            }
        }
    }
    if ids.is_empty() {
        return String::new();
    }
    ids.sort_unstable();
    ids.dedup();
    let listed: Vec<String> = ids.iter().take(100).map(|i| i.to_string()).collect();
    let all = Sha256::digest(ids.iter().flat_map(|i| i.to_le_bytes()).collect::<Vec<u8>>());
    format!(" nodes=[{}] node_count={} nodes_sha256={}", listed.join(","), ids.len(), super::hex(&all[..8]))
}

/// An authenticated caller bound to one tenant's database.
pub struct Session {
    principal: Principal,
    tenant: Option<TenantInfo>,
    db: Option<Arc<Database>>,
    cp: Arc<ControlPlane>,
    opts: ManagerOptions,
}

impl Session {
    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    fn data(&self, action: &str) -> Result<&Arc<Database>> {
        match &self.db {
            Some(d) => Ok(d),
            None => {
                let _ = self.cp.audit(None, &self.principal.user, Some(self.principal.role), action, "denied", "platform admins have no access to tenant data");
                Err(Error::Denied("platform administrators cannot access tenant data; use a tenant account".into()))
            }
        }
    }

    fn audit(&self, action: &str, outcome: &str, detail: &str) {
        let _ = self.cp.audit(self.principal.tenant_id, &self.principal.user, Some(self.principal.role), action, outcome, detail);
    }

    /// Run an NQL statement as this user, enforcing role, size limit, quotas and auditing.
    pub fn query(&self, text: &str, params: &Params) -> Result<QueryResult> {
        let db = self.data("query")?;
        let digest = || {
            let h = Sha256::digest(text.as_bytes());
            super::hex(&h[..8])
        };
        if text.len() > self.opts.max_statement_bytes {
            self.audit("query", "denied", &format!("statement too large ({} bytes)", text.len()));
            return Err(Error::Denied(format!("statement exceeds {} bytes", self.opts.max_statement_bytes)));
        }
        let (need, creates) = match classify_full(text) {
            Ok(n) => n,
            Err(Error::Denied(m)) => {
                self.audit("query", "denied", &format!("{m} sha256={}", digest()));
                return Err(Error::Denied(m));
            }
            Err(e) => {
                self.audit("query", "error", &format!("syntax sha256={}", digest()));
                return Err(e);
            }
        };
        if !role_allows(self.principal.role, need) {
            self.audit("query", "denied", &format!("{:?} needs more than {}; sha256={}", need, self.principal.role.as_str(), digest()));
            return Err(Error::Denied(format!("role {} may not run this kind of statement", self.principal.role.as_str())));
        }
        let quotas = self.tenant.as_ref().map(|t| t.quotas).unwrap_or_default();
        if creates {
            if let Some(max) = quotas.max_entities {
                let s: StoreStats = db.stats();
                if (s.live_nodes + s.live_edges) as u64 >= max {
                    self.audit("query", "denied", "entity quota reached");
                    return Err(Error::Denied(format!("tenant quota reached ({max} nodes+edges)")));
                }
            }
        }
        let policy = &self.opts.policy;
        let restricted = policy.restricts(self.principal.role);
        let plan = if policy.is_empty() {
            Default::default()
        } else {
            match parser::parse(text).and_then(|st| policy.check(self.principal.role, &st, params)) {
                Ok(p) => p,
                Err(Error::Denied(m)) => {
                    self.audit("query", "denied", &format!("policy: {m}; sha256={}", digest()));
                    return Err(Error::Denied(m));
                }
                Err(e) => {
                    self.audit("query", "error", &format!("syntax sha256={}", digest()));
                    return Err(e);
                }
            }
        };
        if need >= Need::Write {
            // Fail closed: a write that cannot be recorded is not executed.
            let kind = if need == Need::Write { "write" } else { "schema" };
            self.cp.audit(self.principal.tenant_id, &self.principal.user, Some(self.principal.role), "query", "allowed", &format!("kind={kind} sha256={}", digest()))?;
        }
        let res = db.query_with(text, params);
        match res {
            Ok(r) => {
                let (r, applied) = match policy.apply(&plan, r) {
                    Ok(x) => x,
                    Err(e) => {
                        self.audit("query", "denied", &format!("policy could not be applied; sha256={}", digest()));
                        return Err(e);
                    }
                };
                if let Some(max) = quotas.max_result_rows {
                    if r.rows.len() as u64 > max {
                        self.audit("query", "denied", &format!("result of {} rows exceeds limit {max}; sha256={}", r.rows.len(), digest()));
                        return Err(Error::Denied(format!("result has {} rows; the limit is {max}", r.rows.len())));
                    }
                }
                if need >= Need::Write || self.opts.audit_reads || restricted {
                    let mut detail = format!("kind={} rows={} sha256={}", if need == Need::Read { "read" } else if need == Need::Write { "write" } else { "schema" }, r.rows.len(), digest());
                    if !policy.is_empty() {
                        if !plan.props.is_empty() {
                            detail.push_str(&format!(" props={}", plan.props.join(",")));
                        }
                        if applied.masked_cells > 0 || applied.suppressed_rows > 0 {
                            detail.push_str(&format!(" masked_cells={} suppressed_rows={}", applied.masked_cells, applied.suppressed_rows));
                        }
                        if restricted {
                            detail.push_str(&node_ids_detail(&r));
                        }
                    }
                    if restricted {
                        // Fail closed: restricted data is not released unless the access is on the record.
                        self.cp.audit(self.principal.tenant_id, &self.principal.user, Some(self.principal.role), "query", "ok", &detail)?;
                    } else {
                        self.audit("query", "ok", &detail);
                    }
                }
                Ok(r)
            }
            Err(e) => {
                self.audit("query", "error", &format!("sha256={}", digest()));
                Err(e)
            }
        }
    }

    /// Bulk-load a CSV of incidents/events as one hyperedge per row (`writer` or above), de-duplicating per
    /// `opts`. Enforces the same role, entity quota and fail-closed audit as a write statement; the importer
    /// is all-or-nothing (a bad option writes nothing). Platform admins have no tenant data and are refused.
    #[cfg(feature = "ingest")]
    pub fn import_hyperedges_csv(&self, o: &crate::ingest::HyperCsv, csv: &str, opts: &crate::ingest::HyperOpts) -> Result<crate::ingest::ImportReport> {
        let db = self.data("import")?;
        if self.principal.role < Role::Writer {
            self.audit("import", "denied", "requires writer or above");
            return Err(Error::Denied(format!("role {} may not import data", self.principal.role.as_str())));
        }
        if self.opts.policy.restricts(self.principal.role) {
            self.audit("import", "denied", "bulk import writes arbitrary columns and is not available to a policy-restricted role");
            return Err(Error::Denied("bulk import is not available to your role under the data policy".into()));
        }
        if let Some(max) = self.tenant.as_ref().and_then(|t| t.quotas.max_entities) {
            let s: StoreStats = db.stats();
            if (s.live_nodes + s.live_edges) as u64 >= max {
                self.audit("import", "denied", "entity quota reached");
                return Err(Error::Denied(format!("tenant quota reached ({max} nodes+edges)")));
            }
        }
        self.cp.audit(self.principal.tenant_id, &self.principal.user, Some(self.principal.role), "import", "allowed", &format!("label={} bytes={}", o.edge_label, csv.len()))?;
        match crate::ingest::import_csv_hyperedges_with(db, o, csv, opts) {
            Ok(r) => {
                self.audit("import", "ok", &format!("edges={} nodes={} duplicates={} skipped={}", r.edges_created, r.nodes_created, r.duplicates_skipped, r.rows_skipped));
                Ok(r)
            }
            Err(e) => {
                self.audit("import", "error", "import failed");
                Err(e)
            }
        }
    }

    /// Run a statement that must not change anything (any role that may read). Used by GET-style endpoints,
    /// where a write would be a cross-site request forgery waiting to happen.
    pub fn query_read_only(&self, text: &str, params: &Params) -> Result<QueryResult> {
        match classify_full(text) {
            Ok((Need::Read, _)) => self.query(text, params),
            Ok(_) => {
                self.audit("query", "denied", "write attempted through a read-only endpoint");
                Err(Error::Denied("this endpoint only runs read-only statements".into()))
            }
            Err(e) => Err(e),
        }
    }

    /// The BI catalog (every table with its columns and row count) at the latest or an `as_of` snapshot.
    pub fn bi_tables(&self, as_of: Option<u64>) -> Result<(Vec<crate::bi::BiTable>, u64)> {
        let db = self.data("bi")?;
        let (tables, ts) = crate::bi::snapshot_tables(db, as_of)?;
        let tables = tables.into_iter().filter_map(|t| self.bi_apply_policy(t).ok()).collect::<Vec<_>>();
        let r = (tables, ts);
        self.bi_check_rows(r.0.iter().map(|t| t.rows.len() as u64).max().unwrap_or(0))?;
        if self.opts.audit_reads {
            self.audit("bi", "ok", &format!("catalog tables={}", r.0.len()));
        }
        Ok(r)
    }

    /// One BI table, subject to the tenant's result-row quota.
    pub fn bi_table(&self, name: &str, as_of: Option<u64>) -> Result<(crate::bi::BiTable, u64)> {
        let db = self.data("bi")?;
        let (t, ts) = crate::bi::snapshot_table(db, name, as_of)?;
        let t = match self.bi_apply_policy(t) {
            Ok(t) => t,
            Err(e) => {
                self.audit("bi", "denied", &format!("policy: table={name}"));
                return Err(e);
            }
        };
        let r = (t, ts);
        self.bi_check_rows(r.0.rows.len() as u64)?;
        if self.opts.audit_reads {
            self.audit("bi", "ok", &format!("table={} rows={}", r.0.name, r.0.rows.len()));
        }
        Ok(r)
    }

    /// Apply the data policy to a BI table. Whole-graph tables (`nodes`, `edges`) carry every property as one
    /// JSON blob and cannot be filtered per column, so a restricted role gets only label tables, minus denied
    /// columns, with masks applied. Aggregate-only roles get no record-level export at all.
    fn bi_apply_policy(&self, mut t: crate::bi::BiTable) -> Result<crate::bi::BiTable> {
        let role = self.principal.role;
        let pol = &self.opts.policy;
        if !pol.restricts(role) {
            return Ok(t);
        }
        if pol.aggregate_only_for(role) {
            return Err(Error::Denied("this role may only run aggregate queries; table export is record-level".into()));
        }
        match t.kind {
            "nodes" | "edges" => return Err(Error::Denied("whole-graph tables are not available to your role; use a label table".into())),
            "edge_members" => return Ok(t),
            _ => {}
        }
        // the real label, not the (sanitised, de-duplicated) table name; unknown label fails closed
        let Some(label) = t.label.clone() else {
            return Err(Error::Denied("table is not available to your role".into()));
        };
        if pol.label_denied(role, &label) {
            return Err(Error::Denied(format!("table {} is not available to your role", t.name)));
        }
        let mut keep: Vec<usize> = vec![];
        let mut masks: Vec<Option<super::policy::Mask>> = vec![];
        for (i, c) in t.columns.iter().enumerate() {
            let fixed = matches!(c.name.as_str(), "id" | "labels" | "weight" | "src" | "dst");
            // a property that collides with a fixed column is exported as `prop_<key>`: check both names
            let key = c.name.strip_prefix("prop_").unwrap_or(&c.name);
            if !fixed && (pol.column_denied(role, &label, &c.name) || pol.column_denied(role, &label, key)) {
                continue;
            }
            keep.push(i);
            masks.push(if fixed { None } else { pol.column_mask(role, &label, &c.name).or_else(|| pol.column_mask(role, &label, key)) });
        }
        let cols: Vec<crate::bi::BiColumn> = keep
            .iter()
            .zip(&masks)
            .map(|(&i, m)| {
                let mut c = t.columns[i].clone();
                if let Some(m) = m {
                    // a pseudonym or redaction is text; numeric masks keep numbers
                    if !matches!(m, super::policy::Mask::Bucket(_) | super::policy::Mask::TopCode(_)) || !matches!(c.ty, crate::bi::ColType::Int | crate::bi::ColType::Float) {
                        c.ty = crate::bi::ColType::Text;
                    }
                }
                c
            })
            .collect();
        for row in t.rows.iter_mut() {
            let mut out = Vec::with_capacity(keep.len());
            for (&i, m) in keep.iter().zip(&masks) {
                let v = std::mem::replace(&mut row[i], crate::value::Value::Null);
                out.push(match m {
                    Some(m) => pol.mask_value(m, &t.columns[i].name, &v),
                    None => v,
                });
            }
            *row = out;
        }
        if masks.iter().flatten().any(|m| *m == super::policy::Mask::Pseudonym) && pol.pseudonym_key.len() < 16 {
            return Err(Error::Denied("policy misconfigured: pseudonym masks need a key of at least 16 bytes".into()));
        }
        t.columns = cols;
        Ok(t)
    }

    fn bi_check_rows(&self, rows: u64) -> Result<()> {
        if let Some(max) = self.tenant.as_ref().and_then(|t| t.quotas.max_result_rows) {
            if rows > max {
                self.audit("bi", "denied", &format!("{rows} rows exceeds limit {max}"));
                return Err(Error::Denied(format!("table has {rows} rows; the limit is {max}")));
            }
        }
        Ok(())
    }

    pub fn stats(&self) -> Result<StoreStats> {
        let db = self.data("stats")?;
        if self.principal.role < Role::Reader {
            unreachable!()
        }
        Ok(db.stats())
    }

    /// Compact history (tenant_admin).
    pub fn checkpoint(&self) -> Result<u64> {
        let db = self.data("checkpoint")?;
        if self.principal.role != Role::TenantAdmin {
            self.audit("checkpoint", "denied", "requires tenant_admin");
            return Err(Error::Denied("checkpoint requires tenant_admin".into()));
        }
        let ts = db.checkpoint()?;
        self.audit("checkpoint", "ok", &format!("ts={ts}"));
        Ok(ts)
    }

    /// Let a PostgreSQL role read this tenant's SQL mirror (tenant_admin).
    pub fn grant_sql_reader(&self, pg_role: &str) -> Result<()> {
        let db = self.data("grant_sql_reader")?;
        if self.principal.role != Role::TenantAdmin {
            self.audit("grant_sql_reader", "denied", "requires tenant_admin");
            return Err(Error::Denied("grant_sql_reader requires tenant_admin".into()));
        }
        db.grant_mirror_select(pg_role)?;
        self.audit("grant_sql_reader", "ok", &format!("role={pg_role}"));
        Ok(())
    }
}
