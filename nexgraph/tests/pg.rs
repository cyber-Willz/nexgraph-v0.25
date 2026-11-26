//! PostgreSQL backend tests. They need a server: set `NEXGRAPH_TEST_PG_URL`
//! (for example `postgres://postgres@127.0.0.1:5433/nexgraph_test`); without it
//! every test prints a notice and returns. Each test uses its own schema and
//! drops it afterwards.
#![cfg(feature = "postgres")]

use nexgraph::model::Body;
use nexgraph::{props, Database, Error, Id, Metric, PgOptions, Props, Value};
use postgres::{Client, NoTls};
use std::time::{Duration, Instant};

fn pg_url() -> Option<String> {
    std::env::var("NEXGRAPH_TEST_PG_URL").ok()
}

macro_rules! need_pg {
    () => {
        match pg_url() {
            Some(u) => u,
            None => {
                eprintln!("NEXGRAPH_TEST_PG_URL not set: skipping");
                return;
            }
        }
    };
}

struct Sch {
    url: String,
    name: String,
}

impl Sch {
    fn new(url: &str, test: &str) -> Sch {
        let name = format!("nxt_{}_{}", test, std::process::id());
        let s = Sch { url: url.to_string(), name };
        s.sql().batch_execute(&format!("DROP SCHEMA IF EXISTS \"{}\" CASCADE", s.name)).unwrap();
        s
    }
    fn sql(&self) -> Client {
        Client::connect(&self.url, NoTls).unwrap()
    }
    fn opts(&self) -> PgOptions {
        PgOptions::default().schema(self.name.clone())
    }
    fn open(&self) -> Database {
        Database::open_postgres_with(&self.url, self.opts()).unwrap()
    }
    fn try_open(&self, o: PgOptions) -> Result<Database, Error> {
        Database::open_postgres_with(&self.url, o)
    }
    fn count(&self, table: &str) -> i64 {
        self.sql().query_one(format!("SELECT count(*) FROM \"{}\".{}", self.name, table).as_str(), &[]).unwrap().get(0)
    }
}

impl Drop for Sch {
    fn drop(&mut self) {
        if let Ok(mut c) = Client::connect(&self.url, NoTls) {
            let _ = c.batch_execute(&format!("DROP SCHEMA IF EXISTS \"{}\" CASCADE", self.name));
        }
    }
}

/// Full live state, comparable across databases.
fn dump(db: &Database) -> Vec<(Id, Body)> {
    db.read(|v| {
        let mut ids: Vec<Id> = v.node_ids().chain(v.edge_ids()).collect();
        ids.sort_unstable();
        ids.into_iter().filter_map(|i| v.body(i).map(|b| (i, b.clone()))).collect()
    })
}

fn dump_at(db: &Database, ts: u64) -> Vec<(Id, Body)> {
    db.read_at(ts, |v| {
        let mut ids: Vec<Id> = v.node_ids().chain(v.edge_ids()).collect();
        ids.sort_unstable();
        ids.into_iter().filter_map(|i| v.body(i).map(|b| (i, b.clone()))).collect()
    })
    .unwrap()
}

#[test]
fn roundtrip_recovery_and_time_travel() {
    let url = need_pg!();
    let s = Sch::new(&url, "roundtrip");
    {
        let db = s.open();
        db.query("CREATE (a:P {n:'a'}), (b:P {n:'b'}), (a)-[:R {weight:3.5}]->(b)").unwrap();
        db.query("CREATE INDEX ON :P(n)").unwrap();
        db.query("MATCH (a:P {n:'a'}) SET a.n = 'a2'").unwrap();
        db.begin().commit().unwrap(); // empty commit changes nothing
        assert_eq!(db.last_ts(), 3);
    }
    {
        let db = s.open();
        assert_eq!(db.last_ts(), 3);
        let r = db.query("MATCH (a:P {n:'a2'})-[r:R]->(b) RETURN b.n, r.weight").unwrap();
        assert_eq!(r.rows[0], vec![Value::Str("b".into()), Value::Float(3.5)]);
        assert_eq!(db.stats().prop_indexes, 1);
        // history survives a plain restart, exactly like the file backend
        assert_eq!(db.query("AS OF 1 MATCH (a:P {n:'a'}) RETURN a").unwrap().rows.len(), 1);
        db.checkpoint().unwrap();
        assert!(db.query("AS OF 1 MATCH (a) RETURN a").is_err(), "history compacted");
        db.query("CREATE (c:P {n:'c'})").unwrap();
    }
    {
        let db = s.open();
        assert_eq!(db.stats().live_nodes, 3);
        assert_eq!(db.last_ts(), 4);
        let before = db.read(|v| v.node_ids().max().unwrap());
        db.query("CREATE (z:P {n:'z'})").unwrap();
        assert!(db.read(|v| v.node_ids().max().unwrap()) > before, "ids keep increasing");
        assert_eq!(db.stats().prop_indexes, 1, "index definitions survive checkpoint + restart");
    }
}

#[test]
fn checkpoint_chunks_and_compaction() {
    let url = need_pg!();
    let s = Sch::new(&url, "chunks");
    let mut o = s.opts();
    o.checkpoint_chunk_bytes = 1024;
    let db = s.try_open(o.clone()).unwrap();
    let mut t = db.begin();
    for i in 0..2000 {
        t.create_node(&["N"], props([("i", Value::Int(i)), ("pad", Value::Str("x".repeat(40)))])).unwrap();
    }
    t.commit().unwrap();
    db.query("MATCH (a:N {i:1}), (b:N {i:2}) CREATE (a)-[:E]->(b)").unwrap();
    assert!(s.count("commits") >= 2);
    let before = dump(&db);
    db.checkpoint().unwrap();
    assert_eq!(s.count("commits"), 0, "commits covered by the checkpoint are dropped");
    let chunks1 = s.count("checkpoint_chunks");
    assert!(chunks1 > 5, "payload is split into several rows, got {chunks1}");
    db.query("CREATE (:N {i:-1})").unwrap();
    db.checkpoint().unwrap();
    let gens: i64 = s.sql().query_one(format!("SELECT count(DISTINCT gen) FROM \"{}\".checkpoint_chunks", s.name).as_str(), &[]).unwrap().get(0);
    assert_eq!(gens, 1, "the previous checkpoint generation is removed");
    db.query("CREATE (:N {i:-2})").unwrap();
    drop(db);
    let db = s.try_open(o).unwrap();
    let after = dump(&db);
    assert_eq!(after.len(), before.len() + 2);
    assert_eq!(&after[..before.len()], &before[..]);
    assert!(db.verify_mirror().unwrap().is_clean());
}

#[test]
fn single_writer_lock_and_read_only_open() {
    let url = need_pg!();
    let s = Sch::new(&url, "lock");
    let other = Sch::new(&url, "lock_other");
    let db = s.open();
    db.query("CREATE (:P {n:1})").unwrap();
    match s.try_open(s.opts()) {
        Err(Error::Backend(m)) => assert!(m.contains("already open for writing"), "{m}"),
        Err(e) => panic!("unexpected error {e}"),
        Ok(_) => panic!("second writer must be refused"),
    }
    // another schema in the same PostgreSQL database is independent
    let db2 = other.open();
    db2.query("CREATE (:Q)").unwrap();
    // a replica opens fine and sees the data
    let ro = s.try_open(s.opts().read_only(true)).unwrap();
    assert_eq!(ro.stats().live_nodes, 1);
    assert!(matches!(ro.query("CREATE (:P {n:2})"), Err(Error::Unsupported(_))));
    assert!(matches!(ro.checkpoint(), Err(Error::Unsupported(_))));
    assert!(matches!(ro.mirror_stats(), Err(Error::Unsupported(_))));
    assert!(matches!(db.refresh(), Err(Error::Unsupported(_))), "the writer does not refresh");
    drop(ro);
    drop(db);
    // closing the writer releases the lock
    let t0 = Instant::now();
    let again = loop {
        match s.try_open(s.opts()) {
            Ok(d) => break d,
            Err(e) => {
                assert!(t0.elapsed() < Duration::from_secs(5), "lock never released: {e}");
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };
    assert_eq!(again.stats().live_nodes, 1);
    // missing schema cannot be opened read-only
    let missing = Sch::new(&url, "lock_missing");
    assert!(missing.try_open(missing.opts().read_only(true)).is_err());
}

#[test]
fn bad_schema_names_are_rejected() {
    let url = need_pg!();
    for bad in ["", "a-b", "x; DROP TABLE y", "1abc", "a\"b", &"x".repeat(64)] {
        let r = Database::open_postgres_with(&url, PgOptions::default().schema(bad));
        assert!(matches!(r, Err(Error::Invalid(_))), "{bad:?} must be rejected");
    }
    assert!(matches!(Database::open_postgres("not a url at all ::"), Err(_)));
}

#[test]
fn mirror_tracks_every_kind_of_change() {
    let url = need_pg!();
    let s = Sch::new(&url, "mirror");
    let db = s.open();
    db.query(
        "CREATE (a:Person {name:'Ada', age:36}), (b:Person {name:'Bob', age:41}), (c:City {name:'Belize'}), \
         (a)-[:KNOWS {since:2001}]->(b), (b)-[:LIVES_IN]->(c)",
    )
    .unwrap();
    let mut t = db.begin();
    let ids: Vec<Id> = db.read(|v| v.nodes_with_label("Person"));
    t.add_label(ids[1], "Admin");
    let hy = t.create_hyperedge("TEAM", &[ids[0], ids[1]], &[], 2.0, props([("k", "v")])).unwrap();
    t.commit().unwrap();
    let mut sql = s.sql();
    let n = &s.name;
    let q1 = |sql: &mut Client, q: &str| -> i64 { sql.query_one(q, &[]).unwrap().get(0) };
    assert_eq!(q1(&mut sql, &format!("SELECT count(*) FROM \"{n}\".nodes")), 3);
    assert_eq!(q1(&mut sql, &format!("SELECT count(*) FROM \"{n}\".edges")), 3);
    assert_eq!(q1(&mut sql, &format!("SELECT count(*) FROM \"{n}\".edge_members")), 6);
    // plain SQL over the graph
    let r = sql
        .query_one(
            &format!(
                "SELECT s.props->>'name', d.props->>'name' FROM \"{n}\".binary_edges e \
                 JOIN \"{n}\".nodes s ON s.id = e.src JOIN \"{n}\".nodes d ON d.id = e.dst WHERE e.label = 'KNOWS'"
            ),
            &[],
        )
        .unwrap();
    assert_eq!((r.get::<_, String>(0), r.get::<_, String>(1)), ("Ada".into(), "Bob".into()));
    assert_eq!(q1(&mut sql, &format!("SELECT count(*) FROM \"{n}\".nodes WHERE labels @> ARRAY['Admin']")), 1);
    assert_eq!(q1(&mut sql, &format!("SELECT count(*) FROM \"{n}\".nodes WHERE props @> '{{\"age\": 36}}'")), 1);

    // updates: props, label add/remove, edge weight
    db.query("MATCH (p:Person {name:'Ada'}) SET p.age = 37, p.nick = 'A'").unwrap();
    db.query("MATCH (p:Person {name:'Ada'}) SET p.nick = null").unwrap();
    let mut t = db.begin();
    let ada = db.read(|v| v.lookup("Person", "name", &Value::Str("Ada".into())).get(0).copied()).unwrap_or(ids[0]);
    t.add_label(ada, "Vip");
    t.remove_label(ada, "Person");
    t.set_weight(hy, 9.5);
    t.commit().unwrap();
    assert!(db.verify_mirror().unwrap().is_clean(), "{:?}", db.verify_mirror());
    assert_eq!(q1(&mut sql, &format!("SELECT count(*) FROM \"{n}\".nodes WHERE labels @> ARRAY['Vip'] AND NOT labels @> ARRAY['Person']")), 1);
    let w: f64 = sql.query_one(&format!("SELECT weight FROM \"{n}\".edges WHERE id = {hy}"), &[]).unwrap().get(0);
    assert_eq!(w, 9.5);
    let created: i64 = sql.query_one(&format!("SELECT created_ts FROM \"{n}\".nodes WHERE id = {ada}"), &[]).unwrap().get(0);
    let updated: i64 = sql.query_one(&format!("SELECT updated_ts FROM \"{n}\".nodes WHERE id = {ada}"), &[]).unwrap().get(0);
    assert!(created < updated, "created_ts is kept, updated_ts moves");

    // deleting a node removes its incident edges and hyperedges, and their member rows
    db.query("MATCH (p:Person {name:'Bob'}) DELETE p").unwrap();
    assert!(db.verify_mirror().unwrap().is_clean());
    assert_eq!(q1(&mut sql, &format!("SELECT count(*) FROM \"{n}\".nodes")), 2);
    assert_eq!(q1(&mut sql, &format!("SELECT count(*) FROM \"{n}\".edges")), 0);
    assert_eq!(q1(&mut sql, &format!("SELECT count(*) FROM \"{n}\".edge_members")), 0);
    let st = db.mirror_stats().unwrap();
    assert_eq!((st.nodes, st.edges, st.edge_members, st.mirror_ts), (2, 0, 0, Some(db.last_ts())));

    // create + delete inside one commit leaves no trace
    let mut t = db.begin();
    let x = t.create_node(&["Tmp"], props([("a", 1i64)])).unwrap();
    let y = t.create_node(&["Tmp"], Props::new()).unwrap();
    t.create_edge("T", x, y, 1.0, Props::new()).unwrap();
    t.delete(x);
    t.commit().unwrap();
    assert!(db.verify_mirror().unwrap().is_clean());
    assert_eq!(db.mirror_stats().unwrap().nodes, 3);
    assert_eq!(db.mirror_stats().unwrap().edges, 0);
}

#[test]
fn mirror_detects_tampering_rebuilds_and_follows_the_setting() {
    let url = need_pg!();
    let s = Sch::new(&url, "tamper");
    {
        // mirror disabled: only the log is written
        let db = s.try_open(s.opts().mirror(false)).unwrap();
        db.query("CREATE (a:P {n:1}), (b:P {n:2}), (a)-[:R]->(b)").unwrap();
        assert!(matches!(db.mirror_stats(), Err(Error::Unsupported(_))));
    }
    {
        // enabling it rebuilds from the log automatically
        let db = s.open();
        assert!(db.verify_mirror().unwrap().is_clean());
        assert_eq!(db.mirror_stats().unwrap().nodes, 2);
        // tamper from outside
        let mut c = s.sql();
        c.batch_execute(&format!("DELETE FROM \"{0}\".nodes WHERE id = 1; UPDATE \"{0}\".edges SET weight = 99; INSERT INTO \"{0}\".nodes VALUES (777, '{{X}}', '{{}}', 1, 1)", s.name)).unwrap();
        let r = db.verify_mirror().unwrap();
        assert!(!r.is_clean());
        assert!(r.mismatched.contains(&1), "{r:?}");
        assert!(r.extra_rows >= 1, "{r:?}");
        assert_eq!(db.rebuild_mirror().unwrap().nodes, 2);
        assert!(db.verify_mirror().unwrap().is_clean());
    }
    {
        // mirror off again: changes are logged but not mirrored ...
        let db = s.try_open(s.opts().mirror(false)).unwrap();
        db.query("CREATE (:P {n:3})").unwrap();
    }
    {
        // ... and the stale mirror is noticed (mirror_ts != last_ts) and rebuilt on next open
        let db = s.open();
        assert_eq!(db.mirror_stats().unwrap().nodes, 3);
        assert!(db.verify_mirror().unwrap().is_clean());
    }
}

#[test]
fn awkward_values_survive_log_and_mirror() {
    let url = need_pg!();
    let s = Sch::new(&url, "values");
    let nasty = "q\"uote \\ back\nnl\ttab nul\u{0}here ünï 🌍 <>&'";
    let mut ids = Vec::new();
    {
        let db = s.open();
        let mut t = db.begin();
        let a = t
            .create_node(
                &["Wëird", "with space"],
                props([
                    ("s", Value::Str(nasty.into())),
                    ("k\"ey", Value::Int(1)),
                    ("big", Value::Int(i64::MAX)),
                    ("small", Value::Int(i64::MIN)),
                    ("huge", Value::Float(1e300)),
                    ("tiny", Value::Float(-1e-300)),
                    ("inf", Value::Float(f64::INFINITY)),
                    ("vec", Value::Vector(vec![1.0, 0.5, -2.25])),
                    ("nested", Value::List(vec![Value::Int(1), Value::Str("x".into()), Value::List(vec![Value::Bool(true), Value::Null])])),
                    ("null", Value::Null),
                    ("empty", Value::Str(String::new())),
                ]),
            )
            .unwrap();
        let b = t.create_node(&["N"], Props::new()).unwrap();
        let c = t.create_node(&["N"], Props::new()).unwrap();
        let e1 = t.create_hyperedge("UNDIR", &[a, b, c], &[], 1.5, Props::new()).unwrap();
        let e2 = t.create_edge("SELF", a, a, -1e308, Props::new()).unwrap();
        let e3 = t.create_hyperedge("DUP", &[a, a, b], &[b, c, c], 1e308, props([("w", 1.5f64)])).unwrap();
        t.commit().unwrap();
        ids.extend([a, b, c, e1, e2, e3]);
        let r = db.verify_mirror().unwrap();
        assert!(r.is_clean(), "{r:?}");
        let mut sql = s.sql();
        let n = &s.name;
        let got: String = sql.query_one(&format!("SELECT props->>'s' FROM \"{n}\".nodes WHERE id = {a}"), &[]).unwrap().get(0);
        assert_eq!(got, nasty.replace('\0', "\u{2400}"), "NUL is the one documented lossy substitution in the mirror");
        let big: i64 = sql.query_one(&format!("SELECT (props->>'big')::bigint FROM \"{n}\".nodes WHERE id = {a}"), &[]).unwrap().get(0);
        assert_eq!(big, i64::MAX);
        let w: Vec<f64> = sql.query(&format!("SELECT weight FROM \"{n}\".edges ORDER BY id"), &[]).unwrap().iter().map(|r| r.get(0)).collect();
        assert_eq!((w[0], w[1], w[2]), (1.5, -1e308, 1e308));
        let m: i64 = sql.query_one(&format!("SELECT count(*) FROM \"{n}\".edge_members WHERE edge_id = {e3}"), &[]).unwrap().get(0);
        assert_eq!(m, 4, "tail {{a,b}} + head {{b,c}} as distinct (node, role) pairs");
    }
    // the log itself is byte-exact: NUL and all
    let db = s.open();
    let d = dump(&db);
    let Body::Node { props: p, labels } = &d[0].1 else { panic!() };
    assert_eq!(p["s"], Value::Str(nasty.into()));
    assert_eq!(labels, &vec!["Wëird".to_string(), "with space".to_string()]);
    assert_eq!(p["big"], Value::Int(i64::MAX));
    assert_eq!(p["vec"], Value::Vector(vec![1.0, 0.5, -2.25]));
    let Body::Edge { weight, tail, head, .. } = &d.last().unwrap().1 else { panic!() };
    assert_eq!(*weight, 1e308);
    assert_eq!((tail.len(), head.len()), (3, 3));
    assert!(db.verify_mirror().unwrap().is_clean());
}

#[test]
fn failed_commits_leave_no_trace() {
    let url = need_pg!();
    let s = Sch::new(&url, "failed");
    let db = s.open();
    db.query("CREATE (a:P {n:1})").unwrap();
    let (c0, n0, ts0) = (s.count("commits"), s.count("nodes"), db.last_ts());
    let mut t = db.begin();
    t.create_node(&["Ghost"], Props::new()).unwrap();
    t.create_edge("R", 1, 424242, 1.0, Props::new()).unwrap();
    assert!(t.commit().is_err());
    assert_eq!((s.count("commits"), s.count("nodes"), db.last_ts()), (c0, n0, ts0));
    db.query("CREATE (b:P {n:2})").unwrap(); // reuses the ts and ids: nothing was half-written
    assert_eq!(db.last_ts(), ts0 + 1);
    // rolled-back transactions write nothing either
    let mut t = db.begin();
    t.create_node(&["Nope"], Props::new()).unwrap();
    t.rollback();
    assert_eq!(s.count("commits"), c0 + 1);
    assert!(db.verify_mirror().unwrap().is_clean());
}

#[test]
fn a_foreign_writer_is_fenced_out_by_the_ts_key() {
    let url = need_pg!();
    let s = Sch::new(&url, "fence");
    let db = s.open();
    db.query("CREATE (:P {n:1})").unwrap();
    // somebody bypasses the advisory lock and writes the next commit row
    s.sql()
        .batch_execute(&format!("INSERT INTO \"{}\".commits (ts, crc, payload) VALUES (2, 0, '\\x00')", s.name))
        .unwrap();
    let before = dump(&db);
    match db.query("CREATE (:P {n:2})") {
        Err(Error::Backend(m)) => assert!(m.contains("already exists") && m.contains("another writer"), "{m}"),
        other => panic!("expected fencing error, got {other:?}"),
    }
    assert_eq!(dump(&db), before, "memory untouched after a failed durable write");
    assert_eq!(db.mirror_stats().unwrap().nodes, 1);
}

#[test]
fn corrupt_logs_are_detected_on_open() {
    let url = need_pg!();
    let s = Sch::new(&url, "corrupt");
    {
        let db = s.open();
        for i in 0..4 {
            db.query(&format!("CREATE (:P {{n:{i}}})")).unwrap();
        }
    }
    let mut c = s.sql();
    c.batch_execute(&format!("UPDATE \"{}\".commits SET crc = crc + 1 WHERE ts = 2", s.name)).unwrap();
    assert!(matches!(s.try_open(s.opts()), Err(Error::Corrupt(m)) if m.contains("checksum")));
    c.batch_execute(&format!("UPDATE \"{0}\".commits SET crc = crc - 1 WHERE ts = 2; DELETE FROM \"{0}\".commits WHERE ts = 3", s.name)).unwrap();
    assert!(matches!(s.try_open(s.opts()), Err(Error::Corrupt(m)) if m.contains("gap")));
    // flip a byte inside a checkpoint chunk
    c.batch_execute(&format!("DELETE FROM \"{0}\".commits WHERE ts > 2", s.name)).unwrap();
    let db = s.open();
    db.checkpoint().unwrap();
    drop(db);
    c.batch_execute(&format!("UPDATE \"{0}\".checkpoint_chunks SET payload = set_byte(payload, 20, get_byte(payload, 20) # 255)", s.name)).unwrap();
    assert!(matches!(s.try_open(s.opts()), Err(Error::Corrupt(_))));
}

#[test]
fn survives_a_killed_connection() {
    let url = need_pg!();
    let s = Sch::new(&url, "kill");
    let db = s.open();
    db.query("CREATE (:P {n:1})").unwrap();
    let kill = |s: &Sch| {
        let n = s
            .sql()
            .execute(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE application_name = $1 AND pid <> pg_backend_pid()",
                &[&format!("nexgraph-{}", s.name)],
            )
            .unwrap();
        assert!(n >= 1, "no backend to kill");
        std::thread::sleep(Duration::from_millis(200));
    };
    kill(&s);
    db.query("CREATE (:P {n:2})").unwrap(); // reconnects transparently
    kill(&s);
    db.query("MATCH (p:P {n:1}) SET p.n = 10").unwrap();
    assert_eq!(db.last_ts(), 3);
    assert_eq!(s.count("commits"), 3, "no duplicate and no lost commit");
    assert!(db.verify_mirror().unwrap().is_clean());
    // the writer lock was re-acquired by the new session
    assert!(s.try_open(s.opts()).is_err());
    drop(db);
    assert_eq!(s.open().stats().live_nodes, 2);
}

#[test]
fn replica_follows_writer_across_checkpoints() {
    let url = need_pg!();
    let s = Sch::new(&url, "replica");
    let w = s.open();
    w.query("CREATE (:P {n:1})").unwrap();
    let r = s.try_open(s.opts().read_only(true)).unwrap();
    assert_eq!(r.stats().live_nodes, 1);
    assert_eq!(r.refresh().unwrap().applied_commits, 0);
    for i in 2..=5 {
        w.query(&format!("CREATE (:P {{n:{i}}})")).unwrap();
    }
    let rf = r.refresh().unwrap();
    assert_eq!((rf.applied_commits, rf.reloaded, rf.last_ts), (4, false, 5));
    assert_eq!(dump(&r), dump(&w));
    assert_eq!(r.query("MATCH (p:P) RETURN count(*)").unwrap().rows[0][0], Value::Int(5));
    // writer checkpoints and keeps going: the replica falls behind the checkpoint and reloads
    w.query("CREATE (:P {n:6})").unwrap();
    w.checkpoint().unwrap();
    w.query("CREATE (:P {n:7})").unwrap();
    let rf = r.refresh().unwrap();
    assert!(rf.reloaded, "{rf:?}");
    assert_eq!(dump(&r), dump(&w));
    assert_eq!(r.last_ts(), w.last_ts());
    // up to date again: incremental
    w.query("CREATE (:P {n:8})").unwrap();
    let rf = r.refresh().unwrap();
    assert_eq!((rf.applied_commits, rf.reloaded), (1, false));
    assert_eq!(dump(&r), dump(&w));
}

// ---------------------------------------------------------------- randomized

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick<T: Copy>(&mut self, v: &[T]) -> T {
        v[self.below(v.len())]
    }
}

fn rand_value(r: &mut Rng) -> Value {
    match r.below(8) {
        0 => Value::Int(r.next() as i64 >> r.below(60)),
        1 => Value::Float((r.next() % 100_000) as f64 / 64.0 - 700.0),
        2 => Value::Str(["", "a", "o'brien", "line\nbreak", "ünï", "🌍", "q\"d", "back\\slash"][r.below(8)].to_string()),
        3 => Value::Bool(r.below(2) == 0),
        4 => Value::Null,
        5 => Value::List((0..r.below(4)).map(|_| Value::Int(r.below(100) as i64)).collect()),
        6 => Value::Vector((0..3).map(|_| r.below(1000) as f32 / 8.0).collect()),
        _ => Value::Str(format!("s{}", r.below(1000))),
    }
}

fn rand_props(r: &mut Rng) -> Props {
    (0..r.below(4)).map(|_| (format!("k{}", r.below(6)), rand_value(r))).collect()
}

/// One random commit applied identically to every database in `dbs`.
fn random_commit(r: &mut Rng, dbs: &[&Database]) {
    let (nodes, edges): (Vec<Id>, Vec<Id>) = dbs[0].read(|v| (v.node_ids().collect(), v.edge_ids().collect()));
    let labels = ["A", "B", "C", "D"];
    let delete_commit = r.below(10) < 3 && nodes.len() + edges.len() > 5;
    let mut plan: Vec<Box<dyn Fn(&mut nexgraph::Txn<'_>)>> = Vec::new();
    if delete_commit {
        let mut gone: Vec<Id> = Vec::new();
        for _ in 0..1 + r.below(3) {
            let all: Vec<Id> = if r.below(3) == 0 { edges.clone() } else { nodes.clone() };
            if all.is_empty() {
                continue;
            }
            let id = r.pick(&all);
            if !gone.contains(&id) {
                gone.push(id);
                plan.push(Box::new(move |t| t.delete(id)));
            }
        }
    } else {
        for _ in 0..1 + r.below(6) {
            match r.below(7) {
                0 | 1 => {
                    let ls: Vec<&'static str> = (0..1 + r.below(2)).map(|_| r.pick(&labels)).collect();
                    let p = rand_props(r);
                    plan.push(Box::new(move |t| {
                        let mut l = ls.clone();
                        l.dedup();
                        t.create_node(&l, p.clone()).unwrap();
                    }));
                }
                2 | 3 if nodes.len() >= 2 => {
                    let k = if r.below(5) == 0 { 3 } else { 1 };
                    let tail: Vec<Id> = (0..k).map(|_| r.pick(&nodes)).collect();
                    let head: Vec<Id> = if r.below(8) == 0 { vec![] } else { (0..if k == 3 { 2 } else { 1 }).map(|_| r.pick(&nodes)).collect() };
                    let w = [0.5, 1.0, -2.5, 1e300][r.below(4)];
                    let p = rand_props(r);
                    let l = r.pick(&["R", "S", "T"]);
                    plan.push(Box::new(move |t| {
                        t.create_hyperedge(l, &tail, &head, w, p.clone()).unwrap();
                    }));
                }
                4 if !nodes.is_empty() || !edges.is_empty() => {
                    let all: Vec<Id> = nodes.iter().chain(edges.iter()).copied().collect();
                    let id = r.pick(&all);
                    let set = rand_props(r);
                    let rm = vec![format!("k{}", r.below(6))];
                    plan.push(Box::new(move |t| {
                        let rm: Vec<&str> = rm.iter().map(|s| s.as_str()).collect();
                        t.set_props(id, set.clone(), &rm).unwrap();
                    }));
                }
                5 if !nodes.is_empty() => {
                    let id = r.pick(&nodes);
                    let l = r.pick(&labels);
                    let add = r.below(2) == 0;
                    plan.push(Box::new(move |t| if add { t.add_label(id, l) } else { t.remove_label(id, l) }));
                }
                6 if !edges.is_empty() => {
                    let id = r.pick(&edges);
                    let w = (r.below(100) as f64) / 4.0;
                    plan.push(Box::new(move |t| t.set_weight(id, w)));
                }
                _ => {}
            }
        }
    }
    for db in dbs {
        let mut t = db.begin();
        for step in &plan {
            step(&mut t);
        }
        t.commit().unwrap();
    }
}

#[test]
fn randomized_differential_against_the_in_memory_engine() {
    let url = need_pg!();
    let s = Sch::new(&url, "fuzz");
    let mem = Database::in_memory();
    let mut pg = s.open();
    let mut r = Rng(0x9e3779b97f4a7c15);
    let mut marks: Vec<(u64, Vec<(Id, Body)>)> = Vec::new();
    for round in 1..=300 {
        random_commit(&mut r, &[&mem, &pg]);
        if round % 25 == 0 {
            let rep = pg.verify_mirror().unwrap();
            assert!(rep.is_clean(), "round {round}: {rep:?}");
            marks.push((mem.last_ts(), dump(&mem)));
        }
        if round == 120 {
            // checkpoint both so compaction + restart are part of the history
            mem.checkpoint().unwrap();
            pg.checkpoint().unwrap();
            marks.clear();
        }
        if round % 100 == 0 {
            drop(pg);
            pg = s.open();
            assert_eq!(dump(&pg), dump(&mem), "state differs after reopen at round {round}");
            assert_eq!(pg.last_ts(), mem.last_ts());
        }
    }
    assert_eq!(dump(&pg), dump(&mem));
    // every remembered historical state is reproduced by time travel after the last restart
    assert!(!marks.is_empty());
    for (ts, d) in &marks {
        assert_eq!(&dump_at(&pg, *ts), d, "time travel to ts {ts}");
    }
    assert!(pg.verify_mirror().unwrap().is_clean());
    let st = pg.mirror_stats().unwrap();
    assert_eq!(st.nodes as usize + st.edges as usize, dump(&pg).len());
    // and the engine on top of PostgreSQL still answers queries the same way
    for q in ["MATCH (n:A) RETURN count(*)", "MATCH (a)-[e:R]->(b) RETURN count(*), sum(e.weight)", "MATCH (n) WHERE n.k1 IS NOT NULL RETURN count(*)"] {
        assert_eq!(pg.query(q).unwrap().rows, mem.query(q).unwrap().rows, "{q}");
    }
}

#[test]
fn large_commit_spans_many_mirror_batches() {
    let url = need_pg!();
    let s = Sch::new(&url, "bulk");
    let db = s.try_open(s.opts().synchronous_commit(false)).unwrap();
    let t0 = Instant::now();
    let mut t = db.begin();
    let ids: Vec<Id> = (0..9_000).map(|i| t.create_node(&["N"], props([("i", i as i64)])).unwrap()).collect();
    for i in 0..ids.len() {
        for j in 1..=3 {
            t.create_edge("E", ids[i], ids[(i * 7 + j * 13) % ids.len()], j as f64, Props::new()).unwrap();
        }
    }
    t.commit().unwrap();
    eprintln!("bulk commit of 9k nodes + 27k edges with mirror: {:?}", t0.elapsed());
    let st = db.mirror_stats().unwrap();
    assert_eq!((st.nodes, st.edges, st.edge_members), (9_000, 27_000, 54_000));
    assert!(db.verify_mirror().unwrap().is_clean());
    // delete a hub node: its edges and members disappear from the mirror too
    db.query("MATCH (n:N {i: 5}) DELETE n").unwrap();
    assert!(db.verify_mirror().unwrap().is_clean());
    drop(db);
    let db = s.open();
    assert_eq!(db.stats().live_nodes, 8_999);
}

#[test]
fn vector_index_and_algorithms_work_over_postgres_storage() {
    let url = need_pg!();
    let s = Sch::new(&url, "vec");
    {
        let db = s.open();
        db.query("CREATE VECTOR INDEX ON :Doc(emb) DIMENSIONS 3 METRIC cosine").ok();
        let mut t = db.begin();
        t.create_vector_index("Doc", "emb", 3, Metric::Cosine);
        for i in 0..50 {
            let a = i as f32 / 50.0;
            t.create_node(&["Doc"], props([("emb", Value::Vector(vec![1.0 - a, a, 0.25])), ("i", Value::Int(i))])).unwrap();
        }
        t.commit().unwrap();
        db.query("MATCH (a:Doc {i:1}), (b:Doc {i:2}) CREATE (a)-[:L]->(b)").unwrap();
    }
    let db = s.open();
    assert!(db.read(|v| v.has_vector_index("Doc", "emb")), "vector index definition is recovered");
    let hits = db.read(|v| v.vector_search("Doc", "emb", &[1.0, 0.0, 0.25], 3, 32, true)).unwrap();
    assert_eq!(hits.len(), 3);
    let r = db.query("CALL algo.pagerank() YIELD node, score RETURN count(*)");
    assert!(r.is_ok(), "{r:?}");
}


#[test]
fn power_bi_views_flatten_the_mirror() {
    let url = need_pg!();
    let sch = Sch::new(&url, "bi_views");
    let db = sch.open();
    db.query("CREATE (a:Host {ip:'10.0.0.1'}), (b:Host {ip:'10.0.0.2'}), (a)-[:FLOW {weight: 2.5, bytes: 7}]->(b)").unwrap();
    db.query("MATCH (a),(b) WHERE a.ip='10.0.0.1' AND b.ip='10.0.0.2' CREATE HYPEREDGE m:Meeting {t:'x'} OVER (a,b)").unwrap();
    let mut c = sch.sql();
    let n = &sch.name;
    let r = c.query_one(format!("SELECT label, labels, props->>'ip' FROM \"{n}\".bi_nodes ORDER BY props->>'ip' LIMIT 1").as_str(), &[]).unwrap();
    assert_eq!((r.get::<_, String>(0), r.get::<_, String>(1), r.get::<_, String>(2)), ("Host".into(), "Host".into(), "10.0.0.1".into()));
    let r = c.query_one(format!("SELECT weight, directed, src IS NOT NULL AND dst IS NOT NULL, props->>'bytes' FROM \"{n}\".bi_edges WHERE label = 'FLOW'").as_str(), &[]).unwrap();
    assert_eq!((r.get::<_, f64>(0), r.get::<_, bool>(1), r.get::<_, bool>(2), r.get::<_, String>(3)), (2.5, true, true, "7".into()));
    let r = c.query_one(format!("SELECT directed, src IS NULL, tail_count, head_count FROM \"{n}\".bi_edges WHERE label = 'Meeting'").as_str(), &[]).unwrap();
    assert_eq!((r.get::<_, bool>(0), r.get::<_, bool>(1), r.get::<_, i32>(2), r.get::<_, i32>(3)), (false, true, 2, 0));
    let m: i64 = c.query_one(format!("SELECT count(*) FROM \"{n}\".bi_edge_members WHERE label = 'Meeting'").as_str(), &[]).unwrap().get(0);
    assert_eq!(m, 2);
    // reopening an existing schema keeps the views working (CREATE OR REPLACE is idempotent) and a rebuild does not break them
    drop(db);
    let db = sch.open();
    db.rebuild_mirror().unwrap();
    let k: i64 = c.query_one(format!("SELECT count(*) FROM \"{n}\".bi_nodes").as_str(), &[]).unwrap().get(0);
    assert_eq!(k, 2);
}
