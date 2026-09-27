//! The Logs tab: the log status bar and the visible log records.

use mira_protocol::clock::LocalClock;
use mira_protocol::run::LogStream;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{App, Focus, Tab};
use crate::logs::{LogPane, cells, display, slice_cells};
use crate::theme::{Theme, Tone};

use super::text::{ellipsize, freshness_of};
use super::widgets::tab_bar;

/// The log status: follow or pin, the run, and anything the view is missing.
pub(super) fn log_bar(p: &LogPane, old: Option<&String>) -> String {
    let mut parts: Vec<String> = Vec::new();
    // An older run chosen in the history list reads as history, never as the current run.
    if let Some(o) = old {
        parts.push(o.clone());
    }
    if p.is_pinned() {
        parts.push(format!("PINNED · {} newer below", p.below()));
    } else {
        parts.push("FOLLOW".into());
    }
    if p.wrap {
        parts.push("wrap".into());
    } else if p.hscroll > 0 {
        parts.push(format!("col +{}", p.hscroll));
    }
    if p.loading || p.loading_older {
        parts.push("loading…".into());
    }
    if p.lost_anchor || p.trimmed {
        parts.push("older lines trimmed from this view".into());
    }
    if let Some((a, b)) = p.gap {
        parts.push(format!("#{a}-#{b} skipped by a stream reset"));
    }
    if p.history_gone()
        && let Some(first) = p.first_available
    {
        parts.push(format!(
            "earlier output no longer available (starts at #{first})"
        ));
    }
    parts.join(" · ")
}

pub(super) fn draw_logs(
    f: &mut Frame,
    app: &mut App,
    t: &Theme,
    a: &mira_protocol::ids::ActionRef,
    tabs_area: Rect,
    body: Rect,
) {
    let w = body.width as usize;
    let gutter = if w >= 40 { 11 } else { 2 };
    let text_w = w.saturating_sub(gutter).max(1);
    let focus = app.focus == Focus::Logs;
    let clock = app.clock;
    let old = app.viewing.get(a).map(|rec| {
        let when = rec
            .ended_at
            .map_or(String::new(), |e| format!(" at {}", app.clock.hm(e)));
        (
            rec.run_id.clone(),
            format!(
                "{} RUN · {}{when}",
                freshness_of(rec).to_uppercase(),
                rec.lifecycle.word()
            ),
        )
    });
    // A PTY run that prints nothing is often waiting for input: show its screen's last line.
    let waiting = app.silent_pty_run().map(|r| app.screens.get(r).cloned());
    let Some(p) = app.panes.get_mut(a) else {
        f.render_widget(Paragraph::new(tab_bar(t, Tab::Logs, "", w)), tabs_area);
        f.render_widget(Paragraph::new(Span::styled("loading…", t.muted())), body);
        return;
    };
    p.height = body.height as usize;
    p.width = text_w;
    let old = old
        .as_ref()
        .filter(|o| p.run_id.as_ref() == Some(&o.0))
        .map(|o| &o.1);
    let bar = log_bar(p, old);
    let mut tabs = tab_bar(t, Tab::Logs, &bar, w);
    if old.is_some() {
        // Historical runs stand out: the whole bar is amber.
        tabs = tabs.patch_style(t.word(Tone::Amber));
    }
    f.render_widget(Paragraph::new(tabs), tabs_area);
    if p.records.is_empty()
        && p.error.is_none()
        && !p.loading
        && let Some(screen) = waiting
    {
        let mut lines = Vec::new();
        if let Some(l) = screen {
            lines.push(Line::from(Span::styled(
                ellipsize(&display(&l), w),
                t.muted(),
            )));
        }
        lines.push(Line::from(vec![
            Span::styled("waiting for input", t.word(Tone::Amber)),
            Span::styled(" · ", t.muted()),
            Span::styled("Enter", t.bold()),
            Span::styled(" to use it", t.muted()),
        ]));
        f.render_widget(Paragraph::new(lines), body);
        return;
    }
    draw_records(f, p, t, focus, clock, body);
}

/// The lines of a log panel, or why there are none.
pub(super) fn draw_records(
    f: &mut Frame,
    p: &LogPane,
    t: &Theme,
    focus: bool,
    clock: LocalClock,
    body: Rect,
) {
    let w = body.width as usize;
    let gutter = if w >= 40 { 11 } else { 2 };
    let text_w = w.saturating_sub(gutter).max(1);
    let sel = t.selected();
    if p.records.is_empty() {
        let msg = if let Some(e) = &p.error {
            format!("Cannot read logs: {e}")
        } else if p.loading {
            "loading…".to_owned()
        } else if p.run_id.is_none() {
            "No runs yet. Press Enter or s to start it.".to_owned()
        } else {
            "(no output yet)".to_owned()
        };
        f.render_widget(Paragraph::new(Span::styled(msg, t.muted())), body);
        return;
    }
    let cursor = if focus && p.is_pinned() {
        p.cursor_at()
    } else {
        None
    };
    let rows = p.visible();
    let mut lines = Vec::with_capacity(rows.len());
    for (idx, row) in rows {
        let r = &p.records[idx];
        let text = display(&r.text);
        let part = if p.wrap {
            slice_cells(&text, row * text_w, text_w)
        } else {
            slice_cells(&text, p.hscroll, text_w)
        };
        let style = if matches!(r.stream, LogStream::Host | LogStream::Plugin) {
            t.muted()
        } else {
            Style::default()
        };
        let picked = p.in_selection(idx) && (p.anchor.is_some() || Some(idx) == cursor);
        let mut spans = Vec::with_capacity(4);
        if gutter > 2 {
            let at = if row == 0 {
                clock.hms(r.recorded_at)
            } else {
                "        ".into()
            };
            spans.push(Span::styled(at, t.muted()));
            spans.push(Span::raw(" "));
        }
        // A thin amber bar marks stderr; host lines get a dim dot.
        let (mark, mstyle) = match r.stream {
            LogStream::Stderr => ("▎", t.fg(Tone::Amber)),
            LogStream::Host if row == 0 => ("·", t.muted()),
            _ => (" ", Style::default()),
        };
        spans.push(Span::styled(mark, mstyle));
        spans.push(Span::raw(" "));
        if picked {
            let n = cells(&part);
            spans.push(Span::styled(
                format!("{part}{}", " ".repeat(text_w.saturating_sub(n))),
                sel,
            ));
        } else {
            spans.push(Span::styled(part, style));
        }
        lines.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(lines), body);
}
