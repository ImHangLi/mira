//! PTY attach: show a managed program's virtual screen and, while this TUI holds the
//! input lock, forward keys and paste to it.
//!
//! One attach opens its own control connection (the input lock belongs to it, so closing it
//! always releases the lock) and its own stream connection subscribed to `terminal` events
//! for the run. While attached as the writer every key goes to the program, Esc and Ctrl-C
//! included; only Ctrl-] returns to Mira. When another client holds the lock the screen is
//! shown read-only. Scrollback is not part of this view: history is the run's log panel in
//! Mira, and scrolling it never sends anything to the program.

use std::collections::HashSet;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use mira_client::{Client, connect};
use mira_protocol::error::{ErrorCode, ErrorInfo};
use mira_protocol::ids::{ActionRef, RunId, ScreenRevision};
use mira_protocol::ipc::*;
use mira_protocol::limits::{MAX_REPLY_BUDGET_BYTES, MAX_TERMINAL_COLS, MAX_TERMINAL_ROWS};
use mira_protocol::manifest::TerminalMode;
use mira_protocol::paths::WorkspacePaths;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use unicode_width::UnicodeWidthChar;

use crate::app::{Binding, Cmd, bind};
use crate::ipc::{self, Event, Failure, Tx};

/// Without stream events (stream lost), the screen is polled this often.
const POLL: Duration = Duration::from_millis(250);
/// A screen larger than one reply is read in row pages that must share one revision.
const PAGE_TRIES: usize = 3;

pub enum Command {
    Input(TerminalInput),
    Resize {
        cols: u16,
        rows: u16,
    },
    /// Try to take the input lock again (read-only viewers).
    Retry,
    Detach,
}

#[derive(Clone, PartialEq, Eq)]
pub enum Ownership {
    Connecting,
    /// This TUI holds the input lock.
    Writer,
    /// Someone else holds it, or the program cannot take input; the reason is shown.
    ReadOnly(String),
}

pub enum Msg {
    Owner(Ownership),
    Snapshot(Box<TerminalSnapshot>),
    Failed(String),
}

pub struct Attach {
    pub action_ref: ActionRef,
    pub run_id: RunId,
    pub ownership: Ownership,
    pub snapshot: Option<Box<TerminalSnapshot>>,
    pub error: Option<String>,
    tx: UnboundedSender<Command>,
    /// The content size the program was last resized to.
    sent: Option<(u16, u16)>,
    /// The content size the view currently has.
    want: Option<(u16, u16)>,
}

pub struct Terminals {
    paths: WorkspacePaths,
    pty: HashSet<ActionRef>,
    pub attach: Option<Attach>,
}

impl Terminals {
    pub fn new(paths: WorkspacePaths) -> Self {
        Self {
            paths,
            pty: HashSet::new(),
            attach: None,
        }
    }

    /// Learns from `item.describe` whether an action runs in a PTY.
    pub fn note_described(&mut self, a: &ActionRef, res: &Result<Box<ItemDescription>, ErrorInfo>) {
        if let Ok(d) = res {
            if d.action
                .as_ref()
                .is_some_and(|x| x.terminal == TerminalMode::Pty)
            {
                self.pty.insert(a.clone());
            } else {
                self.pty.remove(a);
            }
        }
    }

    pub fn is_pty(&self, a: &ActionRef) -> bool {
        self.pty.contains(a)
    }

    pub fn is_open(&self) -> bool {
        self.attach.is_some()
    }

    pub fn open(&mut self, action_ref: ActionRef, run_id: RunId, events: Tx) {
        self.detach();
        let (tx, rx) = unbounded_channel();
        tokio::spawn(worker(self.paths.clone(), run_id.clone(), rx, events));
        self.attach = Some(Attach {
            action_ref,
            run_id,
            ownership: Ownership::Connecting,
            snapshot: None,
            error: None,
            tx,
            sent: None,
            want: None,
        });
    }

    /// Leaves the view; the worker releases the lock and closes its connections.
    pub fn detach(&mut self) {
        if let Some(a) = self.attach.take() {
            let _ = a.tx.send(Command::Detach);
        }
    }

    pub fn handle(&mut self, msg: Msg) {
        let Some(a) = self.attach.as_mut() else {
            return;
        };
        match msg {
            Msg::Owner(o) => {
                a.ownership = o;
                a.error = None;
                if a.ownership == Ownership::Writer {
                    a.sent = None;
                    a.resize_if_needed();
                }
            }
            Msg::Snapshot(s) => {
                let newer = a
                    .snapshot
                    .as_ref()
                    .is_none_or(|old| s.screen_revision >= old.screen_revision || s.exited);
                if newer {
                    a.snapshot = Some(s);
                }
            }
            Msg::Failed(m) => a.error = Some(m),
        }
    }

    fn writer(&self) -> bool {
        self.attach.as_ref().is_some_and(|a| {
            a.ownership == Ownership::Writer && !a.snapshot.as_ref().is_some_and(|s| s.exited)
        })
    }

    /// Routes one key while the view is open.
    pub fn key(&mut self, k: KeyEvent) {
        if k.kind == KeyEventKind::Release {
            return;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        // Ctrl-] arrives as Ctrl-5 from terminals that send the raw 0x1d byte.
        if ctrl && matches!(k.code, KeyCode::Char(']') | KeyCode::Char('5')) {
            self.detach();
            return;
        }
        if self.writer() {
            if let (Some(input), Some(a)) = (encode(k), self.attach.as_ref()) {
                let _ = a.tx.send(Command::Input(input));
            }
            return;
        }
        // Read-only: nothing reaches the program.
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => self.detach(),
            KeyCode::Char('c') if ctrl => self.detach(),
            KeyCode::Char('a') => {
                if let Some(a) = self.attach.as_ref() {
                    let _ = a.tx.send(Command::Retry);
                }
            }
            _ => {}
        }
    }

    pub fn paste(&mut self, text: String) {
        if self.writer()
            && let Some(a) = self.attach.as_ref()
        {
            let _ = a.tx.send(Command::Input(TerminalInput::Paste { text }));
        }
    }

    /// The keys of the open view, for the footer and help (one binding table).
    pub fn bindings(&self) -> Option<Vec<Binding>> {
        let a = self.attach.as_ref()?;
        let mut v = Vec::new();
        let exited = a.snapshot.as_ref().is_some_and(|s| s.exited);
        if self.writer() {
            v.push(bind(
                "keys",
                "go to the program (Esc and Ctrl-C too)",
                Cmd::Forward,
            ));
            v.push(bind("Ctrl-]", "back to Mira (releases input)", Cmd::Detach));
        } else {
            if !exited && matches!(a.ownership, Ownership::ReadOnly(_)) {
                v.push(bind("a", "take input", Cmd::Attach));
            }
            v.push(bind("Ctrl-]/Esc/q", "back to Mira", Cmd::Detach));
        }
        Some(v)
    }
}

impl Attach {
    fn resize_if_needed(&mut self) {
        if self.ownership != Ownership::Writer {
            return;
        }
        if let Some(size) = self.want
            && self.sent != Some(size)
        {
            self.sent = Some(size);
            let _ = self.tx.send(Command::Resize {
                cols: size.0,
                rows: size.1,
            });
        }
    }
}

/// Keys as the program's terminal would send them. `None` for keys with no byte form.
fn encode(k: KeyEvent) -> Option<TerminalInput> {
    let key = |name: &str| Some(TerminalInput::Key { key: name.into() });
    let text = |s: &str| Some(TerminalInput::Text { text: s.into() });
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);
    match k.code {
        KeyCode::Enter => key("enter"),
        KeyCode::Tab => key("tab"),
        KeyCode::BackTab => text("\x1b[Z"),
        KeyCode::Esc => key("escape"),
        KeyCode::Backspace => key("backspace"),
        KeyCode::Delete => key("delete"),
        KeyCode::Up => key("up"),
        KeyCode::Down => key("down"),
        KeyCode::Left => key("left"),
        KeyCode::Right => key("right"),
        KeyCode::Home => text("\x1b[H"),
        KeyCode::End => text("\x1b[F"),
        KeyCode::PageUp => text("\x1b[5~"),
        KeyCode::PageDown => text("\x1b[6~"),
        KeyCode::Insert => text("\x1b[2~"),
        KeyCode::F(n @ 1..=4) => {
            let c = char::from(b'P' + (n - 1));
            Some(TerminalInput::Text {
                text: format!("\x1bO{c}"),
            })
        }
        KeyCode::Char(c) if ctrl => {
            let byte = match c.to_ascii_lowercase() {
                'c' => return key("ctrl-c"),
                'd' => return key("ctrl-d"),
                'z' => return key("ctrl-z"),
                l @ 'a'..='z' => l as u8 - b'a' + 1,
                ' ' | '@' | '2' => 0,
                '4' | '\\' => 0x1c,
                '6' | '^' => 0x1e,
                '7' | '_' | '/' => 0x1f,
                _ => return None,
            };
            let s = char::from(byte).to_string();
            Some(TerminalInput::Text {
                text: if alt { format!("\x1b{s}") } else { s },
            })
        }
        KeyCode::Char(c) => Some(TerminalInput::Text {
            text: if alt {
                format!("\x1b{c}")
            } else {
                c.to_string()
            },
        }),
        _ => None,
    }
}

fn color(c: TerminalColor) -> Option<Color> {
    match c {
        TerminalColor::Default => None,
        TerminalColor::Indexed { index } => Some(Color::Indexed(index)),
        TerminalColor::Rgb { r, g, b } => Some(Color::Rgb(r, g, b)),
    }
}

fn run_style(r: &TerminalStyleRun, use_color: bool) -> Style {
    let mut s = Style::default();
    if use_color {
        if let Some(c) = color(r.fg) {
            s = s.fg(c);
        }
        if let Some(c) = color(r.bg) {
            s = s.bg(c);
        }
    }
    for m in &r.modifiers {
        s = s.add_modifier(match m {
            TerminalModifier::Bold => Modifier::BOLD,
            TerminalModifier::Dim => Modifier::DIM,
            TerminalModifier::Italic => Modifier::ITALIC,
            TerminalModifier::Underline => Modifier::UNDERLINED,
            TerminalModifier::Reverse => Modifier::REVERSED,
            TerminalModifier::Hidden => Modifier::HIDDEN,
            TerminalModifier::Strikethrough => Modifier::CROSSED_OUT,
        });
    }
    s
}

/// One screen row with its style runs (runs address cells; wide characters take two).
fn styled_line<'a>(text: &'a str, runs: &[TerminalStyleRun], use_color: bool) -> Line<'a> {
    if runs.is_empty() {
        return Line::raw(text);
    }
    let mut spans = Vec::new();
    let mut cell: u16 = 0;
    let mut start = 0usize;
    let mut current: Option<usize> = None;
    let run_at = |cell: u16| {
        runs.iter()
            .position(|r| cell >= r.start_cell && cell < r.start_cell.saturating_add(r.cell_count))
    };
    for (i, ch) in text.char_indices() {
        let here = run_at(cell);
        if here != current && i > start {
            let style = current.map_or(Style::default(), |n| run_style(&runs[n], use_color));
            spans.push(Span::styled(&text[start..i], style));
            start = i;
        }
        current = here;
        cell = cell.saturating_add(u16::try_from(ch.width().unwrap_or(0)).unwrap_or(1));
    }
    let style = current.map_or(Style::default(), |n| run_style(&runs[n], use_color));
    spans.push(Span::styled(&text[start..], style));
    // Styled blank cells past the text (for example a reverse-video bar).
    let tail: Vec<_> = runs
        .iter()
        .filter(|r| r.start_cell >= cell && r.cell_count > 0)
        .collect();
    for r in tail {
        if r.start_cell > cell {
            spans.push(Span::raw(" ".repeat(usize::from(r.start_cell - cell))));
        }
        spans.push(Span::styled(
            " ".repeat(usize::from(r.cell_count)),
            run_style(r, use_color),
        ));
        cell = r.start_cell.saturating_add(r.cell_count);
    }
    Line::from(spans)
}

/// Draws the attached screen into `area`: one title row, then the program's rows.
pub fn draw(f: &mut Frame, term: &mut Terminals, area: Rect, use_color: bool) {
    debug_panic();
    let Some(a) = term.attach.as_mut() else {
        return;
    };
    let rows = area.height.saturating_sub(1).clamp(1, MAX_TERMINAL_ROWS);
    let cols = area.width.clamp(1, MAX_TERMINAL_COLS);
    a.want = Some((cols, rows));
    a.resize_if_needed();
    let exited = a.snapshot.as_ref().is_some_and(|s| s.exited);
    let status = if exited {
        "program exited (read-only)".to_owned()
    } else {
        match &a.ownership {
            Ownership::Connecting => "connecting...".to_owned(),
            Ownership::Writer => "input: this TUI".to_owned(),
            Ownership::ReadOnly(why) => format!("read-only: {why}"),
        }
    };
    let mut title = format!("terminal {} · {} · {status}", a.action_ref, a.run_id);
    if let Some(s) = &a.snapshot {
        title.push_str(&format!(" · {}x{}", s.cols, s.rows));
        if s.alternate_screen {
            title.push_str(" · full screen");
        }
    }
    if let Some(e) = &a.error {
        title.push_str(&format!(" · {e}"));
    }
    let title_style = Style::default().add_modifier(Modifier::REVERSED);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(title, title_style))),
        Rect { height: 1, ..area },
    );
    let screen = Rect {
        y: area.y + 1,
        height: area.height.saturating_sub(1),
        ..area
    };
    let Some(s) = &a.snapshot else {
        f.render_widget(Paragraph::new("reading the screen..."), screen);
        return;
    };
    let empty = Vec::new();
    let lines: Vec<Line> = s
        .lines
        .iter()
        .enumerate()
        .take(usize::from(screen.height))
        .map(|(i, text)| {
            let row = s
                .row_start
                .saturating_add(u16::try_from(i).unwrap_or(u16::MAX));
            let runs = s
                .styles
                .as_ref()
                .and_then(|st| st.iter().find(|r| r.row == row))
                .map_or(&empty, |r| &r.runs);
            styled_line(text, runs, use_color)
        })
        .collect();
    f.render_widget(Paragraph::new(lines), screen);
    let writer = a.ownership == Ownership::Writer && !exited;
    if writer && s.cursor.visible && s.cursor.row < screen.height && s.cursor.col < screen.width {
        f.set_cursor_position((screen.x + s.cursor.col, screen.y + s.cursor.row));
    }
}

/// Debug builds only: `MIRA_DEBUG_PANIC_ON_ATTACH=1` panics while a terminal is drawn, to
/// check that the terminal guard restores the outer terminal on the panic path.
#[cfg(debug_assertions)]
#[allow(clippy::panic)]
fn debug_panic() {
    if std::env::var_os("MIRA_DEBUG_PANIC_ON_ATTACH").is_some() {
        panic!("forced panic while attached (MIRA_DEBUG_PANIC_ON_ATTACH)");
    }
}

#[cfg(not(debug_assertions))]
fn debug_panic() {}

// ----- worker -----------------------------------------------------------------------

fn send(tx: &Tx, m: Msg) -> bool {
    tx.send(Event::Terminal(m)).is_ok()
}

async fn acquire(client: &mut Client, run_id: &RunId, tx: &Tx) -> Result<bool, String> {
    let params = TerminalRunParams {
        run_id: run_id.clone(),
    };
    match ipc::call::<_, Ack>(client, Method::TerminalAcquire, &params).await {
        Ok(_) => {
            send(tx, Msg::Owner(Ownership::Writer));
            Ok(true)
        }
        Err(Failure::Reply(e)) => {
            let why = if e.code == ErrorCode::INPUT_BUSY {
                e.message
            } else {
                format!("[{}] {}", e.code, e.message)
            };
            send(tx, Msg::Owner(Ownership::ReadOnly(why)));
            Ok(false)
        }
        Err(Failure::Lost(m)) => Err(m),
    }
}

/// Reads the whole screen with styles, row page by row page, all at one revision.
async fn fetch(client: &mut Client, run_id: &RunId) -> Result<TerminalSnapshot, Failure> {
    let mut tries = 0;
    'again: loop {
        tries += 1;
        let mut params = TerminalSnapshotParams {
            run_id: run_id.clone(),
            row_start: None,
            row_count: None,
            include_style: true,
            max_bytes: u32::try_from(MAX_REPLY_BUDGET_BYTES).ok(),
        };
        let first =
            ipc::call::<_, TerminalSnapshot>(client, Method::TerminalSnapshotM, &params).await?;
        let mut snap = first.data;
        let mut truncated = first.meta.truncated;
        while truncated && !snap.lines.is_empty() {
            let next = snap
                .row_start
                .saturating_add(u16::try_from(snap.lines.len()).unwrap_or(u16::MAX));
            if next >= snap.rows {
                break;
            }
            params.row_start = Some(next);
            let page = ipc::call::<_, TerminalSnapshot>(client, Method::TerminalSnapshotM, &params)
                .await?;
            if page.data.screen_revision != snap.screen_revision {
                if tries < PAGE_TRIES {
                    continue 'again;
                }
                break;
            }
            truncated = page.meta.truncated && !page.data.lines.is_empty();
            snap.lines.extend(page.data.lines);
            if let (Some(all), Some(more)) = (snap.styles.as_mut(), page.data.styles) {
                all.extend(more);
            }
        }
        return Ok(snap);
    }
}

async fn open_stream(paths: &WorkspacePaths, run_id: &RunId) -> Option<Client> {
    let mut c = connect(paths, &ipc::options(ConnectionKind::Stream))
        .await
        .ok()?;
    let params = StreamSubscribeParams {
        kinds: vec![StreamKind::Terminal],
        refs: vec![run_id.to_string()],
        cursor: None,
    };
    ipc::call::<_, Subscribed>(&mut c, Method::StreamSubscribe, &params)
        .await
        .ok()?;
    Some(c)
}

/// The next screen revision announced on the stream; `Err` when the stream ended.
async fn next_revision(stream: &mut Option<Client>) -> Result<ScreenRevision, ()> {
    let Some(c) = stream.as_mut() else {
        return std::future::pending().await;
    };
    loop {
        match c.next_event().await {
            Ok(f) => match f.event {
                StreamEvent::Terminal {
                    screen_revision, ..
                } => return Ok(screen_revision),
                StreamEvent::End { .. } => return Err(()),
                _ => {}
            },
            Err(_) => return Err(()),
        }
    }
}

async fn worker(paths: WorkspacePaths, run_id: RunId, mut rx: UnboundedReceiver<Command>, tx: Tx) {
    let mut control = match connect(&paths, &ipc::options(ConnectionKind::Control)).await {
        Ok(c) => c,
        Err(e) => {
            send(&tx, Msg::Failed(format!("cannot connect: {e}")));
            return;
        }
    };
    let lost = |tx: &Tx, m: String| {
        send(tx, Msg::Failed(format!("host connection lost: {m}")));
    };
    let mut writer = match acquire(&mut control, &run_id, &tx).await {
        Ok(w) => w,
        Err(m) => return lost(&tx, m),
    };
    let mut stream = open_stream(&paths, &run_id).await;
    let mut shown: Option<ScreenRevision> = None;
    let mut refresh = true;
    let mut poll = tokio::time::interval(POLL);
    loop {
        if refresh {
            refresh = false;
            match fetch(&mut control, &run_id).await {
                Ok(s) => {
                    shown = Some(s.screen_revision);
                    if !send(&tx, Msg::Snapshot(Box::new(s))) {
                        return;
                    }
                }
                Err(Failure::Lost(m)) => return lost(&tx, m),
                Err(Failure::Reply(e)) => {
                    send(&tx, Msg::Failed(format!("[{}] {}", e.code, e.message)));
                }
            }
        }
        let mut stream_ended = false;
        tokio::select! {
            cmd = rx.recv() => match cmd {
                None | Some(Command::Detach) => break,
                Some(Command::Input(input)) if writer => {
                    let params = TerminalInputParams {
                        run_id: run_id.clone(),
                        input,
                        expected_screen_revision: None,
                        reply_now: true,
                    };
                    match ipc::call::<_, TerminalSnapshot>(&mut control, Method::TerminalInputM, &params).await {
                        Ok(_) => {}
                        Err(Failure::Lost(m)) => return lost(&tx, m),
                        Err(Failure::Reply(e)) if e.code == ErrorCode::INPUT_BUSY => {
                            writer = false;
                            send(&tx, Msg::Owner(Ownership::ReadOnly(e.message)));
                        }
                        // The message never contains the input itself.
                        Err(Failure::Reply(e)) => {
                            send(&tx, Msg::Failed(format!("input not sent: [{}] {}", e.code, e.message)));
                        }
                    }
                }
                Some(Command::Input(_)) => {}
                Some(Command::Resize { cols, rows }) => {
                    if writer {
                        let params = TerminalResizeParams { run_id: run_id.clone(), cols, rows };
                        match ipc::call::<_, Ack>(&mut control, Method::TerminalResize, &params).await {
                            Ok(_) => refresh = true,
                            Err(Failure::Lost(m)) => return lost(&tx, m),
                            Err(Failure::Reply(e)) => {
                                send(&tx, Msg::Failed(format!("resize refused: {}", e.message)));
                            }
                        }
                    }
                }
                Some(Command::Retry) => {
                    if !writer {
                        writer = match acquire(&mut control, &run_id, &tx).await {
                            Ok(w) => w,
                            Err(m) => return lost(&tx, m),
                        };
                        refresh = true;
                    }
                }
            },
            rev = next_revision(&mut stream) => match rev {
                Ok(r) => refresh = shown.is_none_or(|s| r > s),
                Err(()) => stream_ended = true,
            },
            _ = poll.tick(), if stream.is_none() => refresh = true,
        }
        if stream_ended {
            stream = None;
            refresh = true;
        }
    }
    if writer {
        let params = TerminalRunParams { run_id };
        let _ = ipc::call::<_, Ack>(&mut control, Method::TerminalRelease, &params).await;
    }
    // Dropping the connections ends the subscription and any lock left behind.
}
