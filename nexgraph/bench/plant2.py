import pandas as pd, numpy as np, itertools, sys
from plant import *

def leader(f, pick=0):
    ld = load(f); ids = sorted(ld.id.unique()); L = ld[ld.id == ids[pick % len(ids)]].sort_values("time")
    tg = np.round(np.arange(0.0, 49.85, 0.1), 1)
    P = np.stack([np.interp(tg, L["time"].values, L[c].values) for c in ("px","py","pz")], 1)
    return tg, P

def swarm_rows(name, t, P, k, sigma, rng, rot=0.0, spread=0.25, shift=(0,0,0)):
    out = []; offs = rng.normal(0, spread, (k, 3))
    for i in range(k):
        o = np.tile(offs[i], (len(t), 1))
        if rot:                                   # formation rotates about the leader axis (z) -> followers' velocities differ
            a = rot * t; c, s = np.cos(a), np.sin(a)
            o = np.stack([c*offs[i,0]-s*offs[i,1], s*offs[i,0]+c*offs[i,1], np.full_like(t, offs[i,2])], 1)
        X = P + o + np.array(shift) + rng.normal(0, sigma, P.shape)
        out.append(pd.DataFrame({"id": f"{name}{i}", "time": t, "px": X[:,0], "py": X[:,1], "pz": X[:,2]}))
    return out

def make2(bg_file, f1, f2, scen, sigma, seed):
    rng = np.random.default_rng(seed)
    bg = load(bg_file)[["id","time","px","py","pz"]].copy(); bg["id"] = "B" + bg["id"].astype(str)
    t, P1 = leader(f1, 0); _, P2 = leader(f2, 1)
    truth = []
    if scen == "noise":      rows = swarm_rows("S", t, P1, 5, sigma, rng); truth = [{f"S{i}" for i in range(5)}]
    elif scen == "small3":   rows = swarm_rows("S", t, P1, 3, sigma, rng); truth = [{f"S{i}" for i in range(3)}]
    elif scen == "rotate":   rows = swarm_rows("S", t, P1, 5, sigma, rng, rot=0.15); truth = [{f"S{i}" for i in range(5)}]
    elif scen == "two_far":  # two swarms, different real leaders (different headings), overlapping airspace
        rows = swarm_rows("S", t, P1, 4, sigma, rng) + swarm_rows("T", t, P2, 4, sigma, rng)
        truth = [{f"S{i}" for i in range(4)}, {f"T{i}" for i in range(4)}]
    elif scen == "two_par":  # two swarms flying the SAME path 0.9 m apart (side by side): one or two swarms?
        rows = swarm_rows("S", t, P1, 4, sigma, rng) + swarm_rows("T", t, P1, 4, sigma, rng, shift=(0.9, 0, 0))
        truth = [{f"S{i}" for i in range(4)}, {f"T{i}" for i in range(4)}]
    elif scen == "escort":   # 5-drone swarm plus an independent drone that shadows one member for the middle 40 % of the run
        rows = swarm_rows("S", t, P1, 5, sigma, rng); truth = [{f"S{i}" for i in range(5)}]
        m = (t > 0.3*t.max()) & (t < 0.7*t.max())
        E = np.where(m[:, None], P2*0 + P1 + np.array([0.3,0.3,0]) , P2)
        rows.append(pd.DataFrame({"id": "E0", "time": t, "px": E[:,0]+rng.normal(0,sigma,len(t)), "py": E[:,1]+rng.normal(0,sigma,len(t)), "pz": E[:,2]+rng.normal(0,sigma,len(t))}))
    return pd.concat([bg] + rows, ignore_index=True), truth

def score2(gs, truths):
    # per truth swarm: best F1 among groups; plus: false groups (<50% overlap with any truth), merged (a group covering >1 truth swarm)
    f1s = []
    for T in truths:
        best = 0
        for m, sz, v in gs:
            tp = len(m & T)
            if tp: p, r = tp/len(m), tp/len(T); best = max(best, 2*p*r/(p+r))
        f1s.append(best)
    false = sum(1 for m, sz, v in gs if max(len(m & T) for T in truths) < 0.5*len(m))
    merged = sum(1 for m, sz, v in gs if sum(1 for T in truths if len(m & T) >= 2) > 1)
    return f1s, false, merged

def bench2(opts, win, scens, sigmas, nbg=12, tag="y"):
    rng = np.random.default_rng(11); sel = list(rng.choice(len(files), nbg, replace=False)); rows = []
    for scen, sg in itertools.product(scens, sigmas):
        F, fa, mg, found = [], 0, 0, 0
        for j, bi in enumerate(sel):
            d, truths = make2(files[bi], files[(bi+5) % len(files)], files[(bi+11) % len(files)], scen, sg, j)
            gs = run(d, win, opts, tag); f1s, fg, m = score2(gs, truths)
            F.append(np.mean(f1s)); fa += fg; mg += m; found += sum(1 for x in f1s if x >= 0.99)
        rows.append(dict(scen=scen, sigma=sg, mean_f1=round(float(np.mean(F)), 3), swarms_exact=f"{found}/{nbg*len(truths)}", false_groups=fa, merged=mg))
    return pd.DataFrame(rows)

if __name__ == "__main__":
    win, opts = sys.argv[1], sys.argv[2]
    scens = ["noise","small3","rotate","two_far","two_par","escort"]
    print(bench2(opts, win, scens, [0.05, 0.3, 0.6]).to_string(index=False))
