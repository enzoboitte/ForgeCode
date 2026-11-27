//! The Settings page, built on the component library's settings layout.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::setting::{NumberFieldOptions, SettingField, SettingGroup, SettingItem, SettingPage, Settings as SettingsView};
use gpui_kit::component::theme::{ThemeMode, ThemeRegistry};
use gpui_kit::assets::IconName;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::ai::copilot::{CopilotState, SignIn};
use crate::ai::providers_view::ProvidersView;
use gpui_kit::component::avatar::Avatar;
use gpui_kit::component::progress::Progress;
use crate::plugins::{self, Plugins};
use crate::ai::agent::Mode;
use crate::ai::chat_panel::catalog;
use crate::i18n::t;
use crate::settings::Settings;

/// String setting field bound to a `Settings` field.
macro_rules! text_field {
    ($field:ident) => {
        SettingField::input(
            |cx: &App| SharedString::from(Settings::get(cx).$field.clone()),
            |v: SharedString, cx: &mut App| Settings::update(cx, |s| s.$field = v.to_string()),
        )
    };
}

macro_rules! switch_field {
    ($field:ident) => {
        SettingField::switch(|cx: &App| Settings::get(cx).$field, |v: bool, cx: &mut App| Settings::update(cx, |s| s.$field = v))
    };
}

fn number(min: f64, max: f64) -> NumberFieldOptions {
    NumberFieldOptions { min, max, step: 1. }
}

fn theme_options(mode: ThemeMode, cx: &App) -> Vec<(SharedString, SharedString)> {
    let mut opts = vec![(SharedString::from(""), SharedString::from(t("Default")))];
    opts.extend(
        ThemeRegistry::global(cx)
            .sorted_themes()
            .into_iter()
            .filter(|th| th.mode == mode)
            .map(|th| (th.name.clone(), th.name.clone())),
    );
    opts
}

fn choice(options: &[(&'static str, &'static str)]) -> Vec<(SharedString, SharedString)> {
    options.iter().map(|(v, label)| ((*v).into(), t(label).into())).collect()
}

pub fn settings_view(cx: &App) -> SettingsView {
    let appearance = SettingPage::new(t("Appearance")).icon(IconName::Palette).groups([
        SettingGroup::new().title(t("Theme")).items([
            SettingItem::new(
                t("Language"),
                SettingField::dropdown(
                    vec![("en".into(), "English".into()), ("fr".into(), "Français".into())],
                    |cx: &App| Settings::get(cx).language.clone().into(),
                    |v: SharedString, cx: &mut App| Settings::update(cx, |s| s.language = v.to_string()),
                ),
            ),
            SettingItem::new(
                t("Mode"),
                SettingField::dropdown(
                    choice(&[("light", "Light"), ("dark", "Dark"), ("system", "System")]),
                    |cx: &App| Settings::get(cx).theme_mode.clone().into(),
                    |v: SharedString, cx: &mut App| Settings::update(cx, |s| s.theme_mode = v.to_string()),
                ),
            ),
            SettingItem::new(
                t("Light theme"),
                SettingField::scrollable_dropdown(
                    theme_options(ThemeMode::Light, cx),
                    |cx: &App| Settings::get(cx).light_theme.clone().into(),
                    |v: SharedString, cx: &mut App| Settings::update(cx, |s| s.light_theme = v.to_string()),
                ),
            ),
            SettingItem::new(
                t("Dark theme"),
                SettingField::scrollable_dropdown(
                    theme_options(ThemeMode::Dark, cx),
                    |cx: &App| Settings::get(cx).dark_theme.clone().into(),
                    |v: SharedString, cx: &mut App| Settings::update(cx, |s| s.dark_theme = v.to_string()),
                ),
            ),
        ]),
        SettingGroup::new().title(t("Fonts")).items([
            SettingItem::new(
                t("Interface size"),
                SettingField::number_input(number(10., 24.), |cx: &App| Settings::get(cx).ui_font_size as f64, |v, cx: &mut App| Settings::update(cx, |s| s.ui_font_size = v as f32)),
            ),
            SettingItem::new(
                t("Editor size"),
                SettingField::number_input(number(8., 32.), |cx: &App| Settings::get(cx).editor_font_size as f64, |v, cx: &mut App| Settings::update(cx, |s| s.editor_font_size = v as f32)),
            ),
            SettingItem::new(t("Editor font"), text_field!(editor_font_family)),
        ]),
    ]);

    let editor = SettingPage::new(t("Editor")).icon(IconName::FileCode).group(SettingGroup::new().items([
        SettingItem::new(
            t("Tab size"),
            SettingField::number_input(number(1., 16.), |cx: &App| Settings::get(cx).tab_size as f64, |v, cx: &mut App| Settings::update(cx, |s| s.tab_size = v as usize)),
        ),
        SettingItem::new(t("Soft wrap"), switch_field!(soft_wrap)),
        SettingItem::new(
            t("Auto save"),
            SettingField::dropdown(
                choice(&[("0", "Off"), ("500", "0.5 s"), ("1000", "1 s"), ("3000", "3 s")]),
                |cx: &App| Settings::get(cx).autosave_ms.to_string().into(),
                |v: SharedString, cx: &mut App| Settings::update(cx, |s| s.autosave_ms = v.parse().unwrap_or(1000)),
            ),
        ),
        SettingItem::new(t("Format on save"), switch_field!(format_on_save)),
    ]));

    let ai = SettingPage::new(t("AI")).icon(IconName::Bot).groups([
        SettingGroup::new().title(t("GitHub Copilot")).items([
            SettingItem::render(|_, _, cx| copilot_accounts(cx)),
            SettingItem::new(
                t("Account"),
                SettingField::dropdown(
                    std::iter::once(("auto".into(), t("Auto (smart balancing)").into()))
                        .chain(cx.global::<CopilotState>().accounts.iter().map(|a| (a.login().into(), a.login().into())))
                        .collect(),
                    |cx: &App| Settings::get(cx).copilot_account.clone().into(),
                    |v: SharedString, cx: &mut App| Settings::update(cx, |s| s.copilot_account = v.to_string()),
                ),
            ),
            SettingItem::new(t("Inline suggestions"), switch_field!(copilot_inline)),
            SettingItem::new(
                t("Completion model"),
                SettingField::scrollable_dropdown(
                    copilot_models(cx),
                    |cx: &App| Settings::get(cx).completion_model.clone().into(),
                    |v: SharedString, cx: &mut App| Settings::update(cx, |s| s.completion_model = v.to_string()),
                ),
            ),
        ]),
        SettingGroup::new().title(t("Model servers")).items([SettingItem::render(|_, window, cx| providers_view(window, cx))]),
        SettingGroup::new().title(t("This project (.forgecode)")).items([SettingItem::render(|_, _, _| {
            h_flex()
                .gap_2()
                .child(Button::new("open-instructions").small().icon(IconName::Bot).label(t("AI instructions"))
                    .on_click(|_, w, cx| w.dispatch_action(Box::new(crate::workspace::OpenProjectInstructions), cx)))
                .child(Button::new("open-project-settings").small().icon(IconName::Settings).label(t("Project settings"))
                    .on_click(|_, w, cx| w.dispatch_action(Box::new(crate::workspace::OpenProjectSettings), cx)))
                .into_any_element()
        })]),
        SettingGroup::new().title(t("Agent")).items([
            SettingItem::new(
                t("Context size"),
                SettingField::number_input(
                    NumberFieldOptions { min: 4_000., max: 1_000_000., step: 1_000. },
                    |cx: &App| Settings::get(cx).ai_context_chars as f64,
                    |v, cx: &mut App| Settings::update(cx, |s| s.ai_context_chars = v as usize),
                ),
            ),
            SettingItem::new(
                t("Mode"),
                SettingField::dropdown(
                    Mode::ALL.iter().map(|m| (m.id().into(), t(m.label()).into())).collect(),
                    |cx: &App| Settings::get(cx).agent_mode.clone().into(),
                    |v: SharedString, cx: &mut App| Settings::update(cx, |s| s.agent_mode = v.to_string()),
                ),
            ),
            SettingItem::new(t("Delegation"), switch_field!(agent_delegation)),
            SettingItem::new(
                t("Security model"),
                SettingField::scrollable_dropdown(
                    std::iter::once((SharedString::from(""), SharedString::from(t("Automatic (local, else Copilot)"))))
                        .chain(catalog(cx).into_iter().map(|m| (m.model_ref.clone().into(), m.model_ref.into())))
                        .collect(),
                    |cx: &App| Settings::get(cx).review_model.clone().into(),
                    |v: SharedString, cx: &mut App| Settings::update(cx, |s| s.review_model = v.to_string()),
                ),
            ),
        ]),
        // Hermes (agent module): which models it may use through ForgeCode, and its context.
        SettingGroup::new().title(t("Hermes agent")).items([
            SettingItem::new(
                t("Context window (tokens)"),
                SettingField::number_input(
                    NumberFieldOptions { min: 8_000., max: 2_000_000., step: 1_000. },
                    |cx: &App| Settings::get(cx).agent_context_tokens as f64,
                    |v, cx: &mut App| Settings::update(cx, |s| s.agent_context_tokens = v as u32),
                ),
            ),
            SettingItem::render(|_, _, cx| agent_models(cx)),
            SettingItem::new(t("Supervisor (reviews each turn)"), switch_field!(agent_supervisor)),
            SettingItem::new(
                t("Supervisor model"),
                SettingField::scrollable_dropdown(
                    std::iter::once((SharedString::from(""), SharedString::from(t("Same as Hermes"))))
                        .chain(Settings::get(cx).agent_models.iter().map(|m| (m.clone().into(), m.clone().into())))
                        .collect(),
                    |cx: &App| Settings::get(cx).agent_supervisor_model.clone().into(),
                    |v: SharedString, cx: &mut App| Settings::update(cx, |s| s.agent_supervisor_model = v.to_string()),
                ),
            ),
        ]),
    ]);

    let modules = SettingPage::new(t("Modules"))
        .icon(IconName::Blocks)
        .group(SettingGroup::new().items([SettingItem::render(|_, _, cx| modules_list(cx))]))
        .group(
            SettingGroup::new().title(t("Marketplace")).items([
                SettingItem::new(t("Marketplace folder"), text_field!(marketplace_dir))
                    .description(t("Extensions are read from this folder (empty: the default one). Copy an extension folder there to publish it.")),
                SettingItem::render(|_, window, cx| super::marketplace::view(window, cx)),
            ]),
        );

    let languages = SettingPage::new(t("Languages")).icon(IconName::Code).groups([
        lang_group("python", "Python", true, Some("Python interpreter"), true),
        lang_group("php", "PHP", true, Some("PHP executable"), false),
        lang_group("javascript", "JavaScript", true, Some("Node executable"), false),
        lang_group("typescript", "TypeScript", true, Some("Node executable"), false),
        lang_group("c", "C", false, None, false),
        lang_group("cpp", "C++", false, None, false),
        lang_group("rust", "Rust", false, None, false),
        lang_group("go", "Go", false, None, false),
    ]);

    SettingsView::new("forge-settings").pages([appearance, editor, languages, ai, modules])
}

/// Run, debug and type checking options of one language, stored in `Settings::languages[key]`.
fn lang_group(key: &'static str, title: &'static str, typing: bool, runtime: Option<&'static str>, python: bool) -> SettingGroup {
    use crate::settings::LangSettings;
    fn get<T>(key: &'static str, f: impl Fn(&LangSettings) -> T) -> impl Fn(&App) -> T {
        move |cx: &App| f(&Settings::get(cx).lang(key))
    }
    fn set<T: 'static>(key: &'static str, f: impl Fn(&mut LangSettings, T) + 'static) -> impl Fn(T, &mut App) {
        move |v: T, cx: &mut App| Settings::update(cx, |s| f(s.languages.entry(key.to_string()).or_default(), v))
    }
    let mut items = vec![];
    if typing {
        items.push(
            SettingItem::new(
                t("Type checking"),
                SettingField::dropdown(
                    choice(&[("off", "Off"), ("basic", "Basic"), ("standard", "Standard"), ("strict", "Strict")]),
                    get(key, |l| SharedString::from(l.type_checking.clone())),
                    set(key, |l, v: SharedString| l.type_checking = v.to_string()),
                ),
            )
            .description(t("How strictly the language server reports type errors.")),
        );
    }
    if let Some(label) = runtime {
        use super::runtime_picker::{Runtime, field};
        let kind = match key {
            "python" => Runtime::Python,
            "php" => Runtime::Php,
            _ => Runtime::Node,
        };
        items.push(SettingItem::new(t(label), SettingField::render(move |_, _, cx: &mut App| field(key, kind, cx))));
    }
    items.push(SettingItem::new(
        t("Program arguments"),
        SettingField::input(get(key, |l| SharedString::from(l.args.clone())), set(key, |l, v: SharedString| l.args = v.to_string())),
    ));
    items.push(SettingItem::new(t("Stop at program start when debugging"), SettingField::switch(get(key, |l| l.stop_on_entry), set(key, |l, v| l.stop_on_entry = v))));
    if python {
        items.push(
            SettingItem::new(t("Debug only my code"), SettingField::switch(get(key, |l| l.just_my_code), set(key, |l, v| l.just_my_code = v)))
                .description(t("Step over library code.")),
        );
    }
    SettingGroup::new().title(title).items(items)
}

fn copilot_models(cx: &App) -> Vec<(SharedString, SharedString)> {
    let mut models = cx.global::<CopilotState>().models.clone();
    let current = Settings::get(cx).completion_model.clone();
    if !models.contains(&current) {
        models.insert(0, current);
    }
    models.into_iter().map(|m| (m.clone().into(), m.into())).collect()
}

/// The settings section keeps one providers editor so its inputs survive re-renders.
struct ProvidersEditor(Entity<ProvidersView>);
impl Global for ProvidersEditor {}

fn providers_view(window: &mut Window, cx: &mut App) -> AnyElement {
    if !cx.has_global::<ProvidersEditor>() {
        let view = cx.new(|cx| ProvidersView::new(window, cx));
        cx.set_global(ProvidersEditor(view));
    }
    cx.global::<ProvidersEditor>().0.clone().into_any_element()
}

fn copilot_accounts(cx: &mut App) -> AnyElement {
    let state = cx.global::<CopilotState>();
    let accounts = state.accounts.clone();
    let sign_in = state.sign_in.clone();
    let choice = Settings::get(cx).copilot_account.clone();
    let theme = cx.theme();
    let (muted, border, danger, warning, green) = (theme.muted_foreground, theme.border, theme.danger, theme.warning, theme.green);
    v_flex()
        .w_full()
        .gap_2()
        .children(accounts.iter().map(|a| {
            let info = a.info.lock().unwrap().clone();
            let login = a.login();
            let active = choice == login || choice == "auto";
            let remove = login.clone();
            v_flex()
                .gap_2()
                .p_3()
                .rounded_md()
                .border_1()
                .border_color(border)
                .child(
                    h_flex()
                        .gap_3()
                        .child(match info.as_ref().and_then(|i| i.avatar.clone()) {
                            Some(img) => Avatar::new().small().src(img).into_any_element(),
                            None => Avatar::new().small().name(login.clone()).into_any_element(),
                        })
                        .child(
                            v_flex()
                                .flex_1()
                                .child(h_flex().gap_2().text_sm()
                                    .child(div().font_weight(FontWeight::MEDIUM).child(login.clone()))
                                    .when_some(info.as_ref().filter(|i| !i.name.is_empty()), |el, i| el.child(div().text_color(muted).child(i.name.clone()))))
                                .child(div().text_xs().text_color(muted).child(match &info {
                                    Some(i) => format!("Copilot {}{}{}", i.plan, if i.reset_date.is_empty() { String::new() } else { format!(" · {} {}", t("resets"), i.reset_date) }, if a.cooling_down() { format!(" · {}", t("rate limited")) } else { String::new() }),
                                    None => t("Loading…").into(),
                                })),
                        )
                        .when(active, |el| el.child(div().size(px(8.)).rounded_full().bg(green)))
                        .child(Button::new(SharedString::from(format!("rm-{login}"))).ghost().xsmall().icon(IconName::Delete).tooltip(t("Remove"))
                            .on_click(move |_, _, cx| CopilotState::remove(cx, &remove))),
                )
                .children(info.as_ref().map(|i| {
                    // Chat and completions are unlimited on paid plans: only the monthly premium quota is worth a bar.
                    let premium = i.quotas.iter().find(|q| q.name == "premium_interactions" && !q.unlimited).cloned();
                    let unlimited: Vec<&str> = i.quotas.iter().filter(|q| q.unlimited).map(|q| match q.name.as_str() {
                        "chat" => "chat",
                        "completions" => t("completions"),
                        other => if other == "premium_interactions" { t("premium requests") } else { "?" },
                    }).collect();
                    v_flex()
                        .gap_1()
                        .when_some(premium, |el, q| {
                            let used = q.used();
                            let frac = if q.entitlement > 0. { used / q.entitlement } else { 0. };
                            el.child(h_flex().justify_between().text_xs()
                                    .child(div().text_color(muted).child(t("Premium requests this month")))
                                    .child(format!("{:.0} / {:.0}", used, q.entitlement)))
                                .child(Progress::new(SharedString::from(format!("q-{login}"))).value((frac * 100.) as f32)
                                    .color(if frac > 0.9 { danger } else if frac > 0.7 { warning } else { green }))
                                .child(h_flex().justify_between().text_xs().text_color(muted)
                                    .child(format!("{:.0} {}", q.remaining, t("left")))
                                    .child(if q.overage { t("overage allowed") } else { "" }))
                        })
                        .when(!unlimited.is_empty(), |el| el.child(div().text_xs().text_color(muted).child(format!("{} : {}", t("unlimited"), unlimited.join(", ")))))
                }))
        }))
        .child(
            h_flex()
                .gap_2()
                .child(Button::new("copilot-add").small().primary().icon(IconName::Github)
                    .label(if accounts.is_empty() { t("Sign in with GitHub") } else { t("Add account") })
                    .on_click(|_, _, cx| CopilotState::sign_in(cx)))
                .child(Button::new("copilot-refresh").small().ghost().icon(IconName::RefreshCw).tooltip(t("Refresh")).on_click(|_, _, cx| CopilotState::refresh(cx)))
                .child(match sign_in {
                    SignIn::Idle => div(),
                    SignIn::Pending { user_code, uri } => div().text_xs().text_color(warning).child(format!("{user_code} → {uri}")),
                    SignIn::Error(e) => div().text_xs().text_color(danger).child(e),
                }),
        )
        .into_any_element()
}

fn modules_list(cx: &mut App) -> AnyElement {
    let plugins = Plugins::global(cx).clone();
    let muted = cx.theme().muted_foreground;
    let danger = cx.theme().danger;
    v_flex()
        .w_full()
        .gap_2()
        .child(
            h_flex()
                .gap_2()
                .child(div().flex_1().text_xs().text_color(muted).truncate().child(plugins::plugins_dir().display().to_string()))
                .child(Button::new("new-module").small().primary().label(t("New module…")).on_click(|_, window, cx| {
                    window.dispatch_action(Box::new(crate::workspace::NewModule), cx);
                }))
                .child(Button::new("reload-modules").small().label(t("Reload modules")).on_click(|_, window, cx| {
                    window.dispatch_action(Box::new(crate::workspace::ReloadModules), cx);
                })),
        )
        .children(plugins.errors.iter().map(|(path, e)| {
            div().text_xs().text_color(danger).child(format!("{}: {e}", path.display()))
        }))
        .into_any_element()
}

/// Models Hermes may use: Copilot's and OpenAI-compatible servers' (the gateway relays those).
fn agent_models(cx: &mut App) -> AnyElement {
    use gpui_kit::component::checkbox::Checkbox;
    let settings = Settings::get(cx).clone();
    let resolver = crate::ai::models::Resolver { providers: settings.providers.clone(), copilot: vec![] };
    let models: Vec<String> = catalog(cx).into_iter().map(|m| m.model_ref).filter(|m| crate::ai::gateway::servable(m, &resolver)).collect();
    let muted = cx.theme().muted_foreground;
    v_flex()
        .gap_1()
        .child(div().text_sm().child(t("Models Hermes may use")))
        .child(div().text_xs().text_color(muted).child(t("Hermes reaches them through ForgeCode: your Copilot accounts and local servers. Claude API models are not relayed.")))
        .when(models.is_empty(), |el| el.child(div().text_xs().text_color(muted).child(t("Sign in to Copilot or add a model server above first."))))
        .children(models.into_iter().enumerate().map(|(ix, m)| {
            let checked = settings.agent_models.contains(&m);
            Checkbox::new(("agent-model", ix)).label(m.clone()).checked(checked).on_click(move |on, _, cx| {
                let m = m.clone();
                Settings::update(cx, |s| {
                    s.agent_models.retain(|x| *x != m);
                    if *on {
                        s.agent_models.push(m);
                    }
                });
            })
        }))
        .into_any_element()
}
