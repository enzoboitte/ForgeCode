//! Hermes' conversations and the sub-agents it delegated to, as a tree, read from its session
//! database (`state.db`, read-only): a conversation continued after context compression stays one
//! node; delegated sub-agents are its children.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Row {
    pub id: String,
    pub parent_session_id: Option<String>,
    pub title: Option<String>,
    pub model: Option<String>,
    pub started_at: f64,
    pub ended_at: Option<f64>,
    pub end_reason: Option<String>,
    #[serde(default)]
    pub message_count: i64,
    #[serde(default)]
    pub tool_call_count: i64,
    #[serde(default)]
    pub input_tokens: i64,
    #[serde(default)]
    pub output_tokens: i64,
    pub cwd: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum NodeState {
    Running,
    Done,
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct Node {
    pub id: String,
    pub title: String,
    pub model: String,
    pub state: NodeState,
    pub messages: i64,
    pub tools: i64,
    pub tokens: i64,
    /// How many times the context was compressed (the conversation went on in a new segment).
    pub compressions: usize,
    pub started_at: f64,
    pub children: Vec<Node>,
}

const QUERY: &str = r#"
import json, sqlite3, sys
db = sqlite3.connect("file:" + sys.argv[1] + "?mode=ro", uri=True)
db.row_factory = sqlite3.Row
rows = db.execute("select id, parent_session_id, title, model, started_at, ended_at, end_reason, message_count, tool_call_count, input_tokens, output_tokens, cwd from sessions order by started_at desc limit 500").fetchall()
print(json.dumps([dict(r) for r in rows]))
"#;

/// Every session row, through `python` (Hermes' own, which has sqlite3).
pub fn read_rows(python: &Path, db: &Path) -> Result<Vec<Row>> {
    if !db.is_file() {
        return Ok(vec![]);
    }
    let out = Command::new(python).arg("-c").arg(QUERY).arg(db).stdin(Stdio::null()).output().context("read Hermes sessions")?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("sqlite error"));
    }
    Ok(serde_json::from_slice(&out.stdout)?)
}

/// The project's conversations (newest first) with their sub-agents.
pub fn build(rows: &[Row], root: &Path) -> Vec<Node> {
    let by_id = |id: &str| rows.iter().find(|r| r.id == id);
    let compression_parent = |r: &Row| r.parent_session_id.as_deref().and_then(by_id).filter(|p| p.end_reason.as_deref() == Some("compression"));
    fn node_of(chain: &[&Row], children: Vec<Node>) -> Node {
        let first = chain[0];
        let last = chain[chain.len() - 1];
        let state = match (last.ended_at, last.end_reason.as_deref()) {
            (None, _) => NodeState::Running,
            (Some(_), Some(r)) if r.contains("error") || r.contains("fail") || r.contains("interrupt") => NodeState::Failed(r.to_string()),
            _ => NodeState::Done,
        };
        Node {
            id: first.id.clone(),
            title: chain.iter().rev().find_map(|r| r.title.clone().filter(|t| !t.is_empty())).unwrap_or_else(|| first.id.chars().take(8).collect()),
            model: last.model.clone().unwrap_or_default(),
            state,
            messages: chain.iter().map(|r| r.message_count).sum(),
            tools: chain.iter().map(|r| r.tool_call_count).sum(),
            tokens: chain.iter().map(|r| r.input_tokens + r.output_tokens).sum(),
            compressions: chain.len() - 1,
            started_at: first.started_at,
            children,
        }
    }
    fn tree(rows: &[Row], start: &Row, depth: usize) -> Node {
        let mut chain = vec![start];
        let mut r = start;
        while let Some(next) = rows.iter().find(|c| c.parent_session_id.as_deref() == Some(&r.id) && r.end_reason.as_deref() == Some("compression")) {
            r = next;
            chain.push(r);
        }
        // Sub-agents: children of any segment that aren't compression continuations.
        let mut children: Vec<Node> = if depth > 8 {
            vec![]
        } else {
            // The chain holds the continuations, so what's left are delegated sub-agents.
            rows.iter()
                .filter(|c| c.parent_session_id.as_deref().is_some_and(|p| chain.iter().any(|s| s.id == p)) && !chain.iter().any(|s| s.id == c.id))
                .map(|c| tree(rows, c, depth + 1))
                .collect()
        };
        children.sort_by(|a, b| a.started_at.total_cmp(&b.started_at));
        node_of(&chain, children)
    }
    let root_s = root.display().to_string();
    rows.iter()
        // Conversation starts: no parent, or a parent outside the database.
        .filter(|r| r.parent_session_id.as_deref().and_then(by_id).is_none() && compression_parent(r).is_none())
        .filter(|r| r.cwd.as_deref().is_none_or(|c| c == root_s))
        .map(|r| tree(rows, r, 0))
        .collect()
}

/// Hermes' Python (for its sqlite3), else the system's.
pub fn python_for(runtime_src: &Path) -> PathBuf {
    let own = runtime_src.join(if cfg!(windows) { "venv/Scripts/python.exe" } else { "venv/bin/python" });
    if own.is_file() { own } else { PathBuf::from(if cfg!(windows) { "python" } else { "python3" }) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, parent: Option<&str>, end: Option<&str>, msgs: i64) -> Row {
        Row {
            id: id.into(),
            parent_session_id: parent.map(String::from),
            title: Some(format!("t-{id}")),
            model: Some("m".into()),
            started_at: id.len() as f64,
            ended_at: end.map(|_| 1.0),
            end_reason: end.map(String::from),
            message_count: msgs,
            cwd: Some("/p".into()),
            ..Default::default()
        }
    }

    #[test]
    fn builds_conversations_with_compression_and_subagents() {
        let rows = vec![
            row("a", None, Some("compression"), 10),
            row("a2", Some("a"), None, 3),           // continuation of `a`
            row("sub1", Some("a"), Some("completed"), 4), // delegated from the first segment
            row("sub2", Some("a2"), None, 2),        // delegated after compression, still running
            row("b", None, Some("completed"), 1),
        ];
        let tree = build(&rows, Path::new("/p"));
        assert_eq!(tree.len(), 2, "two conversations");
        let a = tree.iter().find(|n| n.id == "a").unwrap();
        assert_eq!((a.compressions, a.messages, a.state.clone()), (1, 13, NodeState::Running));
        let kids: Vec<&str> = a.children.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(kids.len(), 2, "{kids:?}");
        assert!(kids.contains(&"sub1") && kids.contains(&"sub2"));
        assert!(build(&rows, Path::new("/other")).is_empty(), "other projects are left out");
    }
}
