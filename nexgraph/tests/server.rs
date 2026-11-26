//! End-to-end tests of the HTTPS server: real TLS handshakes, real HTTP, real PostgreSQL.
//!   NEXGRAPH_TEST_PG_URL   plaintext PostgreSQL (the control plane runs with require_tls = false here)
#![cfg(feature = "server")]

use nexgraph::security::{ControlOptions, ControlPlane, ManagerOptions, TenantManager};
use nexgraph::server::{self, ServerConfig};
use nexgraph::PgOptions;
use postgres::{Client, NoTls};
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use serde_json::{json, Value as J};
use std::io::{BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;

const PW: &str = "correct horse battery staple";

fn fx(n: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(n)
}

struct Srv {
    addr: SocketAddr,
    handle: axum_server::Handle,
    thread: Option<std::thread::JoinHandle<()>>,
    cs: String,
    url: String,
}

impl Srv {
    fn start(url: &str, name: &str, cfg: ServerConfig) -> Srv {
        let cs = format!("nxsrv_{name}_{}", std::process::id());
        cleanup(url, &cs);
        let cp = Arc::new(ControlPlane::open(url, ControlOptions { schema: cs.clone(), require_tls: false, ..Default::default() }).unwrap());
        cp.bootstrap_platform_admin("root", PW).unwrap();
        let mgr = Arc::new(TenantManager::new(cp, url, ManagerOptions { pg: PgOptions::default().require_tls(false), ..Default::default() }));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = axum_server::Handle::new();
        let h2 = handle.clone();
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
            rt.block_on(async move {
                let tls = server::tls_config(&fx("server.pem"), &fx("server.key")).await.unwrap();
                server::serve_on(listener, server::router(mgr, cfg), Some(tls), h2).await.unwrap();
            });
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        Srv { addr, handle, thread: Some(thread), cs, url: url.into() }
    }
}

impl Drop for Srv {
    fn drop(&mut self) {
        self.handle.shutdown();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
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

struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Resp {
    fn json(&self) -> J {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("not JSON ({e}): {:?}", self.body))
    }
    fn header(&self, k: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v.as_str())
    }
}

fn client_cfg(ca: &str) -> Arc<ClientConfig> {
    let mut roots = RootCertStore::empty();
    for c in rustls_pemfile::certs(&mut BufReader::new(std::fs::File::open(fx(ca)).unwrap())) {
        roots.add(c.unwrap()).unwrap();
    }
    let p = Arc::new(rustls::crypto::ring::default_provider());
    Arc::new(ClientConfig::builder_with_provider(p).with_safe_default_protocol_versions().unwrap().with_root_certificates(roots).with_no_client_auth())
}

/// One HTTPS request over a fresh connection (`Connection: close`).
fn https(addr: SocketAddr, ca: &str, method: &str, path: &str, token: Option<&str>, body: Option<&J>) -> Resp {
    https_auth(addr, ca, method, path, token.map(|t| format!("Bearer {t}")).as_deref(), body)
}

/// Like [`https`] but with the full `Authorization` header value.
fn https_auth(addr: SocketAddr, ca: &str, method: &str, path: &str, auth: Option<&str>, body: Option<&J>) -> Resp {
    let conn = ClientConnection::new(client_cfg(ca), ServerName::try_from("pg.test").unwrap()).unwrap();
    let tcp = TcpStream::connect(addr).unwrap();
    tcp.set_read_timeout(Some(std::time::Duration::from_secs(20))).unwrap();
    let mut s = StreamOwned::new(conn, tcp);
    let b = body.map(|b| b.to_string()).unwrap_or_default();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: pg.test\r\nConnection: close\r\n");
    if let Some(a) = auth {
        req.push_str(&format!("Authorization: {a}\r\n"));
    }
    if body.is_some() {
        req.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n", b.len()));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).unwrap();
    s.write_all(b.as_bytes()).unwrap();
    let mut raw = Vec::new();
    let _ = s.read_to_end(&mut raw); // close_notify may be missing; we only need the bytes
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let mut lines = head.lines();
    let status: u16 = lines.next().unwrap().split_whitespace().nth(1).unwrap().parse().unwrap();
    let headers = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect();
    // de-chunk if needed
    let body = if head.to_lowercase().contains("transfer-encoding: chunked") {
        let mut out = String::new();
        let mut r = rest;
        while let Some((len, tail)) = r.split_once("\r\n") {
            let n = usize::from_str_radix(len.trim(), 16).unwrap_or(0);
            if n == 0 {
                break;
            }
            out.push_str(&tail[..n]);
            r = &tail[n + 2..];
        }
        out
    } else {
        rest.to_string()
    };
    Resp { status, headers, body }
}

fn call(srv: &Srv, method: &str, path: &str, token: Option<&str>, body: Option<J>) -> Resp {
    https(srv.addr, "ca.pem", method, path, token, body.as_ref())
}

fn login(srv: &Srv, tenant: Option<&str>, user: &str, pw: &str) -> Resp {
    call(srv, "POST", "/v1/login", None, Some(json!({"tenant": tenant, "user": user, "password": pw})))
}

fn token(srv: &Srv, tenant: Option<&str>, user: &str) -> String {
    let r = login(srv, tenant, user, PW);
    assert_eq!(r.status, 200, "{}", r.body);
    r.json()["token"].as_str().unwrap().to_string()
}

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

#[test]
fn full_tenant_lifecycle_over_https() {
    let url = need_pg!();
    let srv = Srv::start(&url, "life", ServerConfig::default());
    let root = token(&srv, None, "root");
    // platform admin provisions two tenants and their admins over the API
    for (t, u) in [("acme", "ann"), ("beta", "bob")] {
        assert_eq!(call(&srv, "POST", "/v1/admin/tenants", Some(&root), Some(json!({"name": t}))).status, 201);
        let r = call(&srv, "POST", "/v1/admin/users", Some(&root), Some(json!({"tenant": t, "name": u, "password": PW, "role": "tenant_admin"})));
        assert_eq!(r.status, 201, "{}", r.body);
    }
    let (ann, bob) = (token(&srv, Some("acme"), "ann"), token(&srv, Some("beta"), "bob"));
    assert_eq!(call(&srv, "GET", "/v1/me", Some(&ann), None).json(), json!({"user": "ann", "tenant": "acme", "role": "tenant_admin"}));
    // queries with parameters; types survive the round trip
    let w = call(&srv, "POST", "/v1/query", Some(&ann), Some(json!({"query": "CREATE (:City {name: $n, pop: $p, tags: $t})", "params": {"n": "Dangriga", "p": 10000, "t": ["a", "b"]}})));
    assert_eq!(w.status, 200, "{}", w.body);
    assert_eq!(w.json()["stats"]["nodes_created"], 1);
    let r = call(&srv, "POST", "/v1/query", Some(&ann), Some(json!({"query": "MATCH (c:City) RETURN c.name, c.pop, c.tags"})));
    assert_eq!(r.json()["rows"], json!([["Dangriga", 10000, ["a", "b"]]]));
    // isolation over the wire
    let r = call(&srv, "POST", "/v1/query", Some(&bob), Some(json!({"query": "MATCH (c) RETURN count(*)"})));
    assert_eq!(r.json()["rows"], json!([[0]]));
    // admin endpoints are closed to tenant admins and platform admin cannot read data
    assert_eq!(call(&srv, "POST", "/v1/admin/tenants", Some(&ann), Some(json!({"name": "evil"}))).status, 403);
    assert_eq!(call(&srv, "POST", "/v1/query", Some(&root), Some(json!({"query": "MATCH (n) RETURN n"}))).status, 403);
    // a tenant admin manages its own users; a reader cannot write
    assert_eq!(call(&srv, "POST", "/v1/admin/users", Some(&ann), Some(json!({"name": "rita", "password": PW, "role": "reader"}))).status, 201);
    let rita = token(&srv, Some("acme"), "rita");
    assert_eq!(call(&srv, "POST", "/v1/query", Some(&rita), Some(json!({"query": "CREATE (:X)"}))).status, 403);
    assert_eq!(call(&srv, "POST", "/v1/query", Some(&rita), Some(json!({"query": "MATCH (c:City) RETURN count(*)"}))).status, 200);
    // audit is scoped
    let a = call(&srv, "GET", "/v1/audit?limit=50", Some(&ann), None).json();
    assert!(a["entries"].as_array().unwrap().iter().all(|e| e["tenant_id"] == a["entries"][0]["tenant_id"]));
    assert_eq!(call(&srv, "GET", "/v1/audit", Some(&rita), None).status, 403);
    // password change and logout revoke tokens
    assert_eq!(call(&srv, "POST", "/v1/me/password", Some(&rita), Some(json!({"old": PW, "new": "a completely new passphrase"}))).status, 200);
    assert_eq!(call(&srv, "GET", "/v1/me", Some(&rita), None).status, 401);
    assert_eq!(login(&srv, Some("acme"), "rita", PW).status, 401);
    let rita2 = login(&srv, Some("acme"), "rita", "a completely new passphrase").json()["token"].as_str().unwrap().to_string();
    assert_eq!(call(&srv, "POST", "/v1/logout", Some(&rita2), None).status, 200);
    assert_eq!(call(&srv, "GET", "/v1/me", Some(&rita2), None).status, 401);
    // disable + purge through the API
    assert_eq!(call(&srv, "DELETE", "/v1/admin/tenants/beta", Some(&root), None).status, 400, "must be disabled first");
    assert_eq!(call(&srv, "POST", "/v1/admin/tenants/beta/disable", Some(&root), None).status, 200);
    assert_eq!(call(&srv, "GET", "/v1/me", Some(&bob), None).status, 401);
    assert_eq!(call(&srv, "DELETE", "/v1/admin/tenants/beta", Some(&root), None).status, 200);
    assert_eq!(login(&srv, Some("beta"), "bob", PW).status, 401);
}

#[test]
fn transport_and_protocol_hardening() {
    let url = need_pg!();
    let srv = Srv::start(&url, "hard", ServerConfig { max_body_bytes: 2048, ..Default::default() });
    // headers on every response
    let r = call(&srv, "GET", "/healthz", None, None);
    assert_eq!((r.status, r.body.as_str()), (200, "ok"));
    assert!(r.header("strict-transport-security").is_some_and(|v| v.contains("max-age")));
    assert_eq!(r.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(r.header("cache-control"), Some("no-store"));
    assert_eq!(r.header("content-security-policy"), Some("default-src 'none'"));
    // unknown paths and methods
    assert_eq!(call(&srv, "GET", "/", None, None).status, 404);
    assert_eq!(call(&srv, "GET", "/v1/admin/tenants", None, None).status, 405);
    assert_eq!(call(&srv, "GET", "/v1/query", None, None).status, 405);
    // missing / malformed / forged credentials: 401 with a uniform body, never a stack trace
    for h in [None, Some(""), Some("ngt_forged"), Some("Basic abc")] {
        let r = call(&srv, "GET", "/v1/me", h, None);
        assert_eq!(r.status, 401, "{h:?}");
        assert_eq!(r.json(), json!({"error": "invalid credentials"}));
        assert_eq!(r.header("www-authenticate"), Some("Bearer"));
    }
    // bad JSON, oversized body
    assert_eq!(login_raw(&srv, "{not json").status, 400);
    let big = json!({"user": "x".repeat(5000), "password": "y"});
    assert_eq!(call(&srv, "POST", "/v1/login", None, Some(big)).status, 413);
    // login does not reveal whether the user exists
    let a = login(&srv, Some("nope"), "ghost", PW);
    let b = login(&srv, None, "root", "wrong password here");
    assert_eq!((a.status, a.body.clone()), (401, b.body.clone()));
    assert_eq!(a.json(), json!({"error": "invalid credentials"}));
    // a client that does not trust our CA cannot even complete the handshake
    let conn = ClientConnection::new(client_cfg("other_ca.pem"), ServerName::try_from("pg.test").unwrap()).unwrap();
    let mut s = StreamOwned::new(conn, TcpStream::connect(srv.addr).unwrap());
    let err = s.write_all(b"GET /healthz HTTP/1.1\r\nHost: pg.test\r\n\r\n").and_then(|_| s.flush()).expect_err("handshake with an untrusted CA must fail");
    assert!(err.to_string().to_lowercase().contains("unknownissuer") || err.to_string().to_lowercase().contains("certificate"), "{err}");
    // the negotiated protocol is TLS 1.3 for a modern client
    let conn = ClientConnection::new(client_cfg("ca.pem"), ServerName::try_from("pg.test").unwrap()).unwrap();
    let mut s = StreamOwned::new(conn, TcpStream::connect(srv.addr).unwrap());
    s.write_all(b"GET /healthz HTTP/1.1\r\nHost: pg.test\r\nConnection: close\r\n\r\n").unwrap();
    let mut sink = Vec::new();
    let _ = s.read_to_end(&mut sink);
    assert_eq!(s.conn.protocol_version(), Some(rustls::ProtocolVersion::TLSv1_3));
    // plain HTTP to the TLS port gets no application response
    let mut t = TcpStream::connect(srv.addr).unwrap();
    t.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
    t.write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
    let mut buf = Vec::new();
    let _ = t.read_to_end(&mut buf);
    assert!(!String::from_utf8_lossy(&buf).contains("200 OK"), "no plaintext service on the TLS port");
}

fn login_raw(srv: &Srv, raw: &str) -> Resp {
    let conn = ClientConnection::new(client_cfg("ca.pem"), ServerName::try_from("pg.test").unwrap()).unwrap();
    let mut s = StreamOwned::new(conn, TcpStream::connect(srv.addr).unwrap());
    s.write_all(format!("POST /v1/login HTTP/1.1\r\nHost: pg.test\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{raw}", raw.len()).as_bytes()).unwrap();
    let mut v = Vec::new();
    let _ = s.read_to_end(&mut v);
    let t = String::from_utf8_lossy(&v).to_string();
    Resp { status: t.split_whitespace().nth(1).unwrap().parse().unwrap(), headers: vec![], body: t.split_once("\r\n\r\n").map(|x| x.1.to_string()).unwrap_or_default() }
}

#[test]
fn login_is_rate_limited_per_client_address() {
    let url = need_pg!();
    let srv = Srv::start(&url, "rate", ServerConfig { logins_per_ip_per_minute: 5, ..Default::default() });
    let mut codes = Vec::new();
    for _ in 0..8 {
        codes.push(login(&srv, None, "root", "wrong password here").status);
    }
    assert_eq!(&codes[..5], &[401; 5]);
    assert!(codes[5..].iter().all(|c| *c == 429), "{codes:?}");
    let r = login(&srv, None, "root", PW);
    assert_eq!(r.status, 429, "even the right password is refused while limited");
    assert_eq!(r.header("retry-after"), Some("60"));
}

#[test]
fn account_lockout_applies_over_the_api() {
    let url = need_pg!();
    let srv = Srv::start(&url, "lock", ServerConfig { logins_per_ip_per_minute: 100, ..Default::default() });
    for _ in 0..5 {
        assert_eq!(login(&srv, None, "root", "wrong password here").status, 401);
    }
    assert_eq!(login(&srv, None, "root", PW).status, 401, "locked after repeated failures");
}


// ------------------------------------------------------------------ BI / Power BI endpoints

fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn basic(user: &str, pw: &str) -> String {
    format!("Basic {}", b64(format!("{user}:{pw}").as_bytes()))
}

fn pct(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

fn get_auth(srv: &Srv, path: &str, auth: Option<&str>) -> Resp {
    https_auth(srv.addr, "ca.pem", "GET", path, auth, None)
}

#[test]
fn bi_endpoints_serve_typed_tables_with_basic_auth() {
    let url = need_pg!();
    let srv = Srv::start(&url, "bi", ServerConfig::default());
    let root = token(&srv, None, "root");
    for (t, u) in [("acme", "ann"), ("beta", "bob")] {
        assert_eq!(call(&srv, "POST", "/v1/admin/tenants", Some(&root), Some(json!({"name": t}))).status, 201);
        assert_eq!(call(&srv, "POST", "/v1/admin/users", Some(&root), Some(json!({"tenant": t, "name": u, "password": PW, "role": "tenant_admin"}))).status, 201);
    }
    assert_eq!(call(&srv, "POST", "/v1/admin/users", Some(&token(&srv, Some("acme"), "ann")), Some(json!({"name": "rita", "password": PW, "role": "reader"}))).status, 201);
    let ann = token(&srv, Some("acme"), "ann");
    for q in [
        "CREATE (a:Host {ip:'10.0.0.1', risk: 0.5, up: true}), (b:Host {ip:'10.0.0.2', risk: 2, up: false}), (a)-[:FLOW {weight: 3, bytes: 10}]->(b)",
    ] {
        assert_eq!(call(&srv, "POST", "/v1/query", Some(&ann), Some(json!({"query": q}))).status, 200);
    }
    let ts0 = call(&srv, "POST", "/v1/query", Some(&ann), Some(json!({"query": "MATCH (h:Host) RETURN count(*)"}))).json()["ts"].as_u64().unwrap();
    assert_eq!(call(&srv, "POST", "/v1/query", Some(&ann), Some(json!({"query": "CREATE (:Host {ip:'10.0.0.3'})"}))).status, 200);

    // Basic auth as a reader, the way Power BI authenticates ("tenant/user")
    let rita = basic("acme/rita", PW);
    let r = get_auth(&srv, "/v1/bi/tables", Some(&rita));
    assert_eq!(r.status, 200, "{}", r.body);
    let names: Vec<String> = r.json()["tables"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
    assert_eq!(names, ["nodes", "edges", "edge_members", "node_Host", "edge_FLOW"]);
    let host = r.json()["tables"][3].clone();
    assert_eq!(host["rows"], 3);
    assert!(host["columns"].as_array().unwrap().contains(&json!({"name": "risk", "type": "double"})));

    let r = get_auth(&srv, "/v1/bi/tables/node_Host", Some(&rita));
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-type"), Some("text/csv; charset=utf-8"));
    assert!(r.header("x-nexgraph-ts").is_some());
    assert!(r.body.starts_with("id,labels,ip,risk,up\r\n"), "{}", r.body);
    assert_eq!(r.body.matches("\r\n").count(), 4);
    // second call goes through the credential cache and still works
    assert_eq!(get_auth(&srv, "/v1/bi/tables/node_Host?format=json", Some(&rita)).json().as_array().unwrap().len(), 3);
    // time travel: as of before the third host
    let old = get_auth(&srv, &format!("/v1/bi/tables/node_Host?format=json&as_of={ts0}"), Some(&rita));
    assert_eq!(old.json().as_array().unwrap().len(), 2, "{}", old.body);
    // Bearer works too
    assert_eq!(get_auth(&srv, "/v1/bi/tables/edges", Some(&format!("Bearer {ann}"))).status, 200);

    // read-only NQL over GET and POST
    let q = pct("MATCH (h:Host) RETURN h.ip AS ip, h.risk AS risk ORDER BY ip");
    let r = get_auth(&srv, &format!("/v1/bi/query?q={q}"), Some(&rita));
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body, "ip,risk\r\n10.0.0.1,0.5\r\n10.0.0.2,2\r\n10.0.0.3,\r\n");
    let r = https_auth(srv.addr, "ca.pem", "POST", "/v1/bi/query?format=json", Some(&rita), Some(&json!({"query": "MATCH (h:Host) WHERE h.ip = $ip RETURN h.risk AS r", "params": {"ip": "10.0.0.1"}})));
    assert_eq!(r.json(), json!([{"r": 0.5}]));
    // a writer must NOT be able to write through the BI endpoint (GET = CSRF-able)
    let w = get_auth(&srv, &format!("/v1/bi/query?q={}", pct("CREATE (:Evil)")), Some(&basic("acme/ann", PW)));
    assert_eq!(w.status, 403, "{}", w.body);
    let n = call(&srv, "POST", "/v1/query", Some(&ann), Some(json!({"query": "MATCH (e:Evil) RETURN count(*)"})));
    assert_eq!(n.json()["rows"], json!([[0]]));

    // errors are clean
    assert_eq!(get_auth(&srv, "/v1/bi/tables/nope", Some(&rita)).status, 400);
    assert_eq!(get_auth(&srv, "/v1/bi/tables/nodes?format=xml", Some(&rita)).status, 400);
    assert_eq!(get_auth(&srv, "/v1/bi/query", Some(&rita)).status, 400);
    assert_eq!(get_auth(&srv, "/v1/bi/tables/nodes?as_of=999999", Some(&rita)).status, 400);
    // credentials: none / wrong password / malformed / platform admin (no tenant data)
    let r = get_auth(&srv, "/v1/bi/tables", None);
    assert_eq!(r.status, 401);
    assert_eq!(r.header("www-authenticate"), Some("Bearer"));
    assert!(r.headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("www-authenticate") && v.starts_with("Basic")), "{:?}", r.headers);
    assert_eq!(get_auth(&srv, "/v1/bi/tables", Some(&basic("acme/rita", "wrong password here"))).status, 401);
    assert_eq!(get_auth(&srv, "/v1/bi/tables", Some("Basic !!!notbase64")).status, 401);
    assert_eq!(get_auth(&srv, "/v1/bi/tables", Some(&basic("rita", PW))).status, 401, "missing tenant prefix");
    assert_eq!(get_auth(&srv, "/v1/bi/tables", Some(&basic("root", PW))).status, 403);
    // tenant isolation
    let bob = get_auth(&srv, "/v1/bi/tables/nodes?format=json", Some(&basic("beta/bob", PW)));
    assert_eq!(bob.json(), json!([]));
    // a disabled account stops working even though its token is cached
    assert_eq!(call(&srv, "POST", "/v1/admin/users/disable", Some(&ann), Some(json!({"name": "rita", "disabled": true}))).status, 200);
    assert_eq!(get_auth(&srv, "/v1/bi/tables", Some(&rita)).status, 401);

    // Power Query script: public, no data
    let r = get_auth(&srv, "/v1/bi/powerquery?base=%22%3E%3Cscript%3E", None);
    assert_eq!(r.status, 400, "quotes and angle brackets must never reach the script");
    assert_eq!(get_auth(&srv, "/v1/bi/powerquery?base=https%3A%2F%2Fbi.example.org%3A8443", None).status, 200, "percent-encoding is decoded first");
    let r = get_auth(&srv, "/v1/bi/powerquery?base=https://bi.example.org:8443", None);
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.header("content-type"), Some("text/plain; charset=utf-8"));
    assert!(r.body.contains("BaseUrl = \"https://bi.example.org:8443\""));
}
