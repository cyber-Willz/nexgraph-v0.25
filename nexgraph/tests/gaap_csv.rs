//! v0.18 journal CSV import (feature `ingest`).
#![cfg(feature = "ingest")]
use nexgraph::gaap::*;
use nexgraph::Database;

const CO: &str = "CSV Co";

fn books() -> Database {
    let db = Database::in_memory();
    let l = Ledger::new(&db).unwrap();
    l.create_company(CO, "Corporation", "USD", 1).unwrap();
    l.seed_reference_chart(CO).unwrap();
    l.add_period(CO, "FY2024", "Annual", "2024-01-01", "2024-12-31").unwrap();
    db
}
fn entry_count(db: &Database) -> usize {
    db.query("MATCH (e:JournalEntry) RETURN e.no").unwrap().rows.len()
}

#[test]
fn imports_lone_cr_files_atomically() {
    let db = books();
    let l = Ledger::new(&db).unwrap();
    // Old-Mac line endings (a bare \r), as in the public Superstore file; one entry spans two rows.
    let csv = "entry,date,account,debit,credit,reference,description,memo\r1,2024-01-02,101,1000.00,,,Capital,cash in\r1,2024-01-02,401,,1000.00,,,\r2,2024-01-05,110,250.50,,INV-9,Sale,\r2,2024-01-05,501,,250.50,,,\r";
    let r = l.import_journal_csv(CO, csv).unwrap();
    assert_eq!(r.iter().map(|p| p.entry_no).collect::<Vec<_>>(), vec![1, 2]);
    let items = db.read(|v| open_items(v, CO, "110", "2024-01-31")).unwrap();
    assert_eq!((items[0].reference.as_str(), items[0].amount, items[0].days), ("INV-9", 25050, 26));
}

#[test]
fn bad_rows_reject_the_whole_file_and_name_the_line() {
    let db = books();
    let l = Ledger::new(&db).unwrap();
    let unbalanced = "entry,date,account,debit,credit\na,2024-01-02,101,10,\na,2024-01-02,401,,10\nb,2024-01-03,101,10,\nb,2024-01-03,401,,9\n";
    let e = l.import_journal_csv(CO, unbalanced).unwrap_err().to_string();
    assert!(e.contains("entry 'b'") && e.contains("line 4") && e.contains("not balanced"), "{e}");
    assert_eq!(entry_count(&db), 0); // entry 'a' was fine but nothing is posted
    let e = l.import_journal_csv(CO, "entry,date,account,debit,credit\na,2024-01-02,101,ten,\n").unwrap_err().to_string();
    assert!(e.contains("line 2") && e.contains("not a number"), "{e}");
    let e = l.import_journal_csv(CO, "entry,date,account,debit,credit\na,2024-01-02,101,10,\na,2024-01-09,401,,10\n").unwrap_err().to_string();
    assert!(e.contains("two different dates"), "{e}");
    assert!(l.import_journal_csv(CO, "entry,date,account\na,2024-01-02,101\n").is_err()); // no debit/credit column
    assert!(l.import_journal_csv(CO, "").is_err());
    assert_eq!(entry_count(&db), 0);
    // the books still accept a normal post as entry 1
    assert_eq!(l.post(CO, &Entry::new("2024-01-02", "x").dr("101", 1.0).cr("401", 1.0)).unwrap().entry_no, 1);
}
