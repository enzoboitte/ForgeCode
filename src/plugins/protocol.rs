//! `@forge {json}` lines printed by plugin commands.

use serde::Deserialize;

const PREFIX: &str = "@forge ";

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum IdeAction {
    Notify { message: String },
    Open { path: String, #[serde(default)] line: Option<u32> },
    ReplaceBuffer { text: String },
    Insert { text: String },
    /// Reload the active buffer from disk.
    Reload,
    /// Send a prompt to the AI chat.
    Chat { message: String },
}

/// `Some(Ok(action))` for a protocol line, `Some(Err)` for a malformed one, `None` for plain output.
pub fn parse_line(line: &str) -> Option<Result<IdeAction, String>> {
    let json = line.trim_end().strip_prefix(PREFIX)?;
    Some(serde_json::from_str(json).map_err(|e| format!("invalid @forge line: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_actions_and_ignores_plain_lines() {
        assert_eq!(parse_line("hello"), None);
        assert_eq!(
            parse_line(r#"@forge {"action":"open","path":"a.rs","line":3}"#),
            Some(Ok(IdeAction::Open { path: "a.rs".into(), line: Some(3) }))
        );
        assert_eq!(parse_line(r#"@forge {"action":"reload"}"#), Some(Ok(IdeAction::Reload)));
        assert!(matches!(parse_line("@forge {bad"), Some(Err(_))));
    }
}
