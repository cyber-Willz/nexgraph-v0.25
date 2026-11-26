import sys, numpy as np, pandas as pd, itertools
import plant2, proto
from plant2 import *
def bench_proto(win, scens, sigmas, nbg=12, **kw):
    rng = np.random.default_rng(11); sel = list(rng.choice(len(files), nbg, replace=False)); rows = []
    for scen, sg in itertools.product(scens, sigmas):
        F, fa, mg, found, sigs = [], 0, 0, 0, []
        for j, bi in enumerate(sel):
            d, truths = make2(files[bi], files[(bi+5) % len(files)], files[(bi+11) % len(files)], scen, sg, j)
            r = proto.detect(d, win, **kw); gs, sig = (r if r else ([], 0)); sigs.append(sig)
            f1s, fg, m = score2(gs, truths); F.append(np.mean(f1s)); fa += fg; mg += m; found += sum(1 for x in f1s if x >= 0.99)
        rows.append(dict(scen=scen, sigma=sg, est_sigma=round(float(np.mean(sigs)), 3), mean_f1=round(float(np.mean(F)), 3), swarms_exact=f"{found}/{nbg*len(truths)}", false_groups=fa, merged=mg))
    return pd.DataFrame(rows)
if __name__ == "__main__":
    win = float(sys.argv[1])
    print(bench_proto(win, ["noise","small3","rotate","two_far","escort"], [0.05, 0.3, 0.6]).to_string(index=False))
