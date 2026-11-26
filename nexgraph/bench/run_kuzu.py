"""Kuzu driver with the same workloads/checksums as examples/bench_compare.rs: run_kuzu.py DATA_DIR OUT.json"""
import json, os, shutil, sys, time, kuzu
d, outp = sys.argv[1], sys.argv[2]
db_dir = os.path.join(d, "kuzu_db")
for p_ in (db_dir, db_dir + ".wal", db_dir + ".shadow"):  # Kuzu 0.11 uses a single file; older versions a directory
    shutil.rmtree(p_, ignore_errors=True) if os.path.isdir(p_) else (os.path.exists(p_) and os.remove(p_))
db = kuzu.Database(db_dir); c = kuzu.Connection(db)
t = time.perf_counter()
c.execute("CREATE NODE TABLE N(k INT64, PRIMARY KEY(k))"); c.execute("CREATE REL TABLE E(FROM N TO N)")
c.execute(f"COPY N FROM '{d}/nodes.csv' (header=true)"); c.execute(f"COPY E FROM '{d}/edges.csv' (header=true)")
load_ms = (time.perf_counter() - t) * 1e3; print(f"kuzu       load {load_ms:.0f} ms")
ids = lambda n: [int(x) for x in open(f"{d}/{n}.txt").read().split()]
rows = []
def first_int(res):
    if res.has_next():
        v = res.get_next()[0]; return int(v) if isinstance(v, int) else -1
    return -1
def summarize(name, lat, check):
    lat = sorted(lat); med = lat[len(lat) // 2]; p95 = lat[int((len(lat) - 1) * 0.95)]
    print(f"kuzu       {name:8} n={len(lat):5} median={med:12.1f}us p95={p95:12.1f}us")
    rows.append({"workload": name, "n": len(lat), "median_us": med, "p95_us": p95, "total_ms": sum(lat) / 1e3, "checksum": check})
def run(name, args, q, warm):
    ps = c.prepare(q)  # prepared once: Kuzu is not charged for parsing/planning on every call
    for a in args[:warm]: c.execute(ps, {"k": a}).has_next()
    lat, res = [], []
    for a in args:
        t = time.perf_counter(); r = c.execute(ps, {"k": a}); v = first_int(r); lat.append((time.perf_counter() - t) * 1e6); res.append(v)
    summarize(name, lat, str(res).replace("'", ""))
def once(name, q, reps=3, fmt=None):
    lat, last = [], None
    for _ in range(reps):
        t = time.perf_counter(); r = c.execute(q); last = [list(x) for x in iter(lambda: r.get_next() if r.has_next() else None, None)]; lat.append((time.perf_counter() - t) * 1e6)
    summarize(name, lat, fmt(last))
ps0 = c.prepare("RETURN 1")
lat = []
for i in range(2000):
    t = time.perf_counter(); c.execute(ps0).has_next(); lat.append((time.perf_counter() - t) * 1e6)
summarize("noop", lat, "[]")
run("point", ids("point"), "MATCH (a:N {k: $k}) RETURN a.k", 30)
run("hop1", ids("hop1"), "MATCH (a:N {k: $k})-[:E]->(b) RETURN count(b)", 30)
run("hop2", ids("hop2"), "MATCH (a:N {k: $k})-[:E]->(b)-[:E]->(c) RETURN count(*)", 30)
run("reach3", ids("reach3"), "MATCH (a:N {k: $k})-[:E*1..3]->(b) RETURN count(DISTINCT b)", 10)
once("scan", "MATCH (a:N)-[:E]->(b) RETURN count(*)", fmt=lambda r: f"[{r[0][0]}]")
once("degree", "MATCH (a:N)-[:E]->(b) RETURN a.k AS k, count(*) AS d ORDER BY d DESC, k LIMIT 5", fmt=lambda r: str([[x[0], x[1]] for x in r]).replace(" ", "").replace("],[", "], [").replace("[[", "[[") )
once("withagg", "MATCH (a:N)-[:E]->(b) WITH a, count(b) AS d WHERE d > 12 RETURN count(*)", fmt=lambda r: f"[{r[0][0]}]")
pairs = [tuple(map(int, l.split())) for l in open(f"{d}/pairs.txt").read().splitlines()]
psp = c.prepare("MATCH (a:N {k:$a})-[e:E* SHORTEST 1..12]->(b:N {k:$b}) RETURN length(e)")
lat, res = [], []
for i, (a, b) in enumerate(pairs):
    t = time.perf_counter()
    r = c.execute(psp, {"a": a, "b": b}); v = first_int(r)
    if i >= 5: lat.append((time.perf_counter() - t) * 1e6); res.append(v)
summarize("path", lat, str(res))
json.dump({"system": "kuzu " + kuzu.__version__, "load_ms": load_ms, "pagerank_ms": None, "rows": rows}, open(outp, "w"))
