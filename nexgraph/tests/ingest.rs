//! v0.7: importers (CSV, Suricata EVE, Zeek JSON), exporters, and an end-to-end pipeline.
#![cfg(feature = "ingest")]
use nexgraph::ingest::{import_csv_edges, import_csv_nodes, import_events, infer_value, parse_csv, parse_timestamp, EdgeCsv, Preset};
use nexgraph::{Database, Value};

fn q(db: &Database, text: &str) -> Vec<Vec<Value>> {
    db.query(text).unwrap_or_else(|e| panic!("{text}: {e}")).rows
}

#[test]
fn csv_parser_follows_rfc4180() {
    let t = "\u{feff}a,b,c\r\n1,\"x,y\",\"say \"\"hi\"\"\"\r\n\r\n2,\"multi\nline\",\n3,,z";
    let r = parse_csv(t, ',').unwrap();
    let cells: Vec<&Vec<String>> = r.iter().map(|x| &x.1).collect();
    assert_eq!(cells[0], &vec!["a", "b", "c"]);
    assert_eq!(cells[1], &vec!["1", "x,y", "say \"hi\""]);
    assert_eq!(cells[2], &vec!["2", "multi\nline", ""]);
    assert_eq!(cells[3], &vec!["3", "", "z"]);
    assert_eq!(r.iter().map(|x| x.0).collect::<Vec<_>>(), vec![1, 2, 4, 6], "line numbers point at each record's first line");
    assert!(parse_csv("a,b\n\"oops,1\n", ',').is_err());
    assert_eq!(parse_csv("", ',').unwrap().len(), 0);
    // a quoted empty cell on its own is a real (one-column) row, not a blank line
    assert_eq!(parse_csv("h\n\"\"\n", ',').unwrap().len(), 2);
}

#[test]
fn type_inference_keeps_identifiers_intact() {
    assert_eq!(infer_value("42"), Some(Value::Int(42)));
    assert_eq!(infer_value("-7"), Some(Value::Int(-7)));
    assert_eq!(infer_value("3.5"), Some(Value::Float(3.5)));
    assert_eq!(infer_value("1e3"), Some(Value::Float(1000.0)));
    assert_eq!(infer_value("TRUE"), Some(Value::Bool(true)));
    assert_eq!(infer_value(""), None);
    assert_eq!(infer_value("  "), None);
    for s in ["007", "0123456789", "10.0.0.1", "inf", "NaN", "12:30", "+1-555-0100", "99999999999999999999"] {
        assert_eq!(infer_value(s), Some(Value::Str(s.to_string())), "{s} must stay a string");
    }
    assert_eq!(infer_value("0.5"), Some(Value::Float(0.5)));
}

#[test]
fn timestamps_parse_to_epoch_seconds() {
    assert_eq!(parse_timestamp("2000-01-01T00:00:00Z"), Some(946684800.0));
    assert_eq!(parse_timestamp("2024-02-29T12:00:00+0000"), Some(1709208000.0)); // leap day, Suricata offset style
    assert_eq!(parse_timestamp("2026-11-14T10:00:00.250000+0000"), Some(1794650400.25));
    assert_eq!(parse_timestamp("2026-11-14T11:00:00+01:00"), Some(1794650400.0));
    assert_eq!(parse_timestamp("2026-11-14T05:00:00-05:00"), Some(1794650400.0));
    assert_eq!(parse_timestamp("2026-11-14 10:00:00"), Some(1794650400.0));
    assert_eq!(parse_timestamp("1969-12-31T23:59:59Z"), Some(-1.0));
    assert_eq!(parse_timestamp("2100-03-01T00:00:00Z"), Some(4107542400.0));
    // date-only values are midnight UTC (accepted since v0.9; this test still expected None)
    assert_eq!(parse_timestamp("2026-11-14"), Some(1794614400.0));
    for bad in ["", "yesterday", "2026-13-01T00:00:00Z", "2026-11-14T25:00:00Z", "2026-02-30"] {
        assert_eq!(parse_timestamp(bad), None, "{bad}");
    }
}

#[test]
fn csv_nodes_import_and_upsert() {
    let db = Database::in_memory();
    let csv = "id,name,dept,salary\n1,Ada,eng,120.5\n2,Bob,ops,\n3,\"Cy, Jr\",eng,99\nbad,row\n,NoKey,x,1\n";
    let r = import_csv_nodes(&db, "Emp", csv, Some("id")).unwrap();
    assert_eq!((r.rows_read, r.nodes_created, r.nodes_updated, r.rows_skipped), (5, 3, 0, 2));
    assert!(r.first_error.as_deref().unwrap().starts_with("line 5: 2 fields"));
    assert_eq!(q(&db, "MATCH (e:Emp) RETURN count(*)")[0][0].as_i64(), Some(3));
    assert_eq!(q(&db, "MATCH (e:Emp {id:3}) RETURN e.name")[0][0], Value::Str("Cy, Jr".into()));
    assert_eq!(q(&db, "MATCH (e:Emp {id:2}) RETURN e.salary")[0][0], Value::Null, "empty cells are absent, not empty strings");
    // re-import: key matches update in place; only new keys create
    let r2 = import_csv_nodes(&db, "Emp", "id,name\n1,Ada L.\n4,Dee\n", Some("id")).unwrap();
    assert_eq!((r2.nodes_created, r2.nodes_updated), (1, 1));
    assert_eq!(q(&db, "MATCH (e:Emp {id:1}) RETURN e.name, e.salary, e.dept")[0], vec![Value::Str("Ada L.".into()), Value::Float(120.5), Value::Str("eng".into())]);
    assert_eq!(q(&db, "MATCH (e:Emp) RETURN count(*)")[0][0].as_i64(), Some(4));
    // no key: every row is a new node; unknown key column or bad header fails without writing anything
    let before = db.stats().live_nodes;
    assert!(import_csv_nodes(&db, "Emp", "id,name\n9,Zed\n", Some("nope")).is_err());
    assert!(import_csv_nodes(&db, "Emp", "id,id\n1,2\n", None).is_err());
    assert_eq!(db.stats().live_nodes, before);
    import_csv_nodes(&db, "Log", "msg\nx\nx\n", None).unwrap();
    assert_eq!(q(&db, "MATCH (l:Log) RETURN count(*)")[0][0].as_i64(), Some(2));
}

#[test]
fn csv_edges_create_missing_endpoints_and_weights() {
    let db = Database::in_memory();
    import_csv_nodes(&db, "Host", "name,zone\nweb,dmz\ndb,core\n", Some("name")).unwrap();
    let csv = "src,dst,w,proto,port\nweb,db,0.25,tcp,5432\nweb,cache,0.5,tcp,6379\nweb,,1,tcp,1\ndb,web,heavy,tcp,2\n";
    let o = EdgeCsv { edge_label: "CONN", src_col: "src", dst_col: "dst", node_label: "Host", node_key: "name", weight_col: Some("w") };
    let r = import_csv_edges(&db, &o, csv).unwrap();
    assert_eq!((r.edges_created, r.nodes_created, r.rows_skipped), (2, 1, 2));
    assert_eq!(q(&db, "MATCH (h:Host) RETURN count(*)")[0][0].as_i64(), Some(3));
    let rows = q(&db, "MATCH (a:Host {name:'web'})-[c:CONN]->(b) RETURN b.name, c.weight, c.port, c.proto ORDER BY b.name");
    assert_eq!(rows[0], vec![Value::Str("cache".into()), Value::Float(0.5), Value::Int(6379), Value::Str("tcp".into())]);
    assert_eq!(rows[1][0], Value::Str("db".into()));
    assert_eq!(q(&db, "MATCH (h:Host {name:'db'}) RETURN h.zone")[0][0], Value::Str("core".into()), "existing endpoint reused, not duplicated");
}

const EVE: &str = r#"{"timestamp":"2026-11-14T10:00:00.000000+0000","event_type":"flow","src_ip":"10.0.0.5","dest_ip":"203.0.113.9","dest_port":443,"proto":"TCP","app_proto":"tls","flow":{"bytes_toserver":1200,"bytes_toclient":48000,"state":"closed"}}
{"timestamp":"2026-11-14T10:00:05.000000+0000","event_type":"alert","src_ip":"10.0.0.5","dest_ip":"203.0.113.9","dest_port":443,"proto":"TCP","alert":{"signature":"ET MALWARE Cobalt Strike Beacon","signature_id":2027865,"severity":1,"category":"A Network Trojan was detected","metadata":{"mitre_technique_id":["T1071"]}}}
{"timestamp":"2026-11-14T10:00:06.000000+0000","event_type":"dns","src_ip":"10.0.0.5","dest_ip":"10.0.0.2","dns":{"type":"query","rrname":"Evil.Example.COM","rrtype":"A"}}
{"timestamp":"2026-11-14T10:00:06.100000+0000","event_type":"dns","src_ip":"10.0.0.2","dest_ip":"10.0.0.5","dns":{"type":"answer","rrname":"evil.example.com","rrtype":"A"}}
{"timestamp":"2026-11-14T10:00:07.000000+0000","event_type":"http","src_ip":"10.0.0.7","dest_ip":"203.0.113.9","http":{"hostname":"evil.example.com","http_method":"GET","url":"/a","status":200}}
{"timestamp":"2026-11-14T10:00:08.000000+0000","event_type":"tls","src_ip":"10.0.0.7","dest_ip":"203.0.113.9","tls":{"sni":"evil.example.com","version":"TLS 1.3"}}
{"timestamp":"2026-11-14T10:00:09.000000+0000","event_type":"stats","stats":{}}
not json at all
{"timestamp":"garbage","event_type":"flow","src_ip":"10.0.0.8","dest_ip":"10.0.0.9"}
"#;

#[test]
fn suricata_eve_import() {
    let db = Database::in_memory();
    db.query("CREATE (:Host {ip:'10.0.0.5', owner:'alice'})").unwrap();
    let r = import_events(&db, EVE, Preset::SuricataEve).unwrap();
    assert_eq!(r.rows_read, 9);
    assert_eq!(r.rows_skipped, 2, "malformed JSON and the bad timestamp are skipped, stats events are ignored: {r:?}");
    assert_eq!(r.edges_created, 5); // flow, alert, query, http, tls (dns answer ignored)
    assert!(r.first_error.as_deref().unwrap().starts_with("line 8: invalid JSON"));
    // hosts: 10.0.0.5 (pre-existing, reused), 10.0.0.2 (dns server, not referenced since only queries are modelled), 203.0.113.9, 10.0.0.7
    assert_eq!(q(&db, "MATCH (h:Host) RETURN count(*)")[0][0].as_i64(), Some(3));
    assert_eq!(q(&db, "MATCH (h:Host {ip:'10.0.0.5'}) RETURN h.owner")[0][0], Value::Str("alice".into()));
    let f = q(&db, "MATCH (a:Host)-[f:FLOW]->(b:Host) RETURN a.ip, b.ip, f.ts, f.bytes, f.bytes_out, f.dst_port, f.proto, f.service, f.state");
    assert_eq!(f.len(), 1);
    assert_eq!(f[0][2].as_f64(), Some(1794650400.0));
    assert_eq!((f[0][3].as_i64(), f[0][4].as_i64(), f[0][5].as_i64()), (Some(49_200), Some(1200), Some(443)));
    assert_eq!((&f[0][6], &f[0][7], &f[0][8]), (&Value::Str("TCP".into()), &Value::Str("tls".into()), &Value::Str("closed".into())));
    let al = q(&db, "MATCH (a:Host)-[x:ALERT]->(b:Host) RETURN x.signature, x.mitre, x.severity, x.signature_id");
    assert_eq!(al[0], vec![Value::Str("ET MALWARE Cobalt Strike Beacon".into()), Value::Str("T1071".into()), Value::Int(1), Value::Int(2027865)]);
    // one Domain shared by the DNS query, the HTTP Host header and the TLS SNI (names lower-cased)
    assert_eq!(q(&db, "MATCH (d:Domain) RETURN count(*)")[0][0].as_i64(), Some(1));
    assert_eq!(q(&db, "MATCH (h:Host)-[e]->(d:Domain {name:'evil.example.com'}) RETURN count(*)")[0][0].as_i64(), Some(3));
    // importing the same file again just adds more events on the same nodes
    let hosts = db.stats().live_nodes;
    import_events(&db, EVE, Preset::SuricataEve).unwrap();
    assert_eq!(db.stats().live_nodes, hosts);
}

#[test]
fn zeek_json_import() {
    let log = r#"{"ts":1794650400.5,"uid":"C1","id.orig_h":"192.168.1.10","id.orig_p":50000,"id.resp_h":"198.51.100.4","id.resp_p":8443,"proto":"tcp","service":"ssl","duration":2.5,"orig_bytes":300,"resp_bytes":9000,"conn_state":"SF"}
{"ts":1794650401.0,"uid":"C2","id.orig_h":"192.168.1.10","id.orig_p":53000,"id.resp_h":"192.168.1.1","id.resp_p":53,"proto":"udp","query":"Update.Example.org","qtype_name":"A"}
{"ts":1794650402.0,"uid":"C3","id.orig_h":"192.168.1.11","id.resp_h":"198.51.100.4","id.resp_p":22,"proto":"tcp"}
{"uid":"C4","id.orig_h":"192.168.1.12"}
"#;
    let db = Database::in_memory();
    let r = import_events(&db, log, Preset::ZeekJson).unwrap();
    assert_eq!((r.rows_read, r.rows_skipped, r.edges_created), (4, 1, 3));
    let f = q(&db, "MATCH (a:Host {ip:'192.168.1.10'})-[f:FLOW]->(b:Host) RETURN b.ip, f.ts, f.bytes, f.dst_port, f.duration, f.state");
    assert_eq!(f[0], vec![Value::Str("198.51.100.4".into()), Value::Float(1794650400.5), Value::Int(9300), Value::Int(8443), Value::Float(2.5), Value::Str("SF".into())]);
    assert_eq!(q(&db, "MATCH (h:Host)-[:QUERY]->(d:Domain) RETURN d.name")[0][0], Value::Str("update.example.org".into()));
    // a flow with no byte counters records zero bytes rather than dropping the edge
    assert_eq!(q(&db, "MATCH (a:Host {ip:'192.168.1.11'})-[f:FLOW]->(b) RETURN f.bytes")[0][0].as_i64(), Some(0));
    assert!("zeek".parse::<Preset>().is_ok() && "suricata".parse::<Preset>().is_ok() && "x".parse::<Preset>().is_err());
}

#[test]
fn exports_are_well_formed() {
    let db = Database::in_memory();
    let mut tx = db.begin();
    tx.create_node(&["T"], nexgraph::props([("name", Value::from("a,b")), ("note", Value::from("say \"hi\"")), ("n", Value::Int(1)), ("x", Value::Float(2.5))])).unwrap();
    tx.create_node(&["T"], nexgraph::props([("name", Value::from("line\nbreak")), ("n", Value::Int(2))])).unwrap();
    tx.commit().unwrap();
    let r = db.query("MATCH (t:T) RETURN t.name AS name, t.note AS note, t.n AS n, t.x AS x ORDER BY n").unwrap();
    assert_eq!(r.to_csv(), "name,note,n,x\r\n\"a,b\",\"say \"\"hi\"\"\",1,2.5\r\n\"line\nbreak\",,2,\r\n");
    assert_eq!(r.to_json(), r#"[{"name":"a,b","note":"say \"hi\"","n":1,"x":2.5},{"name":"line\nbreak","note":null,"n":2,"x":null}]"#);
    let g = db.query("MATCH (t:T {n:1}) RETURN t, [1, 'a'] AS l").unwrap();
    assert!(g.to_json().contains("\"t\":{\"node\":"));
    assert!(g.to_csv().contains("\"[1,\"\"a\"\"]\""));
}

/// Log lines in, detections out: the whole pipeline on synthetic Zeek telemetry.
#[test]
fn zeek_pipeline_flags_beacon_scan_and_exfil() {
    let mut lines = String::new();
    let mut s = 12345u64;
    let mut rnd = || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    let ts0 = 1_794_650_400.0f64;
    // 30 office hosts browse randomly among 12 servers
    for h in 0..30 {
        for _ in 0..25 {
            let t = ts0 + (rnd() % 86_400) as f64;
            lines.push_str(&format!(r#"{{"ts":{t},"id.orig_h":"10.1.0.{h}","id.resp_h":"10.9.0.{}","id.resp_p":443,"proto":"tcp","orig_bytes":{},"resp_bytes":{}}}"#, rnd() % 12, 500 + rnd() % 3000, 2000 + rnd() % 80_000));
            lines.push('\n');
        }
    }
    // 10.1.0.77 beacons to 198.51.100.66 every 60s (+-5%)
    let mut t = ts0;
    for _ in 0..120 {
        t += 60.0 * (0.95 + (rnd() % 100) as f64 / 1000.0);
        lines.push_str(&format!(r#"{{"ts":{t},"id.orig_h":"10.1.0.77","id.resp_h":"198.51.100.66","id.resp_p":8080,"proto":"tcp","orig_bytes":200,"resp_bytes":150}}"#));
        lines.push('\n');
    }
    // 10.1.0.88 sweeps a /24
    for d in 1..=200 {
        lines.push_str(&format!(r#"{{"ts":{},"id.orig_h":"10.1.0.88","id.resp_h":"10.2.0.{d}","id.resp_p":445,"proto":"tcp","orig_bytes":0,"resp_bytes":0}}"#, ts0 + 5000.0 + d as f64));
        lines.push('\n');
    }
    // 10.1.0.99 uploads 3 GB to one external host
    for i in 0..6 {
        lines.push_str(&format!(r#"{{"ts":{},"id.orig_h":"10.1.0.99","id.resp_h":"192.0.2.50","id.resp_p":443,"proto":"tcp","orig_bytes":500000000,"resp_bytes":1000}}"#, ts0 + 40_000.0 + i as f64));
        lines.push('\n');
    }
    let db = Database::in_memory();
    let r = import_events(&db, &lines, Preset::ZeekJson).unwrap();
    assert_eq!(r.rows_skipped, 0);
    assert_eq!(r.edges_created, 30 * 25 + 120 + 200 + 6);

    let ip = |row: &Vec<Value>, c: usize| match &row[c] {
        Value::Str(s) => s.clone(),
        o => panic!("{o:?}"),
    };
    let b = q(&db, "CALL cyber.beaconing('FLOW', 'ts', 20, 0.25) YIELD src, dst, events, cv RETURN src.ip, dst.ip, events, cv");
    assert_eq!(b.len(), 1, "{b:?}");
    assert_eq!((ip(&b[0], 0), ip(&b[0], 1), b[0][2].as_i64()), ("10.1.0.77".into(), "198.51.100.66".into(), Some(120)));
    let fo = q(&db, "CALL cyber.fanout('FLOW') YIELD node, distinct_targets, robust_z RETURN node.ip, distinct_targets, robust_z LIMIT 1");
    assert_eq!((ip(&fo[0], 0), fo[0][1].as_i64()), ("10.1.0.88".into(), Some(200)));
    let vo = q(&db, "CALL cyber.volume('FLOW', 'bytes') YIELD node, total, robust_z RETURN node.ip, total LIMIT 1");
    assert_eq!(ip(&vo[0], 0), "10.1.0.99");
    // the sweep and the beacon also stand out as event-rate spikes in 10-minute buckets
    let sp = q(&db, "CALL stats.spikes('FLOW', 'ts', 600) YIELD bucket_start, count, robust_z RETURN bucket_start, count");
    assert!(sp.iter().any(|r| r[0].as_f64() == Some((ts0 + 5000.0).div_euclid(600.0) * 600.0)), "sweep bucket flagged: {sp:?}");
}
