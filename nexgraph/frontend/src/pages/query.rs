//! NQL workbench: editor, parameters, history, results.
use gloo_storage::{LocalStorage, Storage};
use serde_json::{Map, Value};
use wasm_bindgen_futures::spawn_local;
use yew::prelude::*;

use crate::api::{self, QueryOut};
use crate::components::fields::AreaField;
use crate::components::results::ResultView;
use crate::components::ui::{Alert, Button, Card, PageHeader, Variant};
use crate::{roles, Session};

pub const DRAFT_KEY: &str = "nexgraph.draft";
const HISTORY_KEY: &str = "nexgraph.history";
const HISTORY_MAX: usize = 20;

const EXAMPLES: &[(&str, &str)] = &[
    ("Database statistics", "CALL db.stats()"),
    ("Count nodes", "MATCH (n) RETURN count(*) AS nodes"),
    ("Count edges", "MATCH (a)-[r]->(b) RETURN count(*) AS edges"),
    ("First 25 nodes", "MATCH (n) RETURN n LIMIT 25"),
    ("PageRank", "CALL algo.pagerank(0.85, 50)"),
];

/// Parse the parameters box: empty, or a JSON object of scalars / lists (nested objects are not valid NQL values).
pub fn parse_params(text: &str) -> Result<Map<String, Value>, String> {
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(m)) => {
            if let Some((k, _)) = m.iter().find(|(_, v)| v.is_object()) {
                return Err(format!("Parameter ${k} is an object; use numbers, strings, booleans or lists."));
            }
            Ok(m)
        }
        Ok(_) => Err("Parameters must be a JSON object, e.g. {\"name\": \"Ada\"}.".into()),
        Err(e) => Err(format!("Parameters are not valid JSON: {e}")),
    }
}

/// Newest first, no duplicates, capped.
pub fn push_history(mut h: Vec<String>, q: &str) -> Vec<String> {
    let q = q.trim().to_string();
    if q.is_empty() {
        return h;
    }
    h.retain(|x| *x != q);
    h.insert(0, q);
    h.truncate(HISTORY_MAX);
    h
}

fn load_history() -> Vec<String> {
    LocalStorage::get::<Vec<String>>(HISTORY_KEY).unwrap_or_default()
}

#[function_component(QueryPage)]
pub fn query_page() -> Html {
    let s = use_context::<Session>().expect("session");
    let can_write = roles::can_write(&s.user);
    let text = use_state(|| LocalStorage::get::<String>(DRAFT_KEY).unwrap_or_else(|_| EXAMPLES[0].1.to_string()));
    let params = use_state(String::new);
    let busy = use_state(|| false);
    let result = use_state(|| Option::<Result<QueryOut, String>>::None);
    let history = use_state(load_history);

    let run = {
        let (text, params, busy, result, history) = (text.clone(), params.clone(), busy.clone(), result.clone(), history.clone());
        Callback::from(move |_: ()| {
            if *busy { return; }
            let q = (*text).clone();
            if q.trim().is_empty() { result.set(Some(Err("Type a statement first.".into()))); return; }
            let p = match parse_params(&params) { Ok(p) => p, Err(e) => { result.set(Some(Err(e))); return; } };
            let _ = LocalStorage::set(DRAFT_KEY, &q);
            busy.set(true);
            let (busy, result, history) = (busy.clone(), result.clone(), history.clone());
            spawn_local(async move {
                let r = api::run_query(&q, &p).await.map_err(|e| e.message);
                if r.is_ok() {
                    let h = push_history((*history).clone(), &q);
                    let _ = LocalStorage::set(HISTORY_KEY, &h);
                    history.set(h);
                }
                result.set(Some(r));
                busy.set(false);
            });
        })
    };
    let pick = |q: String| { let text = text.clone(); Callback::from(move |_| text.set(q.clone())) };
    let clear_history = { let h = history.clone(); Callback::from(move |_| { LocalStorage::delete(HISTORY_KEY); h.set(vec![]); }) };
    let run_click = { let r = run.clone(); Callback::from(move |_| r.emit(())) };

    html! {
        <>
            <PageHeader title="Query workbench" subtitle="NQL, the Cypher-style language of nexgraph. Ctrl/⌘+Enter runs the statement." />
            { if !can_write { html! { <Alert tone="info">{ "Your role is read-only: MATCH and CALL statements run, statements that change data are refused by the server." }</Alert> } } else { html!{} } }
            <Card>
                <AreaField label="Statement" value={text.clone()} rows={8} mono=true on_submit={run.clone()} />
                <div style="height:10px" />
                <AreaField label="Parameters (JSON, optional)" value={params.clone()} rows={2} mono=true on_submit={run.clone()} hint="Reference them as $name in the statement, e.g. {\"name\": \"Ada\"}." />
                <div style="height:12px;display:flex;gap:8px;align-items:center;flex-wrap:wrap">
                    <Button variant={Variant::Primary} icon="play" busy={*busy} onclick={run_click}>{ "Run" }</Button>
                    <span style="color:var(--muted);font-size:12.5px">{ "Examples:" }</span>
                    { for EXAMPLES.iter().map(|(l, q)| html! { <Button small=true variant={Variant::Ghost} onclick={pick(q.to_string())}>{ *l }</Button> }) }
                </div>
            </Card>
            { match &*result {
                None => html!{},
                Some(Err(e)) => html! { <Alert tone="err">{ e.clone() }</Alert> },
                Some(Ok(out)) => html! { <ResultView out={out.clone()} file_stem="query" /> },
            } }
            { if history.is_empty() { html!{} } else { html! {
                <Card title="History (this browser)" actions={html! { <Button small=true variant={Variant::Ghost} onclick={clear_history}>{ "Clear" }</Button> }}>
                    <div style="display:grid;gap:4px">
                        { for history.iter().map(|q| html! { <button type="button" onclick={pick(q.clone())} style="text-align:left;background:none;border:0;padding:4px 6px;border-radius:6px;cursor:pointer;color:var(--ink-2);font-family:var(--mono);font-size:12.5px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis" title={q.clone()}>{ q.lines().next().unwrap_or_default().to_string() }</button> }) }
                    </div>
                </Card> } } }
        </>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn params() {
        assert!(parse_params("  ").unwrap().is_empty());
        assert_eq!(parse_params(r#"{"a":1,"b":[1,2]}"#).unwrap().len(), 2);
        assert!(parse_params("[1]").is_err());
        assert!(parse_params(r#"{"a":{"b":1}}"#).unwrap_err().contains("$a"));
        assert!(parse_params("{oops").unwrap_err().contains("valid JSON"));
    }
    #[test]
    fn history_is_deduped_newest_first_and_capped() {
        let h = push_history(vec!["b".into(), "a".into()], "a");
        assert_eq!(h, ["a", "b"]);
        let mut h = vec![];
        for i in 0..30 { h = push_history(h, &format!("q{i}")); }
        assert_eq!(h.len(), HISTORY_MAX);
        assert_eq!(h[0], "q29");
        assert_eq!(push_history(h.clone(), "  "), h);
    }
}
