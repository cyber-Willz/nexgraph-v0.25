//! Authentication, authorization and multi-tenant isolation.
//!
//! * [`ControlPlane`] — users, Argon2id password hashes, hashed session tokens, lockout and a
//!   hash-chained, append-only audit log, all in a PostgreSQL schema (default `nexgraph_admin`).
//! * [`TenantManager`] — one **separate nexgraph database (PostgreSQL schema) per tenant**; a
//!   [`Session`] can only ever reach the database of the tenant its token belongs to.
//! * Roles: `reader` < `writer` < `tenant_admin` (per tenant) and `platform_admin`, who manages
//!   tenants and users but has **no access to tenant graph data**.
//!
//! See `SECURITY.md` for the threat model, guarantees and limits.

mod control;
mod manager;
pub mod policy;

pub use control::{AuditEntry, ControlOptions, ControlPlane, IssuedToken, TenantInfo, UserInfo};
pub use manager::{ManagerOptions, Session, TenantManager};
pub use policy::{Mask, Policy, Rule};
pub(crate) use crate::crypto::hmac_sha256;

use crate::error::{Error, Result};
use crate::query::parser::{self, Clause};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Reader,
    Writer,
    TenantAdmin,
    PlatformAdmin,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Reader => "reader",
            Role::Writer => "writer",
            Role::TenantAdmin => "tenant_admin",
            Role::PlatformAdmin => "platform_admin",
        }
    }
    pub fn parse(s: &str) -> Result<Role> {
        Ok(match s {
            "reader" => Role::Reader,
            "writer" => Role::Writer,
            "tenant_admin" => Role::TenantAdmin,
            "platform_admin" => Role::PlatformAdmin,
            _ => return Err(Error::Invalid(format!("unknown role {s:?} (reader, writer, tenant_admin, platform_admin)"))),
        })
    }
}

/// An authenticated caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Principal {
    pub user_id: i64,
    pub user: String,
    pub tenant_id: Option<i64>,
    pub tenant: Option<String>,
    pub role: Role,
}

/// Per-tenant resource limits (`None` = unlimited).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Quotas {
    /// Soft limit on live nodes + edges: writes are refused once reached (a single statement
    /// can overshoot it).
    pub max_entities: Option<u64>,
    /// Queries returning more rows are refused (not truncated).
    pub max_result_rows: Option<u64>,
}

/// What a statement needs, ordered by privilege.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Need {
    Read,
    Write,
    Schema,
}

/// Procedures a tenant may call. Everything else is refused (fail closed).
fn procedure_allowed(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == "db.stats" || ["algo.", "spectral.", "physics.", "vector.", "stats.", "cyber.", "killchain.", "alm.", "land.", "uw.", "fin.", "gaap.", "swarm."].iter().any(|p| n.starts_with(p))
}

/// Parse `text` and decide the privilege it needs, and whether it can add entities (for quotas).
/// The same text is then executed by the engine, whose parser is deterministic, so
/// classification and execution cannot disagree.
pub(crate) fn classify_full(text: &str) -> Result<(Need, bool)> {
    let st = parser::parse(text)?;
    let mut need = Need::Read;
    let mut creates = false;
    for c in &st.clauses {
        match c {
            Clause::Match { .. } | Clause::Return(_) | Clause::With { .. } | Clause::Unwind { .. } => {}
            Clause::Merge { .. } => {
                need = need.max(Need::Write);
                creates = true;
            }
            Clause::Call { name, .. } => {
                if !procedure_allowed(name) {
                    return Err(Error::Denied(format!("procedure {name} is not available")));
                }
            }
            Clause::Create(_) => {
                need = need.max(Need::Write);
                creates = true;
            }
            Clause::Set(_) | Clause::Delete(_) => need = need.max(Need::Write),
            Clause::CreatePropIndex { .. } | Clause::CreateVectorIndex { .. } => need = need.max(Need::Schema),
        }
    }
    Ok((need, creates))
}

#[cfg(test)]
pub(crate) fn classify(text: &str) -> Result<Need> {
    classify_full(text).map(|x| x.0)
}

pub(crate) fn role_allows(role: Role, need: Need) -> bool {
    match role {
        Role::Reader => need == Need::Read,
        Role::Writer => need <= Need::Write,
        Role::TenantAdmin => true,
        Role::PlatformAdmin => false, // platform admins never touch tenant data
    }
}

pub(crate) fn valid_tenant_name(s: &str) -> bool {
    let b = s.as_bytes();
    (2..=32).contains(&b.len()) && b[0].is_ascii_lowercase() && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

pub(crate) fn valid_user_name(s: &str) -> bool {
    let b = s.as_bytes();
    (1..=64).contains(&b.len()) && b[0].is_ascii_alphanumeric() && b.iter().all(|c| c.is_ascii_alphanumeric() || matches!(*c, b'_' | b'.' | b'@' | b'-'))
}

/// Minimum length 12, at most 256 characters (bounds hashing cost), not the user name, not one repeated character.
pub(crate) fn check_password(user: &str, pw: &str) -> Result<()> {
    let n = pw.chars().count();
    if n < 12 {
        return Err(Error::Invalid("password must be at least 12 characters".into()));
    }
    if n > 256 {
        return Err(Error::Invalid("password must be at most 256 characters".into()));
    }
    let first = pw.chars().next().unwrap();
    if pw.chars().all(|c| c == first) {
        return Err(Error::Invalid("password is too repetitive".into()));
    }
    if pw.to_lowercase().contains(&user.to_lowercase()) {
        return Err(Error::Invalid("password must not contain the user name".into()));
    }
    Ok(())
}

pub(crate) fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// URL-safe base64 without padding.
pub(crate) fn b64url(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut o = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        o.push(T[(n >> 18) as usize & 63] as char);
        o.push(T[(n >> 12) as usize & 63] as char);
        if c.len() > 1 {
            o.push(T[(n >> 6) as usize & 63] as char);
        }
        if c.len() > 2 {
            o.push(T[n as usize & 63] as char);
        }
    }
    o
}

pub(crate) fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).map_err(|e| Error::Backend(format!("no secure randomness available: {e}")))?;
    Ok(b)
}

#[cfg(test)]
mod tests {
    #[test]
    fn merge_needs_write_and_counts_as_creating() {
        assert!(matches!(classify("MERGE (a:X {k: 1})").unwrap(), Need::Write));
        assert!(classify_full("MERGE (a:X {k: 1})").unwrap().1);
        assert!(matches!(classify("UNWIND [1] AS x WITH x RETURN x").unwrap(), Need::Read));
        assert!(matches!(classify("MATCH (a:X) REMOVE a.k").unwrap(), Need::Write));
    }

    use super::*;

    #[test]
    fn base64url_matches_known_vectors() {
        assert_eq!(b64url(b""), "");
        assert_eq!(b64url(b"f"), "Zg");
        assert_eq!(b64url(b"fo"), "Zm8");
        assert_eq!(b64url(b"foo"), "Zm9v");
        assert_eq!(b64url(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn classification_is_fail_closed() {
        assert_eq!(classify("MATCH (n) RETURN n").unwrap(), Need::Read);
        assert_eq!(classify("MATCH (n) SET n.x = 1").unwrap(), Need::Write);
        assert_eq!(classify("MATCH (n) DELETE n").unwrap(), Need::Write);
        assert_eq!(classify("CREATE (:A)").unwrap(), Need::Write);
        assert_eq!(classify("CREATE INDEX ON :A(x)").unwrap(), Need::Schema);
        assert_eq!(classify("CALL algo.pagerank() YIELD node, score RETURN node").unwrap(), Need::Read);
        assert_eq!(classify("CALL cyber.beaconing('FLOW', 'ts') YIELD src RETURN src").unwrap(), Need::Read);
        assert_eq!(classify("CALL stats.describe('M', 'v') YIELD count RETURN count").unwrap(), Need::Read);
        assert!(matches!(classify("CALL cyber.ingest() YIELD x RETURN x").map(|_| ()), Ok(())), "prefix is allowed; the engine rejects unknown names");
        assert!(matches!(classify("CALL db.shutdown() YIELD x RETURN x"), Err(Error::Denied(_))));
        // a read clause hidden before a write cannot lower the requirement
        assert_eq!(classify("MATCH (a) CREATE (b:X) RETURN a").unwrap(), Need::Write);
        assert!(classify("not a query").is_err());
    }

    #[test]
    fn role_matrix() {
        use Need::*;
        for (r, rd, wr, sc) in [
            (Role::Reader, true, false, false),
            (Role::Writer, true, true, false),
            (Role::TenantAdmin, true, true, true),
            (Role::PlatformAdmin, false, false, false),
        ] {
            assert_eq!((role_allows(r, Read), role_allows(r, Write), role_allows(r, Schema)), (rd, wr, sc), "{r:?}");
        }
    }

    #[test]
    fn name_and_password_rules() {
        assert!(valid_tenant_name("acme_corp1") && !valid_tenant_name("Acme") && !valid_tenant_name("a") && !valid_tenant_name("a-b") && !valid_tenant_name("x; drop"));
        assert!(valid_user_name("ada.l@x-y_1") && !valid_user_name("") && !valid_user_name("a b") && !valid_user_name("'; --"));
        assert!(check_password("ada", "short").is_err());
        assert!(check_password("ada", "aaaaaaaaaaaaaaaa").is_err());
        assert!(check_password("ada", "my-ADA-password-1").is_err());
        assert!(check_password("ada", "correct horse battery").is_ok());
    }
}
