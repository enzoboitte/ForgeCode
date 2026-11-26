//! Bridges the editor's completion/hover hooks to the language server, the buffer's own words
//! and Copilot.

use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use gpui_kit::component::input::{CompletionProvider, DefinitionProvider, HoverProvider, Rope, RopeExt as _};
use gpui_kit::{App, AppContext as _, Task, Window};
use lsp_types::{
    CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse, CompletionTextEdit, Hover,
    InlineCompletionContext, InlineCompletionResponse, TextEdit,
};
use serde_json::json;

use super::client::{LspClient, uri};
use crate::ai::copilot::{self, CopilotState};
use crate::settings::Settings;

const MAX_ITEMS: usize = 80;

/// Last Copilot suggestion: (text before the cursor when it was made, suggestion).
type SuggestionCache = Arc<Mutex<Option<(String, String)>>>;

/// The server's last answer for the word being typed. Typing more of the same word filters it
/// locally, like VS Code, instead of waiting for the server again.
struct Cached {
    /// Line text before the word, to tell the same word apart from another one.
    before: String,
    query: String,
    items: Vec<CompletionItem>,
    /// False when the server said its list was incomplete (rust-analyzer often does).
    complete: bool,
}

type CompletionCache = Arc<Mutex<Option<Cached>>>;

pub struct CodeIntel {
    pub path: PathBuf,
    pub language: String,
    pub lsp: Option<Arc<LspClient>>,
    suggestion: SuggestionCache,
    completions: CompletionCache,
}

/// What is left of a cached suggestion once the user typed past `cached_prefix` up to `prefix`.
fn remaining<'a>(cached_prefix: &str, suggestion: &'a str, prefix: &str) -> Option<&'a str> {
    let typed = prefix.strip_prefix(cached_prefix)?;
    let rest = suggestion.strip_prefix(typed)?;
    (!rest.trim().is_empty()).then_some(rest)
}

impl CodeIntel {
    pub fn new(path: PathBuf, language: String, lsp: Option<Arc<LspClient>>) -> Rc<Self> {
        Rc::new(Self { path, language, lsp, suggestion: Default::default(), completions: Default::default() })
    }

    /// The next word of the suggestion shown at a cursor preceded by `prefix`.
    pub fn next_suggested_word(&self, prefix: &str) -> Option<String> {
        let cache = self.suggestion.lock().unwrap();
        let (cached_prefix, suggestion) = cache.as_ref()?;
        remaining(cached_prefix, suggestion, prefix).map(|rest| copilot::next_word(rest).to_string())
    }

    /// Pretend Copilot suggested `suggestion` after `prefix`.
    #[cfg(test)]
    pub fn set_suggestion(&self, prefix: &str, suggestion: &str) {
        *self.suggestion.lock().unwrap() = Some((prefix.into(), suggestion.into()));
    }

    /// What is left of the suggestion at a cursor preceded by `prefix` (after accepting words).
    pub fn suggestion_rest(&self, prefix: &str) -> Option<String> {
        let cache = self.suggestion.lock().unwrap();
        let (cached_prefix, suggestion) = cache.as_ref()?;
        remaining(cached_prefix, suggestion, prefix).map(String::from)
    }

    fn position_params(&self, text: &Rope, offset: usize) -> serde_json::Value {
        let pos = text.offset_to_position(offset);
        json!({"textDocument": {"uri": uri(&self.path)}, "position": pos})
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The identifier being typed before `offset`.
fn word_before(text: &str, offset: usize) -> &str {
    let before = &text[..offset];
    let start = before.char_indices().rev().find(|(_, c)| !is_word(*c)).map_or(0, |(i, c)| i + c.len_utf8());
    &before[start..]
}

/// Fuzzy score of `candidate` for `query`: exact prefix > case-insensitive prefix > word-start
/// subsequence > subsequence. `None` when the characters don't appear in order.
pub fn fuzzy_score(query: &str, candidate: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    if candidate.starts_with(query) {
        return Some(1000 - candidate.len() as i32);
    }
    let (q, c) = (query.to_lowercase(), candidate.to_lowercase());
    if c.starts_with(&q) {
        return Some(800 - candidate.len() as i32);
    }
    let mut score = 0;
    let mut chars = c.char_indices().peekable();
    let mut prev = '_';
    for qc in q.chars() {
        loop {
            let (_, cc) = chars.next()?;
            let at_boundary = prev == '_' || (prev.is_lowercase() && cc.is_uppercase());
            prev = cc;
            if cc == qc {
                score += if at_boundary { 20 } else { 5 };
                break;
            }
        }
    }
    Some(score - candidate.len() as i32 / 4)
}

/// Filter and order completions for `query`; the server's own `sort_text` breaks ties.
pub fn rank(query: &str, items: Vec<CompletionItem>) -> Vec<CompletionItem> {
    let mut scored: Vec<(i32, CompletionItem)> = items
        .into_iter()
        .filter_map(|item| {
            let key = item.filter_text.clone().unwrap_or_else(|| item.label.clone());
            fuzzy_score(query, &key).map(|s| (s, item))
        })
        .collect();
    scored.sort_by(|(a, x), (b, y)| b.cmp(a).then_with(|| x.sort_text.cmp(&y.sort_text)));
    scored.into_iter().take(MAX_ITEMS).map(|(_, i)| i).collect()
}

/// Symbol shown before a completion label, standing in for VS Code's kind icons.
fn kind_name(kind: CompletionItemKind) -> String {
    format!("{kind:?}").to_lowercase().replace('_', " ")
}

/// Highlight the fuzzy match in a label.
pub fn highlight_match(query: &str, item: &CompletionItem) -> Vec<std::ops::Range<usize>> {
    let bare = CompletionItem { label: item.label.clone(), ..Default::default() };
    gpui_kit::component::input::highlight_characters(query, &bare)
}

/// Shape a ranked item for the menu: kind glyph and signature in the label, a detail on the right,
/// and an edit that replaces the whole typed word (`word` is its LSP range).
/// Servers like rust-analyzer and typescript-language-server only send a completion's import
/// (`additionalTextEdits`) when the item is resolved: resolve the best few candidates.
async fn resolve_imports(lsp: &LspClient, query: &str, mut items: Vec<CompletionItem>) -> Vec<CompletionItem> {
    let best: Vec<usize> = {
        let ranked = rank(query, items.clone());
        ranked
            .iter()
            .take(8)
            .filter_map(|r| items.iter().position(|i| i.label == r.label && i.sort_text == r.sort_text))
            .filter(|&ix| items[ix].additional_text_edits.is_none() && items[ix].data.is_some())
            .collect()
    };
    // Never hold the menu back for long: a slow server keeps what it already sent.
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(300);
    for ix in best {
        if std::time::Instant::now() > deadline {
            break;
        }
        let Ok(params) = serde_json::to_value(&items[ix]) else { continue };
        let resolve = lsp.request("completionItem/resolve", params);
        let timeout = async {
            smol::Timer::after(std::time::Duration::from_millis(250)).await;
            Err(anyhow::anyhow!("timeout"))
        };
        if let Ok(value) = smol::future::or(resolve, timeout).await {
            if let Ok(resolved) = serde_json::from_value::<CompletionItem>(value) {
                items[ix].additional_text_edits = resolved.additional_text_edits;
                if items[ix].documentation.is_none() {
                    items[ix].documentation = resolved.documentation;
                }
            }
        }
    }
    items
}

/// Completed right before `<` or `(` (`std::unordered_map|<int, int>`, `foo|(x)`): the arguments
/// are already written, so the completion's own `<${1:class Key}, …>` / `(${1:x})` is dropped.
fn without_arguments(mut item: CompletionItem, next: Option<char>) -> CompletionItem {
    let Some(open) = next.filter(|c| matches!(c, '<' | '(')) else { return item };
    let text = match item.text_edit.as_mut() {
        Some(CompletionTextEdit::Edit(e)) => &mut e.new_text,
        Some(CompletionTextEdit::InsertAndReplace(e)) => &mut e.new_text,
        None => match item.insert_text.as_mut() {
            Some(t) => t,
            None => return item,
        },
    };
    if let Some(at) = text.find(open).filter(|&at| at > 0) {
        text.truncate(at);
        if !text.contains('$') {
            item.insert_text_format = Some(lsp_types::InsertTextFormat::PLAIN_TEXT);
        }
    }
    item
}

/// Fold a completion's import edits (above it, as servers put them) into its own edit, so the
/// editor applies both in one step (one undo) and the cursor still lands after the name.
pub fn merge_imports(mut item: CompletionItem, text: &Rope) -> CompletionItem {
    let Some(adds) = item.additional_text_edits.take().filter(|a| !a.is_empty()) else { return item };
    let (main_range, main_text) = match &item.text_edit {
        Some(CompletionTextEdit::Edit(e)) => (e.range, e.new_text.clone()),
        Some(CompletionTextEdit::InsertAndReplace(e)) => (e.replace, e.new_text.clone()),
        None => {
            item.additional_text_edits = Some(adds);
            return item;
        }
    };
    let full = text.to_string();
    let main_start = text.position_to_offset(&main_range.start);
    let mut spans: Vec<(usize, usize, &str)> =
        adds.iter().map(|e| (text.position_to_offset(&e.range.start), text.position_to_offset(&e.range.end), e.new_text.as_str())).collect();
    spans.sort_by_key(|s| s.0);
    let ordered = spans.windows(2).all(|w| w[0].1 <= w[1].0) && spans.last().is_some_and(|s| s.1 <= main_start);
    if !ordered || spans.iter().any(|s| !full.is_char_boundary(s.0) || !full.is_char_boundary(s.1)) {
        item.additional_text_edits = Some(adds);
        return item;
    }
    // In a snippet, the file's own text and the imports are literal.
    let snippet = item.insert_text_format == Some(lsp_types::InsertTextFormat::SNIPPET);
    let lit = |t: &str| if snippet { t.replace('\\', "\\\\").replace('$', "\\$").replace('}', "\\}") } else { t.to_string() };
    let mut new_text = String::new();
    let mut at = spans[0].0;
    for (start, end, insert) in &spans {
        new_text.push_str(&lit(&full[at..*start]));
        new_text.push_str(&lit(insert));
        at = *end;
    }
    new_text.push_str(&lit(&full[at..main_start]));
    new_text.push_str(&main_text);
    let range = lsp_types::Range::new(text.offset_to_position(spans[0].0), main_range.end);
    item.text_edit = Some(CompletionTextEdit::Edit(TextEdit { range, new_text }));
    item
}

fn present(mut item: CompletionItem, word: lsp_types::Range) -> CompletionItem {
    // clangd pads its labels with a leading space.
    let label = std::mem::take(&mut item.label).trim().to_string();
    let details = item.label_details.take().unwrap_or_default();
    item.detail = item.detail.take().or(details.description).or_else(|| item.kind.map(kind_name)).map(|d| d.lines().next().unwrap_or("").to_string());
    match item.text_edit.as_mut() {
        // Served from the cache: the server's edit ended where the cursor was back then.
        Some(CompletionTextEdit::Edit(e)) => e.range.end = e.range.end.max(word.end),
        Some(CompletionTextEdit::InsertAndReplace(e)) => {
            e.replace.end = e.replace.end.max(word.end);
            e.insert.end = e.insert.end.max(word.end);
        }
        None => {
            let new_text = item.insert_text.take().unwrap_or_else(|| label.clone());
            item.text_edit = Some(CompletionTextEdit::Edit(TextEdit { range: word, new_text }));
        }
    }
    item.filter_text.get_or_insert_with(|| label.clone());
    // The menu draws the kind as a colored icon.
    item.label = format!("{label}{}", details.detail.unwrap_or_default());
    item
}

/// Identifiers from the document, as plain-text completions.
/// `current` is the word under the cursor: offering it back would replace it with itself.
fn buffer_words(text: &str, query: &str, current: &str) -> Vec<CompletionItem> {
    let mut seen = HashSet::new();
    text.split(|c: char| !is_word(c))
        .filter(|w| w.len() > 2 && *w != query && *w != current && !w.chars().next().unwrap().is_ascii_digit() && seen.insert(*w))
        .map(|w| CompletionItem { label: w.to_string(), kind: Some(CompletionItemKind::TEXT), sort_text: Some(format!("~{w}")), ..Default::default() })
        .collect()
}

impl CompletionProvider for CodeIntel {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        _trigger: CompletionContext,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<CompletionResponse>> {
        let full: Arc<str> = text.to_string().into();
        let offset = offset.min(full.len());
        let query = word_before(&full, offset).to_string();
        let word_start = offset - query.len();
        // The rest of the identifier after the cursor is replaced too, not pushed after the completion.
        let word_end = offset + full[offset..].find(|c: char| !is_word(c)).unwrap_or(full.len() - offset);
        let word = lsp_types::Range::new(text.offset_to_position(word_start), text.offset_to_position(word_end));
        let line_start = full[..word_start].rfind('\n').map_or(0, |i| i + 1);
        let before = full[line_start..word_start].to_string();
        let (ranked_for, doc, rope) = (query.clone(), full.clone(), text.clone());
        let current = full[word_start..word_end].to_string();
        let next = full[word_end..].chars().next();
        let finish = move |mut items: Vec<CompletionItem>| {
            // Auto-import: the import a completion needs goes in with it.
            let mut items: Vec<CompletionItem> = items.drain(..).map(|i| merge_imports(without_arguments(i, next), &rope)).collect();
            // Buffer words only fill in when the server has little to say (scanning a big file isn't free).
            if items.len() < 5 {
                let known: HashSet<String> = items.iter().map(|i| i.label.clone()).collect();
                items.extend(buffer_words(&doc, &ranked_for, &current).into_iter().filter(|w| !known.contains(&w.label)));
            }
            CompletionResponse::Array(rank(&ranked_for, items).into_iter().map(|i| present(i, word)).collect())
        };
        let Some(lsp) = self.lsp.clone() else {
            return Task::ready(Ok(finish(vec![])));
        };
        // Like VS Code: while the same word is typed, answer at once from the last server reply
        // filtered locally; when that reply was incomplete, refresh it in the background for the
        // next keystroke instead of making this one wait.
        let cached = self.completions.lock().unwrap().as_ref().and_then(|c| {
            // A truncated list asked without any prefix only holds what's in scope: once a letter
            // is typed, the server also knows what could be imported (classes, functions of
            // libraries and of other project files), so ask it again rather than filter that list.
            let usable = c.complete || !c.query.is_empty();
            (usable && c.before == before && query.len() > c.query.len() && query.starts_with(c.query.as_str())).then(|| (c.items.clone(), c.complete))
        });
        let params = self.position_params(text, offset);
        let request = {
            let (lsp, path, full, cache) = (lsp.clone(), self.path.clone(), full.clone(), self.completions.clone());
            let (before, query) = (before.clone(), query.clone());
            async move {
                // The edit that triggered us may not have been synced yet.
                lsp.sync(&path, &full);
                let value = lsp.request("textDocument/completion", params).await.unwrap_or_default();
                let (items, complete) = match serde_json::from_value::<CompletionResponse>(value) {
                    Ok(CompletionResponse::Array(items)) => (items, true),
                    Ok(CompletionResponse::List(list)) => (list.items, !list.is_incomplete),
                    Err(_) => return vec![],
                };
                let items = resolve_imports(&lsp, &query, items).await;
                // An empty answer (the server was still reading the file) isn't worth keeping:
                // it would hide what the server finds a moment later.
                if !items.is_empty() {
                    *cache.lock().unwrap() = Some(Cached { before, query, items: items.clone(), complete });
                }
                items
            }
        };
        if let Some((items, complete)) = cached {
            let response = finish(items);
            let empty = matches!(&response, CompletionResponse::Array(a) if a.is_empty());
            if complete || !empty {
                if !complete {
                    cx.background_spawn(request).detach();
                }
                return Task::ready(Ok(response));
            }
            // Nothing left in a truncated list: the server may know more, wait for it.
        }
        cx.background_spawn(async move { Ok(finish(request.await)) })
    }

    fn inline_completion(&self, _: &Rope, _: usize, _: InlineCompletionContext, _: &mut Window, _: &mut App) -> Task<Result<InlineCompletionResponse>> {
        // Suggestions are streamed (below).
        Task::ready(Ok(InlineCompletionResponse::Array(vec![])))
    }

    fn inline_completion_stream(&self, text: &Rope, offset: usize, _window: &mut Window, cx: &mut App) -> Option<futures::channel::mpsc::UnboundedReceiver<String>> {
        let settings = Settings::get(cx);
        let accounts = copilot::supporting(&CopilotState::candidates(cx), &settings.completion_model);
        if !settings.copilot_inline || accounts.is_empty() {
            return None;
        }
        let full = text.to_string();
        let offset = offset.min(full.len());
        // Mid-identifier suggestions are mostly noise.
        if full[offset..].chars().next().is_some_and(is_word) {
            return None;
        }
        let (prefix, suffix) = (full[..offset].to_string(), full[offset..].to_string());
        let (tx, rx) = futures::channel::mpsc::unbounded();
        // Typing (or accepting word by word) through the suggestion keeps the rest, without a new request.
        let cached = self.suggestion.lock().unwrap().as_ref().and_then(|(p, s)| remaining(p, s, &prefix).map(String::from));
        if let Some(rest) = cached {
            let _ = tx.unbounded_send(rest);
            return Some(rx);
        }
        let (language, path) = (self.language.clone(), self.path.display().to_string());
        let model = settings.completion_model.clone();
        let cache = self.suggestion.clone();
        cx.background_spawn(async move {
            let shown = tx.clone();
            let result = copilot::inline_completion(&accounts, &path, &language, &prefix, &suffix, &model, |so_far| {
                let _ = shown.unbounded_send(so_far.to_string());
            });
            if let Ok(insert) = result {
                let _ = tx.unbounded_send(insert.clone().unwrap_or_default());
                *cache.lock().unwrap() = insert.map(|s| (prefix, s));
            }
        })
        .detach();
        Some(rx)
    }

    fn inline_completion_debounce(&self) -> std::time::Duration {
        // The code-completion model answers in a few hundred ms: ask soon after typing stops.
        std::time::Duration::from_millis(120)
    }

    fn is_completion_trigger(&self, _offset: usize, new_text: &str, _cx: &mut App) -> bool {
        new_text.chars().last().is_some_and(|c| is_word(c) || (self.lsp.is_some() && matches!(c, '.' | ':' | '>' | '<' | '/' | '"')))
    }
}

impl DefinitionProvider for CodeIntel {
    fn definitions(&self, text: &Rope, offset: usize, _window: &mut Window, cx: &mut App) -> Task<Result<Vec<lsp_types::LocationLink>>> {
        let Some(lsp) = self.lsp.clone() else {
            return Task::ready(Ok(vec![]));
        };
        let params = self.position_params(text, offset);
        cx.background_spawn(async move {
            let value = lsp.request("textDocument/definition", params).await?;
            Ok(to_links(serde_json::from_value(value).ok()))
        })
    }
}

/// Servers answer a location, a list of locations or a list of links: normalize to links.
pub fn to_links(response: Option<lsp_types::GotoDefinitionResponse>) -> Vec<lsp_types::LocationLink> {
    use lsp_types::{GotoDefinitionResponse as R, LocationLink};
    let from = |l: lsp_types::Location| LocationLink { origin_selection_range: None, target_uri: l.uri, target_range: l.range, target_selection_range: l.range };
    match response {
        Some(R::Scalar(l)) => vec![from(l)],
        Some(R::Array(ls)) => ls.into_iter().map(from).collect(),
        Some(R::Link(links)) => links,
        None => vec![],
    }
}

/// Apply LSP text edits to `text` (positions as the editor counts them).
pub fn apply_edits(text: &Rope, edits: &[lsp_types::TextEdit]) -> String {
    let mut ranges: Vec<(usize, usize, &str)> = edits
        .iter()
        .map(|e| (text.position_to_offset(&e.range.start), text.position_to_offset(&e.range.end), e.new_text.as_str()))
        .collect();
    // Apply from the end so earlier offsets stay valid.
    ranges.sort_by(|a, b| b.0.cmp(&a.0));
    let mut out = text.to_string();
    for (start, end, new) in ranges {
        if start <= end && end <= out.len() && out.is_char_boundary(start) && out.is_char_boundary(end) {
            out.replace_range(start..end, new);
        }
    }
    out
}

impl HoverProvider for CodeIntel {
    fn hover(&self, text: &Rope, offset: usize, _window: &mut Window, cx: &mut App) -> Task<Result<Option<Hover>>> {
        let Some(lsp) = self.lsp.clone() else {
            return Task::ready(Ok(None));
        };
        let params = self.position_params(text, offset);
        cx.background_spawn(async move {
            let value = lsp.request("textDocument/hover", params).await?;
            Ok(serde_json::from_value(value).ok())
        })
    }
}

#[cfg(test)]
mod import_tests {
    use super::*;

    #[test]
    fn folds_the_import_into_the_completion() {
        let text = Rope::from("x = 1\ncompute_to");
        let item: CompletionItem = serde_json::from_value(json!({
            "label": "compute_total",
            "textEdit": {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 10}}, "newText": "compute_total"},
            "additionalTextEdits": [{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "newText": "from helpers import compute_total\n\n\n"}]
        })).unwrap();
        let merged = merge_imports(item, &text);
        let Some(CompletionTextEdit::Edit(e)) = merged.text_edit else { panic!() };
        let applied = apply_edits(&text, &[e]);
        assert_eq!(applied, "from helpers import compute_total\n\n\nx = 1\ncompute_total");
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn arguments_already_written_are_not_inserted_again() {
        use lsp_types::{CompletionItem, CompletionTextEdit, InsertTextFormat, Position, Range, TextEdit};
        let item = |t: &str| CompletionItem {
            text_edit: Some(CompletionTextEdit::Edit(TextEdit { range: Range::new(Position::new(0, 5), Position::new(0, 18)), new_text: t.into() })),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            ..Default::default()
        };
        let text = |i: CompletionItem| match i.text_edit { Some(CompletionTextEdit::Edit(e)) => (e.new_text, i.insert_text_format), _ => unreachable!() };
        let t = "unordered_map<${1:class Key}, ${2:class Tp}>";
        assert_eq!(text(super::without_arguments(item(t), Some('<'))), ("unordered_map".into(), Some(InsertTextFormat::PLAIN_TEXT)));
        assert_eq!(text(super::without_arguments(item("foo(${1:int x})"), Some('('))).0, "foo");
        assert_eq!(text(super::without_arguments(item(t), Some(' '))).0, t);
    }

    use super::*;

    fn item(label: &str) -> CompletionItem {
        CompletionItem { label: label.into(), ..Default::default() }
    }

    #[test]
    fn ranks_prefix_then_fuzzy_and_drops_non_matches() {
        let ranked = rank("prl", vec![item("other"), item("print_line"), item("println"), item("PrlBuffer")]);
        let labels: Vec<_> = ranked.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels[0], "PrlBuffer");
        assert!(labels.contains(&"println") && labels.contains(&"print_line"));
        assert!(!labels.contains(&"other"));
    }

    #[test]
    fn applies_formatting_edits_from_the_end() {
        use lsp_types::{Position, Range, TextEdit};
        let text = Rope::from("fn  main(){\nx}\n");
        let edit = |l1, c1, l2, c2, t: &str| TextEdit { range: Range::new(Position::new(l1, c1), Position::new(l2, c2)), new_text: t.into() };
        let out = apply_edits(&text, &[edit(0, 2, 0, 4, " "), edit(0, 10, 0, 10, " "), edit(1, 0, 1, 0, "    ")]);
        assert_eq!(out, "fn main() {\n    x}\n");
    }

    #[test]
    fn cached_suggestion_follows_typing() {
        assert_eq!(remaining("let x", " = foo();", "let x = f"), Some("oo();"));
        assert_eq!(remaining("let x", " = foo();", "let y"), None);
        assert_eq!(remaining("let x", " = foo();", "let x = foo();"), None);
    }

    #[test]
    fn finds_word_before_cursor() {
        assert_eq!(word_before("let foo_ba", 10), "foo_ba");
        assert_eq!(word_before("x.é", 4), "é");
    }

    /// The provider itself, as the editor calls it: buffer words complete without a language server.
    #[gpui_kit::test]
    fn completes_buffer_words_without_a_server(cx: &mut gpui_kit::TestAppContext) {
        let provider = CodeIntel::new(PathBuf::from("/tmp/x.txt"), "text".into(), None);
        let text = Rope::from("let hello_world = 1;\nhel");
        let cx = cx.add_empty_window();
        let task = cx.update(|window, cx| {
            assert!(provider.is_completion_trigger(0, "l", cx));
            let trigger = CompletionContext { trigger_kind: lsp_types::CompletionTriggerKind::INVOKED, trigger_character: None };
            provider.completions(&text, text.len(), trigger, window, cx)
        });
        let items = match cx.foreground_executor().block_test(task).unwrap() {
            CompletionResponse::Array(items) => items,
            CompletionResponse::List(l) => l.items,
        };
        assert_eq!(items.first().and_then(|i| i.filter_text.as_deref()), Some("hello_world"));
    }

    #[test]
    fn presents_items_like_vscode() {
        use lsp_types::{CompletionItemLabelDetails, Position, Range};
        let word = Range::new(Position::new(0, 4), Position::new(0, 7));
        let item = CompletionItem {
            label: "push".into(),
            kind: Some(CompletionItemKind::METHOD),
            label_details: Some(CompletionItemLabelDetails { detail: Some("(value)".into()), description: None }),
            ..Default::default()
        };
        let shown = present(item, word);
        assert_eq!(shown.label, "push(value)", "the kind is an icon now");
        assert_eq!(shown.detail.as_deref(), Some("method"));
        let Some(CompletionTextEdit::Edit(edit)) = &shown.text_edit else { panic!("always an edit") };
        assert_eq!((edit.range, edit.new_text.as_str()), (word, "push"), "replaces the typed word");
        assert_eq!(highlight_match("ph", &shown), vec![0..1, 3..4]);
    }
}
