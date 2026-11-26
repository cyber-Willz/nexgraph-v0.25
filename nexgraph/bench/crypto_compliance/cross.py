import json, pandas as pd, numpy as np, networkx as nx, sys
D="/home/claude/crypto_in/nexgraph_improved_datasets/"
R={}
for f in ["an.jsonl","an2.jsonl"]:
    for l in open(f):
        if l.startswith("{"):
            d=json.loads(l); R[d["name"]]=d
h=pd.read_csv(D+"bitcoin_heist_ransomware.csv"); a=pd.read_csv(D+"eth_fraud_cleaned.csv"); b=pd.read_csv(D+"eth_illicit_features.csv")
s=pd.read_csv(D+"crypto_scam_tx.csv"); m=pd.read_csv(D+"bitcoin_scam_messages.csv"); t=pd.read_csv(D+"blockchain_tx_risk.csv")
p=pd.read_csv(D+"digital_payment_fraud.csv"); dn=pd.read_csv(D+"bitcoin_daily_network.csv")
out=[]
def chk(name,got,exp,tol=1e-9):
    ok = (abs(got-exp)<=tol*max(1,abs(exp))) if isinstance(exp,(int,float,np.integer,np.floating)) and got is not None else got==exp
    out.append((name,got,exp,bool(ok)))
dat=lambda n: R[n]["data"]
chk("heist rows",dat("c01_heist_count")[0]["n"],len(h)); chk("heist distinct addr (keyed)",dat("c02_heistbyaddr_count")[0]["n"],h.address.nunique())
chk("account nodes",dat("c03_account_count")[0]["n"],len(set(a.address)|set(b.address)))
ov=a.merge(b,on="address"); chk("account both",dat("c04_account_both")[0]["n"],len(ov)); chk("overlap label agree",dat("c04_account_both")[0]["agree"],int((ov.is_fraud==ov.flag).sum()))
g=h.groupby("label").agg(n=("label","size"),income=("income_btc","sum"),l=("length","mean")).sort_values("n",ascending=False).head(6)
for r,(k,v) in zip(dat("c05_heist_by_label"),g.iterrows()):
    chk(f"heist {k} n",r["n"],int(v.n)); chk(f"heist {k} income_btc sum",r["income"],float(v.income),1e-9); chk(f"heist {k} mean length",r["avg_len"],float(v.l))
for r in dat("c06_heist_split_ransom"):
    x=h[h.split==r["split"]]; chk(f"split {r['split']} n",r["n"],len(x)); chk(f"split {r['split']} ransom",r["ransom"],int(x.is_ransomware.sum()))
for r in dat("c07_scamtx_by_chain"):
    x=s[s.blockchain==r["chain"]]; chk(f"chain {r['chain']} n",r["n"],len(x)); chk(f"chain {r['chain']} scams",r["scams"],int(x.is_scam.sum())); chk(f"chain {r['chain']} mean amt",r["mean_amt"],float(x.transaction_amount_usd.mean()))
for r in dat("c08_msg_by_type"):
    x=m[m.scam_type==r["t"]]; chk(f"msg {r['t']} n",r["n"],len(x)); chk(f"msg {r['t']} mean return",r["mean_ret"],float(x.promised_return_pct.mean()))
r=dat("c09_txrisk_rows")[0]; chk("txrisk n",r["n"],len(t)); chk("txrisk risky",r["risky"],int(t.risk_label.sum())); chk("txrisk mean amt",r["mean_amt"],float(t.transaction_amount.mean()))
r=dat("c10_payment_rows")[0]; chk("pay n",r["n"],len(p)); chk("pay fraud",r["fraud"],int(p.fraud_label.sum())); chk("pay mean amt",r["mean_amt"],float(p.transaction_amount.mean()))
chk("wallet nodes",dat("c12_wallet_nodes")[0]["n"],t.wallet_id.nunique()); chk("user nodes",dat("c13_user_nodes")[0]["n"],p.user_id.nunique())
d=dat("s01_describe_income")[0]; x=h.income_btc
for k,e in [("count",len(x)),("min",x.min()),("p25",x.quantile(.25)),("median",x.median()),("p75",x.quantile(.75)),("p95",x.quantile(.95)),("max",x.max()),("mean",x.mean()),("stdev",x.std())]: chk(f"describe income {k}",d[k],float(e),1e-6)
c=dat("s02_corr_len_income")[0]; chk("pearson",c["pearson"],float(h.length.corr(h.income_btc)),1e-9); chk("spearman",c["spearman"],float(h.length.corr(h.income_btc,method="spearman")),1e-6)
hist=dat("s03_hist_log_income"); cnt,_=np.histogram(h.log10_income,bins=8,range=(h.log10_income.min(),h.log10_income.max())); chk("histogram counts",[r["count"] for r in hist],[int(v) for v in cnt])
med=x.median(); mad=(x-med).abs().median(); rz=0.6745*(x-med)/mad; chk("outliers |robust z|>=3.5",dat("s04_outliers_income")[0]["n"],int((rz.abs()>=3.5).sum()))
r=dat("s05_ts_scamtx")[0]; chk("timeseries total",r["total"],len(s))
dd=dat("s07_describe_daily_tx")[0]; chk("daily tx median",dd["median"],float(dn.transactions.median())); chk("daily tx mean",dd["mean"],float(dn.transactions.mean()),1e-9)
# graph: tx-wallet + payment cliques
G=nx.Graph()
for tx,w in zip(t.transaction_id,t.wallet_id): G.add_edge("T:"+tx,"W:"+w)
for tx,u,l in zip(p.transaction_id,p.user_id,p.device_location):
    for x_,y_ in [("P:"+tx,"U:"+u),("P:"+tx,"L:"+l),("U:"+u,"L:"+l)]: G.add_edge(x_,y_)
cc=sorted((len(c) for c in nx.connected_components(G)),reverse=True)
top=dat("g01_components_top"); import json as _j
nodes_total=508189  # live_nodes from the load log; every non-graph node is an isolated component
chk("components count (graph comps + isolated rows)",dat("g01b_components_n")[0]["comps"],len(cc)+nodes_total-G.number_of_nodes()); chk("largest 3 components",[r["size"] for r in top],cc[:3])
bad=[o for o in out if not o[3]]
print(len(out),"checks;",len(out)-len(bad),"match"); [print("MISMATCH",o) for o in bad]
json.dump([[a,str(b),str(c),d] for a,b,c,d in out],open("compare.json","w"))
