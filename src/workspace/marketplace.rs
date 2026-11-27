//! The extension marketplace (Settings › Modules): search, filters and a card per extension
//! with its icon, author, categories, what it contributes and its install / update / remove
//! actions. It redraws when modules are installed or removed.

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, Selectable as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::i18n::t;
use crate::plugins::{Plugin, Plugins, marketplace};
use crate::settings::Settings;

#[derive(Clone, Copy, PartialEq)]
enum Filter {
    All,
    Installed,
    Updates,
}

pub struct MarketplaceView {
    search: Entity<InputState>,
    filter: Filter,
    _subs: Vec<Subscription>,
}

/// One view for the whole app, so the search text survives the settings page re-rendering.
struct Shared(Entity<MarketplaceView>);
impl Global for Shared {}

pub fn view(window: &mut Window, cx: &mut App) -> AnyElement {
    if !cx.has_global::<Shared>() {
        let view = cx.new(|cx| MarketplaceView::new(window, cx));
        cx.set_global(Shared(view));
    }
    cx.global::<Shared>().0.clone().into_any_element()
}

/// A marketplace extension and its installed copy, if any.
struct Entry {
    available: Option<Plugin>,
    installed: Option<Plugin>,
}

impl Entry {
    fn plugin(&self) -> &Plugin {
        self.available.as_ref().or(self.installed.as_ref()).expect("one of the two")
    }

    fn update(&self) -> Option<(String, String)> {
        let (a, i) = (self.available.as_ref()?, self.installed.as_ref()?);
        marketplace::newer(&a.manifest.version, &i.manifest.version).then(|| (i.manifest.version.clone(), a.manifest.version.clone()))
    }
}

impl MarketplaceView {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(t("Search extensions")));
        let subs = vec![
            cx.subscribe(&search, |_, _, e: &InputEvent, cx| {
                if matches!(e, InputEvent::Change) {
                    cx.notify();
                }
            }),
            cx.observe_global::<Plugins>(|_, cx| cx.notify()),
            cx.observe_global::<Settings>(|_, cx| cx.notify()),
        ];
        Self { search, filter: Filter::All, _subs: subs }
    }

    fn entries(&self, cx: &App) -> (Vec<Entry>, Vec<(std::path::PathBuf, String)>) {
        let market = marketplace::marketplace_dir(&Settings::get(cx).marketplace_dir);
        let installed = Plugins::global(cx).clone();
        let (available, errors) = marketplace::entries(&market, &installed);
        let mut list: Vec<Entry> = available.into_iter().map(|(a, i)| Entry { available: Some(a), installed: i }).collect();
        // Modules installed by hand (not in the marketplace) are listed too.
        for p in installed.plugins {
            if !list.iter().any(|e| e.installed.as_ref().is_some_and(|i| i.dir == p.dir)) {
                list.push(Entry { available: None, installed: Some(p) });
            }
        }
        list.sort_by_key(|e| e.plugin().manifest.name.to_lowercase());
        (list, errors)
    }

    fn act(&mut self, result: std::io::Result<()>, message: String, window: &mut Window, cx: &mut Context<Self>) {
        match result {
            Ok(()) => {
                // Directly, not through an action: it must not depend on where the focus is.
                crate::load_modules(cx);
                window.push_notification(Notification::new().message(message), cx);
            }
            Err(e) => window.push_notification(Notification::new().message(e.to_string()), cx),
        }
        cx.notify();
    }

    fn render_card(&self, ix: usize, entry: Entry, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let p = entry.plugin().clone();
        let m = &p.manifest;
        let update = entry.update();
        let (installed, available) = (entry.installed.clone(), entry.available.clone());
        let contributions: Vec<String> = [
            (m.commands.len(), t("command"), t("commands")),
            (m.tools.len(), t("AI tool"), t("AI tools")),
            (m.languages.len(), t("language"), t("languages")),
        ]
        .into_iter()
        .filter(|(n, _, _)| *n > 0)
        .map(|(n, one, many)| format!("{n} {}", if n == 1 { one } else { many }))
        .chain(m.themes.is_some().then(|| t("themes").to_string()))
        .collect();
        let chip = |text: String| div().px_1p5().py_0p5().rounded_sm().bg(theme.secondary).text_color(theme.secondary_foreground).text_xs().child(text);
        let status = match (&installed, &update) {
            (Some(_), Some((from, to))) => h_flex().gap_1().text_color(theme.warning).child(IconName::ArrowUp).child(format!("{from} → {to}")).into_any_element(),
            (Some(_), None) => h_flex().gap_1().text_color(theme.green).child(IconName::Check).child(t("Installed")).into_any_element(),
            (None, _) => div().into_any_element(),
        };
        let mut actions: Vec<AnyElement> = vec![];
        if let Some(a) = available.clone().filter(|_| installed.is_none() || update.is_some()) {
            let label = if installed.is_none() { t("Install") } else { t("Update") };
            let name = a.manifest.name.clone();
            actions.push(
                Button::new(("mk-install", ix))
                    .small()
                    .primary()
                    .label(label)
                    .on_click(cx.listener(move |this, _, w, cx| this.act(marketplace::install(&a).map(drop), format!("{name} ✓"), w, cx)))
                    .into_any_element(),
            );
        }
        if let Some(i) = installed.clone() {
            let dir = i.dir.clone();
            actions.push(Button::new(("mk-folder", ix)).small().ghost().icon(IconName::FolderOpen).tooltip(t("Open folder")).on_click(move |_, _, cx| cx.open_with_system(&dir)).into_any_element());
            let name = i.manifest.name.clone();
            actions.push(
                Button::new(("mk-remove", ix))
                    .small()
                    .ghost()
                    .icon(IconName::Delete)
                    .tooltip(t("Remove"))
                    .on_click(cx.listener(move |this, _, w, cx| this.act(marketplace::remove(&i), format!("{name} : {}", t("removed")), w, cx)))
                    .into_any_element(),
            );
        }
        v_flex()
            .id(("mk-card", ix))
            .debug_selector(move || format!("mk-card-{ix}"))
            .w_full()
            .gap_2()
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(if update.is_some() { theme.warning.opacity(0.6) } else { theme.border })
            .bg(theme.background)
            .hover(|s| s.border_color(theme.primary.opacity(0.5)))
            .child(
                h_flex()
                    .gap_3()
                    .items_start()
                    .child(
                        div()
                            .flex_none()
                            .size(px(40.))
                            .rounded_md()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(theme.primary.opacity(0.12))
                            .text_color(theme.primary)
                            .child(crate::icons::module_icon(m.icon.as_deref(), &p.dir, IconName::Blocks, px(24.))),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(h_flex().gap_2().child(div().truncate().font_weight(FontWeight::SEMIBOLD).child(m.name.clone())).child(div().text_xs().text_color(theme.muted_foreground).child(m.version.clone())))
                            .child(div().text_xs().text_color(theme.muted_foreground).truncate().child(if m.author.is_empty() {
                                if available.is_none() { t("Local module").to_string() } else { String::new() }
                            } else {
                                m.author.clone()
                            })),
                    ),
            )
            .when(!m.description.is_empty(), |el| el.child(div().text_sm().text_color(theme.foreground.opacity(0.85)).line_clamp(2).child(m.description.clone())))
            .child(h_flex().flex_wrap().gap_1().children(m.categories.iter().cloned().map(chip)).children(contributions.into_iter().map(chip)))
            .child(h_flex().gap_1().items_center().text_xs().child(div().flex_1().child(status)).children(actions))
    }
}

impl Render for MarketplaceView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Equal columns, about 300 px each in the settings page (the window less its sidebar).
        let columns = ((f32::from(window.viewport_size().width) - 260.) / 300.).floor().clamp(1., 4.) as u16;
        let (entries, errors) = self.entries(cx);
        let query = self.search.read(cx).value().to_lowercase();
        let counts = (entries.len(), entries.iter().filter(|e| e.installed.is_some()).count(), entries.iter().filter(|e| e.update().is_some()).count());
        let shown: Vec<Entry> = entries
            .into_iter()
            .filter(|e| match self.filter {
                Filter::All => true,
                Filter::Installed => e.installed.is_some(),
                Filter::Updates => e.update().is_some(),
            })
            .filter(|e| {
                let m = &e.plugin().manifest;
                query.is_empty()
                    || [&m.name, &m.description, &m.author].iter().any(|s| s.to_lowercase().contains(&query))
                    || m.categories.iter().any(|c| c.to_lowercase().contains(&query))
            })
            .collect();
        let market = marketplace::marketplace_dir(&Settings::get(cx).marketplace_dir);
        let filter = self.filter;
        let tab = |id: &'static str, label: &'static str, n: usize, value: Filter, cx: &mut Context<Self>| {
            Button::new(id).small().ghost().selected(filter == value).label(format!("{} {n}", t(label))).on_click(cx.listener(move |this, _, _, cx| {
                this.filter = value;
                cx.notify();
            }))
        };
        let toolbar = h_flex()
            .gap_2()
            .child(div().flex_1().min_w(px(160.)).child(Input::new(&self.search).small().prefix(IconName::Search)))
            .child(tab("mk-all", "All", counts.0, Filter::All, cx))
            .child(tab("mk-installed", "Installed", counts.1, Filter::Installed, cx))
            .child(tab("mk-updates", "Updates", counts.2, Filter::Updates, cx))
            .child({
                let market = market.clone();
                Button::new("mk-open").small().ghost().icon(IconName::FolderOpen).tooltip(t("Open the marketplace folder")).on_click(move |_, _, cx| {
                    let _ = std::fs::create_dir_all(&market);
                    cx.open_with_system(&market);
                })
            })
            .child(Button::new("mk-refresh").small().ghost().icon(IconName::RefreshCw).tooltip(t("Refresh")).on_click(|_, _, cx| crate::load_modules(cx)));
        let theme = cx.theme().clone();
        let empty = shown.is_empty();
        let cards: Vec<AnyElement> = shown.into_iter().enumerate().map(|(ix, e)| self.render_card(ix, e, cx).into_any_element()).collect();
        v_flex()
            .w_full()
            .gap_3()
            .child(toolbar)
            .child(div().text_xs().text_color(theme.muted_foreground).truncate().child(market.display().to_string()))
            .when(empty, |el| {
                el.child(
                    v_flex()
                        .items_center()
                        .gap_2()
                        .py_8()
                        .text_color(theme.muted_foreground)
                        .child(div().text_color(theme.muted_foreground.opacity(0.6)).child(IconName::PackageSearch))
                        .child(div().text_sm().child(if query.is_empty() { t("No extension in this folder.") } else { t("No extension matches this search.") })),
                )
            })
            .child(div().grid().grid_cols(columns).gap_3().children(cards))
            .children(errors.into_iter().map(|(path, e)| div().text_xs().text_color(theme.danger).child(format!("{}: {e}", path.display()))))
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{AppContext as _, TestAppContext};

    /// The extension cards share equal columns: one alone on the last row is as wide as the others.
    #[gpui_kit::test]
    fn cards_have_equal_widths(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            let base = crate::settings::Settings::default();
            cx.set_global(base.clone());
            cx.set_global(crate::settings::Layers { base, ..Default::default() });
            cx.set_global(crate::plugins::Plugins::default());
        });
        let market = crate::plugins::marketplace::marketplace_dir("");
        crate::plugins::marketplace::seed(&market).unwrap();
        struct V;
        impl gpui_kit::Render for V {
            fn render(&mut self, window: &mut gpui_kit::Window, cx: &mut gpui_kit::Context<Self>) -> impl gpui_kit::IntoElement {
                super::view(window, cx)
            }
        }
        let (_, cx) = cx.add_window_view(|window, cx| gpui_kit::base::Root::new(cx.new(|_| V), window, cx));
        // 3 columns for the 4 bundled extensions: the 4th is alone on its row.
        cx.simulate_resize(gpui_kit::size(gpui_kit::px(1200.), gpui_kit::px(900.)));
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let widths: Vec<_> = (0..4).map(|i| cx.debug_bounds(Box::leak(format!("mk-card-{i}").into_boxed_str())).expect("card").size.width).collect();
        assert!(widths.iter().all(|w| *w == widths[0]), "{widths:?}");
    }
}
