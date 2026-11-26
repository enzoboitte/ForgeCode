//! "Fix with AI", like VS Code's Copilot quick fix: no chat. A small card says the AI is
//! thinking, then the fix is applied in place, highlighted, with Keep / Undo; undoing restores
//! the exact original text.

use std::ops::Range;

use super::*;

/// The AI fix of one open file.
#[derive(Clone, Debug, PartialEq)]
pub enum AiFix {
    Thinking { line: u32, message: String },
    /// Applied: `range` is the new text in the buffer, `original` what it replaced.
    Proposed { range: Range<usize>, original: String, message: String },
    Failed(String),
}

/// Lines `line - radius ..= line + radius` (0-based `line`) as a byte range, whole lines.
pub fn fix_region(text: &str, line: usize, radius: usize) -> Range<usize> {
    let starts: Vec<usize> = std::iter::once(0).chain(text.match_indices('\n').map(|(i, _)| i + 1)).collect();
    let first = line.saturating_sub(radius).min(starts.len() - 1);
    let last = (line + radius + 1).min(starts.len());
    let end = starts.get(last).copied().unwrap_or(text.len());
    starts[first]..end
}

/// The model's code without fences or chatter, ending with a line break like `original` does.
pub fn clean_reply(reply: &str, original: &str) -> String {
    let mut body = reply.trim_matches('\n').to_string();
    if let Some(start) = body.find("```") {
        let after = &body[start + 3..];
        let after = after.split_once('\n').map_or("", |(_, rest)| rest);
        body = after.split("```").next().unwrap_or(after).trim_end_matches('\n').to_string();
    }
    if original.ends_with('\n') && !body.ends_with('\n') {
        body.push('\n');
    }
    body
}

impl Workspace {
    /// Ask the chat model for a fix of `diagnostic` in `path`'s editor, and show it in place.
    pub(super) fn start_ai_fix(&mut self, path: PathBuf, diagnostic: lsp_types::Diagnostic, cx: &mut Context<Self>) {
        let backend = match crate::ai::chat_panel::chat_backend(cx) {
            Ok(b) => b,
            Err(e) => {
                if let Some(b) = self.buffers_mut().find(|b| b.path == path) {
                    b.ai_fix = Some(AiFix::Failed(e.to_string()));
                }
                return cx.notify();
            }
        };
        let Some(b) = self.buffers_mut().find(|b| b.path == path) else { return };
        let text = b.editor.read(cx).text().to_string();
        let line = diagnostic.range.start.line as usize;
        let region = fix_region(&text, line, 6);
        let original = text[region.clone()].to_string();
        let lang = b.lang.lsp_id.clone();
        b.ai_fix = Some(AiFix::Thinking { line: line as u32 + 1, message: diagnostic.message.clone() });
        cx.notify();
        let first_line = text[..region.start].matches('\n').count() + 1;
        let numbered: String = original.lines().enumerate().map(|(i, l)| format!("{:>4} | {l}\n", first_line + i)).collect();
        // The whole file for context, within reason.
        let context: String = text.chars().take(12_000).collect();
        let messages = [
            serde_json::json!({"role": "system", "content": "You fix code. Reply with ONLY the corrected replacement for the given lines: same lines, same indentation, no line numbers, no explanation, no code fences. Change as little as possible."}),
            serde_json::json!({"role": "user", "content": format!(
                "File `{}` ({lang}). Problem on line {}: {}\n\nWhole file for context:\n{context}\n\nReplace exactly these lines (numbers only for reference):\n{numbered}",
                path.display(), line + 1, diagnostic.message
            )}),
        ];
        let task = cx.background_spawn(async move { backend.complete(&messages, 2000) });
        cx.spawn(async move |this, cx| {
            let reply = task.await;
            let Some(window) = cx.update(|cx| cx.windows().first().copied()) else { return };
            let _ = window.update(cx, |_, window, cx| {
                let _ = this.update(cx, |ws, cx| ws.show_ai_fix(&path, region, original, reply, window, cx));
            });
        })
        .detach();
    }

    pub(super) fn show_ai_fix(&mut self, path: &Path, region: Range<usize>, original: String, reply: anyhow::Result<String>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(b) = self.buffers_mut().find(|b| b.path == path) else { return };
        let message = match &b.ai_fix {
            Some(AiFix::Thinking { message, .. }) => message.clone(),
            // Dismissed meanwhile.
            _ => return,
        };
        let reply = match reply {
            Ok(r) => clean_reply(&r, &original),
            Err(e) => {
                b.ai_fix = Some(AiFix::Failed(e.to_string()));
                return cx.notify();
            }
        };
        let text = b.editor.read(cx).text().to_string();
        // The user edited that part meanwhile: don't overwrite it.
        if text.get(region.clone()) != Some(original.as_str()) {
            b.ai_fix = Some(AiFix::Failed(t("The code changed while the AI was thinking.").into()));
            return cx.notify();
        }
        if reply == original {
            b.ai_fix = Some(AiFix::Failed(t("The AI proposes no change.").into()));
            return cx.notify();
        }
        let start = region.start;
        b.editor.update(cx, |e, cx| {
            e.set_selected_range(region.clone(), cx);
            e.replace(reply.clone(), window, cx);
            e.set_selected_range(start..start, cx);
        });
        let range = start..start + reply.len();
        b.show_fix_marks(Some(range.clone()), cx);
        b.ai_fix = Some(AiFix::Proposed { range, original, message });
        cx.notify();
    }

    /// Keep the proposed fix, or put the original back.
    pub(super) fn resolve_ai_fix(&mut self, path: &Path, keep: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(b) = self.buffers_mut().find(|b| b.path == path) else { return };
        if let (false, Some(AiFix::Proposed { range, original, .. })) = (keep, b.ai_fix.clone()) {
            b.editor.update(cx, |e, cx| {
                if e.text().to_string().get(range.clone()).is_some() {
                    e.set_selected_range(range.clone(), cx);
                    e.replace(original, window, cx);
                    e.set_selected_range(range.start..range.start, cx);
                }
            });
        }
        b.ai_fix = None;
        b.show_fix_marks(None, cx);
        cx.notify();
    }
}

/// Start an AI fix from the quick-fix menu.
pub fn request(path: PathBuf, diagnostic: lsp_types::Diagnostic, cx: &mut App) {
    let Some(ws) = cx.try_global::<WorkspaceHandle>().map(|h| h.0.clone()) else { return };
    let _ = ws.update(cx, |w, cx| w.start_ai_fix(path, diagnostic, cx));
}

#[cfg(test)]
mod tests {
    use super::{clean_reply, fix_region};

    #[test]
    fn picks_whole_lines_and_cleans_replies() {
        let text = "a\nb\nc\nd\ne\n";
        assert_eq!(&text[fix_region(text, 2, 1)], "b\nc\nd\n");
        assert_eq!(&text[fix_region(text, 0, 1)], "a\nb\n");
        assert_eq!(&text[fix_region(text, 4, 3)], "b\nc\nd\ne\n");
        assert_eq!(clean_reply("```python\nx = 1\n```", "y = 1\n"), "x = 1\n");
        assert_eq!(clean_reply("x = 1", "y = 1\n"), "x = 1\n");
    }
}
