//! Authentication, authorization and tenant-isolation tests (PostgreSQL required).
//!   NEXGRAPH_TEST_PG_URL        plaintext server (control plane runs with require_tls = false here)
//!   NEXGRAPH_TEST_PG_TLS_PORT   TLS-only server, for the TLS test
#![cfg(feature = "security")]

use nexgraph::query::Params;
use nexgraph::security::{ControlOptions, ControlPlane, ManagerOptions, Principal, Quotas, Role, TenantManager};
use nexgraph::{Error, PgOptions, Value};
use postgres::{Client, NoTls};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const PW: &str = "correct horse battery staple";

macro_rules! need_pg {
    () => {
        match std::env::var("NEXGRAPH_TEST_PG_URL") {
            Ok(u) => u,
            Err(_) => {
                eprintln!("NEXGRAPH_TEST_PG_URL not set: skipping");
                return;
            }
        }
    };
}

struct Env {
    url: String,
    cs: String,
    cp: Arc<ControlPlane>,
    mgr: TenantManager,
    admin: Principal,
}

impl Env {
    fn new(url: &str, name: &str) -> Env {
        Self::with(url, name, |_| {}, |_| {})
    }
    fn with(url: &str, name: &str, c: impl FnOnce(&mut ControlOptions), m: impl FnOnce(&mut ManagerOptions)) -> Env {
        let cs = format!("nxs_{name}_{}", std::process::id());
        cleanup(url, &cs);
        let mut co = ControlOptions { schema: cs.clone(), require_tls: false, ..Default::default() };
        c(&mut co);
        let cp = Arc::new(ControlPlane::open(url, co).unwrap());
        cp.bootstrap_platform_admin("root", PW).unwrap();
        let mut mo = ManagerOptions { pg: PgOptions::default().require_tls(false), ..Default::default() };
        m(&mut mo);
        let mgr = TenantManager::new(cp.clone(), url, mo);
        let t = cp.login(None, "root", PW).unwrap();
        let admin = cp.authenticate(&t.token).unwrap();
        Env { url: url.into(), cs, cp, mgr, admin }
    }
    fn tenant(&self, name: &str) {
        self.cp.create_tenant(&self.admin, name, Quotas::default()).unwrap();
    }
    fn user(&self, tenant: &str, name: &str, role: Role) {
        self.cp.create_user(&self.admin, Some(tenant), name, PW, role).unwrap();
    }
    fn login(&self, tenant: &str, name: &str) -> nexgraph::security::Session {
        let t = self.cp.login(Some(tenant), name, PW).unwrap();
        self.mgr.session(&t.token).unwrap()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        cleanup(&self.url, &self.cs);
    }
}

fn cleanup(url: &str, cs: &str) {
    let Ok(mut c) = Client::connect(url, NoTls) else { return };
    if let Ok(rows) = c.query(&format!("SELECT schema_name FROM information_schema.schemata WHERE schema_name LIKE '{cs}\\_t%'"), &[]) {
        for r in rows {
            let s: String = r.get(0);
            let _ = c.batch_execute(&format!("DROP SCHEMA IF EXISTS \"{s}\" CASCADE"));
        }
    }
    let _ = c.batch_execute(&format!("DROP SCHEMA IF EXISTS \"{cs}\" CASCADE"));
}

fn q(s: &nexgraph::security::Session, text: &str) -> Result<nexgraph::query::QueryResult, Error> {
    s.query(text, &Params::new())
}

fn count(s: &nexgraph::security::Session, label: &str) -> i64 {
    match q(s, &format!("MATCH (n:{label}) RETURN count(*)")).unwrap().rows[0][0] {
        Value::Int(i) => i,
        ref v => panic!("{v:?}"),
    }
}

#[test]
fn login_tokens_and_password_storage() {
    let url = need_pg!();
    let e = Env::new(&url, "login");
    assert!(matches!(e.cp.bootstrap_platform_admin("root2", PW), Err(Error::Denied(_))), "only one bootstrap");
    e.tenant("acme");
    assert!(e.cp.create_user(&e.admin, Some("acme"), "ada", "short", Role::Writer).is_err(), "weak password");
    e.user("acme", "ada", Role::Writer);
    assert!(matches!(e.cp.create_user(&e.admin, Some("acme"), "ada", PW, Role::Writer), Err(Error::Invalid(m)) if m.contains("already exists")));
    // Argon2id, not a bare hash
    let h: String = e.cp.raw_query(&format!("SELECT pw_hash FROM \"{}\".users WHERE name = 'ada'", e.cs)).unwrap()[0].get(0);
    assert!(h.starts_with("$argon2id$"), "{h}");
    assert!(!h.contains(PW));
    // all failure modes look identical
    for (t, u, p) in [(Some("acme"), "ada", "wrong password here"), (Some("acme"), "nobody", PW), (Some("nope"), "ada", PW), (None, "ada", PW), (Some("acme"), "ada", "")] {
        match e.cp.login(t, u, p) {
            Err(Error::Auth(m)) => assert_eq!(m, "invalid credentials"),
            other => panic!("{t:?}/{u}: {other:?}"),
        }
    }
    let t = e.cp.login(Some("acme"), "ada", PW).unwrap();
    assert!(t.token.starts_with("ngt_") && t.token.len() > 40);
    // only the SHA-256 of the token is stored
    let rows = e.cp.raw_query(&format!("SELECT encode(token_hash, 'hex') FROM \"{}\".tokens", e.cs)).unwrap();
    let stored: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
    assert!(stored.contains(&ControlPlane::hex_token_hash(&t.token)));
    assert!(stored.iter().all(|s| !s.contains(&t.token)));
    let p = e.cp.authenticate(&t.token).unwrap();
    assert_eq!((p.user.as_str(), p.tenant.as_deref(), p.role), ("ada", Some("acme"), Role::Writer));
    for bad in ["", "ngt_", "ngt_AAAA", &format!("{}x", t.token), &t.token.replace("ngt_", "xxx_"), &"ngt_".repeat(40)] {
        assert!(matches!(e.cp.authenticate(bad), Err(Error::Auth(_))), "{bad:?}");
    }
    e.cp.logout(&t.token).unwrap();
    assert!(matches!(e.cp.authenticate(&t.token), Err(Error::Auth(_))), "logout revokes");
    // the same user and tenant name can exist in two tenants with different passwords
    e.tenant("beta");
    e.cp.create_user(&e.admin, Some("beta"), "ada", "another long passphrase", Role::Reader).unwrap();
    assert!(e.cp.login(Some("beta"), "ada", PW).is_err());
    assert!(e.cp.login(Some("beta"), "ada", "another long passphrase").is_ok());
}

#[test]
fn expiry_lockout_and_password_changes() {
    let url = need_pg!();
    let e = Env::with(&url, "lock", |c| { c.token_ttl_secs = 1; c.max_failed_logins = 3; }, |_| {});
    e.tenant("acme");
    e.user("acme", "ada", Role::Writer);
    let t = e.cp.login(Some("acme"), "ada", PW).unwrap();
    assert!(e.cp.authenticate(&t.token).is_ok());
    std::thread::sleep(Duration::from_millis(2200));
    assert!(matches!(e.cp.authenticate(&t.token), Err(Error::Auth(_))), "expired token");
    // three wrong passwords lock the account; the right one no longer works
    for _ in 0..3 {
        assert!(e.cp.login(Some("acme"), "ada", "wrong wrong wrong").is_err());
    }
    assert!(matches!(e.cp.login(Some("acme"), "ada", PW), Err(Error::Auth(_))), "locked");
    // an admin reset clears it and revokes tokens
    e.cp.reset_password(&e.admin, Some("acme"), "ada", "brand new passphrase").unwrap();
    assert!(e.cp.login(Some("acme"), "ada", PW).is_err());
    let t2 = e.cp.login(Some("acme"), "ada", "brand new passphrase").unwrap();
    let p = e.cp.authenticate(&t2.token).unwrap();
    // self-service change needs the old password and revokes sessions
    assert!(e.cp.change_password(&p, "not the old one", "yet another passphrase").is_err());
    e.cp.change_password(&p, "brand new passphrase", "yet another passphrase").unwrap();
    assert!(e.cp.authenticate(&t2.token).is_err());
    assert!(e.cp.login(Some("acme"), "ada", "yet another passphrase").is_ok());
    // a successful login resets the failure counter
    assert!(e.cp.login(Some("acme"), "ada", "x wrong x wrong").is_err());
    assert!(e.cp.login(Some("acme"), "ada", "yet another passphrase").is_ok());
    assert!(e.cp.login(Some("acme"), "ada", "x wrong x wrong").is_err());
    assert!(e.cp.login(Some("acme"), "ada", "yet another passphrase").is_ok(), "counter was reset, so two failures do not lock");
}

#[test]
fn tenants_cannot_see_or_touch_each_other() {
    let url = need_pg!();
    let e = Env::new(&url, "iso");
    e.tenant("acme");
    e.tenant("beta");
    e.user("acme", "ann", Role::TenantAdmin);
    e.user("beta", "bob", Role::TenantAdmin);
    let (a, b) = (e.login("acme", "ann"), e.login("beta", "bob"));
    q(&a, "CREATE (:Secret {owner:'acme', v:1}), (:Secret {owner:'acme', v:2})").unwrap();
    q(&b, "CREATE (:Secret {owner:'beta', v:9})").unwrap();
    assert_eq!((count(&a, "Secret"), count(&b, "Secret")), (2, 1));
    let r = q(&b, "MATCH (n) RETURN n.owner").unwrap();
    assert!(r.rows.iter().all(|r| r[0] == Value::Str("beta".into())), "{:?}", r.rows);
    // whole-graph procedures see only the caller's graph
    let r = q(&a, "CALL algo.pagerank() YIELD node, score RETURN count(*)").unwrap();
    assert_eq!(r.rows[0][0], Value::Int(2));
    // writes in one tenant leave the other untouched
    q(&a, "MATCH (n:Secret) DELETE n").unwrap();
    assert_eq!((count(&a, "Secret"), count(&b, "Secret")), (0, 1));
    // separate storage: one PostgreSQL schema per tenant
    let mut c = Client::connect(&url, NoTls).unwrap();
    let n = |c: &mut Client, s: &str| -> i64 { c.query_one(&format!("SELECT count(*) FROM \"{s}\".nodes"), &[]).unwrap().get(0) };
    let (sa, sb) = (e.cp.tenant("acme").unwrap().schema, e.cp.tenant("beta").unwrap().schema);
    assert_ne!(sa, sb);
    assert_eq!((n(&mut c, &sa), n(&mut c, &sb)), (0, 1));
    // a token is bound to its owner's tenant: there is nothing to forge or redirect
    assert_eq!(a.principal().tenant.as_deref(), Some("acme"));
    // data survives a manager restart (new manager, same control plane)
    drop((a, b));
    let mgr2 = TenantManager::new(e.cp.clone(), &url, ManagerOptions { pg: PgOptions::default().require_tls(false), ..Default::default() });
    e.mgr.close_all();
    let t = e.cp.login(Some("beta"), "bob", PW).unwrap();
    let b2 = mgr2.session(&t.token).unwrap();
    assert_eq!(count(&b2, "Secret"), 1);
}

#[test]
fn roles_are_enforced_and_cannot_be_dodged() {
    let url = need_pg!();
    let e = Env::new(&url, "roles");
    e.tenant("acme");
    for (u, r) in [("rita", Role::Reader), ("walt", Role::Writer), ("ann", Role::TenantAdmin)] {
        e.user("acme", u, r);
    }
    let (rd, wr, ad) = (e.login("acme", "rita"), e.login("acme", "walt"), e.login("acme", "ann"));
    q(&wr, "CREATE (:T {x:1})").unwrap();
    // reader: reads only
    assert_eq!(count(&rd, "T"), 1);
    for stmt in ["CREATE (:T {x:2})", "MATCH (n:T) SET n.x = 5", "MATCH (n:T) DELETE n", "MATCH (n:T) CREATE (m:T) RETURN n", "CREATE INDEX ON :T(x)"] {
        assert!(matches!(q(&rd, stmt), Err(Error::Denied(_))), "reader must not run {stmt}");
    }
    assert!(matches!(rd.checkpoint(), Err(Error::Denied(_))));
    // writer: data yes, schema and admin no
    q(&wr, "MATCH (n:T) SET n.x = 7").unwrap();
    assert!(matches!(q(&wr, "CREATE INDEX ON :T(x)"), Err(Error::Denied(_))));
    assert!(matches!(wr.checkpoint(), Err(Error::Denied(_))));
    assert!(matches!(wr.grant_sql_reader("somebody"), Err(Error::Denied(_))));
    // admin: everything
    q(&ad, "CREATE INDEX ON :T(x)").unwrap();
    ad.checkpoint().unwrap();
    // unknown procedures are refused for everyone
    assert!(matches!(q(&ad, "CALL admin.shutdown() YIELD a RETURN a"), Err(Error::Denied(_))));
    // oversized statements are refused before parsing
    assert!(matches!(q(&ad, &format!("MATCH (n) WHERE n.x = '{}' RETURN n", "a".repeat(70_000))), Err(Error::Denied(_))));
    // the platform administrator has no way into tenant data
    let t = e.cp.login(None, "root", PW).unwrap();
    let root = e.mgr.session(&t.token).unwrap();
    assert!(matches!(q(&root, "MATCH (n) RETURN n"), Err(Error::Denied(_))));
    assert!(matches!(root.stats(), Err(Error::Denied(_))));
    // user administration: tenant admins stay inside their tenant and below platform level
    e.tenant("beta");
    let ann = ad.principal().clone();
    e.cp.create_user(&ann, None, "newbie", PW, Role::Reader).unwrap();
    assert!(matches!(e.cp.create_user(&ann, Some("beta"), "spy", PW, Role::Reader), Err(Error::Denied(_))));
    assert!(matches!(e.cp.create_user(&ann, None, "boss", PW, Role::PlatformAdmin), Err(Error::Denied(_))));
    assert!(matches!(e.cp.create_tenant(&ann, "gamma", Quotas::default()), Err(Error::Denied(_))));
    let walt = wr.principal().clone();
    assert!(matches!(e.cp.create_user(&walt, None, "x1", PW, Role::Reader), Err(Error::Denied(_))));
    assert!(matches!(e.cp.reset_password(&walt, None, "rita", "some new passphrase"), Err(Error::Denied(_))));
    assert!(matches!(e.cp.reset_password(&ann, Some("beta"), "x", "some new passphrase"), Err(Error::Denied(_))));
}

#[test]
fn disabling_users_and_tenants_cuts_access_immediately() {
    let url = need_pg!();
    let e = Env::new(&url, "disable");
    e.tenant("acme");
    e.user("acme", "ann", Role::TenantAdmin);
    e.user("acme", "walt", Role::Writer);
    let t = e.cp.login(Some("acme"), "walt", PW).unwrap();
    let s = e.mgr.session(&t.token).unwrap();
    q(&s, "CREATE (:T)").unwrap();
    let ann = e.login("acme", "ann");
    e.cp.set_user_disabled(ann.principal(), None, "walt", true).unwrap();
    assert!(matches!(e.mgr.session(&t.token), Err(Error::Auth(_))), "existing token dies with the account");
    assert!(e.cp.login(Some("acme"), "walt", PW).is_err());
    e.cp.set_user_disabled(ann.principal(), None, "walt", false).unwrap();
    assert!(e.cp.login(Some("acme"), "walt", PW).is_ok());
    // disabling a tenant locks everyone out, including its admin
    let tok = e.cp.login(Some("acme"), "ann", PW).unwrap();
    e.cp.set_tenant_disabled(&e.admin, "acme", true).unwrap();
    assert!(matches!(e.mgr.session(&tok.token), Err(Error::Auth(_))));
    assert!(e.cp.login(Some("acme"), "ann", PW).is_err());
}

#[test]
fn quotas_limit_growth_and_result_size() {
    let url = need_pg!();
    let e = Env::new(&url, "quota");
    e.cp.create_tenant(&e.admin, "small", Quotas { max_entities: Some(3), max_result_rows: Some(2) }).unwrap();
    e.user("small", "walt", Role::Writer);
    let s = e.login("small", "walt");
    q(&s, "CREATE (:T {i:1})").unwrap();
    q(&s, "CREATE (:T {i:2}), (:T {i:3})").unwrap(); // reaches the (soft) limit
    assert!(matches!(q(&s, "CREATE (:T {i:4})"), Err(Error::Denied(m)) if m.contains("quota")));
    assert_eq!(s.stats().unwrap().live_nodes, 3);
    assert!(q(&s, "MATCH (n:T) RETURN n.i LIMIT 2").is_ok());
    assert!(matches!(q(&s, "MATCH (n:T) RETURN n.i"), Err(Error::Denied(m)) if m.contains("rows")));
    // deleting makes room again
    q(&s, "MATCH (n:T {i:1}) DELETE n").unwrap();
    q(&s, "CREATE (:T {i:5})").unwrap();
}

#[test]
fn audit_log_is_complete_scoped_and_tamper_evident() {
    let url = need_pg!();
    let e = Env::new(&url, "audit");
    e.tenant("acme");
    e.tenant("beta");
    e.user("acme", "ann", Role::TenantAdmin);
    e.user("acme", "rita", Role::Reader);
    e.user("beta", "bob", Role::TenantAdmin);
    assert!(e.cp.login(Some("acme"), "ann", "bad bad bad bad").is_err());
    let (ann, rita, bob) = (e.login("acme", "ann"), e.login("acme", "rita"), e.login("beta", "bob"));
    q(&ann, "CREATE (:T {secret:'hunter2-hunter2'})").unwrap();
    q(&rita, "MATCH (n:T) RETURN n").unwrap();
    assert!(q(&rita, "CREATE (:T)").is_err());
    q(&bob, "CREATE (:U)").unwrap();
    let log = e.cp.read_audit(ann.principal(), None, 100).unwrap();
    let has = |act: &str, out: &str, who: &str| log.iter().any(|x| x.action == act && x.outcome == out && x.actor.ends_with(who));
    assert!(has("login", "denied", "ann") && has("login", "ok", "ann") && has("query", "ok", "ann") && has("query", "denied", "rita"));
    assert!(log.iter().all(|x| x.tenant_id == ann.principal().tenant_id), "a tenant admin sees only its own tenant");
    assert!(log.iter().all(|x| !x.detail.contains("hunter2")), "statement text and data are never written to the log");
    assert!(matches!(e.cp.read_audit(rita.principal(), None, 10), Err(Error::Denied(_))), "readers cannot read the audit log");
    assert!(matches!(e.cp.read_audit(ann.principal(), Some("beta"), 10), Err(Error::Denied(_))));
    assert!(e.cp.read_audit(&e.admin, None, 1000).unwrap().len() > log.len(), "the platform admin sees everything");
    // the chain verifies, and the table refuses edits
    let n = e.cp.verify_audit_chain().unwrap();
    assert!(n >= 10);
    let mut c = Client::connect(&url, NoTls).unwrap();
    let t = format!("\"{}\".audit", e.cs);
    assert!(c.batch_execute(&format!("UPDATE {t} SET outcome = 'ok'")).is_err());
    assert!(c.batch_execute(&format!("DELETE FROM {t}")).is_err());
    assert!(c.batch_execute(&format!("TRUNCATE {t}")).is_err());
    // an operator who disables the trigger can edit rows, but the chain then fails verification
    c.batch_execute(&format!("ALTER TABLE {t} DISABLE TRIGGER audit_no_change")).unwrap();
    c.batch_execute(&format!("UPDATE {t} SET outcome = 'ok' WHERE outcome = 'denied' AND action = 'query'")).unwrap();
    assert!(matches!(e.cp.verify_audit_chain(), Err(Error::Corrupt(m)) if m.contains("audit chain broken")));
    // ... and so does deleting an entry from the middle
    c.batch_execute(&format!("UPDATE {t} SET outcome = 'denied' WHERE outcome = 'ok' AND action = 'query' AND actor LIKE '%rita'")).ok();
    c.batch_execute(&format!("DELETE FROM {t} WHERE id = (SELECT id FROM {t} ORDER BY id OFFSET 3 LIMIT 1)")).unwrap();
    assert!(e.cp.verify_audit_chain().is_err());
}

#[test]
fn purging_a_tenant_removes_data_but_keeps_the_audit_trail() {
    let url = need_pg!();
    let e = Env::new(&url, "purge");
    e.tenant("acme");
    e.user("acme", "ann", Role::TenantAdmin);
    let ann = e.login("acme", "ann");
    q(&ann, "CREATE (:T {x:1})").unwrap();
    let tok = e.cp.login(Some("acme"), "ann", PW).unwrap();
    let schema = e.cp.tenant("acme").unwrap().schema;
    let exists = |s: &str| -> bool { Client::connect(&url, NoTls).unwrap().query_one("SELECT EXISTS (SELECT 1 FROM information_schema.schemata WHERE schema_name = $1)", &[&s]).unwrap().get(0) };
    assert!(exists(&schema));
    assert!(matches!(e.mgr.purge_tenant(ann.principal(), "acme"), Err(Error::Denied(_))), "only the platform admin");
    assert!(matches!(e.mgr.purge_tenant(&e.admin, "acme"), Err(Error::Invalid(_))), "must be disabled first");
    e.cp.set_tenant_disabled(&e.admin, "acme", true).unwrap();
    drop(ann);
    e.mgr.purge_tenant(&e.admin, "acme").unwrap();
    assert!(!exists(&schema), "graph schema dropped");
    assert!(e.cp.tenant("acme").is_err());
    assert!(e.mgr.session(&tok.token).is_err());
    let log = e.cp.read_audit(&e.admin, None, 1000).unwrap();
    assert!(log.iter().any(|x| x.action == "purge_tenant" && x.outcome == "ok"));
    assert!(log.iter().any(|x| x.action == "query" && x.actor == "ann"), "history stays");
    e.cp.verify_audit_chain().unwrap();
    // the name can be reused, with a fresh, empty graph
    e.tenant("acme");
    e.user("acme", "ann", Role::TenantAdmin);
    assert_eq!(count(&e.login("acme", "ann"), "T"), 0);
}

#[test]
fn postgres_roles_get_select_on_their_own_tenant_only() {
    let url = need_pg!();
    let e = Env::new(&url, "pgrole");
    e.tenant("acme");
    e.tenant("beta");
    e.user("acme", "ann", Role::TenantAdmin);
    e.user("beta", "bob", Role::TenantAdmin);
    let (a, b) = (e.login("acme", "ann"), e.login("beta", "bob"));
    q(&a, "CREATE (:T {x:1})").unwrap();
    q(&b, "CREATE (:T {x:2})").unwrap();
    let role = format!("nxrole_{}", std::process::id());
    let mut su = Client::connect(&url, NoTls).unwrap();
    su.batch_execute(&format!("DROP ROLE IF EXISTS {role}; CREATE ROLE {role} LOGIN")).unwrap();
    let (sa, sb) = (e.cp.tenant("acme").unwrap().schema, e.cp.tenant("beta").unwrap().schema);
    a.grant_sql_reader(&role).unwrap();
    assert!(b.grant_sql_reader("x; DROP ROLE y").is_err(), "role names are validated");
    let mut u = url.clone();
    u = u.replacen("postgres@", &format!("{role}@"), 1);
    let mut r = Client::connect(&u, NoTls).unwrap();
    let n: i64 = r.query_one(&format!("SELECT count(*) FROM \"{sa}\".nodes"), &[]).unwrap().get(0);
    assert_eq!(n, 1);
    assert!(r.query_one(&format!("SELECT count(*) FROM \"{sb}\".nodes"), &[]).is_err(), "no access to the other tenant");
    assert!(r.batch_execute(&format!("INSERT INTO \"{sa}\".nodes VALUES (99, '{{}}', '{{}}', 1, 1)")).is_err(), "SELECT only");
    assert!(r.query_one(&format!("SELECT count(*) FROM \"{sa}\".commits"), &[]).is_err(), "not the commit log");
    assert!(r.query_one(&format!("SELECT count(*) FROM \"{}\".users", e.cs), &[]).is_err(), "not the control plane");
    drop(r);
    drop((a, b));
    drop(e);
    let _ = su.batch_execute(&format!("DROP ROLE IF EXISTS {role}"));
}

#[test]
fn control_plane_insists_on_tls_by_default() {
    let url = need_pg!();
    // defaults: require_tls = true, so a plaintext-only server is refused
    let r = ControlPlane::open(&url, ControlOptions { schema: format!("nxs_notls_{}", std::process::id()), ..Default::default() });
    match r {
        Err(Error::Backend(m)) => assert!(m.to_lowercase().contains("tls") || m.to_lowercase().contains("ssl"), "{m}"),
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("must not connect without TLS"),
    }
    let Ok(port) = std::env::var("NEXGRAPH_TEST_PG_TLS_PORT") else { return };
    let ca = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ca.pem");
    let tls_url = format!("host=pg.test hostaddr=127.0.0.1 port={port} user=postgres dbname=nexgraph_tls sslmode=require");
    let cs = format!("nxs_tls_{}", std::process::id());
    let mut o = ControlOptions { schema: cs.clone(), ..Default::default() };
    o.tls.ca_file = Some(ca.clone());
    let cp = Arc::new(ControlPlane::open(&tls_url, o).unwrap());
    cp.bootstrap_platform_admin("root", PW).unwrap();
    let t = cp.login(None, "root", PW).unwrap();
    let admin = cp.authenticate(&t.token).unwrap();
    cp.create_tenant(&admin, "acme", Quotas::default()).unwrap();
    cp.create_user(&admin, Some("acme"), "ann", PW, Role::TenantAdmin).unwrap();
    // tenant databases are opened over TLS as well
    let mgr = TenantManager::new(cp.clone(), &tls_url, ManagerOptions { pg: PgOptions::default().tls_ca_file(ca), ..Default::default() });
    let s = mgr.session(&cp.login(Some("acme"), "ann", PW).unwrap().token).unwrap();
    q(&s, "CREATE (:T)").unwrap();
    assert_eq!(count(&s, "T"), 1);
    drop((s, mgr));
    let mut c = Client::connect(&format!("host=127.0.0.1 port={port} user=postgres dbname=nexgraph_tls sslmode=require"), nexgraph::PgTls { danger_skip_cert_verification: true, ..Default::default() }.connector_for_tests()).unwrap();
    for r in c.query(&format!("SELECT schema_name FROM information_schema.schemata WHERE schema_name LIKE '{cs}%'"), &[]).unwrap() {
        let s: String = r.get(0);
        c.batch_execute(&format!("DROP SCHEMA \"{s}\" CASCADE")).unwrap();
    }
}

#[test]
fn a_write_that_cannot_be_audited_is_not_executed() {
    let url = need_pg!();
    let e = Env::new(&url, "failclosed");
    e.tenant("acme");
    e.user("acme", "walt", Role::Writer);
    let s = e.login("acme", "walt");
    q(&s, "CREATE (:T {i:1})").unwrap();
    // make every "allowed" audit entry fail to insert
    let mut c = Client::connect(&url, NoTls).unwrap();
    c.batch_execute(&format!("ALTER TABLE \"{}\".audit ADD CONSTRAINT no_allowed CHECK (outcome <> 'allowed') NOT VALID", e.cs)).unwrap();
    assert!(q(&s, "CREATE (:T {i:2})").is_err(), "write must be refused when its audit entry cannot be written");
    assert!(q(&s, "MATCH (n:T) SET n.i = 9").is_err());
    c.batch_execute(&format!("ALTER TABLE \"{}\".audit DROP CONSTRAINT no_allowed", e.cs)).unwrap();
    // nothing happened in the graph, and reads were never blocked
    assert_eq!(count(&s, "T"), 1);
    assert_eq!(q(&s, "MATCH (n:T) RETURN n.i").unwrap().rows[0][0], Value::Int(1));
    q(&s, "CREATE (:T {i:3})").unwrap();
    assert_eq!(count(&s, "T"), 2);
}
