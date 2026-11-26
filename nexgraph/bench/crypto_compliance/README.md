# Crypto-compliance dataset harness (nexgraph v0.25)

Build once: `cargo build --release` (in this directory). `DATA` is the extracted `nexgraph_improved_datasets` folder.

| Step | Command |
|---|---|
| profile every CSV (kind/risk per column) | `./target/release/prof DATA > prof.tsv` |
| load all ten files into a directory database | `./target/release/load DATA db` |
| run NQL statements, one JSON line each | `./target/release/run db an.nql` and `an2.nql` |
| compare with pandas/networkx (79 checks) | edit `D` in `cross.py`, then `python3 cross.py` after `run ... > an.jsonl` / `an2.jsonl` |
| memory scale test on a CSV | `./target/release/scale file.csv newdir` then `./target/release/rssopen newdir` |

`scale` reports peak RSS (VmHWM) after import and after checkpoint; `rssopen` reports RSS after reopening.
