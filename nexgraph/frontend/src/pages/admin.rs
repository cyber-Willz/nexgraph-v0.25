//! Tenant and user administration.
use serde_json::{json, Map, Value};
use wasm_bindgen_futures::spawn_local;
use yew::prelude::*;

use crate::api;
use crate::components::data::{show, use_fetch};
use crate::components::fields::{SelectField, TextField};
use crate::components::table::{Column, DataTable, Kind};
use crate::components::ui::{Alert, Badge, Button, Card, Modal, PageHeader, Variant};
use crate::{roles, util, Session};

/// Optional positive integer from a text box: `Ok(None)` when empty.
pub fn opt_u64(label: &str, s: &str) -> Result<Option<u64>, String> {
    let s = s.trim();
    if s.is_empty() { return Ok(None); }
    s.parse::<u64>().ok().filter(|n| *n > 0).map(Some).ok_or_else(|| format!("{label} must be a positive whole number."))
}

/// Roles an administrator may assign: platform admins create tenant users (or platform admins with no tenant);
/// tenant admins create reader / writer / tenant_admin in their own tenant.
pub fn assignable_roles(actor_platform: bool, tenant_given: bool) -> Vec<&'static str> {
    if actor_platform && !tenant_given { vec!["platform_admin"] } else { vec!["reader", "writer", "tenant_admin"] }
}

fn flash(msg: &UseStateHandle<Option<(bool, String)>>) -> Html {
    match &**msg { Some((ok, m)) => html! { <Alert tone={if *ok { "ok" } else { "err" }}>{ m.clone() }</Alert> }, None => html!{} }
}

fn run(msg: UseStateHandle<Option<(bool, String)>>, tick: UseStateHandle<u32>, ok_text: String, call: impl std::future::Future<Output = Result<Value, api::ApiError>> + 'static) {
    spawn_local(async move {
        match call.await {
            Ok(_) => { msg.set(Some((true, ok_text))); tick.set(*tick + 1); }
            Err(e) => msg.set(Some((false, e.message))),
        }
    });
}

// ------------------------------------------------------------------ tenants

#[function_component(TenantsPage)]
pub fn tenants_page() -> Html {
    let tick = use_state(|| 0u32);
    let msg = use_state(|| Option::<(bool, String)>::None);
    let f = use_fetch("/v1/admin/tenants".into(), *tick);
    let (name, max_e, max_r) = (use_state(String::new), use_state(String::new), use_state(String::new));
    let purge = use_state(|| Option::<String>::None);
    let confirm = use_state(String::new);

    let create = {
        let (name, max_e, max_r, msg, tick) = (name.clone(), max_e.clone(), max_r.clone(), msg.clone(), tick.clone());
        Callback::from(move |e: SubmitEvent| {
            e.prevent_default();
            let (a, b) = match (opt_u64("Max entities", &max_e), opt_u64("Max result rows", &max_r)) { (Ok(a), Ok(b)) => (a, b), (Err(m), _) | (_, Err(m)) => { msg.set(Some((false, m))); return; } };
            if name.trim().is_empty() { msg.set(Some((false, "Give the tenant a name.".into()))); return; }
            let mut body = Map::new();
            body.insert("name".into(), json!(name.trim()));
            if let Some(a) = a { body.insert("max_entities".into(), json!(a)); }
            if let Some(b) = b { body.insert("max_result_rows".into(), json!(b)); }
            let n = name.trim().to_string();
            name.set(String::new());
            run(msg.clone(), tick.clone(), format!("Tenant {n} created. Add its first administrator under Users."), async move { api::post("/v1/admin/tenants", &Value::Object(body)).await });
        })
    };
    let toggle = { let (msg, tick) = (msg.clone(), tick.clone()); move |name: String, disable: bool| {
        let (msg, tick) = (msg.clone(), tick.clone());
        Callback::from(move |_| {
            let path = format!("/v1/admin/tenants/{}/{}", util::percent_encode(&name), if disable { "disable" } else { "enable" });
            run(msg.clone(), tick.clone(), format!("Tenant {name} {}.", if disable { "disabled" } else { "enabled" }), async move { api::post(&path, &json!({})).await });
        })
    } };
    let do_purge = {
        let (purge, confirm, msg, tick) = (purge.clone(), confirm.clone(), msg.clone(), tick.clone());
        Callback::from(move |_| {
            if let Some(n) = (*purge).clone() {
                if *confirm != n { return; }
                purge.set(None); confirm.set(String::new());
                let path = format!("/v1/admin/tenants/{}", util::percent_encode(&n));
                run(msg.clone(), tick.clone(), format!("Tenant {n} and all its data were deleted."), async move { api::delete(&path).await });
            }
        })
    };
    html! {
        <>
            <PageHeader title="Tenants" subtitle="Each tenant has its own isolated graph, users and quotas." />
            { flash(&msg) }
            <Card title="New tenant">
                <form onsubmit={create} style="display:grid;grid-template-columns:repeat(auto-fill,minmax(200px,1fr));gap:12px;align-items:end">
                    <TextField label="Name" value={name.clone()} />
                    <TextField label="Max nodes + edges" value={max_e.clone()} hint="empty = unlimited" />
                    <TextField label="Max rows per result" value={max_r.clone()} hint="empty = server default" />
                    <div><Button variant={Variant::Primary} submit=true icon="plus">{ "Create" }</Button></div>
                </form>
            </Card>
            { show(&f, |v| {
                let rows = v.get("tenants").and_then(|t| t.as_array()).cloned().unwrap_or_default();
                let cols = vec![Column::plain("name"), { let mut c = Column::plain("disabled"); c.kind = Kind::Bool; c }, Column::plain("max_entities"), Column::plain("max_result_rows")];
                let (toggle, purge) = (toggle.clone(), purge.clone());
                let extra = Callback::from(move |row: Value| {
                    let name = row.get("name").and_then(|n| n.as_str()).unwrap_or_default().to_string();
                    let off = row.get("disabled").and_then(|d| d.as_bool()).unwrap_or(false);
                    let p = purge.clone();
                    let n2 = name.clone();
                    html! { <span style="display:inline-flex;gap:6px">
                        <Button small=true onclick={toggle(name.clone(), !off)}>{ if off { "Enable" } else { "Disable" } }</Button>
                        { if off { html! { <Button small=true variant={Variant::Danger} icon="trash" onclick={Callback::from(move |_| p.set(Some(n2.clone())))}>{ "Delete" }</Button> } } else { html!{} } }
                    </span> }
                });
                html! { <Card pad=false title={format!("{} tenants", rows.len())}><DataTable columns={cols} rows={rows} row_extra={Some(extra)} /></Card> }
            }) }
            { if let Some(n) = &*purge { html! {
                <Modal title="Delete tenant" on_close={{ let (p, c) = (purge.clone(), confirm.clone()); Callback::from(move |_| { p.set(None); c.set(String::new()); }) }}>
                    <Alert tone="err">{ format!("This permanently deletes tenant {n}: its graph, users and tokens. Audit entries stay. It cannot be undone.") }</Alert>
                    <TextField label={format!("Type {n} to confirm")} value={confirm.clone()} />
                    <div style="margin-top:12px"><Button variant={Variant::Danger} icon="trash" disabled={**confirm != *n} onclick={do_purge.clone()}>{ "Delete permanently" }</Button></div>
                </Modal> } } else { html!{} } }
        </>
    }
}

// ------------------------------------------------------------------ users

#[function_component(UsersPage)]
pub fn users_page() -> Html {
    let s = use_context::<Session>().expect("session");
    let platform = roles::is_platform(&s.user);
    let tick = use_state(|| 0u32);
    let msg = use_state(|| Option::<(bool, String)>::None);
    let filter = use_state(String::new);
    let applied = use_state(String::new);
    let path = util::with_query("/v1/admin/users", &[("tenant".into(), (*applied).clone())]);
    let f = use_fetch(path, *tick);
    let (tenant, name, password) = (use_state(String::new), use_state(String::new), use_state(String::new));
    let role = use_state(|| "reader".to_string());
    let reset = use_state(|| Option::<(Option<String>, String)>::None);
    let new_pw = use_state(String::new);

    let role_opts: Vec<(AttrValue, AttrValue)> = assignable_roles(platform, !tenant.trim().is_empty()).into_iter().map(|r| (r.into(), r.replace('_', " ").into())).collect();
    {
        // keep the selected role valid when the tenant box changes
        let role = role.clone();
        let valid: Vec<String> = role_opts.iter().map(|(v, _)| v.to_string()).collect();
        use_effect_with(valid.clone(), move |valid| { if !valid.contains(&*role) { role.set(valid[0].clone()); } || () });
    }
    let create = {
        let (tenant, name, password, role, msg, tick) = (tenant.clone(), name.clone(), password.clone(), role.clone(), msg.clone(), tick.clone());
        Callback::from(move |e: SubmitEvent| {
            e.prevent_default();
            if name.trim().is_empty() || password.is_empty() { msg.set(Some((false, "Enter a user name and a password.".into()))); return; }
            let mut body = Map::new();
            if !tenant.trim().is_empty() { body.insert("tenant".into(), json!(tenant.trim())); }
            body.insert("name".into(), json!(name.trim()));
            body.insert("password".into(), json!(*password));
            body.insert("role".into(), json!(*role));
            let n = name.trim().to_string();
            password.set(String::new()); name.set(String::new());
            run(msg.clone(), tick.clone(), format!("User {n} created."), async move { api::post("/v1/admin/users", &Value::Object(body)).await });
        })
    };
    let set_disabled = { let (msg, tick) = (msg.clone(), tick.clone()); move |tenant: Option<String>, name: String, disabled: bool| {
        let (msg, tick) = (msg.clone(), tick.clone());
        Callback::from(move |_| {
            let mut body = Map::new();
            if let Some(t) = &tenant { body.insert("tenant".into(), json!(t)); }
            body.insert("name".into(), json!(name));
            body.insert("disabled".into(), json!(disabled));
            let n = name.clone();
            run(msg.clone(), tick.clone(), format!("User {n} {}.", if disabled { "disabled" } else { "enabled" }), async move { api::post("/v1/admin/users/disable", &Value::Object(body)).await });
        })
    } };
    let do_reset = {
        let (reset, new_pw, msg, tick) = (reset.clone(), new_pw.clone(), msg.clone(), tick.clone());
        Callback::from(move |_| {
            if let Some((t, n)) = (*reset).clone() {
                if new_pw.is_empty() { return; }
                let mut body = Map::new();
                if let Some(t) = &t { body.insert("tenant".into(), json!(t)); }
                body.insert("name".into(), json!(n));
                body.insert("new_password".into(), json!(*new_pw));
                reset.set(None); new_pw.set(String::new());
                run(msg.clone(), tick.clone(), format!("Password for {n} reset."), async move { api::post("/v1/admin/users/reset-password", &Value::Object(body)).await });
            }
        })
    };
    let apply = { let (a, fl, t) = (applied.clone(), filter.clone(), tick.clone()); Callback::from(move |_| { a.set((*fl).clone()); t.set(*t + 1); }) };
    html! {
        <>
            <PageHeader title="Users" subtitle={if platform { "Accounts across all tenants, plus platform administrators." } else { "Accounts of your tenant." }}>
                { if platform { html! { <div style="display:flex;gap:8px;align-items:flex-end"><TextField label="Tenant filter" value={filter.clone()} hint="empty = all" /><Button icon="refresh" onclick={apply}>{ "Apply" }</Button></div> } } else { html!{} } }
            </PageHeader>
            { flash(&msg) }
            <Card title="New user">
                <form onsubmit={create} style="display:grid;grid-template-columns:repeat(auto-fill,minmax(200px,1fr));gap:12px;align-items:end">
                    { if platform { html! { <TextField label="Tenant" value={tenant.clone()} hint="empty = platform administrator" /> } } else { html!{} } }
                    <TextField label="User name" value={name.clone()} hint="letters, digits, _ . @ -" />
                    <TextField label="Password" value={password.clone()} kind="password" autocomplete="new-password" />
                    <SelectField label="Role" value={role.clone()} options={role_opts} />
                    <div><Button variant={Variant::Primary} submit=true icon="plus">{ "Create" }</Button></div>
                </form>
            </Card>
            { show(&f, |v| {
                let rows = v.get("users").and_then(|t| t.as_array()).cloned().unwrap_or_default();
                let cols = vec![Column::plain("name"), { let mut c = Column::plain("role"); c.kind = Kind::Badge; c }, Column::plain("tenant"), { let mut c = Column::plain("disabled"); c.kind = Kind::Bool; c }, Column::plain("last_login"), Column::plain("created_at")];
                let (set_disabled, reset) = (set_disabled.clone(), reset.clone());
                let me = s.user.user.clone();
                let extra = Callback::from(move |row: Value| {
                    let name = row.get("name").and_then(|n| n.as_str()).unwrap_or_default().to_string();
                    let tenant = row.get("tenant").and_then(|n| n.as_str()).map(String::from);
                    let off = row.get("disabled").and_then(|d| d.as_bool()).unwrap_or(false);
                    let r = reset.clone();
                    let (t2, n2) = (tenant.clone(), name.clone());
                    html! { <span style="display:inline-flex;gap:6px">
                        <Button small=true icon="key" onclick={Callback::from(move |_| r.set(Some((t2.clone(), n2.clone()))))}>{ "Reset password" }</Button>
                        { if name == me { html! { <Badge text="you" tone="info" /> } } else { html! { <Button small=true onclick={set_disabled(tenant, name, !off)}>{ if off { "Enable" } else { "Disable" } }</Button> } } }
                    </span> }
                });
                html! { <Card pad=false title={format!("{} users", rows.len())}><DataTable columns={cols} rows={rows} row_extra={Some(extra)} /></Card> }
            }) }
            { if let Some((_, n)) = &*reset { html! {
                <Modal title={format!("Reset password for {n}")} on_close={{ let (r, p) = (reset.clone(), new_pw.clone()); Callback::from(move |_| { r.set(None); p.set(String::new()); }) }}>
                    <TextField label="New password" value={new_pw.clone()} kind="password" autocomplete="new-password" />
                    <div style="margin-top:12px"><Button variant={Variant::Primary} icon="key" disabled={new_pw.is_empty()} onclick={do_reset.clone()}>{ "Reset" }</Button></div>
                </Modal> } } else { html!{} } }
        </>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quota_boxes() {
        assert_eq!(opt_u64("x", " ").unwrap(), None);
        assert_eq!(opt_u64("x", "500").unwrap(), Some(500));
        assert!(opt_u64("Max entities", "0").unwrap_err().contains("Max entities"));
        assert!(opt_u64("x", "-3").is_err() && opt_u64("x", "1.5").is_err());
    }
    #[test]
    fn roles_by_actor() {
        assert_eq!(assignable_roles(true, false), ["platform_admin"]);
        assert_eq!(assignable_roles(true, true), ["reader", "writer", "tenant_admin"]);
        assert_eq!(assignable_roles(false, false), ["reader", "writer", "tenant_admin"]);
    }
}
