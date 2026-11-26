pub mod acp;
pub mod agent;
pub mod agents_view;
pub mod anthropic;
pub mod changes;
pub mod command_safety;
pub mod chat_panel;
pub mod copilot;
pub mod gateway;
pub mod hermes_graph;
pub mod hermes_panel;
pub mod llm;
pub mod models;
pub mod providers_view;
pub mod sampling;
pub mod supervisor;
pub mod tools;
pub mod web;
pub mod web_mcp;

/// A chat message as Markdown; each code block shows its language and a button copying its code.
pub fn markdown(id: impl Into<gpui_kit::ElementId>, text: impl Into<gpui_kit::SharedString>) -> gpui_kit::component::text::TextView {
    use gpui_kit::assets::IconName;
    use gpui_kit::component::button::{Button, ButtonVariants as _};
    use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex};
    use gpui_kit::{ClipboardItem, ParentElement as _, Styled as _};
    use std::hash::{Hash as _, Hasher as _};
    gpui_kit::component::text::TextView::markdown(id, text).code_block_actions(|block, _, cx| {
        let code = block.code();
        // One id per block (its content), so each button keeps its own state.
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        code.hash(&mut hasher);
        let key = hasher.finish() as usize;
        let lang = block.lang().unwrap_or_default();
        h_flex()
            .gap_1()
            .child(gpui_kit::div().text_xs().text_color(cx.theme().muted_foreground).child(lang))
            .child(Button::new(("copy-code", key)).ghost().xsmall().icon(IconName::Copy).tooltip(crate::i18n::t("Copy code")).on_click(move |_, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(code.to_string()));
            }))
    })
}
