//! v0.14 `swarm.detect`. `reference_*` replay `tests/fixtures/swarm_expected.json`, produced by
//! `tests/fixtures/swarm_expected.py` (numpy only, same splitmix64 stream for the Monte Carlo p-values) on
//! (a) a seeded simulation with planted formations, a crossing drone and a milling crowd, and
//! (b) the supplied `synthetic_drone_swarm_dataset.csv` (20 drones x 1000 steps).
#![cfg(feature = "ingest")]
use nexgraph::ingest::{import_csv_hyperedges_with, Dedup, HyperCsv, HyperOpts};
use nexgraph::{Database, Value};
use serde_json::Value as J;

fn fx(name: &str) -> String {
    std::fs::read_to_string(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}
fn load(csv: &str, id_col: &str) -> Database {
    let db = Database::in_memory();
    let o = HyperCsv { edge_label: "OBS", members: &[(id_col, "Track")], key_prop: "name", weight_col: None, time_cols: &[] };
    import_csv_hyperedges_with(&db, &o, csv, &HyperOpts { dedup: Dedup::None, keep_member_props: true, string_cols: &[id_col], allow_single_member: true, ..Default::default() }).unwrap();
    db
}
fn s(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        o => format!("{o:?}"),
    }
}
fn n(v: &Value) -> f64 {
    match v {
        Value::Int(i) => *i as f64,
        Value::Float(f) => *f,
        o => panic!("not a number: {o:?}"),
    }
}
fn close(a: f64, b: f64, what: &str) {
    assert!((a - b).abs() <= 1e-9 * b.abs().max(1.0), "{what}: got {a}, want {b}");
}
fn run(db: &Database, q: &str) -> (Vec<String>, Vec<Vec<Value>>) {
    let r = db.query(q).unwrap_or_else(|e| panic!("{q}: {e}"));
    (r.columns, r.rows)
}
fn check(db: &Database, ids: (&str, &str, &str), win: f64, opts: &str, want: &J, what: &str) {
    let (c, rows) = run(db, &format!("CALL swarm.detect('OBS','Track','{}','{}','{}','{}',{win},'{opts}') YIELD *", ids.0, ids.1, ids.2, "z"));
    let col = |name: &str| c.iter().position(|x| x == name).unwrap();
    assert_eq!(s(&rows[0][0]), "(summary)");
    assert_eq!(n(&rows[0][col("size")]) as i64, want["tracks"].as_i64().unwrap(), "{what}: tracks");
    assert_eq!(n(&rows[0][col("windows")]) as i64, want["windows"].as_i64().unwrap(), "{what}: windows");
    let groups = want["groups"].as_array().unwrap();
    assert_eq!(rows.len() - 1, groups.len(), "{what}: group count; got {:?}", rows.iter().map(|r| s(&r[col("members")])).collect::<Vec<_>>());
    for (g, r) in groups.iter().zip(&rows[1..]) {
        let mem: Vec<&str> = g["members"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
        let shown = s(&r[col("members")]);
        let want_shown = if mem.len() > 12 { format!("{},...", mem[..12].join(",")) } else { mem.join(",") };
        assert_eq!(shown, want_shown, "{what}: members");
        assert_eq!(n(&r[col("size")]) as i64, g["size"].as_i64().unwrap(), "{what}: size");
        assert_eq!(n(&r[col("windows")]) as i64, g["windows"].as_i64().unwrap(), "{what}: windows seen");
        close(n(&r[col("first_window")]), g["first"].as_f64().unwrap(), "first");
        close(n(&r[col("last_window")]), g["last"].as_f64().unwrap(), "last");
        for (k, f) in [("persistence", "persistence"), ("mean_pair_dist", "mean_pair_dist"), ("heading_coherence", "coherence"), ("speed_mean", "speed_mean"), ("speed_cv", "speed_cv")] {
            close(n(&r[col(k)]), g[f].as_f64().unwrap(), &format!("{what}: {k}"));
        }
        for (k, f) in [("p_coherence", "p"), ("p_background", "p_bg")] {
            match g[f].as_f64() {
                Some(w) => close(n(&r[col(k)]), w, &format!("{what}: {k}")),
                None => assert!(matches!(r[col(k)], Value::Null), "{what}: {k} should be null"),
            }
        }
        assert_eq!(s(&r[col("verdict")]), g["verdict"].as_str().unwrap(), "{what}: verdict");
    }
}

#[test]
fn reference_simulation_matches_numpy() {
    let db = load(&fx("swarm_sim.csv"), "id");
    let ex: J = serde_json::from_str(&fx("swarm_expected.json")).unwrap();
    let ids = ("t", "x", "y");
    check(&db, ids, 10.0, "", &ex["sim"]["default"], "default");
    check(&db, ids, 10.0, "eps=8", &ex["sim"]["tight_eps"], "tight_eps");
    check(&db, ids, 10.0, "perm=0", &ex["sim"]["no_perm"], "no_perm");
    check(&db, ids, 10.0, "cos_min=0.95;persist=0.8", &ex["sim"]["strict_cos"], "strict_cos");
    check(&db, ids, 10.0, "dim=2;perm=300", &ex["sim"]["dim2"], "dim2");
    check(&db, ids, 10.0, "eps=40;cos_min=0;persist=0.5;perm=300;filter=truth=M", &ex["sim"]["crowd_only"], "crowd_only");
}

#[test]
fn simulation_recovers_the_planted_swarms_and_nothing_else() {
    let db = load(&fx("swarm_sim.csv"), "id");
    let truth: std::collections::HashMap<String, String> = fx("swarm_sim.csv").lines().skip(1).map(|l| {
        let f: Vec<&str> = l.split(',').collect();
        (f[0].to_string(), f[5].to_string())
    }).collect();
    let (c, rows) = run(&db, "CALL swarm.detect('OBS','Track','t','x','y','z',10) YIELD *");
    let (mi, vi) = (c.iter().position(|x| x == "members").unwrap(), c.iter().position(|x| x == "verdict").unwrap());
    let mut found: Vec<(String, usize)> = Vec::new();
    for r in &rows[1..] {
        let m = s(&r[mi]);
        let kinds: std::collections::BTreeSet<&String> = m.split(',').map(|id| &truth[id]).collect();
        assert_eq!(kinds.len(), 1, "a group mixes planted sets: {m}");
        assert_eq!(s(&r[vi]), "coordinated");
        found.push((kinds.into_iter().next().unwrap().clone(), m.split(',').count()));
    }
    found.sort();
    assert_eq!(found, vec![("A".into(), 8), ("B".into(), 5), ("C".into(), 4)], "swarms A, B, C recovered; crossing drone and milling crowd (M) not reported");
}

#[test]
fn reference_supplied_dataset_matches_numpy() {
    let db = load(&fx("drone_swarm_dataset_20x1000.csv"), "drone_id");
    let ex: J = serde_json::from_str(&fx("swarm_expected.json")).unwrap();
    for win in ["10.0", "50.0", "100.0"] {
        check(&db, ("timestamp", "x", "y"), win.parse().unwrap(), "", &ex["real"][win], &format!("real win {win}"));
    }
    // at win=10 the 20 drones are within a few metres of each other but the jitter swamps the common drift: no group
    let (_, rows) = run(&db, "CALL swarm.detect('OBS','Track','timestamp','x','y','z',10) YIELD *");
    assert_eq!(rows.len(), 1);
    // at win=100 they all move together; the whole population is the swarm, so it is not distinguishable from background
    let (c, rows) = run(&db, "CALL swarm.detect('OBS','Track','timestamp','x','y','z',100) YIELD *");
    assert_eq!(s(&rows[1][c.iter().position(|x| x == "verdict").unwrap()]), "coordinated (whole scene)");
}

#[test]
fn hand_worked_formation_and_opposite_headings() {
    // three tracks at y = 0, 3, 6 flying +x at speed 2 for 12 steps; a fourth at y = 1 flying -x at speed 2 (close, opposite)
    let mut csv = String::from("id,t,x,y,z\n");
    for t in 0..12 {
        for (id, y, vx) in [("a", 0.0, 2.0), ("b", 3.0, 2.0), ("c", 6.0, 2.0), ("d", 1.0, -2.0)] {
            csv.push_str(&format!("{id},{t},{},{y},0\n", vx * t as f64 + if vx < 0.0 { 100.0 } else { 0.0 }));
        }
    }
    let db = load(&csv, "id");
    let (c, rows) = run(&db, "CALL swarm.detect('OBS','Track','t','x','y','z',4,'perm=0;eps=50') YIELD *");
    let col = |name: &str| c.iter().position(|x| x == name).unwrap();
    assert_eq!(rows.len(), 2, "one group {rows:?}");
    let g = &rows[1];
    assert_eq!(s(&g[col("members")]), "a,b,c", "the opposite-heading neighbour d is not part of it");
    assert_eq!(n(&g[col("windows")]) as i64, 3);
    close(n(&g[col("persistence")]), 1.0, "persistence");
    close(n(&g[col("heading_coherence")]), 1.0, "coherence");
    close(n(&g[col("speed_mean")]), 2.0, "speed");
    close(n(&g[col("speed_cv")]), 0.0 + 0.0, "speed cv");
    close(n(&g[col("mean_pair_dist")]), 4.0, "mean pair distance (3, 6, 3)");
    assert_eq!(s(&g[col("verdict")]), "coherent (untested)");
    assert!(matches!(g[col("p_coherence")], Value::Null));
}

#[test]
fn argument_errors_are_reported() {
    let db = load(&fx("swarm_sim.csv"), "id");
    for q in [
        "CALL swarm.detect('OBS','Track','t','x','y','z',0) YIELD *",
        "CALL swarm.detect('OBS','Track','t','x','y','z',-5) YIELD *",
        "CALL swarm.detect('OBS','Track','t','x','y','z',10,'bogus=1') YIELD *",
        "CALL swarm.detect('OBS','Track','t','x','y','z',10,'eps=0') YIELD *",
        "CALL swarm.detect('OBS','Track','t','x','y','z',10,'dim=4') YIELD *",
        "CALL swarm.detect('OBS','Track','t','x','y','z',10,'eps=abc') YIELD *",
        "CALL swarm.detect('OBS','Track','t','x','y','z',10,'max_tracks=5') YIELD *",
        "CALL swarm.detect('OBS','Track','t','x','y','nope',10) YIELD *",
        "CALL swarm.detect('NOPE','Track','t','x','y','z',10) YIELD *",
    ] {
        assert!(db.query(q).is_err(), "should fail: {q}");
    }
}

#[test]
fn scan_runs_each_window_and_matches_detect() {
    let db = load(&fx("swarm_sim.csv"), "id");
    let one = db.query("CALL swarm.detect('OBS','Track','t','x','y','z',10,'') YIELD *").unwrap();
    let sc = db.query("CALL swarm.scan('OBS','Track','t','x','y','z','10,20','') YIELD *").unwrap();
    let g10: Vec<String> = sc.rows.iter().filter(|r| matches!(r[0], Value::Float(w) if w == 10.0)).map(|r| s(&r[3])).collect();
    let d: Vec<String> = one.rows.iter().map(|r| s(&r[2])).collect();
    assert_eq!(g10, d);
    assert!(sc.rows.iter().any(|r| matches!(r[0], Value::Float(w) if w == 20.0)));
    assert!(db.query("CALL swarm.scan('OBS','Track','t','x','y','z','0,5','') YIELD *").is_err());
}
