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
    /// Follows the end of the console while the build writes to it.
    console_scroll: ScrollHandle,
    /// The program is being built (Stop ends the build).
    building: bool,
    /// Mirror of the workspace breakpoints, for the list.
    breakpoints: Breakpoints,
    _tasks: Vec<Task<()>>,
}

impl EventEmitter<DebugEvent> for DebugPanel {}

/// Start `command` in the shell, each line of its output (stdout and stderr) sent as it comes.
fn start_build(command: &str, cwd: &std::path::Path, env: &[(String, String)], lines: smol::channel::Sender<String>) -> anyhow::Result<std::process::Child> {
    use std::io::BufRead as _;
    use std::process::Stdio;
    let mut cmd = crate::util::shell_command(command);
    // Its own process group, so stopping reaches cmake, the compiler… too.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut child = cmd.current_dir(cwd).envs(env.iter().cloned()).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let pipes: [Option<Box<dyn std::io::Read + Send>>; 2] = [child.stdout.take().map(|p| Box::new(p) as _), child.stderr.take().map(|p| Box::new(p) as _)];
    for pipe in pipes.into_iter().flatten() {
        let tx = lines.clone();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(pipe).lines().map_while(Result::ok) {
                if tx.send_blocking(line).is_err() {
                    break;
                }
            }
        });
    }
    Ok(child)
}

/// A build still running when its task is dropped: end it with everything it started.
struct BuildGuard(Option<u32>);

impl Drop for BuildGuard {
    fn drop(&mut self) {
        let Some(pid) = self.0 else { return };
        #[cfg(unix)]
        let _ = crate::util::command("kill").args(["-KILL", "--", &format!("-{pid}")]).status();
        #[cfg(windows)]
        let _ = crate::util::command("taskkill").args(["/F", "/T", "/PID", &pid.to_string()]).status();
    }
}

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
            console_scroll: ScrollHandle::new(),
            building: false,
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
        self.console_scroll.scroll_to_bottom();
        cx.notify();
    }

    /// Build the program for `file` if it needs it, then launch it under its language's debugger.
    /// Build if needed, then debug the plan's program (a target chosen on F5).
    pub fn start_plan(&mut self, plan: adapters::Plan, workspace: PathBuf, breakpoints: Breakpoints, settings: LangSettings, cx: &mut Context<Self>) {
        self.stop(cx);
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
                    // Shown line by line as it builds (Conan may compile dependencies for minutes).
                    let (tx, rx) = smol::channel::unbounded::<String>();
                    let mut child = match start_build(&build_cmd, &ws, &plan.env, tx) {
                        Ok(c) => c,
                        Err(e) => { let _ = this.update(cx, |this, cx| this.fail(e.to_string(), cx)); return; }
                    };
                    // Stop (or a new session) drops this task: the build and what it started end too.
                    let mut guard = BuildGuard(Some(child.id()));
                    let _ = this.update(cx, |this, _| this.building = true);
                    let mut output = String::new();
                    while let Ok(line) = rx.recv().await {
                        output.push_str(&line);
                        output.push('\n');
                        if !line.starts_with('{') && !line.starts_with("@forge ") {
                            let _ = this.update(cx, |this, cx| this.log(line, cx));
                        }
                    }
                    let ok = cx.background_spawn(async move { child.wait().is_ok_and(|s| s.success()) }).await;
                    guard.0 = None;
                    let _ = this.update(cx, |this, _| this.building = false);
                    if !ok {
                        output.push_str("\n[exit status: failed]");
                    }
                    output
                }
                None => String::new(),
            };
            // A module's build may name the program and its environment (`@forge {"action":"launch"…}`).
            let module = crate::launch::launch_override(&output);
            let program = match plan.program.clone() {
                _ if module.as_ref().is_some_and(|m| m["program"].is_string()) => module.as_ref().and_then(|m| m["program"].as_str()).map(String::from),
                Program::Path(p) => Some(p),
                Program::FromCargo => dap::cargo_executable(&output),
                Program::Newest(dir) => crate::project::newest_executable(&dir).map(|p| p.display().to_string()),
            };
            let failed = output.contains("[exit status");
            let Some(program) = program.filter(|_| !failed) else {
                let _ = this.update(cx, |this, cx| this.fail("Build failed or produced no executable".into(), cx));
                return;
            };
            let mut launch = adapters::launch_args(plan.adapter, &program, &workspace, &settings);
            if let Some(args) = &plan.args {
                launch["args"] = json!(args);
            }
            if let Some(cwd) = &plan.cwd {
                launch["cwd"] = cwd.display().to_string().into();
            }
            let mut env: Vec<String> = plan.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
            if let Some(m) = &module {
                env.extend(m["env"].as_object().into_iter().flatten().map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or(""))));
                if let Some(cwd) = m["cwd"].as_str() {
                    launch["cwd"] = cwd.into();
                }
                if let Some(args) = m["args"].as_array() {
                    launch["args"] = json!(args);
                }
            }
            if !env.is_empty() {
                launch["env"] = json!(env);
            }
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
        if std::mem::take(&mut self.building) {
            self.status = t("Build stopped").into();
            self.log("■ stopped", cx);
        }
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
        let selection = theme.selection;
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
                    .child(btn("dbg-stop", IconName::Square, "Stop", active || self.building).on_click(cx.listener(|t, _, _, cx| t.stop(cx))))
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
                                    .child(div().flex_1().min_w_0().child(gpui_kit::base::SelectableText::new(SharedString::from(format!("dbg-var-{}", v.name)), v.value.clone()).selection_color(selection)))
                                    .child(div().text_color(theme.muted_foreground).child(v.ty.clone()))
                            }))),
                    )
                    .child(
                        v_flex().flex_1().min_w_0().h_full()
                            .child(section(t("DEBUG CONSOLE")))
                            .child(v_flex().id("dbg-console").flex_1().min_h_0().overflow_y_scroll().track_scroll(&self.console_scroll).px_2().text_xs()
                                .font_family(theme.mono_font_family.clone())
                                // Selectable and copyable (⌘C / Ctrl+C), lines in reading order.
                                .children(self.console.iter().enumerate().map(|(ix, l)| div().w_full().whitespace_normal()
                                    .child(gpui_kit::base::SelectableText::new(("dbg-line", ix), l.clone()).document_order(ix as u64).selection_color(selection))))),
                    ),
            )
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::{BuildGuard, start_build};

    /// The build's lines arrive as they are written; dropping the guard ends it and its children.
    #[test]
    fn streams_the_build_and_stops_it() {
        let dir = std::env::temp_dir();
        let (tx, rx) = smol::channel::unbounded();
        let mut child = start_build("echo one; echo two 1>&2", &dir, &[], tx).unwrap();
        let mut lines: Vec<String> = smol::block_on(async { let mut v = vec![]; while let Ok(l) = rx.recv().await { v.push(l) } v });
        lines.sort();
        assert_eq!(lines, ["one", "two"]);
        assert!(child.wait().unwrap().success());

        let (tx, _rx) = smol::channel::unbounded();
        let mut child = start_build("sleep 30; echo late", &dir, &[], tx).unwrap();
        let started = std::time::Instant::now();
        drop(BuildGuard(Some(child.id())));
        assert!(!child.wait().unwrap().success());
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "stopped right away");
    }
}
