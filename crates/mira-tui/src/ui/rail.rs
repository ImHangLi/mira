//! The right rail on wide terminals: recent runs and the session.

use mira_protocol::clock;
use mira_protocol::ipc::{SessionMode, SessionState};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::logs::cells;
use crate::theme::{Theme, Tone};

use super::RAIL_RUNS;
use super::marks::life_mark;
use super::text::{ago, ellipsize};
use super::widgets::{panel, panel_title};

pub(super) fn draw_rail(f: &mut Frame, app: &App, t: &Theme, area: Rect) {
    let runs_h = (RAIL_RUNS as u16 + 2).min(area.height / 2);
    let [runs_area, session_area] =
        Layout::vertical([Constraint::Length(runs_h), Constraint::Min(3)]).areas(area);
    let block = panel(t, panel_title(t, "Recent runs", false), false);
    let inner = block.inner(runs_area);
    f.render_widget(block, runs_area);
    let w = inner.width as usize;
    let mut lines: Vec<Line> = Vec::new();
    match app.selected_ref() {
        None if app.selected_oneoff().is_some() => lines.push(Line::from(Span::styled(
            " A one-off run has no history.",
            t.muted(),
        ))),
        None => lines.push(Line::from(Span::styled(" Views have no runs.", t.muted()))),
        Some(a) => match app.recent.get(&a) {
            Some(r) if !r.is_empty() => {
                for rec in r.iter().take(inner.height as usize) {
                    let m = life_mark(rec.lifecycle);
                    let dur = match rec.ended_at {
                        Some(e) => clock::span(rec.started_at, e),
                        None => "running".into(),
                    };
                    let when = ago(rec.ended_at.unwrap_or(rec.started_at));
                    let left = format!(" {} {when}", m.glyph);
                    let gap = w.saturating_sub(cells(&left) + cells(&dur) + 1);
                    lines.push(Line::from(vec![
                        Span::raw(" "),
                        Span::styled(m.glyph, m.style(t)),
                        Span::raw(format!(" {when}")),
                        Span::raw(" ".repeat(gap)),
                        Span::styled(dur, t.muted()),
                        Span::raw(" "),
                    ]));
                }
            }
            _ if app.last.contains_key(&a) || app.active.contains_key(&a) => {
                lines.push(Line::from(Span::styled(" reading…", t.muted())))
            }
            _ => lines.push(Line::from(Span::styled(" No runs yet.", t.muted()))),
        },
    }
    f.render_widget(Paragraph::new(lines), inner);

    let block = panel(t, panel_title(t, "Session", false), false);
    let inner = block.inner(session_area);
    f.render_widget(block, session_area);
    let w = inner.width as usize;
    let row = |k: &str, v: String, st: Style| {
        Line::from(vec![
            Span::styled(format!(" {k:<12}"), t.muted()),
            Span::styled(ellipsize(&v, w.saturating_sub(14)), st),
        ])
    };
    let mut lines = Vec::new();
    match &app.session {
        None => lines.push(row("mode", "○ no session".into(), t.muted())),
        Some(s) => {
            let (mode, st) = match (s.state, s.mode) {
                (SessionState::Stopping, _) => ("◐ stopping", t.word(Tone::Amber)),
                (_, SessionMode::Foreground) => ("● foreground", t.word(Tone::Leaf)),
                (_, SessionMode::Background) => ("◐ background", t.word(Tone::Amber)),
            };
            lines.push(row("mode", mode.into(), st));
            lines.push(row(
                "windows",
                format!("{} open", s.controller_count),
                Style::default(),
            ));
            let exp = match s.expires_at {
                Some(e) => app.clock.until(e),
                None if s.background_lease => "at `mira down`".into(),
                None => "last window closes".into(),
            };
            lines.push(row("ends", exp, Style::default()));
        }
    }
    let mut active = format!("{} run(s)", app.active.len());
    if app.adhoc_runs > 0 {
        active.push_str(&format!(" + {} one-off", app.adhoc_runs));
    }
    lines.push(row("active", active, Style::default()));
    lines.push(row(
        "this window",
        if app.is_controller() {
            "in control".into()
        } else {
            "watching".into()
        },
        Style::default(),
    ));
    lines.push(row(
        "mouse",
        if app.mouse { "on" } else { "off" }.into(),
        Style::default(),
    ));
    f.render_widget(Paragraph::new(lines), inner);
}
