//! Small controlled form inputs that share one look.
use stylist::yew::styled_component;
use web_sys::{HtmlInputElement, HtmlSelectElement, HtmlTextAreaElement};
use yew::prelude::*;

/// `oninput` callback that writes an `<input>`'s value into a state handle.
pub fn bind(s: &UseStateHandle<String>) -> Callback<InputEvent> {
    let s = s.clone();
    Callback::from(move |e: InputEvent| {
        if let Some(i) = e.target_dyn_into::<HtmlInputElement>() {
            s.set(i.value());
        }
    })
}

#[derive(Properties, PartialEq)]
pub struct TextProps {
    pub label: AttrValue,
    pub value: UseStateHandle<String>,
    #[prop_or("text")]
    pub kind: &'static str,
    #[prop_or_default]
    pub hint: AttrValue,
    #[prop_or_default]
    pub autocomplete: AttrValue,
    #[prop_or_default]
    pub autofocus: bool,
}

macro_rules! field_css {
    () => {
        ::stylist::css!(r#"
    display: grid; gap: 4px; font-size: 12px; color: var(--muted); font-weight: 550; min-width: 0;
    input, select, textarea { background: var(--surface); border: 1px solid var(--line-strong); border-radius: 8px; padding: 8px 11px; font-size: 13.5px; color: var(--ink); width: 100%; font-weight: 400; }
    input:focus, select:focus, textarea:focus { border-color: var(--brand); outline: none; box-shadow: 0 0 0 3px var(--brand-soft); }
    small { font-weight: 400; color: var(--muted); }
"#)
    };
}

#[styled_component(TextField)]
pub fn text_field(p: &TextProps) -> Html {
    let css = field_css!();
    html! {
        <label class={css}>
            { p.label.clone() }
            <input type={p.kind} value={(*p.value).clone()} oninput={bind(&p.value)} autocomplete={p.autocomplete.clone()} autofocus={p.autofocus} />
            { if p.hint.is_empty() { html!{} } else { html!{ <small>{ p.hint.clone() }</small> } } }
        </label>
    }
}

#[derive(Properties, PartialEq)]
pub struct SelectProps {
    pub label: AttrValue,
    pub value: UseStateHandle<String>,
    pub options: Vec<(AttrValue, AttrValue)>,
}

#[styled_component(SelectField)]
pub fn select_field(p: &SelectProps) -> Html {
    let css = field_css!();
    let onchange = {
        let v = p.value.clone();
        Callback::from(move |e: Event| {
            if let Some(s) = e.target_dyn_into::<HtmlSelectElement>() {
                v.set(s.value());
            }
        })
    };
    html! {
        <label class={css}>
            { p.label.clone() }
            <select {onchange}>
                { for p.options.iter().map(|(v, l)| html! { <option value={v.clone()} selected={**v == **p.value}>{ l.clone() }</option> }) }
            </select>
        </label>
    }
}

#[derive(Properties, PartialEq)]
pub struct AreaProps {
    pub label: AttrValue,
    pub value: UseStateHandle<String>,
    #[prop_or(6)]
    pub rows: u32,
    #[prop_or_default]
    pub mono: bool,
    #[prop_or_default]
    pub on_submit: Callback<()>,
    #[prop_or_default]
    pub hint: AttrValue,
}

/// Textarea; Ctrl/⌘+Enter calls `on_submit`.
#[styled_component(AreaField)]
pub fn area_field(p: &AreaProps) -> Html {
    let css = field_css!();
    let mono = css!("textarea { font-family: var(--mono); font-size: 13px; line-height: 1.5; tab-size: 2; }");
    let oninput = {
        let v = p.value.clone();
        Callback::from(move |e: InputEvent| {
            if let Some(t) = e.target_dyn_into::<HtmlTextAreaElement>() {
                v.set(t.value());
            }
        })
    };
    let onkeydown = {
        let cb = p.on_submit.clone();
        Callback::from(move |e: KeyboardEvent| {
            if e.key() == "Enter" && (e.ctrl_key() || e.meta_key()) {
                e.prevent_default();
                cb.emit(());
            }
        })
    };
    html! {
        <label class={classes!(css, p.mono.then_some(mono))}>
            { p.label.clone() }
            <textarea rows={p.rows.to_string()} value={(*p.value).clone()} {oninput} {onkeydown} spellcheck="false" />
            { if p.hint.is_empty() { html!{} } else { html!{ <small>{ p.hint.clone() }</small> } } }
        </label>
    }
}
