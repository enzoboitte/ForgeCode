//! Output panel: runs build/run/module commands and streams their output.
//! Lines `@forge {json}` printed by module commands become IDE actions.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::assets::IconName;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::i18n::t;

use crate::plugins::{IdeAction, protocol};

const MAX_LINES: usize = 5_000;

pub enum OutputEvent {
    Action(IdeAction),
    /// A command started or ended (the editor's Run button becomes Stop meanwhile).
    Running,
}

pub struct OutputPanel {
    lines: VecDeque<SharedString>,
    running: Arc<Mutex<Option<Child>>>,
    /// Variable-height rows: long lines wrap. Bottom-aligned, so it follows new output.
    scroll: ListState,
    _task: Option<Task<()>>,
}

impl EventEmitter<OutputEvent> for OutputPanel {}

enum Msg {
    Line(String),
    Exit(Option<i32>),
}

impl OutputPanel {
    pub fn new() -> Self {
        Self { lines: VecDeque::new(), running: Default::default(), scroll: ListState::new(0, ListAlignment::Bottom, px(200.)), _task: None }
    }

    pub fn push(&mut self, line: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.lines.push_back(line.into());
        self.scroll.splice(self.lines.len() - 1..self.lines.len() - 1, 1);
        while self.lines.len() > MAX_LINES {
            self.lines.pop_front();
            self.scroll.splice(0..1, 0);
        }
        cx.notify();
    }

    pub fn is_running(&self) -> bool {
        self.running.lock().unwrap().is_some()
    }

    pub fn stop(&mut self, cx: &mut Context<Self>) {
        let taken = self.running.lock().unwrap().take();
        if let Some(mut child) = taken {
            // The shell and everything it started (`cargo run` → the program, `npm` → node…):
            // they share a process group, the shell's pid. Asked to stop first, forced 2 s later.
            #[cfg(unix)]
            {
                let group = format!("-{}", child.id());
                let _ = crate::util::command("kill").args(["-TERM", "--", &group]).status();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    let _ = crate::util::command("kill").args(["-KILL", "--", &group]).status();
                });
            }
            let _ = child.kill();
            let _ = child.wait();
            self.push("■ stopped".to_string(), cx);
            cx.emit(OutputEvent::Running);
        }
        cx.notify();
    }

    /// Run `command` in the platform shell, replacing any running command.
    pub fn run(&mut self, title: &str, command: &str, cwd: &Path, env: Vec<(String, String)>, cx: &mut Context<Self>) {
        self.stop(cx);
        self.push(format!("▶ {title}: {command}"), cx);
        let mut command_line = crate::util::shell_command(command);
        // Its own process group, so Stop reaches what the shell started too.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command_line, 0);
        let spawned = command_line
            .current_dir(cwd)
            .envs(env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => return self.push(format!("failed to start: {e}"), cx),
        };
        let (tx, rx) = smol::channel::unbounded();
        for pipe in [child.stdout.take().map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
                     child.stderr.take().map(|p| Box::new(p) as Box<dyn std::io::Read + Send>)].into_iter().flatten() {
            let tx = tx.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                    let _ = tx.send_blocking(Msg::Line(line));
                }
            });
        }
        *self.running.lock().unwrap() = Some(child);
        cx.emit(OutputEvent::Running);
        let running = self.running.clone();
        // Waiter: polls so that `stop()` can take the child out to kill it.
        std::thread::spawn(move || loop {
            let status = match running.lock().unwrap().as_mut() {
                None => break,
                Some(child) => child.try_wait(),
            };
            match status {
                Ok(Some(s)) => {
                    running.lock().unwrap().take();
                    let _ = tx.send_blocking(Msg::Exit(s.code()));
                    break;
                }
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(100)),
                Err(_) => break,
            }
        });
        self._task = Some(cx.spawn(async move |this, cx| {
            while let Ok(msg) = rx.recv().await {
                let alive = this.update(cx, |this, cx| match msg {
                    Msg::Line(line) => match protocol::parse_line(&line) {
                        Some(Ok(action)) => cx.emit(OutputEvent::Action(action)),
                        Some(Err(e)) => this.push(e, cx),
                        None => this.push(line, cx),
                    },
                    Msg::Exit(code) => {
                        this.push(format!("■ exited with {}", code.map_or("signal".into(), |c| c.to_string())), cx);
                        cx.emit(OutputEvent::Running);
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        }));
    }
}

#[derive(Debug, PartialEq)]
enum Tone {
    Error,
    Warning,
    Success,
    Header,
    Muted,
}

/// A build tool's line color, from its text (the commands run without a terminal, so no ANSI colors).
fn tone(line: &str) -> Option<Tone> {
    let lower = line.to_lowercase();
    if line.starts_with('▶') {
        Some(Tone::Header)
    } else if line.starts_with("■ exited with 0") || lower.contains("built target") || lower.starts_with("-- build files have been written") {
        Some(Tone::Success)
    } else if line.starts_with('■') || lower.contains("error") || lower.contains("failed") || lower.contains("fatal") {
        Some(Tone::Error)
    } else if lower.contains("warning") {
        Some(Tone::Warning)
    } else if line.starts_with("--") || (line.starts_with('[') && line.get(4..6) == Some("%]")) {
        Some(Tone::Muted)
    } else {
        None
    }
}

impl Render for OutputPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let lines = self.lines.clone();
        let text = lines.iter().map(|l| l.as_ref()).collect::<Vec<_>>().join("\n");
        let selection_color = cx.theme().selection;
        let theme = cx.theme().clone();
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .child(Button::new("copy-output").ghost().xsmall().icon(IconName::Copy).tooltip(t("Copy")).on_click(
                        move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(text.clone())),
                    ))
                    .child(Button::new("clear-output").ghost().xsmall().icon(IconName::Delete).tooltip(t("Clear")).on_click(
                        cx.listener(|this, _, _, cx| { this.lines.clear(); this.scroll.reset(0); cx.notify(); }),
                    ))
                    .when(self.is_running(), |el| {
                        el.child(Button::new("stop-output").ghost().xsmall().icon(IconName::Square).tooltip(t("Stop")).on_click(
                            cx.listener(|this, _, _, cx| this.stop(cx)),
                        ))
                    }),
            )
            .child(div().relative().flex_1().min_h_0().child(
                list(self.scroll.clone(), move |ix, _, _| {
                    // Long lines wrap (gpui also breaks words longer than the panel, like paths).
                    div()
                        .w_full()
                        .px_2()
                        .whitespace_normal()
                        .debug_selector(move || format!("output-line-{ix}"))
                        .when_some(tone(&lines[ix]), |el, tone| el.text_color(match tone {
                            Tone::Error => theme.danger,
                            Tone::Warning => theme.warning,
                            Tone::Success => theme.success,
                            Tone::Header => theme.primary,
                            Tone::Muted => theme.muted_foreground,
                        }))
                        .child(gpui_kit::base::SelectableText::new(("output-line", ix), lines[ix].clone()).document_order(ix as u64).selection_color(selection_color))
                        .into_any_element()
                })
                .size_full()
                .font_family(cx.theme().mono_font_family.clone())
                // The editor's font size (Settings › Appearance), as the terminal.
                .text_size(cx.theme().mono_font_size),
            ).vertical_scrollbar(&self.scroll))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn build_lines_get_a_tone() {
        use super::{Tone, tone};
        assert_eq!(tone("▶ Build: cmake --build build"), Some(Tone::Header));
        assert_eq!(tone("[ 54%] Built target sfml-graphics"), Some(Tone::Success));
        assert_eq!(tone("[ 55%] Building CXX object a.cpp.o"), Some(Tone::Muted));
        assert_eq!(tone("[100%] Linking CXX static library x.a"), Some(Tone::Muted));
        assert_eq!(tone("main.cpp:3:1: error: unknown type"), Some(Tone::Error));
        assert_eq!(tone("main.cpp:3:1: warning: unused"), Some(Tone::Warning));
        assert_eq!(tone("-- Configuring done"), Some(Tone::Muted));
        assert_eq!(tone("■ exited with 2"), Some(Tone::Error));
        assert_eq!(tone("hello"), None);
    }

    use gpui_kit::{AppContext as _, TestAppContext};

    /// Stop ends what the command started, not just the shell running it.
    #[cfg(unix)]
    #[gpui_kit::test]
    fn stop_ends_the_programs_the_command_started(cx: &mut TestAppContext) {
        let dir = std::env::temp_dir().join(format!("forgecode-stop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let output = cx.new(|_| super::OutputPanel::new());
        // A program started by the shell, which stays to wait for it (as `cargo run` does).
        output.update(cx, |o, cx| o.run("test", "sleep 300 & echo $! > pid; wait", &dir, vec![], cx));
        let pid_file = dir.join("pid");
        for _ in 0..100 {
            if std::fs::read_to_string(&pid_file).is_ok_and(|p| !p.trim().is_empty()) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let pid = std::fs::read_to_string(&pid_file).unwrap().trim().to_string();
        let alive = |pid: &str| crate::util::command("kill").args(["-0", pid]).status().unwrap().success();
        assert!(alive(&pid));
        assert!(output.read_with(cx, |o, _| o.is_running()));
        output.update(cx, |o, cx| o.stop(cx));
        assert!(!output.read_with(cx, |o, _| o.is_running()));
        let mut gone = false;
        for _ in 0..50 {
            if !alive(&pid) {
                gone = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(gone, "the program started by the command was stopped too");
    }
}
