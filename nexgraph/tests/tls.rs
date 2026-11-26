//! TLS tests for the PostgreSQL connection. They need a TLS-only server; see
//! `tests/fixtures/gen_certs.sh` (test-only certificates, SAN `pg.test`, `localhost`, `127.0.0.1`).
//!   NEXGRAPH_TEST_PG_TLS_PORT   port of that server (database `nexgraph_tls`, user `postgres`)
//!   NEXGRAPH_TEST_PG_URL        a plaintext server, for the "refuse plaintext" tests
#![cfg(feature = "postgres")]

use nexgraph::{Database, Error, PgOptions};
use std::path::PathBuf;

fn fx(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

macro_rules! need_tls {
    () => {
        match std::env::var("NEXGRAPH_TEST_PG_TLS_PORT") {
            Ok(p) => p,
            Err(_) => {
                eprintln!("NEXGRAPH_TEST_PG_TLS_PORT not set: skipping");
                return;
            }
        }
    };
}

/// `host` is the name the certificate is checked against; the TCP connection goes to 127.0.0.1.
fn url(port: &str, host: &str, mode: &str) -> String {
    format!("host={host} hostaddr=127.0.0.1 port={port} user=postgres dbname=nexgraph_tls sslmode={mode}")
}

fn schema(t: &str) -> String {
    format!("nxtls_{t}_{}", std::process::id())
}

fn cleanup(port: &str, s: &str) {
    if let Ok(mut c) = postgres::Client::connect(&format!("host=127.0.0.1 port={port} user=postgres dbname=nexgraph_tls sslmode=require"), tls_any()) {
        let _ = c.batch_execute(&format!("DROP SCHEMA IF EXISTS \"{s}\" CASCADE"));
    }
}

fn tls_any() -> tokio_postgres_rustls::MakeRustlsConnect {
    nexgraph::tls::PgTls { danger_skip_cert_verification: true, ..Default::default() }.connector_for_tests()
}

#[test]
fn verified_tls_connection_works_end_to_end() {
    let port = need_tls!();
    let s = schema("ok");
    let db = Database::open_postgres_with(&url(&port, "pg.test", "require"), PgOptions::default().schema(s.clone()).tls_ca_file(fx("ca.pem"))).unwrap();
    let info = db.pg_session_info().unwrap();
    assert!(info.tls, "{info:?}");
    assert_eq!(info.tls_version.as_deref(), Some("TLSv1.3"));
    db.query("CREATE (:P {n:1})").unwrap();
    assert!(db.verify_mirror().unwrap().is_clean());
    drop(db);
    // data written over TLS is readable after reopening
    let db = Database::open_postgres_with(&url(&port, "pg.test", "require"), PgOptions::default().schema(s.clone()).tls_ca_file(fx("ca.pem"))).unwrap();
    assert_eq!(db.stats().live_nodes, 1);
    drop(db);
    cleanup(&port, &s);
}

#[test]
fn untrusted_or_mismatched_certificates_are_refused() {
    let port = need_tls!();
    let s = schema("bad");
    let base = || PgOptions::default().schema(s.clone());
    // CA that did not sign the server certificate
    let r = Database::open_postgres_with(&url(&port, "pg.test", "require"), base().tls_ca_file(fx("other_ca.pem")));
    eprintln!("wrong CA      -> {:?}", r.as_ref().err());
    assert!(matches!(&r, Err(Error::Backend(m)) if m.to_lowercase().contains("certificate") || m.to_lowercase().contains("unknownissuer")), "wrong CA must fail with a certificate error: {:?}", r.err());
    // no private CA configured: the bundled public roots do not know the test CA
    let r = Database::open_postgres_with(&url(&port, "pg.test", "require"), base());
    eprintln!("unknown CA    -> {:?}", r.as_ref().err());
    assert!(matches!(&r, Err(Error::Backend(m)) if m.to_lowercase().contains("certificate") || m.to_lowercase().contains("unknownissuer")), "unknown CA must fail with a certificate error: {:?}", r.err());
    // right CA, but the certificate is not valid for this host name
    let r = Database::open_postgres_with(&url(&port, "other.test", "require"), base().tls_ca_file(fx("ca.pem")));
    eprintln!("name mismatch -> {:?}", r.as_ref().err());
    assert!(matches!(&r, Err(Error::Backend(m)) if m.to_lowercase().contains("notvalidforname") || m.to_lowercase().contains("not valid for")), "host name mismatch must fail with a name error: {:?}", r.err());
    // unreadable / empty CA file is a clear configuration error
    let r = Database::open_postgres_with(&url(&port, "pg.test", "require"), base().tls_ca_file(fx("does-not-exist.pem")));
    assert!(matches!(r, Err(Error::Invalid(_))));
    let r = Database::open_postgres_with(&url(&port, "pg.test", "require"), base().tls_ca_file(fx("gen_certs.sh")));
    assert!(matches!(r, Err(Error::Invalid(_))), "file without certificates");
    // nothing was created by the failed attempts
    cleanup(&port, &s);
}

#[test]
fn skipping_verification_is_explicit_and_still_encrypts() {
    let port = need_tls!();
    let s = schema("skip");
    let mut o = PgOptions::default().schema(s.clone());
    o.tls.danger_skip_cert_verification = true;
    let db = Database::open_postgres_with(&url(&port, "other.test", "require"), o).unwrap();
    assert!(db.pg_session_info().unwrap().tls);
    drop(db);
    cleanup(&port, &s);
}

#[test]
fn plaintext_is_never_used_when_tls_is_required() {
    let port = need_tls!();
    // 1. the TLS-only server rejects an unencrypted session
    let r = Database::open_postgres_with(&url(&port, "pg.test", "disable"), PgOptions::default().schema(schema("pt")));
    assert!(r.is_err());
    // 2. require_tls overrides sslmode=disable in the URL and succeeds over TLS
    let s = schema("force");
    let db = Database::open_postgres_with(&url(&port, "pg.test", "disable"), PgOptions::default().schema(s.clone()).require_tls(true).tls_ca_file(fx("ca.pem"))).unwrap();
    assert!(db.pg_session_info().unwrap().tls);
    drop(db);
    cleanup(&port, &s);
    // 3. against a server without TLS, require_tls refuses to continue
    if let Ok(plain) = std::env::var("NEXGRAPH_TEST_PG_URL") {
        let r = Database::open_postgres_with(&plain, PgOptions::default().schema(schema("nossl")).require_tls(true));
        match r {
            Err(Error::Backend(m)) => assert!(m.to_lowercase().contains("tls") || m.to_lowercase().contains("ssl"), "{m}"),
            Err(e) => panic!("unexpected error {e}"),
            Ok(_) => panic!("must not fall back to plaintext"),
        }
    }
}
