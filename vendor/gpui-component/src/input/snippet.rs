//! ForgeCode patch: LSP snippets (`insertTextFormat: 2`), like VS Code. A completion such as
//! `box-sizing: $0;` or `fn ${1:name}(${2}) {$0}` is inserted as plain text, with the cursor on
//! (or the placeholder selected at) the first tab stop; Tab / Shift+Tab then move between the
//! stops, and the final one (`$0`) ends the snippet.

use std::collections::HashMap;
use std::ops::Range;

use gpui::{App, Context, Entity, EntityId, Global, Window, hsla};

use super::{EditorState, RangeDecoration, RangeDecorationCollection, RangeDecorationStyle};

/// A snippet as plain text and its tab stops (byte ranges in that text), in visiting order:
/// `$1`, `$2`… then `$0` (the end of the text when the snippet has none).
#[derive(Debug, PartialEq)]
pub struct Parsed {
    pub text: String,
    pub stops: Vec<Range<usize>>,
}

/// Parse an LSP / TextMate snippet. Placeholders keep their default text (nested ones too),
/// a choice keeps its first option, variables keep their default (else nothing), and `\$`,
/// `\}`, `\\` are literal. Text that is not a well-formed snippet is kept as written.
pub fn parse(snippet: &str) -> Parsed {
    let chars: Vec<char> = snippet.chars().collect();
    let mut text = String::new();
    // (index, range) for every stop, in the order met.
    let mut found: Vec<(u32, Range<usize>)> = vec![];
    parse_into(&chars, &mut 0, &mut text, &mut found, false);
    // First occurrence of each index; `$0` last.
    let mut stops: Vec<(u32, Range<usize>)> = vec![];
    for (ix, range) in found {
        if !stops.iter().any(|(i, _)| *i == ix) {
            stops.push((ix, range));
        }
    }
    stops.sort_by_key(|(ix, _)| if *ix == 0 { u32::MAX } else { *ix });
    let mut stops: Vec<Range<usize>> = stops.into_iter().map(|(_, r)| r).collect();
    if stops.is_empty() || !snippet_has_final(snippet) {
        stops.push(text.len()..text.len());
    }
    short_params(Parsed { text, stops })
}

/// `${1:const std::string &name}` (clangd) keeps only `name`: the placeholder is a hint of what
/// goes there, the type is in the signature help.
fn short_params(mut p: Parsed) -> Parsed {
    for ix in 0..p.stops.len() {
        let r = p.stops[ix].clone();
        let inner = p.stops.iter().any(|o| *o != r && r.start <= o.start && o.end <= r.end && !o.is_empty());
        let t = &p.text[r.clone()];
        if inner || t.contains(['(', '[', '<', '{', '"', '=']) || !t.trim().contains([' ', '&', '*']) {
            continue;
        }
        let name_len = t.chars().rev().take_while(|c| c.is_alphanumeric() || *c == '_').map(char::len_utf8).sum::<usize>();
        if name_len == 0 {
            continue;
        }
        let cut = r.end - name_len - r.start;
        p.text.replace_range(r.start..r.start + cut, "");
        for o in &mut p.stops {
            let shift = |x: usize| if x > r.start { x.saturating_sub(cut).max(r.start) } else { x };
            *o = shift(o.start)..shift(o.end);
        }
    }
    p
}

fn snippet_has_final(snippet: &str) -> bool {
    let chars: Vec<char> = snippet.chars().collect();
    let mut found = vec![];
    parse_into(&chars, &mut 0, &mut String::new(), &mut found, false);
    found.iter().any(|(ix, _)| *ix == 0)
}

fn digits(chars: &[char], i: &mut usize) -> Option<u32> {
    let start = *i;
    while *i < chars.len() && chars[*i].is_ascii_digit() {
        *i += 1;
    }
    (*i > start).then(|| chars[start..*i].iter().collect::<String>().parse().ok()).flatten()
}

fn name(chars: &[char], i: &mut usize) -> Option<String> {
    let start = *i;
    if *i < chars.len() && (chars[*i].is_ascii_alphabetic() || chars[*i] == '_') {
        while *i < chars.len() && (chars[*i].is_ascii_alphanumeric() || chars[*i] == '_') {
            *i += 1;
        }
    }
    (*i > start).then(|| chars[start..*i].iter().collect())
}

/// Parse until the end (or, `nested`, until the `}` closing a placeholder, consumed).
fn parse_into(chars: &[char], i: &mut usize, out: &mut String, stops: &mut Vec<(u32, Range<usize>)>, nested: bool) {
    while *i < chars.len() {
        let c = chars[*i];
        match c {
            '\\' if *i + 1 < chars.len() && matches!(chars[*i + 1], '$' | '}' | '\\' | ',' | '|') => {
                out.push(chars[*i + 1]);
                *i += 2;
            }
            '}' if nested => {
                *i += 1;
                return;
            }
            '$' => {
                let save = *i;
                *i += 1;
                if let Some(ix) = digits(chars, i) {
                    stops.push((ix, out.len()..out.len()));
                } else if *i < chars.len() && chars[*i] == '{' {
                    *i += 1;
                    if let Some(ix) = digits(chars, i) {
                        match chars.get(*i) {
                            Some('}') => {
                                *i += 1;
                                stops.push((ix, out.len()..out.len()));
                            }
                            Some(':') => {
                                *i += 1;
                                let start = out.len();
                                let at = stops.len();
                                parse_into(chars, i, out, stops, true);
                                stops.insert(at, (ix, start..out.len()));
                            }
                            Some('|') => {
                                // ${1|a,b,c|}: the first choice.
                                *i += 1;
                                let start = out.len();
                                let mut first = true;
                                while *i < chars.len() && !(chars[*i] == '|' && chars.get(*i + 1) == Some(&'}')) {
                                    match chars[*i] {
                                        '\\' if *i + 1 < chars.len() => {
                                            if first {
                                                out.push(chars[*i + 1]);
                                            }
                                            *i += 2;
                                            continue;
                                        }
                                        ',' => first = false,
                                        ch if first => out.push(ch),
                                        _ => {}
                                    }
                                    *i += 1;
                                }
                                *i = (*i + 2).min(chars.len());
                                stops.push((ix, start..out.len()));
                            }
                            _ => {
                                *i = save + 1;
                                out.push('$');
                            }
                        }
                    } else if let Some(_var) = name(chars, i) {
                        match chars.get(*i) {
                            Some('}') => *i += 1,
                            // ${VAR:default}: its default (stops in it are kept).
                            Some(':') => {
                                *i += 1;
                                parse_into(chars, i, out, stops, true);
                            }
                            // ${VAR/regex/format/}: skipped.
                            Some('/') => {
                                while *i < chars.len() && chars[*i] != '}' {
                                    *i += if chars[*i] == '\\' { 2 } else { 1 };
                                }
                                *i += 1;
                            }
                            _ => {
                                *i = save + 1;
                                out.push('$');
                            }
                        }
                    } else {
                        *i = save + 1;
                        out.push('$');
                    }
                } else if name(chars, i).is_some() {
                    // $VAR: no value known here.
                } else {
                    out.push('$');
                }
            }
            _ => {
                out.push(c);
                *i += 1;
            }
        }
    }
}

/// A snippet being filled in: absolute stops in the buffer.
struct Session {
    /// Placeholders not visited yet, drawn as pills; their text goes when the snippet ends.
    pills: RangeDecorationCollection,
    /// The whole snippet (invisible): leaving it ends the snippet.
    span: RangeDecorationCollection,
    stops: Vec<Range<usize>>,
    current: usize,
    /// Buffer length when the current stop was selected: what the user typed since shifts the
    /// stops after it.
    len: usize,
    /// The current stop's placeholder, removed too if the snippet ends before it is typed over.
    selected: Option<(Range<usize>, String)>,
}

#[derive(Default)]
struct Sessions(HashMap<EntityId, Session>);
impl Global for Sessions {}

/// Select stop `ix` of the session (cursor on it, or its placeholder text selected).
fn select(editor: &mut EditorState, range: Range<usize>, cx: &mut Context<EditorState>) {
    let len = editor.text().len();
    editor.set_selected_range(range.start.min(len)..range.end.min(len), cx);
}

/// `parsed` was just inserted at `start`: go to its first stop, and remember the others.
pub fn begin(editor: &mut EditorState, start: usize, parsed: &Parsed, cx: &mut Context<EditorState>) {
    let id = cx.entity_id();
    let stops: Vec<Range<usize>> = parsed.stops.iter().map(|r| start + r.start..start + r.end).collect();
    select(editor, stops[0].clone(), cx);
    let old = if stops.len() > 1 {
        let len = editor.text().len();
        let pills = editor.create_range_decorations_collection(stops[1..stops.len() - 1].iter().filter(|r| !r.is_empty()).map(|r| pill(r.clone())).collect(), cx);
        let whole = start..start + parsed.text.len();
        let span = editor.create_range_decorations_collection(
            vec![RangeDecoration::new(whole).with_style(RangeDecorationStyle::Fill).with_color(hsla(0., 0., 0., 0.))],
            cx,
        );
        let selected = Some((stops[0].clone(), parsed.text[parsed.stops[0].clone()].to_string()));
        cx.default_global::<Sessions>().0.insert(id, Session { pills, span, stops, current: 0, len, selected })
    } else {
        cx.default_global::<Sessions>().0.remove(&id)
    };
    // The editor is being updated: its collections can only be dropped afterwards.
    if let Some(old) = old {
        cx.defer(move |cx| dispose(&old, cx));
    }
}

fn pill(range: Range<usize>) -> RangeDecoration {
    RangeDecoration::new(range).with_style(RangeDecorationStyle::Pill)
}

fn dispose(session: &Session, cx: &mut App) {
    session.pills.dispose(cx);
    session.span.dispose(cx);
}

/// End the snippet: the placeholders never visited are removed (they were only hints).
fn finish(editor: &Entity<EditorState>, session: Session, window: &mut Window, cx: &mut App) {
    let mut edits: Vec<(Range<usize>, String)> = session.pills.get_ranges(cx).into_iter().map(|r| (r, String::new())).collect();
    dispose(&session, cx);
    let untouched = editor.read_with(cx, |e, _| {
        let text = e.text().to_string();
        session.selected.filter(|(r, t)| !t.is_empty() && text.len() == session.len && text.get(r.clone()) == Some(t.as_str())).map(|(r, _)| r)
    });
    edits.extend(untouched.map(|r| (r, String::new())));
    if edits.is_empty() {
        return;
    }
    editor.update(cx, |editor, cx| {
        let selection = editor.selected_range();
        let before = |at: usize| edits.iter().filter(|(r, _)| r.end <= at).map(|(r, _)| r.len()).sum::<usize>();
        editor.replace_text_in_ranges(&edits, window, cx);
        editor.set_selected_range(selection.start - before(selection.start)..selection.end - before(selection.end), cx);
    });
}

/// Tab (`forward`) or Shift+Tab inside a snippet: the next or previous stop. False when no
/// snippet is being filled in here (the key then does what it usually does).
pub fn jump(editor: &Entity<EditorState>, forward: bool, window: &mut Window, cx: &mut App) -> bool {
    let id = editor.entity_id();
    let Some(mut session) = cx.default_global::<Sessions>().0.remove(&id) else { return false };
    let (len, cursor) = editor.read_with(cx, |e, _| (e.text().len(), e.cursor()));
    // What was typed in the current stop moves the ones after it.
    let delta = len as isize - session.len as isize;
    let cur = session.current;
    let shift = |p: usize| (p as isize + delta).max(0) as usize;
    session.stops[cur].end = shift(session.stops[cur].end).max(session.stops[cur].start);
    for stop in session.stops.iter_mut().skip(cur + 1) {
        *stop = shift(stop.start)..shift(stop.end);
    }
    // The cursor left the snippet: it is over.
    let (first, last) = (session.stops.iter().map(|s| s.start).min().unwrap_or(0), session.stops.iter().map(|s| s.end).max().unwrap_or(len));
    let next = if forward { cur + 1 } else { cur.saturating_sub(1) };
    if cursor < first || cursor > last || next >= session.stops.len() {
        finish(editor, session, window, cx);
        return false;
    }
    session.current = next;
    session.len = len;
    let stop = session.stops[next].clone();
    // A visited placeholder is selected instead: typing replaces it.
    let pills = session.pills.get_ranges(cx).into_iter().filter(|r| *r != stop).map(pill).collect();
    session.pills.set(pills, cx);
    session.selected = editor.update(cx, |e, cx| {
        select(e, stop.clone(), cx);
        e.text().to_string().get(stop.clone()).map(|t| (stop, t.to_string()))
    });
    // The final stop ends the snippet.
    if next + 1 < session.stops.len() {
        cx.default_global::<Sessions>().0.insert(id, session);
    } else {
        finish(editor, session, window, cx);
    }
    true
}

/// End the snippet being filled in (Escape).
pub fn end(editor: &Entity<EditorState>, window: &mut Window, cx: &mut App) -> bool {
    let Some(session) = cx.default_global::<Sessions>().0.remove(&editor.entity_id()) else { return false };
    finish(editor, session, window, cx);
    true
}

/// After a click: end the snippet when the cursor left it.
pub fn end_if_left(editor: &Entity<EditorState>, window: &mut Window, cx: &mut App) {
    let Some(span) = cx.default_global::<Sessions>().0.get(&editor.entity_id()).map(|s| s.span.clone()) else { return };
    let cursor = editor.read(cx).cursor();
    if !span.get_ranges(cx).iter().any(|r| r.start <= cursor && cursor <= r.end) {
        end(editor, window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn placeholders_keep_only_the_parameter_name() {
        let p = parse("draw(${1:const sf::Drawable &drawable}, ${2:int n})");
        assert_eq!(p.text, "draw(drawable, n)");
        assert_eq!(p.stops, vec![5..13, 15..16, 17..17]);
        assert_eq!(parse("${1:std::vector<int> v}").text, "std::vector<int> v");
    }

    #[test]
    fn parses_lsp_snippets() {
        let p = parse("box-sizing: $0;");
        assert_eq!((p.text.as_str(), p.stops.clone()), ("box-sizing: ;", vec![12..12]));
        let p = parse("fn ${1:name}(${2:arg}) {$0}");
        assert_eq!(p.text, "fn name(arg) {}");
        assert_eq!(p.stops, vec![3..7, 8..11, 14..14]);
        // No $0: the end of the text is the last stop.
        let p = parse("print($1)");
        assert_eq!((p.text.as_str(), p.stops.clone()), ("print()", vec![6..6, 7..7]));
        // Nested placeholders, choices, variables, escapes.
        let p = parse("${1:foo(${2:x})} ${3|a,b|} ${TM_FILENAME:file} \\$5 $UNKNOWN");
        assert_eq!(p.text, "foo(x) a file $5 ");
        assert_eq!(p.stops[0], 0..6);
        assert_eq!(p.stops[1], 4..5);
        assert_eq!(p.stops[2], 7..8);
        // Plain text stays as is.
        assert_eq!(parse("a $ b").text, "a $ b");
        assert_eq!(parse("cost: 5$").text, "cost: 5$");
    }
}
