//! The control plane: tenants, users, passwords, tokens, lockout and the audit chain.

use super::{b64url, check_password, hex, random_bytes, valid_tenant_name, valid_user_name, Principal, Quotas, Role};
use crate::error::{Error, Result};
use crate::tls::PgTls;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use postgres::{Client, Config};
use sha2::{Digest, Sha256};
use std::str::FromStr;
use std::sync::Mutex;

#[derive(Clone, Debug)]
pub struct ControlOptions {
    /// Schema holding the control-plane tables.
    pub schema: String,
    /// Refuse plaintext connections to PostgreSQL (default **true**).
    pub require_tls: bool,
    pub tls: PgTls,
    /// Lifetime of a login token.
    pub token_ttl_secs: u64,
    /// Consecutive failed logins before an account is locked.
    pub max_failed_logins: i32,
    pub lockout_secs: u64,
}

impl Default for ControlOptions {
    fn default() -> Self {
        ControlOptions { schema: "nexgraph_admin".into(), require_tls: true, tls: PgTls::default(), token_ttl_secs: 8 * 3600, max_failed_logins: 5, lockout_secs: 900 }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssuedToken {
    /// Shown once; only its SHA-256 is stored.
    pub token: String,
    pub expires_in_secs: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TenantInfo {
    pub id: i64,
    pub name: String,
    pub disabled: bool,
    pub quotas: Quotas,
    /// PostgreSQL schema holding this tenant's graph: `<control schema>_t<id>`.
    pub schema: String,
}

/// One account as shown to administrators (no credentials).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserInfo {
    pub name: String,
    pub role: String,
    pub disabled: bool,
    pub tenant: Option<String>,
    pub created_at: String,
    pub last_login: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditEntry {
    pub id: i64,
    pub ts: String,
    pub tenant_id: Option<i64>,
    pub actor: String,
    pub role: Option<String>,
    pub action: String,
    pub outcome: String,
    pub detail: String,
}

pub struct ControlPlane {
    db: std::mem::ManuallyDrop<Mutex<Client>>,
    s: String,
    opts: ControlOptions,
    dummy_hash: String,
}

impl Drop for ControlPlane {
    fn drop(&mut self) {
        // SAFETY: `db` is never used after this point. See `pg::drop_client`.
        let m = unsafe { std::mem::ManuallyDrop::take(&mut self.db) };
        let client = m.into_inner().unwrap_or_else(|e| e.into_inner());
        crate::pg::drop_client(client);
    }
}

fn hash_password(pw: &str) -> Result<String> {
    let salt = SaltString::encode_b64(&random_bytes::<16>()?).map_err(|e| Error::Backend(format!("salt: {e}")))?;
    Argon2::default().hash_password(pw.as_bytes(), &salt).map(|h| h.to_string()).map_err(|e| Error::Backend(format!("password hashing failed: {e}")))
}

fn verify_password(pw: &str, hash: &str) -> bool {
    match PasswordHash::new(hash) {
        Ok(h) => Argon2::default().verify_password(pw.as_bytes(), &h).is_ok(),
        Err(_) => false,
    }
}

fn token_hash(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

fn chain_hash(prev: &[u8], ts: &str, tenant: Option<i64>, actor: &str, role: Option<&str>, action: &str, outcome: &str, detail: &str) -> Vec<u8> {
    let mut h = Sha256::new();
    let t = tenant.map(|x| x.to_string()).unwrap_or_else(|| "-".into());
    for f in [prev, ts.as_bytes(), t.as_bytes(), actor.as_bytes(), role.unwrap_or("-").as_bytes(), action.as_bytes(), outcome.as_bytes(), detail.as_bytes()] {
        h.update((f.len() as u64).to_le_bytes());
        h.update(f);
    }
    h.finalize().to_vec()
}

fn generic_auth_error() -> Error {
    Error::Auth("invalid credentials".into())
}

impl ControlPlane {
    /// Connect, create the control-plane tables if needed.
    pub fn open(url: &str, opts: ControlOptions) -> Result<ControlPlane> {
        let valid = !opts.schema.is_empty() && opts.schema.len() <= 40 && opts.schema.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && opts.schema.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            return Err(Error::Invalid(format!("schema name {:?} must match [A-Za-z_][A-Za-z0-9_]{{0,39}}", opts.schema)));
        }
        let mut cfg = Config::from_str(url).map_err(|e| Error::Invalid(format!("bad PostgreSQL connection string: {e}")))?;
        cfg.application_name("nexgraph-control");
        if opts.require_tls {
            cfg.ssl_mode(postgres::config::SslMode::Require);
        }
        let mut c = cfg.connect(crate::tls::connector(&opts.tls)?)?;
        let s = format!("\"{}\"", opts.schema);
        c.batch_execute(&format!(
            "CREATE SCHEMA IF NOT EXISTS {s};
             REVOKE ALL ON SCHEMA {s} FROM PUBLIC;
             CREATE TABLE IF NOT EXISTS {s}.tenants (
                 id bigserial PRIMARY KEY,
                 name text NOT NULL UNIQUE CHECK (name ~ '^[a-z][a-z0-9_]{{1,31}}$'),
                 max_entities bigint, max_result_rows bigint,
                 disabled boolean NOT NULL DEFAULT false,
                 created_at timestamptz NOT NULL DEFAULT now());
             CREATE TABLE IF NOT EXISTS {s}.users (
                 id bigserial PRIMARY KEY,
                 tenant_id bigint REFERENCES {s}.tenants(id) ON DELETE CASCADE,
                 name text NOT NULL, pw_hash text NOT NULL,
                 role text NOT NULL CHECK (role IN ('reader','writer','tenant_admin','platform_admin')),
                 disabled boolean NOT NULL DEFAULT false,
                 failed_attempts integer NOT NULL DEFAULT 0,
                 locked_until timestamptz,
                 created_at timestamptz NOT NULL DEFAULT now(),
                 last_login timestamptz,
                 CHECK ((role = 'platform_admin') = (tenant_id IS NULL)));
             CREATE UNIQUE INDEX IF NOT EXISTS users_name_uq ON {s}.users (COALESCE(tenant_id, 0), name);
             CREATE TABLE IF NOT EXISTS {s}.tokens (
                 id bigserial PRIMARY KEY,
                 user_id bigint NOT NULL REFERENCES {s}.users(id) ON DELETE CASCADE,
                 token_hash bytea NOT NULL UNIQUE,
                 expires_at timestamptz NOT NULL,
                 revoked boolean NOT NULL DEFAULT false,
                 created_at timestamptz NOT NULL DEFAULT now());
             CREATE TABLE IF NOT EXISTS {s}.audit (
                 id bigserial PRIMARY KEY, ts text NOT NULL, tenant_id bigint,
                 actor text NOT NULL, role text, action text NOT NULL, outcome text NOT NULL, detail text NOT NULL,
                 prev_hash bytea NOT NULL, row_hash bytea NOT NULL);
             CREATE INDEX IF NOT EXISTS audit_tenant_idx ON {s}.audit (tenant_id, id);
             CREATE OR REPLACE FUNCTION {s}.audit_immutable() RETURNS trigger LANGUAGE plpgsql
                 AS $$ BEGIN RAISE EXCEPTION 'the audit log is append-only'; END $$;
             DROP TRIGGER IF EXISTS audit_no_change ON {s}.audit;
             CREATE TRIGGER audit_no_change BEFORE UPDATE OR DELETE ON {s}.audit FOR EACH ROW EXECUTE FUNCTION {s}.audit_immutable();
             DROP TRIGGER IF EXISTS audit_no_truncate ON {s}.audit;
             CREATE TRIGGER audit_no_truncate BEFORE TRUNCATE ON {s}.audit FOR EACH STATEMENT EXECUTE FUNCTION {s}.audit_immutable();
             REVOKE ALL ON ALL TABLES IN SCHEMA {s} FROM PUBLIC;"
        ))?;
        // Used to spend the same time on unknown users as on known ones.
        let dummy_hash = hash_password(&b64url(&random_bytes::<18>()?))?;
        Ok(ControlPlane { db: std::mem::ManuallyDrop::new(Mutex::new(c)), s, opts, dummy_hash })
    }

    pub fn options(&self) -> &ControlOptions {
        &self.opts
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Client> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ---------------------------------------------------------------- audit

    /// Append to the hash-chained audit log. Never fails the caller's operation silently:
    /// errors are returned so security-relevant actions can refuse to proceed.
    pub fn audit(&self, tenant_id: Option<i64>, actor: &str, role: Option<Role>, action: &str, outcome: &str, detail: &str) -> Result<()> {
        let mut c = self.conn();
        self.audit_with(&mut c, tenant_id, actor, role.map(|r| r.as_str()), action, outcome, detail)
    }

    #[allow(clippy::too_many_arguments)]
    fn audit_with(&self, c: &mut Client, tenant_id: Option<i64>, actor: &str, role: Option<&str>, action: &str, outcome: &str, detail: &str) -> Result<()> {
        let s = &self.s;
        let mut tx = c.transaction()?;
        tx.batch_execute(&format!("LOCK TABLE {s}.audit IN SHARE ROW EXCLUSIVE MODE"))?;
        let prev: Vec<u8> = tx.query_opt(format!("SELECT row_hash FROM {s}.audit ORDER BY id DESC LIMIT 1").as_str(), &[])?.map(|r| r.get(0)).unwrap_or_else(|| vec![0u8; 32]);
        let ts: String = tx.query_one("SELECT to_char(clock_timestamp() AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')", &[])?.get(0);
        let actor: String = actor.chars().take(128).collect();
        let detail: String = detail.chars().take(512).collect();
        let h = chain_hash(&prev, &ts, tenant_id, &actor, role, action, outcome, &detail);
        tx.execute(
            format!("INSERT INTO {s}.audit (ts, tenant_id, actor, role, action, outcome, detail, prev_hash, row_hash) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)").as_str(),
            &[&ts, &tenant_id, &actor, &role, &action, &outcome, &detail, &prev, &h],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Re-compute the whole chain; returns the number of entries, or `Corrupt` naming the first bad one.
    pub fn verify_audit_chain(&self) -> Result<u64> {
        let mut c = self.conn();
        let rows = c.query(format!("SELECT id, ts, tenant_id, actor, role, action, outcome, detail, prev_hash, row_hash FROM {}.audit ORDER BY id", self.s).as_str(), &[])?;
        let mut prev = vec![0u8; 32];
        for r in &rows {
            let id: i64 = r.get(0);
            let (ts, tenant, actor, role, action, outcome, detail): (String, Option<i64>, String, Option<String>, String, String, String) = (r.get(1), r.get(2), r.get(3), r.get(4), r.get(5), r.get(6), r.get(7));
            let (p, h): (Vec<u8>, Vec<u8>) = (r.get(8), r.get(9));
            if p != prev || chain_hash(&p, &ts, tenant, &actor, role.as_deref(), &action, &outcome, &detail) != h {
                return Err(Error::Corrupt(format!("audit chain broken at entry {id}")));
            }
            prev = h;
        }
        Ok(rows.len() as u64)
    }

    /// Newest-first audit entries. A platform admin may read everything (or filter by tenant);
    /// a tenant admin only their own tenant's.
    pub fn read_audit(&self, actor: &Principal, tenant: Option<&str>, limit: i64) -> Result<Vec<AuditEntry>> {
        let tenant_id: Option<i64> = match actor.role {
            Role::PlatformAdmin => match tenant {
                Some(t) => Some(self.tenant(t)?.id),
                None => None,
            },
            Role::TenantAdmin => {
                if tenant.is_some_and(|t| Some(t) != actor.tenant.as_deref()) {
                    return Err(Error::Denied("you can only read your own tenant's audit log".into()));
                }
                actor.tenant_id
            }
            _ => return Err(Error::Denied("reading the audit log requires tenant_admin or platform_admin".into())),
        };
        let limit = limit.clamp(1, 1000);
        let rows = self.conn().query(
            format!("SELECT id, ts, tenant_id, actor, role, action, outcome, detail FROM {}.audit WHERE ($1::bigint IS NULL OR tenant_id = $1) ORDER BY id DESC LIMIT $2", self.s).as_str(),
            &[&tenant_id, &limit],
        )?;
        Ok(rows.iter().map(|r| AuditEntry { id: r.get(0), ts: r.get(1), tenant_id: r.get(2), actor: r.get(3), role: r.get(4), action: r.get(5), outcome: r.get(6), detail: r.get(7) }).collect())
    }

    // ------------------------------------------------------------ listings

    /// Every tenant with its quotas and state (`platform_admin`).
    pub fn list_tenants(&self, actor: &Principal) -> Result<Vec<TenantInfo>> {
        self.require_platform(actor, "list_tenants")?;
        let rows = self.conn().query(format!("SELECT id, name, disabled, max_entities, max_result_rows FROM {}.tenants ORDER BY name", self.s).as_str(), &[])?;
        Ok(rows.into_iter().map(|r| self.tenant_row(r)).collect())
    }

    /// Accounts of one tenant (`tenant_admin` of that tenant, or `platform_admin` naming it). Password hashes
    /// are never selected.
    pub fn list_users(&self, actor: &Principal, tenant: Option<&str>) -> Result<Vec<UserInfo>> {
        let tenant_id: Option<i64> = match actor.role {
            Role::PlatformAdmin => match tenant {
                Some(t) => Some(self.tenant(t)?.id),
                None => None,
            },
            Role::TenantAdmin => {
                if tenant.is_some_and(|t| Some(t) != actor.tenant.as_deref()) {
                    return Err(Error::Denied("you can only list your own tenant's users".into()));
                }
                actor.tenant_id
            }
            _ => return Err(Error::Denied("listing users requires tenant_admin or platform_admin".into())),
        };
        let rows = self.conn().query(
            format!("SELECT u.name, u.role, u.disabled, t.name, u.created_at::text, u.last_login::text FROM {s}.users u LEFT JOIN {s}.tenants t ON t.id = u.tenant_id WHERE ($1::bigint IS NULL OR u.tenant_id = $1) ORDER BY t.name NULLS FIRST, u.name", s = self.s).as_str(),
            &[&tenant_id],
        )?;
        Ok(rows.iter().map(|r| UserInfo { name: r.get(0), role: r.get(1), disabled: r.get(2), tenant: r.get(3), created_at: r.get(4), last_login: r.get(5) }).collect())
    }

    // -------------------------------------------------------------- tenants

    pub fn tenant(&self, name: &str) -> Result<TenantInfo> {
        let r = self.conn().query_opt(format!("SELECT id, name, disabled, max_entities, max_result_rows FROM {}.tenants WHERE name = $1", self.s).as_str(), &[&name])?;
        r.map(|r| self.tenant_row(r)).ok_or_else(|| Error::Invalid(format!("no such tenant {name:?}")))
    }

    pub fn tenant_by_id(&self, id: i64) -> Result<TenantInfo> {
        let r = self.conn().query_opt(format!("SELECT id, name, disabled, max_entities, max_result_rows FROM {}.tenants WHERE id = $1", self.s).as_str(), &[&id])?;
        r.map(|r| self.tenant_row(r)).ok_or_else(|| Error::Invalid(format!("no such tenant id {id}")))
    }

    fn require_platform(&self, actor: &Principal, what: &str) -> Result<()> {
        if actor.role != Role::PlatformAdmin {
            let _ = self.audit(actor.tenant_id, &actor.user, Some(actor.role), what, "denied", "requires platform_admin");
            return Err(Error::Denied(format!("{what} requires platform_admin")));
        }
        Ok(())
    }

    pub fn create_tenant(&self, actor: &Principal, name: &str, quotas: Quotas) -> Result<TenantInfo> {
        self.require_platform(actor, "create_tenant")?;
        if !valid_tenant_name(name) {
            return Err(Error::Invalid("tenant name must match [a-z][a-z0-9_]{1,31}".into()));
        }
        let (me, mr) = (quotas.max_entities.map(|x| x as i64), quotas.max_result_rows.map(|x| x as i64));
        let r = self.conn().query_one(format!("INSERT INTO {}.tenants (name, max_entities, max_result_rows) VALUES ($1,$2,$3) RETURNING id, name, disabled, max_entities, max_result_rows", self.s).as_str(), &[&name, &me, &mr]);
        let info = match r {
            Ok(r) => self.tenant_row(r),
            Err(e) if e.code() == Some(&postgres::error::SqlState::UNIQUE_VIOLATION) => return Err(Error::Invalid(format!("tenant {name:?} already exists"))),
            Err(e) => return Err(e.into()),
        };
        self.audit(Some(info.id), &actor.user, Some(actor.role), "create_tenant", "ok", &format!("tenant={name}"))?;
        Ok(info)
    }

    pub fn set_tenant_disabled(&self, actor: &Principal, name: &str, disabled: bool) -> Result<()> {
        self.require_platform(actor, "set_tenant_disabled")?;
        let t = self.tenant(name)?;
        let mut c = self.conn();
        c.execute(format!("UPDATE {}.tenants SET disabled = $2 WHERE id = $1", self.s).as_str(), &[&t.id, &disabled])?;
        if disabled {
            c.execute(format!("UPDATE {s}.tokens SET revoked = true WHERE user_id IN (SELECT id FROM {s}.users WHERE tenant_id = $1)", s = self.s).as_str(), &[&t.id])?;
        }
        self.audit_with(&mut c, Some(t.id), &actor.user, Some(actor.role.as_str()), "set_tenant_disabled", "ok", &format!("tenant={name} disabled={disabled}"))
    }

    /// Delete the tenant row, its users and tokens (audit entries stay). The caller must already have disabled it.
    pub(crate) fn delete_tenant_record(&self, actor: &Principal, t: &TenantInfo) -> Result<()> {
        self.require_platform(actor, "purge_tenant")?;
        let mut c = self.conn();
        c.execute(format!("DELETE FROM {}.tenants WHERE id = $1", self.s).as_str(), &[&t.id])?;
        self.audit_with(&mut c, Some(t.id), &actor.user, Some(actor.role.as_str()), "purge_tenant", "ok", &format!("tenant={} schema={}", t.name, t.schema))
    }

    /// Drop a tenant's graph schema (`t_<id>` only).
    pub(crate) fn drop_tenant_schema(&self, schema: &str) -> Result<()> {
        let prefix = format!("{}_t", self.opts.schema);
        let ok = schema.strip_prefix(prefix.as_str()).is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()));
        if !ok {
            return Err(Error::Invalid("refusing to drop a schema that is not a tenant schema".into()));
        }
        self.conn().batch_execute(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"))?;
        Ok(())
    }

    // ---------------------------------------------------------------- users

    /// First platform administrator. Refused if one already exists.
    pub fn bootstrap_platform_admin(&self, name: &str, password: &str) -> Result<()> {
        if !valid_user_name(name) {
            return Err(Error::Invalid("invalid user name".into()));
        }
        check_password(name, password)?;
        let hash = hash_password(password)?;
        let mut c = self.conn();
        let mut tx = c.transaction()?;
        tx.batch_execute(&format!("LOCK TABLE {}.users IN SHARE ROW EXCLUSIVE MODE", self.s))?;
        let n: i64 = tx.query_one(format!("SELECT count(*) FROM {}.users WHERE role = 'platform_admin'", self.s).as_str(), &[])?.get(0);
        if n > 0 {
            return Err(Error::Denied("a platform administrator already exists".into()));
        }
        tx.execute(format!("INSERT INTO {}.users (tenant_id, name, pw_hash, role) VALUES (NULL, $1, $2, 'platform_admin')", self.s).as_str(), &[&name, &hash])?;
        tx.commit()?;
        self.audit_with(&mut c, None, name, Some("platform_admin"), "bootstrap_platform_admin", "ok", "")
    }

    /// Platform admins may create any user; tenant admins only users of their own tenant (up to `tenant_admin`).
    pub fn create_user(&self, actor: &Principal, tenant: Option<&str>, name: &str, password: &str, role: Role) -> Result<()> {
        let deny = |why: &str| -> Error {
            let _ = self.audit(actor.tenant_id, &actor.user, Some(actor.role), "create_user", "denied", why);
            Error::Denied(why.to_string())
        };
        let tenant_id = match (actor.role, tenant) {
            (Role::PlatformAdmin, None) => {
                if role != Role::PlatformAdmin {
                    return Err(Error::Invalid("tenant users need a tenant".into()));
                }
                None
            }
            (Role::PlatformAdmin, Some(t)) => {
                if role == Role::PlatformAdmin {
                    return Err(Error::Invalid("platform admins do not belong to a tenant".into()));
                }
                Some(self.tenant(t)?.id)
            }
            (Role::TenantAdmin, t) => {
                if t.is_some() && t != actor.tenant.as_deref() {
                    return Err(deny("tenant admins can only create users in their own tenant"));
                }
                if role == Role::PlatformAdmin {
                    return Err(deny("tenant admins cannot create platform admins"));
                }
                actor.tenant_id
            }
            _ => return Err(deny("creating users requires tenant_admin or platform_admin")),
        };
        if !valid_user_name(name) {
            return Err(Error::Invalid("user names: 1-64 of letters, digits, _ . @ -".into()));
        }
        check_password(name, password)?;
        let hash = hash_password(password)?;
        let r = self.conn().execute(format!("INSERT INTO {}.users (tenant_id, name, pw_hash, role) VALUES ($1,$2,$3,$4)", self.s).as_str(), &[&tenant_id, &name, &hash, &role.as_str()]);
        match r {
            Ok(_) => {}
            Err(e) if e.code() == Some(&postgres::error::SqlState::UNIQUE_VIOLATION) => return Err(Error::Invalid(format!("user {name:?} already exists"))),
            Err(e) => return Err(e.into()),
        }
        self.audit(tenant_id, &actor.user, Some(actor.role), "create_user", "ok", &format!("user={name} role={}", role.as_str()))
    }

    fn target_user(&self, actor: &Principal, tenant: Option<&str>, name: &str, action: &str) -> Result<(i64, Option<i64>)> {
        let tenant_id = match actor.role {
            Role::PlatformAdmin => match tenant {
                Some(t) => Some(self.tenant(t)?.id),
                None => None,
            },
            Role::TenantAdmin => {
                if tenant.is_some() && tenant != actor.tenant.as_deref() {
                    let _ = self.audit(actor.tenant_id, &actor.user, Some(actor.role), action, "denied", "other tenant");
                    return Err(Error::Denied("you can only manage users of your own tenant".into()));
                }
                actor.tenant_id
            }
            _ => return Err(Error::Denied(format!("{action} requires tenant_admin or platform_admin"))),
        };
        let r = self.conn().query_opt(format!("SELECT id FROM {}.users WHERE COALESCE(tenant_id, 0) = COALESCE($1::bigint, 0) AND name = $2", self.s).as_str(), &[&tenant_id, &name])?;
        match r {
            Some(r) => Ok((r.get(0), tenant_id)),
            None => Err(Error::Invalid(format!("no such user {name:?}"))),
        }
    }

    pub fn set_user_disabled(&self, actor: &Principal, tenant: Option<&str>, name: &str, disabled: bool) -> Result<()> {
        let (uid, tid) = self.target_user(actor, tenant, name, "set_user_disabled")?;
        let mut c = self.conn();
        c.execute(format!("UPDATE {}.users SET disabled = $2 WHERE id = $1", self.s).as_str(), &[&uid, &disabled])?;
        if disabled {
            c.execute(format!("UPDATE {}.tokens SET revoked = true WHERE user_id = $1", self.s).as_str(), &[&uid])?;
        }
        self.audit_with(&mut c, tid, &actor.user, Some(actor.role.as_str()), "set_user_disabled", "ok", &format!("user={name} disabled={disabled}"))
    }

    /// Administrative reset: new password, all tokens revoked, lockout cleared.
    pub fn reset_password(&self, actor: &Principal, tenant: Option<&str>, name: &str, new_password: &str) -> Result<()> {
        let (uid, tid) = self.target_user(actor, tenant, name, "reset_password")?;
        check_password(name, new_password)?;
        let hash = hash_password(new_password)?;
        let mut c = self.conn();
        c.execute(format!("UPDATE {}.users SET pw_hash = $2, failed_attempts = 0, locked_until = NULL WHERE id = $1", self.s).as_str(), &[&uid, &hash])?;
        c.execute(format!("UPDATE {}.tokens SET revoked = true WHERE user_id = $1", self.s).as_str(), &[&uid])?;
        self.audit_with(&mut c, tid, &actor.user, Some(actor.role.as_str()), "reset_password", "ok", &format!("user={name}"))
    }

    /// Self-service: needs the current password; revokes every other token of the user.
    pub fn change_password(&self, p: &Principal, old: &str, new: &str) -> Result<()> {
        let hash: String = self.conn().query_one(format!("SELECT pw_hash FROM {}.users WHERE id = $1", self.s).as_str(), &[&p.user_id])?.get(0);
        if !verify_password(old, &hash) {
            let _ = self.audit(p.tenant_id, &p.user, Some(p.role), "change_password", "denied", "wrong current password");
            return Err(generic_auth_error());
        }
        check_password(&p.user, new)?;
        let nh = hash_password(new)?;
        let mut c = self.conn();
        c.execute(format!("UPDATE {}.users SET pw_hash = $2 WHERE id = $1", self.s).as_str(), &[&p.user_id, &nh])?;
        c.execute(format!("UPDATE {}.tokens SET revoked = true WHERE user_id = $1", self.s).as_str(), &[&p.user_id])?;
        self.audit_with(&mut c, p.tenant_id, &p.user, Some(p.role.as_str()), "change_password", "ok", "")
    }

    // ---------------------------------------------------------- login/token

    /// Exchange credentials for a token. Every failure looks the same to the caller (wrong
    /// password, unknown user or tenant, disabled or locked account) and takes about as long.
    pub fn login(&self, tenant: Option<&str>, user: &str, password: &str) -> Result<IssuedToken> {
        if password.chars().count() > 256 || user.len() > 64 || tenant.is_some_and(|t| t.len() > 32) {
            return Err(generic_auth_error());
        }
        let row = self.conn().query_opt(
            format!(
                "SELECT u.id, u.pw_hash, u.disabled OR COALESCE(t.disabled, false), COALESCE(u.locked_until > now(), false), u.role, u.tenant_id
                 FROM {s}.users u LEFT JOIN {s}.tenants t ON t.id = u.tenant_id
                 WHERE u.name = $1 AND ((u.tenant_id IS NULL AND $2::text IS NULL) OR t.name = $2)",
                s = self.s
            )
            .as_str(),
            &[&user, &tenant],
        )?;
        let audit_name = format!("{}{}", tenant.map(|t| format!("{t}/")).unwrap_or_default(), user);
        let Some(row) = row else {
            let _ = verify_password(password, &self.dummy_hash); // equalise timing
            let _ = self.audit(None, &audit_name, None, "login", "denied", "unknown user or tenant");
            return Err(generic_auth_error());
        };
        let (uid, hash, off, locked, role, tid): (i64, String, bool, bool, String, Option<i64>) = (row.get(0), row.get(1), row.get(2), row.get(3), row.get(4), row.get(5));
        let ok = verify_password(password, &hash);
        if off || locked || !ok {
            let why = if off {
                "disabled"
            } else if locked {
                "locked"
            } else {
                "wrong password"
            };
            let mut c = self.conn();
            if !ok && !off && !locked {
                c.execute(
                    format!("UPDATE {}.users SET failed_attempts = failed_attempts + 1, locked_until = CASE WHEN failed_attempts + 1 >= $2 THEN now() + $3 * interval '1 second' ELSE locked_until END WHERE id = $1", self.s).as_str(),
                    &[&uid, &self.opts.max_failed_logins, &(self.opts.lockout_secs as f64)],
                )?;
            }
            let _ = self.audit_with(&mut c, tid, &audit_name, Some(&role), "login", "denied", why);
            return Err(generic_auth_error());
        }
        let token = format!("ngt_{}", b64url(&random_bytes::<32>()?));
        let ttl = self.opts.token_ttl_secs as f64;
        let mut c = self.conn();
        c.execute(format!("UPDATE {}.users SET failed_attempts = 0, locked_until = NULL, last_login = now() WHERE id = $1", self.s).as_str(), &[&uid])?;
        c.execute(format!("INSERT INTO {}.tokens (user_id, token_hash, expires_at) VALUES ($1, $2, now() + $3 * interval '1 second')", self.s).as_str(), &[&uid, &token_hash(&token), &ttl])?;
        self.audit_with(&mut c, tid, &audit_name, Some(&role), "login", "ok", "")?;
        Ok(IssuedToken { token, expires_in_secs: self.opts.token_ttl_secs })
    }

    /// Validate a token. Unknown, expired, revoked tokens and disabled users/tenants are all `Auth`.
    pub fn authenticate(&self, token: &str) -> Result<Principal> {
        if token.len() > 100 || !token.starts_with("ngt_") {
            return Err(generic_auth_error());
        }
        let r = self.conn().query_opt(
            format!(
                "SELECT u.id, u.name, u.tenant_id, t.name, u.role
                 FROM {s}.tokens k JOIN {s}.users u ON u.id = k.user_id LEFT JOIN {s}.tenants t ON t.id = u.tenant_id
                 WHERE k.token_hash = $1 AND NOT k.revoked AND k.expires_at > now() AND NOT u.disabled AND NOT COALESCE(t.disabled, false)",
                s = self.s
            )
            .as_str(),
            &[&token_hash(token)],
        )?;
        match r {
            Some(r) => Ok(Principal { user_id: r.get(0), user: r.get(1), tenant_id: r.get(2), tenant: r.get(3), role: Role::parse(r.get::<_, &str>(4))? }),
            None => Err(generic_auth_error()),
        }
    }

    pub fn logout(&self, token: &str) -> Result<()> {
        self.conn().execute(format!("UPDATE {}.tokens SET revoked = true WHERE token_hash = $1", self.s).as_str(), &[&token_hash(token)])?;
        Ok(())
    }

    /// Raw access for tests and operators (read-only use intended).
    #[doc(hidden)]
    pub fn raw_query(&self, sql: &str) -> Result<Vec<postgres::Row>> {
        Ok(self.conn().query(sql, &[])?)
    }

    #[doc(hidden)]
    pub fn schema_name(&self) -> &str {
        &self.opts.schema
    }

    #[doc(hidden)]
    pub fn hex_token_hash(token: &str) -> String {
        hex(&token_hash(token))
    }
}

impl ControlPlane {
    fn tenant_row(&self, r: postgres::Row) -> TenantInfo {
        let id: i64 = r.get(0);
        TenantInfo {
            id,
            name: r.get(1),
            disabled: r.get(2),
            quotas: Quotas { max_entities: r.get::<_, Option<i64>>(3).map(|x| x as u64), max_result_rows: r.get::<_, Option<i64>>(4).map(|x| x as u64) },
            schema: format!("{}_t{}", self.opts.schema, id),
        }
    }
}
