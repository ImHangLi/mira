//! The notice line and the key footer.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{App, Binding, Cmd, Modal};
use crate::logs::{cells, display};
use crate::theme::{Theme, Tone};

use super::text::{fit_spans, line_cells};

/// One footer chip: the key cells, the label cells, and whether it must stay.
#[derive(Clone, Copy, Debug)]
struct ChipSize {
    keys: usize,
    label: usize,
    pinned: bool,
}

/// Which chips fit in `width` cells. A chip is ` key ` plus ` label`; two spaces separate
/// chips. A chip always shows with its label, or not at all: from the last chip back,
/// unpinned chips go first, then pinned ones.
fn fit_chips(chips: &[ChipSize], width: usize) -> Vec<bool> {
    let mut shown: Vec<bool> = vec![true; chips.len()];
    let total = |shown: &[bool]| -> usize {
        let (used, n) = chips.iter().zip(shown).filter(|(_, s)| **s).fold(
            (0usize, 0usize),
            |(used, n), (c, _)| {
                let label = if c.label > 0 { 1 + c.label } else { 0 };
                (used + c.keys + 2 + label, n + 1)
            },
        );
        used + 2 * n.saturating_sub(1)
    };
    for pinned in [false, true] {
        for i in (0..chips.len()).rev() {
            if total(&shown) <= width {
                return shown;
            }
            if chips[i].pinned == pinned {
                shown[i] = false;
            }
        }
    }
    shown
}

/// The notice line, or `None` when there is nothing to say (the row goes to the body).
pub(super) fn status_line(app: &App, t: &Theme) -> Option<Line<'static>> {
    let spans = match &app.modal {
        Modal::Search { logs, text, .. } => vec![
            Span::raw(" "),
            Span::styled(
                if *logs {
                    "find in logs "
                } else {
                    "filter tools "
                },
                t.muted(),
            ),
            Span::styled("/", t.fg(Tone::Accent).add_modifier(Modifier::BOLD)),
            Span::styled(display(text), t.bold()),
            Span::styled("▏", t.fg(Tone::Accent)),
        ],
        Modal::Command { text, error } => vec![
            Span::raw(" "),
            Span::styled(":", t.fg(Tone::Accent).add_modifier(Modifier::BOLD)),
            Span::styled(display(text), t.bold()),
            Span::styled("▏   ", t.fg(Tone::Accent)),
            match error {
                Some(e) => Span::styled(display(e), t.word(Tone::Rose)),
                None => Span::styled(crate::cmdbar::hint(text).to_string(), t.muted()),
            },
        ],
        _ => {
            let (glyph, text, tone) = if let Some(n) = &app.notice {
                if n.error {
                    ("✗", n.text.clone(), Some(Tone::Rose))
                } else if n.ok {
                    ("✓", n.text.clone(), Some(Tone::Leaf))
                } else {
                    ("›", n.text.clone(), Some(Tone::Sky))
                }
            } else if let Some(m) = &app.control_lost {
                (
                    "✗",
                    format!("host connection lost ({m}); actions are unavailable. q quits."),
                    Some(Tone::Rose),
                )
            } else if let Some(m) = &app.stream_issue {
                ("!", m.clone(), Some(Tone::Amber))
            } else {
                let wn = app
                    .storage_warnings
                    .first()
                    .or(app.config_warnings.first())?;
                (
                    "!",
                    format!("warning [{}]: {}", wn.code, wn.message),
                    Some(Tone::Amber),
                )
            };
            let st = tone.map_or(t.muted(), |c| t.fg(c));
            let text_st = match tone {
                Some(Tone::Rose) => t.word(Tone::Rose).add_modifier(Modifier::BOLD),
                Some(Tone::Sky | Tone::Leaf) | None => Style::default(),
                Some(c) => t.word(c),
            };
            vec![
                Span::raw(" "),
                Span::styled(format!("{glyph} "), st.add_modifier(Modifier::BOLD)),
                Span::styled(display(&text), text_st),
            ]
        }
    };
    Some(Line::from(spans))
}

pub(super) fn key_chip(t: &Theme, keys: &str) -> Span<'static> {
    Span::styled(format!(" {keys} "), t.chip(Tone::Accent))
}

pub(super) fn draw_footer(f: &mut Frame, app: &App, t: &Theme, area: Rect) {
    let w = area.width as usize;
    let mut all: Vec<Binding> = app.bindings().into_iter().filter(|b| b.footer).collect();
    let help = all
        .iter()
        .position(|b| b.cmd == Cmd::Help)
        .map(|i| all.remove(i));
    let right: Vec<Span> = match &help {
        Some(b) => vec![
            key_chip(t, b.keys),
            Span::styled(format!(" {}", b.label), t.muted()),
            Span::raw(" "),
        ],
        None => Vec::new(),
    };
    let rw = line_cells(&right);
    let sizes: Vec<ChipSize> = all
        .iter()
        .map(|b| ChipSize {
            keys: cells(b.keys),
            label: cells(&b.label),
            pinned: matches!(b.cmd, Cmd::Tools | Cmd::Focus | Cmd::Quit),
        })
        .collect();
    let room = w.saturating_sub(rw + 3);
    let fit = fit_chips(&sizes, room);
    let mut spans = vec![Span::raw(" ")];
    let mut first = true;
    for (b, shown) in all.iter().zip(fit) {
        if !shown {
            continue;
        }
        if !first {
            spans.push(Span::raw("  "));
        }
        first = false;
        spans.push(key_chip(t, b.keys));
        if !b.label.is_empty() {
            spans.push(Span::styled(format!(" {}", b.label), t.muted()));
        }
    }
    let gap = w.saturating_sub(line_cells(&spans) + rw);
    spans.push(Span::raw(" ".repeat(gap)));
    spans.extend(right);
    f.render_widget(Paragraph::new(Line::from(fit_spans(spans, w))), area);
}

#[cfg(test)]
mod tests {
    use super::{ChipSize, fit_chips};

    fn chip(keys: usize, label: usize, pinned: bool) -> ChipSize {
        ChipSize {
            keys,
            label,
            pinned,
        }
    }

    #[test]
    fn footer_drops_whole_chips_and_never_shows_a_key_alone() {
        // ` j/k  move` (10) + `  ` + ` s  stop` (8) + `  ` + ` q  quit` (8) = 30.
        let chips = [chip(3, 4, false), chip(1, 4, false), chip(1, 4, true)];
        assert_eq!(fit_chips(&chips, 30), [true; 3]);
        // The last unpinned chip goes first, with its label.
        assert_eq!(fit_chips(&chips, 29), [true, false, true]);
        assert_eq!(fit_chips(&chips, 20), [true, false, true]);
        assert_eq!(fit_chips(&chips, 19), [false, false, true]);
        // Pinned chips go last.
        assert_eq!(fit_chips(&chips, 8), [false, false, true]);
        assert_eq!(fit_chips(&chips, 7), [false; 3]);
    }
}
