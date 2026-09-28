//! The main pane of a view: its status, freshness, and content.

use mira_protocol::manifest::ViewKind;
use mira_protocol::time::Timestamp;
use mira_protocol::view::{Durability, Freshness};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Padding, Paragraph};

use crate::app::{App, Focus};
use crate::logs::display;
use crate::theme::{Theme, Tone};
use crate::views::{freshness_word, kind_word};

use super::marks::{kind_glyph, view_tone};
use super::text::{ago, ellipsize, fit_spans};
use super::widgets::{chip, panel, panel_title, right_info};

pub(super) fn draw_view(f: &mut Frame, app: &mut App, t: &Theme, area: Rect) {
    let Some(v) = app.selected_view() else {
        return;
    };
    let focus = app.focus == Focus::Logs;
    let r = v.view_ref.clone();
    let title = display(&v.title);
    let kind = v.kind;
    let description = display(&v.description);
    let tone = view_tone(app, v);
    let block = panel(t, panel_title(t, "View", focus), focus).padding(Padding::horizontal(1));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let w = inner.width as usize;
    let hm = |ts: Timestamp| app.clock.hm(ts);
    let sel = t.selected();
    let Some(p) = app.view_panes.get_mut(&r) else {
        f.render_widget(Paragraph::new(Span::styled("loading…", t.muted())), inner);
        return;
    };
    let l1 = Line::from(fit_spans(
        vec![
            Span::styled(ellipsize(&title, w / 2), t.bold()),
            Span::raw("  "),
            Span::styled(r.to_string(), t.muted()),
            Span::raw("  "),
            chip(
                t,
                &format!("{} {} view", kind_glyph(kind), kind_word(kind)),
                Tone::Sky,
            ),
        ],
        w,
    ));
    // Freshness uses words and timestamps as well as color.
    let mut status: Vec<Span> = match &p.meta {
        None => vec![Span::styled("◌ reading…", t.muted())],
        Some(m) => match m.revision {
            None => vec![
                Span::styled(
                    "○ no data",
                    t.word(Tone::Amber).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(
                        " · {} (this is not an empty result)",
                        m.freshness_reason.clone().unwrap_or_default()
                    ),
                    t.muted(),
                ),
            ],
            // A derived view names its source action and filter; its revision and
            // durability are host bookkeeping, not facts for the reader.
            Some(_) if p.source.is_some() => {
                let src = p
                    .source
                    .as_ref()
                    .map_or(String::new(), |s| s.logs.to_string());
                let filter = p
                    .source
                    .as_ref()
                    .map_or(String::new(), |s| s.filter_words());
                let n = p.len();
                let lines = format!(" · {n} line{}{filter}", if n == 1 { "" } else { "s" });
                match m.freshness {
                    Freshness::Current => vec![
                        Span::styled("● live", t.word(Tone::Leaf).add_modifier(Modifier::BOLD)),
                        Span::styled(format!(" · from {src} (running){lines}"), t.muted()),
                    ],
                    _ => {
                        let ended = m
                            .recorded_at
                            .map_or(String::new(), |a| format!(" {}", hm(a)));
                        vec![
                            Span::styled(format!("○ from {src} (ended{ended})"), t.bold()),
                            Span::styled(lines, t.muted()),
                        ]
                    }
                }
            }
            Some(_) => {
                let at = m.recorded_at.map_or(String::new(), |a| {
                    format!(" · recorded {} ({})", ago(a), hm(a))
                });
                let why = match (&m.freshness, &m.freshness_reason) {
                    (Freshness::Current, _) | (_, None) => String::new(),
                    (_, Some(reason)) => format!(" · {reason}"),
                };
                let glyph = match m.freshness {
                    Freshness::Current => "●",
                    Freshness::Stale => "◐",
                    Freshness::Historical => "○",
                };
                let st = tone.map_or(t.muted(), |c| t.word(c));
                // Completed runs and publications show when the data was updated.
                let (word, why, at) = match (m.freshness, &m.source_run_id, m.recorded_at) {
                    (Freshness::Historical, Some(_), Some(a)) => (
                        format!("updated {}", ago(a)),
                        String::new(),
                        format!(" · {}", hm(a)),
                    ),
                    (Freshness::Historical, None, Some(a)) if m.source_kind.is_some() => (
                        format!("published {}", ago(a)),
                        String::new(),
                        format!(" · {}", hm(a)),
                    ),
                    _ => (freshness_word(m.freshness).to_owned(), why, at),
                };
                // Detailed provenance stays available through `mira view --json`.
                vec![
                    Span::styled(format!("{glyph} {word}"), st.add_modifier(Modifier::BOLD)),
                    Span::styled(format!("{why}{at}"), t.muted()),
                ]
            }
        },
    };
    if p.meta
        .as_ref()
        .is_some_and(|m| m.revision.is_some() && m.durability == Durability::Unavailable)
    {
        status.push(Span::styled(" · not saved", t.word(Tone::Amber)));
    }
    let mut card = vec![l1];
    if !description.trim().is_empty() {
        card.push(Line::from(Span::styled(
            ellipsize(&description, w),
            t.muted(),
        )));
    }
    card.push(Line::from(fit_spans(status, w)));
    let mut bar = vec![position_word(p.kind, p.cursor, p.len())];
    if let (Some(s), Some(cur)) = (p.sel_rev, p.revision())
        && s != cur
    {
        bar.push("the table changed since you chose this row".into());
    }
    if !p.row_actions.is_empty() {
        bar.push("Enter acts on the row".into());
    }
    if p.wrap {
        bar.push("wrap".into());
    } else if p.hscroll > 0 && p.kind != ViewKind::Table {
        bar.push(format!("col +{}", p.hscroll));
    }
    if p.truncated {
        bar.push("partial: the host sent only part of this view".into());
    }
    if p.loading {
        bar.push("reading…".into());
    }
    if let Some(n) = &p.note {
        bar.push(n.clone());
    }
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
    let tab = vec![Span::styled(
        format!(" {} {} ", kind_glyph(kind), capitalized(kind_word(kind))),
        t.word(Tone::Accent)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
    )];
    f.render_widget(
        Paragraph::new(right_info(tab, &bar.join(" · "), t, w)),
        tabs_area,
    );
    if rule {
        f.render_widget(
            Paragraph::new(Span::styled("─".repeat(w), t.dim())),
            rule_area,
        );
    }
    app.hits.push((body, crate::app::Hit::Pane));
    p.height = body.height as usize;
    p.width = w;
    let dim_msg = |s: String| Paragraph::new(Span::styled(s, t.muted()));
    if let Some(e) = &p.error {
        f.render_widget(
            Paragraph::new(format!("Cannot read the view: {e}"))
                .style(t.word(Tone::Rose))
                .wrap(ratatui::widgets::Wrap { trim: true }),
            body,
        );
        return;
    }
    match &p.body {
        crate::views::Body::Reference(summary) => {
            f.render_widget(
                Paragraph::new(format!(
                    "{summary}\nUse `: view {r} --max-bytes 262144` for the full value."
                ))
                .wrap(ratatui::widgets::Wrap { trim: true }),
                body,
            );
            return;
        }
        crate::views::Body::Empty => {
            let msg = if p.meta.is_none() {
                "reading…".to_owned()
            } else {
                "No data is recorded for this view. That is different from an empty result."
                    .to_owned()
            };
            f.render_widget(dim_msg(msg), body);
            return;
        }
        crate::views::Body::Data(_) => {}
    }
    if p.len() == 0 {
        f.render_widget(dim_msg("(the view is empty)".into()), body);
        return;
    }
    let lines = p.lines(focus, sel, t.muted());
    f.render_widget(Paragraph::new(lines), body);
}

/// Where the cursor is in a view: `row 1 of 4`; `no rows` when empty.
fn position_word(kind: ViewKind, cursor: usize, len: usize) -> String {
    let (one, many) = match kind {
        ViewKind::Table => ("row", "rows"),
        ViewKind::Log => ("item", "items"),
        ViewKind::Tree => ("node", "nodes"),
        _ => ("line", "lines"),
    };
    if len == 0 {
        format!("no {many}")
    } else {
        format!("{one} {} of {len}", cursor.min(len - 1) + 1)
    }
}

fn capitalized(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {

    #[test]
    fn view_positions_count_from_one() {
        use mira_protocol::manifest::ViewKind;
        assert_eq!(super::position_word(ViewKind::Table, 0, 4), "row 1 of 4");
        assert_eq!(super::position_word(ViewKind::Log, 9, 4), "item 4 of 4");
        assert_eq!(super::position_word(ViewKind::Table, 0, 0), "no rows");
    }
}
