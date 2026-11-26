//! v0.17 GAAP ledger. The scenario's figures were worked out by hand (see the comments), not taken from the code.
use nexgraph::gaap::*;
use nexgraph::{Database, Value};

const CO: &str = "Acme Demonstration Corp";

fn books() -> Database {
    let db = Database::in_memory();
    let l = Ledger::new(&db).unwrap();
    l.create_company(CO, "Corporation", "USD", 1).unwrap();
    l.seed_reference_chart(CO).unwrap();
    l.add_period(CO, "FY2024", "Annual", "2024-01-01", "2024-12-31").unwrap();
    l.add_period(CO, "Q1 2024", "Quarterly", "2024-01-01", "2024-03-31").unwrap();
    l.add_period(CO, "Q2 2024", "Quarterly", "2024-04-01", "2024-06-30").unwrap();
    db
}

/// Posts the year's entries. Returns the commit timestamp after entry 10 (end of Q1 activity).
fn cycle(db: &Database) -> u64 {
    let l = Ledger::new(db).unwrap();
    let post = |e: Entry| l.post(CO, &e).unwrap();
    post(Entry::new("2024-01-02", "Issue common stock").dr("101", 100000.0).cr("401", 100000.0));
    post(Entry::new("2024-01-05", "Buy equipment").journal("Cash Disbursements").dr("160", 24000.0).cr("101", 24000.0));
    post(Entry::new("2024-01-10", "Buy inventory on account").journal("Purchases").dr("120", 30000.0).cr("201", 30000.0));
    post(Entry::new("2024-02-01", "Credit sale").journal("Sales").dr("110", 50000.0).cr("501", 50000.0));
    post(Entry::new("2024-02-01", "Cost of the sale").dr("601", 20000.0).cr("120", 20000.0));
    post(Entry::new("2024-02-15", "Collect receivable").journal("Cash Receipts").dr("101", 40000.0).cr("110", 40000.0));
    post(Entry::new("2024-03-01", "Rent").dr("620", 3000.0).cr("101", 3000.0));
    post(Entry::new("2024-03-10", "Wages").journal("Payroll").dr("610", 8000.0).cr("101", 8000.0));
    post(Entry::new("2024-03-15", "Pay supplier").dr("201", 10000.0).cr("101", 10000.0));
    post(Entry::new("2024-03-31", "Customer return").dr("502", 1500.0).cr("110", 1500.0));
    let ts = db.last_ts();
    post(Entry::new("2024-04-01", "Cash received in advance").dr("101", 5000.0).cr("230", 5000.0));
    post(Entry::new("2024-06-30", "Interest earned").dr("101", 300.0).cr("520", 300.0));
    post(Entry::new("2024-06-30", "Interest paid").dr("700", 700.0).cr("101", 700.0));
    post(Entry::new("2024-07-01", "Borrow on long-term note").dr("101", 20000.0).cr("301", 20000.0));
    post(Entry::new("2024-08-01", "Buy land").dr("150", 10000.0).cr("101", 10000.0));
    post(Entry::new("2024-12-15", "Declare dividend").dr("420", 4000.0).cr("250", 4000.0));
    post(Entry::adjusting("2024-12-31", "Depreciation, equipment", "630", "161", 2200.0));
    post(Entry::adjusting("2024-12-31", "Accrue wages", "610", "210", 1200.0));
    post(Entry::adjusting("2024-12-31", "Income tax", "690", "220", 5000.0));
    ts
}

fn amount(rows: &[StmtRow], name: &str) -> f64 {
    dollars(rows.iter().find(|r| r.name == name).unwrap_or_else(|| panic!("no row {name}")).amount)
}

#[test]
fn income_statement_matches_hand_computation() {
    let db = books();
    cycle(&db);
    let is = db.read(|v| income_statement(v, CO, "2024-01-01", "2024-12-31")).unwrap();
    // net revenue 50,000 - 1,500 returns = 48,500; COGS 20,000; gross profit 28,500
    assert_eq!(amount(&is, "Net revenue"), 48500.0);
    assert_eq!(amount(&is, "Gross profit"), 28500.0);
    // opex = rent 3,000 + wages 8,000 + accrued 1,200 + depreciation 2,200 = 14,400; operating income 14,100
    assert_eq!(amount(&is, "Total operating expenses"), 14400.0);
    assert_eq!(amount(&is, "Operating income"), 14100.0);
    // + 300 interest revenue - 700 interest expense = 13,700 before tax; tax 5,000; net income 8,700
    assert_eq!(amount(&is, "Income before income taxes"), 13700.0);
    assert_eq!(amount(&is, "Net income"), 8700.0);
    assert_eq!(is.last().unwrap().kind, "total");
    // the sales returns contra account reduces revenue and is shown negative
    assert_eq!(dollars(is.iter().find(|r| r.number == "502").unwrap().amount), -1500.0);
}

#[test]
fn balance_sheet_balances_before_closing_and_matches_hand_totals() {
    let db = books();
    cycle(&db);
    let bs = db.read(|v| balance_sheet(v, CO, "2024-12-31")).unwrap();
    // cash 109,600 + AR 8,500 + inventory 10,000 + land 10,000 + equipment 24,000 - accumulated depreciation 2,200
    assert_eq!(amount(&bs, "Total current assets"), 128100.0);
    assert_eq!(amount(&bs, "Net property, plant and equipment"), 31800.0); // equipment 24,000 + land 10,000 - 2,200
    assert_eq!(amount(&bs, "Total assets"), 159900.0);
    // AP 20,000 + wages 1,200 + tax 5,000 + unearned 5,000 + dividends payable 4,000 + note 20,000
    assert_eq!(amount(&bs, "Total liabilities"), 55200.0);
    // retained earnings = net income 8,700 - dividends 4,000, although nothing is closed yet
    assert_eq!(amount(&bs, "Retained earnings (including income not yet closed, net of dividends)"), 4700.0);
    assert_eq!(amount(&bs, "Total stockholders' equity"), 104700.0);
    assert_eq!(bs.last().unwrap().amount, 0, "A = L + E");
}

#[test]
fn retained_earnings_and_cash_flow() {
    let db = books();
    cycle(&db);
    let re = db.read(|v| retained_earnings_statement(v, CO, "2024-01-01", "2024-12-31")).unwrap();
    assert_eq!(amount(&re, "Retained earnings, beginning"), 0.0);
    assert_eq!(amount(&re, "Add: net income"), 8700.0);
    assert_eq!(amount(&re, "Less: dividends"), -4000.0);
    assert_eq!(amount(&re, "Retained earnings, ending"), 4700.0);
    let cf = db.read(|v| cash_flow(v, CO, "2024-01-01", "2024-12-31")).unwrap();
    // operating: 8,700 - AR 8,500 - inventory 10,000 + AP 20,000 + wages 1,200 + tax 5,000 + unearned 5,000 + depreciation 2,200
    assert_eq!(amount(&cf, "Net cash from operating activities"), 23600.0);
    assert_eq!(amount(&cf, "Net cash from investing activities"), -34000.0); // equipment 24,000 + land 10,000
    assert_eq!(amount(&cf, "Net cash from financing activities"), 120000.0); // stock 100,000 + note 20,000 (dividend declared = payable)
    assert_eq!(amount(&cf, "Net increase (decrease) in cash"), 109600.0);
    assert_eq!(amount(&cf, "Cash, end of period"), 109600.0);
    assert_eq!(cf.last().unwrap().amount, 0, "reconciles to the cash accounts");
}

#[test]
fn trial_balance_kinds() {
    let db = books();
    cycle(&db);
    let tb = |k| db.read(|v| trial_balance(v, CO, k, "2024-01-01", "2024-12-31")).unwrap();
    let un = tb(TbKind::Unadjusted);
    let adj = tb(TbKind::Adjusted);
    assert!(un.balanced() && adj.balanced());
    // the three adjusting entries add 2,200 + 1,200 + 5,000 to each side
    assert_eq!(adj.debits - un.debits, 840000);
    assert!(un.rows.iter().all(|r| r.number != "630" && r.number != "690"));
    // equipment 24,000 and accumulated depreciation 2,200 on their own sides
    let r = |t: &TrialBalance, n: &str| t.rows.iter().find(|x| x.number == n).map(|x| (x.debit, x.credit));
    assert_eq!(r(&adj, "160"), Some((2400000, 0)));
    assert_eq!(r(&adj, "161"), Some((0, 220000)));
    assert_eq!(r(&adj, "502"), Some((150000, 0)), "contra revenue shows on the debit side");
}

#[test]
fn closing_zeroes_temporary_accounts_and_blocks_the_period() {
    let db = books();
    cycle(&db);
    let l = Ledger::new(&db).unwrap();
    let closing = l.close_period(CO, "FY2024", ClosingAccounts::default()).unwrap();
    assert_eq!(closing.len(), 4, "revenue, expenses, income summary, dividends");
    let tb = db.read(|v| trial_balance(v, CO, TbKind::PostClosing, "2024-01-01", "2024-12-31")).unwrap();
    assert!(tb.balanced());
    assert!(tb.rows.iter().all(|r| !matches!(r.number.as_str(), "501" | "502" | "520" | "601" | "610" | "620" | "630" | "690" | "700" | "420" | "430")), "temporary accounts are zero");
    assert_eq!(tb.rows.iter().find(|r| r.number == "415").map(|r| r.credit), Some(470000));
    // reports for the year still show the year, because closing entries are excluded
    assert_eq!(db.read(|v| net_income(v, CO, "2024-01-01", "2024-12-31")).unwrap(), 870000);
    // the balance sheet is unchanged by closing
    let bs = db.read(|v| balance_sheet(v, CO, "2024-12-31")).unwrap();
    assert_eq!(amount(&bs, "Total assets"), 159900.0);
    assert_eq!(bs.last().unwrap().amount, 0);
    // closed: no more entries, no second close
    let e = Entry::new("2024-12-31", "late").dr("101", 1.0).cr("501", 1.0);
    assert!(l.post(CO, &e).unwrap_err().to_string().contains("closed period"));
    assert!(l.close_period(CO, "FY2024", ClosingAccounts::default()).is_err());
    let audit = db.read(|v| audit(v, CO)).unwrap();
    assert!(audit.iter().all(|c| c.status == "pass"), "{audit:?}");
}

#[test]
fn quarter_closed_then_year_closed() {
    let db = books();
    cycle(&db);
    let l = Ledger::new(&db).unwrap();
    // Q1 closes: net income so far = (50,000 - 1,500 - 20,000 - 3,000 - 8,000) = 17,500
    let q1 = l.close_period(CO, "Q1 2024", ClosingAccounts::default()).unwrap();
    assert_eq!(q1.len(), 3, "no dividends yet");
    let after_q1 = db.read(|v| trial_balance(v, CO, TbKind::PostClosing, "2024-01-01", "2024-03-31")).unwrap();
    assert_eq!(after_q1.rows.iter().find(|r| r.number == "415").map(|r| r.credit), Some(1750000));
    // a Q1-dated entry is now refused even though FY2024 (which also covers it) is open
    assert!(l.post(CO, &Entry::new("2024-02-02", "x").dr("101", 1.0).cr("501", 1.0)).is_err());
    // closing the year sweeps the rest; the closed Q1 inside it does not block the year's own closing entries
    l.close_period(CO, "FY2024", ClosingAccounts::default()).unwrap();
    let tb = db.read(|v| trial_balance(v, CO, TbKind::PostClosing, "2024-01-01", "2024-12-31")).unwrap();
    assert_eq!(tb.rows.iter().find(|r| r.number == "415").map(|r| r.credit), Some(470000));
    assert!(db.read(|v| audit(v, CO)).unwrap().iter().all(|c| c.status == "pass"));
}

#[test]
fn rejections() {
    let db = books();
    let l = Ledger::new(&db).unwrap();
    let err = |e: Entry| l.post(CO, &e).unwrap_err().to_string();
    assert!(err(Entry::new("2024-01-02", "x").dr("101", 100.0).cr("401", 99.99)).contains("not balanced"), "no tolerance, not even a cent");
    assert!(err(Entry::new("2024-01-02", "x").dr("101", 100.0)).contains("at least two lines"));
    assert!(err(Entry::new("2024-01-02", "x").line(Line { account: "101".into(), debit: 5.0, credit: 5.0, memo: None }).cr("401", 0.0)).contains("exactly one"));
    assert!(err(Entry::new("2024-01-02", "x").dr("101", 0.0).cr("401", 0.0)).contains("exactly one"));
    assert!(err(Entry::new("2024-01-02", "x").dr("101", -5.0).cr("401", -5.0)).contains("exactly one"));
    assert!(err(Entry::new("2024-01-02", "x").dr("999", 5.0).cr("401", 5.0)).contains("unknown account"));
    assert!(err(Entry::new("2024-02-30", "x").dr("101", 5.0).cr("401", 5.0)).contains("not a calendar date"));
    assert!(err(Entry::new("2024-01-02", "x").dr("101", 1.005).cr("401", 1.005)).contains("two decimal"));
    assert!(err(Entry::new("2024-01-02", "x").dr("101", f64::NAN).cr("401", 1.0)).contains("finite"));
    assert!(err(Entry::new("2025-01-02", "x").dr("101", 5.0).cr("401", 5.0)).contains("no accounting period"));
    assert!(err(Entry::new("2024-01-02", "").dr("101", 5.0).cr("401", 5.0)).contains("description"));
    assert!(err(Entry::new("2024-01-02", "x").journal("Nope").dr("101", 5.0).cr("401", 5.0)).contains("journal"));
    assert!(err(Entry::new("2024-01-02", "x").period("Q2 2024").dr("101", 5.0).cr("401", 5.0)).contains("does not cover"));
    assert!(err(Entry::new("2024-01-02", "x").kind(EntryKind::Closing).dr("101", 5.0).cr("401", 5.0)).contains("close_period"));
    l.set_account_active(CO, "401", false).unwrap();
    assert!(err(Entry::new("2024-01-02", "x").dr("101", 5.0).cr("401", 5.0)).contains("inactive"));
    // nothing leaked: the counter did not advance and no entry exists
    l.set_account_active(CO, "401", true).unwrap();
    let ok = l.post(CO, &Entry::new("2024-01-02", "first real entry").dr("101", 5.0).cr("401", 5.0)).unwrap();
    assert_eq!(ok.entry_no, 1, "failed posts consume no entry numbers");
    assert_eq!(ok.period, "Q1 2024", "the narrowest covering period is chosen");
    assert!(l.create_company(CO, "Corporation", "USD", 1).is_err());
    assert!(l.add_account(CO, "101", "dup", "Current Asset").is_err());
    assert!(l.add_account(CO, "999", "x", "Nonsense").is_err());
    assert!(l.add_period(CO, "bad", "Annual", "2024-12-31", "2024-01-01").is_err());
}

#[test]
fn reversal() {
    let db = books();
    let l = Ledger::new(&db).unwrap();
    let a = l.post(CO, &Entry::new("2024-01-31", "Accrue wages").dr("610", 500.0).cr("210", 500.0)).unwrap();
    let r = l.reverse(CO, a.entry_no, "2024-02-01", None).unwrap();
    assert_eq!(r.entry_no, 2);
    let tb = db.read(|v| trial_balance(v, CO, TbKind::PostClosing, "2024-01-01", "2024-12-31")).unwrap();
    assert!(tb.rows.is_empty(), "debit and credit cancel exactly");
    assert!(l.reverse(CO, 1, "2024-02-01", None).unwrap_err().to_string().contains("already reversed"));
    assert!(l.reverse(CO, 2, "2024-02-02", None).unwrap_err().to_string().contains("cannot be reversed"));
    assert!(l.reverse(CO, 77, "2024-02-02", None).is_err());
    let q = db.query(&format!("MATCH (e:JournalEntry {{no: 2}}) RETURN e.type, e.reverses")).unwrap();
    assert_eq!(q.rows[0], vec![Value::Str("Reversing".into()), Value::Int(1)]);
}

#[test]
fn nql_procedures_and_time_travel() {
    let db = books();
    let ts_q1 = cycle(&db);
    let q = |text: &str| db.query(text).unwrap().rows;
    let pick = |rows: Vec<Vec<Value>>, name: &str| rows.into_iter().find(|r| r[0] == Value::Str(name.into())).map(|r| r[1].clone());
    let ni = q(&format!("CALL gaap.income_statement('{CO}', '2024-01-01', '2024-12-31') YIELD name, amount RETURN name, amount"));
    assert_eq!(pick(ni, "Net income"), Some(Value::Float(8700.0)));
    // the books as they stood after the first quarter's entries
    let then = q(&format!("AS OF {ts_q1} CALL gaap.balance_sheet('{CO}', '2024-12-31') YIELD name, amount RETURN name, amount"));
    // cash 100,000-24,000+40,000-3,000-8,000-10,000 = 95,000; AR 8,500; inventory 10,000; equipment 24,000
    assert_eq!(pick(then, "Total assets"), Some(Value::Float(137500.0)));
    let tb = q(&format!("CALL gaap.trial_balance('{CO}', 'adjusted', '2024-01-01', '2024-12-31') YIELD account, debit, credit RETURN account, debit, credit"));
    let total = tb.iter().find(|r| r[0] == Value::Str("TOTAL".into())).unwrap();
    assert_eq!(total[1], total[2]);
    let led = q(&format!("CALL gaap.ledger('{CO}', '101', '2024-01-01', '2024-03-31') YIELD entry, balance RETURN entry, balance"));
    assert_eq!(led.first().unwrap(), &vec![Value::Int(0), Value::Float(0.0)]);
    assert_eq!(led.last().unwrap(), &vec![Value::Int(9), Value::Float(95000.0)]);
    assert!(db.query(&format!("CALL gaap.trial_balance('{CO}', 'weird', '2024-01-01', '2024-12-31')")).is_err());
    assert!(db.query(&format!("CALL gaap.balance_sheet('Nobody', '2024-12-31')")).is_err());
    assert!(db.query(&format!("CALL gaap.balance_sheet('{CO}', 'yesterday')")).is_err());
    // the entry is also a graph event
    let h = q("MATCH ()-[p:POSTING {entry_no: 2}]->() RETURN p.company");
    assert_eq!(h.len(), 1);
}

#[test]
fn survives_restart() {
    let dir = std::env::temp_dir().join(format!("nx_gaap_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Database::open(&dir).unwrap();
        let l = Ledger::new(&db).unwrap();
        l.create_company(CO, "Corporation", "USD", 1).unwrap();
        l.seed_reference_chart(CO).unwrap();
        l.add_period(CO, "FY2024", "Annual", "2024-01-01", "2024-12-31").unwrap();
        cycle(&db);
        l.close_period(CO, "FY2024", ClosingAccounts::default()).unwrap();
    }
    let db = Database::open(&dir).unwrap();
    let l = Ledger::new(&db).unwrap();
    assert_eq!(db.read(|v| balance_sheet(v, CO, "2024-12-31")).unwrap().iter().find(|r| r.name == "Total assets").unwrap().amount, 15990000);
    assert!(db.read(|v| audit(v, CO)).unwrap().iter().all(|c| c.status == "pass"));
    assert!(l.post(CO, &Entry::new("2024-12-31", "late").dr("101", 1.0).cr("501", 1.0)).is_err(), "closed state persisted");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn audit_detects_tampering() {
    let db = books();
    cycle(&db);
    // bypass the ledger API and write an unbalanced entry straight into the graph
    let mut tx = db.begin();
    let acct = db.read(|v| v.lookup("Account", "number", &Value::Str("101".into())).first().copied().unwrap());
    let en = tx
        .create_node(&["JournalEntry"], nexgraph::props([("company", Value::Str(CO.into())), ("key", Value::Str("x".into())), ("no", Value::Int(25)), ("date", Value::Str("2024-05-05".into())), ("type", Value::Str("Regular".into())), ("description", Value::Str("forged".into()))]))
        .unwrap();
    tx.create_edge("LINE", en, acct, 1.0, nexgraph::props([("line_no", Value::Int(1)), ("debit", Value::Int(100)), ("credit", Value::Int(0))])).unwrap();
    tx.commit().unwrap();
    let a = db.read(|v| audit(v, CO)).unwrap();
    let status = |n: &str| a.iter().find(|c| c.check == n).unwrap().status;
    assert_eq!(status("entries_balanced"), "fail");
    assert_eq!(status("entries_have_two_lines"), "fail");
    assert_eq!(status("posting_hyperedges_match_lines"), "fail");
    assert_eq!(status("trial_balance_balances"), "fail");
    assert_eq!(status("balance_sheet_equation"), "fail");
    assert_eq!(status("entry_numbers_gapless"), "fail");
}

#[test]
fn depreciation_helper() {
    assert_eq!(straight_line_monthly(24000.0, 2000.0, 10.0).unwrap(), 183.33); // 22,000 / 10 / 12 = 183.333...
    assert!(straight_line_monthly(100.0, 200.0, 5.0).is_err());
    assert!(straight_line_monthly(100.0, 0.0, 0.0).is_err());
    assert_eq!(to_cents(19.99).unwrap(), 1999);
    assert_eq!(to_cents(0.07).unwrap(), 7);
    assert_eq!(to_cents(1234567.89).unwrap(), 123456789);
}

/// 3,000 random balanced entries over every account type: the identities must hold whatever the data.
#[test]
fn randomized_books_keep_every_identity() {
    let db = Database::in_memory();
    let l = Ledger::new(&db).unwrap();
    l.create_company(CO, "Corporation", "USD", 1).unwrap();
    l.seed_reference_chart(CO).unwrap();
    l.add_period(CO, "FY2024", "Annual", "2024-01-01", "2024-12-31").unwrap();
    let nums: Vec<&str> = REFERENCE_CHART.iter().map(|c| c.0).collect();
    let mut s = 0x9E3779B97F4A7C15u64;
    let mut rnd = |m: u64| {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s % m
    };
    let mut posted = 0;
    for i in 0..3000 {
        let (mm, dd) = (rnd(12) + 1, rnd(28) + 1);
        let date = format!("2024-{mm:02}-{dd:02}");
        let mut e = Entry::new(&date, &format!("random {i}"));
        let n_dr = rnd(3) + 1;
        let n_cr = rnd(3) + 1;
        let total = (rnd(900_000) + 100) as i64; // cents
        let split = |parts: u64, rnd: &mut dyn FnMut(u64) -> u64| -> Vec<i64> {
            let mut left = total;
            let mut v = vec![];
            for k in 0..parts {
                let x = if k + 1 == parts { left } else { (rnd(left.max(2) as u64 - 1) as i64 + 1).min(left - (parts - k - 1) as i64).max(1) };
                v.push(x);
                left -= x;
            }
            v
        };
        let drs = split(n_dr, &mut rnd);
        let crs = split(n_cr, &mut rnd);
        if drs.iter().chain(crs.iter()).any(|x| *x <= 0) {
            continue;
        }
        for x in drs {
            e = e.dr(nums[rnd(nums.len() as u64) as usize], dollars(x));
        }
        for x in crs {
            e = e.cr(nums[rnd(nums.len() as u64) as usize], dollars(x));
        }
        l.post(CO, &e).unwrap();
        posted += 1;
    }
    assert!(posted > 2900);
    let tb = db.read(|v| trial_balance(v, CO, TbKind::Adjusted, "2024-01-01", "2024-12-31")).unwrap();
    assert!(tb.balanced(), "{} vs {}", tb.debits, tb.credits);
    let bs = db.read(|v| balance_sheet(v, CO, "2024-12-31")).unwrap();
    assert_eq!(bs.last().unwrap().amount, 0);
    let cf = db.read(|v| cash_flow(v, CO, "2024-01-01", "2024-12-31")).unwrap();
    assert_eq!(cf.last().unwrap().amount, 0, "cash flow reconciles on arbitrary data");
    let re = db.read(|v| retained_earnings_statement(v, CO, "2024-01-01", "2024-12-31")).unwrap();
    let ni = db.read(|v| net_income(v, CO, "2024-01-01", "2024-12-31")).unwrap();
    assert_eq!(re.iter().find(|r| r.name == "Add: net income").unwrap().amount, ni);
    l.close_period(CO, "FY2024", ClosingAccounts::default()).unwrap();
    let post = db.read(|v| trial_balance(v, CO, TbKind::PostClosing, "2024-01-01", "2024-12-31")).unwrap();
    assert!(post.balanced());
    assert!(post.rows.iter().all(|r| !matches!(r.category.as_str(), "Sales Revenue" | "Service Revenue" | "Other Income" | "Cost of Goods Sold" | "Operating Expense" | "Other Expense" | "Income Tax Expense" | "Dividends" | "Temporary—Closing")));
    let a = db.read(|v| audit(v, CO)).unwrap();
    assert!(a.iter().all(|c| c.status != "fail"), "{a:?}");
    // year-end retained earnings equal the retained-earnings statement's ending balance
    let ending = re.last().unwrap().amount;
    let direct = post.rows.iter().find(|r| r.number == "415").map_or(0, |r| r.credit - r.debit);
    assert_eq!(ending, direct);
}

/// The reference SQL's `sp_generate_trial_balance` clamps an abnormal balance to zero (`GREATEST(..., 0)`), so a
/// write-off larger than the receivable unbalances it (verified on PostgreSQL 16: debits 217,700 vs credits 207,700).
/// Here the balance is shown on its own side and the trial balance still balances.
#[test]
fn abnormal_balance_does_not_unbalance_the_trial_balance() {
    let db = books();
    cycle(&db);
    let l = Ledger::new(&db).unwrap();
    l.post(CO, &Entry::new("2024-05-06", "write-off larger than the receivable").dr("650", 20000.0).cr("110", 20000.0)).unwrap();
    let tb = db.read(|v| trial_balance(v, CO, TbKind::Adjusted, "2024-01-01", "2024-12-31")).unwrap();
    assert!(tb.balanced());
    let ar = tb.rows.iter().find(|r| r.number == "110").unwrap();
    assert_eq!((ar.debit, ar.credit), (0, 1150000), "AR is 11,500 on the credit side");
    let warn = db.read(|v| audit(v, CO)).unwrap();
    let w = warn.iter().find(|c| c.check == "balances_on_normal_side").unwrap();
    assert_eq!(w.status, "warn");
    assert!(w.detail.contains("110"), "{}", w.detail);
}
