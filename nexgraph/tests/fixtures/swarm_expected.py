#!/usr/bin/env python3
"""Fixtures and reference values for tests/swarm.rs (numpy only; independent of the engine).
usage: python3 -I swarm_expected.py <fixtures dir>   (writes swarm_sim.csv and swarm_expected.json there)

swarm_sim.csv: 35 tracks x 120 time steps (dt = 1). Truth (column `truth`, not used by the engine):
  A (8 drones), B (5), C (4): formations with a common velocity, fixed offsets (sd 3) and per-sample jitter;
  X (12): independent straight-line drones, one of which crosses swarm A's path;
  M (6): a milling crowd: close together, unrelated velocities (hover with random walk) -> co-located, not a swarm."""
import json, sys, numpy as np, pandas as pd
D = sys.argv[1]
M64 = (1 << 64) - 1
class Rng:
    def __init__(s, seed): s.s = seed & M64
    def next(s):
        s.s = (s.s + 0x9E3779B97F4A7C15) & M64; z = s.s
        z = ((z ^ (z >> 30)) * 0xBF58476D1CE4E5B9) & M64
        z = ((z ^ (z >> 27)) * 0x94D049BB133111EB) & M64
        return z ^ (z >> 31)
    def unit(s): return (s.next() >> 11) / 9007199254740992.0
    def direction(s, dim):
        if dim == 2:
            a = 2.0 * np.pi * s.unit(); return np.array([np.cos(a), np.sin(a), 0.0])
        z = 2.0 * s.unit() - 1.0; a = 2.0 * np.pi * s.unit(); r = np.sqrt(max(1.0 - z * z, 0.0)); return np.array([r * np.cos(a), r * np.sin(a), z])
def simulate(jitter, seed=7):
    r = np.random.default_rng(seed); T = np.arange(120); rows = []; k = 0
    def add(tid, truth, P):
        for t, p in zip(T, P): rows.append((f"T{tid:03d}", int(t), *np.round(p, 6), truth))
    ids = list(r.permutation(35))
    for name, size, c0, vel in [("A", 8, [0, 0, 50], [3.0, 1.0, 0.2]), ("B", 5, [400, 100, 80], [-2.0, 2.5, 0.0]), ("C", 4, [100, 600, 30], [0.5, -3.0, 0.3])]:
        off = r.normal(0, 3, (size, 3))
        for i in range(size):
            P = np.array(c0) + off[i] + np.outer(T, vel) + r.normal(0, jitter, (120, 3)); add(ids[k], name, P); k += 1
    for i in range(12):
        if i == 0: start, vel = np.array([150.0, 60.0, 50.0]), np.array([0.0, -2.0, 0.0])   # crosses A's path
        else: start, vel = r.uniform(0, 700, 3), r.normal(0, 2.5, 3)
        add(ids[k], "X", start + np.outer(T, vel) + r.normal(0, jitter, (120, 3))); k += 1
    c = np.array([250.0, 250.0, 60.0])
    for i in range(6):
        walk = np.cumsum(r.normal(0, 1.5, (120, 3)), axis=0)
        add(ids[k], "M", c + r.normal(0, 4, 3) + walk * 0.3 + r.normal(0, jitter, (120, 3))); k += 1
    return pd.DataFrame(rows, columns=["id", "t", "x", "y", "z", "truth"])
def detect(df, id_col, t_col, win, eps=20.0, cos_min=0.8, speed_tol=0.5, persist=0.6, min_windows=3, min_size=3, min_samples=3, perm=1000, seed=1, p_max=0.01, coh_min=0.6, dim=3, pos=("x", "y", "z")):
    df = df.copy(); df["_w"] = np.floor(df[t_col] / win).astype(int)
    names = sorted(df[id_col].astype(str).unique()); rank = {n: i for i, n in enumerate(names)}
    wins = {}
    for (i, w), g in df.groupby([df[id_col].astype(str), "_w"], sort=False):
        g = g.sort_values(t_col, kind="stable")
        if len(g) < min_samples: continue
        t = g[t_col].values.astype(float); sxx = ((t - t.mean()) ** 2).sum()
        if sxx <= 0: continue
        P = g[list(pos)].values.astype(float)
        if dim == 2: P = P.copy(); P[:, 2] = 0.0
        m = P.mean(0); v = ((t - t.mean())[:, None] * (P - m)).sum(0) / sxx
        wins.setdefault(w, []).append((rank[i], m, v, float(np.linalg.norm(v))))
    for w in wins: wins[w].sort(key=lambda x: x[0])
    co = {}
    for w, tw in wins.items():
        for a in range(len(tw)):
            for b in range(a + 1, len(tw)):
                (i, pi, vi, si), (j, pj, vj, sj) = tw[a], tw[b]
                e = co.setdefault((i, j), [0, 0]); e[0] += 1
                if si > 0 and sj > 0 and np.linalg.norm(pi - pj) <= eps and (vi @ vj) / (si * sj) >= cos_min and min(si, sj) / max(si, sj) >= speed_tol: e[1] += 1
    par = list(range(len(names)))
    def find(x):
        while par[x] != x: par[x] = par[par[x]]; x = par[x]
        return x
    for (i, j), (c, l) in co.items():
        if c >= min_windows and l >= persist * c:
            ri, rj = find(i), find(j)
            if ri != rj: par[max(ri, rj)] = min(ri, rj)
    comps = {}
    for i in range(len(names)): comps.setdefault(find(i), []).append(i)
    groups = sorted([sorted(m) for m in comps.values() if len(m) >= min_size], key=lambda m: (-len(m), m[0]))
    out = []
    for gi, m in enumerate(groups):
        ms = set(m); wc = 0; coh_sum = dist_sum = 0.0; first, last = 10**9, -10**9; speeds = []; uw = []
        for w in sorted(wins):
            s = [t for t in wins[w] if t[0] in ms]
            if len(s) < 2: continue
            wc += 1; first = min(first, w); last = max(last, w)
            u = sum((t[2] / t[3] for t in s if t[3] > 0), np.zeros(3)); speeds += [t[3] for t in s]
            coh_sum += np.linalg.norm(u) / len(s)
            ds = [np.linalg.norm(s[a][1] - s[b][1]) for a in range(len(s)) for b in range(a + 1, len(s))]; dist_sum += np.mean(ds); uw.append((w, len(s)))
        coh = coh_sum / wc; sm = np.mean(speeds); sd = np.std(speeds)
        fr = [co[(i, j)][1] / co[(i, j)][0] for a, i in enumerate(m) for j in m[a + 1:] if (i, j) in co]
        p = pb = None
        if perm:
            stream = seed + gi; rng = Rng(stream); riso = Rng(stream ^ 0xD1B54A32D192ED03); ge = gi_ = 0
            for _ in range(perm):
                acc = acci = 0.0
                for w, k in uw:
                    pool = wins[w]; idx = list(range(len(pool))); u = np.zeros(3); ui = np.zeros(3)
                    for i in range(k):
                        r = i + rng.next() % (len(pool) - i); idx[i], idx[r] = idx[r], idx[i]; t = pool[idx[i]]
                        if t[3] > 0: u += t[2] / t[3]
                        ui += riso.direction(dim)
                    acc += np.linalg.norm(u) / k; acci += np.linalg.norm(ui) / k
                if acc / len(uw) >= coh - 1e-12: ge += 1
                if acci / len(uw) >= coh - 1e-12: gi_ += 1
            p = (1 + gi_) / (1 + perm); pb = (1 + ge) / (1 + perm)
        if p is not None and p <= p_max and coh >= coh_min: verdict = "coordinated" if pb <= p_max else "coordinated (whole scene)"
        elif p is None and coh >= coh_min: verdict = "coherent (untested)"
        else: verdict = "co-located"
        out.append({"members": [names[i] for i in m], "size": len(m), "windows": wc, "first": first * win, "last": last * win, "persistence": float(np.mean(fr)) if fr else 0.0,
                    "mean_pair_dist": dist_sum / wc, "coherence": coh, "speed_mean": sm, "speed_cv": sd / sm if sm > 0 else 0.0, "p": p, "p_bg": pb, "verdict": verdict})
    return {"tracks": len(names), "windows": len(wins), "groups": out}
if __name__ == "__main__":
    sim = simulate(1.0); sim.to_csv(f"{D}/swarm_sim.csv", index=False)
    res = {"sim": {}}
    truth = sim.groupby("id").truth.first().to_dict()
    for name, kw in {"default": {}, "tight_eps": {"eps": 8.0}, "no_perm": {"perm": 0}, "strict_cos": {"cos_min": 0.95, "persist": 0.8}}.items():
        r = detect(sim, "id", "t", 10.0, **kw); res["sim"][name] = r
    # recovery of the planted swarms at several jitter levels (not replayed in Rust beyond the default case)
    rec = {}
    for jit in (0.5, 1.0, 2.0, 4.0, 8.0):
        s = simulate(jit, seed=11); tr = s.groupby("id").truth.first().to_dict(); r = detect(s, "id", "t", 10.0, perm=200)
        found = [{"truth": sorted(set(tr[m] for m in g["members"])), "size": g["size"], "verdict": g["verdict"]} for g in r["groups"]]
        rec[str(jit)] = found
    res["recovery"] = rec
    # planar mode on the same simulation, and the milling crowd alone at a loose threshold (co-located control)
    res["sim"]["dim2"] = detect(sim, "id", "t", 10.0, dim=2, perm=300)
    mil = sim[sim.truth == "M"]; res["sim"]["crowd_only"] = detect(mil, "id", "t", 10.0, eps=40.0, cos_min=0.0, persist=0.5, perm=300)
    real = pd.read_csv(f"{D}/drone_swarm_dataset_20x1000.csv")
    res["real"] = {}
    for win in (10.0, 50.0, 100.0):
        r = detect(real, "drone_id", "timestamp", win, eps=20.0, min_samples=3); res["real"][str(win)] = r
    json.dump(res, open(f"{D}/swarm_expected.json", "w"), indent=1, default=float)
    d = res["sim"]["default"]; print("sim default groups:", [(g["size"], sorted(set(truth[m] for m in g["members"])), g["verdict"], round(g["coherence"], 3), g["p"], g["p_bg"]) for g in d["groups"]])
    for j, v in rec.items(): print("jitter", j, [(x["truth"], x["size"], x["verdict"]) for x in v])
    for w, r in res["real"].items(): print("real win", w, [(g["size"], round(g["coherence"], 3), g["p"], g["p_bg"], g["verdict"], g["windows"]) for g in r["groups"]], "windows", r["windows"])
    for k in ("dim2", "crowd_only"): print(k, [(g["size"], round(g["coherence"], 3), g["p"], g["p_bg"], g["verdict"]) for g in res["sim"][k]["groups"]])
