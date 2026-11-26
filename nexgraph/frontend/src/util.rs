//! Pure helpers (formatting, query strings). No browser APIs, so they are unit-tested natively.
use serde_json::Value;

/// `purchase_order_lines` / `purchase-order-lines` -> "Purchase order lines"
pub fn title_case(s: &str) -> String {
    let t = s.replace(['_', '-'], " ");
    let mut c = t.trim().chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// 1234567.891 -> "1,234,567.89"
pub fn money(v: f64) -> String {
    let neg = v < 0.0;
    let fixed = format!("{:.2}", v.abs());
    let (int, dec) = fixed.split_once('.').unwrap_or((&fixed, "00"));
    format!("{}{}.{dec}", if neg { "-" } else { "" }, group3(int))
}

pub fn integer(v: f64) -> String {
    let neg = v < 0.0;
    format!("{}{}", if neg { "-" } else { "" }, group3(&format!("{:.0}", v.abs())))
}

fn group3(int: &str) -> String {
    let mut out = String::new();
    for (i, ch) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// 1_250_000 -> "1.25M" (stat tiles and chart axes)
pub fn compact(v: f64) -> String {
    let a = v.abs();
    let (n, suffix) = if a >= 1e9 { (v / 1e9, "B") } else if a >= 1e6 { (v / 1e6, "M") } else if a >= 1e4 { (v / 1e3, "k") } else {
        return if v.fract() == 0.0 { integer(v) } else if a < 0.01 { format!("{v:.2e}") } else { format!("{v:.3}").trim_end_matches('0').trim_end_matches('.').to_string() };
    };
    let s = format!("{n:.2}");
    format!("{}{suffix}", s.trim_end_matches('0').trim_end_matches('.'))
}

pub fn is_money_key(k: &str) -> bool {
    ["amount", "debit", "credit", "balance", "revenue", "cost", "price", "total"].iter().any(|w| k.contains(w))
}

/// A float with 4 significant digits (graph scores span 1e-6 .. 1e6), grouped when large.
pub fn float_display(f: f64) -> String {
    if !f.is_finite() {
        return f.to_string();
    }
    let a = f.abs();
    if a == 0.0 {
        "0".into()
    } else if a >= 1000.0 {
        money(f)
    } else if a >= 0.001 {
        let s = format!("{f:.4}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        format!("{f:.3e}")
    }
}

/// Format a JSON scalar for a table cell. Query results are raw data, so no unit guessing beyond money columns.
pub fn display(key: &str, v: &Value) -> String {
    match v {
        Value::Null => "—".into(),
        Value::Bool(b) => if *b { "true".into() } else { "false".into() },
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                if key == "id" || key.ends_with("_id") || key == "ts" || key.contains("time") { n.to_string() } else { integer(n.as_f64().unwrap_or(0.0)) }
            } else {
                float_display(n.as_f64().unwrap_or(0.0))
            }
        }
        Value::String(s) => s.chars().take(200).collect(),
        Value::Array(a) => {
            let t = v.to_string();
            if t.chars().count() <= 90 { t } else { format!("[{} items]", a.len()) }
        }
        Value::Object(_) => v.to_string().chars().take(120).collect(),
    }
}

pub fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

pub fn short_id(v: &Value) -> String {
    match v {
        Value::String(s) if s.len() > 12 => format!("{}…", &s[..8]),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Colour family for a status word: "ok" | "warn" | "err" | "info" | "" (neutral).
pub fn tone_for(value: &str) -> &'static str {
    match value.to_lowercase().replace(' ', "_").as_str() {
        "ok" | "pass" | "passed" | "balanced" | "active" | "enabled" | "healthy" | "reader" => "ok",
        "warn" | "warning" | "writer" | "pending" => "warn",
        "denied" | "error" | "fail" | "failed" | "disabled" | "out_of_balance" => "err",
        "tenant_admin" | "platform_admin" => "info",
        _ => "",
    }
}

pub fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `/path` + params -> `/path?a=1&b=x%20y`; empty values are skipped.
pub fn with_query(path: &str, params: &[(String, String)]) -> String {
    let q: Vec<String> = params.iter().filter(|(_, v)| !v.is_empty()).map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v))).collect();
    if q.is_empty() { path.to_string() } else { format!("{path}{}{}", if path.contains('?') { '&' } else { '?' }, q.join("&")) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numbers() {
        assert_eq!(money(1234567.891), "1,234,567.89");
        assert_eq!(money(-5.0), "-5.00");
        assert_eq!(compact(1_250_000.0), "1.25M");
        assert_eq!(compact(0.5), "0.5");
        assert_eq!(float_display(0.000123456), "1.235e-4");
        assert_eq!(float_display(0.85), "0.85");
        assert_eq!(float_display(12345.678), "12,345.68");
    }

    #[test]
    fn cells() {
        assert_eq!(display("n", &json!(1234)), "1,234");
        assert_eq!(display("id", &json!(1234)), "1234");
        assert_eq!(display("score", &json!(0.1234567)), "0.1235");
        assert_eq!(display("x", &json!(null)), "—");
    }

    #[test]
    fn query_strings() {
        assert_eq!(with_query("/v1/audit", &[("limit".into(), "50".into()), ("tenant".into(), "".into())]), "/v1/audit?limit=50");
        assert_eq!(percent_encode("a b/c"), "a%20b%2Fc");
    }
}
