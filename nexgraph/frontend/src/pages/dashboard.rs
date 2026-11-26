//! Landing page: engine statistics, what the graph contains, recent activity, shortcuts.
use serde_json::Value;
use yew::prelude::*;
use yew_router::prelude::*;

use crate::components::charts::BarChart;
use crate::components::data::{show, use_fetch};
use crate::components::table::{Column, DataTable};
use crate::components::ui::{Badge, Button, Card, Grid, PageHeader, StatTile, Variant};
use crate::{roles, util, Route, Session};

fn n(v: &Value, k: &str) -> f64 {
    v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0)
}

#[function_component(StatsCard)]
fn stats_card() -> Html {
    let f = use_fetch("/v1/stats".into(), 0);
    html! { <Card title="Engine">{ show(&f, |v| html! {
        <Grid min={140}>
            <StatTile label="Nodes" value={util::integer(n(v, "nodes"))} />
            <StatTile label="Edges" value={util::integer(n(v, "edges"))} />
            <StatTile label="Retained versions" value={util::integer(n(v, "versions"))} hint="history kept for AS OF queries" />
            <StatTile label="Property indexes" value={util::integer(n(v, "prop_indexes"))} />
            <StatTile label="Vector indexes" value={util::integer(n(v, "vector_indexes"))} />
        </Grid> }) }</Card> }
}

/// Row counts per node label / edge label table of the BI projection.
fn label_counts(cat: &Value) -> (Vec<(String, f64)>, Vec<(String, f64)>) {
    let mut nodes = vec![];
    let mut edges = vec![];
    for t in cat.get("tables").and_then(|t| t.as_array()).into_iter().flatten() {
        let name = t.get("name").and_then(|x| x.as_str()).unwrap_or_default();
        let rows = t.get("rows").and_then(|x| x.as_f64()).unwrap_or(0.0);
        match t.get("kind").and_then(|x| x.as_str()) {
            Some("node_label") => nodes.push((t.get("label").and_then(|x| x.as_str()).unwrap_or(name).to_string(), rows)),
            Some("edge_label") => edges.push((t.get("label").and_then(|x| x.as_str()).unwrap_or(name).to_string(), rows)),
            _ => {}
        }
    }
    for v in [&mut nodes, &mut edges] {
        v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        v.truncate(12);
    }
    (nodes, edges)
}

#[function_component(ContentsCard)]
fn contents_card() -> Html {
    let f = use_fetch("/v1/bi/tables".into(), 0);
    html! { <>{ show(&f, |v| {
        let (nodes, edges) = label_counts(v);
        if nodes.is_empty() && edges.is_empty() {
            return html! { <Card title="Contents"><p style="color:var(--muted);margin:0">{ "The graph is empty. Create data in the " }<Link<Route> to={Route::Query}>{ "query workbench" }</Link<Route>>{ "." }</p></Card> };
        }
        html! { <Grid min={320}>
            { if nodes.is_empty() { html!{} } else { html! { <Card title="Nodes by label"><BarChart items={nodes} /></Card> } } }
            { if edges.is_empty() { html!{} } else { html! { <Card title="Edges by label"><BarChart items={edges} /></Card> } } }
        </Grid> }
    }) }</>}
}

#[function_component(RecentAudit)]
fn recent_audit() -> Html {
    let f = use_fetch("/v1/audit?limit=8".into(), 0);
    html! { <Card title="Recent activity" pad=false actions={html! { <Link<Route> to={Route::Audit}>{ "All entries" }</Link<Route>> }}>{ show(&f, |v| {
        let rows = v.get("entries").and_then(|e| e.as_array()).cloned().unwrap_or_default();
        let cols: Vec<Column> = ["ts", "actor", "action", "outcome", "detail"].iter().map(|k| { let mut c = Column::plain(k); if *k == "outcome" { c.kind = crate::components::table::Kind::Badge; } c }).collect();
        html! { <DataTable columns={cols} rows={rows} /> }
    }) }</Card> }
}

#[function_component(TenantsCard)]
fn tenants_card() -> Html {
    let f = use_fetch("/v1/admin/tenants".into(), 0);
    html! { <Card title="Tenants" actions={html! { <Link<Route> to={Route::Tenants}>{ "Manage" }</Link<Route>> }}>{ show(&f, |v| {
        let ts = v.get("tenants").and_then(|e| e.as_array()).cloned().unwrap_or_default();
        let off = ts.iter().filter(|t| t.get("disabled").and_then(|d| d.as_bool()).unwrap_or(false)).count();
        html! { <Grid min={140}>
            <StatTile label="Tenants" value={ts.len().to_string()} />
            <StatTile label="Disabled" value={off.to_string()} tone={if off > 0 { "warn" } else { "" }} />
        </Grid> }
    }) }</Card> }
}

#[function_component(DashboardPage)]
pub fn dashboard() -> Html {
    let s = use_context::<Session>().expect("session");
    let u = &s.user;
    let nav = use_navigator().expect("navigator");
    let go = |r: Route| { let nav = nav.clone(); Callback::from(move |_| nav.push(&r)) };
    html! {
        <>
            <PageHeader title={format!("Welcome, {}", u.user)} subtitle={match &u.tenant { Some(t) => format!("Tenant {t} · {}", u.role.replace('_', " ")), None => format!("Platform · {}", u.role.replace('_', " ")) }}>
                { if roles::has_data(u) { html! { <>
                    <Button variant={Variant::Primary} icon="terminal" onclick={go(Route::Query)}>{ "Open workbench" }</Button>
                    <Button icon="table" onclick={go(Route::Data)}>{ "Data explorer" }</Button>
                </> } } else { html! { <Button variant={Variant::Primary} icon="shield" onclick={go(Route::Tenants)}>{ "Manage tenants" }</Button> } } }
            </PageHeader>
            { if roles::has_data(u) { html! { <><StatsCard /><ContentsCard /></> } } else { html! {
                <Card><span style="color:var(--muted)">{ "Platform administrators manage tenants and accounts but cannot read tenant data. Sign in with a tenant account to query a graph. " }<Badge text="platform admin" tone="info" /></span></Card> } } }
            { if roles::is_platform(u) { html! { <TenantsCard /> } } else { html!{} } }
            { if roles::can_audit(u) { html! { <RecentAudit /> } } else { html!{} } }
        </>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn label_counts_split_node_and_edge_tables() {
        let cat = json!({"ts":1,"tables":[
            {"name":"nodes","kind":"nodes","rows":9,"columns":[]},
            {"name":"node_Person","kind":"node_label","label":"Person","rows":5,"columns":[]},
            {"name":"node_Org","kind":"node_label","label":"Org","rows":7,"columns":[]},
            {"name":"edge_KNOWS","kind":"edge_label","label":"KNOWS","rows":3,"columns":[]}]});
        let (n, e) = label_counts(&cat);
        assert_eq!(n, vec![("Org".to_string(), 7.0), ("Person".to_string(), 5.0)]);
        assert_eq!(e, vec![("KNOWS".to_string(), 3.0)]);
    }
}
