//! My account: identity, password change, checkpoint, theme.
use serde_json::json;
use wasm_bindgen_futures::spawn_local;
use yew::prelude::*;

use crate::api;
use crate::components::fields::TextField;
use crate::components::ui::{Alert, Badge, Button, Card, PageHeader, StatTile, Variant, Grid};
use crate::theme::ThemeCtx;
use crate::{roles, Session};

/// Why a new password cannot be submitted yet, if it cannot.
pub fn password_problem(old: &str, new: &str, confirm: &str) -> Option<&'static str> {
    if old.is_empty() || new.is_empty() { Some("Fill in all three fields.") }
    else if new != confirm { Some("The new passwords do not match.") }
    else if new == old { Some("The new password must differ from the current one.") }
    else { None }
}

#[function_component(AccountPage)]
pub fn account_page() -> Html {
    let s = use_context::<Session>().expect("session");
    let theme = use_context::<ThemeCtx>().expect("theme");
    let (old, new, confirm) = (use_state(String::new), use_state(String::new), use_state(String::new));
    let msg = use_state(|| Option::<(bool, String)>::None);
    let busy = use_state(|| false);
    let cp_busy = use_state(|| false);
    let submit = {
        let (old, new, confirm, msg, busy) = (old.clone(), new.clone(), confirm.clone(), msg.clone(), busy.clone());
        Callback::from(move |e: SubmitEvent| {
            e.prevent_default();
            if let Some(p) = password_problem(&old, &new, &confirm) { msg.set(Some((false, p.into()))); return; }
            busy.set(true);
            let (o, n, msg, busy, old, new, confirm) = ((*old).clone(), (*new).clone(), msg.clone(), busy.clone(), old.clone(), new.clone(), confirm.clone());
            spawn_local(async move {
                match api::post("/v1/me/password", &json!({"old": o, "new": n})).await {
                    Ok(_) => { old.set(String::new()); new.set(String::new()); confirm.set(String::new()); msg.set(Some((true, "Password changed.".into()))); }
                    Err(e) => msg.set(Some((false, e.message))),
                }
                busy.set(false);
            });
        })
    };
    let checkpoint = {
        let (msg, cp_busy) = (msg.clone(), cp_busy.clone());
        Callback::from(move |_| {
            cp_busy.set(true);
            let (msg, cp_busy) = (msg.clone(), cp_busy.clone());
            spawn_local(async move {
                match api::post("/v1/checkpoint", &json!({})).await {
                    Ok(v) => msg.set(Some((true, format!("Checkpoint written at ts {}; version history before it was discarded.", v.get("ts").map(|t| t.to_string()).unwrap_or_default())))),
                    Err(e) => msg.set(Some((false, e.message))),
                }
                cp_busy.set(false);
            });
        })
    };
    let cycle = { let c = theme.cycle.clone(); Callback::from(move |_| c.emit(())) };
    html! {
        <>
            <PageHeader title="My account" />
            <Card title="Identity"><Grid min={160}>
                <StatTile label="User" value={s.user.user.clone()} />
                <StatTile label="Tenant" value={s.user.tenant.clone().unwrap_or_else(|| "— (platform)".into())} />
                <StatTile label="Role" value={s.user.role.replace('_', " ")} hint={if roles::can_write(&s.user) { "may change data" } else { "read access" }} />
            </Grid></Card>
            { match &*msg { Some((ok, m)) => html! { <Alert tone={if *ok { "ok" } else { "err" }}>{ m.clone() }</Alert> }, None => html!{} } }
            <Card title="Change password">
                <form onsubmit={submit} style="display:grid;gap:12px;max-width:380px">
                    <TextField label="Current password" value={old.clone()} kind="password" autocomplete="current-password" />
                    <TextField label="New password" value={new.clone()} kind="password" autocomplete="new-password" hint="Passphrases work best; the server rejects weak passwords." />
                    <TextField label="Repeat new password" value={confirm.clone()} kind="password" autocomplete="new-password" />
                    <div><Button variant={Variant::Primary} submit=true busy={*busy} icon="key">{ "Change password" }</Button></div>
                </form>
            </Card>
            { if roles::can_checkpoint(&s.user) { html! { <Card title="Maintenance">
                <p style="margin:0 0 10px;color:var(--ink-2)">{ "A checkpoint compacts the write-ahead log and discards version history older than the latest commit, so " }<code>{ "AS OF" }</code>{ " queries before it stop working." }</p>
                <Button variant={Variant::Danger} icon="database" busy={*cp_busy} onclick={checkpoint}>{ "Checkpoint now" }</Button>
            </Card> } } else { html!{} } }
            <Card title="Appearance"><Button icon="sun" onclick={cycle}>{ format!("Theme: {} (click to change)", theme.mode.as_str()) }</Button> <Badge text="saved in this browser" tone="info" /></Card>
        </>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn password_rules() {
        assert!(password_problem("", "a", "a").is_some());
        assert!(password_problem("o", "n", "x").unwrap().contains("match"));
        assert!(password_problem("same", "same", "same").unwrap().contains("differ"));
        assert!(password_problem("old", "new", "new").is_none());
    }
}
