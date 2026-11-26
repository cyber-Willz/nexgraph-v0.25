//! PostgreSQL backend benchmark.
//!
//!   NEXGRAPH_PG_URL=postgres://user@127.0.0.1:5432/db cargo run --release --example bench_pg
//!
//! Uses scratch schemas named `nxbench_*` and drops them afterwards. Numbers
//! depend heavily on where PostgreSQL runs and on its fsync settings: run it
//! against the server you intend to use.
use nexgraph::{props, Database, Id, PgOptions, Props, SyncMode, Value};
use postgres::{Client, NoTls};
use std::time::{Duration, Instant};

fn med(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

fn drop_schema(url: &str, s: &str) {
    let mut c = Client::connect(url, NoTls).unwrap();
    c.batch_execute(&format!("DROP SCHEMA IF EXISTS \"{s}\" CASCADE")).unwrap();
}

fn main() {
    let url = std::env::var("NEXGRAPH_PG_URL").expect("set NEXGRAPH_PG_URL");
    let n_commits = 400usize;
    let (nodes, deg) = (10_000usize, 8usize);

    println!("== single-row commit latency ({n_commits} commits, one node each)");
    let dir = std::env::temp_dir().join(format!("nxbench-file-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Database::open_with(&dir, SyncMode::Always).unwrap();
        let mut lat = Vec::new();
        for i in 0..n_commits {
            let t = Instant::now();
            let mut tx = db.begin();
            tx.create_node(&["N"], props([("i", i as i64)])).unwrap();
            tx.commit().unwrap();
            lat.push(t.elapsed());
        }
        let total: Duration = lat.iter().sum();
        println!("  file WAL, fsync        median {:>9.1?}   {:>8.0} commits/s", med(lat), n_commits as f64 / total.as_secs_f64());
    }
    let _ = std::fs::remove_dir_all(&dir);
    for (sync, mirror) in [(true, false), (true, true), (false, false), (false, true)] {
        let s = format!("nxbench_c_{}_{}", sync as u8, mirror as u8);
        drop_schema(&url, &s);
        let db = Database::open_postgres_with(&url, PgOptions::default().schema(s.clone()).mirror(mirror).synchronous_commit(sync)).unwrap();
        let mut lat = Vec::new();
        for i in 0..n_commits {
            let t = Instant::now();
            let mut tx = db.begin();
            tx.create_node(&["N"], props([("i", i as i64)])).unwrap();
            tx.commit().unwrap();
            lat.push(t.elapsed());
        }
        let total: Duration = lat.iter().sum();
        println!(
            "  postgres sync={:<5} mirror={:<5} median {:>9.1?}   {:>8.0} commits/s",
            sync,
            mirror,
            med(lat),
            n_commits as f64 / total.as_secs_f64()
        );
        drop(db);
        drop_schema(&url, &s);
    }

    println!("\n== bulk load: one commit, {nodes} nodes + {} edges", nodes * deg);
    let mut rng = 0x2545F4914F6CDD1Du64;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let edges: Vec<(usize, usize)> = (0..nodes).flat_map(|i| (0..deg).map(move |_| i)).map(|i| (i, 0)).collect::<Vec<_>>();
    let edges: Vec<(usize, usize)> = edges.into_iter().map(|(i, _)| (i, (next() % nodes as u64) as usize)).filter(|(a, b)| a != b).collect();
    let load = |db: &Database| {
        let t = Instant::now();
        let mut tx = db.begin();
        let ids: Vec<Id> = (0..nodes).map(|i| tx.create_node(&["N"], props([("k", i as i64)])).unwrap()).collect();
        for &(a, b) in &edges {
            tx.create_edge("E", ids[a], ids[b], 1.0, Props::new()).unwrap();
        }
        tx.commit().unwrap();
        t.elapsed()
    };
    let mem = Database::in_memory();
    println!("  in-memory (no durability)       {:>9.2?}", load(&mem));
    let mut last_schema = String::new();
    for mirror in [false, true] {
        let s = format!("nxbench_b_{}", mirror as u8);
        drop_schema(&url, &s);
        let db = Database::open_postgres_with(&url, PgOptions::default().schema(s.clone()).mirror(mirror)).unwrap();
        println!("  postgres mirror={:<5}           {:>9.2?}", mirror, load(&db));
        if mirror {
            last_schema = s.clone();
            let t = Instant::now();
            db.verify_mirror().unwrap();
            println!("  verify_mirror (full compare)    {:>9.2?}", t.elapsed());
            let t = Instant::now();
            db.rebuild_mirror().unwrap();
            println!("  rebuild_mirror                  {:>9.2?}", t.elapsed());
        }
        drop(db);
        if !mirror {
            drop_schema(&url, &s);
        }
    }

    println!("\n== restart (recover {} entities)", nodes + edges.len());
    let o = || PgOptions::default().schema(last_schema.clone());
    let t = Instant::now();
    let db = Database::open_postgres_with(&url, o()).unwrap();
    println!("  open from commit log            {:>9.2?}", t.elapsed());
    let t = Instant::now();
    db.checkpoint().unwrap();
    println!("  checkpoint                      {:>9.2?}", t.elapsed());
    drop(db);
    let t = Instant::now();
    let db = Database::open_postgres_with(&url, o()).unwrap();
    println!("  open from checkpoint            {:>9.2?}", t.elapsed());

    println!("\n== reads: same 1-hop question, in memory (NQL) vs SQL over the mirror");
    db.query("CREATE INDEX ON :N(k)").unwrap();
    let sample: Vec<i64> = (0..500).map(|_| (next() % nodes as u64) as i64).collect();
    let mut lat = Vec::new();
    for k in &sample {
        let t = Instant::now();
        let r = db.query(&format!("MATCH (a:N {{k:{k}}})-[:E]->(b) RETURN count(*)")).unwrap();
        lat.push(t.elapsed());
        assert!(matches!(r.rows[0][0], Value::Int(_)));
    }
    println!("  NQL, property index on k            median {:>9.1?}", med(lat));
    let mut c = Client::connect(&url, NoTls).unwrap();
    // The mirror ships GIN indexes on labels/props and edge_members(node_id); an index on
    // the first tail element (below) is what makes edge traversal by node id cheap in SQL.
    c.batch_execute(&format!("CREATE INDEX IF NOT EXISTS bench_src ON \"{last_schema}\".edges ((tail[1]))")).unwrap();
    c.batch_execute(&format!("ANALYZE \"{last_schema}\".nodes; ANALYZE \"{last_schema}\".edges")).unwrap();
    let st = c
        .prepare(&format!(
            "SELECT count(*) FROM \"{last_schema}\".nodes n JOIN \"{last_schema}\".binary_edges e ON e.src = n.id \
             WHERE n.props @> jsonb_build_object('k', $1::bigint)"
        ))
        .unwrap();
    let mut lat = Vec::new();
    for k in &sample {
        let t = Instant::now();
        c.query_one(&st, &[k]).unwrap();
        lat.push(t.elapsed());
    }
    println!("  SQL, GIN lookup on props + index on tail[1]   median {:>9.1?}", med(lat));
    drop(db);
    drop_schema(&url, &last_schema);
}
