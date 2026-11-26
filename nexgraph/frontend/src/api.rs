//! JSON client for nexgraph-server (`/v1/...`). The bearer token lives in localStorage.
use gloo_net::http::Request;
use gloo_storage::{LocalStorage, Storage};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

const TOKEN_KEY: &str = "nexgraph.token";
const USER_KEY: &str = "nexgraph.user";

#[derive(Clone, Debug, PartialEq)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
}
impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

/// `GET /v1/me`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct User {
    pub user: String,
    #[serde(default)]
    pub tenant: Option<String>,
    pub role: String,
}

pub fn token() -> Option<String> {
    LocalStorage::get::<String>(TOKEN_KEY).ok()
}
pub fn stored_user() -> Option<User> {
    token()?;
    LocalStorage::get::<User>(USER_KEY).ok()
}
pub fn save_session(token: &str, user: &User) {
    let _ = LocalStorage::set(TOKEN_KEY, token);
    let _ = LocalStorage::set(USER_KEY, user);
}
pub fn clear_session() {
    LocalStorage::delete(TOKEN_KEY);
    LocalStorage::delete(USER_KEY);
}

/// Pull `{ "error": "..." }` out of a failed response (falls back to the status line).
pub fn error_message(status: u16, body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
        .unwrap_or_else(|| match status {
            401 => "Your session has expired. Please sign in again.".into(),
            403 => "Your role is not allowed to do that.".into(),
            404 => "Not found.".into(),
            429 => "Too many attempts. Wait a minute and try again.".into(),
            504 => "The request timed out.".into(),
            _ => format!("The request failed ({status})."),
        })
}

async fn send_raw(method: &str, path: &str, body: Option<&Value>) -> Result<String, ApiError> {
    let mut rb = match method {
        "POST" => Request::post(path),
        "DELETE" => Request::delete(path),
        _ => Request::get(path),
    };
    if let Some(t) = token() {
        rb = rb.header("Authorization", &format!("Bearer {t}"));
    }
    let resp = match body {
        Some(b) => rb.header("Content-Type", "application/json").body(b.to_string()).map_err(|e| ApiError { status: 0, message: e.to_string() })?.send().await,
        None => rb.send().await,
    }
    .map_err(|e| ApiError { status: 0, message: format!("Cannot reach the server: {e}") })?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if status == 401 && path != "/v1/login" {
        clear_session();
        if let Some(w) = web_sys::window() {
            let _ = w.location().reload();
        }
    }
    if !(200..300).contains(&status) {
        return Err(ApiError { status, message: error_message(status, &text) });
    }
    Ok(text)
}

fn parse(text: String, status_hint: u16) -> Result<Value, ApiError> {
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text).map_err(|e| ApiError { status: status_hint, message: format!("Unexpected response: {e}") })
}

pub async fn get(path: &str) -> Result<Value, ApiError> {
    parse(send_raw("GET", path, None).await?, 200)
}
pub async fn get_text(path: &str) -> Result<String, ApiError> {
    send_raw("GET", path, None).await
}
pub async fn post(path: &str, body: &Value) -> Result<Value, ApiError> {
    parse(send_raw("POST", path, Some(body)).await?, 200)
}
pub async fn delete(path: &str) -> Result<Value, ApiError> {
    parse(send_raw("DELETE", path, None).await?, 200)
}

pub async fn login(tenant: &str, user: &str, password: &str) -> Result<(String, User), ApiError> {
    let mut body = Map::new();
    if !tenant.trim().is_empty() {
        body.insert("tenant".into(), json!(tenant.trim()));
    }
    body.insert("user".into(), json!(user));
    body.insert("password".into(), json!(password));
    let v = post("/v1/login", &Value::Object(body)).await?;
    let token = v.get("token").and_then(|t| t.as_str()).ok_or(ApiError { status: 0, message: "The server did not return a token.".into() })?.to_string();
    // /v1/me needs the token, which is not stored yet
    let r = Request::get("/v1/me").header("Authorization", &format!("Bearer {token}")).send().await.map_err(|e| ApiError { status: 0, message: format!("Cannot reach the server: {e}") })?;
    let status = r.status();
    let text = r.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        return Err(ApiError { status, message: error_message(status, &text) });
    }
    let user: User = serde_json::from_str(&text).map_err(|e| ApiError { status, message: format!("Unexpected response: {e}") })?;
    Ok((token, user))
}

pub async fn whoami() -> Result<User, ApiError> {
    serde_json::from_value(get("/v1/me").await?).map_err(|e| ApiError { status: 0, message: e.to_string() })
}

pub async fn logout() {
    let _ = post("/v1/logout", &json!({})).await;
}

// ------------------------------------------------------------------ queries

#[derive(Clone, Debug, PartialEq, Default, Deserialize)]
pub struct QueryStats {
    #[serde(default)]
    pub nodes_created: u64,
    #[serde(default)]
    pub edges_created: u64,
    #[serde(default)]
    pub props_set: u64,
    #[serde(default)]
    pub deleted: u64,
    #[serde(default)]
    pub indexes_created: u64,
}
impl QueryStats {
    pub fn any(&self) -> bool {
        self.nodes_created + self.edges_created + self.props_set + self.deleted + self.indexes_created > 0
    }
}

/// `POST /v1/query` response.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct QueryOut {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    #[serde(default)]
    pub stats: QueryStats,
    #[serde(default)]
    pub ts: u64,
}

impl QueryOut {
    /// Rows as JSON objects keyed by (de-duplicated) column name, with node/edge references flattened,
    /// so the generic table component can show them.
    pub fn records(&self) -> (Vec<String>, Vec<Value>) {
        let mut names: Vec<String> = Vec::new();
        for c in &self.columns {
            let mut n = c.clone();
            let mut i = 2;
            while names.contains(&n) {
                n = format!("{c}_{i}");
                i += 1;
            }
            names.push(n);
        }
        let rows = self
            .rows
            .iter()
            .map(|r| {
                let mut m = Map::new();
                for (n, v) in names.iter().zip(r) {
                    m.insert(n.clone(), flatten_ref(v));
                }
                Value::Object(m)
            })
            .collect();
        (names, rows)
    }
}

/// `{"$node": 7}` -> `"#7"`, `{"$edge": 3}` -> `"edge 3"`; lists are flattened element-wise.
pub fn flatten_ref(v: &Value) -> Value {
    match v {
        Value::Object(m) if m.len() == 1 => {
            if let Some(n) = m.get("$node") {
                return Value::String(format!("#{n}"));
            }
            if let Some(n) = m.get("$edge") {
                return Value::String(format!("edge {n}"));
            }
            v.clone()
        }
        Value::Array(a) => Value::Array(a.iter().map(flatten_ref).collect()),
        other => other.clone(),
    }
}

pub async fn run_query(query: &str, params: &Map<String, Value>) -> Result<QueryOut, ApiError> {
    let v = post("/v1/query", &json!({ "query": query, "params": params })).await?;
    serde_json::from_value(v).map_err(|e| ApiError { status: 0, message: format!("Unexpected response: {e}") })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn error_bodies() {
        assert_eq!(error_message(400, r#"{"error":"bad query"}"#), "bad query");
        assert_eq!(error_message(403, "oops"), "Your role is not allowed to do that.");
        assert_eq!(error_message(500, ""), "The request failed (500).");
    }
    #[test]
    fn query_response_to_records() {
        let out: QueryOut = serde_json::from_value(json!({"columns":["node","score","score"],"rows":[[{"$node":3},0.5,[{"$node":1}]]],"stats":{"nodes_created":0},"ts":9})).unwrap();
        let (names, rows) = out.records();
        assert_eq!(names, ["node", "score", "score_2"]);
        assert_eq!(rows[0]["node"], json!("#3"));
        assert_eq!(rows[0]["score_2"], json!(["#1"]));
        assert!(!out.stats.any());
    }
    #[test]
    fn me_without_tenant() {
        let u: User = serde_json::from_value(json!({"user":"root","tenant":null,"role":"platform_admin"})).unwrap();
        assert!(u.tenant.is_none());
    }
}
