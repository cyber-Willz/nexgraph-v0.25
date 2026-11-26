//! v0.9: regressions found by running the 100k-event synthetic cyber-attack dataset.
#![cfg(feature = "ingest")]
use nexgraph::ingest::{import_csv_edges, import_csv_hyperedges, parse_timestamp, EdgeCsv, HyperCsv};
use nexgraph::{Database, Error, Value};

fn rows(db: &Database, q: &str) -> Vec<Vec<Value>> {
    db.query(q).unwrap_or_else(|e| panic!("{q}: {e}")).rows
}

/// v0.8 panicked ("Matrix index out of bounds") when Lanczos collapsed on a graph with thousands of components.
#[test]
fn spectral_on_many_components_errors_instead_of_panicking() {
    let db = Database::in_memory();
    let mut csv = String::from("a,b\n");
    for i in 0..3000 {
        csv.push_str(&format!("h{},t{}\n", i, i));
    }
    import_csv_edges(&db, &EdgeCsv { edge_label: "E", src_col: "a", dst_col: "b", node_label: "H", node_key: "ip", weight_col: None }, &csv).unwrap();
    for q in ["CALL spectral.cluster(4) YIELD node, cluster RETURN count(*)", "CALL spectral.fiedler() YIELD node, value RETURN count(*)", "CALL spectral.embedding(5) YIELD * RETURN count(*)"] {
        match db.query(q) {
            Ok(r) => assert!(r.rows[0][0].as_i64().unwrap() > 0, "{q}"),
            Err(Error::Query(m)) | Err(Error::Invalid(m)) => assert!(m.contains("component") || m.contains("converge"), "unhelpful error: {m}"),
            Err(e) => panic!("{q}: unexpected {e}"),
        }
    }
}

#[test]
fn yield_star_returns_all_columns() {
    let db = Database::in_memory();
    db.query("CREATE (a:N {k:1}), (b:N {k:2}), (a)-[:E]->(b)").unwrap();
    let r = db.query("CALL algo.degree() YIELD * RETURN node, in_degree, out_degree ORDER BY out_degree DESC").unwrap();
    assert_eq!(r.rows.len(), 2);
    assert_eq!(r.rows[0][2], Value::Int(1));
}

/// Timestamps imported as text ("2024-04-03 11:13:15.083419") used to make beaconing/timeseries silently empty.
#[test]
fn time_procedures_accept_iso_strings_and_reject_unreadable_ones() {
    let db = Database::in_memory();
    let mut csv = String::from("src,dst,ts\n");
    for i in 0..20 {
        // every 60 s, +-1 s jitter, from 2024-01-01 00:00:00
        let t = 60 * i + (i % 3) - 1 + 1;
        csv.push_str(&format!("10.0.0.1,10.0.0.9,2024-01-01 00:{:02}:{:02}\n", t / 60, t % 60));
    }
    for i in 0..20 {
        let t = (i * i * 7) % 3500; // irregular
        csv.push_str(&format!("10.0.0.2,10.0.0.9,2024-01-01T01:{:02}:{:02}Z\n", t / 60, t % 60));
    }
    import_csv_edges(&db, &EdgeCsv { edge_label: "F", src_col: "src", dst_col: "dst", node_label: "H", node_key: "ip", weight_col: None }, &csv).unwrap();
    let r = rows(&db, "CALL cyber.beaconing('F','ts',8,0.3) YIELD src, events, cv RETURN src.ip, events");
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!(r[0][0], Value::Str("10.0.0.1".into()));
    let base = parse_timestamp("2024-01-01 00:00:00").unwrap();
    assert_eq!(base, 1704067200.0);
    let ts = rows(&db, "CALL stats.timeseries('F','ts',3600) YIELD bucket_start, count RETURN bucket_start, count");
    assert_eq!(ts.len(), 2);
    assert_eq!((ts[0][1].as_i64(), ts[1][1].as_i64()), (Some(20), Some(20)));
    assert_eq!(ts[0][0].as_f64().unwrap(), base);
    // a property that is text but not a time is an error, not an empty result
    db.query("MATCH (a)-[e:F]->(b) SET e.tag = 'alpha'").unwrap();
    for q in ["CALL cyber.beaconing('F','tag') YIELD * RETURN count(*)", "CALL stats.timeseries('F','tag',60) YIELD * RETURN count(*)"] {
        match db.query(q) {
            Err(Error::Query(m)) => assert!(m.contains("ISO 8601") && m.contains("alpha"), "{m}"),
            o => panic!("{q}: expected error, got {o:?}"),
        }
    }
    // numeric epoch seconds keep working, and a missing key is still just empty
    db.query("MATCH (a)-[e:F]->(b) SET e.n = 5.5").unwrap();
    assert!(db.query("CALL stats.timeseries('F','n',60) YIELD * RETURN count(*)").is_ok());
    assert_eq!(rows(&db, "CALL stats.timeseries('F','nope',60) YIELD count RETURN count").len(), 0);
}

const EVENTS: &str = "type,industry,country,tool,ip,when,gb,sev\n\
Phishing,Energy,Brazil,Firewall,1.1.1.1,2024-04-03 11:13:15.5,15.3,2\n\
DDoS,Retail,Brazil,WAF,2.2.2.2,2024-04-04 11:13:15,65.0,10\n\
Phishing,Retail,Germany,Firewall,1.1.1.1,2024-04-05T00:00:00Z,7,3\n\
Phishing,,,,,2024-04-05T00:00:00Z,1,1\n\
Malware,Energy,Brazil,WAF,3.3.3.3,not-a-time,1,1\n\
Brute,Energy,Brazil,WAF,3.3.3.3,2024-04-06,x,1\n";

fn hyper(db: &Database) -> nexgraph::ingest::ImportReport {
    let m = [("type", "AttackType"), ("industry", "Industry"), ("country", "Country"), ("tool", "Tool"), ("ip", "Ip")];
    import_csv_hyperedges(db, &HyperCsv { edge_label: "EVENT", members: &m, key_prop: "name", weight_col: Some("gb"), time_cols: &["when"] }, EVENTS).unwrap()
}

#[test]
fn csv_rows_become_hyperedges_over_their_dimensions() {
    let db = Database::in_memory();
    let r = hyper(&db);
    // rows: 1 ok, 2 ok, 3 ok, 4 only one member -> skipped, 5 bad time -> skipped, 6 bad weight -> skipped
    assert_eq!((r.rows_read, r.rows_skipped, r.edges_created), (6, 3, 3), "{r:?}");
    assert!(r.first_error.as_deref().unwrap().contains("fewer than two"), "{:?}", r.first_error);
    // nodes only for the rows that were kept: types {Phishing, DDoS}, industries {Energy, Retail}, countries {Brazil, Germany}, tools {Firewall, WAF}, ips {1.1.1.1, 2.2.2.2}
    assert_eq!(db.stats().live_nodes, 10, "no orphan nodes from skipped rows");
    assert_eq!(rows(&db, "MATCH (n:Industry) RETURN count(*)")[0][0], Value::Int(2));
    // one hyperedge of 5 members per kept row, weight = gb, timestamp stored as epoch seconds
    db.read(|v| {
        let es = v.edges_with_label("EVENT");
        assert_eq!(es.len(), 3);
        let mut sizes: Vec<usize> = es.iter().map(|&e| v.edge(e).unwrap().members().len()).collect();
        sizes.sort();
        assert_eq!(sizes, vec![5, 5, 5]);
    });
    let t = rows(&db, "MATCH (a:AttackType)-[e:EVENT]-(b:Industry) WHERE a.name = 'Phishing' AND b.name = 'Energy' RETURN e.when, e.weight, e.sev");
    assert_eq!(t.len(), 1, "{t:?}");
    assert_eq!(t[0][0], Value::Float(parse_timestamp("2024-04-03 11:13:15.5").unwrap()));
    // co-occurrence through the shared member: Phishing appears in 2 events
    assert_eq!(rows(&db, "MATCH (a:AttackType)-[e:EVENT]-(b:Country) WHERE a.name = 'Phishing' RETURN count(DISTINCT b)")[0][0], Value::Int(2));
    // re-importing reuses the member nodes (no duplicates), adds 3 more hyperedges
    let r2 = hyper(&db);
    assert_eq!((r2.nodes_created, r2.edges_created), (0, 3));
    assert_eq!(db.stats().live_nodes, 10);
    // the epoch-second property feeds the time procedures directly
    let ts = rows(&db, "CALL stats.timeseries('EVENT','when',86400) YIELD count RETURN count");
    assert_eq!(ts.iter().map(|r| r[0].as_i64().unwrap()).sum::<i64>(), 6);
    // errors
    let one = [("type", "T")];
    assert!(import_csv_hyperedges(&db, &HyperCsv { edge_label: "E", members: &one, key_prop: "name", weight_col: None, time_cols: &[] }, EVENTS).is_err());
    let nocol = [("type", "T"), ("nope", "X")];
    assert!(import_csv_hyperedges(&db, &HyperCsv { edge_label: "E", members: &nocol, key_prop: "name", weight_col: None, time_cols: &[] }, EVENTS).is_err());
}

#[test]
fn spectral_sees_hyperedge_co_occurrence() {
    // two groups of dimension values that only ever co-occur inside their own group
    let mut csv = String::from("a,b,c\n");
    for i in 0..40 {
        csv.push_str(&format!("x{},y{},z{}\n", i % 4, i % 3, i % 2));
        csv.push_str(&format!("p{},q{},r{}\n", i % 4, i % 3, i % 2));
    }
    let db = Database::in_memory();
    let m = [("a", "A"), ("b", "B"), ("c", "C")];
    import_csv_hyperedges(&db, &HyperCsv { edge_label: "E", members: &m, key_prop: "k", weight_col: None, time_cols: &[] }, &csv).unwrap();
    let r = rows(&db, "CALL spectral.cluster(2) YIELD node, cluster RETURN node.k, cluster");
    let side = |k: &str| r.iter().find(|x| x[0] == Value::Str(k.into())).map(|x| x[1].clone()).unwrap();
    for g in ["x0", "x1", "y0", "z1"] {
        assert_eq!(side(g), side("x0"), "{g}");
    }
    for g in ["p0", "q1", "r1"] {
        assert_eq!(side(g), side("p0"), "{g}");
    }
    assert_ne!(side("x0"), side("p0"), "the two groups are separated");
}

/// `WHERE a.k = <literal|param>` is answered from the property index, and gives the same rows as a scan.
#[test]
fn where_equality_uses_property_index_with_identical_results() {
    let build = |indexed: bool| {
        let db = Database::in_memory();
        if indexed {
            let mut tx = db.begin();
            tx.create_property_index("P", "k");
            tx.commit().unwrap();
        }
        db.query("CREATE (a:P {k:1, n:'int'}), (b:P {k:1.0, n:'float'}), (c:P {k:'1', n:'str'}), (d:P {n:'missing'}), (e:P {k:2, n:'two'}), (f:Q {k:1, n:'other label'})").unwrap();
        db
    };
    let (plain, idx) = (build(false), build(true));
    let mut p = nexgraph::Params::new();
    p.insert("v".into(), Value::Int(1));
    p.insert("s".into(), Value::Str("1".into()));
    p.insert("nil".into(), Value::Null);
    for q in [
        "MATCH (a:P) WHERE a.k = 1 RETURN a.n ORDER BY a.n",
        "MATCH (a:P) WHERE 1 = a.k RETURN a.n ORDER BY a.n",
        "MATCH (a:P) WHERE a.k = $v RETURN a.n ORDER BY a.n",
        "MATCH (a:P) WHERE a.k = $s RETURN a.n ORDER BY a.n",
        "MATCH (a:P) WHERE a.k = $nil RETURN a.n ORDER BY a.n",
        "MATCH (a:P) WHERE a.k = null RETURN a.n ORDER BY a.n",
        "MATCH (a:P) WHERE a.k = 1 AND a.n = 'float' RETURN a.n",
        "MATCH (a:P) WHERE a.k = 1 OR a.k = 2 RETURN a.n ORDER BY a.n",
        "MATCH (a:P) WHERE NOT a.k = 1 RETURN a.n ORDER BY a.n",
        "MATCH (a:P) WHERE a.k = 2.0 RETURN a.n",
        "MATCH (a:P), (b:P) WHERE a.k = 1 AND b.k = 2 RETURN a.n, b.n ORDER BY a.n",
    ] {
        let (x, y) = (plain.query_with(q, &p).unwrap().rows, idx.query_with(q, &p).unwrap().rows);
        assert_eq!(x, y, "{q}");
    }
    assert_eq!(idx.query_with("MATCH (a:P) WHERE a.k = 1 RETURN count(*)", &p).unwrap().rows[0][0], Value::Int(2));
    // and it is much cheaper: a point lookup among 50k nodes
    let big = Database::in_memory();
    {
        let mut tx = big.begin();
        tx.create_property_index("H", "ip");
        for i in 0..50_000 {
            tx.create_node(&["H"], nexgraph::props([("ip", Value::Str(format!("10.0.{}.{}", i / 250, i % 250)))])).unwrap();
        }
        tx.commit().unwrap();
    }
    let t = std::time::Instant::now();
    for _ in 0..200 {
        assert_eq!(big.query("MATCH (a:H) WHERE a.ip = '10.0.100.7' RETURN count(*)").unwrap().rows[0][0], Value::Int(1));
    }
    assert!(t.elapsed().as_millis() < 500, "200 indexed point queries took {:?}", t.elapsed());
}
