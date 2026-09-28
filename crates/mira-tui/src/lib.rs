//! The human TUI: a projection of host state over the shared typed client.
//!
//! The TUI never writes run state, never spawns project commands, and never blocks on IPC.
//! It joins the workspace session as a controller; closing it (q, Ctrl-C, SIGHUP) restores
//! the terminal at once and lets the host decide, by its session rules, whether owned work
//! stops.

mod app;
mod clip;
mod cmdbar;
mod form;
mod git;
mod ipc;
mod logs;
mod term;
mod terminal;
mod theme;
mod ui;
mod update;
mod views;

use std::time::{Duration, Instant};

use mira_client::connect;
use mira_protocol::error::{ErrorCode, ErrorInfo};
use mira_protocol::ipc::*;
use mira_protocol::paths::WorkspacePaths;
use tokio::sync::mpsc::unbounded_channel;

use crate::app::{App, Io, Quit};
use crate::ipc::{Event, Failure, Read, Tx};

/// Redraws caused by stream events are merged to at most ~30 frames per second.
/// How long a close after a signal may take before the process exits anyway.
const FORCE_EXIT_AFTER: Duration = Duration::from_secs(3);
const FRAME_GAP: Duration = Duration::from_millis(33);
/// Coarse tick for clocks and countdowns when nothing else happens.
const TICK: Duration = Duration::from_secs(1);
/// Events handled before a redraw gets a chance.
const EVENT_BATCH: usize = 4096;
/// How long the background color query (OSC 11) may delay the first frame.
const BACKGROUND_WAIT: Duration = Duration::from_millis(100);

pub enum TuiEnd {
    /// The workspace has no configuration; nothing was opened.
    NotSetup(ErrorInfo),
    /// The TUI closed; the line says what happens to the session's work.
    Closed(String),
}

/// Opens the TUI for a workspace. The caller has checked that stdin and stdout are TTYs.
pub fn run(paths: WorkspacePaths) -> Result<TuiEnd, ErrorInfo> {
    if theme::term_is_dumb() {
        return Err(ErrorInfo::new(
            ErrorCode::TTY_REQUIRED,
            "TERM=dumb cannot show the full-screen TUI",
        )
        .with_next_action(
            &["mira", "status"],
            "`mira status` prints the same state as plain text; add `--json` for structured output.",
        ));
    }
    // Read the local offset while the process is still single-threaded.
    let clock = mira_protocol::clock::LocalClock::detect();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| ErrorInfo::new(ErrorCode::INTERNAL, format!("cannot start runtime: {e}")))?;
    let res = rt.block_on(serve(paths, clock));
    // Do not wait for workers or the input thread: the host owns shutdown of the work.
    rt.shutdown_background();
    res
}

async fn serve(
    paths: WorkspacePaths,
    clock: mira_protocol::clock::LocalClock,
) -> Result<TuiEnd, ErrorInfo> {
    let mut control = connect(&paths, &ipc::options(ConnectionKind::Control))
        .await
        .map_err(|e| e.to_error_info())?;
    let (catalog, catalog_revision) = match ipc::call::<_, CatalogList>(
        &mut control,
        Method::CatalogList,
        &ipc::catalog_params(),
    )
    .await
    {
        Ok(a) => (Ok(a.data), a.catalog_revision),
        Err(Failure::Reply(e)) if e.code == ErrorCode::NOT_SETUP => {
            return Ok(TuiEnd::NotSetup(e));
        }
        Err(Failure::Lost(m)) => return Err(ErrorInfo::new(ErrorCode::INTERNAL, m)),
        Err(f) => (Err(f.info()), None),
    };
    let env = ClientEnv::capture().map_err(|m| ErrorInfo::new(ErrorCode::INVALID_ARGUMENT, m))?;
    let attached = ipc::call::<_, SessionData>(
        &mut control,
        Method::SessionAttach,
        &ipc::attach_params(&env),
    )
    .await;
    let reader = connect(&paths, &ipc::options(ConnectionKind::Control))
        .await
        .map_err(|e| e.to_error_info())?;
    let stream = connect(&paths, &ipc::options(ConnectionKind::Stream))
        .await
        .ok();

    let (tx, mut rx) = unbounded_channel::<Event>();
    let (control_tx, control_rx) = unbounded_channel();
    let (read_tx, read_rx) = unbounded_channel();
    tokio::spawn(ipc::control_worker(control, env, control_rx, tx.clone()));
    tokio::spawn(ipc::read_worker(
        reader,
        catalog_revision,
        read_rx,
        tx.clone(),
    ));
    tokio::spawn(ipc::stream_worker(paths.clone(), stream, tx.clone()));
    tokio::spawn(signals(tx.clone()));
    update::start(tx.clone());

    let mut app = App::new(
        paths.root.to_string(),
        clock,
        Io {
            control: control_tx,
            read: read_tx.clone(),
            events: tx.clone(),
            paths: paths.clone(),
        },
    );
    app.catalog_revision = catalog_revision;
    match attached {
        Ok(a) => app.set_attached(a.data.session),
        Err(f) => {
            let e = f.info();
            app.error(format!(
                "could not join the session: [{}] {}",
                e.code, e.message
            ));
        }
    }
    match catalog {
        Ok(c) => app.set_catalog(c),
        Err(e) => app.catalog_error = Some(e),
    }
    let _ = read_tx.send(Read::Recent);

    let color = theme::ColorMode::detect();
    theme::detect_glyphs();
    let mut guard = term::TerminalGuard::enter().map_err(|e| {
        ErrorInfo::new(
            ErrorCode::INTERNAL,
            format!("cannot open the terminal: {e}"),
        )
    })?;
    // The query must finish before the input thread starts, so its reply is not read as keys.
    let bg = background(color);
    spawn_input(tx.clone());
    cmdbar::defaults(app.root.clone(), tx.clone());
    let look = theme::Theme::new(color, bg);

    let mut last_draw = Instant::now() - FRAME_GAP;
    let mut dirty = true;
    let mut urgent = true;
    loop {
        if dirty && (urgent || last_draw.elapsed() >= FRAME_GAP) {
            if guard
                .terminal
                .draw(|f| ui::draw(f, &mut app, &look))
                .is_err()
            {
                app.quit
                    .get_or_insert(Quit::Normal(Some("terminal closed")));
            }
            last_draw = Instant::now();
            dirty = false;
            urgent = false;
        }
        if app.quit.is_some() {
            break;
        }
        let wait = if dirty {
            FRAME_GAP.saturating_sub(last_draw.elapsed())
        } else {
            TICK
        };
        tokio::select! {
            ev = rx.recv() => {
                let mut next = ev;
                let mut n = 0usize;
                while let Some(ev) = next {
                    // Input is drawn at the next chance; stream updates are merged.
                    if matches!(ev, Event::Input(_) | Event::Signal(_)) {
                        urgent = true;
                    }
                    app.handle(ev);
                    n += 1;
                    if n >= EVENT_BATCH || app.quit.is_some() {
                        break;
                    }
                    next = rx.try_recv().ok();
                }
                dirty = true;
                app.sync_terminal();
                if let Some(on) = app.mouse_changed.take() {
                    term::set_mouse(on);
                }
                for (title, message) in app.notifications.drain(..) {
                    term::notify(&title, &message);
                }
            }
            _ = tokio::time::sleep(wait) => {
                app.tick();
                dirty = true;
            }
        }
    }
    let message = match app.quit.take() {
        Some(Quit::Kept(until)) => crate::app::close_message(
            &crate::app::Close::Kept(until.map(|t| app.clock.hm(t))),
            app.run_count(),
        ),
        Some(Quit::Normal(Some(sig))) => format!("{sig}: {}", app.quit_message()),
        Some(Quit::Normal(None)) | None => app.quit_message(),
    };
    // Restore the outer terminal now; the host finishes any stop on its own.
    drop(guard);
    Ok(TuiEnd::Closed(message))
}

/// The terminal background: `MIRA_THEME=light|dark` when set, else the terminal's reply
/// to OSC 11, else `COLORFGBG`, else unknown.
fn background(color: theme::ColorMode) -> theme::Background {
    use theme::Background;
    if !color.enabled() {
        return Background::Unknown;
    }
    let var = |k: &str| std::env::var(k).ok();
    Background::from_override(var("MIRA_THEME").as_deref())
        .or_else(|| term::query_background(BACKGROUND_WAIT).map(Background::from_rgb))
        .or_else(|| Background::from_colorfgbg(var("COLORFGBG").as_deref()))
        .unwrap_or(Background::Unknown)
}

/// Terminal input on a plain thread; it ends with the process.
fn spawn_input(tx: Tx) {
    std::thread::spawn(move || {
        while let Ok(ev) = crossterm::event::read() {
            if tx.send(Event::Input(ev)).is_err() {
                return;
            }
        }
        // The terminal is gone (read failed). Close as for SIGHUP, which may never arrive,
        // so no TUI keeps running without a window.
        let _ = tx.send(Event::Signal("terminal closed"));
    });
}

/// Window close (SIGHUP) and TERM/INT follow the same close rules as `q`. The close normally
/// ends the process at once. If it does not within [`FORCE_EXIT_AFTER`], or a second signal
/// arrives, the process exits anyway: a draw can block forever on a terminal that nobody reads.
async fn signals(tx: Tx) {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut hup), Ok(mut term), Ok(mut int)) = (
        signal(SignalKind::hangup()),
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        return;
    };
    let next = async |hup: &mut tokio::signal::unix::Signal,
                      term: &mut tokio::signal::unix::Signal,
                      int: &mut tokio::signal::unix::Signal| {
        tokio::select! {
            _ = hup.recv() => "SIGHUP",
            _ = term.recv() => "SIGTERM",
            _ = int.recv() => "SIGINT",
        }
    };
    let name = next(&mut hup, &mut term, &mut int).await;
    let _ = tx.send(Event::Signal(name));
    tokio::select! {
        _ = tokio::time::sleep(FORCE_EXIT_AFTER) => {}
        _ = next(&mut hup, &mut term, &mut int) => {}
    }
    force_exit();
}

/// Restores the terminal if it still answers, then ends the process. The restore runs on
/// its own thread because writing to a stalled terminal blocks. `process::exit` is not
/// enough: its cleanup waits for the stdout lock, which the blocked draw holds.
fn force_exit() -> ! {
    std::thread::spawn(term::restore);
    std::thread::sleep(Duration::from_millis(300));
    let _ = rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::KILL);
    std::process::exit(1)
}
