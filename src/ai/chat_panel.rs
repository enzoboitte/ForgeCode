//! AI chat panel: conversation with the agent, tool activity, approvals, attachments and model choice.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{InputEvent, Paste, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::base::SelectableText;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::assets::IconName;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::i18n::t;
use serde_json::{Value, json};

use super::agent::{self, AgentEvent, Mode, Runtime};
use super::agents_view::{AgentNode, AgentTree, AgentTreeEvent, RESUME_NOTE, Status, status_icon};
use super::models::{ModelInfo, Resolver};
use super::anthropic;
use super::copilot::CopilotState;
use super::llm::ApiKind;
use super::changes::{self, FileChange};
use super::tools::{Checkpoint, SideEffect, Toolbox};
use crate::plugins::Plugins;
use crate::settings::{Settings, split_model};
use crate::util::{base64, image_media_type};

/// Text attachments larger than this are cut, with a note telling the model so.
const MAX_ATTACHMENT: usize = 100_000;

#[derive(Clone)]
enum PopupItem {
    Command(String, String),
    File(String),
    Selection(ChatContext),
    ActiveFile(PathBuf),
}

struct Popup {
    /// Where the `/…`/`@…` word starts in the composer text.
    start: usize,
    items: Vec<PopupItem>,
    selected: usize,
}

pub enum ChatEvent {
    Effect(SideEffect),
    OpenSettings,
    OpenAgents,
    /// Ask the workspace for the editor selection.
    AddSelection,
    /// Ask the workspace to show the project file picker.
    PickProjectFile,
}

#[derive(Clone, Copy, PartialEq)]
enum ToolState {
    Running,
    Done,
    Failed,
}

enum Item {
    /// `history_ix`: where the message sits in the conversation, to edit it.
    User { text: String, files: Vec<String>, history_ix: Option<usize> },
    /// Files the agent changed during a turn.
    Changes { files: Vec<FileChange>, expanded: bool },
    Assistant(String),
    Tool { name: String, summary: String, output: String, state: ToolState, expanded: bool, review: Option<(bool, String)> },
    Error(String),
    /// A sub-agent started by delegation (details in the Agents view).
    Agent { id: usize, role: String, model: String, task: String, status: Status },
    /// The security reviewer's verdict.
    Review { name: String, allowed: bool, reason: String },
}

struct Approval {
    name: String,
    args: String,
    reply: smol::channel::Sender<bool>,
}

pub struct ChatPanel {
    items: Vec<Item>,
    composer: Entity<TextareaState>,
    attachments: Vec<ChatContext>,
    /// Link the file open in the editor to the next message.
    include_active: bool,
    /// Selection offered for linking (set when the editor loses focus with a selection).
    suggested: Option<ChatContext>,
    /// `/` or `@` suggestions under the composer.
    popup: Option<Popup>,
    /// File name of this conversation in `.forgecode/chats/`.
    chat_id: String,
    /// Project files for `@` mentions, read when first needed.
    files: Option<Rc<Vec<String>>>,
    history: Vec<Value>,
    cancel: Option<Arc<AtomicBool>>,
    /// Stops and messages for single agents of the current run.
    control: Option<Arc<agent::Control>>,
    /// The mode the running agents follow; the picker changes it mid-run.
    live_mode: Arc<std::sync::Mutex<Mode>>,
    approval: Option<Approval>,
    pub workspace: PathBuf,
    pub active_file: Option<PathBuf>,
    pub agents: Entity<AgentTree>,
    /// The last run was in Plan mode and produced a plan to execute.
    plan_ready: bool,
    /// (history index of the user message, files as they were before its turn)
    checkpoints: Vec<(usize, Checkpoint)>,
    /// The user message (item index) being edited in the composer.
    editing: Option<usize>,
    /// The messages, then a footer (plan button, progress, approval): only the visible ones are
    /// laid out. Re-synced with `items` in `render`.
    list: ListState,
    /// Scroll to the end once the list knows its new rows.
    to_bottom: bool,
    _task: Option<Task<()>>,
    _sub: Subscription,
    _tree_sub: Subscription,
}

impl EventEmitter<ChatEvent> for ChatPanel {}

/// Fetch the model lists of Copilot and every configured provider.
pub fn refresh_models(cx: &mut App) {
    CopilotState::refresh(cx);
    let providers = Settings::get(cx).providers.clone();
    cx.spawn(async move |cx| {
        let lists = cx
            .background_executor()
            .spawn(async move {
                providers
                    .iter()
                    .map(|p| {
                        let url = if p.base_url.is_empty() && p.kind == ApiKind::Anthropic { anthropic::DEFAULT_URL } else { &p.base_url };
                        (p.id.clone(), super::llm::list_models(p.kind, url, &p.api_key).ok())
                    })
                    .collect::<Vec<_>>()
            })
            .await;
        cx.update(|cx| {
            Settings::update(cx, |s| {
                for (id, models) in lists {
                    if let (Some(p), Some(models)) = (s.providers.iter_mut().find(|p| p.id == id), models) {
                        p.models = models;
                    }
                }
            })
        });
    })
    .detach();
}

/// `/` commands: (name, description key, prompt template; `{}` = the rest of the message).
pub const COMMANDS: &[(&str, &str, &str)] = &[
    ("explain", "Explain the code", "Explain this code clearly and concisely. {}"),
    ("fix", "Fix the problems", "Find and fix the bugs in this code (check the diagnostics and build). {}"),
    ("tests", "Write tests", "Write unit tests for this code with the project's test framework, then run them. {}"),
    ("doc", "Add documentation", "Add concise documentation comments to this code. {}"),
    ("review", "Review the code", "Review this code for bugs, security issues and readability. List findings as file:line. {}"),
    ("commit", "Write a commit message", "Write a conventional commit message for the staged changes (run `git diff --cached`). Ask me before committing. {}"),
    ("search", "Search the web", "Search the web and summarize what you find, with links: {}"),
    ("plan", "Plan only (no changes)", "{}"),
    ("clear", "New chat", ""),
];

/// Built-in commands followed by the project's `.forgecode/commands/*.md`: (name, description, template).
pub fn all_commands(workspace: &std::path::Path) -> Vec<(String, String, String)> {
    let mut out: Vec<_> = COMMANDS.iter().map(|(n, d, tpl)| (n.to_string(), t(d).to_string(), tpl.to_string())).collect();
    out.extend(crate::dotforge::commands(workspace).into_iter().filter(|(n, _, _)| !COMMANDS.iter().any(|(b, _, _)| b == n)));
    out
}

/// Expand a leading `/command`; returns (prompt, command name). Templates use `{}` or `$ARGUMENTS`.
pub fn expand_command(text: &str, commands: &[(String, String, String)]) -> (String, Option<String>) {
    let Some(rest) = text.strip_prefix('/') else { return (text.to_string(), None) };
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    match commands.iter().find(|(n, _, _)| n == name) {
        Some((n, _, template)) => {
            let body = template.replace("$ARGUMENTS", args.trim()).replace("{}", args.trim());
            (body.trim().to_string(), Some(n.clone()))
        }
        None => (text.to_string(), None),
    }
}

/// The `/…` or `@…` word being typed at the end of `text` (byte offset, word).
pub fn trigger_word(text: &str) -> Option<(usize, &str)> {
    let start = text.rfind(char::is_whitespace).map_or(0, |i| i + 1);
    let word = &text[start..];
    match word.chars().next() {
        Some('@') => Some((start, word)),
        Some('/') if start == 0 => Some((start, word)),
        _ => None,
    }
}

/// Chat rows for a saved conversation: user messages (without inlined attachments), answers and tool calls.
fn items_from_history(history: &[Value], workspace: &std::path::Path) -> Vec<Item> {
    let mut items = vec![];
    for (history_ix, m) in history.iter().enumerate() {
        match m["role"].as_str() {
            Some("user") => {
                let text = match &m["content"] {
                    Value::String(s) => s.clone(),
                    v => v[0]["text"].as_str().unwrap_or("").to_string(),
                };
                if text.starts_with("Result of ") {
                    continue; // text-protocol tool results
                }
                let shown = ["\n\nAttached file `", "\n\nSelected code from `", RESUME_NOTE].iter().filter_map(|m| text.find(m)).min().map_or(text.as_str(), |i| &text[..i]);
                items.push(Item::User { text: shown.to_string(), files: vec![], history_ix: Some(history_ix) });
            }
            Some("assistant") => {
                if let Some(text) = m["content"].as_str().filter(|t| !t.trim().is_empty()) {
                    items.push(Item::Assistant(text.to_string()));
                }
                for call in m["tool_calls"].as_array().into_iter().flatten() {
                    let id = call["id"].as_str().unwrap_or("");
                    let output = history
                        .iter()
                        .find(|r| r["role"] == "tool" && r["tool_call_id"] == id)
                        .and_then(|r| r["content"].as_str())
                        .unwrap_or("")
                        .to_string();
                    let failed = output.starts_with("Error") || output.starts_with("Denied") || output.starts_with("The user denied");
                    items.push(Item::Tool {
                        name: call["function"]["name"].as_str().unwrap_or("").to_string(),
                        summary: self_root_relative(workspace, &summarize(call["function"]["arguments"].as_str().unwrap_or(""))),
                        output: self_root_relative(workspace, &output),
                        state: if failed { ToolState::Failed } else { ToolState::Done },
                        expanded: false,
                        review: None,
                    });
                }
            }
            _ => {}
        }
    }
    items
}

/// The chat model's backend, for one-off requests (commit messages…).
pub fn chat_backend(cx: &App) -> anyhow::Result<super::agent::Backend> {
    let settings = Settings::get(cx);
    Resolver { providers: settings.providers.clone(), copilot: CopilotState::candidates(cx) }.backend(&settings.chat_model)
}

/// Every reachable model with its profile.
pub fn catalog(cx: &App) -> Vec<ModelInfo> {
    let copilot = cx.global::<CopilotState>();
    super::models::catalog(&Settings::get(cx).providers, &copilot.models, !copilot.accounts.is_empty())
}

/// Something linked to a message, like Copilot Chat's references.
#[derive(Clone, Debug, PartialEq)]
pub enum ChatContext {
    File(PathBuf),
    /// Lines `start..=end` (1-based) of a file, as selected in the editor.
    Snippet { path: PathBuf, start: u32, end: u32, text: String },
}

impl ChatContext {
    fn label(&self) -> String {
        let name = |p: &PathBuf| p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        match self {
            ChatContext::File(p) => name(p),
            ChatContext::Snippet { path, start, end, .. } if start == end => format!("{}:{start}", name(path)),
            ChatContext::Snippet { path, start, end, .. } => format!("{}:{start}-{end}", name(path)),
        }
    }

    fn icon(&self) -> IconName {
        match self {
            ChatContext::File(p) if image_media_type(p).is_some() => IconName::FileImage,
            ChatContext::File(p) => crate::icons::file_icon(p).0,
            ChatContext::Snippet { .. } => IconName::TextCursorInput,
        }
    }
}

/// The lines an edit or a write would change, with 2 lines of context: (`+`/`-`/` `, text);
/// None for other tools or unreadable arguments. Long diffs keep their first 200 lines.
fn approval_diff(tool: &str, args: &str, workspace: &std::path::Path) -> Option<(String, Vec<(char, String)>)> {
    let v: Value = serde_json::from_str(args).ok()?;
    let path = v["path"].as_str().or(v["file_path"].as_str())?.to_string();
    let (old, new) = match tool {
        "edit_file" => (v["old"].as_str()?.to_string(), v["new"].as_str()?.to_string()),
        "write_file" => (std::fs::read_to_string(workspace.join(&path)).unwrap_or_default(), v["content"].as_str().or(v["contents"].as_str())?.to_string()),
        _ => return None,
    };
    let diff = similar::TextDiff::from_lines(&old, &new);
    let mut lines = vec![];
    for group in diff.grouped_ops(2) {
        if !lines.is_empty() {
            lines.push((' ', "…".to_string()));
        }
        for op in group {
            for change in diff.iter_changes(&op) {
                let tag = match change.tag() {
                    similar::ChangeTag::Insert => '+',
                    similar::ChangeTag::Delete => '-',
                    similar::ChangeTag::Equal => ' ',
                };
                lines.push((tag, change.value().trim_end_matches(['\n', '\r']).to_string()));
            }
        }
    }
    lines.truncate(200);
    Some((path, lines))
}

/// User message content: text plus linked files (images as data URIs) and code selections.
fn user_content(text: &str, context: &[ChatContext], workspace: &std::path::Path) -> Value {
    let mut body = text.to_string();
    let mut images = Vec::new();
    for item in context {
        let path = match item {
            ChatContext::File(p) | ChatContext::Snippet { path: p, .. } => p,
        };
        let name = path.strip_prefix(workspace).unwrap_or(path).display();
        let lang = crate::icons::fence_language(path);
        if let ChatContext::Snippet { start, end, text, .. } = item {
            body.push_str(&format!("\n\nSelected code from `{name}` (lines {start}-{end}):\n```{lang}\n{text}\n```"));
            continue;
        }
        let Ok(bytes) = std::fs::read(path) else {
            body.push_str(&format!("\n\n[could not read {name}]"));
            continue;
        };
        if let Some(mime) = image_media_type(path) {
            images.push(json!({"type": "image_url", "image_url": {"url": format!("data:{mime};base64,{}", base64(&bytes))}}));
            continue;
        }
        let mut content = String::from_utf8_lossy(&bytes).to_string();
        if content.len() > MAX_ATTACHMENT {
            let mut cut = MAX_ATTACHMENT;
            while !content.is_char_boundary(cut) {
                cut -= 1;
            }
            content.truncate(cut);
            content.push_str(&format!("\n[attachment truncated: {} KB total]", bytes.len() / 1024));
        }
        body.push_str(&format!("\n\nAttached file `{name}`:\n```{lang}\n{content}\n```"));
    }
    if images.is_empty() {
        return json!(body);
    }
    Value::Array(std::iter::once(json!({"type": "text", "text": body})).chain(images).collect())
}

fn tool_icon(name: &str) -> IconName {
    match name {
        "read_file" => IconName::Eye,
        "write_file" | "edit_file" => IconName::FileText,
        "list_dir" | "glob" => IconName::Folder,
        "grep" => IconName::Search,
        "bash" => IconName::SquareTerminal,
        "open_file" => IconName::ExternalLink,
        _ => IconName::LayoutDashboard,
    }
}

fn self_root_relative(root: &std::path::Path, s: &str) -> String {
    let root = root.display().to_string();
    s.replace(&format!("{root}/"), "").replace(&root, ".")
}

/// The one argument worth showing for a tool call (never raw JSON).
fn summarize(args: &str) -> String {
    let v: Value = serde_json::from_str(args).unwrap_or(Value::Null);
    ["path", "file_path", "filepath", "file", "filename", "command", "pattern", "query", "url", "task"]
        .iter()
        .find_map(|k| v[k].as_str())
        .map(|s| s.lines().next().unwrap_or("").to_string())
        .unwrap_or_default()
}

fn last_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.trim_end().lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

fn first_lines(s: &str, n: usize) -> String {
    let mut out: String = s.lines().take(n).collect::<Vec<_>>().join("\n");
    if s.lines().count() > n {
        out.push_str("\n…");
    }
    out
}

/// Short display name for a model reference.
fn model_label(model_ref: &str, cx: &App) -> String {
    let (provider, model) = split_model(model_ref);
    let provider = if provider == "copilot" {
        "Copilot".to_string()
    } else {
        Settings::get(cx).providers.iter().find(|p| p.id == provider).map_or(provider.to_string(), |p| p.name.clone())
    };
    // Long local model ids (`qwen3.6-35b-a3b-…-preserved`) must not push the composer out.
    let model = model.rsplit('/').next().unwrap_or(model);
    let model: String = if model.chars().count() > 26 { model.chars().take(25).chain(['…']).collect() } else { model.to_string() };
    format!("{model} | {provider}")
}

impl ChatPanel {
    pub fn new(workspace: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let composer = cx.new(|cx| {
            TextareaState::new(window, cx).auto_grow(1, 10).submit_on_enter(true).placeholder(format!("{}…  / commandes | @ fichiers", t("Ask anything")))
        });
        let _sub = cx.subscribe_in(&composer, window, |this, input, event: &InputEvent, window, cx| {
            if let InputEvent::Change = event {
                this.update_popup(cx);
            }
            if let InputEvent::PressEnter { shift: false, .. } = event {
                if this.popup.is_some() {
                    return this.apply_popup(window, cx);
                }
                let text = input.read(cx).value().to_string();
                // While editing an earlier message, sending stops the run in progress.
                if (!text.trim().is_empty() || !this.attachments.is_empty()) && (this.cancel.is_none() || this.editing.is_some()) {
                    input.update(cx, |i, cx| i.set_value("", window, cx));
                    this.send(text, cx);
                }
            }
        });
        let agents = cx.new(|_| AgentTree::default());
        let _tree_sub = cx.subscribe(&agents, |this, _, e: &AgentTreeEvent, cx| this.on_tree_event(e, cx));
        let mut this = Self {
            items: vec![],
            composer,
            attachments: vec![],
            include_active: false,
            suggested: None,
            popup: None,
            chat_id: crate::dotforge::new_chat_id(),
            files: None,
            history: vec![],
            cancel: None,
            control: None,
            live_mode: Arc::new(std::sync::Mutex::new(Mode::Ask)),
            approval: None,
            workspace,
            active_file: None,
            agents,
            plan_ready: false,
            checkpoints: vec![],
            editing: None,
            list: {
                let list = ListState::new(1, ListAlignment::Top, px(400.));
                list.set_follow_mode(FollowMode::Tail);
                list
            },
            to_bottom: false,
            _task: None,
            _sub,
            _tree_sub,
        };
        // Back where we left off: the latest conversation of this folder, its agents included.
        if !cfg!(test) {
            if let Some((id, ..)) = crate::dotforge::list_chats(&this.workspace).into_iter().next() {
                this.open_chat(&id, cx);
            }
        }
        this
    }

    /// Paths under the workspace shown relative to it.
    fn relative(&self, s: &str) -> String {
        self_root_relative(&self.workspace, s)
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.composer.read(cx).focus_handle(cx).focus(window, cx);
    }

    fn push(&mut self, item: Item, cx: &mut Context<Self>) {
        self.items.push(item);
        self.to_bottom = true;
        cx.notify();
    }

    pub fn new_chat(&mut self, cx: &mut Context<Self>) {
        self.stop(cx);
        self.agents.update(cx, |a, cx| a.clear(cx));
        self.items.clear();
        self.history.clear();
        self.attachments.clear();
        self.checkpoints.clear();
        self.editing = None;
        self.chat_id = crate::dotforge::new_chat_id();
        cx.notify();
    }

    /// Keep the conversation (`.forgecode/chats/` in projects, the config dir otherwise).
    fn save_chat(&self, cx: &App) {
        let Some(first) = self.history.iter().find(|m| m["role"] == "user") else { return };
        let text = match &first["content"] {
            Value::String(s) => s.clone(),
            v => v[0]["text"].as_str().unwrap_or("").to_string(),
        };
        let title: String = text.lines().next().unwrap_or("").chars().take(60).collect();
        let chat = crate::dotforge::SavedChat {
            id: self.chat_id.clone(),
            title,
            updated: crate::dotforge::now(),
            model: Settings::get(cx).chat_model.clone(),
            history: self.history.clone(),
            agents: serde_json::to_value(self.agents.read(cx).snapshot()).unwrap_or_default(),
        };
        if let Err(e) = crate::dotforge::save_chat(&self.workspace, &chat) {
            eprintln!("save chat: {e}");
        }
    }

    pub fn open_chat(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(chat) = crate::dotforge::load_chat(&self.workspace, id) else { return };
        self.stop(cx);
        let mut items = items_from_history(&chat.history, &self.workspace);
        let nodes: Vec<AgentNode> = serde_json::from_value(chat.agents).unwrap_or_default();
        self.agents.update(cx, |a, cx| if nodes.is_empty() { a.clear(cx) } else { a.restore(nodes, cx) });
        // The sub-agents of the last run, after the message that started it.
        let tree = self.agents.read(cx);
        let rows: Vec<Item> = tree
            .nodes
            .iter()
            .filter(|n| n.id > tree.global(0) && n.role != "security")
            .map(|n| Item::Agent { id: n.id, role: n.role.clone(), model: n.model.clone(), task: n.task.clone(), status: n.status })
            .collect();
        let at = items.iter().rposition(|i| matches!(i, Item::User { .. })).map_or(items.len(), |i| i + 1);
        items.splice(at..at, rows);
        self.items = items;
        // Other messages, maybe as many: measure them all again.
        self.list.reset(0);
        self.history = chat.history;
        self.chat_id = chat.id;
        self.attachments.clear();
        self.checkpoints.clear();
        self.editing = None;
        self.to_bottom = true;
        cx.notify();
    }

    /// A turn as `send` records it, with `work` standing in for the agent's tool calls.
    #[cfg(test)]
    pub fn fake_turn(&mut self, text: &str, work: impl FnOnce(&Toolbox), cx: &mut Context<Self>) {
        if self.history.is_empty() {
            self.history.push(json!({"role": "system", "content": "s"}));
        }
        let checkpoint = Checkpoint::default();
        let history_ix = self.history.len();
        self.checkpoints.push((history_ix, checkpoint.clone()));
        self.history.push(json!({"role": "user", "content": text}));
        self.push(Item::User { text: text.into(), files: vec![], history_ix: Some(history_ix) }, cx);
        work(&Toolbox { workspace: self.workspace.clone(), plugin_tools: vec![], allowed: None, checkpoint: Some(checkpoint) });
        self.show_changes(cx);
    }

    /// Rows as text, for tests.
    #[cfg(test)]
    pub fn rows(&self) -> Vec<String> {
        self.items.iter().map(|i| match i {
            Item::User { text, .. } => format!("user: {text}"),
            Item::Changes { files, .. } => files.iter().map(|f| format!("{} +{} -{}", f.path.file_name().unwrap().to_string_lossy(), f.added, f.removed)).collect::<Vec<_>>().join(", "),
            Item::Assistant(t) => format!("assistant: {t}"),
            Item::Agent { role, status, .. } => format!("agent {role} {status:?}"),
            Item::Error(_) => "error".into(),
            _ => "other".into(),
        }).collect()
    }

    /// Simulates quitting during a run and reopening the conversation.
    #[cfg(test)]
    pub fn save_and_reopen(&mut self, cx: &mut Context<Self>) {
        self.save_chat(cx);
        let id = self.chat_id.clone();
        self.items.clear();
        self.agents.update(cx, |a, cx| a.clear(cx));
        self.open_chat(&id, cx);
        crate::dotforge::delete_chat(&self.workspace, &id);
    }

    #[cfg(test)]
    pub fn feed(&mut self, id: usize, event: AgentEvent, cx: &mut Context<Self>) {
        self.on_agent_event(id, event, cx);
    }

    #[cfg(test)]
    pub fn edit_and_send(&mut self, item_ix: usize, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.start_edit(item_ix, window, cx);
        self.send(text.into(), cx);
    }

    #[cfg(test)]
    pub fn tree_event(&mut self, event: AgentTreeEvent, cx: &mut Context<Self>) {
        self.on_tree_event(&event, cx);
    }

    #[cfg(test)]
    pub fn push_answer(&mut self, text: &str, cx: &mut Context<Self>) {
        self.push(Item::Assistant(text.into()), cx);
    }

    fn on_tree_event(&mut self, event: &AgentTreeEvent, cx: &mut Context<Self>) {
        match event {
            AgentTreeEvent::Stop(gid) => self.stop_agent(*gid, cx),
            AgentTreeEvent::Delete(gid) => {
                self.stop_agent(*gid, cx);
                let gone = self.agents.update(cx, |a, cx| a.remove(*gid, cx));
                self.items.retain(|i| !matches!(i, Item::Agent { id, .. } if gone.contains(id)));
                self.save_chat(cx);
                cx.notify();
            }
            AgentTreeEvent::Message(gid, text) => self.message_agent(*gid, text, cx),
        }
    }

    /// Run id of tree node `gid` if it is working in the current run.
    fn running_agent(&self, gid: usize, cx: &App) -> Option<usize> {
        let tree = self.agents.read(cx);
        let running = self.cancel.is_some() && tree.get(gid).is_some_and(|n| n.status == Status::Running);
        running.then(|| tree.run_id(gid)).flatten()
    }

    fn stop_agent(&mut self, gid: usize, cx: &mut Context<Self>) {
        match self.running_agent(gid, cx) {
            None => {}
            Some(0) => self.stop(cx),
            Some(_) => {
                let ids = self.agents.read(cx).subtree(gid);
                if let Some(c) = &self.control {
                    for id in &ids {
                        if let Some(r) = self.running_agent(*id, cx) {
                            c.stop(r);
                        }
                    }
                }
                self.agents.update(cx, |a, cx| a.mark_stopped(gid, cx));
                for item in &mut self.items {
                    if let Item::Agent { id, status: status @ Status::Running, .. } = item {
                        if ids.contains(id) {
                            *status = Status::Stopped;
                        }
                    }
                }
                self.save_chat(cx);
                cx.notify();
            }
        }
    }

    /// A message typed in the Agents view: straight to the agent if it is working, otherwise to the
    /// main agent (now if a run is going, as a new chat message if not).
    fn message_agent(&mut self, gid: usize, text: &str, cx: &mut Context<Self>) {
        let Some(node) = self.agents.read(cx).get(gid).cloned() else { return };
        if let (Some(r), Some(c)) = (self.running_agent(gid, cx), &self.control) {
            c.message(r, text.to_string());
            return self.agents.update(cx, |a, cx| a.note_message(gid, text, cx));
        }
        let text = if node.parent.is_none() {
            text.to_string()
        } else {
            let task: String = node.task.chars().take(80).collect();
            format!("@{} \"{task}\": {text}", node.role)
        };
        match &self.control {
            Some(c) if self.cancel.is_some() => {
                c.message(0, text.clone());
                let main = self.agents.read(cx).global(0);
                self.agents.update(cx, |a, cx| a.note_message(main, &text, cx));
            }
            _ => self.send(text, cx),
        }
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        self.control = None;
        if let Some(cancel) = self.cancel.take() {
            cancel.store(true, Ordering::Relaxed);
            self.agents.update(cx, |a, cx| a.stop(cx));
            for item in &mut self.items {
                if let Item::Agent { status: status @ Status::Running, .. } = item {
                    *status = Status::Stopped;
                }
            }
            self.save_chat(cx);
        }
        if let Some(a) = self.approval.take() {
            let _ = a.reply.try_send(false);
        }
        cx.notify();
    }

    /// Cmd+V with an image on the clipboard: saved, attached and "[Image #n]" typed; false
    /// when there is no image (a normal paste follows).
    fn paste_image(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(saved) = crate::util::save_clipboard_image(cx) else { return false };
        match saved {
            Ok(path) => {
                self.attach([path], cx);
                let n = self.attachments.iter().filter(|a| matches!(a, ChatContext::File(p) if image_media_type(p).is_some())).count();
                self.composer.update(cx, |c, cx| c.insert(format!("[Image #{n}]"), window, cx));
            }
            Err(e) => {
                use gpui_kit::component::WindowExt as _;
                window.push_notification(gpui_kit::component::notification::Notification::error(e.to_string()), cx);
            }
        }
        true
    }

    pub fn attach(&mut self, paths: impl IntoIterator<Item = PathBuf>, cx: &mut Context<Self>) {
        for p in paths {
            let item = ChatContext::File(p.clone());
            if p.is_file() && !self.attachments.contains(&item) {
                self.attachments.push(item);
            }
        }
        cx.notify();
    }

    fn update_popup(&mut self, cx: &mut Context<Self>) {
        let state = self.composer.read(cx);
        let text = state.value().to_string();
        let cursor = state.cursor().min(text.len());
        let Some((start, word)) = trigger_word(&text[..cursor]) else {
            if self.popup.take().is_some() {
                cx.notify();
            }
            return;
        };
        let query = &word[1..];
        let items: Vec<PopupItem> = if word.starts_with('/') {
            all_commands(&self.workspace)
                .into_iter()
                .filter(|(n, _, _)| crate::lsp::provider::fuzzy_score(query, n).is_some())
                .map(|(n, d, _)| PopupItem::Command(n, d))
                .collect()
        } else {
            let files = self.files.get_or_insert_with(|| Rc::new(crate::workspace::project_files(&self.workspace))).clone();
            let mut items = vec![];
            if let Some(sel) = self.suggested.clone().filter(|_| "selection".starts_with(&query.to_lowercase())) {
                items.push(PopupItem::Selection(sel));
            }
            if let Some(active) = self.active_file.clone().filter(|_| query.is_empty()) {
                items.push(PopupItem::ActiveFile(active));
            }
            items.extend(crate::workspace::filter_files(&files, query).into_iter().take(8).map(PopupItem::File));
            items
        };
        self.popup = (!items.is_empty()).then(|| Popup { start, items, selected: 0 });
        cx.notify();
    }

    /// Insert the chosen command, or link the chosen file and remove the `@word`.
    fn apply_popup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(popup) = self.popup.take() else { return };
        let Some(item) = popup.items.get(popup.selected).cloned() else { return };
        let text = self.composer.read(cx).value().to_string();
        let start = popup.start.min(text.len());
        let new_text = match &item {
            PopupItem::Command(name, _) => format!("/{name} "),
            _ => text[..start].to_string(),
        };
        match item {
            PopupItem::File(rel) => self.attach_context(ChatContext::File(self.workspace.join(rel)), cx),
            PopupItem::Selection(sel) => self.attach_context(sel, cx),
            PopupItem::ActiveFile(p) => self.attach_context(ChatContext::File(p), cx),
            PopupItem::Command(..) => {}
        }
        self.composer.update(cx, |c, cx| c.set_value(new_text, window, cx));
        self.popup = None;
        cx.notify();
    }

    fn render_popup(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let popup = self.popup.as_ref()?;
        let theme = cx.theme();
        let rows = popup.items.iter().enumerate().map(|(ix, item)| {
            let (icon, label, detail): (IconName, String, String) = match item {
                PopupItem::Command(name, desc) => (IconName::SquareSlash, format!("/{name}"), desc.clone()),
                PopupItem::File(rel) => {
                    let (dir, name) = rel.rsplit_once('/').map_or(("", rel.as_str()), |(d, n)| (d, n));
                    (crate::icons::file_icon(std::path::Path::new(rel)).0, name.to_string(), dir.to_string())
                }
                PopupItem::Selection(sel) => (IconName::TextCursorInput, "@selection".into(), sel.label()),
                PopupItem::ActiveFile(p) => (crate::icons::file_icon(p).0, "@file".into(), p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()),
            };
            h_flex()
                .id(("popup", ix))
                .gap_2()
                .px_2()
                .py_1()
                .rounded_md()
                .text_sm()
                .cursor_pointer()
                .when(ix == popup.selected, |el| el.bg(theme.list_active))
                .hover(|s| s.bg(theme.list_hover))
                .on_click(cx.listener(move |t, _, w, cx| {
                    if let Some(p) = t.popup.as_mut() {
                        p.selected = ix;
                    }
                    t.apply_popup(w, cx);
                }))
                .child(div().text_color(theme.muted_foreground).child(icon))
                .child(label)
                .child(div().flex_1().truncate().text_xs().text_color(theme.muted_foreground).child(detail))
        });
        Some(v_flex().p_1().border_b_1().border_color(theme.border).children(rows).into_any_element())
    }

    /// Link a file or code selection to the next message.
    pub fn attach_context(&mut self, item: ChatContext, cx: &mut Context<Self>) {
        if self.suggested.as_ref() == Some(&item) {
            self.suggested = None;
        }
        if !self.attachments.contains(&item) {
            self.attachments.push(item);
        }
        cx.notify();
    }

    /// The editor selection the user just left, offered as a one-click chip.
    pub fn suggest_selection(&mut self, snippet: Option<ChatContext>, cx: &mut Context<Self>) {
        if self.suggested != snippet {
            self.suggested = snippet.filter(|s| !self.attachments.contains(s));
            cx.notify();
        }
    }

    fn pick_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: true, prompt: None });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = paths.await {
                let _ = this.update(cx, |this, cx| this.attach(paths, cx));
            }
        })
        .detach();
    }

    /// Send a user message and run the agents.
    pub fn send(&mut self, text: String, cx: &mut Context<Self>) {
        self.popup = None;
        let (text, command) = expand_command(text.trim_start(), &all_commands(&self.workspace));
        match command.as_deref() {
            Some("clear") => return self.new_chat(cx),
            Some("plan") => Settings::update(cx, |s| s.agent_mode = Mode::Plan.id().into()),
            // Code commands act on the linked code, or on the open file when nothing is linked.
            Some(c) if c != "search" && c != "commit" && self.attachments.is_empty() => {
                if let Some(sel) = self.suggested.take() {
                    self.attachments.push(sel);
                } else {
                    self.include_active = true;
                }
            }
            _ => {}
        }
        if text.is_empty() && self.attachments.is_empty() {
            return;
        }
        if let Some(item_ix) = self.editing.take() {
            self.stop(cx);
            self.rewind(item_ix, cx);
        }
        let settings = Settings::get(cx).clone();
        let mode = Mode::parse(&settings.agent_mode);
        let catalog = catalog(cx);
        let resolver = Resolver { providers: settings.providers.clone(), copilot: CopilotState::candidates(cx) };
        if let Err(e) = resolver.backend(&settings.chat_model) {
            return self.push(Item::Error(e.to_string()), cx);
        }
        let checkpoint = Checkpoint::default();
        let toolbox = Toolbox {
            workspace: self.workspace.clone(),
            plugin_tools: Plugins::global(cx).tools().map(|(p, t)| (p.dir.clone(), t.clone())).collect(),
            allowed: None,
            checkpoint: Some(checkpoint.clone()),
        };
        let active = self.active_file.as_ref().map(|p| p.strip_prefix(&self.workspace).unwrap_or(p).display().to_string());
        let prompt = agent::system_prompt(&toolbox, active.as_deref(), mode, &catalog, &settings.chat_model, settings.agent_delegation);
        let system = json!({"role": "system", "content": prompt});
        if self.history.is_empty() {
            self.history.push(system);
        } else {
            self.history[0] = system;
        }
        let mut files = std::mem::take(&mut self.attachments);
        if let Some(active) = self.active_file.clone().filter(|_| self.include_active) {
            let item = ChatContext::File(active);
            if !files.contains(&item) {
                files.insert(0, item);
            }
        }
        let history_ix = self.history.len();
        self.checkpoints.push((history_ix, checkpoint));
        // After an interrupted run, tell the model where its agents stopped so "continue" works.
        let mut content = user_content(&text, &files, &self.workspace);
        if let Some(note) = self.agents.read(cx).resume_note() {
            match &mut content {
                Value::String(s) => s.push_str(&note),
                Value::Array(parts) => parts.push(json!({"type": "text", "text": note})),
                _ => {}
            }
        }
        self.history.push(json!({"role": "user", "content": content}));
        let names = files.iter().map(ChatContext::label).collect();
        let model = settings.chat_model.clone();
        self.agents.update(cx, |a, cx| a.reset(&model, &text, history_ix, cx));
        self.plan_ready = false;
        self.push(Item::User { text, files: names, history_ix: Some(history_ix) }, cx);
        // Saved right away: a stopped or interrupted conversation still shows up in the history.
        self.save_chat(cx);

        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = Some(cancel.clone());
        let history = self.history.clone();
        let (tx, rx) = smol::channel::unbounded();
        *self.live_mode.lock().unwrap() = mode;
        let rt = Runtime::new(tx, cancel, self.live_mode.clone(), resolver, catalog, model, settings.review_model.clone(), settings.agent_delegation, settings.ai_context_chars);
        self.control = Some(rt.control.clone());
        std::thread::spawn(move || agent::run(Arc::new(rt), toolbox, history));
        self._task = Some(cx.spawn(async move |this, cx| {
            while let Ok((id, event)) = rx.recv().await {
                if this.update(cx, |this, cx| this.on_agent_event(id, event, cx)).is_err() {
                    break;
                }
            }
        }));
    }

    fn on_agent_event(&mut self, id: usize, event: AgentEvent, cx: &mut Context<Self>) {
        self.agents.update(cx, |a, cx| a.apply(id, &event, cx));
        let gid = self.agents.read(cx).global(id);
        // Sub-agents: one row each in the chat; their details live in the Agents view.
        if id != 0 {
            match event {
                AgentEvent::Spawned { role, model, task, .. } if role != "security" => {
                    self.push(Item::Agent { id: gid, role, model, task, status: Status::Running }, cx);
                    // Saved as it goes: closing the app mid-run keeps the agents (as stopped).
                    self.save_chat(cx);
                }
                AgentEvent::Finished { .. } => {
                    // Messages it had no time to read go to the main agent.
                    if let Some(c) = &self.control {
                        let role = self.agents.read(cx).get(gid).map(|n| n.role.clone()).unwrap_or_default();
                        for text in c.take_messages(id) {
                            c.message(0, format!("(for the {role} agent, which has finished) {text}"));
                        }
                    }
                    let node_status = self.agents.read(cx).nodes.iter().find(|n| n.id == gid).map(|n| n.status);
                    if let (Some(Item::Agent { status, .. }), Some(s)) = (self.items.iter_mut().find(|i| matches!(i, Item::Agent { id: a, .. } if *a == gid)), node_status) {
                        *status = s;
                    }
                    self.save_chat(cx);
                    cx.notify();
                }
                AgentEvent::Approval { name, args, reply } => {
                    self.approval = Some(Approval { name, args, reply });
                    cx.notify();
                }
                // Sub-agent verdicts are shown in the Agents view; only refusals surface here.
                AgentEvent::Review { name, allowed: false, reason } => self.push(Item::Review { name, allowed: false, reason }, cx),
                AgentEvent::Effect(effect) => self.forward_effect(effect, cx),
                _ => {}
            }
            return;
        }
        match event {
            AgentEvent::Text(delta) => {
                match self.items.last_mut() {
                    Some(Item::Assistant(text)) => text.push_str(&delta),
                    _ => self.items.push(Item::Assistant(delta)),
                }
                self.to_bottom = true;
                cx.notify();
            }
            AgentEvent::ToolStart { name, args } if name != "delegate" => {
                let summary = summarize(&args);
                let summary = self.relative(&summary);
                self.push(Item::Tool { name, summary, output: String::new(), state: ToolState::Running, expanded: false, review: None }, cx)
            }
            AgentEvent::ToolOutput(fresh) => {
                if let Some(Item::Tool { output, .. }) = self.items.iter_mut().rev().find(|i| matches!(i, Item::Tool { state: ToolState::Running, .. })) {
                    output.push_str(&fresh);
                }
                self.to_bottom = true;
                cx.notify();
            }
            AgentEvent::ToolResult { output: out } => {
                if let Some(Item::Tool { output, state, .. }) = self.items.iter_mut().rev().find(|i| matches!(i, Item::Tool { state: ToolState::Running, .. })) {
                    *state = if out.starts_with("Error") || out.starts_with("The user denied") || out.starts_with("Denied") { ToolState::Failed } else { ToolState::Done };
                    *output = self_root_relative(&self.workspace, &out);
                }
                cx.notify();
            }
            AgentEvent::Review { allowed, reason, .. } => {
                if let Some(Item::Tool { review, .. }) = self.items.iter_mut().rev().find(|i| matches!(i, Item::Tool { state: ToolState::Running, .. })) {
                    *review = Some((allowed, reason));
                }
                cx.notify();
            }
            AgentEvent::Approval { name, args, reply } => {
                self.approval = Some(Approval { name, args, reply });
                self.to_bottom = true;
                cx.notify();
            }
            AgentEvent::Effect(effect) => self.forward_effect(effect, cx),
            AgentEvent::Done(history) => {
                self.history = history;
                cx.emit(ChatEvent::Effect(SideEffect::WorkspaceChanged));
                self.cancel = None;
                self.show_changes(cx);
                self.plan_ready = Mode::parse(&Settings::get(cx).agent_mode) == Mode::Plan;
                self.save_chat(cx);
                cx.notify();
                // Messages sent from the Agents view after the main agent's last step: next turn.
                let late = self.control.take().map(|c| c.take_messages(0)).unwrap_or_default();
                if !late.is_empty() {
                    self.send(late.join("\n\n"), cx);
                }
            }
            AgentEvent::Error(error, history) => {
                self.history = history;
                cx.emit(ChatEvent::Effect(SideEffect::WorkspaceChanged));
                self.cancel = None;
                self.show_changes(cx);
                self.save_chat(cx);
                self.push(Item::Error(error), cx);
            }
            _ => {}
        }
    }

    /// Pass a tool's effect to the workspace; a file the turn just wrote shows its changes in
    /// its editor at once, with Keep / Undo, like VS Code.
    fn forward_effect(&mut self, effect: SideEffect, cx: &mut Context<Self>) {
        let written = match &effect {
            SideEffect::FileChanged(path) => Some(path.clone()),
            _ => None,
        };
        cx.emit(ChatEvent::Effect(effect));
        let Some((_, checkpoint)) = self.checkpoints.last() else { return };
        let Some(path) = written else { return };
        let files: Vec<changes::FileChange> = checkpoint.lock().map(|o| changes::summarize(&o)).unwrap_or_default().into_iter().filter(|f| f.path == path).collect();
        if !files.is_empty() {
            cx.emit(ChatEvent::Effect(SideEffect::ReviewChanges(files)));
        }
    }

    /// Summarize what the last turn changed on disk, like VS Code's chat.
    fn show_changes(&mut self, cx: &mut Context<Self>) {
        let Some((_, checkpoint)) = self.checkpoints.last() else { return };
        let files = checkpoint.lock().map(|o| changes::summarize(&o)).unwrap_or_default();
        if !files.is_empty() {
            cx.emit(ChatEvent::Effect(SideEffect::ReviewChanges(files.clone())));
            self.push(Item::Changes { files, expanded: false }, cx);
        }
    }

    /// Files changed by the turns from `history_ix` on.
    fn changed_since(&self, history_ix: usize) -> usize {
        let mut paths = std::collections::BTreeSet::new();
        for (_, cp) in self.checkpoints.iter().filter(|(ix, _)| *ix >= history_ix) {
            if let Ok(o) = cp.lock() {
                paths.extend(o.keys().cloned());
            }
        }
        paths.len()
    }

    /// Edit a previous message: put it in the composer; sending it rewinds the conversation.
    fn start_edit(&mut self, item_ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Item::User { text, .. }) = self.items.get(item_ix) else { return };
        let text = text.clone();
        self.editing = Some(item_ix);
        self.composer.update(cx, |c, cx| c.set_value(text, window, cx));
        self.focus(window, cx);
        cx.notify();
    }

    fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editing = None;
        self.composer.update(cx, |c, cx| c.set_value("", window, cx));
        cx.notify();
    }

    /// Drop the conversation from the user message `item_ix` on and undo the file changes
    /// its turn and the later ones made.
    fn rewind(&mut self, item_ix: usize, cx: &mut Context<Self>) {
        let Some(Item::User { history_ix: Some(history_ix), .. }) = self.items.get(item_ix) else { return };
        let history_ix = *history_ix;
        // Newest first, so a file changed by several turns ends as it was before the first one.
        for (_, cp) in self.checkpoints.iter().rev().filter(|(ix, _)| *ix >= history_ix) {
            let paths = cp.lock().map(|o| changes::restore(&o)).unwrap_or_default();
            for path in paths {
                cx.emit(ChatEvent::Effect(SideEffect::FileChanged(path)));
            }
        }
        self.checkpoints.retain(|(ix, _)| *ix < history_ix);
        self.history.truncate(history_ix);
        self.items.truncate(item_ix);
        self.agents.update(cx, |a, cx| a.remove_turns_from(history_ix, cx));
        self.plan_ready = false;
    }

    /// Leave Plan mode and carry out the plan, with risky actions reviewed.
    fn run_plan(&mut self, cx: &mut Context<Self>) {
        Settings::update(cx, |s| s.agent_mode = Mode::Review.id().into());
        self.send(t("Execute the plan above, step by step.").into(), cx);
    }

    /// Pick the chat mode; agents already running follow it from their next step.
    fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        Settings::update(cx, |s| s.agent_mode = mode.id().into());
        *self.live_mode.lock().unwrap() = mode;
        // Auto approves what was waiting for the user, as it would have from the start.
        if mode == Mode::Auto && self.approval.as_ref().is_some_and(|a| a.name != agent::CONTINUE) {
            self.answer(true, cx);
        }
        cx.notify();
    }

    /// The running command goes on in the background and the agent continues.
    fn to_background(&mut self, cx: &mut Context<Self>) {
        if let Some(c) = &self.control {
            c.background(0);
        }
        cx.notify();
    }

    fn answer(&mut self, allow: bool, cx: &mut Context<Self>) {
        if let Some(a) = self.approval.take() {
            let _ = a.reply.try_send(allow);
        }
        cx.notify();
    }

    fn render_item(&self, ix: usize, item: &Item, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        match item {
            Item::User { text, files, history_ix } => h_flex()
                .group("user-msg")
                .w_full()
                .justify_end()
                .items_start()
                .gap_1()
                // Edit (and rewind to) this message, like VS Code's chat.
                .when(history_ix.is_some(), |el| {
                    el.child(
                        div().invisible().group_hover("user-msg", |s| s.visible()).child(
                            Button::new(("edit-msg", ix)).ghost().xsmall().icon(IconName::Pencil).tooltip(t("Edit message"))
                                .on_click(cx.listener(move |this, _, window, cx| this.start_edit(ix, window, cx))),
                        ),
                    )
                })
                .child(
                    v_flex()
                        .max_w(relative(0.88))
                        .gap_1()
                        .px_3()
                        .py_2()
                        .rounded_lg()
                        .bg(theme.secondary)
                        .when(self.editing == Some(ix), |el| el.border_1().border_color(theme.ring))
                        .text_sm()
                        .when(!text.is_empty(), |el| el.child(SelectableText::new(("chat-user", ix), text.clone()).selection_color(theme.selection)))
                        .when(!files.is_empty(), |el| {
                            el.child(h_flex().flex_wrap().gap_1().children(files.iter().map(|f| {
                                h_flex().gap_1().px_1p5().rounded_md().bg(theme.background).text_xs().text_color(theme.muted_foreground)
                                    .child(IconName::File).child(f.clone())
                            })))
                        }),
                )
                .into_any_element(),
            Item::Changes { files, expanded } => {
                let (added, removed) = files.iter().fold((0, 0), |(a, r), f| (a + f.added, r + f.removed));
                let counts = |added: usize, removed: usize| {
                    h_flex().gap_1().font_family(theme.mono_font_family.clone())
                        .child(div().text_color(theme.green).child(format!("+{added}")))
                        .child(div().text_color(theme.red).child(format!("−{removed}")))
                };
                let label = format!("{} {}", files.len(), t(if files.len() == 1 { "file changed" } else { "files changed" }));
                v_flex()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .text_xs()
                    .child(
                        h_flex()
                            .id(("changes", ix))
                            .gap_2()
                            .px_2()
                            .py_1()
                            .cursor_pointer()
                            .hover(|s| s.bg(theme.secondary))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(Item::Changes { expanded, .. }) = this.items.get_mut(ix) {
                                    *expanded = !*expanded;
                                }
                                cx.notify();
                            }))
                            .child(div().text_color(theme.muted_foreground).child(if *expanded { IconName::ChevronDown } else { IconName::ChevronRight }))
                            .child(div().flex_1().font_weight(FontWeight::MEDIUM).child(label))
                            .child(counts(added, removed)),
                    )
                    // Every change still waiting in the editors, as VS Code's chat offers.
                    .child(
                        h_flex().px_2().pb_1().gap_1().justify_end()
                            .child(Button::new(("undo-all", ix)).xsmall().ghost().label(t("Undo all"))
                                .on_click(cx.listener(|_, _, _, cx| cx.emit(ChatEvent::Effect(SideEffect::SettleReviews(false))))))
                            .child(Button::new(("keep-all", ix)).xsmall().primary().label(t("Keep all"))
                                .on_click(cx.listener(|_, _, _, cx| cx.emit(ChatEvent::Effect(SideEffect::SettleReviews(true)))))),
                    )
                    .when(*expanded, |el| {
                        el.children(files.iter().enumerate().map(|(row, f)| {
                            let (path, before) = (f.path.clone(), f.before.clone());
                            let name = self.relative(&f.path.display().to_string());
                            h_flex()
                                .id(("changed-file", ix * 1000 + row))
                                .gap_2()
                                .px_2()
                                .py_0p5()
                                .cursor_pointer()
                                .hover(|s| s.bg(theme.secondary))
                                // Before / after, side by side, like VS Code's chat.
                                .on_click(cx.listener(move |_, _, _, cx| cx.emit(ChatEvent::Effect(SideEffect::OpenChanges(path.clone(), before.clone())))))
                                .child(div().text_color(crate::icons::file_icon(&f.path).1).child(crate::icons::file_icon(&f.path).0))
                                .child(div().flex_1().truncate().child(name))
                                .child(counts(f.added, f.removed))
                        }))
                    })
                    .into_any_element()
            }
            Item::Assistant(text) => div().debug_selector(move || format!("chat-msg-{ix}")).text_sm().child(super::markdown(("chat-msg", ix), text.clone())).into_any_element(),
            Item::Tool { name, summary, output, state, expanded, review } => {
                let (status, color) = match state {
                    ToolState::Running => (Spinner::new().xsmall().into_any_element(), theme.muted_foreground),
                    ToolState::Done => (div().child(IconName::Check).into_any_element(), theme.success),
                    ToolState::Failed => (div().child(IconName::CircleX).into_any_element(), theme.danger),
                };
                v_flex()
                    .child(
                        h_flex()
                            .id(("tool", ix))
                            .gap_2()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .text_xs()
                            .cursor_pointer()
                            .hover(|s| s.bg(theme.secondary))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(Item::Tool { expanded, .. }) = this.items.get_mut(ix) {
                                    *expanded = !*expanded;
                                }
                                cx.notify();
                            }))
                            .child(div().text_color(theme.muted_foreground).child(tool_icon(name)))
                            .child(div().font_weight(FontWeight::MEDIUM).child(name.clone()))
                            .child(div().flex_1().truncate().text_color(theme.muted_foreground).font_family(theme.mono_font_family.clone()).child(summary.clone()))
                            .when_some(review.clone(), |el, (ok, reason)| {
                                // The security reviewer's verdict, on the action it judged.
                                el.child(div().id(("review", ix)).text_color(if ok { theme.muted_foreground } else { theme.warning })
                                    .child(IconName::ShieldCheck)
                                    .tooltip(move |w, cx| gpui_kit::component::tooltip::Tooltip::new(format!("{} - {reason}", t(if ok { "Approved" } else { "Refused" }))).build(w, cx)))
                            })
                            .when(*state == ToolState::Running && name == "bash", |el| {
                                el.child(Button::new(("to-bg", ix)).ghost().xsmall().label(t("Background")).tooltip(t("Let it run in the background and go on"))
                                    .on_click(cx.listener(|this, _, _, cx| { cx.stop_propagation(); this.to_background(cx) })))
                            })
                            .child(div().text_color(color).child(status)),
                    )
                    // A running command shows its latest output as it comes.
                    .when((*expanded || *state == ToolState::Running) && !output.is_empty(), |el| {
                        el.child(
                            div()
                                .id(("tool-out", ix))
                                .ml_6()
                                .mt_1()
                                .p_2()
                                .max_h(px(220.))
                                .overflow_y_scrollbar()
                                .rounded_md()
                                .bg(theme.secondary)
                                .text_xs()
                                .font_family(theme.mono_font_family.clone())
                                .child(SelectableText::new(("tool-out-text", ix), if *state == ToolState::Running { last_lines(output, 12) } else { first_lines(output, 200) }).selection_color(theme.selection)),
                        )
                    })
                    .into_any_element()
            }
            Item::Error(e) => h_flex()
                .gap_2()
                .p_2()
                .rounded_md()
                .border_1()
                .border_color(theme.danger.opacity(0.4))
                .text_sm()
                .text_color(theme.danger)
                .child(IconName::CircleAlert)
                .child(div().flex_1().child(SelectableText::new(("chat-err", ix), e.clone()).selection_color(theme.selection)))
                .into_any_element(),
            Item::Agent { role, model, task, status, .. } => h_flex()
                .id(("agent-row", ix))
                .gap_2()
                .px_2()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .text_xs()
                .cursor_pointer()
                .hover(|s| s.bg(theme.secondary))
                .on_click(cx.listener(|_, _, _, cx| cx.emit(ChatEvent::OpenAgents)))
                .child(div().text_color(theme.primary).child(IconName::Network))
                .child(div().font_weight(FontWeight::MEDIUM).child(role.clone()))
                .child(div().text_color(theme.muted_foreground).child(model_label(&format!("x/{model}"), cx).split(" | ").next().unwrap_or("").to_string()))
                .child(div().flex_1().truncate().text_color(theme.muted_foreground).child(task.clone()))
                .child(status_icon(*status, px(14.), cx))
                .into_any_element(),
            Item::Review { name, allowed, reason } => h_flex()
                .gap_2()
                .px_2()
                .text_xs()
                .text_color(if *allowed { theme.muted_foreground } else { theme.warning })
                .child(IconName::ShieldCheck)
                .child(format!("{} | {name}", t(if *allowed { "Approved" } else { "Refused" })))
                .when(!*allowed, |el| el.child(div().flex_1().truncate().child(reason.clone())))
                .into_any_element(),
        }
    }

    fn render_mode_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = Mode::parse(&Settings::get(cx).agent_mode);
        let view = cx.entity().downgrade();
        Button::new("mode-picker").ghost().xsmall().label(t(current.label())).dropdown_menu(move |mut menu, _, _| {
            for m in Mode::ALL {
                let view = view.clone();
                menu = menu.item(PopupMenuItem::new(t(m.label())).checked(m == current).on_click(move |_, _, cx| {
                    let _ = view.update(cx, |this, cx| this.set_mode(m, cx));
                }));
            }
            menu
        })
    }

    fn render_model_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = Settings::get(cx).chat_model.clone();
        let label = model_label(&current, cx);
        let view = cx.entity().downgrade();
        Button::new("model-picker")
            .ghost()
            .xsmall()
            .label(label)
            .icon(IconName::ChevronDown)
            .dropdown_menu(move |mut menu, _, cx| {
                let current = Settings::get(cx).chat_model.clone();
                let mut groups: Vec<(String, String, Vec<String>)> = vec![];
                if !cx.global::<CopilotState>().accounts.is_empty() {
                    let mut models = cx.global::<CopilotState>().models.clone();
                    if models.is_empty() {
                        models.push(split_model(&current).1.to_string());
                    }
                    groups.push(("copilot".into(), "GitHub Copilot".into(), models));
                }
                for p in &Settings::get(cx).providers {
                    groups.push((p.id.clone(), p.name.clone(), p.models.clone()));
                }
                for (id, name, models) in groups {
                    menu = menu.label(name);
                    if models.is_empty() {
                        menu = menu.item(PopupMenuItem::new(t("No models - refresh")).disabled(true));
                    }
                    for m in models {
                        let model_ref = format!("{id}/{m}");
                        let checked = model_ref == current;
                        menu = menu.item(PopupMenuItem::new(m).checked(checked).on_click(move |_, _, cx| {
                            let model_ref = model_ref.clone();
                            Settings::update(cx, |s| s.chat_model = model_ref);
                        }));
                    }
                    menu = menu.separator();
                }
                let view = view.clone();
                menu.scrollable(true)
                    .max_h(px(420.))
                    .item(PopupMenuItem::new(t("Refresh model lists")).icon(IconName::RefreshCw).on_click(|_, _, cx| refresh_models(cx)))
                    .item(PopupMenuItem::new(t("Manage providers…")).icon(IconName::Settings).on_click(move |_, _, cx| {
                        let _ = view.update(cx, |_, cx| cx.emit(ChatEvent::OpenSettings));
                    }))
            })
    }
}

impl ChatPanel {
    /// Below the messages: the empty state, the plan button, progress and a pending approval.
    fn render_footer(&self, running: bool, waiting: bool, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        v_flex()
            .w_full()
            .px_3()
            .pt_3()
            .pb_2()
            .gap_3()
            .when(self.items.is_empty(), |el| {
                el.child(
                    v_flex().w_full().mt_16().items_center().text_center().gap_2().text_color(theme.muted_foreground)
                        .child(div().text_2xl().child(IconName::Bot))
                        .child(div().text_sm().child(t("Ask ForgeCode to explain, fix or build something.")))
                )
            })
            .when(self.plan_ready && !running, |el| {
                el.child(h_flex().child(Button::new("run-plan").small().primary().icon(IconName::Play).label(t("Run the plan"))
                    .on_click(cx.listener(|t, _, _, cx| t.run_plan(cx)))))
            })
            .when(waiting, |el| {
                el.child(h_flex().gap_2().text_xs().text_color(theme.muted_foreground).child(Spinner::new().xsmall()).child(t("Working…")))
            })
            .when_some(self.approval.as_ref().filter(|a| a.name == agent::CONTINUE), |el, _| {
                // Many steps in: go on or pause, like VS Code's "Continue to iterate?".
                el.child(
                    v_flex().gap_2().p_3().rounded_lg().border_1().border_color(theme.border).bg(theme.background)
                        .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(t("Continue to iterate?")))
                        .child(div().text_xs().text_color(theme.muted_foreground).child(t("The agent has been working on this for a while. It can go on, or you can pause and send a new message to guide it.")))
                        .child(
                            h_flex().gap_2().justify_end()
                                .child(Button::new("pause-run").small().ghost().label(t("Pause")).on_click(cx.listener(|t, _, _, cx| t.answer(false, cx))))
                                .child(Button::new("continue-run").small().primary().label(t("Continue")).on_click(cx.listener(|t, _, _, cx| t.answer(true, cx)))),
                        ),
                )
            })
            .when_some(self.approval.as_ref().filter(|a| a.name != agent::CONTINUE), |el, a| {
                el.child(
                    v_flex()
                        .gap_2()
                        .p_3()
                        .rounded_lg()
                        .border_1()
                        .border_color(theme.warning.opacity(0.6))
                        .bg(theme.background)
                        .child(h_flex().gap_2().text_sm().child(div().text_color(theme.warning).child(tool_icon(&a.name))).child(format!("Allow {}?", a.name)))
                        .child(match approval_diff(&a.name, &a.args, &self.workspace) {
                            // An edit: what changes, red and green, like VS Code.
                            Some((path, lines)) => v_flex()
                                .rounded_md()
                                .bg(theme.secondary)
                                .text_xs()
                                .font_family(theme.mono_font_family.clone())
                                .child(div().px_2().py_1().text_color(theme.muted_foreground).child(path))
                                .child(
                                    v_flex().id("approval-diff").max_h(px(320.)).overflow_y_scroll().pb_1().children(lines.into_iter().map(|(tag, line)| {
                                        let (bg, fg) = match tag {
                                            '+' => (theme.green.opacity(0.18), theme.foreground),
                                            '-' => (theme.red.opacity(0.18), theme.foreground),
                                            _ => (gpui_kit::transparent_black(), theme.muted_foreground),
                                        };
                                        div().px_2().bg(bg).text_color(fg).whitespace_nowrap().child(format!("{tag} {line}"))
                                    })),
                                )
                                .into_any_element(),
                            None => div().p_2().rounded_md().bg(theme.secondary).text_xs().font_family(theme.mono_font_family.clone()).child(first_lines(&a.args, 15)).into_any_element(),
                        })
                        .child(
                            h_flex().gap_2().justify_end()
                                .child(Button::new("deny").small().ghost().label(t("Deny")).on_click(cx.listener(|t, _, _, cx| t.answer(false, cx))))
                                .child(Button::new("allow").small().primary().label(t("Allow")).on_click(cx.listener(|t, _, _, cx| t.answer(true, cx)))),
                        ),
                )
            })
            .into_any_element()
    }
}

impl Render for ChatPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.cancel.is_some();
        let waiting = running && self.approval.is_none() && !matches!(self.items.last(), Some(Item::Assistant(_)));
        // Rows added at the end keep the measured ones; anything else is measured again.
        let rows = self.items.len() + 1;
        match self.list.item_count() {
            n if n == rows => {}
            n if n > 0 && n < rows => self.list.splice(n - 1..n - 1, rows - n),
            _ => self.list.reset(rows),
        }
        if std::mem::take(&mut self.to_bottom) {
            self.list.scroll_to_end();
        }
        let model_picker = self.render_model_picker(cx).into_any_element();
        let mode_picker = self.render_mode_picker(cx).into_any_element();
        let theme = cx.theme();
        let composer_focused = self.composer.read(cx).focus_handle(cx).is_focused(window);
        let editing = self.editing.and_then(|ix| match self.items.get(ix) {
            Some(Item::User { history_ix: Some(h), .. }) => Some(self.changed_since(*h)),
            _ => None,
        });

        v_flex()
            .size_full()
            .bg(theme.sidebar)
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| this.attach(paths.paths().to_vec(), cx)))
            .on_drop(cx.listener(|this, f: &crate::dnd::DraggedFile, _, cx| this.attach([f.0.clone()], cx)))
            .on_drop(cx.listener(|this, tab: &crate::dnd::DraggedTab, _, cx| this.attach(tab.path.clone(), cx)))
            .drag_over::<ExternalPaths>(|style, _, _, cx| style.bg(cx.theme().accent.opacity(0.3)))
            .drag_over::<crate::dnd::DraggedFile>(|style, _, _, cx| style.bg(cx.theme().accent.opacity(0.3)))
            .drag_over::<crate::dnd::DraggedTab>(|style, _, _, cx| style.bg(cx.theme().accent.opacity(0.3)))
            .child(
                h_flex()
                    .h(px(36.))
                    .px_3()
                    .gap_1()
                    .child(div().flex_1().text_xs().font_weight(FontWeight::MEDIUM).text_color(theme.muted_foreground).child(t("CHAT")))
                    .child(Button::new("chat-history").ghost().xsmall().icon(IconName::ClockArrowDown).tooltip(t("History")).dropdown_menu_with_anchor(Anchor::TopRight, {
                        let view = cx.entity().downgrade();
                        let workspace = self.workspace.clone();
                        move |mut menu, _, _| {
                            let chats = crate::dotforge::list_chats(&workspace);
                            if chats.is_empty() {
                                menu = menu.label(t("No saved conversations"));
                            }
                            for (id, title, _) in chats.into_iter().take(30) {
                                let view = view.clone();
                                let label = if title.is_empty() { id.clone() } else { title };
                                menu = menu.item(PopupMenuItem::new(label).on_click(move |_, _, cx| {
                                    let _ = view.update(cx, |t, cx| t.open_chat(&id, cx));
                                }));
                            }
                            let view = view.clone();
                            menu.separator().item(PopupMenuItem::new(t("Delete this conversation")).icon(IconName::Trash).on_click(move |_, _, cx| {
                                let _ = view.update(cx, |t, cx| {
                                    crate::dotforge::delete_chat(&t.workspace, &t.chat_id);
                                    t.new_chat(cx);
                                });
                            }))
                        }
                    }))
                    // Back to Hermes when its module is installed.
                    .when(Plugins::global(cx).agent().is_some(), |el| {
                        el.child(Button::new("use-hermes").ghost().xsmall().icon(IconName::Bot).tooltip(t("Switch to Hermes"))
                            .on_click(|_, _, cx| Settings::update(cx, |s| s.chat_engine = "agent".into())))
                    })
                    .child(Button::new("open-agents").ghost().xsmall().icon(IconName::Network).tooltip(t("Agents"))
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(ChatEvent::OpenAgents))))
                    .child(Button::new("new-chat").ghost().xsmall().icon(IconName::Plus).tooltip(t("New chat"))
                        .on_click(cx.listener(|this, _, _, cx| this.new_chat(cx)))),
            )
            .child(
                div().flex_1().min_h_0().child(
                    list(self.list.clone(), cx.processor(move |this: &mut Self, ix: usize, window, cx| {
                        match this.items.get(ix) {
                            Some(item) => div().w_full().px_3().pt_3().child(this.render_item(ix, item, cx)).into_any_element(),
                            None => this.render_footer(running, waiting, window, cx),
                        }
                    }))
                    .size_full(),
                )
                .vertical_scrollbar(&self.list),
            )
            .when_some(editing, |el, changed| {
                let note = if changed > 0 {
                    format!("{} ({changed} {})", t("Editing a previous message: sending it undoes the changes made since"), t(if changed == 1 { "file" } else { "files" }))
                } else {
                    t("Editing a previous message").to_string()
                };
                el.child(
                    h_flex().mx_2().mt_1().gap_2().px_2().py_1().rounded_md().bg(theme.secondary).text_xs().text_color(theme.muted_foreground)
                        .child(IconName::Pencil)
                        .child(div().flex_1().child(note))
                        .child(Button::new("cancel-edit").ghost().xsmall().label(t("Cancel")).on_click(cx.listener(|this, _, w, cx| this.cancel_edit(w, cx)))),
                )
            })
            .child(
                div().p_2().child(
                    v_flex()
                        .rounded_lg()
                        .border_1()
                        .border_color(if composer_focused { theme.ring } else { theme.border })
                        .bg(theme.background)
                        .child(
                            h_flex()
                                .flex_wrap()
                                .gap_1()
                                .px_2()
                                .pt_2()
                                .when_some(self.active_file.clone(), |el, active| {
                                    // The open file, linked on demand (dimmed while not included), like Copilot Chat.
                                    let on = self.include_active;
                                    let name = active.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                                    el.child(
                                        h_flex().id("ctx-active").gap_1().px_2().py_0p5().rounded_md().text_xs().cursor_pointer()
                                            .border_1()
                                            .border_color(if on { theme.border } else { theme.border.opacity(0.4) })
                                            .when(on, |el| el.bg(theme.secondary))
                                            .text_color(if on { theme.foreground } else { theme.muted_foreground })
                                            .child(crate::icons::file_icon(&active).0)
                                            .child(name)
                                            .child(div().text_color(theme.muted_foreground).child(if on { t("current file") } else { t("+ current file") }))
                                            .on_click(cx.listener(|t, _, _, cx| { t.include_active = !t.include_active; cx.notify(); })),
                                    )
                                })
                                .when_some(self.suggested.clone(), |el, sel| {
                                    el.child(
                                        h_flex().id("ctx-suggested").gap_1().px_2().py_0p5().rounded_md().text_xs().cursor_pointer()
                                            .border_1().border_dashed().border_color(theme.primary.opacity(0.6))
                                            .text_color(theme.muted_foreground)
                                            .child(IconName::TextCursorInput)
                                            .child(format!("+ {}", sel.label()))
                                            .on_click(cx.listener(move |t, _, _, cx| t.attach_context(sel.clone(), cx))),
                                    )
                                })
                                .children(self.attachments.iter().enumerate().map(|(ix, item)| {
                                    h_flex().gap_1().pl_2().rounded_md().bg(theme.secondary).text_xs()
                                        .child(item.icon())
                                        .child(item.label())
                                        .child(Button::new(("rm-attachment", ix)).ghost().xsmall().icon(IconName::X)
                                            .on_click(cx.listener(move |t, _, _, cx| { t.attachments.remove(ix); cx.notify(); })))
                                })),
                        )
                        .children(self.render_popup(cx))
                        .child(
                            div()
                                .px_1()
                                // While suggestions show, the arrows, Tab and Escape drive them, not the text.
                                .capture_key_down(cx.listener(|t, e: &KeyDownEvent, w, cx| {
                                    let Some(popup) = t.popup.as_mut() else { return };
                                    let n = popup.items.len();
                                    match e.keystroke.key.as_str() {
                                        "down" => popup.selected = (popup.selected + 1) % n,
                                        "up" => popup.selected = (popup.selected + n - 1) % n,
                                        "tab" => return { cx.stop_propagation(); t.apply_popup(w, cx) },
                                        "escape" => t.popup = None,
                                        _ => return,
                                    }
                                    cx.stop_propagation();
                                    cx.notify();
                                }))
                                .capture_action(cx.listener(|t, _: &Paste, w, cx| {
                                    if t.paste_image(w, cx) {
                                        cx.stop_propagation();
                                    }
                                }))
                                .child(Textarea::new(&self.composer).appearance(false)),
                        )
                        .child(
                            h_flex()
                                .px_1()
                                .pb_1()
                                .gap_1()
                                .child(Button::new("attach").ghost().xsmall().icon(IconName::Paperclip).tooltip(t("Attach files")).dropdown_menu({
                                    let view = cx.entity().downgrade();
                                    move |menu, _, _| {
                                        let (v1, v2, v3) = (view.clone(), view.clone(), view.clone());
                                        menu.item(PopupMenuItem::new(t("Editor selection")).icon(IconName::TextCursorInput).on_click(move |_, _, cx| {
                                                let _ = v1.update(cx, |_, cx| cx.emit(ChatEvent::AddSelection));
                                            }))
                                            .item(PopupMenuItem::new(t("Project file…")).icon(IconName::FileSearch).on_click(move |_, _, cx| {
                                                let _ = v2.update(cx, |_, cx| cx.emit(ChatEvent::PickProjectFile));
                                            }))
                                            .item(PopupMenuItem::new(t("Files from disk…")).icon(IconName::FolderOpen).on_click(move |_, w, cx| {
                                                let _ = v3.update(cx, |t, cx| t.pick_files(w, cx));
                                            }))
                                    }
                                }))
                                .child(mode_picker)
                                .child(h_flex().flex_1().min_w_0().overflow_hidden().justify_start().child(model_picker))
                                .child(if running {
                                    Button::new("stop-agent").small().icon(IconName::Square).tooltip(t("Stop")).on_click(cx.listener(|t, _, _, cx| t.stop(cx)))
                                } else {
                                    let composer = self.composer.clone();
                                    Button::new("send").small().primary().icon(IconName::ArrowUp).tooltip("Send (⏎)").on_click(cx.listener(move |t, _, w, cx| {
                                        let text = composer.read(cx).value().to_string();
                                        if !text.trim().is_empty() || !t.attachments.is_empty() {
                                            composer.update(cx, |i, cx| i.set_value("", w, cx));
                                            t.send(text, cx);
                                        }
                                    }))
                                }),
                        ),
                ),
            )
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn approval_shows_what_an_edit_changes() {
        let args = serde_json::json!({"path": "a.cpp", "old": "int a;\nint b;\n", "new": "int a;\nlong b;\n"}).to_string();
        let (path, lines) = super::approval_diff("edit_file", &args, std::path::Path::new("/nowhere")).unwrap();
        assert_eq!(path, "a.cpp");
        assert_eq!(lines, [(' ', "int a;".to_string()), ('-', "int b;".into()), ('+', "long b;".into())]);
        assert!(super::approval_diff("bash", "{\"command\": \"ls\"}", std::path::Path::new("/")).is_none());
    }

    use super::{ChatContext, user_content};

    #[test]
    fn commands_and_mentions() {
        use super::{expand_command, trigger_word};
        let cmds = vec![
            ("fix".to_string(), String::new(), "Fix it. {}".to_string()),
            ("simple".to_string(), String::new(), "# Simple\nExplain simply. $ARGUMENTS".to_string()),
        ];
        assert_eq!(expand_command("/fix the loop", &cmds), ("Fix it. the loop".to_string(), Some("fix".to_string())));
        assert_eq!(expand_command("/simple main.rs", &cmds).0, "# Simple\nExplain simply. main.rs");
        assert_eq!(expand_command("/unknown x", &cmds).1, None);
        assert_eq!(expand_command("hello", &cmds).0, "hello");
        assert_eq!(trigger_word("look at @mai"), Some((8, "@mai")));
        assert_eq!(trigger_word("/ex"), Some((0, "/ex")));
        assert_eq!(trigger_word("a /ex"), None, "commands only at the start");
        assert_eq!(trigger_word("plain"), None);
    }

    #[test]
    fn saved_history_becomes_chat_rows() {
        use serde_json::json;
        let h = vec![
            json!({"role": "system", "content": "s"}),
            json!({"role": "user", "content": "do it\n\nAttached file `a.rs`:\n```\nx\n```"}),
            json!({"role": "assistant", "content": "", "tool_calls": [{"id": "1", "function": {"name": "read_file", "arguments": "{\"path\":\"/w/a.rs\"}"}}]}),
            json!({"role": "tool", "tool_call_id": "1", "content": "ok"}),
            json!({"role": "assistant", "content": "done"}),
        ];
        let items = super::items_from_history(&h, std::path::Path::new("/w"));
        assert!(matches!(&items[0], super::Item::User { text, .. } if text == "do it"));
        assert!(matches!(&items[1], super::Item::Tool { summary, output, .. } if summary == "a.rs" && output == "ok"));
        assert!(matches!(&items[2], super::Item::Assistant(t) if t == "done"));
    }

    #[test]
    fn attachments_become_text_and_image_parts() {
        let dir = std::env::temp_dir().join(format!("forge-chat-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "fn a() {}").unwrap();
        std::fs::write(dir.join("i.png"), [0x89, b'P', b'N', b'G']).unwrap();
        assert!(user_content("hi", &[ChatContext::File(dir.join("a.rs"))], &dir).as_str().unwrap().contains("`a.rs`:\n```rust\nfn a() {}"));
        let snippet = ChatContext::Snippet { path: dir.join("a.rs"), start: 3, end: 4, text: "x();".into() };
        assert!(user_content("hi", &[snippet], &dir).as_str().unwrap().contains("`a.rs` (lines 3-4):\n```rust\nx();"));
        let v = user_content("hi", &[ChatContext::File(dir.join("i.png"))], &dir);
        assert!(v[1]["image_url"]["url"].as_str().unwrap().starts_with("data:image/png;base64,"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
