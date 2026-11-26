//! The file backend refuses a second open of the same directory (v0.22): two writers on one WAL corrupt it.
use nexgraph::{Database, Error};
use std::process::Command;

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("nexgraph_lock_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

#[test]
fn second_open_is_refused_until_the_first_is_dropped() {
    let d = tmp("same_process");
    let db = Database::open(&d).unwrap();
    db.query("CREATE (:A {n: 1})").unwrap();
    match Database::open(&d) {
        Err(Error::Invalid(m)) => assert!(m.contains("already open"), "{m}"),
        Err(e) => panic!("wrong error: {e}"),
        Ok(_) => panic!("a second handle opened a locked directory"),
    }
    // the refused attempt must not have disturbed the first handle or the data
    db.query("CREATE (:A {n: 2})").unwrap();
    drop(db);
    let db = Database::open(&d).unwrap();
    assert_eq!(db.query("MATCH (a:A) RETURN count(*)").unwrap().rows[0][0], nexgraph::Value::Int(2));
    drop(db);
    let _ = std::fs::remove_dir_all(&d);
}

/// A different process is refused too. The child is this same test binary re-run with an env var, so no extra
/// executable is needed; it exits 0 if its open was refused with the lock error.
#[test]
fn other_process_is_refused() {
    if let Ok(dir) = std::env::var("NG_LOCK_CHILD_DIR") {
        let ok = matches!(Database::open(&dir), Err(Error::Invalid(m)) if m.contains("already open"));
        std::process::exit(if ok { 0 } else { 3 });
    }
    let d = tmp("other_process");
    let db = Database::open(&d).unwrap();
    db.query("CREATE (:A {n: 1})").unwrap();
    let st = Command::new(std::env::current_exe().unwrap())
        .args(["other_process_is_refused", "--exact", "--nocapture"])
        .env("NG_LOCK_CHILD_DIR", &d)
        .status()
        .unwrap();
    assert!(st.success(), "child process was not refused: {st:?}");
    drop(db);
    // once the holder is gone the same child-style open succeeds (no stale lock left behind)
    Database::open(&d).unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_crashed_holder_leaves_no_stale_lock() {
    let d = tmp("crash");
    if let Ok(dir) = std::env::var("NG_LOCK_CRASH_DIR") {
        let db = Database::open(&dir).unwrap();
        db.query("CREATE (:A {n: 1})").unwrap();
        std::process::abort(); // no destructors, no clean unlock
    }
    let st = Command::new(std::env::current_exe().unwrap())
        .args(["a_crashed_holder_leaves_no_stale_lock", "--exact", "--nocapture"])
        .env("NG_LOCK_CRASH_DIR", &d)
        .status()
        .unwrap();
    assert!(!st.success(), "child should have aborted");
    let db = Database::open(&d).expect("the OS releases the lock when the holder dies");
    assert_eq!(db.query("MATCH (a:A) RETURN count(*)").unwrap().rows[0][0], nexgraph::Value::Int(1));
    drop(db);
    let _ = std::fs::remove_dir_all(&d);
}
