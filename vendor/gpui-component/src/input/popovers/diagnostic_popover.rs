use std::rc::Rc;

use gpui::{
    App, AppContext as _, Context, Empty, Entity, IntoElement, Render, Styled, WeakEntity, Window,
};

use crate::{
    highlighter::DiagnosticEntry,
    input::{
        EditorState,
        popovers::{Popover, render_markdown},
    },
};

/// Labels of the problem popover, set by the app in its language (English by default).
pub struct ProblemLabels {
    pub quick_fix: gpui::SharedString,
}

impl gpui::Global for ProblemLabels {}

/// ForgeCode patch: the problem's message, colored by severity.
pub(crate) fn problem_message(d: &DiagnosticEntry, window: &mut Window, cx: &mut App) -> gpui::AnyElement {
    use gpui::ParentElement as _;
    let fg = crate::highlighter::diagnostic_foreground(d.severity, cx);
    let source = d.source.as_ref().map(|s| format!("  ({s})")).unwrap_or_default();
    gpui::div()
        .flex()
        .flex_col()
        .text_color(fg)
        .child(render_markdown("problem", format!("{}{source}", d.message), window, cx))
        .into_any_element()
}

/// ForgeCode patch, like VS Code: "Quick Fix… (⌘.)" moves the cursor to the problem and opens
/// the code actions (the language server's fixes, and the app's own such as fixing with AI).
pub(crate) fn problem_actions(d: &DiagnosticEntry, editor: Entity<EditorState>, cx: &mut App) -> gpui::AnyElement {
    use crate::ActiveTheme as _;
    use gpui::{InteractiveElement as _, ParentElement as _, StatefulInteractiveElement as _};
    let label = cx.try_global::<ProblemLabels>().map(|l| l.quick_fix.clone()).unwrap_or_else(|| "Quick Fix…".into());
    let shortcut = if cfg!(target_os = "macos") { "⌘." } else { "Ctrl+." };
    let start = d.range.start;
    gpui::div()
        .flex()
        .gap_3()
        .child(
            gpui::div()
                .id("quick-fix")
                .text_sm()
                .text_color(cx.theme().link)
                .cursor_pointer()
                .hover(|s| s.underline())
                .child(format!("{label} ({shortcut})"))
                .on_click(move |_, window, cx| {
                    editor.update(cx, |e, cx| {
                        e.set_selected_range(start..start, cx);
                        e.focus(window, cx);
                    });
                    window.dispatch_action(Box::new(gpui_base::input::ToggleCodeActions), cx);
                }),
        )
        .into_any_element()
}

/// The problem alone (no documentation for this spot): message, then actions.
pub(crate) fn problem_section(d: &DiagnosticEntry, editor: Entity<EditorState>, window: &mut Window, cx: &mut App) -> gpui::AnyElement {
    use crate::ActiveTheme as _;
    use gpui::ParentElement as _;
    gpui::div()
        .flex()
        .flex_col()
        .gap_1()
        .child(problem_message(d, window, cx))
        .child(gpui::div().h(gpui::px(1.)).bg(cx.theme().border))
        .child(problem_actions(d, editor, cx))
        .into_any_element()
}

pub struct DiagnosticPopover {
    state: WeakEntity<EditorState>,
    pub(crate) diagnostic: Rc<DiagnosticEntry>,
}

impl DiagnosticPopover {
    pub fn new(
        diagnostic: &DiagnosticEntry,
        state: Entity<EditorState>,
        cx: &mut App,
    ) -> Entity<Self> {
        let diagnostic = Rc::new(diagnostic.clone());

        cx.new(|_| Self {
            diagnostic,
            state: state.downgrade(),
        })
    }
}

impl Render for DiagnosticPopover {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(state) = self.state.upgrade() else {
            return Empty.into_any_element();
        };
        let message = self.diagnostic.message.clone();

        let (border, bg, fg) = (
            crate::highlighter::diagnostic_border(self.diagnostic.severity, cx),
            crate::highlighter::diagnostic_background(self.diagnostic.severity, cx),
            crate::highlighter::diagnostic_foreground(self.diagnostic.severity, cx),
        );

        let diagnostic = self.diagnostic.clone();
        let editor = state.clone();
        let _ = message;
        Popover::new(
            "diagnostic-popover",
            state,
            self.diagnostic.range.clone(),
            move |window, cx| problem_section(&diagnostic, editor.clone(), window, cx),
        )
        .px_1()
        .py_0p5()
        .bg(bg)
        .text_color(fg)
        .border_1()
        .border_color(border)
        .into_any_element()
    }
}
