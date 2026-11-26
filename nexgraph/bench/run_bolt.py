"""UNTESTED HERE: no Neo4j/Memgraph server could be installed in the build sandbox (no Docker daemon; vendor hosts blocked).
Same workloads and checksums as run_kuzu.py / bench_compare.rs, for any Bolt server.
  pip install neo4j ; python3 run_bolt.py DATA_DIR OUT.json bolt://localhost:7687 neo4j|memgraph [user password]
Memgraph differs in the shortest-path syntax only; both variants are below."""
import json, sys, time
from neo4j import GraphDatabase
d, outp, uri, flavour = sys.argv[1:5]
auth = (sys.argv[5], sys.argv[6]) if len(sys.argv) > 6 else None
drv = GraphDatabase.driver(uri, auth=auth); s = drv.session()
rd = lambda n: [l.strip() for l in open(f"{d}/{n}").read().splitlines() if l.strip()]
s.run("MATCH (n) DETACH DELETE n").consume()
t = time.perf_counter()
s.run("CREATE INDEX ON :N(k)" if flavour == "memgraph" else "CREATE INDEX n_k IF NOT EXISTS FOR (n:N) ON (n.k)").consume()
nodes = [int(x) for x in rd("nodes.csv")[1:]]
for i in range(0, len(nodes), 10000): s.run("UNWIND $ks AS k CREATE (:N {k:k})", ks=nodes[i:i + 10000]).consume()
edges = [tuple(map(int, l.split(","))) for l in rd("edges.csv")[1:]]
for i in range(0, len(edges), 10000): s.run("UNWIND $es AS e MATCH (a:N {k:e[0]}), (b:N {k:e[1]}) CREATE (a)-[:E]->(b)", es=[list(e) for e in edges[i:i + 10000]]).consume()
load_ms = (time.perf_counter() - t) * 1e3
rows = []
def summarize(name, lat, check):
    lat = sorted(lat); rows.append({"workload": name, "n": len(lat), "median_us": lat[len(lat) // 2], "p95_us": lat[int((len(lat) - 1) * .95)], "total_ms": sum(lat) / 1e3, "checksum": check})
def first(q, **p):
    r = s.run(q, **p).single(strict=False); return -1 if r is None else int(r[0])
def run(name, f, q, warm):
    ids = [int(x) for x in rd(f + ".txt")]
    for a in ids[:warm]: first(q, k=a)
    lat, res = [], []
    for a in ids:
        t = time.perf_counter(); res.append(first(q, k=a)); lat.append((time.perf_counter() - t) * 1e6)
    summarize(name, lat, str(res))
lat = []
for _ in range(2000):
    t = time.perf_counter(); first("RETURN 1"); lat.append((time.perf_counter() - t) * 1e6)
summarize("noop", lat, "[]")
run("point", "point", "MATCH (a:N {k: $k}) RETURN a.k", 30)
run("hop1", "hop1", "MATCH (a:N {k: $k})-[:E]->(b) RETURN count(b)", 30)
run("hop2", "hop2", "MATCH (a:N {k: $k})-[:E]->(b)-[:E]->(c) RETURN count(*)", 30)
run("reach3", "reach3", "MATCH (a:N {k: $k})-[:E*1..3]->(b) RETURN count(DISTINCT b)", 10)
def once(name, q, fmt):
    lat, last = [], None
    for _ in range(3):
        t = time.perf_counter(); last = [list(r.values()) for r in s.run(q)]; lat.append((time.perf_counter() - t) * 1e6)
    summarize(name, lat, fmt(last))
once("scan", "MATCH (a:N)-[:E]->(b) RETURN count(*)", lambda r: f"[{r[0][0]}]")
once("degree", "MATCH (a:N)-[:E]->(b) RETURN a.k AS k, count(*) AS d ORDER BY d DESC, k LIMIT 5", lambda r: str([[x[0], x[1]] for x in r]))
once("withagg", "MATCH (a:N)-[:E]->(b) WITH a, count(b) AS d WHERE d > 12 RETURN count(*)", lambda r: f"[{r[0][0]}]")
sp = ("MATCH (a:N {k:$a}), (b:N {k:$b}) MATCH p = shortestPath((a)-[:E*..12]->(b)) RETURN length(p)" if flavour == "neo4j"
      else "MATCH p=(a:N {k:$a})-[:E *BFS ..12]->(b:N {k:$b}) RETURN size(relationships(p))")
pairs = [tuple(map(int, l.split())) for l in rd("pairs.txt")]; lat, res = [], []
for i, (a, b) in enumerate(pairs):
    t = time.perf_counter(); v = first(sp, a=a, b=b)
    if i >= 5: lat.append((time.perf_counter() - t) * 1e6); res.append(v)
summarize("path", lat, str(res))
json.dump({"system": flavour, "load_ms": load_ms, "pagerank_ms": None, "rows": rows}, open(outp, "w"))
