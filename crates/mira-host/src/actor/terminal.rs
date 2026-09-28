//! Shared PTY terminals: snapshot, input lock, input, and resize.
//!
//! At most one writer per run. `terminal.acquire` binds the lock to the calling connection
//! until `terminal.release` or disconnect. Without a held lock, one `terminal.input` call
//! takes the lock for that single write, inside one actor step. Input bodies are never
//! logged, stored, or echoed in errors.

use std::time::Duration;

use mira_protocol::error::{ErrorCode, ErrorInfo};
use mira_protocol::ids::{ClientId, RunId, ScreenRevision};
use mira_protocol::ipc::*;
use mira_protocol::limits::{
    DEFAULT_REPLY_BUDGET_BYTES, MAX_REPLY_BUDGET_BYTES, MAX_TERMINAL_COLS,
    MAX_TERMINAL_INPUT_BYTES, MAX_TERMINAL_ROWS,
};
use mira_protocol::reply::ReplyMeta;
use mira_protocol::run::StopReason;
use serde_json::{Map, Value};
use tokio::time::Instant;

use super::{Actor, Responder, reply_ok};
use crate::pty::{PtyHandle, SnapshotRequest, TerminalEvent, WriteError, key_bytes};

/// Finished terminals whose last screen stays readable.
const KEEP_EXITED: usize = 8;
/// After input, the reply waits for the screen to settle: quiet this long...
const SETTLE_QUIET: Duration = Duration::from_millis(100);
/// ...but never longer than this.
const SETTLE_MAX: Duration = Duration::from_millis(1000);
const SETTLE_POLL: Duration = Duration::from_millis(20);

/// Every key name `terminal.input` accepts, and the bytes it sends.
pub const KEYS: &[&str] = &[
    "enter",
    "tab",
    "escape",
    "backspace",
    "delete",
    "up",
    "down",
    "left",
    "right",
    "ctrl-c",
    "ctrl-d",
    "ctrl-z",
    "ctrl-right-bracket",
];

pub struct TerminalEntry {
    handle: PtyHandle,
    owner: Option<ClientId>,
    started: Instant,
}

/// Pasted text as a terminal sends it: line breaks become CR (no Enter is added), and the
/// text is wrapped in bracketed-paste markers only when the child enabled mode 2004.
fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let body = text.replace("\r\n", "\r").replace('\n', "\r");
    if bracketed {
        // A pasted end marker must not end the paste early.
        let body = body.replace("\x1b[201~", "");
        let mut out = Vec::with_capacity(body.len() + 12);
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(body.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        body.into_bytes()
    }
}

fn invalid(msg: impl Into<String>) -> ErrorInfo {
    ErrorInfo::new(ErrorCode::INVALID_ARGUMENT, msg)
}

impl Actor {
    /// Registers a started PTY run and forgets the oldest finished terminals.
    pub(crate) fn pty_started(&mut self, run_id: RunId, handle: PtyHandle) {
        self.terminals.insert(
            run_id,
            TerminalEntry {
                handle,
                owner: None,
                started: Instant::now(),
            },
        );
        let mut exited: Vec<(Instant, RunId)> = self
            .terminals
            .iter()
            .filter(|(id, t)| !self.runs.contains_key(*id) && t.handle.exited())
            .map(|(id, t)| (t.started, id.clone()))
            .collect();
        if exited.len() > KEEP_EXITED {
            exited.sort();
            for (_, id) in exited.iter().take(exited.len() - KEEP_EXITED) {
                self.terminals.remove(id);
            }
        }
    }

    pub(crate) fn terminal_event(&mut self, run_id: &RunId, ev: TerminalEvent) {
        match ev {
            TerminalEvent::Screen(rev) => {
                if let Some(t) = self.terminals.get_mut(run_id)
                    && t.handle.exited()
                {
                    t.owner = None;
                }
                self.broadcast_terminal(run_id, rev);
            }
            TerminalEvent::OutputLimit => {
                if let Some(run) = self.runs.get(run_id)
                    && let Ok(mut log) = run.log.lock()
                {
                    log.push_line(
                        mira_protocol::run::LogStream::Host,
                        mira_protocol::view::LogLevel::Error,
                        "terminal output exceeded what the host can parse; stopping the run",
                        false,
                    );
                }
                if self.stop_run(run_id, StopReason::OutputLimit).is_some() {
                    self.state_changed();
                }
            }
        }
    }

    /// Releases every input lock the closed connection held.
    pub(crate) fn release_terminal_locks(&mut self, client: &ClientId) {
        for t in self.terminals.values_mut() {
            if t.owner.as_ref() == Some(client) {
                t.owner = None;
            }
        }
    }

    fn terminal(&self, run_id: &RunId) -> Result<&TerminalEntry, ErrorInfo> {
        if let Some(t) = self.terminals.get(run_id) {
            return Ok(t);
        }
        let known =
            self.runs.contains_key(run_id) || self.recent.iter().any(|(r, _)| &r.run_id == run_id);
        Err(if known {
            invalid(format!(
                "run {run_id} has no terminal (it is not a PTY run, or its screen is no longer kept)"
            ))
            .with_next_action(&["mira", "logs", run_id.as_str()], "Read the run's output.")
        } else {
            ErrorInfo::new(
                ErrorCode::NOT_FOUND,
                format!(
                    "no terminal for run {run_id} in this host (screens are not kept after the host restarts)"
                ),
            )
        })
    }

    /// The terminal of a live PTY run, for acquire/input/resize.
    fn live_terminal(&self, run_id: &RunId) -> Result<&TerminalEntry, ErrorInfo> {
        let t = self.terminal(run_id)?;
        let active = self
            .runs
            .get(run_id)
            .is_some_and(|r| r.record.lifecycle.is_active());
        if !active || t.handle.exited() {
            return Err(invalid(format!(
                "run {run_id} has exited; its last screen is still readable"
            ))
            .with_next_action(
                &["mira", "terminal", run_id.as_str()],
                "Read the final screen.",
            ));
        }
        Ok(t)
    }

    fn input_busy(&self, owner: &ClientId) -> ErrorInfo {
        let kind = serde_json::to_value(self.client_kind(owner)).unwrap_or(Value::Null);
        let label = kind.as_str().unwrap_or("another").to_owned();
        let mut details = Map::new();
        details.insert("owner_kind".into(), kind);
        ErrorInfo::new(
            ErrorCode::INPUT_BUSY,
            format!("another {label} client holds this terminal's input"),
        )
        .with_details(details)
        .retryable(true)
    }

    pub(super) fn terminal_snapshot(&mut self, p: TerminalSnapshotParams, r: Responder) {
        let t = match self.terminal(&p.run_id) {
            Ok(t) => t,
            Err(e) => return r.send(self.fail(e)),
        };
        let budget = p.max_bytes.map_or(DEFAULT_REPLY_BUDGET_BYTES, |b| {
            (b as usize).clamp(1024, MAX_REPLY_BUDGET_BYTES)
        });
        let req = SnapshotRequest {
            row_start: p.row_start.unwrap_or(0),
            row_count: p.row_count,
            include_style: p.include_style,
            budget,
            owner: t.owner.clone(),
        };
        let (snap, truncated) = t.handle.snapshot(&req);
        if p.row_start.is_some_and(|s| s >= snap.rows && snap.rows > 0) {
            return r.send(self.fail(invalid(format!(
                "row_start must be below the screen height {}",
                snap.rows
            ))));
        }
        let meta = ReplyMeta {
            truncated,
            ..ReplyMeta::default()
        };
        r.send(self.ok(snap, meta));
    }

    pub(super) fn terminal_acquire(
        &mut self,
        client: &ClientId,
        p: TerminalRunParams,
        r: Responder,
    ) {
        let owner = match self.live_terminal(&p.run_id) {
            Ok(t) => t.owner.clone(),
            Err(e) => return r.send(self.fail(e)),
        };
        match owner {
            Some(o) if &o != client => r.send(self.fail(self.input_busy(&o))),
            _ => {
                if let Some(t) = self.terminals.get_mut(&p.run_id) {
                    t.owner = Some(client.clone());
                }
                r.send(self.ok(Ack { ok: true }, ReplyMeta::default()));
            }
        }
    }

    pub(super) fn terminal_release(
        &mut self,
        client: &ClientId,
        p: TerminalRunParams,
        r: Responder,
    ) {
        if let Err(e) = self.terminal(&p.run_id) {
            return r.send(self.fail(e));
        }
        let mut released = false;
        if let Some(t) = self.terminals.get_mut(&p.run_id)
            && t.owner.as_ref() == Some(client)
        {
            t.owner = None;
            released = true;
        }
        r.send(self.ok(Ack { ok: released }, ReplyMeta::default()));
    }

    pub(super) fn terminal_input(
        &mut self,
        client: &ClientId,
        p: TerminalInputParams,
        r: Responder,
    ) {
        let t = match self.live_terminal(&p.run_id) {
            Ok(t) => t,
            Err(e) => return r.send(self.fail(e)),
        };
        if let Some(o) = &t.owner
            && o != client
        {
            return r.send(self.fail(self.input_busy(o)));
        }
        let (bracketed, app_cursor) = t.handle.modes();
        let bytes = match &p.input {
            TerminalInput::Mouse { mouse } => t.handle.mouse_bytes(mouse),
            TerminalInput::Text { text } => text.as_bytes().to_vec(),
            TerminalInput::Paste { text } => paste_bytes(text, bracketed),
            TerminalInput::Key { key } => match key_bytes(key, app_cursor) {
                Some(b) => b.to_vec(),
                None => {
                    return r.send(self.fail(invalid(format!(
                        "unknown key `{key}`; use one of: {}",
                        KEYS.join(", ")
                    ))));
                }
            },
        };
        if bytes.is_empty() && !matches!(p.input, TerminalInput::Mouse { .. }) {
            return r.send(self.fail(invalid("input is empty; nothing was written")));
        }
        if bytes.len() > MAX_TERMINAL_INPUT_BYTES {
            return r.send(self.fail(invalid(format!(
                "input is {} bytes; the limit is 64 KiB per call, send it in smaller parts",
                bytes.len()
            ))));
        }
        if let Some(expected) = p.expected_screen_revision {
            let current = t.handle.revision();
            if expected != current {
                let mut details = Map::new();
                details.insert("screen_revision".into(), Value::from(current.get()));
                return r.send(self.fail(
                    ErrorInfo::new(
                        ErrorCode::SCREEN_CHANGED,
                        format!(
                            "the screen is at revision {current}, not {expected}; nothing was written"
                        ),
                    )
                    .with_details(details)
                    .with_next_action(
                        &["mira", "terminal", p.run_id.as_str()],
                        "Read the current screen, then decide again.",
                    ),
                ));
            }
        }
        let before = t.handle.revision();
        let ignored = bytes.is_empty();
        let written = if ignored {
            Ok(())
        } else {
            t.handle.write(bytes)
        };
        match written {
            Ok(()) => {}
            Err(WriteError::Full) => {
                return r.send(
                    self.fail(
                        ErrorInfo::new(
                            ErrorCode::BUSY,
                            "the program is not reading its input; nothing was written",
                        )
                        .retryable(true),
                    ),
                );
            }
            Err(WriteError::Closed) => {
                return r.send(self.fail(invalid(format!(
                    "run {} has exited; nothing was written",
                    p.run_id
                ))));
            }
        }
        // Reply with the screen once the program has reacted (or stayed quiet).
        let screen = t.handle.clone();
        let owner = t.owner.clone();
        let ctx = self.ctx();
        let reply_now = p.reply_now || ignored;
        tokio::spawn(async move {
            if !reply_now {
                settle(&screen, before).await;
            }
            let req = SnapshotRequest {
                row_start: 0,
                row_count: None,
                include_style: false,
                budget: DEFAULT_REPLY_BUDGET_BYTES,
                owner,
            };
            let (snap, truncated) = screen.snapshot(&req);
            let meta = ReplyMeta {
                truncated,
                ..ReplyMeta::default()
            };
            r.send(reply_ok(ctx, snap, meta));
        });
    }

    pub(super) fn terminal_resize(
        &mut self,
        client: &ClientId,
        p: TerminalResizeParams,
        r: Responder,
    ) {
        let t = match self.live_terminal(&p.run_id) {
            Ok(t) => t,
            Err(e) => return r.send(self.fail(e)),
        };
        match &t.owner {
            Some(o) if o == client => {}
            Some(o) => return r.send(self.fail(self.input_busy(o))),
            None => {
                return r.send(self.fail(
                    invalid("only the input lock holder can resize; observers never change the program's size")
                        .with_next_action(
                            &["mira", "terminal", p.run_id.as_str()],
                            "Read the screen at its current size.",
                        ),
                ));
            }
        }
        if !(1..=MAX_TERMINAL_COLS).contains(&p.cols) || !(1..=MAX_TERMINAL_ROWS).contains(&p.rows)
        {
            return r.send(self.fail(invalid(format!(
                "terminal size must be 1-{MAX_TERMINAL_COLS} columns and 1-{MAX_TERMINAL_ROWS} rows"
            ))));
        }
        match t.handle.resize(p.rows, p.cols) {
            Ok(()) => r.send(self.ok(Ack { ok: true }, ReplyMeta::default())),
            Err(e) => r.send(self.fail(e)),
        }
    }
}

/// Waits until the screen changed and then stayed quiet, or the bound passes.
async fn settle(screen: &PtyHandle, before: ScreenRevision) {
    let start = Instant::now();
    let mut last = before;
    let mut last_change = start;
    while start.elapsed() < SETTLE_MAX {
        tokio::time::sleep(SETTLE_POLL).await;
        let now = screen.revision();
        if now != last {
            last = now;
            last_change = Instant::now();
        } else if (last != before || screen.exited()) && last_change.elapsed() >= SETTLE_QUIET {
            break;
        } else if last == before && start.elapsed() >= SETTLE_QUIET * 3 {
            // The program did not react; do not hold the reply longer.
            break;
        }
    }
}
