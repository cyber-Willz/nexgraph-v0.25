//! Business-intelligence tables (Power BI, and anything else that wants flat, typed tables).
//!
//! A property hypergraph is not a table, so this module defines a fixed, documented relational
//! projection of a snapshot:
//!
//! | table | one row per | columns |
//! |---|---|---|
//! | `nodes` | node | `id, label, labels, props` |
//! | `edges` | edge / hyperedge | `id, label, weight, directed, tail_count, head_count, src, dst, tail, head, props` |
//! | `edge_members` | (edge, member node) | `edge_id, node_id, role, label, weight` |
//! | `node_<Label>` | node carrying `Label` | `id, labels,` then one typed column per property key |
//! | `edge_<Label>` | edge with that label | `id, weight, src, dst,` then one typed column per property key |
//!
//! * `label` is the first label; `labels` joins all of them with `|`. `props` is the full property map as JSON text
//!   (Power Query: `Json.Document`), so nothing is lost by the typed per-label tables.
//! * `src`/`dst` are filled only for binary edges (one tail, one head); hyperedges are reached through
//!   `edge_members`, which is the bridge table for a star schema (`nodes` 1—* `edge_members` *—1 `edges`).
//! * Per-label property columns are typed by inference: all `Int` -> `int64`; `Int`/`Float` -> `double`;
//!   all `Bool` -> `boolean`; everything else (strings, lists, mixed) -> `text` (lists as JSON text).
//!   A property key that collides with a fixed column is renamed `prop_<key>`.
//! * Non-finite floats become null. Timestamps are whatever you stored (the cyber loaders use epoch seconds);
//!   convert in Power Query with `#datetime(1970,1,1,0,0,0) + #duration(0,0,0,seconds)` (see [`power_query`]).
//!
//! Table names contain only `[A-Za-z0-9_]`; labels with other characters are mapped to `_` and made unique.

use crate::db::Database;
use crate::error::{Error, Result};
use crate::model::Body;
use crate::query::{QueryResult, Stats};
use crate::store::View;
use crate::value::{Id, Props, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Column data type, in the vocabulary Power BI / Power Query uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColType {
    Int,
    Float,
    Bool,
    Text,
}

impl ColType {
    /// `int64`, `double`, `boolean` or `text`.
    pub fn as_str(self) -> &'static str {
        match self {
            ColType::Int => "int64",
            ColType::Float => "double",
            ColType::Bool => "boolean",
            ColType::Text => "text",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct BiColumn {
    pub name: String,
    pub ty: ColType,
}

/// A flat, typed table. Every cell is `Null`, `Int`, `Float`, `Bool` or `Str` consistent with the column type.
#[derive(Clone, Debug)]
pub struct BiTable {
    pub name: String,
    /// `nodes`, `edges`, `edge_members`, `node_label` or `edge_label`.
    pub kind: &'static str,
    /// For `node_label` / `edge_label` tables, the real label (the table name may be sanitised or de-duplicated).
    pub label: Option<String>,
    pub columns: Vec<BiColumn>,
    pub rows: Vec<Vec<Value>>,
}

impl BiTable {
    /// As a [`QueryResult`] (so `to_csv` / `to_json` / `to_table` apply). `ts` is the snapshot timestamp.
    pub fn to_result(&self, ts: u64) -> QueryResult {
        QueryResult { columns: self.columns.iter().map(|c| c.name.clone()).collect(), rows: self.rows.clone(), stats: Stats::default(), ts }
    }
    pub fn to_csv(&self) -> String {
        self.to_result(0).to_csv()
    }
    pub fn to_json(&self) -> String {
        self.to_result(0).to_json()
    }
}

fn finite(x: f64) -> Value {
    if x.is_finite() {
        Value::Float(x)
    } else {
        Value::Null
    }
}

/// JSON text of any value (lists and vectors included).
fn json_text(v: &Value) -> String {
    let mut s = String::new();
    json_into(v, &mut s);
    s
}

fn json_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn json_into(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(i) => out.push_str(&i.to_string()),
        Value::Float(f) if f.is_finite() => out.push_str(&f.to_string()),
        Value::Float(_) => out.push_str("null"),
        Value::Str(s) => json_str(s, out),
        Value::List(l) => {
            out.push('[');
            for (i, x) in l.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                json_into(x, out);
            }
            out.push(']');
        }
        Value::Vector(v) => json_into(&Value::List(v.iter().map(|x| Value::Float(*x as f64)).collect()), out),
        Value::Node(i) | Value::Edge(i) => out.push_str(&i.to_string()),
    }
}

fn props_json(p: &Props) -> String {
    let mut s = String::from("{");
    for (i, (k, v)) in p.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        json_str(k, &mut s);
        s.push(':');
        json_into(v, &mut s);
    }
    s.push('}');
    s
}

fn ids_json(ids: &[Id]) -> String {
    format!("[{}]", ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(","))
}

fn id_val(i: Id) -> Value {
    Value::Int(i as i64)
}

fn col(name: &str, ty: ColType) -> BiColumn {
    BiColumn { name: name.to_string(), ty }
}

/// Turn a label into a table-name fragment (`[A-Za-z0-9_]`, never empty).
fn sanitize(label: &str) -> String {
    let s: String = label.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect();
    if s.is_empty() {
        "_".into()
    } else {
        s
    }
}

/// Table name -> (kind, label) for every table of this snapshot. Deterministic.
struct Catalog {
    node_labels: BTreeMap<String, String>, // table name -> label
    edge_labels: BTreeMap<String, String>,
}

fn catalog(view: &View) -> Catalog {
    let mut nl: BTreeSet<String> = BTreeSet::new();
    let mut el: BTreeSet<String> = BTreeSet::new();
    for id in view.node_ids() {
        if let Some(Body::Node { labels, .. }) = view.node(id) {
            nl.extend(labels.iter().cloned());
        }
    }
    for id in view.edge_ids() {
        if let Some(Body::Edge { label, .. }) = view.edge(id) {
            el.insert(label.clone());
        }
    }
    let mut taken: BTreeSet<String> = ["nodes", "edges", "edge_members"].iter().map(|s| s.to_lowercase()).collect();
    let mut name_all = |prefix: &str, labels: BTreeSet<String>| {
        let mut m = BTreeMap::new();
        for l in labels {
            let base = format!("{prefix}_{}", sanitize(&l));
            let mut name = base.clone();
            let mut n = 2;
            while taken.contains(&name.to_lowercase()) {
                name = format!("{base}_{n}");
                n += 1;
            }
            taken.insert(name.to_lowercase());
            m.insert(name, l);
        }
        m
    };
    let node_labels = name_all("node", nl);
    let edge_labels = name_all("edge", el);
    Catalog { node_labels, edge_labels }
}

/// Names of every table in this snapshot, fixed tables first.
pub fn table_names(view: &View) -> Vec<String> {
    let c = catalog(view);
    let mut v: Vec<String> = vec!["nodes".into(), "edges".into(), "edge_members".into()];
    v.extend(c.node_labels.keys().cloned());
    v.extend(c.edge_labels.keys().cloned());
    v
}

fn nodes_table(view: &View) -> BiTable {
    let mut rows = Vec::new();
    for id in view.node_ids() {
        if let Some(Body::Node { labels, props }) = view.node(id) {
            rows.push(vec![id_val(id), labels.first().map(|l| Value::Str(l.clone())).unwrap_or(Value::Null), Value::Str(labels.join("|")), Value::Str(props_json(props))]);
        }
    }
    BiTable { name: "nodes".into(), kind: "nodes", label: None, columns: vec![col("id", ColType::Int), col("label", ColType::Text), col("labels", ColType::Text), col("props", ColType::Text)], rows }
}

fn edges_table(view: &View) -> BiTable {
    let mut rows = Vec::new();
    for id in view.edge_ids() {
        if let Some(Body::Edge { label, tail, head, weight, props }) = view.edge(id) {
            let binary = tail.len() == 1 && head.len() == 1;
            rows.push(vec![
                id_val(id),
                Value::Str(label.clone()),
                finite(*weight),
                Value::Bool(!head.is_empty()),
                Value::Int(tail.len() as i64),
                Value::Int(head.len() as i64),
                if binary { id_val(tail[0]) } else { Value::Null },
                if binary { id_val(head[0]) } else { Value::Null },
                Value::Str(ids_json(tail)),
                Value::Str(ids_json(head)),
                Value::Str(props_json(props)),
            ]);
        }
    }
    BiTable {
        name: "edges".into(),
        kind: "edges",
        label: None,
        columns: vec![
            col("id", ColType::Int),
            col("label", ColType::Text),
            col("weight", ColType::Float),
            col("directed", ColType::Bool),
            col("tail_count", ColType::Int),
            col("head_count", ColType::Int),
            col("src", ColType::Int),
            col("dst", ColType::Int),
            col("tail", ColType::Text),
            col("head", ColType::Text),
            col("props", ColType::Text),
        ],
        rows,
    }
}

fn members_table(view: &View) -> BiTable {
    let mut rows = Vec::new();
    for id in view.edge_ids() {
        if let Some(Body::Edge { label, tail, head, weight, .. }) = view.edge(id) {
            // A member that is on both sides appears once per role, like the PostgreSQL mirror's edge_members.
            for (role, side) in [("tail", tail), ("head", head)] {
                let mut seen = BTreeSet::new();
                for &n in side {
                    if seen.insert(n) {
                        rows.push(vec![id_val(id), id_val(n), Value::Str(role.into()), Value::Str(label.clone()), finite(*weight)]);
                    }
                }
            }
        }
    }
    BiTable {
        name: "edge_members".into(),
        kind: "edge_members",
        label: None,
        columns: vec![col("edge_id", ColType::Int), col("node_id", ColType::Int), col("role", ColType::Text), col("label", ColType::Text), col("weight", ColType::Float)],
        rows,
    }
}

/// Infer a column type from every non-null value seen.
fn infer(values: &[&Value]) -> ColType {
    let (mut ints, mut floats, mut bools, mut other) = (0usize, 0usize, 0usize, 0usize);
    for v in values {
        match v {
            Value::Null => {}
            Value::Int(_) => ints += 1,
            Value::Float(f) if f.is_finite() => floats += 1,
            Value::Float(_) => {}
            Value::Bool(_) => bools += 1,
            _ => other += 1,
        }
    }
    if other > 0 || (bools > 0 && ints + floats > 0) {
        ColType::Text
    } else if bools > 0 {
        ColType::Bool
    } else if floats > 0 {
        ColType::Float
    } else if ints > 0 {
        ColType::Int
    } else {
        ColType::Text // no data: text keeps every later value representable
    }
}

fn cast(v: &Value, ty: ColType) -> Value {
    match (v, ty) {
        (Value::Null, _) => Value::Null,
        (Value::Int(i), ColType::Int) => Value::Int(*i),
        (Value::Int(i), ColType::Float) => Value::Float(*i as f64),
        (Value::Float(f), ColType::Float) => finite(*f),
        (Value::Bool(b), ColType::Bool) => Value::Bool(*b),
        (Value::Str(s), ColType::Text) => Value::Str(s.clone()),
        (other, ColType::Text) => Value::Str(json_text(other)),
        // Unreachable by construction of `infer`; keep a lossless fallback instead of panicking.
        (other, _) => Value::Str(json_text(other)),
    }
}

/// Typed per-label table. `edge` selects edges (with `weight`, `src`, `dst`) instead of nodes.
fn label_table(view: &View, name: &str, label: &str, edge: bool) -> BiTable {
    let ids: Vec<Id> = if edge { view.edges_with_label(label) } else { view.nodes_with_label(label) };
    let mut keys: BTreeSet<&str> = BTreeSet::new();
    let bodies: Vec<(Id, &Body)> = ids.iter().filter_map(|&i| view.body(i).map(|b| (i, b))).collect();
    for (_, b) in &bodies {
        keys.extend(b.props().keys().map(|k| k.as_str()));
    }
    let fixed: Vec<BiColumn> = if edge {
        vec![col("id", ColType::Int), col("weight", ColType::Float), col("src", ColType::Int), col("dst", ColType::Int)]
    } else {
        vec![col("id", ColType::Int), col("labels", ColType::Text)]
    };
    let reserved: BTreeSet<&str> = fixed.iter().map(|c| c.name.as_str()).collect();
    let mut columns = fixed.clone();
    let mut prop_cols: Vec<(&str, ColType)> = Vec::new();
    for k in keys {
        let vals: Vec<&Value> = bodies.iter().filter_map(|(_, b)| b.props().get(k)).collect();
        let ty = infer(&vals);
        let mut cname = if reserved.contains(k) { format!("prop_{k}") } else { k.to_string() };
        while columns.iter().any(|c| c.name == cname) {
            cname = format!("prop_{cname}");
        }
        columns.push(BiColumn { name: cname, ty });
        prop_cols.push((k, ty));
    }
    let mut rows = Vec::with_capacity(bodies.len());
    for (id, b) in &bodies {
        let mut r: Vec<Value> = Vec::with_capacity(columns.len());
        match b {
            Body::Edge { tail, head, weight, .. } => {
                let binary = tail.len() == 1 && head.len() == 1;
                r.push(id_val(*id));
                r.push(finite(*weight));
                r.push(if binary { id_val(tail[0]) } else { Value::Null });
                r.push(if binary { id_val(head[0]) } else { Value::Null });
            }
            Body::Node { labels, .. } => {
                r.push(id_val(*id));
                r.push(Value::Str(labels.join("|")));
            }
        }
        for (k, ty) in &prop_cols {
            r.push(b.props().get(*k).map(|v| cast(v, *ty)).unwrap_or(Value::Null));
        }
        rows.push(r);
    }
    BiTable { name: name.to_string(), kind: if edge { "edge_label" } else { "node_label" }, label: Some(label.to_string()), columns, rows }
}

/// One table by name (see [`table_names`]). Names are matched exactly; an unknown name is `Error::Query`.
pub fn table(view: &View, name: &str) -> Result<BiTable> {
    match name {
        "nodes" => return Ok(nodes_table(view)),
        "edges" => return Ok(edges_table(view)),
        "edge_members" => return Ok(members_table(view)),
        _ => {}
    }
    let c = catalog(view);
    if let Some(l) = c.node_labels.get(name) {
        return Ok(label_table(view, name, l, false));
    }
    if let Some(l) = c.edge_labels.get(name) {
        return Ok(label_table(view, name, l, true));
    }
    Err(Error::Query(format!("no such table {name:?}")))
}

/// Every table of the snapshot.
pub fn all_tables(view: &View) -> Vec<BiTable> {
    let c = catalog(view);
    let mut v = vec![nodes_table(view), edges_table(view), members_table(view)];
    for (n, l) in &c.node_labels {
        v.push(label_table(view, n, l, false));
    }
    for (n, l) in &c.edge_labels {
        v.push(label_table(view, n, l, true));
    }
    v
}

/// JSON catalog: `{"ts":..,"tables":[{"name","kind","rows","columns":[{"name","type"}]}]}`.
pub fn catalog_json(tables: &[BiTable], ts: u64) -> String {
    let mut s = format!("{{\"ts\":{ts},\"tables\":[");
    for (i, t) in tables.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str("{\"name\":");
        json_str(&t.name, &mut s);
        s.push_str(&format!(",\"kind\":\"{}\",\"rows\":{},\"columns\":[", t.kind, t.rows.len()));
        for (j, c) in t.columns.iter().enumerate() {
            if j > 0 {
                s.push(',');
            }
            s.push_str("{\"name\":");
            json_str(&c.name, &mut s);
            s.push_str(&format!(",\"type\":\"{}\"}}", c.ty.as_str()));
        }
        s.push_str("]}");
    }
    s.push_str("]}");
    s
}

/// What [`export_dir`] wrote.
#[derive(Clone, Debug)]
pub struct ExportReport {
    pub dir: PathBuf,
    /// `(table, rows)` for every CSV file.
    pub tables: Vec<(String, usize)>,
    /// Snapshot timestamp (all tables are from this one consistent snapshot).
    pub ts: u64,
}

/// Write every table as `<dir>/<table>.csv` (UTF-8, RFC 4180, header row), `<dir>/catalog.json`
/// and `<dir>/nexgraph_folder.pq` (a Power Query script that loads the folder with correct types).
/// All files come from one snapshot, so relationships between tables are consistent.
pub fn export_dir(db: &Database, dir: impl AsRef<Path>) -> Result<ExportReport> {
    let dir = dir.as_ref();
    std::fs::create_dir_all(dir).map_err(|e| Error::Invalid(format!("cannot create {}: {e}", dir.display())))?;
    let (tables, ts) = db.read(|v| (all_tables(v), v.ts));
    let io = |p: &Path, e: std::io::Error| Error::Invalid(format!("cannot write {}: {e}", p.display()));
    let mut report = Vec::new();
    for t in &tables {
        let p = dir.join(format!("{}.csv", t.name));
        std::fs::write(&p, t.to_csv()).map_err(|e| io(&p, e))?;
        report.push((t.name.clone(), t.rows.len()));
    }
    let p = dir.join("catalog.json");
    std::fs::write(&p, catalog_json(&tables, ts)).map_err(|e| io(&p, e))?;
    let p = dir.join("nexgraph_folder.pq");
    std::fs::write(&p, power_query_folder(&tables)).map_err(|e| io(&p, e))?;
    Ok(ExportReport { dir: dir.to_path_buf(), tables: report, ts })
}

fn m_type(t: ColType) -> &'static str {
    match t {
        ColType::Int => "Int64.Type",
        ColType::Float => "type number",
        ColType::Bool => "type logical",
        ColType::Text => "type text",
    }
}

fn m_str(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Power Query (M) script for a folder written by [`export_dir`]. Paste into a Blank Query in the Advanced Editor
/// and set `Folder`. Returns a record of typed tables, one field per CSV.
pub fn power_query_folder(tables: &[BiTable]) -> String {
    let mut s = String::new();
    s.push_str("// nexgraph -> Power BI: load the CSV folder written by `.powerbi DIR` / bi::export_dir.\n");
    s.push_str("// Set Folder to that directory. The query returns a record with one typed table per field: create one query\n");
    s.push_str("// from this, then reference fields from other queries, e.g. NexGraph[nodes]. `props` is JSON text: Json.Document([props]).\n");
    s.push_str("let\n    Folder = \"C:\\path\\to\\export\",\n");
    s.push_str("    Load = (name as text, types as list) as table =>\n        let\n");
    s.push_str("            Raw = Csv.Document(File.Contents(Folder & \"\\\" & name & \".csv\"), [Delimiter = \",\", Encoding = 65001, QuoteStyle = QuoteStyle.Csv]),\n");
    s.push_str("            Hdr = Table.PromoteHeaders(Raw, [PromoteAllScalars = true]),\n");
    s.push_str("            Typed = Table.TransformColumnTypes(Hdr, types, \"en-US\")\n        in\n            Typed,\n");
    s.push_str("    Tables = [\n");
    for (i, t) in tables.iter().enumerate() {
        let types: Vec<String> = t.columns.iter().map(|c| format!("{{{}, {}}}", m_str(&c.name), m_type(c.ty))).collect();
        s.push_str(&format!("        {} = Load({}, {{{}}}){}\n", m_ident(&t.name), m_str(&t.name), types.join(", "), if i + 1 < tables.len() { "," } else { "" }));
    }
    s.push_str("    ]\nin\n    Tables\n");
    s
}

fn m_ident(name: &str) -> String {
    // Table names are [A-Za-z0-9_]; a leading digit would need quoting in M.
    if name.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        format!("#\"{name}\"")
    } else {
        name.to_string()
    }
}

/// Power Query (M) script for the HTTPS API (`nexgraph-server`). `base_url` like `https://host:8443`.
/// Credentials are Basic: user `tenant/user` and the password, supplied as query parameters of the function
/// so that no secret is stored in the script; the tables arrive typed from the server's catalog.
pub fn power_query(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    format!(
        r#"// nexgraph -> Power BI over HTTPS (nexgraph-server).
// 1. Home > Transform data > New Source > Blank Query > Advanced Editor, paste this.
// 2. Set the data-source credentials to "Anonymous": the Authorization header below is what authenticates.
//    (Do not publish the password inside the M; create text Parameters User and Password and use them.)
// Use: NexGraph[Table]("nodes")   NexGraph[Query]("MATCH (h:Host) RETURN h.ip AS ip, h.risk AS risk")
let
    BaseUrl = {base_q},
    User = "tenant/user",
    Password = "change-me",
    Auth = "Basic " & Binary.ToText(Text.ToBinary(User & ":" & Password, TextEncoding.Utf8), BinaryEncoding.Base64),
    Get = (path as text, query as record) as binary =>
        Web.Contents(BaseUrl, [RelativePath = path, Query = query, Headers = [Authorization = Auth]]),
    TypeOf = (t as text) as type =>
        if t = "int64" then Int64.Type else if t = "double" then type number else if t = "boolean" then type logical else type text,
    Catalog = Json.Document(Get("v1/bi/tables", []))[tables],
    NexTable = (name as text) as table =>
        let
            Entry = List.First(List.Select(Catalog, each [name] = name)),
            Raw = Csv.Document(Get("v1/bi/tables/" & name, [format = "csv"]), [Delimiter = ",", Encoding = 65001, QuoteStyle = QuoteStyle.Csv]),
            Hdr = Table.PromoteHeaders(Raw, [PromoteAllScalars = true]),
            Typed = Table.TransformColumnTypes(Hdr, List.Transform(Entry[columns], each {{[name], TypeOf([type])}}), "en-US")
        in
            Typed,
    // Any read-only NQL statement. POST is not used; long statements may hit URL limits (see POWERBI.md).
    NexQuery = (nql as text) as table =>
        Table.PromoteHeaders(Csv.Document(Get("v1/bi/query", [q = nql, format = "csv"]), [Delimiter = ",", Encoding = 65001, QuoteStyle = QuoteStyle.Csv]), [PromoteAllScalars = true]),
    // Epoch seconds -> datetime (UTC).
    Epoch = (seconds as number) as datetime => #datetime(1970, 1, 1, 0, 0, 0) + #duration(0, 0, 0, seconds)
in
    [Table = NexTable, Query = NexQuery, Epoch = Epoch, Catalog = Catalog]
"#,
        base_q = m_str(base)
    )
}

/// Snapshot read of one table, optionally time-travelling with `as_of` (a commit timestamp). Returns the table and its snapshot ts.
pub fn snapshot_table(db: &Database, name: &str, as_of: Option<u64>) -> Result<(BiTable, u64)> {
    let f = |v: &View| table(v, name).map(|t| (t, v.ts));
    match as_of {
        Some(ts) => db.read_at(ts, f)?,
        None => db.read(f),
    }
}

/// Snapshot read of every table (for the catalog), optionally as of a commit timestamp.
pub fn snapshot_tables(db: &Database, as_of: Option<u64>) -> Result<(Vec<BiTable>, u64)> {
    let f = |v: &View| (all_tables(v), v.ts);
    match as_of {
        Some(ts) => db.read_at(ts, f),
        None => Ok(db.read(f)),
    }
}
