"""compare.py A.json B.json: side-by-side table; flags any workload whose results differ."""
import json, sys
a, b = (json.load(open(x)) for x in sys.argv[1:3])
ra = {r["workload"]: r for r in a["rows"]}; rb = {r["workload"]: r for r in b["rows"]}
print(f"{'workload':9} {a['system']:>22} {b['system']:>22}  ratio  results")
for w in ra:
    x, y = ra[w], rb.get(w)
    if not y: continue
    same = "match" if str(x["checksum"]).replace(" ", "") == str(y["checksum"]).replace(" ", "") else "DIFFER"
    print(f"{w:9} {x['median_us']:>19.1f}us {y['median_us']:>19.1f}us  {y['median_us']/x['median_us']:5.2f}x  {same}")
print(f"load      {a['load_ms']:>19.0f}ms {b['load_ms']:>19.0f}ms")
