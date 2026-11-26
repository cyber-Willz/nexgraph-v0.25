"""Plant known formations into REAL drone traffic (UAM-FD field tests) and score swarm.detect."""
import pandas as pd, numpy as np, glob, subprocess, sys, json, os, itertools
B = os.environ.get("EVAL_BIN", "/home/claude/work/nexgraph_src/nexgraph/target/release/examples/swarm_eval")
ROOT = "/home/claude/work/uamfd/field_test_trajs"
files = sorted(glob.glob(f"{ROOT}/*/*_D4.csv") + glob.glob(f"{ROOT}/*/*_D6.csv") + glob.glob(f"{ROOT}/*/*_D8.csv"))

def load(f):
    d = pd.read_csv(f); d["id"] = d["id"].astype(int); return d

def make(bg_file, lead_file, scen, sigma, k=5, seed=0):
    rng = np.random.default_rng(seed)
    bg = load(bg_file)[["id","time","px","py","pz"]].copy(); bg["id"] = "B" + bg["id"].astype(str)
    ld = load(lead_file); lid = ld["id"].min()
    L = ld[ld.id == lid].sort_values("time"); t = L["time"].values; P = L[["px","py","pz"]].values
    rows = []
    offs = rng.normal(0, 0.25, (k, 3))
    for i in range(k):
        tau = 0.0 if scen not in ("lag",) else 0.1 * i          # leader-follower delay
        Pi = np.stack([np.interp(t - tau, t, P[:, c]) for c in range(3)], 1)
        o = np.tile(offs[i], (len(t), 1))
        if scen == "breathe": o = o * (1 + 0.3 * np.sin(2 * np.pi * t / 12 + i))[:, None]
        X = Pi + o + rng.normal(0, sigma, Pi.shape)
        df = pd.DataFrame({"id": f"S{i}", "time": t, "px": X[:, 0], "py": X[:, 1], "pz": X[:, 2]})
        rows.append(df)
    d = pd.concat([bg] + rows, ignore_index=True)
    if scen == "dropout": d = d.sample(frac=0.6, random_state=seed)
    return d, {f"S{i}" for i in range(k)}

def run(d, win, opts, tag):
    p = f"/tmp/plant_{tag}.csv"; d.to_csv(p, index=False)
    out = subprocess.run([B, p, "id", "time", "px", "py", "pz", str(win), opts], capture_output=True, text=True).stdout.strip().splitlines()[1:]
    gs = []
    for l in out:
        m, sz, coh, pc, pb, v = l.split("|"); gs.append((set(m.replace(",...", "").split(",")), int(sz), v))
    return gs

def score(gs, truth):
    best = (0, 0, 0); false = 0
    for m, sz, v in gs:
        tp = len(m & truth)
        if tp == 0 or tp < 0.5 * len(m): false += 1
        p = tp / len(m); r = tp / len(truth); f1 = 0 if tp == 0 else 2 * p * r / (p + r)
        if f1 > best[0]: best = (f1, p, r)
    return best, false

def bench(opts, win, tag, scens=("rigid","lag","breathe","dropout"), sigmas=(0.01, 0.05, 0.12), nbg=12):
    rng = np.random.default_rng(7); res = []
    sel = list(rng.choice(len(files), nbg, replace=False))
    for scen, sg in itertools.product(scens, sigmas):
        f1s, fa, rec = [], 0, []
        for j, bi in enumerate(sel):
            li = (bi + 5) % len(files)
            d, truth = make(files[bi], files[li], scen, sg, seed=j)
            gs = run(d, win, opts, tag)
            (f1, p, r), fg = score(gs, truth)
            f1s.append(f1); rec.append(r); fa += fg
        res.append(dict(scen=scen, sigma=sg, f1=round(float(np.mean(f1s)), 3), found=int(sum(1 for x in f1s if x >= 0.99)), n=len(f1s), false_groups=fa))
    return pd.DataFrame(res)

if __name__ == "__main__":
    win = sys.argv[1]; opts = sys.argv[2]
    print(bench(opts, win, "x").to_string(index=False))
