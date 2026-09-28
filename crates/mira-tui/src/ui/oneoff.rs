//! The main pane of a one-off run: its logs or its details.

use mira_protocol::clock;
use mira_protocol::run::Lifecycle;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Padding, Paragraph};

use crate::app::{App, Focus, OneOff};
use crate::logs::display;
use crate::theme::{Theme, Tone};

use super::log_panel::{draw_records, log_bar};
use super::marks::oneoff_mark;
use super::text::{ago, cleanup_word, ellipsize, enum_word, fit_spans, short_id};
use super::widgets::{chip, panel, panel_title, tabs_line};

/// The main pane of a one-off `mira exec` run: a card like a tool's, then its logs or
/// the details of the run.
pub(super) fn draw_oneoff(f: &mut Frame, app: &mut App, t: &Theme, i: usize, area: Rect) {
    let focus = app.focus == Focus::Logs;
    let block =
        panel(t, panel_title(t, "One-off run", focus), focus).padding(Padding::horizontal(1));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let w = inner.width as usize;
    let o = &app.oneoffs[i];
    let label = display(&o.title());
    let how = format!("by {}", by(o));
    let card = vec![
        Line::from(fit_spans(
            vec![
                Span::styled(ellipsize(&label, w / 2), t.bold()),
                Span::raw("  "),
                chip(t, "one-off", Tone::Sky),
            ],
            w,
        )),
        Line::from(Span::styled(ellipsize(&how, w), t.muted())),
        Line::from(fit_spans(oneoff_status(app, t, o), w)),
        Line::from(Span::styled(
            ellipsize(
                "Worth keeping? Ask your agent to `mira save` it as a tool.",
                w,
            ),
            t.muted().add_modifier(Modifier::ITALIC),
        )),
    ];
    let details = oneoff_details(app, t, o, w);
    let rule = inner.height >= 16;
    let [card_area, _, tabs_area, rule_area, body] = Layout::vertical([
        Constraint::Length(card.len() as u16),
        Constraint::Length(u16::from(inner.height >= 14)),
        Constraint::Length(1),
        Constraint::Length(u16::from(rule)),
        Constraint::Min(1),
    ])
    .areas(inner);
    f.render_widget(Paragraph::new(card), card_area);
    if rule {
        f.render_widget(
            Paragraph::new(Span::styled("─".repeat(w), t.dim())),
            rule_area,
        );
    }
    let labels = ["Logs", "Details"];
    if app.oneoff_details {
        f.render_widget(Paragraph::new(tabs_line(t, &labels, 1, "", w)), tabs_area);
        f.render_widget(Paragraph::new(details), body);
        return;
    }
    let clock = app.clock;
    let Some(p) = app.oneoffs[i].pane.as_mut() else {
        f.render_widget(Paragraph::new(tabs_line(t, &labels, 0, "", w)), tabs_area);
        f.render_widget(Paragraph::new(Span::styled("loading…", t.muted())), body);
        return;
    };
    let bw = body.width as usize;
    p.height = body.height as usize;
    p.width = bw.saturating_sub(if bw >= 40 { 11 } else { 2 }).max(1);
    let bar = log_bar(p, None);
    f.render_widget(Paragraph::new(tabs_line(t, &labels, 0, &bar, w)), tabs_area);
    draw_records(f, p, t, focus, clock, body);
}

/// The one-off card's status line: a mark and a word, then details.
fn oneoff_status(app: &App, t: &Theme, o: &OneOff) -> Vec<Span<'static>> {
    let mark = oneoff_mark(o);
    let mut word = if o.stopping {
        "stopping…".to_owned()
    } else {
        match o.lifecycle {
            Lifecycle::Stopping { reason } => format!("stopping ({})", enum_word(reason)),
            l => l.word().to_owned(),
        }
    };
    let mut details: Vec<String> = Vec::new();
    match o.ended_at {
        Some(e) if !o.lifecycle.is_active() => {
            word = format!("{word} in {}", clock::span(o.started_at, e));
            details.push(ago(e));
            if let Some(x) = &o.exit {
                match (x.code, &x.signal) {
                    (Some(0), _) => {}
                    (Some(c), _) => details.push(format!("exit {c}")),
                    (None, Some(s)) => details.push(s.to_string()),
                    _ => {}
                }
            }
        }
        _ => details.push(format!(
            "started {} ({})",
            ago(o.started_at),
            app.clock.hm(o.started_at)
        )),
    }
    let mut spans = vec![
        Span::styled(format!("{} ", mark.glyph), mark.style(t)),
        Span::styled(word, mark.word_style(t).add_modifier(Modifier::BOLD)),
    ];
    if let Some(c) = o.cleanup.as_ref().and_then(cleanup_word) {
        spans.push(Span::styled(
            format!("  {c}"),
            t.word(Tone::Rose).add_modifier(Modifier::BOLD),
        ));
    }
    for d in details {
        spans.push(Span::styled(format!(" · {d}"), t.muted()));
    }
    spans
}

/// The Details tab of a one-off run: its record, field by field.
fn oneoff_details(app: &App, t: &Theme, o: &OneOff, w: usize) -> Vec<Line<'static>> {
    let row = |k: &str, v: String| {
        Line::from(vec![
            Span::styled(format!("{k:<10}"), t.muted()),
            Span::raw(ellipsize(&display(&v), w.saturating_sub(10))),
        ])
    };
    let id = o.run_id.to_string();
    let mut lines = vec![
        row("label", o.title()),
        row("by", by(o)),
        row(
            "state",
            format!("{} {}", oneoff_mark(o).glyph, o.lifecycle.word()),
        ),
        row(
            "started",
            format!(
                "{} ({})",
                app.clock.when_seconds(o.started_at),
                ago(o.started_at)
            ),
        ),
    ];
    if let Some(e) = o.ended_at {
        lines.push(row("ended", app.clock.when_seconds(e)));
        lines.push(row("took", clock::span(o.started_at, e)));
    } else {
        lines.push(row(
            "running",
            format!("{} so far", clock::age(clock::secs_since(o.started_at))),
        ));
    }
    if let Some(x) = &o.exit {
        let v = match (x.code, &x.signal) {
            (Some(c), _) => format!("code {c}"),
            (None, Some(s)) => format!("signal {s}"),
            _ => "none".into(),
        };
        lines.push(row("exit", v));
    }
    if let Some(c) = o.cleanup.as_ref().and_then(cleanup_word) {
        lines.push(row("cleanup", c));
    }
    lines.push(Line::from(""));
    for hint in [
        format!("`mira logs {}` prints its output.", short_id(&id)),
        format!("`mira runs {}` prints the full record.", short_id(&id)),
    ] {
        lines.push(Line::from(Span::styled(ellipsize(&hint, w), t.muted())));
    }
    lines
}

/// Who asked for the run: the agent's name and its task, or `You`.
fn by(o: &OneOff) -> String {
    match &o.requester {
        Some(r) => match &r.task {
            Some(task) => format!("{} · {task}", r.name),
            None => r.name.clone(),
        },
        None => "an agent or person (not recorded before Mira 0.14)".to_owned(),
    }
}
