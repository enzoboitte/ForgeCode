//! A SQL console of one connection: a code editor (SQL highlighting), ⌘↵ runs the selection or
//! the whole text, results in a grid; a statement that changes data or structure is confirmed.

use std::time::Instant;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::json;

use super::{DbHub, Rows};
use crate::i18n::t;

pub struct ConsoleTab {
    pub hub: Entity<DbHub>,
    pub conn: String,
    editor: Entity<EditorState>,
    result: Option<Rows>,
    message: Option<String>,
    error: Option<String>,
    running: bool,
}

impl ConsoleTab {
    pub fn new(hub: Entity<DbHub>, conn: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let first = hub.read(cx).schemas.get(&conn).and_then(|t| t.first().map(|t| t.name.clone()));
        let starter = first.map(|t| format!("select * from {t} limit 50;\n")).unwrap_or_else(|| "select 1;\n".into());
        let editor = cx.new(|cx| {
            let mut e = EditorState::new(window, cx).language("sql").default_value(starter);
            e.set_highlighter_factory(crate::workspace::sql::factory(), cx);
            e
        });
        Self { hub, conn, editor, result: None, message: None, error: None, running: false }
    }

    pub fn title(&self, cx: &App) -> String {
        let conn = self.hub.read(cx).conn(&self.conn).map(|c| c.name.clone()).unwrap_or_default();
        format!("SQL · {conn}")
    }

    /// The selection, else the whole text.
    fn sql(&self, cx: &App) -> String {
        let e = self.editor.read(cx);
        let selected = e.selected_value().to_string();
        if selected.trim().is_empty() { e.text().to_string() } else { selected }
    }

    fn run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let sql = self.sql(cx);
        if sql.trim().is_empty() || self.running {
            return;
        }
        self.execute(sql, false, window, cx);
    }

    fn execute(&mut self, sql: String, allow_write: bool, window: &mut Window, cx: &mut Context<Self>) {
        let task = self.hub.update(cx, |h, cx| h.call(&self.conn, "query", json!({"sql": sql, "allow_write": allow_write}), cx));
        self.running = true;
        self.error = None;
        cx.notify();
        let started = Instant::now();
        let window_handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let ms = started.elapsed().as_millis();
            let _ = cx.update_window(window_handle, |_, window, cx| {
                let _ = this.update(cx, |this, cx| {
                    this.running = false;
                    match result {
                        // A change: shown, then run once confirmed.
                        Ok(v) if v["needs_confirmation"].as_bool() == Some(true) => {
                            let view = cx.entity().downgrade();
                            let (hub, conn) = (this.hub.clone(), this.conn.clone());
                            let shown = sql.clone();
                            super::table::confirm_sql(hub, conn, shown, window, cx, move |cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.message = Some(t("Change applied").into());
                                    this.result = None;
                                    cx.notify();
                                });
                            });
                        }
                        Ok(v) if v.get("columns").is_some() => {
                            let rows = Rows::from(&v);
                            this.message = Some(format!("{} {}{} · {ms} ms", rows.rows.len(), t("rows"), if rows.truncated { " (+)" } else { "" }));
                            this.result = Some(rows);
                        }
                        Ok(v) => {
                            this.message = Some(format!("{} {} · {ms} ms", v["rowcount"].as_i64().unwrap_or(0), t("rows affected")));
                            this.result = None;
                        }
                        Err(e) => this.error = Some(e.to_string()),
                    }
                    cx.notify();
                });
            });
        })
        .detach();
    }
}

impl Render for ConsoleTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let result = self.result.clone().map(|rows| super::table::grid::<Self>("sql", &rows, None, None, None, cx, None, None, None));
        v_flex()
            .size_full()
            .bg(theme.background)
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(Button::new("sql-run").small().primary().icon(IconName::Play).label(t("Run")).tooltip("⌘↵").on_click(cx.listener(|this, _, w, cx| this.run(w, cx))))
                    .when(self.running, |el| el.child(Spinner::new().xsmall()))
                    .child(div().flex_1().text_xs().text_color(theme.muted_foreground).child(t("Runs the selection, or everything. Changes are confirmed first."))),
            )
            .child(
                div()
                    .h(px(200.))
                    .flex_none()
                    .border_b_1()
                    .border_color(theme.border)
                    // ⌘↵ / Ctrl+↵ runs.
                    .capture_key_down(cx.listener(|this, e: &KeyDownEvent, window, cx| {
                        let m = &e.keystroke.modifiers;
                        if e.keystroke.key == "enter" && (m.platform || m.control) {
                            cx.stop_propagation();
                            this.run(window, cx);
                        }
                    }))
                    .child(Editor::new(&self.editor).h_full().bordered(false)),
            )
            .when_some(self.error.clone(), |el, e| el.child(div().px_3().py_1().text_xs().text_color(theme.danger).child(e)))
            .when_some(self.message.clone(), |el, m| el.child(div().px_3().py_1().text_xs().text_color(theme.muted_foreground).child(m)))
            .child(div().flex_1().min_h_0().children(result))
    }
}
