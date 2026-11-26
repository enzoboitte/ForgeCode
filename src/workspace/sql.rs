//! SQL highlighting with a small lexer. The tree-sitter SQL grammar knows neither MySQL's `\'`
//! escape nor most dump syntax, and one bad string flips the colors of the rest of the file.

use std::ops::Range;
use std::rc::Rc;

use gpui_kit::component::input::{
    EditorState, FoldRange, HighlightStyleResolver, InputEdit, InputHighlighter, InputHighlighterFactory, Rope,
};
use gpui_kit::{Context, HighlightStyle, SharedString, Window};

const KEYWORDS: &[&str] = &[
    "add", "all", "alter", "and", "as", "asc", "auto_increment", "begin", "between", "by", "cascade", "case", "check",
    "column", "commit", "constraint", "create", "cross", "database", "default", "delete", "desc", "distinct", "drop",
    "else", "end", "engine", "exists", "explain", "foreign", "from", "full", "grant", "group", "having", "if", "ignore",
    "in", "index", "inner", "insert", "into", "is", "join", "key", "left", "like", "limit", "lock", "not", "null",
    "offset", "on", "or", "order", "outer", "primary", "procedure", "references", "rename", "replace", "returning",
    "revoke", "right", "rollback", "schema", "select", "set", "show", "table", "tables", "then", "to", "transaction",
    "trigger", "truncate", "union", "unique", "unlock", "unsigned", "update", "use", "using", "values", "view", "when",
    "where", "with", "true", "false",
];

const TYPES: &[&str] = &[
    "bigint", "binary", "bit", "blob", "bool", "boolean", "char", "date", "datetime", "decimal", "double", "enum",
    "float", "int", "integer", "json", "longtext", "mediumint", "mediumtext", "numeric", "real", "serial", "smallint",
    "text", "time", "timestamp", "tinyint", "tinytext", "uuid", "varchar", "varbinary", "year",
];

/// Highlighter for buffers whose language is `sql`; other languages keep the default one.
pub fn factory() -> InputHighlighterFactory {
    Rc::new(|language| (language == "sql").then(|| Box::new(SqlHighlighter(vec![])) as Box<dyn InputHighlighter>))
}

struct SqlHighlighter(Vec<(Range<usize>, &'static str)>);

/// Ordered, non-overlapping highlight ranges of `text`.
fn lex(text: &str) -> Vec<(Range<usize>, &'static str)> {
    let b = text.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    let until = |from: usize, pat: &str| text[from..].find(pat).map_or(b.len(), |p| from + p + pat.len());
    while i < b.len() {
        let start = i;
        let kind = match b[i] {
            b'-' if b.get(i + 1) == Some(&b'-') => {
                i = text[i..].find('\n').map_or(b.len(), |p| i + p);
                "comment"
            }
            b'#' => {
                i = text[i..].find('\n').map_or(b.len(), |p| i + p);
                "comment"
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = until(i + 2, "*/");
                "comment"
            }
            q @ (b'\'' | b'"') => {
                i += 1;
                // Both `\'` and `''` escape a quote.
                while i < b.len() {
                    match b[i] {
                        b'\\' => i += 2,
                        c if c == q && b.get(i + 1) == Some(&q) => i += 2,
                        c if c == q => {
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
                i = i.min(b.len());
                "string"
            }
            b'`' => {
                i = until(i + 1, "`");
                continue;
            }
            c if c.is_ascii_digit() => {
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.') {
                    i += 1;
                }
                "number"
            }
            c if c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 => {
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] >= 0x80) {
                    i += 1;
                }
                let word = text[start..i].to_ascii_lowercase();
                if text[i..].trim_start_matches([' ', '\t']).starts_with('(') && !KEYWORDS.contains(&word.as_str()) {
                    "function"
                } else if KEYWORDS.contains(&word.as_str()) {
                    "keyword"
                } else if TYPES.contains(&word.as_str()) {
                    "type"
                } else {
                    continue;
                }
            }
            _ => {
                i += 1;
                continue;
            }
        };
        out.push((start..i, kind));
    }
    out
}

impl InputHighlighter for SqlHighlighter {
    fn language(&self) -> SharedString {
        "sql".into()
    }

    fn update(&mut self, _: Option<InputEdit>, text: &Rope, _: bool, _: &mut Window, _: &mut Context<EditorState>) {
        // shortcut: relexes the whole file on each edit (a few ms per MB); go incremental if big dumps lag.
        self.0 = lex(&text.to_string());
    }

    fn styles(&self, range: &Range<usize>, resolver: &dyn HighlightStyleResolver) -> Vec<(Range<usize>, HighlightStyle)> {
        let first = self.0.partition_point(|(r, _)| r.end <= range.start);
        let mut runs = vec![];
        let mut cursor = range.start;
        for (r, name) in &self.0[first..] {
            if r.start >= range.end {
                break;
            }
            let (start, end) = (r.start.max(cursor), r.end.min(range.end));
            if start >= end {
                continue;
            }
            if cursor < start {
                runs.push((cursor..start, HighlightStyle::default()));
            }
            runs.push((start..end, resolver.style(name).unwrap_or_default()));
            cursor = end;
        }
        if cursor < range.end {
            runs.push((cursor..range.end, HighlightStyle::default()));
        }
        runs
    }

    fn fold_ranges(&self, _: &Rope) -> Vec<FoldRange> {
        vec![]
    }
}

#[cfg(test)]
mod tests {
    use super::lex;

    #[test]
    fn escaped_quotes_keep_strings_balanced() {
        let src = "INSERT INTO film VALUES (101, 'Auberge, L\\'', '', 2002, 'It''s'), (106, 'True Romance');";
        let found: Vec<(&str, &str)> = lex(src).into_iter().map(|(r, k)| (&src[r], k)).collect();
        assert_eq!(&found[..3], [("INSERT", "keyword"), ("INTO", "keyword"), ("VALUES", "keyword")]);
        assert!(found.contains(&("'Auberge, L\\''", "string")));
        assert!(found.contains(&("'It''s'", "string")));
        assert!(found.contains(&("'True Romance'", "string")), "strings after the escape stay strings");
        assert!(found.contains(&("2002", "number")));
        assert!(!found.iter().any(|(t, _)| *t == "True"));
        let src = "SELECT count(id) -- total\nFROM t; /* x */";
        let kinds: Vec<_> = lex(src).into_iter().map(|(_, k)| k).collect();
        assert_eq!(kinds, ["keyword", "function", "comment", "keyword", "comment"]);
    }
}
