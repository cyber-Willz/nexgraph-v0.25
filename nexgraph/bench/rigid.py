import numpy as np, itertools, pandas as pd, glob, sys
from plant2 import *
def detect(d, win, eps=1.5, tol=0.8, persist=0.6, min_windows=3, min_size=3):
    d = d.copy(); d["w"] = np.floor(d["time"]/win).astype(int)
    m = {k: g[["px","py","pz"]].values.mean(0) for k, g in d.groupby(["id","w"]) if len(g) >= 3}
    ids = sorted({i for i,_ in m}); ws = sorted({w for _,w in m}); co = {}
    for w in ws[1:]:
        pres = [i for i in ids if (i,w) in m and (i,w-1) in m]
        for a,b in itertools.combinations(pres,2):
            s1 = m[(a,w)]-m[(b,w)]; s0 = m[(a,w-1)]-m[(b,w-1)]
            c = co.setdefault((a,b),[0,0]); c[0]+=1
            if np.linalg.norm(s1)<=eps and np.linalg.norm(s0)<=eps and np.linalg.norm(s1-s0)<=tol: c[1]+=1
    par={i:i for i in ids}
    def f(x):
        while par[x]!=x: par[x]=par[par[x]]; x=par[x]
        return x
    for (a,b),(c,l) in co.items():
        if c>=min_windows and l>=persist*c: par[f(a)]=f(b)
    comps={}
    for i in ids: comps.setdefault(f(i),set()).add(i)
    return [(s,len(s),"rigid") for s in comps.values() if len(s)>=min_size]
if __name__=="__main__":
    win=float(sys.argv[1]); tol=float(sys.argv[2])
    fa=sum(len(detect(load(f).rename(columns={}).assign(id=lambda x:x.id.astype(str)),win,tol=tol)) for f in files)
    print(f"win {win} tol {tol}: false groups on 36 real uncoordinated files = {fa}")
    rng=np.random.default_rng(11); sel=list(rng.choice(len(files),12,replace=False)); rows=[]
    for scen,sg in itertools.product(["noise","rotate","two_far","escort"],[0.05,0.3,0.6]):
        F=[];fx=0;fg=0;fo=0
        for j,bi in enumerate(sel):
            d,tr=make2(files[bi],files[(bi+5)%len(files)],files[(bi+11)%len(files)],scen,sg,j)
            gs=detect(d,win,tol=tol); f1s,g,mg=score2(gs,tr); F.append(np.mean(f1s)); fx+=sum(x>=.99 for x in f1s); fg+=g; fo+=len(tr)
        rows.append((scen,sg,round(np.mean(F),3),f"{fx}/{12*len(tr)}",fg))
    print(pd.DataFrame(rows,columns=["scen","sigma","f1","exact","false"]).to_string(index=False))
