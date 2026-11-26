//! Fuzzy pickers: project files (⌘P, chat links) and the command palette (⌘⇧P).

use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::icons;
use crate::lsp::provider::fuzzy_score;

const MAX_FILES: usize = 20_000;
const MAX_SHOWN: usize = 50;

#[derive(Clone)]
pub struct PickerItem {
    pub label: String,
    pub detail: String,
    /// Keyboard shortcut, shown right-aligned.
    pub keys: String,
    pub icon: IconName,
    pub tint: Option<Hsla>,
}

type OnPick = Rc<dyn Fn(usize, &mut Window, &mut App)>;

pub struct Picker {
    items: Vec<PickerItem>,
    query: Entity<InputState>,
    selected: usize,
    on_pick: OnPick,
    _sub: Subscription,
}

/// Project files relative to `root`, respecting .gitignore.
pub fn project_files(root: &Path) -> Vec<String> {
    ignore::WalkBuilder::new(root)
        .build()
        .flatten()
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .filter_map(|e| e.path().strip_prefix(root).ok().map(|p| p.display().to_string()))
        .take(MAX_FILES)
        .collect()
}

/// Indices of the best matches; the label counts more than the detail.
fn rank(items: &[PickerItem], query: &str) -> Vec<usize> {
    let query = query.trim();
    if query.is_empty() {
        return (0..items.len().min(MAX_SHOWN)).collect(); // keep the given order
    }
    let mut scored: Vec<(i32, usize)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, it)| {
            let score = fuzzy_score(query, &it.label).map(|s| s + 100).or_else(|| fuzzy_score(query, &format!("{} {}", it.detail, it.label)))?;
            Some((score, i))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(items[a.1].label.len().cmp(&items[b.1].label.len())));
    scored.into_iter().take(MAX_SHOWN).map(|(_, i)| i).collect()
}

/// Files ranked for `query` (also used by the chat's `@` mentions).
pub fn filter(files: &[String], query: &str) -> Vec<String> {
    let items: Vec<PickerItem> = files.iter().map(|f| file_item(f)).collect();
    rank(&items, query).into_iter().map(|i| files[i].clone()).collect()
}

fn file_item(rel: &str) -> PickerItem {
    let (dir, name) = rel.rsplit_once('/').map_or(("", rel), |(d, n)| (d, n));
    let (icon, tint) = icons::file_icon(Path::new(rel));
    PickerItem { label: name.to_string(), detail: dir.to_string(), keys: String::new(), icon, tint: Some(tint) }
}

impl Picker {
    pub fn open(items: Vec<PickerItem>, placeholder: SharedString, window: &mut Window, cx: &mut App, on_pick: impl Fn(usize, &mut Window, &mut App) + 'static) {
        let query = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        let focus = query.read(cx).focus_handle(cx);
        let view = cx.new(|cx| {
            let _sub = cx.subscribe_in(&query, window, |this: &mut Self, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => {
                    this.selected = 0;
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => this.confirm(window, cx),
                _ => {}
            });
            Self { items, query, selected: 0, on_pick: Rc::new(on_pick), _sub }
        });
        window.open_dialog(cx, move |dialog, _, _| dialog.w(px(560.)).margin_top(px(60.)).close_button(false).child(view.clone()));
        // The dialog takes focus when it opens: hand it to the search field right after.
        window.defer(cx, move |window, cx| focus.focus(window, cx));
    }

    /// Pick a project file.
    pub fn files(root: PathBuf, title: SharedString, window: &mut Window, cx: &mut App, on_pick: impl Fn(PathBuf, &mut Window, &mut App) + 'static) {
        let files = project_files(&root);
        let items = files.iter().map(|f| file_item(f)).collect();
        Self::open(items, title, window, cx, move |ix, w, cx| on_pick(root.join(&files[ix]), w, cx));
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = rank(&self.items, &self.query.read(cx).value()).into_iter().nth(self.selected) else { return };
        window.close_dialog(cx);
        (self.on_pick)(ix, window, cx);
    }
}

impl Render for Picker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let matches = rank(&self.items, &self.query.read(cx).value());
        let selected = self.selected.min(matches.len().saturating_sub(1));
        let theme = cx.theme();
        v_flex()
            .gap_2()
            .on_key_down(cx.listener(|this, e: &KeyDownEvent, _, cx| match e.keystroke.key.as_str() {
                "down" => {
                    this.selected += 1;
                    cx.notify();
                }
                "up" => {
                    this.selected = this.selected.saturating_sub(1);
                    cx.notify();
                }
                _ => {}
            }))
            .child(Input::new(&self.query))
            .child(
                v_flex().id("picker-list").max_h(px(380.)).overflow_y_scrollbar().children(matches.into_iter().enumerate().map(|(row, ix)| {
                    let item = &self.items[ix];
                    h_flex()
                        .id(("pick", row))
                        .gap_2()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .text_sm()
                        .cursor_pointer()
                        .when(row == selected, |el| el.bg(theme.list_active))
                        .hover(|s| s.bg(theme.list_hover))
                        .on_click(cx.listener(move |this, _, w, cx| {
                            this.selected = row;
                            this.confirm(w, cx);
                        }))
                        .child(div().text_color(item.tint.unwrap_or(theme.muted_foreground)).child(item.icon.clone()))
                        .child(item.label.clone())
                        .child(div().flex_1().truncate().text_xs().text_color(theme.muted_foreground).child(item.detail.clone()))
                        .when(!item.keys.is_empty(), |el| {
                            el.child(div().px_1p5().rounded_sm().bg(theme.secondary).text_xs().text_color(theme.muted_foreground).font_family(theme.mono_font_family.clone()).child(item.keys.clone()))
                        })
                })),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::filter;

    #[test]
    fn file_names_rank_before_folders() {
        let files: Vec<String> = ["src/main.rs", "main/other.rs", "docs/maintenance.md"].iter().map(|s| s.to_string()).collect();
        assert_eq!(filter(&files, "main")[0], "src/main.rs");
        assert_eq!(filter(&files, "zzz").len(), 0);
        assert_eq!(filter(&files, "").len(), 3);
    }
}
