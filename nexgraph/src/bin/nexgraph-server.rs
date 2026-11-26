//! `nexgraph-server` — multi-tenant HTTPS API.
//!
//!   nexgraph-server init  --pg URL [--admin NAME]            create the control plane and the first platform admin
//!   nexgraph-server serve --pg URL --cert cert.pem --key key.pem [--listen 0.0.0.0:8443]
//!
//! Options: --frontend DIR (serve the web UI built in frontend/dist; or $NEXGRAPH_FRONTEND_DIR), --pg-ca FILE (private CA for the PostgreSQL server), --pg-allow-plaintext (NOT recommended),
//! --control-schema NAME, --insecure-http (plain HTTP, loopback addresses only).
//! The PostgreSQL URL may come from $NEXGRAPH_PG_URL; the admin password from $NEXGRAPH_ADMIN_PASSWORD
//! (or the first line of stdin). See SECURITY.md.

use nexgraph::security::{ControlOptions, ControlPlane, ManagerOptions, TenantManager};
use nexgraph::server::{self, ServerConfig};
use nexgraph::PgOptions;
use std::io::BufRead;
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const USAGE: &str = "usage:\n  nexgraph-server init  --pg URL [--admin NAME] [--control-schema S] [--pg-ca FILE] [--pg-allow-plaintext]\n  nexgraph-server serve --pg URL --cert FILE --key FILE [--listen ADDR] [--control-schema S] [--pg-ca FILE] [--pg-allow-plaintext] [--insecure-http]\n                [--frontend DIR] [--policy FILE.json] [--master-key-file FILE] [--master-key-id N] [--previous-master-key ID:FILE]";

fn die(m: &str) -> ! {
    eprintln!("{m}");
    std::process::exit(1);
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        die(USAGE);
    }
    let cmd = args.remove(0);
    let mut o: std::collections::HashMap<String, String> = Default::default();
    let mut flags = std::collections::HashSet::new();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--pg-allow-plaintext" | "--insecure-http" => {
                flags.insert(a);
            }
            "--pg" | "--pg-ca" | "--policy" | "--master-key-file" | "--master-key-id" | "--previous-master-key" | "--admin" | "--control-schema" | "--cert" | "--key" | "--listen" | "--frontend" => {
                let v = it.next().unwrap_or_else(|| die(&format!("{a} needs a value")));
                o.insert(a, v);
            }
            _ => die(&format!("unknown argument {a}\n{USAGE}")),
        }
    }
    let url = o.get("--pg").cloned().or_else(|| std::env::var("NEXGRAPH_PG_URL").ok()).unwrap_or_else(|| die("--pg URL (or $NEXGRAPH_PG_URL) is required"));
    let require_tls = !flags.contains("--pg-allow-plaintext");
    if !require_tls {
        eprintln!("WARNING: connecting to PostgreSQL without TLS; credentials and tenant data cross the network unencrypted");
    }
    let ca = o.get("--pg-ca").map(PathBuf::from);
    let mut co = ControlOptions { require_tls, ..Default::default() };
    co.tls.ca_file = ca.clone();
    if let Some(s) = o.get("--control-schema") {
        co.schema = s.clone();
    }
    let cp = Arc::new(ControlPlane::open(&url, co).unwrap_or_else(|e| die(&format!("cannot open the control plane: {e}"))));
    match cmd.as_str() {
        "init" => {
            let name = o.get("--admin").cloned().unwrap_or_else(|| "admin".into());
            let pw = std::env::var("NEXGRAPH_ADMIN_PASSWORD").ok().unwrap_or_else(|| {
                let mut l = String::new();
                std::io::stdin().lock().read_line(&mut l).ok();
                l.trim_end_matches(['\r', '\n']).to_string()
            });
            match cp.bootstrap_platform_admin(&name, &pw) {
                Ok(()) => println!("platform administrator {name:?} created"),
                Err(e) => die(&format!("init failed: {e}")),
            }
        }
        "serve" => {
            let listen: SocketAddr = o.get("--listen").map(|s| s.as_str()).unwrap_or("127.0.0.1:8443").parse().unwrap_or_else(|e| die(&format!("bad --listen: {e}")));
            let tls = match (o.get("--cert"), o.get("--key")) {
                (Some(c), Some(k)) => Some((PathBuf::from(c), PathBuf::from(k))),
                (None, None) => None,
                _ => die("--cert and --key go together"),
            };
            if tls.is_none() {
                if !flags.contains("--insecure-http") {
                    die("refusing to serve without TLS: pass --cert/--key (or --insecure-http for loopback development)");
                }
                if !listen.ip().is_loopback() {
                    die("--insecure-http is only allowed on loopback addresses");
                }
                eprintln!("WARNING: serving plain HTTP on loopback; tokens and data are not encrypted");
            }
            let pg = PgOptions { require_tls, tls: nexgraph::PgTls { ca_file: ca, ..Default::default() }, ..Default::default() };
            let read_key = |path: &str| -> nexgraph::crypto::Secret {
                let t = std::fs::read_to_string(path).unwrap_or_else(|e| die(&format!("cannot read key file {path}: {e}")));
                let t = t.trim();
                if t.len() < 64 || t.len() % 2 != 0 || !t.bytes().all(|b| b.is_ascii_hexdigit()) {
                    die(&format!("{path}: a master key is at least 32 bytes written as 64+ hexadecimal digits"));
                }
                nexgraph::crypto::Secret((0..t.len()).step_by(2).map(|i| u8::from_str_radix(&t[i..i + 2], 16).unwrap()).collect())
            };
            let mut mo = ManagerOptions { pg, ..Default::default() };
            if let Some(f) = o.get("--policy") {
                let text = std::fs::read_to_string(f).unwrap_or_else(|e| die(&format!("cannot read policy {f}: {e}")));
                mo.policy = nexgraph::security::Policy::from_json(&text).unwrap_or_else(|e| die(&format!("bad policy {f}: {e}")));
                println!("data policy loaded from {f}");
            }
            if let Some(f) = o.get("--master-key-file") {
                mo.master_key = Some(read_key(f));
                mo.master_key_id = o.get("--master-key-id").map(|s| s.parse().unwrap_or_else(|_| die("bad --master-key-id"))).unwrap_or(1);
                if let Some(prev) = o.get("--previous-master-key") {
                    let (id, file) = prev.split_once(':').unwrap_or_else(|| die("--previous-master-key takes ID:FILE"));
                    mo.previous_master_keys.push((id.parse().unwrap_or_else(|_| die("bad key id")), read_key(file)));
                }
                println!("encryption at rest on (key id {}); the relational mirror is disabled for tenant databases", mo.master_key_id);
            } else if o.contains_key("--master-key-id") || o.contains_key("--previous-master-key") {
                die("--master-key-id / --previous-master-key need --master-key-file");
            }
            let frontend = o.get("--frontend").cloned().or_else(|| std::env::var("NEXGRAPH_FRONTEND_DIR").ok()).map(PathBuf::from);
            if let Some(d) = &frontend {
                if !d.join("index.html").is_file() {
                    die(&format!("--frontend {}: no index.html there (build the UI first: cd frontend && trunk build --release)", d.display()));
                }
                println!("web UI served from {}", d.display());
            }
            let cfg = ServerConfig { frontend_dir: frontend, ..ServerConfig::default() };
            let mgr = Arc::new(TenantManager::new(cp, &url, mo));
            let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
            rt.block_on(async move {
                let tls_cfg = match tls {
                    Some((c, k)) => Some(server::tls_config(&c, &k).await.unwrap_or_else(|e| die(&format!("cannot load certificate/key: {e}")))),
                    None => None,
                };
                let listener = TcpListener::bind(listen).unwrap_or_else(|e| die(&format!("cannot bind {listen}: {e}")));
                println!("nexgraph-server listening on {}://{}", if tls_cfg.is_some() { "https" } else { "http" }, listener.local_addr().unwrap());
                let handle = axum_server::Handle::new();
                let h2 = handle.clone();
                tokio::spawn(async move {
                    let _ = tokio::signal::ctrl_c().await;
                    h2.graceful_shutdown(Some(Duration::from_secs(10)));
                });
                if let Err(e) = server::serve_on(listener, server::router(mgr, cfg), tls_cfg, handle).await {
                    die(&format!("server error: {e}"));
                }
            });
        }
        _ => die(USAGE),
    }
}
