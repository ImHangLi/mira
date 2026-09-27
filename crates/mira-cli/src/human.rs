//! Small text helpers for human output: local clock times, durations, and intervals.
//! JSON output never uses these.

use std::sync::OnceLock;

use mira_protocol::Timestamp;
use mira_protocol::ids::RunId;
use mira_protocol::ipc::{SessionInfo, SessionMode, SessionState};
use mira_protocol::run::{CleanupState, RunRecord};
use time::{OffsetDateTime, UtcOffset};

static OFFSET: OnceLock<UtcOffset> = OnceLock::new();

/// Reads the local UTC offset. Call it while the process is still single-threaded.
pub fn init_local_offset() {
    let _ = OFFSET.set(UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC));
}

fn local(t: Timestamp) -> Option<OffsetDateTime> {
    let offset = OFFSET.get().copied().unwrap_or(UtcOffset::UTC);
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(t.unix_ms()) * 1_000_000)
        .ok()
        .map(|dt| dt.to_offset(offset))
}

/// `14:07` for today, `Sep 26 14:07` for another day.
pub fn clock(t: Timestamp) -> String {
    let (Some(dt), Some(now)) = (local(t), local(Timestamp::now())) else {
        return t.to_string();
    };
    let hm = format!("{:02}:{:02}", dt.hour(), dt.minute());
    if dt.date() == now.date() {
        hm
    } else {
        let month = &dt.month().to_string()[..3];
        format!("{month} {} {hm}", dt.day())
    }
}

/// `14:07:05` (local time).
pub fn clock_seconds(t: Timestamp) -> String {
    match local(t) {
        Some(dt) => format!("{:02}:{:02}:{:02}", dt.hour(), dt.minute(), dt.second()),
        None => t.to_string(),
    }
}

/// A compact duration: `450ms`, `3.2s`, `42s`, `5m10s`, `1h59m`, `24h`.
pub fn duration(ms: u64) -> String {
    if ms < 1000 {
        return format!("{ms}ms");
    }
    if ms < 10_000 {
        let tenths = ms / 100;
        return if tenths.is_multiple_of(10) {
            format!("{}s", tenths / 10)
        } else {
            format!("{}.{}s", tenths / 10, tenths % 10)
        };
    }
    let s = ms / 1000;
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    match (h, m, sec) {
        (0, 0, s) => format!("{s}s"),
        (0, m, 0) => format!("{m}m"),
        (0, m, s) => format!("{m}m{s}s"),
        (h, 0, _) => format!("{h}h"),
        (h, m, _) => format!("{h}h{m}m"),
    }
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
            let left = t
                .unix_ms()
                .saturating_sub(Timestamp::now().unix_ms())
                .max(0) as u64;
            let left = if left < 60_000 {
                "under 1m".to_owned()
            } else {
                duration(left.div_ceil(60_000) * 60_000)
            };
            format!("session: background, stops at {} (in {left})", clock(t))
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
    let end = r.ended_at.unwrap_or_else(Timestamp::now);
    duration(end.unix_ms().saturating_sub(r.started_at.unix_ms()).max(0) as u64)
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
    fn durations_are_compact() {
        assert_eq!(duration(450), "450ms");
        assert_eq!(duration(3200), "3.2s");
        assert_eq!(duration(3000), "3s");
        assert_eq!(duration(42_000), "42s");
        assert_eq!(duration(310_000), "5m10s");
        assert_eq!(duration(7_140_000), "1h59m");
        assert_eq!(duration(86_400_000), "24h");
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
