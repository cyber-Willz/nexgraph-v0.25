//! `gaap`: a double-entry general ledger and the US-GAAP financial statements, stored in the graph.
//!
//! The chart of accounts, categories and statement layout follow the reference `GAAP_ACCOUNTING_SYSTEM`
//! PostgreSQL schema (Weygandt, *Accounting Principles*): 20 account categories, `Debit`/`Credit` normal
//! balances, contra categories, and the Balance Sheet / Income Statement / Retained Earnings classification.
//!
//! # Model
//!
//! | entity | label | notes |
//! |---|---|---|
//! | reporting entity | `:Company` | economic-entity assumption: every other record names its company; owns the gapless entry counter |
//! | account | `:Account` | `number`, `name`, `category`, derived `type` / `normal` / `statement`, `cash_flow`, `active` |
//! | period | `:Period` | `Monthly` / `Quarterly` / `Annual`, inclusive ISO dates, `closed` |
//! | journal entry | `:JournalEntry` | gapless `no` per company, `date`, `journal`, `type`, `description`, `period` |
//! | journal line | `(:JournalEntry)-[:LINE {debit, credit}]->(:Account)` | amounts are integer **cents**; exactly one side is non-zero |
//! | entry as one event | `(debit accounts)-[:POSTING]->(credit accounts)` | a directed hyperedge per entry, weight = total in dollars, so graph and spectral analytics see the n-ary posting |
//!
//! # Rules enforced when an entry is posted ([`Ledger::post`])
//!
//! * at least two lines, each with exactly one positive side; **debits equal credits to the cent** (no tolerance);
//! * every account exists, belongs to the company and is active; the date is a real calendar date;
//! * the date falls in at least one period of the company and **in no closed period**;
//! * entries are immutable once posted. Errors are corrected with [`Ledger::reverse`], never by editing;
//! * `Closing` and `Reversing` entries are only produced by [`Ledger::close_period`] and [`Ledger::reverse`].
//!
//! Posting is one atomic transaction, so a failed post leaves nothing behind.
//!
//! # Reports
//!
//! All reports are computed from the entries, so they work on any historical snapshot (`AS OF n`, [`Database::read_at`]).
//! They are plain functions over a [`View`] and also `CALL gaap.*` procedures (dates are `YYYY-MM-DD` strings):
//!
//! * `gaap.trial_balance(company, type, start, end)`: `type` is `unadjusted`, `adjusted` or `post-closing`;
//! * `gaap.income_statement(company, from, to)`: multi-step, closing entries excluded;
//! * `gaap.retained_earnings(company, from, to)`: beginning + net income - dividends = ending;
//! * `gaap.balance_sheet(company, asof)`: classified; the equation is checked, and current-period income is
//!   folded into retained earnings so the sheet balances **before** closing;
//! * `gaap.cash_flow(company, from, to)`: indirect method, reconciled to the actual change in cash;
//! * `gaap.ledger(company, account, from, to)`: the T-account with a running balance;
//! * `gaap.audit(company)`: integrity checks (balanced entries, gapless numbering, closed-period violations, ...).
//! * `gaap.ratios(company, from, to)`: profitability, liquidity, solvency and efficiency ratios (v0.18);
//! * `gaap.open_items(company, account, asof)` / `gaap.aging(company, account, asof)`: unsettled items per `reference` (invoice or
//!   bill number) and 0-30 / 31-60 / 61-90 / 91+ day aging of a receivable or payable account, tied to the ledger balance (v0.18).
//!
//! Bulk loading (v0.18): [`Ledger::post_many`] posts a batch atomically; `Ledger::import_journal_csv` (feature `ingest`) loads a journal from CSV.
use crate::db::{Database, Txn};
use crate::error::{Error, Result};
use crate::model::Body;
use crate::netgraph::{bad, need_str, Rows};
use crate::store::{Direction, View};
use crate::value::{props, Id, Props, Value};
use std::collections::{BTreeMap, BTreeSet};

fn inv<T>(m: impl Into<String>) -> Result<T> {
    Err(Error::Invalid(m.into()))
}

// ------------------------------------------------------------------ static reference data

/// `(name, type, normal balance, financial statement)`, in the reference schema's `category_id` order.
pub const CATEGORIES: [(&str, &str, &str, &str); 20] = [
    ("Current Asset", "Asset", "Debit", "Balance Sheet"),
    ("Current Asset—Contra", "Asset", "Credit", "Balance Sheet"),
    ("Plant Asset", "Asset", "Debit", "Balance Sheet"),
    ("Plant Asset—Contra", "Asset", "Credit", "Balance Sheet"),
    ("Intangible Asset", "Asset", "Debit", "Balance Sheet"),
    ("Long-Term Investment", "Asset", "Debit", "Balance Sheet"),
    ("Current Liability", "Liability", "Credit", "Balance Sheet"),
    ("Long-Term Liability", "Liability", "Credit", "Balance Sheet"),
    ("Long-Term Liability—Contra", "Liability", "Debit", "Balance Sheet"),
    ("Stockholders' Equity", "Equity", "Credit", "Balance Sheet"),
    ("Retained Earnings", "Equity", "Credit", "Balance Sheet"),
    ("Dividends", "Equity", "Debit", "Retained Earnings Statement"),
    ("Sales Revenue", "Revenue", "Credit", "Income Statement"),
    ("Service Revenue", "Revenue", "Credit", "Income Statement"),
    ("Other Income", "Revenue", "Credit", "Income Statement"),
    ("Cost of Goods Sold", "COGS", "Debit", "Income Statement"),
    ("Operating Expense", "Expense", "Debit", "Income Statement"),
    ("Other Expense", "Expense", "Debit", "Income Statement"),
    ("Income Tax Expense", "Expense", "Debit", "Income Statement"),
    ("Temporary—Closing", "Equity", "Debit", "Not Applicable"),
];

/// The reference schema's demonstration chart of accounts: `(number, name, category number 1..=20)`.
pub const REFERENCE_CHART: [(&str, &str, usize); 41] = [
    ("101", "Cash and Cash Equivalents", 1),
    ("110", "Accounts Receivable", 1),
    ("111", "Allowance for Doubtful Accounts", 2),
    ("120", "Merchandise Inventory", 1),
    ("130", "Prepaid Insurance", 1),
    ("140", "Office Supplies", 1),
    ("150", "Land", 3),
    ("155", "Buildings", 3),
    ("156", "Accumulated Depreciation—Bldg", 4),
    ("160", "Equipment", 3),
    ("161", "Accumulated Depreciation—Equip", 4),
    ("170", "Patents", 5),
    ("201", "Accounts Payable", 7),
    ("210", "Salaries and Wages Payable", 7),
    ("220", "Income Taxes Payable", 7),
    ("230", "Unearned Revenue", 7),
    ("240", "Notes Payable (Current)", 7),
    ("250", "Dividends Payable", 7),
    ("301", "Long-Term Notes Payable", 8),
    ("310", "Bonds Payable", 8),
    ("401", "Common Stock", 10),
    ("410", "Additional Paid-In Capital", 10),
    ("415", "Retained Earnings", 11),
    ("420", "Dividends Declared", 12),
    ("430", "Income Summary", 20),
    ("501", "Sales Revenue", 13),
    ("502", "Sales Returns and Allowances", 13),
    ("503", "Sales Discounts", 13),
    ("510", "Service Revenue", 14),
    ("520", "Interest Revenue", 15),
    ("601", "Cost of Goods Sold", 16),
    ("610", "Salaries and Wages Expense", 17),
    ("620", "Rent Expense", 17),
    ("630", "Depreciation Expense", 17),
    ("640", "Insurance Expense", 17),
    ("650", "Utilities Expense", 17),
    ("660", "Supplies Expense", 17),
    ("670", "Advertising Expense", 17),
    ("680", "Bad Debt Expense", 17),
    ("690", "Income Tax Expense", 19),
    ("700", "Interest Expense", 18),
];

const JOURNALS: [&str; 8] = ["General", "Cash Receipts", "Cash Disbursements", "Sales", "Purchases", "Payroll", "Adjusting", "Closing"];
const LEGAL_FORMS: [&str; 4] = ["Sole Proprietorship", "Partnership", "Corporation", "LLC"];
const PERIOD_KINDS: [&str; 3] = ["Monthly", "Quarterly", "Annual"];
const CASH_FLOW: [&str; 4] = ["Cash", "Operating", "Investing", "Financing"];
const RE_CAT: &str = "Retained Earnings";
const DIV_CAT: &str = "Dividends";
const TEMP_CAT: &str = "Temporary—Closing";

fn category(name: &str) -> Option<&'static (&'static str, &'static str, &'static str, &'static str)> {
    CATEGORIES.iter().find(|c| c.0 == name)
}

/// Default cash-flow class of an account in `category` (override per account with [`Ledger::set_cash_flow`]).
///
/// Contra plant assets (accumulated depreciation) are an operating add-back; plant and intangibles are investing.
fn default_cash_flow(cat: &str) -> &'static str {
    match cat {
        "Current Asset" | "Current Asset—Contra" | "Current Liability" | "Plant Asset—Contra" | TEMP_CAT => "Operating",
        "Plant Asset" | "Intangible Asset" | "Long-Term Investment" => "Investing",
        _ => "Financing",
    }
}

// ------------------------------------------------------------------ money and dates

/// Dollars to integer cents. Rejects non-finite values and anything with more than two decimals.
pub fn to_cents(x: f64) -> Result<i64> {
    if !x.is_finite() || x.abs() > 9.0e13 {
        return inv("amount must be a finite number below 90 trillion");
    }
    let c = x * 100.0;
    let r = c.round();
    if (c - r).abs() > 1e-6 * c.abs().max(1.0) {
        return inv(format!("amount {x} has more than two decimal places"));
    }
    Ok(r as i64)
}

/// Integer cents to dollars.
pub fn dollars(c: i64) -> f64 {
    c as f64 / 100.0
}

fn parse_date(s: &str) -> Option<(i64, i64, i64)> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> { s.get(r).filter(|t| t.bytes().all(|c| c.is_ascii_digit())).and_then(|t| t.parse().ok()) };
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let dim = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => 28 + leap as i64,
        _ => return None,
    };
    (1..=dim).contains(&d).then_some((y, m, d))
}

/// Validates a `YYYY-MM-DD` calendar date.
pub fn check_date(s: &str) -> Result<()> {
    parse_date(s).map(|_| ()).ok_or_else(|| Error::Invalid(format!("'{s}' is not a calendar date (YYYY-MM-DD)")))
}

fn day_no(s: &str) -> i64 {
    let (y, m, d) = parse_date(s).unwrap_or((1970, 1, 1));
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468
}

/// Monthly straight-line depreciation, rounded to cents: `(cost - salvage) / life_years / 12`.
pub fn straight_line_monthly(cost: f64, salvage: f64, life_years: f64) -> Result<f64> {
    if !(life_years.is_finite() && life_years > 0.0) {
        return inv("useful life must be positive");
    }
    if !(cost.is_finite() && salvage.is_finite() && cost >= salvage && salvage >= 0.0) {
        return inv("need cost >= salvage >= 0");
    }
    Ok(((cost - salvage) / life_years / 12.0 * 100.0).round() / 100.0)
}

fn nkey(n: &str) -> (u64, String) {
    (n.parse::<u64>().unwrap_or(u64::MAX), n.to_string())
}

fn akey(company: &str, number: &str) -> String {
    format!("{company}\u{1f}{number}")
}

fn p(items: Vec<(&str, Value)>) -> Props {
    props(items)
}

// ------------------------------------------------------------------ entry builder

/// Kind of journal entry. `Closing` and `Reversing` are produced only by [`Ledger::close_period`] / [`Ledger::reverse`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Regular,
    Adjusting,
    Closing,
    Reversing,
    Correcting,
}

impl EntryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EntryKind::Regular => "Regular",
            EntryKind::Adjusting => "Adjusting",
            EntryKind::Closing => "Closing",
            EntryKind::Reversing => "Reversing",
            EntryKind::Correcting => "Correcting",
        }
    }
}

/// One debit or credit line, in dollars with at most two decimals.
#[derive(Clone, Debug)]
pub struct Line {
    pub account: String,
    pub debit: f64,
    pub credit: f64,
    pub memo: Option<String>,
}

impl Line {
    pub fn dr(account: &str, amount: f64) -> Line {
        Line { account: account.into(), debit: amount, credit: 0.0, memo: None }
    }
    pub fn cr(account: &str, amount: f64) -> Line {
        Line { account: account.into(), debit: 0.0, credit: amount, memo: None }
    }
    pub fn memo(mut self, m: &str) -> Line {
        self.memo = Some(m.into());
        self
    }
}

/// A journal entry to post.
#[derive(Clone, Debug)]
pub struct Entry {
    pub date: String,
    pub journal: String,
    pub kind: EntryKind,
    pub description: String,
    pub reference: Option<String>,
    /// Period name; when `None` the narrowest period containing `date` is used.
    pub period: Option<String>,
    pub lines: Vec<Line>,
}

impl Entry {
    pub fn new(date: &str, description: &str) -> Entry {
        Entry { date: date.into(), journal: "General".into(), kind: EntryKind::Regular, description: description.into(), reference: None, period: None, lines: Vec::new() }
    }
    /// An adjusting entry in the Adjusting journal: `Dr debit_acct, Cr credit_acct` for `amount`.
    pub fn adjusting(date: &str, description: &str, debit_acct: &str, credit_acct: &str, amount: f64) -> Entry {
        Entry::new(date, description).journal("Adjusting").kind(EntryKind::Adjusting).dr(debit_acct, amount).cr(credit_acct, amount)
    }
    pub fn journal(mut self, j: &str) -> Entry {
        self.journal = j.into();
        self
    }
    pub fn kind(mut self, k: EntryKind) -> Entry {
        self.kind = k;
        self
    }
    pub fn reference(mut self, r: &str) -> Entry {
        self.reference = Some(r.into());
        self
    }
    pub fn period(mut self, name: &str) -> Entry {
        self.period = Some(name.into());
        self
    }
    pub fn line(mut self, l: Line) -> Entry {
        self.lines.push(l);
        self
    }
    pub fn dr(self, account: &str, amount: f64) -> Entry {
        self.line(Line::dr(account, amount))
    }
    pub fn cr(self, account: &str, amount: f64) -> Entry {
        self.line(Line::cr(account, amount))
    }
}

/// Result of posting.
#[derive(Clone, Debug, PartialEq)]
pub struct Posted {
    pub entry_no: i64,
    pub id: Id,
    pub period: String,
    /// Total debits (= total credits), dollars.
    pub total: f64,
}

/// Which accounts [`Ledger::close_period`] closes into. `None` picks the only account of the category
/// (`Temporary—Closing` for the income summary, `Retained Earnings`), and fails if there is not exactly one.
#[derive(Clone, Copy, Debug, Default)]
pub struct ClosingAccounts<'a> {
    pub income_summary: Option<&'a str>,
    pub retained_earnings: Option<&'a str>,
}

// ------------------------------------------------------------------ reading the ledger

fn ps(b: &Body, k: &str) -> String {
    b.props().get(k).and_then(|v| v.as_str()).unwrap_or("").to_string()
}

fn pi(b: &Body, k: &str) -> i64 {
    b.props().get(k).and_then(|v| v.as_i64()).unwrap_or(0)
}

fn pb(b: &Body, k: &str) -> bool {
    matches!(b.props().get(k), Some(Value::Bool(true)))
}

struct Acct {
    id: Id,
    number: String,
    name: String,
    category: String,
    atype: String,
    normal: String,
    statement: String,
    cash_flow: String,
    active: bool,
}

impl Acct {
    fn is_pl(&self) -> bool {
        matches!(self.atype.as_str(), "Revenue" | "Expense" | "COGS")
    }
    /// Accounts whose net balance makes up retained earnings: RE itself, dividends, the income summary and
    /// every revenue/expense account (so it is right before and after closing).
    fn in_re_group(&self) -> bool {
        self.is_pl() || self.category == RE_CAT || self.category == DIV_CAT || self.category == TEMP_CAT
    }
}

fn company_id(view: &View, company: &str) -> Result<Id> {
    view.lookup("Company", "name", &Value::Str(company.into())).first().copied().ok_or_else(|| Error::Invalid(format!("unknown company '{company}'")))
}

fn load_accounts(view: &View, company: &str) -> BTreeMap<Id, Acct> {
    let mut m = BTreeMap::new();
    for id in view.nodes_with_label("Account") {
        if let Some(b) = view.node(id) {
            if ps(b, "company") == company {
                m.insert(id, Acct { id, number: ps(b, "number"), name: ps(b, "name"), category: ps(b, "category"), atype: ps(b, "type"), normal: ps(b, "normal"), statement: ps(b, "statement"), cash_flow: ps(b, "cash_flow"), active: pb(b, "active") });
            }
        }
    }
    m
}

fn sorted_accounts(a: &BTreeMap<Id, Acct>) -> Vec<&Acct> {
    let mut v: Vec<&Acct> = a.values().collect();
    v.sort_by_key(|x| nkey(&x.number));
    v
}

struct L {
    acct: Id,
    dr: i64,
    cr: i64,
    memo: String,
}

struct Ent {
    no: i64,
    date: String,
    kind: String,
    desc: String,
    reference: String,
    /// Number of the entry this one reverses (0 if none).
    reverses: i64,
    lines: Vec<L>,
}

fn load_entries(view: &View, company: &str) -> Vec<Ent> {
    let mut out = Vec::new();
    for id in view.nodes_with_label("JournalEntry") {
        let Some(b) = view.node(id) else { continue };
        if ps(b, "company") != company {
            continue;
        }
        let mut lines: Vec<(i64, L)> = Vec::new();
        for (_, eb) in view.incident_dir(id, Direction::Out) {
            if let Body::Edge { label, head, .. } = eb {
                if label == "LINE" {
                    if let Some(&a) = head.first() {
                        lines.push((pi(eb, "line_no"), L { acct: a, dr: pi(eb, "debit"), cr: pi(eb, "credit"), memo: ps(eb, "memo") }));
                    }
                }
            }
        }
        lines.sort_by_key(|x| x.0);
        out.push(Ent { no: pi(b, "no"), date: ps(b, "date"), kind: ps(b, "type"), desc: ps(b, "description"), reference: ps(b, "reference"), reverses: pi(b, "reverses"), lines: lines.into_iter().map(|x| x.1).collect() });
    }
    out.sort_by_key(|e| e.no);
    out
}

/// Net debit-minus-credit per account over the entries selected by `pred`.
fn net(entries: &[Ent], pred: impl Fn(&Ent) -> bool) -> BTreeMap<Id, i64> {
    let mut m: BTreeMap<Id, i64> = BTreeMap::new();
    for e in entries.iter().filter(|e| pred(e)) {
        for l in &e.lines {
            *m.entry(l.acct).or_insert(0) += l.dr - l.cr;
        }
    }
    m
}

fn check_range(from: &str, to: &str) -> Result<()> {
    check_date(from)?;
    check_date(to)?;
    if from > to {
        return inv("from is after to");
    }
    Ok(())
}

// ------------------------------------------------------------------ reports

/// One line of a statement. `kind` is `line`, `subtotal`, `total` or `check`.
#[derive(Clone, Debug, PartialEq)]
pub struct StmtRow {
    pub section: String,
    pub number: String,
    pub name: String,
    /// Cents.
    pub amount: i64,
    pub kind: &'static str,
}

fn row(section: &str, number: &str, name: &str, amount: i64, kind: &'static str) -> StmtRow {
    StmtRow { section: section.into(), number: number.into(), name: name.into(), amount, kind }
}

/// Trial-balance flavour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TbKind {
    /// Before the period's adjusting entries.
    Unadjusted,
    /// After adjusting entries, before closing.
    Adjusted,
    /// After closing: only permanent accounts carry balances.
    PostClosing,
}

impl TbKind {
    pub fn parse(s: &str) -> Option<TbKind> {
        match s.to_ascii_lowercase().replace([' ', '_'], "-").as_str() {
            "unadjusted" => Some(TbKind::Unadjusted),
            "adjusted" => Some(TbKind::Adjusted),
            "post-closing" | "postclosing" => Some(TbKind::PostClosing),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TbRow {
    pub number: String,
    pub name: String,
    pub category: String,
    pub debit: i64,
    pub credit: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TrialBalance {
    pub rows: Vec<TbRow>,
    pub debits: i64,
    pub credits: i64,
}

impl TrialBalance {
    pub fn balanced(&self) -> bool {
        self.debits == self.credits
    }
}

/// Trial balance of `company` for the period `[start, end]`.
///
/// Balances are cumulative to `end`. Closing entries dated inside the period are left out of `Unadjusted` and
/// `Adjusted` (so the revenue and expense accounts still show the period); `Unadjusted` also leaves out the period's
/// adjusting entries. An account with a balance on its abnormal side shows it on that side, so the totals always
/// agree when the entries balance. Accounts with a zero balance are omitted.
pub fn trial_balance(view: &View, company: &str, kind: TbKind, start: &str, end: &str) -> Result<TrialBalance> {
    check_range(start, end)?;
    company_id(view, company)?;
    let accts = load_accounts(view, company);
    let entries = load_entries(view, company);
    let n = net(&entries, |e| {
        e.date.as_str() <= end && !(kind != TbKind::PostClosing && e.kind == "Closing" && e.date.as_str() >= start) && !(kind == TbKind::Unadjusted && e.kind == "Adjusting" && e.date.as_str() >= start)
    });
    let mut rows = Vec::new();
    let (mut debits, mut credits) = (0i64, 0i64);
    for a in sorted_accounts(&accts) {
        let b = n.get(&a.id).copied().unwrap_or(0);
        if b == 0 {
            continue;
        }
        let (d, c) = if b > 0 { (b, 0) } else { (0, -b) };
        debits += d;
        credits += c;
        rows.push(TbRow { number: a.number.clone(), name: a.name.clone(), category: a.category.clone(), debit: d, credit: c });
    }
    Ok(TrialBalance { rows, debits, credits })
}

/// Multi-step income statement for `[from, to]` (closing entries excluded). The last row is `Net income`.
pub fn income_statement(view: &View, company: &str, from: &str, to: &str) -> Result<Vec<StmtRow>> {
    check_range(from, to)?;
    company_id(view, company)?;
    let accts = load_accounts(view, company);
    let entries = load_entries(view, company);
    let n = net(&entries, |e| e.kind != "Closing" && e.date.as_str() >= from && e.date.as_str() <= to);
    let sorted = sorted_accounts(&accts);
    let mut rows: Vec<StmtRow> = Vec::new();
    let emit = |rows: &mut Vec<StmtRow>, section: &str, cats: &[&str], total_name: &str| -> i64 {
        let mut total = 0i64;
        let mut any = false;
        for a in sorted.iter().filter(|a| a.statement == "Income Statement" && cats.contains(&a.category.as_str())) {
            let b = n.get(&a.id).copied().unwrap_or(0);
            let amt = if a.normal == "Credit" { -b } else { b };
            if amt != 0 {
                rows.push(row(section, &a.number, &a.name, amt, "line"));
                total += amt;
                any = true;
            }
        }
        if any {
            rows.push(row(section, "", total_name, total, "subtotal"));
        }
        total
    };
    let rev = emit(&mut rows, "Revenue", &["Sales Revenue", "Service Revenue"], "Net revenue");
    let cogs = emit(&mut rows, "Cost of goods sold", &["Cost of Goods Sold"], "Total cost of goods sold");
    let gp = rev - cogs;
    rows.push(row("Gross profit", "", "Gross profit", gp, "subtotal"));
    let opex = emit(&mut rows, "Operating expenses", &["Operating Expense"], "Total operating expenses");
    let oi = gp - opex;
    rows.push(row("Operating income", "", "Operating income", oi, "subtotal"));
    let oinc = emit(&mut rows, "Other income", &["Other Income"], "Total other income");
    let oexp = emit(&mut rows, "Other expenses", &["Other Expense"], "Total other expenses");
    let ibt = oi + oinc - oexp;
    rows.push(row("Income before income taxes", "", "Income before income taxes", ibt, "subtotal"));
    let tax = emit(&mut rows, "Income taxes", &["Income Tax Expense"], "Total income tax expense");
    rows.push(row("Net income", "", "Net income", ibt - tax, "total"));
    Ok(rows)
}

/// Net income for `[from, to]` in cents.
pub fn net_income(view: &View, company: &str, from: &str, to: &str) -> Result<i64> {
    Ok(income_statement(view, company, from, to)?.last().map_or(0, |r| r.amount))
}

/// Retained-earnings statement for `[from, to]`: beginning + net income - dividends = ending.
pub fn retained_earnings_statement(view: &View, company: &str, from: &str, to: &str) -> Result<Vec<StmtRow>> {
    check_range(from, to)?;
    company_id(view, company)?;
    let accts = load_accounts(view, company);
    let entries = load_entries(view, company);
    let group: BTreeSet<Id> = accts.values().filter(|a| a.in_re_group()).map(|a| a.id).collect();
    let before = net(&entries, |e| e.date.as_str() < from);
    let begin: i64 = -before.iter().filter(|(k, _)| group.contains(k)).map(|(_, v)| v).sum::<i64>();
    let inrange = net(&entries, |e| e.kind != "Closing" && e.date.as_str() >= from && e.date.as_str() <= to);
    let ni: i64 = -inrange.iter().filter(|(k, _)| accts.get(k).map_or(false, |a| a.is_pl())).map(|(_, v)| v).sum::<i64>();
    let div: i64 = inrange.iter().filter(|(k, _)| accts.get(k).map_or(false, |a| a.category == DIV_CAT)).map(|(_, v)| v).sum();
    // Any direct entry to the retained-earnings account itself (prior-period adjustments).
    let adj: i64 = -inrange.iter().filter(|(k, _)| accts.get(k).map_or(false, |a| a.category == RE_CAT || a.category == TEMP_CAT)).map(|(_, v)| v).sum::<i64>();
    let mut rows = vec![row("Retained earnings", "", "Retained earnings, beginning", begin, "line")];
    rows.push(row("Retained earnings", "", "Add: net income", ni, "line"));
    if adj != 0 {
        rows.push(row("Retained earnings", "", "Direct adjustments to retained earnings", adj, "line"));
    }
    rows.push(row("Retained earnings", "", "Less: dividends", -div, "line"));
    rows.push(row("Retained earnings", "", "Retained earnings, ending", begin + ni + adj - div, "total"));
    Ok(rows)
}

/// Classified balance sheet as of `asof` (all entries dated on or before it). The last row (`check`) is
/// assets minus liabilities and equity and is zero whenever every entry balanced.
pub fn balance_sheet(view: &View, company: &str, asof: &str) -> Result<Vec<StmtRow>> {
    check_date(asof)?;
    company_id(view, company)?;
    let accts = load_accounts(view, company);
    let entries = load_entries(view, company);
    let n = net(&entries, |e| e.date.as_str() <= asof);
    let sorted = sorted_accounts(&accts);
    let mut rows: Vec<StmtRow> = Vec::new();
    let block = |rows: &mut Vec<StmtRow>, section: &str, cats: &[&str], sign: i64, total_name: &str| -> i64 {
        let mut total = 0;
        let mut any = false;
        for a in sorted.iter().filter(|a| a.statement == "Balance Sheet" && cats.contains(&a.category.as_str())) {
            let amt = sign * n.get(&a.id).copied().unwrap_or(0);
            if amt != 0 {
                rows.push(row(section, &a.number, &a.name, amt, "line"));
                total += amt;
                any = true;
            }
        }
        if any {
            rows.push(row(section, "", total_name, total, "subtotal"));
        }
        total
    };
    let ca = block(&mut rows, "Current assets", &["Current Asset", "Current Asset—Contra"], 1, "Total current assets");
    let ppe = block(&mut rows, "Property, plant and equipment", &["Plant Asset", "Plant Asset—Contra"], 1, "Net property, plant and equipment");
    let ia = block(&mut rows, "Intangible assets", &["Intangible Asset"], 1, "Total intangible assets");
    let lti = block(&mut rows, "Long-term investments", &["Long-Term Investment"], 1, "Total long-term investments");
    let assets = ca + ppe + ia + lti;
    rows.push(row("Total assets", "", "Total assets", assets, "total"));
    let cl = block(&mut rows, "Current liabilities", &["Current Liability"], -1, "Total current liabilities");
    let ll = block(&mut rows, "Long-term liabilities", &["Long-Term Liability", "Long-Term Liability—Contra"], -1, "Total long-term liabilities");
    let liab = cl + ll;
    rows.push(row("Total liabilities", "", "Total liabilities", liab, "total"));
    let contributed = block(&mut rows, "Stockholders' equity", &["Stockholders' Equity"], -1, "Total contributed capital");
    let re: i64 = -n.iter().filter(|(k, _)| accts.get(k).map_or(false, |a| a.in_re_group())).map(|(_, v)| v).sum::<i64>();
    rows.push(row("Stockholders' equity", "", "Retained earnings (including income not yet closed, net of dividends)", re, "line"));
    let equity = contributed + re;
    rows.push(row("Total stockholders' equity", "", "Total stockholders' equity", equity, "total"));
    rows.push(row("Total liabilities and equity", "", "Total liabilities and stockholders' equity", liab + equity, "total"));
    rows.push(row("Check", "", "Assets minus (liabilities + equity)", assets - liab - equity, "check"));
    Ok(rows)
}

/// Statement of cash flows, indirect method, for `[from, to]` (closing entries excluded).
///
/// Starts from net income and adds the cash effect of the change in every other account, grouped by the account's
/// `cash_flow` class (`Operating`, `Investing`, `Financing`; `Cash` marks the cash accounts). The result is
/// reconciled to the actual change in the cash accounts and the final `check` row is the difference (zero).
pub fn cash_flow(view: &View, company: &str, from: &str, to: &str) -> Result<Vec<StmtRow>> {
    check_range(from, to)?;
    company_id(view, company)?;
    let accts = load_accounts(view, company);
    let entries = load_entries(view, company);
    let inr = net(&entries, |e| e.kind != "Closing" && e.date.as_str() >= from && e.date.as_str() <= to);
    let cash_ids: BTreeSet<Id> = accts.values().filter(|a| a.cash_flow == "Cash").map(|a| a.id).collect();
    if cash_ids.is_empty() {
        return inv("no account is tagged as cash (cash_flow = 'Cash')");
    }
    let ni: i64 = -inr.iter().filter(|(k, _)| accts.get(k).map_or(false, |a| a.is_pl())).map(|(_, v)| v).sum::<i64>();
    let mut rows: Vec<StmtRow> = vec![row("Operating activities", "", "Net income", ni, "line")];
    let mut totals: BTreeMap<&str, i64> = BTreeMap::new();
    totals.insert("Operating", ni);
    for a in sorted_accounts(&accts) {
        if a.is_pl() || cash_ids.contains(&a.id) {
            continue;
        }
        let d = inr.get(&a.id).copied().unwrap_or(0);
        if d == 0 {
            continue;
        }
        let eff = -d;
        let class = if CASH_FLOW.contains(&a.cash_flow.as_str()) && a.cash_flow != "Cash" { a.cash_flow.as_str() } else { default_cash_flow(&a.category) };
        let section = match class {
            "Operating" => "Operating activities",
            "Investing" => "Investing activities",
            _ => "Financing activities",
        };
        rows.push(row(section, &a.number, &a.name, eff, "line"));
        *totals.entry(match class { "Operating" => "Operating", "Investing" => "Investing", _ => "Financing" }).or_insert(0) += eff;
    }
    // Group rows by section in the conventional order, adding subtotals.
    let mut out: Vec<StmtRow> = Vec::new();
    for (section, key, label) in [("Operating activities", "Operating", "Net cash from operating activities"), ("Investing activities", "Investing", "Net cash from investing activities"), ("Financing activities", "Financing", "Net cash from financing activities")] {
        out.extend(rows.iter().filter(|r| r.section == section).cloned());
        out.push(row(section, "", label, totals.get(key).copied().unwrap_or(0), "subtotal"));
    }
    let change: i64 = totals.values().sum();
    out.push(row("Net change in cash", "", "Net increase (decrease) in cash", change, "total"));
    let before = net(&entries, |e| e.date.as_str() < from);
    let begin: i64 = before.iter().filter(|(k, _)| cash_ids.contains(k)).map(|(_, v)| v).sum();
    out.push(row("Cash", "", "Cash, beginning of period", begin, "line"));
    out.push(row("Cash", "", "Cash, end of period", begin + change, "total"));
    let actual: i64 = inr.iter().filter(|(k, _)| cash_ids.contains(k)).map(|(_, v)| v).sum();
    out.push(row("Check", "", "Computed change minus actual change in cash accounts", change - actual, "check"));
    Ok(out)
}

/// One line of an account's general-ledger listing.
#[derive(Clone, Debug, PartialEq)]
pub struct LedgerRow {
    pub date: String,
    pub entry_no: i64,
    pub kind: String,
    pub description: String,
    pub debit: i64,
    pub credit: i64,
    /// Running balance in the account's normal direction (a credit-normal account's credits are positive).
    pub balance: i64,
}

/// The T-account for `account` over `[from, to]`, starting from the balance before `from`.
pub fn general_ledger(view: &View, company: &str, account: &str, from: &str, to: &str) -> Result<Vec<LedgerRow>> {
    check_range(from, to)?;
    company_id(view, company)?;
    let accts = load_accounts(view, company);
    let a = accts.values().find(|a| a.number == account).ok_or_else(|| Error::Invalid(format!("unknown account '{account}'")))?;
    let sign = if a.normal == "Credit" { -1 } else { 1 };
    let entries = load_entries(view, company);
    let opening: i64 = entries.iter().filter(|e| e.date.as_str() < from).flat_map(|e| e.lines.iter()).filter(|l| l.acct == a.id).map(|l| l.dr - l.cr).sum();
    let mut bal = sign * opening;
    let mut rows = vec![LedgerRow { date: from.into(), entry_no: 0, kind: "Opening".into(), description: "Opening balance".into(), debit: 0, credit: 0, balance: bal }];
    let mut sel: Vec<&Ent> = entries.iter().filter(|e| e.date.as_str() >= from && e.date.as_str() <= to).collect();
    sel.sort_by(|x, y| (&x.date, x.no).cmp(&(&y.date, y.no)));
    for e in sel {
        for l in e.lines.iter().filter(|l| l.acct == a.id) {
            bal += sign * (l.dr - l.cr);
            rows.push(LedgerRow { date: e.date.clone(), entry_no: e.no, kind: e.kind.clone(), description: if l.memo.is_empty() { e.desc.clone() } else { format!("{} ({})", e.desc, l.memo) }, debit: l.dr, credit: l.cr, balance: bal });
        }
    }
    Ok(rows)
}

/// One integrity check.
#[derive(Clone, Debug, PartialEq)]
pub struct Check {
    pub check: &'static str,
    /// `pass`, `fail` or `warn`.
    pub status: &'static str,
    pub count: i64,
    pub detail: String,
}

fn chk(check: &'static str, bad_items: Vec<String>, warn_only: bool, ok: &str) -> Check {
    if bad_items.is_empty() {
        Check { check, status: "pass", count: 0, detail: ok.into() }
    } else {
        let mut d: Vec<String> = bad_items.iter().take(5).cloned().collect();
        if bad_items.len() > 5 {
            d.push(format!("... and {} more", bad_items.len() - 5));
        }
        Check { check, status: if warn_only { "warn" } else { "fail" }, count: bad_items.len() as i64, detail: d.join("; ") }
    }
}

/// Integrity audit of a company's books.
pub fn audit(view: &View, company: &str) -> Result<Vec<Check>> {
    company_id(view, company)?;
    let accts = load_accounts(view, company);
    let entries = load_entries(view, company);
    let mut out = Vec::new();
    let mut unbalanced = Vec::new();
    let mut short = Vec::new();
    let mut bad_lines = Vec::new();
    let mut bad_acct = Vec::new();
    for e in &entries {
        let (d, c): (i64, i64) = e.lines.iter().fold((0, 0), |s, l| (s.0 + l.dr, s.1 + l.cr));
        if d != c {
            unbalanced.push(format!("#{} debits {} credits {}", e.no, dollars(d), dollars(c)));
        }
        if e.lines.len() < 2 {
            short.push(format!("#{}", e.no));
        }
        for l in &e.lines {
            if !((l.dr > 0 && l.cr == 0) || (l.cr > 0 && l.dr == 0)) {
                bad_lines.push(format!("#{}", e.no));
            }
            if !accts.contains_key(&l.acct) {
                bad_acct.push(format!("#{} -> node {}", e.no, l.acct));
            }
        }
    }
    out.push(chk("entries_balanced", unbalanced, false, "every entry has debits equal to credits"));
    out.push(chk("entries_have_two_lines", short, false, "every entry has at least two lines"));
    bad_lines.dedup();
    out.push(chk("lines_one_sided", bad_lines, false, "every line has exactly one positive side"));
    out.push(chk("lines_reference_company_accounts", bad_acct, false, "every line points at an account of this company"));
    let nos: Vec<i64> = entries.iter().map(|e| e.no).collect();
    let gaps: Vec<String> = nos.iter().enumerate().filter(|(i, n)| **n != *i as i64 + 1).take(1).map(|(i, n)| format!("expected #{} but found #{}", i + 1, n)).collect();
    out.push(chk("entry_numbers_gapless", gaps, false, "entry numbers run 1..N without gaps or duplicates"));
    // POSTING hyperedges mirror the lines.
    let mut posting: BTreeMap<i64, (BTreeSet<Id>, BTreeSet<Id>)> = BTreeMap::new();
    for eid in view.edges_with_label("POSTING") {
        if let Some(b @ Body::Edge { tail, head, .. }) = view.edge(eid) {
            if ps(b, "company") == company {
                posting.insert(pi(b, "entry_no"), (tail.iter().copied().collect(), head.iter().copied().collect()));
            }
        }
    }
    let mut mism = Vec::new();
    for e in &entries {
        let dset: BTreeSet<Id> = e.lines.iter().filter(|l| l.dr > 0).map(|l| l.acct).collect();
        let cset: BTreeSet<Id> = e.lines.iter().filter(|l| l.cr > 0).map(|l| l.acct).collect();
        if posting.get(&e.no) != Some(&(dset, cset)) {
            mism.push(format!("#{}", e.no));
        }
    }
    out.push(chk("posting_hyperedges_match_lines", mism, false, "each entry's POSTING hyperedge has the debit and credit accounts of its lines"));
    // closed periods
    let mut viol = Vec::new();
    for id in view.nodes_with_label("Period") {
        let Some(b) = view.node(id) else { continue };
        if ps(b, "company") == company && pb(b, "closed") {
            let (s, e, at) = (ps(b, "start"), ps(b, "end"), pi(b, "closed_at_entry"));
            for en in entries.iter().filter(|x| x.date >= s && x.date <= e && x.no >= at) {
                viol.push(format!("#{} dated {} in closed period {}", en.no, en.date, ps(b, "name")));
            }
        }
    }
    out.push(chk("no_entries_after_period_close", viol, false, "nothing was posted into a closed period after it closed"));
    let last = entries.iter().map(|e| e.date.clone()).max().unwrap_or_else(|| "1970-01-01".into());
    let all = net(&entries, |_| true);
    let (td, tc): (i64, i64) = all.values().fold((0, 0), |s, v| if *v > 0 { (s.0 + v, s.1) } else { (s.0, s.1 - v) });
    out.push(chk("trial_balance_balances", if td == tc { vec![] } else { vec![format!("debits {} credits {}", dollars(td), dollars(tc))] }, false, "total debit balances equal total credit balances"));
    let bs = balance_sheet(view, company, &last)?;
    let diff = bs.last().map_or(0, |r| r.amount);
    out.push(chk("balance_sheet_equation", if diff == 0 { vec![] } else { vec![format!("assets - (liabilities + equity) = {}", dollars(diff))] }, false, "assets equal liabilities plus equity"));
    let abn: Vec<String> = sorted_accounts(&accts)
        .into_iter()
        .filter(|a| a.statement == "Balance Sheet" && !a.category.contains("Contra") && a.category != RE_CAT)
        .filter(|a| {
            let b = all.get(&a.id).copied().unwrap_or(0);
            (a.normal == "Debit" && b < 0) || (a.normal == "Credit" && b > 0)
        })
        .map(|a| format!("{} {}", a.number, a.name))
        .collect();
    out.push(chk("balances_on_normal_side", abn, true, "no permanent account has a balance on its abnormal side"));
    Ok(out)
}

// ------------------------------------------------------------------ ratio analysis

/// One financial ratio. `value` is `None` when its denominator is zero. Margins and turnovers are plain ratios
/// (0.25 = 25 %); `working_capital` is in dollars; `days_*` are days.
#[derive(Clone, Debug, PartialEq)]
pub struct RatioRow {
    pub group: &'static str,
    pub name: &'static str,
    pub value: Option<f64>,
    pub basis: &'static str,
}

#[derive(Default, Clone, Copy)]
struct Snap {
    assets: i64,
    liab: i64,
    ca: i64,
    cl: i64,
    cash: i64,
    ar: i64,
    inventory: i64,
    non_quick: i64,
}

/// Balance-sheet aggregates (cents) from per-account net debit-minus-credit balances.
///
/// Receivables are the current-asset accounts (and their contras) whose name contains "receivable" or "doubtful";
/// inventory those containing "inventor"; prepaid items and supplies ("prepaid", "suppl") are excluded from quick assets
/// together with inventory. Cash is every account tagged `cash_flow = Cash`.
fn snapshot(accts: &BTreeMap<Id, Acct>, n: &BTreeMap<Id, i64>) -> Snap {
    let mut s = Snap::default();
    for a in accts.values() {
        let b = n.get(&a.id).copied().unwrap_or(0);
        if b == 0 {
            continue;
        }
        let nm = a.name.to_lowercase();
        match a.atype.as_str() {
            "Asset" => s.assets += b,
            "Liability" => s.liab -= b,
            _ => {}
        }
        if a.category == "Current Asset" || a.category == "Current Asset—Contra" {
            s.ca += b;
            if nm.contains("receivable") || nm.contains("doubtful") {
                s.ar += b;
            }
            if nm.contains("inventor") {
                s.inventory += b;
            }
            if nm.contains("inventor") || nm.contains("prepaid") || nm.contains("suppl") {
                s.non_quick += b;
            }
        }
        if a.category == "Current Liability" {
            s.cl -= b;
        }
        if a.cash_flow == "Cash" {
            s.cash += b;
        }
    }
    s
}

/// Ratio analysis for `[from, to]`: flows (revenue, profit) come from the period's income statement, balances from the
/// books as of `to`. Return-on-assets, return-on-equity and the turnovers use the average of the balance before `from`
/// and the balance at `to`; when the opening balance is zero (the first period) the ending balance is used.
pub fn ratios(view: &View, company: &str, from: &str, to: &str) -> Result<Vec<RatioRow>> {
    check_range(from, to)?;
    company_id(view, company)?;
    let accts = load_accounts(view, company);
    let entries = load_entries(view, company);
    let is = income_statement(view, company, from, to)?;
    let pick = |name: &str| -> i64 { is.iter().find(|r| r.name == name).map_or(0, |r| r.amount) };
    let (rev, gp, oi, cogs) = (pick("Net revenue"), pick("Gross profit"), pick("Operating income"), pick("Total cost of goods sold"));
    let ni = is.last().map_or(0, |r| r.amount);
    let open = snapshot(&accts, &net(&entries, |e| e.date.as_str() < from));
    let end = snapshot(&accts, &net(&entries, |e| e.date.as_str() <= to));
    let days = (day_no(to) - day_no(from) + 1) as f64;
    let avg = |o: i64, c: i64| -> f64 { if o == 0 { c as f64 } else { (o + c) as f64 / 2.0 } };
    let div = |a: f64, b: f64| -> Option<f64> { if b == 0.0 { None } else { Some(a / b) } };
    let (eq_o, eq_c) = (open.assets - open.liab, end.assets - end.liab);
    let (avg_assets, avg_equity) = (avg(open.assets, end.assets), avg(eq_o, eq_c));
    let ar_turn = div(rev as f64, avg(open.ar, end.ar));
    let inv_turn = div(cogs as f64, avg(open.inventory, end.inventory));
    let r = |group, name, value, basis| RatioRow { group, name, value, basis };
    Ok(vec![
        r("Profitability", "gross_margin", div(gp as f64, rev as f64), "gross profit / net revenue"),
        r("Profitability", "operating_margin", div(oi as f64, rev as f64), "operating income / net revenue"),
        r("Profitability", "net_margin", div(ni as f64, rev as f64), "net income / net revenue"),
        r("Profitability", "return_on_assets", div(ni as f64, avg_assets), "net income / average total assets"),
        r("Profitability", "return_on_equity", div(ni as f64, avg_equity), "net income / average stockholders' equity"),
        r("Liquidity", "current_ratio", div(end.ca as f64, end.cl as f64), "current assets / current liabilities"),
        r("Liquidity", "quick_ratio", div((end.ca - end.non_quick) as f64, end.cl as f64), "(current assets - inventory - prepaids - supplies) / current liabilities"),
        r("Liquidity", "cash_ratio", div(end.cash as f64, end.cl as f64), "cash accounts / current liabilities"),
        r("Liquidity", "working_capital", Some(dollars(end.ca - end.cl)), "current assets - current liabilities, dollars"),
        r("Solvency", "debt_ratio", div(end.liab as f64, end.assets as f64), "total liabilities / total assets"),
        r("Solvency", "debt_to_equity", div(end.liab as f64, eq_c as f64), "total liabilities / stockholders' equity"),
        r("Solvency", "equity_multiplier", div(end.assets as f64, eq_c as f64), "total assets / stockholders' equity"),
        r("Efficiency", "asset_turnover", div(rev as f64, avg_assets), "net revenue / average total assets"),
        r("Efficiency", "receivables_turnover", ar_turn, "net revenue / average receivables"),
        r("Efficiency", "days_sales_outstanding", ar_turn.and_then(|t| div(days, t)), "days in period / receivables turnover"),
        r("Efficiency", "inventory_turnover", inv_turn, "cost of goods sold / average inventory"),
        r("Efficiency", "days_inventory", inv_turn.and_then(|t| div(days, t)), "days in period / inventory turnover"),
    ])
}

// ------------------------------------------------------------------ open items and aging

/// Aging buckets, in days since the item's origin date.
pub const AGING_BUCKETS: [&str; 4] = ["0-30", "31-60", "61-90", "91+"];

fn bucket(days: i64) -> &'static str {
    match days {
        ..=30 => AGING_BUCKETS[0],
        31..=60 => AGING_BUCKETS[1],
        61..=90 => AGING_BUCKETS[2],
        _ => AGING_BUCKETS[3],
    }
}

/// An unsettled item on one account: all entries sharing a `reference` (an invoice or bill number), netted.
#[derive(Clone, Debug, PartialEq)]
pub struct OpenItem {
    /// The shared reference; an entry without one is its own item, named `(entry N)`.
    pub reference: String,
    /// Date of the earliest entry that increased the balance in the account's normal direction (the invoice date).
    pub date: String,
    /// Days from `date` to the as-of date.
    pub days: i64,
    /// Cents, in the account's normal direction (a receivable's debit balance and a payable's credit balance are positive).
    pub amount: i64,
    pub bucket: &'static str,
}

/// Open items on `account` as of `asof`: entries are grouped by reference, a reversing entry counts toward the reference of
/// the entry it reverses, and items that net to zero are settled and dropped. The items sum to the account's ledger balance.
pub fn open_items(view: &View, company: &str, account: &str, asof: &str) -> Result<Vec<OpenItem>> {
    check_date(asof)?;
    company_id(view, company)?;
    let accts = load_accounts(view, company);
    let a = accts.values().find(|a| a.number == account).ok_or_else(|| Error::Invalid(format!("unknown account '{account}'")))?;
    let sign: i64 = if a.normal == "Credit" { -1 } else { 1 };
    let entries: Vec<Ent> = load_entries(view, company).into_iter().filter(|e| e.date.as_str() <= asof).collect();
    let refs: BTreeMap<i64, &str> = entries.iter().map(|e| (e.no, e.reference.as_str())).collect();
    // reference -> (amount, earliest increasing date, earliest date)
    let mut m: BTreeMap<String, (i64, Option<String>, String)> = BTreeMap::new();
    for e in &entries {
        if e.lines.iter().all(|l| l.acct != a.id) {
            continue;
        }
        let d: i64 = e.lines.iter().filter(|l| l.acct == a.id).map(|l| sign * (l.dr - l.cr)).sum();
        let key = if e.reverses > 0 {
            refs.get(&e.reverses).copied().filter(|r| !r.is_empty()).map(String::from).unwrap_or_else(|| format!("(entry {})", e.reverses))
        } else if e.reference.is_empty() {
            format!("(entry {})", e.no)
        } else {
            e.reference.clone()
        };
        let slot = m.entry(key).or_insert((0, None, e.date.clone()));
        slot.0 += d;
        if d > 0 && slot.1.as_deref().map_or(true, |x| e.date.as_str() < x) {
            slot.1 = Some(e.date.clone());
        }
        if e.date < slot.2 {
            slot.2 = e.date.clone();
        }
    }
    let end = day_no(asof);
    let mut out: Vec<OpenItem> = m
        .into_iter()
        .filter(|(_, v)| v.0 != 0)
        .map(|(reference, (amount, origin, first))| {
            let date = origin.unwrap_or(first);
            let days = end - day_no(&date);
            OpenItem { reference, date, days, amount, bucket: bucket(days) }
        })
        .collect();
    out.sort_by(|x, y| (&x.date, &x.reference).cmp(&(&y.date, &y.reference)));
    Ok(out)
}

/// Aging summary per bucket for `account` as of `asof`: `(bucket, items, cents)` in [`AGING_BUCKETS`] order, plus the
/// account's ledger balance in the account's normal direction (the totals must agree).
pub fn aging(view: &View, company: &str, account: &str, asof: &str) -> Result<(Vec<(&'static str, i64, i64)>, i64)> {
    let items = open_items(view, company, account, asof)?;
    let accts = load_accounts(view, company);
    let a = accts.values().find(|a| a.number == account).ok_or_else(|| Error::Invalid(format!("unknown account '{account}'")))?;
    let sign: i64 = if a.normal == "Credit" { -1 } else { 1 };
    let entries = load_entries(view, company);
    let balance = sign * net(&entries, |e| e.date.as_str() <= asof).get(&a.id).copied().unwrap_or(0);
    let rows = AGING_BUCKETS.iter().map(|b| (*b, items.iter().filter(|i| i.bucket == *b).count() as i64, items.iter().filter(|i| i.bucket == *b).map(|i| i.amount).sum())).collect();
    Ok((rows, balance))
}

// ------------------------------------------------------------------ write side

struct PeriodInfo {
    id: Id,
    name: String,
    start: String,
    end: String,
    closed: bool,
}

struct Ctx {
    company: Id,
    next: i64,
    periods: Vec<PeriodInfo>,
}

fn ctx(view: &View, company: &str) -> Result<Ctx> {
    let cid = company_id(view, company)?;
    let next = view.node(cid).map_or(1, |b| pi(b, "next_entry")).max(1);
    let mut periods = Vec::new();
    for id in view.nodes_with_label("Period") {
        if let Some(b) = view.node(id) {
            if ps(b, "company") == company {
                periods.push(PeriodInfo { id, name: ps(b, "name"), start: ps(b, "start"), end: ps(b, "end"), closed: pb(b, "closed") });
            }
        }
    }
    Ok(Ctx { company: cid, next, periods })
}

struct Resolved {
    date: String,
    journal: String,
    kind: &'static str,
    desc: String,
    reference: Option<String>,
    period: String,
    lines: Vec<(Id, i64, i64, String)>,
    total: i64,
    reverses: Option<i64>,
}

/// Validate an entry against the committed state. `exempt` is a `[start, end]` range of closed sub-periods that a
/// closing entry may still be dated in.
fn resolve(view: &View, c: &Ctx, company: &str, e: &Entry, reverses: Option<i64>, exempt: Option<(&str, &str)>) -> Result<Resolved> {
    check_date(&e.date)?;
    if !JOURNALS.contains(&e.journal.as_str()) {
        return inv(format!("journal must be one of {}", JOURNALS.join(", ")));
    }
    let desc = e.description.trim();
    if desc.is_empty() || desc.chars().count() > 500 {
        return inv("description is required (1-500 characters)");
    }
    if e.reference.as_deref().map_or(false, |r| r.chars().count() > 30) {
        return inv("reference is at most 30 characters");
    }
    if e.lines.len() < 2 {
        return inv("an entry needs at least two lines");
    }
    let mut lines = Vec::new();
    let (mut td, mut tc) = (0i64, 0i64);
    for (i, l) in e.lines.iter().enumerate() {
        let (d, cr) = (to_cents(l.debit)?, to_cents(l.credit)?);
        if d < 0 || cr < 0 || !((d > 0) ^ (cr > 0)) {
            return inv(format!("line {}: exactly one of debit and credit must be positive (debit {}, credit {})", i + 1, l.debit, l.credit));
        }
        let id = view.lookup("Account", "key", &Value::Str(akey(company, &l.account))).first().copied().ok_or_else(|| Error::Invalid(format!("line {}: unknown account '{}'", i + 1, l.account)))?;
        if !view.node(id).map_or(false, |b| pb(b, "active")) {
            return inv(format!("line {}: account '{}' is inactive", i + 1, l.account));
        }
        td += d;
        tc += cr;
        lines.push((id, d, cr, l.memo.clone().unwrap_or_default()));
    }
    if td != tc {
        return inv(format!("entry is not balanced: debits {} credits {}", dollars(td), dollars(tc)));
    }
    let cover: Vec<&PeriodInfo> = c.periods.iter().filter(|p| p.start <= e.date && e.date <= p.end).collect();
    if cover.is_empty() {
        return inv(format!("no accounting period covers {}", e.date));
    }
    for p in &cover {
        let exempted = exempt.map_or(false, |(s, en)| p.start.as_str() >= s && p.end.as_str() <= en);
        if p.closed && !exempted {
            return inv(format!("{} falls in closed period '{}'", e.date, p.name));
        }
    }
    let period = match &e.period {
        Some(n) => cover.iter().find(|p| &p.name == n).map(|p| p.name.clone()).ok_or_else(|| Error::Invalid(format!("period '{n}' does not cover {}", e.date)))?,
        None => cover.iter().min_by(|a, b| (day_no(&a.end) - day_no(&a.start), &a.name).cmp(&(day_no(&b.end) - day_no(&b.start), &b.name))).map(|p| p.name.clone()).unwrap_or_default(),
    };
    Ok(Resolved { date: e.date.clone(), journal: e.journal.clone(), kind: e.kind.as_str(), desc: desc.to_string(), reference: e.reference.clone(), period, lines, total: td, reverses })
}

fn stage(tx: &mut Txn, company: &str, r: &Resolved, no: i64) -> Result<Id> {
    let mut np = p(vec![
        ("company", Value::Str(company.into())),
        ("key", Value::Str(akey(company, &no.to_string()))),
        ("no", Value::Int(no)),
        ("date", Value::Str(r.date.clone())),
        ("journal", Value::Str(r.journal.clone())),
        ("type", Value::Str(r.kind.into())),
        ("description", Value::Str(r.desc.clone())),
        ("period", Value::Str(r.period.clone())),
    ]);
    if let Some(x) = &r.reference {
        np.insert("reference".into(), Value::Str(x.clone()));
    }
    if let Some(x) = r.reverses {
        np.insert("reverses".into(), Value::Int(x));
    }
    let en = tx.create_node(&["JournalEntry"], np)?;
    let (mut debit_set, mut credit_set) = (Vec::new(), Vec::new());
    for (i, (acct, d, c, memo)) in r.lines.iter().enumerate() {
        let mut lp = p(vec![("line_no", Value::Int(i as i64 + 1)), ("debit", Value::Int(*d)), ("credit", Value::Int(*c))]);
        if !memo.is_empty() {
            lp.insert("memo".into(), Value::Str(memo.clone()));
        }
        tx.create_edge("LINE", en, *acct, dollars(*d + *c), lp)?;
        let set = if *d > 0 { &mut debit_set } else { &mut credit_set };
        if !set.contains(acct) {
            set.push(*acct);
        }
    }
    tx.create_hyperedge("POSTING", &debit_set, &credit_set, dollars(r.total), p(vec![("company", Value::Str(company.into())), ("entry_no", Value::Int(no))]))?;
    Ok(en)
}

/// The write side of the ledger. All operations are single atomic transactions.
pub struct Ledger<'a> {
    db: &'a Database,
}

impl<'a> Ledger<'a> {
    /// Attach to a database, creating the lookup indexes on first use.
    pub fn new(db: &'a Database) -> Result<Ledger<'a>> {
        let need = [("Company", "name"), ("Account", "key"), ("Period", "key"), ("JournalEntry", "key")];
        let missing: Vec<(&str, &str)> = db.read(|v| need.iter().filter(|(l, k)| !v.has_prop_index(l, k)).copied().collect());
        if !missing.is_empty() {
            let mut tx = db.begin();
            for (l, k) in missing {
                tx.create_property_index(l, k);
            }
            tx.commit()?;
        }
        Ok(Ledger { db })
    }

    pub fn create_company(&self, name: &str, legal_form: &str, currency: &str, fiscal_year_start: u8) -> Result<Id> {
        if name.trim().is_empty() || name.contains('\u{1f}') {
            return inv("company name is required");
        }
        if !LEGAL_FORMS.contains(&legal_form) {
            return inv(format!("legal form must be one of {}", LEGAL_FORMS.join(", ")));
        }
        if currency.len() != 3 || !currency.bytes().all(|b| b.is_ascii_uppercase()) {
            return inv("currency is a three-letter ISO code such as USD");
        }
        if !(1..=12).contains(&fiscal_year_start) {
            return inv("fiscal_year_start is a month 1..=12");
        }
        let mut tx = self.db.begin();
        if !self.db.read(|v| v.lookup("Company", "name", &Value::Str(name.into()))).is_empty() {
            return inv(format!("company '{name}' already exists"));
        }
        let id = tx.create_node(&["Company"], p(vec![("name", Value::Str(name.into())), ("legal_form", Value::Str(legal_form.into())), ("currency", Value::Str(currency.into())), ("fiscal_year_start", Value::Int(fiscal_year_start as i64)), ("next_entry", Value::Int(1))]))?;
        tx.commit()?;
        Ok(id)
    }

    /// Add an account in one of the 20 [`CATEGORIES`]; type, normal balance and statement follow from the category.
    pub fn add_account(&self, company: &str, number: &str, name: &str, category_name: &str) -> Result<Id> {
        if number.is_empty() || number.chars().count() > 10 || number.contains('\u{1f}') {
            return inv("account number is 1-10 characters");
        }
        if name.trim().is_empty() {
            return inv("account name is required");
        }
        let cat = category(category_name).ok_or_else(|| Error::Invalid(format!("unknown category '{category_name}'")))?;
        let mut tx = self.db.begin();
        company_id_in(self.db, company)?;
        if !self.db.read(|v| v.lookup("Account", "key", &Value::Str(akey(company, number)))).is_empty() {
            return inv(format!("account {number} already exists"));
        }
        let id = tx.create_node(
            &["Account"],
            p(vec![
                ("company", Value::Str(company.into())),
                ("key", Value::Str(akey(company, number))),
                ("number", Value::Str(number.into())),
                ("name", Value::Str(name.into())),
                ("category", Value::Str(cat.0.into())),
                ("type", Value::Str(cat.1.into())),
                ("normal", Value::Str(cat.2.into())),
                ("statement", Value::Str(cat.3.into())),
                ("contra", Value::Bool(cat.0.contains("Contra"))),
                ("cash_flow", Value::Str(default_cash_flow(cat.0).into())),
                ("active", Value::Bool(true)),
            ]),
        )?;
        tx.commit()?;
        Ok(id)
    }

    /// Load the reference schema's 41-account demonstration chart (cash account 101 is tagged `Cash`; notes and
    /// dividends payable are financing).
    pub fn seed_reference_chart(&self, company: &str) -> Result<()> {
        for (num, name, cat) in REFERENCE_CHART {
            self.add_account(company, num, name, CATEGORIES[cat - 1].0)?;
        }
        self.set_cash_flow(company, "101", "Cash")?;
        self.set_cash_flow(company, "240", "Financing")?;
        self.set_cash_flow(company, "250", "Financing")
    }

    pub fn set_cash_flow(&self, company: &str, number: &str, class: &str) -> Result<()> {
        if !CASH_FLOW.contains(&class) {
            return inv(format!("cash-flow class must be one of {}", CASH_FLOW.join(", ")));
        }
        let mut tx = self.db.begin();
        let id = self.account_id(company, number)?;
        tx.set_prop(id, "cash_flow", Value::Str(class.into()))?;
        tx.commit()?;
        Ok(())
    }

    /// Retire or reinstate an account. Inactive accounts keep their history but accept no new lines.
    pub fn set_account_active(&self, company: &str, number: &str, active: bool) -> Result<()> {
        let mut tx = self.db.begin();
        let id = self.account_id(company, number)?;
        tx.set_prop(id, "active", Value::Bool(active))?;
        tx.commit()?;
        Ok(())
    }

    fn account_id(&self, company: &str, number: &str) -> Result<Id> {
        self.db.read(|v| v.lookup("Account", "key", &Value::Str(akey(company, number))).first().copied()).ok_or_else(|| Error::Invalid(format!("unknown account '{number}'")))
    }

    /// Add an accounting period (`Monthly`, `Quarterly` or `Annual`; inclusive dates). Periods may nest, as a year
    /// does its quarters; a date can be posted only if none of the periods covering it is closed.
    pub fn add_period(&self, company: &str, name: &str, kind: &str, start: &str, end: &str) -> Result<Id> {
        if !PERIOD_KINDS.contains(&kind) {
            return inv(format!("period type must be one of {}", PERIOD_KINDS.join(", ")));
        }
        check_range(start, end)?;
        let mut tx = self.db.begin();
        company_id_in(self.db, company)?;
        if !self.db.read(|v| v.lookup("Period", "key", &Value::Str(akey(company, name)))).is_empty() {
            return inv(format!("period '{name}' already exists"));
        }
        let id = tx.create_node(
            &["Period"],
            p(vec![("company", Value::Str(company.into())), ("key", Value::Str(akey(company, name))), ("name", Value::Str(name.into())), ("type", Value::Str(kind.into())), ("start", Value::Str(start.into())), ("end", Value::Str(end.into())), ("closed", Value::Bool(false)), ("closed_at_entry", Value::Int(0))]),
        )?;
        tx.commit()?;
        Ok(id)
    }

    /// Post a balanced entry. See the module docs for the rules.
    pub fn post(&self, company: &str, e: &Entry) -> Result<Posted> {
        if matches!(e.kind, EntryKind::Closing | EntryKind::Reversing) {
            return inv("closing and reversing entries are created by close_period and reverse");
        }
        let mut tx = self.db.begin();
        let (c, r) = self.db.read(|v| -> Result<(Ctx, Resolved)> {
            let c = ctx(v, company)?;
            let r = resolve(v, &c, company, e, None, None)?;
            Ok((c, r))
        })?;
        let no = c.next;
        let id = stage(&mut tx, company, &r, no)?;
        tx.set_prop(c.company, "next_entry", Value::Int(no + 1))?;
        tx.commit()?;
        Ok(Posted { entry_no: no, id, period: r.period, total: dollars(r.total) })
    }

    /// Post many entries as **one atomic transaction**: every entry is validated first (against the committed books,
    /// exactly as [`Ledger::post`] does), and either all are posted with consecutive entry numbers or none is and the
    /// error names the first failing entry (`entry 3: ...`). `Closing` and `Reversing` entries are refused.
    pub fn post_many(&self, company: &str, entries: &[Entry]) -> Result<Vec<Posted>> {
        self.post_many_labeled(company, entries, &|i| format!("entry {}", i + 1))
    }

    fn post_many_labeled(&self, company: &str, entries: &[Entry], label: &dyn Fn(usize) -> String) -> Result<Vec<Posted>> {
        if entries.is_empty() {
            return Ok(Vec::new());
        }
        let mut tx = self.db.begin();
        let (c, rs) = self.db.read(|v| -> Result<(Ctx, Vec<Resolved>)> {
            let c = ctx(v, company)?;
            let mut rs = Vec::with_capacity(entries.len());
            for (i, e) in entries.iter().enumerate() {
                let tag = |er: Error| match er {
                    Error::Invalid(m) => Error::Invalid(format!("{}: {m}", label(i))),
                    o => o,
                };
                if matches!(e.kind, EntryKind::Closing | EntryKind::Reversing) {
                    return Err(tag(Error::Invalid("closing and reversing entries are created by close_period and reverse".into())));
                }
                rs.push(resolve(v, &c, company, e, None, None).map_err(tag)?);
            }
            Ok((c, rs))
        })?;
        let mut no = c.next;
        let mut out = Vec::with_capacity(rs.len());
        for r in &rs {
            let id = stage(&mut tx, company, r, no)?;
            out.push(Posted { entry_no: no, id, period: r.period.clone(), total: dollars(r.total) });
            no += 1;
        }
        tx.set_prop(c.company, "next_entry", Value::Int(no))?;
        tx.commit()?;
        Ok(out)
    }

    /// Import a journal from CSV text, atomically (all entries or none). One row per line; rows sharing the `entry` value
    /// form one entry. Required columns: `entry`, `date` (YYYY-MM-DD), `account`, and `debit` and/or `credit`. Optional:
    /// `journal`, `reference`, `description`, `period`, `memo`. Date, journal, reference, description and period are taken from
    /// the entry's first row (a later row with a different date is an error). Errors name the entry and CSV line.
    #[cfg(feature = "ingest")]
    pub fn import_journal_csv(&self, company: &str, csv: &str) -> Result<Vec<Posted>> {
        let mut rows = crate::ingest::parse_csv(csv, ',')?.into_iter();
        let (_, head) = rows.next().ok_or_else(|| Error::Invalid("journal CSV is empty".into()))?;
        let head: Vec<String> = head.iter().map(|h| h.trim().to_lowercase()).collect();
        let col = |n: &str| head.iter().position(|h| h == n);
        let need = |n: &str| col(n).ok_or_else(|| Error::Invalid(format!("journal CSV needs a '{n}' column")));
        let (c_entry, c_date, c_acct) = (need("entry")?, need("date")?, need("account")?);
        let (c_dr, c_cr) = (col("debit"), col("credit"));
        if c_dr.is_none() && c_cr.is_none() {
            return inv("journal CSV needs a 'debit' and/or 'credit' column");
        }
        let (c_j, c_ref, c_desc, c_memo, c_per) = (col("journal"), col("reference"), col("description"), col("memo"), col("period"));
        let get = |r: &Vec<String>, c: Option<usize>| -> String { c.and_then(|i| r.get(i)).map(|s| s.trim().to_string()).unwrap_or_default() };
        let num = |line: usize, what: &str, s: String| -> Result<f64> {
            if s.is_empty() {
                Ok(0.0)
            } else {
                s.parse::<f64>().map_err(|_| Error::Invalid(format!("line {line}: {what} '{s}' is not a number")))
            }
        };
        let (mut entries, mut labels, mut idx): (Vec<Entry>, Vec<String>, BTreeMap<String, usize>) = (Vec::new(), Vec::new(), BTreeMap::new());
        for (line, r) in rows {
            let key = get(&r, Some(c_entry));
            if key.is_empty() {
                return inv(format!("line {line}: the entry column is empty"));
            }
            let date = get(&r, Some(c_date));
            let i = match idx.get(&key) {
                Some(&i) => {
                    if !date.is_empty() && date != entries[i].date {
                        return inv(format!("line {line}: entry '{key}' has two different dates"));
                    }
                    i
                }
                None => {
                    let desc = get(&r, c_desc);
                    let mut e = Entry::new(&date, if desc.is_empty() { "" } else { &desc });
                    if desc.is_empty() {
                        e.description = format!("Imported entry {key}");
                    }
                    let j = get(&r, c_j);
                    if !j.is_empty() {
                        e = e.journal(&j);
                    }
                    let rf = get(&r, c_ref);
                    if !rf.is_empty() {
                        e = e.reference(&rf);
                    }
                    let pr = get(&r, c_per);
                    if !pr.is_empty() {
                        e = e.period(&pr);
                    }
                    entries.push(e);
                    labels.push(format!("entry '{key}' (line {line})"));
                    idx.insert(key.clone(), entries.len() - 1);
                    entries.len() - 1
                }
            };
            let (debit, credit) = (num(line, "debit", get(&r, c_dr))?, num(line, "credit", get(&r, c_cr))?);
            let memo = get(&r, c_memo);
            entries[i].lines.push(Line { account: get(&r, Some(c_acct)), debit, credit, memo: if memo.is_empty() { None } else { Some(memo) } });
        }
        self.post_many_labeled(company, &entries, &|i| labels[i].clone())
    }

    /// Reverse a posted entry: a new `Reversing` entry with every line's side swapped, dated `date`. An entry can be
    /// reversed once; closing and reversing entries cannot be reversed.
    pub fn reverse(&self, company: &str, entry_no: i64, date: &str, description: Option<&str>) -> Result<Posted> {
        let mut tx = self.db.begin();
        let (c, r, orig) = self.db.read(|v| -> Result<(Ctx, Resolved, Id)> {
            let c = ctx(v, company)?;
            let orig = v.lookup("JournalEntry", "key", &Value::Str(akey(company, &entry_no.to_string()))).first().copied().ok_or_else(|| Error::Invalid(format!("no entry #{entry_no}")))?;
            let ob = v.node(orig).ok_or(Error::NotFound(orig))?;
            let kind = ps(ob, "type");
            if kind == "Closing" || kind == "Reversing" {
                return inv(format!("entry #{entry_no} is a {kind} entry and cannot be reversed"));
            }
            if ob.props().contains_key("reversed_by") {
                return inv(format!("entry #{entry_no} was already reversed"));
            }
            let accts = load_accounts(v, company);
            let ent = load_entries(v, company).into_iter().find(|x| x.no == entry_no).ok_or(Error::NotFound(orig))?;
            let mut e = Entry::new(date, description.map(str::to_string).unwrap_or_else(|| format!("Reversal of entry #{entry_no}: {}", ent.desc)).as_str()).journal(&ps(ob, "journal")).kind(EntryKind::Reversing);
            e.reference = Some(format!("REV-{entry_no}"));
            for l in &ent.lines {
                let num = accts.get(&l.acct).map(|a| a.number.clone()).ok_or(Error::NotFound(l.acct))?;
                e.lines.push(Line { account: num, debit: dollars(l.cr), credit: dollars(l.dr), memo: if l.memo.is_empty() { None } else { Some(l.memo.clone()) } });
            }
            let r = resolve(v, &c, company, &e, Some(entry_no), None)?;
            Ok((c, r, orig))
        })?;
        let no = c.next;
        let id = stage(&mut tx, company, &r, no)?;
        tx.set_prop(orig, "reversed_by", Value::Int(no))?;
        tx.set_prop(c.company, "next_entry", Value::Int(no + 1))?;
        tx.commit()?;
        Ok(Posted { entry_no: no, id, period: r.period, total: dollars(r.total) })
    }

    /// Close a period: revenue to the income summary, expenses and cost of goods sold to the income summary, the
    /// income summary to retained earnings, and dividends to retained earnings; then mark the period closed. The
    /// closing entries are dated the period's end date. Balances are taken cumulatively to that date, so earlier
    /// unclosed periods are swept too. Returns the entries posted (empty when nothing needed closing).
    pub fn close_period(&self, company: &str, period: &str, accounts: ClosingAccounts) -> Result<Vec<Posted>> {
        let mut tx = self.db.begin();
        let (c, pid, resolved) = self.db.read(|v| -> Result<(Ctx, Id, Vec<Resolved>)> {
            let c = ctx(v, company)?;
            let per = c.periods.iter().find(|x| x.name == period).ok_or_else(|| Error::Invalid(format!("unknown period '{period}'")))?;
            if per.closed {
                return inv(format!("period '{period}' is already closed"));
            }
            let (pid, start, end) = (per.id, per.start.clone(), per.end.clone());
            let accts = load_accounts(v, company);
            let pick = |given: Option<&str>, cat: &str, what: &str| -> Result<String> {
                if let Some(n) = given {
                    return accts.values().find(|a| a.number == n && a.active).map(|a| a.number.clone()).ok_or_else(|| Error::Invalid(format!("{what} account '{n}' not found or inactive")));
                }
                let m: Vec<&Acct> = accts.values().filter(|a| a.category == cat && a.active).collect();
                if m.len() == 1 {
                    Ok(m[0].number.clone())
                } else {
                    inv(format!("{what}: found {} active '{cat}' accounts, name one in ClosingAccounts", m.len()))
                }
            };
            let es = pick(accounts.income_summary, TEMP_CAT, "income summary")?;
            let re = pick(accounts.retained_earnings, RE_CAT, "retained earnings")?;
            let entries = load_entries(v, company);
            let bal = net(&entries, |e| e.date.as_str() <= end.as_str());
            let es_bal = accts.values().find(|a| a.number == es).and_then(|a| bal.get(&a.id).copied()).unwrap_or(0);
            let make = |desc: &str, ids: Vec<&Acct>, to_acct: &str| -> Option<Entry> {
                let mut e = Entry::new(&end, desc).journal("Closing").kind(EntryKind::Closing).reference(&format!("CLOSE-{period}"));
                let mut netd = 0i64;
                for a in ids {
                    let b = bal.get(&a.id).copied().unwrap_or(0);
                    if b == 0 {
                        continue;
                    }
                    e.lines.push(if b > 0 { Line::cr(&a.number, dollars(b)) } else { Line::dr(&a.number, dollars(-b)) });
                    netd -= b;
                }
                if e.lines.is_empty() {
                    return None;
                }
                e.lines.push(if netd > 0 { Line::cr(to_acct, dollars(netd)) } else { Line::dr(to_acct, dollars(-netd)) });
                Some(e)
            };
            let sorted = sorted_accounts(&accts);
            let rev: Vec<&Acct> = sorted.iter().copied().filter(|a| a.atype == "Revenue").collect();
            let exp: Vec<&Acct> = sorted.iter().copied().filter(|a| a.atype == "Expense" || a.atype == "COGS").collect();
            let div: Vec<&Acct> = sorted.iter().copied().filter(|a| a.category == DIV_CAT).collect();
            let mut plan: Vec<Entry> = Vec::new();
            plan.extend(make(&format!("Close revenue accounts to income summary ({period})"), rev, &es));
            plan.extend(make(&format!("Close expense accounts to income summary ({period})"), exp, &es));
            // Income summary now carries (its prior balance) + the lines posted above.
            let posted_to_es: i64 = plan.iter().flat_map(|e| e.lines.iter().filter(|l| l.account == es)).map(|l| to_cents(l.debit).unwrap_or(0) - to_cents(l.credit).unwrap_or(0)).sum();
            let es_net = es_bal + posted_to_es;
            if es_net != 0 {
                let mut e = Entry::new(&end, &format!("Close income summary to retained earnings ({period})")).journal("Closing").kind(EntryKind::Closing).reference(&format!("CLOSE-{period}"));
                if es_net < 0 {
                    e = e.dr(&es, dollars(-es_net)).cr(&re, dollars(-es_net));
                } else {
                    e = e.dr(&re, dollars(es_net)).cr(&es, dollars(es_net));
                }
                plan.push(e);
            }
            plan.extend(make(&format!("Close dividends to retained earnings ({period})"), div, &re));
            let mut resolved = Vec::new();
            for e in &plan {
                resolved.push(resolve(v, &c, company, e, None, Some((start.as_str(), end.as_str())))?);
            }
            Ok((c, pid, resolved))
        })?;
        let mut no = c.next;
        let mut out = Vec::new();
        for r in &resolved {
            let id = stage(&mut tx, company, r, no)?;
            out.push(Posted { entry_no: no, id, period: r.period.clone(), total: dollars(r.total) });
            no += 1;
        }
        tx.set_prop(pid, "closed", Value::Bool(true))?;
        tx.set_prop(pid, "closed_at_entry", Value::Int(no))?;
        tx.set_prop(c.company, "next_entry", Value::Int(no))?;
        tx.commit()?;
        Ok(out)
    }
}

fn company_id_in(db: &Database, company: &str) -> Result<Id> {
    db.read(|v| company_id(v, company))
}

// ------------------------------------------------------------------ procedures

fn fl(c: i64) -> Value {
    Value::Float(dollars(c))
}

fn stmt_rows(rows: Vec<StmtRow>) -> Rows {
    (vec!["section", "account", "name", "amount", "kind"], rows.into_iter().map(|r| vec![Value::Str(r.section), Value::Str(r.number), Value::Str(r.name), fl(r.amount), Value::Str(r.kind.into())]).collect())
}

/// `CALL gaap.*` dispatcher.
pub fn call(view: &View, name: &str, a: &[Value]) -> Result<Rows> {
    let co = need_str(a, 0, "company")?;
    match name {
        "gaap.trial_balance" => {
            let kind = TbKind::parse(need_str(a, 1, "type")?).ok_or_else(|| Error::Query("type must be unadjusted, adjusted or post-closing".into()))?;
            let tb = trial_balance(view, co, kind, need_str(a, 2, "start")?, need_str(a, 3, "end")?)?;
            let mut rows: Vec<Vec<Value>> = tb.rows.iter().map(|r| vec![Value::Str(r.number.clone()), Value::Str(r.name.clone()), Value::Str(r.category.clone()), fl(r.debit), fl(r.credit)]).collect();
            rows.push(vec![Value::Str("TOTAL".into()), Value::Str(if tb.balanced() { "balanced".into() } else { "OUT OF BALANCE".into() }), Value::Null, fl(tb.debits), fl(tb.credits)]);
            Ok((vec!["account", "name", "category", "debit", "credit"], rows))
        }
        "gaap.income_statement" => Ok(stmt_rows(income_statement(view, co, need_str(a, 1, "from")?, need_str(a, 2, "to")?)?)),
        "gaap.retained_earnings" => Ok(stmt_rows(retained_earnings_statement(view, co, need_str(a, 1, "from")?, need_str(a, 2, "to")?)?)),
        "gaap.balance_sheet" => Ok(stmt_rows(balance_sheet(view, co, need_str(a, 1, "asof")?)?)),
        "gaap.cash_flow" => Ok(stmt_rows(cash_flow(view, co, need_str(a, 1, "from")?, need_str(a, 2, "to")?)?)),
        "gaap.ledger" => {
            let rows = general_ledger(view, co, need_str(a, 1, "account")?, need_str(a, 2, "from")?, need_str(a, 3, "to")?)?;
            Ok((vec!["date", "entry", "type", "description", "debit", "credit", "balance"], rows.into_iter().map(|r| vec![Value::Str(r.date), Value::Int(r.entry_no), Value::Str(r.kind), Value::Str(r.description), fl(r.debit), fl(r.credit), fl(r.balance)]).collect()))
        }
        "gaap.audit" => Ok((vec!["check", "status", "count", "detail"], audit(view, co)?.into_iter().map(|c| vec![Value::Str(c.check.into()), Value::Str(c.status.into()), Value::Int(c.count), Value::Str(c.detail)]).collect())),
        "gaap.ratios" => {
            let rows = ratios(view, co, need_str(a, 1, "from")?, need_str(a, 2, "to")?)?;
            Ok((vec!["group", "ratio", "value", "basis"], rows.into_iter().map(|r| vec![Value::Str(r.group.into()), Value::Str(r.name.into()), r.value.map_or(Value::Null, Value::Float), Value::Str(r.basis.into())]).collect()))
        }
        "gaap.open_items" => {
            let items = open_items(view, co, need_str(a, 1, "account")?, need_str(a, 2, "asof")?)?;
            Ok((vec!["reference", "date", "days", "amount", "bucket"], items.into_iter().map(|i| vec![Value::Str(i.reference), Value::Str(i.date), Value::Int(i.days), fl(i.amount), Value::Str(i.bucket.into())]).collect()))
        }
        "gaap.aging" => {
            let (rows, balance) = aging(view, co, need_str(a, 1, "account")?, need_str(a, 2, "asof")?)?;
            let (n, total): (i64, i64) = rows.iter().fold((0, 0), |t, r| (t.0 + r.1, t.1 + r.2));
            let mut out: Vec<Vec<Value>> = rows.iter().map(|r| vec![Value::Str(r.0.into()), Value::Int(r.1), fl(r.2)]).collect();
            out.push(vec![Value::Str("TOTAL".into()), Value::Int(n), fl(total)]);
            out.push(vec![Value::Str("Ledger balance".into()), Value::Null, fl(balance)]);
            out.push(vec![Value::Str("Difference".into()), Value::Null, fl(total - balance)]);
            Ok((vec!["bucket", "items", "amount"], out))
        }
        o => bad(format!("unknown procedure {o}; the gaap procedures are gaap.trial_balance, gaap.income_statement, gaap.retained_earnings, gaap.balance_sheet, gaap.cash_flow, gaap.ledger, gaap.audit, gaap.ratios, gaap.open_items, gaap.aging")),
    }
}
