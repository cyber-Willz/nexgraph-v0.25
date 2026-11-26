//! (v0.18: posts all entries with `Ledger::post_many` and adds ratio and aging reports.)
//! Load a real transaction-level CSV (curran/data "superstoreSales", fetched live from GitHub) into the GAAP ledger
//! and print the statements as Markdown.  `cargo run --release --example gaap_live -- superstore.csv`
//!
//! Mapping, per order row (all amounts in integer cents):
//!   cost  = Sales - Profit            (the file's Profit is treated as net of every order cost)
//!   ship  = min(Shipping Cost, cost)  (the file's own shipping figure, capped so COGS is never negative)
//!   cogs  = cost - ship
//!   order date : Dr 110 A/R sales | Dr 601 COGS | Dr 635 Shipping   /   Cr 501 Sales | Cr 201 A/P cost
//!   ship date  : Dr 101 Cash / Cr 110 A/R (collect)     and     Dr 201 A/P / Cr 101 Cash (pay suppliers)
use nexgraph::gaap::*;
use nexgraph::{Database, Value};
use std::collections::BTreeMap;
use std::time::Instant;

const CO: &str = "Superstore Live Co";

fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let (mut rows, mut row, mut f, mut q) = (Vec::new(), Vec::new(), String::new(), false);
    let mut it = text.chars().peekable();
    while let Some(c) = it.next() {
        match (c, q) {
            ('"', true) => if it.peek() == Some(&'"') { f.push('"'); it.next(); } else { q = false },
            ('"', false) => q = true,
            (',', false) => row.push(std::mem::take(&mut f)),
            ('\r', false) if it.peek() == Some(&'\n') => {}
            ('\r' | '\n', false) => { row.push(std::mem::take(&mut f)); rows.push(std::mem::take(&mut row)); }
            (c, _) => f.push(c),
        }
    }
    if !f.is_empty() || !row.is_empty() { row.push(f); rows.push(row); }
    rows
}
fn cents(s: &str) -> i64 { (s.trim().parse::<f64>().expect("number") * 100.0).round() as i64 }
fn iso(s: &str) -> String {
    let p: Vec<&str> = s.trim().split('/').collect();
    format!("{:04}-{:02}-{:02}", p[2].parse::<i32>().unwrap(), p[0].parse::<u32>().unwrap(), p[1].parse::<u32>().unwrap())
}
fn d(c: i64) -> f64 { c as f64 / 100.0 }
fn cell(v: &Value, prec: usize) -> String {
    match v { Value::Str(s) => s.clone(), Value::Float(f) => format!("{f:.prec$}"), Value::Int(i) => i.to_string(), Value::Null => "n/a".into(), o => format!("{o:?}") }
}
fn table(db: &Database, title: &str, q: &str, head: &[&str]) { table_p(db, title, q, head, 2) }
fn table_p(db: &Database, title: &str, q: &str, head: &[&str], prec: usize) {
    println!("\n#### {title}\n");
    println!("| {} |", head.join(" | "));
    println!("|{}|", head.iter().map(|_| "---").collect::<Vec<_>>().join("|"));
    for r in db.query(q).expect("query").rows {
        println!("| {} |", r.iter().map(|v| cell(v, prec)).collect::<Vec<_>>().join(" | "));
    }
}

fn main() -> nexgraph::Result<()> {
    let path = std::env::args().nth(1).expect("csv path");
    let bytes = std::fs::read(&path).unwrap();
    let text: String = bytes.iter().map(|&b| b as char).collect(); // file is Latin-1
    let rows = parse_csv(&text);
    let h: Vec<&str> = rows[0].iter().map(|s| s.as_str()).collect();
    let ix = |n: &str| h.iter().position(|x| *x == n).unwrap_or_else(|| panic!("column {n}"));
    let (c_oid, c_od, c_s, c_p, c_sh, c_sd) = (ix("Order ID"), ix("Order Date"), ix("Sales"), ix("Profit"), ix("Shipping Cost"), ix("Ship Date"));

    struct Ord { oid: String, od: String, sd: String, sales: i64, cogs: i64, ship: i64 }
    let (mut orders, mut capped, mut neg_cost) = (Vec::new(), 0usize, 0usize);
    let (mut tot_sales, mut tot_profit, mut tot_ship_file) = (0i64, 0i64, 0i64);
    for r in rows.iter().skip(1).filter(|r| r.len() == h.len()) {
        let (sales, profit, shf) = (cents(&r[c_s]), cents(&r[c_p]), cents(&r[c_sh]));
        let cost = sales - profit;
        if cost < 0 { neg_cost += 1; }
        let ship = shf.min(cost.max(0));
        if ship < shf { capped += 1; }
        tot_sales += sales; tot_profit += profit; tot_ship_file += shf;
        orders.push(Ord { oid: r[c_oid].clone(), od: iso(&r[c_od]), sd: iso(&r[c_sd]), sales, cogs: cost - ship, ship });
    }

    let db = Database::in_memory();
    let l = Ledger::new(&db)?;
    l.create_company(CO, "Corporation", "USD", 1)?;
    l.seed_reference_chart(CO)?;
    l.add_account(CO, "635", "Shipping and Delivery Expense", "Operating Expense")?;
    let years: Vec<i32> = { let mut y: Vec<i32> = orders.iter().flat_map(|o| [&o.od, &o.sd]).map(|s| s[..4].parse().unwrap()).collect(); y.sort(); y.dedup(); y };
    for y in &years { l.add_period(CO, &format!("FY{y}"), "Annual", &format!("{y}-01-01"), &format!("{y}-12-31"))?; }

    // chronological event list
    let mut ev: Vec<(String, u8, Entry)> = Vec::new();
    let first = format!("{}-01-01", years[0]);
    ev.push((first.clone(), 0, Entry::new(&first, "Initial capital contribution").journal("General").dr("101", 50_000.0).cr("401", 50_000.0)));
    for o in &orders {
        let cost = o.cogs + o.ship;
        let mut e = Entry::new(&o.od, &format!("Order {}", o.oid)).journal("Sales").reference(&o.oid).dr("110", d(o.sales)).cr("501", d(o.sales));
        if o.cogs != 0 { e = e.dr("601", d(o.cogs)); }
        if o.ship != 0 { e = e.dr("635", d(o.ship)); }
        if cost != 0 { e = e.cr("201", d(cost)); }
        ev.push((o.od.clone(), 1, e));
        if o.sales != 0 { ev.push((o.sd.clone(), 2, Entry::new(&o.sd, &format!("Collect order {}", o.oid)).journal("Cash Receipts").reference(&o.oid).dr("101", d(o.sales)).cr("110", d(o.sales)))); }
        if cost != 0 { ev.push((o.sd.clone(), 3, Entry::new(&o.sd, &format!("Pay suppliers order {}", o.oid)).journal("Cash Disbursements").reference(&o.oid).dr("201", d(cost)).cr("101", d(cost)))); }
    }
    ev.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
    let n_ev = ev.len();
    let t0 = Instant::now();
    let entries: Vec<Entry> = ev.into_iter().map(|x| x.2).collect();
    l.post_many(CO, &entries)?; // one atomic transaction (v0.18)
    let post_secs = t0.elapsed().as_secs_f64();

    println!("<!--META orders={} entries={} post_secs={:.2} tot_sales={:.2} tot_profit={:.2} tot_ship_file={:.2} capped={} neg_cost={} years={:?}-->",
        orders.len(), n_ev, post_secs, d(tot_sales), d(tot_profit), d(tot_ship_file), capped, neg_cost, years);

    // statements per fiscal year, before closing
    let mut ni: BTreeMap<i32, f64> = BTreeMap::new();
    for y in &years {
        let (a, b) = (format!("{y}-01-01"), format!("{y}-12-31"));
        println!("\n### FY{y}");
        table(&db, &format!("Income statement FY{y}"), &format!("CALL gaap.income_statement('{CO}', '{a}', '{b}') YIELD section, account, name, amount, kind RETURN account, name, amount, kind"), &["Account", "Name", "Amount", "Kind"]);
        table(&db, &format!("Balance sheet {b}"), &format!("CALL gaap.balance_sheet('{CO}', '{b}') YIELD section, account, name, amount, kind RETURN account, name, amount, kind"), &["Account", "Name", "Amount", "Kind"]);
        table(&db, &format!("Cash flow FY{y}"), &format!("CALL gaap.cash_flow('{CO}', '{a}', '{b}') YIELD section, account, name, amount, kind RETURN account, name, amount, kind"), &["Account", "Name", "Amount", "Kind"]);
        table(&db, &format!("Retained earnings FY{y}"), &format!("CALL gaap.retained_earnings('{CO}', '{a}', '{b}') YIELD section, account, name, amount, kind RETURN name, amount, kind"), &["Item", "Amount", "Kind"]);
        table_p(&db, &format!("Ratios FY{y}"), &format!("CALL gaap.ratios('{CO}', '{a}', '{b}') YIELD group, ratio, value, basis RETURN group, ratio, value"), &["Group", "Ratio", "Value"], 4);
        table(&db, &format!("Receivables aging at {b} (by order id)"), &format!("CALL gaap.aging('{CO}', '110', '{b}') YIELD bucket, items, amount RETURN bucket, items, amount"), &["Bucket", "Items", "Amount"]);
        table(&db, &format!("Payables aging at {b} (by order id)"), &format!("CALL gaap.aging('{CO}', '201', '{b}') YIELD bucket, items, amount RETURN bucket, items, amount"), &["Bucket", "Items", "Amount"]);
        let n = db.read(|v| net_income(v, CO, &a, &b))?;
        ni.insert(*y, d(n));
    }
    println!("\n<!--NI {:?}-->", ni);

    table(&db, "Adjusted trial balance, full period (pre-closing)", &format!("CALL gaap.trial_balance('{CO}', 'adjusted', '{}-01-01', '{}-12-31') YIELD account, name, debit, credit RETURN account, name, debit, credit", years[0], years[years.len() - 1]), &["Account", "Name", "Debit", "Credit"]);
    table(&db, "Audit before closing", &format!("CALL gaap.audit('{CO}') YIELD check, status, count, detail RETURN check, status, detail"), &["Check", "Status", "Detail"]);

    for y in &years { l.close_period(CO, &format!("FY{y}"), ClosingAccounts::default())?; }
    table(&db, "Post-closing trial balance", &format!("CALL gaap.trial_balance('{CO}', 'post-closing', '{}-01-01', '{}-12-31') YIELD account, name, debit, credit RETURN account, name, debit, credit", years[0], years[years.len() - 1]), &["Account", "Name", "Debit", "Credit"]);
    table(&db, "Audit after closing all years", &format!("CALL gaap.audit('{CO}') YIELD check, status, count, detail RETURN check, status, detail"), &["Check", "Status", "Detail"]);
    Ok(())
}
