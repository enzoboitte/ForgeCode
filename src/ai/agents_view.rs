//! Live diagram of the agents of a run: who delegated what to whom, on which model, doing what now.
//! Click a node to read its transcript. The diagram pans (wheel, trackpad, drag) and zooms
//! (pinch, cmd/ctrl + wheel).

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use serde::{Deserialize, Serialize};

use super::agent::AgentEvent;
use crate::i18n::t;

#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
pub enum Status {
    Running,
    Done,
    Failed,
    /// Interrupted: stopped by the user or the app closed during the run.
    Stopped,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub kind: String,
    pub text: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct AgentNode {
    pub id: usize,
    pub parent: Option<usize>,
    pub role: String,
    pub model: String,
    pub task: String,
    pub status: Status,
    pub log: Vec<LogEntry>,
    /// The agent's final answer (start of it).
    #[serde(default)]
    pub summary: String,
    /// Main agents: history index of the user message that started the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<usize>,
}

impl AgentNode {
    /// What the agent is doing now, in one line.
    fn activity(&self) -> String {
        self.log.last().map(|e| e.text.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").to_string()).unwrap_or_default()
    }
}

const MIN_ZOOM: f32 = 0.25;
const MAX_ZOOM: f32 = 2.5;
/// Card size at zoom 1; the height is fixed so big trees stay readable.
const CARD_W: f32 = 230.;
const CARD_H: f32 = 104.;
const MIN_PANEL: f32 = 220.;

/// What the user asks from the Agents view; the chat carries it out.
pub enum AgentTreeEvent {
    Stop(usize),
    Delete(usize),
    Message(usize, String),
}

pub struct AgentTree {
    pub nodes: Vec<AgentNode>,
    selected: Option<usize>,
    /// Pan of the diagram, in pixels.
    offset: Point<Pixels>,
    zoom: f32,
    /// Last mouse position while dragging the background.
    drag: Option<Point<Pixels>>,
    /// Where the diagram area is on screen, to zoom around the cursor.
    bounds: Rc<Cell<Bounds<Pixels>>>,
    /// Id of the current run's main agent. Every message starts a run whose agents are numbered
    /// from 0; they are stored from `base` on so earlier runs stay in the diagram.
    base: usize,
    /// Side panel (transcript and actions of the selected agent).
    panel_open: bool,
    panel_w: f32,
    resizing: bool,
    composer: Option<Entity<InputState>>,
    _subs: Vec<Subscription>,
}

impl Default for AgentTree {
    fn default() -> Self {
        Self {
            nodes: vec![],
            selected: None,
            offset: point(px(0.), px(24.)),
            zoom: 1.,
            drag: None,
            bounds: Rc::default(),
            base: 0,
            panel_open: true,
            panel_w: 340.,
            resizing: false,
            composer: None,
            _subs: vec![],
        }
    }
}

impl EventEmitter<AgentTreeEvent> for AgentTree {}

impl AgentTree {
    /// Start a run for the user message at history index `turn`: the main agent is node 0.
    pub fn reset(&mut self, model: &str, task: &str, turn: usize, cx: &mut Context<Self>) {
        self.base = self.nodes.iter().map(|n| n.id + 1).max().unwrap_or(0);
        self.nodes.push(AgentNode { id: self.base, parent: None, role: "main".into(), model: model.into(), task: task.into(), status: Status::Running, log: vec![], summary: String::new(), turn: Some(turn) });
        self.selected = Some(self.base);
        cx.notify();
    }

    /// Id in the tree of agent `id` of the current run.
    pub fn global(&self, id: usize) -> usize {
        self.base + id
    }

    /// Id in the current run of tree node `gid`, if it belongs to it.
    pub fn run_id(&self, gid: usize) -> Option<usize> {
        gid.checked_sub(self.base)
    }

    pub fn get(&self, gid: usize) -> Option<&AgentNode> {
        self.nodes.iter().find(|n| n.id == gid)
    }

    /// `gid` and every agent below it.
    pub fn subtree(&self, gid: usize) -> Vec<usize> {
        let mut out = vec![gid];
        let mut i = 0;
        while i < out.len() {
            let p = out[i];
            out.extend(self.nodes.iter().filter(|n| n.parent == Some(p)).map(|n| n.id));
            i += 1;
        }
        out
    }

    /// Remove agents (with the agents they started); returns the removed ids.
    pub fn remove(&mut self, gid: usize, cx: &mut Context<Self>) -> Vec<usize> {
        let gone = self.subtree(gid);
        self.nodes.retain(|n| !gone.contains(&n.id));
        if self.selected.is_some_and(|s| gone.contains(&s)) {
            self.selected = None;
        }
        cx.notify();
        gone
    }

    /// Remove the runs of the user messages from history index `turn` on (the conversation was rewound).
    pub fn remove_turns_from(&mut self, turn: usize, cx: &mut Context<Self>) -> Vec<usize> {
        let roots: Vec<usize> = self.nodes.iter().filter(|n| n.parent.is_none() && n.turn.is_some_and(|t| t >= turn)).map(|n| n.id).collect();
        roots.into_iter().flat_map(|r| self.remove(r, cx)).collect()
    }

    /// The user stopped `gid`: it and the agents it started are stopped.
    pub fn mark_stopped(&mut self, gid: usize, cx: &mut Context<Self>) {
        let ids = self.subtree(gid);
        for n in self.nodes.iter_mut().filter(|n| ids.contains(&n.id) && n.status == Status::Running) {
            n.status = Status::Stopped;
        }
        cx.notify();
    }

    /// Show a message the user sent to `gid` in its transcript.
    pub fn note_message(&mut self, gid: usize, text: &str, cx: &mut Context<Self>) {
        if let Some(n) = self.node(gid) {
            n.log.push(LogEntry { kind: "user".into(), text: format!("› {text}") });
        }
        cx.notify();
    }

    fn send_message(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(gid), Some(input)) = (self.selected, self.composer.clone()) else { return };
        let text = input.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        input.update(cx, |i, cx| i.set_value("", window, cx));
        cx.emit(AgentTreeEvent::Message(gid, text));
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.nodes.clear();
        self.base = 0;
        self.selected = None;
        cx.notify();
    }

    /// The run was interrupted: agents still working are marked stopped.
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        for n in &mut self.nodes {
            if n.status == Status::Running {
                n.status = Status::Stopped;
            }
        }
        cx.notify();
    }

    /// The tree as saved with the conversation (transcripts shortened).
    pub fn snapshot(&self) -> Vec<AgentNode> {
        let mut nodes = self.nodes.clone();
        for n in &mut nodes {
            let skip = n.log.len().saturating_sub(40);
            n.log.drain(..skip);
            for e in &mut n.log {
                if e.text.len() > 2000 {
                    e.text = e.text.chars().take(2000).collect();
                }
            }
        }
        nodes
    }

    /// A saved tree; agents that were running when it was saved were interrupted.
    pub fn restore(&mut self, mut nodes: Vec<AgentNode>, cx: &mut Context<Self>) {
        for n in &mut nodes {
            if n.status == Status::Running {
                n.status = Status::Stopped;
            }
        }
        self.base = nodes.iter().rfind(|n| n.parent.is_none()).map_or(0, |n| n.id);
        self.selected = Some(self.base);
        self.nodes = nodes;
        self.reset_view();
        cx.notify();
    }

    /// For the next message after an interrupted run: what each agent had done, so the model can resume.
    pub fn resume_note(&self) -> Option<String> {
        let run: Vec<&AgentNode> = self.nodes.iter().filter(|n| n.id >= self.base).collect();
        if !run.iter().any(|n| n.status == Status::Stopped) {
            return None;
        }
        let short = |s: &str, n: usize| {
            let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
            if s.chars().count() > n { format!("{}…", s.chars().take(n).collect::<String>()) } else { s }
        };
        let mut note = String::from(RESUME_NOTE);
        for n in run.iter().filter(|n| n.parent.is_some() && n.role != "security") {
            let state = match n.status {
                Status::Done => format!("done: {}", short(&n.summary, 200)),
                Status::Failed => format!("failed: {}", short(&n.summary, 120)),
                _ => format!("stopped, last step: {}", short(&n.activity(), 120)),
            };
            note.push_str(&format!("\n- {} \"{}\": {state}", n.role, short(&n.task, 120)));
        }
        note.push_str("\nResume the unfinished work; delegate again only what is not done.");
        Some(note)
    }

    fn reset_view(&mut self) {
        self.offset = point(px(0.), px(24.));
        self.zoom = 1.;
    }

    /// Zoom by `factor` keeping the point under `position` (window coordinates) in place.
    fn zoom_at(&mut self, factor: f32, position: Point<Pixels>, cx: &mut Context<Self>) {
        let zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let bounds = self.bounds.get();
        // The diagram is centered horizontally: its origin is the top center of the area, plus the pan.
        let origin = point(bounds.origin.x + bounds.size.width / 2. + self.offset.x, bounds.origin.y + self.offset.y);
        let content = (position - origin) / self.zoom;
        self.offset = position - point(bounds.origin.x + bounds.size.width / 2., bounds.origin.y) - content * zoom;
        self.zoom = zoom;
        cx.notify();
    }

    fn zoom_center(&mut self, factor: f32, cx: &mut Context<Self>) {
        let b = self.bounds.get();
        self.zoom_at(factor, b.center(), cx);
    }

    fn node(&mut self, id: usize) -> Option<&mut AgentNode> {
        self.nodes.iter_mut().find(|n| n.id == id)
    }

    pub fn apply(&mut self, run_id: usize, event: &AgentEvent, cx: &mut Context<Self>) {
        let id = self.global(run_id);
        if let AgentEvent::Spawned { parent, role, model, task } = event {
            self.nodes.push(AgentNode { id, parent: parent.map(|p| self.base + p), role: role.clone(), model: model.clone(), task: task.clone(), status: Status::Running, log: vec![], summary: String::new(), turn: None });
            return cx.notify();
        }
        let Some(node) = self.node(id) else { return };
        match event {
            AgentEvent::Text(delta) => match node.log.last_mut() {
                Some(e) if e.kind == "text" => e.text.push_str(delta),
                _ => node.log.push(LogEntry { kind: "text".into(), text: delta.clone() }),
            },
            AgentEvent::ToolStart { name, args } => node.log.push(LogEntry { kind: "tool".into(), text: format!("{name} {}", args.chars().take(120).collect::<String>()) }),
            AgentEvent::ToolResult { output } => node.log.push(LogEntry { kind: "result".into(), text: output.chars().take(400).collect() }),
            AgentEvent::Review { name, allowed, reason } => {
                node.log.push(LogEntry { kind: "review".into(), text: format!("{} {name}: {reason}", if *allowed { "✓" } else { "✕" }) })
            }
            // A stopped agent still finishes its current step: it stays "stopped".
            AgentEvent::Finished { summary } => {
                node.summary = summary.clone();
                if node.status != Status::Stopped {
                    node.status = if summary.starts_with("error") { Status::Failed } else { Status::Done };
                }
            }
            AgentEvent::Done(_) if node.status != Status::Stopped => node.status = Status::Done,
            AgentEvent::Error(e, _) => {
                if node.status != Status::Stopped {
                    node.status = Status::Failed;
                }
                node.log.push(LogEntry { kind: "error".into(), text: e.clone() });
            }
            _ => {}
        }
        // The security reviewer stays "running" while it waits for the next request; mark it idle once the run ends.
        if matches!(event, AgentEvent::Done(_) | AgentEvent::Error(..)) && run_id == 0 {
            let base = self.base;
            for n in self.nodes.iter_mut().filter(|n| n.id >= base) {
                if n.status == Status::Running {
                    n.status = Status::Done;
                }
            }
        }
        cx.notify();
    }

    fn render_card(&self, node: &AgentNode, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let selected = self.selected == Some(node.id);
        let (icon, accent) = match node.role.as_str() {
            "main" => (IconName::Bot, theme.primary),
            "security" => (IconName::ShieldCheck, theme.warning),
            "researcher" => (IconName::Search, theme.blue),
            "reviewer" => (IconName::Eye, theme.cyan),
            _ => (IconName::Code, theme.green),
        };
        let id = node.id;
        let z = self.zoom;
        let s = |v: f32| px(v * z);
        v_flex()
            .id(("agent-card", id))
            .w(s(CARD_W))
            .h(s(CARD_H))
            .flex_none()
            .overflow_hidden()
            .p(s(8.))
            .gap(s(4.))
            .rounded(s(8.))
            .text_size(s(12.))
            .line_height(s(16.))
            .border_1()
            .border_color(if selected { accent } else { theme.border })
            .bg(theme.background)
            .cursor_pointer()
            .on_click(cx.listener(move |t, _, _, cx| {
                t.selected = Some(id);
                t.panel_open = true;
                cx.notify();
            }))
            .child(
                h_flex()
                    .gap(s(6.))
                    .text_size(s(14.))
                    .line_height(s(20.))
                    .child(div().text_color(accent).child(Icon::new(icon).size(s(16.))))
                    .child(div().flex_1().font_weight(FontWeight::MEDIUM).child(t(match node.role.as_str() {
                        "main" => "Main agent",
                        "security" => "Security",
                        "researcher" => "Researcher",
                        "reviewer" => "Reviewer",
                        "coder" => "Coder",
                        _ => "Agent",
                    })))
                    .child(status_icon(node.status, s(14.), cx)),
            )
            .child(div().truncate().text_color(theme.muted_foreground).child(node.model.clone()))
            .child(div().line_clamp(2).child(node.task.clone()))
            .child(div().truncate().text_color(theme.muted_foreground.opacity(0.8)).font_family(theme.mono_font_family.clone()).child(node.activity()))
            .into_any_element()
    }

    /// The node and its children below it, linked like an org chart.
    fn render_subtree(&self, id: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(node) = self.nodes.iter().find(|n| n.id == id) else { return div().into_any_element() };
        let card = self.render_card(node, cx);
        let children: Vec<usize> = self.nodes.iter().filter(|n| n.parent == Some(id)).map(|n| n.id).collect();
        let line = cx.theme().border;
        let z = self.zoom;
        let link = px(14. * z);
        let subtrees: Vec<AnyElement> = children
            .iter()
            .map(|c| v_flex().flex_none().items_center().child(div().w(px(1.)).h(link).bg(line)).child(self.render_subtree(*c, cx)).into_any_element())
            .collect();
        v_flex()
            .flex_none()
            .items_center()
            .child(card)
            .when(!subtrees.is_empty(), |el| {
                el.child(div().w(px(1.)).h(link).bg(line))
                    .child(h_flex().flex_none().items_start().gap(px(16. * z)).px(px(8. * z)).border_t_1().border_color(line).children(subtrees))
            })
            .into_any_element()
    }
}

pub const RESUME_NOTE: &str = "\n\nNote: the previous run was stopped before it finished.";

pub fn status_icon(status: Status, size: Pixels, cx: &App) -> AnyElement {
    let theme = cx.theme();
    match status {
        Status::Running => Spinner::new().xsmall().into_any_element(),
        Status::Done => Icon::new(IconName::Check).size(size).text_color(theme.success).into_any_element(),
        Status::Failed => Icon::new(IconName::CircleX).size(size).text_color(theme.danger).into_any_element(),
        Status::Stopped => Icon::new(IconName::CirclePause).size(size).text_color(theme.warning).into_any_element(),
    }
}

impl Render for AgentTree {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.composer.is_none() {
            let input = cx.new(|cx| InputState::new(window, cx).placeholder(t("Message to this agent…")));
            self._subs.push(cx.subscribe_in(&input, window, |this, _, e: &InputEvent, window, cx| {
                if matches!(e, InputEvent::PressEnter { .. }) {
                    this.send_message(window, cx);
                }
            }));
            self.composer = Some(input);
        }
        if self.nodes.is_empty() {
            let theme = cx.theme();
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_3()
                .px_8()
                .text_color(theme.muted_foreground)
                .child(div().text_2xl().child(IconName::Network))
                .child(div().max_w(px(420.)).text_sm().text_center().child(t("No agents yet. Send a message in Smart mode to see delegation.")))
                .into_any_element();
        }
        let diagram = {
            // One tree per message, oldest first.
            let roots: Vec<usize> = self.nodes.iter().filter(|n| n.parent.is_none()).map(|n| n.id).collect();
            let gap = px(40. * self.zoom);
            v_flex().flex_none().items_center().gap(gap).children(roots.into_iter().map(|r| self.render_subtree(r, cx)).collect::<Vec<_>>())
        };
        let selected = self.selected.and_then(|id| self.nodes.iter().find(|n| n.id == id));
        let transcript: Vec<AnyElement> = selected
            .map(|n| {
                n.log
                    .iter()
                    .map(|e| {
                        let theme = cx.theme();
                        let color = match e.kind.as_str() {
                            "tool" => theme.blue,
                            "review" => theme.warning,
                            "error" => theme.danger,
                            "result" => theme.muted_foreground,
                            "user" => theme.primary,
                            _ => theme.foreground,
                        };
                        div()
                            .text_xs()
                            .text_color(color)
                            .when(e.kind != "text", |el| el.font_family(theme.mono_font_family.clone()))
                            .child(e.text.clone())
                            .into_any_element()
                    })
                    .collect()
            })
            .unwrap_or_default();
        let title = selected.map(|n| format!("{} · {}", n.role, n.model)).unwrap_or_default();
        let (sel_id, sel_running, sel_main) = selected.map_or((None, false, false), |n| (Some(n.id), n.status == Status::Running, n.parent.is_none()));
        let panel_open = self.panel_open;
        let theme = cx.theme();
        let bounds = self.bounds.clone();
        let zoom_label = format!("{}%", (self.zoom * 100.).round());
        let canvas_area = div()
            .id("agents-diagram")
            .relative()
            .flex_1()
            .h_full()
            .min_w_0()
            .overflow_hidden()
            .cursor(if self.drag.is_some() { CursorStyle::ClosedHand } else { CursorStyle::default() })
            .child(canvas(move |b, _, _| bounds.set(b), |_, _, _, _| {}).absolute().size_full())
            // Wheel / two-finger scroll pans (shift: horizontal); with cmd or ctrl it zooms.
            .on_scroll(cx.listener(|this, e: &ScrollEvent, window, cx| {
                let delta = e.delta.pixel_delta(window.line_height());
                if e.modifiers.platform || e.modifiers.control {
                    let factor = (1. + f32::from(delta.y) / 200.).clamp(0.5, 2.);
                    this.zoom_at(factor, e.position, cx);
                } else {
                    let delta = if e.modifiers.shift && delta.x == px(0.) { point(delta.y, px(0.)) } else { delta };
                    this.offset = this.offset + delta;
                    cx.notify();
                }
                cx.stop_propagation();
            }))
            .on_pinch(cx.listener(|this, e: &PinchEvent, _, cx| this.zoom_at(1. + e.delta, e.position, cx)))
            .on_mouse_down(MouseButton::Left, cx.listener(|this, e: &MouseDownEvent, _, _| this.drag = Some(e.position)))
            .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| {
                let Some(last) = this.drag else { return };
                if e.pressed_button != Some(MouseButton::Left) {
                    this.drag = None;
                    return cx.notify();
                }
                this.offset = this.offset + (e.position - last);
                this.drag = Some(e.position);
                cx.notify();
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, cx| {
                this.drag = None;
                cx.notify();
            }))
            .child(
                h_flex()
                    .absolute()
                    .left(self.offset.x)
                    .top(self.offset.y)
                    .w_full()
                    .justify_center()
                    .child(diagram),
            )
            .child(
                h_flex()
                    .absolute()
                    .bottom_2()
                    .left_2()
                    .gap_1()
                    .p_0p5()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.background)
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(Button::new("agents-zoom-out").ghost().xsmall().icon(IconName::Minus).tooltip(t("Zoom out")).on_click(cx.listener(|this, _, _, cx| this.zoom_center(1. / 1.2, cx))))
                    .child(div().w(px(40.)).text_center().text_xs().text_color(theme.muted_foreground).child(zoom_label))
                    .child(Button::new("agents-zoom-in").ghost().xsmall().icon(IconName::Plus).tooltip(t("Zoom in")).on_click(cx.listener(|this, _, _, cx| this.zoom_center(1.2, cx))))
                    .child(Button::new("agents-zoom-reset").ghost().xsmall().icon(IconName::Maximize).tooltip(t("Reset view")).on_click(cx.listener(|this, _, _, cx| {
                        this.reset_view();
                        cx.notify();
                    })))
                    .child(Button::new("agents-panel").ghost().xsmall().icon(IconName::PanelRight).when(panel_open, |b| b.primary())
                        .tooltip(if panel_open { t("Hide details") } else { t("Show details") })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.panel_open = !this.panel_open;
                            cx.notify();
                        }))),
            );
        let panel = v_flex()
            .relative()
            .w(px(self.panel_w))
            .flex_none()
            .h_full()
            .border_l_1()
            .border_color(theme.border)
            // Drag the left edge to resize.
            .child(
                div()
                    .id("agents-panel-resize")
                    .absolute()
                    .left(px(-3.))
                    .top_0()
                    .h_full()
                    .w(px(6.))
                    .cursor(CursorStyle::ResizeLeftRight)
                    .hover(|s| s.bg(theme.primary.opacity(0.4)))
                    .when(self.resizing, |s| s.bg(theme.primary.opacity(0.4)))
                    .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| {
                        this.resizing = true;
                        cx.stop_propagation();
                        cx.notify();
                    })),
            )
            .child(
                h_flex()
                    .px_3()
                    .py_1()
                    .gap_1()
                    .child(div().flex_1().min_w_0().truncate().text_xs().text_color(theme.muted_foreground).child(title))
                    .when_some(sel_id.filter(|_| sel_running), |el, id| {
                        el.child(Button::new("agent-stop").ghost().xsmall().icon(IconName::Square).tooltip(t("Stop this agent"))
                            .on_click(cx.listener(move |_, _, _, cx| cx.emit(AgentTreeEvent::Stop(id)))))
                    })
                    .when_some(sel_id, |el, id| {
                        el.child(Button::new("agent-delete").ghost().xsmall().icon(IconName::Delete)
                            .tooltip(if sel_main { t("Delete this run and its agents") } else { t("Delete this agent") })
                            .on_click(cx.listener(move |_, _, _, cx| cx.emit(AgentTreeEvent::Delete(id)))))
                    })
                    .child(Button::new("agent-panel-close").ghost().xsmall().icon(IconName::Close).tooltip(t("Hide details"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.panel_open = false;
                            cx.notify();
                        }))),
            )
            .child(v_flex().id("agent-transcript").flex_1().min_h_0().overflow_y_scrollbar().px_3().gap_2().children(transcript))
            .when_some(sel_id.filter(|_| !sel_main).zip(self.composer.clone()), |el, (_, input)| {
                el.child(
                    h_flex()
                        .p_2()
                        .gap_1()
                        .border_t_1()
                        .border_color(theme.border)
                        .child(div().flex_1().child(Input::new(&input).small()))
                        .child(Button::new("agent-send").small().primary().icon(IconName::ArrowUp).tooltip(t("Send"))
                            .on_click(cx.listener(|this, _, w, cx| this.send_message(w, cx)))),
                )
            });
        let bounds = self.bounds.clone();
        h_flex()
            .size_full()
            .when(self.resizing, |el| el.cursor(CursorStyle::ResizeLeftRight))
            .on_mouse_move(cx.listener(move |this, e: &MouseMoveEvent, _, cx| {
                if !this.resizing {
                    return;
                }
                if e.pressed_button != Some(MouseButton::Left) {
                    this.resizing = false;
                    return cx.notify();
                }
                // The panel's right edge is the diagram's right edge plus the panel width.
                let b = bounds.get();
                let right = f32::from(b.origin.x + b.size.width) + this.panel_w;
                let total = f32::from(b.size.width) + this.panel_w;
                this.panel_w = (right - f32::from(e.position.x)).clamp(MIN_PANEL, (total - 160.).max(MIN_PANEL));
                cx.notify();
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, cx| {
                if this.resizing {
                    this.resizing = false;
                    cx.notify();
                }
            }))
            .child(canvas_area)
            .when(panel_open, |el| el.child(panel))
            .into_any_element()
    }
}
