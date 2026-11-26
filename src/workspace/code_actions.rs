//! Quick fixes (⌘. / Ctrl+.), like VS Code's light bulb: the language server's own fixes for the
//! problems at the cursor, then "Fix with AI" for each of them, which asks the chat to fix it.
//! Nothing is changed unless one is picked.

use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use gpui_kit::component::input::{CodeActionProvider, EditorState, Rope, RopeExt as _};
use gpui_kit::{App, AppContext as _, Entity, SharedString, Task, Window};
use lsp_types::{CodeAction, CodeActionKind, Diagnostic, DiagnosticSeverity};
use serde_json::json;

use super::editing::{Edit, apply};
use crate::i18n::t;
use crate::lsp::client::{LspClient, uri};

/// The file's current problems, kept up to date by the buffer.
pub type Problems = Arc<Mutex<Vec<Diagnostic>>>;

const AI: &str = "forge.ai-fix";

pub struct QuickFixes {
    pub path: PathBuf,
    pub language: String,
    pub lsp: Option<Arc<LspClient>>,
    pub problems: Problems,
}

impl QuickFixes {
    /// Problems on the lines of `range` (the cursor's line when nothing is selected).
    fn problems_at(&self, text: &Rope, range: &Range<usize>) -> Vec<Diagnostic> {
        let (first, last) = (text.offset_to_position(range.start).line, text.offset_to_position(range.end).line);
        self.problems.lock().unwrap().iter().filter(|d| d.range.start.line <= last && d.range.end.line >= first).cloned().collect()
    }
}

impl CodeActionProvider for QuickFixes {
    fn id(&self) -> SharedString {
        "forge.quick-fixes".into()
    }

    fn code_actions(&self, state: Entity<EditorState>, range: Range<usize>, _: &mut Window, cx: &mut App) -> Task<Result<Vec<CodeAction>>> {
        let text = state.read(cx).text().clone();
        let problems = self.problems_at(&text, &range);
        let ai: Vec<CodeAction> = problems
            .iter()
            .filter(|d| d.severity != Some(DiagnosticSeverity::HINT))
            .map(|d| CodeAction {
                title: format!("{} : {}", t("Fix with AI"), first_line(&d.message)),
                kind: Some(CodeActionKind::QUICKFIX),
                data: Some(json!({AI: ai_prompt(&self.path, &self.language, &text, d), "diagnostic": d})),
                ..Default::default()
            })
            .collect();
        let Some(lsp) = self.lsp.clone() else { return Task::ready(Ok(ai)) };
        let params = json!({
            "textDocument": {"uri": uri(&self.path)},
            "range": {"start": text.offset_to_position(range.start), "end": text.offset_to_position(range.end)},
            "context": {"diagnostics": problems, "triggerKind": 1},
        });
        cx.background_spawn(async move {
            // A server without code actions, or one that fails, still leaves the AI fixes.
            let value = lsp.request("textDocument/codeAction", params).await.unwrap_or_default();
            let mut fixes: Vec<CodeAction> = value
                .as_array()
                .into_iter()
                .flatten()
                // Plain `Command`s (no `edit`) are wrapped so they run through executeCommand.
                .filter_map(|v| serde_json::from_value::<CodeAction>(v.clone()).ok().or_else(|| {
                    let title = v["title"].as_str()?.to_string();
                    Some(CodeAction { title, command: serde_json::from_value(v.clone()).ok(), ..Default::default() })
                }))
                .filter(|a| a.disabled.is_none())
                .collect();
            // The server's preferred fix first, as VS Code does.
            fixes.sort_by_key(|a| a.is_preferred != Some(true));
            fixes.extend(ai);
            Ok(fixes)
        })
    }

    fn perform_code_action(&self, state: Entity<EditorState>, action: CodeAction, _: bool, window: &mut Window, cx: &mut App) -> Task<Result<()>> {
        if action.data.as_ref().is_some_and(|d| d.get(AI).is_some()) {
            // Like VS Code: fixed in place with Keep / Undo, not in the chat.
            if let Some(d) = action.data.as_ref().and_then(|d| serde_json::from_value::<Diagnostic>(d["diagnostic"].clone()).ok()) {
                let path = self.path.clone();
                window.defer(cx, move |_, cx| super::ai_fix::request(path, d, cx));
            }
            return Task::ready(Ok(()));
        }
        let Some(lsp) = self.lsp.clone() else { return Task::ready(Ok(())) };
        let path = self.path.clone();
        let window_handle = window.window_handle();
        cx.spawn(async move |cx| {
            // Fixes sent without their edit are completed by `codeAction/resolve`.
            let action = if action.edit.is_none() && action.data.is_some() {
                let resolved = lsp.request("codeAction/resolve", serde_json::to_value(&action)?).await?;
                serde_json::from_value(resolved).unwrap_or(action)
            } else {
                action
            };
            if let Some(edits) = action.edit.as_ref().map(|e| edits_for(e, &uri(&path))) {
                cx.update_window(window_handle, |_, window, cx| {
                    state.update(cx, |e, cx| {
                        let text = e.text().clone();
                        if let Some(edit) = minimal_edit(&text.to_string(), &crate::lsp::provider::apply_edits(&text, &edits)) {
                            apply(edit, e, window, cx);
                        }
                    })
                })?;
            }
            if let Some(command) = action.command {
                let _ = lsp.request("workspace/executeCommand", json!({"command": command.command, "arguments": command.arguments})).await;
            }
            Ok(())
        })
    }
}

/// The edits a workspace edit makes to the file at `file_uri`.
fn edits_for(edit: &lsp_types::WorkspaceEdit, file_uri: &str) -> Vec<lsp_types::TextEdit> {
    let mut out: Vec<lsp_types::TextEdit> =
        edit.changes.iter().flatten().filter(|(u, _)| u.as_str() == file_uri).flat_map(|(_, e)| e.iter().cloned()).collect();
    if let Some(lsp_types::DocumentChanges::Edits(docs)) = &edit.document_changes {
        for doc in docs.iter().filter(|d| d.text_document.uri.as_str() == file_uri) {
            out.extend(doc.edits.iter().map(|e| match e {
                lsp_types::OneOf::Left(e) => e.clone(),
                lsp_types::OneOf::Right(a) => a.text_edit.clone(),
            }));
        }
    }
    out
}

/// One replacement turning `old` into `new` (what differs between their common ends), so the
/// editor keeps the cursor and a single undo step.
pub fn minimal_edit(old: &str, new: &str) -> Option<Edit> {
    if old == new {
        return None;
    }
    let mut start = old.bytes().zip(new.bytes()).take_while(|(a, b)| a == b).count();
    while !old.is_char_boundary(start) || !new.is_char_boundary(start) {
        start -= 1;
    }
    let max_suffix = (old.len() - start).min(new.len() - start);
    let mut suffix = old.bytes().rev().zip(new.bytes().rev()).take(max_suffix).take_while(|(a, b)| a == b).count();
    while !old.is_char_boundary(old.len() - suffix) || !new.is_char_boundary(new.len() - suffix) {
        suffix -= 1;
    }
    let text = new[start..new.len() - suffix].to_string();
    let end = start + text.len();
    Some(Edit { range: start..old.len() - suffix, text, selection: end..end })
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s)
}

/// What the chat is asked: the problem, where it is, and the code around it.
pub fn ai_prompt(path: &std::path::Path, language: &str, text: &Rope, d: &Diagnostic) -> String {
    let line = d.range.start.line as usize;
    let source_text = text.to_string();
    let all: Vec<&str> = source_text.lines().collect();
    let (from, to) = (line.saturating_sub(8), (line + 9).min(all.len()));
    let numbered: String = (from..to).map(|i| format!("{:>4} | {}\n", i + 1, all[i])).collect();
    let source = d.source.as_deref().map(|s| format!(" ({s})")).unwrap_or_default();
    format!(
        "Corrige ce problème dans `{}`, ligne {}{source} :\n\n> {}\n\nExtrait :\n```{language}\n{numbered}```\n\nVérifie les noms exacts dans la bibliothèque concernée si l'erreur en vient, puis applique la correction au fichier.",
        path.display(),
        line + 1,
        d.message.replace('\n', "\n> "),
    )
}

/// Used by the buffer: the providers list for its editor.
pub fn provider(path: PathBuf, language: String, lsp: Option<Arc<LspClient>>, problems: Problems) -> Rc<dyn CodeActionProvider> {
    Rc::new(QuickFixes { path, language, lsp, problems })
}

#[cfg(test)]
mod tests {
    use super::minimal_edit;

    #[test]
    fn minimal_edit_keeps_common_ends() {
        let e = minimal_edit("from lib import Sesion\nx = 1\n", "from lib import Session\nx = 1\n").unwrap();
        assert_eq!((e.range.clone(), e.text.as_str()), (19..19, "s"));
        assert!(minimal_edit("a", "a").is_none());
        let e = minimal_edit("é", "è").unwrap();
        assert_eq!((e.range, e.text.as_str()), (0..2, "è"));
    }
}
