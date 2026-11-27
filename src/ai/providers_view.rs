//! Settings section managing model servers (llama.cpp, Ollama, LM Studio, OpenAI-compatible, Claude API).

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::assets::IconName;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::i18n::t;

use super::anthropic;
use super::chat_panel::refresh_models;
use super::sampling::{self, Purpose, Sampling};
use crate::settings::{Provider, ProviderKind, Settings};

/// Generation settings editable per server, in display order.
const FIELDS: [&str; 7] = ["Temperature", "Top P", "Top K", "Min P", "Repeat penalty", "Presence penalty", "Max length (tokens)"];

fn field(s: &Sampling, i: usize) -> Option<String> {
    let f = |v: Option<f32>| v.map(|v| format!("{}", (v * 100.).round() / 100.));
    match i {
        0 => f(s.temperature),
        1 => f(s.top_p),
        2 => s.top_k.map(|v| v.to_string()),
        3 => f(s.min_p),
        4 => f(s.repeat_penalty),
        5 => f(s.presence_penalty),
        _ => s.max_tokens.map(|v| v.to_string()),
    }
}

/// Store field `i` from what the user typed; empty = automatic. False if it is not a number.
fn set_field(s: &mut Sampling, i: usize, text: &str) -> bool {
    let text = text.trim().replace(',', ".");
    if text.is_empty() {
        match i {
            0 => s.temperature = None,
            1 => s.top_p = None,
            2 => s.top_k = None,
            3 => s.min_p = None,
            4 => s.repeat_penalty = None,
            5 => s.presence_penalty = None,
            _ => s.max_tokens = None,
        }
        return true;
    }
    match i {
        2 | 6 => match text.parse::<u32>() {
            Ok(v) if i == 2 => s.top_k = Some(v),
            Ok(v) => s.max_tokens = Some(v),
            Err(_) => return false,
        },
        _ => match text.parse::<f32>() {
            Ok(v) => match i {
                0 => s.temperature = Some(v),
                1 => s.top_p = Some(v),
                3 => s.min_p = Some(v),
                4 => s.repeat_penalty = Some(v),
                _ => s.presence_penalty = Some(v),
            },
            Err(_) => return false,
        },
    }
    true
}

/// Presets: (label, kind, default URL).
const PRESETS: [(&str, ProviderKind, &str); 5] = [
    ("llama.cpp", ProviderKind::OpenAi, "http://localhost:8080/v1"),
    ("Ollama", ProviderKind::OpenAi, "http://localhost:11434/v1"),
    ("LM Studio", ProviderKind::OpenAi, "http://localhost:1234/v1"),
    ("OpenAI-compatible", ProviderKind::OpenAi, "https://"),
    ("Claude API", ProviderKind::Anthropic, anthropic::DEFAULT_URL),
];

pub struct ProvidersView {
    preset: usize,
    name: Entity<InputState>,
    url: Entity<InputState>,
    key: Entity<InputState>,
    /// Provider whose generation settings are open, with one input per [`FIELDS`] entry.
    tuning: Option<(String, Vec<Entity<InputState>>)>,
    _subs: Vec<Subscription>,
}

impl ProvidersView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            preset: 0,
            name: cx.new(|cx| InputState::new(window, cx).placeholder(t("Name"))),
            url: cx.new(|cx| InputState::new(window, cx).placeholder(t("Base URL"))),
            key: cx.new(|cx| InputState::new(window, cx).placeholder(t("API key")).masked(true)),
            tuning: None,
            _subs: vec![],
        };
        this.select(0, window, cx);
        this
    }

    fn select(&mut self, preset: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.preset = preset;
        let (label, _, url) = PRESETS[preset];
        self.name.update(cx, |i, cx| i.set_value(label, window, cx));
        self.url.update(cx, |i, cx| i.set_value(url, window, cx));
        cx.notify();
    }

    fn add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.name.read(cx).value().trim().to_string();
        let base_url = self.url.read(cx).value().trim().to_string();
        let api_key = self.key.read(cx).value().trim().to_string();
        if name.is_empty() || base_url.is_empty() {
            return;
        }
        let kind = PRESETS[self.preset].1;
        Settings::update(cx, |s| {
            let base: String = name.to_lowercase().chars().map(|c| if c.is_alphanumeric() { c } else { '-' }).collect();
            let id = (1..).map(|i| if i == 1 { base.clone() } else { format!("{base}-{i}") }).find(|id| id != "copilot" && !s.providers.iter().any(|p| &p.id == id)).unwrap();
            s.providers.push(Provider { id, name, kind, base_url, api_key, models: vec![], sampling: Default::default() });
        });
        self.key.update(cx, |i, cx| i.set_value("", window, cx));
        refresh_models(cx);
        // Local servers are where these settings matter: show them right away.
        if let Some(p) = Settings::get(cx).providers.last().filter(|p| sampling::self_hosted(&p.base_url)) {
            let id = p.id.clone();
            self.toggle_tuning(id, window, cx);
        }
    }

    fn toggle_tuning(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        self._subs.clear();
        if self.tuning.as_ref().is_some_and(|(open, _)| *open == id) {
            self.tuning = None;
            return cx.notify();
        }
        let Some(p) = Settings::get(cx).providers.iter().find(|p| p.id == id).cloned() else { return };
        let auto = auto_sampling(&p);
        let mut inputs = vec![];
        for i in 0..FIELDS.len() {
            let hint = match field(&auto, i) {
                Some(v) => format!("auto | {v}"),
                None => t("not sent").to_string(),
            };
            let value = field(&p.sampling, i).unwrap_or_default();
            let input = cx.new(|cx| InputState::new(window, cx).placeholder(hint));
            input.update(cx, |s, cx| s.set_value(value, window, cx));
            let pid = id.clone();
            self._subs.push(cx.subscribe(&input, move |_, input, e: &InputEvent, cx| {
                if !matches!(e, InputEvent::Change) {
                    return;
                }
                let text = input.read(cx).value().to_string();
                let mut s = Settings::get(cx).providers.iter().find(|p| p.id == pid).map(|p| p.sampling).unwrap_or_default();
                if set_field(&mut s, i, &text) {
                    let pid = pid.clone();
                    Settings::update(cx, move |st| st.providers.iter_mut().filter(|p| p.id == pid).for_each(|p| p.sampling = s));
                }
            }));
            inputs.push(input);
        }
        self.tuning = Some((id, inputs));
        cx.notify();
    }

    fn reset_tuning(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((id, inputs)) = &self.tuning {
            let id = id.clone();
            Settings::update(cx, |s| s.providers.iter_mut().filter(|p| p.id == id).for_each(|p| p.sampling = Sampling::default()));
            for input in inputs {
                input.update(cx, |i, cx| i.set_value("", window, cx));
            }
        }
    }
}

/// What is sent when the user sets nothing: the recommended values of the server's first model.
fn auto_sampling(p: &Provider) -> Sampling {
    if p.kind == ProviderKind::OpenAi && sampling::self_hosted(&p.base_url) {
        sampling::resolve(p.models.first().map_or("", |m| m.as_str()), Purpose::Code, Sampling::default())
    } else {
        Sampling::default()
    }
}

impl Render for ProvidersView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let providers = Settings::get(cx).providers.clone();
        v_flex()
            .w_full()
            .gap_3()
            .children(providers.into_iter().map(|p| {
                let id = p.id.clone();
                let tune_id = p.id.clone();
                let open = self.tuning.as_ref().filter(|(open, _)| *open == p.id).map(|(_, inputs)| inputs.clone());
                let row = h_flex()
                    .gap_3()
                    .p_2()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .child(div().text_color(theme.muted_foreground).child(if p.kind == ProviderKind::Anthropic { IconName::Bot } else { IconName::HardDrive }))
                    .child(
                        v_flex()
                            .flex_1()
                            .child(div().text_sm().child(p.name.clone()))
                            .child(div().text_xs().text_color(theme.muted_foreground).child(format!("{} | {} {}", p.base_url, p.models.len(), t("models")))),
                    )
                    .child(Button::new(SharedString::from(format!("tune-{id}"))).ghost().xsmall().icon(IconName::Settings2).tooltip(t("Generation settings"))
                        .when(open.is_some(), |b| b.primary())
                        .on_click(cx.listener(move |this, _, w, cx| this.toggle_tuning(tune_id.clone(), w, cx))))
                    .child(Button::new(SharedString::from(format!("rm-{id}"))).ghost().xsmall().icon(IconName::Delete).tooltip(t("Remove"))
                        .on_click(move |_, _, cx| { let id = id.clone(); Settings::update(cx, |s| s.providers.retain(|p| p.id != id)) }));
                v_flex().gap_1().child(row).when_some(open, |col, inputs| {
                    col.child(
                        v_flex()
                            .gap_2()
                            .p_2()
                            .rounded_md()
                            .bg(theme.secondary)
                            .child(div().text_xs().text_color(theme.muted_foreground).child(t("Empty = automatic: the values recommended for the model and for what each agent does (code, orchestration, review). Too low a temperature makes some models (Qwen3…) loop.")))
                            .child(h_flex().flex_wrap().gap_2().children(inputs.into_iter().enumerate().map(|(i, input)| {
                                v_flex().w(px(150.)).gap_0p5()
                                    .child(div().text_xs().child(t(FIELDS[i])))
                                    .child(Input::new(&input).small())
                            })))
                            .child(h_flex().justify_end().child(Button::new("reset-sampling").xsmall().ghost().label(t("Reset to automatic"))
                                .on_click(cx.listener(|this, _, w, cx| this.reset_tuning(w, cx))))),
                    )
                })
            }))
            .child(
                v_flex()
                    .gap_2()
                    .p_2()
                    .rounded_md()
                    .bg(theme.secondary)
                    .child(div().text_xs().text_color(theme.muted_foreground).child(t("Add")))
                    .child(h_flex().flex_wrap().gap_1().children(PRESETS.iter().enumerate().map(|(ix, (label, _, _))| {
                        Button::new(("preset", ix)).xsmall().label(*label)
                            .when(ix == self.preset, |b| b.primary())
                            .when(ix != self.preset, |b| b.ghost())
                            .on_click(cx.listener(move |t, _, w, cx| t.select(ix, w, cx)))
                    })))
                    .child(Input::new(&self.name).small())
                    .child(Input::new(&self.url).small())
                    .child(Input::new(&self.key).small())
                    .child(h_flex().justify_end().gap_2()
                        .child(Button::new("refresh-models").small().ghost().label(t("Refresh model lists")).on_click(|_, _, cx| refresh_models(cx)))
                        .child(Button::new("add-provider").small().primary().label(t("Add")).on_click(cx.listener(|t, _, w, cx| t.add(w, cx))))),
            )
    }
}
