//! Debugger panel: build, launch under the language's debug adapter, breakpoints, stepping, call stack, variables.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::assets::IconName;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::i18n::t;
use serde_json::{Value, json};

use super::adapters::{self, Adapter};
use super::dap::{self, DapClient, DapEvent};
use crate::project::Program;
use crate::settings::LangSettings;
use crate::ai::tools::run_shell;

pub type Breakpoints = BTreeMap<PathBuf, BTreeSet<u32>>;

pub enum DebugEvent {
    /// Execution stopped at (file, 1-based line).
    Stopped(PathBuf, u32),
    Ended,
    /// Show a source location (breakpoint list click).
    Reveal(PathBuf, u32),
    RemoveBreakpoint(PathBuf, u32),
}

#[derive(Clone)]
struct Frame {
    id: i64,
    name: SharedString,
    path: Option<PathBuf>,
    line: u32,
}

#[derive(Clone)]
struct Var {
    name: SharedString,
    value: SharedString,
    ty: SharedString,
}

pub struct DebugPanel {
    /// The session commands go to (js-debug: the child session).
    client: Option<Arc<DapClient>>,
    /// Every adapter connection of the session, kept alive until it ends.
    session: Vec<Arc<DapClient>>,
    status: SharedString,
    thread_id: Option<i64>,
    frames: Vec<Frame>,
    selected_frame: usize,
    variables: Vec<Var>,
    console: VecDeque<SharedString>,
    /// Mirror of the workspace breakpoints, for the list.
    breakpoints: Breakpoints,
    _tasks: Vec<Task<()>>,
}

impl EventEmitter<DebugEvent> for DebugPanel {}

impl DebugPanel {
    pub fn new() -> Self {
        Self {
            client: None,
            session: vec![],
            status: t("Idle").into(),
            thread_id: None,
            frames: vec![],
            selected_frame: 0,
            variables: vec![],
            console: VecDeque::new(),
            breakpoints: Breakpoints::new(),
            _tasks: vec![],
        }
    }

    pub fn is_active(&self) -> bool {
        self.client.is_some()
    }

    fn log(&mut self, text: impl AsRef<str>, cx: &mut Context<Self>) {
        for line in text.as_ref().lines() {
            self.console.push_back(line.to_string().into());
        }
        while self.console.len() > 2000 {
            self.console.pop_front();
        }
        cx.notify();
    }

    /// Build the program for `file` if it needs it, then launch it under its language's debugger.
    pub fn start(&mut self, file: PathBuf, lsp_id: &str, workspace: PathBuf, breakpoints: Breakpoints, settings: LangSettings, cx: &mut Context<Self>) {
        self.stop(cx);
        let Some(plan) = adapters::plan(&workspace, &file, lsp_id) else {
            self.status = format!("{} `{lsp_id}` (C, C++, Rust, Zig, Swift, Python, JavaScript, TypeScript, PHP, Go)", t("No debugger for")).into();
            return cx.notify();
        };
        self.console.clear();
        self.status = t("Preparing the debugger…").into();
        cx.notify();
        let task = cx.spawn(async move |this, cx| {
            let ws = workspace.clone();
            let output = match plan.build.clone() {
                Some(build_cmd) => {
                    let _ = this.update(cx, |this, cx| {
                        this.status = t("Building…").into();
                        this.log(format!("$ {build_cmd}"), cx);
                    });
                    match cx.background_spawn(async move { run_shell(&build_cmd, &ws, &[], 600) }).await {
                        Ok(o) => o,
                        Err(e) => { let _ = this.update(cx, |this, cx| this.fail(e.to_string(), cx)); return; }
                    }
                }
                None => String::new(),
            };
            let program = match plan.program {
                Program::Path(p) => Some(p),
                Program::FromCargo => dap::cargo_executable(&output),
                Program::Newest(dir) => crate::project::newest_executable(&dir).map(|p| p.display().to_string()),
            };
            let shown: String = output.lines().filter(|l| !l.starts_with('{')).collect::<Vec<_>>().join("\n");
            let failed = output.contains("[exit status");
            let _ = this.update(cx, |this, cx| this.log(shown, cx));
            let Some(program) = program.filter(|_| !failed) else {
                let _ = this.update(cx, |this, cx| this.fail("Build failed or produced no executable".into(), cx));
                return;
            };
            let launch = adapters::launch_args(plan.adapter, &program, &workspace, &settings);
            if let Err(e) = Self::launch(this.clone(), plan.adapter, launch, settings, workspace, breakpoints, cx).await {
                let _ = this.update(cx, |this, cx| this.fail(e.to_string(), cx));
            }
        });
        self._tasks = vec![task];
    }

    async fn launch(
        this: WeakEntity<Self>,
        adapter: Adapter,
        launch: Value,
        settings: LangSettings,
        cwd: PathBuf,
        breakpoints: Breakpoints,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<()> {
        let (tx, rx) = smol::channel::unbounded();
        this.update(cx, |this, cx| { this.status = t("Installing the debugger if needed…").into(); cx.notify(); })?;
        let (dir, events) = (cwd.clone(), tx.clone());
        let (client, port) = cx
            .background_spawn(async move {
                let (argv, tcp) = adapters::command(adapter, &settings, &dir)?;
                if tcp {
                    let port = dap::free_port()?;
                    anyhow::Ok((DapClient::start_tcp(&argv, &dir, port, events)?, Some(port)))
                } else {
                    anyhow::Ok((DapClient::start(&argv, &dir, events)?, None))
                }
            })
            .await?;
        this.update(cx, |this, cx| {
            this.client = Some(client.clone());
            this.session = vec![client.clone()];
            this.status = format!("{} {}", t("Launching"), launch["program"].as_str().unwrap_or("")).into();
            cx.notify();
        })?;
        let mut client = client;
        initialize(&client).await?;
        // The launch response may only arrive after configurationDone: don't wait for it here.
        spawn_launch(&client, "launch", launch, tx.clone(), cx);
        while let Ok(event) = rx.recv().await {
            match event {
                DapEvent::Initialized => {
                    for (path, lines) in &breakpoints {
                        set_breakpoints(&client, path, lines).await?;
                    }
                    if adapter == Adapter::Python {
                        // debugpy stops on uncaught exceptions only when asked.
                        let _ = client.request("setExceptionBreakpoints", json!({"filters": ["uncaught"]})).await;
                    } else {
                        let _ = client.request("setExceptionBreakpoints", json!({"filters": []})).await;
                    }
                    client.request("configurationDone", json!({})).await?;
                    this.update(cx, |this, cx| { this.status = t("Running").into(); cx.notify(); })?;
                }
                // js-debug runs the program in a child session on a second connection.
                DapEvent::StartDebugging(args) => {
                    let Some(port) = port else { continue };
                    let events = tx.clone();
                    let child = cx.background_spawn(async move { DapClient::connect(port, events) }).await?;
                    initialize(&child).await?;
                    let request = args["request"].as_str().unwrap_or("launch").to_string();
                    spawn_launch(&child, &request, args["configuration"].clone(), tx.clone(), cx);
                    client = child.clone();
                    this.update(cx, |this, _| {
                        this.client = Some(child.clone());
                        this.session.push(child);
                    })?;
                }
                DapEvent::Stopped { thread_id, reason } => {
                    let frames = stack_trace(&client, thread_id).await.unwrap_or_default();
                    let variables = match frames.first() {
                        Some(f) => variables(&client, f.id).await.unwrap_or_default(),
                        None => vec![],
                    };
                    this.update(cx, |this, cx| {
                        this.status = format!("{} ({reason})", t("Paused")).into();
                        this.thread_id = Some(thread_id);
                        this.selected_frame = 0;
                        if let Some(Frame { path: Some(p), line, .. }) = frames.first() {
                            cx.emit(DebugEvent::Stopped(p.clone(), *line));
                        }
                        this.frames = frames;
                        this.variables = variables;
                        cx.notify();
                    })?;
                }
                DapEvent::Continued => this.update(cx, |this, cx| { this.status = t("Running").into(); this.frames.clear(); this.variables.clear(); cx.notify(); })?,
                DapEvent::Output(text) => this.update(cx, |this, cx| this.log(text, cx))?,
                DapEvent::Terminated => break,
            }
        }
        this.update(cx, |this, cx| this.end(cx))?;
        Ok(())
    }

    fn fail(&mut self, error: String, cx: &mut Context<Self>) {
        self.log(format!("error: {error}"), cx);
        self.end(cx);
        self.status = format!("Error: {error}").into();
    }

    fn end(&mut self, cx: &mut Context<Self>) {
        self.client = None;
        self.session.clear();
        self.thread_id = None;
        self.frames.clear();
        self.variables.clear();
        self.status = t("Session ended").into();
        cx.emit(DebugEvent::Ended);
        cx.notify();
    }

    /// Update one file's breakpoints in a running session.
    pub fn update_breakpoints(&mut self, path: PathBuf, lines: BTreeSet<u32>, cx: &mut Context<Self>) {
        if lines.is_empty() {
            self.breakpoints.remove(&path);
        } else {
            self.breakpoints.insert(path.clone(), lines.clone());
        }
        cx.notify();
        if let Some(client) = self.client.clone() {
            cx.background_spawn(async move { let _ = set_breakpoints(&client, &path, &lines).await; }).detach();
        }
    }

    fn command(&mut self, command: &'static str, cx: &mut Context<Self>) {
        let (Some(client), Some(thread_id)) = (self.client.clone(), self.thread_id) else { return };
        cx.background_spawn(async move { let _ = client.request(command, json!({"threadId": thread_id})).await; }).detach();
    }

    pub fn continue_(&mut self, cx: &mut Context<Self>) { self.command("continue", cx) }
    pub fn step_over(&mut self, cx: &mut Context<Self>) { self.command("next", cx) }
    pub fn step_in(&mut self, cx: &mut Context<Self>) { self.command("stepIn", cx) }
    pub fn step_out(&mut self, cx: &mut Context<Self>) { self.command("stepOut", cx) }

    pub fn pause(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else { return };
        cx.background_spawn(async move { let _ = client.request("pause", json!({"threadId": 0})).await; }).detach();
    }

    pub fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(client) = self.client.take() {
            cx.background_spawn(async move { let _ = client.request("disconnect", json!({"terminateDebuggee": true})).await; }).detach();
            self.end(cx);
        }
        self._tasks.clear();
    }

    fn select_frame(&mut self, ix: usize, cx: &mut Context<Self>) {
        let (Some(client), Some(frame)) = (self.client.clone(), self.frames.get(ix).cloned()) else { return };
        self.selected_frame = ix;
        if let Some(path) = frame.path.clone() {
            cx.emit(DebugEvent::Stopped(path, frame.line));
        }
        let task = cx.spawn(async move |this, cx| {
            let vars = variables(&client, frame.id).await.unwrap_or_default();
            let _ = this.update(cx, |this, cx| { this.variables = vars; cx.notify(); });
        });
        self._tasks.push(task);
        cx.notify();
    }
}

async fn initialize(client: &DapClient) -> anyhow::Result<()> {
    client
        .request("initialize", json!({
            "clientID": "forgecode", "clientName": "ForgeCode", "adapterID": "forgecode",
            "linesStartAt1": true, "columnsStartAt1": true, "pathFormat": "path",
            "supportsVariableType": true, "supportsStartDebuggingRequest": true, "supportsRunInTerminalRequest": false,
        }))
        .await
        .map(drop)
}

/// Send `launch` without waiting; a failure ends the session with the adapter's message.
fn spawn_launch(client: &Arc<DapClient>, request: &str, args: Value, events: smol::channel::Sender<DapEvent>, cx: &mut AsyncApp) {
    let (client, request) = (client.clone(), request.to_string());
    cx.background_spawn(async move {
        if let Err(e) = client.request(&request, args).await {
            let _ = events.send(DapEvent::Output(format!("error: {e}"))).await;
            let _ = events.send(DapEvent::Terminated).await;
        }
    })
    .detach();
}

async fn set_breakpoints(client: &DapClient, path: &std::path::Path, lines: &BTreeSet<u32>) -> anyhow::Result<()> {
    let bps: Vec<Value> = lines.iter().map(|l| json!({"line": l})).collect();
    client
        .request("setBreakpoints", json!({"source": {"path": path.display().to_string()}, "breakpoints": bps}))
        .await?;
    Ok(())
}

async fn stack_trace(client: &DapClient, thread_id: i64) -> anyhow::Result<Vec<Frame>> {
    let body = client.request("stackTrace", json!({"threadId": thread_id, "levels": 50})).await?;
    Ok(body["stackFrames"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|f| Frame {
            id: f["id"].as_i64().unwrap_or(0),
            name: f["name"].as_str().unwrap_or("?").to_string().into(),
            path: f["source"]["path"].as_str().map(PathBuf::from),
            line: f["line"].as_u64().unwrap_or(0) as u32,
        })
        .collect())
}

/// Variables of the frame's first scope (locals).
// shortcut: one level only, no expansion of structs; add a tree keyed by variablesReference when needed.
async fn variables(client: &DapClient, frame_id: i64) -> anyhow::Result<Vec<Var>> {
    let scopes = client.request("scopes", json!({"frameId": frame_id})).await?;
    let Some(reference) = scopes["scopes"][0]["variablesReference"].as_i64() else { return Ok(vec![]) };
    let body = client.request("variables", json!({"variablesReference": reference})).await?;
    Ok(body["variables"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|v| Var {
            name: v["name"].as_str().unwrap_or("").to_string().into(),
            value: v["value"].as_str().unwrap_or("").to_string().into(),
            ty: v["type"].as_str().unwrap_or("").to_string().into(),
        })
        .collect())
}

impl Render for DebugPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let paused = self.thread_id.is_some() && !self.frames.is_empty();
        let active = self.is_active();
        let btn = |id: &'static str, icon: IconName, tip: &'static str, enabled: bool| {
            Button::new(id).ghost().xsmall().icon(icon).tooltip(t(tip)).disabled(!enabled)
        };
        let section = |title: &'static str| div().px_2().py_1().text_xs().text_color(theme.muted_foreground).child(title);

        v_flex()
            .size_full()
            .child(
                h_flex()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(btn("dbg-continue", IconName::Play, "Continue", paused).on_click(cx.listener(|t, _, _, cx| t.continue_(cx))))
                    .child(btn("dbg-pause", IconName::Pause, "Pause", active && !paused).on_click(cx.listener(|t, _, _, cx| t.pause(cx))))
                    .child(btn("dbg-over", IconName::ArrowRight, "Step over", paused).on_click(cx.listener(|t, _, _, cx| t.step_over(cx))))
                    .child(btn("dbg-in", IconName::ArrowDown, "Step into", paused).on_click(cx.listener(|t, _, _, cx| t.step_in(cx))))
                    .child(btn("dbg-out", IconName::ArrowUp, "Step out", paused).on_click(cx.listener(|t, _, _, cx| t.step_out(cx))))
                    .child(btn("dbg-stop", IconName::Square, "Stop", active).on_click(cx.listener(|t, _, _, cx| t.stop(cx))))
                    .child(div().ml_2().text_xs().text_color(theme.muted_foreground).child(self.status.clone())),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_start()
                    .child(
                        v_flex().w(relative(0.25)).h_full().border_r_1().border_color(theme.border)
                            .child(section(t("CALL STACK")))
                            .child(v_flex().id("frames").flex_1().overflow_y_scrollbar().children(self.frames.iter().enumerate().map(|(ix, f)| {
                                let file = f.path.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                                div().id(("frame", ix)).px_2().text_xs().cursor_pointer()
                                    .when(ix == self.selected_frame, |el| el.bg(theme.accent))
                                    .hover(|s| s.bg(theme.accent))
                                    .child(format!("{}  {}:{}", f.name, file, f.line))
                                    .on_click(cx.listener(move |t, _, _, cx| t.select_frame(ix, cx)))
                            }))),
                    )
                    .child(
                        v_flex().w(relative(0.2)).h_full().border_r_1().border_color(theme.border)
                            .child(section(t("BREAKPOINTS")))
                            .child(v_flex().id("bps").flex_1().overflow_y_scrollbar()
                                .when(self.breakpoints.is_empty(), |el| el.child(div().px_2().text_xs().text_color(theme.muted_foreground).child(t("F9 or click a line number"))))
                                .children(self.breakpoints.iter().flat_map(|(p, lines)| lines.iter().map(move |l| (p.clone(), *l))).enumerate().map(|(ix, (path, line))| {
                                    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                                    let (p1, p2) = (path.clone(), path);
                                    h_flex().id(("bp", ix)).px_2().gap_1().text_xs().cursor_pointer().hover(|s| s.bg(theme.accent))
                                        .on_click(cx.listener(move |_, _, _, cx| cx.emit(DebugEvent::Reveal(p1.clone(), line))))
                                        .child(div().size(px(8.)).rounded_full().bg(theme.red))
                                        .child(div().flex_1().truncate().child(format!("{name}:{line}")))
                                        .child(Button::new(("bp-rm", ix)).ghost().xsmall().icon(IconName::Close)
                                            .on_click(cx.listener(move |_, _, _, cx| cx.emit(DebugEvent::RemoveBreakpoint(p2.clone(), line)))))
                                }))),
                    )
                    .child(
                        v_flex().w(relative(0.3)).h_full().border_r_1().border_color(theme.border)
                            .child(section(t("VARIABLES")))
                            .child(v_flex().id("vars").flex_1().overflow_y_scrollbar().font_family(theme.mono_font_family.clone()).children(self.variables.iter().map(|v| {
                                h_flex().px_2().gap_2().text_xs()
                                    .child(div().text_color(theme.blue).child(v.name.clone()))
                                    .child(div().flex_1().truncate().child(v.value.clone()))
                                    .child(div().text_color(theme.muted_foreground).child(v.ty.clone()))
                            }))),
                    )
                    .child(
                        v_flex().flex_1().h_full()
                            .child(section(t("DEBUG CONSOLE")))
                            .child(v_flex().id("dbg-console").flex_1().overflow_y_scrollbar().px_2().text_xs()
                                .font_family(theme.mono_font_family.clone())
                                .children(self.console.iter().map(|l| div().whitespace_nowrap().child(l.clone())))),
                    ),
            )
    }
}
