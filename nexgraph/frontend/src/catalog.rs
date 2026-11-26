//! Launcher catalogue: the CALL procedures nexgraph ships, with typed parameters. Statements are built with
//! `$param` placeholders (values travel as query parameters, never spliced into the text).
use serde_json::{json, Map, Value};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Ty {
    Int,
    Float,
    Text,
}

#[derive(PartialEq)]
pub struct Param {
    pub name: &'static str,
    pub label: &'static str,
    pub ty: Ty,
    pub default: &'static str,
    pub required: bool,
    pub hint: &'static str,
}

#[derive(PartialEq)]
pub struct Proc {
    pub group: &'static str,
    pub name: &'static str,
    pub title: &'static str,
    pub about: &'static str,
    pub params: &'static [Param],
}

const fn p(name: &'static str, label: &'static str, ty: Ty, default: &'static str, required: bool, hint: &'static str) -> Param {
    Param { name, label, ty, default, required, hint }
}

pub const GROUPS: &[(&str, &str)] = &[
    ("graph", "Graph algorithms"),
    ("spectral", "Spectral & communities"),
    ("physics", "Diffusion (PDE)"),
    ("gaap", "GAAP ledger"),
];

const NONE: &[Param] = &[];
const K: &[Param] = &[p("k", "Clusters / eigenpairs (k)", Ty::Int, "2", false, "number of clusters or eigenpairs")];
const COMPANY: Param = p("company", "Company", Ty::Text, "", true, "ledger company key");
const FROM: Param = p("from", "From", Ty::Text, "", true, "YYYY-MM-DD");
const TO: Param = p("to", "To", Ty::Text, "", true, "YYYY-MM-DD");

pub const PROCS: &[Proc] = &[
    Proc { group: "graph", name: "db.stats", title: "Database statistics", about: "Live nodes, edges, retained versions and the snapshot timestamp.", params: NONE },
    Proc { group: "graph", name: "algo.pagerank", title: "PageRank", about: "Importance of every node by link structure.", params: &[
        p("damping", "Damping", Ty::Float, "0.85", false, "0–1"), p("iters", "Iterations", Ty::Int, "50", false, "1–1000")] },
    Proc { group: "graph", name: "algo.components", title: "Connected components", about: "Component id per node.", params: NONE },
    Proc { group: "graph", name: "algo.degree", title: "Degree", about: "In/out degree, distinct neighbours and weights; optionally for one edge label.", params: &[
        p("label", "Edge label", Ty::Text, "", false, "leave empty for all edges")] },
    Proc { group: "graph", name: "algo.betweenness", title: "Betweenness centrality", about: "Nodes that sit on many shortest paths.", params: NONE },
    Proc { group: "graph", name: "algo.closeness", title: "Closeness centrality", about: "How near a node is to all others.", params: NONE },
    Proc { group: "graph", name: "algo.kcore", title: "k-core", about: "Core number per node (densely connected cores).", params: NONE },
    Proc { group: "graph", name: "algo.triangles", title: "Triangle counts", about: "Triangles through each node.", params: NONE },
    Proc { group: "graph", name: "algo.scc", title: "Strongly connected components", about: "Directed cycles / mutual reachability groups.", params: NONE },
    Proc { group: "graph", name: "algo.articulation", title: "Articulation points", about: "Nodes whose removal disconnects the graph.", params: NONE },
    Proc { group: "graph", name: "algo.louvain", title: "Louvain communities", about: "Modularity-based community detection.", params: NONE },
    Proc { group: "spectral", name: "spectral.cluster", title: "Spectral clustering", about: "k-way clustering on the normalised hypergraph Laplacian.", params: K },
    Proc { group: "spectral", name: "spectral.fiedler", title: "Fiedler vector", about: "Second eigenvector: the natural two-way cut.", params: NONE },
    Proc { group: "spectral", name: "spectral.eigenvalues", title: "Laplacian eigenvalues", about: "Smallest k eigenvalues of the hypergraph Laplacian.", params: K },
    Proc { group: "spectral", name: "spectral.embedding", title: "Spectral embedding", about: "k-dimensional coordinates per node.", params: K },
    Proc { group: "spectral", name: "spectral.communities", title: "Non-backtracking communities", about: "Bethe Hessian communities; k is found automatically.", params: NONE },
    Proc { group: "spectral", name: "spectral.nb_radius", title: "Non-backtracking radius", about: "Spectral radius of the Hashimoto operator.", params: NONE },
    Proc { group: "physics", name: "physics.heat", title: "Heat diffusion", about: "Diffuse a numeric node property for time t (exact, by uniformisation).", params: &[
        p("key", "Property", Ty::Text, "", true, "numeric node property to diffuse"), p("t", "Time t", Ty::Float, "1", true, "0–10000"),
        p("mode", "Mode", Ty::Text, "rw", false, "rw (random walk) or sym (normalised Laplacian)")] },
    Proc { group: "gaap", name: "gaap.audit", title: "Ledger audit", about: "Integrity checks over a company's ledger.", params: &[COMPANY] },
    Proc { group: "gaap", name: "gaap.balance_sheet", title: "Balance sheet", about: "Statement of financial position as of a date.", params: &[
        COMPANY, p("asof", "As of", Ty::Text, "", true, "YYYY-MM-DD")] },
    Proc { group: "gaap", name: "gaap.income_statement", title: "Income statement", about: "Revenue and expenses for a period.", params: &[COMPANY, FROM, TO] },
    Proc { group: "gaap", name: "gaap.trial_balance", title: "Trial balance", about: "Debits and credits per account for a period.", params: &[
        COMPANY, p("type", "Type", Ty::Text, "adjusted", true, "unadjusted, adjusted or post-closing"),
        p("start", "Start", Ty::Text, "", true, "YYYY-MM-DD"), p("end", "End", Ty::Text, "", true, "YYYY-MM-DD")] },
    Proc { group: "gaap", name: "gaap.ratios", title: "Financial ratios", about: "Liquidity, leverage, efficiency and profitability ratios.", params: &[COMPANY, FROM, TO] },
    Proc { group: "gaap", name: "gaap.aging", title: "Receivable / payable aging", about: "Open items by age bucket, tied to the ledger balance.", params: &[
        COMPANY, p("account", "Account", Ty::Text, "", true, "account number"), p("asof", "As of", Ty::Text, "", true, "YYYY-MM-DD")] },
];

pub fn in_group(g: &str) -> impl Iterator<Item = &'static Proc> + '_ {
    PROCS.iter().filter(move |p| p.group == g)
}
pub fn find(name: &str) -> Option<&'static Proc> {
    PROCS.iter().find(|p| p.name == name)
}

/// Build `(statement, params)` from the user's field values (same order as `proc.params`).
/// Trailing optional parameters left empty are dropped; an empty required one is an error.
pub fn build(proc: &Proc, values: &[String]) -> Result<(String, Map<String, Value>), String> {
    let mut args: Vec<&Param> = Vec::new();
    let mut params = Map::new();
    let last_used = proc.params.iter().enumerate().rev().find(|(i, q)| q.required || !values.get(*i).map(|v| v.trim().is_empty()).unwrap_or(true)).map(|(i, _)| i + 1).unwrap_or(0);
    for (i, q) in proc.params.iter().enumerate().take(last_used) {
        let raw = values.get(i).map(|v| v.trim()).unwrap_or("");
        let raw = if raw.is_empty() { q.default } else { raw };
        if raw.is_empty() {
            if q.required {
                return Err(format!("{} is required.", q.label));
            }
            // a gap before a later value: pass null so positions stay right
            params.insert(q.name.into(), Value::Null);
            args.push(q);
            continue;
        }
        let v = match q.ty {
            Ty::Int => json!(raw.parse::<i64>().map_err(|_| format!("{} must be a whole number.", q.label))?),
            Ty::Float => json!(raw.parse::<f64>().ok().filter(|f| f.is_finite()).ok_or_else(|| format!("{} must be a number.", q.label))?),
            Ty::Text => json!(raw),
        };
        params.insert(q.name.into(), v);
        args.push(q);
    }
    let list = args.iter().map(|q| format!("${}", q.name)).collect::<Vec<_>>().join(", ");
    Ok((format!("CALL {}({list})", proc.name), params))
}

pub fn default_values(proc: &Proc) -> Vec<String> {
    proc.params.iter().map(|q| q.default.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn names_are_unique_and_groups_exist() {
        let mut seen = std::collections::HashSet::new();
        for q in PROCS {
            assert!(seen.insert(q.name), "{}", q.name);
            assert!(GROUPS.iter().any(|(g, _)| *g == q.group));
            assert!(q.name.starts_with(&format!("{}.", if q.group == "graph" { if q.name.starts_with("db.") { "db" } else { "algo" } } else { q.group })));
        }
    }

    #[test]
    fn statements_build_with_placeholders() {
        let pr = find("algo.pagerank").unwrap();
        let (s, p) = build(pr, &default_values(pr)).unwrap();
        assert_eq!(s, "CALL algo.pagerank($damping, $iters)");
        assert_eq!(p["iters"], json!(50));
        // all-empty optional trailing params are dropped
        let dg = find("algo.degree").unwrap();
        assert_eq!(build(dg, &[String::new()]).unwrap().0, "CALL algo.degree()");
        assert_eq!(build(dg, &["K".into()]).unwrap().0, "CALL algo.degree($label)");
        // required
        let bs = find("gaap.balance_sheet").unwrap();
        assert!(build(bs, &["".into(), "2025-12-31".into()]).unwrap_err().contains("Company"));
        // injection stays in the parameter value, not the statement
        let (s, p) = build(bs, &["x') DELETE".into(), "2025-12-31".into()]).unwrap();
        assert_eq!(s, "CALL gaap.balance_sheet($company, $asof)");
        assert_eq!(p["company"], json!("x') DELETE"));
        assert!(build(pr, &["abc".into(), "".into()]).is_err());
    }
}
