"""Deterministic benchmark graph: gen_data.py OUT_DIR [N=100000] [M=1000000] [SEED=7]
Files: nodes.csv (k), edges.csv (a,b), point/hop1/hop2/reach3.txt (start keys), pairs.txt (a b)."""
import os, random, sys
out = sys.argv[1]; n = int(sys.argv[2]) if len(sys.argv) > 2 else 100_000
m = int(sys.argv[3]) if len(sys.argv) > 3 else 1_000_000; seed = int(sys.argv[4]) if len(sys.argv) > 4 else 7
os.makedirs(out, exist_ok=True); r = random.Random(seed)
edges = set()
while len(edges) < m:
    a = r.randrange(n); b = int(n * r.random() ** 2)  # skewed targets: a few hubs
    if a != b: edges.add((a, b))
edges = sorted(edges)
with open(f"{out}/nodes.csv", "w") as f: f.write("k\n"); f.writelines(f"{i}\n" for i in range(n))
with open(f"{out}/edges.csv", "w") as f: f.write("a,b\n"); f.writelines(f"{a},{b}\n" for a, b in edges)
for name, cnt in [("point", 2000), ("hop1", 2000), ("hop2", 500), ("reach3", 100)]:
    with open(f"{out}/{name}.txt", "w") as f: f.write("\n".join(str(r.randrange(n)) for _ in range(cnt)))
with open(f"{out}/pairs.txt", "w") as f: f.write("\n".join(f"{r.randrange(n)} {r.randrange(n)}" for _ in range(60)))
print(f"{n} nodes, {len(edges)} edges -> {out}")
