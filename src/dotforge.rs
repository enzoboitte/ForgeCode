//! The per-project `.forgecode/` folder:
//! - `instructions.md`: project instructions given to every AI agent;
//! - `agents/<role>.md`: extra instructions per role (coder, researcher, reviewer, security, main);
//! - `settings.json`: project settings overriding the global ones;
//! - `commands/<name>.md`: custom chat commands (`/name`), `$ARGUMENTS` = what follows the command;
//! - `chats/<id>.json`: conversation history (git-ignored).

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const DIR: &str = ".forgecode";
/// Instructions are cut beyond this size to protect small models' context.
const MAX_INSTRUCTIONS: usize = 8_000;

pub fn dir(root: &Path) -> PathBuf {
    root.join(DIR)
}

pub fn instructions_path(root: &Path) -> PathBuf {
    dir(root).join("instructions.md")
}

pub fn settings_path(root: &Path) -> PathBuf {
    dir(root).join("settings.json")
}

/// A folder worth configuring: a git repository or a known build system.
pub fn is_project(root: &Path) -> bool {
    root.join(".git").exists() || crate::project::detect(root) != crate::project::BuildSystem::None || dir(root).is_dir()
}

/// Create the folder with commented defaults; existing files are never touched.
pub fn ensure(root: &Path) -> std::io::Result<PathBuf> {
    let d = dir(root);
    for sub in ["agents", "commands", "chats"] {
        std::fs::create_dir_all(d.join(sub))?;
    }
    let defaults: [(&str, &str); 5] = [
        ("instructions.md", include_str!("dotforge/instructions.md")),
        ("settings.json", "{}\n"),
        (".gitignore", "chats/\n"),
        ("agents/README.md", include_str!("dotforge/agents.md")),
        ("commands/explain-simply.md", include_str!("dotforge/command.md")),
    ];
    for (rel, content) in defaults {
        let path = d.join(rel);
        if !path.exists() {
            std::fs::write(path, content)?;
        }
    }
    Ok(d)
}

fn read_capped(path: &Path) -> Option<String> {
    let mut s = std::fs::read_to_string(path).ok()?;
    if s.trim().is_empty() {
        return None;
    }
    if s.len() > MAX_INSTRUCTIONS {
        let mut cut = MAX_INSTRUCTIONS;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.push_str("\n[truncated]");
    }
    Some(s)
}

/// Project instructions, plus the role's own file when `role` is given.
pub fn instructions(root: &Path, role: Option<&str>) -> String {
    let mut parts = vec![];
    if let Some(s) = read_capped(&instructions_path(root)) {
        parts.push(s);
    }
    if let Some(s) = role.and_then(|r| read_capped(&dir(root).join("agents").join(format!("{r}.md")))) {
        parts.push(s);
    }
    parts.join("\n\n")
}

/// Project settings overrides (keys of the global settings file).
pub fn overrides(root: &Path) -> Map<String, Value> {
    std::fs::read_to_string(settings_path(root))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

pub fn write_overrides(root: &Path, map: &Map<String, Value>) -> std::io::Result<()> {
    std::fs::create_dir_all(dir(root))?;
    std::fs::write(settings_path(root), serde_json::to_string_pretty(map).unwrap_or_default() + "\n")
}

/// Custom commands: (name, one-line description, template).
pub fn commands(root: &Path) -> Vec<(String, String, String)> {
    let Ok(entries) = std::fs::read_dir(dir(root).join("commands")) else { return vec![] };
    let mut out: Vec<_> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "md"))
        .filter_map(|p| {
            let template = std::fs::read_to_string(&p).ok()?;
            let name = p.file_stem()?.to_string_lossy().to_string();
            let description = template.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim_start_matches('#').trim().to_string();
            Some((name, description, template))
        })
        .collect();
    out.sort();
    out
}

// ---- conversations ----------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedChat {
    pub id: String,
    pub title: String,
    /// Unix seconds.
    pub updated: u64,
    pub model: String,
    pub history: Vec<Value>,
    /// The agent tree of the last run (see `ai::agents_view`).
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub agents: Value,
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn new_chat_id() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("{nanos:x}")
}

/// Projects keep their conversations in `.forgecode/chats`; other folders in the config dir,
/// so a plain folder doesn't get a `.forgecode` it never asked for.
fn chats_dir(root: &Path) -> PathBuf {
    if is_project(root) {
        return dir(root).join("chats");
    }
    let name: String = root.display().to_string().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    crate::settings::config_dir().join("chats").join(name)
}

fn chat_path(root: &Path, id: &str) -> Option<PathBuf> {
    // Ids are ours (hex); anything else could escape the folder.
    id.chars().all(|c| c.is_ascii_alphanumeric()).then(|| chats_dir(root).join(format!("{id}.json")))
}

pub fn save_chat(root: &Path, chat: &SavedChat) -> std::io::Result<()> {
    let path = chat_path(root, &chat.id).ok_or_else(|| std::io::Error::other("invalid chat id"))?;
    if is_project(root) {
        ensure(root)?;
    }
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(path, serde_json::to_string(chat).unwrap_or_default())
}

pub fn load_chat(root: &Path, id: &str) -> Option<SavedChat> {
    serde_json::from_str(&std::fs::read_to_string(chat_path(root, id)?).ok()?).ok()
}

pub fn delete_chat(root: &Path, id: &str) {
    if let Some(p) = chat_path(root, id) {
        let _ = std::fs::remove_file(p);
    }
}

/// Saved conversations, newest first: (id, title, updated).
pub fn list_chats(root: &Path) -> Vec<(String, String, u64)> {
    let Ok(entries) = std::fs::read_dir(chats_dir(root)) else { return vec![] };
    let mut out: Vec<_> = entries
        .flatten()
        .filter_map(|e| {
            let id = e.path().file_stem()?.to_string_lossy().to_string();
            let chat = load_chat(root, &id)?;
            Some((chat.id, chat.title, chat.updated))
        })
        .collect();
    out.sort_by(|a, b| b.2.cmp(&a.2));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_instructions_commands_and_chats() {
        let root = std::env::temp_dir().join(format!("forgecode-dot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        ensure(&root).unwrap();
        assert!(dir(&root).join(".gitignore").is_file());
        std::fs::write(instructions_path(&root), "Use French comments.").unwrap();
        std::fs::write(dir(&root).join("agents/coder.md"), "Prefer iterators.").unwrap();
        assert_eq!(instructions(&root, None), "Use French comments.");
        assert_eq!(instructions(&root, Some("coder")), "Use French comments.\n\nPrefer iterators.");

        let mut o = Map::new();
        o.insert("agent_mode".into(), "plan".into());
        write_overrides(&root, &o).unwrap();
        assert_eq!(overrides(&root)["agent_mode"], "plan");

        assert!(commands(&root).iter().any(|(n, _, t)| n == "explain-simply" && t.contains("$ARGUMENTS")));

        let chat = SavedChat { id: new_chat_id(), title: "t".into(), updated: now(), model: "m".into(), history: vec![], agents: Value::Null };
        save_chat(&root, &chat).unwrap();
        assert_eq!(list_chats(&root)[0].0, chat.id);
        assert!(load_chat(&root, "../../etc/passwd").is_none(), "ids cannot leave the folder");
        // A folder that isn't a project still keeps its conversations (outside the folder).
        let plain = std::env::temp_dir().join(format!("forgecode-plain-{}", std::process::id()));
        std::fs::create_dir_all(&plain).unwrap();
        save_chat(&plain, &chat).unwrap();
        assert_eq!(list_chats(&plain)[0].0, chat.id);
        assert!(!dir(&plain).exists());
        delete_chat(&plain, &chat.id);
        std::fs::remove_dir_all(&plain).unwrap();
        delete_chat(&root, &chat.id);
        assert!(list_chats(&root).is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }
}
