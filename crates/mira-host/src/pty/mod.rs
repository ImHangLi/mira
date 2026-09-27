//! PTY command runner: one child per run in its own session with the PTY
//! as controlling terminal, a vt100 screen model, and serialized input.
//!
//! Bytes from the child never reach a client. A reader thread moves raw chunks to a parser
//! thread through a bounded queue; the parser owns the virtual screen and records plain-text
//! transcript lines into the run log. Sequences that act on a real terminal (OSC 52, titles,
//! window operations, DCS and other strings) only change the virtual screen or are dropped.
//! Keyboard input goes through a writer thread and is never logged or stored.
//!
//! Stop, timeout, grace, drain, and cleanup reuse the pipe runner's rules (`runner.rs`).

mod screen;
mod transcript;

use std::io::{Read, Write};
use std::path::Path;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as smpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant as StdInstant};

use mira_protocol::error::{ErrorCode, ErrorInfo};
use mira_protocol::ids::{ClientId, RunId, ScreenRevision};
use mira_protocol::ipc::{TerminalCursor, TerminalSnapshot, TerminalStyleRow};
use mira_protocol::limits::{DEFAULT_TERMINAL_COLS, DEFAULT_TERMINAL_ROWS};
use mira_protocol::manifest::{StopSignal, TimeoutPolicy};
use mira_protocol::run::{CleanupState, LogStream};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use rustix::process::Signal;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, sleep_until};

use crate::logs::{FLUSH_EVERY, SharedLog};
use crate::runner::{
    self, Batcher, CommandSpec, DRAIN_AFTER_EXIT, EventSink, FAR, FinishedRun, RunnerEvent,
    StopKind,
};

use screen::{Screen, printable, style_row};
use transcript::Transcript;

/// Raw output chunks waiting for the parser (64 × 16 KiB = 1 MiB).
const READ_CHUNK: usize = 16 * 1024;
const PARSE_QUEUE: usize = 64;
/// When the parse queue stays full this long, the run stops with stop reason `output_limit`.
const OUTPUT_STALL: Duration = Duration::from_secs(10);
/// Pending input writes; a full queue means the child is not reading its input.
const INPUT_QUEUE: usize = 32;
/// Screen change notifications are coalesced to at most one per interval (~30 fps).
const NOTIFY_EVERY: Duration = Duration::from_millis(33);

#[derive(Debug)]
pub enum TerminalEvent {
    /// The screen changed; only the latest revision is reported.
    Screen(ScreenRevision),
    /// The parser fell too far behind the child's output.
    OutputLimit,
}

pub struct SnapshotRequest {
    pub row_start: u16,
    pub row_count: Option<u16>,
    pub include_style: bool,
    pub budget: usize,
    pub owner: Option<ClientId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteError {
    /// The input queue is full: the child is not reading.
    Full,
    /// The child exited; nothing was written.
    Closed,
}

/// The actor's view of one PTY run.
#[derive(Clone)]
pub struct PtyHandle {
    run_id: RunId,
    screen: Arc<Mutex<Screen>>,
    input: smpsc::SyncSender<Vec<u8>>,
    master: Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>,
    closed: Arc<AtomicBool>,
}

impl PtyHandle {
    pub fn revision(&self) -> ScreenRevision {
        self.screen
            .lock()
            .map_or(ScreenRevision::ZERO, |s| s.revision)
    }

    pub fn exited(&self) -> bool {
        self.closed.load(Ordering::SeqCst) || self.screen.lock().map_or(true, |s| s.exited)
    }

    /// Input modes the child enabled: (bracketed paste, application cursor keys).
    pub fn modes(&self) -> (bool, bool) {
        self.screen.lock().map_or((false, false), |s| {
            let sc = s.parser.screen();
            (sc.bracketed_paste(), sc.application_cursor())
        })
    }

    /// Queues bytes for the child without blocking the caller.
    pub fn write(&self, bytes: Vec<u8>) -> Result<(), WriteError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(WriteError::Closed);
        }
        self.input.try_send(bytes).map_err(|e| match e {
            smpsc::TrySendError::Full(_) => WriteError::Full,
            smpsc::TrySendError::Disconnected(_) => WriteError::Closed,
        })
    }

    pub fn resize(&self, rows: u16, cols: u16) -> Result<(), ErrorInfo> {
        let fail = |m: String| ErrorInfo::new(ErrorCode::EXECUTION_FAILED, m);
        let master = self
            .master
            .lock()
            .map_err(|_| fail("terminal unavailable".into()))?;
        let Some(master) = master.as_ref() else {
            return Err(fail("the program has exited".into()));
        };
        master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| fail(format!("cannot resize the terminal: {e}")))?;
        if let Ok(mut s) = self.screen.lock() {
            s.parser.screen_mut().set_size(rows, cols);
            s.refresh();
        }
        Ok(())
    }

    /// One consistent read of the screen; returns the snapshot and whether rows were cut to
    /// fit the byte budget.
    pub fn snapshot(&self, req: &SnapshotRequest) -> (TerminalSnapshot, bool) {
        let Ok(s) = self.screen.lock() else {
            return (self.empty(), false);
        };
        let screen = s.parser.screen();
        let (rows, cols) = screen.size();
        let start = req.row_start.min(rows);
        let end = req
            .row_count
            .map_or(rows, |n| start.saturating_add(n).min(rows));
        let (crow, ccol) = screen.cursor_position();
        let mut size = 512usize;
        let mut lines = Vec::new();
        let mut styles = req.include_style.then(Vec::new);
        let mut truncated = false;
        for (i, text) in screen.rows(0, cols).enumerate() {
            let Ok(row) = u16::try_from(i) else { break };
            if row < start {
                continue;
            }
            if row >= end {
                break;
            }
            let text = printable(text);
            let mut n = serde_json::to_vec(&text).map_or(text.len() + 3, |v| v.len() + 1);
            let runs = styles.as_ref().map(|_| style_row(screen, row, cols));
            if let Some(runs) = &runs {
                n += serde_json::to_vec(runs).map_or(0, |v| v.len() + 16);
            }
            if size + n > req.budget && !lines.is_empty() {
                truncated = true;
                break;
            }
            size += n;
            lines.push(text);
            if let (Some(all), Some(runs)) = (styles.as_mut(), runs)
                && !runs.is_empty()
            {
                all.push(TerminalStyleRow { row, runs });
            }
        }
        let snap = TerminalSnapshot {
            run_id: self.run_id.clone(),
            screen_revision: s.revision,
            cols,
            rows,
            row_start: start,
            lines,
            cursor: TerminalCursor {
                row: crow,
                col: ccol,
                visible: !screen.hide_cursor(),
            },
            alternate_screen: screen.alternate_screen(),
            input_owner: if s.exited { None } else { req.owner.clone() },
            exited: s.exited,
            styles,
        };
        (snap, truncated)
    }

    fn empty(&self) -> TerminalSnapshot {
        TerminalSnapshot {
            run_id: self.run_id.clone(),
            screen_revision: ScreenRevision::ZERO,
            cols: 0,
            rows: 0,
            row_start: 0,
            lines: vec![],
            cursor: TerminalCursor {
                row: 0,
                col: 0,
                visible: false,
            },
            alternate_screen: false,
            input_owner: None,
            exited: true,
            styles: None,
        }
    }
}

fn spawn_thread(name: &str, f: impl FnOnce() + Send + 'static) -> bool {
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(f)
        .is_ok()
}

/// Reads raw chunks and queues them for the parser. Never drops bytes: when the queue is
/// full it waits (the kernel then applies backpressure to the child), and a stall longer
/// than [`OUTPUT_STALL`] reports [`TerminalEvent::OutputLimit`] once so the actor stops the run.
fn reader_loop(
    mut reader: Box<dyn Read + Send>,
    queue: smpsc::SyncSender<Vec<u8>>,
    run_id: RunId,
    events: EventSink,
) {
    let mut buf = vec![0u8; READ_CHUNK];
    let mut reported = false;
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let mut chunk = buf[..n].to_vec();
        let stalled_at = StdInstant::now();
        loop {
            match queue.try_send(chunk) {
                Ok(()) => break,
                Err(smpsc::TrySendError::Disconnected(_)) => return,
                Err(smpsc::TrySendError::Full(c)) => {
                    chunk = c;
                    if !reported && stalled_at.elapsed() >= OUTPUT_STALL {
                        reported = true;
                        let _ = events.try_send((
                            run_id.clone(),
                            RunnerEvent::Terminal(TerminalEvent::OutputLimit),
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }
}

/// Feeds the virtual screen and the transcript; answers terminal queries.
fn parser_loop(
    queue: smpsc::Receiver<Vec<u8>>,
    screen: Arc<Mutex<Screen>>,
    input: smpsc::SyncSender<Vec<u8>>,
    mut batch: Batcher,
    done: oneshot::Sender<()>,
) {
    let mut transcript = Transcript::default();
    let mut lines = Vec::new();
    loop {
        match queue.recv_timeout(FLUSH_EVERY) {
            Ok(chunk) => {
                let replies = match screen.lock() {
                    Ok(mut s) => {
                        s.parser.process(&chunk);
                        s.refresh();
                        std::mem::take(&mut s.parser.callbacks_mut().out)
                    }
                    Err(_) => Vec::new(),
                };
                if !replies.is_empty() {
                    let _ = input.try_send(replies);
                }
                transcript.feed(&chunk, &mut lines);
                for l in lines.drain(..) {
                    batch.line(LogStream::Pty, &l);
                }
                batch.tick();
            }
            Err(smpsc::RecvTimeoutError::Timeout) => batch.tick(),
            Err(smpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    transcript.finish(&mut lines);
    for l in lines.drain(..) {
        batch.line(LogStream::Pty, &l);
    }
    batch.finish();
    let _ = done.send(());
}

/// Writes queued input to the child. Stops when `closed` is set or the queue closes.
fn writer_loop(
    mut writer: Box<dyn Write + Send>,
    input: smpsc::Receiver<Vec<u8>>,
    closed: Arc<AtomicBool>,
) {
    while !closed.load(Ordering::SeqCst) {
        match input.recv_timeout(Duration::from_millis(200)) {
            Ok(bytes) => {
                if writer
                    .write_all(&bytes)
                    .and_then(|()| writer.flush())
                    .is_err()
                {
                    break;
                }
            }
            Err(smpsc::RecvTimeoutError::Timeout) => {}
            Err(smpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn build_command(spec: &CommandSpec) -> Result<CommandBuilder, ErrorInfo> {
    if !Path::new(&spec.cwd).is_dir() {
        return Err(ErrorInfo::new(
            ErrorCode::EXECUTION_FAILED,
            format!("working directory {} does not exist", spec.cwd.display()),
        ));
    }
    let mut cmd = CommandBuilder::from_argv(spec.argv.iter().map(Into::into).collect());
    cmd.env_clear();
    for (k, v) in &spec.env.0 {
        cmd.env(k, v);
    }
    cmd.env("TERM", "xterm-256color");
    cmd.cwd(&spec.cwd);
    Ok(cmd)
}

struct Spawned {
    pid: u32,
    handle: PtyHandle,
    exit_rx: oneshot::Receiver<Option<ExitStatus>>,
    parsed_rx: oneshot::Receiver<()>,
}

fn spawn(spec: &CommandSpec, log: &SharedLog, events: &EventSink) -> Result<Spawned, ErrorInfo> {
    let fail = |m: String| ErrorInfo::new(ErrorCode::EXECUTION_FAILED, m);
    let cmd = build_command(spec)?;
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: DEFAULT_TERMINAL_ROWS,
            cols: DEFAULT_TERMINAL_COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| fail(format!("cannot open a terminal: {e}")))?;
    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| fail(format!("cannot start `{}`: {e}", spec.argv[0])))?;
    // The parent must not keep the slave open, or the reader never sees end of output.
    drop(pair.slave);
    let pid = child.process_id().unwrap_or(0);
    let mut child = match child.into_any().downcast::<std::process::Child>() {
        Ok(c) => c,
        Err(_) => {
            runner::signal_group(pid, Signal::KILL);
            return Err(fail("unsupported terminal child handle".into()));
        }
    };
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| fail(format!("cannot read the terminal: {e}")))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| fail(format!("cannot write the terminal: {e}")))?;
    let screen = Arc::new(Mutex::new(Screen::new(
        DEFAULT_TERMINAL_ROWS,
        DEFAULT_TERMINAL_COLS,
    )));
    let closed = Arc::new(AtomicBool::new(false));
    let (input_tx, input_rx) = smpsc::sync_channel::<Vec<u8>>(INPUT_QUEUE);
    let (queue_tx, queue_rx) = smpsc::sync_channel::<Vec<u8>>(PARSE_QUEUE);
    let (exit_tx, exit_rx) = oneshot::channel();
    let (parsed_tx, parsed_rx) = oneshot::channel();
    let batch = Batcher::new(spec.run_id.clone(), log.clone(), events.clone());
    let started = [
        spawn_thread("mira-pty-wait", move || {
            let _ = exit_tx.send(child.wait().ok());
        }),
        {
            let (run_id, events) = (spec.run_id.clone(), events.clone());
            spawn_thread("mira-pty-read", move || {
                reader_loop(reader, queue_tx, run_id, events);
            })
        },
        {
            let (screen, input) = (screen.clone(), input_tx.clone());
            spawn_thread("mira-pty-parse", move || {
                parser_loop(queue_rx, screen, input, batch, parsed_tx);
            })
        },
        {
            let closed = closed.clone();
            spawn_thread("mira-pty-write", move || {
                writer_loop(writer, input_rx, closed);
            })
        },
    ];
    if started.iter().any(|ok| !ok) {
        runner::signal_group(pid, Signal::KILL);
        return Err(fail("cannot start terminal threads".into()));
    }
    Ok(Spawned {
        pid,
        handle: PtyHandle {
            run_id: spec.run_id.clone(),
            screen,
            input: input_tx,
            master: Arc::new(Mutex::new(Some(pair.master))),
            closed,
        },
        exit_rx,
        parsed_rx,
    })
}

/// Starts the child and its supervisor. Returns the actor's handle, or `None` when the child
/// could not start (the supervisor then reports the spawn error as the run's end).
pub fn start(
    spec: CommandSpec,
    log: SharedLog,
    events: EventSink,
    stop_rx: mpsc::Receiver<StopKind>,
) -> Option<PtyHandle> {
    match spawn(&spec, &log, &events) {
        Ok(s) => {
            let shared = PtyShared {
                screen: s.handle.screen.clone(),
                input: s.handle.input.clone(),
                master: s.handle.master.clone(),
                closed: s.handle.closed.clone(),
            };
            let handle = s.handle;
            crate::groups::record_soon(&spec.run_id, s.pid);
            tokio::spawn(supervise(
                spec,
                log,
                events,
                stop_rx,
                s.pid,
                shared,
                s.exit_rx,
                s.parsed_rx,
            ));
            Some(handle)
        }
        Err(err) => {
            tokio::spawn(async move {
                let mut batch = Batcher::new(spec.run_id.clone(), log, events.clone());
                batch.note(&err.message);
                batch.finish();
                runner::remove_temp(&spec.temp_files);
                let _ = events
                    .send((
                        spec.run_id.clone(),
                        RunnerEvent::Finished(Box::new(FinishedRun {
                            exit: None,
                            spawn_error: Some(err),
                            cleanup: CleanupState::NotNeeded,
                        })),
                    ))
                    .await;
            });
            None
        }
    }
}

/// The supervisor's share of the handle.
struct PtyShared {
    screen: Arc<Mutex<Screen>>,
    input: smpsc::SyncSender<Vec<u8>>,
    master: Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>,
    closed: Arc<AtomicBool>,
}

#[allow(clippy::too_many_arguments)]
async fn supervise(
    spec: CommandSpec,
    log: SharedLog,
    events: EventSink,
    mut stop_rx: mpsc::Receiver<StopKind>,
    pid: u32,
    shared: PtyShared,
    mut exit_rx: oneshot::Receiver<Option<ExitStatus>>,
    mut parsed_rx: oneshot::Receiver<()>,
) {
    let run_id = spec.run_id.clone();
    let mut batch = Batcher::new(run_id.clone(), log, events.clone());
    let _ = events.send((run_id.clone(), RunnerEvent::Spawned)).await;
    let timeout_at = Instant::now()
        + match spec.timeout {
            TimeoutPolicy::Unlimited => FAR,
            TimeoutPolicy::After(d) => d,
        };
    let mut kill_at = Instant::now() + FAR;
    let mut stopping = false;
    let mut timed_out = false;
    let mut stop_kind = None;
    let mut notified = ScreenRevision::ZERO;
    let mut notify = tokio::time::interval(NOTIFY_EVERY);
    let notify_screen = |notified: &mut ScreenRevision| {
        let rev = shared.screen.lock().map_or(*notified, |s| s.revision);
        if rev != *notified {
            *notified = rev;
            let _ = events.try_send((
                run_id.clone(),
                RunnerEvent::Terminal(TerminalEvent::Screen(rev)),
            ));
        }
    };
    let begin_stop = |batch: &mut Batcher, why: &str| {
        match spec.stop_signal {
            StopSignal::Interrupt => {
                batch.note(&format!("{why}: sending Ctrl-C through the terminal"));
                if shared.input.try_send(vec![0x03]).is_err() {
                    runner::signal_group(pid, Signal::INT);
                }
            }
            StopSignal::Term => {
                batch.note(&format!("{why}: sending SIGTERM to the process group"));
                runner::signal_group(pid, Signal::TERM);
            }
        }
        Instant::now() + spec.grace
    };
    let status = loop {
        tokio::select! {
            status = &mut exit_rx => break status.ok().flatten(),
            kind = stop_rx.recv(), if !stopping => {
                stop_kind = kind;
                stopping = true;
                kill_at = begin_stop(&mut batch, "stopping");
            }
            _ = sleep_until(timeout_at), if !stopping => {
                stopping = true;
                timed_out = true;
                let _ = events.try_send((run_id.clone(), RunnerEvent::TimedOut));
                kill_at = begin_stop(&mut batch, "timeout reached");
            }
            _ = sleep_until(kill_at) => {
                batch.note("grace period over: sending SIGKILL to the process group");
                runner::signal_group(pid, Signal::KILL);
                kill_at = Instant::now() + FAR;
            }
            _ = notify.tick() => notify_screen(&mut notified),
        }
    };
    // Let the parser consume what the session still writes, then clear the group if stopping.
    let drained = tokio::time::timeout(DRAIN_AFTER_EXIT, &mut parsed_rx).await;
    if drained.is_err() && stopping {
        runner::signal_group(pid, Signal::KILL);
        let _ = tokio::time::timeout(DRAIN_AFTER_EXIT, &mut parsed_rx).await;
    }
    shared.closed.store(true, Ordering::SeqCst);
    if let Ok(mut s) = shared.screen.lock() {
        s.exited = true;
        s.bump();
    }
    if let Ok(mut m) = shared.master.lock() {
        m.take();
    }
    notify_screen(&mut notified);
    let reason = runner::cleanup_reason(timed_out, stop_kind, status);
    let cleanup = runner::run_cleanup(&spec, reason, &mut batch).await;
    batch.finish();
    runner::remove_temp(&spec.temp_files);
    crate::groups::forget(&spec.run_id);
    let _ = events
        .send((
            run_id,
            RunnerEvent::Finished(Box::new(FinishedRun {
                exit: status.map(runner::exit_info),
                spawn_error: None,
                cleanup,
            })),
        ))
        .await;
}
