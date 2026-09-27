//! Overlays: help, the input form, command output, and the row action chooser.

use mira_protocol::ids::ActionId;
use mira_protocol::manifest::ActionMode;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Padding, Paragraph};

use crate::app::{App, Binding, Intent, Modal};
use crate::form::Kind;
use crate::logs::{cells, display, slice_cells};
use crate::theme::{Theme, Tone};

use super::footer::key_chip;
use super::text::{line_cells, pad, wrap};
use super::widgets::panel_title;

/// Cells of the key part of one help column: the widest chip plus a gap.
fn help_key_w(bindings: &[Binding]) -> usize {
    bindings
        .iter()
        .map(|b| cells(b.keys) + 2)
        .max()
        .unwrap_or(0)
        + 2
}

fn help_col_w(bindings: &[Binding]) -> usize {
    help_key_w(bindings) + bindings.iter().map(|b| cells(&b.label)).max().unwrap_or(0)
}

/// One column of help: each key chip is exactly ` key `; labels start in one column after
/// the widest chip and wrap within `width`.
fn help_column(t: &Theme, bindings: &[Binding], key_w: usize, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for b in bindings {
        let chip_w = cells(b.keys) + 2;
        let label_w = width.saturating_sub(key_w).max(8);
        for (i, part) in wrap(&b.label, label_w).into_iter().enumerate() {
            let mut spans = if i == 0 {
                vec![
                    key_chip(t, b.keys),
                    Span::raw(" ".repeat(key_w.saturating_sub(chip_w))),
                ]
            } else {
                vec![Span::raw(" ".repeat(key_w))]
            };
            spans.push(Span::raw(part));
            lines.push(Line::from(spans));
        }
    }
    lines
}

/// The help overlay's lines and their content width, at most `avail` cells; two columns
/// when they fit.
fn help_lines(app: &App, t: &Theme, avail: usize) -> (Vec<Line<'static>>, usize) {
    const GAP: usize = 4;
    /// Notes wrap to at least this width when the keys are narrower.
    const NOTES_W: usize = 72;
    let all = app.normal_bindings();
    let (l, r) = all.split_at(all.len().div_ceil(2));
    let two = help_col_w(l) + GAP + help_col_w(r);
    let (body, keys_w) = if two <= avail && all.len() > 6 {
        let (lw, rw) = (help_col_w(l), help_col_w(r));
        let left = help_column(t, l, help_key_w(l), lw);
        let right = help_column(t, r, help_key_w(r), rw);
        let mut out = Vec::new();
        for i in 0..left.len().max(right.len()) {
            let mut spans: Vec<Span> = left.get(i).map(|x| x.spans.clone()).unwrap_or_default();
            let used = line_cells(&spans);
            spans.push(Span::raw(" ".repeat(lw.saturating_sub(used) + GAP)));
            if let Some(x) = right.get(i) {
                spans.extend(x.spans.clone());
            }
            out.push(Line::from(spans));
        }
        (out, two)
    } else {
        let w = help_col_w(&all).min(avail);
        (help_column(t, &all, help_key_w(&all), w), w)
    };
    let width = keys_w.max(NOTES_W.min(avail));
    let mut lines = vec![
        Line::from(Span::styled(
            "Keys that work here",
            t.word(Tone::Accent).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    lines.extend(body);
    lines.push(Line::from(""));
    let notes = [
        if app.mouse {
            "Mouse mode is on (m): the wheel scrolls; terminal selection needs Option/Shift."
        } else {
            "Mouse mode is off (m turns it on): your terminal's own text selection works."
        },
        "Tabs: [ and ] switch Logs, History, and Output; 1, 2, 3 go straight to one. \
         H opens History; Enter there shows that run's logs.",
        "ONE-OFF RUNS lists `mira exec` runs from any terminal: Logs and Details tabs \
         (1, 2); s stops a running one.",
        ": runs one public mira command (not a shell). Forms: Tab moves, Enter runs.",
        "q and Ctrl-C close this window. When it is the last Mira window, its runs stop. \
         b keeps them running for 2h; `mira down` stops them.",
        "Marks: ● running · ✓ ok · ✗ failed · ◐ starting or stopping · ○ not run yet · \
         ‖ disabled · ■ stopped. ASCII mode (MIRA_ASCII=1, or a locale that is not UTF-8) \
         shows them as * v x ~ o - =.",
        "Screen reader: `mira status` prints the same state as plain text; `--json` adds \
         structure.",
        "If a crash leaves the terminal in raw mode, type `reset` and press Enter.",
    ];
    for n in notes {
        lines.extend(
            wrap(n, width)
                .into_iter()
                .map(|l| Line::from(Span::styled(l, t.muted()))),
        );
    }
    (lines, width)
}

fn overlay<'a>(t: &Theme, title: Line<'a>) -> Block<'a> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(t.fg(Tone::Accent))
        .title(title)
        .padding(Padding::horizontal(1))
}

pub(super) fn draw_help(f: &mut Frame, app: &mut App, t: &Theme, area: Rect) {
    // Borders and one column of padding on each side take 4 cells.
    let avail = (area.width as usize).saturating_sub(8);
    let (lines, content_w) = help_lines(app, t, avail);
    let w = (content_w + 4).min(area.width.saturating_sub(4) as usize) as u16;
    let h = (lines.len() + 2).min(area.height.saturating_sub(2) as usize) as u16;
    let rect = centered(area, w, h);
    let rows = h.saturating_sub(2) as usize;
    let max = lines.len().saturating_sub(rows);
    let top = match &mut app.modal {
        Modal::Help { top, max: m } => {
            *m = max;
            *top = (*top).min(max);
            *top
        }
        _ => 0,
    };
    let mut block = overlay(t, panel_title(t, "Help · Esc closes", true));
    if max > 0 {
        let last = (top + rows).min(lines.len());
        block = block.title_bottom(
            Line::from(Span::styled(
                format!(" j/k scroll · {}-{last} of {} ", top + 1, lines.len()),
                t.muted(),
            ))
            .right_aligned(),
        );
    }
    let shown: Vec<Line> = lines.into_iter().skip(top).take(rows).collect();
    clear_around(f, rect, area);
    f.render_widget(Paragraph::new(shown).block(block), rect);
}

/// Clears `rect` and a one-cell margin around it, so no cut text from the panes behind
/// touches an overlay's border. A side with only a sliver left clears to the edge.
fn clear_around(f: &mut Frame, rect: Rect, area: Rect) {
    const SLIVER: u16 = 3;
    let x = if rect.x - area.x <= SLIVER {
        area.x
    } else {
        rect.x - 1
    };
    let y = if rect.y - area.y <= SLIVER {
        area.y
    } else {
        rect.y - 1
    };
    let (area_r, area_b) = (area.x + area.width, area.y + area.height);
    let (rect_r, rect_b) = (rect.x + rect.width, rect.y + rect.height);
    let right = if area_r - rect_r <= SLIVER {
        area_r
    } else {
        rect_r + 1
    };
    let bottom = if area_b - rect_b <= SLIVER {
        area_b
    } else {
        rect_b + 1
    };
    f.render_widget(
        Clear,
        Rect {
            x,
            y,
            width: right - x,
            height: bottom - y,
        },
    );
}

pub(super) fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(2)).max(1);
    let h = h.min(area.height.saturating_sub(2)).max(1);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

pub(super) fn draw_form(f: &mut Frame, app: &App, t: &Theme, area: Rect) {
    let Modal::Form(form) = &app.modal else {
        return;
    };
    let rect = centered(area, 90, area.height.saturating_sub(2));
    let inner_w = rect.width.saturating_sub(4) as usize;
    let mut lines: Vec<Line> = Vec::new();
    let mut focus_line = 0usize;
    for (i, fl) in form.fields.iter().enumerate() {
        let focused = i == form.focus;
        if focused {
            focus_line = lines.len();
        }
        let label = format!(
            "{}{}{}",
            if focused { "▸ " } else { "  " },
            display(&fl.title),
            if fl.required { " *" } else { "" }
        );
        let mut value = fl.shown();
        if focused && !matches!(fl.kind, Kind::Boolean | Kind::Enum(_)) {
            value.push('▏');
        }
        let label_w = 22.min(inner_w / 3);
        let value_w = inner_w.saturating_sub(label_w + 1);
        // Keep the end of long values (the typing position) visible.
        let vw = cells(&value);
        let shown = if vw > value_w {
            slice_cells(&value, vw - value_w, value_w)
        } else {
            value
        };
        let (lst, vst) = if focused {
            (
                t.word(Tone::Accent).add_modifier(Modifier::BOLD),
                t.selected(),
            )
        } else {
            (t.bold(), Style::default())
        };
        lines.push(Line::from(vec![
            Span::styled(pad(&label, label_w), lst),
            Span::raw(" "),
            Span::styled(shown, vst),
        ]));
        let mut hint = fl.hint();
        if !fl.description.is_empty() {
            hint = if hint.is_empty() {
                display(&fl.description)
            } else {
                format!("{} · {hint}", display(&fl.description))
            };
        }
        if !hint.is_empty() {
            lines.push(Line::from(Span::styled(
                slice_cells(&format!("    {hint}"), 0, inner_w),
                t.muted(),
            )));
        }
        if let Some(e) = &fl.error {
            lines.push(Line::from(Span::styled(
                slice_cells(&display(&format!("    ✗ {e}")), 0, inner_w),
                t.word(Tone::Rose).add_modifier(Modifier::BOLD),
            )));
        }
    }
    let mut foot = vec![Line::from("")];
    if let Some(e) = &form.error {
        foot.push(Line::from(Span::styled(
            slice_cells(&display(e), 0, inner_w),
            t.word(Tone::Rose).add_modifier(Modifier::BOLD),
        )));
    }
    foot.push(Line::from(Span::styled(
        slice_cells(
            if form.pending {
                "sending to the host…"
            } else {
                "* required · empty fields use the declared default · the host validates again"
            },
            0,
            inner_w,
        ),
        t.muted(),
    )));
    let body_h = (rect.height as usize).saturating_sub(2 + foot.len());
    let skip = focus_line.saturating_sub(body_h.saturating_sub(3));
    let mut shown: Vec<Line> = lines.into_iter().skip(skip).take(body_h).collect();
    while shown.len() < body_h {
        shown.push(Line::from(""));
    }
    shown.extend(foot);
    let verb = match form.intent {
        Intent::Restart
            if app
                .item(&form.action_ref)
                .is_some_and(|i| i.mode == ActionMode::Process) =>
        {
            "Restart"
        }
        Intent::Restart => "Run again",
        _ => "Run",
    };
    clear_around(f, rect, area);
    f.render_widget(
        Paragraph::new(shown).block(overlay(
            t,
            panel_title(t, &format!("{verb} {}", form.action_ref), true),
        )),
        rect,
    );
}

pub(super) fn draw_output(f: &mut Frame, app: &App, t: &Theme, area: Rect) {
    let Modal::Output(o) = &app.modal else {
        return;
    };
    let rect = centered(area, 100, area.height.saturating_sub(2));
    let w = rect.width.saturating_sub(4) as usize;
    let h = rect.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = o
        .lines
        .iter()
        .skip(o.top)
        .take(h)
        .map(|l| Line::from(slice_cells(&display(l), 0, w)))
        .collect();
    clear_around(f, rect, area);
    let title_style = if o.failed {
        t.word(Tone::Rose).add_modifier(Modifier::BOLD)
    } else {
        t.word(Tone::Accent).add_modifier(Modifier::BOLD)
    };
    let mark = if o.failed { "✗ " } else { "" };
    f.render_widget(
        Paragraph::new(lines).block(overlay(
            t,
            Line::from(Span::styled(
                format!(" {mark}{} ", display(&o.title)),
                title_style,
            )),
        )),
        rect,
    );
}

pub(super) fn draw_confirm(f: &mut Frame, app: &App, t: &Theme, area: Rect) {
    let Modal::Confirm { title, lines, .. } = &app.modal else {
        return;
    };
    let w = lines.iter().map(|l| cells(l)).max().unwrap_or(0) as u16 + 6;
    let rect = centered(area, w.max(40), lines.len() as u16 + 4);
    let mut text: Vec<Line> = lines.iter().map(|l| Line::from(display(l))).collect();
    text.push(Line::from(""));
    text.push(Line::from(vec![
        Span::styled("y", t.bold()),
        Span::raw(" yes   "),
        Span::styled("n", t.bold()),
        Span::raw(" cancel"),
    ]));
    clear_around(f, rect, area);
    f.render_widget(
        Paragraph::new(text).block(overlay(t, panel_title(t, title, true))),
        rect,
    );
}

pub(super) fn draw_row_actions(f: &mut Frame, app: &App, t: &Theme, area: Rect) {
    let Modal::RowAction {
        choices,
        index,
        view_ref,
    } = &app.modal
    else {
        return;
    };
    let row = app
        .view_panes
        .get(view_ref)
        .and_then(|p| p.selected_row_id())
        .unwrap_or_default();
    // The action title, when the catalog has it, and its ref.
    let label = |c: &ActionId| {
        let title = app
            .items
            .iter()
            .find(|i| i.action_ref.plugin == view_ref.plugin && i.action_ref.action == *c)
            .map(|i| i.title.clone());
        match title {
            Some(title) => format!("{title}  ({}.{c})", view_ref.plugin),
            None => format!("{}.{c}", view_ref.plugin),
        }
    };
    let w = choices
        .iter()
        .map(|c| cells(&label(c)) + 4)
        .chain([cells(&row) + 18, 46])
        .max()
        .unwrap_or(46) as u16
        + 4;
    let rect = centered(area, w, choices.len() as u16 + 4);
    let w = rect.width.saturating_sub(4) as usize;
    let mut lines = vec![Line::from(vec![
        Span::styled("Run for row ", t.bold()),
        Span::styled(
            display(&row),
            t.word(Tone::Accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled("?", t.bold()),
    ])];
    for (i, c) in choices.iter().enumerate() {
        let text = pad(&format!("  {}", label(c)), w);
        let st = if i == *index {
            t.selected()
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(text, st)));
    }
    clear_around(f, rect, area);
    f.render_widget(
        Paragraph::new(lines).block(overlay(t, panel_title(t, "Row action", true))),
        rect,
    );
}
