//! Rendering: a header, the tool sidebar, the main pane (a card, tabs, and the tab
//! content), a right rail on wide terminals, a notice line only when there is a notice, and
//! the key footer. Only visible rows are built. State is never shown by color alone: every
//! state has a glyph and a word (see [`crate::theme::Mark`]).
//!
//! Each submodule draws one part of the screen; [`widgets`], [`text`], and [`marks`]
//! hold the pieces they share.

mod footer;
mod header;
mod history;
mod log_panel;
mod main_pane;
mod marks;
mod oneoff;
mod output_tab;
mod overlays;
mod rail;
mod sidebar;
mod text;
mod view_panel;
mod widgets;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{App, Focus, Modal, Tab};
use crate::theme::{self, Theme};

use footer::{draw_footer, status_line};
use header::draw_header;
use main_pane::draw_main;
use overlays::{draw_form, draw_help, draw_output, draw_row_actions};
use rail::draw_rail;
use sidebar::draw_sidebar;

const MIN_W: u16 = 60;
const MIN_H: u16 = 18;
/// Below this width one pane shows at a time.
const NARROW_W: u16 = 80;
/// From this width a right rail shows recent runs and the session.
const RAIL_MIN_W: u16 = 160;
const RAIL_W: u16 = 34;
/// Runs listed in the rail.
const RAIL_RUNS: usize = 8;

/// Sidebar width: 30% of the terminal, at least 28 and at most 48 columns.
fn sidebar_width(total: u16) -> u16 {
    let w = (u32::from(total) * 30 / 100).clamp(28, 48);
    u16::try_from(w).unwrap_or(48)
}

pub fn draw(f: &mut Frame, app: &mut App, t: &Theme) {
    draw_screen(f, app, t);
    if theme::ascii() {
        for cell in &mut f.buffer_mut().content {
            if let Some(a) = theme::ascii_cell(cell.symbol()) {
                cell.set_symbol(a);
            }
        }
    }
}

fn draw_screen(f: &mut Frame, app: &mut App, t: &Theme) {
    let area = f.area();
    if area.width < MIN_W || area.height < MIN_H {
        let msg = vec![
            Line::from(Span::styled("Window too small", t.bold())),
            Line::from(format!(
                "Mira needs at least {MIN_W}x{MIN_H}; this window is {}x{}.",
                area.width, area.height
            )),
            Line::from("Resize to continue. q quits."),
        ];
        f.render_widget(Paragraph::new(msg), area);
        return;
    }
    let status = status_line(app, t);
    let [header, body, status_row, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(u16::from(status.is_some())),
        Constraint::Length(1),
    ])
    .areas(area);
    draw_header(f, app, t, header);
    app.narrow = area.width < NARROW_W;
    app.wide = area.width >= RAIL_MIN_W;
    if app.wide {
        app.want_recent();
    }
    if app.term.is_open() {
        crate::terminal::draw(f, &mut app.term, body, t.mode.enabled());
    } else if app.narrow {
        if app.focus == Focus::Logs || app.shown_tab() == Tab::History {
            draw_main(f, app, t, body);
        } else {
            draw_sidebar(f, app, t, body);
        }
    } else {
        let side_w = sidebar_width(area.width);
        let rail_w = if app.wide { RAIL_W } else { 0 };
        let [side, main, rail] = Layout::horizontal([
            Constraint::Length(side_w),
            Constraint::Min(1),
            Constraint::Length(rail_w),
        ])
        .areas(body);
        draw_sidebar(f, app, t, side);
        draw_main(f, app, t, main);
        if app.wide {
            draw_rail(f, app, t, rail);
        }
    }
    if let Some(line) = status {
        f.render_widget(Paragraph::new(line), status_row);
    }
    draw_footer(f, app, t, footer);
    match &app.modal {
        Modal::Help { .. } => draw_help(f, app, t, area),
        Modal::Form(_) => draw_form(f, app, t, area),
        Modal::Output(_) => draw_output(f, app, t, area),
        Modal::RowAction { .. } => draw_row_actions(f, app, t, area),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::sidebar_width;

    #[test]
    fn sidebar_width_is_clamped() {
        assert_eq!(sidebar_width(60), 28);
        assert_eq!(sidebar_width(100), 30);
        assert_eq!(sidebar_width(120), 36);
        assert_eq!(sidebar_width(200), 48);
        assert_eq!(sidebar_width(u16::MAX), 48);
    }
}
