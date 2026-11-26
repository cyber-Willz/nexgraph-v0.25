//! Loaders that turn tabular and security-log data into graph structure (feature `ingest`).
//!
//! * [`import_csv_nodes`]: one node per CSV row (optionally upserted on a key column).
//! * [`import_csv_edges`]: one edge per CSV row between nodes looked up (or created) by key.
//! * [`import_events`]: JSON-lines network telemetry, [`Preset::SuricataEve`] (`eve.json`) or
//!   [`Preset::ZeekJson`] (`conn.log` / `dns.log` in JSON format).
//!
//! Every import runs in **one transaction**: it either commits whole or not at all. Malformed
//! rows/lines are skipped and counted (the first problem is kept in the report); they never abort the load.
//!
//! ## Graph shape produced by [`import_events`]
//!
//! | record | creates |
//! |---|---|
//! | flow / Zeek conn | `(:Host {ip})-[:FLOW {ts, proto, dst_port, bytes, bytes_out, bytes_in, service, state}]->(:Host)` |
//! | alert (Suricata) | `(:Host)-[:ALERT {ts, signature, signature_id, severity, category, mitre}]->(:Host)` |
//! | dns | `(:Host)-[:QUERY {ts, qtype}]->(:Domain {name})` |
//! | http / tls | `(:Host)-[:HTTP|:TLS {ts, ...}]->(:Domain {name})` (Host header / SNI) |
//!
//! `ts` is Unix epoch seconds (float), the format `cyber.beaconing`, `stats.timeseries` and
//! `stats.spikes` expect. Bytes of a flow are `bytes = bytes_out + bytes_in`.

use crate::db::{Database, Txn};
use crate::error::{Error, Result};
use crate::value::{Id, Props, Value};
use std::collections::{HashMap, HashSet};

/// What an import did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImportReport {
    pub rows_read: usize,
    pub rows_skipped: usize,
    pub nodes_created: usize,
    pub nodes_updated: usize,
    pub edges_created: usize,
    /// Rows dropped because an identical row (or the same key with identical content) was already imported
    /// in this call (see [`Dedup`]). Not counted in `rows_skipped`.
    pub duplicates_skipped: usize,
    /// Rows dropped because their dedup key was already imported with *different* content (first one wins).
    /// Also counted in `rows_skipped`.
    pub key_conflicts: usize,
    /// Keyed node imports only: rows whose key had already been seen *earlier in the same file*, so they overwrote
    /// that row's properties instead of creating a node. A non-zero value usually means the key column is not unique
    /// (e.g. an address that repeats with different measurements) and rows were merged.
    pub duplicate_keys: usize,
    /// First skipped row's reason (1-based line number included), if any.
    pub first_error: Option<String>,
    /// Commit timestamp (0 if nothing was written).
    pub ts: u64,
}

impl ImportReport {
    fn skip(&mut self, line: usize, why: impl Into<String>) {
        self.rows_skipped += 1;
        if self.first_error.is_none() {
            self.first_error = Some(format!("line {line}: {}", why.into()));
        }
    }
}

// ---------------------------------------------------------------- CSV

/// RFC 4180 parser: quoted fields, doubled quotes, embedded newlines, CRLF or LF, optional BOM.
/// Returns rows with their 1-based starting line number. Blank lines are ignored.
pub fn parse_csv(text: &str, delim: char) -> Result<Vec<(usize, Vec<String>)>> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let (mut rows, mut row, mut field) = (Vec::new(), Vec::new(), String::new());
    let (mut in_q, mut was_quoted, mut line, mut start) = (false, false, 1usize, 1usize);
    let mut chars = text.chars().peekable();
    let flush_row = |row: &mut Vec<String>, field: &mut String, was_quoted: &mut bool, start: usize, rows: &mut Vec<(usize, Vec<String>)>| {
        row.push(std::mem::take(field));
        let blank = row.len() == 1 && row[0].is_empty() && !*was_quoted;
        if !blank {
            rows.push((start, std::mem::take(row)));
        } else {
            row.clear();
        }
        *was_quoted = false;
    };
    while let Some(c) = chars.next() {
        if in_q {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    field.push('"');
                }
                '"' => in_q = false,
                '\n' => {
                    line += 1;
                    field.push(c);
                }
                _ => field.push(c),
            }
        } else if c == '"' && field.is_empty() && !was_quoted {
            in_q = true;
            was_quoted = true;
        } else if c == delim {
            row.push(std::mem::take(&mut field));
            was_quoted = false;
        } else if c == '\n' || c == '\r' {
            if c == '\r' && chars.peek() == Some(&'\n') {
                chars.next();
            }
            flush_row(&mut row, &mut field, &mut was_quoted, start, &mut rows);
            line += 1;
            start = line;
        } else {
            field.push(c);
        }
    }
    if in_q {
        return Err(Error::Query(format!("CSV: unterminated quoted field starting before line {line}")));
    }
    if !field.is_empty() || !row.is_empty() || was_quoted {
        flush_row(&mut row, &mut field, &mut was_quoted, start, &mut rows);
    }
    Ok(rows)
}

/// Type inference for one cell: bool, integer, float, else string. Empty cells yield `None`.
/// Numbers with a leading zero (`007`, zip codes, phone numbers) and numeric-looking IDs too long for
/// an i64 stay strings, so identifiers survive the round trip.
pub fn infer_value(cell: &str) -> Option<Value> {
    let t = cell.trim();
    if t.is_empty() {
        return None;
    }
    match t.to_ascii_lowercase().as_str() {
        "true" => return Some(Value::Bool(true)),
        "false" => return Some(Value::Bool(false)),
        _ => {}
    }
    let digits = t.trim_start_matches(['+', '-']);
    let starts_num = digits.chars().next().map_or(false, |c| c.is_ascii_digit() || c == '.');
    if starts_num {
        let lead_zero = digits.len() > 1 && digits.starts_with('0') && !digits.starts_with("0.");
        if !lead_zero {
            if let Ok(i) = t.parse::<i64>() {
                return Some(Value::Int(i));
            }
            if let Ok(x) = t.parse::<f64>() {
                if x.is_finite() && !digits.chars().all(|c| c.is_ascii_digit()) {
                    return Some(Value::Float(x));
                }
            }
        }
    }
    Some(Value::Str(cell.to_string()))
}

fn key_string(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        o => o.to_string(),
    }
}

fn header(rows: &[(usize, Vec<String>)]) -> Result<Vec<String>> {
    let h = rows.first().ok_or_else(|| Error::Query("CSV is empty (a header row is required)".into()))?;
    let cols: Vec<String> = h.1.iter().map(|c| c.trim().to_string()).collect();
    for (i, c) in cols.iter().enumerate() {
        if c.is_empty() {
            return Err(Error::Query(format!("CSV header: column {} has no name", i + 1)));
        }
        if cols[..i].contains(c) {
            return Err(Error::Query(format!("CSV header: duplicate column '{c}'")));
        }
    }
    Ok(cols)
}

fn col_index(cols: &[String], name: &str, what: &str) -> Result<usize> {
    cols.iter().position(|c| c == name).ok_or_else(|| Error::Query(format!("CSV has no column '{name}' ({what}); columns: {}", cols.join(", "))))
}

/// Existing `label` nodes by key value, so imports can upsert and link.
fn existing(db: &Database, label: &str, key: &str) -> HashMap<String, Id> {
    db.read(|v| {
        v.nodes_with_label(label)
            .into_iter()
            .filter_map(|id| v.node(id).and_then(|b| b.props().get(key)).map(|k| (key_string(k), id)))
            .collect()
    })
}

/// One node per row, labelled `label`; every non-empty cell becomes a typed property.
/// With `key = Some(col)` rows are **upserted**: an existing `label` node whose `col` equals the cell
/// is updated (new cell values overwrite, absent cells keep their old value); a row with an empty key
/// cell is skipped. Without a key every row creates a new node.
pub fn import_csv_nodes(db: &Database, label: &str, csv: &str, key: Option<&str>) -> Result<ImportReport> {
    import_nodes_impl(db, label, csv, key.map(|k| (k, k)))
}

/// Like [`import_csv_nodes`] with an upsert key, but the matching node property is `key_prop` rather than the column
/// name (v0.12.1). This is how a table joins onto the nodes a hyperedge import created: those are keyed by `name`
/// (`HyperCsv::key_prop`), so `import_csv_nodes_keyed(db, "Customer", csv, "Customer_ID", "name")` adds the customer
/// columns to the existing `Customer` nodes (creating the ones that are missing), after which a record can read them
/// as `Customer.Credit_Score` (see `killchain::lookup`). The key cell is also stored under `key_prop`.
pub fn import_csv_nodes_keyed(db: &Database, label: &str, csv: &str, key_col: &str, key_prop: &str) -> Result<ImportReport> {
    import_nodes_impl(db, label, csv, Some((key_col, key_prop)))
}

fn import_nodes_impl(db: &Database, label: &str, csv: &str, key: Option<(&str, &str)>) -> Result<ImportReport> {
    let rows = parse_csv(csv, ',')?;
    let cols = header(&rows)?;
    let kc = key.map(|(k, _)| col_index(&cols, k, "key")).transpose()?;
    let mut known = match key {
        Some((_, prop)) => existing(db, label, prop),
        None => HashMap::new(),
    };
    let mut rep = ImportReport::default();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut tx = db.begin();
    for (line, r) in rows.iter().skip(1) {
        rep.rows_read += 1;
        if r.len() != cols.len() {
            rep.skip(*line, format!("{} fields, header has {}", r.len(), cols.len()));
            continue;
        }
        let mut p: Props = cols.iter().zip(r).filter_map(|(c, v)| infer_value(v).map(|v| (c.clone(), v))).collect();
        if let Some(kc) = kc {
            let Some(kv) = infer_value(&r[kc]) else {
                rep.skip(*line, "empty key");
                continue;
            };
            if let Some((_, prop)) = key {
                p.insert(prop.to_string(), kv.clone());
            }
            let ks = key_string(&kv);
            if !seen.insert(ks.clone()) {
                rep.duplicate_keys += 1;
            }
            if let Some(&id) = known.get(&ks) {
                tx.set_props(id, p, &[])?;
                rep.nodes_updated += 1;
                continue;
            }
            let id = tx.create_node(&[label], p)?;
            known.insert(ks, id);
        } else {
            tx.create_node(&[label], p)?;
        }
        rep.nodes_created += 1;
    }
    rep.ts = commit(tx, &rep)?;
    Ok(rep)
}

fn commit(tx: Txn<'_>, rep: &ImportReport) -> Result<u64> {
    if tx.pending_ops() == 0 {
        tx.rollback();
        return Ok(0);
    }
    let _ = rep;
    tx.commit()
}

/// Options for [`import_csv_edges`].
#[derive(Debug, Clone)]
pub struct EdgeCsv<'a> {
    pub edge_label: &'a str,
    pub src_col: &'a str,
    pub dst_col: &'a str,
    /// Label and key property of the endpoint nodes (looked up, created when missing).
    pub node_label: &'a str,
    pub node_key: &'a str,
    /// Column holding the edge weight (default 1.0 when absent or empty).
    pub weight_col: Option<&'a str>,
}

/// One directed edge per row from the node whose key equals `src_col` to the one for `dst_col`.
/// All other non-empty cells become edge properties. Rows with an empty endpoint or a non-numeric
/// weight are skipped; missing endpoint nodes are created with just the key property.
pub fn import_csv_edges(db: &Database, o: &EdgeCsv, csv: &str) -> Result<ImportReport> {
    let rows = parse_csv(csv, ',')?;
    let cols = header(&rows)?;
    let (sc, dc) = (col_index(&cols, o.src_col, "source")?, col_index(&cols, o.dst_col, "target")?);
    let wc = o.weight_col.map(|w| col_index(&cols, w, "weight")).transpose()?;
    let mut known = existing(db, o.node_label, o.node_key);
    let mut rep = ImportReport::default();
    let mut tx = db.begin();
    let mut endpoint = |tx: &mut Txn, rep: &mut ImportReport, cell: &str| -> Result<Id> {
        let v = infer_value(cell).unwrap();
        let ks = key_string(&v);
        if let Some(&id) = known.get(&ks) {
            return Ok(id);
        }
        let id = tx.create_node(&[o.node_label], std::iter::once((o.node_key.to_string(), v)).collect())?;
        known.insert(ks, id);
        rep.nodes_created += 1;
        Ok(id)
    };
    for (line, r) in rows.iter().skip(1) {
        rep.rows_read += 1;
        if r.len() != cols.len() {
            rep.skip(*line, format!("{} fields, header has {}", r.len(), cols.len()));
            continue;
        }
        if r[sc].trim().is_empty() || r[dc].trim().is_empty() {
            rep.skip(*line, "empty source or target");
            continue;
        }
        let w = match wc.map(|i| r[i].trim()).filter(|s| !s.is_empty()) {
            None => 1.0,
            Some(s) => match s.parse::<f64>() {
                Ok(x) if x.is_finite() => x,
                _ => {
                    rep.skip(*line, format!("weight '{s}' is not a number"));
                    continue;
                }
            },
        };
        let p: Props = cols.iter().enumerate().filter(|(i, _)| *i != sc && *i != dc && Some(*i) != wc).filter_map(|(i, c)| infer_value(&r[i]).map(|v| (c.clone(), v))).collect();
        let (a, b) = (endpoint(&mut tx, &mut rep, &r[sc])?, endpoint(&mut tx, &mut rep, &r[dc])?);
        tx.create_edge(o.edge_label, a, b, w, p)?;
        rep.edges_created += 1;
    }
    rep.ts = commit(tx, &rep)?;
    Ok(rep)
}

/// Options for [`import_csv_hyperedges`].
#[derive(Debug, Clone)]
pub struct HyperCsv<'a> {
    /// Label of the created hyperedges (one per row).
    pub edge_label: &'a str,
    /// `(column, node label)`: every non-empty cell of the column is a *member* of the row's hyperedge, a node
    /// of that label found (or created) by its value. Rows therefore become `k`-ary hyperedges over e.g.
    /// attacker, target, attack type, industry, country, ...
    pub members: &'a [(&'a str, &'a str)],
    /// Property holding the cell value on member nodes (a property index is created on it for each label).
    pub key_prop: &'a str,
    /// Optional numeric column used as the hyperedge weight (default 1.0).
    pub weight_col: Option<&'a str>,
    /// Columns holding ISO 8601 timestamps; stored as epoch seconds (float) under the same name.
    pub time_cols: &'a [&'a str],
}

/// How [`import_csv_hyperedges_with`] treats repeated rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Dedup<'a> {
    /// Import every row (the behaviour of [`import_csv_hyperedges`]).
    #[default]
    None,
    /// Skip a row identical, cell for cell, to one already imported in this call.
    ExactRow,
    /// Rows sharing the value of this column are one record: a repeat with identical content is skipped and
    /// counted in `duplicates_skipped`; a repeat with different content is skipped and counted in
    /// `key_conflicts` (the first row wins), so conflicting records are never merged silently.
    Key(&'a str),
}

/// Extra options for [`import_csv_hyperedges_with`].
#[derive(Debug, Clone, Copy, Default)]
pub struct HyperOpts<'a> {
    pub dedup: Dedup<'a>,
    /// Also store each member column's value as an edge property under the column name. Costs memory but
    /// lets `WHERE e.Attack_Vector = 'Email'` and the `killchain.*` procedures group by a member column
    /// without joining to the member nodes.
    pub keep_member_props: bool,
    /// How the `time_cols` dates are written (default ISO 8601). `11/8/2016` needs [`DateOrder::MonthFirst`] or
    /// [`DateOrder::DayFirst`]: the two cannot be told apart, so the importer never guesses.
    pub date_order: crate::timeutil::DateOrder,
    /// Qualified member keys (v0.12): `(member column, parent columns)`. The member node is looked up and created
    /// under `parent1/parent2/value`, so the same name in different parents is a different node. Land registries
    /// reuse `Taluk-A` or `Village-9` under every district; keyed by the bare value they were merged into one
    /// node that joined unrelated districts (and every spectral / community procedure then saw links that do not
    /// exist). A row with an empty parent cell is skipped and counted. The edge property keeps the raw cell.
    pub qualify: &'a [(&'a str, &'a [&'a str])],
    /// Columns kept as text even when they look numeric (phone numbers, survey and sub-division codes, parcel
    /// numbers), so an identifier is never summed, averaged or exported as a number.
    pub string_cols: &'a [&'a str],
    /// Commit every `chunk_rows` edges instead of once at the end (0, the default, is one atomic commit).
    /// A 1.03 million row bank ledger (97 MB of CSV) needed 5.9 GB in one transaction, because every operation is
    /// held until the commit and then copied into the graph; in chunks the operation buffer is freed as it goes.
    /// The price is atomicity: if a later chunk fails, the earlier chunks stay committed, and the error says how many
    /// rows were already in. Nodes created by a chunk are visible to the next one.
    pub chunk_rows: usize,
    /// Field separator; `None` is a comma. The SEC Financial Statement Data Sets (`sub.txt`, `num.txt`, `pre.txt`) and
    /// many bank exports are tab-separated, which v0.12 could not read at all.
    pub delimiter: Option<char>,
    /// Allow a single member column. A track / observation table (one row per sighting of one track) has no second
    /// entity, so the default of two or more columns made it impossible to import. Off by default: a one-member
    /// hyperedge carries no relation, so a typo in `members` should still be an error.
    pub allow_single_member: bool,
}

/// 128-bit fingerprint of a row (two independently seeded SipHash runs), so a collision silently dropping a
/// distinct row needs ~2^64 rows.
fn fingerprint(cells: &[String]) -> u128 {
    use std::hash::{Hash, Hasher};
    let mut h1 = std::collections::hash_map::DefaultHasher::new();
    let mut h2 = std::collections::hash_map::DefaultHasher::new();
    0xA5u8.hash(&mut h2);
    for c in cells {
        let t = c.trim();
        t.hash(&mut h1);
        t.len().hash(&mut h1);
        0x5Au8.hash(&mut h2);
        t.hash(&mut h2);
    }
    ((h1.finish() as u128) << 64) | h2.finish() as u128
}

/// One **undirected hyperedge per row** over the nodes named by `members` (all other cells become typed edge
/// properties, `time_cols` are converted to epoch seconds). This is the natural model for event tables where a
/// row relates several entities at once, and it keeps the co-occurrence structure that a star of binary edges
/// loses, so `spectral.*`, `physics.*` and `algo.*` see it. A row with fewer than two distinct members, an
/// unparsable timestamp or a non-numeric weight is skipped and counted.
pub fn import_csv_hyperedges(db: &Database, o: &HyperCsv, csv: &str) -> Result<ImportReport> {
    import_csv_hyperedges_with(db, o, csv, &HyperOpts::default())
}

/// [`import_csv_hyperedges`] with row de-duplication and optional member properties (v0.10).
/// Real event exports often repeat records (the Enterprise Cyber Kill Chain file has 500 exact repeats among
/// 100,500 rows); with [`Dedup::None`] they would be double-counted by every aggregate.
pub fn import_csv_hyperedges_with(db: &Database, o: &HyperCsv, csv: &str, opts: &HyperOpts) -> Result<ImportReport> {
    let rows = parse_csv(csv, opts.delimiter.unwrap_or(','))?;
    let cols = header(&rows)?;
    let mem: Vec<(usize, &str)> = o.members.iter().map(|(c, l)| col_index(&cols, c, "member").map(|i| (i, *l))).collect::<Result<_>>()?;
    if mem.len() < if opts.allow_single_member { 1 } else { 2 } {
        return Err(Error::Invalid(if opts.allow_single_member { "import_csv_hyperedges needs at least one member column" } else { "import_csv_hyperedges needs at least two member columns (set allow_single_member for a one-entity observation table)" }.into()));
    }
    let wc = o.weight_col.map(|w| col_index(&cols, w, "weight")).transpose()?;
    let tcs: Vec<usize> = o.time_cols.iter().map(|c| col_index(&cols, c, "time")).collect::<Result<_>>()?;
    let mut known: HashMap<&str, HashMap<String, Id>> = HashMap::new();
    for (_, l) in &mem {
        known.entry(l).or_insert_with(|| existing(db, l, o.key_prop));
    }
    // qualifier: member column index -> parent column indices (validated before anything is read)
    let mut qual: HashMap<usize, Vec<usize>> = HashMap::new();
    for (col, parents) in opts.qualify {
        let ci = col_index(&cols, col, "qualified member")?;
        if !mem.iter().any(|m| m.0 == ci) {
            return Err(Error::Invalid(format!("qualify: '{col}' is not a member column")));
        }
        let pi: Vec<usize> = parents.iter().map(|p| col_index(&cols, p, "qualifier")).collect::<Result<_>>()?;
        if pi.contains(&ci) {
            return Err(Error::Invalid(format!("qualify: '{col}' cannot qualify itself")));
        }
        qual.insert(ci, pi);
    }
    let str_idx: HashSet<usize> = opts.string_cols.iter().map(|c| col_index(&cols, c, "string column")).collect::<Result<_>>()?;
    let skip_cols: HashSet<usize> = if opts.keep_member_props { wc.into_iter().collect() } else { mem.iter().map(|m| m.0).chain(wc).collect() };
    let dedup_key_col = match opts.dedup {
        Dedup::Key(c) => Some(col_index(&cols, c, "dedup key")?),
        _ => None,
    };
    let mut seen_rows: HashSet<u128> = HashSet::new();
    let mut seen_keys: HashMap<String, u128> = HashMap::new();
    let mut rep = ImportReport::default();
    let mut tx = db.begin();
    let mut seen_labels = HashSet::new();
    for (_, l) in &mem {
        if seen_labels.insert(*l) {
            tx.create_property_index(l, o.key_prop);
        }
    }
    for (line, r) in rows.iter().skip(1) {
        rep.rows_read += 1;
        if r.len() != cols.len() {
            rep.skip(*line, format!("{} fields, header has {}", r.len(), cols.len()));
            continue;
        }
        let w = match wc.map(|i| r[i].trim()).filter(|s| !s.is_empty()) {
            None => 1.0,
            Some(s) => match s.parse::<f64>() {
                Ok(x) if x.is_finite() => x,
                _ => {
                    rep.skip(*line, format!("weight '{s}' is not a number"));
                    continue;
                }
            },
        };
        let mut p = Props::new();
        let mut bad_time = None;
        for (i, c) in cols.iter().enumerate() {
            if skip_cols.contains(&i) || r[i].trim().is_empty() {
                continue;
            }
            if tcs.contains(&i) {
                match crate::timeutil::parse_timestamp_ordered(&r[i], opts.date_order) {
                    Some(t) => {
                        p.insert(c.clone(), Value::Float(t));
                    }
                    None => {
                        bad_time = Some(format!(
                            "column '{c}': '{}' is not {}",
                            r[i],
                            match opts.date_order {
                                crate::timeutil::DateOrder::Iso => "an ISO 8601 date or timestamp (2025-10-16, 2025-10-16 13:05:00); if it is month/day/year or day/month/year, say so with the date format option",
                                crate::timeutil::DateOrder::MonthFirst => "a month/day/year date (11/8/2016, optionally with a time)",
                                crate::timeutil::DateOrder::DayFirst => "a day/month/year date (8/11/2016, optionally with a time)",
                            }
                        ))
                    }
                }
            } else if str_idx.contains(&i) {
                p.insert(c.clone(), Value::Str(r[i].trim().to_string()));
            } else if let Some(v) = infer_value(&r[i]) {
                p.insert(c.clone(), v);
            }
        }
        if let Some(why) = bad_time {
            rep.skip(*line, why);
            continue;
        }
        // distinct (label, value) members in column order; judge the row before creating anything
        let mut want: Vec<(&str, String, Value)> = vec![];
        let mut bad_parent = None;
        for (i, l) in &mem {
            if let Some(v) = infer_value(&r[*i]) {
                let v = match qual.get(i) {
                    None => v,
                    Some(parents) => {
                        let mut parts: Vec<String> = parents.iter().map(|&pi| r[pi].trim().to_string()).collect();
                        if let Some(empty) = parents.iter().zip(&parts).find(|(_, t)| t.is_empty()) {
                            bad_parent = Some(format!("qualifier column '{}' is empty for '{}'", cols[*empty.0], key_string(&v)));
                            break;
                        }
                        parts.push(key_string(&v));
                        Value::Str(parts.join("/"))
                    }
                };
                let k = key_string(&v);
                if !want.iter().any(|w| w.0 == *l && w.1 == k) {
                    want.push((l, k, v));
                }
            }
        }
        if let Some(why) = bad_parent {
            rep.skip(*line, why);
            continue;
        }
        if want.len() < if opts.allow_single_member { 1 } else { 2 } {
            rep.skip(*line, if opts.allow_single_member { "no member value" } else { "fewer than two distinct members" });
            continue;
        }
        match opts.dedup {
            Dedup::None => {}
            Dedup::ExactRow => {
                if !seen_rows.insert(fingerprint(r)) {
                    rep.duplicates_skipped += 1;
                    continue;
                }
            }
            Dedup::Key(_) => {
                let k = r[dedup_key_col.unwrap()].trim();
                if !k.is_empty() {
                    let fp = fingerprint(r);
                    match seen_keys.get(k) {
                        Some(&old) if old == fp => {
                            rep.duplicates_skipped += 1;
                            continue;
                        }
                        Some(_) => {
                            rep.key_conflicts += 1;
                            rep.skip(*line, format!("key '{k}' already imported with different content (first row kept)"));
                            continue;
                        }
                        None => {
                            seen_keys.insert(k.to_string(), fp);
                        }
                    }
                }
            }
        }
        let mut ids: Vec<Id> = Vec::with_capacity(want.len());
        for (l, k, v) in want {
            let id = match known[l].get(&k) {
                Some(&id) => id,
                None => {
                    let id = tx.create_node(&[l], std::iter::once((o.key_prop.to_string(), v)).collect())?;
                    known.get_mut(l).unwrap().insert(k, id);
                    rep.nodes_created += 1;
                    id
                }
            };
            ids.push(id);
        }
        tx.create_hyperedge(o.edge_label, &ids, &[], w, p)?;
        rep.edges_created += 1;
        if opts.chunk_rows > 0 && rep.edges_created % opts.chunk_rows == 0 {
            rep.ts = commit(tx, &rep).map_err(|e| Error::Invalid(format!("{e} (chunked import: {} rows were already committed)", rep.edges_created - opts.chunk_rows)))?;
            tx = db.begin();
        }
    }
    rep.ts = commit(tx, &rep).map_err(|e| if opts.chunk_rows > 0 { Error::Invalid(format!("{e} (chunked import: {} rows were already committed)", rep.edges_created - rep.edges_created % opts.chunk_rows)) } else { e })?;
    Ok(rep)
}

// ------------------------------------------------------------- events

/// Supported JSON-lines telemetry formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    /// Suricata `eve.json`: `flow`, `alert`, `dns`, `http`, `tls` events; others are skipped.
    SuricataEve,
    /// Zeek logs written as JSON (`LogAscii::use_json`): `conn.log` and `dns.log` records.
    ZeekJson,
}

impl std::str::FromStr for Preset {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "suricata" | "eve" | "suricata-eve" => Ok(Preset::SuricataEve),
            "zeek" | "zeek-json" | "bro" => Ok(Preset::ZeekJson),
            o => Err(Error::Query(format!("unknown event preset '{o}' (use suricata or zeek)"))),
        }
    }
}

pub use crate::timeutil::parse_timestamp;

use serde_json::Value as J;

fn path<'a>(j: &'a J, p: &str) -> Option<&'a J> {
    // Zeek JSON keeps dotted names as literal keys ("id.orig_h"); Suricata nests objects.
    if let Some(v) = j.get(p) {
        return Some(v);
    }
    p.split('.').try_fold(j, |c, k| c.get(k))
}

fn s_at(j: &J, p: &str) -> Option<String> {
    match path(j, p)? {
        J::String(s) if !s.is_empty() => Some(s.clone()),
        J::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn f_at(j: &J, p: &str) -> Option<f64> {
    path(j, p)?.as_f64().filter(|x| x.is_finite())
}

fn put_s(p: &mut Props, k: &str, v: Option<String>) {
    if let Some(v) = v {
        p.insert(k.into(), Value::Str(v));
    }
}

fn put_n(p: &mut Props, k: &str, v: Option<f64>) {
    if let Some(v) = v {
        p.insert(k.into(), if v.fract() == 0.0 && v.abs() < 9e15 { Value::Int(v as i64) } else { Value::Float(v) });
    }
}

struct Ev {
    label: &'static str,
    src: String,
    dst: String,
    dst_is_domain: bool,
    props: Props,
}

fn eve_event(j: &J) -> std::result::Result<Option<Ev>, String> {
    let ty = s_at(j, "event_type").ok_or("no event_type")?;
    let ts = s_at(j, "timestamp").and_then(|t| parse_timestamp(&t)).ok_or("missing or unparsable timestamp")?;
    let mut p = Props::new();
    p.insert("ts".into(), Value::Float(ts));
    let src = s_at(j, "src_ip");
    let dst = s_at(j, "dest_ip");
    let host_pair = |src: Option<String>, dst: Option<String>| -> std::result::Result<(String, String), String> { Ok((src.ok_or("missing src_ip")?, dst.ok_or("missing dest_ip")?)) };
    match ty.as_str() {
        "flow" => {
            let (s, d) = host_pair(src, dst)?;
            put_s(&mut p, "proto", s_at(j, "proto"));
            put_n(&mut p, "dst_port", f_at(j, "dest_port"));
            put_s(&mut p, "service", s_at(j, "app_proto"));
            let (bo, bi) = (f_at(j, "flow.bytes_toserver").unwrap_or(0.0), f_at(j, "flow.bytes_toclient").unwrap_or(0.0));
            put_n(&mut p, "bytes_out", Some(bo));
            put_n(&mut p, "bytes_in", Some(bi));
            put_n(&mut p, "bytes", Some(bo + bi));
            put_s(&mut p, "state", s_at(j, "flow.state"));
            Ok(Some(Ev { label: "FLOW", src: s, dst: d, dst_is_domain: false, props: p }))
        }
        "alert" => {
            let (s, d) = host_pair(src, dst)?;
            put_s(&mut p, "signature", s_at(j, "alert.signature"));
            put_n(&mut p, "signature_id", f_at(j, "alert.signature_id"));
            put_n(&mut p, "severity", f_at(j, "alert.severity"));
            put_s(&mut p, "category", s_at(j, "alert.category"));
            put_s(&mut p, "proto", s_at(j, "proto"));
            put_n(&mut p, "dst_port", f_at(j, "dest_port"));
            // MITRE ATT&CK technique when the rule metadata carries one
            if let Some(m) = path(j, "alert.metadata.mitre_technique_id").and_then(|v| v.as_array()).and_then(|a| a.first()).and_then(|v| v.as_str()) {
                p.insert("mitre".into(), Value::Str(m.to_string()));
            }
            Ok(Some(Ev { label: "ALERT", src: s, dst: d, dst_is_domain: false, props: p }))
        }
        "dns" => {
            if s_at(j, "dns.type").as_deref() == Some("answer") {
                return Ok(None); // count each lookup once, via the query
            }
            let s = src.ok_or("missing src_ip")?;
            let d = s_at(j, "dns.rrname").ok_or("dns event without rrname")?.to_ascii_lowercase();
            put_s(&mut p, "qtype", s_at(j, "dns.rrtype"));
            Ok(Some(Ev { label: "QUERY", src: s, dst: d, dst_is_domain: true, props: p }))
        }
        "http" => {
            let s = src.ok_or("missing src_ip")?;
            let d = s_at(j, "http.hostname").ok_or("http event without hostname")?.to_ascii_lowercase();
            put_s(&mut p, "method", s_at(j, "http.http_method"));
            put_s(&mut p, "url", s_at(j, "http.url"));
            put_n(&mut p, "status", f_at(j, "http.status"));
            put_s(&mut p, "user_agent", s_at(j, "http.http_user_agent"));
            Ok(Some(Ev { label: "HTTP", src: s, dst: d, dst_is_domain: true, props: p }))
        }
        "tls" => {
            let s = src.ok_or("missing src_ip")?;
            let d = s_at(j, "tls.sni").ok_or("tls event without sni")?.to_ascii_lowercase();
            put_s(&mut p, "version", s_at(j, "tls.version"));
            put_s(&mut p, "ja3", s_at(j, "tls.ja3.hash"));
            Ok(Some(Ev { label: "TLS", src: s, dst: d, dst_is_domain: true, props: p }))
        }
        _ => Ok(None),
    }
}

fn zeek_event(j: &J) -> std::result::Result<Option<Ev>, String> {
    let ts = f_at(j, "ts").ok_or("missing numeric ts")?;
    let src = s_at(j, "id.orig_h").ok_or("missing id.orig_h")?;
    let mut p = Props::new();
    p.insert("ts".into(), Value::Float(ts));
    if let Some(q) = s_at(j, "query") {
        put_s(&mut p, "qtype", s_at(j, "qtype_name"));
        return Ok(Some(Ev { label: "QUERY", src, dst: q.to_ascii_lowercase(), dst_is_domain: true, props: p }));
    }
    let dst = s_at(j, "id.resp_h").ok_or("missing id.resp_h")?;
    put_s(&mut p, "proto", s_at(j, "proto"));
    put_n(&mut p, "dst_port", f_at(j, "id.resp_p"));
    put_s(&mut p, "service", s_at(j, "service"));
    put_s(&mut p, "state", s_at(j, "conn_state"));
    let (bo, bi) = (f_at(j, "orig_bytes").unwrap_or(0.0), f_at(j, "resp_bytes").unwrap_or(0.0));
    put_n(&mut p, "bytes_out", Some(bo));
    put_n(&mut p, "bytes_in", Some(bi));
    put_n(&mut p, "bytes", Some(bo + bi));
    put_n(&mut p, "duration", f_at(j, "duration"));
    Ok(Some(Ev { label: "FLOW", src, dst, dst_is_domain: false, props: p }))
}

/// Load JSON-lines telemetry (see the module docs for the resulting graph). Hosts are shared across
/// events (one `Host` per IP, one `Domain` per name), including ones already in the database.
/// Event types the preset does not model are counted in `rows_read` but not as skipped.
pub fn import_events(db: &Database, jsonl: &str, preset: Preset) -> Result<ImportReport> {
    let mut hosts = existing(db, "Host", "ip");
    let mut domains = existing(db, "Domain", "name");
    let mut rep = ImportReport::default();
    let mut tx = db.begin();
    for (i, line) in jsonl.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        rep.rows_read += 1;
        let j: J = match serde_json::from_str(line) {
            Ok(j) => j,
            Err(e) => {
                rep.skip(i + 1, format!("invalid JSON ({e})"));
                continue;
            }
        };
        let ev = match preset {
            Preset::SuricataEve => eve_event(&j),
            Preset::ZeekJson => zeek_event(&j),
        };
        let ev = match ev {
            Ok(Some(e)) => e,
            Ok(None) => continue,
            Err(why) => {
                rep.skip(i + 1, why);
                continue;
            }
        };
        let mut node = |tx: &mut Txn, rep: &mut ImportReport, is_domain: bool, v: &str| -> Result<Id> {
            let (map, label, key) = if is_domain { (&mut domains, "Domain", "name") } else { (&mut hosts, "Host", "ip") };
            if let Some(&id) = map.get(v) {
                return Ok(id);
            }
            let id = tx.create_node(&[label], std::iter::once((key.to_string(), Value::Str(v.to_string()))).collect())?;
            map.insert(v.to_string(), id);
            rep.nodes_created += 1;
            Ok(id)
        };
        let a = node(&mut tx, &mut rep, false, &ev.src)?;
        let b = node(&mut tx, &mut rep, ev.dst_is_domain, &ev.dst)?;
        tx.create_edge(ev.label, a, b, 1.0, ev.props)?;
        rep.edges_created += 1;
    }
    rep.ts = commit(tx, &rep)?;
    Ok(rep)
}
