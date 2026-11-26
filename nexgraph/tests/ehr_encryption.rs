//! Encryption at rest (file backend) on the eICU demo extract, including tampering.
#![cfg(feature = "security")]

use nexgraph::crypto::Keyring;
use nexgraph::ingest::import_csv_nodes;
use nexgraph::value::crc32;
use nexgraph::{Database, Error, SyncMode};
use std::fs;
use std::path::{Path, PathBuf};

const CSV: &str = include_str!("fixtures/eicu_demo_patient.csv");

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("nx_enc_{name}_{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    d
}

fn load(db: &Database) {
    import_csv_nodes(db, "Stay", CSV, Some("patientunitstayid")).unwrap();
}

fn all_bytes(d: &Path) -> Vec<u8> {
    let mut out = vec![];
    for e in fs::read_dir(d).unwrap() {
        let p = e.unwrap().path();
        if p.is_file() && p.file_name().unwrap() != "encryption.marker" {
            out.extend(fs::read(p).unwrap());
        }
    }
    out
}

fn contains(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

fn ring(b: u8) -> Keyring {
    Keyring::new(1, [b; 32])
}

fn count(db: &Database) -> i64 {
    match &db.query("MATCH (s:Stay) RETURN count(*) AS n").unwrap().rows[0][0] {
        nexgraph::Value::Int(n) => *n,
        v => panic!("{v:?}"),
    }
}

/// Split wal.log into (header-less) frames: `len u32 | crc u32 | payload`.
fn frames(b: &[u8]) -> Vec<Vec<u8>> {
    let mut v = vec![];
    let mut i = 0;
    while i + 8 <= b.len() {
        let n = u32::from_le_bytes(b[i..i + 4].try_into().unwrap()) as usize;
        v.push(b[i + 8..i + 8 + n].to_vec());
        i += 8 + n;
    }
    v
}

fn frame(p: &[u8]) -> Vec<u8> {
    let mut o = (p.len() as u32).to_le_bytes().to_vec();
    o.extend(crc32(p).to_le_bytes());
    o.extend(p);
    o
}

#[test]
fn no_plaintext_on_disk_and_roundtrip_works() {
    let (plain, enc) = (dir("plain"), dir("enc"));
    let a = Database::open_with(&plain, SyncMode::Always).unwrap();
    load(&a);
    let b = Database::open_encrypted(&enc, SyncMode::Always, ring(7)).unwrap();
    load(&b);
    let n = count(&b);
    assert_eq!(n, count(&a));
    // control: the check can fail. Real identifiers are visible in the plaintext directory ...
    let first = "Skilled Nursing Facility".to_string(); // a text value from the file
    let needles = [first.as_str(), "Caucasian", "uniquepid", "Female"];
    for s in needles {
        assert!(contains(&all_bytes(&plain), s), "control: {s} should be readable in the plaintext db");
        assert!(!contains(&all_bytes(&enc), s), "{s} readable in the encrypted db");
    }
    drop((a, b));
    // WAL-only reopen, then after a checkpoint
    let b = Database::open_encrypted(&enc, SyncMode::Always, ring(7)).unwrap();
    assert_eq!(count(&b), n);
    b.checkpoint().unwrap();
    for s in needles {
        assert!(!contains(&all_bytes(&enc), s), "{s} readable after checkpoint");
    }
    drop(b);
    let b = Database::open_encrypted(&enc, SyncMode::Always, ring(7)).unwrap();
    assert_eq!(count(&b), n);
}

#[test]
fn wrong_key_no_key_and_mixing_are_refused() {
    let (enc, plain) = (dir("k_enc"), dir("k_plain"));
    load(&Database::open_encrypted(&enc, SyncMode::Always, ring(1)).unwrap());
    assert!(Database::open_encrypted(&enc, SyncMode::Always, ring(2)).is_err(), "wrong key");
    assert!(matches!(Database::open_with(&enc, SyncMode::Always), Err(Error::Invalid(_))), "no key");
    load(&Database::open_with(&plain, SyncMode::Always).unwrap());
    assert!(matches!(Database::open_encrypted(&plain, SyncMode::Always, ring(1)), Err(Error::Invalid(_))), "plaintext data must not be silently mixed");
}

#[test]
fn authentication_catches_edits_even_when_the_frame_checksum_is_recomputed() {
    let d = dir("tamper");
    let db = Database::open_encrypted(&d, SyncMode::Always, ring(3)).unwrap();
    db.query("CREATE (:Stay {id: 1, age: 40})").unwrap();
    db.query("CREATE (:Stay {id: 2, age: 50})").unwrap();
    db.query("CREATE (:Stay {id: 3, age: 60})").unwrap();
    drop(db);
    let wal = d.join("wal.log");
    let orig = fs::read(&wal).unwrap();
    let fr = frames(&orig);
    assert!(fr.len() >= 3);

    // flip one ciphertext byte in the middle frame, fix the CRC: only the AEAD can notice
    let mut f2 = fr.clone();
    let k = f2[1].len() / 2;
    f2[1][k] ^= 0x01;
    fs::write(&wal, f2.iter().flat_map(|p| frame(p)).collect::<Vec<u8>>()).unwrap();
    assert!(Database::open_encrypted(&d, SyncMode::Always, ring(3)).is_err(), "edited record accepted");

    // swap two frames (reorder): position is bound into the record
    let mut f3 = fr.clone();
    f3.swap(0, 1);
    fs::write(&wal, f3.iter().flat_map(|p| frame(p)).collect::<Vec<u8>>()).unwrap();
    assert!(Database::open_encrypted(&d, SyncMode::Always, ring(3)).is_err(), "reordered records accepted");

    // duplicate a frame (replay)
    let mut f4 = fr.clone();
    f4.insert(1, fr[0].clone());
    fs::write(&wal, f4.iter().flat_map(|p| frame(p)).collect::<Vec<u8>>()).unwrap();
    assert!(Database::open_encrypted(&d, SyncMode::Always, ring(3)).is_err(), "replayed record accepted");

    // untouched log opens
    fs::write(&wal, &orig).unwrap();
    assert_eq!(Database::open_encrypted(&d, SyncMode::Always, ring(3)).unwrap().stats().live_nodes, 3);
}

/// A limit, stated as a test: dropping the newest records leaves a valid log, indistinguishable from a crash
/// before they were written. Encryption gives confidentiality and integrity, not freshness.
#[test]
fn known_limit_tail_truncation_is_not_detected() {
    let d = dir("trunc");
    let db = Database::open_encrypted(&d, SyncMode::Always, ring(3)).unwrap();
    for i in 0..3 {
        db.query(&format!("CREATE (:Stay {{id: {i}}})")).unwrap();
    }
    drop(db);
    let wal = d.join("wal.log");
    let fr = frames(&fs::read(&wal).unwrap());
    fs::write(&wal, fr[..fr.len() - 1].iter().flat_map(|p| frame(p)).collect::<Vec<u8>>()).unwrap();
    assert_eq!(Database::open_encrypted(&d, SyncMode::Always, ring(3)).unwrap().stats().live_nodes, 2);
}

#[test]
fn tampered_checkpoint_is_rejected_and_key_rotation_works() {
    let d = dir("rot");
    let old = || Keyring::new(1, [1u8; 32]);
    let db = Database::open_encrypted(&d, SyncMode::Always, old()).unwrap();
    load(&db);
    let n = count(&db);
    db.checkpoint().unwrap();
    drop(db);

    // rotate: new active key, old one kept to read; checkpoint re-seals everything under key 2
    let new_ring = || Keyring::new(2, [2u8; 32]).with_previous(1, [1u8; 32]);
    let db = Database::open_encrypted(&d, SyncMode::Always, new_ring()).unwrap();
    assert_eq!(count(&db), n);
    db.query("CREATE (:Stay {id: 999999})").unwrap(); // sealed under key 2 in the log
    db.checkpoint().unwrap();
    drop(db);
    assert!(Database::open_encrypted(&d, SyncMode::Always, old()).is_err(), "the old key alone must no longer open it");
    let only_new = Keyring::new(2, [2u8; 32]);
    assert_eq!(count(&Database::open_encrypted(&d, SyncMode::Always, only_new).unwrap()), n + 1);

    // flip a byte inside the checkpoint and fix its CRC
    let ck = d.join("checkpoint.bin");
    let orig = fs::read(&ck).unwrap();
    let mut p = frames(&orig).remove(0);
    let k = p.len() / 2;
    p[k] ^= 1;
    fs::write(&ck, frame(&p)).unwrap();
    assert!(Database::open_encrypted(&d, SyncMode::Always, Keyring::new(2, [2u8; 32])).is_err());
}
