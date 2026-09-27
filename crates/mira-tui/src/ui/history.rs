//! The History tab: the newest runs of the selected action.

use mira_protocol::clock;
use mira_protocol::run::RunRecord;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{App, Modal};
use crate::logs::{cells, display};
use crate::theme::{Theme, Tone};

use super::marks::life_mark;
use super::text::{cleanup_word, ellipsize, fit_spans, freshness_of, line_cells, pad, short_id};

pub(super) fn draw_history(f: &mut Frame, app: &App, t: &Theme, area: Rect) {
    let Modal::History {
        runs, error, index, ..
    } = &app.modal
    else {
        return;
    };
    let w = area.width as usize;
    let a = app.selected_ref();
    let shown = a
        .as_ref()
        .and_then(|a| app.panes.get(a))
        .and_then(|p| p.run_id.as_ref());
    // The outcome column grows to fit a failed cleanup next to the outcome.
    let outcome_of = |rec: &RunRecord| {
        let mut outcome = rec.lifecycle.word().to_owned();
        if let Some(c) = rec.exit.as_ref().and_then(|e| e.code)
            && c != 0
        {
            outcome = format!("{outcome} (exit {c})");
        }
        (outcome, cleanup_word(&rec.cleanup))
    };
    let outcome_w = runs
        .iter()
        .flatten()
        .map(|rec| match outcome_of(rec) {
            (o, Some(c)) => cells(&o) + 2 + cells(&c) + 2,
            (o, None) => cells(&o) + 2,
        })
        .max()
        .unwrap_or(0)
        .clamp(18, 44);
    let rest = |dur: &str, id: &str, state: &str| format!("{}{}{state}", pad(dur, 12), pad(id, 13));
    let mut lines = vec![Line::from(Span::styled(
        ellipsize(
            &format!(
                "  {}{}{}",
                pad("started", 14),
                pad("outcome", outcome_w),
                rest("took", "run", "state")
            ),
            w,
        ),
        t.muted().add_modifier(Modifier::BOLD),
    ))];
    match (runs, error) {
        (_, Some(e)) => lines.push(Line::from(Span::styled(
            ellipsize(&display(&format!("Cannot read run history: {e}")), w),
            t.word(Tone::Rose),
        ))),
        (None, None) => lines.push(Line::from(Span::styled("reading…", t.muted()))),
        (Some(r), None) if r.is_empty() => {
            lines.push(Line::from(Span::styled("No runs recorded yet.", t.muted())))
        }
        (Some(r), None) => {
            let body_h = (area.height as usize).saturating_sub(1).max(1);
            let skip = (index + 1).saturating_sub(body_h);
            for (i, rec) in r.iter().enumerate().skip(skip).take(body_h) {
                let m = life_mark(rec.lifecycle);
                let (outcome, cleanup) = outcome_of(rec);
                let dur = match rec.ended_at {
                    Some(e) => clock::span(rec.started_at, e),
                    None => format!("{} so far", clock::age(clock::secs_since(rec.started_at))),
                };
                let mut state = if rec.lifecycle.is_active() {
                    "current".to_owned()
                } else {
                    freshness_of(rec).to_owned()
                };
                if Some(&rec.run_id) == shown {
                    state.push_str(" · shown");
                }
                let selected = i == *index;
                let (base, glyph_st, rose) = if selected {
                    let s = t.selected();
                    (s, s, s.add_modifier(Modifier::BOLD))
                } else {
                    (
                        Style::default(),
                        m.style(t),
                        t.word(Tone::Rose).add_modifier(Modifier::BOLD),
                    )
                };
                let mut used = cells(&outcome);
                let mut spans = vec![
                    Span::styled(format!("{} ", m.glyph), glyph_st),
                    Span::styled(pad(&app.clock.when_seconds(rec.started_at), 14), base),
                    Span::styled(outcome, base),
                ];
                if let Some(c) = cleanup {
                    used += 2 + cells(&c);
                    spans.push(Span::styled("  ", base));
                    spans.push(Span::styled(c, rose));
                }
                spans.push(Span::styled(
                    " ".repeat(outcome_w.saturating_sub(used).max(1)),
                    base,
                ));
                spans.push(Span::styled(
                    rest(&dur, &short_id(&rec.run_id.to_string()), &state),
                    base,
                ));
                if selected {
                    let n = line_cells(&spans);
                    spans.push(Span::styled(" ".repeat(w.saturating_sub(n)), base));
                }
                lines.push(Line::from(fit_spans(spans, w)));
            }
        }
    }
    f.render_widget(Paragraph::new(lines), area);
}
