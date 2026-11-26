//! Power BI / BI tables: projection correctness, typing, CSV safety, time travel, M scripts. No PostgreSQL needed.

use nexgraph::bi::{self, ColType};
use nexgraph::{Database, Value};

fn sample() -> Database {
    let db = Database::in_memory();
    db.query(
        "CREATE (a:Host {ip:'10.0.0.1', risk: 0.5, ports: 3, up: true}), \
                (b:Host {ip:'10.0.0.2', risk: 2, ports: 4, up: false}), \
                (c:Host {ip:'10.0.0.3', ports: 'many', id: 99}), \
                (a)-[:FLOW {weight: 2.5, bytes: 100}]->(b), (b)-[:FLOW {bytes: 7}]->(c)",
    )
    .unwrap();
    // NQL has no multi-label syntax; add the second label through the transaction API.
    let r = db.query("MATCH (c:Host) WHERE c.ip='10.0.0.3' RETURN id(c)").unwrap();
    let cid = match r.rows[0][0] {
        Value::Int(i) => i as u64,
        Value::Node(i) => i,
        ref o => panic!("{o:?}"),
    };
    let mut tx = db.begin();
    tx.add_label(cid, "Crown");
    tx.commit().unwrap();
    let mut p = nexgraph::Params::new();
    p.insert("n".into(), Value::Str("has, comma and \"quote\"".into()));
    db.query_with("MATCH (a:Host) WHERE a.ip='10.0.0.1' SET a.note = $n", &p).unwrap();
    db.query("MATCH (a),(b),(c) WHERE a.ip='10.0.0.1' AND b.ip='10.0.0.2' AND c.ip='10.0.0.3' CREATE HYPEREDGE m:Meeting {t:'x'} OVER (a,b,c)").unwrap();
    db
}

fn col<'a>(t: &'a bi::BiTable, n: &str) -> usize {
    t.columns.iter().position(|c| c.name == n).unwrap_or_else(|| panic!("no column {n} in {:?}", t.columns))
}

#[test]
fn table_names_are_fixed_then_per_label() {
    let db = sample();
    let names = db.read(|v| bi::table_names(v));
    assert_eq!(&names[..3], ["nodes", "edges", "edge_members"]);
    for n in ["node_Crown", "node_Host", "edge_FLOW", "edge_Meeting"] {
        assert!(names.iter().any(|x| x == n), "{n} missing from {names:?}");
    }
}

#[test]
fn fixed_tables_match_the_graph() {
    let db = sample();
    let (nodes, _) = bi::snapshot_table(&db, "nodes", None).unwrap();
    assert_eq!(nodes.rows.len(), 3);
    let (edges, _) = bi::snapshot_table(&db, "edges", None).unwrap();
    assert_eq!(edges.rows.len(), 3);
    let (c_src, c_dst, c_dir, c_label) = (col(&edges, "src"), col(&edges, "dst"), col(&edges, "directed"), col(&edges, "label"));
    for r in &edges.rows {
        match &r[c_label] {
            Value::Str(l) if l == "Meeting" => {
                assert_eq!(r[c_src], Value::Null, "hyperedge has no src");
                assert_eq!(r[c_dst], Value::Null);
                assert_eq!(r[c_dir], Value::Bool(false), "empty head = undirected");
            }
            _ => {
                assert!(matches!(r[c_src], Value::Int(_)) && matches!(r[c_dst], Value::Int(_)));
                assert_eq!(r[c_dir], Value::Bool(true));
            }
        }
    }
    // Bridge table: 2 binary edges x (1 tail + 1 head) + undirected hyperedge with 3 members.
    let (m, _) = bi::snapshot_table(&db, "edge_members", None).unwrap();
    assert_eq!(m.rows.len(), 2 * 2 + 3);
    let role = col(&m, "role");
    assert_eq!(m.rows.iter().filter(|r| r[role] == Value::Str("head".into())).count(), 2);
}

#[test]
fn per_label_columns_are_typed_by_inference() {
    let db = sample();
    let (t, _) = bi::snapshot_table(&db, "node_Host", None).unwrap();
    let ty = |n: &str| t.columns[col(&t, n)].ty;
    assert_eq!(ty("risk"), ColType::Float, "0.5 and 2 widen to double");
    assert_eq!(ty("up"), ColType::Bool);
    assert_eq!(ty("ports"), ColType::Text, "3, 4 and 'many' mix, so text");
    assert_eq!(ty("ip"), ColType::Text);
    // a property named like a fixed column is renamed, not lost
    assert_eq!(t.columns.iter().filter(|c| c.name == "id").count(), 1);
    let pid = col(&t, "prop_id");
    assert_eq!(t.rows.iter().filter(|r| r[pid] == Value::Int(99)).count(), 1);
    // widened ints are real doubles; absent values are null
    let risk = col(&t, "risk");
    let ip = col(&t, "ip");
    for r in &t.rows {
        match &r[ip] {
            Value::Str(s) if s == "10.0.0.2" => assert_eq!(r[risk], Value::Float(2.0)),
            Value::Str(s) if s == "10.0.0.3" => assert_eq!(r[risk], Value::Null),
            _ => {}
        }
    }
    // mixed column keeps ints readable as text
    let ports = col(&t, "ports");
    assert!(t.rows.iter().any(|r| r[ports] == Value::Str("many".into())));
    assert!(t.rows.iter().any(|r| r[ports] == Value::Str("3".into())));
}

#[test]
fn edge_label_table_has_weight_src_dst() {
    let db = sample();
    let (t, _) = bi::snapshot_table(&db, "edge_FLOW", None).unwrap();
    assert_eq!(t.rows.len(), 2);
    let (w, bytes) = (col(&t, "weight"), col(&t, "bytes"));
    let mut ws: Vec<f64> = t.rows.iter().map(|r| if let Value::Float(f) = r[w] { f } else { panic!() }).collect();
    ws.sort_by(|a, b| a.partial_cmp(b).unwrap());
    assert_eq!(ws, vec![1.0, 2.5]);
    assert_eq!(t.columns[bytes].ty, ColType::Int);
}

#[test]
fn csv_quotes_and_keeps_every_row() {
    let db = sample();
    let (t, _) = bi::snapshot_table(&db, "node_Host", None).unwrap();
    let csv = t.to_csv();
    assert!(csv.contains("\"has, comma and \"\"quote\"\"\""), "{csv}");
    assert_eq!(csv.matches("\r\n").count(), 1 + 3, "header + 3 rows");
}

#[test]
fn props_json_is_valid_and_complete() {
    let db = sample();
    let (t, _) = bi::snapshot_table(&db, "nodes", None).unwrap();
    let p = col(&t, "props");
    let a = t.rows.iter().find_map(|r| match &r[p] {
        Value::Str(s) if s.contains("\"risk\":0.5") => Some(s.clone()),
        _ => None,
    });
    let a = a.expect("node a's props");
    assert!(a.starts_with('{') && a.ends_with('}'));
    assert!(a.contains("\"note\":\"has, comma and \\\"quote\\\"\""), "{a}");
    assert!(a.contains("\"up\":true"));
}

#[test]
fn unknown_table_is_an_error_and_names_are_exact() {
    let db = sample();
    assert!(bi::snapshot_table(&db, "nope", None).is_err());
    assert!(bi::snapshot_table(&db, "NODES", None).is_err());
    assert!(bi::snapshot_table(&db, "node_../etc", None).is_err());
}

#[test]
fn awkward_labels_get_safe_unique_names() {
    let db = Database::in_memory();
    db.query("CREATE (:`A b`), (:`A-b`), (:`nodes`)").ok(); // backticks may not parse; fall back to the Rust API below
    let mut tx = db.begin();
    tx.create_node(&["A b"], Default::default()).unwrap();
    tx.create_node(&["A-b"], Default::default()).unwrap();
    tx.create_node(&["s/../x"], Default::default()).unwrap();
    tx.commit().unwrap();
    let names = db.read(|v| bi::table_names(v));
    let per: Vec<&String> = names.iter().skip(3).collect();
    assert!(per.iter().all(|n| n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')), "{per:?}");
    let uniq: std::collections::BTreeSet<_> = per.iter().map(|s| s.to_lowercase()).collect();
    assert_eq!(uniq.len(), per.len(), "names collide: {per:?}");
    for n in &names {
        assert!(bi::snapshot_table(&db, n, None).is_ok(), "{n}");
    }
}

#[test]
fn empty_database_yields_header_only_tables() {
    let db = Database::in_memory();
    for n in ["nodes", "edges", "edge_members"] {
        let (t, _) = bi::snapshot_table(&db, n, None).unwrap();
        assert!(t.rows.is_empty());
        assert!(t.to_csv().ends_with("\r\n") && t.to_csv().matches("\r\n").count() == 1, "{n}: header only");
    }
}

#[test]
fn time_travel_reads_the_old_state() {
    let db = sample();
    let ts0 = db.last_ts();
    db.query("CREATE (:Host {ip:'10.0.0.9'})").unwrap();
    let (now, ts_now) = bi::snapshot_table(&db, "node_Host", None).unwrap();
    let (then, ts_then) = bi::snapshot_table(&db, "node_Host", Some(ts0)).unwrap();
    assert_eq!(now.rows.len(), 4);
    assert_eq!(then.rows.len(), 3);
    assert_eq!(ts_then, ts0);
    assert!(ts_now > ts0);
    assert!(bi::snapshot_table(&db, "nodes", Some(ts_now + 10)).is_err());
}

#[test]
fn catalog_json_lists_types() {
    let db = sample();
    let (tables, ts) = bi::snapshot_tables(&db, None).unwrap();
    let j = bi::catalog_json(&tables, ts);
    assert!(j.starts_with("{\"ts\":"));
    assert!(j.contains("\"name\":\"node_Host\""));
    assert!(j.contains("{\"name\":\"risk\",\"type\":\"double\"}"));
    assert!(j.contains("{\"name\":\"up\",\"type\":\"boolean\"}"));
}

#[test]
fn export_dir_writes_a_consistent_snapshot_and_scripts() {
    let db = sample();
    let dir = std::env::temp_dir().join(format!("nxbi_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let r = bi::export_dir(&db, &dir).unwrap();
    for (n, rows) in &r.tables {
        let text = std::fs::read_to_string(dir.join(format!("{n}.csv"))).unwrap();
        assert_eq!(text.matches("\r\n").count(), rows + 1, "{n}");
    }
    let cat = std::fs::read_to_string(dir.join("catalog.json")).unwrap();
    assert!(cat.contains("\"edge_members\""));
    let pq = std::fs::read_to_string(dir.join("nexgraph_folder.pq")).unwrap();
    assert!(pq.contains("Csv.Document") && pq.contains("node_Host = Load(\"node_Host\""), "{pq}");
    assert!(pq.contains("{\"risk\", type number}") && pq.contains("{\"up\", type logical}") && pq.contains("{\"id\", Int64.Type}"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn power_query_script_is_balanced_and_has_no_secret() {
    let pq = bi::power_query("https://bi.example.org:8443/");
    assert!(pq.contains("BaseUrl = \"https://bi.example.org:8443\""));
    assert!(pq.contains("v1/bi/tables") && pq.contains("v1/bi/query"));
    assert!(pq.contains("Password = \"change-me\""));
    // M brackets balance outside of string literals.
    let (mut depth, mut in_str) = ([0i32; 3], false);
    let chars: Vec<char> = pq.lines().filter(|l| !l.trim_start().starts_with("//")).collect::<Vec<_>>().join("\n").chars().collect();
    for c in chars {
        if c == '"' {
            in_str = !in_str;
        }
        if in_str {
            continue;
        }
        match c {
            '(' => depth[0] += 1,
            ')' => depth[0] -= 1,
            '[' => depth[1] += 1,
            ']' => depth[1] -= 1,
            '{' => depth[2] += 1,
            '}' => depth[2] -= 1,
            _ => {}
        }
    }
    assert_eq!(depth, [0, 0, 0]);
}
