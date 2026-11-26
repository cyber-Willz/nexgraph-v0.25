//! Range predicates (`<`, `<=`, `>`, `>=`) on an indexed property are answered from the ordered property index.
//! The tests are differential: the same data and queries run on a database without the index (the label scan, the
//! reference semantics) and on one with it, including mixed value types, missing properties, history (`AS OF`)
//! and later updates, and the results must be identical.
use nexgraph::query::Params;
use nexgraph::{Database, Value};

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

fn val_literal(r: &mut Lcg) -> String {
    match r.next() % 10 {
        0..=3 => format!("{}", (r.next() % 121) as i64 - 60),
        4 | 5 => format!("{:.1}", (r.next() % 1210) as f64 / 10.0 - 60.0),
        6 | 7 => format!("'{}{}'", (b'a' + (r.next() % 6) as u8) as char, (b'a' + (r.next() % 6) as u8) as char),
        8 => (r.next() % 2 == 0).to_string(),
        _ => "null".into(),
    }
}

fn load(db: &Database, n: u64, seed: u64) {
    let mut r = Lcg(seed);
    for i in 0..n {
        let v = match r.next() % 12 {
            0 => String::new(), // property missing
            _ => format!(", v: {}", val_literal(&mut r)),
        };
        db.query(&format!("CREATE (:P {{id: {i}{v}}})")).unwrap();
    }
    // a second label with the same key must never leak into :P results
    for i in 0..50 {
        db.query(&format!("CREATE (:Q {{id: {}, v: {}}})", 10_000 + i, i)).unwrap();
    }
}

fn ids(db: &Database, q: &str, p: &Params) -> Vec<i64> {
    let res = nexgraph::query::run(db, q, p).unwrap_or_else(|e| panic!("{q}\n  {e}"));
    let mut v: Vec<i64> = res
        .rows
        .iter()
        .map(|r| match &r[0] {
            Value::Int(i) => *i,
            o => panic!("not an id: {o:?}"),
        })
        .collect();
    v.sort_unstable();
    v
}

fn queries(seed: u64, count: usize) -> Vec<String> {
    let mut r = Lcg(seed);
    let ops = ["<", "<=", ">", ">="];
    let mut out = vec![];
    for _ in 0..count {
        let a = val_literal(&mut r);
        let b = val_literal(&mut r);
        let (o1, o2) = (ops[(r.next() % 4) as usize], ops[(r.next() % 4) as usize]);
        out.push(match r.next() % 5 {
            0 => format!("MATCH (p:P) WHERE p.v {o1} {a} RETURN p.id"),
            1 => format!("MATCH (p:P) WHERE {a} {o1} p.v RETURN p.id"), // flipped
            2 => format!("MATCH (p:P) WHERE p.v {o1} {a} AND p.v {o2} {b} RETURN p.id"),
            3 => format!("MATCH (p:P) WHERE p.v {o1} {a} AND p.id % 3 = 0 RETURN p.id"),
            _ => format!("MATCH (p:P) WHERE p.v {o1} {a} OR p.id = 7 RETURN p.id"), // OR must not be pushed down
        });
    }
    out
}

#[test]
fn indexed_ranges_equal_the_label_scan() {
    let (plain, idx) = (Database::in_memory(), Database::in_memory());
    load(&plain, 1500, 7);
    load(&idx, 1500, 7);
    idx.query("CREATE INDEX ON :P(v)").unwrap(); // backfilled from existing data
    // CREATE INDEX is a commit of its own, so the two databases' "before the updates" timestamps differ by one
    let (ts_plain, ts_idx) = (plain.last_ts(), idx.last_ts());
    assert_eq!(ts_idx, ts_plain + 1);
    // later writes go through index maintenance, and leave old versions in the index
    for q in [
        "MATCH (p:P) WHERE p.id % 10 = 0 SET p.v = 1000",
        "MATCH (p:P) WHERE p.id % 10 = 1 SET p.v = 'zz'",
        "MATCH (p:P) WHERE p.id % 10 = 2 SET p.v = -1000.5",
        "MATCH (p:P) WHERE p.id % 10 = 3 DELETE p",
    ] {
        plain.query(q).unwrap_or_else(|e| panic!("{q}: {e}"));
        idx.query(q).unwrap_or_else(|e| panic!("{q}: {e}"));
    }
    let none = Params::default();
    let mut nonempty = 0;
    for q in queries(99, 400) {
        for (pa, pb) in [(String::new(), String::new()), (format!("AS OF {ts_plain} "), format!("AS OF {ts_idx} "))] {
            let (a, b) = (ids(&plain, &format!("{pa}{q}"), &none), ids(&idx, &format!("{pb}{q}"), &none));
            assert_eq!(a, b, "{pa}{q}");
            nonempty += (!a.is_empty()) as usize;
        }
    }
    assert!(nonempty > 200, "the generated queries should mostly return rows ({nonempty})");
}

#[test]
fn parameters_and_edge_cases() {
    let (plain, idx) = (Database::in_memory(), Database::in_memory());
    for db in [&plain, &idx] {
        for (i, v) in ["1", "2", "2.5", "3", "9007199254740993", "9007199254740992", "'a'", "'b'", "true", "false"].iter().enumerate() {
            db.query(&format!("CREATE (:P {{id: {i}, v: {v}}})")).unwrap();
        }
        db.query("CREATE (:P {id: 10})").unwrap();
    }
    idx.query("CREATE INDEX ON :P(v)").unwrap();
    let mut p = Params::default();
    p.insert("lo".into(), Value::Int(2));
    p.insert("s".into(), Value::Str("a".into()));
    p.insert("n".into(), Value::Null);
    for q in [
        "MATCH (p:P) WHERE p.v >= $lo RETURN p.id",
        "MATCH (p:P) WHERE p.v > $lo AND p.v <= 3 RETURN p.id",
        "MATCH (p:P) WHERE p.v >= $s RETURN p.id",
        "MATCH (p:P) WHERE p.v > $n RETURN p.id", // null bound: nothing matches, same as the scan
        "MATCH (p:P) WHERE p.v > 5 AND p.v < 1 RETURN p.id", // empty interval
        "MATCH (p:P) WHERE p.v > 1 AND p.v < 'x' RETURN p.id", // mixed-type interval: never true
        "MATCH (p:P) WHERE p.v > 9007199254740992 RETURN p.id", // beyond f64 precision: exact i64 compare decides
        "MATCH (p:P) WHERE p.v >= 9007199254740993 RETURN p.id",
        "MATCH (p:P) WHERE p.v > false RETURN p.id",
        "MATCH (p:P) WHERE p.v < 2.5 RETURN p.id",
        "MATCH (p:P) WHERE p.v <= 2.5 RETURN p.id",
    ] {
        assert_eq!(ids(&plain, q, &p), ids(&idx, q, &p), "{q}");
    }
    // spot-check absolute answers so the two sides cannot both be wrong the same way
    assert_eq!(ids(&idx, "MATCH (p:P) WHERE p.v >= 2 AND p.v < 3 RETURN p.id", &p), vec![1, 2]);
    assert_eq!(ids(&idx, "MATCH (p:P) WHERE p.v >= 'a' RETURN p.id", &p), vec![6, 7]);
    assert_eq!(ids(&idx, "MATCH (p:P) WHERE p.v > 9007199254740992 RETURN p.id", &p), vec![4]);
    assert_eq!(ids(&idx, "MATCH (p:P) WHERE 3 > p.v RETURN p.id", &p), vec![0, 1, 2]);
}

#[test]
fn view_lookup_range_contract() {
    let db = Database::in_memory();
    for i in 0..100 {
        db.query(&format!("CREATE (:P {{id: {i}, v: {i}}})")).unwrap();
    }
    db.read(|v| {
        assert!(v.lookup_range("P", "v", Some((&Value::Int(1), true)), None).is_none(), "no index yet: caller must scan");
    });
    db.query("CREATE INDEX ON :P(v)").unwrap();
    db.read(|v| {
        let r = |lo: Option<(i64, bool)>, hi: Option<(i64, bool)>| {
            let (l, h) = (lo.map(|(x, i)| (Value::Int(x), i)), hi.map(|(x, i)| (Value::Int(x), i)));
            v.lookup_range("P", "v", l.as_ref().map(|(x, i)| (x, *i)), h.as_ref().map(|(x, i)| (x, *i))).unwrap()
        };
        assert_eq!(r(Some((10, true)), Some((13, true))).len(), 4);
        assert_eq!(r(Some((10, false)), Some((13, false))).len(), 2);
        assert_eq!(r(Some((10, true)), Some((13, true))).len(), 4);
        assert_eq!(r(None, Some((4, false))).len(), 4);
        assert_eq!(r(Some((95, true)), None).len(), 5);
        assert!(r(Some((5, true)), Some((4, true))).is_empty());
        assert!(v.lookup_range("P", "v", None, None).is_none());
        assert!(v.lookup_range("P", "v", Some((&Value::Null, true)), None).is_none());
    });
}
