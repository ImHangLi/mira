//! Terminal ownership: one guard enters raw mode and the alternate screen,
//! and every exit path (normal, error, panic) restores the outer terminal exactly once.

use std::io::{Stdout, Write};
use std::mem::ManuallyDrop;
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

static ACTIVE: AtomicBool = AtomicBool::new(false);

pub type Term = Terminal<CrosstermBackend<Stdout>>;

/// Restores the outer terminal. Safe to call more than once and from the panic hook.
pub fn restore() {
    if ACTIVE.swap(false, Ordering::SeqCst) {
        let mut out = std::io::stdout();
        let _ = disable_raw_mode();
        let _ = execute!(
            out,
            DisableMouseCapture,
            DisableBracketedPaste,
            LeaveAlternateScreen,
            Show
        );
        let _ = out.flush();
    }
}

pub struct TerminalGuard {
    /// Never dropped: ratatui's `Drop` prints to stderr when showing the cursor fails, and
    /// that print panics once the window is gone (SIGHUP). [`restore`] does its work instead.
    pub terminal: ManuallyDrop<Term>,
}

impl TerminalGuard {
    pub fn enter() -> std::io::Result<Self> {
        install_panic_hook();
        enable_raw_mode()?;
        ACTIVE.store(true, Ordering::SeqCst);
        let mut out = std::io::stdout();
        if let Err(e) = execute!(out, EnterAlternateScreen, Hide, EnableBracketedPaste) {
            restore();
            return Err(e);
        }
        match Terminal::new(CrosstermBackend::new(std::io::stdout())) {
            Ok(terminal) => Ok(Self {
                terminal: ManuallyDrop::new(terminal),
            }),
            Err(e) => {
                restore();
                Err(e)
            }
        }
    }
}

/// Asks the terminal for its background color (OSC 11) and waits at most `wait` for the
/// reply. Call it in raw mode, before anything else reads input. A Primary Device
/// Attributes request follows the query: every terminal answers it, in order, so a
/// terminal without OSC 11 support ends the wait at once. Both replies are read here and
/// never reach the key input.
pub fn query_background(wait: Duration) -> Option<(u8, u8, u8)> {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    use rustix::io::Errno;

    let mut out = std::io::stdout();
    out.write_all(b"\x1b]11;?\x07\x1b[c").ok()?;
    out.flush().ok()?;
    let stdin = std::io::stdin();
    let fd = stdin.as_fd();
    let end = Instant::now() + wait;
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    let mut chunk = [0u8; 128];
    while !has_device_attributes(&buf) && buf.len() < 1024 {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let Ok(timeout) = Timespec::try_from(left) else {
            break;
        };
        let mut fds = [PollFd::new(&fd, PollFlags::IN)];
        match poll(&mut fds, Some(&timeout)) {
            Ok(0) => break,
            Ok(_) => {}
            Err(Errno::INTR) => continue,
            Err(_) => break,
        }
        match rustix::io::read(fd, &mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(Errno::INTR | Errno::AGAIN) => continue,
            Err(_) => break,
        }
    }
    crate::theme::parse_osc11(&buf)
}

/// Whether `buf` holds a complete Primary Device Attributes reply (`ESC [ ? … c`).
fn has_device_attributes(buf: &[u8]) -> bool {
    buf.windows(3)
        .position(|w| w == b"\x1b[?")
        .is_some_and(|i| {
            buf[i + 3..]
                .iter()
                .find(|b| !(b.is_ascii_digit() || **b == b';'))
                .is_some_and(|&b| b == b'c')
        })
}

/// Application mouse mode; off by default so the terminal's own selection works.
pub fn set_mouse(on: bool) {
    let mut out = std::io::stdout();
    let _ = if on {
        execute!(out, EnableMouseCapture)
    } else {
        execute!(out, DisableMouseCapture)
    };
}

/// Asks the terminal for a desktop notification (OSC 9). Ghostty, iTerm2, and WezTerm show
/// it as a system notification, which the user allows once for the terminal app; other
/// terminals ignore it. The host already rejected control characters; this strips them again
/// so nothing can end the sequence early.
pub fn notify(title: &str, message: &str) {
    let clean = |s: &str| s.chars().filter(|c| !c.is_control()).collect::<String>();
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]9;{}: {}\x07", clean(title), clean(message));
    let _ = out.flush();
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore();
    }
}

fn install_panic_hook() {
    static INSTALLED: AtomicBool = AtomicBool::new(false);
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Restore first so the panic message lands on the normal screen.
        restore();
        previous(info);
    }));
}
