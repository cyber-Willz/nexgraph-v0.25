"""Prototype: displacement-consistency linking (noise-adaptive). Returns groups as [(set(members), size, 'proto')]."""
import numpy as np, pandas as pd, itertools
from scipy.stats import chi2

def detect(d, win, eps=1.5, persist=0.6, min_windows=3, min_size=3, rel_tol=0.35, alpha=0.999, move_z=3.0, dim=3, min_samples=3):
    d = d.copy(); d["w"] = np.floor(d["time"] / win).astype(int)
    tw = {}
    resid = []; trs = {}
    for (i, w), g in d.groupby(["id", "w"]):
        if len(g) < min_samples: continue
        g = g.sort_values("time"); t = g["time"].values; P = g[["px","py","pz"]].values[:, :dim]
        tb = t.mean(); sxx = ((t - tb) ** 2).sum()
        if sxx <= 0: continue
        m = P.mean(0); v = ((t - tb)[:, None] * (P - m)).sum(0) / sxx
        r = P - m - np.outer(t - tb, v); n = len(t)
        rs = np.sqrt((r ** 2).sum() / max(n - 2, 1) / dim)                 # per-axis residual sd of this track-window
        resid.append(rs); trs.setdefault(i, []).append(rs)
        tw[(i, w)] = (m, n)
    if not resid: return []
    sig = float(np.median(resid))                                          # scene sensor noise
    sg = {i: max(float(np.median(v)), 0.5 * sig) for i, v in trs.items()}   # per-track noise (median over its windows), floored
    ids = sorted({i for i, _ in tw}); idx = {i: k for k, i in enumerate(ids)}
    co = {}; k2 = chi2.ppf(alpha, dim)
    ws = sorted({w for _, w in tw})
    for w in ws:
        if (w + 1) not in ws: continue
        pres = [i for i in ids if (i, w) in tw and (i, w + 1) in tw]
        for a, b in itertools.combinations(pres, 2):
            ma0, na0 = tw[(a, w)]; ma1, na1 = tw[(a, w + 1)]; mb0, nb0 = tw[(b, w)]; mb1, nb1 = tw[(b, w + 1)]
            if np.linalg.norm(ma0 - mb0) > eps or np.linalg.norm(ma1 - mb1) > eps:
                key = (a, b); c = co.setdefault(key, [0, 0]); c[0] += 1; continue
            da, db = ma1 - ma0, mb1 - mb0
            var = sg[a] ** 2 * (1 / na0 + 1 / na1) + sg[b] ** 2 * (1 / nb0 + 1 / nb1)   # per-axis variance of (da - db)
            mv = max(np.linalg.norm(da), np.linalg.norm(db))
            floor = move_z * max(sg[a], sg[b]) * np.sqrt(dim * (1 / na0 + 1 / na1))   # smallest displacement distinguishable from noise
            ok = min(np.linalg.norm(da), np.linalg.norm(db)) >= floor and np.linalg.norm(da - db) ** 2 <= k2 * var + (rel_tol * mv) ** 2
            c = co.setdefault((a, b), [0, 0]); c[0] += 1; c[1] += int(ok)
    par = {i: i for i in ids}
    def f(x):
        while par[x] != x: par[x] = par[par[x]]; x = par[x]
        return x
    for (a, b), (c, l) in co.items():
        if c >= min_windows and l >= persist * c: par[f(a)] = f(b)
    comps = {}
    for i in ids: comps.setdefault(f(i), set()).add(i)
    return [(m, len(m), "proto") for m in comps.values() if len(m) >= min_size], sig
