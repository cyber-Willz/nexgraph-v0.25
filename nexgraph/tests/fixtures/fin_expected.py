#!/usr/bin/env python3
"""Reference for tests/fin.rs: pandas only. usage: python3 -I fin_expected.py sec_2010q1_balance_facts.txt > fin_expected.json"""
import json, sys, pandas as pd
n = pd.read_csv(sys.argv[1], sep="\t", dtype={"segments": str, "coreg": str}, low_memory=False)
b = n[(n.segments.isna()) & (n.coreg.isna()) & (n.uom == "USD")]
p = b.pivot_table(index=["adsh", "ddate"], columns="tag", values="value", aggfunc=lambda s: s.iloc[0] if s.nunique() == 1 else float("nan"))
amb = int((b.groupby(["adsh", "ddate", "tag"]).value.nunique() > 1).groupby(level=[0, 1]).any().sum())
def run(total, req, opt, tol):
    g = p[p[total].notna()]
    cols = [c for c in req]
    ok = g[cols].notna().all(axis=1)
    c = g[ok]
    s = sum(c[t] for t in req) + sum(c[t].fillna(0) if t in c else 0 for t in opt)
    gap = (c[total] - s)
    pct = gap.abs() / c[total].abs() * 100
    viol = pct[pct > tol].sort_values(ascending=False)
    return {"with_total": int(len(g)), "checked": int(len(c)), "violations": int(len(viol)), "incomplete": int((~ok).sum()), "worst": [[a, d, float(v)] for (a, d), v in viol.head(3).items()]}
out = {"ambiguous_groups": amb,
       "lse": run("Assets", ["LiabilitiesAndStockholdersEquity"], [], 0.01),
       "le": run("Assets", ["Liabilities", "StockholdersEquity"], [], 1.0),
       "le_opt": run("Assets", ["Liabilities", "StockholdersEquity"], ["MinorityInterest", "RedeemableNoncontrollingInterestEquityCarryingAmount", "TemporaryEquityCarryingAmountAttributableToParent"], 1.0)}
json.dump(out, sys.stdout, indent=1)
