//! Shared widgets: bordered panels, empty-state cards, chips, and tab bars.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Padding, Paragraph};

use crate::app::{App, Hit, Tab};
use crate::logs::{cells, display};
use crate::theme::{Theme, Tone};

use super::overlays::centered;
use super::text::{ellipsize, line_cells, wrap};

pub(super) fn panel<'a>(t: &Theme, title: impl Into<Line<'a>>, focus: bool) -> Block<'a> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(t.border(focus))
        .title(title)
}

/// The focused panel uses the same accent chip as keys and selected rows.
pub(super) fn panel_title(t: &Theme, text: &str, focus: bool) -> Line<'static> {
    let st = if focus {
        t.chip(Tone::Accent)
    } else {
        t.muted()
    };
    Line::from(Span::styled(format!(" {text} "), st))
}

/// A centered card with a bold headline and a dim hint, for empty states.
pub(super) fn empty_card(f: &mut Frame, t: &Theme, area: Rect, head: &str, hint: &str) {
    let block = panel(t, "", false);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let w = inner.width.saturating_sub(4).min(64);
    let hint_lines = wrap(hint, w.saturating_sub(4) as usize);
    let h = (hint_lines.len() as u16 + 4).min(inner.height);
    let rect = centered(inner, w, h);
    let mut lines = vec![
        Line::from(Span::styled(
            head.to_owned(),
            t.word(Tone::Accent).add_modifier(Modifier::BOLD),
        ))
        .centered(),
        Line::from(""),
    ];
    lines.extend(
        hint_lines
            .into_iter()
            .map(|l| Line::from(Span::styled(l, t.muted())).centered()),
    );
    f.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(t.dim())
                .padding(Padding::horizontal(1)),
        ),
        rect,
    );
}

pub(super) fn chip(t: &Theme, text: &str, tone: Tone) -> Span<'static> {
    Span::styled(format!(" {text} "), t.chip(tone))
}

pub(super) fn tab_bar(t: &Theme, shown: Tab, info: &str, w: usize) -> Line<'static> {
    let labels = Tab::ALL.map(Tab::label);
    let at = Tab::ALL.iter().position(|x| *x == shown).unwrap_or(0);
    tabs_line(t, &labels, at, info, w)
}

/// Tab labels with `shown` underlined, then `info` on the right.
pub(super) fn tabs_line(
    t: &Theme,
    labels: &[&str],
    shown: usize,
    info: &str,
    w: usize,
) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, name) in labels.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        let label = format!(" {name} ");
        if i == shown {
            spans.push(Span::styled(
                label,
                t.word(Tone::Accent)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            ));
        } else {
            spans.push(Span::styled(label, t.muted()));
        }
    }
    right_info(spans, info, t, w)
}

/// Puts `info` right-aligned after `spans`, cut to what is left of `w`.
pub(super) fn right_info(
    mut spans: Vec<Span<'static>>,
    info: &str,
    t: &Theme,
    w: usize,
) -> Line<'static> {
    let used = line_cells(&spans);
    let room = w.saturating_sub(used + 3);
    if room > 0 && !info.is_empty() {
        let info = ellipsize(&display(info), room);
        spans.push(Span::raw(" ".repeat(w - used - cells(&info))));
        spans.push(Span::styled(info, t.muted()));
    }
    Line::from(spans)
}

pub(super) fn tab_hits(app: &mut App, labels: &[&str], area: Rect) {
    let mut x = area.x;
    for (i, label) in labels.iter().enumerate() {
        let width = (cells(label) as u16 + 2).min(area.right().saturating_sub(x));
        app.hits
            .push((Rect::new(x, area.y, width, area.height), Hit::Tab(i)));
        x = x.saturating_add(width + 2);
    }
}
