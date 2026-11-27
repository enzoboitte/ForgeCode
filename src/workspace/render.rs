//! Rendering of the IDE window.

use std::path::PathBuf;

use gpui_kit::assets::IconName;
use gpui_kit::component::avatar::Avatar;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::diff::{Diff, DiffMode};
use gpui_kit::component::input::Editor;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::resizable::{h_resizable, resizable_panel, v_resizable};
use gpui_kit::component::scroll::{ScrollableElement as _, ScrollbarAxis};
use gpui_kit::component::text::TextView;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, TitleBar, Selectable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use lsp_types::DiagnosticSeverity;

use super::pane::{DraggedFile, DraggedTab, Layout, Zone};
use super::*;
use crate::ai::copilot::CopilotState;
use crate::i18n::t;

fn icon_button(id: impl Into<ElementId>, icon: IconName, tip: &'static str) -> Button {
    Button::new(id).ghost().xsmall().icon(icon).tooltip(t(tip))
}

impl Workspace {
    fn render_activity_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let changes = self.git.read(cx).changed_count();
        let item = |id: &'static str, icon: IconName, tip: &'static str, active: bool| {
            Button::new(id).ghost().icon(icon).tooltip(t(tip)).selected(active).size(px(34.))
        };
        let accounts = cx.global::<CopilotState>().accounts.clone();
        let first = accounts.first().and_then(|a| a.info.lock().unwrap().clone());
        let avatar = match first.as_ref().and_then(|i| i.avatar.clone()) {
            Some(img) => Avatar::new().with_size(px(18.)).src(img).into_any_element(),
            None => div().child(IconName::CircleUser).into_any_element(),
        };
        let settings_open = matches!(self.active_tab(), Some(Tab::Settings));
        v_flex()
            .w(px(44.))
            .h_full()
            .py_1()
            .gap_1()
            .items_center()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .child(item("act-explorer", IconName::Files, "Explorer", self.sidebar == Some(Sidebar::Explorer))
                .on_click(cx.listener(|t, _, _, cx| t.toggle_sidebar(Sidebar::Explorer, cx))))
            .child(item("act-search", IconName::Search, "Search in project", self.sidebar == Some(Sidebar::Search))
                .on_click(cx.listener(|t, _, w, cx| {
                    if t.sidebar == Some(Sidebar::Search) {
                        t.sidebar = None;
                        cx.notify();
                    } else {
                        t.show_search(&ShowSearch, w, cx);
                    }
                })))
            .child(
                div()
                    .relative()
                    .child(item("act-git", IconName::GitBranch, "Source Control", self.sidebar == Some(Sidebar::Git))
                        .on_click(cx.listener(|t, _, _, cx| t.toggle_sidebar(Sidebar::Git, cx))))
                    .when(changes > 0, |el| {
                        el.child(
                            div().absolute().bottom_0().right_0().px_1().rounded_full().bg(theme.primary)
                                .text_color(theme.primary_foreground).text_size(px(9.)).child(changes.to_string()),
                        )
                    }),
            )
            // Databases module: just under Source Control.
            .when(crate::plugins::Plugins::global(cx).database().is_some(), |el| {
                el.child(item("act-db", IconName::Database, "Databases", self.sidebar == Some(Sidebar::Databases))
                    .on_click(cx.listener(|t, _, _, cx| t.toggle_sidebar(Sidebar::Databases, cx))))
            })
            // Conan module: just under Databases.
            .when(crate::plugins::Plugins::global(cx).conan().is_some(), |el| {
                el.child(item("act-conan", IconName::Package, "Conan packages", self.sidebar == Some(Sidebar::Conan))
                    .on_click(cx.listener(|t, _, _, cx| t.toggle_sidebar(Sidebar::Conan, cx))))
            })
            .child(div().flex_1())
            .child(Button::new("act-account").ghost().size(px(34.)).child(avatar).tooltip(t("Accounts")).dropdown_menu_with_anchor(Anchor::BottomLeft, |mut menu, _, cx| {
                let state = cx.global::<CopilotState>();
                let choice = Settings::get(cx).copilot_account.clone();
                if state.accounts.is_empty() {
                    menu = menu.label(t("No GitHub account"));
                } else {
                    menu = menu.item(PopupMenuItem::new(t("Auto (smart balancing)")).checked(choice == "auto")
                        .on_click(|_, _, cx| Settings::update(cx, |s| s.copilot_account = "auto".into())));
                }
                for a in state.accounts.clone() {
                    let login = a.login();
                    let detail = a.info.lock().unwrap().as_ref().map(|i| format!("{login} | {}", i.plan)).unwrap_or(login.clone());
                    menu = menu.item(PopupMenuItem::new(detail).checked(choice == login).on_click(move |_, _, cx| {
                        let login = login.clone();
                        Settings::update(cx, |s| s.copilot_account = login);
                    }));
                }
                menu.separator()
                    .item(PopupMenuItem::new(t("Add GitHub account…")).icon(IconName::Github).on_click(|_, _, cx| CopilotState::sign_in(cx)))
                    .item(PopupMenuItem::new(t("Manage accounts…")).icon(IconName::Settings).on_click(|_, w, cx| w.dispatch_action(Box::new(ToggleSettings), cx)))
            }))
            .child(item("act-settings", IconName::Settings, "Settings", settings_open)
                .on_click(cx.listener(|t, _, w, cx| t.open_settings(w, cx))))
    }

    pub(super) fn toggle_sidebar(&mut self, which: Sidebar, cx: &mut Context<Self>) {
        self.sidebar = if self.sidebar == Some(which) { None } else { Some(which) };
        if which == Sidebar::Git {
            self.git.update(cx, |g, cx| g.refresh(cx));
        }
        // Read when shown: listing the remotes' packages runs Conan.
        if which == Sidebar::Conan && self.sidebar == Some(Sidebar::Conan) {
            self.conan_panel.update(cx, |c, cx| c.refresh(false, cx));
        }
        cx.notify();
    }

    /// VS Code-like title bar: drags the window, menu bar left (not on macOS), title centered, actions right.
    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let module_buttons = self.module_buttons(crate::plugins::Placement::TitleBar, cx);
        let theme = cx.theme();
        let name = self.root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let branch = self.git.read(cx).repo.as_ref().map(|_| self.git.read(cx).status.branch.clone());
        TitleBar::new().child(
            h_flex()
                .w_full()
                .h_full()
                .pr_2()
                .gap_0p5()
                .when_some(self.menu_bar.clone(), |el, bar| el.child(bar))
                .child(
                    h_flex()
                        .flex_1()
                        .justify_center()
                        .gap_2()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!("ForgeCode - {name}"))
                        .when_some(branch, |el, b| el.child(h_flex().gap_1().child(IconName::GitBranch).child(b))),
                )
                // Module commands placed in the title bar, with their own icons. `occlude`: on Windows
                // the bar is a drag area that would take the clicks (the app menu bar does the same).
                .child(h_flex().occlude().gap_0p5().children(module_buttons)
                .child(icon_button("theme", if theme.is_dark() { IconName::Sun } else { IconName::Moon }, "Toggle theme")
                    .on_click(|_, w, cx| w.dispatch_action(Box::new(ToggleTheme), cx)))
                .child(div().mx_1().w(px(1.)).h(px(16.)).bg(theme.border))
                .child(icon_button("toggle-left", IconName::PanelLeft, "Toggle sidebar").selected(self.sidebar.is_some())
                    .on_click(cx.listener(|t, _, _, cx| { t.sidebar = if t.sidebar.is_some() { None } else { Some(Sidebar::Explorer) }; cx.notify(); })))
                .child(icon_button("toggle-bottom", IconName::PanelBottom, "Toggle panel").selected(self.show_bottom)
                    .on_click(cx.listener(|t, _, _, cx| { t.show_bottom = !t.show_bottom; cx.notify(); })))
                .child(icon_button("toggle-chat", IconName::PanelRight, "Toggle chat").selected(self.show_right)
                    .on_click(cx.listener(|t, _, _, cx| { t.show_right = !t.show_right; cx.notify(); })))),
        )
    }

    fn render_layout(&self, node: &Layout, path: String, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        match node {
            Layout::Pane(id) => self.render_pane(*id, window, cx),
            Layout::Split { axis, children } => {
                let id = SharedString::from(format!("split{path}"));
                let mut group = if *axis == Axis::Horizontal { h_resizable(id) } else { v_resizable(id) };
                for (i, child) in children.iter().enumerate() {
                    group = group.child(resizable_panel().child(self.render_layout(child, format!("{path}-{i}"), window, cx)));
                }
                group.into_any_element()
            }
        }
    }

    fn render_tab(&self, pane: usize, ix: usize, tab: &Tab, active: bool, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let (icon, tint, dirty) = match tab {
            Tab::File(b) => {
                let (icon, tint) = crate::icons::file_icon(&b.path);
                (icon, tint, b.dirty)
            }
            Tab::Diff { .. } => (IconName::GitCompare, muted, false),
            Tab::Graph(_) => (IconName::GitGraph, muted, false),
            Tab::Agents(_) => (IconName::Network, muted, false),
            Tab::Image { path, .. } => {
                let (icon, tint) = crate::icons::file_icon(path);
                (icon, tint, false)
            }
            Tab::Pdf(v) => {
                let (icon, tint) = crate::icons::file_icon(&v.read(cx).path);
                (icon, tint, false)
            }
            Tab::Video(v) => {
                let (icon, tint) = crate::icons::file_icon(&v.read(cx).path);
                (icon, tint, false)
            }
            Tab::Table(_) => (IconName::Table, muted, false),
            Tab::Console(_) => (IconName::SquareTerminal, muted, false),
            Tab::Settings => (IconName::Settings, muted, false),
        };
        let label = tab.label(cx);
        let transient = self.pane(pane).is_some_and(|p| p.is_transient(tab, cx));
        let group = SharedString::from(format!("tab-{pane}-{ix}"));
        let drag = DraggedTab { pane, ix, label: label.clone().into(), path: match tab { Tab::File(b) => Some(b.path.clone()), _ => None } };
        let focused_pane = pane == self.focused;
        h_flex()
            .id(group.clone())
            .debug_selector(|| format!("tab-{pane}-{ix}"))
            .group(group.clone())
            .relative()
            .h_full()
            .pl_3()
            .pr_1()
            .gap_1p5()
            .text_sm()
            .border_r_1()
            .border_color(theme.border)
            .cursor_pointer()
            .when(active, |el| el.bg(theme.background).text_color(theme.foreground))
            .when(!active, |el| el.text_color(theme.muted_foreground).hover(|s| s.bg(theme.secondary)))
            .when(active && focused_pane, |el| el.child(div().absolute().top_0().left_0().right_0().h(px(2.)).bg(theme.primary)))
            .on_click(cx.listener(move |t, e: &ClickEvent, w, cx| {
                // A double click keeps a temporary tab, as in VS Code.
                if e.click_count() >= 2 {
                    t.pin_tab(pane, ix, cx);
                }
                t.activate(pane, ix, w, cx)
            }))
            .on_mouse_down(MouseButton::Middle, cx.listener(move |t, _, w, cx| t.close_tab(pane, ix, w, cx)))
            .on_drag(drag, |d, _, _, cx| cx.new(|_| d.clone()))
            .child(div().text_color(tint).child(icon))
            // Temporary tab: its title in italics.
            .child(div().when(transient, |el| el.italic()).child(label))
            .child(
                // One slot: the unsaved dot, swapped for the close button on hover (like VS Code).
                div()
                    .relative()
                    .size(px(20.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .when(dirty, |el| el.child(div().group_hover(group.clone(), |s| s.invisible()).size(px(8.)).rounded_full().bg(theme.foreground.opacity(0.6))))
                    .child(
                        div()
                            .absolute()
                            .inset_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .when(dirty || !active, |el| el.invisible().group_hover(group.clone(), |s| s.visible()))
                            .child(Button::new(SharedString::from(format!("close-{pane}-{ix}"))).ghost().xsmall().icon(IconName::X)
                                .on_click(cx.listener(move |t, _, w, cx| {
                                    cx.stop_propagation();
                                    t.close_tab(pane, ix, w, cx);
                                }))),
                    ),
            )
            .into_any_element()
    }

    /// Run / Debug / Preview / Split for the active tab, at the right of its tab strip.
    fn render_tab_actions(&self, pane: usize, cx: &mut Context<Self>) -> AnyElement {
        let p = self.pane(pane);
        let Some(tab) = p.and_then(|p| p.tabs.get(p.active?)) else { return div().into_any_element() };
        let can_split = p.is_some_and(|p| p.tabs.len() > 1);
        let focus_then = move |t: &mut Self| t.focused = pane;
        let split = icon_button(("split", pane), IconName::Columns2, "Split right").disabled(!can_split)
            .on_click(cx.listener(move |t, _, w, cx| { focus_then(t); t.split_right(&SplitRight, w, cx) }));
        let actions = h_flex().gap_0p5().px_1();
        match tab {
            Tab::File(b) => {
                let runnable = crate::project::run_command(&self.root, &b.path, &b.lang).is_some();
                let debuggable = crate::debug::adapters::plan(&self.root, &b.path, &b.lang.lsp_id).is_some();
                let csv = super::csv_table::is_csv(&b.path);
                let glsl = b.lang.highlight == "glsl";
                let md = b.is_markdown() || csv || glsl;
                let preview = b.preview;
                let ws = cx.entity().downgrade();
                let module_buttons = self.module_buttons(crate::plugins::Placement::Editor, cx);
                actions
                    .children(module_buttons)
                    .when(runnable && self.output.read(cx).is_running(), |el| el.child(icon_button(("run", pane), IconName::Square, "Stop").on_click(cx.listener(|t, _, _, cx| t.output.update(cx, |o, cx| o.stop(cx))))))
                    .when(runnable && !self.output.read(cx).is_running(), |el| el.child(icon_button(("run", pane), IconName::Play, "Run").on_click(cx.listener(move |t, _, w, cx| { focus_then(t); t.run_file(&RunFile, w, cx) }))))
                    .when(debuggable, |el| el.child(icon_button(("debug", pane), IconName::Bug, "Debug").on_click(cx.listener(move |t, _, w, cx| { focus_then(t); t.start_debug(&StartDebug, w, cx) }))))
                    .when(md, |el| {
                        let (icon, label) = if csv { (IconName::Sheet, "Table") } else if glsl { (IconName::Zap, "Shader preview") } else { (IconName::BookOpen, "Preview") };
                        el.child(icon_button(("md", pane), icon, label).selected(preview != Preview::Off).dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                            let set = |mode: Preview| {
                                let ws = ws.clone();
                                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                                    let _ = ws.update(cx, |t, cx| {
                                        if let Some(b) = t.pane_mut(pane).and_then(|p| p.active.and_then(|a| p.tabs.get_mut(a))).and_then(|tab| match tab { Tab::File(b) => Some(b), _ => None }) {
                                            b.preview = mode;
                                            let path = b.path.clone();
                                            t.refresh_shader_preview(&path, window, cx);
                                        }
                                        cx.notify();
                                    });
                                }
                            };
                            menu.item(PopupMenuItem::new(t("Preview to the side")).checked(preview == Preview::Side).on_click(set(Preview::Side)))
                                .item(PopupMenuItem::new(t("Preview only")).checked(preview == Preview::Only).on_click(set(Preview::Only)))
                                .item(PopupMenuItem::new(t("Close preview")).disabled(preview == Preview::Off).on_click(set(Preview::Off)))
                        }))
                    })
                    .child(split)
                    .into_any_element()
            }
            Tab::Diff { patch, mode, .. } => {
                let (patch, mode) = (patch.clone(), *mode);
                actions
                    .child(Button::new(("diff-mode", pane)).ghost().xsmall().label(if mode == DiffMode::Split { t("Unified") } else { t("Side by side") })
                        .on_click(cx.listener(move |t, _, _, cx| {
                            let next = if mode == DiffMode::Split { DiffMode::Unified } else { DiffMode::Split };
                            let state = diff_state(&patch, next, cx);
                            if let Some(Tab::Diff { mode, state: s, .. }) = t.pane_mut(pane).and_then(|p| p.active.and_then(|a| p.tabs.get_mut(a))) {
                                *mode = next;
                                *s = state;
                            }
                            cx.notify();
                        })))
                    .child(split)
                    .into_any_element()
            }
            Tab::Image { path, zoom, .. } => {
                let path = path.clone();
                actions
                    .child(Button::new(("img-size", pane)).ghost().xsmall().label(format!("{:.0} %", zoom * 100.)).tooltip(t("Fit")).on_click(cx.listener(move |t, _, _, cx| {
                        if let Some(Tab::Image { zoom, scroll, .. }) = t.pane_mut(pane).and_then(|p| p.active.and_then(|a| p.tabs.get_mut(a))) {
                            *zoom = 1.;
                            scroll.set_offset(Point::default());
                        }
                        cx.notify();
                    })))
                    .child(icon_button(("img-open", pane), IconName::ExternalLink, "Open with default app").on_click(move |_, _, cx| cx.open_with_system(&path)))
                    .child(split)
                    .into_any_element()
            }
            Tab::Pdf(view) => {
                let (view, path) = (view.clone(), view.read(cx).path.clone());
                let (zoom, pages) = (view.read(cx).zoom, view.read(cx).page_count());
                actions
                    .child(div().text_xs().text_color(cx.theme().muted_foreground).child(format!("{pages} p.")))
                    .child(icon_button(("pdf-out", pane), IconName::Minus, "Zoom out").on_click({
                        let view = view.clone();
                        move |_, _, cx| view.update(cx, |v, cx| v.set_zoom(v.zoom / 1.25, None, cx))
                    }))
                    .child(Button::new(("pdf-zoom", pane)).ghost().xsmall().label(format!("{:.0} %", zoom * 100.)).tooltip(t("Actual size")).on_click({
                        let view = view.clone();
                        move |_, _, cx| view.update(cx, |v, cx| v.set_zoom(1., None, cx))
                    }))
                    .child(icon_button(("pdf-in", pane), IconName::Plus, "Zoom in").on_click(move |_, _, cx| view.update(cx, |v, cx| v.set_zoom(v.zoom * 1.25, None, cx))))
                    .child(icon_button(("pdf-open", pane), IconName::ExternalLink, "Open with default app").on_click(move |_, _, cx| cx.open_with_system(&path)))
                    .child(split)
                    .into_any_element()
            }
            _ => actions.child(split).into_any_element(),
        }
    }

    fn render_tab_body(&self, pane: usize, tab: &Tab, cx: &mut Context<Self>) -> AnyElement {
        match tab {
            Tab::File(b) => {
                let path = b.path.clone();
                let (enter_editor, tab_editor, key_editor) = (b.editor.clone(), b.editor.clone(), b.editor.clone());
                let (back_editor, esc_editor, click_editor) = (b.editor.clone(), b.editor.clone(), b.editor.clone());
                let lsp_id = b.lang.lsp_id.clone();
                let editor = div()
                    .size_full()
                    // Like VS Code: a quote or bracket typed over a selection surrounds it.
                    .capture_key_down(move |e: &KeyDownEvent, window, cx| {
                        let m = &e.keystroke.modifiers;
                        if m.control || m.platform || m.function {
                            return;
                        }
                        if let Some(typed) = e.keystroke.key_char.as_deref() {
                            if super::editing::surround_typed(&key_editor, &lsp_id, typed, window, cx) {
                                cx.stop_propagation();
                            }
                        }
                    })
                    // The completion menu lets Enter fall through as text after accepting (extra
                    // newline). Route Enter/Tab to the menu first and stop there when it took them.
                    .capture_action(move |action: &gpui_kit::component::input::Enter, window, cx| {
                        let handled = enter_editor.update(cx, |e, cx| e.route_overlay_action(Box::new(action.clone()), window, cx));
                        if handled {
                            cx.stop_propagation();
                        }
                    })
                    .capture_action(move |_: &gpui_kit::component::input::IndentInline, window, cx| {
                        let accept = gpui_kit::component::input::Enter { secondary: false, shift: false };
                        let handled = tab_editor.update(cx, |e, cx| e.route_overlay_action(Box::new(accept), window, cx));
                        // Else, in a completed snippet: its next stop (like VS Code), unless a gray
                        // suggestion is shown: the editor accepts it.
                        let ghost = tab_editor.read(cx).has_inline_completion();
                        if handled || (!ghost && gpui_kit::component::input::snippet::jump(&tab_editor, true, window, cx)) {
                            cx.stop_propagation();
                        }
                    })
                    .capture_action(move |_: &gpui_kit::component::input::OutdentInline, window, cx| {
                        if gpui_kit::component::input::snippet::jump(&back_editor, false, window, cx) {
                            cx.stop_propagation();
                        }
                    })
                    .capture_action(move |_: &gpui_kit::component::input::Escape, window, cx| {
                        gpui_kit::component::input::snippet::end(&esc_editor, window, cx);
                    })
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_size(cx.theme().mono_font_size)
                    // A click outside a snippet being filled in ends it (once the editor moved the cursor).
                    .capture_any_mouse_up(move |_, window, cx| {
                        let editor = click_editor.clone();
                        window.defer(cx, move |window, cx| gpui_kit::component::input::snippet::end_if_left(&editor, window, cx));
                    })
                    .capture_any_mouse_down(cx.listener(move |t, e: &MouseDownEvent, _, cx| {
                        let line = t.buffers().find(|b| b.path == path).and_then(|b| b.gutter_line_at(e.position, cx));
                        if let (Some(line), MouseButton::Left) = (line, e.button) {
                            t.toggle_breakpoint_at(path.clone(), line, cx);
                        }
                    }))
                    .child(Editor::new(&b.editor).h_full().bordered(false).context_menu(|menu, _, _| {
                        use gpui_kit::component::input::{Copy, Cut, Paste, SelectAll};
                        menu.menu(t("Add to chat"), Box::new(AddSelectionToChat))
                            .menu(t("Toggle breakpoint"), Box::new(ToggleBreakpoint))
                            .separator()
                            .menu(t("Cut"), Box::new(Cut))
                            .menu(t("Copy"), Box::new(Copy))
                            .menu(t("Paste"), Box::new(Paste))
                            .menu(t("Select all"), Box::new(SelectAll))
                    }))
                    .relative()
                    .child(super::viewer::wheel_zoom(|factor, _, _, cx| {
                        crate::settings::Settings::update(cx, |s| s.editor_font_size = ((s.editor_font_size * factor).clamp(8., 32.) * 10.).round() / 10.)
                    }));
                // Merge conflicts in this file: VS Code's actions, for the conflict at the cursor.
                let editor = if b.conflicts.is_empty() {
                    editor.into_any_element()
                } else {
                    let theme = cx.theme();
                    let count = b.conflicts.len();
                    let p = b.path.clone();
                    let act = |id: &'static str, label: &'static str, accept: super::editing::Accept, cx: &mut Context<Self>| {
                        let p = p.clone();
                        Button::new(id).xsmall().ghost().label(t(label)).on_click(cx.listener(move |t, _, w, cx| t.accept_conflict(&p, accept, w, cx)))
                    };
                    let nav = |id: &'static str, icon: IconName, forward: bool, cx: &mut Context<Self>| {
                        let p = p.clone();
                        Button::new(id).xsmall().ghost().icon(icon).on_click(cx.listener(move |t, _, w, cx| t.goto_conflict(&p, forward, w, cx)))
                    };
                    let bar = h_flex()
                        .flex_none()
                        .h(px(30.))
                        .px_2()
                        .gap_1()
                        .border_b_1()
                        .border_color(theme.border)
                        .bg(theme.warning.opacity(0.08))
                        .text_xs()
                        .child(div().child(IconName::GitMerge))
                        .child(div().mr_2().child(format!("{count} {}", t("conflict(s)"))))
                        .child(act("accept-current", "Accept current", super::editing::Accept::Current, cx))
                        .child(act("accept-incoming", "Accept incoming", super::editing::Accept::Incoming, cx))
                        .child(act("accept-both", "Accept both", super::editing::Accept::Both, cx))
                        .child(div().flex_1())
                        .child(nav("conflict-prev", IconName::ChevronUp, false, cx))
                        .child(nav("conflict-next", IconName::ChevronDown, true, cx));
                    v_flex().size_full().child(bar).child(div().flex_1().min_h_0().child(editor)).into_any_element()
                };
                // Changed by the chat, like VS Code's: the changes are marked, Keep or Undo them.
                let editor = match self.reviews.get(&b.path) {
                    None => editor,
                    Some(review) => {
                        // The block at the cursor, with its own Keep / Undo, as VS Code's per-change buttons.
                        let hunks = self.review_hunks(&b.path, cx).map(|(_, _, h)| h).unwrap_or_default();
                        let current = self.hunk_at_cursor(&b.path, &hunks, cx);
                        let path = b.path.clone();
                        let block = |id: &'static str, label: &'static str, keep: bool, cx: &mut Context<Self>| {
                            let p = path.clone();
                            Button::new(id).xsmall().ghost().label(t(label)).on_click(cx.listener(move |ws, _, w, cx| ws.settle_hunk(&p, keep, w, cx)))
                        };
                        let nav = |id: &'static str, icon: IconName, forward: bool, cx: &mut Context<Self>| {
                            let p = path.clone();
                            Button::new(id).xsmall().ghost().icon(icon).on_click(cx.listener(move |ws, _, w, cx| ws.goto_hunk(&p, forward, w, cx)))
                        };
                        let blocks = h_flex()
                            .gap_1()
                            .when(hunks.len() > 1, |el| el.child(nav("review-prev", IconName::ChevronUp, false, cx)).child(nav("review-next", IconName::ChevronDown, true, cx)))
                            .child(div().text_color(cx.theme().muted_foreground).child(format!("{} {}/{}", t("Block"), (current + 1).min(hunks.len()), hunks.len())))
                            .child(block("review-undo-block", "Undo block", false, cx))
                            .child(block("review-keep-block", "Keep block", true, cx))
                            .child(div().mx_1().h(px(14.)).w(px(1.)).bg(cx.theme().border));
                        let theme = cx.theme();
                        let (p1, p2, p3, before) = (b.path.clone(), b.path.clone(), b.path.clone(), review.before.clone());
                        let bar = h_flex()
                            .flex_none()
                            .h(px(30.))
                            .px_2()
                            .gap_1()
                            .border_b_1()
                            .border_color(theme.border)
                            .bg(theme.primary.opacity(0.08))
                            .text_xs()
                            .child(div().child(IconName::Sparkles))
                            .child(div().child(t("Changed by the chat")))
                            .child(div().text_color(theme.green).child(format!("+{}", review.added)))
                            .child(div().mr_2().text_color(theme.red).child(format!("−{}", review.removed)))
                            .child(Button::new("review-diff").xsmall().ghost().icon(IconName::GitCompare).tooltip(t("Before / after"))
                                .on_click(cx.listener(move |ws, _, w, cx| {
                                    let name = p3.strip_prefix(&ws.root).unwrap_or(&p3).display().to_string();
                                    let title = format!("{} ({})", p3.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(), t("chat changes"));
                                    let (path, before) = (p3.clone(), before.clone());
                                    ws.open_patch(title, move || Ok(crate::ai::changes::patch(&path, &name, before.as_deref())), w, cx);
                                })))
                            .child(div().flex_1())
                            .when(!hunks.is_empty(), |el| el.child(blocks))
                            .child(Button::new("review-undo").xsmall().ghost().label(t("Undo file")).on_click(cx.listener(move |t, _, w, cx| t.settle_review(&p1, false, w, cx))))
                            .child(Button::new("review-keep").xsmall().primary().label(t("Keep file")).on_click(cx.listener(move |t, _, w, cx| t.settle_review(&p2, true, w, cx))));
                        v_flex().size_full().child(bar).child(div().flex_1().min_h_0().child(editor)).into_any_element()
                    }
                };
                // AI fix in place, like VS Code's: thinking, then Keep / Undo.
                let editor = match b.ai_fix.clone() {
                    None => editor,
                    Some(fix) => {
                        let theme = cx.theme().clone();
                        let path = b.path.clone();
                        let (p1, p2) = (path.clone(), path.clone());
                        let card = v_flex()
                            .absolute()
                            .top_2()
                            .right_4()
                            .w(px(360.))
                            .p_2()
                            .gap_1p5()
                            .rounded_lg()
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.popover)
                            .shadow_lg()
                            .text_xs();
                        let card = match fix {
                            super::ai_fix::AiFix::Thinking { line, message } => card
                                .child(h_flex().gap_2().child(gpui_kit::component::spinner::Spinner::new().xsmall()).child(div().font_weight(FontWeight::MEDIUM).child(format!("{} ({} {line})", t("The AI is looking for a fix…"), t("line")))))
                                .child(div().text_color(theme.muted_foreground).line_clamp(2).child(message))
                                .child(h_flex().justify_end().child(Button::new("ai-fix-cancel").xsmall().ghost().label(t("Cancel"))
                                    .on_click(cx.listener(move |t, _, w, cx| t.resolve_ai_fix(&p1, true, w, cx))))),
                            super::ai_fix::AiFix::Proposed { original, message, .. } => card
                                .child(h_flex().gap_1().font_weight(FontWeight::MEDIUM).child(div().text_color(theme.green).child(IconName::Sparkles)).child(t("Fix applied - keep it?")))
                                .child(div().text_color(theme.muted_foreground).line_clamp(2).child(message))
                                // What was there before, struck out in red.
                                .child(
                                    v_flex().max_h(px(120.)).overflow_hidden().rounded_sm().bg(theme.danger.opacity(0.08)).p_1().font_family("Menlo")
                                        .children(original.lines().take(8).map(|l| div().whitespace_nowrap().text_color(theme.danger).line_through().child(l.to_string()))),
                                )
                                .child(
                                    h_flex()
                                        .gap_1()
                                        .justify_end()
                                        .child(Button::new("ai-fix-undo").xsmall().ghost().label(t("Undo"))
                                            .on_click(cx.listener(move |t, _, w, cx| t.resolve_ai_fix(&p1, false, w, cx))))
                                        .child(Button::new("ai-fix-keep").xsmall().primary().label(t("Keep"))
                                            .on_click(cx.listener(move |t, _, w, cx| t.resolve_ai_fix(&p2, true, w, cx)))),
                                ),
                            super::ai_fix::AiFix::Failed(e) => card
                                .child(h_flex().gap_1().text_color(theme.danger).child(IconName::CircleX).child(div().flex_1().child(e)))
                                .child(h_flex().justify_end().child(Button::new("ai-fix-close").xsmall().ghost().label(t("Close"))
                                    .on_click(cx.listener(move |t, _, w, cx| t.resolve_ai_fix(&p2, true, w, cx))))),
                        };
                        div().relative().size_full().child(editor).child(card).into_any_element()
                    }
                };
                if b.preview == Preview::Off {
                    return editor;
                }
                let preview = if super::csv_table::is_csv(&b.path) {
                    div().size_full().child(super::csv_table::view(SharedString::from(format!("csv-{pane}")), &b.path, &b.text(cx), cx))
                } else if let Some(shader) = self.shader_previews.get(&b.path) {
                    div().size_full().child(shader.clone())
                } else { div().size_full().child(
                    v_flex()
                        .id(SharedString::from(format!("md-{pane}")))
                        .size_full()
                        .overflow_y_scrollbar()
                        .px_6()
                        .py_4()
                        .child(TextView::markdown(SharedString::from(format!("md-view-{}", b.path.display())), b.text(cx))),
                ) };
                match b.preview {
                    Preview::Only => preview.into_any_element(),
                    _ => h_flex()
                        .size_full()
                        .child(div().w(relative(0.5)).h_full().child(editor))
                        .child(div().w(relative(0.5)).h_full().border_l_1().border_color(cx.theme().border).child(preview))
                        .into_any_element(),
                }
            }
            Tab::Diff { state, .. } => Diff::new(state).into_any_element(),
            Tab::Graph(g) => g.clone().into_any_element(),
            Tab::Agents(a) => a.clone().into_any_element(),
            Tab::Image { path, zoom, scroll, crisp } => {
                // Zoom 1 fits the pane; zoomed in, the frame grows from the pane's size and scrolls.
                let (zoom, view) = (*zoom, scroll.bounds().size);
                // Small images come pre-enlarged pixel by pixel, so they stay sharp.
                let source: ImageSource = crisp.clone().map_or_else(|| path.clone().into(), Into::into);
                let frame = div().flex_none().p_4().child(img(source).object_fit(ObjectFit::Contain).size_full());
                let frame = if zoom > 1. { frame.w(view.width * zoom).h(view.height * zoom) } else { frame.size_full() };
                let ws = cx.entity().downgrade();
                div()
                    .size_full()
                    .relative()
                    .bg(cx.theme().secondary.opacity(0.3))
                    .child(div().id(SharedString::from(format!("image-{}", path.display()))).size_full().flex().overflow_scroll().track_scroll(scroll).child(frame))
                    .child(super::viewer::wheel_zoom(move |factor, mouse, _, cx| {
                        let _ = ws.update(cx, |t, cx| {
                            if let Some(Tab::Image { zoom, scroll, .. }) = t.pane_mut(pane).and_then(|p| p.active.and_then(|a| p.tabs.get_mut(a))) {
                                let new = (*zoom * factor).clamp(1., 10.);
                                super::viewer::zoom_scroll(scroll, mouse, new / *zoom);
                                *zoom = new;
                            }
                            cx.notify();
                        });
                    }))
                    .scrollbar(scroll, ScrollbarAxis::Both)
                    .into_any_element()
            }
            Tab::Pdf(view) => view.clone().into_any_element(),
            Tab::Video(view) => view.clone().into_any_element(),
            Tab::Table(view) => view.clone().into_any_element(),
            Tab::Console(view) => view.clone().into_any_element(),
            Tab::Settings => super::settings_page::settings_view(cx).into_any_element(),
        }
    }

    /// Five drop zones over a pane while something is dragged: edges split, center moves here.
    fn render_drop_zones(&self, pane: usize, cx: &mut Context<Self>) -> AnyElement {
        let zone = |z: Zone, el: Div, cx: &mut Context<Self>| {
            let hint = cx.theme().primary.opacity(0.18);
            el.id(SharedString::from(format!("drop-{pane}-{z:?}")))
                .debug_selector(|| format!("drop-{pane}-{z:?}"))
                .absolute()
                .drag_over::<DraggedTab>(move |s, _, _, _| s.bg(hint))
                .drag_over::<DraggedFile>(move |s, _, _, _| s.bg(hint))
                .drag_over::<ExternalPaths>(move |s, _, _, _| s.bg(hint))
                .on_drop(cx.listener(move |t, d: &DraggedTab, w, cx| t.move_tab(d.pane, d.ix, pane, z, w, cx)))
                .on_drop(cx.listener(move |t, d: &DraggedFile, w, cx| t.open_path_in(d.0.clone(), pane, z, w, cx)))
                .on_drop(cx.listener(move |t, d: &ExternalPaths, w, cx| {
                    for p in d.paths() {
                        t.open_path_in(p.clone(), pane, z, w, cx);
                    }
                }))
        };
        div()
            .absolute()
            .inset_0()
            .child(zone(Zone::Left, div().left_0().top_0().h_full().w(relative(0.25)), cx))
            .child(zone(Zone::Right, div().right_0().top_0().h_full().w(relative(0.25)), cx))
            .child(zone(Zone::Top, div().top_0().left(relative(0.25)).w(relative(0.5)).h(relative(0.3)), cx))
            .child(zone(Zone::Bottom, div().bottom_0().left(relative(0.25)).w(relative(0.5)).h(relative(0.3)), cx))
            .child(zone(Zone::Center, div().top(relative(0.3)).left(relative(0.25)).w(relative(0.5)).h(relative(0.4)), cx))
            .into_any_element()
    }

    fn render_pane(&self, id: usize, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(pane) = self.pane(id) else { return div().into_any_element() };
        let dragging = cx.has_active_drag();
        if pane.tabs.is_empty() {
            return self.render_welcome(id, dragging, cx);
        }
        let tabs: Vec<AnyElement> = pane.tabs.iter().enumerate().map(|(ix, tab)| self.render_tab(id, ix, tab, pane.active == Some(ix), cx)).collect();
        let actions = self.render_tab_actions(id, cx);
        let body = pane.active.and_then(|a| pane.tabs.get(a)).map(|tab| self.render_tab_body(id, tab, cx));
        let overlay = dragging.then(|| self.render_drop_zones(id, cx));
        let theme = cx.theme();
        v_flex()
            .size_full()
            // Finder drags start without a repaint: repaint on movement so the drop zones show up.
            .on_drag_move::<ExternalPaths>(cx.listener(move |_, _, _, cx| if !dragging { cx.notify() }))
            .capture_any_mouse_down(cx.listener(move |t, _, _, cx| {
                if t.focused != id {
                    t.focused = id;
                    cx.notify();
                }
            }))
            .child(
                h_flex()
                    .id(("strip", id))
                    .h(px(34.))
                    .flex_shrink_0()
                    .bg(theme.tab_bar)
                    .border_b_1()
                    .border_color(theme.border)
                    .child(h_flex().id(("tabs", id)).h_full().flex_1().min_w_0().overflow_x_scroll().children(tabs)
                        .on_drop(cx.listener(move |t, d: &DraggedTab, w, cx| t.move_tab(d.pane, d.ix, id, Zone::Center, w, cx)))
                        .on_drop(cx.listener(move |t, d: &DraggedFile, w, cx| t.open_path_in(d.0.clone(), id, Zone::Center, w, cx))))
                    .child(actions)
                    .child(div().pr_1().child(Button::new(("tabs-more", id)).ghost().xsmall().icon(IconName::Ellipsis).tooltip(t("More actions")).dropdown_menu_with_anchor(Anchor::TopRight, {
                        let ws = cx.entity().downgrade();
                        move |menu, _, _| {
                            let close = |how: super::CloseTabs| {
                                let ws = ws.clone();
                                move |_: &ClickEvent, w: &mut Window, cx: &mut App| {
                                    let _ = ws.update(cx, |t, cx| t.close_tabs(id, how, w, cx));
                                }
                            };
                            menu.item(PopupMenuItem::new(t("Close saved")).on_click(close(super::CloseTabs::Saved)))
                                .item(PopupMenuItem::new(t("Close all")).on_click(close(super::CloseTabs::All)))
                                .item(PopupMenuItem::new(t("Save all and close")).on_click(close(super::CloseTabs::SaveAndCloseAll)))
                        }
                    }))),
            )
            .child(div().relative().flex_1().min_h_0().children(body).children(overlay))
            .into_any_element()
    }

    fn render_welcome(&self, pane: usize, dragging: bool, cx: &mut Context<Self>) -> AnyElement {
        let overlay = dragging.then(|| self.render_drop_zones(pane, cx));
        let recent: Vec<PathBuf> = crate::recent::list().into_iter().filter(|p| *p != self.root).take(6).collect();
        let theme = cx.theme();
        let hint = |keys: &'static str, what: &'static str| {
            h_flex().gap_4().child(div().w(px(if cfg!(target_os = "macos") { 64. } else { 120. })).font_family(theme.mono_font_family.clone()).text_color(theme.foreground).child(crate::util::keys(keys))).child(t(what))
        };
        div()
            .relative()
            .size_full()
            .overflow_hidden()
            .child(
                // Scrolls when the pane is short (a tall bottom panel), centred otherwise (`my_auto`).
                v_flex().id(("welcome", pane)).size_full().overflow_y_scroll().child(v_flex()
                    .my_auto()
                    .py_4()
                    .pl(relative(0.18))
                    .gap_2()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(
                        h_flex()
                            .gap_3()
                            .mb_4()
                            .child(img(crate::icons::logo()).h(px(52.)).w(px(36.)))
                            .child(div().text_2xl().text_color(theme.foreground).child("ForgeCode")),
                    )
                    .child(hint("⌘O", "Open folder"))
                    .child(hint("⌘L", "Ask the agent"))
                    .child(hint("ctrl-`", "Terminal"))
                    .child(hint("⌃⇧G", "Source Control"))
                    .child(hint("⌘\\", "Split right"))
                    .child(hint("⌘⇧P", "Command palette"))
                    .child(hint("⌘⇧F", "Search in project"))
                    .when(!recent.is_empty(), |el| {
                        el.child(div().mt_6().mb_1().text_xs().text_color(theme.muted_foreground).child(t("RECENT PROJECTS")))
                            .children(recent.into_iter().enumerate().map(|(ix, path)| {
                                let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                                let shown = path.display().to_string().replace(&dirs::home_dir().map(|h| h.display().to_string()).unwrap_or_default(), "~");
                                h_flex()
                                    .id(("recent", ix))
                                    .gap_3()
                                    .cursor_pointer()
                                    .child(div().text_color(theme.link).hover(|s| s.underline()).child(name))
                                    .child(div().text_xs().text_color(theme.muted_foreground).child(shown))
                                    .on_click(cx.listener(move |t, _, _, cx| t.set_root(path.clone(), cx)))
                            }))
                    })),
            )
            .children(overlay)
            .into_any_element()
    }

    fn render_problems(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        if self.diagnostics.is_empty() {
            return div().p_3().text_sm().text_color(theme.muted_foreground).child(t("No problems.")).into_any_element();
        }
        let mut rows: Vec<AnyElement> = Vec::new();
        for (path, diags) in &self.diagnostics {
            let rel = path.strip_prefix(&self.root).unwrap_or(path).display().to_string();
            let (e, w) = severity_counts(diags.iter());
            rows.push(
                h_flex().px_2().pt_1().gap_2().text_xs().text_color(theme.muted_foreground)
                    .child(IconName::File).child(rel)
                    .child(h_flex().gap_1().child(IconName::CircleX).child(e.to_string()))
                    .child(h_flex().gap_1().child(IconName::TriangleAlert).child(w.to_string()))
                    .into_any_element(),
            );
            for (i, d) in diags.iter().enumerate() {
                let (icon, color) = match d.severity {
                    Some(DiagnosticSeverity::ERROR) | None => (IconName::CircleX, theme.red),
                    Some(DiagnosticSeverity::WARNING) => (IconName::TriangleAlert, theme.yellow),
                    _ => (IconName::Info, theme.blue),
                };
                let (path, line): (PathBuf, u32) = (path.clone(), d.range.start.line + 1);
                rows.push(
                    h_flex()
                        .id(SharedString::from(format!("diag-{}-{i}", path.display())))
                        .pl_6()
                        .pr_2()
                        .gap_2()
                        .text_sm()
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.list_hover))
                        .on_click(cx.listener(move |t, _, w, cx| t.open_file(path.clone(), Some(line), w, cx)))
                        .child(div().text_color(color).child(icon))
                        .child(div().flex_1().truncate().child(d.message.lines().next().unwrap_or("").to_string()))
                        .child(div().text_xs().text_color(theme.muted_foreground).child(format!(
                            "{} [{}:{}]",
                            d.source.clone().unwrap_or_default(),
                            line,
                            d.range.start.character + 1
                        )))
                        .into_any_element(),
                );
            }
        }
        v_flex().id("problems").size_full().overflow_y_scrollbar().py_1().children(rows).into_any_element()
    }

    fn render_terminals(&self, cx: &mut Context<Self>) -> AnyElement {
        let shells = self.shells.clone();
        let ws = cx.entity().downgrade();
        let body = match self.terminals.get(self.active_terminal) {
            Some(t) => t.clone().into_any_element(),
            None => div().into_any_element(),
        };
        let rows: Vec<AnyElement> = self
            .terminals
            .iter()
            .enumerate()
            .map(|(ix, term)| {
                let term = term.read(cx);
                let label = if term.has_exited() { format!("{} ({})", term.shell.name, t("exited")) } else { term.shell.name.clone() };
                let theme = cx.theme();
                h_flex()
                    .id(("term-tab", ix))
                    .px_2()
                    .gap_1()
                    .text_sm()
                    .cursor_pointer()
                    .when(ix == self.active_terminal, |el| el.bg(theme.list_active))
                    .hover(|s| s.bg(theme.list_hover))
                    .on_click(cx.listener(move |t, _, w, cx| {
                        t.active_terminal = ix;
                        t.show_bottom_tab(BottomTab::Terminal, w, cx);
                    }))
                    .child(div().text_color(theme.muted_foreground).child(IconName::SquareTerminal))
                    .child(div().flex_1().truncate().child(label))
                    .child(icon_button(("term-kill", ix), IconName::Trash, "Kill terminal").on_click(cx.listener(move |t, _, _, cx| {
                        t.terminals.remove(ix);
                        t.active_terminal = t.active_terminal.min(t.terminals.len().saturating_sub(1));
                        cx.notify();
                    })))
                    .into_any_element()
            })
            .collect();
        let theme = cx.theme();
        h_flex()
            .size_full()
            .child(div().flex_1().h_full().min_w_0().child(body))
            .child(
                v_flex()
                    .w(px(150.))
                    .h_full()
                    .border_l_1()
                    .border_color(theme.border)
                    .child(
                        h_flex()
                            .px_2()
                            .py_1()
                            .gap_1()
                            .child(div().flex_1().text_xs().text_color(theme.muted_foreground).child(t("TERMINALS")))
                            .child(icon_button("term-new", IconName::Plus, "New terminal").dropdown_menu_with_anchor(Anchor::TopRight, move |mut menu, _, _| {
                                for shell in shells.clone() {
                                    let ws = ws.clone();
                                    menu = menu.item(PopupMenuItem::new(shell.name.clone()).on_click(move |_, w, cx| {
                                        let shell = shell.clone();
                                        let _ = ws.update(cx, |t, cx| t.new_terminal(shell, None, w, cx));
                                    }));
                                }
                                menu
                            })),
                    )
                    .children(rows),
            )
            .into_any_element()
    }

    fn render_bottom(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (errors, warnings) = severity_counts(self.diagnostics.values().flatten());
        let body: AnyElement = match self.bottom_tab {
            BottomTab::Problems => self.render_problems(cx),
            BottomTab::Output => self.output.clone().into_any_element(),
            BottomTab::Debug => self.debug.clone().into_any_element(),
            BottomTab::Terminal => self.render_terminals(cx),
        };
        let tabs = [
            (BottomTab::Problems, if errors + warnings > 0 { format!("{} {}", t("Problems"), errors + warnings) } else { t("Problems").into() }),
            (BottomTab::Output, t("Output").into()),
            (BottomTab::Terminal, t("Terminal").into()),
            (BottomTab::Debug, t("Debug").into()),
        ];
        let selected = tabs.iter().position(|(t, _)| *t == self.bottom_tab).unwrap_or(0);
        let theme = cx.theme();
        let tab_els: Vec<AnyElement> = tabs
            .into_iter()
            .enumerate()
            .map(|(ix, (which, label))| {
                let active = ix == selected;
                h_flex()
                    .id(("bottom-tab", ix))
                    .relative()
                    .h_full()
                    .px_1()
                    .text_xs()
                    .cursor_pointer()
                    .text_color(if active { theme.foreground } else { theme.muted_foreground })
                    .hover(|s| s.text_color(theme.foreground))
                    .when(active, |el| el.child(div().absolute().bottom_0().left_0().right_0().h(px(1.)).bg(theme.foreground)))
                    .on_click(cx.listener(move |t, _, w, cx| t.show_bottom_tab(which, w, cx)))
                    .child(label.to_uppercase())
                    .into_any_element()
            })
            .collect();
        v_flex()
            .size_full()
            .border_t_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .h(px(32.))
                    .px_3()
                    .gap_5()
                    .child(h_flex().h_full().flex_1().gap_5().children(tab_els))
                    .child(icon_button("bottom-close", IconName::X, "Toggle panel").on_click(cx.listener(|t, _, _, cx| { t.show_bottom = false; cx.notify(); }))),
            )
            .child(div().flex_1().min_h_0().child(body))
    }

    fn render_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let module_buttons = self.module_buttons(crate::plugins::Placement::StatusBar, cx);
        let b = self.active_buffer();
        let pos = b.map(|b| b.editor.read(cx).cursor_position());
        let (errors, warnings) = severity_counts(self.diagnostics.values().flatten());
        // The diagnostic under the cursor, like an inline error lens.
        let here = b.zip(pos).and_then(|(b, p)| {
            self.diagnostics.get(&b.path)?.iter()
                .find(|d| d.range.start.line <= p.line && p.line <= d.range.end.line)
                .map(|d| d.message.lines().next().unwrap_or("").to_string())
        });
        let git = self.git.read(cx);
        let branch = git.repo.as_ref().map(|_| (git.status.branch.clone(), git.status.ahead, git.status.behind));
        let lsp = self.server_status();
        let copilot = match cx.global::<CopilotState>().accounts.len() {
            0 => t("Copilot: sign in"),
            _ if !Settings::get(cx).copilot_inline => t("Copilot: chat only"),
            _ => "Copilot",
        };
        let theme = cx.theme();
        let seg = |id: &'static str| h_flex().id(id).h_full().px_2().gap_1().items_center().cursor_pointer().hover(|s| s.bg(theme.secondary));
        h_flex()
            .h(px(24.))
            .px_1()
            .text_xs()
            .bg(theme.status_bar)
            .border_t_1()
            .border_color(theme.status_bar_border)
            .text_color(theme.muted_foreground)
            .when_some(branch, |el, (name, ahead, behind)| {
                el.child(seg("st-branch").child(IconName::GitBranch).child(name)
                    .when(ahead + behind > 0, |el| el.child(format!("↓{behind} ↑{ahead}")))
                    .on_click(cx.listener(|t, _, w, cx| t.branch_picker(w, cx))))
            })
            .child(seg("st-problems")
                .child(div().text_color(if errors > 0 { theme.red } else { theme.muted_foreground }).child(IconName::CircleX)).child(errors.to_string())
                .child(div().text_color(if warnings > 0 { theme.yellow } else { theme.muted_foreground }).child(IconName::TriangleAlert)).child(warnings.to_string())
                .on_click(cx.listener(|t, _, w, cx| t.show_bottom_tab(BottomTab::Problems, w, cx))))
            .child(div().flex_1().min_w_0().px_2().truncate().text_color(theme.foreground.opacity(0.7)).children(here))
            .when(self.debug.read(cx).is_active(), |el| el.child(div().px_2().text_color(theme.warning).child(format!("● {}", t("Debugging")))))
            .when_some(pos, |el, p| el.child(div().px_2().child(format!("{}:{}", p.line + 1, p.character + 1))))
            .when_some(b, |el, b| el.child(div().px_2().child(b.lang.lsp_id.clone())))
            .when_some(lsp, |el, (name, ok, error)| {
                el.child(
                    h_flex()
                        .id("st-lsp")
                        .px_2()
                        .gap_1()
                        .child(div().size(px(6.)).rounded_full().bg(if ok { theme.green } else { theme.muted_foreground }))
                        .child(name)
                        .when_some(error, |el, e| el.tooltip(move |w, cx| gpui_kit::component::tooltip::Tooltip::new(e.clone()).build(w, cx))),
                )
            })
            .children(module_buttons)
            .child(seg("st-copilot").child(IconName::Github).child(copilot).on_click(cx.listener(|t, _, w, cx| t.open_settings(w, cx))))
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Nothing focused (just started, or the focused tab closed): the menu bar would gray out
        // every command, since no element would handle them. The workspace takes the focus.
        if window.focused(cx).is_none() {
            self.focus.focus(window, cx);
        }
        let layout = self.layout.clone();
        let center = self.render_layout(&layout, String::new(), window, cx);
        let bottom = self.render_bottom(cx).into_any_element();
        let title_bar = self.render_title_bar(cx).into_any_element();
        let activity = self.render_activity_bar(cx).into_any_element();
        let status = self.render_status(cx).into_any_element();
        let sidebar: AnyElement = match self.sidebar {
            Some(Sidebar::Git) => self.git.clone().into_any_element(),
            Some(Sidebar::Databases) => self.db_panel.clone().into_any_element(),
            Some(Sidebar::Conan) => self.conan_panel.clone().into_any_element(),
            Some(Sidebar::Search) => self.search.clone().into_any_element(),
            _ => self.file_tree.clone().into_any_element(),
        };
        let theme = cx.theme();
        v_flex()
            .size_full()
            .track_focus(&self.focus)
            .key_context("Workspace")
            .bg(theme.background)
            .text_color(theme.foreground)
            .text_size(theme.font_size)
            .font_family(theme.font_family.clone())
            .on_action(cx.listener(Self::save))
            .on_action(cx.listener(Self::run_module_by_index))
            .on_action(cx.listener(Self::toggle_line_comment))
            .on_action(cx.listener(Self::move_line_up))
            .on_action(cx.listener(Self::move_line_down))
            .on_action(cx.listener(Self::copy_line_up))
            .on_action(cx.listener(Self::copy_line_down))
            .on_action(cx.listener(Self::delete_line))
            .on_action(cx.listener(Self::insert_line_below))
            .on_action(cx.listener(Self::insert_line_above))
            .on_action(cx.listener(Self::select_next_occurrence))
            .on_action(cx.listener(Self::select_all_occurrences))
            .on_action(cx.listener(Self::open_folder))
            .on_action(cx.listener(|t, _: &CloseTab, w, cx| {
                let focused = t.focused;
                if let Some(ix) = t.pane(focused).and_then(|p| p.active) {
                    t.close_tab(focused, ix, w, cx);
                }
            }))
            .on_action(cx.listener(Self::run_file))
            .on_action(cx.listener(Self::start_debug))
            .on_action(cx.listener(Self::toggle_breakpoint))
            .on_action(cx.listener(Self::new_module))
            .on_action(cx.listener(Self::reload_modules))
            .on_action(cx.listener(Self::split_right))
            .on_action(cx.listener(Self::new_project))
            .on_action(cx.listener(Self::command_palette))
            .on_action(cx.listener(Self::show_search))
            .on_action(cx.listener(Self::format_document))
            .on_action(cx.listener(|t, _: &OpenProjectInstructions, w, cx| t.open_project_file("instructions", w, cx)))
            .on_action(cx.listener(|t, _: &OpenProjectSettings, w, cx| t.open_project_file("settings", w, cx)))
            .on_action(cx.listener(Self::accept_suggestion_word))
            .on_action(cx.listener(|t, _: &StopDebug, _, cx| t.debug.update(cx, |d, cx| d.stop(cx))))
            .on_action(cx.listener(|t, _: &StepOver, _, cx| t.debug.update(cx, |d, cx| d.step_over(cx))))
            .on_action(cx.listener(|t, _: &StepIn, _, cx| t.debug.update(cx, |d, cx| d.step_in(cx))))
            .on_action(cx.listener(|t, _: &StepOut, _, cx| t.debug.update(cx, |d, cx| d.step_out(cx))))
            .on_action(cx.listener(|t, _: &ToggleSettings, w, cx| t.open_settings(w, cx)))
            .on_action(cx.listener(|t, _: &ShowExplorer, _, cx| t.toggle_sidebar(Sidebar::Explorer, cx)))
            .on_action(cx.listener(|t, _: &ShowGit, _, cx| t.toggle_sidebar(Sidebar::Git, cx)))
            .on_action(cx.listener(|t, _: &ShowGitGraph, window, cx| {
                let git = t.git.clone();
                t.on_git_event(&git, &GitEvent::OpenGraph, window, cx)
            }))
            .on_action(cx.listener(|t, _: &ShowProblems, w, cx| t.show_bottom_tab(BottomTab::Problems, w, cx)))
            .on_action(cx.listener(|t, _: &ToggleLeft, _, cx| { t.sidebar = if t.sidebar.is_some() { None } else { Some(Sidebar::Explorer) }; cx.notify(); }))
            .on_action(cx.listener(|t, _: &ToggleBottom, _, cx| { t.show_bottom = !t.show_bottom; cx.notify(); }))
            .on_action(cx.listener(|t, _: &ToggleRight, _, cx| { t.show_right = !t.show_right; cx.notify(); }))
            .on_action(cx.listener(|t, _: &NewTerminal, w, cx| {
                if let Some(shell) = t.shells.first().cloned() {
                    t.new_terminal(shell, None, w, cx);
                }
            }))
            .on_action(cx.listener(|t, _: &ToggleTerminal, w, cx| {
                if t.show_bottom && t.bottom_tab == BottomTab::Terminal {
                    t.show_bottom = false;
                    cx.notify();
                } else {
                    t.show_bottom_tab(BottomTab::Terminal, w, cx);
                }
            }))
            .on_action(cx.listener(|t, _: &FocusChat, w, cx| {
                // With code selected, ⌘L links it to the chat (Cursor / Copilot behaviour).
                let has_selection = t.active_buffer().is_some_and(|b| !b.editor.read(cx).selected_range().is_empty());
                if has_selection {
                    return t.add_selection_to_chat(&AddSelectionToChat, w, cx);
                }
                t.show_right = true;
                t.chat.update(cx, |c, cx| c.focus(w, cx));
                cx.notify();
            }))
            .on_action(cx.listener(Self::add_selection_to_chat))
            .on_action(cx.listener(Self::quick_open))
            // From what is shown: in "system" mode the setting alone doesn't say (a light system
            // made the first click switch light to light).
            .on_action(|_: &ToggleTheme, _, cx| {
                let dark = cx.theme().is_dark();
                Settings::update(cx, |s| s.theme_mode = if dark { "light".into() } else { "dark".into() })
            })
            .child(title_bar)
            .child(
                // flex_1 + min_h_0: the panels take the remaining height and never push the status bar out.
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(activity)
                    .child(
                        div().flex_1().min_w_0().h_full().child(
                            h_resizable("main")
                                .child(resizable_panel().size(px(260.)).size_range(px(160.)..px(600.)).flex_none().visible(self.sidebar.is_some()).child(sidebar))
                                .child(
                                    resizable_panel().child(
                                        v_resizable("center")
                                            .child(resizable_panel().child(center))
                                            .child(resizable_panel().size(px(260.)).size_range(px(100.)..px(900.)).flex_none().visible(self.show_bottom).child(bottom)),
                                    ),
                                )
                                .child(resizable_panel().size(px(380.)).size_range(px(280.)..px(900.)).flex_none().visible(self.show_right).child(self.ai_panel(cx))),
                        ),
                    ),
            )
            .child(status)
    }
}

#[cfg(test)]
mod resize_tests {
    use gpui_kit::component::resizable::{h_resizable, resizable_panel};
    use gpui_kit::{Context, IntoElement, Modifiers, MouseButton, ParentElement as _, Render, Styled as _, InteractiveElement as _, TestAppContext, Window, div, point, px};

    struct SidebarHarness {
        right: bool,
    }

    impl Render for SidebarHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            // As in the window: an activity bar before the panels.
            gpui_kit::component::h_flex().w(px(1048.)).h(px(100.)).child(div().w(px(48.)).h_full()).child(div().flex_1().min_w_0().h_full().child(
                h_resizable("main")
                    .child(resizable_panel().size(px(200.)).size_range(px(100.)..px(600.)).flex_none().child(div().size_full().debug_selector(|| "sb-left".into())))
                    .child(resizable_panel().child(div().size_full().debug_selector(|| "sb-center".into())))
                    .child(resizable_panel().size(px(300.)).size_range(px(280.)..px(900.)).flex_none().visible(self.right).child(div().size_full().debug_selector(|| "sb-right".into()))),
            ))
        }
    }

    /// With the right panel closed, dragging the left panel's edge follows the pointer (its
    /// last width must not count against the others), and the right panel comes back as it was.
    #[gpui_kit::test]
    fn a_hidden_panel_does_not_disturb_dragging_the_others(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        // The chat closed from the start.
        let (view, cx) = cx.add_window_view(|_, _| SidebarHarness { right: false });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        // Two drags in a row (the second starts from sizes measured by the first), widening
        // then narrowing.
        for path in [[278., 308., 348.], [328., 298., 268.]] {
            let edge = cx.debug_bounds("sb-left").unwrap().right();
            cx.simulate_mouse_down(point(edge - px(2.), px(50.)), MouseButton::Left, Modifiers::default());
            // The first move starts the drag.
            cx.simulate_mouse_move(point(edge + px(1.), px(50.)), Some(MouseButton::Left), Modifiers::default());
            for x in path {
                cx.simulate_mouse_move(point(px(x), px(50.)), Some(MouseButton::Left), Modifiers::default());
                cx.update(|window, cx| window.draw(cx).clear(cx));
                assert_eq!(cx.debug_bounds("sb-left").unwrap().right(), px(x), "follows the pointer");
            }
            cx.simulate_mouse_up(point(px(path[2]), px(50.)), MouseButton::Left, Modifiers::default());
            cx.update(|window, cx| window.draw(cx).clear(cx));
        }
        view.update(cx, |v, cx| {
            v.right = true;
            cx.notify();
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert_eq!(cx.debug_bounds("sb-left").unwrap().size.width, px(220.));
        assert!(cx.debug_bounds("sb-right").unwrap().size.width >= px(280.));
    }
}
