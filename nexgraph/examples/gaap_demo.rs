//! GAAP ledger walk-through: `cargo run --release --example gaap_demo [csv|reports]`.
//! `csv` prints the posted lines (read back from the graph with NQL) so another system can load the same scenario.
use nexgraph::gaap::*;
use nexgraph::{Database, Value};

const CO: &str = "Acme Demonstration Corp";

fn main() -> nexgraph::Result<()> {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "reports".into());
    let db = Database::in_memory();
    let l = Ledger::new(&db)?;
    l.create_company(CO, "Corporation", "USD", 1)?;
    l.seed_reference_chart(CO)?;
    l.add_period(CO, "FY2024", "Annual", "2024-01-01", "2024-12-31")?;
    let e = |d: &str, t: &str, dr: &str, cr: &str, a: f64| Entry::new(d, t).dr(dr, a).cr(cr, a);
    for x in [
        e("2024-01-02", "Issue common stock", "101", "401", 100000.0),
        e("2024-01-05", "Buy equipment", "160", "101", 24000.0),
        e("2024-01-10", "Buy inventory on account", "120", "201", 30000.0),
        e("2024-02-01", "Credit sale", "110", "501", 50000.0),
        e("2024-02-01", "Cost of the sale", "601", "120", 20000.0),
        e("2024-02-15", "Collect receivable", "101", "110", 40000.0),
        e("2024-03-01", "Rent", "620", "101", 3000.0),
        e("2024-03-10", "Wages", "610", "101", 8000.0),
        e("2024-03-15", "Pay supplier", "201", "101", 10000.0),
        e("2024-03-31", "Customer return", "502", "110", 1500.0),
        e("2024-04-01", "Cash received in advance", "101", "230", 5000.0),
        e("2024-06-30", "Interest earned", "101", "520", 300.0),
        e("2024-06-30", "Interest paid", "700", "101", 700.0),
        e("2024-07-01", "Borrow on long-term note", "101", "301", 20000.0),
        e("2024-08-01", "Buy land", "150", "101", 10000.0),
        e("2024-12-15", "Declare dividend", "420", "250", 4000.0),
        Entry::adjusting("2024-12-31", "Depreciation, equipment", "630", "161", 2200.0),
        Entry::adjusting("2024-12-31", "Accrue wages", "610", "210", 1200.0),
        Entry::adjusting("2024-12-31", "Income tax", "690", "220", 5000.0),
    ] {
        l.post(CO, &x)?;
    }
    if mode == "csv" {
        let r = db.query("MATCH (e:JournalEntry)-[l:LINE]->(a:Account) RETURN e.no, e.date, e.type, a.number, l.debit, l.credit")?;
        let mut rows = r.rows;
        rows.sort_by_key(|r| r[0].as_i64());
        for r in rows {
            let s = |v: &Value| match v { Value::Str(s) => s.clone(), Value::Int(i) => i.to_string(), o => format!("{o:?}") };
            println!("{}", r.iter().map(s).collect::<Vec<_>>().join(","));
        }
        return Ok(());
    }
    for (title, q) in [
        ("TRIAL BALANCE (adjusted)", format!("CALL gaap.trial_balance('{CO}', 'adjusted', '2024-01-01', '2024-12-31') YIELD account, name, debit, credit RETURN account, name, debit, credit")),
        ("INCOME STATEMENT", format!("CALL gaap.income_statement('{CO}', '2024-01-01', '2024-12-31') YIELD section, account, name, amount, kind RETURN account, name, amount, kind")),
        ("BALANCE SHEET", format!("CALL gaap.balance_sheet('{CO}', '2024-12-31') YIELD section, account, name, amount, kind RETURN account, name, amount, kind")),
        ("CASH FLOW", format!("CALL gaap.cash_flow('{CO}', '2024-01-01', '2024-12-31') YIELD section, account, name, amount, kind RETURN account, name, amount, kind")),
        ("AUDIT", format!("CALL gaap.audit('{CO}') YIELD check, status, count, detail RETURN check, status, detail")),
    ] {
        println!("\n== {title}");
        for r in db.query(&q)?.rows {
            println!("  {}", r.iter().map(|v| match v { Value::Str(s) => s.clone(), Value::Float(f) => format!("{f:.2}"), o => format!("{o:?}") }).collect::<Vec<_>>().join(" | "));
        }
    }
    Ok(())
}
