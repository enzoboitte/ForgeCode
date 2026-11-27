//! Integrated terminal: the user's shell on a real PTY, rendered from a VT100 screen model.

use std::cell::Cell;
use std::io::{Read, Write};
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::scroll::{ScrollableElement as _, ScrollbarHandle};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

const SCROLLBACK: usize = 5_000;

/// A shell the user can open a terminal with.
#[derive(Clone, Debug, PartialEq)]
pub struct Shell {
    pub name: String,
    pub path: PathBuf,
}

/// Shells installed on this machine: the login shell first, then /etc/shells and common extras
/// (PowerShell, cmd and Git Bash on Windows).
pub fn available_shells() -> Vec<Shell> {
    let mut paths: Vec<PathBuf> = std::env::var("SHELL").ok().map(PathBuf::from).into_iter().collect();
    #[cfg(windows)]
    paths.extend(["pwsh", "powershell", "cmd", "bash", "nu"].iter().filter_map(|n| crate::lsp::install::which(n)));
    let listed = std::fs::read_to_string("/etc/shells").unwrap_or_default();
    paths.extend(listed.lines().map(str::trim).filter(|l| l.starts_with('/')).map(PathBuf::from));
    for dir in ["/opt/homebrew/bin", "/usr/local/bin"] {
        for name in ["fish", "nu", "pwsh", "bash", "zsh", "xonsh", "elvish"] {
            paths.push(Path::new(dir).join(name));
        }
    }
    let mut shells: Vec<Shell> = Vec::new();
    for path in paths {
        let canonical = path.canonicalize().unwrap_or(path.clone());
        if !path.is_file() || shells.iter().any(|s| s.path.canonicalize().unwrap_or(s.path.clone()) == canonical) {
            continue;
        }
        let base = path.file_stem().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        // Same name in two places (e.g. system and Homebrew bash): show where it lives.
        let name = if shells.iter().any(|s| s.name == base) { format!("{base} ({})", path.display()) } else { base };
        shells.push(Shell { name, path });
    }
    shells
}

/// Scrollbar adapter over the VT100 scrollback.
#[derive(Clone)]
struct TermScroll {
    parser: Arc<Mutex<Vt>>,
    bounds: Rc<Cell<Bounds<Pixels>>>,
    line_height: Rc<Cell<Pixels>>,
}

impl ScrollbarHandle for TermScroll {
    fn viewport_bounds(&self) -> Bounds<Pixels> {
        self.bounds.get()
    }

    fn offset(&self) -> Point<Pixels> {
        let p = self.parser.lock().unwrap();
        let s = p.screen();
        point(px(0.), -(self.line_height.get() * (s.scrollback_len() - s.scrollback()) as f32))
    }

    fn set_offset(&self, offset: Point<Pixels>) {
        let mut p = self.parser.lock().unwrap();
        let len = p.screen().scrollback_len();
        let from_top = (f32::from(-offset.y) / f32::from(self.line_height.get())).round().max(0.) as usize;
        p.screen_mut().set_scrollback(len.saturating_sub(from_top));
    }

    fn content_size(&self) -> Size<Pixels> {
        let p = self.parser.lock().unwrap();
        let rows = p.screen().size().0.get() as usize + p.screen().scrollback_len();
        size(self.bounds.get().size.width, self.line_height.get() * rows as f32)
    }
}

pub struct Terminal {
    pub shell: Shell,
    parser: Arc<Mutex<Vt>>,
    writer: Option<Arc<Mutex<Box<dyn Write + Send>>>>,
    master: Option<Box<dyn MasterPty + Send>>,
    size: (u16, u16),
    focus: FocusHandle,
    error: Option<String>,
    exited: Arc<AtomicBool>,
    scroll: TermScroll,
    /// Text selected with the mouse: (anchor, head) as (row, column) cells of the visible screen.
    selection: Option<(Abs, Abs)>,
    /// Where the last plain click landed: a later shift-click selects from there.
    anchor: Option<Abs>,
    /// Where the mouse last was while dragging (to keep scrolling when it rests past an edge).
    last_mouse: Point<Pixels>,
    _autoscroll: Option<Task<()>>,
    /// The left button is down and dragging extends the selection.
    selecting: bool,
    /// The word (2 clicks) or line (3) a multi-click selected: dragging then extends by whole units.
    unit: Option<(usize, Abs, Abs)>,
    /// Width of one character cell, as last rendered.
    char_width: Pixels,
    /// The URL under the mouse while Cmd is held: (visible row, first column, last column).
    link: Option<(u16, u16, u16)>,
    _reader: Option<Task<()>>,
    /// What was sent to the program, for tests.
    #[cfg(test)]
    pub sent: Vec<u8>,
    /// A button pressed in a program that takes the mouse: (button, last cell reported).
    reported: Option<(u8, Cell2)>,
    /// The last cell a button-less move was reported at (programs that track every move).
    hovered: Option<Cell2>,
}

/// The emulator, with the replies to programs' queries.
pub type Vt = vt100::Parser<Replies>;

/// Answers to what programs ask the terminal, which vt100 leaves to its user: without them a
/// program waits (cursor position) or guesses (the background, for its light or dark theme).
#[derive(Default)]
pub struct Replies {
    /// Bytes to send back to the program.
    pub out: Vec<u8>,
    /// The theme's foreground and background, as `rgb:rrrr/gggg/bbbb`.
    pub colors: (String, String),
}

impl vt100::Callbacks for Replies {
    fn unhandled_csi(&mut self, screen: &mut vt100::Screen, i1: Option<u8>, _: Option<u8>, params: &[&[u16]], c: char) {
        let first = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        let reply = match (i1, c, first) {
            // Cursor position report (1-based).
            (None, 'n', 6) => {
                let (row, col) = screen.cursor_position();
                format!("\x1b[{};{}R", row + 1, col + 1)
            }
            (None, 'n', 5) => "\x1b[0n".into(),
            // Primary device attributes: a VT220 with ANSI colors.
            (None, 'c', 0) => "\x1b[?62;22c".into(),
            (Some(b'>'), 'c', 0) => "\x1b[>0;10;1c".into(),
            _ => return,
        };
        self.out.extend_from_slice(reply.as_bytes());
    }

    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        // OSC 10/11 ;? : the foreground / background color.
        let color = match params {
            [b"10", b"?"] => &self.colors.0,
            [b"11", b"?"] => &self.colors.1,
            _ => return,
        };
        if !color.is_empty() {
            let which = std::str::from_utf8(params[0]).unwrap_or("11");
            self.out.extend_from_slice(format!("\x1b]{which};{color}\x1b\\").as_bytes());
        }
    }
}

/// `rgb:rrrr/gggg/bbbb`, as OSC 10/11 replies carry a color.
fn osc_rgb(color: Hsla) -> String {
    let c = color.to_rgb();
    let h = |v: f32| { let v = (v.clamp(0., 1.) * 255.).round() as u16; v << 8 | v };
    format!("rgb:{:04x}/{:04x}/{:04x}", h(c.r), h(c.g), h(c.b))
}

/// (row, column) on the visible screen.
type Cell2 = (u16, u16);

/// (line, column) in the whole history: line 0 is the oldest scrollback line, so a selection
/// stays on its text while the view scrolls.
type Abs = (usize, u16);

/// History line shown on the first visible row.
fn first_visible(screen: &vt100::Screen) -> usize {
    screen.scrollback_len().saturating_sub(screen.scrollback())
}

/// The text between two history positions (both included), reading the scrollback page by page;
/// the view is put back where it was.
pub fn history_text<CB: vt100::Callbacks>(parser: &mut vt100::Parser<CB>, a: Abs, b: Abs) -> String {
    let (start, end) = if a <= b { (a, b) } else { (b, a) };
    let shown = parser.screen().scrollback();
    let (rows, cols) = parser.screen().size();
    let (rows, last_col) = (rows.get() as usize, cols.get().saturating_sub(1));
    let mut out = String::new();
    let mut line = start.0;
    while line <= end.0 {
        // Put `line` on the first row (or as high as the history allows).
        let len = parser.screen().scrollback_len();
        parser.screen_mut().set_scrollback(len.saturating_sub(line));
        let screen = parser.screen();
        let top = first_visible(screen);
        let from_row = line - top.min(line);
        let last_row = (end.0 - top).min(rows - 1);
        if from_row > last_row {
            break;
        }
        let from = (from_row as u16, if line == start.0 { start.1 } else { 0 });
        let to = (last_row as u16, if top + last_row == end.0 { end.1 } else { last_col });
        out.push_str(&selected_text(screen, from, to));
        line = top + last_row + 1;
        if line <= end.0 && !screen.row_wrapped(last_row as u16) {
            out.push('\n');
        }
    }
    parser.screen_mut().set_scrollback(shown);
    out
}

/// The selection's cells in reading order (both ends included).
fn ordered(a: Cell2, b: Cell2) -> (Cell2, Cell2) {
    if a <= b { (a, b) } else { (b, a) }
}

/// The text of the cells from `start` to `end` (included): trailing blanks of each line dropped,
/// lines the terminal wrapped joined back together.
pub fn selected_text(screen: &vt100::Screen, start: Cell2, end: Cell2) -> String {
    let (start, end) = ordered(start, end);
    let cols = screen.size().1.get();
    let mut out = String::new();
    for r in start.0..=end.0 {
        let from = if r == start.0 { start.1 } else { 0 };
        let to = if r == end.0 { end.1.min(cols.saturating_sub(1)) } else { cols.saturating_sub(1) };
        let mut line = String::new();
        for c in from..=to {
            let Some(cell) = screen.cell(r, c) else { continue };
            if cell.is_wide_continuation() {
                continue;
            }
            line.push_str(if cell.has_contents() { cell.contents() } else { " " });
        }
        out.push_str(line.trim_end());
        if r != end.0 && !screen.row_wrapped(r) {
            out.push('\n');
        }
    }
    out
}

/// The word around `at` (letters, digits and what paths and URLs are made of).
/// The http(s) URL covering column `at` of a row of cells: (first column, last column, url).
pub fn url_at(row: &[char], at: usize) -> Option<(usize, usize, String)> {
    let stop = |c: &char| c.is_whitespace() || "\"'<>`|".contains(*c);
    let mut from = at;
    while from > 0 && !stop(row.get(from - 1)?) {
        from -= 1;
    }
    let mut to = from;
    while to < row.len() && !stop(&row[to]) {
        to += 1;
    }
    let token: String = row[from..to].iter().collect();
    let start = token.find("https://").or_else(|| token.find("http://"))?;
    // Punctuation closing a sentence or a bracket is not part of the link.
    let url = token[start..].trim_end_matches(|c| ".,;:!?)]}".contains(c));
    let (first, last) = (from + token[..start].chars().count(), from + token[..start].chars().count() + url.chars().count() - 1);
    (at >= first && at <= last && url.len() > "https://".len()).then(|| (first, last, url.to_string()))
}

pub fn word_cells(screen: &vt100::Screen, at: Cell2) -> (Cell2, Cell2) {
    let cols = screen.size().1.get();
    let is_word = |c: u16| {
        screen.cell(at.0, c).is_some_and(|cell| cell.contents().chars().next().is_some_and(|ch| ch.is_alphanumeric() || "_-./~:@%+=?&#".contains(ch)))
    };
    if !is_word(at.1) {
        return (at, at);
    }
    let (mut from, mut to) = (at.1, at.1);
    while from > 0 && is_word(from - 1) {
        from -= 1;
    }
    while to + 1 < cols && is_word(to + 1) {
        to += 1;
    }
    ((at.0, from), (at.0, to))
}

// Tab must reach the shell (completion): gpui's root binds it to "focus next".
actions!(terminal, [SendTab, SendBackTab]);

pub fn key_bindings() -> Vec<KeyBinding> {
    vec![KeyBinding::new("tab", SendTab, Some("Terminal")), KeyBinding::new("shift-tab", SendBackTab, Some("Terminal"))]
}

impl Terminal {
    pub fn new(cwd: &Path, shell: Shell, cx: &mut Context<Self>) -> Self {
        let size = (24, 80);
        let parser = Arc::new(Mutex::new(vt100::Parser::new_with_callbacks(nz(size.0), nz(size.1), SCROLLBACK, Replies::default())));
        let scroll = TermScroll { parser: parser.clone(), bounds: Default::default(), line_height: Rc::new(Cell::new(px(18.))) };
        let mut this = Self {
            shell,
            parser,
            writer: None,
            master: None,
            size,
            focus: cx.focus_handle(),
            error: None,
            exited: Default::default(),
            scroll,
            selection: None,
            anchor: None,
            last_mouse: Point::default(),
            _autoscroll: None,
            selecting: false,
            unit: None,
            char_width: px(8.),
            link: None,
            _reader: None,
            #[cfg(test)]
            sent: vec![],
            reported: None,
            hovered: None,
        };
        // Tests show a terminal without a shell (its threads would wake the test scheduler).
        if !cwd.as_os_str().is_empty() {
            if let Err(e) = this.spawn_shell(cwd, cx) {
                this.error = Some(format!("Failed to start shell: {e:#}"));
            }
        }
        this
    }

    /// (origin of the view, line height, character width) as last rendered.
    #[cfg(test)]
    pub fn geometry(&self) -> (Point<Pixels>, Pixels, Pixels) {
        (self.scroll.bounds.get().origin, self.scroll.line_height.get(), self.char_width)
    }

    #[cfg(test)]
    pub fn rows(&self) -> u16 {
        self.size.0
    }

    #[cfg(test)]
    pub fn scrollback(&self) -> usize {
        self.parser.lock().unwrap().screen().scrollback()
    }

    /// Scroll back by `lines` (as the mouse wheel does).
    #[cfg(test)]
    pub fn scroll_back(&mut self, lines: usize) {
        let mut p = self.parser.lock().unwrap();
        let shown = p.screen().scrollback();
        p.screen_mut().set_scrollback(shown + lines);
    }

    /// What the terminal answers to `bytes` printed by a program.
    #[cfg(test)]
    pub fn replies_to(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut p = self.parser.lock().unwrap();
        p.process(bytes);
        std::mem::take(&mut p.callbacks_mut().out)
    }

    /// Feed bytes as if the shell had printed them.
    #[cfg(test)]
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.lock().unwrap().process(bytes);
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    fn spawn_shell(&mut self, cwd: &Path, cx: &mut Context<Self>) -> anyhow::Result<()> {
        let pair = native_pty_system().openpty(PtySize { rows: self.size.0, cols: self.size.1, pixel_width: 0, pixel_height: 0 })?;
        let mut cmd = CommandBuilder::new(&self.shell.path);
        // Login shell where the convention exists (sh-like shells and fish).
        if ["zsh", "bash", "sh", "fish", "ksh", "dash"].iter().any(|n| self.shell.name.starts_with(n)) {
            cmd.arg("-l");
        }
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        // Name this terminal: started from iTerm or VS Code, ForgeCode would pass theirs on, and
        // programs (Claude Code…) would expect their key handling and features.
        cmd.env("TERM_PROGRAM", "ForgeCode");
        cmd.env("COLORTERM", "truecolor");
        for inherited in ["TERM_PROGRAM_VERSION", "ITERM_SESSION_ID", "ITERM_PROFILE", "LC_TERMINAL", "LC_TERMINAL_VERSION", "VSCODE_INJECTION", "VSCODE_GIT_IPC_HANDLE", "KITTY_WINDOW_ID", "WEZTERM_PANE", "TMUX", "TMUX_PANE"] {
            cmd.env_remove(inherited);
        }
        // The project's Python virtualenv is active in its terminals, like VS Code.
        for (key, value) in crate::language::venv_env(cwd) {
            cmd.env(key, value);
        }
        let _child = pair.slave.spawn_command(cmd)?;
        let mut reader = pair.master.try_clone_reader()?;
        let writer = Arc::new(Mutex::new(pair.master.take_writer()?));
        self.writer = Some(writer.clone());
        self.master = Some(pair.master);

        let (tx, rx) = smol::channel::unbounded::<()>();
        let parser = self.parser.clone();
        let exited = self.exited.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                // Answered at once: programs wait for these replies (cursor position, colors…).
                let replies = {
                    let mut p = parser.lock().unwrap();
                    p.process(&buf[..n]);
                    std::mem::take(&mut p.callbacks_mut().out)
                };
                if !replies.is_empty() {
                    let mut w = writer.lock().unwrap();
                    let _ = w.write_all(&replies);
                    let _ = w.flush();
                }
                let _ = tx.try_send(());
            }
            exited.store(true, Ordering::Relaxed);
            let _ = tx.try_send(());
        });
        self._reader = Some(cx.spawn(async move |this, cx| {
            while rx.recv().await.is_ok() {
                while rx.try_recv().is_ok() {} // coalesce bursts into one repaint
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        }));
        Ok(())
    }

    pub fn has_exited(&self) -> bool {
        self.exited.load(Ordering::Relaxed)
    }

    pub fn write(&mut self, bytes: &[u8]) {
        #[cfg(test)]
        self.sent.extend_from_slice(bytes);
        if let Some(w) = self.writer.as_ref() {
            let mut w = w.lock().unwrap();
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        if (rows, cols) == self.size || rows < 2 || cols < 10 {
            return;
        }
        self.size = (rows, cols);
        self.parser.lock().unwrap().screen_mut().set_size(nz(rows), nz(cols));
        if let Some(m) = &self.master {
            let _ = m.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
        }
    }

    /// The cell under a window position.
    fn cell_at(&self, position: Point<Pixels>) -> Cell2 {
        let bounds = self.scroll.bounds.get();
        let pad = px(4.);
        let (rows, cols) = self.size;
        let row = (f32::from(position.y - bounds.origin.y - pad) / f32::from(self.scroll.line_height.get())).floor();
        let col = (f32::from(position.x - bounds.origin.x - pad) / f32::from(self.char_width)).floor();
        (row.clamp(0., rows.saturating_sub(1) as f32) as u16, col.clamp(0., cols.saturating_sub(1) as f32) as u16)
    }

    /// The history position of a visible cell.
    fn abs(&self, cell: Cell2) -> Abs {
        (first_visible(self.parser.lock().unwrap().screen()) + cell.0 as usize, cell.1)
    }

    /// The URL at a visible cell, read from the screen.
    fn url_at_cell(&self, at: Cell2) -> Option<(usize, usize, String)> {
        let parser = self.parser.lock().unwrap();
        let screen = parser.screen();
        let row: Vec<char> = (0..screen.size().1.get())
            .map(|c| screen.cell(at.0, c).and_then(|cell| cell.contents().chars().next()).unwrap_or(' '))
            .collect();
        url_at(&row, at.1 as usize)
    }

    /// Cmd held over a URL: it is underlined and the cursor becomes a hand.
    fn hover(&mut self, position: Point<Pixels>, cmd: bool, cx: &mut Context<Self>) {
        let bounds = self.scroll.bounds.get();
        let link = (cmd && bounds.contains(&position))
            .then(|| self.cell_at(position))
            .and_then(|at| self.url_at_cell(at).map(|(a, b, _)| (at.0, a as u16, b as u16)));
        if link != self.link {
            self.link = link;
            cx.notify();
        }
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
        let at = self.cell_at(event.position);
        if event.modifiers.platform {
            if let Some((_, _, url)) = self.url_at_cell(at) {
                cx.open_url(&url);
                return;
            }
        }
        if self.press_to_program(0, event) {
            return cx.notify();
        }
        let unit = (event.click_count >= 2).then(|| {
            let (a, b) = self.unit_at(at, event.click_count);
            (event.click_count, a, b)
        });
        self.unit = unit;
        self.selection = match unit {
            Some((_, a, b)) => Some((a, b)),
            // Shift-click extends from where the selection started, even after scrolling.
            _ if event.modifiers.shift => Some((self.selection.map(|(anchor, _)| anchor).or(self.anchor).unwrap_or(self.abs(at)), self.abs(at))),
            _ => {
                self.anchor = Some(self.abs(at));
                Some((self.abs(at), self.abs(at)))
            }
        };
        self.selecting = !event.modifiers.shift || event.click_count >= 2;
        self.last_mouse = event.position;
        // While the button is held past the top or bottom edge, keep scrolling and selecting.
        self._autoscroll = self.selecting.then(|| {
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(std::time::Duration::from_millis(50)).await;
                    let going = this.update(cx, |this, cx| {
                        if this.selecting && this.outside(this.last_mouse) {
                            this.drag_to(this.last_mouse, cx);
                        }
                        this.selecting
                    });
                    if !going.unwrap_or(false) {
                        break;
                    }
                }
            })
        });
        cx.notify();
    }

    /// Scroll back (`lines` > 0) or forward. A full-screen program (Claude Code, vim, less…) scrolls
    /// itself: it gets wheel events when it asked for the mouse, else arrow keys on its alternate
    /// screen; otherwise the view moves through the scrollback.
    fn scroll_lines(&mut self, lines: i64, at: Cell2) {
        if lines == 0 {
            return;
        }
        let bytes = {
            let mut p = self.parser.lock().unwrap();
            let screen = p.screen();
            if screen.mouse_protocol_mode() != vt100::MouseProtocolMode::None {
                Some(wheel_bytes(lines > 0, at, screen.mouse_protocol_encoding()).repeat(lines.unsigned_abs() as usize))
            } else if screen.alternate_screen() {
                let arrow = key_bytes(if lines > 0 { "up" } else { "down" }, None, false, false, screen.application_cursor()).unwrap_or_default();
                Some(arrow.repeat(lines.unsigned_abs() as usize))
            } else {
                let current = screen.scrollback() as i64;
                p.screen_mut().set_scrollback((current + lines).max(0) as usize);
                None
            }
        };
        if let Some(bytes) = bytes {
            self.write(&bytes);
        }
    }

    /// The mouse mode and encoding a full-screen program asked for, if it takes the mouse.
    fn mouse_mode(&self) -> Option<(vt100::MouseProtocolMode, vt100::MouseProtocolEncoding)> {
        let p = self.parser.lock().unwrap();
        let mode = p.screen().mouse_protocol_mode();
        (mode != vt100::MouseProtocolMode::None).then(|| (mode, p.screen().mouse_protocol_encoding()))
    }

    fn report_mouse(&mut self, button: u8, release: bool, at: Cell2, modifiers: Modifiers) {
        let Some((_, encoding)) = self.mouse_mode() else { return };
        let mods = if modifiers.alt { 8 } else { 0 } | if modifiers.control { 16 } else { 0 };
        self.write(&mouse_bytes(button | mods, release, at, encoding));
    }

    /// A press in a program that takes the mouse goes to it (Shift keeps ForgeCode's own
    /// selection, as in other terminals). True when it was sent.
    fn press_to_program(&mut self, button: u8, event: &MouseDownEvent) -> bool {
        if event.modifiers.shift || self.mouse_mode().is_none() {
            return false;
        }
        let at = self.cell_at(event.position);
        self.selection = None;
        self.report_mouse(button, false, at, event.modifiers);
        self.reported = Some((button, at));
        true
    }

    /// The mouse moved: a drag (or any move, if asked) is reported to the program.
    fn move_to_program(&mut self, event: &MouseMoveEvent) {
        let Some((mode, _)) = self.mouse_mode() else { return };
        let at = self.cell_at(event.position);
        use vt100::MouseProtocolMode::{AnyMotion, ButtonMotion};
        match self.reported {
            Some((button, last)) if last != at && matches!(mode, ButtonMotion | AnyMotion) => {
                self.report_mouse(button + 32, false, at, event.modifiers);
                self.reported = Some((button, at));
            }
            None if mode == AnyMotion && event.pressed_button.is_none() && self.hovered != Some(at) && self.scroll.bounds.get().contains(&event.position) => {
                self.report_mouse(3 + 32, false, at, event.modifiers);
                self.hovered = Some(at);
            }
            _ => {}
        }
    }

    fn release_to_program(&mut self, event: &MouseUpEvent) {
        if let Some((button, _)) = self.reported.take()
            && self.mouse_mode().is_some_and(|(mode, _)| mode != vt100::MouseProtocolMode::Press)
        {
            self.report_mouse(button, true, self.cell_at(event.position), event.modifiers);
        }
    }

    /// Whether `position` is above or below the view.
    fn outside(&self, position: Point<Pixels>) -> bool {
        let bounds = self.scroll.bounds.get();
        position.y < bounds.top() || position.y > bounds.bottom()
    }

    /// The mouse moved (anywhere in the window) with the button down: extend the selection to
    /// it, scrolling one line when it is past the top or bottom edge.
    /// The history positions of the word (2 clicks) or line (3 or more) at a visible cell.
    fn unit_at(&self, at: Cell2, clicks: usize) -> (Abs, Abs) {
        let (word, cols) = {
            let parser = self.parser.lock().unwrap();
            (word_cells(parser.screen(), at), parser.screen().size().1.get())
        };
        if clicks >= 3 { (self.abs((at.0, 0)), self.abs((at.0, cols.saturating_sub(1)))) } else { (self.abs(word.0), self.abs(word.1)) }
    }

    fn drag_to(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        if !self.selecting {
            return;
        }
        self.last_mouse = position;
        let bounds = self.scroll.bounds.get();
        if self.outside(position) {
            let cell = self.cell_at(position);
            self.scroll_lines(if position.y < bounds.top() { 1 } else { -1 }, cell);
        }
        let cell = self.cell_at(position);
        if let Some((clicks, start, end)) = self.unit {
            // By whole words or lines, keeping the first one selected whichever way it goes.
            let (a, b) = self.unit_at(cell, clicks);
            self.selection = Some(if a >= start { (start, b) } else { (end, a) });
        } else {
            let at = self.abs(cell);
            if let Some((_, head)) = &mut self.selection {
                *head = at;
            }
        }
        cx.notify();
    }

    fn end_drag(&mut self, cx: &mut Context<Self>) {
        if self.selecting {
            self.selecting = false;
            self._autoscroll = None;
            // A plain click selects nothing.
            if self.unit.is_none() && self.selection.is_some_and(|(a, b)| a == b) {
                self.selection = None;
            }
            cx.notify();
        }
    }

    /// The selected text, if any.
    pub fn selected(&self) -> Option<String> {
        let (a, b) = self.selection?;
        let text = history_text(&mut self.parser.lock().unwrap(), a, b);
        (!text.is_empty()).then_some(text)
    }

    fn copy_selection(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(text) = self.selected() else { return false };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        true
    }

    fn on_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let ks = &event.keystroke;
        // Copy: ⌘C on macOS; Ctrl+Shift+C elsewhere (Ctrl+C alone stays the interrupt signal).
        let copy = ks.key == "c" && if cfg!(target_os = "macos") { ks.modifiers.platform } else { ks.modifiers.control && ks.modifiers.shift };
        if copy && self.copy_selection(cx) {
            cx.stop_propagation();
            return;
        }
        // Ctrl is for the shell outside macOS, so paste is Ctrl+Shift+V there, as in other terminals.
        let paste = ks.key == "v" && if cfg!(target_os = "macos") { ks.modifiers.platform } else { ks.modifiers.control && ks.modifiers.shift };
        if let Some(bytes) = edit_shortcut(&ks.key, ks.modifiers.platform, ks.modifiers.alt && !ks.modifiers.control) {
            self.parser.lock().unwrap().screen_mut().set_scrollback(0);
            self.selection = None;
            self.write(bytes);
            cx.stop_propagation();
            return;
        }
        if ks.modifiers.platform || paste {
            if paste {
                match cx.read_from_clipboard().and_then(|c| c.text()) {
                    Some(text) => {
                        let bracketed = self.parser.lock().unwrap().screen().bracketed_paste();
                        self.write(&paste_bytes(&text, bracketed));
                    }
                    // An image: programs like Claude Code read it from the clipboard on Ctrl+V.
                    None => self.write(b"\x16"),
                }
                cx.stop_propagation();
            }
            return; // leave cmd-shortcuts to the app
        }
        let app_cursor = self.parser.lock().unwrap().screen().application_cursor();
        // Shift+Enter: a new line in TUIs (Claude Code…), sent as Option+Enter is.
        let shift_enter = ks.key == "enter" && ks.modifiers.shift && !ks.modifiers.control;
        let special = special_key(&ks.key, ks.modifiers.shift, ks.modifiers.alt, ks.modifiers.control);
        if let Some(bytes) = if shift_enter { Some(b"\x1b\r".to_vec()) } else if special.is_some() { special } else { key_bytes(&ks.key, ks.key_char.as_deref(), ks.modifiers.control, ks.modifiers.alt, app_cursor) } {
            self.parser.lock().unwrap().screen_mut().set_scrollback(0);
            // Typing moves the text: the selection no longer points at what was selected.
            self.selection = None;
            self.write(&bytes);
            cx.stop_propagation();
        }
    }
}

/// A wheel notch at a cell, in the mouse encoding the program asked for.
pub fn wheel_bytes(up: bool, at: Cell2, encoding: vt100::MouseProtocolEncoding) -> Vec<u8> {
    mouse_bytes(if up { 64 } else { 65 }, false, at, encoding)
}

/// A mouse report: `button` 0 left, 1 middle, 2 right (+32 for a move, +4/8/16 for
/// shift/alt/ctrl, 64/65 the wheel); `release` ends a press.
pub fn mouse_bytes(button: u8, release: bool, (row, col): Cell2, encoding: vt100::MouseProtocolEncoding) -> Vec<u8> {
    let (x, y) = (col as u32 + 1, row as u32 + 1);
    // Outside SGR a release doesn't say which button: it is "3".
    let legacy = if release { (button & !3) | 3 } else { button } as u32;
    match encoding {
        vt100::MouseProtocolEncoding::Sgr => format!("\x1b[<{button};{x};{y}{}", if release { 'm' } else { 'M' }).into_bytes(),
        vt100::MouseProtocolEncoding::Utf8 => {
            let c = |v: u32| char::from_u32(32 + v).unwrap_or(' ');
            format!("\x1b[M{}{}{}", c(legacy), c(x), c(y)).into_bytes()
        }
        _ => [b"\x1b[M".as_slice(), &[(32 + legacy).min(255) as u8, (32 + x).min(255) as u8, (32 + y).min(255) as u8]].concat(),
    }
}

/// Pasted text, marked as such when the program asked (it then won't run pasted lines).
pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed { format!("\x1b[200~{text}\x1b[201~").into_bytes() } else { text.as_bytes().to_vec() }
}

fn nz(v: u16) -> NonZeroU16 {
    NonZeroU16::new(v.max(1)).unwrap()
}

/// The macOS text-moving shortcuts, as the shell's line editor understands them: Option+arrows
/// by word, Cmd+arrows to the start or end of the line, Cmd/Option+Delete to delete back.
pub fn edit_shortcut(key: &str, cmd: bool, alt: bool) -> Option<&'static [u8]> {
    Some(match (key, cmd, alt) {
        ("left", false, true) => b"\x1bb",
        ("right", false, true) => b"\x1bf",
        ("left", true, false) => b"\x01",
        ("right", true, false) => b"\x05",
        ("backspace", true, false) => b"\x15",
        ("delete", false, true) => b"\x1bd",
        ("delete", true, false) => b"\x0b",
        _ => return None,
    })
}

/// Function keys, and navigation keys held with Shift/Ctrl (Alt too, except the arrows Option
/// already moves by word with), in xterm's encoding: `CSI 1;<mods> A`, `CSI 15;<mods> ~`…
pub fn special_key(key: &str, shift: bool, alt: bool, ctrl: bool) -> Option<Vec<u8>> {
    let m = 1 + shift as u8 + 2 * alt as u8 + 4 * ctrl as u8;
    let tilde = |code: u8| if m > 1 { format!("\x1b[{code};{m}~") } else { format!("\x1b[{code}~") };
    let letter = |l: char, ss3: bool| if m > 1 { format!("\x1b[1;{m}{l}") } else if ss3 { format!("\x1bO{l}") } else { format!("\x1b[{l}") };
    let s = match key {
        "f1" => letter('P', true),
        "f2" => letter('Q', true),
        "f3" => letter('R', true),
        "f4" => letter('S', true),
        "f5" => tilde(15),
        "f6" => tilde(17),
        "f7" => tilde(18),
        "f8" => tilde(19),
        "f9" => tilde(20),
        "f10" => tilde(21),
        "f11" => tilde(23),
        "f12" => tilde(24),
        _ if m == 1 => return None,
        "up" | "down" | "right" | "left" | "home" | "end" => {
            let l = match key { "up" => 'A', "down" => 'B', "right" => 'C', "left" => 'D', "home" => 'H', _ => 'F' };
            letter(l, false)
        }
        "pageup" => tilde(5),
        "pagedown" => tilde(6),
        "delete" => tilde(3),
        "insert" => tilde(2),
        _ => return None,
    };
    Some(s.into_bytes())
}

/// Bytes a key press sends to the PTY.
/// Whether Ctrl+Alt+`key` typed `c` through AltGr: a printable character other than the key's own letter.
fn altgr_typed(key: &str, c: &str) -> bool {
    !c.is_empty() && !c.chars().any(char::is_control) && (!c.eq_ignore_ascii_case(key) || !key.chars().all(|k| k.is_ascii_alphabetic()))
}

pub fn key_bytes(key: &str, key_char: Option<&str>, ctrl: bool, alt: bool, app_cursor: bool) -> Option<Vec<u8>> {
    let arrow = |c: char| Some(if app_cursor { format!("\x1bO{c}") } else { format!("\x1b[{c}") }.into_bytes());
    let bytes = match key {
        "enter" => Some(b"\r".to_vec()),
        "backspace" => Some(vec![0x7f]),
        "tab" => Some(b"\t".to_vec()),
        "escape" => Some(vec![0x1b]),
        "up" => arrow('A'),
        "down" => arrow('B'),
        "right" => arrow('C'),
        "left" => arrow('D'),
        "home" => Some(b"\x1b[H".to_vec()),
        "end" => Some(b"\x1b[F".to_vec()),
        "delete" => Some(b"\x1b[3~".to_vec()),
        "pageup" => Some(b"\x1b[5~".to_vec()),
        "pagedown" => Some(b"\x1b[6~".to_vec()),
        // AltGr (Ctrl+Alt on Windows): the character it types (`\`, `@`, `€`…), not a control code.
        k if ctrl && alt && key_char.is_some_and(|c| altgr_typed(k, c)) => {
            return key_char.map(|s| s.as_bytes().to_vec());
        }
        k if ctrl && k.len() == 1 => {
            let c = k.as_bytes()[0].to_ascii_lowercase();
            c.is_ascii_lowercase().then(|| vec![c - b'a' + 1])
        }
        "space" => Some(b" ".to_vec()),
        _ => key_char.map(|s| s.as_bytes().to_vec()),
    }?;
    Some(if alt && !ctrl { [vec![0x1b], bytes].concat() } else { bytes })
}

fn ansi_color(color: vt100::Color, default: Hsla) -> Hsla {
    const BASE: [(u8, u8, u8); 16] = [
        (0, 0, 0), (205, 49, 49), (13, 188, 121), (229, 229, 16), (36, 114, 200), (188, 63, 188), (17, 168, 205), (229, 229, 229),
        (102, 102, 102), (241, 76, 76), (35, 209, 139), (245, 245, 67), (59, 142, 234), (214, 112, 214), (41, 184, 219), (255, 255, 255),
    ];
    let (r, g, b) = match color {
        vt100::Color::Default => return default,
        vt100::Color::Rgb(r, g, b) => (r, g, b),
        vt100::Color::Idx(i) if i < 16 => BASE[i as usize],
        vt100::Color::Idx(i) if i < 232 => {
            let i = i - 16;
            let step = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            (step(i / 36), step((i / 6) % 6), step(i % 6))
        }
        vt100::Color::Idx(i) => {
            let v = 8 + (i - 232) * 10;
            (v, v, v)
        }
    };
    Rgba { r: r as f32 / 255., g: g as f32 / 255., b: b as f32 / 255., a: 1. }.into()
}

impl Render for Terminal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (fg, bg) = (theme.foreground, theme.background);
        let font_size = theme.mono_font_size;
        let line_height = font_size * 1.35;
        let char_width = font_size * 0.6;
        self.char_width = char_width;
        let focused = self.focus.is_focused(window);
        let selection = self.selection.filter(|(a, b)| a != b || !self.selecting).map(|(a, b)| if a <= b { (a, b) } else { (b, a) });
        let selection_bg = theme.selection;

        let mut parser = self.parser.lock().unwrap();
        parser.callbacks_mut().colors = (osc_rgb(fg), osc_rgb(bg));
        let screen = parser.screen();
        let top = first_visible(screen);
        let (rows, cols) = screen.size();
        let (cur_row, cur_col) = screen.cursor_position();
        let show_cursor = focused && !screen.hide_cursor() && screen.scrollback() == 0;
        let mut lines = Vec::with_capacity(rows.get() as usize);
        for r in 0..rows.get() {
            let mut text = String::new();
            let mut highlights = Vec::new();
            for c in 0..cols.get() {
                let Some(cell) = screen.cell(r, c) else { continue };
                if cell.is_wide_continuation() {
                    continue;
                }
                let start = text.len();
                text.push_str(if cell.has_contents() { cell.contents() } else { " " });
                let (mut cfg, mut cbg) = (ansi_color(cell.fgcolor(), fg), ansi_color(cell.bgcolor(), bg));
                // Dim (faint) text: Claude Code's hints, shell suggestions…
                if cell.dim() {
                    cfg = cfg.opacity(0.6);
                }
                if cell.inverse() || (show_cursor && r == cur_row && c == cur_col) {
                    std::mem::swap(&mut cfg, &mut cbg);
                }
                if selection.is_some_and(|(from, to)| (top + r as usize, c) >= from && (top + r as usize, c) <= to) {
                    cbg = selection_bg;
                }
                let linked = self.link.is_some_and(|(lr, a, b)| lr == r && c >= a && c <= b);
                let style = HighlightStyle {
                    underline: (linked || cell.underline()).then(|| UnderlineStyle { thickness: px(1.), color: Some(cfg), wavy: false }),
                    font_style: cell.italic().then_some(FontStyle::Italic),
                    color: (cfg != fg).then_some(cfg),
                    background_color: (cbg != bg).then_some(cbg),
                    font_weight: cell.bold().then_some(FontWeight::BOLD),
                    ..Default::default()
                };
                if style != HighlightStyle::default() {
                    highlights.push((start..text.len(), style));
                }
            }
            lines.push(div().h(line_height).whitespace_nowrap().child(StyledText::new(text).with_highlights(highlights)));
        }
        drop(parser);

        let view = cx.entity().downgrade();
        let (drag_view, up_view) = (view.clone(), view.clone());
        let scroll = self.scroll.clone();
        div()
            .id("terminal")
            .track_focus(&self.focus)
            .key_context("Terminal")
            .on_key_down(cx.listener(Self::on_key))
            .on_action(cx.listener(|this, _: &SendTab, _, _| this.write(b"\t")))
            .on_action(cx.listener(|this, _: &SendBackTab, _, _| this.write(b"\x1b[Z")))
            .cursor(if self.link.is_some() { CursorStyle::PointingHand } else { CursorStyle::IBeam })
            .on_modifiers_changed(cx.listener(|this, e: &ModifiersChangedEvent, window, cx| {
                this.hover(window.mouse_position(), e.modifiers.platform, cx)
            }))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(|this, e: &MouseDownEvent, _, _| { this.press_to_program(2, e); }))
            .on_mouse_down(MouseButton::Middle, cx.listener(|this, e: &MouseDownEvent, _, _| { this.press_to_program(1, e); }))
            .on_scroll(cx.listener(|this, ev: &ScrollEvent, window, cx| {
                let dy = ev.delta.pixel_delta(window.line_height()).y;
                let lines = (f32::from(dy) / f32::from(this.scroll.line_height.get())).round() as i64;
                let cell = this.cell_at(ev.position);
                this.scroll_lines(lines, cell);
                // The selection stays on its text; while dragging, it follows to the mouse.
                if this.selecting {
                    this.drag_to(this.last_mouse, cx);
                }
                cx.notify();
            }))
            .relative()
            .size_full()
            .overflow_hidden()
            .p_1()
            .bg(bg)
            .text_color(fg)
            .font_family(cx.theme().mono_font_family.clone())
            .text_size(font_size)
            .line_height(line_height)
            .children(lines)
            .when_some(self.error.clone(), |el, e| el.child(e))
            .when(self.has_exited(), |el| el.child(div().text_color(cx.theme().muted_foreground).child("[process exited]")))
            .child(
                // Measures the panel to keep the PTY size in sync with the view.
                canvas(
                    move |bounds, _, cx| {
                        scroll.bounds.set(bounds);
                        scroll.line_height.set(line_height);
                        let rows = (f32::from(bounds.size.height) / f32::from(line_height)).floor() as u16;
                        let cols = (f32::from(bounds.size.width) / f32::from(char_width)).floor() as u16;
                        let _ = view.update(cx, |t, _| t.resize(rows, cols));
                    },
                    // The drag is followed over the whole window, not only over the terminal: the
                    // mouse may leave it (that is how it scrolls while selecting).
                    move |_, _, window, _| {
                        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                            if phase == DispatchPhase::Bubble {
                                let _ = drag_view.update(cx, |t, cx| {
                                    t.hover(event.position, event.modifiers.platform, cx);
                                    t.move_to_program(event);
                                    if event.pressed_button == Some(MouseButton::Left) {
                                        t.drag_to(event.position, cx);
                                    } else {
                                        t.end_drag(cx);
                                    }
                                });
                            }
                        });
                        window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                            if phase == DispatchPhase::Bubble {
                                let _ = up_view.update(cx, |t, cx| {
                                    t.release_to_program(event);
                                    if event.button == MouseButton::Left {
                                        t.end_drag(cx);
                                    }
                                });
                            }
                        });
                    },
                )
                // Pinned to the view's corner: without it, it sits below the text lines and the
                // measured origin (used to find the cell under the mouse) is off by their height.
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
            .vertical_scrollbar(&self.scroll)
    }
}

#[cfg(test)]
mod tests {
    /// Tab reaches the shell (completion) instead of gpui's root "focus next" binding.
    #[test]
    fn tab_goes_to_the_shell() {
        use gpui_kit::{KeyBinding, KeyContext, Keymap, Keystroke, actions};
        actions!(test_root, [FocusNext]);
        let mut keymap = Keymap::default();
        // What gpui-component's root binds (root.rs).
        keymap.add_bindings([KeyBinding::new("tab", FocusNext, Some("Root"))]);
        keymap.add_bindings(super::key_bindings());
        let stack = [KeyContext::parse("Root").unwrap(), KeyContext::parse("Terminal").unwrap()];
        let (bindings, _) = keymap.bindings_for_input(&[Keystroke::parse("tab").unwrap()], &stack);
        assert!(bindings.first().is_some_and(|b| b.action().partial_eq(&super::SendTab)));
    }

    use super::key_bytes; // not `*`: gpui exports its own `test` macro

    #[test]
    fn finds_the_url_under_the_mouse() {
        let row: Vec<char> = "see (https://example.com/a?b=1). ok".chars().collect();
        let (a, b, url) = super::url_at(&row, 10).unwrap();
        assert_eq!(url, "https://example.com/a?b=1");
        assert_eq!((a, b), (5, 29));
        assert!(super::url_at(&row, 33).is_none());
        assert!(super::url_at(&row, 2).is_none());
    }

    #[test]
    fn selection_reads_through_the_scrollback() {
        use super::{first_visible, history_text};
        let mut parser = vt100::Parser::new(super::nz(3), super::nz(10), 100);
        let text: String = (0..9).map(|i| format!("line {i}\r\n")).collect();
        parser.process(text.trim_end().as_bytes());
        // 9 lines on 3 rows: 6 in the scrollback, the view on lines 6..8.
        assert_eq!(first_visible(parser.screen()), 6);
        assert_eq!(history_text(&mut parser, (1, 5), (7, 3)), "1\nline 2\nline 3\nline 4\nline 5\nline 6\nline");
        assert_eq!(history_text(&mut parser, (8, 5), (7, 0)), "line 7\nline 8", "backwards drag");
        assert_eq!(parser.screen().scrollback(), 0, "the view is put back");
        parser.screen_mut().set_scrollback(4);
        assert_eq!(history_text(&mut parser, (0, 0), (0, 5)), "line 0");
        assert_eq!(parser.screen().scrollback(), 4);
    }

    #[test]
    fn selects_text_across_lines_and_words() {
        use super::{selected_text, word_cells};
        let mut parser = vt100::Parser::new(super::nz(4), super::nz(10), 0);
        // The second line is longer than the 10 columns: the terminal wraps it.
        parser.process(b"ls -la\r\ncd ~/web/PROJ531\r\nok");
        let screen = parser.screen();
        assert_eq!(selected_text(screen, (0, 0), (0, 9)), "ls -la", "trailing blanks dropped");
        assert_eq!(selected_text(screen, (0, 3), (1, 1)), "-la\ncd");
        assert_eq!(selected_text(screen, (2, 9), (1, 0)), "cd ~/web/PROJ531", "a wrapped line is one line, either drag direction");
        assert_eq!(word_cells(screen, (1, 5)), ((1, 3), (1, 9)), "a path is one word");
        assert_eq!(word_cells(screen, (0, 2)), ((0, 2), (0, 2)), "a blank selects itself");
    }

    #[test]
    fn maps_keys_to_pty_bytes() {
        assert_eq!(key_bytes("c", Some("c"), true, false, false), Some(vec![3]));
        assert_eq!(key_bytes("up", None, false, false, true), Some(b"\x1bOA".to_vec()));
        assert_eq!(key_bytes("a", Some("a"), false, false, false), Some(b"a".to_vec()));
        assert_eq!(key_bytes("b", Some("b"), false, true, false), Some(b"\x1bb".to_vec()));
        assert_eq!(key_bytes("shift", None, false, false, false), None);
        // AltGr on Windows arrives as Ctrl+Alt: AZERTY AltGr+8 types `\`, AltGr+E `€`.
        assert_eq!(key_bytes("8", Some("\\"), true, true, false), Some(b"\\".to_vec()));
        assert_eq!(key_bytes("\\", Some("\\"), true, true, false), Some(b"\\".to_vec()));
        assert_eq!(key_bytes("e", Some("€"), true, true, false), Some("€".as_bytes().to_vec()));
        assert_eq!(key_bytes("a", Some("a"), true, true, false), Some(vec![1]), "Ctrl+Alt+A stays a control key");
        assert_eq!(super::edit_shortcut("left", false, true), Some(&b"\x1bb"[..]));
        assert_eq!(super::edit_shortcut("right", true, false), Some(&b"\x05"[..]));
        assert_eq!(super::edit_shortcut("backspace", true, false), Some(&b"\x15"[..]));
        assert_eq!(super::edit_shortcut("c", true, false), None);
    }
}
