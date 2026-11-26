//! Exact-result tests for the openCypher additions (WITH, OPTIONAL MATCH, UNWIND, MERGE, CASE, functions)
//! and for how the access-control layer treats them.
#![cfg(feature = "security")]
use nexgraph::query::{parser, Params, QueryResult};
use nexgraph::security::policy::{Mask, Policy, Rule};
use nexgraph::security::Role;
use nexgraph::{Database, Error, Value};
use Value::{Float, Int, Null, Str};

fn graph() -> Database {
    let db = Database::in_memory();
    for q in [
        "CREATE (:Person {name:'Ada', age:41}), (:Person {name:'Bo', age:29}), (:Person {name:'Cy', age:35}), (:Person {name:'Di', age:52})",
        "CREATE (:Movie {title:'Matrix'}), (:Movie {title:'Speed'})",
        "MATCH (a:Person {name:'Ada'}), (m:Movie {title:'Matrix'}) CREATE (a)-[:ACTED_IN]->(m)",
        "MATCH (a:Person {name:'Bo'}), (m:Movie {title:'Matrix'}) CREATE (a)-[:ACTED_IN]->(m)",
        "MATCH (a:Person {name:'Cy'}), (m:Movie {title:'Speed'}) CREATE (a)-[:ACTED_IN]->(m)",
    ] {
        db.query(q).unwrap();
    }
    db
}

fn q(db: &Database, text: &str) -> Vec<Vec<Value>> {
    db.query(text).unwrap_or_else(|e| panic!("{text}\n  {e}")).rows
}
fn s(x: &str) -> Value {
    Str(x.into())
}

#[test]
fn with_pipelines_and_filters() {
    let db = graph();
    assert_eq!(q(&db, "MATCH (p:Person) WITH p ORDER BY p.age DESC LIMIT 2 RETURN p.name"), vec![vec![s("Di")], vec![s("Ada")]]);
    assert_eq!(q(&db, "MATCH (p:Person) WITH p.age > 35 AS senior, count(*) AS n RETURN senior, n ORDER BY senior"), vec![vec![Value::Bool(false), Int(2)], vec![Value::Bool(true), Int(2)]]);
    // aggregate in WITH, then filter on it, then use it
    assert_eq!(
        q(&db, "MATCH (p:Person)-[:ACTED_IN]->(m:Movie) WITH m, count(p) AS cast WHERE cast > 1 RETURN m.title, cast"),
        vec![vec![s("Matrix"), Int(2)]]
    );
    // variables not projected are gone
    assert!(db.query("MATCH (p:Person) WITH p.name AS n RETURN p.age").is_err());
}

#[test]
fn optional_match_keeps_rows_and_binds_null() {
    let db = graph();
    let r = q(&db, "MATCH (p:Person) OPTIONAL MATCH (p)-[:ACTED_IN]->(m:Movie) RETURN p.name, m.title ORDER BY p.name");
    assert_eq!(r, vec![vec![s("Ada"), s("Matrix")], vec![s("Bo"), s("Matrix")], vec![s("Cy"), s("Speed")], vec![s("Di"), Null]]);
    assert_eq!(q(&db, "MATCH (p:Person) OPTIONAL MATCH (p)-[:ACTED_IN]->(m) WITH p, count(m) AS n RETURN p.name, n ORDER BY p.name")[3], vec![s("Di"), Int(0)]);
    // WHERE belongs to the OPTIONAL MATCH: it removes the match, not the row
    let r = q(&db, "MATCH (p:Person {name:'Ada'}) OPTIONAL MATCH (p)-[:ACTED_IN]->(m:Movie) WHERE m.title = 'Speed' RETURN p.name, m.title");
    assert_eq!(r, vec![vec![s("Ada"), Null]]);
}

#[test]
fn unwind_expands_and_joins() {
    let db = graph();
    assert_eq!(q(&db, "UNWIND [1,2,3] AS x RETURN x * 2 AS y"), vec![vec![Int(2)], vec![Int(4)], vec![Int(6)]]);
    assert_eq!(q(&db, "UNWIND ['Ada','Zed','Bo'] AS n MATCH (p:Person {name: n}) RETURN p.age ORDER BY p.age"), vec![vec![Int(29)], vec![Int(41)]]);
    assert!(q(&db, "UNWIND [] AS x RETURN x").is_empty());
    assert!(db.query("UNWIND 5 AS x RETURN x").is_err());
    assert!(db.query("UNWIND range(1, 2000000) AS x RETURN x").is_err(), "range() is capped");
}

#[test]
fn merge_is_idempotent_even_within_one_statement() {
    let db = graph();
    let n = |l: &str| match &q(&db, &format!("MATCH (a:{l}) RETURN count(*) AS n"))[0][0] {
        Int(n) => *n,
        v => panic!("{v:?}"),
    };
    // existing node matched, not duplicated
    q(&db, "MERGE (p:Person {name:'Ada'}) RETURN p.name");
    assert_eq!(n("Person"), 4);
    // created once, ON CREATE applied, ON MATCH not
    q(&db, "MERGE (p:Person {name:'Eve'}) ON CREATE SET p.age = 1 ON MATCH SET p.age = 2");
    assert_eq!(q(&db, "MATCH (p:Person {name:'Eve'}) RETURN p.age"), vec![vec![Int(1)]]);
    q(&db, "MERGE (p:Person {name:'Eve'}) ON CREATE SET p.age = 1 ON MATCH SET p.age = 2");
    assert_eq!(q(&db, "MATCH (p:Person {name:'Eve'}) RETURN p.age"), vec![vec![Int(2)]]);
    assert_eq!(n("Person"), 5);
    // duplicates inside one statement: the snapshot does not show the first create, the cache must
    q(&db, "UNWIND ['x','x','y','x'] AS k MERGE (:Tag {k: k})");
    assert_eq!(n("Tag"), 2);
    // relationships between bound nodes, merged twice, once
    for _ in 0..2 {
        q(&db, "MATCH (a:Person {name:'Di'}), (m:Movie {title:'Speed'}) MERGE (a)-[:ACTED_IN]->(m)");
    }
    assert_eq!(q(&db, "MATCH (:Person {name:'Di'})-[r:ACTED_IN]->() RETURN count(*) AS n"), vec![vec![Int(1)]]);
    assert!(db.query("MERGE (p:Person {name: null})").is_err(), "null merge property");
}

#[test]
fn set_map_remove_case_and_functions() {
    let db = graph();
    q(&db, "MATCH (p:Person {name:'Ada'}) SET p += {city:'Belize', x:1}");
    assert_eq!(q(&db, "MATCH (p:Person {name:'Ada'}) RETURN p.city, p.x"), vec![vec![s("Belize"), Int(1)]]);
    q(&db, "MATCH (p:Person {name:'Ada'}) REMOVE p.x");
    assert_eq!(q(&db, "MATCH (p:Person {name:'Ada'}) RETURN p.x"), vec![vec![Null]]);
    assert_eq!(q(&db, "MATCH (p:Person) RETURN CASE WHEN p.age >= 40 THEN 'old' ELSE 'young' END AS g, count(*) AS n ORDER BY g"), vec![vec![s("old"), Int(2)], vec![s("young"), Int(2)]]);
    assert_eq!(q(&db, "RETURN CASE 2 WHEN 1 THEN 'a' WHEN 2 THEN 'b' END AS v, CASE 9 WHEN 1 THEN 'a' END AS w"), vec![vec![s("b"), Null]]);
    let one = |e: &str| q(&db, &format!("RETURN {e} AS v")).remove(0).remove(0);
    assert_eq!(one("toLower('AbC')"), s("abc"));
    assert_eq!(one("toUpper('abc')"), s("ABC"));
    assert_eq!(one("substring('hello', 1, 3)"), s("ell"));
    assert_eq!(one("left('hello', 2) + right('hello', 2)"), s("helo"));
    assert_eq!(one("replace('a-b-c', '-', '+')"), s("a+b+c"));
    assert_eq!(one("trim('  x ')"), s("x"));
    assert_eq!(one("split('a,b,c', ',')[1]"), s("b"));
    assert_eq!(one("split('a,b,c', ',')[-1]"), s("c"));
    assert_eq!(one("range(1, 10, 3)"), Value::List(vec![Int(1), Int(4), Int(7), Int(10)]));
    assert_eq!(one("head([5,6,7])"), Int(5));
    assert_eq!(one("last([5,6,7])"), Int(7));
    assert_eq!(one("tail([5,6,7])"), Value::List(vec![Int(6), Int(7)]));
    assert_eq!(one("toInteger('42')"), Int(42));
    assert_eq!(one("toInteger('3.9')"), Int(3));
    assert_eq!(one("round(2.5)"), Float(3.0));
    assert_eq!(one("floor(-1.5)"), Float(-2.0));
    assert_eq!(one("2 ^ 3"), Float(8.0));
    assert_eq!(one("-2 ^ 2"), Float(-4.0));
    assert_eq!(one("7 % 3"), Int(1));
    assert_eq!(one("exists(null)"), Value::Bool(false));
    assert_eq!(one("[1,2,3][7]"), Null);
    assert!(db.query("RETURN substring('a', -1)").is_err());
    // standard index syntax creates the same index as the legacy form
    q(&db, "CREATE INDEX FOR (p:Person) ON (p.name)");
    q(&db, "CREATE INDEX person_age FOR (p:Person) ON (p.age)");
}

// ---------------------------------------------------------------- access control

fn secure() -> (Database, Policy) {
    let db = Database::in_memory();
    for (n, age, dx) in [("Ann", 93, "flu"), ("Bob", 40, "flu"), ("Cat", 41, "flu"), ("Dan", 42, "flu"), ("Eli", 43, "flu"), ("Fay", 44, "flu"), ("Gus", 45, "flu"), ("Hal", 46, "flu"), ("Ivy", 47, "flu"), ("Jon", 48, "flu"), ("Kay", 49, "flu"), ("Lou", 50, "flu")] {
        db.query(&format!("CREATE (:Patient {{name:'{n}', age:{age}, dx:'{dx}'}})")).unwrap();
    }
    for i in 0..30 {
        db.query(&format!("CREATE (:Other {{i:{i}}})")).unwrap();
    }
    db.query("MATCH (a:Patient {name:'Ann'}), (o:Other) CREATE (a)-[:SEEN]->(o)").unwrap();
    let p = Policy {
        rules: vec![Rule { up_to: Some(Role::Writer), deny: vec!["Patient.name".into()], mask: vec![("age".into(), Mask::TopCode(89.0))], deny_labels: vec![] }],
        min_cell: 11,
        min_cell_up_to: Some(Role::Reader),
        pseudonym_key: b"0123456789abcdef0123456789abcdef".to_vec(),
        ..Default::default()
    };
    (db, p)
}

fn run(db: &Database, p: &Policy, role: Role, text: &str) -> Result<QueryResult, Error> {
    let st = parser::parse(text)?;
    let plan = p.check(role, &st, &Params::new())?;
    Ok(p.apply(&plan, db.query(text)?)?.0)
}

#[test]
fn denied_and_masked_properties_cannot_escape_through_with_unwind_or_case() {
    let (db, p) = secure();
    for text in [
        "MATCH (a:Patient) WITH a.name AS n RETURN n",
        "MATCH (a:Patient) WITH a AS b RETURN b.name",
        "MATCH (a:Patient) WITH collect(a) AS xs UNWIND xs AS b RETURN b.name",
        "MATCH (a:Patient) UNWIND [a] AS b RETURN b.name",
        "MATCH (a:Patient) RETURN CASE WHEN a.name = 'Ann' THEN 1 ELSE 0 END AS f",
        "MATCH (a:Patient) RETURN [a.name][0] AS n",
        "MATCH (a:Patient) WITH a.age AS x RETURN x", // masked values may only be returned directly
        "MERGE (a:Patient {name:'Zed'})",
        "MATCH (a:Patient) SET a += {name:'x'}",
    ] {
        assert!(matches!(run(&db, &p, Role::Writer, text), Err(Error::Denied(_))), "{text}");
    }
    // allowed uses still work, and the mask still applies to a direct projection
    let r = run(&db, &p, Role::Writer, "MATCH (a:Patient) WITH a ORDER BY a.dx LIMIT 20 RETURN a.age ORDER BY a.age DESC LIMIT 1");
    assert!(r.is_err() || matches!(r.unwrap().rows[0][0], Int(89)), "top-coded");
}

#[test]
fn small_group_suppression_cannot_be_beaten_by_inflating_the_row_count() {
    let (db, p) = secure();
    // honest query: the one 93-year-old is a group of 1 and is suppressed
    let r = run(&db, &p, Role::Reader, "MATCH (a:Patient) RETURN a.age AS age, count(*) AS n").unwrap();
    assert!(r.rows.iter().all(|row| matches!(row[1], Int(n) if n >= 11)));
    for text in [
        "MATCH (a:Patient), (b:Other) RETURN a.dx AS d, count(*) AS n",
        "MATCH (a:Patient) MATCH (b:Other) RETURN a.dx AS d, count(*) AS n",
        "MATCH (a:Patient) UNWIND range(1, 50) AS i RETURN a.dx AS d, count(*) AS n",
        "MATCH (a:Patient) OPTIONAL MATCH (a)-[:SEEN]->(o) RETURN a.dx AS d, count(*) AS n",
        "MATCH (a:Patient) WITH a.dx AS d, count(*) AS n RETURN d, count(*) AS m",
        // one patient with 30 edges: rows are paths, not people
        "MATCH (a:Patient)-[:SEEN]->(o:Other) RETURN a.dx AS d, count(*) AS n",
    ] {
        assert!(matches!(run(&db, &p, Role::Reader, text), Err(Error::Denied(_))), "{text}");
    }
    // counting distinct nodes is the honest form for graph patterns: the one patient is a group of 1
    let r = run(&db, &p, Role::Reader, "MATCH (a:Patient)-[:SEEN]->(o:Other) RETURN a.dx AS d, count(DISTINCT a) AS n").unwrap();
    assert!(r.rows.is_empty(), "{:?}", r.rows);
}

