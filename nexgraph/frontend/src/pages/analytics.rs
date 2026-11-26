//! Analytics launcher: pick a procedure, fill its parameters, run, read the result.
use gloo_storage::{LocalStorage, Storage};
use wasm_bindgen_futures::spawn_local;
use yew::prelude::*;
use yew_router::prelude::*;

use crate::api::{self, QueryOut};
use crate::catalog::{self, Proc, Ty};
use crate::components::results::ResultView;
use crate::components::ui::{Alert, Button, Card, Grid, PageHeader, Variant};
use crate::pages::query::DRAFT_KEY;
use crate::Route;

#[derive(Properties, PartialEq)]
pub struct Props {
    pub group: String,
}

#[derive(Properties, PartialEq)]
struct RunnerProps {
    proc: &'static Proc,
}

#[function_component(Runner)]
fn runner(p: &RunnerProps) -> Html {
    let values = use_state(|| catalog::default_values(p.proc));
    let busy = use_state(|| false);
    let out = use_state(|| Option::<Result<QueryOut, String>>::None);
    let nav = use_navigator().expect("navigator");
    let built = catalog::build(p.proc, &values);
    let stmt = built.as_ref().map(|(s, _)| s.clone()).unwrap_or_default();

    let run = {
        let (values, busy, out) = (values.clone(), busy.clone(), out.clone());
        let proc = p.proc;
        Callback::from(move |e: SubmitEvent| {
            e.prevent_default();
            let (q, params) = match catalog::build(proc, &values) { Ok(x) => x, Err(m) => { out.set(Some(Err(m))); return; } };
            busy.set(true);
            let (busy, out) = (busy.clone(), out.clone());
            spawn_local(async move {
                out.set(Some(api::run_query(&q, &params).await.map_err(|e| e.message)));
                busy.set(false);
            });
        })
    };
    let open_in_workbench = {
        let (nav, stmt) = (nav.clone(), stmt.clone());
        Callback::from(move |_| { let _ = LocalStorage::set(DRAFT_KEY, &stmt); nav.push(&Route::Query); })
    };
    html! {
        <Card title={p.proc.title} actions={html! { <code style="color:var(--muted)">{ p.proc.name }</code> }}>
            <p style="margin:0 0 12px;color:var(--ink-2)">{ p.proc.about }</p>
            <form onsubmit={run} style="display:grid;grid-template-columns:repeat(auto-fill,minmax(210px,1fr));gap:12px;align-items:end">
                { for p.proc.params.iter().enumerate().map(|(i, q)| {
                    let cur = values[i].clone();
                    let values = values.clone();
                    let oninput = Callback::from(move |e: InputEvent| {
                        if let Some(inp) = e.target_dyn_into::<web_sys::HtmlInputElement>() { let mut v = (*values).clone(); v[i] = inp.value(); values.set(v); }
                    });
                    let ph = if q.required { q.hint.to_string() } else { format!("{} (optional)", q.hint) };
                    html! { <label style="display:grid;gap:4px;font-size:12px;color:var(--muted);font-weight:550">
                        { format!("{}{}", q.label, if q.required { " *" } else { "" }) }
                        <input type={if q.ty == Ty::Text { "text" } else { "number" }} step={if q.ty == Ty::Float { "any" } else { "1" }} placeholder={ph} value={cur} {oninput}
                            style="background:var(--surface);border:1px solid var(--line-strong);border-radius:8px;padding:8px 11px;font-size:13.5px;color:var(--ink);font-weight:400" />
                    </label> }
                }) }
                <div style="display:flex;gap:8px"><Button variant={Variant::Primary} submit=true icon="play" busy={*busy}>{ "Run" }</Button>
                    <Button icon="terminal" onclick={open_in_workbench} title="Open this statement in the workbench">{ "Workbench" }</Button></div>
            </form>
            { if stmt.is_empty() { html!{} } else { html! { <pre style="margin:12px 0 0;color:var(--muted);font-size:12.5px;overflow-x:auto">{ stmt }</pre> } } }
            { match &*out {
                None => html!{},
                Some(Err(e)) => html! { <Alert tone="err">{ e.clone() }</Alert> },
                Some(Ok(o)) => html! { <div style="margin-top:14px"><ResultView out={o.clone()} file_stem={p.proc.name.replace('.', "_")} /></div> },
            } }
        </Card>
    }
}

#[function_component(AnalyticsPage)]
pub fn analytics_page(p: &Props) -> Html {
    let title = catalog::GROUPS.iter().find(|(g, _)| *g == p.group).map(|(_, t)| *t);
    let Some(title) = title else { return html! { <Alert tone="err">{ "Unknown analytics group." }</Alert> } };
    let procs: Vec<&'static Proc> = catalog::in_group(&p.group).collect();
    let sel = use_state(|| 0usize);
    let cur = procs.get((*sel).min(procs.len().saturating_sub(1))).copied();
    let note = match p.group.as_str() {
        "spectral" => "Spectral procedures use every edge with two or more members and positive weight.",
        "gaap" => "GAAP procedures read the double-entry ledger stored in the graph.",
        "physics" => "Diffusion needs a numeric node property to start from.",
        _ => "Run on the live snapshot of your tenant's graph.",
    };
    html! {
        <>
            <PageHeader title={title} subtitle={note} />
            <Grid min={200}>
                { for procs.iter().enumerate().map(|(i, q)| { let sel = sel.clone(); html! {
                    <button type="button" onclick={Callback::from(move |_| sel.set(i))}
                        style={format!("text-align:left;background:var(--surface);border:1px solid {};border-radius:10px;padding:10px 12px;cursor:pointer;color:var(--ink)", if i == *sel { "var(--brand)" } else { "var(--line)" })}>
                        <b style="font-size:13.5px">{ q.title }</b><div style="color:var(--muted);font-size:12px">{ q.name }</div>
                    </button> } }) }
            </Grid>
            <div style="height:16px" />
            { if let Some(q) = cur { html! { <Runner key={q.name} proc={q} /> } } else { html!{} } }
        </>
    }
}
