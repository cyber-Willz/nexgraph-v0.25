//! Power BI: the generated Power Query (M) script and how to connect.
use wasm_bindgen_futures::spawn_local;
use yew::prelude::*;

use crate::browser;
use crate::components::ui::{Alert, Button, Card, PageHeader};
use crate::{api, util};

fn origin() -> String {
    web_sys::window().and_then(|w| w.location().origin().ok()).unwrap_or_default()
}

#[function_component(PowerBiPage)]
pub fn powerbi_page() -> Html {
    let base = origin();
    let path = util::with_query("/v1/bi/powerquery", &[("base".into(), base.clone())]);
    // text endpoint: fetch as text, not JSON
    let script = use_state(|| Option::<Result<String, String>>::None);
    {
        let script = script.clone();
        use_effect_with(path.clone(), move |p| {
            let p = p.clone();
            spawn_local(async move { script.set(Some(api::get_text(&p).await.map_err(|e| e.message))); });
            || ()
        });
    }
    let copy = { let s = script.clone(); Callback::from(move |_| { if let Some(Ok(t)) = &*s { browser::copy_text(t); } }) };
    let save = { let s = script.clone(); Callback::from(move |_| { if let Some(Ok(t)) = &*s { let _ = browser::download_text("nexgraph.pq", "text/plain", t); } }) };
    html! {
        <>
            <PageHeader title="Power BI" subtitle="Connect Power BI Desktop or the service to this server with Power Query (M).">
                <Button icon="copy" onclick={copy}>{ "Copy" }</Button><Button icon="download" onclick={save}>{ "Download .pq" }</Button>
            </PageHeader>
            <Card title="How to connect">
                <ol style="margin:0;padding-left:20px;display:grid;gap:6px;color:var(--ink-2)">
                    <li>{ "In Power BI choose Get data → Blank query → Advanced editor, and paste the script below." }</li>
                    <li>{ "When asked for credentials pick Basic. The user name is " }<code>{ "tenant/user" }</code>{ " and the password is the account's password (scheduled refresh can only send static credentials)." }</li>
                    <li>{ "Use a dedicated reader account; its role and the tenant's data policy decide what Power BI can see." }</li>
                </ol>
            </Card>
            { match &*script {
                None => html! { <crate::components::ui::Spinner /> },
                Some(Err(e)) => html! { <Alert tone="err">{ e.clone() }</Alert> },
                Some(Ok(t)) => html! { <Card title="Power Query script" pad=false><pre style="margin:0;padding:14px 16px;overflow:auto;font-size:12.5px;max-height:60vh">{ t.clone() }</pre></Card> },
            } }
        </>
    }
}
