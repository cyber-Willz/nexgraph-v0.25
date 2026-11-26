#!/usr/bin/env python3
"""Reference values for tests/uw.rs, computed with numpy / scipy / sklearn / statsmodels only (no nexgraph code).
usage: python3 -I uw_expected.py <fixtures dir> > uw_expected.json"""
import json, sys, numpy as np, pandas as pd
from scipy import stats
from sklearn.linear_model import LogisticRegression
from sklearn.metrics import roc_auc_score, log_loss, brier_score_loss
import statsmodels.api as sm
D = sys.argv[1]
M = (1 << 64) - 1
def fnv(s):
    h = 0xcbf29ce484222325
    for b in s.encode():
        h = ((h ^ b) * 0x100000001b3) & M
    return h
def delong(y, s):
    pos, neg = s[y == 1], s[y == 0]
    psi = (pos[:, None] > neg[None, :]) + 0.5 * (pos[:, None] == neg[None, :])
    auc = psi.mean(); v10 = psi.mean(1); v01 = psi.mean(0)
    se = np.sqrt(v10.var(ddof=1) / len(pos) + v01.var(ddof=1) / len(neg))
    return auc, se, max(0, auc - 1.959963984540054 * se), min(1, auc + 1.959963984540054 * se)
out = {}
# ---------------------------------------------------------------- scorecard on the LendingClub fixture
d = pd.read_csv(f"{D}/lending_club_loans.csv")
num = ["fico", "int.rate", "inq.last.6mths", "dti", "revol.util", "log.annual.inc"]
y = d["not.fully.paid"].values.astype(float)
test = np.array([(fnv(str(i)) % 100) < 30 for i in range(len(d))])
tr = ~test
def design(cols_levels):
    X, names = [], []
    for c in num:
        m, sd = d.loc[tr, c].mean(), d.loc[tr, c].std(ddof=0)
        X.append(((d[c] - m) / sd).values); names.append((c, ""))
    vc = d.loc[tr, "purpose"].value_counts()
    ref = sorted([k for k, v in vc.items() if v == vc.max()])[0]
    for lv in sorted(vc.index):
        if lv != ref and vc[lv] >= 30:
            X.append((d["purpose"] == lv).astype(float).values); names.append(("purpose", lv))
    return np.column_stack(X), names, ref
X, names, ref = design(None)
def newton(X, y, l2):
    n, p = X.shape; A = np.column_stack([np.ones(n), X]); b = np.zeros(p + 1)
    P = np.diag([0.0] + [l2] * p)
    for _ in range(100):
        pr = 1 / (1 + np.exp(-A @ b)); W = pr * (1 - pr)
        g = A.T @ (pr - y) + P @ b; H = (A * W[:, None]).T @ A + P
        st = np.linalg.solve(H, g); b -= st
        if abs(st).max() < 1e-12: break
    pr = 1 / (1 + np.exp(-A @ b)); W = pr * (1 - pr)
    se = np.sqrt(np.diag(np.linalg.inv((A * W[:, None]).T @ A + P)))
    return b, se
cases = []
for l2 in (1.0, 0.0):
    b, se = newton(X[tr], y[tr], l2)
    if l2 == 1.0:
        sk = LogisticRegression(C=1.0, tol=1e-12, max_iter=10000).fit(X[tr], y[tr])
        assert np.allclose(sk.coef_[0], b[1:], atol=1e-6) and abs(sk.intercept_[0] - b[0]) < 1e-6, "sklearn disagrees"
    else:
        sm_ = sm.Logit(y[tr], sm.add_constant(X[tr])).fit(disp=0, tol=1e-12)
        assert np.allclose(sm_.params, b, atol=1e-7) and np.allclose(sm_.bse, se, atol=1e-7), "statsmodels disagrees"
    rows = {}
    for k, (nm, lv) in enumerate([("(intercept)", "")] + names):
        z = b[k] / se[k]
        rows[f"coef|{nm}|{lv}"] = {"value": b[k], "std_err": se[k], "p_value": 2 * stats.norm.sf(abs(z)), "odds_ratio": float(np.exp(b[k]))}
    s = np.column_stack([np.ones(len(d)), X]) @ b; pd_ = 1 / (1 + np.exp(-s))
    for nm, mask in (("train", tr), ("test", test)):
        yy, pp, ss = y[mask], pd_[mask], s[mask]
        auc, se_, lo, hi = delong(yy, ss)
        assert abs(auc - roc_auc_score(yy, ss)) < 1e-12
        rows[f"{nm}|auc|"] = {"value": auc, "std_err": se_, "ci_low": lo, "ci_high": hi}
        rows[f"{nm}|gini|"] = {"value": 2 * auc - 1}
        rows[f"{nm}|ks|"] = {"value": stats.ks_2samp(ss[yy == 1], ss[yy == 0]).statistic}
        rows[f"{nm}|logloss|"] = {"value": log_loss(yy, pp)}
        rows[f"{nm}|brier|"] = {"value": brier_score_loss(yy, pp)}
        rows[f"{nm}|bads|"] = {"value": yy.sum()}
        rows[f"{nm}|records|"] = {"value": mask.sum()}
    cases.append({"l2": l2, "rows": rows})
out["scorecard"] = {"features": ",".join(num + ["purpose"]), "cases": cases, "reference": ref}
# ---------------------------------------------------------------- audit AUC per numeric feature (all rows)
out["audit"] = {c: {"auc": delong(y, d[c].values.astype(float))[0], "low": delong(y, d[c].values.astype(float))[2], "high": delong(y, d[c].values.astype(float))[3]} for c in num}
# ---------------------------------------------------------------- PSI
def psi(base, comp, bins=10):
    edges = np.unique(np.quantile(base, np.linspace(0, 1, bins + 1)[1:-1]))
    f = lambda v: np.bincount(np.searchsorted(edges, v, side="left"), minlength=len(edges) + 1)
    b, c = f(base), f(comp)
    bs, cs = np.maximum(b / len(base), 1e-4), np.maximum(c / len(comp), 1e-4)
    return {"total": float(((cs - bs) * np.log(cs / bs)).sum()), "base": b.tolist(), "compare": c.tolist()}
lo, hi = d[d["int.rate"] < 0.12], d[d["int.rate"] >= 0.12]
out["psi"] = psi(lo["fico"].values, hi["fico"].values)
# ---------------------------------------------------------------- disparity on the loan-approval sample
a = pd.read_csv(f"{D}/loan_approval_3000.csv")
fav = (a["Loan_Approved"] == "Approved")
g = a.groupby("Marital_Status").apply(lambda t: pd.Series({"n": len(t), "fav": int((t["Loan_Approved"] == "Approved").sum())}), include_groups=False)
g["rate"] = g.fav / g.n; refg = g.rate.idxmax()
ct = np.array([[r.fav, r.n - r.fav] for r in g.itertuples()])
chi2, p, dof, _ = stats.chi2_contingency(ct, correction=False)
out["disparity"] = {"groups": {k: {"n": int(r.n), "fav": int(r.fav), "rate": r.rate, "air": r.rate / g.rate[refg]} for k, r in g.iterrows()}, "ref": refg, "chi2": chi2, "df": dof, "p": p}
# ---------------------------------------------------------------- fraud hotspots (one-sided)
c = pd.read_csv(f"{D}/insurance_claims_graph.csv")
res = {}
for col in ["clinic_provider_id", "repair_provider_id", "attorney_provider_id"]:
    cc = c[c[col].notna()]; p0 = cc.is_fraud.mean()
    gg = cc.groupby(col).is_fraud.agg(["size", "sum"]); gg = gg[gg["size"] >= 10]
    z = (gg["sum"] / gg["size"] - p0) / np.sqrt(p0 * (1 - p0) / gg["size"]); pv = 2 * stats.norm.sf(abs(z))
    flagged_high = sorted(gg.index[(pv < 0.05 / len(gg)) & (z > 0)]); flagged_both = sorted(gg.index[pv < 0.05 / len(gg)])
    ring = set(c[c.fraud_ring_id.notna()][col].dropna())
    res[col] = {"high": flagged_high, "both": flagged_both, "ring_hits": len(set(flagged_high) & ring), "ring_total": len(ring), "false_pos_high": len(set(flagged_high) - ring)}
out["hotspots"] = res
json.dump(out, sys.stdout, indent=1, default=float)
