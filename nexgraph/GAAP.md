# GAAP ledger (v0.17, extended in v0.18)

A double-entry general ledger with the US-GAAP statements, stored in the graph. Module `nexgraph::gaap`; procedures `CALL gaap.*`.
The chart of accounts, 20 categories, normal balances and statement classification come from the reference
`GAAP_ACCOUNTING_SYSTEM` PostgreSQL schema (Weygandt, *Accounting Principles*, 12th ed.).

## Quick start
```rust
use nexgraph::{Database, gaap::*};
let db = Database::open("books")?;                       // or Database::in_memory()
let l = Ledger::new(&db)?;
l.create_company("Acme", "Corporation", "USD", 1)?;
l.seed_reference_chart("Acme")?;                         // the reference 41-account chart
l.add_period("Acme", "FY2024", "Annual", "2024-01-01", "2024-12-31")?;
l.post("Acme", &Entry::new("2024-01-02", "Issue stock").dr("101", 100000.0).cr("401", 100000.0))?;
l.post("Acme", &Entry::adjusting("2024-12-31", "Depreciation", "630", "161", 2200.0))?;
l.close_period("Acme", "FY2024", ClosingAccounts::default())?;
```
```
CALL gaap.balance_sheet('Acme', '2024-12-31')
AS OF 12 CALL gaap.income_statement('Acme', '2024-01-01', '2024-12-31')      -- the books as they stood at commit 12
CALL gaap.trial_balance('Acme', 'post-closing', '2024-01-01', '2024-12-31')
CALL gaap.audit('Acme')
```
`cargo run --release --example gaap_demo` prints the worked year used in the tests.

## Model
| | label | notes |
|---|---|---|
| entity | `:Company` | holds the gapless entry counter |
| account | `:Account` | `number`, `category` -> `type`, `normal`, `statement`; `cash_flow` class; `active` |
| period | `:Period` | `Monthly`/`Quarterly`/`Annual`, may nest (quarters inside a year) |
| entry | `:JournalEntry` | `no` (1..N, no gaps), `date`, `journal`, `type`, `description`, `period`, `reverses`/`reversed_by` |
| line | `(:JournalEntry)-[:LINE {debit, credit}]->(:Account)` | integer cents, exactly one side positive |
| event | `(debit accounts)-[:POSTING]->(credit accounts)` | one directed hyperedge per entry, weight = total |

The `:POSTING` hyperedge makes each transaction a first-class n-ary event, so `spectral.*`, `algo.*` (with the label `POSTING`)
and community detection run on the flow of money between accounts with no extra step.

## What is enforced
Balanced to the cent (no tolerance); at least two lines; one positive side per line; known, active accounts; real calendar dates;
a period that covers the date and **no closed period covering it** (a closed quarter blocks its dates even while the year is open);
immutability (corrections are reversing entries); `Closing`/`Reversing` entries only from `close_period`/`reverse`.
Every operation is one atomic transaction; a rejected post consumes no entry number.

## Statements
* **Trial balance**: `unadjusted` / `adjusted` / `post-closing`; abnormal balances are shown on their own side.
* **Income statement**: multi-step (net revenue, gross profit, operating income, income before tax, net income); contra revenue is negative.
* **Retained earnings**: beginning + net income - dividends (+ direct adjustments) = ending; correct whether or not prior years were closed.
* **Balance sheet**: classified; current-period income is folded into retained earnings, so it balances before closing. Last row is the equation check.
* **Cash flow**: indirect; classification is the account's `cash_flow` (`Cash`/`Operating`/`Investing`/`Financing`, defaults by category,
  override with `set_cash_flow`); reconciled to the real change in the cash accounts, last row is the difference.
* **Ledger**: T-account with running balance. **Audit**: balanced entries, gapless numbers, hyperedges match lines, nothing posted after a
  period closed, trial balance and balance-sheet equation, abnormal balances (warning).

## Verification
* 14 tests in `tests/gaap.rs` (v0.17) and 10 in `tests/gaap_v018.rs` / `tests/gaap_csv.rs` (v0.18: ratios hand-computed on the same year, average-balance rule, atomic batches, aging with partial payments and a reversal, 1,500 random invoices tied to the ledger, CSV import); the year's figures are hand-computed in the comments (net income 8,700; total assets 159,900;
  operating cash flow 23,600; investing -34,000; financing 120,000).
* **Differential check against the reference SQL on PostgreSQL 16**: the same 19 entries posted through `sp_post_journal_entry`.
  All 23 per-account balances match nexgraph's trial balance, and every income-statement amount matches `vw_income_statement`.
* 3,000 random balanced entries: trial balance, balance sheet, cash-flow reconciliation, retained-earnings roll-forward and audit hold.
* Restart test: books and the closed-period state survive a WAL replay.

## Differences from the reference SQL (deliberate)
1. `vw_trial_balance`, `vw_income_statement`, `vw_balance_sheet` and `sp_generate_trial_balance` join `journal_entries` with
   `LEFT JOIN ... AND is_posted = 1`, which does not remove the lines of unposted entries. A draft Dr Cash 777 raised cash from 109,600 to 110,377
   in `vw_trial_balance` (reproduced). nexgraph has no drafts; an entry exists only once it is posted.
2. `sp_generate_trial_balance` clamps each balance with `GREATEST(..., 0)`. A write-off larger than the receivable left debits 217,700 against
   credits 207,700 (reproduced). nexgraph shows the abnormal balance on its side and warns in `gaap.audit`.
3. `vw_balance_sheet` has no retained-earnings line before closing, so it does not balance mid-year. nexgraph adds current income and dividends.
4. `sp_post_journal_entry` tolerates a 0.01 imbalance and lets a posted entry be posted into any period. nexgraph is exact and checks periods.

## v0.18 additions

### Ratio analysis: `CALL gaap.ratios(company, from, to)`
Columns `group, ratio, value, basis`. Flows come from the period's income statement, balances from the books at `to`.

| group | ratios |
|---|---|
| Profitability | `gross_margin`, `operating_margin`, `net_margin` (fractions: 0.25 = 25 %), `return_on_assets`, `return_on_equity` |
| Liquidity | `current_ratio`, `quick_ratio`, `cash_ratio`, `working_capital` (dollars) |
| Solvency | `debt_ratio`, `debt_to_equity`, `equity_multiplier` |
| Efficiency | `asset_turnover`, `receivables_turnover`, `days_sales_outstanding`, `inventory_turnover`, `days_inventory` |

* ROA, ROE and the turnovers use the **average** of the balance before `from` and the balance at `to`; with no opening balance (a first period) the ending balance is used.
* A ratio whose denominator is zero is `NULL`, never `NaN`/`inf`. Equity is assets minus liabilities.
* Account roles are found by **name and category**, so a custom chart works if its names are conventional: receivables = current-asset accounts (and contras) named
  `*receivable*` / `*doubtful*`; inventory = `*inventor*`; quick assets exclude inventory, `*prepaid*` and `*suppl*`; cash = accounts with `cash_flow = Cash`.
  Interest coverage and tax-rate ratios are not included (no standard account roles for them).

### Open items and aging: `gaap.open_items`, `gaap.aging`
```
CALL gaap.open_items('Acme', '110', '2024-06-30')   -- reference, date, days, amount, bucket
CALL gaap.aging('Acme', '110', '2024-06-30')        -- bucket (0-30, 31-60, 61-90, 91+), items, amount, TOTAL, Ledger balance, Difference
```
Works on any account; amounts are in the account's normal direction (a receivable's debit balance, a payable's credit balance). Entries are grouped by their `reference`
(use the invoice or bill number when posting). A reversing entry counts toward the reference of the entry it reverses. An entry without a reference is its own item,
`(entry N)`. Items netting to zero are settled and dropped; the age runs from the earliest entry that *increased* the balance. The summary ends with the ledger balance and the
difference, which is zero because every entry on the account lands in exactly one item. Historical `AS OF` queries work as for the other reports.
This is reference-matching on the general ledger, not a subledger: there are no customer or vendor records, credit terms or write-off workflow.

### Bulk loading
* `Ledger::post_many(company, &[Entry])`: one atomic transaction. Every entry is validated as in `post`; either all are posted with consecutive numbers or none is, and the error names
  the first failing entry (`entry 3: ...`). Closing and reversing entries are refused.
* `Ledger::import_journal_csv(company, csv)` (feature `ingest`): one row per line, rows with the same `entry` value form one entry. Columns `entry, date, account`, `debit` and/or `credit`;
  optional `journal, reference, description, period, memo`. All-or-nothing; errors name the entry and CSV line. Bare-`\r`, CRLF and LF files are all accepted.

## Not in this first pass
The reference schema has 212 tables. This release covers the financial-accounting core (chart of accounts, journals, periods, posting, adjusting,
reversing and closing entries, the four statements, ledger, audit). Not yet ported: subledgers with customer/vendor records and credit terms (reference-based aging exists since v0.18), inventory costing,
fixed-asset registers and depreciation schedules (only the monthly straight-line helper `straight_line_monthly`), payroll, leases, bonds, taxes,
managerial and cost accounting. Entry drafts and approval workflow, multi-currency translation, and an HTTP/UI surface
for posting (the library API is the write path; procedures are read-only) are also not included.
