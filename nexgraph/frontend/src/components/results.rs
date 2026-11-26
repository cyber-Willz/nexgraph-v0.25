//! Result viewer shared by the workbench and the analytics launcher: summary tiles, bar chart, paged table, CSV.
use serde_json::Value;
use yew::prelude::*;

use crate::api::QueryOut;
use crate::browser;
use crate::components::charts::BarChart;
use crate::components::table::{Column, DataTable};
use crate::components::ui::{Button, Card, Grid, Pager, StatTile};
use crate::util;

pub const PAGE: u32 = 50;

/// Top-N (label, value) pairs when the result has a label column followed by a numeric one.
/// Label = first column; value = first later column whose non-null cells are all numbers.
pub fn chart_series(names: &[String], rows: &[Value], top: usize) -> Option<Vec<(String, f64)>> {
    if rows.len() < 2 || rows.len() > 20_000 || names.len() < 2 {
        return None;
    }
    let label = &names[0];
    let value = names[1..].iter().find(|c| {
        let mut any = false;
        let ok = rows.iter().all(|r| match r.get(*c) {
            Some(Value::Number(_)) => {
                any = true;
                true
            }
            Some(Value::Null) | None => true,
            _ => false,
        });
        ok && any && !matches!(c.as_str(), "id" | "ts" | "component" | "cluster" | "rank")
    })?;
    let mut pts: Vec<(String, f64)> = rows
        .iter()
        .filter_map(|r| {
            let v = r.get(value)?.as_f64().filter(|f| f.is_finite())?;
            let l = match r.get(label)? {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            Some((l, v))
        })
        .collect();
    pts.sort_by(|a, b| b.1.abs().partial_cmp(&a.1.abs()).unwrap_or(std::cmp::Ordering::Equal));
    pts.truncate(top);
    if pts.len() < 2 { None } else { Some(pts) }
}

#[derive(Properties, PartialEq)]
pub struct ResultProps {
    pub out: QueryOut,
    #[prop_or("result".into())]
    pub file_stem: AttrValue,
}

#[function_component(ResultView)]
pub fn result_view(p: &ResultProps) -> Html {
    let page = use_state(|| 1u32);
    let sort = use_state(|| Option::<(String, bool)>::None);
    let (names, mut rows) = p.out.records();
    {
        // a new result starts at page 1
        let page = page.clone();
        use_effect_with(p.out.clone(), move |_| {
            page.set(1);
            || ()
        });
    }
    if let Some((k, desc)) = &*sort {
        rows.sort_by(|a, b| {
            let (x, y) = (a.get(k).unwrap_or(&Value::Null), b.get(k).unwrap_or(&Value::Null));
            let o = match (util::as_f64(x).filter(|_| !x.is_string()), util::as_f64(y).filter(|_| !y.is_string())) {
                (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
                _ => x.to_string().cmp(&y.to_string()),
            };
            if *desc { o.reverse() } else { o }
        });
    }
    let columns: Vec<Column> = names.iter().map(|n| { let mut c = Column::auto(n, &rows); c.sortable = true; c }).collect();
    let series = chart_series(&names, &rows, 15);
    let total = rows.len();
    let start = ((*page - 1) * PAGE) as usize;
    let shown: Vec<Value> = rows.iter().skip(start).take(PAGE as usize).cloned().collect();
    let on_sort = {
        let sort = sort.clone();
        Callback::from(move |k: String| {
            sort.set(match &*sort {
                Some((cur, desc)) if *cur == k => if *desc { None } else { Some((k, true)) },
                _ => Some((k, false)),
            })
        })
    };
    let export = {
        let (rows, names, stem) = (rows.clone(), names.clone(), p.file_stem.to_string());
        Callback::from(move |_| { let _ = browser::download_text(&format!("{stem}.csv"), "text/csv", &browser::to_csv(&rows, &names)); })
    };
    let s = &p.out.stats;
    html! {
        <>
            <Grid min={130}>
                <StatTile label="Rows" value={util::integer(total as f64)} />
                <StatTile label="Columns" value={names.len().to_string()} />
                <StatTile label="Snapshot ts" value={p.out.ts.to_string()} />
                { if s.any() { html! { <>
                    <StatTile label="Nodes created" value={s.nodes_created.to_string()} tone="ok" />
                    <StatTile label="Edges created" value={s.edges_created.to_string()} tone="ok" />
                    <StatTile label="Properties set" value={s.props_set.to_string()} tone="ok" />
                    <StatTile label="Deleted" value={s.deleted.to_string()} tone={if s.deleted > 0 { "warn" } else { "" }} />
                </> } } else { html!{} } }
            </Grid>
            <div style="height:14px" />
            { if let Some(pts) = series { html! { <Card title={format!("Top {} by {}", pts.len(), names.get(1).cloned().unwrap_or_default())}><BarChart items={pts} /></Card> } } else { html!{} } }
            <Card title="Rows" pad=false actions={html! { <Button small=true icon="download" onclick={export} disabled={total == 0}>{ "CSV" }</Button> }}>
                <DataTable {columns} rows={shown} sort={(*sort).clone()} {on_sort} />
                <div style="padding:0 12px 10px"><Pager page={*page} per_page={PAGE} total={Some(total as u64)} shown={total.saturating_sub(start).min(PAGE as usize)} on_page={{ let page = page.clone(); Callback::from(move |n| page.set(n)) }} /></div>
            </Card>
        </>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn picks_label_and_numeric_column() {
        let names = vec!["node".to_string(), "score".to_string()];
        let rows = vec![json!({"node":"#1","score":0.1}), json!({"node":"#2","score":0.7}), json!({"node":"#3","score":null})];
        let pts = chart_series(&names, &rows, 15).unwrap();
        assert_eq!(pts[0], ("#2".to_string(), 0.7));
        assert_eq!(pts.len(), 2);
    }

    #[test]
    fn no_chart_for_ids_text_or_single_row() {
        let names = vec!["node".to_string(), "component".to_string()];
        let rows = vec![json!({"node":"#1","component":0}), json!({"node":"#2","component":0})];
        assert!(chart_series(&names, &rows, 5).is_none());
        let names = vec!["a".to_string(), "b".to_string()];
        assert!(chart_series(&names, &[json!({"a":"x","b":"y"}), json!({"a":"z","b":"w"})], 5).is_none());
        assert!(chart_series(&names, &[json!({"a":"x","b":1})], 5).is_none());
    }
}
