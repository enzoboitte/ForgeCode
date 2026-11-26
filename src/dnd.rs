//! Values carried by drag & drop inside the window.

use std::path::PathBuf;

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::*;

/// A tab being dragged, with its drag preview.
#[derive(Clone)]
pub struct DraggedTab {
    pub pane: usize,
    pub ix: usize,
    pub label: SharedString,
    /// The file shown by the tab, if any.
    pub path: Option<PathBuf>,
}

/// A file dragged from the explorer.
#[derive(Clone)]
pub struct DraggedFile(pub PathBuf);

fn drag_chip(label: SharedString, cx: &App) -> impl IntoElement {
    div()
        .px_2()
        .py_1()
        .rounded_md()
        .text_sm()
        .bg(cx.theme().popover)
        .border_1()
        .border_color(cx.theme().border)
        .shadow_md()
        .child(label)
}

impl Render for DraggedTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        drag_chip(self.label.clone(), cx)
    }
}

impl Render for DraggedFile {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let name = self.0.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        drag_chip(name.into(), cx)
    }
}

