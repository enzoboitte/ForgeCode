//! Table view of a CSV/TSV buffer: parsed from the editor text, header row pinned, rows virtualized.

use std::path::Path;
use std::rc::Rc;

use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

pub fn is_csv(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("csv") || e.eq_ignore_ascii_case("tsv"))
}

/// The separator: tab for .tsv, else whichever of , ; tab is most frequent on the first line.
fn separator(path: &Path, text: &str) -> char {
    if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("tsv")) {
        return '\t';
    }
    let first = text.lines().next().unwrap_or("");
    [',', ';', '\t'].into_iter().max_by_key(|c| first.matches(*c).count()).unwrap_or(',')
}

/// RFC 4180 records: quoted fields may hold the separator, newlines and "" for a quote.
pub fn parse(text: &str, sep: char) -> Vec<Vec<String>> {
    let (mut rows, mut row, mut field) = (Vec::new(), Vec::new(), String::new());
    let (mut quoted, mut chars) = (false, text.chars().peekable());
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                chars.next();
                field.push('"');
            }
            '"' if quoted => quoted = false,
            '"' if field.is_empty() => quoted = true,
            c if c == sep && !quoted => row.push(std::mem::take(&mut field)),
            '\r' if !quoted => {}
            '\n' if !quoted => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            c => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

pub fn view(id: SharedString, path: &Path, text: &str, cx: &App) -> AnyElement {
    // shortcut: parsed again on each render of the pane; cache by buffer version if big files lag.
    let rows = Rc::new(parse(text, separator(path, text)));
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Rc<Vec<Pixels>> = Rc::new(
        (0..cols)
            .map(|c| {
                let chars = rows.iter().take(500).filter_map(|r| r.get(c)).map(|s| s.chars().count()).max().unwrap_or(0);
                px((chars as f32 * 7. + 18.).clamp(50., 320.))
            })
            .collect(),
    );
    let num_w = px(rows.len().to_string().len() as f32 * 7. + 18.);
    let total = widths.iter().fold(num_w, |a, w| a + *w);
    let theme = cx.theme().clone();
    let line = move |cells: &[String], n: Option<usize>, widths: &[Pixels], theme: &gpui_kit::component::Theme| {
        // At least as wide as the columns, wider when the pane is: the columns share the rest.
        h_flex()
            .min_w(total)
            .w_full()
            .h(px(24.))
            .text_xs()
            .border_b_1()
            .border_color(theme.border.opacity(0.5))
            .child(div().w(num_w).flex_none().px_1().text_color(theme.muted_foreground).when_some(n, |el, n| el.child(n.to_string())))
            .children(widths.iter().enumerate().map(|(c, w)| {
                div().flex_basis(*w).flex_grow(f32::from(*w)).flex_shrink_0().min_w_0().px_2().truncate().border_r_1().border_color(theme.border.opacity(0.5))
                    .when(n.is_none(), |el| el.debug_selector(move || format!("csv-h{c}")))
                    .child(cells.get(c).cloned().unwrap_or_default())
            }))
    };
    let header = rows.first().map(|h| line(h, None, &widths, &theme).h(px(26.)).bg(theme.secondary).font_weight(FontWeight::MEDIUM));
    let body = {
        let (rows, widths, theme) = (rows.clone(), widths.clone(), theme.clone());
        uniform_list(SharedString::from(format!("{id}-rows")), rows.len().saturating_sub(1), move |range, _, _| {
            range.map(|i| line(&rows[i + 1], Some(i + 1), &widths, &theme)).collect::<Vec<_>>()
        })
        .flex_1()
    };
    div()
        .id(id)
        .size_full()
        .overflow_x_scroll()
        .child(v_flex().min_w(total).w_full().h_full().children(header).child(body))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::{Path, parse, separator};

    /// Few narrow columns in a wide pane: they stretch to fill it.
    #[gpui_kit::test]
    fn columns_fill_a_wide_pane(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::{AppContext as _, IntoElement as _, Render, px};
        struct V;
        impl Render for V {
            fn render(&mut self, _: &mut gpui_kit::Window, cx: &mut gpui_kit::Context<Self>) -> impl gpui_kit::IntoElement {
                super::view("t".into(), Path::new("x.csv"), "a,b\n1,2\n", cx).into_any_element()
            }
        }
        cx.update(gpui_kit::init);
        let (_, cx) = cx.add_window_view(|window, cx| gpui_kit::base::Root::new(cx.new(|_| V), window, cx));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let (a, b) = (cx.debug_bounds("csv-h0").unwrap(), cx.debug_bounds("csv-h1").unwrap());
        let view = cx.update(|window, _| window.viewport_size().width);
        assert!(b.right() > view - px(2.), "last column ends at {:?}, view {view:?}", b.right());
        assert!(a.size.width > px(100.));
    }

    #[test]
    fn parses_quotes_separators_and_newlines() {
        let rows = parse("a,b,c\r\n1,\"x, y\",\"he said \"\"hi\"\"\"\n2,\"two\nlines\",\n", ',');
        let want: [&[&str]; 3] = [&["a", "b", "c"], &["1", "x, y", "he said \"hi\""], &["2", "two\nlines", ""]];
        assert_eq!(rows, want.map(|r| r.iter().map(|s| s.to_string()).collect::<Vec<_>>()));
        assert_eq!(separator(Path::new("x.csv"), "a;b;c\n"), ';');
        assert_eq!(separator(Path::new("x.tsv"), "a,b\n"), '\t');
    }
}
