//! End to end through logins and roles on PostgreSQL: encryption at rest, data policy, record-level audit,
//! fail-closed audit, BI export filtering, key rotation. Needs NEXGRAPH_TEST_PG_URL.
#![cfg(feature = "security")]

use nexgraph::crypto::Secret;
use nexgraph::query::Params;
use nexgraph::security::policy::{Mask, Policy, Rule};
use nexgraph::security::{ControlOptions, ControlPlane, ManagerOptions, Principal, Quotas, Role, Session, TenantManager};
use nexgraph::{Error, PgOptions, Value};
use postgres::{Client, NoTls};
use std::sync::Arc;

const PW: &str = "correct horse battery staple";
const CSV: &str = include_str!("fixtures/eicu_demo_patient.csv");

fn url() -> Option<String> {
    let u = std::env::var("NEXGRAPH_TEST_PG_URL").ok();
    if u.is_none() {
        eprintln!("NEXGRAPH_TEST_PG_URL not set: skipping");
    }
    u
}

fn policy() -> Policy {
    Policy {
        rules: vec![Rule {
            up_to: Some(Role::Writer),
            deny: vec!["Stay.hospitalid".into(), "Stay.wardid".into()], // label-qualified on purpose: BI must resolve the real label
            mask: vec![("patienthealthsystemstayid".into(), Mask::Pseudonym), ("age".into(), Mask::TopCode(80.0))],
            deny_labels: vec![],
        }],
        min_cell: 11,
        min_cell_up_to: Some(Role::Reader),
        pseudonym_key: b"0123456789abcdef0123456789abcdef".to_vec(),
        summary_procedures: vec![],
    }
}

struct Env {
    url: String,
    cs: String,
    cp: Arc<ControlPlane>,
    admin: Principal,
}

fn mk_mgr(e: &Env, master: u8, id: u32, prev: Vec<(u32, Secret)>) -> TenantManager {
    let mo = ManagerOptions {
        pg: PgOptions::default().require_tls(false),
        policy: policy(),
        master_key: Some(Secret(vec![master; 32])),
        master_key_id: id,
        previous_master_keys: prev,
        ..Default::default()
    };
    TenantManager::new(e.cp.clone(), &e.url, mo)
}

impl Env {
    fn new(url: &str, name: &str) -> Env {
        let cs = format!("nxe_{name}_{}", std::process::id());
        cleanup(url, &cs);
        let cp = Arc::new(ControlPlane::open(url, ControlOptions { schema: cs.clone(), require_tls: false, ..Default::default() }).unwrap());
        cp.bootstrap_platform_admin("root", PW).unwrap();
        let t = cp.login(None, "root", PW).unwrap();
        let admin = cp.authenticate(&t.token).unwrap();
        Env { url: url.into(), cs, cp, admin }
    }
    fn login(&self, m: &TenantManager, tenant: &str, name: &str) -> Session {
        m.session(&self.cp.login(Some(tenant), name, PW).unwrap().token).unwrap()
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

fn q(s: &Session, text: &str) -> Result<nexgraph::query::QueryResult, Error> {
    s.query(text, &Params::new())
}

/// Load the first `n` stays that need no string escaping, through NQL as the tenant admin.
fn load(admin: &Session, n: usize) -> usize {
    let mut it = nexgraph::ingest::parse_csv(CSV, ',').unwrap().into_iter().map(|(_, r)| r);
    let h = it.next().unwrap();
    let ix = |c: &str| h.iter().position(|x| x == c).unwrap();
    let (a, g, hid, w, d, u, e) = (ix("age"), ix("gender"), ix("hospitalid"), ix("wardid"), ix("apacheadmissiondx"), ix("unittype"), ix("ethnicity"));
    let hs = ix("patienthealthsystemstayid");
    let mut stmts = vec![];
    for r in it {
        if r.iter().any(|f| f.contains(['\'', '\\'])) || r[a].parse::<i64>().is_err() || r[hid].is_empty() || r[w].is_empty() {
            continue;
        }
        stmts.push(format!(
            "(:Stay {{hs: '{}', age: {}, gender: '{}', hospitalid: {}, wardid: {}, dx: '{}', unittype: '{}', ethnicity: '{}'}})",
            r[hs], r[a], r[g], r[hid], r[w], r[d], r[u], r[e]
        ));
        if stmts.len() == n {
            break;
        }
    }
    let _ = hs;
    for chunk in stmts.chunks(40) {
        q(admin, &format!("CREATE {}", chunk.join(", "))).unwrap();
    }
    stmts.len()
}

fn setup(e: &Env, m: &TenantManager) -> (Session, usize) {
    e.cp.create_tenant(&e.admin, "icu", Quotas::default()).unwrap();
    for (u, r) in [("boss", Role::TenantAdmin), ("analyst", Role::Reader), ("nurse", Role::Writer)] {
        e.cp.create_user(&e.admin, Some("icu"), u, PW, r).unwrap();
    }
    let admin = e.login(m, "icu", "boss");
    let n = load(&admin, 400);
    assert!(n >= 300);
    (admin, n)
}

fn pg_schema(e: &Env, t: &str) -> String {
    format!("{}_t{}", e.cs, e.cp.tenant(t).unwrap().id)
}

#[test]
fn encrypted_tenant_policy_audit_and_bi() {
    let Some(url) = url() else { return };
    let e = Env::new(&url, "main");
    let m = mk_mgr(&e, 9, 1, vec![]);
    let (admin, n) = setup(&e, &m);

    // --- at rest: PostgreSQL holds ciphertext, no mirror tables
    let mut c = Client::connect(&url, NoTls).unwrap();
    let schema = pg_schema(&e, "icu");
    let rows = c.query(&format!("SELECT payload FROM \"{schema}\".commits"), &[]).unwrap();
    assert!(!rows.is_empty());
    for r in &rows {
        let p: Vec<u8> = r.get(0);
        assert_eq!(p[0], 0xE1, "every commit is sealed");
        for needle in ["Caucasian", "Neuro ICU", "Med-Surg", "Female"] {
            assert!(!p.windows(needle.len()).any(|w| w == needle.as_bytes()), "{needle} readable in commit log");
        }
    }
    let mirror: i64 = c.query_one("SELECT count(*) FROM information_schema.tables WHERE table_schema = $1 AND table_name IN ('nodes','edges')", &[&schema]).unwrap().get(0);
    assert_eq!(mirror, 0, "no plaintext mirror when encrypted");

    // --- reader: aggregates only, small cells suppressed
    let analyst = e.login(&m, "icu", "analyst");
    let r = q(&analyst, "MATCH (s:Stay) RETURN s.gender AS g, count(*) AS n").unwrap();
    assert!(r.rows.iter().all(|row| matches!(row[1], Value::Int(k) if k >= 11)));
    let total: i64 = r.rows.iter().map(|row| if let Value::Int(k) = row[1] { k } else { 0 }).sum();
    assert!(total as usize <= n);
    for bad in ["MATCH (s:Stay) RETURN s.age", "MATCH (s:Stay) RETURN s.hospitalid, count(*) AS n", "MATCH (s:Stay) WHERE s.wardid = 89 RETURN count(*) AS n"] {
        assert!(matches!(q(&analyst, bad), Err(Error::Denied(_))), "{bad}");
    }
    let bi = analyst.bi_table("node_Stay", None);
    assert!(matches!(bi, Err(Error::Denied(_))), "aggregate-only roles get no record export: {:?}", bi.map(|t| t.0.name));

    // --- writer: record level, masked and top-coded, denied columns stay out of reach
    let nurse = e.login(&m, "icu", "nurse");
    let r = q(&nurse, "MATCH (s:Stay) RETURN s.hs AS hs, s.patienthealthsystemstayid AS pid, s.age AS age LIMIT 50").unwrap();
    assert_eq!(r.rows.len(), 50);
    assert!(r.rows.iter().all(|row| matches!(row[2], Value::Int(a) if a <= 80)));
    assert!(matches!(q(&nurse, "MATCH (s:Stay) RETURN s.hospitalid"), Err(Error::Denied(_))));
    assert!(matches!(nurse.import_hyperedges_csv(&nexgraph::ingest::HyperCsv { edge_label: "E", members: &[("a", "A")], key_prop: "k", weight_col: None, time_cols: &[] }, "a\n1\n", &nexgraph::ingest::HyperOpts::default()), Err(Error::Denied(_))), "bulk import closed to restricted roles");

    // BI for the writer: label table only, no denied columns
    let (tables, _) = nurse.bi_tables(None).unwrap();
    assert!(tables.iter().all(|t| t.name != "nodes" && t.name != "edges"));
    let (stay, _) = nurse.bi_table("node_Stay", None).unwrap();
    let names: Vec<&str> = stay.columns.iter().map(|c| c.name.as_str()).collect();
    assert!(!names.contains(&"hospitalid") && !names.contains(&"wardid") && names.contains(&"age"), "{names:?}");
    let ai = names.iter().position(|c| *c == "age").unwrap();
    assert!(stay.rows.iter().all(|row| matches!(row[ai], Value::Int(a) if a <= 80)));
    assert!(matches!(nurse.bi_table("nodes", None), Err(Error::Denied(_))));
    // the tenant admin still sees everything
    let (full, _) = admin.bi_table("node_Stay", None).unwrap();
    assert!(full.columns.iter().any(|c| c.name == "hospitalid"));

    // --- audit: denials and restricted reads are recorded, with the properties touched
    let log = e.cp.read_audit(&e.admin, Some("icu"), 500).unwrap();
    assert!(log.iter().any(|a| a.actor == "analyst" && a.outcome == "denied" && a.detail.starts_with("policy:")));
    let ok = log.iter().find(|a| a.actor == "nurse" && a.action == "query" && a.outcome == "ok" && a.detail.contains("props=")).expect("restricted read audited with props");
    assert!(ok.detail.contains("props=hs,patienthealthsystemstayid,age") && ok.detail.contains("masked_cells="), "{}", ok.detail);
    let nodes = q(&nurse, "MATCH (s:Stay) WHERE s.unittype = 'Neuro ICU' RETURN s LIMIT 3").unwrap();
    assert_eq!(nodes.rows.len(), 3);
    let log = e.cp.read_audit(&e.admin, Some("icu"), 5).unwrap();
    assert!(log.iter().any(|a| a.actor == "nurse" && a.detail.contains("nodes=[") && a.detail.contains("node_count=3")), "record-level audit missing: {:?}", log.iter().map(|a| &a.detail).collect::<Vec<_>>());
    e.cp.verify_audit_chain().unwrap();

    // --- fail closed: if the access cannot be recorded, restricted data is not released
    let ctrl = e.cp.schema_name().to_string();
    let mut sup = Client::connect(&url, NoTls).unwrap();
    sup.batch_execute(&format!("ALTER TABLE \"{ctrl}\".audit RENAME TO audit_away")).unwrap();
    let res = q(&nurse, "MATCH (s:Stay) RETURN s.age AS age LIMIT 5");
    sup.batch_execute(&format!("ALTER TABLE \"{ctrl}\".audit_away RENAME TO audit")).unwrap();
    assert!(res.is_err(), "data was released although the audit write failed");
    assert!(q(&nurse, "MATCH (s:Stay) RETURN s.age AS age LIMIT 5").is_ok(), "works again once audit is back");
}

#[test]
fn encrypted_records_cannot_be_moved_between_tenants_and_keys_rotate() {
    let Some(url) = url() else { return };
    let e = Env::new(&url, "rot");
    let m = mk_mgr(&e, 9, 1, vec![]);
    let (admin, n) = setup(&e, &m);
    e.cp.create_tenant(&e.admin, "other", Quotas::default()).unwrap();
    e.cp.create_user(&e.admin, Some("other"), "boss2", PW, Role::TenantAdmin).unwrap();
    let other = e.login(&m, "other", "boss2");
    q(&other, "CREATE (:Stay {hs: 'x', age: 1})").unwrap();
    admin.checkpoint().unwrap();
    m.close_all();
    drop((admin, other));

    // rotation: new master 2, old master 1 kept for reading; checkpoint re-seals under key 2
    let m2 = mk_mgr(&e, 5, 2, vec![(1, Secret(vec![9; 32]))]);
    let admin = e.login(&m2, "icu", "boss");
    let cnt = |s: &Session| match q(s, "MATCH (s:Stay) RETURN count(*) AS n").unwrap().rows[0][0] {
        Value::Int(i) => i as usize,
        _ => panic!(),
    };
    assert_eq!(cnt(&admin), n);
    q(&admin, "CREATE (:Stay {hs: 'new', age: 2})").unwrap();
    admin.checkpoint().unwrap();
    m2.close_all();
    drop(admin);
    let m3 = mk_mgr(&e, 5, 2, vec![]); // old key gone
    assert_eq!(cnt(&e.login(&m3, "icu", "boss")), n + 1);
    q(&e.login(&m3, "icu", "boss"), "CREATE (:Stay {hs: 'after', age: 4})").unwrap(); // an un-checkpointed commit
    m3.close_all();
    let m_old = mk_mgr(&e, 9, 1, vec![]);
    assert!(m_old.session(&e.cp.login(Some("icu"), "boss", PW).unwrap().token).and_then(|s| q(&s, "MATCH (s:Stay) RETURN count(*) AS n")).is_err(), "old key alone must fail");
    m_old.close_all();

    // move a sealed commit from tenant `other` into tenant `icu`: authentication must fail
    let mut c = Client::connect(&url, NoTls).unwrap();
    let (s_icu, s_other) = (pg_schema(&e, "icu"), pg_schema(&e, "other"));
    let row = c.query_one(&format!("SELECT ts, payload, crc FROM \"{s_other}\".commits ORDER BY ts DESC LIMIT 1"), &[]).unwrap();
    let (ts, payload, crc): (i64, Vec<u8>, i64) = (row.get(0), row.get(1), row.get(2));
    let hit = c.execute(&format!("UPDATE \"{s_icu}\".commits SET payload = $1, crc = $2 WHERE ts = (SELECT max(ts) FROM \"{s_icu}\".commits)"), &[&payload, &crc]).unwrap();
    assert_eq!(hit, 1, "the test must really tamper with a commit");
    let _ = ts;
    let m4 = mk_mgr(&e, 5, 2, vec![]);
    let r = m4.session(&e.cp.login(Some("icu"), "boss", PW).unwrap().token).and_then(|s| q(&s, "MATCH (s:Stay) RETURN count(*) AS n"));
    assert!(r.is_err(), "a record from another tenant was accepted");
}

#[test]
fn encryption_with_the_plaintext_mirror_is_refused_and_plaintext_schemas_are_not_mixed() {
    let Some(url) = url() else { return };
    let e = Env::new(&url, "mix");
    // 1. no key: plaintext tenant
    let plain = TenantManager::new(e.cp.clone(), &url, ManagerOptions { pg: PgOptions::default().require_tls(false), ..Default::default() });
    e.cp.create_tenant(&e.admin, "icu", Quotas::default()).unwrap();
    e.cp.create_user(&e.admin, Some("icu"), "boss", PW, Role::TenantAdmin).unwrap();
    let s = e.login(&plain, "icu", "boss");
    q(&s, "CREATE (:Stay {hs: 'a', age: 3})").unwrap();
    plain.close_all();
    drop(s);
    // 2. turning encryption on over existing plaintext records is refused, not silently mixed
    let enc = mk_mgr(&e, 9, 1, vec![]);
    let r = enc.session(&e.cp.login(Some("icu"), "boss", PW).unwrap().token).and_then(|s| q(&s, "MATCH (s:Stay) RETURN count(*) AS n"));
    assert!(matches!(r, Err(Error::Invalid(ref m)) if m.contains("unencrypted")), "{r:?}");
    // 3. library level: a key with the mirror on is refused
    let ring = nexgraph::crypto::Keyring::new(1, [1; 32]);
    let r = nexgraph::Database::open_postgres_with(&url, PgOptions::default().require_tls(false).schema(format!("{}_mirror", e.cs)).encrypt(ring));
    assert!(matches!(r, Err(Error::Invalid(ref m)) if m.contains("mirror")), "{:?}", r.err());
}
