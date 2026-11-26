//! nexgraph web UI: Yew (WebAssembly) + Stylist, built on the Enterprise Suite design flow
//! (sign-in -> dashboard -> role-aware sidebar -> list/detail pages -> generic result renderer -> admin).
mod api;
mod browser;
mod catalog;
#[allow(dead_code)] // design-system primitives kept whole (Tabs, LineChart, ...) for future pages
mod components;
mod pages;
mod roles;
mod theme;
mod util;

use wasm_bindgen_futures::spawn_local;
use yew::prelude::*;
use yew_router::prelude::*;

use api::User;
use components::layout::Shell;
use components::ui::Spinner;

#[derive(Clone, Routable, PartialEq, Debug)]
pub enum Route {
    #[at("/")]
    Dashboard,
    #[at("/query")]
    Query,
    #[at("/analytics/:group")]
    Analytics { group: String },
    #[at("/data")]
    Data,
    #[at("/data/:name")]
    Table { name: String },
    #[at("/powerbi")]
    PowerBi,
    #[at("/audit")]
    Audit,
    #[at("/admin/tenants")]
    Tenants,
    #[at("/admin/users")]
    Users,
    #[at("/account")]
    Account,
    #[not_found]
    #[at("/404")]
    NotFound,
}

impl Route {
    /// Sidebar group this page belongs to.
    pub fn group(&self) -> Option<String> {
        match self {
            Route::Query | Route::Data | Route::Table { .. } | Route::PowerBi => Some("data".into()),
            Route::Analytics { group } => Some(group.clone()),
            Route::Audit | Route::Tenants | Route::Users | Route::Account => Some("admin".into()),
            _ => None,
        }
    }
    pub fn crumb(&self) -> String {
        match self {
            Route::Dashboard => "Dashboard".into(),
            Route::Query => "Data / Query workbench".into(),
            Route::Analytics { group } => format!("Analytics / {}", catalog::GROUPS.iter().find(|(g, _)| g == group).map(|(_, t)| t.to_string()).unwrap_or_else(|| util::title_case(group))),
            Route::Data => "Data / Explorer".into(),
            Route::Table { name } => format!("Data / Explorer / {name}"),
            Route::PowerBi => "Data / Power BI".into(),
            Route::Audit => "Administration / Audit log".into(),
            Route::Tenants => "Administration / Tenants".into(),
            Route::Users => "Administration / Users".into(),
            Route::Account => "Administration / My account".into(),
            Route::NotFound => "Not found".into(),
        }
    }
}

/// Signed-in user + logout, available to every page through a context.
#[derive(Clone, PartialEq)]
pub struct Session {
    pub user: User,
    pub logout: Callback<()>,
}

fn switch(route: Route) -> Html {
    match route {
        Route::Dashboard => html! { <pages::dashboard::DashboardPage /> },
        Route::Query => html! { <pages::query::QueryPage /> },
        Route::Analytics { group } => html! { <pages::analytics::AnalyticsPage key={group.clone()} group={group.clone()} /> },
        Route::Data => html! { <pages::data::DataPage /> },
        Route::Table { name } => html! { <pages::data::TablePage key={name.clone()} name={name.clone()} /> },
        Route::PowerBi => html! { <pages::powerbi::PowerBiPage /> },
        Route::Audit => html! { <pages::audit::AuditPage /> },
        Route::Tenants => html! { <pages::admin::TenantsPage /> },
        Route::Users => html! { <pages::admin::UsersPage /> },
        Route::Account => html! { <pages::account::AccountPage /> },
        Route::NotFound => html! { <pages::not_found::NotFoundPage /> },
    }
}

#[function_component(Root)]
fn root() -> Html {
    let user = use_state(api::stored_user);
    // a stored token may have expired: confirm it once on load
    let checked = use_state(|| api::stored_user().is_none());
    {
        let (user, checked) = (user.clone(), checked.clone());
        use_effect_with((), move |_| {
            if !*checked {
                spawn_local(async move {
                    match api::whoami().await {
                        Ok(u) => {
                            if let Some(t) = api::token() { api::save_session(&t, &u); }
                            user.set(Some(u));
                        }
                        Err(e) if e.status == 401 || e.status == 403 => {
                            api::clear_session();
                            user.set(None);
                        }
                        Err(_) => {} // server unreachable: keep the cached session, pages will report the error
                    }
                    checked.set(true);
                });
            }
            || ()
        });
    }
    let on_login = {
        let user = user.clone();
        Callback::from(move |u: User| user.set(Some(u)))
    };
    let logout = {
        let user = user.clone();
        Callback::from(move |_| {
            let user = user.clone();
            spawn_local(async move {
                api::logout().await; // revoke the token server-side, then forget it
                api::clear_session();
                user.set(None);
            });
        })
    };
    if !*checked {
        return html! { <Spinner /> };
    }
    match (*user).clone() {
        None => html! { <pages::login::LoginPage {on_login} /> },
        Some(u) => html! {
            <ContextProvider<Session> context={Session { user: u, logout }}>
                <BrowserRouter>
                    <Shell><Switch<Route> render={switch} /></Shell>
                </BrowserRouter>
            </ContextProvider<Session>>
        },
    }
}

#[function_component(App)]
fn app() -> Html {
    html! { <theme::ThemeProvider><Root /></theme::ThemeProvider> }
}

fn main() {
    yew::Renderer::<App>::new().render();
}

#[cfg(test)]
mod tests {
    use super::*;
    use components::layout::nav_groups;

    fn u(role: &str, tenant: Option<&str>) -> User {
        User { user: "x".into(), tenant: tenant.map(String::from), role: role.into() }
    }

    #[test]
    fn nav_respects_roles() {
        let keys = |u: &User| nav_groups(u).iter().map(|g| g.key).collect::<Vec<_>>();
        let items = |u: &User, k: &str| nav_groups(u).iter().find(|g| g.key == k).map(|g| g.items.iter().map(|i| i.route.clone()).collect::<Vec<_>>()).unwrap_or_default();
        let reader = u("reader", Some("t"));
        assert!(keys(&reader).contains(&"data") && keys(&reader).contains(&"gaap"));
        assert_eq!(items(&reader, "admin"), vec![Route::Account]);
        let ta = u("tenant_admin", Some("t"));
        assert!(items(&ta, "admin").contains(&Route::Audit) && items(&ta, "admin").contains(&Route::Users) && !items(&ta, "admin").contains(&Route::Tenants));
        // platform admins manage tenants but have no graph: no data or analytics pages
        let pa = u("platform_admin", None);
        assert_eq!(keys(&pa), vec!["admin"]);
        assert!(items(&pa, "admin").contains(&Route::Tenants));
    }

    #[test]
    fn every_nav_item_has_a_group_and_crumb() {
        for user in [u("writer", Some("t")), u("tenant_admin", Some("t")), u("platform_admin", None)] {
            for g in nav_groups(&user) {
                for i in g.items {
                    assert!(i.route.group().is_some(), "{:?}", i.route);
                    assert!(!i.route.crumb().is_empty());
                }
            }
        }
        assert_eq!(Route::Analytics { group: "gaap".into() }.crumb(), "Analytics / GAAP ledger");
    }
}
