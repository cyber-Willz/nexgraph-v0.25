//! Data explorer: the BI projection (nodes, edges, per-label tables) with time travel, search, sort and export.
use serde_json::Value;
use wasm_bindgen_futures::spawn_local;
use yew::prelude::*;
use yew_router::prelude::*;

use crate::api;
use crate::browser;
use crate::components::data::{show, use_fetch};
use crate::components::fields::TextField;
use crate::components::table::{Column, DataTable, Kind};
use crate::components::ui::{Alert, Badge, Button, Card, PageHeader, Pager};
use crate::{util, Route};

/// `?as_of=` suffix for a typed timestamp ("" when empty or not a whole number).
pub fn as_of_param(s: &str) -> Vec<(String, String)> {
    match s.trim().parse::<u64>() {
        Ok(n) => vec![("as_of".into(), n.to_string())],
        Err(_) => vec![],
    }
}

/// Case-insensitive substring match across every cell.
pub fn matches_row(row: &Value, needle: &str) -> bool {
    let n = needle.trim().to_lowercase();
    n.is_empty() || row.as_object().map(|m| m.values().any(|v| match v { Value::String(s) => s.to_lowercase().contains(&n), Value::Null => false, o => o.to_string().to_lowercase().contains(&n) })).unwrap_or(false)
}

#[function_component(DataPage)]
pub fn data_page() -> Html {
    let as_of = use_state(String::new);
    let applied = use_state(String::new);
    let path = util::with_query("/v1/bi/tables", &as_of_param(&applied));
    let f = use_fetch(path, 0);
    let apply = { let (a, s) = (applied.clone(), as_of.clone()); Callback::from(move |_| a.set((*s).clone())) };
    html! {
        <>
            <PageHeader title="Data explorer" subtitle="A flat, typed projection of the graph: nodes, edges, edge members and one table per label.">
                <div style="display:flex;gap:8px;align-items:flex-end"><TextField label="As of (timestamp)" value={as_of.clone()} hint="empty = latest" /><Button onclick={apply} icon="refresh">{ "Apply" }</Button></div>
            </PageHeader>
            { show(&f, |v| {
                let rows: Vec<Value> = v.get("tables").and_then(|t| t.as_array()).cloned().unwrap_or_default().into_iter().map(|mut t| {
                    let cols = t.get("columns").and_then(|c| c.as_array()).map(|c| c.len()).unwrap_or(0);
                    if let Some(m) = t.as_object_mut() { m.insert("columns".into(), Value::from(cols)); }
                    t }).collect();
                let columns = vec![Column::plain("name"), { let mut c = Column::plain("kind"); c.kind = Kind::Badge; c }, { let mut c = Column::plain("rows"); c.kind = Kind::Number; c }, { let mut c = Column::plain("columns"); c.kind = Kind::Number; c }];
                html! { <ExplorerTable {columns} rows={rows} ts={v.get("ts").and_then(|t| t.as_u64()).unwrap_or(0)} as_of={(*applied).clone()} /> }
            }) }
        </>
    }
}

#[derive(Properties, PartialEq)]
struct ExplorerProps {
    columns: Vec<Column>,
    rows: Vec<Value>,
    ts: u64,
    as_of: String,
}

#[function_component(ExplorerTable)]
fn explorer_table(p: &ExplorerProps) -> Html {
    let nav = use_navigator().expect("navigator");
    let on_row = {
        let nav = nav.clone();
        Callback::from(move |row: Value| if let Some(n) = row.get("name").and_then(|n| n.as_str()) { nav.push(&Route::Table { name: n.to_string() }) })
    };
    html! {
        <Card title={format!("{} tables · snapshot {}", p.rows.len(), p.ts)} pad=false>
            <DataTable columns={p.columns.clone()} rows={p.rows.clone()} on_row={Some(on_row)} />
        </Card>
    }
}

#[derive(Properties, PartialEq)]
pub struct TableProps {
    pub name: String,
}

#[function_component(TablePage)]
pub fn table_page(p: &TableProps) -> Html {
    let as_of = use_state(String::new);
    let applied = use_state(String::new);
    let q = use_state(String::new);
    let page = use_state(|| 1u32);
    let sort = use_state(|| Option::<(String, bool)>::None);
    let err = use_state(|| Option::<String>::None);
    let mut params = vec![("format".to_string(), "json".to_string())];
    params.extend(as_of_param(&applied));
    let path = util::with_query(&format!("/v1/bi/tables/{}", util::percent_encode(&p.name)), &params);
    let f = use_fetch(path, 0);
    let apply = { let (a, s, pg) = (applied.clone(), as_of.clone(), page.clone()); Callback::from(move |_| { a.set((*s).clone()); pg.set(1); }) };
    let csv = {
        let (name, applied, err) = (p.name.clone(), applied.clone(), err.clone());
        Callback::from(move |_| {
            let mut ps = vec![("format".to_string(), "csv".to_string())];
            ps.extend(as_of_param(&applied));
            let (path, name, err) = (util::with_query(&format!("/v1/bi/tables/{}", util::percent_encode(&name)), &ps), name.clone(), err.clone());
            spawn_local(async move {
                match api::get_text(&path).await { Ok(t) => { let _ = browser::download_text(&format!("{name}.csv"), "text/csv", &t); } Err(e) => err.set(Some(e.message)) }
            });
        })
    };
    let oninput_q = { let (q, pg) = (q.clone(), page.clone()); Callback::from(move |e: InputEvent| { if let Some(i) = e.target_dyn_into::<web_sys::HtmlInputElement>() { q.set(i.value()); pg.set(1); } }) };
    let on_sort = { let sort = sort.clone(); Callback::from(move |k: String| sort.set(match &*sort { Some((c, d)) if *c == k => if *d { None } else { Some((k, true)) }, _ => Some((k, false)) })) };
    html! {
        <>
            <PageHeader title={p.name.clone()} subtitle="Click a header to sort; the search box filters every column.">
                <Link<Route> to={Route::Data}>{ "← All tables" }</Link<Route>>
                <Button icon="download" onclick={csv}>{ "CSV" }</Button>
            </PageHeader>
            { if let Some(m) = &*err { html! { <Alert tone="err">{ m.clone() }</Alert> } } else { html!{} } }
            <Card>
                <div style="display:flex;gap:12px;flex-wrap:wrap;align-items:flex-end">
                    <label style="display:grid;gap:4px;font-size:12px;color:var(--muted);font-weight:550;flex:1;min-width:200px">{ "Search" }
                        <input type="search" value={(*q).clone()} oninput={oninput_q} style="background:var(--surface);border:1px solid var(--line-strong);border-radius:8px;padding:8px 11px;font-size:13.5px;color:var(--ink)" /></label>
                    <TextField label="As of (timestamp)" value={as_of.clone()} hint="empty = latest" /><Button onclick={apply} icon="refresh">{ "Apply" }</Button>
                </div>
            </Card>
            { show(&f, |v| {
                let all: Vec<Value> = v.as_array().cloned().unwrap_or_default();
                let names = all.first().and_then(|r| r.as_object()).map(|m| m.keys().cloned().collect::<Vec<_>>()).unwrap_or_default();
                let columns: Vec<Column> = names.iter().map(|n| { let mut c = Column::auto(n, &all); c.sortable = true; c }).collect();
                let mut rows: Vec<Value> = all.iter().filter(|r| matches_row(r, &q)).cloned().collect();
                if let Some((k, desc)) = &*sort {
                    rows.sort_by(|a, b| { let (x, y) = (a.get(k).unwrap_or(&Value::Null), b.get(k).unwrap_or(&Value::Null));
                        let o = match (x.as_f64(), y.as_f64()) { (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal), _ => x.to_string().cmp(&y.to_string()) };
                        if *desc { o.reverse() } else { o } });
                }
                let total = rows.len();
                let start = ((*page - 1) * 50) as usize;
                let shown: Vec<Value> = rows.into_iter().skip(start).take(50).collect();
                let count = shown.len();
                html! { <Card pad=false title={format!("{} of {} rows", total, all.len())} actions={html! { <Badge text="read-only" tone="info" /> }}>
                    <DataTable {columns} rows={shown} sort={(*sort).clone()} {on_sort} />
                    <div style="padding:0 12px 10px"><Pager page={*page} per_page={50} total={Some(total as u64)} shown={count} on_page={{ let page = page.clone(); Callback::from(move |n| page.set(n)) }} /></div>
                </Card> }
            }) }
        </>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn as_of() {
        assert_eq!(as_of_param(" 42 "), vec![("as_of".to_string(), "42".to_string())]);
        assert!(as_of_param("").is_empty() && as_of_param("-1").is_empty() && as_of_param("x").is_empty());
    }
    #[test]
    fn search() {
        let r = json!({"name":"Ada Lovelace","n":42,"x":null});
        assert!(matches_row(&r, "love") && matches_row(&r, "42") && matches_row(&r, "  ") && !matches_row(&r, "bob"));
    }
}
