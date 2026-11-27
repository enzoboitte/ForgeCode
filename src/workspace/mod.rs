//! The IDE window: state and logic. Rendering lives in [`render`].

mod buffer;
mod dialogs;
mod file_tree;
mod new_project;
mod output;
mod pane;
mod quick_open;
mod palette;
mod search;
mod render;
mod settings_page;
mod terminal;
pub mod menus;
mod advisor;
pub mod databases;
mod ai_fix;
mod watcher;
pub mod runtime_picker;
mod code_actions;
mod csv_table;
mod glsl_lint;
mod conan_panel;
pub mod shader;
mod shader_view;
mod marketplace;
mod modules;
mod sql;
mod video;
mod browser;
mod viewer;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui_kit::component::diff::{DiffFile, DiffMode, DiffState};
use gpui_kit::component::input::{InputEvent, MoveEnd, ShowCompletions};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::WindowExt as _;
use gpui_kit::*;
use lsp_types::{Diagnostic, DiagnosticSeverity};

use crate::ai::chat_panel::{ChatEvent, ChatPanel};
use crate::ai::copilot::CopilotState;
use crate::ai::tools::SideEffect;
use crate::debug::{Breakpoints, DebugEvent, DebugPanel};
use crate::git::graph::{GitGraph, GraphEvent};
use crate::git::panel::{GitEvent, GitPanel};
use crate::i18n::t;
use crate::language::{self, LanguageInfo};
use crate::lsp::client::path_from_uri;
use crate::lsp::{LspClient, LspEvent};
use crate::plugins::{self, IdeAction, Plugins};
use crate::settings::Settings;
use buffer::{Buffer, Preview};
use file_tree::{FileTree, FileTreeEvent};
use output::{OutputEvent, OutputPanel};
use pane::{Layout, Zone};
use terminal::{Shell, Terminal, available_shells};

pub use quick_open::{filter as filter_files, project_files};

actions!(
    forge,
    [
        Save, OpenFolder, CloseTab, ToggleSettings, ToggleLeft, ToggleBottom, ToggleRight, ToggleTerminal, NewTerminal,
        ShowExplorer, ShowGit, ShowGitGraph, ShowProblems, RunFile, StartDebug, StopDebug, StepOver, StepIn, StepOut, ToggleBreakpoint,
        FocusChat, AddSelectionToChat, QuickOpenFile, CommandPalette, ShowSearch, FormatDocument, OpenProjectInstructions, OpenProjectSettings, ToggleTheme, NewModule, ReloadModules, NewProject, SplitRight, AcceptSuggestionWord, Quit, OpenBrowser,
        ToggleLineComment, MoveLineUp, MoveLineDown, CopyLineUp, CopyLineDown, DeleteLine, InsertLineBelow, InsertLineAbove, SelectNextOccurrence, SelectAllOccurrences
    ]
);

/// Cmd on macOS, Ctrl elsewhere: `KeyBinding::new` doesn't resolve gpui's `secondary-`.
macro_rules! secondary {
    ($keys:literal) => {
        if cfg!(target_os = "macos") { concat!("cmd-", $keys) } else { concat!("ctrl-", $keys) }
    };
}

// After `secondary!`, which it uses.
mod editing;

pub fn key_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new(secondary!("s"), Save, None),
        KeyBinding::new(secondary!("o"), OpenFolder, None),
        KeyBinding::new(secondary!("w"), CloseTab, None),
        KeyBinding::new(secondary!(","), ToggleSettings, None),
        KeyBinding::new(secondary!("b"), ToggleLeft, None),
        KeyBinding::new(secondary!("j"), ToggleBottom, None),
        KeyBinding::new(secondary!("alt-b"), ToggleRight, None),
        KeyBinding::new("ctrl-`", ToggleTerminal, None),
        KeyBinding::new("ctrl-shift-`", NewTerminal, None),
        KeyBinding::new(secondary!("shift-e"), ShowExplorer, None),
        KeyBinding::new("ctrl-shift-g", ShowGit, None),
        KeyBinding::new(secondary!("shift-m"), ShowProblems, None),
        KeyBinding::new(secondary!("\\"), SplitRight, None),
        KeyBinding::new(secondary!("r"), RunFile, None),
        KeyBinding::new("f5", StartDebug, None),
        KeyBinding::new("shift-f5", StopDebug, None),
        KeyBinding::new("f10", StepOver, None),
        KeyBinding::new("f11", StepIn, None),
        KeyBinding::new("shift-f11", StepOut, None),
        KeyBinding::new("f9", ToggleBreakpoint, None),
        KeyBinding::new(secondary!("l"), FocusChat, None),
        KeyBinding::new(secondary!("p"), QuickOpenFile, None),
        KeyBinding::new(secondary!("shift-p"), CommandPalette, None),
        KeyBinding::new(secondary!("shift-f"), ShowSearch, None),
        KeyBinding::new("shift-alt-f", FormatDocument, None),
        KeyBinding::new("f12", gpui_kit::component::input::GoToDefinition, Some("Input")),
        KeyBinding::new(secondary!("q"), Quit, None),
        KeyBinding::new("ctrl-space", ShowCompletions, Some("Input")),
        KeyBinding::new(secondary!("right"), AcceptSuggestionWord, Some("Input")),
    ]
    .into_iter()
    .chain(editing::key_bindings())
    .chain(terminal::key_bindings())
    .collect()
}

#[derive(Clone, Copy, PartialEq)]
enum Sidebar {
    Explorer,
    Search,
    Git,
    Databases,
    Conan,
}

/// The workspace, reachable from editor callbacks that only get `&mut App`.
struct WorkspaceHandle(WeakEntity<Workspace>);
impl Global for WorkspaceHandle {}

/// Open a file at a line from anywhere (LSP "show document").
/// The open project's folder.
pub fn current_root(cx: &App) -> Option<PathBuf> {
    let ws = cx.try_global::<WorkspaceHandle>()?.0.upgrade()?;
    Some(ws.read(cx).root.clone())
}

fn open_in_workspace(path: PathBuf, line: Option<u32>, window: &mut Window, cx: &mut App) {
    let Some(ws) = cx.try_global::<WorkspaceHandle>().map(|h| h.0.clone()) else { return };
    // The editor that asked is still being updated: open on the next turn.
    window.defer(cx, move |window, cx| {
        let _ = ws.update(cx, |w, cx| w.open_file(path, line, window, cx));
    });
}

#[derive(Clone, Copy, PartialEq)]
enum BottomTab {
    Problems,
    Output,
    Terminal,
    Debug,
}

enum Tab {
    File(Buffer),
    Diff { title: SharedString, patch: String, mode: DiffMode, state: Entity<DiffState> },
    Graph(Entity<GitGraph>),
    Agents(Entity<crate::ai::agents_view::AgentTree>),
    /// Image viewer; `zoom` 1 fits the pane, more scrolls.
    Image { path: PathBuf, zoom: f32, scroll: ScrollHandle, crisp: Option<Arc<gpui_kit::Image>> },
    Pdf(Entity<viewer::PdfView>),
    Video(Entity<video::VideoView>),
    /// A database table (databases module).
    Table(Entity<databases::table::TableTab>),
    /// A SQL console of one connection.
    Console(Entity<databases::console::ConsoleTab>),
    Browser(Entity<browser::BrowserView>),
    Settings,
}

impl Tab {
    fn label(&self, cx: &App) -> String {
        match self {
            Tab::File(b) => b.name(),
            Tab::Diff { title, .. } => title.to_string(),
            Tab::Graph(_) => t("History").into(),
            Tab::Agents(_) => t("Agents").into(),
            Tab::Image { path, .. } => path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
            Tab::Pdf(v) => v.read(cx).path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
            Tab::Video(v) => v.read(cx).path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
            Tab::Table(v) => v.read(cx).title(cx),
            Tab::Console(v) => v.read(cx).title(cx),
            Tab::Browser(v) => v.read(cx).title.chars().take(30).collect(),
            Tab::Settings => t("Settings").into(),
        }
    }

    /// The file a tab shows (text, image, PDF, video), if any.
    fn path(&self, cx: &App) -> Option<PathBuf> {
        match self {
            Tab::File(b) => Some(b.path.clone()),
            Tab::Image { path, .. } => Some(path.clone()),
            Tab::Pdf(v) => Some(v.read(cx).path.clone()),
            Tab::Video(v) => Some(v.read(cx).path.clone()),
            _ => None,
        }
    }
}

fn is_pdf(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
}

/// Small images (pixel art, icons) with crisp pixels: GPUI smooths what it enlarges, so they are
/// enlarged here first, each pixel repeated. None for big, vector or animated images.
fn crisp_image(path: &Path) -> Option<Arc<gpui_kit::Image>> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if matches!(ext.as_str(), "svg" | "gif") {
        return None;
    }
    // The header alone says the size: big images are not decoded here.
    let (w, h) = image::image_dimensions(path).ok()?;
    let side = w.max(h);
    if side == 0 || side > 512 {
        return None;
    }
    // About 2048 px on the long side, enough for a full-screen pane: GPUI then only shrinks it.
    let factor = (2048 / side).max(2);
    let big = image::open(path).ok()?.resize_exact(w * factor, h * factor, image::imageops::FilterType::Nearest);
    let mut png = Vec::new();
    big.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).ok()?;
    Some(Arc::new(gpui_kit::Image::from_bytes(gpui_kit::ImageFormat::Png, png)))
}

/// A viewer tab for images, PDFs and videos, `None` for files the editor opens.
fn viewer_tab(path: &Path, window: &mut Window, cx: &mut App) -> Option<Tab> {
    if crate::util::is_viewable_image(path) {
        Some(Tab::Image { path: path.to_path_buf(), zoom: 1., scroll: ScrollHandle::new(), crisp: crisp_image(path) })
    } else if is_pdf(path) {
        Some(Tab::Pdf(cx.new(|_| viewer::PdfView::new(path.to_path_buf()))))
    } else if video::is_video(path) {
        Some(Tab::Video(cx.new(|cx| video::VideoView::new(path.to_path_buf(), window, cx))))
    } else {
        None
    }
}

fn viewer_shows(tab: &Tab, path: &Path, cx: &App) -> bool {
    match tab {
        Tab::Image { path: p, .. } => p == path,
        Tab::Pdf(v) => v.read(cx).path == path,
        Tab::Video(v) => v.read(cx).path == path,
        _ => false,
    }
}

/// What the "…" menu of a tab strip closes.
#[derive(Clone, Copy, PartialEq)]
pub enum CloseTabs {
    /// Only tabs with nothing unsaved.
    Saved,
    /// Everything (unsaved edits are asked about first, unless autosave keeps them).
    All,
    SaveAndCloseAll,
    /// Everything, once the user agreed to lose unsaved edits.
    AllConfirmed,
}

/// An editor group: tabs shown together in one split.
struct Pane {
    id: usize,
    tabs: Vec<Tab>,
    active: Option<usize>,
    /// The file of the temporary tab (one click in the explorer, like VS Code's preview): the
    /// next file opened that way takes its place, until it is edited or double-clicked.
    transient: Option<PathBuf>,
}

impl Pane {
    fn new(id: usize) -> Self {
        Self { id, tabs: vec![], active: None, transient: None }
    }

    fn is_transient(&self, tab: &Tab, cx: &App) -> bool {
        self.transient.is_some() && tab.path(cx) == self.transient
    }
}

enum LspSlot {
    Installing,
    Starting,
    Ready(Arc<LspClient>),
    Failed(SharedString),
}

pub struct Workspace {
    root: PathBuf,
    /// Files a chat turn changed, waiting for Keep / Undo (with their content before it).
    pub reviews: HashMap<PathBuf, crate::ai::changes::FileChange>,
    /// In-window menu bar (Windows, Linux); macOS uses the system one.
    menu_bar: Option<Entity<gpui_kit::component::menu::AppMenuBar>>,
    sidebar: Option<Sidebar>,
    file_tree: Entity<FileTree>,
    search: Entity<search::SearchPanel>,
    git: Entity<GitPanel>,
    panes: Vec<Pane>,
    layout: Layout,
    /// Pane receiving opened files and editor commands.
    focused: usize,
    next_pane: usize,
    show_bottom: bool,
    show_right: bool,
    /// The target last debugged with F5, offered first next time.
    last_launch: Option<String>,
    bottom_tab: BottomTab,
    output: Entity<OutputPanel>,
    terminals: Vec<Entity<Terminal>>,
    active_terminal: usize,
    shells: Vec<Shell>,
    debug: Entity<DebugPanel>,
    chat: Entity<ChatPanel>,
    /// The AI panel when an agent module (Hermes) is installed.
    hermes: Entity<crate::ai::hermes_panel::HermesPanel>,
    /// Databases module: connections and engine, and its sidebar.
    db_hub: Entity<databases::DbHub>,
    db_panel: Entity<databases::panel::DbPanel>,
    /// Conan module's panel: dependencies, remotes, available packages.
    conan_panel: Entity<conan_panel::ConanPanel>,
    /// Watches the project folder for changes made outside ForgeCode.
    watcher: Option<watcher::ProjectWatcher>,
    /// Setup advice already shown in this project (see [`advisor`]).
    advised: std::collections::HashSet<String>,
    /// Keyed by the server command line.
    servers: HashMap<String, LspSlot>,
    /// How many times each server was restarted after stopping by itself.
    server_restarts: HashMap<String, u32>,
    lsp_events: smol::channel::Sender<LspEvent>,
    /// What the Problems view and editors show: the language server's and the linters'.
    diagnostics: BTreeMap<PathBuf, Vec<Diagnostic>>,
    lsp_diagnostics: BTreeMap<PathBuf, Vec<Diagnostic>>,
    lint_diagnostics: BTreeMap<PathBuf, Vec<Diagnostic>>,
    /// Pending shader lint per file (waits for typing to pause).
    lints: HashMap<PathBuf, Task<()>>,
    /// Live previews of the shaders shown with one.
    shader_previews: HashMap<PathBuf, Entity<shader_view::ShaderPreview>>,
    breakpoints: Breakpoints,
    /// Pending autosave timer per file (a timer costs nothing while it waits).
    autosave: HashMap<PathBuf, Task<()>>,
    focus: FocusHandle,
    _subs: Vec<Subscription>,
    _lsp_task: Task<()>,
}

fn severity_counts<'a>(diags: impl Iterator<Item = &'a Diagnostic>) -> (usize, usize) {
    diags.fold((0, 0), |(e, w), d| match d.severity {
        Some(DiagnosticSeverity::ERROR) | None => (e + 1, w),
        Some(DiagnosticSeverity::WARNING) => (e, w + 1),
        _ => (e, w),
    })
}

impl Workspace {
    pub fn new(root: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let file_tree = cx.new(|cx| FileTree::new(root.clone(), cx));
        let git = cx.new(|cx| GitPanel::new(root.clone(), window, cx));
        let search = cx.new(|cx| search::SearchPanel::new(root.clone(), window, cx));
        cx.set_global(WorkspaceHandle(cx.entity().downgrade()));
        let output = cx.new(|_| OutputPanel::new());
        let debug = cx.new(|_| DebugPanel::new());
        let chat = cx.new(|cx| ChatPanel::new(root.clone(), window, cx));
        let hermes = cx.new(|cx| crate::ai::hermes_panel::HermesPanel::new(root.clone(), window, cx));
        let db_hub = cx.new(|_| databases::DbHub::new(root.clone()));
        let db_panel = cx.new(|cx| databases::panel::DbPanel::new(db_hub.clone(), cx));
        let conan_panel = cx.new(|cx| conan_panel::ConanPanel::new(root.clone(), window, cx));

        let subs = vec![
            cx.subscribe_in(&conan_panel, window, |this, _, event: &conan_panel::ConanEvent, window, cx| match event {
                // Installing and configuring CMake take a while: in the Output panel, as it goes.
                conan_panel::ConanEvent::Run(title, args) => {
                    let Some((python, script)) = conan_panel::script(cx) else { return };
                    let quoted: Vec<String> = args.iter().map(|a| format!("\"{a}\"")).collect();
                    let line = format!("\"{}\" \"{}\" {}", python.display(), script.display(), quoted.join(" "));
                    let mut env = this.command_env(cx);
                    env.push(("FORGE_WORKSPACE".into(), this.root.display().to_string()));
                    let root = this.root.clone();
                    this.output.update(cx, |o, cx| o.run(title, &line, &root, env, cx));
                    this.show_bottom_tab(BottomTab::Output, window, cx);
                }
                conan_panel::ConanEvent::Open(path) => this.open_file(path.clone(), None, window, cx),
            }),
            cx.subscribe_in(&file_tree, window, Self::on_file_tree_event),
            cx.subscribe_in(&git, window, Self::on_git_event),
            // A CI run's log (GitHub, GitLab, Gitea) goes to the Output tab.
            cx.subscribe_in(&git.read(cx).actions.clone(), window, |this, _, event: &crate::git::actions::ActionsEvent, window, cx| match event {
                crate::git::actions::ActionsEvent::Log(title, text) => {
                    this.output.update(cx, |o, cx| {
                        o.push(format!("▶ CI: {title}"), cx);
                        for line in text.lines() {
                            o.push(line.to_string(), cx);
                        }
                    });
                    this.show_bottom_tab(BottomTab::Output, window, cx);
                }
            }),
            cx.subscribe_in(&search, window, |this, _, event: &search::SearchEvent, window, cx| match event {
                search::SearchEvent::Open(path, line) => this.open_file(path.clone(), Some(*line), window, cx),
                search::SearchEvent::Replaced(files) => {
                    for b in this.buffers_mut().filter(|b| files.contains(&b.path) && !b.dirty) {
                        b.reload(window, cx);
                    }
                    this.git.update(cx, |g, cx| g.refresh(cx));
                }
            }),
            cx.subscribe_in(&output, window, |this, _, event: &OutputEvent, window, cx| match event {
                OutputEvent::Action(action) => this.run_ide_action(action.clone(), window, cx),
                OutputEvent::Running => cx.notify(),
            }),
            cx.subscribe_in(&db_hub, window, |this, hub, event: &databases::HubEvent, window, cx| {
                use databases::HubEvent;
                match event {
                    HubEvent::OpenTable(conn, table) => {
                        let found = this.locate(|t| matches!(t, Tab::Table(v) if v.read(cx).conn == *conn && v.read(cx).table == *table));
                        match found {
                            Some((pane, ix)) => this.activate(pane, ix, window, cx),
                            None => {
                                let view = cx.new(|cx| databases::table::TableTab::new(hub.clone(), conn.clone(), table.clone(), window, cx));
                                this.open_tab(Tab::Table(view), window, cx);
                            }
                        }
                    }
                    HubEvent::OpenConsole(conn) => {
                        let view = cx.new(|cx| databases::console::ConsoleTab::new(hub.clone(), conn.clone(), window, cx));
                        this.open_tab(Tab::Console(view), window, cx);
                    }
                    HubEvent::Notify(m) => this.notify(m.clone(), window, cx),
                    HubEvent::Changed => {}
                }
                cx.notify();
            }),
            cx.subscribe_in(&hermes, window, |this, _, event: &crate::ai::hermes_panel::HermesEvent, window, cx| {
                use crate::ai::hermes_panel::HermesEvent;
                match event {
                    HermesEvent::FileChanged(path) => {
                        if let Some(b) = this.buffers_mut().find(|b| &b.path == path && !b.dirty) {
                            b.reload(window, cx);
                        }
                        this.file_tree.update(cx, |t, cx| t.refresh(cx));
                        this.git.update(cx, |g, cx| g.refresh(cx));
                    }
                    HermesEvent::UseClassicChat => Settings::update(cx, |s| s.chat_engine = "classic".into()),
                    HermesEvent::OpenSettings => this.open_settings(window, cx),
                }
                cx.notify();
            }),
            cx.subscribe_in(&chat, window, |this, _, event: &ChatEvent, window, cx| match event {
                ChatEvent::Effect(SideEffect::OpenFile(path, line)) => this.open_file(path.clone(), *line, window, cx),
                ChatEvent::Effect(SideEffect::OpenChanges(path, before)) => {
                    let name = path.strip_prefix(&this.root).unwrap_or(path).display().to_string();
                    let title = format!("{} ({})", path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(), t("chat changes"));
                    let (path, before) = (path.clone(), before.clone());
                    this.open_patch(title, move || Ok(crate::ai::changes::patch(&path, &name, before.as_deref())), window, cx);
                }
                ChatEvent::Effect(SideEffect::FileChanged(path)) => {
                    if let Some(b) = this.buffers_mut().find(|b| &b.path == path && !b.dirty) {
                        b.reload(window, cx);
                    }
                    this.file_tree.update(cx, |t, cx| t.refresh(cx));
                    this.git.update(cx, |g, cx| g.refresh(cx));
                }
                ChatEvent::Effect(SideEffect::WorkspaceChanged) => this.sync_with_disk(window, cx),
                ChatEvent::Effect(SideEffect::ReviewChanges(files)) => this.add_reviews(files.clone(), cx),
                ChatEvent::Effect(SideEffect::SettleReviews(keep)) => this.settle_all_reviews(*keep, window, cx),
                ChatEvent::Effect(SideEffect::None) => {}
                ChatEvent::OpenSettings => this.open_settings(window, cx),
                ChatEvent::AddSelection => this.add_selection_to_chat(&AddSelectionToChat, window, cx),
                ChatEvent::PickProjectFile => {
                    let chat = this.chat.downgrade();
                    quick_open::Picker::files(this.root.clone(), t("Link a file…").into(), window, cx, move |path, _, cx| {
                        let _ = chat.update(cx, |c, cx| c.attach([path], cx));
                    });
                }
                ChatEvent::OpenAgents => match this.locate(|t| matches!(t, Tab::Agents(_))) {
                    Some((pane, ix)) => this.activate(pane, ix, window, cx),
                    None => {
                        let tree = this.chat.read(cx).agents.clone();
                        this.open_tab(Tab::Agents(tree), window, cx);
                    }
                },
            }),
            cx.subscribe_in(&debug, window, |this, _, event: &DebugEvent, window, cx| match event {
                DebugEvent::Stopped(path, line) => {
                    this.open_file(path.clone(), Some(*line), window, cx);
                    this.set_debug_line(Some((path.clone(), *line)), cx);
                }
                DebugEvent::Ended => this.set_debug_line(None, cx),
                DebugEvent::Reveal(path, line) => this.open_file(path.clone(), Some(*line), window, cx),
                DebugEvent::RemoveBreakpoint(path, line) => this.toggle_breakpoint_at(path.clone(), *line, cx),
            }),
            // Modules changed (installed, removed, reloaded): menus and buttons follow.
            cx.observe_global::<Plugins>(|this, cx| {
                menus::install(cx);
                if let Some(bar) = &this.menu_bar {
                    bar.update(cx, |b, cx| b.reload(cx));
                }
                cx.notify()
            }),
            cx.observe_global::<Settings>(|this, cx| {
                // A changed type checking level reaches the running servers.
                let settings = crate::language::server_settings(Settings::get(cx), &this.root);
                for slot in this.servers.values() {
                    if let LspSlot::Ready(client) = slot {
                        client.configure(settings.clone());
                    }
                }
                cx.notify()
            }),
            // Leaving the window saves everything, like JetBrains IDEs.
            cx.observe_window_activation(window, |this, window, cx| {
                if !window.is_window_active() && Settings::get(cx).autosave_ms > 0 {
                    this.save_all(cx);
                }
                // Back in the window: pick up what changed outside (terminal, other tools).
                if window.is_window_active() {
                    this.sync_with_disk(window, cx);
                }
            }),
            cx.on_app_quit(|this, cx| {
                this.save_all(cx);
                async {}
            }),
            cx.observe_global::<CopilotState>(|_, cx| cx.notify()),
        ];

        let (lsp_events, rx) = smol::channel::unbounded();
        let _lsp_task = cx.spawn(async move |this, cx| {
            while let Ok(event) = rx.recv().await {
                let ok = match event {
                    LspEvent::Diagnostics { uri, diagnostics } => {
                        let Some(path) = path_from_uri(&uri) else { continue };
                        this.update(cx, |this, cx| this.set_diagnostics(path, diagnostics, cx))
                    }
                    LspEvent::Exited { id } => this.update(cx, |this, cx| this.on_server_exit(id, cx)),
                };
                if ok.is_err() {
                    break;
                }
            }
        });

        {
            let root = root.clone();
            cx.background_spawn(async move {
                if let Some(cmd) = crate::project::configure_command(&root) {
                    let _ = crate::ai::tools::run_shell(&cmd, &root, &[], 120);
                }
            })
            .detach();
        }
        Self::load_project_folder(&root, cx);
        // Database changes an agent asks for: shown here, answered by the user.
        let (confirm_tx, confirm_rx) = smol::channel::unbounded::<crate::ai::gateway::Confirmation>();
        crate::ai::gateway::set_confirm_handler(confirm_tx);
        cx.spawn_in(window, async move |_, cx| {
            while let Ok((request, answer)) = confirm_rx.recv().await {
                let _ = cx.update(|window, cx| databases::confirm_agent_change(request, answer, window, cx));
            }
        })
        .detach();
        cx.defer_in(window, |this, window, cx| {
            this.watch_project(window, cx);
            this.run_triggered(None, |c| c.on_startup, window, cx);
        });
        Self {
            root,
            reviews: HashMap::new(),
            menu_bar:(!cfg!(target_os = "macos")).then(|| gpui_kit::component::menu::AppMenuBar::new(cx)),
            sidebar: Some(Sidebar::Explorer),
            file_tree,
            search,
            git,
            panes: vec![Pane::new(0)],
            layout: Layout::Pane(0),
            focused: 0,
            next_pane: 1,
            show_bottom: false,
            show_right: true,
            last_launch: None,
            bottom_tab: BottomTab::Problems,
            output,
            terminals: vec![],
            active_terminal: 0,
            shells: available_shells(),
            debug,
            chat,
            hermes,
            db_hub,
            db_panel,
            conan_panel,
            advised: Default::default(),
            watcher: None,
            servers: HashMap::new(),
            server_restarts: HashMap::new(),
            lsp_events,
            diagnostics: BTreeMap::new(),
            lsp_diagnostics: BTreeMap::new(),
            lint_diagnostics: BTreeMap::new(),
            lints: HashMap::new(),
            shader_previews: HashMap::new(),
            breakpoints: Breakpoints::new(),
            autosave: HashMap::new(),
            focus: cx.focus_handle(),
            _subs: subs,
            _lsp_task,
        }
    }

    // ---- panes & tabs ---------------------------------------------------------------

    fn pane(&self, id: usize) -> Option<&Pane> {
        self.panes.iter().find(|p| p.id == id)
    }

    fn pane_mut(&mut self, id: usize) -> Option<&mut Pane> {
        self.panes.iter_mut().find(|p| p.id == id)
    }

    fn tabs(&self) -> impl Iterator<Item = &Tab> {
        self.panes.iter().flat_map(|p| &p.tabs)
    }

    fn buffers(&self) -> impl Iterator<Item = &Buffer> {
        self.tabs().filter_map(|t| match t {
            Tab::File(b) => Some(b),
            _ => None,
        })
    }

    fn buffers_mut(&mut self) -> impl Iterator<Item = &mut Buffer> {
        self.panes.iter_mut().flat_map(|p| &mut p.tabs).filter_map(|t| match t {
            Tab::File(b) => Some(b),
            _ => None,
        })
    }

    fn active_tab(&self) -> Option<&Tab> {
        let pane = self.pane(self.focused)?;
        pane.tabs.get(pane.active?)
    }

    fn active_buffer(&self) -> Option<&Buffer> {
        match self.active_tab() {
            Some(Tab::File(b)) => Some(b),
            _ => None,
        }
    }

    fn active_buffer_mut(&mut self) -> Option<&mut Buffer> {
        let focused = self.focused;
        let pane = self.pane_mut(focused)?;
        match pane.tabs.get_mut(pane.active?) {
            Some(Tab::File(b)) => Some(b),
            _ => None,
        }
    }

    /// (pane id, tab index) of the first tab matching `pred`.
    fn locate(&self, pred: impl Fn(&Tab) -> bool) -> Option<(usize, usize)> {
        self.panes.iter().find_map(|p| p.tabs.iter().position(&pred).map(|ix| (p.id, ix)))
    }

    fn notify(&self, message: impl Into<SharedString>, window: &mut Window, cx: &mut App) {
        window.push_notification(Notification::new().message(message), cx);
    }

    /// Add a tab to the focused pane and show it.
    fn open_tab(&mut self, tab: Tab, window: &mut Window, cx: &mut Context<Self>) {
        let focused = self.focused;
        let Some(pane) = self.pane_mut(focused) else { return };
        pane.tabs.push(tab);
        let ix = pane.tabs.len() - 1;
        self.activate(focused, ix, window, cx);
    }

    /// A new browser tab, on the given page or a blank one.
    fn open_browser(&mut self, url: &str, window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.new(|cx| browser::BrowserView::new(url, window, cx));
        cx.observe(&view, |_, _, cx| cx.notify()).detach();
        self.open_tab(Tab::Browser(view), window, cx);
    }

    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.locate(|t| matches!(t, Tab::Settings)) {
            Some((pane, ix)) => self.activate(pane, ix, window, cx),
            None => self.open_tab(Tab::Settings, window, cx),
        }
    }

    fn activate(&mut self, pane: usize, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.pane_mut(pane).filter(|p| ix < p.tabs.len()) else { return };
        p.active = Some(ix);
        self.focused = pane;
        if let Some(Tab::File(b)) = self.pane(pane).and_then(|p| p.tabs.get(ix)) {
            let (path, editor) = (b.path.clone(), b.editor.clone());
            self.chat.update(cx, |c, _| c.active_file = Some(path));
            editor.update(cx, |e, cx| e.focus(window, cx));
        }
        cx.notify();
    }

    fn close_tab(&mut self, pane: usize, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        // With autosave on, closing keeps the edits; with it off they are dropped (no prompt).
        let autosave = Settings::get(cx).autosave_ms > 0;
        let Some(p) = self.pane_mut(pane).filter(|p| ix < p.tabs.len()) else { return };
        if p.is_transient(&p.tabs[ix], cx) {
            p.transient = None;
        }
        if let Tab::File(mut b) = p.tabs.remove(ix) {
            if autosave && b.dirty {
                let _ = b.save(cx);
            }
            if let Some(lsp) = &b.lsp {
                lsp.did_close(&b.path);
            }
        }
        let next = p.active.map(|a| if a > ix || a == p.tabs.len() { a.saturating_sub(1) } else { a }).filter(|_| !p.tabs.is_empty());
        p.active = next;
        if self.remove_pane_if_empty(pane) {
            return cx.notify();
        }
        if let Some(next) = next {
            self.activate(pane, next, window, cx);
        }
        cx.notify();
    }

    /// Drop an empty split (never the last pane).
    fn remove_pane_if_empty(&mut self, pane: usize) -> bool {
        let empty = self.pane(pane).is_some_and(|p| p.tabs.is_empty());
        if !empty || self.panes.len() == 1 || !self.layout.remove(pane) {
            return false;
        }
        self.panes.retain(|p| p.id != pane);
        if self.focused == pane {
            self.focused = self.layout.panes()[0];
        }
        true
    }

    /// The pane a drop on `target`/`zone` goes to, creating a split if needed.
    fn pane_for_drop(&mut self, target: usize, zone: Zone) -> usize {
        if zone == Zone::Center {
            return target;
        }
        let id = self.next_pane;
        self.next_pane += 1;
        self.panes.push(Pane::new(id));
        self.layout.split(target, id, zone);
        id
    }

    /// Move a tab to another pane, or into a new split next to it.
    fn move_tab(&mut self, from: usize, ix: usize, to: usize, zone: Zone, window: &mut Window, cx: &mut Context<Self>) {
        let alone = self.pane(from).is_some_and(|p| p.tabs.len() == 1);
        if from == to && (zone == Zone::Center || alone) {
            return;
        }
        let Some(p) = self.pane_mut(from).filter(|p| ix < p.tabs.len()) else { return };
        let tab = p.tabs.remove(ix);
        p.active = (!p.tabs.is_empty()).then(|| p.active.unwrap_or(0).min(p.tabs.len() - 1));
        let target = self.pane_for_drop(to, zone);
        let Some(t) = self.pane_mut(target) else { return };
        t.tabs.push(tab);
        let new_ix = t.tabs.len() - 1;
        self.remove_pane_if_empty(from);
        self.activate(target, new_ix, window, cx);
    }

    /// Open a file into a pane/zone (moving its tab if it is already open: a file has one tab).
    fn open_path_in(&mut self, path: PathBuf, to: usize, zone: Zone, window: &mut Window, cx: &mut Context<Self>) {
        let path = crate::util::canonical(&path).unwrap_or(path);
        if path.is_dir() {
            return;
        }
        if let Some((pane, ix)) = self.locate(|t| matches!(t, Tab::File(b) if b.path == path) || viewer_shows(t, &path, cx)) {
            return self.move_tab(pane, ix, to, zone, window, cx);
        }
        let tab = match viewer_tab(&path, window, cx) {
            Some(tab) => tab,
            None => Tab::File(match self.create_buffer(path, window, cx) {
                Some(buffer) => buffer,
                None => return,
            }),
        };
        let target = self.pane_for_drop(to, zone);
        if let Some(p) = self.pane_mut(target) {
            p.tabs.push(tab);
            let ix = p.tabs.len() - 1;
            self.activate(target, ix, window, cx);
        }
    }

    fn split_right(&mut self, _: &SplitRight, window: &mut Window, cx: &mut Context<Self>) {
        let focused = self.focused;
        if let Some(ix) = self.pane(focused).and_then(|p| p.active) {
            self.move_tab(focused, ix, focused, Zone::Right, window, cx);
        }
    }

    // ---- files ---------------------------------------------------------------

    fn create_buffer(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Option<Buffer> {
        let text = match std::fs::read(&path).map(String::from_utf8) {
            Ok(Ok(text)) => text,
            Ok(Err(_)) => {
                self.notify(t("Binary files are not supported"), window, cx);
                return None;
            }
            Err(e) => {
                self.notify(format!("{}: {e}", path.display()), window, cx);
                return None;
            }
        };
        let lang = language::detect(&path, Plugins::global(cx));
        let buffer = Buffer::new(path.clone(), text, lang.clone(), window, cx);
        buffer.show_breakpoints(self.breakpoints.get(&path), cx);
        buffer.show_review(self.reviews.get(&path).map(|r| &r.before), cx);
        if let Some(d) = self.diagnostics.get(&path) {
            buffer.set_diagnostics(d, cx);
        }
        let sub = cx.subscribe_in(&buffer.editor, window, {
            let path = path.clone();
            move |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => {
                    this.on_buffer_changed(&path, cx);
                    // A shader's preview redraws as it is typed.
                    if this.shader_previews.contains_key(&path) {
                        this.refresh_shader_preview(&path, window, cx);
                    }
                }
                // Leaving the editor with a selection: offer it to the chat (the editor hides it while unfocused).
                InputEvent::Blur => {
                    let snippet = this.buffers().find(|b| b.path == path).and_then(|b| selection_snippet(b, false, cx));
                    this.chat.update(cx, |c, cx| c.suggest_selection(snippet, cx));
                }
                _ => {}
            }
        });
        self._subs.push(sub);
        // Start the server now; attach once the buffer is in its tab (it isn't yet: a server
        // already running would not find it, and the file would get no errors or completions).
        self.ensure_server(&lang, cx);
        cx.defer_in(window, move |this, _, cx| this.ensure_server(&lang, cx));
        self.refresh_git_markers(Some(path), cx);
        Some(buffer)
    }

    /// Images and PDFs get a viewer tab; other binary files open in the system's default app.
    /// Returns true when handled here.
    fn open_non_text(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if let Some((pane, ix)) = self.locate(|t| viewer_shows(t, path, cx)) {
            self.activate(pane, ix, window, cx);
            return true;
        }
        if let Some(tab) = viewer_tab(path, window, cx) {
            self.open_tab(tab, window, cx);
            return true;
        }
        // SQLite files open in the databases panel when the module is installed.
        if databases::is_sqlite(path) && Plugins::global(cx).database().is_some() {
            let p = path.display().to_string();
            self.db_hub.update(cx, |h, cx| {
                let id = h.conns.iter().find(|c| c.path == p).map(|c| c.id.clone());
                match id {
                    Some(id) => h.refresh_schema(&id, cx),
                    None => {
                        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                        h.add_or_update(databases::Conn { id: format!("file:{p}"), name, kind: "sqlite".into(), path: p.clone(), ..Default::default() }, None, cx);
                    }
                }
            });
            self.sidebar = Some(Sidebar::Databases);
            cx.notify();
            return true;
        }
        let binary = std::fs::read(path).map(|b| std::str::from_utf8(&b[..b.len().min(8192)]).is_err()).unwrap_or(false);
        if binary {
            cx.open_with_system(path);
            return true;
        }
        false
    }

    pub fn open_file(&mut self, path: PathBuf, line: Option<u32>, window: &mut Window, cx: &mut Context<Self>) {
        let path = crate::util::canonical(&path).unwrap_or(path);
        // Opened for good (double click, search, chat…): a temporary tab of it stays.
        self.keep_transient(&path);
        if self.open_non_text(&path, window, cx) {
            return;
        }
        match self.locate(|t| matches!(t, Tab::File(b) if b.path == path)) {
            Some((pane, ix)) => self.activate(pane, ix, window, cx),
            None => {
                let Some(buffer) = self.create_buffer(path.clone(), window, cx) else { return };
                let lsp_id = buffer.lang.lsp_id.clone();
                self.open_tab(Tab::File(buffer), window, cx);
                self.run_triggered(Some(&lsp_id), |c| c.on_open, window, cx);
                self.advise(&lsp_id, window, cx);
                self.lint_soon(&path, cx);
            }
        }
        if let (Some(line), Some(b)) = (line, self.active_buffer()) {
            b.go_to_line(line, window, cx);
        }
    }

    /// One click in the explorer, like VS Code: a temporary tab (title in italics) that the next
    /// file opened this way replaces; editing it or a double click keeps it.
    /// Any kind of file: text, image, PDF, video.
    pub fn open_file_preview(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let path = crate::util::canonical(&path).unwrap_or(path);
        if let Some((pane, ix)) = self.locate(|t| t.path(cx).as_ref() == Some(&path)) {
            return self.activate(pane, ix, window, cx);
        }
        let pane = self.focused;
        // The current temporary tab goes, unless it holds unsaved edits.
        let replaced = self.pane(pane).and_then(|p| p.tabs.iter().position(|t| p.is_transient(t, cx) && !matches!(t, Tab::File(b) if b.dirty)));
        if let Some(ix) = replaced {
            self.close_tab(pane, ix, window, cx);
        }
        let pane = self.focused;
        let before = self.pane(pane).map_or(0, |p| p.tabs.len());
        self.open_file(path.clone(), None, window, cx);
        // A tab was added (files opened by another app add none): it is the temporary one,
        // in the place of the one it replaced.
        let Some(p) = self.pane_mut(pane).filter(|p| p.tabs.len() == before + 1) else { return };
        p.transient = Some(path);
        if let Some(ix) = replaced.filter(|ix| *ix < p.tabs.len()) {
            let tab = p.tabs.pop().unwrap();
            p.tabs.insert(ix, tab);
            self.activate(pane, ix, window, cx);
        }
        cx.notify();
    }

    /// `path` is kept open: no longer a temporary tab anywhere.
    fn keep_transient(&mut self, path: &Path) {
        for p in &mut self.panes {
            if p.transient.as_deref() == Some(path) {
                p.transient = None;
            }
        }
    }

    /// Keep a temporary tab (double click on it).
    fn pin_tab(&mut self, pane: usize, ix: usize, cx: &mut Context<Self>) {
        if let Some(path) = self.pane(pane).and_then(|p| p.tabs.get(ix)).and_then(|t| t.path(cx)) {
            self.keep_transient(&path);
            cx.notify();
        }
    }

    /// The "…" menu of a tab strip: close what is saved, everything, or save then close.
    pub fn close_tabs(&mut self, pane: usize, how: CloseTabs, window: &mut Window, cx: &mut Context<Self>) {
        if how == CloseTabs::SaveAndCloseAll {
            self.save_all(cx);
        }
        let Some(p) = self.pane(pane) else { return };
        // Without autosave, closing drops unsaved edits: ask first.
        let unsaved = p.tabs.iter().filter(|t| matches!(t, Tab::File(b) if b.dirty)).count();
        if how == CloseTabs::All && unsaved > 0 && Settings::get(cx).autosave_ms == 0 {
            let ws = cx.entity().downgrade();
            let title = format!("{unsaved} {}", t(if unsaved == 1 { "unsaved file" } else { "unsaved files" }));
            return dialogs::confirm(title, t("Their changes will be lost."), t("Close all"), window, cx, move |w, cx| {
                let _ = ws.update(cx, |ws, cx| ws.close_tabs(pane, CloseTabs::AllConfirmed, w, cx));
            });
        }
        // From the last, so indices stay valid while closing.
        let doomed: Vec<usize> = (0..p.tabs.len())
            .rev()
            .filter(|&ix| how != CloseTabs::Saved || !matches!(&p.tabs[ix], Tab::File(b) if b.dirty))
            .collect();
        for ix in doomed {
            self.close_tab(pane, ix, window, cx);
        }
    }

    fn on_buffer_changed(&mut self, path: &Path, cx: &mut Context<Self>) {
        // Edited: a temporary tab is kept from now on.
        let was_transient = self.panes.iter().any(|p| p.transient.as_deref() == Some(path));
        self.keep_transient(path);
        let Some(b) = self.buffers_mut().find(|b| b.path == path) else { return };
        let had_conflicts = !b.conflicts.is_empty();
        b.refresh_conflicts(cx);
        let repaint = !b.dirty || was_transient || b.preview != Preview::Off || had_conflicts != !b.conflicts.is_empty();
        b.dirty = true;
        if let Some(lsp) = &b.lsp {
            lsp.sync(&b.path, &b.editor.read(cx).value());
        }
        if repaint {
            cx.notify();
        }
        self.lint_soon(path, cx);
        let delay = Settings::get(cx).autosave_ms;
        if delay > 0 {
            let path = path.to_path_buf();
            let task = cx.spawn({
                let path = path.clone();
                async move |this, cx| {
                    cx.background_executor().timer(std::time::Duration::from_millis(delay)).await;
                    let _ = this.update(cx, |this, cx| this.save_path(&path, cx));
                }
            });
            self.autosave.insert(path, task);
        }
    }

    /// Quiet save (no format hooks) used by autosave.
    fn save_path(&mut self, path: &Path, cx: &mut Context<Self>) {
        self.autosave.remove(path);
        let Some(b) = self.buffers_mut().find(|b| b.path == path && b.dirty) else { return };
        if let Err(e) = b.save(cx) {
            eprintln!("autosave {}: {e}", path.display());
            return;
        }
        self.git.update(cx, |g, cx| g.refresh(cx));
        self.refresh_git_markers(Some(path.to_path_buf()), cx);
        cx.notify();
    }

    fn save_all(&mut self, cx: &mut Context<Self>) {
        let dirty: Vec<PathBuf> = self.buffers().filter(|b| b.dirty).map(|b| b.path.clone()).collect();
        for path in dirty {
            self.save_path(&path, cx);
        }
    }

    fn format_document(&mut self, _: &FormatDocument, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(b) = self.active_buffer() {
            b.format(window, cx);
        }
    }

    fn show_search(&mut self, _: &ShowSearch, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar = Some(Sidebar::Search);
        self.search.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    fn save(&mut self, _: &Save, window: &mut Window, cx: &mut Context<Self>) {
        if Settings::get(cx).format_on_save {
            if let Some(b) = self.active_buffer() {
                b.format(window, cx);
            }
        }
        let Some(b) = self.active_buffer_mut() else { return };
        if let Err(e) = b.save(cx) {
            return self.notify(format!("{}: {e}", t("Save failed")), window, cx);
        }
        let (path, lsp_id) = (b.path.clone(), b.lang.lsp_id.clone());
        if path == crate::dotforge::settings_path(&self.root) {
            Settings::set_project(cx, Some(&self.root));
        }
        // Other servers learn about the file too (a new module others can now import).
        for slot in self.servers.values() {
            if let LspSlot::Ready(client) = slot {
                client.notify("workspace/didChangeWatchedFiles", serde_json::json!({"changes": [{"uri": crate::lsp::client::uri(&path), "type": 2}]}));
            }
        }
        // Module `on_save` hooks run whatever the format-on-save setting.
        self.run_triggered(Some(&lsp_id), |c| c.on_save, window, cx);
        self.git.update(cx, |g, cx| g.refresh(cx));
        self.refresh_git_markers(Some(path), cx);
        cx.notify();
    }

    /// `⌘→`: accept the next word of the Copilot suggestion, else move to the end of the line.
    fn accept_suggestion_word(&mut self, _: &AcceptSuggestionWord, window: &mut Window, cx: &mut Context<Self>) {
        // Only while the suggestion is on screen (not one dismissed with Escape).
        let word = self.active_buffer().filter(|b| b.editor.read(cx).focus_handle(cx).is_focused(window) && b.editor.read(cx).has_inline_completion()).and_then(|b| {
            let word = b.intel.next_suggested_word(&b.prefix(cx))?;
            Some((b.editor.clone(), b.intel.clone(), word))
        });
        match word {
            Some((editor, intel, word)) => editor.update(cx, |e, cx| {
                e.insert(word, window, cx);
                // The rest stays shown, as in VS Code: the edit would hide it and open the menu.
                e.dismiss_lsp_overlays(cx);
                let text = e.value();
                let prefix = &text[..e.cursor().min(text.len())];
                e.show_inline_suggestion(intel.suggestion_rest(prefix).unwrap_or_default(), cx);
            }),
            // The editor's own ⌘→ / End (this binding shadows it).
            None => window.dispatch_action(Box::new(MoveEnd), cx),
        }
    }

    /// The AI panel: Hermes when its module is installed and chosen, else the classic chat.
    fn ai_panel(&self, cx: &App) -> AnyElement {
        if Settings::get(cx).chat_engine != "classic" && crate::ai::hermes_panel::HermesPanel::has_agent(cx) {
            self.hermes.clone().into_any_element()
        } else {
            self.chat.clone().into_any_element()
        }
    }

    /// Link the editor selection (or the current line) to the chat, like Copilot Chat.
    fn add_selection_to_chat(&mut self, _: &AddSelectionToChat, window: &mut Window, cx: &mut Context<Self>) {
        let Some(snippet) = self.active_buffer().and_then(|b| selection_snippet(b, true, cx)) else { return };
        self.show_right = true;
        self.chat.update(cx, |c, cx| {
            c.attach_context(snippet, cx);
            c.focus(window, cx);
        });
        cx.notify();
    }

    fn quick_open(&mut self, _: &QuickOpenFile, window: &mut Window, cx: &mut Context<Self>) {
        let ws = cx.entity().downgrade();
        quick_open::Picker::files(self.root.clone(), t("Go to file…").into(), window, cx, move |path, window, cx| {
            let _ = ws.update(cx, |w, cx| w.open_file(path, None, window, cx));
        });
    }

    fn open_folder(&mut self, _: &OpenFolder, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions { files: false, directories: true, multiple: false, prompt: None });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let Some(root) = paths.into_iter().next() else { return };
            let _ = this.update_in(cx, |this, _, cx| this.set_root(root, cx));
        })
        .detach();
    }

    fn new_project(&mut self, _: &NewProject, window: &mut Window, cx: &mut Context<Self>) {
        let location = self.root.parent().map(Path::to_path_buf).unwrap_or_else(|| self.root.clone());
        new_project::NewProject::open(location, cx.entity().downgrade(), window, cx);
    }

    /// The CMake build description changed: configure again in the background, so the
    /// libraries it now pulls in are completed and checked.
    pub(super) fn reconfigure_if_cmake(&self, changed: &Path, cx: &mut Context<Self>) {
        if crate::project::is_cmake_file(changed) {
            let (root, changed) = (self.root.clone(), changed.to_path_buf());
            cx.background_spawn(async move {
                if let Some(cmd) = crate::project::reconfigure_command(&root, &changed) {
                    let _ = crate::ai::tools::run_shell(&cmd, &root, &[], 300);
                }
            })
            .detach();
        }
    }

    /// CMake projects get `build/compile_commands.json` in the background so clangd understands them.
    fn configure_project(&self, cx: &mut Context<Self>) {
        let root = self.root.clone();
        cx.background_spawn(async move {
            if let Some(cmd) = crate::project::configure_command(&root) {
                let _ = crate::ai::tools::run_shell(&cmd, &root, &[], 120);
            }
        })
        .detach();
    }

    /// `.forgecode/`: created for real projects, and its settings become the project layer.
    fn load_project_folder(root: &Path, cx: &mut App) {
        crate::recent::touch(root);
        if crate::dotforge::is_project(root) {
            if let Err(e) = crate::dotforge::ensure(root) {
                eprintln!(".forgecode: {e}");
            }
        }
        Settings::set_project(cx, Some(root));
    }

    fn open_project_file(&mut self, which: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(e) = crate::dotforge::ensure(&self.root) {
            return self.notify(e.to_string(), window, cx);
        }
        let path = match which {
            "settings" => crate::dotforge::settings_path(&self.root),
            _ => crate::dotforge::instructions_path(&self.root),
        };
        self.file_tree.update(cx, |t, cx| t.refresh(cx));
        self.open_file(path, None, window, cx);
    }

    fn set_root(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        let root = crate::util::plain_path(root);
        self.root = root.clone();
        self.configure_project(cx);
        Self::load_project_folder(&root, cx);
        self.servers.clear(); // servers are rooted in the old folder
        self.server_restarts.clear();
        // Files staying open get the new folder's servers.
        let mut langs: Vec<LanguageInfo> = vec![];
        for b in self.buffers_mut() {
            b.detach_lsp(cx);
            if !langs.iter().any(|l| l.server == b.lang.server) {
                langs.push(b.lang.clone());
            }
        }
        for lang in langs {
            self.ensure_server(&lang, cx);
        }
        self.terminals.clear();
        self.diagnostics.clear();
        self.lsp_diagnostics.clear();
        self.lint_diagnostics.clear();
        self.file_tree.update(cx, |t, cx| t.set_root(root.clone(), cx));
        self.search.update(cx, |s, cx| s.set_root(root.clone(), cx));
        self.git.update(cx, |g, cx| g.set_workspace(root.clone(), cx));
        let conan_open = self.sidebar == Some(Sidebar::Conan);
        self.conan_panel.update(cx, |c, cx| if conan_open { c.set_root(root.clone(), cx) } else { c.root = root.clone() });
        self.advised.clear();
        self.db_hub.update(cx, |h, cx| h.set_root(root.clone(), cx));
        // Watch the new folder (needs the window: done on the next frame).
        self.watcher = None;
        if let Some(window) = cx.windows().first().copied() {
            let ws = cx.entity().downgrade();
            cx.defer(move |cx| {
                let _ = window.update(cx, |_, window, cx| {
                    let _ = ws.update(cx, |w, cx| w.watch_project(window, cx));
                });
            });
        }
        self.hermes.update(cx, |h, cx| h.set_root(root.clone(), cx));
        self.chat.update(cx, |c, cx| {
            c.workspace = root;
            c.new_chat(cx);
        });
        cx.notify();
    }

    /// Close every tab whose file is `path` or inside it.
    fn close_files_under(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) -> Vec<PathBuf> {
        let mut closed = vec![];
        while let Some((pane, ix)) = self.locate(|t| matches!(t, Tab::File(b) if b.path.starts_with(path))) {
            if let Some(Tab::File(b)) = self.pane(pane).and_then(|p| p.tabs.get(ix)) {
                closed.push(b.path.clone());
            }
            self.close_tab(pane, ix, window, cx);
        }
        closed
    }

    fn on_file_tree_event(&mut self, _: &Entity<FileTree>, event: &FileTreeEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ws = cx.entity().downgrade();
        let done = move |result: std::io::Result<()>, window: &mut Window, cx: &mut App| {
            let _ = ws.update(cx, |this, cx| {
                if let Err(e) = result {
                    this.notify(e.to_string(), window, cx);
                }
                this.file_tree.update(cx, |t, cx| t.refresh(cx));
                this.git.update(cx, |g, cx| g.refresh(cx));
            });
        };
        let ws = cx.entity().downgrade();
        match event {
            FileTreeEvent::Open(path) => self.open_file(path.clone(), None, window, cx),
            FileTreeEvent::Preview(path) => self.open_file_preview(path.clone(), window, cx),
            FileTreeEvent::Created(path) => {
                self.git.update(cx, |g, cx| g.refresh(cx));
                if path.is_file() {
                    self.open_file(path.clone(), None, window, cx);
                }
            }
            FileTreeEvent::Renamed(from, to) => {
                // Reopen tabs of the renamed file (or of files inside a renamed folder).
                for old in self.close_files_under(from, window, cx) {
                    let new = to.join(old.strip_prefix(from).unwrap_or(Path::new("")));
                    self.open_file(if &old == from { to.clone() } else { new }, None, window, cx);
                }
                self.git.update(cx, |g, cx| g.refresh(cx));
            }
            FileTreeEvent::Error(e) => self.notify(e.clone(), window, cx),
            FileTreeEvent::Delete(path) => {
                let path = path.clone();
                let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                dialogs::confirm(format!("{} {name} ?", t("Delete")), t("This cannot be undone."), t("Delete"), window, cx, move |window, cx| {
                    let result = if path.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
                    if result.is_ok() {
                        let _ = ws.update(cx, |this, cx| {
                            this.close_files_under(&path, window, cx);
                        });
                    }
                    done(result, window, cx);
                });
            }
            FileTreeEvent::OpenInTerminal(dir) => {
                if let Some(shell) = self.shells.first().cloned() {
                    self.new_terminal(shell, Some(dir.clone()), window, cx);
                }
            }
        }
    }

    // ---- language servers -------------------------------------------------------

    fn ensure_server(&mut self, lang: &LanguageInfo, cx: &mut Context<Self>) {
        if lang.server.is_empty() {
            return;
        }
        let key = lang.server.join(" ");
        if let Some(LspSlot::Ready(client)) = self.servers.get(&key) {
            let client = client.clone();
            for b in self.buffers_mut().filter(|b| b.lang.server == lang.server && b.lsp.is_none()) {
                b.attach_lsp(client.clone(), cx);
            }
            return;
        }
        if self.servers.contains_key(&key) {
            return;
        }
        let slot = if crate::lsp::install::needs_install(&lang.server[0]) { LspSlot::Installing } else { LspSlot::Starting };
        self.servers.insert(key.clone(), slot);
        let (argv, root, events) = (lang.server.clone(), self.root.clone(), self.lsp_events.clone());
        let server_settings = crate::language::server_settings(Settings::get(cx), &self.root);
        let lang = lang.clone();
        cx.spawn(async move |this, cx| {
            let started = cx
                .background_spawn(async move { LspClient::start(&crate::lsp::install::resolve(&argv)?, &root, events, server_settings) })
                .await;
            let _ = this.update(cx, |this, cx| {
                match started {
                    Ok(client) => {
                        this.servers.insert(key, LspSlot::Ready(client));
                        this.ensure_server(&lang, cx);
                    }
                    Err(e) => {
                        eprintln!("language server: {e:#}");
                        this.servers.insert(key, LspSlot::Failed(format!("{e:#}").into()));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// A server stopped by itself: restart it (a few times at most) and reattach its files.
    fn on_server_exit(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(key) = self.servers.iter().find(|(_, s)| matches!(s, LspSlot::Ready(c) if c.id == id)).map(|(k, _)| k.clone()) else { return };
        self.servers.remove(&key);
        let mut lang = None;
        for b in self.buffers_mut().filter(|b| b.lsp.as_ref().is_some_and(|c| c.id == id)) {
            b.detach_lsp(cx);
            lang = Some(b.lang.clone());
        }
        let restarts = self.server_restarts.entry(key.clone()).or_insert(0);
        *restarts += 1;
        if *restarts > 3 {
            self.servers.insert(key, LspSlot::Failed(t("The language server stopped several times.").into()));
        } else if let Some(lang) = lang {
            self.ensure_server(&lang, cx);
        }
        cx.notify();
    }

    /// (label, healthy, why it failed) for the active file's language server.
    fn server_status(&self) -> Option<(SharedString, bool, Option<SharedString>)> {
        let b = self.active_buffer()?;
        let name = b.lang.server.first()?.clone();
        Some(match self.servers.get(&b.lang.server.join(" ")) {
            Some(LspSlot::Ready(_)) => (name.into(), true, None),
            Some(LspSlot::Starting) => (format!("{name} …").into(), true, None),
            Some(LspSlot::Installing) => (format!("{} {name}…", t("Installing")).into(), true, None),
            Some(LspSlot::Failed(e)) => (format!("{name} ✕").into(), false, Some(e.clone())),
            None => (format!("{name} ✕").into(), false, None),
        })
    }

    /// The language server's diagnostics for `path`.
    fn set_diagnostics(&mut self, path: PathBuf, diagnostics: Vec<Diagnostic>, cx: &mut Context<Self>) {
        Self::store(&mut self.lsp_diagnostics, &path, diagnostics);
        self.publish_diagnostics(path, cx);
    }

    /// A linter's diagnostics for `path` (they don't replace the language server's).
    fn set_lint(&mut self, path: PathBuf, diagnostics: Vec<Diagnostic>, cx: &mut Context<Self>) {
        Self::store(&mut self.lint_diagnostics, &path, diagnostics);
        self.publish_diagnostics(path, cx);
    }

    fn store(map: &mut BTreeMap<PathBuf, Vec<Diagnostic>>, path: &Path, diagnostics: Vec<Diagnostic>) {
        if diagnostics.is_empty() {
            map.remove(path);
        } else {
            map.insert(path.to_path_buf(), diagnostics);
        }
    }

    /// The live preview of the shader `path` follows its editor; created when first shown,
    /// dropped when the preview is closed.
    pub fn refresh_shader_preview(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let Some(b) = self.buffers().find(|b| b.path == path && b.lang.highlight == "glsl") else { return };
        if b.preview == Preview::Off {
            self.shader_previews.remove(path);
            return;
        }
        let text = b.text(cx);
        let preview = self.shader_previews.entry(path.to_path_buf()).or_insert_with(|| cx.new(|_| shader_view::ShaderPreview::new())).clone();
        preview.update(cx, |p, cx| p.set_source(text, window, cx));
    }

    /// Check a shader once typing pauses, with the text in the editor (saved or not).
    fn lint_soon(&mut self, path: &Path, cx: &mut Context<Self>) {
        let Some(b) = self.buffers().find(|b| b.path == path && b.lang.highlight == "glsl") else { return };
        let (path, text) = (path.to_path_buf(), b.text(cx));
        let task = cx.spawn({
            let path = path.clone();
            async move |this, cx| {
                cx.background_executor().timer(std::time::Duration::from_millis(400)).await;
                let checked = path.clone();
                // No linter (offline, unknown platform): the shader just isn't checked.
                let Ok(diagnostics) = cx.background_spawn(async move { glsl_lint::lint(&checked, &text) }).await else { return };
                let _ = this.update(cx, |this, cx| this.set_lint(path, diagnostics, cx));
            }
        });
        self.lints.insert(path, task);
    }

    /// Show the server's and the linters' diagnostics together.
    fn publish_diagnostics(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let diagnostics: Vec<Diagnostic> = [&self.lsp_diagnostics, &self.lint_diagnostics].iter().filter_map(|m| m.get(&path)).flatten().cloned().collect();
        if let Some(b) = self.buffers().find(|b| b.path == path) {
            b.set_diagnostics(&diagnostics, cx);
        }
        if diagnostics.is_empty() {
            self.diagnostics.remove(&path);
        } else {
            self.diagnostics.insert(path, diagnostics);
        }
        cx.notify();
    }

    /// Files may have changed on disk: refresh the tree and Git, reload unmodified open buffers.
    fn sync_with_disk(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let stale: Vec<PathBuf> = self
            .buffers()
            .filter(|b| !b.dirty && buffer::disk_stamp(&b.path) != b.disk && std::fs::read_to_string(&b.path).is_ok_and(|disk| disk != b.text(cx)))
            .map(|b| b.path.clone())
            .collect();
        for b in self.buffers_mut().filter(|b| !b.dirty) {
            if stale.contains(&b.path) {
                b.reload(window, cx);
            } else {
                // Touched but unchanged: not reread at the next sync.
                b.disk = buffer::disk_stamp(&b.path);
            }
        }
        self.refresh_review_marks(cx);
        self.file_tree.update(cx, |t, cx| t.refresh(cx));
        self.git.update(cx, |g, cx| g.refresh(cx));
    }

    /// A chat turn changed `files`: their editors show it, with Keep / Undo.
    pub fn add_reviews(&mut self, files: Vec<crate::ai::changes::FileChange>, cx: &mut Context<Self>) {
        for f in files {
            // Keyed as editors know files (`/var` is `/private/var` on macOS…).
            let path = review_key(&f.path);
            // Several turns in a row: Undo goes back to before the first one.
            let before = self.reviews.get(&path).map_or(f.before.clone(), |r| r.before.clone());
            self.reviews.insert(path.clone(), crate::ai::changes::FileChange { path, before, ..f });
        }
        self.refresh_review_marks(cx);
        cx.notify();
    }

    fn refresh_review_marks(&self, cx: &mut Context<Self>) {
        for b in self.buffers() {
            b.show_review(self.reviews.get(&b.path).map(|r| &r.before), cx);
        }
    }

    /// Keep what the chat changed in `path`, or (`keep` false) put the file back as it was
    /// before the chat's first change to it, through the same restore as undoing a message.
    pub fn settle_review(&mut self, path: &Path, keep: bool, window: &mut Window, cx: &mut Context<Self>) {
        let path = &review_key(path);
        let Some(review) = self.reviews.remove(path) else { return };
        if !keep {
            crate::ai::changes::restore(&[(path.to_path_buf(), review.before)].into());
            self.sync_with_disk(window, cx);
        }
        self.refresh_review_marks(cx);
        cx.notify();
    }

    /// Keep (or undo) everything the chat changed, in every file.
    pub fn settle_all_reviews(&mut self, keep: bool, window: &mut Window, cx: &mut Context<Self>) {
        let paths: Vec<PathBuf> = self.reviews.keys().cloned().collect();
        for path in paths {
            self.settle_review(&path, keep, window, cx);
        }
    }

    /// The text before the chat, the text now (its editor's when open) and their changed blocks.
    pub fn review_hunks(&self, path: &Path, cx: &App) -> Option<(String, String, Vec<crate::ai::changes::Hunk>)> {
        let path = &review_key(path);
        let review = self.reviews.get(path)?;
        let old = String::from_utf8_lossy(review.before.as_deref().unwrap_or_default()).to_string();
        let new = self.buffers().find(|b| b.path == *path).map(|b| b.text(cx)).or_else(|| std::fs::read_to_string(path).ok()).unwrap_or_default();
        let hunks = crate::ai::changes::hunks(&old, &new);
        Some((old, new, hunks))
    }

    /// The block the cursor is in, else the next one, else the last.
    pub fn hunk_at_cursor(&self, path: &Path, hunks: &[crate::ai::changes::Hunk], cx: &App) -> usize {
        use gpui_kit::component::input::RopeExt as _;
        let path = &review_key(path);
        let line = self.buffers().find(|b| b.path == *path).map_or(0, |b| {
            let e = b.editor.read(cx);
            e.text().offset_to_point(e.cursor()).row
        });
        hunks.iter().position(|h| line < h.new.end.max(h.new.start + 1)).unwrap_or(hunks.len().saturating_sub(1))
    }

    /// Keep or undo the block at the cursor; the file leaves review once no block is left.
    pub fn settle_hunk(&mut self, path: &Path, keep: bool, window: &mut Window, cx: &mut Context<Self>) {
        let path = &review_key(path);
        let Some((old, new, hunks)) = self.review_hunks(path, cx) else { return };
        let Some(hunk) = hunks.get(self.hunk_at_cursor(path, &hunks, cx)).cloned() else {
            return self.settle_review(path, true, window, cx);
        };
        let (old, new) = if keep {
            (crate::ai::changes::keep_hunk(&old, &new, &hunk), new)
        } else {
            let reverted = crate::ai::changes::undo_hunk(&old, &new, &hunk);
            // Saved with the rest of the editor's text, so nothing typed since is lost.
            if let Err(e) = std::fs::write(path, &reverted) {
                return self.notify(format!("{}: {e}", path.display()), window, cx);
            }
            if let Some(b) = self.buffers_mut().find(|b| b.path == *path) {
                b.reload(window, cx);
            }
            (old, reverted)
        };
        if crate::ai::changes::hunks(&old, &new).is_empty() {
            self.reviews.remove(path);
        } else if let Some(review) = self.reviews.get_mut(path) {
            review.before = Some(old.into_bytes());
        }
        self.refresh_review_marks(cx);
        cx.notify();
    }

    /// Move the cursor to the next (or previous) changed block.
    pub fn goto_hunk(&mut self, path: &Path, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_kit::component::input::RopeExt as _;
        let path = &review_key(path);
        let Some((_, _, hunks)) = self.review_hunks(path, cx) else { return };
        let Some(b) = self.buffers().find(|b| b.path == *path) else { return };
        let editor = b.editor.clone();
        editor.update(cx, |e, cx| {
            let line = e.text().offset_to_point(e.cursor()).row;
            let target = if forward {
                hunks.iter().find(|h| h.new.start > line).or(hunks.first())
            } else {
                hunks.iter().rev().find(|h| h.new.start < line).or(hunks.last())
            };
            if let Some(h) = target {
                let at = e.text().line_start_offset(h.new.start.min(e.text().lines_len().saturating_sub(1)));
                e.set_selected_range(at..at, cx);
            }
            e.focus(window, cx);
        });
    }

    // ---- git -------------------------------------------------------------------------

    /// Recompute gutter change bars for one open file, or all of them.
    fn refresh_git_markers(&mut self, only: Option<PathBuf>, cx: &mut Context<Self>) {
        let Some(repo) = self.git.read(cx).repo.clone() else { return };
        let paths: Vec<PathBuf> = self.buffers().map(|b| b.path.clone()).filter(|p| only.as_ref().is_none_or(|o| o == p)).collect();
        cx.spawn(async move |this, cx| {
            let changes = cx
                .background_spawn(async move {
                    paths.into_iter().map(|p| {
                        let c = repo.line_changes(&p).unwrap_or_default();
                        (p, c)
                    }).collect::<Vec<_>>()
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                for (path, c) in changes {
                    if let Some(b) = this.buffers().find(|b| b.path == path) {
                        b.set_git_changes(&c, cx);
                    }
                }
            });
        })
        .detach();
    }

    fn on_git_event(&mut self, _: &Entity<GitPanel>, event: &GitEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.git.read(cx).repo.clone() else { return };
        match event {
            GitEvent::Changed => {
                let files: Vec<(PathBuf, char)> =
                    self.git.read(cx).status.files.iter().map(|f| (repo.root.join(&f.path), f.letter(!f.unstaged()))).collect();
                self.file_tree.update(cx, |t, cx| t.set_git_status(files, cx));
                self.refresh_git_markers(None, cx);
                // New commits or branches (here, in the terminal or by the AI) show up in the history.
                let graphs: Vec<Entity<GitGraph>> = self.panes.iter().flat_map(|p| &p.tabs).filter_map(|t| if let Tab::Graph(g) = t { Some(g.clone()) } else { None }).collect();
                for g in graphs {
                    g.update(cx, |g, cx| g.reload(cx));
                }
            }
            GitEvent::OperationDone => self.sync_with_disk(window, cx),
            GitEvent::OpenFile(path) => self.open_file(path.clone(), None, window, cx),
            GitEvent::OpenDiff { file, staged } => {
                let (file, staged) = (file.clone(), *staged);
                let side = if staged { "index" } else { "working tree" };
                let title = format!("{} ({side})", file.path.rsplit('/').next().unwrap_or(&file.path));
                self.open_patch(title, move || repo.diff_file(&file, staged), window, cx);
            }
            GitEvent::OpenGraph => {
                if let Some((pane, ix)) = self.locate(|t| matches!(t, Tab::Graph(_))) {
                    return self.activate(pane, ix, window, cx);
                }
                let graph = cx.new(|cx| GitGraph::new(repo.clone(), cx));
                let sub = cx.subscribe_in(&graph, window, move |this, _, GraphEvent::OpenCommit { hash, subject }, window, cx| {
                    let (repo, hash) = (repo.clone(), hash.clone());
                    let title = format!("{} {}", &hash[..7.min(hash.len())], subject);
                    this.open_patch(title, move || repo.show(&hash), window, cx);
                });
                self._subs.push(sub);
                self.open_tab(Tab::Graph(graph), window, cx);
            }
            GitEvent::ConfirmDiscard(files) => {
                let (files, git) = (files.clone(), self.git.downgrade());
                dialogs::confirm(t("Discard changes?"), t("This cannot be undone."), t("Discard"), window, cx, move |_, cx| {
                    let _ = git.update(cx, |g, cx| g.discard(files.clone(), cx));
                });
            }
            GitEvent::NewBranch => {
                let git = self.git.downgrade();
                dialogs::prompt_text_with_option(t("New branch"), "", t("Create"), ("Publish it on the remote repository", false), window, cx, move |name, publish, _, cx| {
                    let _ = git.update(cx, |g, cx| g.create_branch(name, publish, cx));
                });
            }
            GitEvent::SetRemote { publish } => {
                let (git, publish) = (self.git.downgrade(), *publish);
                let current = self.git.read(cx).repo.as_ref().and_then(|r| r.remote_url()).unwrap_or_default();
                dialogs::prompt_text(t("Remote repository URL (e.g. https://github.com/me/project.git)"), &current, t("Link"), window, cx, move |url, _, cx| {
                    let _ = git.update(cx, |g, cx| g.set_remote(url, publish, cx));
                });
            }
            GitEvent::RenameBranch(from) => self.rename_branch(from.clone(), window, cx),
            GitEvent::DeleteBranch(name) => self.delete_branch(name.clone(), window, cx),
        }
    }

    fn rename_branch(&mut self, from: String, window: &mut Window, cx: &mut Context<Self>) {
        let git = self.git.downgrade();
        let initial = from.clone();
        dialogs::prompt_text(t("Rename branch"), &initial, t("Rename"), window, cx, move |to, _, cx| {
            if to != from {
                let _ = git.update(cx, |g, cx| g.rename_branch(from.clone(), to, cx));
            }
        });
    }

    fn delete_branch(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let git = self.git.downgrade();
        let title = format!("{} {name}", t("Delete branch"));
        dialogs::confirm(title, t("The branch is deleted here and on the remote repository: commits only on it will be lost."), t("Delete"), window, cx, move |_, cx| {
            let _ = git.update(cx, |g, cx| g.delete_branch(name.clone(), cx));
        });
    }

    /// Open (or refresh) a diff tab for a patch computed in the background.
    fn open_patch(&mut self, title: String, patch: impl FnOnce() -> anyhow::Result<String> + Send + 'static, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            let patch = cx.background_spawn(async move { patch() }).await;
            let _ = this.update_in(cx, |this, window, cx| {
                let patch = patch.unwrap_or_else(|e| format!("error: {e}"));
                let state = diff_state(&patch, DiffMode::Split, cx);
                let title: SharedString = title.into();
                let tab = Tab::Diff { title: title.clone(), patch, mode: DiffMode::Split, state };
                match this.locate(|t| matches!(t, Tab::Diff { title: t, .. } if *t == title)) {
                    Some((pane, ix)) => {
                        if let Some(p) = this.pane_mut(pane) {
                            p.tabs[ix] = tab;
                        }
                        this.activate(pane, ix, window, cx);
                    }
                    None => this.open_tab(tab, window, cx),
                }
            });
        })
        .detach();
    }

    // ---- run / terminals / modules -------------------------------------------------------

    fn command_env(&self, cx: &App) -> Vec<(String, String)> {
        let b = self.active_buffer();
        let selection = b.map(|b| b.editor.read(cx).selected_value().to_string()).unwrap_or_default();
        // Commands and Run see the project's Python virtualenv as activated.
        crate::language::venv_env(&self.root).into_iter().chain([
            ("FORGE_WORKSPACE".into(), self.root.display().to_string()),
            ("FORGE_FILE".into(), b.map(|b| b.path.display().to_string()).unwrap_or_default()),
            ("FORGE_LANGUAGE".into(), b.map(|b| b.lang.lsp_id.clone()).unwrap_or_default()),
            ("FORGE_SELECTION".into(), selection),
        ]).collect()
    }

    fn show_bottom_tab(&mut self, tab: BottomTab, window: &mut Window, cx: &mut Context<Self>) {
        self.show_bottom = true;
        self.bottom_tab = tab;
        if tab == BottomTab::Terminal {
            if self.terminals.is_empty() {
                if let Some(shell) = self.shells.first().cloned() {
                    return self.new_terminal(shell, None, window, cx);
                }
            }
            if let Some(t) = self.terminals.get(self.active_terminal) {
                t.read(cx).focus_handle().clone().focus(window, cx);
            }
        }
        cx.notify();
    }

    fn new_terminal(&mut self, shell: Shell, cwd: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let cwd = cwd.unwrap_or_else(|| self.root.clone());
        let term = cx.new(|cx| Terminal::new(&cwd, shell, cx));
        self.terminals.push(term);
        self.active_terminal = self.terminals.len() - 1;
        self.show_bottom_tab(BottomTab::Terminal, window, cx);
    }

    fn run_file(&mut self, _: &RunFile, window: &mut Window, cx: &mut Context<Self>) {
        let Some(b) = self.active_buffer() else {
            return self.notify(t("Open a file to run"), window, cx);
        };
        let Some(command) = crate::project::run_command(&self.root, &b.path, &b.lang) else {
            return self.notify(t("No run command for this file"), window, cx);
        };
        let command = crate::project::with_lang_settings(command, &Settings::get(cx).lang(&b.lang.lsp_id));
        let title = b.name();
        self.save(&Save, window, cx);
        let (root, env) = (self.root.clone(), self.command_env(cx));
        self.output.update(cx, |o, cx| o.run(&title, &command, &root, env, cx));
        self.show_bottom_tab(BottomTab::Output, window, cx);
    }

    fn run_module_command(&mut self, dir: &Path, command: &plugins::Command, window: &mut Window, cx: &mut Context<Self>) {
        let file = self.active_buffer().map(|b| b.path.clone());
        let line = language::expand(&command.run, file.as_deref(), &self.root);
        let mut env = self.command_env(cx);
        env.extend(plugins::command_env(dir));
        let root = self.root.clone();
        self.output.update(cx, |o, cx| o.run(&command.title, &line, &root, env, cx));
        self.show_bottom_tab(BottomTab::Output, window, cx);
    }

    fn run_ide_action(&mut self, action: IdeAction, window: &mut Window, cx: &mut Context<Self>) {
        match action {
            IdeAction::Notify { message } => self.notify(message, window, cx),
            IdeAction::Open { path, line } => {
                let path = if Path::new(&path).is_absolute() { PathBuf::from(path) } else { self.root.join(path) };
                self.open_file(path, line, window, cx);
            }
            IdeAction::ReplaceBuffer { text } => {
                if let Some(b) = self.active_buffer() {
                    b.editor.update(cx, |e, cx| e.set_value(text, window, cx));
                }
            }
            IdeAction::Insert { text } => {
                if let Some(b) = self.active_buffer() {
                    b.editor.update(cx, |e, cx| e.insert(text, window, cx));
                }
            }
            IdeAction::Reload => {
                if let Some(b) = self.active_buffer_mut() {
                    b.reload(window, cx);
                }
            }
            IdeAction::Chat { message } => {
                self.show_right = true;
                self.chat.update(cx, |c, cx| c.send(message, cx));
            }
            IdeAction::Launch => {}
        }
        cx.notify();
    }

    fn new_module(&mut self, _: &NewModule, window: &mut Window, cx: &mut Context<Self>) {
        let name = (1..).map(|i| format!("my-module-{i}")).find(|n| !plugins::plugins_dir().join(n).exists()).unwrap();
        match plugins::scaffold(&name) {
            Ok(dir) => {
                self.reload_modules(&ReloadModules, window, cx);
                self.open_file(dir.join("plugin.toml"), None, window, cx);
            }
            Err(e) => self.notify(e.to_string(), window, cx),
        }
    }

    fn reload_modules(&mut self, _: &ReloadModules, window: &mut Window, cx: &mut Context<Self>) {
        crate::load_modules(cx);
        let p = Plugins::global(cx);
        let msg = format!("{} : {} ✓  {} ✕", t("Modules"), p.plugins.len(), p.errors.len());
        self.notify(msg, window, cx);
        cx.notify();
    }

    // ---- debugger ----------------------------------------------------------------------

    /// F5: continue a paused session, else pick how to start the project (launch.json, Conan,
    /// CMake presets, the build system, the current file), like VS Code's picker.
    fn start_debug(&mut self, _: &StartDebug, window: &mut Window, cx: &mut Context<Self>) {
        if self.debug.read(cx).is_active() {
            return self.debug.update(cx, |d, cx| d.continue_(cx));
        }
        let file = self.active_buffer().map(|b| (b.path.clone(), b.lang.lsp_id.clone()));
        let modules: Vec<crate::launch::ModuleLaunch> = Plugins::global(cx)
            .plugins
            .iter()
            .filter_map(|p| Some(crate::launch::ModuleLaunch { name: p.manifest.name.clone(), dir: p.dir.clone(), run: p.manifest.launch.clone()? }))
            .collect();
        let root = self.root.clone();
        cx.spawn_in(window, async move |this, cx| {
            let targets = cx.background_spawn(async move { crate::launch::targets(&root, file.as_ref().map(|(p, l)| (p.as_path(), l.as_str())), &modules) }).await;
            let _ = this.update_in(cx, |w, window, cx| w.pick_launch(targets, window, cx));
        })
        .detach();
    }

    fn pick_launch(&mut self, mut targets: Vec<crate::launch::Target>, window: &mut Window, cx: &mut Context<Self>) {
        if targets.is_empty() {
            return self.notify(t("Open the program's source file to debug it"), window, cx);
        }
        if let Some(ix) = self.last_launch.as_ref().and_then(|l| targets.iter().position(|t| &t.label == l)) {
            let last = targets.remove(ix);
            targets.insert(0, last);
        }
        if targets.len() == 1 {
            return self.launch_target(targets.remove(0), window, cx);
        }
        let items = targets
            .iter()
            .map(|t| quick_open::PickerItem { label: t.label.clone(), detail: t.detail.clone(), keys: String::new(), icon: gpui_kit::assets::IconName::Bug, tint: None })
            .collect();
        let ws = cx.entity().downgrade();
        quick_open::Picker::open(items, t("Select what to debug").into(), window, cx, move |ix, window, cx| {
            let (ws, target) = (ws.clone(), targets[ix].clone());
            window.defer(cx, move |window, cx| {
                let _ = ws.update(cx, |w, cx| w.launch_target(target, window, cx));
            });
        });
    }

    fn launch_target(&mut self, target: crate::launch::Target, window: &mut Window, cx: &mut Context<Self>) {
        self.save_all(cx);
        let lsp_id = if target.plan.adapter == crate::debug::adapters::Adapter::Lldb { "cpp".to_string() } else { self.active_buffer().map(|b| b.lang.lsp_id.clone()).unwrap_or_default() };
        let settings = Settings::get(cx).lang(&lsp_id);
        let (root, bps) = (self.root.clone(), self.breakpoints.clone());
        self.last_launch = Some(target.label.clone());
        self.debug.update(cx, |d, cx| d.start_plan(target.plan, root, bps, settings, cx));
        self.show_bottom_tab(BottomTab::Debug, window, cx);
    }

    fn toggle_breakpoint_at(&mut self, path: PathBuf, line: u32, cx: &mut Context<Self>) {
        let lines = self.breakpoints.entry(path.clone()).or_default();
        if !lines.remove(&line) {
            lines.insert(line);
        }
        let lines = lines.clone();
        if lines.is_empty() {
            self.breakpoints.remove(&path);
        }
        if let Some(b) = self.buffers().find(|b| b.path == path) {
            b.show_breakpoints(Some(&lines), cx);
        }
        self.debug.update(cx, |d, cx| d.update_breakpoints(path, lines, cx));
        cx.notify();
    }

    fn toggle_breakpoint(&mut self, _: &ToggleBreakpoint, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(b) = self.active_buffer() {
            let (path, line) = (b.path.clone(), b.cursor_line(cx));
            self.toggle_breakpoint_at(path, line, cx);
        }
    }

    fn set_debug_line(&mut self, at: Option<(PathBuf, u32)>, cx: &mut Context<Self>) {
        for b in self.buffers() {
            let line = at.as_ref().filter(|(p, _)| *p == b.path).map(|(_, l)| *l);
            b.show_debug_line(line, cx);
        }
    }
}

/// The selected lines of a buffer as chat context; with `current_line`, an empty selection
/// gives the line under the cursor.
fn selection_snippet(b: &Buffer, current_line: bool, cx: &App) -> Option<crate::ai::chat_panel::ChatContext> {
    use gpui_kit::component::input::RopeExt as _;
    let e = b.editor.read(cx);
    let range = e.selected_range();
    if range.is_empty() && !current_line {
        return None;
    }
    let text = e.text();
    let (start, mut end) = (text.offset_to_point(range.start).row, text.offset_to_point(range.end).row);
    if range.end > range.start && text.offset_to_point(range.end).column == 0 && end > start {
        end -= 1; // a selection ending at the start of a line does not include it
    }
    let body = if range.is_empty() {
        text.slice(text.line_start_offset(start)..text.line_end_offset(start)).to_string()
    } else {
        e.selected_value().to_string()
    };
    Some(crate::ai::chat_panel::ChatContext::Snippet { path: b.path.clone(), start: start as u32 + 1, end: end as u32 + 1, text: body })
}

/// A file as open editors name it (resolved symlinks), so reviews find their editor.
fn review_key(path: &Path) -> PathBuf {
    crate::util::canonical(path).unwrap_or_else(|_| path.to_path_buf())
}

fn diff_state(patch: &str, mode: DiffMode, cx: &mut App) -> Entity<DiffState> {
    let files: Vec<_> = DiffFile::parse(patch)
        .unwrap_or_default()
        .into_iter()
        .map(|f| {
            let lang = language::detect(Path::new(f.path().as_ref()), Plugins::global(cx)).highlight;
            f.with_language(lang)
        })
        .collect();
    cx.new(|cx| DiffState::new(files, cx).with_mode(mode))
}

#[cfg(test)]
mod tests {
    use super::severity_counts;
    use lsp_types::{Diagnostic, DiagnosticSeverity};

    #[test]
    fn counts_errors_and_warnings() {
        let d = |s| Diagnostic { severity: s, ..Default::default() };
        let all = [d(Some(DiagnosticSeverity::ERROR)), d(None), d(Some(DiagnosticSeverity::WARNING)), d(Some(DiagnosticSeverity::HINT))];
        assert_eq!(severity_counts(all.iter()), (2, 1));
    }

    /// `KeyBinding::new` panics on a keystroke it can't parse, which would crash at startup.
    #[test]
    fn key_bindings_parse() {
        assert!(!super::key_bindings().is_empty());
    }
}

#[cfg(test)]
mod ui_tests {
    use super::Workspace;
    use gpui_kit::{AppContext as _, Entity, Modifiers, MouseButton, TestAppContext, point, px};

    fn setup(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut gpui_kit::VisualTestContext) {
        let dir = std::env::temp_dir().join(format!("forgecode-ui-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["a.txt", "b.txt"] {
            std::fs::write(dir.join(f), "hello\n").unwrap();
        }
        setup_at(cx, dir)
    }

    fn setup_at(cx: &mut TestAppContext, dir: std::path::PathBuf) -> (Entity<Workspace>, &mut gpui_kit::VisualTestContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            let base = crate::settings::Settings { autosave_ms: 0, ..Default::default() };
            cx.set_global(base.clone());
            cx.set_global(crate::settings::Layers { base, ..Default::default() });
            cx.set_global(crate::ai::copilot::CopilotState { sign_in: crate::ai::copilot::SignIn::Idle, accounts: vec![], models: vec![] });
            cx.set_global(crate::plugins::Plugins::default());
            cx.bind_keys(super::key_bindings());
        });
        let slot = std::rc::Rc::new(std::cell::RefCell::new(None));
        let s = slot.clone();
        let (_, vcx) = cx.add_window_view(move |window, cx| {
            let ws = cx.new(|cx| Workspace::new(dir.clone(), window, cx));
            *s.borrow_mut() = Some(ws.clone());
            gpui_kit::base::Root::new(ws, window, cx)
        });
        let ws = slot.borrow().clone().unwrap();
        (ws, vcx)
    }

    #[gpui_kit::test]
    fn dragging_a_tab_to_the_right_edge_splits_the_editor(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        let root = ws.read_with(cx, |w, _| w.root.clone());
        cx.update(|window, cx| {
            ws.update(cx, |w, cx| {
                w.open_file(root.join("a.txt"), None, window, cx);
                w.open_file(root.join("b.txt"), None, window, cx);
            })
        });
        cx.run_until_parked();
        let tab = cx.debug_bounds("tab-0-1").expect("tab is rendered");
        let start = tab.center();
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(start + point(gpui_kit::px(20.), gpui_kit::px(40.)), MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        let zone = cx.debug_bounds("drop-0-Right").expect("drop zones appear while dragging");
        cx.simulate_mouse_move(zone.center(), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(zone.center(), MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        ws.read_with(cx, |w, _| {
            assert_eq!(w.panes.len(), 2, "a second editor group was created");
            assert_eq!(w.panes[1].tabs.len(), 1);
        });
    }

    #[gpui_kit::test]
    fn one_click_in_the_explorer_opens_the_file(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        cx.run_until_parked();
        let file = cx.debug_bounds("tree-b.txt").expect("explorer row is rendered");
        // A trackpad click: the finger moves a few pixels between press and release.
        cx.simulate_mouse_down(file.center(), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(file.center() + point(gpui_kit::px(4.), gpui_kit::px(1.)), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(file.center() + point(gpui_kit::px(4.), gpui_kit::px(1.)), MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        let tabs = |cx: &mut gpui_kit::VisualTestContext| ws.read_with(cx, |w, cx| w.panes[0].tabs.iter().map(|t| (t.label(cx), w.panes[0].is_transient(t, cx))).collect::<Vec<_>>());
        assert_eq!(tabs(cx), [("b.txt".into(), true)], "one click: a temporary tab");
        // Another file with one click takes its place, like VS Code's preview.
        let other = cx.debug_bounds("tree-a.txt").expect("row").center();
        cx.simulate_click(other, Modifiers::none());
        cx.run_until_parked();
        assert_eq!(tabs(cx), [("a.txt".into(), true)], "the temporary tab is replaced");
        // A double click keeps it: the next one-click file opens beside it.
        cx.simulate_event(gpui_kit::MouseDownEvent { position: other, button: MouseButton::Left, modifiers: Modifiers::none(), click_count: 2, first_mouse: false });
        cx.simulate_event(gpui_kit::MouseUpEvent { position: other, button: MouseButton::Left, modifiers: Modifiers::none(), click_count: 2 });
        cx.run_until_parked();
        assert_eq!(tabs(cx), [("a.txt".into(), false)], "a double click keeps it");
        // What a single click does (simulated clicks are timing-sensitive when the machine is busy).
        let root = ws.read_with(cx, |w, _| w.root.clone());
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_file_preview(root.join("b.txt"), window, cx)));
        cx.run_until_parked();
        assert_eq!(tabs(cx), [("a.txt".into(), false), ("b.txt".into(), true)]);
        // Editing keeps a temporary tab too.
        cx.simulate_input("x");
        cx.run_until_parked();
        assert_eq!(tabs(cx), [("a.txt".into(), false), ("b.txt".into(), false)], "edited: kept");

        // Every kind of file: an image, then a PDF, take the temporary tab in turn.
        let root = ws.read_with(cx, |w, _| w.root.clone());
        std::fs::write(root.join("c.png"), b"\x89PNG\r\n\x1a\n").unwrap();
        std::fs::write(root.join("d.pdf"), b"%PDF-1.4\n").unwrap();
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_file_preview(root.join("c.png"), window, cx)));
        cx.run_until_parked();
        assert_eq!(tabs(cx)[2], ("c.png".into(), true), "an image opens as a temporary tab");
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_file_preview(root.join("d.pdf"), window, cx)));
        cx.run_until_parked();
        assert_eq!(tabs(cx).len(), 3);
        assert_eq!(tabs(cx)[2], ("d.pdf".into(), true), "the PDF takes the image's place");
        // Opened for good (as a double click does): it stays.
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_file(root.join("d.pdf"), None, window, cx)));
        assert_eq!(tabs(cx)[2], ("d.pdf".into(), false));
    }

    /// A small image is enlarged pixel by pixel: no blended color where two pixels meet.
    #[test]
    fn small_images_keep_sharp_pixels() {
        let dir = std::env::temp_dir().join(format!("forge-crisp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pixels.png");
        let mut tiny = image::RgbaImage::new(2, 1);
        tiny.put_pixel(0, 0, image::Rgba([255, 0, 0, 255]));
        tiny.put_pixel(1, 0, image::Rgba([0, 0, 255, 255]));
        tiny.save(&path).unwrap();
        let crisp = super::crisp_image(&path).expect("a small image is enlarged");
        let big = image::load_from_memory(crisp.bytes()).unwrap().to_rgba8();
        assert_eq!(big.dimensions(), (2048, 1024));
        assert_eq!(big.get_pixel(1023, 500).0, [255, 0, 0, 255], "last red column");
        assert_eq!(big.get_pixel(1024, 500).0, [0, 0, 255, 255], "first blue column, nothing in between");
        // A big image is left to GPUI as it is.
        image::RgbaImage::new(1000, 10).save(&path).unwrap();
        assert!(super::crisp_image(&path).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The tab strip's "…": close the saved tabs, or save everything and close.
    #[gpui_kit::test]
    fn tab_menu_closes_saved_tabs_or_saves_and_closes_all(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        let root = ws.read_with(cx, |w, _| w.root.clone());
        cx.update(|_, cx| crate::settings::Settings::update(cx, |s| s.autosave_ms = 0));
        for f in ["a.txt", "b.txt"] {
            cx.update(|window, cx| ws.update(cx, |w, cx| w.open_file(root.join(f), None, window, cx)));
        }
        cx.run_until_parked();
        cx.simulate_input("edit ");
        cx.run_until_parked();
        let labels = |cx: &mut gpui_kit::VisualTestContext| ws.read_with(cx, |w, cx| w.panes[0].tabs.iter().map(|t| t.label(cx)).collect::<Vec<String>>());
        cx.update(|window, cx| ws.update(cx, |w, cx| w.close_tabs(0, super::CloseTabs::Saved, window, cx)));
        cx.run_until_parked();
        assert_eq!(labels(cx), ["b.txt"], "the unsaved one stays");
        cx.update(|window, cx| ws.update(cx, |w, cx| w.close_tabs(0, super::CloseTabs::SaveAndCloseAll, window, cx)));
        cx.run_until_parked();
        assert!(ws.read_with(cx, |w, _| w.panes[0].tabs.is_empty()));
        assert!(std::fs::read_to_string(root.join("b.txt")).unwrap().starts_with("edit "), "saved before closing");
    }

    #[gpui_kit::test]
    fn dragging_a_file_from_the_explorer_opens_it_in_a_split(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        let root = ws.read_with(cx, |w, _| w.root.clone());
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_file(root.join("a.txt"), None, window, cx)));
        cx.run_until_parked();
        let file = cx.debug_bounds("tree-b.txt").expect("explorer row is rendered");
        let start = file.center();
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(start + point(gpui_kit::px(30.), gpui_kit::px(5.)), MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        let zone = cx.debug_bounds("drop-0-Right").expect("drop zones appear while dragging a file");
        cx.simulate_mouse_move(zone.center(), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(zone.center(), MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        ws.read_with(cx, |w, _| assert_eq!(w.panes.len(), 2));
    }

    #[gpui_kit::test]
    fn dropping_a_file_from_finder_opens_it_in_a_split(cx: &mut TestAppContext) {
        use gpui_kit::{ExternalPaths, FileDropEvent};
        let (ws, cx) = setup(cx);
        let root = ws.read_with(cx, |w, _| w.root.clone());
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_file(root.join("a.txt"), None, window, cx)));
        cx.run_until_parked();
        let editor = cx.debug_bounds("tab-0-0").unwrap();
        let paths = ExternalPaths(vec![root.join("b.txt")].into());
        let over = editor.center() + point(gpui_kit::px(0.), gpui_kit::px(200.));
        cx.simulate_event(FileDropEvent::Entered { position: over, paths });
        cx.simulate_event(FileDropEvent::Pending { position: over + point(gpui_kit::px(5.), gpui_kit::px(0.)) });
        cx.run_until_parked();
        let zone = cx.debug_bounds("drop-0-Right").expect("drop zones appear for Finder drags");
        cx.simulate_event(FileDropEvent::Pending { position: zone.center() });
        cx.simulate_event(FileDropEvent::Submit { position: zone.center() });
        cx.run_until_parked();
        ws.read_with(cx, |w, _| assert_eq!(w.panes.len(), 2));
    }

    #[gpui_kit::test]
    fn typing_opens_completions_and_enter_accepts(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        let root = ws.read_with(cx, |w, _| w.root.clone());
        std::fs::write(root.join("c.txt"), "hello_world\n").unwrap();
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_file(root.join("c.txt"), Some(2), window, cx)));
        cx.run_until_parked();
        cx.simulate_input("hel");
        cx.run_until_parked();
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        let text = ws.read_with(cx, |w, cx| w.active_buffer().unwrap().text(cx));
        assert_eq!(text, "hello_world\nhello_world", "Enter accepts the completion without adding a newline");
        cx.simulate_input("\nhel");
        cx.run_until_parked();
        cx.simulate_keystrokes("tab");
        cx.run_until_parked();
        let text = ws.read_with(cx, |w, cx| w.active_buffer().unwrap().text(cx));
        assert_eq!(text, "hello_world\nhello_world\nhello_world", "Tab accepts too");
    }

    /// A completion accepted inside a word replaces the whole word.
    #[gpui_kit::test]
    fn completion_replaces_the_rest_of_the_word(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        let root = ws.read_with(cx, |w, _| w.root.clone());
        std::fs::write(root.join("w.txt"), "hello_world\nhelxyz;\n").unwrap();
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_file(root.join("w.txt"), Some(2), window, cx)));
        cx.run_until_parked();
        let editor = ws.read_with(cx, |w, _| w.active_buffer().unwrap().editor.clone());
        cx.update(|_, cx| editor.update(cx, |e, cx| e.set_selected_range(14..14, cx)));
        cx.simulate_input("l");
        cx.run_until_parked();
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(ws.read_with(cx, |w, cx| w.active_buffer().unwrap().text(cx)), "hello_world\nhello_world;\n");
    }

    /// VS Code editing: a quote over a selection surrounds it; Cmd/Ctrl+/ comments the line.
    #[gpui_kit::test]
    fn quotes_surround_the_selection_and_lines_toggle_comments(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        let root = ws.read_with(cx, |w, _| w.root.clone());
        std::fs::write(root.join("s.rs"), "let a = hello;\n").unwrap();
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_file(root.join("s.rs"), Some(1), window, cx)));
        cx.run_until_parked();
        let editor = ws.read_with(cx, |w, _| w.active_buffer().unwrap().editor.clone());
        cx.update(|_, cx| editor.update(cx, |e, cx| e.set_selected_range(8..13, cx)));
        cx.simulate_keystrokes("\"");
        cx.run_until_parked();
        let text = || ws.read_with(cx, |w, cx| w.active_buffer().unwrap().text(cx));
        assert_eq!(text(), "let a = \"hello\";\n");
        cx.simulate_keystrokes(if cfg!(target_os = "macos") { "cmd-/" } else { "ctrl-/" });
        cx.run_until_parked();
        assert_eq!(ws.read_with(cx, |w, cx| w.active_buffer().unwrap().text(cx)), "// let a = \"hello\";\n");
    }

    /// ⌘→ takes the suggestion's next word and keeps the rest on screen, like VS Code.
    #[gpui_kit::test]
    fn cmd_right_takes_a_word_and_keeps_the_rest(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        let root = ws.read_with(cx, |w, _| w.root.clone());
        std::fs::write(root.join("g.txt"), "x = ").unwrap();
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_file(root.join("g.txt"), Some(1), window, cx)));
        cx.run_until_parked();
        let (editor, intel) = ws.read_with(cx, |w, _| { let b = w.active_buffer().unwrap(); (b.editor.clone(), b.intel.clone()) });
        intel.set_suggestion("x = ", "alpha beta gamma;");
        cx.update(|_, cx| editor.update(cx, |e, cx| { e.set_selected_range(4..4, cx); e.show_inline_suggestion("alpha beta gamma;".into(), cx); }));
        cx.simulate_keystrokes(if cfg!(target_os = "macos") { "cmd-right" } else { "ctrl-right" });
        cx.run_until_parked();
        assert_eq!(cx.update(|_, cx| editor.read(cx).text().to_string()), "x = alpha");
        assert_eq!(cx.update(|_, cx| editor.read(cx).inline_suggestion().map(String::from)), Some(" beta gamma;".into()));
        cx.simulate_keystrokes(if cfg!(target_os = "macos") { "cmd-right" } else { "ctrl-right" });
        cx.run_until_parked();
        assert_eq!(cx.update(|_, cx| editor.read(cx).text().to_string()), "x = alpha beta");
        assert_eq!(cx.update(|_, cx| editor.read(cx).inline_suggestion().map(String::from)), Some(" gamma;".into()));
    }

    /// ⌘→ without a Copilot suggestion goes to the end of the line.
    #[gpui_kit::test]
    fn cmd_right_goes_to_the_end_of_the_line(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        let root = ws.read_with(cx, |w, _| w.root.clone());
        std::fs::write(root.join("e.txt"), "hello world\nsecond\n").unwrap();
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_file(root.join("e.txt"), Some(1), window, cx)));
        cx.run_until_parked();
        let editor = ws.read_with(cx, |w, _| w.active_buffer().unwrap().editor.clone());
        cx.update(|_, cx| editor.update(cx, |e, cx| e.set_selected_range(0..0, cx)));
        cx.simulate_keystrokes(if cfg!(target_os = "macos") { "cmd-right" } else { "ctrl-right" });
        cx.run_until_parked();
        assert_eq!(cx.update(|_, cx| editor.read(cx).selected_range()), 11..11);
    }

    /// ⌘← / ⌘→ stop at the line's text first (after its indent, before trailing blanks), then at its edge.
    #[gpui_kit::test]
    fn cmd_arrows_stop_at_the_text_then_the_line_edge(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        let root = ws.read_with(cx, |w, _| w.root.clone());
        std::fs::write(root.join("h.txt"), "x
\t  hello  \n").unwrap();
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_file(root.join("h.txt"), Some(1), window, cx)));
        cx.run_until_parked();
        let editor = ws.read_with(cx, |w, _| w.active_buffer().unwrap().editor.clone());
        cx.update(|_, cx| editor.update(cx, |e, cx| e.set_selected_range(7..7, cx)));
        let mut press = |key: &str| {
            let key = if cfg!(target_os = "macos") { format!("cmd-{key}") } else { key.to_string() };
            cx.simulate_keystrokes(&key);
            cx.run_until_parked();
            cx.update(|_, cx| editor.read(cx).selected_range().start)
        };
        let (left, right) = if cfg!(target_os = "macos") { ("left", "right") } else { ("home", "end") };
        assert_eq!([press(left), press(left), press(left)], [5, 2, 5]);
        assert_eq!([press(right), press(right)], [10, 12]);
    }

    #[test]
    fn parses_lsp_snippets() {
        use gpui_kit::component::input::snippet::parse;
        let p = parse("box-sizing: $0;");
        assert_eq!((p.text.as_str(), p.stops.clone()), ("box-sizing: ;", vec![12..12]));
        let p = parse("fn ${1:name}(${2:arg}) {$0}");
        assert_eq!((p.text.as_str(), p.stops.clone()), ("fn name(arg) {}", vec![3..7, 8..11, 14..14]));
        let p = parse("print($1)");
        assert_eq!((p.text.as_str(), p.stops.clone()), ("print()", vec![6..6, 7..7]));
        let p = parse("${1:foo(${2:x})} ${3|a,b|} ${TM_FILENAME:file} \\$5 $UNKNOWN");
        assert_eq!(p.text, "foo(x) a file $5 ");
        assert_eq!(&p.stops[..3], &[0..6, 4..5, 7..8]);
        assert_eq!(parse("a $ b").text, "a $ b");
        assert_eq!(parse("cost: 5$").text, "cost: 5$");
    }

    /// A snippet completion (`fn ${1:name}(${2:arg}) {$0}`) goes in as plain text with its first
    /// placeholder selected; Tab and Shift+Tab move between the stops, following what was typed.
    /// clangd's `${1:int a}` shows only `a`; leaving the snippet (Escape) removes the placeholders
    /// not typed over, as hints would, and keeps what was typed.
    #[gpui_kit::test]
    fn untyped_placeholders_go_when_the_snippet_ends(cx: &mut TestAppContext) {
        use gpui_kit::component::input::snippet;
        let (ws, cx) = setup(cx);
        let root = ws.read_with(cx, |w, _| w.root.clone());
        std::fs::write(root.join("p.txt"), "").unwrap();
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_file(root.join("p.txt"), Some(1), window, cx)));
        cx.run_until_parked();
        let editor = ws.read_with(cx, |w, _| w.active_buffer().unwrap().editor.clone());
        let insert = |cx: &mut gpui_kit::VisualTestContext| cx.update(|window, cx| {
            editor.update(cx, |e, cx| {
                let parsed = snippet::parse("f(${1:int a}, ${2:const T &b}, ${3:c})");
                e.set_selected_range(0..e.text().len(), cx);
                e.replace(parsed.text.clone(), window, cx);
                snippet::begin(e, 0, &parsed, cx);
                e.focus(window, cx);
            })
        });
        let text = |cx: &mut gpui_kit::VisualTestContext| cx.update(|_, cx| editor.read(cx).text().to_string());
        insert(cx);
        assert_eq!(text(cx), "f(a, b, c)");
        cx.simulate_keystrokes("escape");
        assert_eq!(text(cx), "f(, , )", "nothing typed: every hint goes");
        insert(cx);
        cx.simulate_input("x");
        cx.simulate_keystrokes("tab");
        cx.simulate_input("y");
        cx.simulate_keystrokes("escape");
        assert_eq!(text(cx), "f(x, y, )");
    }

    #[gpui_kit::test]
    fn snippet_tab_stops_follow_tab(cx: &mut TestAppContext) {
        use gpui_kit::component::input::snippet;
        let (ws, cx) = setup(cx);
        let root = ws.read_with(cx, |w, _| w.root.clone());
        std::fs::write(root.join("s.txt"), "").unwrap();
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_file(root.join("s.txt"), Some(1), window, cx)));
        cx.run_until_parked();
        let editor = ws.read_with(cx, |w, _| w.active_buffer().unwrap().editor.clone());
        cx.update(|window, cx| {
            editor.update(cx, |e, cx| {
                let parsed = snippet::parse("fn ${1:name}(${2:arg}) {$0}");
                e.set_selected_range(0..0, cx);
                e.replace(parsed.text.clone(), window, cx);
                snippet::begin(e, 0, &parsed, cx);
                e.focus(window, cx);
            })
        });
        cx.run_until_parked();
        let state = |cx: &mut gpui_kit::VisualTestContext| cx.update(|_, cx| (editor.read(cx).text().to_string(), editor.read(cx).selected_range()));
        assert_eq!(state(cx), ("fn name(arg) {}".into(), 3..7), "the first placeholder is selected");
        cx.simulate_input("go");
        cx.simulate_keystrokes("tab");
        assert_eq!(state(cx), ("fn go(arg) {}".into(), 6..9), "Tab: the next placeholder, moved by what was typed");
        cx.simulate_keystrokes("shift-tab");
        assert_eq!(state(cx).1, 3..5, "Shift+Tab: back to the previous one");
        cx.simulate_keystrokes("tab");
        cx.simulate_keystrokes("tab");
        assert_eq!(state(cx), ("fn go(arg) {}".into(), 12..12), "the final stop ($0)");
        // The snippet is over: Tab indents again.
        cx.simulate_keystrokes("tab");
        assert_ne!(state(cx).0, "fn go(arg) {}");
    }

    /// The theme button switches what is shown, even in "system" mode with a light system.
    #[gpui_kit::test]
    fn the_theme_button_switches_what_is_shown(cx: &mut TestAppContext) {
        let (_ws, cx) = setup(cx);
        let dark = |cx: &mut gpui_kit::VisualTestContext| cx.update(|_, cx| gpui_kit::component::ActiveTheme::theme(cx).is_dark());
        cx.update(|_, cx| crate::settings::Settings::update(cx, |s| s.theme_mode = "light".into()));
        // As in "system" mode when the system is light: the setting doesn't say "light".
        cx.update(|_, cx| crate::settings::Settings::update(cx, |s| s.theme_mode = "system".into()));
        let before = dark(cx);
        cx.update(|window, cx| window.dispatch_action(Box::new(super::ToggleTheme), cx));
        cx.run_until_parked();
        assert_ne!(dark(cx), before, "one click switches the theme");
        cx.update(|window, cx| window.dispatch_action(Box::new(super::ToggleTheme), cx));
        cx.run_until_parked();
        assert_eq!(dark(cx), before, "and back");
    }

    /// On a fresh start nothing else has the focus: the workspace takes it, so the menu bar's
    /// commands (File, Edit…) are enabled.
    #[gpui_kit::test]
    fn the_workspace_has_the_focus_at_start(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let focused = cx.update(|window, cx| window.focused(cx));
        assert!(focused.is_some());
        let _ = ws;
    }

    /// Full-screen programs (Claude Code…): Option/Shift+Enter, Ctrl+V and the wheel reach them.
    #[gpui_kit::test]
    fn tui_programs_get_keys_and_the_wheel(cx: &mut TestAppContext) {
        use gpui_kit::{Modifiers, ScrollDelta, ScrollEvent, point, px};
        cx.update(|cx| {
            gpui_kit::init(cx);
            let base = crate::settings::Settings::default();
            cx.set_global(base.clone());
            cx.set_global(crate::settings::Layers { base, ..Default::default() });
        });
        let slot = std::rc::Rc::new(std::cell::RefCell::new(None));
        let s = slot.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let shell = super::terminal::Shell { name: "test".into(), path: "/bin/sh".into() };
            let term = cx.new(|cx| super::Terminal::new(std::path::Path::new(""), shell, cx));
            *s.borrow_mut() = Some(term.clone());
            gpui_kit::base::Root::new(term, window, cx)
        });
        let term = slot.borrow().clone().unwrap();
        cx.run_until_parked();
        cx.update(|window, cx| term.read(cx).focus_handle().clone().focus(window, cx));
        let sent = |cx: &mut gpui_kit::VisualTestContext, keys: &str| {
            term.update(cx, |t, _| t.sent.clear());
            cx.simulate_keystrokes(keys);
            term.update(cx, |t, _| std::mem::take(&mut t.sent))
        };
        assert_eq!(sent(cx, "alt-enter"), b"\x1b\r");
        assert_eq!(sent(cx, "shift-enter"), b"\x1b\r");
        assert_eq!(sent(cx, "ctrl-v"), b"\x16");
        // The program asked for the mouse (SGR): a wheel notch up is reported, not scrolled here.
        term.update(cx, |t, _| { t.feed(b"\x1b[?1049h\x1b[?1000h\x1b[?1006h"); t.sent.clear(); });
        let (origin, line, _) = term.read_with(cx, |t, _| t.geometry());
        let at = point(origin.x + px(20.), origin.y + px(20.));
        let wheel = |cx: &mut gpui_kit::VisualTestContext| cx.simulate_event(ScrollEvent { position: at, delta: ScrollDelta::Pixels(point(px(0.), line)), modifiers: Modifiers::default(), ..Default::default() });
        wheel(cx);
        assert!(term.update(cx, |t, _| std::mem::take(&mut t.sent)).starts_with(b"\x1b[<64;"));
        // Alternate screen without the mouse: arrow keys.
        term.update(cx, |t, _| t.feed(b"\x1b[?1000l"));
        wheel(cx);
        assert_eq!(term.update(cx, |t, _| std::mem::take(&mut t.sent)), b"\x1b[A");
        assert_eq!(super::terminal::paste_bytes("a\nb", true), b"\x1b[200~a\nb\x1b[201~");
        // Clicks and drags go to the program too (SGR: press, move, release).
        term.update(cx, |t, _| { t.feed(b"\x1b[?1002h"); t.sent.clear(); });
        let (_, _, ch) = term.read_with(cx, |t, _| t.geometry());
        let cell = |row: f32, col: f32| point(origin.x + px(4.) + ch * (col + 0.5), origin.y + px(4.) + line * (row + 0.5));
        cx.simulate_mouse_down(cell(1., 2.), gpui_kit::MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(cell(1., 5.), gpui_kit::MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_up(cell(1., 5.), gpui_kit::MouseButton::Left, Modifiers::default());
        assert_eq!(String::from_utf8(term.update(cx, |t, _| std::mem::take(&mut t.sent))).unwrap(), "\x1b[<0;3;2M\x1b[<32;6;2M\x1b[<0;6;2m");
        // Queries get answers: the cursor position (1-based).
        term.update(cx, |t, _| t.feed(b"\x1b[3;4H"));
        let reply = term.update(cx, |t, _| t.replies_to(b"\x1b[6n"));
        assert_eq!(reply, b"\x1b[3;4R");
        assert_eq!(super::terminal::special_key("left", false, false, true).unwrap(), b"\x1b[1;5D");
        assert_eq!(super::terminal::special_key("f5", false, false, false).unwrap(), b"\x1b[15~");
    }

    /// Dragging the mouse down the terminal selects several lines.
    #[gpui_kit::test]
    fn dragging_in_the_terminal_selects_lines(cx: &mut TestAppContext) {
        use gpui_kit::{Modifiers, MouseButton, point, px};
        cx.update(|cx| {
            gpui_kit::init(cx);
            let base = crate::settings::Settings::default();
            cx.set_global(base.clone());
            cx.set_global(crate::settings::Layers { base, ..Default::default() });
        });
        let slot = std::rc::Rc::new(std::cell::RefCell::new(None));
        let s = slot.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let shell = super::terminal::Shell { name: "test".into(), path: "/bin/sh".into() };
            let term = cx.new(|cx| {
                let mut t = super::Terminal::new(std::path::Path::new(""), shell, cx);
                t.feed(b"first line\r\nsecond line\r\nthird line");
                t
            });
            *s.borrow_mut() = Some(term.clone());
            gpui_kit::base::Root::new(term, window, cx)
        });
        let term = slot.borrow().clone().unwrap();
        cx.run_until_parked();
        let (origin, line, ch) = term.read_with(cx, |t, _| t.geometry());
        let at = |row: f32, col: f32| point(origin.x + px(4.) + ch * (col + 0.5), origin.y + px(4.) + line * (row + 0.5));
        cx.simulate_mouse_down(at(0., 6.), MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(at(1., 3.), MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(at(2., 4.), MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_up(at(2., 4.), MouseButton::Left, Modifiers::default());
        cx.run_until_parked();
        assert_eq!(term.read_with(cx, |t, _| t.selected()).as_deref(), Some("line\nsecond line\nthird"));

        // Double-click then drag: whole words, the first one staying selected backwards too.
        let double = |pos| gpui_kit::MouseDownEvent { button: MouseButton::Left, position: pos, modifiers: Modifiers::default(), click_count: 2, first_mouse: false };
        cx.simulate_event(double(at(1., 9.)));
        cx.simulate_mouse_move(at(2., 2.), MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_up(at(2., 2.), MouseButton::Left, Modifiers::default());
        assert_eq!(term.read_with(cx, |t, _| t.selected()).as_deref(), Some("line\nthird"));
        cx.simulate_event(double(at(1., 9.)));
        cx.simulate_mouse_move(at(0., 8.), MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_up(at(0., 8.), MouseButton::Left, Modifiers::default());
        assert_eq!(term.read_with(cx, |t, _| t.selected()).as_deref(), Some("line\nsecond line"));

        // More output than the view holds, numbered lines.
        let rows = term.read_with(cx, |t, _| t.rows()) as usize;
        let text: String = (0..rows * 3).map(|i| format!("\r\nrow {i}")).collect();
        term.update(cx, |t, cx| {
            t.feed(text.as_bytes());
            cx.notify();
        });
        cx.run_until_parked();
        let shown = |row: usize| rows * 3 - rows + row; // number printed on a visible row
        let shift = Modifiers { shift: true, ..Default::default() };

        // Click, scroll up, shift-click: the selection runs from the click to the shift-click.
        cx.simulate_mouse_down(at(5., 0.), MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_up(at(5., 0.), MouseButton::Left, Modifiers::default());
        assert_eq!(term.read_with(cx, |t, _| t.selected()), None, "a plain click selects nothing");
        term.update(cx, |t, cx| {
            t.scroll_back(10);
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_mouse_down(at(3., 2.), MouseButton::Left, shift);
        cx.simulate_mouse_up(at(3., 2.), MouseButton::Left, shift);
        cx.run_until_parked();
        let got = term.read_with(cx, |t, _| t.selected()).unwrap();
        let lines: Vec<&str> = got.lines().collect();
        assert_eq!(lines.len(), 13, "from 10 rows above + row 3 down to row 5: {got}");
        assert_eq!(lines[0], format!("w {}", shown(3) - 10));
        assert_eq!(lines[12], "r");
        assert_eq!(lines[11], format!("row {}", shown(5) - 1));

        // Dragging above the top edge scrolls and keeps selecting.
        let before = term.read_with(cx, |t, _| t.scrollback());
        cx.simulate_mouse_down(at(1., 0.), MouseButton::Left, Modifiers::default());
        for _ in 0..4 {
            cx.simulate_mouse_move(point(origin.x + px(40.), origin.y - px(30.)), MouseButton::Left, Modifiers::default());
        }
        cx.simulate_mouse_up(point(origin.x + px(40.), origin.y - px(30.)), MouseButton::Left, Modifiers::default());
        cx.run_until_parked();
        assert_eq!(term.read_with(cx, |t, _| t.scrollback()), before + 4, "one line per move past the edge");
        assert_eq!(term.read_with(cx, |t, _| t.selected()).unwrap().lines().count(), 6, "4 lines scrolled in + the 2 first rows");
    }

    /// Every file gets its language's server, not only the first one opened.
    #[gpui_kit::test]
    fn files_opened_after_the_server_started_are_attached(cx: &mut TestAppContext) {
        const FAKE_SERVER: &str = r#"
import json, sys
def read():
    n = 0
    while True:
        line = sys.stdin.buffer.readline()
        if not line: sys.exit(0)
        if line.strip() == b"": break
        if line.lower().startswith(b"content-length:"): n = int(line.split(b":")[1])
    return json.loads(sys.stdin.buffer.read(n))
while True:
    m = read()
    if m.get("method") == "initialize":
        body = json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": {"capabilities": {}}}).encode()
        sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body) + body); sys.stdout.buffer.flush()
"#;
        let (ws, cx) = setup(cx);
        let root = ws.read_with(cx, |w, _| w.root.clone());
        let script = root.join("fake_lsp.py");
        std::fs::write(&script, FAKE_SERVER).unwrap();
        for f in ["one.py", "two.py"] {
            std::fs::write(root.join(f), "x = 1\n").unwrap();
        }
        // A server already running for Python files (events go nowhere: the test scheduler
        // must not be woken from its threads).
        let (tx, _rx) = smol::channel::unbounded();
        let client = crate::lsp::LspClient::start(&["/usr/bin/python3".to_string(), script.display().to_string()], &root, tx, serde_json::json!({})).unwrap();
        let key = crate::language::detect(&root.join("one.py"), &crate::plugins::Plugins::default()).server.join(" ");
        ws.update(cx, |w, _| w.servers.insert(key, super::LspSlot::Ready(client.clone())));
        for f in ["one.py", "two.py"] {
            cx.update(|window, cx| ws.update(cx, |w, cx| w.open_file(root.join(f), None, window, cx)));
            cx.run_until_parked();
        }
        let attached = ws.read_with(cx, |w, _| w.buffers().filter(|b| b.lsp.is_some()).count());
        assert_eq!(attached, 2, "both files talk to the server");
        // Kept alive past the test: its exit must not wake the scheduler from another thread.
        std::mem::forget(client);
    }

    /// An AI fix is applied in place, then kept or undone exactly.
    #[gpui_kit::test]
    fn ai_fix_is_applied_then_kept_or_undone(cx: &mut TestAppContext) {
        use super::ai_fix::AiFix;
        let (ws, cx) = setup(cx);
        let root = ws.read_with(cx, |w, _| w.root.clone());
        let file = root.join("fix.txt");
        let original = "a\nfor q in self.questions:\n    pass\n";
        std::fs::write(&file, original).unwrap();
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_file(file.clone(), Some(1), window, cx)));
        cx.run_until_parked();
        let path = ws.read_with(cx, |w, _| w.active_buffer().unwrap().path.clone());
        let text = |cx: &mut gpui_kit::VisualTestContext| ws.read_with(cx, |w, cx| w.active_buffer().unwrap().text(cx));
        let propose = |cx: &mut gpui_kit::VisualTestContext| {
            let region = super::ai_fix::fix_region(original, 1, 6);
            let p = path.clone();
            cx.update(|window, cx| {
                ws.update(cx, |w, cx| {
                    w.buffers_mut().find(|b| b.path == p).unwrap().ai_fix = Some(AiFix::Thinking { line: 2, message: "not iterable".into() });
                    w.show_ai_fix(&p, region, original.into(), Ok("a\nfor q in list(self.questions):\n    pass".into()), window, cx);
                })
            });
        };
        propose(cx);
        assert_eq!(text(cx), "a\nfor q in list(self.questions):\n    pass\n");
        assert!(ws.read_with(cx, |w, _| matches!(w.active_buffer().unwrap().ai_fix, Some(AiFix::Proposed { .. }))));
        let p = path.clone();
        cx.update(|window, cx| ws.update(cx, |w, cx| w.resolve_ai_fix(&p, false, window, cx)));
        assert_eq!(text(cx), original, "undo puts the original back");
        propose(cx);
        let p = path.clone();
        cx.update(|window, cx| ws.update(cx, |w, cx| w.resolve_ai_fix(&p, true, window, cx)));
        assert_eq!(text(cx), "a\nfor q in list(self.questions):\n    pass\n", "keep keeps it");
        assert!(ws.read_with(cx, |w, _| w.active_buffer().unwrap().ai_fix.is_none()));
    }

    /// ⌘. on a line with a problem offers to fix it with AI; elsewhere nothing.
    #[gpui_kit::test]
    fn problems_offer_an_ai_fix(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        let root = ws.read_with(cx, |w, _| w.root.clone());
        let file = root.join("m.txt"); // no language server in tests: only the AI fix
        std::fs::write(&file, "import lib\nx = lib.Sesion()\n").unwrap();
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_file(file.clone(), Some(1), window, cx)));
        cx.run_until_parked();
        let path = ws.read_with(cx, |w, _| w.active_buffer().unwrap().path.clone());
        let d: lsp_types::Diagnostic = serde_json::from_value(serde_json::json!({
            "range": {"start": {"line": 1, "character": 8}, "end": {"line": 1, "character": 14}},
            "severity": 1, "source": "Pyright", "message": "\"Sesion\" is not a known attribute of module \"lib\""
        })).unwrap();
        cx.update(|_, cx| ws.update(cx, |w, cx| w.set_diagnostics(path, vec![d], cx)));
        let editor = ws.read_with(cx, |w, _| w.active_buffer().unwrap().editor.clone());
        let actions_at = |offset: usize, cx: &mut gpui_kit::VisualTestContext| {
            let task = cx.update(|window, cx| {
                let provider = editor.read(cx).lsp().code_action_providers[0].clone();
                provider.code_actions(editor.clone(), offset..offset, window, cx)
            });
            cx.run_until_parked();
            smol::block_on(task).unwrap()
        };
        let on_error = actions_at(15, cx);
        assert_eq!(on_error.len(), 1);
        assert!(on_error[0].title.contains("Sesion"));
        let prompt = on_error[0].data.as_ref().unwrap()["forge.ai-fix"].as_str().unwrap().to_string();
        assert!(prompt.contains("ligne 2") && prompt.contains("x = lib.Sesion()"));
        assert!(actions_at(0, cx).is_empty(), "no fix on a line without problems");
    }

    #[gpui_kit::test]
    fn dragging_a_file_onto_a_folder_moves_it(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        let root = ws.read_with(cx, |w, _| w.root.clone());
        std::fs::create_dir_all(root.join("dest")).unwrap();
        std::fs::write(root.join("moveme.txt"), "x").unwrap();
        ws.update(cx, |w, cx| w.file_tree.update(cx, |t, cx| t.refresh(cx)));
        cx.run_until_parked();
        let file = cx.debug_bounds("tree-moveme.txt").expect("file row");
        let folder = cx.debug_bounds("tree-dest").expect("folder row");
        cx.simulate_mouse_down(file.center(), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(file.center() + point(gpui_kit::px(10.), gpui_kit::px(10.)), MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        cx.simulate_mouse_move(folder.center(), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(folder.center(), MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        assert!(root.join("dest/moveme.txt").is_file(), "moved into the folder");
        assert!(!root.join("moveme.txt").exists());
    }

    #[gpui_kit::test]
    fn command_palette_takes_typing_right_away(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        assert!(ws.read_with(cx, |w, _| w.sidebar.is_some()));
        cx.update(|window, cx| { let f = ws.read(cx).focus.clone(); f.focus(window, cx) });
        cx.dispatch_action(super::CommandPalette);
        cx.run_until_parked();
        let focused_in_dialog = cx.update(|window, cx| window.focused(cx).is_some() && !ws.read(cx).focus.is_focused(window));
        assert!(focused_in_dialog, "focus moved into the palette");
        cx.run_until_parked();
        cx.simulate_input("toggle sidebar");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert!(ws.read_with(cx, |w, _| w.sidebar.is_none()), "the typed query ran the action");
    }

    #[gpui_kit::test]
    fn images_open_in_a_viewer_tab(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        let root = ws.read_with(cx, |w, _| w.root.clone());
        std::fs::write(root.join("logo.svg"), include_str!("../../assets/logo.svg")).unwrap();
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_file(root.join("logo.svg"), None, window, cx)));
        cx.run_until_parked();
        ws.read_with(cx, |w, _| assert!(matches!(w.active_tab(), Some(super::Tab::Image { .. }))));
        // Secondary + wheel over the image zooms it.
        cx.run_until_parked();
        let zoom = |cx: &mut gpui_kit::VisualTestContext| ws.read_with(cx, |w, _| match w.active_tab() {
            Some(super::Tab::Image { zoom, .. }) => *zoom,
            _ => 0.,
        });
        let center = cx.update(|window, _| point(window.viewport_size().width / 2., window.viewport_size().height / 2.));
        let modifiers = if cfg!(target_os = "macos") { Modifiers::command() } else { Modifiers::control() };
        cx.simulate_event(gpui_kit::ScrollEvent { position: center, delta: gpui_kit::ScrollDelta::Pixels(point(px(0.), px(100.))), modifiers, ..Default::default() });
        cx.run_until_parked();
        assert!(zoom(cx) > 1., "zoomed to {}", zoom(cx));
        // PDFs open in-app too, even unreadable ones (the tab shows the error).
        std::fs::write(root.join("doc.pdf"), "not a pdf").unwrap();
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_file(root.join("doc.pdf"), None, window, cx)));
        cx.run_until_parked();
        ws.read_with(cx, |w, cx| assert_eq!(w.active_tab().map(|t| t.label(cx)).as_deref(), Some("doc.pdf")));
        ws.read_with(cx, |w, _| assert!(matches!(w.active_tab(), Some(super::Tab::Pdf(_)))));
    }

    #[gpui_kit::test]
    fn output_lines_can_be_selected(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        let output = ws.read_with(cx, |w, _| w.output.clone());
        cx.update(|window, cx| ws.update(cx, |w, cx| w.show_bottom_tab(super::BottomTab::Output, window, cx)));
        cx.update(|_, cx| output.update(cx, |o, cx| { o.push("first selectable line", cx); o.push("second line here", cx); }));
        cx.run_until_parked();
        let (a, b) = (cx.debug_bounds("output-line-0").expect("rendered"), cx.debug_bounds("output-line-1").unwrap());
        let start = a.origin + point(px(10.), px(4.));
        let end = point(b.origin.x + px(60.), b.origin.y + px(4.));
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(end, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        let selected = cx.update(|window, cx| gpui_kit::base::TextSelection::selected_text(window, cx));
        assert!(selected.contains("selectable") && selected.contains("second"), "selected: {selected:?}");
    }

    #[gpui_kit::test]
    fn chat_answers_can_be_selected(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        let chat = ws.read_with(cx, |w, _| w.chat.clone());
        cx.update(|_, cx| chat.update(cx, |c, cx| c.push_answer("Hello selectable world from the agent", cx)));
        cx.run_until_parked();
        let msg = cx.debug_bounds("chat-msg-0").expect("answer is rendered");
        let start = msg.origin + point(px(2.), px(6.));
        let end = point(msg.right() - px(2.), start.y);
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(end, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        let selected = cx.update(|window, cx| gpui_kit::base::TextSelection::selected_text(window, cx));
        assert!(selected.contains("selectable"), "selected: {selected:?}");
        cx.simulate_keystrokes("cmd-c");
        let copied = cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text()));
        assert!(copied.is_some_and(|c| c.contains("selectable")), "copy");
    }

    /// A file the chat changed waits for Keep / Undo in its editor; Undo puts it back.
    #[gpui_kit::test]
    fn chat_changes_are_kept_or_undone_in_the_file(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        let root = ws.read_with(cx, |w, _| w.root.clone());
        let (a, b) = (root.join("r1.txt"), root.join("r2.txt"));
        std::fs::write(&a, "one\ntwo\n").unwrap();
        std::fs::write(&b, "x\n").unwrap();
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_file(a.clone(), None, window, cx)));
        cx.run_until_parked();
        // The chat's turn.
        std::fs::write(&a, "one\n2\nthree\n").unwrap();
        std::fs::write(&b, "y\n").unwrap();
        let change = |path: &std::path::PathBuf, before: &str| crate::ai::changes::FileChange { path: path.clone(), added: 1, removed: 1, before: Some(before.as_bytes().to_vec()) };
        cx.update(|window, cx| ws.update(cx, |w, cx| {
            w.sync_with_disk(window, cx);
            w.add_reviews(vec![change(&a, "one\ntwo\n"), change(&b, "x\n")], cx);
        }));
        cx.run_until_parked();
        assert_eq!(ws.read_with(cx, |w, _| w.reviews.len()), 2);
        cx.update(|window, cx| ws.update(cx, |w, cx| {
            w.settle_review(&a, false, window, cx);
            w.settle_review(&b, true, window, cx);
        }));
        cx.run_until_parked();
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "one\ntwo\n", "Undo restores the file");
        assert_eq!(ws.read_with(cx, |w, cx| w.active_buffer().unwrap().text(cx)), "one\ntwo\n", "and its editor");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "y\n", "Keep leaves it");
        assert!(ws.read_with(cx, |w, _| w.reviews.is_empty()));

        // Block by block: two changes, undo the one at the cursor, keep the other.
        std::fs::write(&a, "ONE\ntwo\nTHREE\n").unwrap();
        cx.update(|window, cx| ws.update(cx, |w, cx| {
            w.sync_with_disk(window, cx);
            w.add_reviews(vec![crate::ai::changes::FileChange { path: a.clone(), added: 2, removed: 1, before: Some(b"one\ntwo\n".to_vec()) }], cx);
        }));
        cx.run_until_parked();
        let blocks = |cx: &mut gpui_kit::VisualTestContext| ws.read_with(cx, |w, cx| w.review_hunks(&a, cx).map_or(0, |(_, _, h)| h.len()));
        assert_eq!(blocks(cx), 2);
        // From the first block, Next goes to the second.
        let editor = ws.read_with(cx, |w, _| w.active_buffer().unwrap().editor.clone());
        cx.update(|_, cx| editor.update(cx, |e, cx| e.set_selected_range(0..0, cx)));
        cx.update(|window, cx| ws.update(cx, |w, cx| w.goto_hunk(&a, true, window, cx)));
        cx.update(|window, cx| ws.update(cx, |w, cx| w.settle_hunk(&a, false, window, cx)));
        cx.run_until_parked();
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "ONE\ntwo\n", "the block at the cursor (THREE) is undone");
        assert_eq!(blocks(cx), 1);
        cx.update(|window, cx| ws.update(cx, |w, cx| w.settle_hunk(&a, true, window, cx)));
        assert!(ws.read_with(cx, |w, _| w.reviews.is_empty()), "no block left: the file leaves review");
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "ONE\ntwo\n");
    }

    /// A long conversation only lays out the messages on screen, and shows its end.
    #[gpui_kit::test]
    fn chat_only_renders_visible_messages(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        let chat = ws.read_with(cx, |w, _| w.chat.clone());
        cx.update(|_, cx| chat.update(cx, |c, cx| (0..300).for_each(|i| c.push_answer(&format!("answer {i}"), cx))));
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("chat-msg-299").is_some(), "the last answer is shown");
        assert!(cx.debug_bounds("chat-msg-0").is_none(), "the first one is not laid out");
    }

    /// The chat shows what a turn changed; editing that message undoes it.
    #[gpui_kit::test]
    fn editing_a_message_undoes_its_changes(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        let (chat, root) = ws.read_with(cx, |w, _| (w.chat.clone(), w.root.clone()));
        std::fs::write(root.join("a.txt"), "hello\n").unwrap();
        let _ = std::fs::remove_file(root.join("new.txt"));
        cx.update(|_, cx| chat.update(cx, |c, cx| c.fake_turn("change a", |tb| {
            tb.run("edit_file", &serde_json::json!({"path": "a.txt", "old": "hello", "new": "bye\nnow"}));
            tb.run("write_file", &serde_json::json!({"path": "new.txt", "content": "x\n"}));
        }, cx)));
        let rows = chat.read_with(cx, |c, _| c.rows());
        assert_eq!(rows, vec!["user: change a".to_string(), "a.txt +2 -1, new.txt +1 -0".into()]);

        cx.update(|window, cx| chat.update(cx, |c, cx| c.edit_and_send(0, "something else", window, cx)));
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "hello\n");
        assert!(!root.join("new.txt").exists());
        let rows = chat.read_with(cx, |c, _| c.rows());
        assert!(!rows.iter().any(|r| r.contains("change a") || r.contains("a.txt")), "{rows:?}");
    }

    /// Closing the app mid-run keeps the agents, shown as stopped, and the next message says so.
    #[gpui_kit::test]
    fn interrupted_agents_survive_a_restart(cx: &mut TestAppContext) {
        use crate::ai::agent::AgentEvent;
        let (ws, cx) = setup(cx);
        let chat = ws.read_with(cx, |w, _| w.chat.clone());
        cx.update(|_, cx| {
            chat.update(cx, |c, cx| {
                c.fake_turn("build the app", |_| {}, cx);
                c.agents.update(cx, |a, cx| a.reset("m", "build the app", 1, cx));
                let spawn = |task: &str| AgentEvent::Spawned { parent: Some(0), role: "coder".into(), model: "m".into(), task: task.into() };
                c.feed(1, spawn("write the API"), cx);
                c.feed(2, spawn("write the UI"), cx);
                c.feed(1, AgentEvent::Finished { summary: "API ready".into() }, cx);
                c.save_and_reopen(cx);
            })
        });
        let (rows, note) = chat.read_with(cx, |c, cx| (c.rows(), c.agents.read(cx).resume_note().unwrap_or_default()));
        assert_eq!(rows, vec!["user: build the app".to_string(), "agent coder Done".into(), "agent coder Stopped".into()]);
        assert!(note.contains("\"write the API\": done: API ready") && note.contains("\"write the UI\": stopped"), "{note}");
    }

    /// Editing a message drops the agents of the turns it replaces; deleting an agent drops its sub-agents.
    #[gpui_kit::test]
    fn agents_follow_edits_and_deletes(cx: &mut TestAppContext) {
        use crate::ai::agent::AgentEvent;
        use crate::ai::agents_view::AgentTreeEvent;
        let (ws, cx) = setup(cx);
        let chat = ws.read_with(cx, |w, _| w.chat.clone());
        let spawn = |parent: usize, task: &str| AgentEvent::Spawned { parent: Some(parent), role: "coder".into(), model: "m".into(), task: task.into() };
        cx.update(|_, cx| {
            chat.update(cx, |c, cx| {
                for (turn, task) in [(1, "first"), (2, "second")] {
                    c.fake_turn(task, |_| {}, cx);
                    c.agents.update(cx, |a, cx| a.reset("m", task, turn, cx));
                    c.feed(1, spawn(0, task), cx);
                    c.feed(2, spawn(1, task), cx);
                    c.feed(2, AgentEvent::Finished { summary: "ok".into() }, cx);
                    c.feed(1, AgentEvent::Finished { summary: "ok".into() }, cx);
                    c.push_answer("done", cx);
                }
            })
        });
        let count = |cx: &mut gpui_kit::VisualTestContext| chat.read_with(cx, |c, cx| c.agents.read(cx).subtree(0).len() + c.agents.read(cx).subtree(3).len());
        assert_eq!(count(cx), 6);
        // The first run's coder (id 1) and the agent it started go away.
        cx.update(|_, cx| chat.update(cx, |c, cx| c.tree_event(AgentTreeEvent::Delete(1), cx)));
        let ids = chat.read_with(cx, |c, cx| c.agents.read(cx).subtree(0));
        assert_eq!(ids, vec![0]);
        let ix = chat.read_with(cx, |c, _| c.rows().iter().position(|r| r == "user: second").unwrap());
        cx.update(|window, cx| chat.update(cx, |c, cx| c.edit_and_send(ix, "other", window, cx)));
        let (first, second) = chat.read_with(cx, |c, cx| (c.agents.read(cx).get(0).is_some(), c.agents.read(cx).get(3).is_some()));
        assert!(first && !second, "only the edited turn's run is removed");
    }

    /// Switching branch shows the other branch's files and reloads open ones.
    #[gpui_kit::test]
    fn checkout_refreshes_files(cx: &mut TestAppContext) {
        let dir = std::env::temp_dir().join(format!("forgecode-checkout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| assert!(crate::util::command("git").args(args).current_dir(&dir).output().unwrap().status.success(), "{args:?}");
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(dir.join("a.txt"), "main\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "main"]);
        git(&["checkout", "-qb", "feature"]);
        std::fs::create_dir_all(dir.join("feature_dir")).unwrap();
        std::fs::write(dir.join("feature_dir/x.txt"), "x\n").unwrap();
        std::fs::write(dir.join("a.txt"), "feature\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "feature"]);
        git(&["checkout", "-q", "main"]);

        let (ws, cx) = setup_at(cx, dir.clone());
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_file(dir.join("a.txt"), None, window, cx)));
        cx.run_until_parked();
        let shows = |cx: &mut gpui_kit::VisualTestContext| ws.read_with(cx, |w, cx| w.file_tree.read(cx).shows_dir(&dir.join("feature_dir")));
        assert!(!shows(cx));

        let git_panel = ws.read_with(cx, |w, _| w.git.clone());
        cx.update(|_, cx| git_panel.update(cx, |g, cx| g.checkout("feature".into(), false, cx)));
        for _ in 0..50 {
            cx.run_until_parked();
            if shows(cx) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(shows(cx), "the new folder appears");
        let text = ws.read_with(cx, |w, cx| w.buffers().find(|b| b.path.ends_with("a.txt")).map(|b| b.text(cx)));
        assert_eq!(text.as_deref(), Some("feature\n"), "the open file is reloaded");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
