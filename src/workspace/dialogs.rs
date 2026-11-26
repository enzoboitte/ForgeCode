//! Modal prompts built on the component library's dialog.

use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::i18n::t;

type OnSubmit = Rc<dyn Fn(String, bool, &mut Window, &mut App)>;

/// Ask for a line of text; `on_submit` gets the trimmed, non-empty value.
pub fn prompt_text(
    title: impl Into<SharedString>,
    initial: &str,
    ok_label: &'static str,
    window: &mut Window,
    cx: &mut App,
    on_submit: impl Fn(String, &mut Window, &mut App) + 'static,
) {
    prompt(title.into(), initial, ok_label, None, window, cx, move |v, _, w, cx| on_submit(v, w, cx));
}

/// [`prompt_text`] with a checkbox under the field (`label`, checked at first or not);
/// `on_submit` also gets whether it is checked.
pub fn prompt_text_with_option(
    title: impl Into<SharedString>,
    initial: &str,
    ok_label: &'static str,
    option: (&'static str, bool),
    window: &mut Window,
    cx: &mut App,
    on_submit: impl Fn(String, bool, &mut Window, &mut App) + 'static,
) {
    prompt(title.into(), initial, ok_label, Some(option), window, cx, on_submit);
}

fn prompt(
    title: SharedString,
    initial: &str,
    ok_label: &'static str,
    option: Option<(&'static str, bool)>,
    window: &mut Window,
    cx: &mut App,
    on_submit: impl Fn(String, bool, &mut Window, &mut App) + 'static,
) {
    let checked = Rc::new(std::cell::Cell::new(option.is_some_and(|(_, on)| on)));
    let input = cx.new(|cx| InputState::new(window, cx).default_value(initial.to_string()));
    let on_submit: OnSubmit = Rc::new(on_submit);
    let submit = {
        let (input, on_submit, checked) = (input.clone(), on_submit.clone(), checked.clone());
        Rc::new(move |window: &mut Window, cx: &mut App| {
            let value = input.read(cx).value().trim().to_string();
            if !value.is_empty() {
                window.close_dialog(cx);
                on_submit(value, checked.get(), window, cx);
            }
        })
    };
    let enter = submit.clone();
    window
        .subscribe(&input, cx, move |_, event: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = event {
                enter(window, cx);
            }
        })
        .detach();
    let focus = input.read(cx).focus_handle(cx);
    window.open_dialog(cx, move |dialog, _, _| {
        let submit = submit.clone();
        let checked = checked.clone();
        dialog.title(title.clone()).w(px(420.)).child(
            v_flex()
                .gap_3()
                .child(Input::new(&input))
                .when_some(option, |el, (label, _)| {
                    el.child(gpui_kit::component::checkbox::Checkbox::new("dlg-option").label(t(label)).checked(checked.get()).on_click(move |on, window, _| {
                        checked.set(*on);
                        window.refresh();
                    }))
                })
                .child(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(Button::new("dlg-cancel").small().ghost().label(t("Cancel")).on_click(|_, w, cx| w.close_dialog(cx)))
                        .child(Button::new("dlg-ok").small().primary().label(ok_label).on_click(move |_, w, cx| submit(w, cx))),
                ),
        )
    });
    // The dialog takes focus when it opens: give it to the field afterwards.
    window.defer(cx, move |window, cx| focus.focus(window, cx));
}

/// Ask for confirmation of a destructive action.
pub fn confirm(
    title: impl Into<SharedString>,
    message: impl Into<SharedString>,
    ok_label: &'static str,
    window: &mut Window,
    cx: &mut App,
    on_ok: impl Fn(&mut Window, &mut App) + 'static,
) {
    let (title, message) = (title.into(), message.into());
    let on_ok = Rc::new(on_ok);
    window.open_dialog(cx, move |dialog, _, _| {
        let on_ok = on_ok.clone();
        dialog.title(title.clone()).w(px(420.)).child(
            v_flex()
                .gap_3()
                .child(message.clone())
                .child(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(Button::new("dlg-cancel").small().ghost().label(t("Cancel")).on_click(|_, w, cx| w.close_dialog(cx)))
                        .child(Button::new("dlg-ok").small().danger().label(ok_label).on_click(move |_, w, cx| {
                            w.close_dialog(cx);
                            on_ok(w, cx);
                        })),
                ),
        )
    });
}
