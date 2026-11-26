//! v0.18 GAAP additions: ratios, open items / aging, atomic batch posting. Expected figures are worked out by hand in the comments.
use nexgraph::gaap::*;
use nexgraph::{Database, Value};
use std::collections::BTreeMap;

const CO: &str = "Acme Demonstration Corp";

fn books() -> Database {
    let db = Database::in_memory();
    let l = Ledger::new(&db).unwrap();
    l.create_company(CO, "Corporation", "USD", 1).unwrap();
    l.seed_reference_chart(CO).unwrap();
    l.add_period(CO, "FY2024", "Annual", "2024-01-01", "2024-12-31").unwrap();
    db
}

/// The v0.17 demonstration year, posted as ONE batch.
fn year_entries() -> Vec<Entry> {
    vec![
        Entry::new("2024-01-02", "Issue common stock").dr("101", 100000.0).cr("401", 100000.0),
        Entry::new("2024-01-05", "Buy equipment").dr("160", 24000.0).cr("101", 24000.0),
        Entry::new("2024-01-10", "Buy inventory on account").dr("120", 30000.0).cr("201", 30000.0),
        Entry::new("2024-02-01", "Credit sale").dr("110", 50000.0).cr("501", 50000.0),
        Entry::new("2024-02-01", "Cost of the sale").dr("601", 20000.0).cr("120", 20000.0),
        Entry::new("2024-02-15", "Collect receivable").dr("101", 40000.0).cr("110", 40000.0),
        Entry::new("2024-03-01", "Rent").dr("620", 3000.0).cr("101", 3000.0),
        Entry::new("2024-03-10", "Wages").dr("610", 8000.0).cr("101", 8000.0),
        Entry::new("2024-03-15", "Pay supplier").dr("201", 10000.0).cr("101", 10000.0),
        Entry::new("2024-03-31", "Customer return").dr("502", 1500.0).cr("110", 1500.0),
        Entry::new("2024-04-01", "Cash received in advance").dr("101", 5000.0).cr("230", 5000.0),
        Entry::new("2024-06-30", "Interest earned").dr("101", 300.0).cr("520", 300.0),
        Entry::new("2024-06-30", "Interest paid").dr("700", 700.0).cr("101", 700.0),
        Entry::new("2024-07-01", "Borrow on long-term note").dr("101", 20000.0).cr("301", 20000.0),
        Entry::new("2024-08-01", "Buy land").dr("150", 10000.0).cr("101", 10000.0),
        Entry::new("2024-12-15", "Declare dividend").dr("420", 4000.0).cr("250", 4000.0),
        Entry::adjusting("2024-12-31", "Depreciation, equipment", "630", "161", 2200.0),
        Entry::adjusting("2024-12-31", "Accrue wages", "610", "210", 1200.0),
        Entry::adjusting("2024-12-31", "Income tax", "690", "220", 5000.0),
    ]
}

fn close(a: f64, b: f64) {
    assert!((a - b).abs() < 1e-9 * b.abs().max(1.0), "{a} != {b}");
}

fn ratio(rows: &[RatioRow], name: &str) -> Option<f64> {
    rows.iter().find(|r| r.name == name).unwrap_or_else(|| panic!("no ratio {name}")).value
}

#[test]
fn ratios_match_hand_computation() {
    let db = books();
    let l = Ledger::new(&db).unwrap();
    let posted = l.post_many(CO, &year_entries()).unwrap();
    assert_eq!(posted.len(), 19);
    assert_eq!(posted.iter().map(|p| p.entry_no).collect::<Vec<_>>(), (1..=19).collect::<Vec<i64>>());
    let r = db.read(|v| ratios(v, CO, "2024-01-01", "2024-12-31")).unwrap();
    // Income statement: net revenue 48,500; gross profit 28,500; operating income 14,100; net income 8,700; COGS 20,000.
    // Balance sheet: cash 109,600 + A/R 8,500 + inventory 10,000 = current assets 128,100; PP&E 24,000-2,200+10,000 = 31,800; assets 159,900.
    // Current liabilities: A/P 20,000 + wages 1,200 + taxes 5,000 + unearned 5,000 + dividends 4,000 = 35,200; plus note 20,000 = liabilities 55,200.
    // Equity = 159,900 - 55,200 = 104,700. Opening balances are zero, so averages equal the ending balances.
    close(ratio(&r, "gross_margin").unwrap(), 28500.0 / 48500.0);
    close(ratio(&r, "operating_margin").unwrap(), 14100.0 / 48500.0);
    close(ratio(&r, "net_margin").unwrap(), 8700.0 / 48500.0);
    close(ratio(&r, "return_on_assets").unwrap(), 8700.0 / 159900.0);
    close(ratio(&r, "return_on_equity").unwrap(), 8700.0 / 104700.0);
    close(ratio(&r, "current_ratio").unwrap(), 128100.0 / 35200.0);
    close(ratio(&r, "quick_ratio").unwrap(), 118100.0 / 35200.0); // minus inventory 10,000
    close(ratio(&r, "cash_ratio").unwrap(), 109600.0 / 35200.0);
    close(ratio(&r, "working_capital").unwrap(), 92900.0);
    close(ratio(&r, "debt_ratio").unwrap(), 55200.0 / 159900.0);
    close(ratio(&r, "debt_to_equity").unwrap(), 55200.0 / 104700.0);
    close(ratio(&r, "equity_multiplier").unwrap(), 159900.0 / 104700.0);
    close(ratio(&r, "asset_turnover").unwrap(), 48500.0 / 159900.0);
    close(ratio(&r, "receivables_turnover").unwrap(), 48500.0 / 8500.0);
    close(ratio(&r, "days_sales_outstanding").unwrap(), 366.0 / (48500.0 / 8500.0)); // 2024 is a leap year
    close(ratio(&r, "inventory_turnover").unwrap(), 2.0);
    close(ratio(&r, "days_inventory").unwrap(), 183.0);
    // Closing the year changes none of the balance-sheet ratios.
    l.close_period(CO, "FY2024", ClosingAccounts::default()).unwrap();
    let r2 = db.read(|v| ratios(v, CO, "2024-01-01", "2024-12-31")).unwrap();
    close(ratio(&r2, "current_ratio").unwrap(), 128100.0 / 35200.0);
    close(ratio(&r2, "net_margin").unwrap(), 8700.0 / 48500.0);
}

#[test]
fn ratios_with_no_activity_are_none_not_nan() {
    let db = books();
    let r = db.read(|v| ratios(v, CO, "2024-01-01", "2024-12-31")).unwrap();
    for x in &r {
        if x.name == "working_capital" {
            assert_eq!(x.value, Some(0.0));
        } else {
            assert_eq!(x.value, None, "{}", x.name);
        }
    }
}

#[test]
fn second_year_ratios_use_average_balances() {
    let db = Database::in_memory();
    let l = Ledger::new(&db).unwrap();
    l.create_company("Two", "Corporation", "USD", 1).unwrap();
    l.seed_reference_chart("Two").unwrap();
    l.add_period("Two", "FY1", "Annual", "2025-01-01", "2025-12-31").unwrap();
    l.add_period("Two", "FY2", "Annual", "2026-01-01", "2026-12-31").unwrap();
    l.post_many("Two", &[
        Entry::new("2025-01-01", "Capital").dr("101", 100000.0).cr("401", 100000.0),
        Entry::new("2025-06-01", "Sale on account").dr("110", 20000.0).cr("501", 20000.0),
        Entry::new("2026-02-01", "Collect").dr("101", 20000.0).cr("110", 20000.0),
        Entry::new("2026-03-01", "Sale on account").dr("110", 30000.0).cr("501", 30000.0),
    ])
    .unwrap();
    let r = db.read(|v| ratios(v, "Two", "2026-01-01", "2026-12-31")).unwrap();
    // Assets: 120,000 at 2025-12-31 (cash 100,000 + A/R 20,000), 150,000 at 2026-12-31 (cash 120,000 + A/R 30,000): average 135,000.
    // Equity: 120,000 then 150,000: average 135,000. FY2 net income 30,000. Average A/R (20,000+30,000)/2 = 25,000.
    close(ratio(&r, "return_on_assets").unwrap(), 30000.0 / 135000.0);
    close(ratio(&r, "return_on_equity").unwrap(), 30000.0 / 135000.0);
    close(ratio(&r, "asset_turnover").unwrap(), 30000.0 / 135000.0);
    close(ratio(&r, "receivables_turnover").unwrap(), 30000.0 / 25000.0);
    close(ratio(&r, "days_sales_outstanding").unwrap(), 365.0 / 1.2);
}

#[test]
fn post_many_is_atomic_and_gapless() {
    let db = books();
    let l = Ledger::new(&db).unwrap();
    assert!(l.post_many(CO, &[]).unwrap().is_empty());
    let good = |d: &str| Entry::new(d, "ok").dr("101", 10.0).cr("401", 10.0);
    let r = l.post_many(CO, &[good("2024-01-02"), good("2024-01-03")]).unwrap();
    assert_eq!(r.iter().map(|p| p.entry_no).collect::<Vec<_>>(), vec![1, 2]);

    // Entry 2 of the next batch is unbalanced: nothing from the batch is posted and no number is consumed.
    let e = l.post_many(CO, &[good("2024-01-04"), Entry::new("2024-01-05", "bad").dr("101", 10.0).cr("401", 9.99), good("2024-01-06")]).unwrap_err().to_string();
    assert!(e.contains("entry 2") && e.contains("not balanced"), "{e}");
    let n = db.query("MATCH (e:JournalEntry) RETURN e.no").unwrap().rows.len();
    assert_eq!(n, 2);
    // Unknown account in entry 3, closed-kind entry, and a date outside every period are all refused whole.
    let e = l.post_many(CO, &[good("2024-01-04"), good("2024-01-04"), Entry::new("2024-01-05", "x").dr("999", 1.0).cr("401", 1.0)]).unwrap_err().to_string();
    assert!(e.contains("entry 3") && e.contains("999"), "{e}");
    assert!(l.post_many(CO, &[good("2024-01-04").kind(EntryKind::Closing)]).is_err());
    assert!(l.post_many(CO, &[good("2023-12-31")]).is_err());
    assert_eq!(db.query("MATCH (e:JournalEntry) RETURN e.no").unwrap().rows.len(), 2);
    // Numbering continues without a gap and the audit is clean.
    assert_eq!(l.post(CO, &good("2024-01-07")).unwrap().entry_no, 3);
    let audit = db.query(&format!("CALL gaap.audit('{CO}') YIELD check, status, count, detail RETURN check, status")).unwrap();
    assert!(audit.rows.iter().all(|r| matches!(&r[1], Value::Str(s) if s == "pass")), "{:?}", audit.rows);
}

fn ar(l: &Ledger, date: &str, rf: &str, amt: f64) -> Entry {
    let _ = l;
    Entry::new(date, "invoice").reference(rf).dr("110", amt).cr("501", amt)
}
fn pay(date: &str, rf: &str, amt: f64) -> Entry {
    Entry::new(date, "payment").reference(rf).dr("101", amt).cr("110", amt)
}

#[test]
fn aging_by_reference_with_partial_payment_and_reversal() {
    let db = books();
    let l = Ledger::new(&db).unwrap();
    l.post_many(CO, &[
        ar(&l, "2024-01-10", "INV-1", 1000.0),
        pay("2024-02-01", "INV-1", 400.0),
        ar(&l, "2024-02-20", "INV-2", 2000.0),
        pay("2024-03-05", "INV-2", 2000.0), // INV-2 fully settled
        ar(&l, "2024-05-01", "INV-3", 500.0),
        ar(&l, "2024-06-10", "INV-4", 300.0),
        Entry::new("2024-06-20", "sale without reference").dr("110", 250.0).cr("501", 250.0),
    ])
    .unwrap();
    let inv5 = l.post(CO, &ar(&l, "2024-06-01", "INV-5", 700.0)).unwrap();
    l.reverse(CO, inv5.entry_no, "2024-06-15", None).unwrap(); // reversal must settle INV-5, not open a REV-n item

    let items = db.read(|v| open_items(v, CO, "110", "2024-06-30")).unwrap();
    let got: Vec<(&str, i64, i64, &str)> = items.iter().map(|i| (i.reference.as_str(), i.days, i.amount, i.bucket)).collect();
    // INV-1: 1,000-400 = 600 since 01-10 = 172 days (21+29+31+30+31+30). INV-3: 500, 60 days. INV-4: 300, 20 days.
    // The unreferenced entry is entry 7, 250, 10 days. INV-2 and INV-5 are settled and absent.
    assert_eq!(got, vec![("INV-1", 172, 60000, "91+"), ("INV-3", 60, 50000, "31-60"), ("INV-4", 20, 30000, "0-30"), ("(entry 7)", 10, 25000, "0-30")]);

    let (rows, balance) = db.read(|v| aging(v, CO, "110", "2024-06-30")).unwrap();
    assert_eq!(rows, vec![("0-30", 2, 55000), ("31-60", 1, 50000), ("61-90", 0, 0), ("91+", 1, 60000)]);
    assert_eq!(balance, 165000); // 1,000+2,000+500+300+250+700 - 400 - 2,000 - 700 = 1,650
    assert_eq!(rows.iter().map(|r| r.2).sum::<i64>(), balance);

    // As of an earlier date the books are the books as of then: INV-1 is 31 days old on 2024-02-10 and INV-2 does not exist yet.
    let early = db.read(|v| open_items(v, CO, "110", "2024-02-10")).unwrap();
    assert_eq!(early.len(), 1);
    assert_eq!((early[0].reference.as_str(), early[0].days, early[0].amount, early[0].bucket), ("INV-1", 31, 60000, "31-60"));

    assert!(db.read(|v| open_items(v, CO, "999", "2024-06-30")).is_err());
    assert!(db.read(|v| open_items(v, CO, "110", "2024-13-01")).is_err());
}

#[test]
fn payables_age_in_the_credit_direction() {
    let db = books();
    let l = Ledger::new(&db).unwrap();
    l.post_many(CO, &[
        Entry::new("2024-03-01", "bill").reference("BILL-1").dr("120", 800.0).cr("201", 800.0),
        Entry::new("2024-03-20", "part payment").reference("BILL-1").dr("201", 300.0).cr("101", 300.0),
    ])
    .unwrap();
    let items = db.read(|v| open_items(v, CO, "201", "2024-04-15")).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!((items[0].amount, items[0].days, items[0].bucket), (50000, 45, "31-60"));
}

#[test]
fn randomized_open_items_equal_the_ledger() {
    // Deterministic LCG; 1,500 invoices, each with 0-3 partial payments that never overpay.
    let mut s: u64 = 0x9E3779B97F4A7C15;
    let mut next = move |n: u64| -> u64 {
        s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (s >> 33) % n
    };
    let md = [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]; // 2024
    let date = |mut d: u64| -> String {
        for (m, n) in md.iter().enumerate() {
            if d < *n {
                return format!("2024-{:02}-{:02}", m + 1, d + 1);
            }
            d -= n;
        }
        unreachable!()
    };
    let db = books();
    let l = Ledger::new(&db).unwrap();
    let mut entries = Vec::new();
    let mut expect: BTreeMap<String, (i64, u64)> = BTreeMap::new(); // reference -> (open cents, invoice day)
    for i in 0..1500 {
        let (day, cents) = (next(300), 1000 + next(500000) as i64);
        let rf = format!("INV-{i}");
        entries.push(ar(&l, &date(day), &rf, cents as f64 / 100.0));
        let mut left = cents;
        for _ in 0..next(4) {
            if left == 0 {
                break;
            }
            let part = if next(3) == 0 { left } else { 1 + next(left as u64) as i64 };
            let pd = day + next(60);
            entries.push(pay(&date(pd), &rf, part as f64 / 100.0));
            left -= part;
        }
        expect.insert(rf, (left, day));
    }
    l.post_many(CO, &entries).unwrap();
    let asof = "2024-12-31"; // day 365 of the leap year is index 365; days = 365 - invoice day index
    let items = db.read(|v| open_items(v, CO, "110", asof)).unwrap();
    let want: BTreeMap<String, (i64, u64)> = expect.into_iter().filter(|(_, v)| v.0 != 0).collect();
    assert_eq!(items.len(), want.len());
    for it in &items {
        let (amt, day) = want[&it.reference];
        assert_eq!(it.amount, amt, "{}", it.reference);
        assert_eq!(it.days, 365 - day as i64, "{}", it.reference);
    }
    let (rows, balance) = db.read(|v| aging(v, CO, "110", asof)).unwrap();
    assert_eq!(rows.iter().map(|r| r.2).sum::<i64>(), balance);
    assert_eq!(balance, want.values().map(|v| v.0).sum::<i64>());
    assert_eq!(rows.iter().map(|r| r.1).sum::<i64>() as usize, want.len());
}

#[test]
fn nql_procedures() {
    let db = books();
    let l = Ledger::new(&db).unwrap();
    l.post_many(CO, &year_entries()).unwrap();
    let r = db.query(&format!("CALL gaap.ratios('{CO}', '2024-01-01', '2024-12-31') YIELD group, ratio, value, basis RETURN ratio, value")).unwrap();
    let cr = r.rows.iter().find(|x| matches!(&x[0], Value::Str(s) if s == "current_ratio")).unwrap();
    match &cr[1] {
        Value::Float(f) => close(*f, 128100.0 / 35200.0),
        o => panic!("{o:?}"),
    }
    let a = db.query(&format!("CALL gaap.aging('{CO}', '110', '2024-12-31') YIELD bucket, items, amount RETURN bucket, items, amount")).unwrap();
    let get = |name: &str| a.rows.iter().find(|x| matches!(&x[0], Value::Str(s) if s == name)).unwrap().clone();
    assert!(matches!(get("Difference")[2], Value::Float(f) if f == 0.0));
    assert!(matches!(get("Ledger balance")[2], Value::Float(f) if f == 8500.0));
    let o = db.query(&format!("CALL gaap.open_items('{CO}', '110', '2024-12-31') YIELD reference, date, days, amount, bucket RETURN reference, amount")).unwrap();
    // The demo entries carry no references, so each A/R entry is its own item: the sale (entry 4, +50,000), the collection
    // (entry 6, -40,000) and the return (entry 10, -1,500), which together are the 8,500 balance.
    let items: Vec<(String, f64)> = o.rows.iter().map(|r| (match &r[0] { Value::Str(s) => s.clone(), x => panic!("{x:?}") }, match &r[1] { Value::Float(f) => *f, x => panic!("{x:?}") })).collect();
    assert_eq!(items, vec![("(entry 4)".to_string(), 50000.0), ("(entry 6)".to_string(), -40000.0), ("(entry 10)".to_string(), -1500.0)]);
}
