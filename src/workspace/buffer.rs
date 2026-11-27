//! An open file: its editor state, language, language server and decorations.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::input::{
    EditorState, GutterDecoration, GutterDecorationCollection, OverviewRulerDecoration, OverviewRulerDecorationCollection,
    OverviewRulerLane, RangeDecoration, RangeDecorationCollection, RopeExt as _, TabSize,
};
use gpui_kit::*;
use lsp_types::DiagnosticSeverity;

use crate::git::LineChange;

use crate::language::LanguageInfo;
use crate::lsp::LspClient;
use crate::lsp::provider::CodeIntel;
use crate::settings::Settings;

#[derive(Clone, Copy, PartialEq)]
pub enum Preview {
    Off,
    Side,
    Only,
}

pub struct Buffer {
    pub path: PathBuf,
    /// Markdown rendering next to or instead of the source.
    pub preview: Preview,
    pub intel: Rc<CodeIntel>,
    pub editor: Entity<EditorState>,
    pub lang: LanguageInfo,
    /// The file's own indentation, detected when it was opened.
    pub indent: TabSize,
    pub dirty: bool,
    /// The file on disk when last read or written: a sync only rereads files whose stamp moved.
    pub disk: Option<(std::time::SystemTime, u64)>,
    pub lsp: Option<Arc<LspClient>>,
    // Later collections paint over earlier ones: git < diagnostics < breakpoints.
    git_marks: GutterDecorationCollection,
    diagnostic_marks: GutterDecorationCollection,
    breakpoint_marks: GutterDecorationCollection,
    ruler: OverviewRulerDecorationCollection,
    debug_line: RangeDecorationCollection,
    conflict_marks: RangeDecorationCollection,
    /// The AI fix in progress or proposed (see [`super::ai_fix`]).
    pub ai_fix: Option<super::ai_fix::AiFix>,
    fix_marks: RangeDecorationCollection,
    /// What a chat turn changed here, waiting for Keep / Undo: lines added (green), deletions (red).
    review_marks: RangeDecorationCollection,
    review_gutter: GutterDecorationCollection,
    /// This file's problems, read by the quick fixes (⌘.).
    problems: super::code_actions::Problems,
    /// Merge conflict blocks in the text, kept current on every edit.
    pub conflicts: Vec<super::editing::Conflict>,
}

/// The indentation a file already uses, like VS Code's "detect indentation": tabs when most
/// indented lines start with one, otherwise the most common step between consecutive indent
/// levels. `None` when there isn't enough indentation to tell.
pub fn detect_indent(text: &str) -> Option<TabSize> {
    let (mut tabs, mut spaces) = (0, 0);
    let mut steps = [0usize; 9];
    let mut previous = 0;
    for line in text.lines().take(10_000).filter(|l| !l.trim().is_empty()) {
        let width = line.len() - line.trim_start_matches(' ').len();
        if line.starts_with('\t') {
            tabs += 1;
        } else if width > 0 {
            spaces += 1;
        }
        if !line.starts_with('\t') {
            let step = width.abs_diff(previous);
            if (2..=8).contains(&step) {
                steps[step] += 1;
            }
            previous = width;
        }
    }
    if tabs > spaces {
        return Some(TabSize { tab_size: 4, hard_tabs: true });
    }
    let (best, count) = steps.iter().enumerate().max_by_key(|&(step, n)| (*n, std::cmp::Reverse(step)))?;
    (*count > 0).then_some(TabSize { tab_size: best, hard_tabs: false })
}

/// Modification time and size of `path`, to notice changes without reading the file.
// shortcut: a rewrite with the same size inside the filesystem's time granularity goes unseen; hash the file if that bites.
pub fn disk_stamp(path: &std::path::Path) -> Option<(std::time::SystemTime, u64)> {
    std::fs::metadata(path).ok().and_then(|m| Some((m.modified().ok()?, m.len())))
}

impl Buffer {
    pub fn new(path: PathBuf, text: String, lang: LanguageInfo, window: &mut Window, cx: &mut App) -> Self {
        let s = Settings::get(cx).clone();
        let indent = detect_indent(&text).unwrap_or(TabSize { tab_size: s.tab_size, hard_tabs: false });
        let editor = cx.new(|cx| {
            let mut e = EditorState::new(window, cx)
                .language(lang.highlight.clone())
                .tab_size(indent.clone())
                .soft_wrap(s.soft_wrap)
                .default_value(text);
            if lang.highlight == "sql" {
                e.set_highlighter_factory(super::sql::factory(), cx);
            }
            e
        });
        let (git_marks, diagnostic_marks, breakpoint_marks, ruler, debug_line, conflict_marks, fix_marks) = editor.update(cx, |e, cx| {
            (
                e.create_gutter_decorations_collection(vec![], cx),
                e.create_gutter_decorations_collection(vec![], cx),
                e.create_gutter_decorations_collection(vec![], cx),
                e.create_overview_ruler_decorations_collection(vec![], cx),
                e.create_range_decorations_collection(vec![], cx),
                e.create_range_decorations_collection(vec![], cx),
                e.create_range_decorations_collection(vec![], cx),
            )
        });
        let (review_marks, review_gutter) = editor.update(cx, |e, cx| (e.create_range_decorations_collection(vec![], cx), e.create_gutter_decorations_collection(vec![], cx)));
        let intel = CodeIntel::new(path.clone(), lang.lsp_id.clone(), None);
        let disk = disk_stamp(&path);
        let mut this = Self { path, preview: Preview::Off, intel, editor, lang, indent, dirty: false, disk, lsp: None, git_marks, diagnostic_marks, breakpoint_marks, ruler, debug_line, conflict_marks, conflicts: vec![], problems: Default::default(), ai_fix: None, fix_marks, review_marks, review_gutter };
        this.install_providers(cx);
        this.refresh_conflicts(cx);
        this
    }

    pub fn is_markdown(&self) -> bool {
        self.lang.highlight == "markdown"
    }

    /// Text before the cursor.
    pub fn prefix(&self, cx: &App) -> String {
        let e = self.editor.read(cx);
        let text = e.value();
        text[..e.cursor().min(text.len())].to_string()
    }

    pub fn name(&self) -> String {
        self.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
    }

    pub fn text(&self, cx: &App) -> String {
        self.editor.read(cx).value().to_string()
    }

    /// (Re)install completion/hover: Copilot always, LSP once attached.
    fn install_providers(&mut self, cx: &mut App) {
        let intel = CodeIntel::new(self.path.clone(), self.lang.lsp_id.clone(), self.lsp.clone());
        self.intel = intel.clone();
        self.editor.update(cx, |e, cx| {
            e.lsp_mut().completion_provider = Some(intel.clone());
            e.lsp_mut().completion_menu.max_width = px(520.);
            e.lsp_mut().set_completion_highlighter(Rc::new(crate::lsp::provider::highlight_match));
            e.lsp_mut().hover_provider = Some(intel.clone());
            e.lsp_mut().definition_provider = Some(intel);
            e.lsp_mut().code_action_providers =
                vec![super::code_actions::provider(self.path.clone(), self.lang.lsp_id.clone(), self.lsp.clone(), self.problems.clone())];
            // Definitions in other files open in the workspace (same file: the editor jumps itself).
            let own = self.path.clone();
            e.lsp_mut().show_document = Some(Rc::new(move |params, window, cx| {
                let Some(path) = crate::lsp::client::path_from_uri(params.uri.as_str()).filter(|p| *p != own) else { return false };
                let line = params.selection.map(|r| r.start.line + 1);
                super::open_in_workspace(path, line, window, cx);
                true
            }));
            cx.notify();
        });
    }

    /// Forget the language server (it stopped, or belongs to another folder).
    pub fn detach_lsp(&mut self, cx: &mut App) {
        self.lsp = None;
        self.install_providers(cx);
    }

    pub fn attach_lsp(&mut self, client: Arc<LspClient>, cx: &mut App) {
        client.did_open(&self.path, &self.lang.lsp_id, &self.text(cx));
        self.lsp = Some(client);
        self.install_providers(cx);
    }

    /// Squiggles + hover (editor), a gutter mark per line and a mark on the scrollbar ruler.
    pub fn set_diagnostics(&self, diagnostics: &[lsp_types::Diagnostic], cx: &mut App) {
        *self.problems.lock().unwrap() = diagnostics.to_vec();
        let (red, yellow, blue) = (cx.theme().red, cx.theme().yellow, cx.theme().blue);
        let color = |d: &lsp_types::Diagnostic| match d.severity {
            Some(DiagnosticSeverity::ERROR) | None => red,
            Some(DiagnosticSeverity::WARNING) => yellow,
            _ => blue,
        };
        // Errors last so they paint over warnings on the same line.
        let mut sorted: Vec<&lsp_types::Diagnostic> = diagnostics.iter().collect();
        sorted.sort_by_key(|d| std::cmp::Reverse(d.severity.map_or(1, |s| if s == DiagnosticSeverity::ERROR { 1 } else { 2 })));
        let gutter = sorted.iter().map(|d| GutterDecoration::new(self.line_range(d.range.start.line + 1, cx)).with_color(color(d).opacity(0.8))).collect();
        let ruler = sorted
            .iter()
            .map(|d| OverviewRulerDecoration::new(self.line_range(d.range.start.line + 1, cx)).with_lane(OverviewRulerLane::Right).with_color(color(d)))
            .collect();
        self.diagnostic_marks.set(gutter, cx);
        self.ruler.set(ruler, cx);
        self.editor.update(cx, |e, cx| {
            if let Some(set) = e.diagnostics_mut() {
                set.clear();
                set.extend(diagnostics.iter().cloned());
            }
            cx.notify();
        });
    }

    /// Lines changed since HEAD, as colored gutter bars.
    pub fn set_git_changes(&self, changes: &[(u32, u32, LineChange)], cx: &mut App) {
        let (green, blue, red) = (cx.theme().green, cx.theme().blue, cx.theme().red);
        let marks = changes
            .iter()
            .map(|(start, end, kind)| {
                let range = self.line_range(*start, cx).start..self.line_range(*end, cx).end;
                let color = match kind {
                    LineChange::Added => green,
                    LineChange::Modified => blue,
                    LineChange::Deleted => red,
                };
                GutterDecoration::new(range).with_color(color.opacity(0.7))
            })
            .collect();
        self.git_marks.set(marks, cx);
    }

    /// 1-based line under a click in the line-number gutter, if the click is there.
    // shortcut: assumes no folds or soft wrap above the click and estimates the gutter width;
    // ask gpui-component for a gutter click event if it ever exposes one.
    pub fn gutter_line_at(&self, position: Point<Pixels>, cx: &App) -> Option<u32> {
        let e = self.editor.read(cx);
        let bounds = e.text_bounds()?;
        let line_height = e.line_height()?;
        let digits = e.text().lines_len().max(1).to_string().len() as f32;
        let gutter = px((digits + 2.) * 0.6 * f32::from(cx.theme().mono_font_size) + 20.);
        let inside = position.x >= e.input_bounds().origin.x && position.x < e.input_bounds().origin.x + gutter;
        let row = f32::from(position.y - bounds.origin.y) / f32::from(line_height);
        (inside && row >= 0. && (row as usize) < e.text().lines_len()).then(|| row as u32 + 1)
    }

    /// Byte range of a 1-based line.
    fn line_range(&self, line: u32, cx: &App) -> std::ops::Range<usize> {
        let text = self.editor.read(cx).text();
        let row = (line.max(1) as usize - 1).min(text.lines_len().saturating_sub(1));
        text.line_start_offset(row)..text.line_end_offset(row)
    }

    pub fn show_breakpoints(&self, lines: Option<&BTreeSet<u32>>, cx: &mut App) {
        let red = gpui_kit::red();
        let marks = lines
            .into_iter()
            .flatten()
            .map(|l| GutterDecoration::new(self.line_range(*l, cx)).with_color(red))
            .collect();
        self.breakpoint_marks.set(marks, cx);
    }

    /// Find merge conflicts and color them like VS Code: current in green, incoming in blue.
    pub fn refresh_conflicts(&mut self, cx: &mut App) {
        let text = self.editor.read(cx).text().to_string();
        // Cheap check first: most files never contain a marker.
        let conflicts = if text.contains("<<<<<<<") { super::editing::conflicts(&text) } else { vec![] };
        if conflicts.is_empty() && self.conflicts.is_empty() {
            return;
        }
        let (green, blue, gray) = (gpui_kit::green().opacity(0.18), gpui_kit::blue().opacity(0.18), gpui_kit::black().opacity(0.12));
        let marks = conflicts
            .iter()
            .flat_map(|c| {
                [
                    RangeDecoration::new(c.whole.start..c.ours.start).with_color(gray),
                    RangeDecoration::new(c.ours.clone()).with_color(green),
                    RangeDecoration::new(c.ours.end..c.theirs.start).with_color(gray),
                    RangeDecoration::new(c.theirs.clone()).with_color(blue),
                    RangeDecoration::new(c.theirs.end..c.whole.end).with_color(gray),
                ]
            })
            .collect();
        self.conflict_marks.set(marks, cx);
        self.conflicts = conflicts;
    }

    /// Highlight the code an AI fix put in (green, like an accepted diff), or clear it.
    pub fn show_fix_marks(&self, range: Option<std::ops::Range<usize>>, cx: &mut App) {
        let marks = range.map(|r| RangeDecoration::new(r).with_style(gpui_kit::component::input::RangeDecorationStyle::Fill).with_color(gpui_kit::green().opacity(0.18))).into_iter().collect();
        self.fix_marks.set(marks, cx);
    }

    /// Mark what changed since `before` (the file before a chat turn; None = it was created):
    /// added or changed lines in green, a red bar where lines were deleted. None clears.
    pub fn show_review(&self, before: Option<&Option<Vec<u8>>>, cx: &mut App) {
        let (mut fills, mut bars) = (vec![], vec![]);
        if let Some(before) = before {
            let old = String::from_utf8_lossy(before.as_deref().unwrap_or_default()).to_string();
            let new = self.text(cx);
            // The new-text line each change sits at: deletions mark the line that follows them.
            let mut line = 0u32;
            for change in similar::TextDiff::from_lines(&old, &new).iter_all_changes() {
                match change.tag() {
                    similar::ChangeTag::Insert => {
                        let fill = gpui_kit::green().opacity(0.16);
                        fills.push(RangeDecoration::new(self.line_range(line + 1, cx)).with_style(gpui_kit::component::input::RangeDecorationStyle::Fill).with_color(fill));
                        line += 1;
                    }
                    similar::ChangeTag::Delete => bars.push(GutterDecoration::new(self.line_range(line + 1, cx)).with_color(gpui_kit::red())),
                    similar::ChangeTag::Equal => line += 1,
                }
            }
        }
        self.review_marks.set(fills, cx);
        self.review_gutter.set(bars, cx);
    }

    pub fn show_debug_line(&self, line: Option<u32>, cx: &mut App) {
        let marks = line
            .map(|l| RangeDecoration::new(self.line_range(l, cx)).with_color(gpui_kit::yellow().opacity(0.25)))
            .into_iter()
            .collect();
        self.debug_line.set(marks, cx);
    }

    /// 1-based cursor line.
    pub fn cursor_line(&self, cx: &App) -> u32 {
        self.editor.read(cx).cursor_position().line + 1
    }

    pub fn go_to_line(&self, line: u32, window: &mut Window, cx: &mut App) {
        self.editor.update(cx, |e, cx| {
            e.set_cursor_position(lsp_types::Position::new(line.saturating_sub(1), 0), window, cx);
            e.focus(window, cx);
        });
    }

    pub fn reload(&mut self, window: &mut Window, cx: &mut App) {
        if let Ok(text) = std::fs::read_to_string(&self.path) {
            self.editor.update(cx, |e, cx| e.set_value(text.clone(), window, cx));
            if let Some(lsp) = &self.lsp {
                lsp.sync(&self.path, &text);
            }
            self.dirty = false;
            self.disk = disk_stamp(&self.path);
        }
    }

    /// Ask the language server to format the document (no-op without a server).
    pub fn format(&self, window: &mut Window, cx: &mut App) {
        let Some(lsp) = self.lsp.clone() else { return };
        let (editor, path) = (self.editor.clone(), self.path.clone());
        lsp.sync(&path, &self.text(cx));
        let params = serde_json::json!({
            "textDocument": {"uri": crate::lsp::client::uri(&path)},
            "options": {"tabSize": self.indent.tab_size, "insertSpaces": !self.indent.hard_tabs}
        });
        window
            .spawn(cx, async move |cx| {
                let Ok(value) = lsp.request("textDocument/formatting", params).await else { return };
                let edits: Vec<lsp_types::TextEdit> = serde_json::from_value(value).unwrap_or_default();
                if edits.is_empty() {
                    return;
                }
                let _ = editor.update_in(cx, |e, window, cx| {
                    let formatted = crate::lsp::provider::apply_edits(e.text(), &edits);
                    e.replace_all(formatted, window, cx);
                });
            })
            .detach();
    }

    pub fn save(&mut self, cx: &mut App) -> std::io::Result<()> {
        // Write a sibling temp file then rename: a crash mid-write never truncates the file.
        let tmp = self.path.with_file_name(format!(".{}.forgecode-tmp", self.name()));
        std::fs::write(&tmp, self.text(cx))?;
        if let Ok(meta) = std::fs::metadata(&self.path) {
            let _ = std::fs::set_permissions(&tmp, meta.permissions());
        }
        std::fs::rename(&tmp, &self.path)?;
        self.dirty = false;
        self.disk = disk_stamp(&self.path);
        if let Some(lsp) = &self.lsp {
            lsp.did_save(&self.path);
        }
        Ok(())
    }
}

#[cfg(test)]
mod indent_tests {
    use super::detect_indent;

    #[test]
    fn detects_the_files_indentation() {
        let two = "fn a() {\n  if x {\n    y();\n  }\n}\n";
        assert_eq!(detect_indent(two).map(|t| (t.tab_size, t.hard_tabs)), Some((2, false)));
        let four = "def a():\n    if x:\n        y()\n    return 1\n";
        assert_eq!(detect_indent(four).map(|t| (t.tab_size, t.hard_tabs)), Some((4, false)));
        let tabs = "int main() {\n\tif (x) {\n\t\ty();\n\t}\n}\n";
        assert!(detect_indent(tabs).is_some_and(|t| t.hard_tabs));
        assert!(detect_indent("plain\ntext\n").is_none());
    }
}
