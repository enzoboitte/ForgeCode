//! The databases sidebar: connections (saved ones, the project's SQLite files), their tables and
//! columns; actions open tabs (table, SQL console) or dialogs (connection, new table).

use std::collections::HashSet;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::json;

use super::{Conn, DbHub, HubEvent, KINDS};
use crate::i18n::t;

pub struct DbPanel {
    hub: Entity<DbHub>,
    /// Connections and tables unfolded.
    expanded: HashSet<String>,
    _sub: Subscription,
}

impl DbPanel {
    pub fn new(hub: Entity<DbHub>, cx: &mut Context<Self>) -> Self {
        let _sub = cx.observe(&hub, |_, _, cx| cx.notify());
        Self { hub, expanded: HashSet::new(), _sub }
    }

    fn toggle(&mut self, key: String, conn: Option<String>, cx: &mut Context<Self>) {
        if !self.expanded.remove(&key) {
            self.expanded.insert(key);
            if let Some(id) = conn {
                if !self.hub.read(cx).schemas.contains_key(&id) {
                    self.hub.update(cx, |h, cx| h.refresh_schema(&id, cx));
                }
            }
        }
        cx.notify();
    }
}

/// The add / edit connection dialog.
pub fn connection_dialog(hub: Entity<DbHub>, existing: Option<Conn>, window: &mut Window, cx: &mut App) {
    let conn = existing.clone().unwrap_or(Conn { kind: "postgresql".into(), host: "localhost".into(), port: "5432".into(), ..Default::default() });
    let field = |value: &str, placeholder: &str, window: &mut Window, cx: &mut App| {
        let (value, placeholder) = (value.to_string(), placeholder.to_string());
        cx.new(|cx| InputState::new(window, cx).default_value(value).placeholder(placeholder))
    };
    let name = field(&conn.name, "Ma base", window, cx);
    let host = field(&conn.host, "localhost", window, cx);
    let port = field(&conn.port, "5432", window, cx);
    let user = field(&conn.user, "postgres", window, cx);
    let database = field(&conn.database, "app", window, cx);
    let path = field(&conn.path, "/chemin/vers/base.db", window, cx);
    let password = cx.new(|cx| InputState::new(window, cx).masked(true).placeholder(if existing.is_some() { t("(unchanged)") } else { "" }));
    let kind = cx.new(|_| conn.kind.clone());
    let readonly = cx.new(|_| conn.readonly);
    let status: Entity<Option<Result<String, String>>> = cx.new(|_| None);
    let build = {
        let (kind, readonly, name, host, port, user, database, path) = (kind.clone(), readonly.clone(), name.clone(), host.clone(), port.clone(), user.clone(), database.clone(), path.clone());
        let id = conn.id.clone();
        move |cx: &App| {
            let v = |e: &Entity<InputState>| e.read(cx).value().trim().to_string();
            let kind = kind.read(cx).clone();
            let mut name = v(&name);
            if name.is_empty() {
                name = if kind == "sqlite" { v(&path).rsplit('/').next().unwrap_or("SQLite").to_string() } else { format!("{} {}", v(&database), v(&host)) };
            }
            Conn {
                id: if id.is_empty() || id.starts_with("file:") { format!("c{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)) } else { id.clone() },
                name,
                kind,
                path: v(&path),
                host: v(&host),
                port: v(&port),
                user: v(&user),
                database: v(&database),
                readonly: *readonly.read(cx),
                detected: false,
            }
        }
    };
    let title = if existing.is_some() { t("Edit connection") } else { t("New connection") };
    window.open_dialog(cx, move |dialog, window, cx| {
        let theme = cx.theme().clone();
        let current = kind.read(cx).clone();
        let sqlite = current == "sqlite";
        let row = |label: &'static str, input: &Entity<InputState>| v_flex().gap_1().child(div().text_xs().text_color(theme.muted_foreground).child(t(label))).child(Input::new(input).small());
        let kinds = h_flex().flex_wrap().gap_1().children(KINDS.iter().map(|(k, n)| {
            let (kind, port) = (kind.clone(), port.clone());
            Button::new(SharedString::from(format!("kind-{k}"))).xsmall().label(*n).when(current == *k, |b| b.primary()).when(current != *k, |b| b.ghost()).on_click(move |_, window, cx| {
                kind.update(cx, |v, _| *v = k.to_string());
                let default = match *k {
                    "postgresql" => "5432",
                    "mysql" | "mariadb" => "3306",
                    "sqlserver" => "1433",
                    "oracle" => "1521",
                    _ => "",
                };
                port.update(cx, |p, cx| p.set_value(default, window, cx));
                window.refresh();
            })
        }));
        let test = {
            let (hub, build, status, password) = (hub.clone(), build.clone(), status.clone(), password.clone());
            move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                let mut conn = build(cx);
                conn.id = format!("test-{}", conn.id);
                let pw = password.read(cx).value().to_string();
                status.update(cx, |s, _| *s = Some(Ok(t("Connecting…").into())));
                let mut config = conn.config(&hub.read(cx).root);
                if !pw.is_empty() {
                    config["password"] = json!(pw);
                }
                let task = hub.update(cx, |h, cx| {
                    let engine = h.engine(conn.kind != "sqlite", cx);
                    cx.background_spawn(async move {
                        let engine = engine.await?;
                        let v = engine.call("open", json!({"id": conn.id, "config": config})).await?;
                        let _ = engine.call("close", json!({"id": conn.id})).await;
                        anyhow::Ok(v["version"].as_str().unwrap_or("OK").to_string())
                    })
                });
                let status = status.clone();
                cx.spawn(async move |cx| {
                    let result = task.await.map_err(|e| e.to_string());
                    let _ = cx.update(|cx| status.update(cx, |s, _| *s = Some(result)));
                    let _ = cx.refresh();
                })
                .detach();
            }
        };
        let save = {
            let (hub, build, password) = (hub.clone(), build.clone(), password.clone());
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                let conn = build(cx);
                let pw = password.read(cx).value().to_string();
                hub.update(cx, |h, cx| h.add_or_update(conn, (!pw.is_empty()).then_some(pw), cx));
                window.close_dialog(cx);
            }
        };
        let browse = {
            let path = path.clone();
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                let picked = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: false, prompt: None });
                let path = path.clone();
                window
                    .spawn(cx, async move |cx| {
                        if let Ok(Ok(Some(paths))) = picked.await {
                            if let Some(p) = paths.first() {
                                let p = p.display().to_string();
                                let _ = cx.update(|window, cx| path.update(cx, |i, cx| i.set_value(p, window, cx)));
                            }
                        }
                    })
                    .detach();
            }
        };
        let status_view = status.read(cx).clone().map(|s| match s {
            Ok(m) => h_flex().gap_1().text_xs().text_color(theme.green).child(IconName::CircleCheck).child(m),
            Err(e) => h_flex().gap_1().text_xs().text_color(theme.danger).child(IconName::CircleX).child(div().flex_1().child(e)),
        });
        let _ = window;
        dialog.title(title).w(px(480.)).child(
            v_flex()
                .gap_2()
                .child(kinds)
                .child(row("Name", &name))
                .when(sqlite, |el| el.child(h_flex().gap_1().items_end().child(div().flex_1().child(row("File", &path))).child(Button::new("db-browse").small().ghost().icon(IconName::FolderOpen).on_click(browse.clone()))))
                .when(!sqlite, |el| {
                    el.child(h_flex().gap_2().child(div().flex_1().child(row("Host", &host))).child(div().w(px(90.)).child(row("Port", &port))))
                        .child(h_flex().gap_2().child(div().flex_1().child(row("User", &user))).child(div().flex_1().child(row("Password", &password))))
                        .child(row("Database", &database))
                })
                .child({
                    let readonly = readonly.clone();
                    Checkbox::new("db-readonly").label(t("Read-only (no changes possible)")).checked(*readonly.read(cx)).on_click(move |on, window, cx| {
                        readonly.update(cx, |r, _| *r = *on);
                        window.refresh();
                    })
                })
                .children(status_view)
                .child(
                    h_flex()
                        .gap_2()
                        .child(Button::new("db-test").small().ghost().label(t("Test")).on_click(test.clone()))
                        .child(div().flex_1())
                        .child(Button::new("db-cancel").small().ghost().label(t("Cancel")).on_click(|_, w, cx| w.close_dialog(cx)))
                        .child(Button::new("db-save").small().primary().label(t("Save")).on_click(save.clone())),
                ),
        )
    });
}

impl Render for DbPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let hub = self.hub.read(cx);
        let busy = hub.busy.clone();
        let general_error = hub.errors.get("").cloned();
        let conns = hub.conns.clone();
        let mut rows: Vec<AnyElement> = vec![];
        for conn in conns {
            let open = self.expanded.contains(&conn.id);
            let id = conn.id.clone();
            let (i1, i2, i3, i4, i5) = (id.clone(), id.clone(), id.clone(), id.clone(), id.clone());
            let hub_entity = self.hub.clone();
            let conn_for_edit = conn.clone();
            let detected = conn.detected;
            rows.push(
                h_flex()
                    .id(SharedString::from(format!("conn-{}", conn.id)))
                    .group(SharedString::from(format!("conn-{}", conn.id)))
                    .h(px(26.))
                    .px_2()
                    .gap_1()
                    .text_sm()
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.list_hover))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle(i1.clone(), Some(i1.clone()), cx)))
                    .child(div().text_color(theme.muted_foreground).child(if open { IconName::ChevronDown } else { IconName::ChevronRight }))
                    .child(div().text_color(theme.primary).child(IconName::Database))
                    .child(div().flex_1().min_w_0().truncate().child(conn.name.clone()))
                    .tooltip({
                        let sub = conn.subtitle();
                        move |w, cx| gpui_kit::component::tooltip::Tooltip::new(sub.clone()).build(w, cx)
                    })
                    .when(conn.readonly, |el| el.child(div().text_xs().text_color(theme.muted_foreground).child(IconName::Lock)))
                    .child(div().text_xs().text_color(theme.muted_foreground).child(conn.kind_name()))
                    .child(
                        Button::new(SharedString::from(format!("conn-menu-{}", conn.id))).ghost().xsmall().icon(IconName::Ellipsis).dropdown_menu(move |menu, _, _| {
                            let (h1, h2, h3, h4) = (hub_entity.clone(), hub_entity.clone(), hub_entity.clone(), hub_entity.clone());
                            let (c1, c2, c3) = (i2.clone(), i3.clone(), i4.clone());
                            let edit = conn_for_edit.clone();
                            let rm = i5.clone();
                            let mut menu = menu
                                .item(PopupMenuItem::new(t("SQL console")).icon(IconName::SquareTerminal).on_click(move |_, _, cx| h1.update(cx, |_, cx| cx.emit(HubEvent::OpenConsole(c1.clone())))))
                                .item(PopupMenuItem::new(t("New table…")).icon(IconName::Plus).on_click(move |_, window, cx| super::table::create_table_dialog(h2.clone(), c2.clone(), window, cx)))
                                .item(PopupMenuItem::new(t("Refresh")).icon(IconName::RefreshCw).on_click(move |_, _, cx| h3.update(cx, |h, cx| h.refresh_schema(&c3, cx))))
                                .separator()
                                .item(PopupMenuItem::new(if detected { t("Save as connection…") } else { t("Edit…") }).icon(IconName::Settings).on_click({
                                    let h = h4.clone();
                                    move |_, window, cx| connection_dialog(h.clone(), Some(edit.clone()), window, cx)
                                }));
                            if !detected {
                                menu = menu.item(PopupMenuItem::new(t("Remove connection")).icon(IconName::Trash).on_click(move |_, _, cx| h4.update(cx, |h, cx| h.remove(&rm, cx))));
                            }
                            menu
                        }),
                    )
                    .into_any_element(),
            );
            if !open {
                continue;
            }
            let hub = self.hub.read(cx);
            if let Some(e) = hub.errors.get(&conn.id) {
                rows.push(div().pl_6().pr_2().py_1().text_xs().text_color(theme.danger).child(e.clone()).into_any_element());
                continue;
            }
            match hub.schemas.get(&conn.id).cloned() {
                None => rows.push(h_flex().pl_6().gap_2().text_xs().text_color(theme.muted_foreground).child(Spinner::new().xsmall()).child(t("Connecting…")).into_any_element()),
                Some(tables) if tables.is_empty() => rows.push(div().pl_6().text_xs().text_color(theme.muted_foreground).child(t("No table.")).into_any_element()),
                Some(tables) => {
                    for table in tables {
                        let key = format!("{}::{}", conn.id, table.name);
                        let unfolded = self.expanded.contains(&key);
                        let (cid, tname, k) = (conn.id.clone(), table.name.clone(), key.clone());
                        let hub_entity = self.hub.clone();
                        rows.push(
                            h_flex()
                                .id(SharedString::from(format!("tbl-{key}")))
                                .h(px(24.))
                                .pl_6()
                                .pr_2()
                                .gap_1()
                                .text_sm()
                                .cursor_pointer()
                                .hover(|s| s.bg(theme.list_hover))
                                .on_click(move |_, _, cx| hub_entity.update(cx, |_, cx| cx.emit(HubEvent::OpenTable(cid.clone(), tname.clone()))))
                                .child(
                                    div()
                                        .id(SharedString::from(format!("fold-{key}")))
                                        .text_color(theme.muted_foreground)
                                        .child(if unfolded { IconName::ChevronDown } else { IconName::ChevronRight })
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            cx.stop_propagation();
                                            this.toggle(k.clone(), None, cx);
                                        })),
                                )
                                .child(div().text_color(if table.kind == "view" { theme.blue } else { theme.muted_foreground }).child(if table.kind == "view" { IconName::Eye } else { IconName::Table }))
                                .child(div().flex_1().min_w_0().truncate().child(table.name.clone()))
                                .child(div().text_xs().text_color(theme.muted_foreground).child(table.columns.len().to_string()))
                                .into_any_element(),
                        );
                        if unfolded {
                            for c in &table.columns {
                                rows.push(
                                    h_flex()
                                        .h(px(20.))
                                        .pl(px(52.))
                                        .pr_2()
                                        .gap_1()
                                        .text_xs()
                                        .child(div().text_color(if c.pk { theme.warning } else { theme.muted_foreground }).child(if c.pk { IconName::Key } else { IconName::Minus }))
                                        .child(div().flex_1().min_w_0().truncate().child(c.name.clone()))
                                        .child(div().text_color(theme.muted_foreground).child(c.ty.to_lowercase()))
                                        .into_any_element(),
                                );
                            }
                        }
                    }
                }
            }
        }
        let empty = rows.is_empty();
        let hub_entity = self.hub.clone();
        let hub_refresh = self.hub.clone();
        v_flex()
            .size_full()
            .bg(theme.sidebar)
            .child(
                h_flex()
                    .h(px(36.))
                    .px_3()
                    .gap_1()
                    .child(div().flex_1().text_xs().font_weight(FontWeight::MEDIUM).text_color(theme.muted_foreground).child(t("DATABASES")))
                    .child(Button::new("db-add").ghost().xsmall().icon(IconName::Plus).tooltip(t("New connection"))
                        .on_click(move |_, window, cx| connection_dialog(hub_entity.clone(), None, window, cx)))
                    .child(Button::new("db-reload").ghost().xsmall().icon(IconName::RefreshCw).tooltip(t("Refresh"))
                        .on_click(move |_, _, cx| hub_refresh.update(cx, |h, cx| h.reload(cx)))),
            )
            .when_some(busy, |el, b| el.child(h_flex().px_3().pb_1().gap_2().text_xs().text_color(theme.muted_foreground).child(Spinner::new().xsmall()).child(b)))
            .when_some(general_error, |el, e| el.child(div().px_3().pb_1().text_xs().text_color(theme.danger).child(e)))
            .child(
                div().flex_1().min_h_0().child(
                    v_flex()
                        .id("db-tree")
                        .size_full()
                        .overflow_y_scrollbar()
                        .children(rows)
                        .when(empty, |el| {
                            el.child(
                                v_flex()
                                    .p_3()
                                    .gap_2()
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .child(t("No database yet. SQLite files of the project appear here; add a server connection with +."))
                            )
                        }),
                ),
            )
    }
}
