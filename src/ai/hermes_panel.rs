//! The AI panel when an agent module (Hermes) is installed: ForgeCode installs the agent in the
//! background, starts it as an ACP server wired to the local gateway (Copilot and the local
//! models allowed in Settings), and shows its work: streamed answer, thoughts, tool calls, plan,
//! and permission cards (with the diff) the user answers. One conversation per project is kept
//! and resumed with its whole context.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Escape, InputEvent, MoveDown, MoveUp, Paste, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::Value;

use super::acp::{AcpClient, AcpEvent, PermissionOption, Session};
use super::copilot::CopilotState;
use super::gateway::{self, Gateway, Routes};
use super::models::Resolver;
use crate::i18n::t;
use crate::plugins::{Plugins, agent_runtime};
use crate::settings::Settings;

pub enum HermesEvent {
    /// The agent wrote this file: reload it if it is open.
    FileChanged(PathBuf),
    /// Back to the built-in chat.
    UseClassicChat,
    OpenSettings,
}

impl EventEmitter<HermesEvent> for HermesPanel {}

#[derive(Clone, PartialEq)]
enum State {
    /// No agent module installed.
    NoModule,
    Installing(Vec<String>),
    /// No model allowed for agents yet.
    NeedsModels,
    Starting,
    Ready,
    Failed(String),
}

enum Entry {
    User(String),
    Assistant(String),
    Thought(String),
    Tool { id: String, title: String, kind: String, status: String, output: String },
    Plan(Vec<(String, String)>),
    Permission { request: Value, title: String, diff: Option<(String, String, String)>, command: String, options: Vec<PermissionOption>, answer: Option<String> },
    Error(String),
    /// The supervisor's reading of the turn: `None` while it reads, then its verdict.
    Review(Option<Review>),
}

enum Review {
    Ok,
    Issues(String),
    Failed(String),
}

pub struct HermesPanel {
    root: PathBuf,
    state: State,
    client: Option<Arc<AcpClient>>,
    session: Option<Session>,
    entries: Vec<Entry>,
    running: bool,
    composer: Entity<TextareaState>,
    /// The conversation, virtualized: only the entries on screen are built and laid out,
    /// whatever its length. One item per entry, plus the footer (status, activity).
    list: ListState,
    /// Entries built since the panel exists (tests check that off-screen ones are skipped).
    #[cfg(test)]
    built: std::cell::Cell<usize>,
    /// Scroll of the graph view.
    scroll: ScrollHandle,
    /// Thoughts unfolded by the user.
    open_thoughts: std::collections::HashSet<usize>,
    /// Messages written while Hermes works, sent in turn when it's done.
    queue: Vec<String>,
    /// The queued message being edited in the composer (↑/↓ or a click on it).
    editing: Option<usize>,
    /// Files joined to the next message.
    attachments: Vec<PathBuf>,
    /// Images pasted so far, for their "[Image #n]" labels.
    pasted_images: usize,
    /// The conversations and sub-agents graph instead of the conversation.
    show_graph: bool,
    graph: Vec<super::hermes_graph::Node>,
    graph_error: Option<String>,
    /// The project's earlier conversations (id, title), newest first, for the history menu.
    history: Vec<(String, String)>,
    _graph_task: Option<Task<()>>,
    /// The supervisor is reading the turn that just ended.
    supervising: bool,
    /// What the supervisor is writing to Hermes, and how much of it is on screen.
    supervisor_typing: Option<(String, usize)>,
    /// Corrections the supervisor sent in a row (reset by the user's own messages): it stops
    /// at [`MAX_SUPERVISOR_ROUNDS`] and leaves the user to decide.
    supervisor_rounds: usize,
    _supervisor_task: Option<Task<()>>,
    _tasks: Vec<Task<()>>,
    _subs: Vec<Subscription>,
}

/// Messages the supervisor sends start with this (they don't reset its round count).
const SUPERVISOR_PREFIX: &str = "[Supervisor] ";
const MAX_SUPERVISOR_ROUNDS: usize = 2;

/// ForgeCode's "safe auto" mode (see [`HermesPanel::set_mode`]).
const AUTO_SAFE: &str = "auto_safe";

/// Short model name for the picker: `custom:lm/qwen3-coder` → `qwen3-coder`.
fn short_model(id: &str) -> String {
    let id = id.strip_prefix("custom:").unwrap_or(id);
    id.rsplit('/').next().unwrap_or(id).to_string()
}

/// Sent on the install log once the agent is installed and starting.
const STARTING: &str = "\u{0}starting";

/// Standing instruction: a new conversation still knows the earlier ones.
const HISTORY_GUIDANCE: &str = "Earlier conversations with the user are kept. When the user refers to earlier work (\"like before\", \"the bug from yesterday\"), or a new conversation continues an ongoing task, search them with `session_search` before asking the user to repeat themselves.";

/// Where the project's conversation id is kept, to resume it.
fn session_file(root: &Path) -> PathBuf {
    crate::dotforge::dir(root).join("agent-session")
}

impl HermesPanel {
    pub fn new(root: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let composer = cx.new(|cx| TextareaState::new(window, cx).auto_grow(1, 10).submit_on_enter(true).placeholder(t("Ask Hermes…")));
        let sub = cx.subscribe_in(&composer, window, |this, input, event: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { shift: false, secondary } = event {
                // Without the line break Enter may leave behind.
                let text = input.read(cx).value().trim_end_matches(['\n', '\r']).to_string();
                // Enter on a queued message being edited saves it (⌘Enter sends it now).
                if this.editing.is_some() {
                    return this.finish_edit(true, *secondary, window, cx);
                }
                if (!text.trim().is_empty() || !this.attachments.is_empty()) && this.state == State::Ready {
                    input.update(cx, |i, cx| i.set_value("", window, cx));
                    if *secondary { this.force(text, cx) } else { this.submit(text, cx) }
                }
            }
        });
        let settings_sub = cx.observe_global::<Settings>(|this, cx| this.refresh_routes(cx));
        // Quitting ForgeCode stops its agent (else it outlives us and holds the conversation).
        let quit_sub = cx.on_app_quit(|this, _| {
            if let Some(client) = this.client.take() {
                client.kill();
            }
            async {}
        });
        let plugins_sub = cx.observe_global::<Plugins>(|this, cx| {
            // Only when the module appears or disappears: plugins reload often.
            let has = Plugins::global(cx).agent().is_some();
            if (this.state == State::NoModule) == has {
                this.restart(cx);
            }
        });
        let mut this = Self {
            root,
            state: State::NoModule,
            client: None,
            session: None,
            entries: vec![],
            running: false,
            composer,
            list: {
                let list = ListState::new(1, ListAlignment::Top, px(600.));
                list.set_follow_mode(FollowMode::Tail);
                list
            },
            #[cfg(test)]
            built: Default::default(),
            scroll: ScrollHandle::new(),
            open_thoughts: Default::default(),
            queue: vec![],
            editing: None,
            attachments: vec![],
            pasted_images: 0,
            show_graph: false,
            graph: vec![],
            graph_error: None,
            history: vec![],
            _graph_task: None,
            supervising: false,
            supervisor_typing: None,
            supervisor_rounds: 0,
            _supervisor_task: None,
            _tasks: vec![],
            _subs: vec![sub, settings_sub, plugins_sub, quit_sub],
        };
        this.restart(cx);
        this
    }

    pub fn set_root(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        // Same project: keep the running agent (and the install or start in progress).
        if self.root == root {
            return;
        }
        self.root = root;
        self.restart(cx);
    }

    fn state_name(&self) -> &'static str {
        match self.state {
            State::NoModule => "no-module",
            State::Installing(_) => "installing",
            State::NeedsModels => "needs-models",
            State::Starting => "starting",
            State::Ready => "ready",
            State::Failed(_) => "failed",
        }
    }

    pub fn has_agent(cx: &App) -> bool {
        Plugins::global(cx).agent().is_some()
    }

    /// Models the agent may use, as served by the gateway.
    fn routes(cx: &App) -> Routes {
        let settings = Settings::get(cx);
        let resolver = Resolver { providers: settings.providers.clone(), copilot: CopilotState::candidates(cx) };
        let allowed = settings.agent_models.iter().filter(|m| gateway::servable(m, &resolver)).cloned().collect();
        Routes { resolver, allowed }
    }

    fn refresh_routes(&mut self, cx: &mut Context<Self>) {
        let routes = Self::routes(cx);
        if self.state == State::NeedsModels && !routes.allowed.is_empty() {
            return self.restart(cx);
        }
        if let Some(g) = gateway_if_started() {
            g.update(routes);
        }
    }

    /// (Re)start: install if needed, then launch the agent and open the project's conversation.
    pub fn restart(&mut self, cx: &mut Context<Self>) {
        if let Some((plugin, _)) = Plugins::global(cx).agent() {
            let log = agent_runtime::agent_home(plugin).join("logs").join("forgecode-panel.log");
            let _ = std::fs::create_dir_all(log.parent().unwrap());
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log) {
                use std::io::Write as _;
                let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
                let _ = writeln!(f, "{}.{:03} restart root={} previous state={}", t.as_secs(), t.subsec_millis(), self.root.display(), self.state_name());
            }
        }
        // The previous agent must stop: a turn in progress keeps the client alive, and two
        // agents on one conversation make the new one wait for the old one forever.
        if let Some(client) = self.client.take() {
            if let Some(session) = &self.session {
                client.cancel(&session.id);
            }
            client.kill();
        }
        self.session = None;
        self.running = false;
        self.queue.clear();
        self.editing = None;
        self._tasks.clear();
        let Some((plugin, spec)) = Plugins::global(cx).agent().map(|(p, s)| (p.clone(), s.clone())) else {
            self.state = State::NoModule;
            return cx.notify();
        };
        let routes = Self::routes(cx);
        if routes.allowed.is_empty() {
            self.state = State::NeedsModels;
            return cx.notify();
        }
        let settings = Settings::get(cx);
        let model = Some(settings.agent_model.clone()).filter(|m| routes.allowed.contains(m)).unwrap_or_else(|| routes.allowed[0].clone());
        let context = settings.agent_context_tokens;
        self.state = State::Starting;
        cx.notify();

        // The gateway first: the databases tools need its confirmation endpoint.
        if let Err(e) = Gateway::get(routes.clone()) {
            self.state = State::Failed(e.to_string());
            return cx.notify();
        }
        // The agent's extra tools: the project's databases, and the web.
        let mcp: Vec<Value> = crate::workspace::databases::agent_mcp_server(&self.root, cx).into_iter().chain(super::web_mcp::server_config()).collect();
        let (events_tx, events_rx) = smol::channel::unbounded();
        let (log_tx, log_rx) = smol::channel::unbounded::<String>();
        let root = self.root.clone();
        let resume = std::fs::read_to_string(session_file(&root)).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let start = cx.background_spawn(async move {
            // The user's own install, or the module's, before installing anything (checked here:
            // it runs the agent once).
            if !agent_runtime::is_installed(&plugin, &spec) {
                let _ = log_tx.try_send(spec.install_note.clone());
                agent_runtime::install(&plugin, &spec, |line| {
                    let _ = log_tx.try_send(line.to_string());
                })?;
            }
            let gateway = Gateway::get(routes)?;
            let home = agent_runtime::agent_home(&plugin);
            agent_runtime::write_config(&home, &agent_runtime::hermes_config(&gateway.base_url(), &gateway.secret, &model, context))?;
            // Standing instruction: look things up on the web rather than guess.
            let _ = agent_runtime::write_guidance(&home, &format!("{}\n\n{HISTORY_GUIDANCE}", super::web_mcp::GUIDANCE));
            let _ = log_tx.try_send(STARTING.into());
            agent_runtime::stop_orphan(&plugin);
            agent_runtime::repoint_sessions(&plugin, &gateway.base_url());
            let (argv, env) = agent_runtime::launch(&plugin, &spec);
            let trace = agent_runtime::agent_home(&plugin).join("logs").join("forgecode-acp.log");
            let client = AcpClient::start_traced(&argv, &root, &env, events_tx, Some(trace.clone()))?;
            agent_runtime::record_agent(&plugin, client.pid());
            // The project's conversation, with its context; a new one when it can't be reopened.
            let open = async {
                match resume {
                    Some(id) => match client.load_session(&id, &root, &mcp).await {
                        Ok(s) => Ok(s),
                        Err(_) => client.new_session(&root, &mcp).await,
                    },
                    None => client.new_session(&root, &mcp).await,
                }
            };
            let timeout = async {
                smol::Timer::after(std::time::Duration::from_secs(90)).await;
                let tail: String = std::fs::read_to_string(&trace).unwrap_or_default().lines().rev().take(8).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
                Err(anyhow::anyhow!("Hermes did not open the conversation within 90 s.\n{}\n{tail}", trace.display()))
            };
            let session = smol::future::or(open, timeout).await?;
            anyhow::Ok((client, session))
        });
        let install_log = cx.spawn(async move |this, cx| {
            while let Ok(line) = log_rx.recv().await {
                let _ = this.update(cx, |this, cx| {
                    // Installed: now starting.
                    if line == STARTING {
                        if matches!(this.state, State::Installing(_)) {
                            this.state = State::Starting;
                            cx.notify();
                        }
                        return;
                    }
                    if this.state == State::Starting {
                        this.state = State::Installing(vec![]);
                    }
                    if let State::Installing(lines) = &mut this.state {
                        lines.push(line);
                        if lines.len() > 8 {
                            lines.remove(1);
                        }
                        cx.notify();
                    }
                });
            }
        });
        let started = cx.spawn(async move |this, cx| {
            let result = start.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok((client, session)) => {
                        let _ = std::fs::create_dir_all(crate::dotforge::dir(&this.root));
                        let _ = std::fs::write(session_file(&this.root), &session.id);
                        this.client = Some(client);
                        this.session = Some(session);
                        this.state = State::Ready;
                        let mode = Settings::get(cx).agent_permission_mode.clone();
                        this.set_mode(mode, cx);
                        this.refresh_history(cx);
                    }
                    Err(e) => this.state = State::Failed(e.to_string()),
                }
                cx.notify();
            });
        });
        let events = cx.spawn(async move |this, cx| {
            while let Ok(event) = events_rx.recv().await {
                if this.update(cx, |this, cx| this.on_event(event, cx)).is_err() {
                    break;
                }
            }
        });
        self._tasks = vec![install_log, started, events];
    }

    /// A blank conversation; the current one stays in the history menu.
    fn new_conversation(&mut self, cx: &mut Context<Self>) {
        let _ = std::fs::remove_file(session_file(&self.root));
        self.entries.clear();
        self.attachments.clear();
        self.show_graph = false;
        self._graph_task = None;
        self.restart(cx);
    }

    /// The project's conversations as Hermes keeps them (its sessions database).
    fn refresh_history(&mut self, cx: &mut Context<Self>) {
        let Some((plugin, _)) = Plugins::global(cx).agent().map(|(p, s)| (p.clone(), s.clone())) else { return };
        let python = super::hermes_graph::python_for(&agent_runtime::checkout(&plugin));
        let db = agent_runtime::agent_home(&plugin).join("state.db");
        let root = self.root.clone();
        cx.spawn(async move |this, cx| {
            let nodes = cx.background_spawn(async move { super::hermes_graph::read_rows(&python, &db).map(|rows| super::hermes_graph::build(&rows, &root)) }).await;
            if let Ok(mut nodes) = nodes {
                nodes.sort_by(|a, b| b.started_at.total_cmp(&a.started_at));
                let _ = this.update(cx, |this, cx| {
                    this.history = nodes.into_iter().filter(|n| n.messages > 0).take(30).map(|n| (n.id, n.title)).collect();
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Send now, or after the current turn when Hermes is working.
    fn submit(&mut self, text: String, cx: &mut Context<Self>) {
        if self.running {
            self.queue.push(text);
            return cx.notify();
        }
        self.send(text, cx);
    }

    /// Send now, interrupting Hermes when it is working: its turn is stopped and this message
    /// goes first, before the other queued ones.
    fn force(&mut self, text: String, cx: &mut Context<Self>) {
        if !self.running {
            return self.send(text, cx);
        }
        self.queue.insert(0, text);
        if let Some(i) = &mut self.editing {
            *i += 1;
        }
        self.stop(cx);
    }

    /// The next queued message, when Hermes is free and nothing queued is being edited.
    fn drain_queue(&mut self, cx: &mut Context<Self>) {
        if !self.running && self.editing.is_none() && !self.queue.is_empty() && self.client.is_some() {
            let next = self.queue.remove(0);
            self.send(next, cx);
        }
    }

    /// Edit queued message `i` in the composer (what was being edited is saved first).
    fn edit_queued(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        if i >= self.queue.len() {
            return;
        }
        self.save_edit(cx);
        self.editing = Some(i);
        let text = self.queue[i].clone();
        // Cursor at the end, as in a shell's history.
        let end = gpui_kit::component::input::Position::new(text.split('\n').count().saturating_sub(1) as u32, text.rsplit('\n').next().unwrap_or("").chars().count() as u32);
        self.composer.update(cx, |c, cx| {
            c.set_value(text, window, cx);
            c.set_cursor_position(end, window, cx);
        });
        cx.notify();
    }

    /// The composer's text back into the message being edited (an emptied one is kept as it was).
    fn save_edit(&mut self, cx: &mut Context<Self>) {
        if let Some(i) = self.editing {
            let text = self.composer.read(cx).value().trim_end_matches(['\n', '\r']).to_string();
            if let Some(q) = self.queue.get_mut(i)
                && !text.trim().is_empty()
            {
                *q = text;
            }
        }
    }

    /// Stop editing: keep the text (`save`) or drop the changes; `now` sends that message at once.
    fn finish_edit(&mut self, save: bool, now: bool, window: &mut Window, cx: &mut Context<Self>) {
        if save {
            self.save_edit(cx);
        }
        let edited = self.editing.take();
        self.composer.update(cx, |c, cx| c.set_value("", window, cx));
        if now && let Some(i) = edited.filter(|i| *i < self.queue.len()) {
            let text = self.queue.remove(i);
            self.force(text, cx);
        }
        self.drain_queue(cx);
        cx.notify();
    }

    /// ↑ / ↓ in the composer walk through the queued messages (like a shell's history): ↑ in an
    /// empty composer edits the last one; ↑ on the first line goes to the previous one, ↓ on the
    /// last line to the next one, and past the last back to an empty composer. False when the
    /// key should move the cursor as usual.
    fn step_queue(&mut self, up: bool, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let composer = self.composer.read(cx);
        let text = composer.value().to_string();
        let line = composer.cursor_position().line as usize;
        let last_line = text.split('\n').count().saturating_sub(1);
        match (self.editing, up) {
            (None, true) if text.trim().is_empty() && !self.queue.is_empty() => self.edit_queued(self.queue.len() - 1, window, cx),
            (Some(i), true) if line == 0 => {
                if i > 0 {
                    self.edit_queued(i - 1, window, cx);
                }
            }
            (Some(i), false) if line >= last_line => {
                if i + 1 < self.queue.len() {
                    self.edit_queued(i + 1, window, cx);
                } else {
                    self.finish_edit(true, false, window, cx);
                }
            }
            _ => return false,
        }
        true
    }

    fn remove_queued(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        if i >= self.queue.len() {
            return;
        }
        match self.editing {
            Some(e) if e == i => {
                self.editing = None;
                self.composer.update(cx, |c, cx| c.set_value("", window, cx));
            }
            Some(e) if e > i => self.editing = Some(e - 1),
            _ => {}
        }
        self.queue.remove(i);
        self.drain_queue(cx);
        cx.notify();
    }

    fn send(&mut self, text: String, cx: &mut Context<Self>) {
        let (Some(client), Some(session)) = (self.client.clone(), self.session.clone()) else { return };
        if !text.starts_with(SUPERVISOR_PREFIX) {
            self.supervisor_rounds = 0;
        }
        // Attached files as links: Hermes reads them (text, images) itself.
        let files = std::mem::take(&mut self.attachments);
        let images = client.accepts_images();
        let mut blocks = vec![serde_json::json!({"type": "text", "text": text})];
        blocks.extend(files.iter().map(|p| attachment_block(p, images)));
        let shown = if files.is_empty() {
            text.clone()
        } else {
            let names: Vec<String> = files.iter().map(|p| p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()).collect();
            format!("{text}\n\n{} : {}", t("Attachments"), names.join(", "))
        };
        self.entries.push(Entry::User(shown));
        self.running = true;
        self.follow();
        cx.notify();
        let task = cx.background_spawn(async move { client.prompt_blocks(&session.id, blocks).await });
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.running = false;
                match result {
                    Ok(reason) if reason == "max_turn_requests" || reason == "max_tokens" => {
                        this.entries.push(Entry::Error(format!("{} ({reason}). {}", t("Hermes stopped"), t("Send \"continue\" to go on: the conversation keeps its context."))))
                    }
                    Ok(reason) if reason == "cancelled" => {}
                    Ok(_) => this.supervise(cx),
                    Err(e) => this.entries.push(Entry::Error(e.to_string())),
                }
                this.drain_queue(cx);
                cx.notify();
            });
        }));
    }

    /// The supervisor reads the turn that just ended (when it did something worth checking).
    fn supervise(&mut self, cx: &mut Context<Self>) {
        let settings = Settings::get(cx);
        if !settings.agent_supervisor {
            return;
        }
        let Some(start) = self.entries.iter().rposition(|e| matches!(e, Entry::User(_))) else { return };
        let Entry::User(request) = &self.entries[start] else { return };
        let steps: Vec<String> = self.entries[start + 1..]
            .iter()
            .filter_map(|e| match e {
                Entry::Assistant(text) => Some(format!("AGENT SAYS:\n{text}")),
                Entry::Tool { title, kind, status, output, .. } => Some(format!("TOOL [{kind}] {title} → {status}{}", if output.is_empty() { String::new() } else { format!("\n{output}") })),
                Entry::Plan(steps) => Some(format!("PLAN: {}", steps.iter().map(|(s, st)| format!("{s} ({st})")).collect::<Vec<_>>().join("; "))),
                Entry::Permission { title, answer, .. } => Some(format!("ASKED PERMISSION: {title} → {}", answer.as_deref().unwrap_or("no answer"))),
                Entry::Error(e) => Some(format!("ERROR: {e}")),
                Entry::User(_) | Entry::Thought(_) | Entry::Review(_) => None,
            })
            .collect();
        // A plain chat answer with no tool used: nothing to verify.
        if !self.entries[start + 1..].iter().any(|e| matches!(e, Entry::Tool { .. })) {
            return;
        }
        let model = Some(settings.agent_supervisor_model.clone())
            .filter(|m| !m.is_empty())
            .or_else(|| self.session.as_ref().map(|s| s.current_model.strip_prefix("custom:").unwrap_or(&s.current_model).to_string()))
            .unwrap_or_default();
        let transcript = super::supervisor::transcript(request, &steps);
        self.entries.push(Entry::Review(None));
        self.supervising = true;
        let ix = self.entries.len() - 1;
        let task = cx.background_spawn(async move { super::supervisor::review(&model, &transcript) });
        self._supervisor_task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.supervising = false;
                let issues = result.as_ref().ok().and_then(|r| r.clone());
                if let Some(Entry::Review(slot)) = this.entries.get_mut(ix) {
                    *slot = Some(match result {
                        Ok(None) => Review::Ok,
                        Ok(Some(issues)) => Review::Issues(issues),
                        Err(e) => Review::Failed(e.to_string()),
                    });
                    this.list.remeasure_items(ix..ix + 1);
                }
                // It writes its corrections to Hermes itself, a limited number of times in a row.
                if let Some(issues) = issues
                    && this.supervisor_rounds < MAX_SUPERVISOR_ROUNDS
                    && this.state == State::Ready
                {
                    this.supervisor_write(supervisor_message(&issues), cx);
                }
                cx.notify();
            });
        }));
    }

    /// While Hermes works: the same tool failing the same way again and again is a loop it won't
    /// leave alone; the supervisor steps in at once (no need to wait for the end of the turn).
    fn watch_loop(&mut self, cx: &mut Context<Self>) {
        if !self.running || self.supervisor_typing.is_some() || self.supervisor_rounds >= MAX_SUPERVISOR_ROUNDS || !Settings::get(cx).agent_supervisor {
            return;
        }
        let Some((title, error, count)) = repeated_failure(&self.entries) else { return };
        if count < LOOP_FAILURES {
            return;
        }
        let issues = format!(
            "- `{title}` : {count} {}\n  `{}`\n- {}",
            t("identical failures in a row:"),
            error.lines().next().unwrap_or(""),
            t("Repeating the same call will not work. Read the error and change approach. If the call arrives without an argument (e.g. \"path required\"), it was cut off because it is too long: write the file in several smaller steps, giving `path` first, or edit only the lines that change.")
        );
        self.supervisor_write(supervisor_message(&issues), cx);
    }

    /// The supervisor types `text` in the composer (shown character by character), then sends it,
    /// stopping Hermes if it is working. [`Self::stop_supervisor`] cancels it.
    fn supervisor_write(&mut self, text: String, cx: &mut Context<Self>) {
        self.supervisor_typing = Some((text, 0));
        cx.notify();
        self._supervisor_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(std::time::Duration::from_millis(16)).await;
                let done = this.update(cx, |this, cx| {
                    let Some((text, shown)) = &mut this.supervisor_typing else { return true };
                    // A few characters per frame: a long message still takes about two seconds.
                    let step = (text.len() / 120).max(2);
                    let mut next = (*shown + step).min(text.len());
                    while !text.is_char_boundary(next) {
                        next += 1;
                    }
                    *shown = next;
                    cx.notify();
                    next >= text.len()
                });
                match done {
                    Ok(true) => break,
                    Ok(false) => {}
                    Err(_) => return,
                }
            }
            cx.background_executor().timer(std::time::Duration::from_millis(400)).await;
            let _ = this.update(cx, |this, cx| {
                if let Some((text, _)) = this.supervisor_typing.take() {
                    this.supervisor_rounds += 1;
                    this.force(text, cx);
                }
                cx.notify();
            });
        }));
    }

    /// The user takes over: the supervisor stops reading or writing.
    fn stop_supervisor(&mut self, cx: &mut Context<Self>) {
        self._supervisor_task = None;
        self.supervisor_typing = None;
        self.supervising = false;
        self.supervisor_rounds = MAX_SUPERVISOR_ROUNDS;
        // A review still running: its card says so.
        for entry in &mut self.entries {
            if let Entry::Review(slot @ None) = entry {
                *slot = Some(Review::Failed(t("stopped").into()));
            }
        }
        self.list.remeasure();
        cx.notify();
    }

    pub fn attach(&mut self, paths: impl IntoIterator<Item = PathBuf>, cx: &mut Context<Self>) {
        for p in paths {
            if p.exists() && !self.attachments.contains(&p) {
                self.attachments.push(p);
            }
        }
        cx.notify();
    }

    /// ⌘V with an image on the clipboard: saved, attached, and "[Image #n]" typed in the
    /// composer; false when there is no image (a normal paste follows).
    fn paste_image(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(saved) = crate::util::save_clipboard_image(cx) else { return false };
        if !self.client.as_ref().is_some_and(|c| c.accepts_images()) {
            window.push_notification(gpui_kit::component::notification::Notification::error(t("Hermes' model does not accept images.")), cx);
            return true;
        }
        let path = match saved {
            Ok(p) => p,
            Err(e) => {
                window.push_notification(gpui_kit::component::notification::Notification::error(e.to_string()), cx);
                return true;
            }
        };
        self.pasted_images += 1;
        self.attach([path], cx);
        let label = format!("[Image #{}]", self.pasted_images);
        self.composer.update(cx, |c, cx| c.insert(label, window, cx));
        true
    }

    fn pick_files(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions { files: true, directories: true, multiple: true, prompt: None });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = paths.await {
                let _ = this.update(cx, |this, cx| this.attach(paths, cx));
            }
        })
        .detach();
    }

    /// Hermes' own mode, or "auto_safe": its "don't ask" for edits, plus commands ForgeCode
    /// judges harmless answered automatically.
    fn set_mode(&mut self, mode: String, cx: &mut Context<Self>) {
        Settings::update(cx, |s| s.agent_permission_mode = mode.clone());
        let (Some(client), Some(session)) = (self.client.clone(), self.session.as_mut()) else { return };
        let hermes_mode = if mode == AUTO_SAFE { "dont_ask".to_string() } else { mode };
        if !session.modes.iter().any(|(id, _, _)| *id == hermes_mode) || session.current_mode == hermes_mode {
            return cx.notify();
        }
        session.current_mode = hermes_mode.clone();
        let id = session.id.clone();
        cx.background_spawn(async move {
            let _ = client.set_mode(&id, &hermes_mode).await;
        })
        .detach();
        cx.notify();
    }

    fn toggle_graph(&mut self, cx: &mut Context<Self>) {
        self.show_graph = !self.show_graph;
        self.graph_error = None;
        if !self.show_graph {
            self.follow();
        }
        self._graph_task = None;
        if self.show_graph {
            // Refreshed while shown: sub-agents appear as Hermes creates them.
            let Some((plugin, _)) = Plugins::global(cx).agent().map(|(p, s)| (p.clone(), s.clone())) else { return cx.notify() };
            let python = super::hermes_graph::python_for(&agent_runtime::checkout(&plugin));
            let db = agent_runtime::agent_home(&plugin).join("state.db");
            let root = self.root.clone();
            self._graph_task = Some(cx.spawn(async move |this, cx| {
                loop {
                    let (python, db, root) = (python.clone(), db.clone(), root.clone());
                    let result = cx.background_spawn(async move { super::hermes_graph::read_rows(&python, &db).map(|rows| super::hermes_graph::build(&rows, &root)) }).await;
                    if this
                        .update(cx, |this, cx| {
                            match result {
                                Ok(graph) => {
                                    this.graph = graph;
                                    this.graph_error = None;
                                }
                                Err(e) => this.graph_error = Some(e.to_string()),
                            }
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                    cx.background_executor().timer(std::time::Duration::from_secs(3)).await;
                }
            }));
        }
        cx.notify();
    }

    /// Reopen a conversation from the graph.
    fn open_conversation(&mut self, id: String, cx: &mut Context<Self>) {
        // The open conversation: just show it again. It keeps running; nothing is restarted.
        if self.session.as_ref().is_some_and(|s| s.id == id) {
            self.show_graph = false;
            self._graph_task = None;
            self.follow();
            return cx.notify();
        }
        // Switching restarts the agent: never under a turn in progress.
        if self.running {
            self.graph_error = Some(t("Hermes is working: stop it (■) before opening another conversation.").into());
            return cx.notify();
        }
        let _ = std::fs::create_dir_all(crate::dotforge::dir(&self.root));
        let _ = std::fs::write(session_file(&self.root), &id);
        self.entries.clear();
        self.show_graph = false;
        self._graph_task = None;
        self.restart(cx);
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        if let (Some(client), Some(session)) = (&self.client, &self.session) {
            client.cancel(&session.id);
        }
        // Pending permission cards are now moot.
        for entry in &mut self.entries {
            if let Entry::Permission { request, answer: answer @ None, .. } = entry {
                if let Some(client) = &self.client {
                    client.answer_permission(request, None);
                }
                *answer = Some(t("Cancelled").into());
            }
        }
        self.list.remeasure();
        cx.notify();
    }

    fn set_model(&mut self, model: String, cx: &mut Context<Self>) {
        let (Some(client), Some(session)) = (self.client.clone(), self.session.as_mut()) else { return };
        session.current_model = model.clone();
        let id = session.id.clone();
        Settings::update(cx, |s| s.agent_model = model.clone());
        cx.background_spawn(async move {
            let _ = client.set_model(&id, &model).await;
        })
        .detach();
        cx.notify();
    }

    fn answer(&mut self, ix: usize, option: String, cx: &mut Context<Self>) {
        let Some(Entry::Permission { request, answer, options, .. }) = self.entries.get_mut(ix) else { return };
        if answer.is_some() {
            return;
        }
        if let Some(client) = &self.client {
            client.answer_permission(request, Some(&option));
        }
        *answer = options.iter().find(|o| o.id == option).map(|o| o.name.clone()).or(Some(option));
        self.list.remeasure_items(ix..ix + 1);
        cx.notify();
    }

    fn on_event(&mut self, event: AcpEvent, cx: &mut Context<Self>) {
        match event {
            AcpEvent::Update { update } => {
                // An entry that changed off screen has a new height.
                if let Some(ix) = apply_update(&mut self.entries, &update)
                    && ix < self.list.item_count().saturating_sub(1)
                {
                    self.list.remeasure_items(ix..ix + 1);
                }
                self.watch_loop(cx);
            }
            AcpEvent::Permission { request, tool_call, options, .. } => {
                let diff = tool_call["content"].as_array().into_iter().flatten().find(|c| c["type"] == "diff").map(|d| {
                    (d["path"].as_str().unwrap_or("").to_string(), d["oldText"].as_str().unwrap_or("").to_string(), d["newText"].as_str().unwrap_or("").to_string())
                });
                let command = tool_call["rawInput"]["command"].as_str().map(String::from).unwrap_or_default();
                let title = tool_call["title"].as_str().unwrap_or("Permission").to_string();
                // Safe auto mode: harmless commands go through, shown as such.
                let auto = Settings::get(cx).agent_permission_mode == AUTO_SAFE
                    && !command.is_empty()
                    && super::command_safety::is_safe(&command, &self.root);
                let allow = options.iter().find(|o| o.kind == "allow_once").or_else(|| options.iter().find(|o| o.kind.starts_with("allow")));
                match (auto, allow, &self.client) {
                    (true, Some(allow), Some(client)) => {
                        client.answer_permission(&request, Some(&allow.id));
                        self.entries.push(Entry::Tool {
                            id: String::new(),
                            title: format!("{} | $ {command}", t("Allowed automatically (safe)")),
                            kind: "execute".into(),
                            status: "completed".into(),
                            output: String::new(),
                        });
                    }
                    _ => self.entries.push(Entry::Permission { request, title, diff, command, options, answer: None }),
                }
            }
            AcpEvent::FileWritten(path) => cx.emit(HermesEvent::FileChanged(path)),
            AcpEvent::Exited(log) => {
                if self.client.is_some() {
                    self.client = None;
                    self.running = false;
                    self.state = State::Failed(format!("{}\n{}", t("Hermes stopped"), log.lines().rev().take(6).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n")));
                }
            }
        }
        // The list follows the answer while the reader is at the bottom, and leaves them where
        // they scrolled to otherwise.
        cx.notify();
    }

    /// Keep the list's items in step with the entries (the footer is its last item).
    fn sync_list(&mut self) {
        let (want, have) = (self.entries.len() + 1, self.list.item_count());
        if want > have {
            self.list.splice(have.saturating_sub(1)..have, want - have + 1);
        } else if want < have {
            self.list.reset(want);
            self.list.set_follow_mode(FollowMode::Tail);
        }
    }

    /// Back to the end of the conversation, following what comes next.
    fn follow(&mut self) {
        self.sync_list();
        self.list.set_follow_mode(FollowMode::Tail);
    }

    fn render_entry(&self, ix: usize, entry: &Entry, cx: &mut Context<Self>) -> AnyElement {
        #[cfg(test)]
        self.built.set(self.built.get() + 1);
        let theme = cx.theme();
        match entry {
            Entry::User(text) => div()
                .p_2()
                .rounded_md()
                .bg(theme.secondary)
                .text_sm()
                .child(text.clone())
                .into_any_element(),
            Entry::Assistant(text) => div().text_sm().child(super::markdown(("hermes-msg", ix), text.clone())).into_any_element(),
            Entry::Thought(text) => {
                let open = self.open_thoughts.contains(&ix);
                v_flex()
                    .child(
                        h_flex()
                            .id(("thought", ix))
                            .gap_1()
                            .cursor_pointer()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(if open { IconName::ChevronDown } else { IconName::ChevronRight })
                            .child(t("Thinking"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !this.open_thoughts.remove(&ix) {
                                    this.open_thoughts.insert(ix);
                                }
                                this.list.remeasure_items(ix..ix + 1);
                                cx.notify();
                            })),
                    )
                    .when(open, |el| el.child(div().pl_4().text_xs().text_color(theme.muted_foreground).child(text.clone())))
                    .into_any_element()
            }
            Entry::Tool { title, kind, status, output, .. } => {
                let icon = match kind.as_str() {
                    "read" => IconName::FileText,
                    "edit" => IconName::Pencil,
                    "delete" => IconName::Trash,
                    "search" => IconName::Search,
                    "execute" => IconName::SquareTerminal,
                    "fetch" => IconName::Globe,
                    "think" => IconName::Brain,
                    _ => IconName::Wrench,
                };
                let color = match status.as_str() {
                    "completed" => theme.green,
                    "failed" => theme.danger,
                    _ => theme.muted_foreground,
                };
                v_flex()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .text_xs()
                            .child(div().text_color(color).child(icon))
                            .child(div().flex_1().min_w_0().truncate().child(title.clone()))
                            .when(matches!(status.as_str(), "pending" | "in_progress"), |el| el.child(Spinner::new().xsmall())),
                    )
                    .when(!output.is_empty(), |el| {
                        el.child(div().ml_5().p_1().rounded_sm().bg(theme.background).text_xs().font_family("Menlo").text_color(theme.muted_foreground).max_h(px(120.)).overflow_hidden().child(output.clone()))
                    })
                    .into_any_element()
            }
            Entry::Plan(steps) => v_flex()
                .gap_0p5()
                .p_2()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .text_xs()
                .child(div().font_weight(FontWeight::MEDIUM).child(t("Plan")))
                .children(steps.iter().map(|(step, status)| {
                    let mark = match status.as_str() {
                        "completed" => "✓",
                        "in_progress" => "▸",
                        _ => "○",
                    };
                    h_flex().gap_1().child(mark).child(step.clone())
                }))
                .into_any_element(),
            Entry::Permission { title, diff, command, options, answer, .. } => {
                let mut card = v_flex().gap_1p5().p_2().rounded_md().border_1().border_color(theme.warning).bg(theme.warning.opacity(0.06)).text_xs();
                // Paths relative to the project, cut at the panel's width (the full title as tooltip).
                let short = title.replace(&format!("{}/", self.root.display()), "");
                card = card.child(
                    h_flex()
                        .id(("perm-title", ix))
                        .w_full()
                        .min_w_0()
                        .gap_1()
                        .font_weight(FontWeight::MEDIUM)
                        .child(div().flex_none().child(IconName::ShieldQuestionMark))
                        .child(div().flex_1().min_w_0().truncate().child(short))
                        .tooltip({
                            let full = title.clone();
                            move |w, cx| gpui_kit::component::tooltip::Tooltip::new(full.clone()).build(w, cx)
                        }),
                );
                if !command.is_empty() {
                    card = card.child(div().p_1().rounded_sm().bg(theme.background).font_family("Menlo").whitespace_normal().child(format!("$ {command}")));
                }
                if let Some((path, old, new)) = diff {
                    card = card.child(render_diff(path, old, new, cx));
                }
                card = match answer {
                    Some(a) => card.child(div().text_color(theme.muted_foreground).child(format!("→ {a}"))),
                    None => card.child(h_flex().gap_1().flex_wrap().children(options.iter().enumerate().map(|(n, o)| {
                        let id = o.id.clone();
                        let b = Button::new(("perm", ix * 16 + n)).xsmall().label(o.name.clone());
                        let b = if o.kind.starts_with("allow") { if n == 0 { b.primary() } else { b } } else { b.ghost() };
                        b.on_click(cx.listener(move |this, _, _, cx| this.answer(ix, id.clone(), cx)))
                    }))),
                };
                card.into_any_element()
            }
            Entry::Error(e) => div().p_2().rounded_md().bg(theme.danger.opacity(0.1)).text_xs().text_color(theme.danger).child(e.clone()).into_any_element(),
            Entry::Review(review) => {
                let head = |icon: IconName, color: Hsla, text: String| h_flex().gap_1p5().text_xs().font_weight(FontWeight::MEDIUM).child(div().text_color(color).child(icon)).child(text);
                match review {
                    None => h_flex().gap_2().text_xs().text_color(theme.muted_foreground).child(Spinner::new().xsmall()).child(t("Supervisor is reviewing this turn…")).into_any_element(),
                    Some(Review::Ok) => head(IconName::ShieldCheck, theme.green, t("Supervisor: nothing to report").into()).text_color(theme.muted_foreground).into_any_element(),
                    Some(Review::Failed(e)) => head(IconName::ShieldQuestionMark, theme.muted_foreground, format!("{} {e}", t("Supervisor unavailable:"))).text_color(theme.muted_foreground).into_any_element(),
                    Some(Review::Issues(issues)) => {
                        let message = supervisor_message(issues);
                        v_flex()
                            .gap_1p5()
                            .p_2()
                            .rounded_md()
                            .border_1()
                            .border_color(theme.warning)
                            .bg(theme.warning.opacity(0.06))
                            .child(head(IconName::TriangleAlert, theme.warning, t("Supervisor: to check").into()))
                            .child(div().text_xs().child(super::markdown(("review", ix), issues.clone())))
                            .child(
                                h_flex().child(Button::new(("review-send", ix)).xsmall().primary().icon(IconName::Send).label(t("Ask Hermes to fix")).on_click(cx.listener(move |this, _, _, cx| {
                                    if this.state == State::Ready {
                                        this.submit(message.clone(), cx);
                                    }
                                }))),
                            )
                            .into_any_element()
                    }
                }
            }
        }
    }

    /// What closes the conversation: the agent's status and the sign that it is working.
    fn render_footer(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let working = self.running && !matches!(self.entries.last(), Some(Entry::Assistant(_) | Entry::Permission { answer: None, .. }));
        v_flex()
            .gap_2()
            .min_h(px(1.))
            .children(self.render_status(cx))
            .when(working, |el| el.child(h_flex().gap_2().text_xs().text_color(muted).child(Spinner::new().xsmall()).child(t("Hermes is working…"))))
            .into_any_element()
    }

    fn render_status(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = cx.theme().clone();
        let block = |icon: IconName, text: String| v_flex().items_center().gap_2().p_4().text_sm().text_color(theme.muted_foreground).child(icon).child(div().text_center().child(text));
        Some(match &self.state {
            State::Ready => return None,
            State::NoModule => block(IconName::Blocks, t("Install the Hermes module from Settings › Modules › Marketplace.").into())
                .child(Button::new("hermes-market").small().primary().label(t("Open the marketplace")).on_click(cx.listener(|_, _, _, cx| cx.emit(HermesEvent::OpenSettings))))
                .into_any_element(),
            State::NeedsModels => block(IconName::Bot, t("Choose which AI models Hermes may use: Settings › AI › Agent.").into())
                .child(Button::new("hermes-settings").small().primary().label(t("Open settings")).on_click(cx.listener(|_, _, _, cx| cx.emit(HermesEvent::OpenSettings))))
                .into_any_element(),
            State::Installing(lines) => v_flex()
                .gap_2()
                .p_4()
                .child(h_flex().gap_2().text_sm().child(Spinner::new().small()).child(t("Installing Hermes…")))
                .child(v_flex().text_xs().font_family("Menlo").text_color(theme.muted_foreground).children(lines.iter().map(|l| div().truncate().child(l.clone()))))
                .into_any_element(),
            State::Starting => h_flex().gap_2().p_4().text_sm().text_color(theme.muted_foreground).child(Spinner::new().small()).child(t("Starting Hermes…")).into_any_element(),
            State::Failed(e) => block(IconName::TriangleAlert, e.clone())
                .child(Button::new("hermes-retry").small().label(t("Retry")).on_click(cx.listener(|this, _, _, cx| this.restart(cx))))
                .into_any_element(),
        })
    }
}

/// Fold one `session/update` into the conversation; the entry it changed.
fn apply_update(entries: &mut Vec<Entry>, u: &Value) -> Option<usize> {
    let text = || u["content"]["text"].as_str().unwrap_or("").to_string();
    match u["sessionUpdate"].as_str().unwrap_or("") {
        "agent_message_chunk" => match entries.last_mut() {
            Some(Entry::Assistant(s)) => s.push_str(&text()),
            _ => entries.push(Entry::Assistant(text())),
        },
        "agent_thought_chunk" => match entries.last_mut() {
            Some(Entry::Thought(s)) => s.push_str(&text()),
            _ => entries.push(Entry::Thought(text())),
        },
        // A resumed conversation replays its history.
        "user_message_chunk" => match entries.last_mut() {
            Some(Entry::User(s)) => s.push_str(&text()),
            _ => entries.push(Entry::User(text())),
        },
        "tool_call" => entries.push(Entry::Tool {
            id: u["toolCallId"].as_str().unwrap_or("").into(),
            title: u["title"].as_str().unwrap_or("").into(),
            kind: u["kind"].as_str().unwrap_or("other").into(),
            status: u["status"].as_str().unwrap_or("pending").into(),
            output: tool_output(u),
        }),
        "tool_call_update" => {
            let id = u["toolCallId"].as_str().unwrap_or("");
            let ix = entries.iter().rposition(|e| matches!(e, Entry::Tool { id: i, .. } if i == id));
            if let Some(Entry::Tool { title, status, output, .. }) = ix.and_then(|ix| entries.get_mut(ix)) {
                if let Some(s) = u["status"].as_str() {
                    *status = s.into();
                }
                if let Some(t) = u["title"].as_str() {
                    *title = t.into();
                }
                let more = tool_output(u);
                if !more.is_empty() {
                    *output = more;
                }
            }
            return ix;
        }
        "plan" => {
            let steps = u["entries"].as_array().into_iter().flatten().map(|e| (e["content"].as_str().unwrap_or("").to_string(), e["status"].as_str().unwrap_or("").to_string())).collect();
            let ix = entries.iter().rposition(|e| matches!(e, Entry::Plan(_)));
            match ix.and_then(|ix| entries.get_mut(ix)) {
                Some(Entry::Plan(p)) => *p = steps,
                _ => entries.push(Entry::Plan(steps)),
            }
            return ix;
        }
        _ => return None,
    }
    // A streamed chunk grows the last entry.
    Some(entries.len().saturating_sub(1))
}


/// What the supervisor writes to Hermes about the issues it found.
/// Identical failures in a row that make the supervisor step in during a turn.
const LOOP_FAILURES: usize = 3;

/// The last failed tool of the current turn (title, error) and how many times in a row (ignoring
/// the agent's words in between) it failed that same way.
fn repeated_failure(entries: &[Entry]) -> Option<(String, String, usize)> {
    let start = entries.iter().rposition(|e| matches!(e, Entry::User(_))).map_or(0, |i| i + 1);
    let mut tools = entries[start..].iter().rev().filter_map(|e| match e {
        Entry::Tool { title, status, output, .. } => Some((title, status, output)),
        _ => None,
    });
    let (title, status, output) = tools.next()?;
    if status != "failed" {
        return None;
    }
    let count = 1 + tools.take_while(|(t, s, o)| *t == title && *s == "failed" && *o == output).count();
    Some((title.clone(), output.clone(), count))
}

fn supervisor_message(issues: &str) -> String {
    format!("{SUPERVISOR_PREFIX}{}\n\n{issues}", t("I reviewed your last turn and found these problems. Check each one, fix what is real (run the tests/build to prove it), and say which ones were wrong:"))
}

fn gateway_if_started() -> Option<&'static Gateway> {
    Gateway::running()
}

/// Text a tool reported (command output, search results), shortened.
fn tool_output(u: &Value) -> String {
    let text: String = u["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["content"]["text"].as_str().or(c["text"].as_str()))
        .collect::<Vec<_>>()
        .join("\n");
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() > 8 { format!("{}\n…", lines[lines.len() - 8..].join("\n")) } else { text }
}

/// A compact line diff for permission cards.
fn render_diff(path: &str, old: &str, new: &str, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let diff = similar::TextDiff::from_lines(old, new);
    let mut rows = vec![];
    for change in diff.iter_all_changes().filter(|c| c.tag() != similar::ChangeTag::Equal).take(60) {
        let (sign, bg) = match change.tag() {
            similar::ChangeTag::Delete => ("-", theme.danger.opacity(0.15)),
            _ => ("+", theme.green.opacity(0.15)),
        };
        rows.push(div().px_1().bg(bg).whitespace_nowrap().child(format!("{sign} {}", change.value().trim_end_matches('\n'))));
    }
    v_flex()
        .child(div().text_color(theme.muted_foreground).child(path.to_string()))
        .child(v_flex().rounded_sm().bg(theme.background).font_family("Menlo").overflow_hidden().max_h(px(240.)).children(rows))
}

impl HermesPanel {
    fn render_graph(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let mut rows: Vec<AnyElement> = vec![];
        fn walk(this: &HermesPanel, node: &super::hermes_graph::Node, depth: usize, last: bool, rows: &mut Vec<AnyElement>, theme: &gpui_kit::component::Theme, cx: &mut Context<HermesPanel>) {
            use super::hermes_graph::NodeState;
            // The database only knows "not ended": whether a turn is running is this panel's.
            let current = this.session.as_ref().is_some_and(|s| s.id == node.id);
            let (icon, color, state) = match &node.state {
                _ if current && this.running => (IconName::LoaderCircle, theme.blue, t("working")),
                _ if current => (IconName::MessageSquare, theme.primary, t("open")),
                NodeState::Running if depth > 0 => (IconName::LoaderCircle, theme.blue, t("working")),
                NodeState::Failed(_) => (IconName::CircleX, theme.danger, t("failed")),
                NodeState::Done => (IconName::CircleCheck, theme.green, t("done")),
                NodeState::Running => (IconName::Circle, theme.muted_foreground, t("paused")),
            };
            let branch = if depth == 0 { String::new() } else { format!("{}{} ", "   ".repeat(depth - 1), if last { "└─" } else { "├─" }) };
            let stats = format!(
                "{state} | {} msg | {} {} | {}k tok{}",
                node.messages,
                node.tools,
                t("tools"),
                node.tokens / 1000,
                if node.compressions > 0 { format!(" | ⟳{}", node.compressions) } else { String::new() }
            );
            let id = node.id.clone();
            let root_node = depth == 0;
            rows.push(
                v_flex()
                    .id(SharedString::from(format!("node-{}", node.id)))
                    .px_1()
                    .py_0p5()
                    .rounded_sm()
                    .when(root_node, |el| el.cursor_pointer().hover(|s| s.bg(theme.list_hover)).on_click(cx.listener(move |this, _, _, cx| this.open_conversation(id.clone(), cx))))
                    .child(
                        h_flex()
                            .gap_1()
                            .text_xs()
                            .child(div().flex_none().font_family("Menlo").text_color(theme.muted_foreground).child(branch.clone()))
                            .child(div().flex_none().text_color(color).child(icon))
                            .child(div().flex_1().min_w_0().truncate().font_weight(if root_node { FontWeight::MEDIUM } else { FontWeight::NORMAL }).child(node.title.clone()))
                            .child(div().flex_none().text_color(theme.muted_foreground).child(short_model(&node.model))),
                    )
                    .child(div().pl(px(14. + 21. * depth as f32)).text_xs().text_color(theme.muted_foreground).child(stats))
                    .into_any_element(),
            );
            let n = node.children.len();
            for (i, child) in node.children.iter().enumerate() {
                walk(this, child, depth + 1, i + 1 == n, rows, theme, cx);
            }
            if current && depth == 0 {
                if n == 0 {
                    rows.push(div().pl(px(14.)).text_xs().text_color(theme.muted_foreground).child(t("No sub-agent: Hermes did everything itself in this conversation.")).into_any_element());
                }
                // What it is doing right now: the tools of the turn in progress, live.
                let turn_start = this.entries.iter().rposition(|e| matches!(e, Entry::User(_))).map_or(0, |i| i + 1);
                let tools: Vec<(&String, &String)> = this.entries[turn_start..].iter().filter_map(|e| match e {
                    Entry::Tool { title, status, .. } => Some((title, status)),
                    _ => None,
                }).collect();
                if this.running || !tools.is_empty() {
                    rows.push(div().pl(px(14.)).pt_1().text_xs().font_weight(FontWeight::MEDIUM).child(format!("{} ({})", t("This turn"), tools.len())).into_any_element());
                }
                let skipped = tools.len().saturating_sub(12);
                for (title, status) in tools.into_iter().skip(skipped) {
                    let (mark, color) = match status.as_str() {
                        "completed" => ("✓", theme.green),
                        "failed" => ("✕", theme.danger),
                        _ => ("▸", theme.blue),
                    };
                    let delegated = title.contains("delegate");
                    let short = title.replace(&format!("{}/", this.root.display()), "");
                    rows.push(
                        h_flex()
                            .pl(px(14.))
                            .gap_1()
                            .text_xs()
                            .child(div().flex_none().text_color(color).child(mark))
                            .when(delegated, |el| el.child(div().flex_none().text_color(theme.primary).child(IconName::Network)))
                            .child(div().flex_1().min_w_0().truncate().text_color(theme.muted_foreground).child(short))
                            .into_any_element(),
                    );
                }
            }
        }
        for node in &self.graph {
            walk(self, node, 0, true, &mut rows, &theme, cx);
        }
        v_flex()
            .gap_1()
            .when_some(self.graph_error.clone(), |el, e| el.child(div().text_xs().text_color(theme.danger).child(e)))
            .when(self.graph.is_empty() && self.graph_error.is_none(), |el| el.child(div().text_sm().text_color(theme.muted_foreground).child(t("No conversation yet."))))
            .children(rows)
            .into_any_element()
    }
}

impl Render for HermesPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_list();
        let graph = self.show_graph.then(|| self.render_graph(cx));
        let theme = cx.theme().clone();
        let ready = self.state == State::Ready;
        let models = self.session.as_ref().map(|s| (s.models.clone(), s.current_model.clone())).unwrap_or_default();
        let modes = self.session.as_ref().map(|s| s.modes.clone()).unwrap_or_default();
        let mode = Settings::get(cx).agent_permission_mode.clone();
        let mode_label = if mode == AUTO_SAFE {
            t("Safe auto").to_string()
        } else {
            modes.iter().find(|(id, _, _)| *id == mode).map(|(_, n, _)| n.clone()).unwrap_or_else(|| t("Default").to_string())
        };
        let view = cx.entity().downgrade();
        let model_label = short_model(&models.1);
        let attachments = self.attachments.clone();
        let queue = self.queue.clone();
        // The supervisor at work: the panel is outlined in blue while it reads or writes.
        let supervisor_active = self.supervising || self.supervisor_typing.is_some();
        v_flex()
            .size_full()
            .bg(theme.sidebar)
            .border_2()
            .border_color(if supervisor_active { theme.blue } else { gpui_kit::transparent_black() })
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| this.attach(paths.paths().to_vec(), cx)))
            .on_drop(cx.listener(|this, f: &crate::dnd::DraggedFile, _, cx| this.attach([f.0.clone()], cx)))
            .on_drop(cx.listener(|this, tab: &crate::dnd::DraggedTab, _, cx| this.attach(tab.path.clone(), cx)))
            .drag_over::<ExternalPaths>(|style, _, _, cx| style.bg(cx.theme().accent.opacity(0.3)))
            .drag_over::<crate::dnd::DraggedFile>(|style, _, _, cx| style.bg(cx.theme().accent.opacity(0.3)))
            .child(
                h_flex()
                    .h(px(36.))
                    .px_3()
                    .gap_1()
                    .child(div().flex_1().text_xs().font_weight(FontWeight::MEDIUM).text_color(theme.muted_foreground).child("HERMES"))
                    .when(supervisor_active, |el| {
                        el.child(
                            h_flex()
                                .gap_1()
                                .text_xs()
                                .text_color(theme.blue)
                                .child(IconName::ShieldCheck)
                                .child(if self.supervisor_typing.is_some() { t("Supervisor is writing…") } else { t("Supervisor is reading…") }),
                        )
                        .child(Button::new("supervisor-stop").ghost().xsmall().icon(IconName::Square).tooltip(t("Stop the supervisor"))
                            .on_click(cx.listener(|this, _, _, cx| this.stop_supervisor(cx))))
                    })
                    .child(Button::new("hermes-graph").ghost().xsmall().icon(IconName::Network).selected(self.show_graph).tooltip(t("Conversations and sub-agents graph"))
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_graph(cx))))
                    .child(Button::new("hermes-classic").ghost().xsmall().icon(IconName::MessageSquare).tooltip(t("Switch to the classic chat"))
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(HermesEvent::UseClassicChat))))
                    .child(Button::new("hermes-restart").ghost().xsmall().icon(IconName::RefreshCw).tooltip(t("Restart Hermes"))
                        .on_click(cx.listener(|this, _, _, cx| this.restart(cx))))
                    .child({
                        let history = self.history.clone();
                        let view = view.clone();
                        let current = self.session.as_ref().map(|s| s.id.clone()).unwrap_or_default();
                        Button::new("hermes-history").ghost().xsmall().icon(IconName::Clock).tooltip(t("Earlier conversations")).disabled(history.is_empty())
                            .dropdown_menu(move |mut menu, _, _| {
                                for (id, title) in history.clone() {
                                    let view = view.clone();
                                    menu = menu.item(PopupMenuItem::new(title).checked(id == current).on_click(move |_, _, cx| {
                                        let _ = view.update(cx, |this, cx| this.open_conversation(id.clone(), cx));
                                    }));
                                }
                                menu
                            })
                    })
                    .child(Button::new("hermes-new").ghost().xsmall().icon(IconName::MessageSquarePlus).label(t("New")).tooltip(t("New conversation"))
                        .on_click(cx.listener(|this, _, _, cx| this.new_conversation(cx)))),
            )
            .child(
                div().flex_1().min_h_0().child(match graph {
                    Some(graph) => v_flex().id("hermes-graph-view").size_full().overflow_y_scroll().track_scroll(&self.scroll).p_3().gap_2().child(graph).into_any_element(),
                    // Only the entries on screen are built: a long conversation costs no more than a short one.
                    None => list(
                        self.list.clone(),
                        cx.processor(|this, ix: usize, _, cx| match this.entries.get(ix) {
                            Some(entry) => div().pb_2().child(this.render_entry(ix, entry, cx)).into_any_element(),
                            None => this.render_footer(cx),
                        }),
                    )
                    .size_full()
                    .p_3()
                    .into_any_element(),
                }),
            )
            .child(
                v_flex()
                    .m_2()
                    .p_1()
                    .gap_1()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.background)
                    // Messages waiting for the current turn: click (or ↑) to edit, ⚡ to send now.
                    .children(queue.iter().enumerate().map(|(i, q)| {
                        let editing = self.editing == Some(i);
                        h_flex()
                            .id(("queued", i))
                            .gap_1()
                            .px_1()
                            .rounded_sm()
                            .text_xs()
                            .cursor_pointer()
                            .text_color(if editing { theme.foreground } else { theme.muted_foreground })
                            .when(editing, |el| el.bg(theme.accent.opacity(0.5)))
                            .hover(|el| el.bg(theme.secondary))
                            .child(if editing { IconName::Pencil } else { IconName::Clock })
                            .child(div().flex_1().min_w_0().truncate().child(q.lines().next().unwrap_or("").to_string()))
                            .tooltip(move |w, cx| gpui_kit::component::tooltip::Tooltip::new(t("Click to edit (↑ ↓ to go through the queue)")).build(w, cx))
                            .on_click(cx.listener(move |this, _, window, cx| this.edit_queued(i, window, cx)))
                            .child(Button::new(("send-now", i)).ghost().xsmall().icon(IconName::Zap).tooltip(t("Send now (interrupts Hermes)")).on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                if this.editing == Some(i) {
                                    return this.finish_edit(true, true, window, cx);
                                }
                                this.save_edit(cx);
                                if i < this.queue.len() {
                                    if let Some(e) = this.editing.filter(|e| *e > i) {
                                        this.editing = Some(e - 1);
                                    }
                                    let text = this.queue.remove(i);
                                    this.force(text, cx);
                                }
                                cx.notify();
                            })))
                            .child(Button::new(("unqueue", i)).ghost().xsmall().icon(IconName::Close).tooltip(t("Remove from the queue")).on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.remove_queued(i, window, cx);
                            })))
                    }))
                    .when(self.editing.is_some(), |el| {
                        el.child(div().px_1().text_xs().text_color(theme.muted_foreground).child(t("Editing a queued message: ↵ save | ⌘↵ send now | Esc cancel")))
                    })
                    .when(!attachments.is_empty(), |el| {
                        el.child(h_flex().flex_wrap().gap_1().children(attachments.iter().enumerate().map(|(i, p)| {
                            let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                            h_flex()
                                .gap_1()
                                .pl_1p5()
                                .rounded_sm()
                                .bg(theme.secondary)
                                .text_xs()
                                .child(name)
                                .child(Button::new(("unattach", i)).ghost().xsmall().icon(IconName::Close).on_click(cx.listener(move |this, _, _, cx| {
                                    if i < this.attachments.len() {
                                        this.attachments.remove(i);
                                    }
                                    cx.notify();
                                })))
                        })))
                    })
                    .child(
                        div()
                            // Before the composer moves its cursor: ↑/↓ through the queue, Esc to cancel an edit.
                            .capture_action(cx.listener(|this, _: &MoveUp, window, cx| {
                                if this.step_queue(true, window, cx) {
                                    cx.stop_propagation();
                                }
                            }))
                            .capture_action(cx.listener(|this, _: &MoveDown, window, cx| {
                                if this.step_queue(false, window, cx) {
                                    cx.stop_propagation();
                                }
                            }))
                            .capture_action(cx.listener(|this, _: &Paste, window, cx| {
                                if this.supervisor_typing.is_none() && this.paste_image(window, cx) {
                                    cx.stop_propagation();
                                }
                            }))
                            .capture_action(cx.listener(|this, _: &Escape, window, cx| {
                                if this.editing.is_some() {
                                    cx.stop_propagation();
                                    this.finish_edit(false, false, window, cx);
                                }
                            }))
                            .child(match &self.supervisor_typing {
                                // The supervisor's message being typed, with its (blue) cursor.
                                Some((text, shown)) => div()
                                    .px_2()
                                    .py_1()
                                    .max_h(px(160.))
                                    .overflow_hidden()
                                    .text_sm()
                                    .whitespace_normal()
                                    .child({
                                        let typed = format!("{}▍", &text[..*shown]);
                                        let cursor = typed.len() - "▍".len()..typed.len();
                                        StyledText::new(typed).with_highlights(vec![(cursor, HighlightStyle { color: Some(theme.blue), ..Default::default() })])
                                    })
                                    .into_any_element(),
                                None => Textarea::new(&self.composer).appearance(false).disabled(!ready).into_any_element(),
                            }),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .min_w_0()
                            .gap_1()
                            .child(Button::new("hermes-attach").ghost().xsmall().icon(IconName::Paperclip).tooltip(t("Attach files")).disabled(!ready)
                                .on_click(cx.listener(|this, _, _, cx| this.pick_files(cx))))
                            .child({
                                let view = view.clone();
                                let current = mode.clone();
                                Button::new("hermes-mode").ghost().xsmall().icon(IconName::ShieldCheck).label(mode_label).disabled(!ready).dropdown_menu(move |mut menu, _, _| {
                                    for (id, name, desc) in modes.clone() {
                                        let view = view.clone();
                                        let checked = current == id;
                                        menu = menu.item(PopupMenuItem::new(format!("{name} - {desc}")).checked(checked).on_click(move |_, _, cx| {
                                            let _ = view.update(cx, |t, cx| t.set_mode(id.clone(), cx));
                                        }));
                                    }
                                    let view = view.clone();
                                    menu.separator().item(
                                        PopupMenuItem::new(t("Safe auto - edits allowed; commands run without asking only when harmless (read, build, test)"))
                                            .checked(current == AUTO_SAFE)
                                            .on_click(move |_, _, cx| {
                                                let _ = view.update(cx, |t, cx| t.set_mode(AUTO_SAFE.to_string(), cx));
                                            }),
                                    )
                                })
                            })
                            // The model name can be long: it shrinks, the buttons stay.
                            .child(
                                div().flex_1().min_w_0().overflow_hidden().child(
                                    Button::new("hermes-model").ghost().xsmall().icon(IconName::Bot).label(if model_label.is_empty() { "-".to_string() } else { model_label }).disabled(!ready)
                                        .dropdown_menu(move |mut menu, _, _| {
                                            for (id, name) in models.0.clone() {
                                                let view = view.clone();
                                                let current = id == models.1;
                                                menu = menu.item(PopupMenuItem::new(name).checked(current).on_click(move |_, _, cx| {
                                                    let _ = view.update(cx, |t, cx| t.set_model(id.clone(), cx));
                                                }));
                                            }
                                            menu
                                        }),
                                ),
                            )
                            .when(self.running, |el| {
                                el.child(Button::new("hermes-send-now").xsmall().flex_none().ghost().icon(IconName::Zap).disabled(!ready).tooltip(t("Send now (interrupts Hermes) | ⌘↵"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        if this.editing.is_some() {
                                            return this.finish_edit(true, true, window, cx);
                                        }
                                        let text = this.composer.read(cx).value().to_string();
                                        if !text.trim().is_empty() || !this.attachments.is_empty() {
                                            this.composer.update(cx, |i, cx| i.set_value("", window, cx));
                                            this.force(text, cx);
                                        }
                                    })))
                                .child(Button::new("hermes-stop").xsmall().flex_none().icon(IconName::Square).tooltip(t("Stop")).on_click(cx.listener(|this, _, _, cx| this.stop(cx))))
                            })
                            .child(
                                Button::new("hermes-send").xsmall().flex_none().primary().icon(IconName::ArrowUp).disabled(!ready)
                                    .tooltip(if self.running { t("Queue this message") } else { t("Send") })
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        if this.editing.is_some() {
                                            return this.finish_edit(true, false, window, cx);
                                        }
                                        let text = this.composer.read(cx).value().to_string();
                                        if !text.trim().is_empty() || !this.attachments.is_empty() {
                                            this.composer.update(cx, |i, cx| i.set_value("", window, cx));
                                            this.submit(text, cx);
                                        }
                                    })),
                            ),
                    ),
            )
    }
}


/// One attachment for `session/prompt`: the image itself when the agent takes images, else a
/// link to the file that Hermes opens with its own tools.
fn attachment_block(path: &Path, images: bool) -> Value {
    if images
        && let Some(mime) = crate::util::image_media_type(path)
        && let Ok(bytes) = std::fs::read(path)
    {
        return serde_json::json!({"type": "image", "mimeType": mime, "data": crate::util::base64(&bytes)});
    }
    serde_json::json!({
        "type": "resource_link",
        "uri": crate::lsp::client::uri(path),
        "name": path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::{Entry, apply_update, attachment_block, repeated_failure, short_model};

    #[test]
    fn spots_a_tool_failing_the_same_way() {
        let tool = |status: &str, output: &str| Entry::Tool { id: "x".into(), title: "write_file".into(), kind: "edit".into(), status: status.into(), output: output.into() };
        let err = "Edit approval denied: could not prepare diff (path required)";
        let mut entries = vec![Entry::User("fix".into()), tool("failed", err), Entry::Assistant("again".into()), tool("failed", err), tool("failed", err)];
        assert_eq!(repeated_failure(&entries).map(|r| r.2), Some(3));
        // A different error breaks the run; a success ends it.
        entries.insert(2, tool("failed", "other"));
        assert_eq!(repeated_failure(&entries).map(|r| r.2), Some(2));
        entries.push(tool("completed", ""));
        assert_eq!(repeated_failure(&entries), None);
        // Only the current turn counts.
        let entries = vec![tool("failed", err), tool("failed", err), Entry::User("go".into()), tool("failed", err)];
        assert_eq!(repeated_failure(&entries).map(|r| r.2), Some(1));
    }

    #[test]
    fn images_go_inline_only_when_the_agent_takes_them() {
        let dir = std::env::temp_dir().join(format!("forgecode-attach-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("shot.png");
        std::fs::write(&png, b"\x89PNG").unwrap();
        let inline = attachment_block(&png, true);
        assert_eq!(inline["type"], "image");
        assert_eq!(inline["mimeType"], "image/png");
        assert_eq!(inline["data"], crate::util::base64(b"\x89PNG"));
        assert_eq!(attachment_block(&png, false)["type"], "resource_link");
        assert_eq!(attachment_block(&dir.join("a.rs"), true)["type"], "resource_link");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shortens_model_names() {
        assert_eq!(short_model("custom:lm/qwen3-coder"), "qwen3-coder");
        assert_eq!(short_model("copilot/kimi-k3"), "kimi-k3");
    }
    use serde_json::json;

    #[test]
    fn folds_agent_updates_into_the_conversation() {
        let mut entries = vec![];
        for u in [
            json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "Let me "}}),
            json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "look."}}),
            json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "read_file main.py", "kind": "read", "status": "in_progress"}),
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed", "content": [{"type": "content", "content": {"type": "text", "text": "12 lines"}}]}),
            json!({"sessionUpdate": "plan", "entries": [{"content": "Fix import", "status": "in_progress"}]}),
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Done"}}),
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "."}}),
        ] {
            apply_update(&mut entries, &u);
        }
        assert!(matches!(&entries[0], Entry::Thought(t) if t == "Let me look."));
        assert!(matches!(&entries[1], Entry::Tool { status, output, .. } if status == "completed" && output == "12 lines"));
        assert!(matches!(&entries[2], Entry::Plan(p) if p[0].0 == "Fix import"));
        assert!(matches!(&entries[3], Entry::Assistant(t) if t == "Done."));
    }

    use gpui_kit::{AppContext as _, TestAppContext};

    fn panel(cx: &mut TestAppContext) -> (gpui_kit::Entity<super::HermesPanel>, &mut gpui_kit::VisualTestContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            let base = crate::settings::Settings::default();
            cx.set_global(base.clone());
            cx.set_global(crate::settings::Layers { base, ..Default::default() });
            cx.set_global(crate::ai::copilot::CopilotState { sign_in: crate::ai::copilot::SignIn::Idle, accounts: vec![], models: vec![] });
            cx.set_global(crate::plugins::Plugins::default());
        });
        let slot = std::rc::Rc::new(std::cell::RefCell::new(None));
        let s = slot.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let panel = cx.new(|cx| super::HermesPanel::new(std::env::temp_dir(), window, cx));
            *s.borrow_mut() = Some(panel.clone());
            gpui_kit::base::Root::new(panel, window, cx)
        });
        let panel = slot.borrow().clone().unwrap();
        (panel, cx)
    }

    /// The supervisor types its correction (visible, outlined in blue), then sends it, stopping
    /// Hermes if needed; the user can stop it while it writes, and it gives up after two rounds.
    #[gpui_kit::test]
    fn the_supervisor_writes_to_hermes_and_can_be_stopped(cx: &mut TestAppContext) {
        let (panel, cx) = panel(cx);
        panel.update(cx, |p, cx| {
            p.state = super::State::Ready;
            p.running = true;
            p.supervisor_write(super::supervisor_message("- tests not run"), cx);
        });
        cx.executor().advance_clock(std::time::Duration::from_millis(50));
        cx.run_until_parked();
        let (typing, shown) = panel.read_with(cx, |p, _| p.supervisor_typing.as_ref().map(|(t, n)| (t.len(), *n)).unwrap());
        assert!(shown > 0 && shown < typing, "typed progressively ({shown}/{typing})");
        cx.executor().advance_clock(std::time::Duration::from_secs(10));
        cx.run_until_parked();
        panel.read_with(cx, |p, _| {
            assert!(p.supervisor_typing.is_none());
            assert!(p.queue[0].starts_with(super::SUPERVISOR_PREFIX) && p.queue[0].contains("tests not run"), "sent first, Hermes stopped");
            assert_eq!(p.supervisor_rounds, 1);
        });

        // Stopped while writing: nothing is sent, and it won't write again on its own.
        panel.update(cx, |p, cx| {
            p.queue.clear();
            p.supervisor_write("[Supervisor] again".into(), cx);
            p.stop_supervisor(cx);
        });
        cx.executor().advance_clock(std::time::Duration::from_secs(10));
        cx.run_until_parked();
        panel.read_with(cx, |p, _| {
            assert!(p.queue.is_empty() && p.supervisor_typing.is_none());
            assert_eq!(p.supervisor_rounds, super::MAX_SUPERVISOR_ROUNDS);
        });
    }

    /// While Hermes works: ↑/↓ walk through the queued messages to edit them, Esc drops an
    /// edit, and "send now" puts a message first and stops the turn.
    #[gpui_kit::test]
    fn queued_messages_can_be_edited_and_forced(cx: &mut TestAppContext) {
        let (panel, cx) = panel(cx);
        cx.update(|window, cx| {
            panel.update(cx, |p, cx| {
                p.state = super::State::Ready;
                p.running = true;
                p.queue = vec!["first".into(), "second\nline".into()];
                p.composer.update(cx, |c, cx| c.focus(window, cx));
                cx.notify();
            })
        });
        cx.run_until_parked();
        let composer = |cx: &mut gpui_kit::VisualTestContext| panel.read_with(cx, |p, cx| (p.editing, p.composer.read(cx).value().to_string()));

        cx.simulate_keystrokes("up");
        assert_eq!(composer(cx), (Some(1), "second\nline".into()), "↑ in an empty composer edits the last queued message");
        // Cursor at the end, on the second line: ↑ first moves the cursor inside the message.
        cx.simulate_keystrokes("up");
        assert_eq!(composer(cx).0, Some(1));
        cx.simulate_keystrokes("up");
        assert_eq!(composer(cx), (Some(0), "first".into()), "↑ on the first line goes to the previous message");
        cx.simulate_input(" (edited)");
        cx.simulate_keystrokes("down");
        assert_eq!(composer(cx).0, Some(1), "↓ on the last line goes to the next message");
        assert_eq!(panel.read_with(cx, |p, _| p.queue[0].clone()), "first (edited)", "the edit is kept");
        cx.simulate_keystrokes("down");
        assert_eq!(composer(cx), (None, String::new()), "past the last message: back to an empty composer");

        cx.simulate_keystrokes("up");
        cx.simulate_input(" dropped");
        cx.simulate_keystrokes("escape");
        assert_eq!(composer(cx), (None, String::new()));
        assert_eq!(panel.read_with(cx, |p, _| p.queue[1].clone()), "second\nline", "Esc drops the changes");

        // Send now: first in line (Hermes is stopped, then it goes).
        cx.simulate_input("urgent");
        cx.simulate_keystrokes("cmd-enter");
        assert_eq!(panel.read_with(cx, |p, _| p.queue.clone()), ["urgent", "first (edited)", "second\nline"]);
        assert_eq!(composer(cx), (None, String::new()));
        // Enter while working only queues.
        cx.simulate_input("later");
        cx.simulate_keystrokes("enter");
        assert_eq!(panel.read_with(cx, |p, _| p.queue.last().cloned()), Some("later".into()));
    }

    /// A conversation of thousands of entries builds only what the panel shows, and follows
    /// the answer being streamed at its end.
    #[gpui_kit::test]
    fn long_conversations_only_build_what_is_on_screen(cx: &mut TestAppContext) {
        let (panel, cx) = panel(cx);
        panel.update(cx, |p, cx| {
            for i in 0..3000 {
                p.entries.push(if i % 2 == 0 { Entry::User(format!("question {i}")) } else { Entry::Assistant(format!("answer **{i}**\n\n```rust\nfn f{i}() {{}}\n```")) });
            }
            cx.notify();
        });
        cx.run_until_parked();
        let built = panel.read_with(cx, |p, _| p.built.get());
        assert!(built > 0 && built < 300, "{built} entries built for 3000");
        assert!(panel.read_with(cx, |p, _| p.list.is_following_tail()), "the end of the conversation is shown");

        // A streamed chunk redraws the visible entries only.
        panel.update(cx, |p, cx| {
            p.built.set(0);
            p.on_event(super::AcpEvent::Update { update: json!({"sessionUpdate": "agent_message_chunk", "content": {"text": " more"}}) }, cx);
        });
        cx.run_until_parked();
        let built = panel.read_with(cx, |p, _| p.built.get());
        assert!(built > 0 && built < 300, "{built} entries rebuilt for one chunk");
        assert!(panel.read_with(cx, |p, _| matches!(p.entries.last(), Some(Entry::Assistant(t)) if t.ends_with(" more"))));
    }
}
