use stylist::yew::styled_component;
use wasm_bindgen_futures::spawn_local;
use yew::prelude::*;

use crate::api::{self, User};
use crate::components::fields::bind as bind_input;
use crate::components::ui::{Alert, Button, Icon, Variant};

#[derive(Properties, PartialEq)]
pub struct LoginProps {
    pub on_login: Callback<User>,
}

#[styled_component(LoginPage)]
pub fn login_page(p: &LoginProps) -> Html {
    let css = css!(
        r#"
        min-height: 100vh; display: grid; grid-template-columns: minmax(0, 1.1fr) minmax(0, 1fr);
        .hero { background: linear-gradient(160deg, var(--side-bg), #12395a 70%, var(--brand)); color: #fff; padding: 48px; display: flex; flex-direction: column; justify-content: center; gap: 18px; }
        .hero h1 { font-size: 34px; line-height: 1.15; max-width: 460px; }
        .hero p { color: #c9d6e6; max-width: 440px; margin: 0; }
        .hero ul { list-style: none; padding: 0; margin: 8px 0 0; display: grid; gap: 8px; color: #dbe6f3; }
        .hero li::before { content: "▸ "; color: var(--c1); }
        .logo { width: 44px; height: 44px; border-radius: 12px; background: var(--brand); color: var(--brand-ink); display: grid; place-items: center; }
        .side { display: grid; place-items: center; padding: 24px; }
        form { width: 100%; max-width: 360px; display: grid; gap: 14px; }
        form h2 { font-size: 22px; } form p { margin: 0; color: var(--muted); }
        label { display: grid; gap: 4px; font-size: 12px; color: var(--muted); font-weight: 550; }
        input { background: var(--surface); border: 1px solid var(--line-strong); border-radius: 8px; padding: 10px 12px; font-size: 14px; color: var(--ink); }
        input:focus { border-color: var(--brand); outline: none; box-shadow: 0 0 0 3px var(--brand-soft); }
        @media (max-width: 820px) { grid-template-columns: minmax(0, 1fr); .hero { display: none; } }
        "#
    );
    let tenant = use_state(String::new);
    let email = use_state(String::new);
    let password = use_state(String::new);
    let error = use_state(|| Option::<String>::None);
    let busy = use_state(|| false);

    let bind = |s: &UseStateHandle<String>| bind_input(s);
    let onsubmit = {
        let (tenant, email, password, error, busy) = (tenant.clone(), email.clone(), password.clone(), error.clone(), busy.clone());
        let on_login = p.on_login.clone();
        Callback::from(move |e: SubmitEvent| {
            e.prevent_default();
            if email.trim().is_empty() || password.is_empty() {
                error.set(Some("Enter your user name and password.".into()));
                return;
            }
            busy.set(true);
            error.set(None);
            let (tenant, email, password, error, busy, on_login) = ((*tenant).clone(), (*email).clone(), (*password).clone(), error.clone(), busy.clone(), on_login.clone());
            spawn_local(async move {
                match api::login(&tenant, email.trim(), &password).await {
                    Ok((token, user)) => {
                        api::save_session(&token, &user);
                        on_login.emit(user);
                    }
                    Err(e) => {
                        error.set(Some(if e.status == 401 { "Wrong tenant, user name or password.".to_string() } else { e.message }));
                        busy.set(false);
                    }
                }
            });
        })
    };
    html! {
        <div class={css}>
            <div class="hero">
                <span class="logo"><Icon name="network" size={24} /></span>
                <h1>{ "Query, analyse and audit your hypergraph." }</h1>
                <p>{ "A multi-tenant property-hypergraph database with MVCC time travel, spectral analytics, a GAAP ledger and Power BI export." }</p>
                <ul><li>{ "Role-based access, enforced by the API" }</li><li>{ "Every statement recorded in the audit log" }</li><li>{ "Per-tenant isolation and quotas" }</li></ul>
            </div>
            <div class="side">
                <form {onsubmit}>
                    <div><h2>{ "Sign in" }</h2><p>{ "Use your nexgraph account." }</p></div>
                    <label>{ "Tenant" }<input type="text" autocomplete="organization" placeholder="leave empty for platform administrators" value={(*tenant).clone()} oninput={bind(&tenant)} /></label>
                    <label>{ "User name" }<input type="text" autocomplete="username" autofocus=true value={(*email).clone()} oninput={bind(&email)} /></label>
                    <label>{ "Password" }<input type="password" autocomplete="current-password" value={(*password).clone()} oninput={bind(&password)} /></label>
                    { if let Some(m) = &*error { html! { <Alert tone="err">{ m.clone() }</Alert> } } else { html! {} } }
                    <Button variant={Variant::Primary} submit=true busy={*busy}>{ "Sign in" }</Button>
                </form>
            </div>
        </div>
    }
}
