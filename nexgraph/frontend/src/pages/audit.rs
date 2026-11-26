//! Audit log (tenant_admin: own tenant; platform_admin: everything or one tenant).
use yew::prelude::*;

use crate::components::data::{show, use_fetch};
use crate::components::fields::{SelectField, TextField};
use crate::components::table::{Column, DataTable, Kind};
use crate::components::ui::{Button, Card, PageHeader};
use crate::{roles, util, Session};

#[function_component(AuditPage)]
pub fn audit_page() -> Html {
    let s = use_context::<Session>().expect("session");
    let limit = use_state(|| "100".to_string());
    let tenant = use_state(String::new);
    let applied = use_state(|| ("100".to_string(), String::new()));
    let tick = use_state(|| 0u32);
    let path = util::with_query("/v1/audit", &[("limit".into(), applied.0.clone()), ("tenant".into(), applied.1.clone())]);
    let f = use_fetch(path, *tick);
    let apply = { let (l, t, a, k) = (limit.clone(), tenant.clone(), applied.clone(), tick.clone()); Callback::from(move |_| { a.set(((*l).clone(), (*t).clone())); k.set(*k + 1); }) };
    let opts: Vec<(AttrValue, AttrValue)> = ["50", "100", "250", "500", "1000"].iter().map(|n| (AttrValue::from(*n), AttrValue::from(format!("Last {n}")))).collect();
    html! {
        <>
            <PageHeader title="Audit log" subtitle="Every login, statement, denial and administrative change, newest first." />
            <Card>
                <div style="display:flex;gap:12px;flex-wrap:wrap;align-items:flex-end">
                    <SelectField label="Show" value={limit.clone()} options={opts} />
                    { if roles::is_platform(&s.user) { html! { <TextField label="Tenant filter" value={tenant.clone()} hint="empty = all tenants" /> } } else { html!{} } }
                    <Button icon="refresh" onclick={apply}>{ "Refresh" }</Button>
                </div>
            </Card>
            { show(&f, |v| {
                let rows = v.get("entries").and_then(|e| e.as_array()).cloned().unwrap_or_default();
                let cols: Vec<Column> = ["id", "ts", "actor", "role", "action", "outcome", "detail"].iter().map(|k| { let mut c = Column::plain(k); if *k == "outcome" { c.kind = Kind::Badge; } c }).collect();
                html! { <Card pad=false title={format!("{} entries", rows.len())}><DataTable columns={cols} rows={rows} /></Card> }
            }) }
        </>
    }
}
