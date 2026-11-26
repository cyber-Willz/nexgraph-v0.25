//! HTTPS JSON API over [`TenantManager`] (feature `server`).
//!
//! ```text
//! POST /v1/login            {tenant?, user, password}      -> {token, expires_in}
//! POST /v1/logout           (Bearer)
//! GET  /v1/me               (Bearer)                       -> principal
//! POST /v1/me/password      {old, new}
//! POST /v1/query            {query, params?}               -> {columns, rows, stats, ts}
//! POST /v1/checkpoint       (tenant_admin)
//! POST /v1/admin/tenants    {name, max_entities?, max_result_rows?}   (platform_admin)
//! POST /v1/admin/tenants/{name}/disable | /enable                      (platform_admin)
//! DELETE /v1/admin/tenants/{name}                                      (platform_admin, tenant disabled)
//! POST /v1/admin/users      {tenant?, name, password, role}
//! POST /v1/admin/users/disable        {tenant?, name, disabled}
//! POST /v1/admin/users/reset-password {tenant?, name, new_password}
//! GET  /v1/audit?limit=&tenant=
//! GET  /v1/bi/tables                [?as_of=ts]            -> catalog: tables, columns, types, row counts   (Bearer or Basic)
//! GET  /v1/bi/tables/{name}         [?format=csv|json&as_of=ts]  -> one flat table (see `bi`)                   (Bearer or Basic)
//! GET|POST /v1/bi/query             q=NQL | {query, params}  [?format=csv|json]  read-only statements only     (Bearer or Basic)
//! GET  /v1/bi/powerquery            [?base=https://host:port]  -> Power Query (M) script for Power BI          (no auth)
//! GET  /v1/stats                                          -> {nodes, edges, versions, prop_indexes, vector_indexes}   (tenant accounts)
//! GET  /v1/admin/tenants                                                (platform_admin)
//! GET  /v1/admin/users      [?tenant=]                                  (tenant_admin / platform_admin)
//! GET  /healthz
//! ```
//! Every other path is 404 — unless [`ServerConfig::frontend_dir`] is set, in which case unknown `GET`/`HEAD` paths
//! outside `/v1/` are served from that directory (the web UI in `frontend/`, built with `trunk build --release`),
//! with `index.html` as the single-page-app fallback. Errors are `{"error": "..."}`; internal details are logged, never sent.

use crate::error::Error;
use crate::query::Params;
use crate::security::{Principal, Quotas, Role, Session, TenantManager};
use crate::value::Value;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{middleware, Json, Router};
use serde_json::{json, Value as J};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::{Component, Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// Maximum request body.
    pub max_body_bytes: usize,
    /// Responses are 504 after this long (the engine call itself cannot be cancelled).
    pub request_timeout: Duration,
    /// Engine/control-plane calls running at once; the rest wait.
    pub max_concurrent: usize,
    /// Login attempts per client IP per minute (credential-stuffing brake; per-account lockout is separate).
    pub logins_per_ip_per_minute: u32,
    /// Directory holding the built web UI (`frontend/dist`). `None` (the default) serves no UI: every non-API path is 404.
    pub frontend_dir: Option<PathBuf>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig { max_body_bytes: 1 << 20, request_timeout: Duration::from_secs(30), max_concurrent: 16, logins_per_ip_per_minute: 20, frontend_dir: None }
    }
}

struct App {
    mgr: Arc<TenantManager>,
    cfg: ServerConfig,
    sem: Semaphore,
    logins: Mutex<HashMap<IpAddr, (Instant, u32)>>,
    /// HTTP Basic credentials (hashed) -> issued token, so BI refreshes do not run Argon2id on every request.
    basic: Mutex<HashMap<String, (String, Instant)>>,
}

/// Longest a Basic credential's token is reused (shorter than the token's own lifetime; a changed or disabled
/// account stops working within this time even through the cache, and a disabled user is rejected immediately
/// because the token itself is re-checked on every request).
const BASIC_CACHE_TTL: Duration = Duration::from_secs(300);
const BASIC_CACHE_MAX: usize = 1024;

impl App {
    /// Count one login attempt for `ip`; false when the per-minute budget is spent.
    fn login_allowed(&self, ip: IpAddr) -> bool {
        let mut g = self.logins.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        g.retain(|_, (t, _)| now.duration_since(*t) < Duration::from_secs(60));
        let e = g.entry(ip).or_insert((now, 0));
        e.1 += 1;
        e.1 <= self.cfg.logins_per_ip_per_minute
    }
}

type St = State<Arc<App>>;

struct ApiError(Error);

impl From<Error> for ApiError {
    fn from(e: Error) -> Self {
        ApiError(e)
    }
}

fn reply(code: StatusCode, body: J) -> Response {
    (code, Json(body)).into_response()
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self.0 {
            Error::Auth(_) => {
                let mut r = reply(StatusCode::UNAUTHORIZED, json!({"error": "invalid credentials"}));
                r.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
                // BI tools (Power BI) answer a Basic challenge; the first header stays Bearer for API clients.
                r.headers_mut().append(header::WWW_AUTHENTICATE, HeaderValue::from_static("Basic realm=\"nexgraph\", charset=\"UTF-8\""));
                r
            }
            Error::Denied(m) => reply(StatusCode::FORBIDDEN, json!({"error": m})),
            Error::Parse { .. } | Error::Query(_) | Error::Invalid(_) => reply(StatusCode::BAD_REQUEST, json!({"error": self.0.to_string()})),
            other => {
                eprintln!("nexgraph-server: internal error: {other}");
                reply(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": "internal error"}))
            }
        }
    }
}

type Res = Result<Response, ApiError>;

fn to_json(v: &Value) -> J {
    match v {
        Value::Null => J::Null,
        Value::Bool(b) => J::Bool(*b),
        Value::Int(i) => json!(i),
        Value::Float(f) => serde_json::Number::from_f64(*f).map(J::Number).unwrap_or_else(|| J::String(f.to_string())),
        Value::Str(s) => J::String(s.clone()),
        Value::List(l) => J::Array(l.iter().map(to_json).collect()),
        Value::Vector(v) => J::Array(v.iter().map(|x| to_json(&Value::Float(*x as f64))).collect()),
        Value::Node(id) => json!({"$node": id}),
        Value::Edge(id) => json!({"$edge": id}),
    }
}

fn from_json(j: &J) -> Result<Value, Error> {
    Ok(match j {
        J::Null => Value::Null,
        J::Bool(b) => Value::Bool(*b),
        J::Number(n) => match n.as_i64() {
            Some(i) => Value::Int(i),
            None => Value::Float(n.as_f64().ok_or_else(|| Error::Invalid("number out of range".into()))?),
        },
        J::String(s) => Value::Str(s.clone()),
        J::Array(a) => Value::List(a.iter().map(from_json).collect::<Result<_, _>>()?),
        J::Object(_) => return Err(Error::Invalid("objects are not valid parameter values".into())),
    })
}

fn body<T: serde::de::DeserializeOwned>(b: &Bytes) -> Result<T, ApiError> {
    serde_json::from_slice(b).map_err(|e| Error::Invalid(format!("invalid JSON body: {e}")).into())
}

fn bearer(h: &HeaderMap) -> Result<String, ApiError> {
    let v = h.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).ok_or_else(|| Error::Auth("missing token".into()))?;
    match v.strip_prefix("Bearer ") {
        Some(t) if !t.is_empty() => Ok(t.trim().to_string()),
        _ => Err(Error::Auth("malformed Authorization header".into()).into()),
    }
}

async fn blocking<T: Send + 'static>(app: &Arc<App>, f: impl FnOnce() -> Result<T, Error> + Send + 'static) -> Result<T, ApiError> {
    let _permit = app.sem.acquire().await.map_err(|_| Error::Backend("server is shutting down".into()))?;
    match tokio::time::timeout(app.cfg.request_timeout, tokio::task::spawn_blocking(f)).await {
        Err(_) => Err(Error::Backend("request timed out".into()).into()),
        Ok(Err(e)) => Err(Error::Backend(format!("worker failed: {e}")).into()),
        Ok(Ok(r)) => r.map_err(Into::into),
    }
}

async fn principal(app: &Arc<App>, h: &HeaderMap) -> Result<(String, Principal), ApiError> {
    let token = bearer(h)?;
    let (cp, t) = (app.mgr.control_plane().clone(), token.clone());
    let p = blocking(app, move || cp.authenticate(&t)).await?;
    Ok((token, p))
}

fn p_json(p: &Principal) -> J {
    json!({"user": p.user, "tenant": p.tenant, "role": p.role.as_str()})
}

// ------------------------------------------------------------------ handlers

async fn healthz() -> &'static str {
    "ok"
}

#[derive(serde::Deserialize)]
struct LoginReq {
    tenant: Option<String>,
    user: String,
    password: String,
}

async fn login(State(app): St, ConnectInfo(addr): ConnectInfo<SocketAddr>, b: Bytes) -> Res {
    if !app.login_allowed(addr.ip()) {
        let mut r = reply(StatusCode::TOO_MANY_REQUESTS, json!({"error": "too many login attempts"}));
        r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static("60"));
        return Ok(r);
    }
    let r: LoginReq = body(&b)?;
    let cp = app.mgr.control_plane().clone();
    let t = blocking(&app, move || cp.login(r.tenant.as_deref().filter(|t| !t.is_empty()), &r.user, &r.password)).await?;
    Ok(reply(StatusCode::OK, json!({"token": t.token, "expires_in": t.expires_in_secs})))
}

async fn logout(State(app): St, h: HeaderMap) -> Res {
    let (token, _) = principal(&app, &h).await?;
    let cp = app.mgr.control_plane().clone();
    blocking(&app, move || cp.logout(&token)).await?;
    Ok(reply(StatusCode::OK, json!({"ok": true})))
}

async fn me(State(app): St, h: HeaderMap) -> Res {
    let (_, p) = principal(&app, &h).await?;
    Ok(reply(StatusCode::OK, p_json(&p)))
}

#[derive(serde::Deserialize)]
struct PwReq {
    old: String,
    new: String,
}

async fn change_password(State(app): St, h: HeaderMap, b: Bytes) -> Res {
    let (_, p) = principal(&app, &h).await?;
    let r: PwReq = body(&b)?;
    let cp = app.mgr.control_plane().clone();
    blocking(&app, move || cp.change_password(&p, &r.old, &r.new)).await?;
    Ok(reply(StatusCode::OK, json!({"ok": true})))
}

#[derive(serde::Deserialize)]
struct QueryReq {
    query: String,
    #[serde(default)]
    params: HashMap<String, J>,
}

async fn query(State(app): St, h: HeaderMap, b: Bytes) -> Res {
    let token = bearer(&h)?;
    let r: QueryReq = body(&b)?;
    let params: Params = r.params.iter().map(|(k, v)| from_json(v).map(|v| (k.clone(), v))).collect::<Result<_, _>>()?;
    let mgr = app.mgr.clone();
    let out = blocking(&app, move || mgr.session(&token)?.query(&r.query, &params)).await?;
    let s = &out.stats;
    Ok(reply(
        StatusCode::OK,
        json!({
            "columns": out.columns,
            "rows": out.rows.iter().map(|r| r.iter().map(to_json).collect::<Vec<_>>()).collect::<Vec<_>>(),
            "stats": {"nodes_created": s.nodes_created, "edges_created": s.edges_created, "props_set": s.props_set, "deleted": s.deleted, "indexes_created": s.indexes_created},
            "ts": out.ts,
        }),
    ))
}

async fn checkpoint(State(app): St, h: HeaderMap) -> Res {
    let token = bearer(&h)?;
    let mgr = app.mgr.clone();
    let ts = blocking(&app, move || mgr.session(&token)?.checkpoint()).await?;
    Ok(reply(StatusCode::OK, json!({"ts": ts})))
}

#[derive(serde::Deserialize)]
struct TenantReq {
    name: String,
    max_entities: Option<u64>,
    max_result_rows: Option<u64>,
}

async fn create_tenant(State(app): St, h: HeaderMap, b: Bytes) -> Res {
    let (_, p) = principal(&app, &h).await?;
    let r: TenantReq = body(&b)?;
    let cp = app.mgr.control_plane().clone();
    let t = blocking(&app, move || cp.create_tenant(&p, &r.name, Quotas { max_entities: r.max_entities, max_result_rows: r.max_result_rows })).await?;
    Ok(reply(StatusCode::CREATED, json!({"id": t.id, "name": t.name})))
}

async fn tenant_state(State(app): St, h: HeaderMap, Path((name, action)): Path<(String, String)>) -> Res {
    let (_, p) = principal(&app, &h).await?;
    let disabled = match action.as_str() {
        "disable" => true,
        "enable" => false,
        _ => return Ok(reply(StatusCode::NOT_FOUND, json!({"error": "not found"}))),
    };
    let cp = app.mgr.control_plane().clone();
    blocking(&app, move || cp.set_tenant_disabled(&p, &name, disabled)).await?;
    Ok(reply(StatusCode::OK, json!({"ok": true})))
}

async fn purge_tenant(State(app): St, h: HeaderMap, Path(name): Path<String>) -> Res {
    let (_, p) = principal(&app, &h).await?;
    let mgr = app.mgr.clone();
    blocking(&app, move || mgr.purge_tenant(&p, &name)).await?;
    Ok(reply(StatusCode::OK, json!({"ok": true})))
}

#[derive(serde::Deserialize)]
struct UserReq {
    tenant: Option<String>,
    name: String,
    password: String,
    role: String,
}

async fn create_user(State(app): St, h: HeaderMap, b: Bytes) -> Res {
    let (_, p) = principal(&app, &h).await?;
    let r: UserReq = body(&b)?;
    let role = Role::parse(&r.role)?;
    let cp = app.mgr.control_plane().clone();
    blocking(&app, move || cp.create_user(&p, r.tenant.as_deref().filter(|t| !t.is_empty()), &r.name, &r.password, role)).await?;
    Ok(reply(StatusCode::CREATED, json!({"ok": true})))
}

#[derive(serde::Deserialize)]
struct DisableReq {
    tenant: Option<String>,
    name: String,
    disabled: bool,
}

async fn user_disable(State(app): St, h: HeaderMap, b: Bytes) -> Res {
    let (_, p) = principal(&app, &h).await?;
    let r: DisableReq = body(&b)?;
    let cp = app.mgr.control_plane().clone();
    blocking(&app, move || cp.set_user_disabled(&p, r.tenant.as_deref().filter(|t| !t.is_empty()), &r.name, r.disabled)).await?;
    Ok(reply(StatusCode::OK, json!({"ok": true})))
}

#[derive(serde::Deserialize)]
struct ResetReq {
    tenant: Option<String>,
    name: String,
    new_password: String,
}

async fn reset_password(State(app): St, h: HeaderMap, b: Bytes) -> Res {
    let (_, p) = principal(&app, &h).await?;
    let r: ResetReq = body(&b)?;
    let cp = app.mgr.control_plane().clone();
    blocking(&app, move || cp.reset_password(&p, r.tenant.as_deref().filter(|t| !t.is_empty()), &r.name, &r.new_password)).await?;
    Ok(reply(StatusCode::OK, json!({"ok": true})))
}

#[derive(serde::Deserialize)]
struct AuditQ {
    limit: Option<i64>,
    tenant: Option<String>,
}

async fn audit(State(app): St, h: HeaderMap, Query(q): Query<AuditQ>) -> Res {
    let (_, p) = principal(&app, &h).await?;
    let cp = app.mgr.control_plane().clone();
    let rows = blocking(&app, move || cp.read_audit(&p, q.tenant.as_deref(), q.limit.unwrap_or(100))).await?;
    let out: Vec<J> = rows.iter().map(|e| json!({"id": e.id, "ts": e.ts, "tenant_id": e.tenant_id, "actor": e.actor, "role": e.role, "action": e.action, "outcome": e.outcome, "detail": e.detail})).collect();
    Ok(reply(StatusCode::OK, json!({"entries": out})))
}

async fn stats(State(app): St, h: HeaderMap) -> Res {
    let token = bearer(&h)?;
    let mgr = app.mgr.clone();
    let s = blocking(&app, move || mgr.session(&token)?.stats()).await?;
    Ok(reply(StatusCode::OK, json!({"nodes": s.live_nodes, "edges": s.live_edges, "versions": s.versions, "prop_indexes": s.prop_indexes, "vector_indexes": s.vector_indexes})))
}

async fn list_tenants(State(app): St, h: HeaderMap) -> Res {
    let (_, p) = principal(&app, &h).await?;
    let cp = app.mgr.control_plane().clone();
    let rows = blocking(&app, move || cp.list_tenants(&p)).await?;
    let out: Vec<J> = rows.iter().map(|t| json!({"id": t.id, "name": t.name, "disabled": t.disabled, "max_entities": t.quotas.max_entities, "max_result_rows": t.quotas.max_result_rows})).collect();
    Ok(reply(StatusCode::OK, json!({"tenants": out})))
}

#[derive(serde::Deserialize)]
struct UsersQ {
    tenant: Option<String>,
}

async fn list_users(State(app): St, h: HeaderMap, Query(q): Query<UsersQ>) -> Res {
    let (_, p) = principal(&app, &h).await?;
    let cp = app.mgr.control_plane().clone();
    let rows = blocking(&app, move || cp.list_users(&p, q.tenant.as_deref().filter(|t| !t.is_empty()))).await?;
    let out: Vec<J> = rows.iter().map(|u| json!({"name": u.name, "role": u.role, "disabled": u.disabled, "tenant": u.tenant, "created_at": u.created_at, "last_login": u.last_login})).collect();
    Ok(reply(StatusCode::OK, json!({"users": out})))
}

// ------------------------------------------------------------------ web UI (optional static files)

fn b64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

/// `'sha256-...'` CSP sources for every inline `<script>` in `html` (trunk's loader is inline), so the UI works
/// under a policy that otherwise forbids inline script.
fn inline_script_hashes(html: &str) -> Vec<String> {
    use sha2::{Digest, Sha256};
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(i) = rest.find("<script") {
        let after = &rest[i..];
        let Some(gt) = after.find('>') else { break };
        let tag = &after[..gt];
        let body_start = gt + 1;
        let Some(end) = after[body_start..].find("</script>") else { break };
        let has_src = tag.split_whitespace().any(|a| a.starts_with("src=") || a.starts_with("src ="));
        let body = &after[body_start..body_start + end];
        if !has_src && !body.trim().is_empty() {
            out.push(format!("'sha256-{}'", b64_encode(&Sha256::digest(body.as_bytes()))));
        }
        rest = &after[body_start + end + "</script>".len()..];
    }
    out
}

fn ui_csp(script_hashes: &[String]) -> String {
    format!(
        "default-src 'none'; script-src 'self' 'wasm-unsafe-eval' {}; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
        script_hashes.join(" ")
    )
}

fn content_type_of(path: &FsPath) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("wasm") => "application/wasm",
        Some("css") => "text/css; charset=utf-8",
        Some("json") | Some("map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// A URL path as a safe relative file path: plain components only (no `..`, no absolute parts, no backslashes or NULs).
fn safe_rel_path(url_path: &str) -> Option<PathBuf> {
    let p = url_path.trim_start_matches('/');
    if p.contains(['\\', '\0']) {
        return None;
    }
    let mut out = PathBuf::new();
    for c in FsPath::new(p).components() {
        match c {
            Component::Normal(s) => out.push(s),
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(out)
}

/// Serve one UI file (or `index.html` for an extension-less path, the SPA fallback). Pure over the filesystem so it
/// can be tested without a database.
fn serve_static(dir: &FsPath, url_path: &str) -> Response {
    let nf = || reply(StatusCode::NOT_FOUND, json!({"error": "not found"}));
    if url_path == "/v1" || url_path.starts_with("/v1/") {
        return nf();
    }
    let Some(rel) = safe_rel_path(url_path) else { return nf() };
    let wants_index = rel.as_os_str().is_empty();
    let mut file = dir.join(if wants_index { FsPath::new("index.html") } else { rel.as_path() });
    let mut is_index = wants_index;
    // A real file wins; otherwise a path without an extension is a client-side route.
    if wants_index || !file.is_file() {
        if wants_index || rel.extension().is_none() {
            file = dir.join("index.html");
            is_index = true;
        } else {
            return nf();
        }
    }
    let Ok(bytes) = std::fs::read(&file) else { return nf() };
    let mut r = (StatusCode::OK, bytes.clone()).into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type_of(&file)));
    if is_index {
        let html = String::from_utf8_lossy(&bytes);
        if let Ok(v) = HeaderValue::from_str(&ui_csp(&inline_script_hashes(&html))) {
            h.insert(HeaderName::from_static("content-security-policy"), v);
        }
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    } else {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=3600"));
    }
    r
}

async fn fallback(State(app): St, uri: axum::http::Uri, method: axum::http::Method) -> Response {
    match (&app.cfg.frontend_dir, method == axum::http::Method::GET || method == axum::http::Method::HEAD) {
        (Some(dir), true) => {
            let (dir, path) = (dir.clone(), uri.path().to_string());
            tokio::task::spawn_blocking(move || serve_static(&dir, &path)).await.unwrap_or_else(|_| reply(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": "internal error"})))
        }
        _ => not_found().await,
    }
}

// ------------------------------------------------------------------ BI (Power BI and other flat-table consumers)

fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.trim_end_matches('=').bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// Where a token came from, so a stale cached one can be replaced once.
enum Cred {
    Bearer(String),
    /// (cache key, token, came from the cache)
    Basic(String, String, bool),
}

impl Cred {
    fn token(&self) -> &str {
        match self {
            Cred::Bearer(t) | Cred::Basic(_, t, _) => t,
        }
    }
}

/// `Authorization: Bearer <token>` or `Basic base64(user:password)` where user is `tenant/user`.
/// Basic exists because Power BI's scheduled refresh can only send static credentials.
async fn credentials(app: &Arc<App>, h: &HeaderMap, ip: IpAddr, fresh: bool) -> Result<Cred, ApiError> {
    let v = h.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).ok_or_else(|| Error::Auth("missing credentials".into()))?;
    if let Some(t) = v.strip_prefix("Bearer ") {
        return if t.trim().is_empty() { Err(Error::Auth("malformed Authorization header".into()).into()) } else { Ok(Cred::Bearer(t.trim().to_string())) };
    }
    let Some(b64) = v.strip_prefix("Basic ") else { return Err(Error::Auth("unsupported Authorization scheme".into()).into()) };
    let key = {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(v.as_bytes()))
    };
    if !fresh {
        let mut g = app.basic.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((tok, exp)) = g.get(&key) {
            if *exp > Instant::now() {
                return Ok(Cred::Basic(key, tok.clone(), true));
            }
        }
        g.remove(&key);
    }
    let raw = b64_decode(b64.trim()).and_then(|b| String::from_utf8(b).ok()).ok_or_else(|| Error::Auth("malformed Basic credentials".into()))?;
    let (user, pass) = raw.split_once(':').ok_or_else(|| Error::Auth("malformed Basic credentials".into()))?;
    let (tenant, user) = match user.split_once('/') {
        Some((t, u)) => (Some(t.to_string()), u.to_string()),
        None => (None, user.to_string()),
    };
    if !app.login_allowed(ip) {
        return Err(Error::Auth("too many login attempts".into()).into());
    }
    let (cp, pass) = (app.mgr.control_plane().clone(), pass.to_string());
    let t = blocking(app, move || cp.login(tenant.as_deref().filter(|t| !t.is_empty()), &user, &pass)).await?;
    let ttl = Duration::from_secs(t.expires_in_secs).min(BASIC_CACHE_TTL);
    {
        let mut g = app.basic.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        g.retain(|_, (_, e)| *e > now);
        if g.len() >= BASIC_CACHE_MAX {
            g.clear();
        }
        g.insert(key.clone(), (t.token.clone(), now + ttl));
    }
    Ok(Cred::Basic(key, t.token, false))
}

/// Run `f` as the authenticated user. A cached Basic token that the server no longer accepts is replaced once.
async fn with_session<T: Send + 'static>(app: &Arc<App>, h: &HeaderMap, ip: IpAddr, f: impl Fn(&Session) -> Result<T, Error> + Send + Sync + 'static) -> Result<T, ApiError> {
    let f = Arc::new(f);
    let mut fresh = false;
    loop {
        let cred = credentials(app, h, ip, fresh).await?;
        let (mgr, tok, f2) = (app.mgr.clone(), cred.token().to_string(), f.clone());
        match blocking(app, move || f2(&mgr.session(&tok)?)).await {
            Err(ApiError(Error::Auth(_))) => {
                if let Cred::Basic(key, _, true) = cred {
                    app.basic.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
                    if !fresh {
                        fresh = true;
                        continue;
                    }
                }
                return Err(Error::Auth("invalid credentials".into()).into());
            }
            other => return other,
        }
    }
}

#[derive(serde::Deserialize)]
struct BiParams {
    format: Option<String>,
    as_of: Option<u64>,
    q: Option<String>,
}

#[derive(Clone, Copy)]
enum Fmt {
    Csv,
    Json,
}

fn fmt_of(f: &Option<String>) -> Result<Fmt, ApiError> {
    match f.as_deref() {
        None | Some("csv") => Ok(Fmt::Csv),
        Some("json") => Ok(Fmt::Json),
        Some(o) => Err(Error::Invalid(format!("format must be csv or json, not {o:?}")).into()),
    }
}

fn table_reply(body: String, f: Fmt, ts: u64) -> Response {
    let ct = match f {
        Fmt::Csv => "text/csv; charset=utf-8",
        Fmt::Json => "application/json",
    };
    let mut r = (StatusCode::OK, body).into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(ct));
    r.headers_mut().insert(HeaderName::from_static("x-nexgraph-ts"), HeaderValue::from(ts));
    r
}

async fn bi_catalog(State(app): St, ConnectInfo(addr): ConnectInfo<SocketAddr>, h: HeaderMap, Query(p): Query<BiParams>) -> Res {
    let as_of = p.as_of;
    let (tables, ts) = with_session(&app, &h, addr.ip(), move |s| s.bi_tables(as_of)).await?;
    Ok(table_reply(crate::bi::catalog_json(&tables, ts), Fmt::Json, ts))
}

async fn bi_table(State(app): St, ConnectInfo(addr): ConnectInfo<SocketAddr>, h: HeaderMap, Path(name): Path<String>, Query(p): Query<BiParams>) -> Res {
    let f = fmt_of(&p.format)?;
    let as_of = p.as_of;
    let (t, ts) = with_session(&app, &h, addr.ip(), move |s| s.bi_table(&name, as_of)).await?;
    let r = t.to_result(ts);
    Ok(table_reply(
        match f {
            Fmt::Csv => r.to_csv(),
            Fmt::Json => r.to_json(),
        },
        f,
        ts,
    ))
}

async fn bi_run(app: &Arc<App>, addr: SocketAddr, h: &HeaderMap, text: String, params: Params, f: Fmt) -> Res {
    let out = with_session(app, h, addr.ip(), move |s| s.query_read_only(&text, &params)).await?;
    Ok(table_reply(
        match f {
            Fmt::Csv => out.to_csv(),
            Fmt::Json => out.to_json(),
        },
        f,
        out.ts,
    ))
}

async fn bi_query_get(State(app): St, ConnectInfo(addr): ConnectInfo<SocketAddr>, h: HeaderMap, Query(p): Query<BiParams>) -> Res {
    let f = fmt_of(&p.format)?;
    let q = p.q.ok_or_else(|| Error::Invalid("missing q (the NQL statement)".into()))?;
    bi_run(&app, addr, &h, q, Params::new(), f).await
}

async fn bi_query_post(State(app): St, ConnectInfo(addr): ConnectInfo<SocketAddr>, h: HeaderMap, Query(p): Query<BiParams>, b: Bytes) -> Res {
    let f = fmt_of(&p.format)?;
    let r: QueryReq = body(&b)?;
    let params: Params = r.params.iter().map(|(k, v)| from_json(v).map(|v| (k.clone(), v))).collect::<Result<_, _>>()?;
    bi_run(&app, addr, &h, r.query, params, f).await
}

#[derive(serde::Deserialize)]
struct PqParams {
    base: Option<String>,
}

/// The Power Query (M) script, with this server's address filled in. No data and no secrets: no auth required.
async fn bi_powerquery(h: HeaderMap, Query(p): Query<PqParams>) -> Response {
    let ok = |s: &str| !s.is_empty() && s.len() <= 200 && s.chars().all(|c| c.is_ascii_alphanumeric() || ".-:/_[]".contains(c));
    let base = match p.base {
        Some(b) if ok(&b) => b,
        Some(_) => return reply(StatusCode::BAD_REQUEST, json!({"error": "invalid base"})),
        None => match h.get(header::HOST).and_then(|v| v.to_str().ok()).filter(|v| ok(v)) {
            Some(host) => format!("https://{host}"),
            None => "https://localhost:8443".to_string(),
        },
    };
    let mut r = (StatusCode::OK, crate::bi::power_query(&base)).into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    r
}

async fn not_found() -> Response {
    reply(StatusCode::NOT_FOUND, json!({"error": "not found"}))
}

async fn security_headers(mut r: Response) -> Response {
    let h = r.headers_mut();
    for (k, v) in [
        ("strict-transport-security", "max-age=63072000; includeSubDomains"),
        ("x-content-type-options", "nosniff"),
        ("cache-control", "no-store"),
        ("content-security-policy", "default-src 'none'"),
        ("x-frame-options", "DENY"),
        ("referrer-policy", "no-referrer"),
    ] {
        // the UI's index.html carries its own (hash-extended) policy
        if !h.contains_key(k) {
            h.insert(HeaderName::from_static(k), HeaderValue::from_static(v));
        }
    }
    r
}

pub fn router(mgr: Arc<TenantManager>, cfg: ServerConfig) -> Router {
    let limit = cfg.max_body_bytes;
    let app = Arc::new(App { sem: Semaphore::new(cfg.max_concurrent.max(1)), logins: Mutex::new(HashMap::new()), basic: Mutex::new(HashMap::new()), mgr, cfg });
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/login", post(login))
        .route("/v1/logout", post(logout))
        .route("/v1/me", get(me))
        .route("/v1/me/password", post(change_password))
        .route("/v1/query", post(query))
        .route("/v1/checkpoint", post(checkpoint))
        .route("/v1/stats", get(stats))
        .route("/v1/audit", get(audit))
        .route("/v1/bi/tables", get(bi_catalog))
        .route("/v1/bi/tables/{name}", get(bi_table))
        .route("/v1/bi/query", get(bi_query_get).post(bi_query_post))
        .route("/v1/bi/powerquery", get(bi_powerquery))
        .route("/v1/admin/tenants", get(list_tenants).post(create_tenant))
        .route("/v1/admin/tenants/{name}", delete(purge_tenant))
        .route("/v1/admin/tenants/{name}/{action}", post(tenant_state))
        .route("/v1/admin/users", get(list_users).post(create_user))
        .route("/v1/admin/users/disable", post(user_disable))
        .route("/v1/admin/users/reset-password", post(reset_password))
        .fallback(fallback)
        .layer(DefaultBodyLimit::max(limit))
        .layer(middleware::map_response(security_headers))
        .with_state(app)
}

/// TLS settings for [`serve_on`]. `None` serves plain HTTP (the caller must have checked that this is acceptable).
pub async fn tls_config(cert_pem: &std::path::Path, key_pem: &std::path::Path) -> std::io::Result<axum_server::tls_rustls::RustlsConfig> {
    // A process-wide crypto provider is required by rustls; ignore "already installed".
    let _ = rustls::crypto::ring::default_provider().install_default();
    axum_server::tls_rustls::RustlsConfig::from_pem_file(cert_pem, key_pem).await
}

/// Serve on an already-bound listener until `handle` is told to shut down.
pub async fn serve_on(listener: std::net::TcpListener, router: Router, tls: Option<axum_server::tls_rustls::RustlsConfig>, handle: axum_server::Handle) -> std::io::Result<()> {
    listener.set_nonblocking(true)?;
    let svc = router.into_make_service_with_connect_info::<SocketAddr>();
    match tls {
        Some(cfg) => axum_server::from_tcp_rustls(listener, cfg).handle(handle).serve(svc).await,
        None => axum_server::from_tcp(listener).handle(handle).serve(svc).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nexgraph_ui_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn status(r: &Response) -> u16 {
        r.status().as_u16()
    }

    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (i, o) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")] {
            assert_eq!(b64_encode(i.as_bytes()), o);
        }
        assert_eq!(b64_decode(&b64_encode(b"nexgraph\x00\xff")).unwrap(), b"nexgraph\x00\xff");
    }

    #[test]
    fn inline_script_hash_is_sha256_of_the_body() {
        let html = r#"<html><head><script src="/a.js"></script><script type="module">alert(1)</script></head></html>"#;
        let h = inline_script_hashes(html);
        // independently computed with Python hashlib/base64
        assert_eq!(h, vec!["'sha256-bhHHL3z2vDgxUt0W3dWQOrprscmda2Y5pLsLg4GF+pI='".to_string()], "external scripts are not hashed");
    }

    #[test]
    fn paths_cannot_escape_the_directory() {
        assert_eq!(safe_rel_path("/app.js"), Some(PathBuf::from("app.js")));
        assert_eq!(safe_rel_path("/"), Some(PathBuf::new()));
        for bad in ["/../secret", "/a/../../secret", "/a\\b", "/a%00b".replace("%00", "\0").as_str(), "//etc/passwd/.."] {
            assert!(safe_rel_path(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn serves_files_spa_fallback_and_refuses_the_rest() {
        let d = tmpdir("static");
        std::fs::write(d.join("index.html"), r#"<html><script type="module">boot()</script></html>"#).unwrap();
        std::fs::write(d.join("app_bg.wasm"), [0u8, 0x61, 0x73, 0x6d]).unwrap();
        std::fs::write(d.parent().unwrap().join(format!("nexgraph_secret_{}", std::process::id())), "secret").unwrap();

        let r = serve_static(&d, "/");
        assert_eq!(status(&r), 200);
        let csp = r.headers()["content-security-policy"].to_str().unwrap().to_string();
        assert!(csp.contains("'sha256-") && csp.contains("'wasm-unsafe-eval'") && csp.contains("frame-ancestors 'none'"), "{csp}");
        assert_eq!(r.headers()["cache-control"], "no-store");

        // client-side route -> index.html
        let r = serve_static(&d, "/query/history");
        assert_eq!(status(&r), 200);
        assert_eq!(r.headers()["content-type"], "text/html; charset=utf-8");

        // asset
        let r = serve_static(&d, "/app_bg.wasm");
        assert_eq!(status(&r), 200);
        assert_eq!(r.headers()["content-type"], "application/wasm");
        assert!(!r.headers().contains_key("content-security-policy"), "assets use the middleware default");

        // missing asset (has an extension) is a real 404, not index.html
        assert_eq!(status(&serve_static(&d, "/missing.js")), 404);
        // API namespace never falls back to the UI
        assert_eq!(status(&serve_static(&d, "/v1/nope")), 404);
        assert_eq!(status(&serve_static(&d, "/v1")), 404);
        // traversal
        assert_eq!(status(&serve_static(&d, &format!("/../nexgraph_secret_{}", std::process::id()))), 404);
        let _ = std::fs::remove_dir_all(&d);
    }
}
