//! Local clock times, durations, and ages for human output (CLI text and the TUI).
//! JSON output never uses these.

use time::{OffsetDateTime, UtcOffset};

use crate::time::Timestamp;

/// Formats timestamps in one local UTC offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalClock(UtcOffset);

impl LocalClock {
    pub const UTC: Self = Self(UtcOffset::UTC);

    /// The local UTC offset, or UTC when it cannot be read. Call it while the process is
    /// still single-threaded.
    pub fn detect() -> Self {
        Self(UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC))
    }

    fn local(self, t: Timestamp) -> Option<OffsetDateTime> {
        OffsetDateTime::from_unix_timestamp_nanos(i128::from(t.unix_ms()) * 1_000_000)
            .ok()
            .map(|dt| dt.to_offset(self.0))
    }

    /// `14:07`.
    pub fn hm(self, t: Timestamp) -> String {
        self.local(t).map_or_else(
            || "--:--".into(),
            |d| format!("{:02}:{:02}", d.hour(), d.minute()),
        )
    }

    /// `14:07:05`.
    pub fn hms(self, t: Timestamp) -> String {
        self.local(t).map_or_else(
            || "--:--:--".into(),
            |d| format!("{:02}:{:02}:{:02}", d.hour(), d.minute(), d.second()),
        )
    }

    /// `14:07` for today, `Sep 26 14:07` for another day.
    pub fn when(self, t: Timestamp) -> String {
        match (self.local(t), self.local(Timestamp::now())) {
            (Some(d), Some(now)) if d.date() != now.date() => {
                format!("{} {} {}", month_word(d), d.day(), self.hm(t))
            }
            _ => self.hm(t),
        }
    }

    /// A future time and how long until it: `16:07 (in 1h52m)`.
    pub fn until(self, t: Timestamp) -> String {
        format!("{} (in {})", self.when(t), time_left(-secs_since(t)))
    }

    /// `14:07:05` for today, `Sep 26 14:07` for another day.
    pub fn when_seconds(self, t: Timestamp) -> String {
        match (self.local(t), self.local(Timestamp::now())) {
            (Some(d), Some(now)) if d.date() != now.date() => self.when(t),
            _ => self.hms(t),
        }
    }
}

fn month_word(d: OffsetDateTime) -> String {
    d.month().to_string().chars().take(3).collect()
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

/// The time between two timestamps as a [`duration`]; negative spans read as zero.
pub fn span(from: Timestamp, to: Timestamp) -> String {
    duration(u64::try_from(to.unix_ms().saturating_sub(from.unix_ms())).unwrap_or(0))
}

/// A relative age in one unit: `12s`, `3m`, `2h`, `4d`.
pub fn age(secs: i64) -> String {
    let s = secs.max(0);
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86_400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

/// Time left in at most two units: `42s`, `12m`, `1h52m`.
pub fn time_left(secs: i64) -> String {
    let s = secs.max(0);
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        _ => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

/// Seconds from `t` until now; negative for a future `t`.
pub fn secs_since(t: Timestamp) -> i64 {
    (Timestamp::now().unix_ms() - t.unix_ms()) / 1000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_are_compact() {
        assert_eq!(duration(450), "450ms");
        assert_eq!(duration(3200), "3.2s");
        assert_eq!(duration(3000), "3s");
        assert_eq!(duration(42_000), "42s");
        assert_eq!(duration(310_000), "5m10s");
        assert_eq!(duration(7_140_000), "1h59m");
        assert_eq!(duration(86_400_000), "24h");
        let t = Timestamp::from_unix_ms(10_000);
        assert_eq!(span(t, Timestamp::from_unix_ms(5_000)), "0ms");
    }

    #[test]
    fn ages_are_short() {
        assert_eq!(age(-5), "0s");
        assert_eq!(age(12), "12s");
        assert_eq!(age(185), "3m");
        assert_eq!(age(7300), "2h");
        assert_eq!(age(3 * 86_400 + 5), "3d");
        assert_eq!(time_left(42), "42s");
        assert_eq!(time_left(6720), "1h52m");
    }

    #[test]
    fn clock_times_use_the_offset() {
        let t = Timestamp::from_unix_ms(3_723_000);
        assert_eq!(LocalClock::UTC.hm(t), "01:02");
        assert_eq!(LocalClock::UTC.hms(t), "01:02:03");
        let plus_two = LocalClock(UtcOffset::from_hms(2, 0, 0).expect("offset"));
        assert_eq!(plus_two.hm(t), "03:02");
        assert!(LocalClock::UTC.when(t).starts_with("Jan 1 "));
    }
}
