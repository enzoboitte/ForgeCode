//! A table tab: its rows in a grid (pages of 200), a WHERE filter, sorting by a column, cells edited
//! in place (double-click), rows added, deleted (confirmed), exported to CSV, and the structure
//! changed (add, drop, rename a column) with the SQL shown before it runs. Also the shared grid
//! (with the SQL console) and the new-table dialog.

use std::collections::BTreeSet;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::{Value, json};

use super::{DbHub, Rows, TableInfo};
use crate::i18n::t;

const PAGE: i64 = 200;
const CELL_W: f32 = 170.;

/// The filter as a condition: a leading `WHERE` and a trailing `;` are accepted and dropped.
pub fn condition(filter: &str) -> String {
    let f = filter.trim().trim_end_matches(';').trim();
    match f.get(..5) {
        Some(w) if w.eq_ignore_ascii_case("where") && f[5..].starts_with(char::is_whitespace) => f[5..].trim().to_string(),
        _ => f.to_string(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn filter_accepts_a_leading_where() {
        assert_eq!(super::condition("WHERE label LIKE \"H%\""), "label LIKE \"H%\"");
        assert_eq!(super::condition(" where x = 1; "), "x = 1");
        assert_eq!(super::condition("whereabouts = 1"), "whereabouts = 1");
        assert_eq!(super::condition(""), "");
    }
}

/// A cell as text: NULL shown as such.
pub fn cell_text(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::String(s) => s.replace('\n', "⏎"),
        other => other.to_string(),
    }
}

/// The grid, shared by tables and the console. `selected`/`on_select` and `on_edit` are optional.
pub fn grid<T: 'static>(
    id: &str,
    rows: &Rows,
    selected: Option<&BTreeSet<usize>>,
    editing: Option<(usize, usize, &Entity<InputState>)>,
    sort: Option<(&str, bool)>,
    cx: &mut Context<T>,
    on_header: Option<fn(&mut T, String, &mut Context<T>)>,
    on_select: Option<fn(&mut T, usize, &mut Context<T>)>,
    on_edit: Option<fn(&mut T, usize, usize, &mut Window, &mut Context<T>)>,
) -> AnyElement {
    let theme = cx.theme().clone();
    let width = px(CELL_W * rows.columns.len().max(1) as f32 + 40.);
    // At least as wide as the columns, wider when the pane is: the columns share the rest.
    let header = h_flex()
        .min_w(width)
        .w_full()
        .h(px(26.))
        .bg(theme.secondary)
        .border_b_1()
        .border_color(theme.border)
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
        .child(div().w(px(40.)).flex_none())
        .children(rows.columns.iter().enumerate().map(|(ci, c)| {
            let arrow = sort.filter(|(s, _)| *s == c).map(|(_, asc)| if asc { " ↑" } else { " ↓" }).unwrap_or("");
            let name = c.clone();
            div()
                .id(SharedString::from(format!("{id}-h{ci}")))
                .flex_basis(px(CELL_W))
                .flex_grow(1.)
                .min_w_0()
                .flex_shrink_0()
                .px_2()
                .truncate()
                .border_r_1()
                .border_color(theme.border)
                .when(on_header.is_some(), |el| el.cursor_pointer().hover(|s| s.bg(theme.list_hover)))
                .when_some(on_header, |el, f| el.on_click(cx.listener(move |this, _, _, cx| f(this, name.clone(), cx))))
                .child(format!("{c}{arrow}"))
        }));
    let body = rows.rows.iter().enumerate().map(|(ri, row)| {
        let is_selected = selected.is_some_and(|s| s.contains(&ri));
        h_flex()
            .id(SharedString::from(format!("{id}-r{ri}")))
            .min_w(width)
            .w_full()
            .h(px(24.))
            .text_xs()
            .border_b_1()
            .border_color(theme.border.opacity(0.5))
            .when(is_selected, |el| el.bg(theme.primary.opacity(0.15)))
            .child(
                div()
                    .id(SharedString::from(format!("{id}-n{ri}")))
                    .w(px(40.))
                    .flex_none()
                    .px_1()
                    .text_color(theme.muted_foreground)
                    .when(on_select.is_some(), |el| el.cursor_pointer())
                    .when_some(on_select, |el, f| el.on_click(cx.listener(move |this, _, _, cx| f(this, ri, cx))))
                    .child((ri + 1).to_string()),
            )
            .children(row.iter().enumerate().map(|(ci, v)| {
                let cell = div().flex_basis(px(CELL_W)).flex_grow(1.).flex_shrink_0().min_w_0().px_2().border_r_1().border_color(theme.border.opacity(0.5));
                if let Some((er, ec, input)) = editing.filter(|(er, ec, _)| *er == ri && *ec == ci) {
                    let _ = (er, ec);
                    return cell.child(Input::new(input).xsmall()).into_any_element();
                }
                let null = v.is_null();
                cell.id(SharedString::from(format!("{id}-c{ri}-{ci}")))
                    .truncate()
                    .when(null, |el| el.italic().text_color(theme.muted_foreground))
                    .when_some(on_edit, |el, f| {
                        el.on_click(cx.listener(move |this, e: &ClickEvent, window, cx| {
                            if e.click_count() >= 2 {
                                f(this, ri, ci, window, cx);
                            }
                        }))
                    })
                    .child(cell_text(v))
                    .into_any_element()
            }))
    });
    div()
        .id(SharedString::from(format!("{id}-grid")))
        .size_full()
        .overflow_scroll()
        .child(v_flex().min_w(width).w_full().child(header).children(body))
        .into_any_element()
}

pub struct TableTab {
    pub hub: Entity<DbHub>,
    pub conn: String,
    pub table: String,
    data: Rows,
    offset: i64,
    filter: Entity<InputState>,
    sort: Option<(String, bool)>,
    selected: BTreeSet<usize>,
    editing: Option<(usize, usize, Entity<InputState>)>,
    loading: bool,
    error: Option<String>,
    status: Option<String>,
    _subs: Vec<Subscription>,
}

impl TableTab {
    pub fn new(hub: Entity<DbHub>, conn: String, table: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filtre : label LIKE 'H%' AND score > 10"));
        let sub = cx.subscribe_in(&filter, window, |this, _, e: &InputEvent, _, cx| {
            if matches!(e, InputEvent::PressEnter { .. }) {
                this.offset = 0;
                this.load(cx);
            }
        });
        let mut this = Self {
            hub,
            conn,
            table,
            data: Rows::default(),
            offset: 0,
            filter,
            sort: None,
            selected: BTreeSet::new(),
            editing: None,
            loading: false,
            error: None,
            status: None,
            _subs: vec![sub],
        };
        this.load(cx);
        this
    }

    pub fn title(&self, cx: &App) -> String {
        let conn = self.hub.read(cx).conn(&self.conn).map(|c| c.name.clone()).unwrap_or_default();
        format!("{} | {conn}", self.table)
    }

    fn info(&self, cx: &App) -> Option<TableInfo> {
        self.hub.read(cx).schemas.get(&self.conn)?.iter().find(|t| t.name == self.table).cloned()
    }

    fn kind(&self, cx: &App) -> String {
        self.hub.read(cx).conn(&self.conn).map(|c| c.kind.clone()).unwrap_or_default()
    }

    fn quote(kind: &str, name: &str) -> String {
        match kind {
            "mysql" | "mariadb" => format!("`{}`", name.replace('`', "``")),
            "sqlserver" => format!("[{}]", name.replace(']', "]]")),
            _ => format!("\"{}\"", name.replace('"', "\"\"")),
        }
    }

    pub fn load(&mut self, cx: &mut Context<Self>) {
        let kind = self.kind(cx);
        let order = self.sort.as_ref().map(|(c, asc)| format!("{} {}", Self::quote(&kind, c), if *asc { "asc" } else { "desc" })).unwrap_or_default();
        let params = json!({"table": self.table, "limit": PAGE, "offset": self.offset, "where": condition(&self.filter.read(cx).value()), "order": order});
        let task = self.hub.update(cx, |h, cx| h.call(&self.conn, "rows", params, cx));
        self.loading = true;
        self.editing = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.loading = false;
                match result {
                    Ok(v) => {
                        this.data = Rows::from(&v);
                        this.error = None;
                    }
                    Err(e) => this.error = Some(e.to_string()),
                }
                this.selected.clear();
                cx.notify();
            });
        })
        .detach();
        // Columns (primary key) for editing.
        if self.info(cx).is_none() {
            let conn = self.conn.clone();
            self.hub.update(cx, |h, cx| h.refresh_schema(&conn, cx));
        }
    }

    /// What identifies row `ri`: its primary key, else every value it had.
    fn keys(&self, ri: usize, cx: &App) -> serde_json::Map<String, Value> {
        let row = &self.data.rows[ri];
        let pks: Vec<String> = self.info(cx).map(|i| i.columns.iter().filter(|c| c.pk).map(|c| c.name.clone()).collect()).unwrap_or_default();
        self.data
            .columns
            .iter()
            .zip(row)
            .filter(|(c, _)| pks.is_empty() || pks.contains(c))
            .map(|(c, v)| (c.clone(), v.clone()))
            .collect()
    }

    fn run_write(&mut self, method: &'static str, params: Value, done: String, cx: &mut Context<Self>) {
        let task = self.hub.update(cx, |h, cx| h.call(&self.conn, method, params, cx));
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(v) => this.status = Some(format!("{done} ({} {})", v["rowcount"].as_i64().unwrap_or(0), t("rows"))),
                    Err(e) => this.error = Some(e.to_string()),
                }
                this.load(cx);
            });
        })
        .detach();
    }

    fn sort_by(&mut self, column: String, cx: &mut Context<Self>) {
        self.sort = match &self.sort {
            Some((c, true)) if *c == column => Some((column, false)),
            Some((c, false)) if *c == column => None,
            _ => Some((column, true)),
        };
        self.load(cx);
    }

    fn toggle_row(&mut self, ri: usize, cx: &mut Context<Self>) {
        if !self.selected.remove(&ri) {
            self.selected.insert(ri);
        }
        cx.notify();
    }

    fn start_edit(&mut self, ri: usize, ci: usize, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.data.rows.get(ri).and_then(|r| r.get(ci)).map(|v| if v.is_null() { String::new() } else { cell_text(v).replace('⏎', "\n") }).unwrap_or_default();
        let input = cx.new(|cx| InputState::new(window, cx).default_value(value));
        let sub = cx.subscribe_in(&input, window, move |this, input, e: &InputEvent, _, cx| match e {
            InputEvent::PressEnter { .. } => {
                let text = input.read(cx).value().to_string();
                this.commit_edit(ri, ci, text, cx);
            }
            InputEvent::Blur => {
                this.editing = None;
                cx.notify();
            }
            _ => {}
        });
        self._subs.push(sub);
        let focus = input.read(cx).focus_handle(cx);
        self.editing = Some((ri, ci, input));
        window.defer(cx, move |window, cx| focus.focus(window, cx));
        cx.notify();
    }

    fn commit_edit(&mut self, ri: usize, ci: usize, text: String, cx: &mut Context<Self>) {
        self.editing = None;
        let column = self.data.columns[ci].clone();
        // An empty cell becomes NULL when the column allows it.
        let nullable = self.info(cx).and_then(|i| i.columns.iter().find(|c| c.name == column).map(|c| c.nullable)).unwrap_or(false);
        let value = if text.is_empty() && nullable { Value::Null } else { Value::String(text) };
        let params = json!({"table": self.table, "keys": self.keys(ri, cx), "column": column, "value": value});
        self.run_write("update_cell", params, t("Cell updated").into(), cx);
    }

    fn delete_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rows: Vec<Value> = self.selected.iter().map(|ri| Value::Object(self.keys(*ri, cx))).collect();
        if rows.is_empty() {
            return;
        }
        let view = cx.entity().downgrade();
        let table = self.table.clone();
        crate::workspace::dialogs::confirm(
            t("Delete rows"),
            format!("{} {} {table} ?", rows.len(), t("row(s) will be deleted from")),
            "Delete",
            window,
            cx,
            move |_, cx| {
                let rows = rows.clone();
                let table = table.clone();
                let _ = view.update(cx, |this, cx| this.run_write("delete_rows", json!({"table": table, "rows": rows}), t("Rows deleted").into(), cx));
            },
        );
    }

    fn add_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(info) = self.info(cx) else { return };
        let fields: Vec<(String, String, Entity<InputState>)> = info
            .columns
            .iter()
            .map(|c| {
                let placeholder = match &c.default {
                    Some(d) if !d.is_null() => format!("{} ({} {})", c.ty, t("default"), cell_text(d)),
                    _ if c.nullable => format!("{} | NULL", c.ty),
                    _ => c.ty.clone(),
                };
                (c.name.clone(), c.ty.clone(), cx.new(|cx| InputState::new(window, cx).placeholder(placeholder)))
            })
            .collect();
        let view = cx.entity().downgrade();
        let table = self.table.clone();
        window.open_dialog(cx, move |dialog, _, cx| {
            let theme = cx.theme().clone();
            let fields2 = fields.clone();
            let (view, table) = (view.clone(), table.clone());
            dialog.title(format!("{} | {}", t("New row"), table)).w(px(480.)).child(
                v_flex()
                    .gap_2()
                    .child(div().text_xs().text_color(theme.muted_foreground).child(t("Leave a field empty to use its default value.")))
                    .children(fields.iter().map(|(name, _, input)| {
                        h_flex().gap_2().child(div().w(px(140.)).text_sm().truncate().child(name.clone())).child(div().flex_1().child(Input::new(input).small()))
                    }))
                    .child(
                        h_flex()
                            .justify_end()
                            .gap_2()
                            .child(Button::new("row-cancel").small().ghost().label(t("Cancel")).on_click(|_, w, cx| w.close_dialog(cx)))
                            .child(Button::new("row-add").small().primary().label(t("Add")).on_click(move |_, window, cx| {
                                let values: serde_json::Map<String, Value> = fields2
                                    .iter()
                                    .filter_map(|(n, _, i)| {
                                        let v = i.read(cx).value().to_string();
                                        (!v.is_empty()).then(|| (n.clone(), Value::String(v)))
                                    })
                                    .collect();
                                let table = table.clone();
                                let _ = view.update(cx, |this, cx| this.run_write("insert_row", json!({"table": table, "values": values}), t("Row added").into(), cx));
                                window.close_dialog(cx);
                            })),
                    ),
            )
        });
    }

    fn export(&mut self, cx: &mut Context<Self>) {
        let dir = self.hub.read(cx).root.clone();
        let name = format!("{}.csv", self.table.replace('.', "_"));
        let target = cx.prompt_for_new_path(&dir, Some(&name));
        let kind = self.kind(cx);
        let mut sql = format!("select * from {}", self.table.split('.').map(|p| Self::quote(&kind, p)).collect::<Vec<_>>().join("."));
        let filter = condition(&self.filter.read(cx).value());
        if !filter.trim().is_empty() {
            sql.push_str(&format!(" where {filter}"));
        }
        let hub = self.hub.clone();
        let conn = self.conn.clone();
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(path))) = target.await else { return };
            let task = cx.update(|cx| hub.update(cx, |h, cx| h.call(&conn, "export_csv", json!({"sql": sql, "path": path.display().to_string()}), cx)));
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(v) => this.status = Some(format!("{} {} → {}", v["rows"].as_i64().unwrap_or(0), t("rows exported"), path.display())),
                    Err(e) => this.error = Some(e.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Add, drop or rename a column: the SQL is generated, shown, then run on confirmation.
    fn structure(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let kind = self.kind(cx);
        let name = cx.new(|cx| InputState::new(window, cx).placeholder(t("column")));
        let ty = cx.new(|cx| InputState::new(window, cx).placeholder("TEXT, INTEGER, VARCHAR(255)…"));
        let new_name = cx.new(|cx| InputState::new(window, cx).placeholder(t("new name (rename)")));
        let hub = self.hub.clone();
        let conn = self.conn.clone();
        let table = self.table.clone();
        let view = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let theme = cx.theme().clone();
            let action = |label: &'static str, act: &'static str| {
                let (hub, conn, table, kind, name, ty, new_name, view) = (hub.clone(), conn.clone(), table.clone(), kind.clone(), name.clone(), ty.clone(), new_name.clone(), view.clone());
                Button::new(SharedString::from(format!("col-{act}"))).small().when(act == "add", |b| b.primary()).when(act != "add", |b| b.ghost()).label(t(label)).on_click(move |_, window, cx| {
                    let column = json!({
                        "name": name.read(cx).value().trim(), "type": ty.read(cx).value().trim(),
                        "new_name": new_name.read(cx).value().trim(), "nullable": true,
                    });
                    if column["name"].as_str().unwrap_or("").is_empty() {
                        return;
                    }
                    window.close_dialog(cx);
                    let params = json!({"kind": kind, "table": table, "action": act, "column": column});
                    let task = hub.update(cx, |h, cx| h.call(&conn, "alter_sql", params, cx));
                    let (hub, conn, view) = (hub.clone(), conn.clone(), view.clone());
                    window
                        .spawn(cx, async move |cx| {
                            let Ok(v) = task.await else { return };
                            let sql = v["sql"].as_str().unwrap_or("").to_string();
                            let _ = cx.update(|window, cx| confirm_sql(hub, conn, sql, window, cx, move |cx| {
                                let _ = view.update(cx, |this, cx| this.load(cx));
                            }));
                        })
                        .detach();
                })
            };
            dialog.title(format!("{} | {}", t("Structure"), table)).w(px(460.)).child(
                v_flex()
                    .gap_2()
                    .child(div().text_xs().text_color(theme.muted_foreground).child(t("The SQL is shown before it runs.")))
                    .child(Input::new(&name).small())
                    .child(Input::new(&ty).small())
                    .child(Input::new(&new_name).small())
                    .child(h_flex().gap_2().justify_end().child(action("Drop column", "drop")).child(action("Rename column", "rename")).child(action("Add column", "add"))),
            )
        });
    }
}

/// Show `sql` and run it on confirmation (a statement that changes data or structure).
pub fn confirm_sql(hub: Entity<DbHub>, conn: String, sql: String, window: &mut Window, cx: &mut App, done: impl Fn(&mut App) + 'static) {
    let done = std::rc::Rc::new(done);
    let shown = sql.clone();
    window.open_dialog(cx, move |dialog, _, cx| {
        let theme = cx.theme().clone();
        let (hub, conn, sql, done) = (hub.clone(), conn.clone(), sql.clone(), done.clone());
        dialog.title(t("Run this change?")).w(px(560.)).child(
            v_flex()
                .gap_3()
                .child(div().p_2().rounded_md().bg(theme.secondary).font_family("Menlo").text_xs().whitespace_normal().child(shown.clone()))
                .child(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(Button::new("sql-cancel").small().ghost().label(t("Cancel")).on_click(|_, w, cx| w.close_dialog(cx)))
                        .child(Button::new("sql-run").small().primary().label(t("Run")).on_click(move |_, window, cx| {
                            window.close_dialog(cx);
                            let task = hub.update(cx, |h, cx| h.call(&conn, "query", json!({"sql": sql, "allow_write": true}), cx));
                            let (hub, conn, done) = (hub.clone(), conn.clone(), done.clone());
                            window
                                .spawn(cx, async move |cx| {
                                    let result = task.await;
                                    let _ = cx.update(|window, cx| {
                                        match result {
                                            Ok(_) => {
                                                hub.update(cx, |h, cx| h.refresh_schema(&conn, cx));
                                                done(cx);
                                            }
                                            Err(e) => window.push_notification(gpui_kit::component::notification::Notification::error(e.to_string()), cx),
                                        }
                                    });
                                })
                                .detach();
                        })),
                ),
        )
    });
}

/// New table: columns (name, type, primary key, nullable), the CREATE TABLE previewed then run.
pub fn create_table_dialog(hub: Entity<DbHub>, conn: String, window: &mut Window, cx: &mut App) {
    let kind = hub.read(cx).conn(&conn).map(|c| c.kind.clone()).unwrap_or_default();
    let name = cx.new(|cx| InputState::new(window, cx).placeholder(t("table name")));
    let id_type = match kind.as_str() {
        "postgresql" => "SERIAL",
        "mysql" | "mariadb" => "INT AUTO_INCREMENT",
        "sqlserver" => "INT IDENTITY(1,1)",
        "oracle" => "NUMBER GENERATED BY DEFAULT AS IDENTITY",
        _ => "INTEGER",
    };
    type ColRow = (Entity<InputState>, Entity<InputState>, Entity<bool>, Entity<bool>);
    let new_col = |n: &str, ty: &str, pk: bool, window: &mut Window, cx: &mut App| -> ColRow {
        let (n, ty) = (n.to_string(), ty.to_string());
        (
            cx.new(|cx| InputState::new(window, cx).default_value(n).placeholder(t("column"))),
            cx.new(|cx| InputState::new(window, cx).default_value(ty).placeholder("TEXT")),
            cx.new(|_| pk),
            cx.new(|_| !pk),
        )
    };
    let first = new_col("id", id_type, true, window, cx);
    let second = new_col("", "TEXT", false, window, cx);
    let cols: Entity<Vec<ColRow>> = cx.new(|_| vec![first, second]);
    window.open_dialog(cx, move |dialog, window, cx| {
        let theme = cx.theme().clone();
        let rows = cols.read(cx).clone();
        let add_col = {
            let cols = cols.clone();
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                let c = (
                    cx.new(|cx| InputState::new(window, cx).placeholder(t("column"))),
                    cx.new(|cx| InputState::new(window, cx).placeholder("TEXT")),
                    cx.new(|_| false),
                    cx.new(|_| true),
                );
                cols.update(cx, |v, _| v.push(c));
                window.refresh();
            }
        };
        let create = {
            let (hub, conn, kind, name, cols) = (hub.clone(), conn.clone(), kind.clone(), name.clone(), cols.clone());
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                let table = name.read(cx).value().trim().to_string();
                if table.is_empty() {
                    return;
                }
                let columns: Vec<Value> = cols
                    .read(cx)
                    .iter()
                    .filter(|(n, ..)| !n.read(cx).value().trim().is_empty())
                    .map(|(n, ty, pk, nullable)| json!({"name": n.read(cx).value().trim(), "type": ty.read(cx).value().trim(), "pk": *pk.read(cx), "nullable": *nullable.read(cx)}))
                    .collect();
                window.close_dialog(cx);
                let task = hub.update(cx, |h, cx| h.call(&conn, "create_table_sql", json!({"kind": kind, "table": table, "columns": columns}), cx));
                let (hub, conn) = (hub.clone(), conn.clone());
                window
                    .spawn(cx, async move |cx| {
                        let Ok(v) = task.await else { return };
                        let sql = v["sql"].as_str().unwrap_or("").to_string();
                        let _ = cx.update(|window, cx| confirm_sql(hub, conn, sql, window, cx, |_| {}));
                    })
                    .detach();
            }
        };
        let _ = window;
        dialog.title(t("New table")).w(px(560.)).child(
            v_flex()
                .gap_2()
                .child(Input::new(&name).small())
                .child(
                    h_flex().gap_2().text_xs().text_color(theme.muted_foreground)
                        .child(div().flex_1().child(t("Column"))).child(div().w(px(170.)).child(t("Type"))).child(div().w(px(40.)).child("PK")).child(div().w(px(50.)).child("NULL")),
                )
                .children(rows.iter().enumerate().map(|(i, (n, ty, pk, nullable))| {
                    let (pk, nullable) = (pk.clone(), nullable.clone());
                    let (pk_on, null_on) = (*pk.read(cx), *nullable.read(cx));
                    h_flex()
                        .gap_2()
                        .child(div().flex_1().child(Input::new(n).small()))
                        .child(div().w(px(170.)).child(Input::new(ty).small()))
                        .child(div().w(px(40.)).child(Checkbox::new(("pk", i)).checked(pk_on).on_click(move |on, w, cx| {
                            pk.update(cx, |v, _| *v = *on);
                            w.refresh();
                        })))
                        .child(div().w(px(50.)).child(Checkbox::new(("null", i)).checked(null_on).on_click(move |on, w, cx| {
                            nullable.update(cx, |v, _| *v = *on);
                            w.refresh();
                        })))
                }))
                .child(Button::new("col-more").xsmall().ghost().icon(IconName::Plus).label(t("Column")).on_click(add_col))
                .child(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(Button::new("tbl-cancel").small().ghost().label(t("Cancel")).on_click(|_, w, cx| w.close_dialog(cx)))
                        .child(Button::new("tbl-create").small().primary().label(t("Preview the SQL")).on_click(create)),
                ),
        )
    });
}

impl Render for TableTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let readonly = self.hub.read(cx).conn(&self.conn).is_some_and(|c| c.readonly);
        let total = self.data.total.unwrap_or(0);
        let (from, to) = (if total == 0 { 0 } else { self.offset + 1 }, (self.offset + self.data.rows.len() as i64));
        let sort = self.sort.clone();
        let editing = self.editing.as_ref().map(|(r, c, i)| (*r, *c, i));
        let grid = grid(
            "tbl",
            &self.data,
            Some(&self.selected),
            editing,
            sort.as_ref().map(|(c, a)| (c.as_str(), *a)),
            cx,
            Some(TableTab::sort_by),
            Some(TableTab::toggle_row),
            (!readonly).then_some(TableTab::start_edit as fn(&mut TableTab, usize, usize, &mut Window, &mut Context<TableTab>)),
        );
        let has_selection = !self.selected.is_empty();
        let prev_ok = self.offset > 0;
        let next_ok = self.offset + PAGE < total;
        v_flex()
            .size_full()
            .bg(theme.background)
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_1()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(div().flex_1().min_w_0().child(Input::new(&self.filter).small().prefix(IconName::Search)))
                    .child(Button::new("tbl-refresh").small().ghost().icon(IconName::RefreshCw).tooltip(t("Refresh")).on_click(cx.listener(|this, _, _, cx| this.load(cx))))
                    .when(!readonly, |el| {
                        el.child(Button::new("tbl-add").small().ghost().icon(IconName::Plus).tooltip(t("New row")).on_click(cx.listener(|this, _, w, cx| this.add_row(w, cx))))
                            .child(Button::new("tbl-del").small().ghost().icon(IconName::Trash).tooltip(t("Delete selected rows")).disabled(!has_selection).on_click(cx.listener(|this, _, w, cx| this.delete_selected(w, cx))))
                            .child(Button::new("tbl-struct").small().ghost().icon(IconName::Columns3).tooltip(t("Structure")).on_click(cx.listener(|this, _, w, cx| this.structure(w, cx))))
                    })
                    .child(Button::new("tbl-csv").small().ghost().icon(IconName::Download).tooltip(t("Export to CSV")).on_click(cx.listener(|this, _, _, cx| this.export(cx)))),
            )
            .when_some(self.error.clone(), |el, e| el.child(div().px_3().py_1().text_xs().text_color(theme.danger).child(e)))
            .child(div().flex_1().min_h_0().child(grid))
            .child(
                h_flex()
                    .px_2()
                    .h(px(28.))
                    .gap_2()
                    .border_t_1()
                    .border_color(theme.border)
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .when(self.loading, |el| el.child(Spinner::new().xsmall()))
                    .child(format!("{from}–{to} / {total}"))
                    .when(readonly, |el| el.child(h_flex().gap_1().child(IconName::Lock).child(t("read-only"))))
                    .when_some(self.status.clone(), |el, s| el.child(div().text_color(theme.green).truncate().child(s)))
                    .child(div().flex_1())
                    .when(!readonly, |el| el.child(t("Double-click a cell to edit it; click a row number to select it.")))
                    .child(Button::new("tbl-prev").xsmall().ghost().icon(IconName::ChevronLeft).disabled(!prev_ok).on_click(cx.listener(|this, _, _, cx| {
                        this.offset = (this.offset - PAGE).max(0);
                        this.load(cx);
                    })))
                    .child(Button::new("tbl-next").xsmall().ghost().icon(IconName::ChevronRight).disabled(!next_ok).on_click(cx.listener(|this, _, _, cx| {
                        this.offset += PAGE;
                        this.load(cx);
                    }))),
            )
    }
}
