//! Text helpers: cutting, padding, and wrapping to cell widths, and short state words.

use mira_protocol::clock;
use mira_protocol::run::{CleanupState, RunRecord};
use mira_protocol::time::Timestamp;
use mira_protocol::view::Freshness;
use ratatui::text::Span;

use crate::logs::{cells, slice_cells};
use crate::views::freshness_word;

pub(super) fn ago(ts: Timestamp) -> String {
    format!("{} ago", clock::age(clock::secs_since(ts)))
}

pub(super) fn short_root(root: &str, max: usize) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let r = if !home.is_empty() && root.starts_with(&home) {
        format!("~{}", &root[home.len()..])
    } else {
        root.to_owned()
    };
    if cells(&r) <= max {
        return r;
    }
    let tail: Vec<&str> = r.rsplit('/').take(2).collect();
    let s = format!(
        "…/{}/{}",
        tail.get(1).unwrap_or(&""),
        tail.first().unwrap_or(&"")
    );
    ellipsize(&s, max)
}

/// Longest branch name the header shows before it cuts the end.
pub(super) const BRANCH_MAX: usize = 24;
/// Shortest branch the header keeps before it drops the chip.
pub(super) const BRANCH_MIN: usize = 8;

/// Cuts `s` to `max` cells, marking the cut with one `…`.
pub(super) fn ellipsize(s: &str, max: usize) -> String {
    if cells(s) <= max {
        return s.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    format!("{}…", slice_cells(s, 0, max - 1))
}

/// `s` cut or padded to exactly `w` cells.
pub(super) fn pad(s: &str, w: usize) -> String {
    let s = ellipsize(s, w);
    let n = cells(&s);
    format!("{s}{}", " ".repeat(w.saturating_sub(n)))
}

/// Word-wraps `text` to `width` cells; words longer than a line are cut into pieces.
pub(super) fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let mut word = word.to_owned();
        loop {
            let used = cells(&line);
            let gap = usize::from(used > 0);
            if used + gap + cells(&word) <= width {
                if gap == 1 {
                    line.push(' ');
                }
                line.push_str(&word);
                break;
            }
            if used > 0 {
                out.push(std::mem::take(&mut line));
                continue;
            }
            // A word wider than the whole line.
            out.push(slice_cells(&word, 0, width));
            word = slice_cells(&word, width, cells(&word) - width);
            if word.is_empty() {
                break;
            }
        }
    }
    if !line.is_empty() || out.is_empty() {
        out.push(line);
    }
    out
}

pub(super) fn line_cells(spans: &[Span]) -> usize {
    spans.iter().map(|s| cells(&s.content)).sum()
}

/// Cuts a span list to `w` cells.
pub(super) fn fit_spans(spans: Vec<Span<'static>>, w: usize) -> Vec<Span<'static>> {
    let mut out = Vec::with_capacity(spans.len());
    let mut left = w;
    for s in spans {
        if left == 0 {
            break;
        }
        let n = cells(&s.content);
        if n <= left {
            left -= n;
            out.push(s);
        } else {
            out.push(Span::styled(ellipsize(&s.content, left), s.style));
            break;
        }
    }
    out
}

pub(super) fn short_id(id: &str) -> String {
    slice_cells(id, 0, 10)
}

/// The run's freshness word; runs without provenance read as historical.
pub(super) fn freshness_of(rec: &RunRecord) -> &'static str {
    freshness_word(
        rec.provenance
            .as_ref()
            .map_or(Freshness::Historical, |p| p.freshness),
    )
}

/// A failed cleanup in a few words: `cleanup failed (exit 4)`, `cleanup timed out`;
/// `None` when cleanup did not fail.
pub(super) fn cleanup_word(c: &CleanupState) -> Option<String> {
    match c {
        CleanupState::Failed {
            timed_out: true, ..
        } => Some("cleanup timed out".into()),
        CleanupState::Failed {
            exit_code: Some(code),
            ..
        } => Some(format!("cleanup failed (exit {code})")),
        CleanupState::Failed { .. } => Some("cleanup failed".into()),
        _ => None,
    }
}

pub(super) fn enum_word<T: serde::Serialize>(v: T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{cleanup_word, ellipsize, wrap};
    use mira_protocol::error::{ErrorCode, ErrorInfo};
    use mira_protocol::run::CleanupState;
    use mira_protocol::time::Timestamp;

    #[test]
    fn a_failed_cleanup_reads_as_a_short_phrase() {
        let failed = |exit_code, timed_out| CleanupState::Failed {
            ended_at: Timestamp::now(),
            exit_code,
            timed_out,
            error: ErrorInfo::new(ErrorCode::EXECUTION_FAILED, "cleanup failed"),
        };
        assert_eq!(
            cleanup_word(&failed(Some(4), false)).as_deref(),
            Some("cleanup failed (exit 4)")
        );
        assert_eq!(
            cleanup_word(&failed(None, true)).as_deref(),
            Some("cleanup timed out")
        );
        assert_eq!(
            cleanup_word(&failed(None, false)).as_deref(),
            Some("cleanup failed")
        );
        assert_eq!(cleanup_word(&CleanupState::NotNeeded), None);
    }

    #[test]
    fn ellipsize_marks_the_cut() {
        assert_eq!(
            ellipsize("Tutorial app (dev server)", 15),
            "Tutorial app (…"
        );
        assert_eq!(ellipsize("Lint", 15), "Lint");
        assert_eq!(ellipsize("Lint", 0), "");
    }

    #[test]
    fn wrap_keeps_words_within_the_width() {
        assert_eq!(
            wrap("the host stops owned work only when", 12),
            ["the host", "stops owned", "work only", "when"]
        );
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("", 4), [""]);
    }
}
