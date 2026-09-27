//! Small text helpers for human output: intervals, sessions, and cleanup failures.
//! JSON output never uses these.

use std::sync::OnceLock;

use mira_protocol::Timestamp;
use mira_protocol::clock::{self, LocalClock};
use mira_protocol::ids::RunId;
use mira_protocol::ipc::{SessionInfo, SessionMode, SessionState};
use mira_protocol::run::{CleanupState, RunRecord};

static CLOCK: OnceLock<LocalClock> = OnceLock::new();

/// Reads the local UTC offset. Call it while the process is still single-threaded.
pub fn init_local_offset() {
    let _ = CLOCK.set(LocalClock::detect());
}

/// The local clock read by [`init_local_offset`].
pub fn clock() -> LocalClock {
    CLOCK.get().copied().unwrap_or(LocalClock::UTC)
}

/// A schedule interval: `every 2s`, `every 5m`, `every 24h`, `every 1h30m`.
pub fn interval(every_ms: u64) -> String {
    let s = every_ms / 1000;
    let text = if !every_ms.is_multiple_of(1000) || s == 0 {
        format!("{every_ms}ms")
    } else {
        let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
        let mut out = String::new();
        if h > 0 {
            out.push_str(&format!("{h}h"));
        }
        if m > 0 {
            out.push_str(&format!("{m}m"));
        }
        if sec > 0 {
            out.push_str(&format!("{sec}s"));
        }
        out
    };
    format!("every {text}")
}

/// `r_fb60aacf`: the run ID as the TUI shows it.
pub fn short_run(id: &RunId) -> String {
    id.as_str().chars().take(10).collect()
}

/// `1 window`, `2 windows`.
pub fn count(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// `session: background, stops at 16:07 (in 1h59m)` or `session: open in 1 window`.
pub fn session_line(s: Option<&SessionInfo>) -> String {
    let Some(s) = s else {
        return "session: none".into();
    };
    if s.state == SessionState::Stopping {
        return "session: stopping".into();
    }
    match (s.mode, s.expires_at) {
        (SessionMode::Background, Some(t)) => {
            format!("session: background, stops at {}", clock().until(t))
        }
        (SessionMode::Background, None) => "session: background, no time limit".into(),
        (SessionMode::Foreground, _) => format!(
            "session: open in {}",
            count(u64::from(s.controller_count), "window", "windows")
        ),
    }
}

/// `cleanup failed (exit 4)` or `cleanup timed out` when cleanup failed; `None` otherwise.
/// Other failures carry the host's message.
pub fn cleanup_failure(c: &CleanupState) -> Option<String> {
    match c {
        CleanupState::Failed {
            timed_out: true, ..
        } => Some("cleanup timed out".into()),
        CleanupState::Failed {
            exit_code: Some(code),
            ..
        } => Some(format!("cleanup failed (exit {code})")),
        CleanupState::Failed { error, .. } => Some(format!("cleanup failed: {}", error.message)),
        _ => None,
    }
}

/// How long a run took (or has been running).
pub fn run_duration(r: &RunRecord) -> String {
    clock::span(r.started_at, r.ended_at.unwrap_or_else(Timestamp::now))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intervals_read_like_people_say_them() {
        assert_eq!(interval(2000), "every 2s");
        assert_eq!(interval(86_400_000), "every 24h");
        assert_eq!(interval(300_000), "every 5m");
        assert_eq!(interval(5_400_000), "every 1h30m");
        assert_eq!(interval(90_000), "every 1m30s");
        assert_eq!(interval(1500), "every 1500ms");
    }

    #[test]
    fn cleanup_failure_names_the_exit_status() {
        let c = CleanupState::Failed {
            ended_at: Timestamp::from_unix_ms(0),
            exit_code: Some(4),
            timed_out: false,
            error: mira_protocol::ErrorInfo::new(
                mira_protocol::ErrorCode::EXECUTION_FAILED,
                "cleanup exited with status 4",
            ),
        };
        assert_eq!(
            cleanup_failure(&c).as_deref(),
            Some("cleanup failed (exit 4)")
        );
        assert_eq!(cleanup_failure(&CleanupState::NotNeeded), None);
    }
}
