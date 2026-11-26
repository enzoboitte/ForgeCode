//! Find (and replace) in the whole project (⌘⇧F).

use std::path::{Path, PathBuf};

use anyhow::Result;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::*;

use crate::i18n::t;
use crate::icons;

const MAX_MATCHES: usize = 2_000;

#[derive(Clone, Copy, Default, PartialEq)]
pub struct Options {
    pub case: bool,
    pub word: bool,
    pub regex: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Match {
    pub line: u32,
    pub text: String,
}

pub enum SearchEvent {
    Open(PathBuf, u32),
    /// Files rewritten by "replace all".
    Replaced(Vec<PathBuf>),
}

/// The query as a regex honouring the options.
pub fn pattern(query: &str, opts: Options) -> Result<regex::Regex> {
    let body = if opts.regex { query.to_string() } else { regex::escape(query) };
    let body = if opts.word { format!(r"\b(?:{body})\b") } else { body };
    Ok(regex::RegexBuilder::new(&body).case_insensitive(!opts.case).build()?)
}

/// Matches per file under `root` (gitignore respected), capped.
pub fn search(root: &Path, re: &regex::Regex) -> Vec<(PathBuf, Vec<Match>)> {
    let mut out = vec![];
    let mut total = 0;
    for entry in ignore::WalkBuilder::new(root).build().flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else { continue };
        let matches: Vec<Match> = text
            .lines()
            .enumerate()
            .filter(|(_, l)| re.is_match(l))
            .map(|(i, l)| Match { line: i as u32 + 1, text: l.trim().chars().take(200).collect() })
            .collect();
        if !matches.is_empty() {
            total += matches.len();
            out.push((entry.into_path(), matches));
            if total >= MAX_MATCHES {
                break;
            }
        }
    }
    out
}

/// Replace every match in the given files; returns the files that changed.
pub fn replace_all(files: &[PathBuf], re: &regex::Regex, with: &str, literal: bool) -> std::io::Result<Vec<PathBuf>> {
    let mut changed = vec![];
    for path in files {
        let text = std::fs::read_to_string(path)?;
        let new = if literal { re.replace_all(&text, regex::NoExpand(with)) } else { re.replace_all(&text, with) };
        if new != text {
            std::fs::write(path, new.as_ref())?;
            changed.push(path.clone());
        }
    }
    Ok(changed)
}

pub struct SearchPanel {
    root: PathBuf,
    query: Entity<InputState>,
    replace: Entity<InputState>,
    opts: Options,
    results: Vec<(PathBuf, Vec<Match>)>,
    error: Option<String>,
    busy: bool,
    _subs: Vec<Subscription>,
    _task: Option<Task<()>>,
}

impl EventEmitter<SearchEvent> for SearchPanel {}

impl SearchPanel {
    pub fn new(root: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let query = cx.new(|cx| InputState::new(window, cx).placeholder(t("Search")));
        let replace = cx.new(|cx| InputState::new(window, cx).placeholder(t("Replace")));
        let _subs = vec![cx.subscribe(&query, |this, _, e: &InputEvent, cx| {
            if matches!(e, InputEvent::Change | InputEvent::PressEnter { .. }) {
                this.run(cx);
            }
        })];
        Self { root, query, replace, opts: Options::default(), results: vec![], error: None, busy: false, _subs, _task: None }
    }

    pub fn set_root(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        self.root = root;
        self.results.clear();
        cx.notify();
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.query.read(cx).focus_handle(cx).focus(window, cx);
    }

    fn run(&mut self, cx: &mut Context<Self>) {
        let query = self.query.read(cx).value().to_string();
        self.error = None;
        if query.trim().is_empty() {
            self.results.clear();
            return cx.notify();
        }
        let re = match pattern(&query, self.opts) {
            Ok(re) => re,
            Err(e) => {
                self.error = Some(e.to_string());
                return cx.notify();
            }
        };
        let root = self.root.clone();
        self.busy = true;
        // Typing restarts the search: the previous task is dropped (cancelled).
        self._task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(std::time::Duration::from_millis(150)).await;
            let results = cx.background_spawn(async move { search(&root, &re) }).await;
            let _ = this.update(cx, |this, cx| {
                this.results = results;
                this.busy = false;
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn replace_all(&mut self, cx: &mut Context<Self>) {
        let query = self.query.read(cx).value().to_string();
        let Ok(re) = pattern(&query, self.opts) else { return };
        let with = self.replace.read(cx).value().to_string();
        let files: Vec<PathBuf> = self.results.iter().map(|(p, _)| p.clone()).collect();
        match replace_all(&files, &re, &with, !self.opts.regex) {
            Ok(changed) => cx.emit(SearchEvent::Replaced(changed)),
            Err(e) => self.error = Some(e.to_string()),
        }
        self.run(cx);
    }

    fn toggle(&mut self, f: impl FnOnce(&mut Options), cx: &mut Context<Self>) {
        f(&mut self.opts);
        self.run(cx);
    }
}

impl Render for SearchPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let total: usize = self.results.iter().map(|(_, m)| m.len()).sum();
        let opts = self.opts;
        let mut rows: Vec<AnyElement> = vec![];
        for (fi, (path, matches)) in self.results.iter().enumerate() {
            let theme = cx.theme();
            let rel = path.strip_prefix(&self.root).unwrap_or(path).display().to_string();
            let (icon, tint) = icons::file_icon(path);
            let (dir, name) = rel.rsplit_once('/').map_or(("", rel.as_str()), |(d, n)| (d, n));
            rows.push(
                h_flex().px_2().pt_1().gap_1p5().text_sm()
                    .child(div().text_color(tint).child(icon))
                    .child(name.to_string())
                    .child(div().flex_1().truncate().text_xs().text_color(theme.muted_foreground).child(dir.to_string()))
                    .child(div().text_xs().text_color(theme.muted_foreground).child(matches.len().to_string()))
                    .into_any_element(),
            );
            for (mi, m) in matches.iter().enumerate() {
                let (path, line) = (path.clone(), m.line);
                rows.push(
                    h_flex()
                        .id(("match", fi * 10_000 + mi))
                        .pl_7()
                        .pr_2()
                        .gap_2()
                        .text_xs()
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.list_hover))
                        .on_click(cx.listener(move |_, _, _, cx| cx.emit(SearchEvent::Open(path.clone(), line))))
                        .child(div().w(px(32.)).text_color(theme.muted_foreground).child(m.line.to_string()))
                        .child(div().flex_1().truncate().font_family(theme.mono_font_family.clone()).child(m.text.clone()))
                        .into_any_element(),
                );
            }
        }
        let theme = cx.theme();
        let toggle = |id: &'static str, label: &'static str, tip: &'static str, on: bool| Button::new(id).ghost().xsmall().label(label).tooltip(t(tip)).selected(on);
        v_flex()
            .size_full()
            .bg(theme.sidebar)
            .child(h_flex().h(px(36.)).px_3().child(div().text_xs().font_weight(FontWeight::MEDIUM).text_color(theme.muted_foreground).child(t("SEARCH"))))
            .child(
                v_flex()
                    .px_2()
                    .gap_1()
                    .child(h_flex().gap_0p5()
                        .child(div().flex_1().child(Input::new(&self.query).small()))
                        .child(toggle("s-case", "Aa", "Match case", opts.case).on_click(cx.listener(|t, _, _, cx| t.toggle(|o| o.case = !o.case, cx))))
                        .child(toggle("s-word", "ab", "Whole word", opts.word).on_click(cx.listener(|t, _, _, cx| t.toggle(|o| o.word = !o.word, cx))))
                        .child(toggle("s-regex", ".*", "Regular expression", opts.regex).on_click(cx.listener(|t, _, _, cx| t.toggle(|o| o.regex = !o.regex, cx)))))
                    .child(h_flex().gap_0p5()
                        .child(div().flex_1().child(Input::new(&self.replace).small()))
                        .child(Button::new("s-replace").ghost().xsmall().icon(IconName::ReplaceAll).tooltip(t("Replace all")).disabled(self.results.is_empty())
                            .on_click(cx.listener(|t, _, _, cx| t.replace_all(cx)))))
                    .child(div().text_xs().text_color(theme.muted_foreground).child(match (&self.error, self.busy) {
                        (Some(e), _) => e.clone(),
                        (None, true) => "…".into(),
                        (None, false) if total > 0 => format!("{total} {} · {} {}", t("results"), self.results.len(), t("files")),
                        _ => String::new(),
                    })),
            )
            .child(div().flex_1().min_h_0().mt_1().child(v_flex().id("search-results").size_full().overflow_y_scrollbar().children(rows)))
    }
}

#[cfg(test)]
mod tests {
    use super::{Options, pattern, replace_all, search}; // not `*`: gpui exports its own `test` macro

    #[test]
    fn search_options_and_replace() {
        let root = std::env::temp_dir().join(format!("forgecode-search-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.rs"), "let foo = 1;\nlet foobar = Foo;\n").unwrap();
        let count = |q, o| search(&root, &pattern(q, o).unwrap()).iter().map(|(_, m)| m.len()).sum::<usize>();
        assert_eq!(count("foo", Options::default()), 2, "case-insensitive by default");
        assert_eq!(count("foo", Options { word: true, ..Default::default() }), 2, "`Foo` is a whole word too");
        assert_eq!(count("foo", Options { word: true, case: true, ..Default::default() }), 1);
        assert_eq!(count("Foo", Options { case: true, ..Default::default() }), 1);
        assert_eq!(count("f.o", Options { regex: true, ..Default::default() }), 2);
        assert!(pattern("(", Options { regex: true, ..Default::default() }).is_err());
        let files = vec![root.join("a.rs")];
        replace_all(&files, &pattern("foo", Options { word: true, case: true, ..Default::default() }).unwrap(), "$x", true).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "let $x = 1;\nlet foobar = Foo;\n", "literal replacement");
        std::fs::remove_dir_all(root).unwrap();
    }
}
