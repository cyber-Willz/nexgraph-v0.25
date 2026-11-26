#!/usr/bin/env python3
"""Independent expected values for tests/land.rs `real_registry`, computed with pandas / numpy / scipy.

usage: python3 land_expected.py tn_land_registry_1000.csv expected.json
Nothing here calls nexgraph: quartiles are numpy's, p-values are scipy.stats.norm, Wilson intervals are
scipy.stats.binomtest(...).proportion_ci(method='wilson').
"""
import json, re, sys
import numpy as np, pandas as pd
from scipy import stats

df = pd.read_csv(sys.argv[1], dtype=str, keep_default_na=False)
df['area'] = df['Land Area (Acres)'].astype(float)
df['value'] = df['Market Value (INR)'].astype(float)
cases = []
Q = lambda s: "'" + s + "'"

def wil(k, n):
    ci = stats.binomtest(int(k), int(n)).proportion_ci(confidence_level=0.95, method='wilson')
    return ci.low, ci.high

def rule(name, cond_if, cond_then, grp, mask_if, mask_then, by=None):
    nql = f"CALL land.rule_check('PARCEL',{Q(cond_if)},{Q(cond_then)}" + (f",{Q(grp)}" if grp else "") + ") YIELD *"
    parts = [('ALL', df)] if by is None else [("/".join(k) if isinstance(k, tuple) else k, g) for k, g in df.groupby(by)]
    for key, g in parts:
        app = g[mask_if(g)]; viol = app[~mask_then(app)]
        lo, hi = wil(len(viol), len(app))
        cases.append(dict(nql=nql, where=dict(group=key), expect=dict(records=len(g), applicable=len(app), violations=len(viol), unknown=0,
                          violation_rate=len(viol) / len(app), wilson_low=lo, wilson_high=hi)))

rule('active', 'Record Status=Active', 'Tahsildar Approval Status=Approved', None, lambda g: g['Record Status'] == 'Active', lambda g: g['Tahsildar Approval Status'] == 'Approved')
rule('active', 'Record Status=Active', 'Tahsildar Approval Status=Approved', 'District', lambda g: g['Record Status'] == 'Active', lambda g: g['Tahsildar Approval Status'] == 'Approved', by='District')
rule('disp', 'Record Status=Under Dispute', 'Tahsildar Approval Status!=Approved', 'District', lambda g: g['Record Status'] == 'Under Dispute', lambda g: g['Tahsildar Approval Status'] != 'Approved', by='District')

def hot(group_cols, outcome_col, outcome_val, min_n):
    keys = [c for c in group_cols]
    sel = df
    p0 = (sel[outcome_col] == outcome_val).mean()
    grouped = [(("/".join(k) if isinstance(k, tuple) else k), g) for k, g in sel.groupby(keys if len(keys) > 1 else keys[0])]
    grouped = [(k, g) for k, g in grouped if len(g) >= min_n]
    tested = max(len(grouped), 1)
    nql = f"CALL land.hotspots('PARCEL',{Q(','.join(keys))},{Q(outcome_col + '=' + outcome_val)},null,{min_n}) YIELD *"
    for k, g in grouped:
        n = len(g); h = int((g[outcome_col] == outcome_val).sum())
        z = (h / n - p0) / np.sqrt(p0 * (1 - p0) / n); p = 2 * stats.norm.sf(abs(z)); lo, hi = wil(h, n)
        cases.append(dict(nql=nql, where=dict(group=k), expect=dict(records=n, hits=h, rate=h / n, overall_rate=p0, wilson_low=lo, wilson_high=hi, z=z, p_value=p)))
    return tested

t1 = hot(['Village Administrative Officer (VAO)'], 'Record Status', 'Under Dispute', 10)
t2 = hot(['District', 'Taluk'], 'Record Status', 'Under Dispute', 10)
t3 = hot(['Revenue Inspector'], 'Tahsildar Approval Status', 'Rejected', 10)

def val(grp):
    nql = "CALL land.valuation('PARCEL','Market Value (INR)','Land Area (Acres)'" + (f",{Q(grp)}" if grp else "") + ") YIELD *"
    parts = [('ALL', df)] if grp is None else list(df.groupby(grp))
    for k, g in parts:
        ppa = (g['value'] / g['area']).to_numpy(); lg = np.log(ppa)
        q1, q3 = np.quantile(lg, [0.25, 0.75]); iqr = q3 - q1; lo, hi = q1 - 1.5 * iqr, q3 + 1.5 * iqr
        cases.append(dict(nql=nql, where=dict(group=k), expect=dict(records=len(g), excluded=0, total_value=g['value'].sum(), total_area=g['area'].sum(),
            weighted_value_per_area=g['value'].sum() / g['area'].sum(), median_value_per_area=float(np.median(ppa)), p25=float(np.exp(q1)), p75=float(np.exp(q3)),
            fence_low=float(np.exp(lo)), fence_high=float(np.exp(hi)), outliers_low=int((lg < lo).sum()), outliers_high=int((lg > hi).sum()))))
val(None); val('District'); val('Usage Type')

def survey(grp):
    nql = "CALL land.survey_check('PARCEL','Survey Number','Sub Division'" + (f",{Q(grp)}" if grp else "") + ") YIELD *"
    parts = [('ALL', df)] if grp is None else list(df.groupby(grp))
    for k, g in parts:
        suf = g['Survey Number'].str.extract(r'^\d+[A-Za-z]?/(\d+)$')[0]
        has = suf.notna(); same = (suf[has].astype(int) == g.loc[has, 'Sub Division'].astype(int)).sum(); diff = int(has.sum()) - int(same)
        mal = int((~g['Survey Number'].str.fullmatch(r'\d+[A-Za-z]?(/\d+)?')).sum())
        cases.append(dict(nql=nql, where=dict(group=k), expect=dict(records=len(g), malformed=mal, without_subdivision=int((~has).sum()) - mal, suffix_matches=int(same), suffix_differs=diff, differs_rate=diff / (diff + int(same)))))
survey(None); survey('District')

json.dump(cases, open(sys.argv[2], 'w'), indent=1)
k4 = df.groupby(['District', 'Taluk', 'Village', 'Survey Number', 'Sub Division']).size()
k2 = df.groupby(['Village', 'Survey Number']).size()
print(json.dumps(dict(cases=len(cases), tested_vao=t1, tested_village=t2, tested_ri=t3,
    dup_qualified=int((k4 > 1).sum()), dup_unqualified=int((k2 > 1).sum()),
    distinct_village_unq=df['Village'].nunique(), distinct_village_q=len(df.groupby(['District', 'Taluk', 'Village'])),
    distinct_taluk_unq=df['Taluk'].nunique(), distinct_taluk_q=len(df.groupby(['District', 'Taluk'])), parcels=len(df))))
