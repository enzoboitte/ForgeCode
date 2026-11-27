//! VS Code editing commands the editor component lacks: surround a selection with quotes or
//! brackets, toggle comments, move / copy / delete lines, insert a line above or below, and
//! select the next or every occurrence.
//!
//! Each command is a pure function from (text, selection) to an [`Edit`]; [`apply`] performs it
//! on an editor as one undo step.

use std::ops::Range;

use gpui_kit::component::input::EditorState;
use gpui_kit::{Context, Window};

use super::*;

/// Replace `range` with `text`, then select `selection` (offsets in the edited text).
#[derive(Debug, PartialEq)]
pub struct Edit {
    pub range: Range<usize>,
    pub text: String,
    pub selection: Range<usize>,
}

pub fn apply(edit: Edit, editor: &mut EditorState, window: &mut Window, cx: &mut Context<EditorState>) {
    editor.set_selected_range(edit.range, cx);
    editor.replace(edit.text, window, cx);
    editor.set_selected_range(edit.selection, cx);
}

/// The closing half when `typed` wraps a selection, like VS Code's `autoSurround`.
pub fn surround_pair(typed: &str, lsp_id: &str) -> Option<&'static str> {
    Some(match typed {
        "\"" => "\"",
        "'" => "'",
        "`" => "`",
        "(" => ")",
        "[" => "]",
        "{" => "}",
        "<" if matches!(lsp_id, "html" | "xml" | "php" | "svelte" | "vue" | "markdown") => ">",
        "*" if lsp_id == "markdown" => "*",
        "_" if lsp_id == "markdown" => "_",
        _ => return None,
    })
}

pub fn surround(text: &str, selection: Range<usize>, open: &str, close: &str) -> Edit {
    let inner = &text[selection.clone()];
    let start = selection.start + open.len();
    Edit { range: selection, text: format!("{open}{inner}{close}"), selection: start..start + inner.len() }
}

/// How a language comments code.
pub enum CommentStyle {
    Line(&'static str),
    Block(&'static str, &'static str),
}

pub fn comment_style(lsp_id: &str) -> Option<CommentStyle> {
    use CommentStyle::*;
    Some(match lsp_id {
        "python" | "shellscript" | "ruby" | "yaml" | "toml" | "makefile" | "cmake" | "elixir" | "r" | "perl" | "dockerfile" => Line("#"),
        "sql" | "lua" | "haskell" => Line("--"),
        "html" | "xml" | "markdown" | "svelte" | "vue" => Block("<!--", "-->"),
        "css" => Block("/*", "*/"),
        "plaintext" | "json" => return None,
        _ => Line("//"),
    })
}

/// Byte range of the whole lines touched by `selection`, without the last line break. A
/// selection ending at the start of a line doesn't include that line, like VS Code.
fn line_span(text: &str, selection: &Range<usize>) -> Range<usize> {
    let start = text[..selection.start].rfind('\n').map_or(0, |i| i + 1);
    let mut end = selection.end;
    if selection.end > selection.start && end > start && text[..end].ends_with('\n') {
        end -= 1;
    }
    let end = text[end..].find('\n').map_or(text.len(), |i| end + i);
    start..end
}

fn indent_of(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

pub fn toggle_comment(text: &str, selection: Range<usize>, style: CommentStyle) -> Edit {
    let span = line_span(text, &selection);
    let block = &text[span.clone()];
    let new = match style {
        CommentStyle::Line(token) => {
            let lines: Vec<&str> = block.split('\n').collect();
            let filled = || lines.iter().filter(|l| !l.trim().is_empty());
            let commented = filled().clone().count() > 0 && filled().all(|l| l.trim_start().starts_with(token));
            let column = filled().map(|l| indent_of(l).len()).min().unwrap_or(0);
            lines
                .iter()
                .map(|line| {
                    if line.trim().is_empty() {
                        line.to_string()
                    } else if commented {
                        let indent = indent_of(line);
                        let rest = &line[indent.len() + token.len()..];
                        format!("{indent}{}", rest.strip_prefix(' ').unwrap_or(rest))
                    } else {
                        format!("{}{token} {}", &line[..column], &line[column..])
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        CommentStyle::Block(open, close) => {
            let indent = indent_of(block);
            let body = block.trim();
            match body.strip_prefix(open).and_then(|b| b.strip_suffix(close)) {
                Some(inner) => format!("{indent}{}", inner.trim()),
                None => format!("{indent}{open} {body} {close}"),
            }
        }
    };
    // Keep the cursor on the same text: shift it by what changed before it on its line.
    let delta = new.len() as isize - block.len() as isize;
    let selection = if selection.is_empty() {
        let line_delta = new.split('\n').next().map_or(0, |l| l.len() as isize) - block.split('\n').next().map_or(0, |l| l.len() as isize);
        let at = (selection.start as isize + line_delta).clamp(span.start as isize, (span.end as isize + delta).max(span.start as isize)) as usize;
        at..at
    } else {
        span.start..(span.end as isize + delta) as usize
    };
    Edit { range: span, text: new, selection }
}

/// Swap the selected lines with the line above or below; `None` at the edge of the file.
pub fn move_lines(text: &str, selection: Range<usize>, up: bool) -> Option<Edit> {
    let span = line_span(text, &selection);
    let block = &text[span.clone()];
    if up {
        if span.start == 0 {
            return None;
        }
        let prev_start = text[..span.start - 1].rfind('\n').map_or(0, |i| i + 1);
        let prev = &text[prev_start..span.start - 1];
        let shift = prev.len() + 1;
        Some(Edit {
            range: prev_start..span.end,
            text: format!("{block}\n{prev}"),
            selection: selection.start - shift..selection.end - shift,
        })
    } else {
        if span.end >= text.len() {
            return None;
        }
        let next_end = text[span.end + 1..].find('\n').map_or(text.len(), |i| span.end + 1 + i);
        let next = &text[span.end + 1..next_end];
        let shift = next.len() + 1;
        Some(Edit {
            range: span.start..next_end,
            text: format!("{next}\n{block}"),
            selection: selection.start + shift..selection.end + shift,
        })
    }
}

/// Duplicate the selected lines above or below, selecting the copy like VS Code.
pub fn copy_lines(text: &str, selection: Range<usize>, up: bool) -> Edit {
    let span = line_span(text, &selection);
    let block = &text[span.clone()];
    let shift = if up { 0 } else { block.len() + 1 };
    Edit {
        range: span.clone(),
        text: format!("{block}\n{block}"),
        selection: selection.start + shift..selection.end + shift,
    }
}

pub fn delete_lines(text: &str, selection: Range<usize>) -> Edit {
    let span = line_span(text, &selection);
    // Take a line break with the lines: the following one, or the previous one at the end.
    if span.end < text.len() {
        return Edit { range: span.start..span.end + 1, text: String::new(), selection: span.start..span.start };
    }
    let range = span.start.saturating_sub(1)..span.end;
    let line_start = text[..range.start].rfind('\n').map_or(0, |i| i + 1);
    Edit { range, text: String::new(), selection: line_start..line_start }
}

/// A new line with the current indentation below (or above) the cursor line.
pub fn insert_line(text: &str, selection: Range<usize>, below: bool) -> Edit {
    let span = line_span(text, &(selection.start..selection.start));
    let indent = indent_of(&text[span.clone()]).to_string();
    if below {
        let at = span.end + 1 + indent.len();
        Edit { range: span.end..span.end, text: format!("\n{indent}"), selection: at..at }
    } else {
        let at = span.start + indent.len();
        Edit { range: span.start..span.start, text: format!("{indent}\n"), selection: at..at }
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The word around `offset`.
pub fn word_at(text: &str, offset: usize) -> Option<Range<usize>> {
    let start = text[..offset].char_indices().rev().take_while(|(_, c)| is_word(*c)).last().map_or(offset, |(i, _)| i);
    let end = text[offset..].char_indices().find(|(_, c)| !is_word(*c)).map_or(text.len(), |(i, _)| offset + i);
    (start < end).then_some(start..end)
}

/// The next occurrence of the selected text after it, wrapping around the file.
pub fn next_occurrence(text: &str, selection: &Range<usize>) -> Option<Range<usize>> {
    let needle = &text[selection.clone()];
    if needle.is_empty() {
        return None;
    }
    let found = text[selection.end..].find(needle).map(|i| selection.end + i).or_else(|| text[..selection.start].find(needle))?;
    Some(found..found + needle.len())
}

pub fn all_occurrences(text: &str, needle: &str) -> Vec<Range<usize>> {
    if needle.is_empty() {
        return vec![];
    }
    text.match_indices(needle).map(|(i, m)| i..i + m.len()).collect()
}

/// A `<<<<<<<` / `=======` / `>>>>>>>` block left by a merge (with an optional `|||||||` base).
#[derive(Clone, Debug, PartialEq)]
pub struct Conflict {
    /// From the `<<<<<<<` line to the end of the `>>>>>>>` line, line break included.
    pub whole: Range<usize>,
    /// The current branch's lines.
    pub ours: Range<usize>,
    /// The incoming branch's lines.
    pub theirs: Range<usize>,
}

pub fn conflicts(text: &str) -> Vec<Conflict> {
    let mut out = vec![];
    let (mut start, mut ours, mut theirs_start) = (None, None, None);
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let next = offset + line.len();
        if line.starts_with("<<<<<<<") {
            (start, ours, theirs_start) = (Some(offset), Some(next..next), None);
        } else if line.starts_with("|||||||") && start.is_some() && theirs_start.is_none() {
            ours = ours.map(|o| o.start..offset);
        } else if line.starts_with("=======") && start.is_some() && theirs_start.is_none() {
            // Without a base section, ours ends here.
            if ours.as_ref().is_some_and(|o| o.end == o.start) {
                ours = ours.map(|o| o.start..offset);
            }
            theirs_start = Some(next);
        } else if line.starts_with(">>>>>>>") {
            if let (Some(s), Some(o), Some(t)) = (start, ours.clone(), theirs_start) {
                out.push(Conflict { whole: s..next, ours: o, theirs: t..offset });
            }
            (start, ours, theirs_start) = (None, None, None);
        }
        offset = next;
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Accept {
    Current,
    Incoming,
    Both,
}

pub fn resolve_conflict(text: &str, c: &Conflict, accept: Accept) -> Edit {
    let kept = match accept {
        Accept::Current => text[c.ours.clone()].to_string(),
        Accept::Incoming => text[c.theirs.clone()].to_string(),
        Accept::Both => format!("{}{}", &text[c.ours.clone()], &text[c.theirs.clone()]),
    };
    let at = c.whole.start;
    Edit { range: c.whole.clone(), text: kept, selection: at..at }
}

/// The conflict around `offset`, else the next one, else the first.
pub fn conflict_near(all: &[Conflict], offset: usize) -> Option<usize> {
    all.iter().position(|c| c.whole.contains(&offset) || c.whole.start >= offset).or((!all.is_empty()).then_some(0))
}

/// The code editor's key context: these commands never reach the chat or other text fields.
const EDITOR: &str = "Input && mode == editor";

pub fn key_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new(secondary!("/"), ToggleLineComment, Some(EDITOR)),
        KeyBinding::new("alt-up", MoveLineUp, Some(EDITOR)),
        KeyBinding::new("alt-down", MoveLineDown, Some(EDITOR)),
        KeyBinding::new("ctrl-shift-up", MoveLineUp, Some(EDITOR)),
        KeyBinding::new("ctrl-shift-down", MoveLineDown, Some(EDITOR)),
        KeyBinding::new("shift-alt-up", CopyLineUp, Some(EDITOR)),
        KeyBinding::new("shift-alt-down", CopyLineDown, Some(EDITOR)),
        KeyBinding::new(secondary!("shift-k"), DeleteLine, Some(EDITOR)),
        KeyBinding::new(secondary!("enter"), InsertLineBelow, Some(EDITOR)),
        KeyBinding::new(secondary!("shift-enter"), InsertLineAbove, Some(EDITOR)),
        KeyBinding::new(secondary!("d"), SelectNextOccurrence, Some(EDITOR)),
        KeyBinding::new(secondary!("shift-l"), SelectAllOccurrences, Some(EDITOR)),
    ]
}

impl Workspace {
    /// The active file's editor, when it has the keyboard focus.
    fn focused_editor(&self, window: &Window, cx: &App) -> Option<(Entity<EditorState>, String)> {
        let b = self.active_buffer()?;
        b.editor.read(cx).focus_handle(cx).is_focused(window).then(|| (b.editor.clone(), b.lang.lsp_id.clone()))
    }

    /// Run a line command on the focused editor.
    fn edit_lines(&mut self, window: &mut Window, cx: &mut Context<Self>, command: impl FnOnce(&str, Range<usize>, &str) -> Option<Edit>) {
        let Some((editor, lsp_id)) = self.focused_editor(window, cx) else { return };
        editor.update(cx, |e, cx| {
            let text = e.text().to_string();
            if let Some(edit) = command(&text, e.selected_range(), &lsp_id) {
                apply(edit, e, window, cx);
            }
        });
    }

    pub(super) fn toggle_line_comment(&mut self, _: &ToggleLineComment, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_lines(window, cx, |text, sel, lsp_id| comment_style(lsp_id).map(|style| toggle_comment(text, sel, style)));
    }
    pub(super) fn move_line_up(&mut self, _: &MoveLineUp, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_lines(window, cx, |text, sel, _| move_lines(text, sel, true));
    }
    pub(super) fn move_line_down(&mut self, _: &MoveLineDown, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_lines(window, cx, |text, sel, _| move_lines(text, sel, false));
    }
    pub(super) fn copy_line_up(&mut self, _: &CopyLineUp, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_lines(window, cx, |text, sel, _| Some(copy_lines(text, sel, true)));
    }
    pub(super) fn copy_line_down(&mut self, _: &CopyLineDown, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_lines(window, cx, |text, sel, _| Some(copy_lines(text, sel, false)));
    }
    pub(super) fn delete_line(&mut self, _: &DeleteLine, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_lines(window, cx, |text, sel, _| Some(delete_lines(text, sel)));
    }
    pub(super) fn insert_line_below(&mut self, _: &InsertLineBelow, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_lines(window, cx, |text, sel, _| Some(insert_line(text, sel, true)));
    }
    pub(super) fn insert_line_above(&mut self, _: &InsertLineAbove, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_lines(window, cx, |text, sel, _| Some(insert_line(text, sel, false)));
    }

    /// Ctrl/Cmd+D: select the word under the cursor, then add the next occurrence as a cursor.
    pub(super) fn select_next_occurrence(&mut self, _: &SelectNextOccurrence, window: &mut Window, cx: &mut Context<Self>) {
        let Some((editor, _)) = self.focused_editor(window, cx) else { return };
        editor.update(cx, |e, cx| {
            let text = e.text().to_string();
            let sel = e.selected_range();
            if sel.is_empty() {
                if let Some(word) = word_at(&text, sel.start) {
                    e.set_selected_range(word, cx);
                }
            } else if let Some(next) = next_occurrence(&text, &sel) {
                e.add_selection(next, cx);
            }
        });
    }

    pub(super) fn select_all_occurrences(&mut self, _: &SelectAllOccurrences, window: &mut Window, cx: &mut Context<Self>) {
        let Some((editor, _)) = self.focused_editor(window, cx) else { return };
        editor.update(cx, |e, cx| {
            let text = e.text().to_string();
            let mut sel = e.selected_range();
            if sel.is_empty() {
                let Some(word) = word_at(&text, sel.start) else { return };
                sel = word;
            }
            let found = all_occurrences(&text, &text[sel.clone()]);
            e.set_selected_range(sel.clone(), cx);
            for range in found.into_iter().filter(|r| *r != sel) {
                e.add_selection(range, cx);
            }
        });
    }
}

impl Workspace {
    /// Apply a resolution to the conflict near the cursor of `path`'s editor.
    pub(super) fn accept_conflict(&mut self, path: &std::path::Path, accept: Accept, window: &mut Window, cx: &mut Context<Self>) {
        let Some(b) = self.buffers().find(|b| b.path == path) else { return };
        let editor = b.editor.clone();
        editor.update(cx, |e, cx| {
            let text = e.text().to_string();
            let all = conflicts(&text);
            if let Some(ix) = conflict_near(&all, e.selected_range().start) {
                apply(resolve_conflict(&text, &all[ix], accept), e, window, cx);
            }
        });
    }

    /// Move the cursor to the next (or previous) conflict, wrapping around.
    pub(super) fn goto_conflict(&mut self, path: &std::path::Path, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(b) = self.buffers().find(|b| b.path == path) else { return };
        let (editor, all) = (b.editor.clone(), b.conflicts.clone());
        editor.update(cx, |e, cx| {
            let at = e.selected_range().start;
            let target = if forward {
                all.iter().find(|c| c.whole.start > at).or(all.first())
            } else {
                all.iter().rev().find(|c| c.whole.start < at && !c.whole.contains(&at)).or(all.last())
            };
            if let Some(c) = target {
                e.set_selected_range(c.whole.start..c.whole.start, cx);
            }
            e.focus(window, cx);
        });
    }
}

/// Typing a quote or bracket over a selection wraps it instead of replacing it. Returns
/// whether it did, so the key stops there.
pub(super) fn surround_typed(editor: &Entity<EditorState>, lsp_id: &str, typed: &str, window: &mut Window, cx: &mut App) -> bool {
    let Some(close) = surround_pair(typed, lsp_id) else { return false };
    editor.update(cx, |e, cx| {
        let sel = e.selected_range();
        if sel.is_empty() {
            return false;
        }
        let text = e.text().to_string();
        apply(surround(&text, sel, typed, close), e, window, cx);
        true
    })
}

#[cfg(test)]
mod tests {
    use super::{Accept, CommentStyle, Edit, conflict_near, conflicts, resolve_conflict, all_occurrences, copy_lines, delete_lines, insert_line, move_lines, next_occurrence, surround, surround_pair, toggle_comment, word_at};

    fn run(text: &str, edit: Edit) -> (String, String) {
        let mut out = text.to_string();
        out.replace_range(edit.range, &edit.text);
        let selected = out[edit.selection].to_string();
        (out, selected)
    }

    #[test]
    fn surrounds_the_selection() {
        let text = "let a = hello;";
        assert_eq!(surround_pair("\"", "rust"), Some("\""));
        assert_eq!(run(text, surround(text, 8..13, "\"", "\"")), ("let a = \"hello\";".into(), "hello".into()));
        assert_eq!(surround_pair("<", "rust"), None);
    }

    #[test]
    fn toggles_line_comments() {
        let text = "fn a() {\n    x();\n\n    y();\n}";
        let (commented, _) = run(text, toggle_comment(text, 9..25, CommentStyle::Line("//")));
        assert_eq!(commented, "fn a() {\n    // x();\n\n    // y();\n}");
        let (back, _) = run(&commented, toggle_comment(&commented, 9..30, CommentStyle::Line("//")));
        assert_eq!(back, text);
        let html = "  <p>hi</p>";
        assert_eq!(run(html, toggle_comment(html, 4..4, CommentStyle::Block("<!--", "-->"))).0, "  <!-- <p>hi</p> -->");
    }

    #[test]
    fn moves_copies_and_deletes_lines() {
        let text = "a\nb\nc";
        assert_eq!(run(text, move_lines(text, 2..2, true).unwrap()).0, "b\na\nc");
        assert_eq!(run(text, move_lines(text, 2..2, false).unwrap()).0, "a\nc\nb");
        assert!(move_lines(text, 0..0, true).is_none() && move_lines(text, 4..4, false).is_none());
        assert_eq!(run(text, copy_lines(text, 2..3, false)), ("a\nb\nb\nc".into(), "b".into()));
        assert_eq!(run(text, delete_lines(text, 2..2)).0, "a\nc");
        assert_eq!(run(text, delete_lines(text, 4..4)).0, "a\nb");
        let indented = "    x";
        assert_eq!(run(indented, insert_line(indented, 5..5, true)).0, "    x\n    ");
    }

    #[test]
    fn resolves_merge_conflicts() {
        let text = "a\n<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> feature\nb\n<<<<<<< HEAD\nx\n||||||| base\nw\n=======\ny\n>>>>>>> f\n";
        let all = conflicts(text);
        assert_eq!(all.len(), 2);
        assert_eq!((&text[all[0].ours.clone()], &text[all[0].theirs.clone()]), ("ours\n", "theirs\n"));
        assert_eq!((&text[all[1].ours.clone()], &text[all[1].theirs.clone()]), ("x\n", "y\n"), "the base section is dropped");
        assert_eq!(run(text, resolve_conflict(text, &all[0], Accept::Incoming)).0, "a\ntheirs\nb\n<<<<<<< HEAD\nx\n||||||| base\nw\n=======\ny\n>>>>>>> f\n");
        assert_eq!(run(text, resolve_conflict(text, &all[1], Accept::Both)).0.ends_with("b\nx\ny\n"), true);
        assert_eq!(conflict_near(&all, 0), Some(0));
        assert_eq!(conflict_near(&all, text.len()), Some(0), "wraps to the first");
    }

    #[test]
    fn finds_occurrences() {
        let text = "foo bar foo";
        assert_eq!(word_at(text, 1), Some(0..3));
        assert_eq!(next_occurrence(text, &(0..3)), Some(8..11));
        assert_eq!(next_occurrence(text, &(8..11)), Some(0..3), "wraps around");
        assert_eq!(all_occurrences(text, "foo"), vec![0..3, 8..11]);
    }
}
