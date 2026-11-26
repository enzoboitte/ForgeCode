//! "New project" dialog: template, name, location.

use std::path::PathBuf;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::Workspace;
use crate::i18n::t;
use crate::project::{self, Template};

pub struct NewProject {
    template: Template,
    name: Entity<InputState>,
    location: PathBuf,
    error: Option<String>,
    workspace: WeakEntity<Workspace>,
}

impl NewProject {
    pub fn open(location: PathBuf, workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut App) {
        let name = cx.new(|cx| InputState::new(window, cx).default_value("my-project"));
        let focus = name.read(cx).focus_handle(cx);
        let view = cx.new(|_| Self { template: Template::Cpp, name, location, error: None, workspace });
        window.open_dialog(cx, move |dialog, _, _| dialog.title(t("New project")).w(px(480.)).child(view.clone()));
        window.defer(cx, move |window, cx| focus.focus(window, cx));
    }

    fn pick_location(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions { files: false, directories: true, multiple: false, prompt: None });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = paths.await {
                if let Some(p) = paths.into_iter().next() {
                    let _ = this.update(cx, |this, cx| {
                        this.location = p;
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.name.read(cx).value().trim().to_string();
        match project::create(self.template, &self.location, &name) {
            Ok((dir, main)) => {
                window.close_dialog(cx);
                let _ = self.workspace.update(cx, |ws, cx| {
                    ws.set_root(dir, cx);
                    ws.open_file(main, None, window, cx);
                });
            }
            Err(e) => {
                self.error = Some(e.to_string());
                cx.notify();
            }
        }
    }
}

impl Render for NewProject {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .gap_3()
            .child(h_flex().flex_wrap().gap_1().children(Template::ALL.iter().enumerate().map(|(ix, tpl)| {
                let tpl = *tpl;
                Button::new(("tpl", ix)).small().label(tpl.label())
                    .when(tpl == self.template, |b| b.primary())
                    .when(tpl != self.template, |b| b.ghost())
                    .on_click(cx.listener(move |t, _, _, cx| { t.template = tpl; cx.notify(); }))
            })))
            .child(Input::new(&self.name))
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().truncate().text_xs().text_color(theme.muted_foreground).child(self.location.display().to_string()))
                    .child(Button::new("np-loc").small().ghost().label(t("Folder")).on_click(cx.listener(|t, _, w, cx| t.pick_location(w, cx)))),
            )
            .when_some(self.error.clone(), |el, e| el.child(div().text_xs().text_color(theme.danger).child(e)))
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(Button::new("np-cancel").small().ghost().label(t("Cancel")).on_click(|_, w, cx| w.close_dialog(cx)))
                    .child(Button::new("np-create").small().primary().label(t("Create")).on_click(cx.listener(|t, _, w, cx| t.create(w, cx)))),
            )
    }
}
