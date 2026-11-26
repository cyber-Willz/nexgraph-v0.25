//! `nexgraph [DIR | postgres://URL]` — interactive shell. Statements end with
//! `;` (or a blank line). Without a target the database is in memory.
//! Meta commands: `.checkpoint`, `.stats`, `.quit`, `.mode table|csv|json`, `.drift OLD_TS NEW_TS`,
//! `.powerbi DIR` (export typed CSV tables + a Power Query script for Power BI),
//! `.import nodes|edges|events ...` (feature `ingest`); for PostgreSQL also
//! `.mirror`, `.verify`, `.rebuild`, `.refresh`. See `nexgraph --help`.

use nexgraph::Database;
use std::io::{self, BufRead, Write};

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Table,
    Csv,
    Json,
}

fn show(r: &nexgraph::QueryResult, mode: Mode) {
    if r.columns.is_empty() {
        println!("ok {:?} (ts {})", r.stats, r.ts);
        return;
    }
    match mode {
        Mode::Table => {
            println!("{}", r.to_table());
            println!("({} rows, ts {})", r.rows.len(), r.ts);
        }
        Mode::Csv => print!("{}", r.to_csv()),
        Mode::Json => println!("{}", r.to_json()),
    }
}

const IMPORT_USAGE: &str = "usage:
  .import nodes  LABEL FILE.csv [KEYCOL]
  .import edges  EDGELABEL FILE.csv SRCCOL DSTCOL NODELABEL NODEKEY [WEIGHTCOL]
  .import events suricata|zeek FILE.jsonl
  .import hyper  EDGELABEL FILE.csv KEYPROP COL:Label[,COL:Label...] [TIMECOL[,TIMECOL...]|-] [dedup=row|dedup=key:COL] [props] [single] [dates=iso|mdy|dmy]   (one hyperedge per row)";

#[cfg(feature = "ingest")]
fn import_cmd(db: &Database, t: &str) {
    use nexgraph::ingest::{self, EdgeCsv, Preset};
    let a: Vec<&str> = t.split_whitespace().skip(1).collect();
    let read = |p: &str| std::fs::read_to_string(p).map_err(|e| format!("cannot read {p}: {e}"));
    let res = match a.as_slice() {
        ["nodes", label, file] | ["nodes", label, file, _] => read(file).and_then(|s| ingest::import_csv_nodes(db, label, &s, a.get(3).copied()).map_err(|e| e.to_string())),
        ["edges", el, file, sc, dc, nl, nk] | ["edges", el, file, sc, dc, nl, nk, _] => read(file).and_then(|s| {
            let o = EdgeCsv { edge_label: el, src_col: sc, dst_col: dc, node_label: nl, node_key: nk, weight_col: a.get(7).copied() };
            ingest::import_csv_edges(db, &o, &s).map_err(|e| e.to_string())
        }),
        ["hyper", el, file, kp, members, rest @ ..] => read(file).and_then(|s| {
            let m: Vec<(&str, &str)> = members.split(',').filter_map(|x| x.split_once(':')).collect();
            if m.is_empty() || m.len() != members.split(',').count() {
                return Err("members must be COL:Label[,COL:Label...]".to_string());
            }
            // optional: TIMECOLS (or `-`), then any of `dedup=row`, `dedup=key:COL`, `props`
            let tc: Vec<&str> = rest.first().filter(|t| **t != "-" && !t.starts_with("dedup=") && **t != "props" && **t != "single" && !t.starts_with("dates=")).map(|t| t.split(',').collect()).unwrap_or_default();
            let mut opts = ingest::HyperOpts::default();
            for x in rest.iter().skip(if tc.is_empty() && rest.first().map_or(false, |t| *t == "-") { 1 } else if tc.is_empty() { 0 } else { 1 }) {
                match *x {
                    "props" => opts.keep_member_props = true,
                    "single" => opts.allow_single_member = true,
                    d if d.starts_with("dates=") => opts.date_order = nexgraph::timeutil::DateOrder::parse(&d[6..]).ok_or_else(|| format!("unknown date order '{}' (iso, mdy, dmy)", &d[6..]))?,
                    "dedup=row" => opts.dedup = ingest::Dedup::ExactRow,
                    d if d.starts_with("dedup=key:") && d.len() > 10 => opts.dedup = ingest::Dedup::Key(&d[10..]),
                    other => return Err(format!("unknown option '{other}' (use dedup=row, dedup=key:COL, props, single, dates=iso|mdy|dmy)")),
                }
            }
            let o = ingest::HyperCsv { edge_label: el, members: &m, key_prop: kp, weight_col: None, time_cols: &tc };
            ingest::import_csv_hyperedges_with(db, &o, &s, &opts).map_err(|e| e.to_string())
        }),
        ["events", preset, file] => preset.parse::<Preset>().map_err(|e| e.to_string()).and_then(|p| read(file).and_then(|s| ingest::import_events(db, &s, p).map_err(|e| e.to_string()))),
        _ => Err(IMPORT_USAGE.to_string()),
    };
    match res {
        Ok(r) => println!("{r:?}"),
        Err(e) => eprintln!("{e}"),
    }
}

const USAGE: &str = "usage: nexgraph [TARGET] [--schema NAME] [--no-mirror] [--read-only] [--async-commit]
  TARGET   a directory (file WAL), a postgres:// or postgresql:// URL, or omitted for in-memory.
           For PostgreSQL the URL may also come from $NEXGRAPH_PG_URL (use `--pg` to select it).
  --schema NAME    PostgreSQL schema that holds the database (default: nexgraph)
  --no-mirror      do not maintain the relational mirror tables
  --read-only      open as a replica; use `.refresh` to follow the writer
  --async-commit   synchronous_commit=off for this session (faster, last commits may be lost on server crash)";

fn open_db(args: &[String]) -> Result<(Database, String), String> {
    let mut target: Option<String> = None;
    let mut schema = "nexgraph".to_string();
    let (mut mirror, mut read_only, mut sync_commit, mut use_env) = (true, false, true, false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--schema" => schema = it.next().ok_or("--schema needs a value")?.clone(),
            "--no-mirror" => mirror = false,
            "--read-only" => read_only = true,
            "--async-commit" => sync_commit = false,
            "--pg" => use_env = true,
            "-h" | "--help" => return Err(USAGE.to_string()),
            x if x.starts_with("--") => return Err(format!("unknown option {x}\n{USAGE}")),
            x => target = Some(x.to_string()),
        }
    }
    if use_env && target.is_none() {
        target = Some(std::env::var("NEXGRAPH_PG_URL").map_err(|_| "--pg needs $NEXGRAPH_PG_URL".to_string())?);
    }
    match target {
        None => Ok((Database::in_memory(), "in-memory".into())),
        Some(t) if t.starts_with("postgres://") || t.starts_with("postgresql://") => {
            #[cfg(feature = "postgres")]
            {
                let opts = nexgraph::PgOptions::default().schema(schema.clone()).mirror(mirror).read_only(read_only).synchronous_commit(sync_commit);
                let db = Database::open_postgres_with(&t, opts).map_err(|e| format!("cannot open PostgreSQL database: {e}"))?;
                Ok((db, format!("postgres schema \"{schema}\"{}", if read_only { " (read-only)" } else { "" })))
            }
            #[cfg(not(feature = "postgres"))]
            {
                let _ = (schema, mirror, read_only, sync_commit);
                Err("this build has no PostgreSQL support (rebuild with the `postgres` feature)".into())
            }
        }
        Some(dir) => {
            let db = Database::open(&dir).map_err(|e| format!("cannot open {dir}: {e}"))?;
            Ok((db, dir))
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (db, arg) = match open_db(&args) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    let arg = Some(arg);
    let interactive = atty_stdin();
    if interactive {
        println!("nexgraph {} — {}", env!("CARGO_PKG_VERSION"), arg.as_deref().unwrap_or("in-memory"));
    }
    let stdin = io::stdin();
    let mut mode = Mode::Table;
    let mut buf = String::new();
    let mut code = 0;
    let prompt = |cont: bool| {
        if interactive {
            print!("{}", if cont { "   ... " } else { "nql> " });
            let _ = io::stdout().flush();
        }
    };
    prompt(false);
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let t = line.trim();
        if buf.is_empty() && t.starts_with('.') {
            match t.split_whitespace().next().unwrap_or("") {
                ".quit" | ".exit" => break,
                ".mode" => match t.split_whitespace().nth(1) {
                    Some("table") => mode = Mode::Table,
                    Some("csv") => mode = Mode::Csv,
                    Some("json") => mode = Mode::Json,
                    _ => eprintln!("usage: .mode table|csv|json"),
                },
                ".drift" => {
                    let a: Vec<Option<u64>> = t.split_whitespace().skip(1).map(|x| x.parse().ok()).collect();
                    match a.as_slice() {
                        [Some(o), Some(n)] => match nexgraph::drift(&db, *o, *n) {
                            Ok(d) => println!(
                                "new nodes {:?}\nremoved nodes {:?}\nnovel links between existing nodes ({}): {:?}\nlinks to new nodes {}, removed links {}, link jaccard {:.4}",
                                d.new_nodes, d.removed_nodes, d.novel_links.len(), d.novel_links, d.links_to_new_nodes, d.removed_links, d.link_jaccard
                            ),
                            Err(e) => eprintln!("{e}"),
                        },
                        _ => eprintln!("usage: .drift OLD_TS NEW_TS"),
                    }
                }
                ".powerbi" => match t.split_whitespace().nth(1) {
                    Some(dir) => match nexgraph::bi::export_dir(&db, dir) {
                        Ok(r) => {
                            for (n, rows) in &r.tables {
                                println!("{n}.csv  {rows} rows");
                            }
                            println!("wrote {} tables, catalog.json and nexgraph_folder.pq to {} (snapshot ts {})", r.tables.len(), r.dir.display(), r.ts);
                        }
                        Err(e) => eprintln!("{e}"),
                    },
                    None => eprintln!("usage: .powerbi DIR"),
                },
                #[cfg(feature = "ingest")]
                ".import" => import_cmd(&db, t),
                #[cfg(not(feature = "ingest"))]
                ".import" => eprintln!("this build has no import support (rebuild with the `ingest` feature)"),
                ".checkpoint" => match db.checkpoint() {
                    Ok(ts) => println!("checkpoint at ts {ts}"),
                    Err(e) => eprintln!("{e}"),
                },
                ".stats" => println!("{:?} last_ts={}", db.stats(), db.last_ts()),
                #[cfg(feature = "postgres")]
                ".mirror" => match db.mirror_stats() {
                    Ok(m) => println!("{m:?}"),
                    Err(e) => eprintln!("{e}"),
                },
                #[cfg(feature = "postgres")]
                ".verify" => match db.verify_mirror() {
                    Ok(r) => println!("{} {r:?}", if r.is_clean() { "mirror OK" } else { "MIRROR DIFFERS" }),
                    Err(e) => eprintln!("{e}"),
                },
                #[cfg(feature = "postgres")]
                ".rebuild" => match db.rebuild_mirror() {
                    Ok(m) => println!("rebuilt: {m:?}"),
                    Err(e) => eprintln!("{e}"),
                },
                #[cfg(feature = "postgres")]
                ".refresh" => match db.refresh() {
                    Ok(r) => println!("{r:?}"),
                    Err(e) => eprintln!("{e}"),
                },
                _ => eprintln!("unknown command {t}"),
            }
            prompt(false);
            continue;
        }
        buf.push_str(&line);
        buf.push('\n');
        if t.ends_with(';') || (t.is_empty() && !buf.trim().is_empty()) {
            let stmt = buf.trim().trim_end_matches(';').to_string();
            buf.clear();
            if !stmt.trim().is_empty() {
                match db.query(&stmt) {
                    Ok(r) => show(&r, mode),
                    Err(e) => {
                        eprintln!("{e}");
                        code = 1;
                    }
                }
            }
            prompt(false);
        } else {
            prompt(!buf.trim().is_empty());
        }
    }
    if !buf.trim().is_empty() {
        match db.query(buf.trim().trim_end_matches(';')) {
            Ok(r) => show(&r, mode),
            Err(e) => {
                eprintln!("{e}");
                code = 1;
            }
        }
    }
    std::process::exit(code);
}

fn atty_stdin() -> bool {
    // No libc dependency: treat as interactive only when explicitly asked.
    std::env::var_os("NEXGRAPH_INTERACTIVE").is_some()
}
