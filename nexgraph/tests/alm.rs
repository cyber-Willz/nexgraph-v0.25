//! v0.11 regression tests: the `alm.*` procedures.
//!
//! Every expected number was computed independently in Python (plain arithmetic and numpy quantiles) from the
//! small tables below; the same checks run on the real ALM insurance dataset v2.1 in the last test, which needs
//! `NG_ALM_DATA` (the dataset root) and `NG_ALM_EXPECTED` (a JSON file produced by `tests/fixtures/alm_expected.py`).
#![cfg(feature = "ingest")]
use nexgraph::ingest::{import_csv_hyperedges_with, Dedup, HyperCsv, HyperOpts};
use nexgraph::{Database, Value};

const ASSETS: &str = "id,cls,mv,dur,conv\nA1,GOVT,1000,5,40\nA2,GOVT,500,10,120\nA3,CORP,300,4,25\nA4,EQUITY,200,0,0\nA5,CASH,100,0,0\n";
const LIAB: &str = "id,book,month,cf,df\nL1,TRAD,12,300,0.9803921568627451\nL2,TRAD,24,400,0.9518143961927426\nL3,TRAD,60,500,0.8419731668585242\nL4,TRAD,120,600,0.6755641688257986\nL5,UL,12,-50,0.9803921568627451\nL6,UL,36,250,0.9151416593531595\n";
const INFLOW: &str = "id,month,coupon,principal\nA1,6,10,0\nA1,12,10,1000\nA2,12,5,0\nA2,30,5,500\nA3,40,12,300\n";
const KRD: &str = "side,bucket,pv,krd_1p0y,krd_5p0y,krd_10p0y\nASSET,GOVT,1000,0.5,2,4\nASSET,CORP,500,0.2,1.5,2\nLIABILITY,X,-800,0.7,1.8,1.5\nLIABILITY,Y,-600,0.4,2.5,3\n";
const PATHS: &str = "scen,month,rate,eq\n1,0,0.03,100\n1,12,0.01,80\n2,0,0.03,100\n2,12,0.02,90\n3,0,0.03,100\n3,12,0.025,100\n4,0,0.03,100\n4,12,0.03,110\n5,0,0.03,100\n5,12,0.035,120\n6,0,0.03,100\n6,12,0.04,130\n7,0,0.03,100\n7,12,0.05,150\n8,0,0.03,100\n8,12,0.07,60\n";

fn load(db: &Database, label: &str, members: &[(&str, &str)], csv: &str) {
    let o = HyperCsv { edge_label: label, members, key_prop: "name", weight_col: None, time_cols: &[] };
    let r = import_csv_hyperedges_with(db, &o, csv, &HyperOpts { dedup: Dedup::None, keep_member_props: true, ..Default::default() }).unwrap();
    assert_eq!(r.rows_skipped, 0, "{label}: {:?}", r.first_error);
}

fn fixture() -> Database {
    let db = Database::in_memory();
    load(&db, "ASSET", &[("id", "Asset"), ("cls", "AssetClass")], ASSETS);
    load(&db, "LIAB", &[("id", "Liab"), ("book", "Book")], LIAB);
    load(&db, "INFLOW", &[("id", "Asset"), ("month", "Month")], INFLOW);
    load(&db, "KRD", &[("side", "Side"), ("bucket", "Bucket")], KRD);
    load(&db, "PATH", &[("scen", "Scenario"), ("month", "Month")], PATHS);
    db
}

fn call(db: &Database, q: &str) -> (Vec<String>, Vec<Vec<Value>>) {
    let r = db.query(q).unwrap_or_else(|e| panic!("{q}: {e}"));
    (r.columns, r.rows)
}

fn cell(cols: &[String], row: &[Value], c: &str) -> Value {
    row[cols.iter().position(|x| x == c).unwrap_or_else(|| panic!("no column {c} in {cols:?}"))].clone()
}

fn x(cols: &[String], row: &[Value], c: &str) -> f64 {
    cell(cols, row, c).as_f64().unwrap_or_else(|| panic!("{c} is {:?}", cell(cols, row, c)))
}

fn close(a: f64, b: f64, what: &str) {
    assert!((a - b).abs() <= 1e-9 * b.abs().max(1.0), "{what}: got {a}, want {b}");
}

const DG: &str = "CALL alm.duration_gap('ASSET','mv','dur','conv','LIAB','cf','df','month',null,'book=TRAD') YIELD *";

#[test]
fn position_groups_by_class_with_weighted_duration_and_concentration() {
    let db = fixture();
    let (c, rows) = call(&db, "CALL alm.position('ASSET','mv','dur','conv','AssetClass') YIELD * RETURN group, holdings, market_value, share, mod_duration, convexity, dv01, dur_coverage, largest_holding_share, hhi");
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0][0].as_str(), Some("GOVT"), "largest group first");
    for (g, mv, d, cv, dv, big, hhi) in [
        ("GOVT", 1500.0, 6.666666666666667, 66.66666666666667, 1.0, 0.6666666666666666, 0.5555555555555556),
        ("CORP", 300.0, 4.0, 25.0, 0.12000000000000001, 1.0, 1.0),
        ("EQUITY", 200.0, 0.0, 0.0, 0.0, 1.0, 1.0),
    ] {
        let r = rows.iter().find(|r| r[0].as_str() == Some(g)).unwrap();
        close(x(&c, r, "market_value"), mv, g);
        close(x(&c, r, "mod_duration"), d, g);
        close(x(&c, r, "convexity"), cv, g);
        close(x(&c, r, "dv01"), dv, g);
        close(x(&c, r, "largest_holding_share"), big, g);
        close(x(&c, r, "hhi"), hhi, g);
        close(x(&c, r, "dur_coverage"), 1.0, g);
    }
    close(rows.iter().map(|r| x(&c, r, "share")).sum::<f64>(), 1.0, "shares sum to 1");
    // filter: only government bonds, no grouping
    let (c, rows) = call(&db, "CALL alm.position('ASSET','mv','dur',null,null,'cls=GOVT') YIELD * RETURN group, market_value, mod_duration");
    assert_eq!(rows.len(), 1);
    close(x(&c, &rows[0], "market_value"), 1500.0, "filtered mv");
}

#[test]
fn liability_profile_matches_independent_present_value_duration_and_runoff() {
    let db = fixture();
    let (c, rows) = call(&db, "CALL alm.liability_profile('LIAB','cf','df','month') YIELD * RETURN group, cashflows, dropped, pv, mod_duration, convexity, dv01, implied_yield, half_life_years, pv_share_5y, pv_share_10y");
    assert_eq!(rows.len(), 1);
    let r = &rows[0];
    close(x(&c, r, "pv"), 1680.9342972558145, "pv");
    close(x(&c, r, "mod_duration"), 4.509857537267778, "duration");
    close(x(&c, r, "convexity"), 34.65128818275443, "convexity");
    close(x(&c, r, "dv01"), 0.758077421013105, "dv01");
    close(x(&c, r, "half_life_years"), 3.0, "half life");
    close(x(&c, r, "pv_share_5y"), 0.7588611869260989, "share 5y");
    close(x(&c, r, "pv_share_10y"), 1.0, "share 10y");
    close(x(&c, r, "implied_yield"), 0.031073039409394747, "implied yield");
    // by book, with a filter: the traditional book alone
    let (c, rows) = call(&db, "CALL alm.liability_profile('LIAB','cf','df','month','book','book=TRAD') YIELD * RETURN group, pv, mod_duration, convexity");
    assert_eq!(rows.len(), 1);
    close(x(&c, &rows[0], "pv"), 1501.1684902606619, "trad pv");
    close(x(&c, &rows[0], "mod_duration"), 4.638031912092177, "trad duration");
    close(x(&c, &rows[0], "convexity"), 37.13969817012901, "trad convexity");
}

#[test]
fn duration_gap_funding_ratio_and_net_sensitivities() {
    let db = fixture();
    let (c, rows) = call(&db, DG);
    let r = &rows[0];
    close(x(&c, r, "assets"), 2100.0, "assets");
    close(x(&c, r, "liabilities"), 1501.1684902606619, "liabilities");
    close(x(&c, r, "surplus"), 598.8315097393381, "surplus");
    close(x(&c, r, "funding_ratio"), 1.3989102579919976, "funding ratio");
    close(x(&c, r, "asset_duration"), 5.333333333333333, "asset duration");
    close(x(&c, r, "liability_duration"), 4.638031912092177, "liability duration");
    close(x(&c, r, "duration_gap"), 2.017872684163722, "leverage-adjusted gap");
    close(x(&c, r, "asset_dv01"), 1.12, "asset dv01");
    close(x(&c, r, "liability_dv01"), 0.6962467363256184, "liability dv01");
    close(x(&c, r, "net_dv01"), 0.4237532636743817, "net dv01");
    close(x(&c, r, "net_dollar_convexity"), 51747.05536921077, "net convexity");
    assert_eq!(cell(&c, r, "warning"), Value::Null);
}

#[test]
fn mismatched_books_are_flagged_not_hidden() {
    let db = fixture();
    // Assets of 2,100 against all liabilities of ~1,681 is fine; against a tiny book it is flagged.
    let (c, rows) = call(&db, "CALL alm.duration_gap('ASSET','mv','dur','conv','LIAB','cf','df','month','cls=CASH','book=TRAD') YIELD *");
    let w = cell(&c, &rows[0], "warning");
    assert!(w.as_str().is_some_and(|s| s.contains("assets are") && s.contains("same obligations")), "{w:?}");
    // a selection that contains no liabilities is an error, not an empty row
    assert!(db.query("CALL alm.duration_gap('ASSET','mv','dur','conv','LIAB','cf','df','month',null,'book=NOPE') YIELD *").is_err());
    // a key that exists on no event names what the events do carry
    let e = db.query("CALL alm.duration_gap('ASSET','market_value','dur','conv','LIAB','cf','df','month') YIELD *").unwrap_err().to_string();
    assert!(e.contains("market_value") && e.contains("first event has properties"), "{e}");
}

#[test]
fn rate_shock_reprices_liabilities_exactly_and_assets_by_duration_convexity() {
    let db = fixture();
    let (c, rows) = call(&db, "CALL alm.rate_shock('ASSET','mv','dur','conv','LIAB','cf','df','month',[-100, 0, 100, 250],null,'book=TRAD') YIELD *");
    assert_eq!(rows.len(), 4);
    for (bp, (a, l, s, da, dl)) in [(-100.0, (2217.375, 1573.6778951719468, 643.6971048280532, 117.375, 72.50940491128495)), (0.0, (2100.0, 1501.1684902606619, 598.8315097393381, 0.0, 0.0)), (100.0, (1993.375, 1434.2398914436887, 559.1351085563113, -106.625, -66.92859881697314)), (250.0, (1853.59375, 1343.1578388414848, 510.4359111585152, -246.40625, -158.01065141917707))] {
        let r = rows.iter().find(|r| r[0].as_f64() == Some(bp)).unwrap();
        close(x(&c, r, "assets"), a, "assets");
        close(x(&c, r, "liabilities"), l, "liabilities");
        close(x(&c, r, "surplus"), s, "surplus");
        close(x(&c, r, "d_assets"), da, "d_assets");
        close(x(&c, r, "d_liabilities"), dl, "d_liabilities");
        close(x(&c, r, "d_surplus"), s - 598.8315097393381, "d_surplus");
    }
    // default shock set
    let (_, rows) = call(&db, "CALL alm.rate_shock('ASSET','mv','dur','conv','LIAB','cf','df','month',null,null,'book=TRAD') YIELD *");
    assert_eq!(rows.len(), 7);
    // a shift that takes a yield to -100% is reported, not computed
    let (c, rows) = call(&db, "CALL alm.rate_shock('ASSET','mv','dur','conv','LIAB','cf','df','month',[-12000],null,'book=TRAD') YIELD *");
    assert!(cell(&c, &rows[0], "warning").as_str().unwrap().contains("-100%"));
    assert_eq!(cell(&c, &rows[0], "liabilities"), Value::Null);
}

#[test]
fn rate_shock_can_reprice_bonds_exactly_from_their_cash_flows() {
    // INFLOW holds the bond cash flows of A1..A3 (coupon + principal). Their discount factors come from a flat 3 % curve.
    let db = fixture();
    let mut csv = String::from("id,month,cf,df\n");
    let flows = [("A1", 12, 1050.0), ("A1", 24, 1050.0), ("A2", 36, 600.0), ("A3", 60, 360.0)];
    for (id, m, cf) in flows {
        csv += &format!("{id},{m},{cf},{}\n", 1.03f64.powf(-(m as f64) / 12.0));
    }
    load(&db, "BONDCF", &[("id", "Asset"), ("month", "Month")], &csv);
    let (c, rows) = call(&db, "CALL alm.rate_shock('ASSET','mv','dur','conv','LIAB','cf','df','month',[200],null,'book=TRAD','BONDCF',['cf'],'df','month') YIELD *");
    let r = &rows[0];
    let exact: f64 = flows.iter().map(|(_, m, cf)| cf * (1.05f64.powf(-(*m as f64) / 12.0) - 1.03f64.powf(-(*m as f64) / 12.0))).sum();
    close(x(&c, r, "d_assets"), exact, "exact bond P&L");
    close(x(&c, r, "d_assets_approx"), -(1000.0 * 5.0 + 500.0 * 10.0 + 300.0 * 4.0) * 0.02 + 0.5 * (1000.0 * 40.0 + 500.0 * 120.0 + 300.0 * 25.0) * 0.0004, "approx");
    assert_eq!(cell(&c, r, "asset_method").as_str(), Some("cash-flow repricing"));
    // a filter on the bond selection works like the others
    let (c, rows) = call(&db, "CALL alm.rate_shock('ASSET','mv','dur','conv','LIAB','cf','df','month',[200],null,'book=TRAD','BONDCF',['cf'],'df','month','id=A1') YIELD *");
    let only_a1: f64 = flows.iter().filter(|f| f.0 == "A1").map(|(_, m, cf)| cf * (1.05f64.powf(-(*m as f64) / 12.0) - 1.03f64.powf(-(*m as f64) / 12.0))).sum();
    close(x(&c, &rows[0], "d_assets"), only_a1, "filtered bond selection");
}

#[test]
fn cashflow_ladder_buckets_cumulates_and_flags_shortfalls() {
    let db = fixture();
    let (c, rows) = call(&db, "CALL alm.cashflow_ladder('INFLOW','month',['coupon','principal'],'LIAB','month','cf',12,null,'book=TRAD') YIELD *");
    let want: Vec<(&str, f64, f64, f64, f64, Option<f64>, i64, i64)> = vec![
        ("m0-11", 10.0, 0.0, 10.0, 10.0, None, 0, 0),
        ("m12-23", 1015.0, 300.0, 715.0, 725.0, Some(3.3833333333333333), 0, 0),
        ("m24-35", 505.0, 400.0, 105.0, 830.0, Some(1.2625), 0, 0),
        ("m36-47", 312.0, 0.0, 312.0, 1142.0, None, 0, 0),
        ("m60-71", 0.0, 500.0, -500.0, 642.0, Some(0.0), 1, 0),
        ("m120-131", 0.0, 600.0, -600.0, 42.0, Some(0.0), 1, 0),
    ];
    assert_eq!(rows.len(), want.len());
    for (r, w) in rows.iter().zip(&want) {
        assert_eq!(cell(&c, r, "bucket").as_str(), Some(w.0));
        close(x(&c, r, "inflow"), w.1, w.0);
        close(x(&c, r, "outflow"), w.2, w.0);
        close(x(&c, r, "net"), w.3, w.0);
        close(x(&c, r, "cum_net"), w.4, w.0);
        match w.5 { Some(v) => close(x(&c, r, "cover_ratio"), v, w.0), None => assert_eq!(cell(&c, r, "cover_ratio"), Value::Null) }
        assert_eq!(cell(&c, r, "shortfall"), Value::Int(w.6), "{}", w.0);
        assert_eq!(cell(&c, r, "cum_shortfall"), Value::Int(w.7), "{}", w.0);
    }
    assert!(db.query("CALL alm.cashflow_ladder('INFLOW','month',['coupon'],'LIAB','month','cf',0) YIELD *").is_err());
}

#[test]
fn krd_gap_sums_key_rate_dv01_by_side() {
    let db = fixture();
    let (c, rows) = call(&db, "CALL alm.krd_gap('KRD','side','pv',['krd_10p0y','krd_1p0y','krd_5p0y']) YIELD *");
    assert_eq!(rows.len(), 3);
    let want = [("krd_1p0y", 1.0, 0.060000000000000005, 0.08), ("krd_5p0y", 5.0, 0.275, 0.29400000000000004), ("krd_10p0y", 10.0, 0.5, 0.30000000000000004)];
    for (i, (k, t, a, l)) in want.iter().enumerate() {
        assert_eq!(cell(&c, &rows[i], "key").as_str(), Some(*k), "sorted by tenor");
        close(x(&c, &rows[i], "tenor_years"), *t, k);
        close(x(&c, &rows[i], "asset_dv01"), *a, k);
        close(x(&c, &rows[i], "liability_dv01"), *l, k);
        close(x(&c, &rows[i], "net_dv01"), a - l, k);
    }
    assert!(db.query("CALL alm.krd_gap('KRD','side','pv',['krd_1p0y'],'ASSETS') YIELD *").is_err(), "unknown side name is an error");
}

#[test]
fn scenario_risk_distribution_var_and_cte() {
    let db = fixture();
    let (c, rows) = call(&db, "CALL alm.scenario_risk('PATH','scen','month','rate',12,0,'change',-50000,2000000,0.75) YIELD *");
    let r = &rows[0];
    assert_eq!(cell(&c, r, "scenarios"), Value::Int(8));
    for (k, v) in [("change_mean", 0.005000000000000003), ("change_stdev", 0.01870828693386971), ("change_min", -0.019999999999999997), ("change_p005", -0.019649999999999997), ("change_p05", -0.016499999999999997), ("change_p50", 0.0025000000000000022), ("change_p95", 0.032999999999999995), ("change_p995", 0.0393), ("change_max", 0.04000000000000001), ("pnl_mean", 81.24999999999994), ("pnl_var", 399.99999999999983), ("pnl_cte", 500.0), ("prob_loss", 0.5)] {
        close(x(&c, r, k), v, k);
    }
    // relative mode for an index level
    let (c, rows) = call(&db, "CALL alm.scenario_risk('PATH','scen','month','eq',12,0,'relative') YIELD *");
    close(x(&c, &rows[0], "change_mean"), 0.05000000000000002, "relative mean");
    close(x(&c, &rows[0], "change_min"), -0.4, "relative min");
    close(x(&c, &rows[0], "change_max"), 0.5, "relative max");
    assert_eq!(cell(&c, &rows[0], "pnl_var"), Value::Null);
    assert!(db.query("CALL alm.scenario_risk('PATH','scen','month','rate',99) YIELD *").is_err(), "month 99 exists in no scenario");
    assert!(db.query("CALL alm.scenario_risk('PATH','scen','month','rate',12,0,'bogus') YIELD *").is_err());
}

#[test]
fn killchain_rejects_a_mostly_zero_amount_used_as_an_outcome() {
    // Found by the ALM dataset: a continuous column that is mostly 0 passed the binary test and produced a
    // meaningless "rate". It is now refused with the count of values that are neither 0 nor 1.
    let db = Database::in_memory();
    let mut csv = String::from("id,grp,amount\n");
    for i in 0..20 {
        csv += &format!("{i},{},{}\n", if i % 2 == 0 { "a" } else { "b" }, if i < 14 { 0 } else { 7 });
    }
    load(&db, "E", &[("id", "Id"), ("grp", "Grp")], &csv);
    let e = db.query("CALL killchain.exposure('E','Grp','amount','amount') YIELD *").unwrap_err().to_string();
    assert!(e.contains("neither 0 nor 1") && e.contains("only") && e.contains("Key!=Value"), "{e}");
    // a genuine 0/1 flag still works
    let mut csv = String::from("id,grp,flag,amount\n");
    for i in 0..20 {
        csv += &format!("{i},{},{},{}\n", if i % 2 == 0 { "a" } else { "b" }, i % 3 / 2, i * 10);
    }
    let db = Database::in_memory();
    load(&db, "E", &[("id", "Id"), ("grp", "Grp")], &csv);
    assert!(db.query("CALL killchain.exposure('E','Grp','flag','amount') YIELD *").is_ok());
}

/// Replays `tests/fixtures/alm_expected.py` on the real ALM insurance dataset v2.1 (skipped without the env vars).
#[test]
fn real_dataset() {
    let (Ok(root), Ok(exp)) = (std::env::var("NG_ALM_DATA"), std::env::var("NG_ALM_EXPECTED")) else {
        eprintln!("skipped: set NG_ALM_DATA and NG_ALM_EXPECTED");
        return;
    };
    let db = Database::in_memory();
    let t0 = std::time::Instant::now();
    for (file, label, members) in [
        ("assets.csv", "ASSET", vec![("asset_id", "Asset"), ("asset_class", "AssetClass"), ("issuer_or_country", "Issuer"), ("nace", "NACE"), ("rating_bucket", "Rating"), ("liquidity_tier", "Tier")]),
        ("asset_cashflows.csv", "ASSETCF", vec![("asset_id", "Asset"), ("month", "Month"), ("asset_class", "AssetClass"), ("rating_bucket", "Rating")]),
        ("liability_cashflows.csv", "LIABCF", vec![("cohort", "Cohort"), ("product_code", "Product"), ("month", "Month"), ("book", "Book")]),
        ("cashflow_gap_monthly.csv", "GAPM", vec![("portfolio_id", "Portfolio"), ("month", "Month")]),
        ("key_rate_durations.csv", "KRD", vec![("side", "Side"), ("bucket", "Bucket"), ("book", "Book")]),
        ("scenarios_stochastic_sample.csv", "PATH", vec![("scenario_id", "Scenario"), ("month", "Month")]),
    ] {
        let csv = std::fs::read_to_string(format!("{root}/extended/{file}")).unwrap();
        // timestamp columns are not needed here and would only exercise the importer
        load(&db, label, &members, &csv);
    }
    eprintln!("loaded in {:.1}s", t0.elapsed().as_secs_f64());
    let cases: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(exp).unwrap()).unwrap();
    let mut n = 0;
    for c in cases.as_array().unwrap() {
        let q = c["nql"].as_str().unwrap();
        let t = std::time::Instant::now();
        let (cols, rows) = call(&db, q);
        let ms = t.elapsed().as_millis();
        let want_where = c["where"].as_object().unwrap();
        let row = rows
            .iter()
            .find(|r| {
                want_where.iter().all(|(k, v)| {
                    let got = cell(&cols, r, k);
                    match (v.as_f64(), v.as_str()) {
                        (Some(f), _) => got.as_f64() == Some(f),
                        (_, Some(s)) => got.as_str() == Some(s),
                        _ => false,
                    }
                })
            })
            .unwrap_or_else(|| panic!("no row {want_where:?} in {q}"));
        for (k, v) in c["expect"].as_object().unwrap() {
            let got = cell(&cols, row, k);
            match v {
                serde_json::Value::Null => assert_eq!(got, Value::Null, "{k} in {q}"),
                serde_json::Value::String(s) if s == "WARN" => assert!(got.as_str().is_some(), "{k} should carry a warning in {q}"),
                serde_json::Value::String(s) => assert_eq!(got.as_str(), Some(s.as_str()), "{k} in {q}"),
                other => close(got.as_f64().unwrap_or_else(|| panic!("{k} is {got:?} in {q}")), other.as_f64().unwrap(), &format!("{k} ({where_:?}) in {q} [{ms} ms]", where_ = want_where)),
            }
        }
        n += 1;
        if ms > 500 {
            eprintln!("slow: {ms} ms  {q}");
        }
    }
    eprintln!("{n} cases matched pandas to 1e-9 relative");
    assert!(n >= 90);
}
