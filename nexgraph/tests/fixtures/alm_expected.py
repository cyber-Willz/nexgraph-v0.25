#!/usr/bin/env python3
"""Independent (pandas/numpy) expectations for the alm.* procedures on the ALM insurance dataset v2.1.

usage: alm_expected.py DATA_ROOT OUT.json
DATA_ROOT holds extended/ (part 1). Run the engine side with
  NG_ALM_DATA=DATA_ROOT NG_ALM_EXPECTED=OUT.json cargo test --release --test alm real_dataset -- --nocapture
"""
import json, math, sys
import numpy as np
import pandas as pd

root, out = sys.argv[1:3]
E = root.rstrip('/') + '/extended/'
cases = []


def case(nql, expect, where=None):
    cases.append(dict(nql=nql, where=where or {}, expect={k: (None if isinstance(v, float) and not math.isfinite(v) else v) for k, v in expect.items()}))


def liab_stats(df, cf, dfc, month):
    t = df[month] / 12.0
    d = df[dfc]
    y = np.where(t > 0, d ** (-1.0 / t.where(t > 0, 1.0)) - 1.0, 0.0)
    pv_i = df[cf] * d
    pv = pv_i.sum()
    D = (pv_i * t / (1 + y)).sum() / pv
    C = (pv_i * t * (t + 1) / (1 + y) ** 2).sum() / pv
    return dict(pv=pv, mod_duration=D, convexity=C, dv01=pv * D * 1e-4, y=y, t=t, pv_i=pv_i)


def reprice(df, cf, dfc, month, d):
    s = liab_stats(df, cf, dfc, month)
    t, y = s['t'], s['y']
    # t == 0 (paid within the first month): the stated discount factor is kept and no shift applies
    return float((df[cf] * np.where(t > 0, (1 + y + d) ** (-t), df[dfc])).sum())


# ------------------------------------------------------------- assets
a = pd.read_csv(E + 'assets.csv', keep_default_na=False, na_values=[''])
for cls, g in a.groupby('asset_class'):
    mv = g.market_value.sum()
    case("CALL alm.position('ASSET','market_value','mod_duration','convexity','asset_class') YIELD * RETURN group, holdings, market_value, mod_duration, convexity, dv01, largest_holding_share, hhi",
         dict(holdings=int(len(g)), market_value=float(mv), mod_duration=float((g.market_value * g.mod_duration).sum() / mv), convexity=float((g.market_value * g.convexity).sum() / mv),
              dv01=float((g.market_value * g.mod_duration).sum() * 1e-4), largest_holding_share=float(g.market_value.max() / mv), hhi=float((g.market_value ** 2).sum() / mv ** 2)),
         dict(group=cls))
bonds = a[a.asset_class.isin(['GOVT', 'CORP'])]
case("CALL alm.position('ASSET','market_value','mod_duration',null,'rating_bucket','asset_class!=EQUITY') YIELD * RETURN group, market_value, mod_duration",
     dict(market_value=float(bonds[bonds.rating_bucket == 'AA'].market_value.sum()),
          mod_duration=float((bonds[bonds.rating_bucket == 'AA'].market_value * bonds[bonds.rating_bucket == 'AA'].mod_duration).sum() / bonds[bonds.rating_bucket == 'AA'].market_value.sum())), dict(group='AA'))

# --------------------------------------------------- traditional book
l = pd.read_csv(E + 'liability_cashflows.csv')
tr = l[l.book == 'TRADITIONAL']
s = liab_stats(tr, 'net_outflow_cf', 'discount_factor', 'month')
case("CALL alm.liability_profile('LIABCF','net_outflow_cf','discount_factor','month',null,'book=TRADITIONAL') YIELD * RETURN group, cashflows, pv, mod_duration, convexity, dv01",
     dict(cashflows=int(len(tr)), pv=float(s['pv']), mod_duration=float(s['mod_duration']), convexity=float(s['convexity']), dv01=float(s['dv01'])), dict(group='ALL'))
# the dataset's own reconciliation figure: PV of traditional net outflows with their discount factors
rec = pd.read_csv(E + 'books_reconciliation.csv')
rec_pv = float(rec[rec['item'] == 'traditional_pv_of_net_outflows'].value.iloc[0])
assert abs(s['pv'] - rec_pv) <= 1e-6 * rec_pv, (s['pv'], rec_pv)
for prod, g in tr.groupby('product_code'):
    sg = liab_stats(g, 'net_outflow_cf', 'discount_factor', 'month')
    pos_ = sg['pv'] > 0   # a net-inflow product has no duration or convexity, but still a signed DV01
    case("CALL alm.liability_profile('LIABCF','net_outflow_cf','discount_factor','month','product_code','book=TRADITIONAL') YIELD * RETURN group, pv, mod_duration, convexity, dv01",
         dict(pv=float(sg['pv']), mod_duration=float(sg['mod_duration']) if pos_ else None, convexity=float(sg['convexity']) if pos_ else None,
              dv01=float((sg['pv_i'] * sg['t'] / (1 + sg['y'])).sum() * 1e-4)), dict(group=prod))
    print(prod, 'pv', round(sg['pv'] / 1e6, 1), 'M', 'duration', round(sg['mod_duration'], 3) if pos_ else 'n/a (net inflows)')

# ---- assets (all) against the traditional book: must warn (funding ratio 0.26)
A = a.market_value.sum(); d1 = (a.market_value * a.mod_duration).sum(); c1 = (a.market_value * a.convexity).sum()
pv = s['pv']; D = s['mod_duration']; C = s['convexity']
case("CALL alm.duration_gap('ASSET','market_value','mod_duration','convexity','LIABCF','net_outflow_cf','discount_factor','month',null,'book=TRADITIONAL') YIELD *",
     dict(assets=float(A), liabilities=float(pv), surplus=float(A - pv), funding_ratio=float(A / pv), asset_duration=float(d1 / A), liability_duration=float(D), duration_gap=float(d1 / A - pv / A * D),
          asset_dv01=float(d1 * 1e-4), liability_dv01=float(pv * D * 1e-4), net_dv01=float(d1 * 1e-4 - pv * D * 1e-4), net_dollar_convexity=float(c1 - pv * C), warning='WARN'))

# ---- assets against the unit-linked outflows in the cash-flow gap table (the book the assets are sized against)
g = pd.read_csv(E + 'cashflow_gap_monthly.csv')
ul_pv = float((g.unit_linked_outflow * g.discount_factor).sum())
recul = float(rec[rec['item'] == 'unit_linked_pv_of_cash_flows'].value.iloc[0])
print(f'unit-linked PV from cashflow_gap_monthly {ul_pv:,.0f} vs books_reconciliation {recul:,.0f} ({ul_pv / recul - 1:+.3%}; month-end discounting vs OSEM dates)')
assert abs(ul_pv - recul) <= 0.01 * recul, (ul_pv, recul)
su = liab_stats(g, 'unit_linked_outflow', 'discount_factor', 'month')
case("CALL alm.duration_gap('ASSET','market_value','mod_duration','convexity','GAPM','unit_linked_outflow','discount_factor','month') YIELD *",
     dict(assets=float(A), liabilities=float(su['pv']), funding_ratio=float(A / su['pv']), liability_duration=float(su['mod_duration']), duration_gap=float(d1 / A - su['pv'] / A * su['mod_duration']),
          net_dv01=float(d1 * 1e-4 - su['pv'] * su['mod_duration'] * 1e-4), warning=None))
for bp in (-200, -100, 100, 200):
    d = bp * 1e-4
    dA = -d1 * d + 0.5 * c1 * d * d
    L = reprice(g, 'unit_linked_outflow', 'discount_factor', 'month', d)
    case("CALL alm.rate_shock('ASSET','market_value','mod_duration','convexity','GAPM','unit_linked_outflow','discount_factor','month',[-200,-100,100,200]) YIELD *",
         dict(assets=float(A + dA), liabilities=L, surplus=float(A + dA - L), d_assets=float(dA), d_liabilities=float(L - su['pv']), d_surplus=float((A + dA - L) - (A - su['pv']))), dict(shock_bp=float(bp)))

# exact bond repricing from ASSETCF (GOVT + CORP; equities and cash stay rate-insensitive)
cfb = pd.read_csv(E + 'asset_cashflows.csv')
cfb = cfb[cfb.asset_class != 'EQUITY'].copy()
cfb['cf'] = cfb.coupon_or_dividend_cf + cfb.principal_cf
cfb['t'] = cfb.month / 12.0
cfb['y'] = np.where(cfb.t > 0, cfb.discount_factor ** (-1.0 / cfb.t.where(cfb.t > 0, 1.0)) - 1.0, 0.0)
base_b = float((cfb.cf * cfb.discount_factor).sum())
print('bond PV from cash flows', round(base_b / 1e6, 3), 'M vs bond market value', round(bonds.market_value.sum() / 1e6, 3), 'M')
for bp in (-300, -100, 100, 300):
    d = bp * 1e-4
    ex = float((cfb.cf * np.where(cfb.t > 0, (1 + cfb.y + d) ** (-cfb.t), cfb.discount_factor)).sum() - base_b)
    approx = -d1 * d + 0.5 * c1 * d * d
    L = reprice(g, 'unit_linked_outflow', 'discount_factor', 'month', d)
    case("CALL alm.rate_shock('ASSET','market_value','mod_duration','convexity','GAPM','unit_linked_outflow','discount_factor','month',[-300,-100,100,300],null,null,'ASSETCF',['coupon_or_dividend_cf','principal_cf'],'discount_factor','month','asset_class!=EQUITY') YIELD *",
         dict(d_assets=ex, d_assets_approx=float(approx), assets=float(A + ex), liabilities=L, d_surplus=float((A + ex - L) - (A - su['pv'])), asset_method='cash-flow repricing'), dict(shock_bp=float(bp)))
    print(f'{bp:+d} bp bond P&L exact {ex/1e6:,.2f}M  duration+convexity {approx/1e6:,.2f}M  ({(approx-ex)/abs(ex):+.2%})')

# ------------------------------------------------------- cash flows
ac = pd.read_csv(E + 'asset_cashflows.csv')
ac['cf'] = ac.coupon_or_dividend_cf + ac.principal_cf
inflow = ac.groupby(ac.month // 12).cf.sum()
outflow = tr.groupby(tr.month // 12).net_outflow_cf.sum()
idx = sorted(set(inflow.index) | set(outflow.index))
cum = 0.0
for i in idx:
    i_, o_ = float(inflow.get(i, 0.0)), float(outflow.get(i, 0.0))
    cum += i_ - o_
    case("CALL alm.cashflow_ladder('ASSETCF','month',['coupon_or_dividend_cf','principal_cf'],'LIABCF','month','net_outflow_cf',12,null,'book=TRADITIONAL') YIELD *",
         dict(inflow=i_, outflow=o_, net=i_ - o_, cum_net=cum, shortfall=int(i_ - o_ < 0), cum_shortfall=int(cum < 0)), dict(start_month=i * 12))
# monthly ladder from the gap table itself
gi = g.groupby(g.month // 12)[['asset_cf_in', 'unit_linked_outflow']].sum()
cum = 0.0
for i, r in gi.iterrows():
    cum += r.asset_cf_in - r.unit_linked_outflow
    case("CALL alm.cashflow_ladder('GAPM','month',['asset_cf_in'],'GAPM','month','unit_linked_outflow',12) YIELD *",
         dict(inflow=float(r.asset_cf_in), outflow=float(r.unit_linked_outflow), cum_net=float(cum), cum_shortfall=int(cum < 0)), dict(start_month=int(i) * 12))

# ----------------------------------------------------------- key rates
k = pd.read_csv(E + 'key_rate_durations.csv')
keys = [c for c in k.columns if c.startswith('krd_')]
for c in keys:
    av = float((k[k.side == 'ASSET'].pv.abs() * k[k.side == 'ASSET'][c]).sum() * 1e-4)
    lv = float((k[k.side == 'LIABILITY'].pv.abs() * k[k.side == 'LIABILITY'][c]).sum() * 1e-4)
    case("CALL alm.krd_gap('KRD','side','pv',[" + ','.join(f"'{x}'" for x in keys) + "]) YIELD *",
         dict(asset_dv01=av, liability_dv01=lv, net_dv01=av - lv, tenor_years=float(c[4:-1].replace('p', '.'))), dict(key=c))

# --------------------------------------------------------- scenarios
p = pd.read_csv(E + 'scenarios_stochastic_sample.csv')
for col, mode, month in (('short_rate', 'change', 60), ('equity_index', 'relative', 60), ('credit_spread_ig', 'change', 24)):
    b = p[p.month == 0].set_index('scenario_id')[col]
    h = p[p.month == month].set_index('scenario_id')[col]
    ch = (h - b) if mode == 'change' else (h / b - 1.0)
    s_ = np.sort(ch.values)
    q = lambda x: float(np.quantile(s_, x))
    exp = dict(scenarios=int(len(ch)), change_mean=float(ch.mean()), change_stdev=float(ch.std(ddof=1)), change_min=float(s_[0]), change_p005=q(.005), change_p05=q(.05), change_p50=q(.5), change_p95=q(.95), change_p995=q(.995), change_max=float(s_[-1]))
    e1, e2, alpha = None, None, 0.995
    nql = f"CALL alm.scenario_risk('PATH','scenario_id','month','{col}',{month},0,'{mode}'"
    if col == 'short_rate':
        e1, e2 = -float(d1 * 1e-4 - su['pv'] * su['mod_duration'] * 1e-4) * 1e4, float(c1 - su['pv'] * su['convexity'])
        pnl = e1 * ch.values + 0.5 * e2 * ch.values ** 2
        sp = np.sort(pnl); kk = max(1, math.ceil((1 - alpha) * len(sp)))
        exp.update(pnl_mean=float(pnl.mean()), pnl_var=float(-np.quantile(sp, 1 - alpha)), pnl_cte=float(-sp[:kk].mean()), prob_loss=float((pnl < 0).mean()))
        nql += f",{e1!r},{e2!r},{alpha}"
    case(nql + ") YIELD *", exp)

json.dump(cases, open(out, 'w'), indent=1)
print(len(cases), 'cases written to', out)
