//! The Output tab: the structured result of the shown run.

use mira_protocol::run::RunResult;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::logs::display;
use crate::theme::{Theme, Tone};

use super::text::ellipsize;

/// The result of the run shown in the Logs tab.
fn shown_result<'a>(app: &'a App, a: &mira_protocol::ids::ActionRef) -> Option<&'a RunResult> {
    match app.viewing.get(a) {
        Some(rec) => rec.result.as_ref(),
        None if app.active.contains_key(a) => None,
        None => app.last.get(a).and_then(|l| l.result.as_ref()),
    }
}

pub(super) fn draw_output_tab(
    f: &mut Frame,
    app: &mut App,
    t: &Theme,
    a: &mira_protocol::ids::ActionRef,
    area: Rect,
) {
    let w = area.width as usize;
    let Some(res) = shown_result(app, a) else {
        let hint = if app.active.contains_key(a) {
            "The result appears here when this run ends."
        } else if app.last.contains_key(a) || app.viewing.contains_key(a) {
            "This run returned no structured result. Its logs are in the Logs tab."
        } else {
            "Run the tool to see its result here."
        };
        let lines = vec![
            Line::from(""),
            Line::from(Span::styled("No output yet", t.bold())).centered(),
            Line::from(Span::styled(hint, t.muted())).centered(),
        ];
        f.render_widget(Paragraph::new(lines), area);
        return;
    };
    let (mark, tone) = if res.ok {
        ("✓ ok", Tone::Leaf)
    } else {
        ("✗ failed", Tone::Rose)
    };
    let mut lines: Vec<Line> = vec![Line::from(vec![
        Span::styled(mark, t.word(tone).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(
            ellipsize(&display(&res.summary), w.saturating_sub(10)),
            t.bold(),
        ),
    ])];
    if let Some(e) = &res.error {
        lines.push(Line::from(Span::styled(
            ellipsize(&display(&format!("[{}] {}", e.code, e.message)), w),
            t.word(Tone::Rose),
        )));
    }
    let mut body: Vec<String> = Vec::new();
    if let Some(pl) = &res.payload {
        body.push(format!(
            "The result is stored as a payload ({} bytes). Use `: runs get` to read it.",
            pl.size_bytes
        ));
    } else if !res.data.is_null() {
        body.extend(
            serde_json::to_string_pretty(&res.data)
                .unwrap_or_default()
                .lines()
                .map(display),
        );
    }
    let rows = (area.height as usize)
        .saturating_sub(lines.len() + 1)
        .max(1);
    let max = body.len().saturating_sub(rows);
    app.output_top = app.output_top.min(max);
    if !body.is_empty() {
        lines.push(Line::from(""));
    }
    for l in body.iter().skip(app.output_top).take(rows) {
        lines.push(Line::from(ellipsize(l, w)));
    }
    if max > 0 {
        let last = (app.output_top + rows).min(body.len());
        let n = lines.len();
        if let Some(l) = lines
            .get_mut(n.saturating_sub(1))
            .filter(|_| n as u16 >= area.height)
        {
            *l = Line::from(Span::styled(
                format!(
                    "… lines {}-{last} of {} · j/k scroll",
                    app.output_top + 1,
                    body.len()
                ),
                t.muted(),
            ));
        }
    }
    f.render_widget(Paragraph::new(lines), area);
}
