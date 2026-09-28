//! The header line: workspace, branch, and session.

use mira_protocol::ipc::{SessionMode, SessionState};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::logs::{cells, display};
use crate::theme::{Theme, Tone};

use super::text::{BRANCH_MAX, BRANCH_MIN, ellipsize, fit_spans, line_cells, short_root};

fn session_spans(app: &App, t: &Theme) -> Vec<Span<'static>> {
    let mut out = match &app.session {
        None => vec![
            Span::styled("○", t.muted()),
            Span::styled(" no session", t.muted()),
        ],
        Some(s) if s.state == SessionState::Stopping => vec![
            Span::styled("◐", t.fg(Tone::Amber)),
            Span::styled(" stopping", t.word(Tone::Amber)),
        ],
        Some(s) => {
            let until = s.expires_at.map(|e| app.clock.hm(e));
            match (s.mode, until) {
                (SessionMode::Foreground, until) => {
                    let n = s.controller_count;
                    let mut v = vec![
                        Span::styled("●", t.fg(Tone::Leaf)),
                        Span::styled(
                            format!(" {n} window{}", if n == 1 { "" } else { "s" }),
                            t.bold(),
                        ),
                    ];
                    // Kept in the background: runs outlive the last window until then.
                    if let Some(u) = until {
                        v.push(Span::styled(
                            format!(" · kept until {u}"),
                            t.word(Tone::Amber),
                        ));
                    }
                    v
                }
                (SessionMode::Background, until) => {
                    let text = match until {
                        Some(u) => format!(" background · until {u}"),
                        None => " background".to_owned(),
                    };
                    vec![
                        Span::styled("◐", t.fg(Tone::Amber)),
                        Span::styled(text, t.word(Tone::Amber).add_modifier(Modifier::BOLD)),
                    ]
                }
            }
        }
    };
    if !app.is_controller() && app.session.is_some() {
        out.push(Span::styled(" · watching", t.muted()));
    }
    let warnings = app.storage_warnings.len() + app.config_warnings.len();
    if warnings > 0 {
        out.push(Span::styled(
            format!(
                " · ! {warnings} warning{}",
                if warnings == 1 { "" } else { "s" }
            ),
            t.word(Tone::Amber),
        ));
    }
    out
}

pub(super) fn draw_header(f: &mut Frame, app: &App, t: &Theme, area: Rect) {
    let w = area.width as usize;
    let mut right = session_spans(app, t);
    if let Some(latest) = &app.update_available {
        let notice = vec![
            Span::styled(" · ", t.muted()),
            Span::styled(latest.clone(), t.word(Tone::Accent)),
            Span::styled(" available · mira update", t.muted()),
        ];
        // Keep room for the workspace and branch; omit the notice before other fields.
        let name = app.workspace_name.as_deref().unwrap_or(&app.root);
        let left = 10
            + cells(name).min(32)
            + app
                .branch
                .as_deref()
                .map_or(0, |b| cells(b).min(BRANCH_MAX) + 4)
            + cells(&short_root(&app.root, 40))
            + 2;
        if left + line_cells(&right) + line_cells(&notice) < w {
            right.extend(notice);
        }
    }
    let rw = line_cells(&right) + 1;
    let brand = " ◆ mira";
    let root = app.root.trim_end_matches('/');
    let name = display(
        app.workspace_name
            .as_deref()
            .unwrap_or_else(|| root.rsplit('/').next().unwrap_or(root)),
    );
    // Brand, two spaces, the name; then the branch chip and the path share what is left.
    let mut room = w.saturating_sub(rw + cells(brand) + 2 + 2);
    let name = ellipsize(&name, room.min(32));
    room = room.saturating_sub(cells(&name));
    let branch = app.branch.as_deref().map(display).and_then(|b| {
        // ` ` + name + ` `, two spaces before it; the chip itself marks the branch.
        let cap = room.saturating_sub(4).min(BRANCH_MAX);
        (cap >= BRANCH_MIN.min(cells(&b))).then(|| format!(" {} ", ellipsize(&b, cap)))
    });
    if let Some(b) = &branch {
        room = room.saturating_sub(cells(b) + 2);
    }
    let path = if room >= 10 {
        short_root(root, room - 2)
    } else {
        String::new()
    };
    let mut spans = vec![
        Span::styled(brand, t.word(Tone::Accent).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(name, t.bold()),
    ];
    if let Some(b) = branch {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(b, t.chip(Tone::Sky)));
    }
    if !path.is_empty() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(path, t.muted()));
    }
    let gap = w.saturating_sub(line_cells(&spans) + rw);
    spans.push(Span::raw(" ".repeat(gap)));
    spans.extend(right);
    spans.push(Span::raw(" "));
    f.render_widget(Paragraph::new(Line::from(fit_spans(spans, w))), area);
}
