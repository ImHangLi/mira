//! The main pane of an action: its card, status, and tab content.

use mira_protocol::clock;
use mira_protocol::manifest::ActionMode;
use mira_protocol::run::{Lifecycle, Outcome};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Padding, Paragraph};

use crate::app::{App, Focus, Hit, Inputs, Intent, Item, Modal, Tab};
use crate::logs::display;
use crate::theme::{Theme, Tone};

use super::history::draw_history;
use super::log_panel::draw_logs;
use super::marks::item_mark;
use super::oneoff::draw_oneoff;
use super::output_tab::draw_output_tab;
use super::text::{ago, cleanup_word, ellipsize, enum_word, fit_spans, wrap};
use super::view_panel::draw_view;
use super::widgets::{chip, empty_card, panel, panel_title, tab_bar, tab_hits};

pub(super) fn draw_main(f: &mut Frame, app: &mut App, t: &Theme, area: Rect) {
    // The selected interactive program shows its live screen here.
    if app.term.is_open() {
        let title = app
            .term
            .attach
            .as_ref()
            .map_or_else(|| "Program".to_owned(), |a| app.title_of(&a.action_ref));
        let title = match app.term.note() {
            Some(n) => format!("{title} · {n}"),
            None => title,
        };
        let block = panel(
            t,
            panel_title(t, &display(&title), app.term.is_focused()),
            app.term.is_focused(),
        );
        let inner = block.inner(area);
        f.render_widget(block, area);
        crate::terminal::draw(f, &mut app.term, &mut app.hits, inner, t.mode.enabled());
        return;
    }
    if let Some(i) = app.selected_oneoff_index() {
        draw_oneoff(f, app, t, i, area);
        return;
    }
    if app.items.is_empty() && app.views.is_empty() {
        match &app.catalog_error {
            Some(e) => {
                let hint = format!("[{}] {}", e.code, e.message);
                empty_card(f, t, area, "Catalog unavailable", &hint)
            }
            None => empty_card(
                f,
                t,
                area,
                "No tools yet",
                "Ask your agent to set up Mira (docs/agents.md), or write a plugin in .mira/plugins/ \
                 (see `mira schema plugin`), then `mira validate .mira`.",
            ),
        }
        return;
    }
    if app.selected_view().is_some() {
        draw_view(f, app, t, area);
        return;
    }
    let Some(item) = app.selected_item() else {
        empty_card(
            f,
            t,
            area,
            "Nothing selected",
            "Pick a tool on the left, or press Esc to clear the filter.",
        );
        return;
    };
    let focus = app.focus == Focus::Logs;
    // The card shows the title; the border names the pane.
    let block = panel(t, panel_title(t, "Tool", focus), focus).padding(Padding::horizontal(1));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let w = inner.width as usize;
    let a = item.action_ref.clone();

    // Card: title and chips, description, status, inputs and schedule.
    let mut card: Vec<Line> = Vec::new();
    let mut l1 = vec![
        Span::styled(ellipsize(&display(&item.title), w / 2), t.bold()),
        Span::raw("  "),
        Span::styled(a.to_string(), t.muted()),
        Span::raw("  "),
        chip(
            t,
            match item.mode {
                ActionMode::Process => "service",
                ActionMode::Task => "task",
            },
            Tone::Sky,
        ),
    ];
    if !item.enabled {
        l1.push(Span::raw(" "));
        l1.push(Span::styled(
            " ‖ disabled ",
            t.muted().add_modifier(Modifier::REVERSED),
        ));
    }
    card.push(Line::from(fit_spans(l1, w)));
    let desc = display(&item.description);
    if !desc.trim().is_empty() {
        let mut parts = wrap(&desc, w);
        if parts.len() > 2 {
            parts.truncate(2);
            parts[1] = ellipsize(&format!("{} …", parts[1]), w);
        }
        card.extend(
            parts
                .into_iter()
                .map(|p| Line::from(Span::styled(p, t.muted()))),
        );
    }
    card.push(Line::from(fit_spans(status_spans(app, t, item), w)));
    let extras = extras_text(app, &a);
    if !extras.is_empty() {
        card.push(Line::from(Span::styled(ellipsize(&extras, w), t.muted())));
    }
    let shown = app.shown_tab();
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
    app.hits.push((body, Hit::Pane));
    app.pane_height = body.height as usize;
    tab_hits(app, &Tab::ALL.map(Tab::label), tabs_area);
    match shown {
        Tab::Logs => draw_logs(f, app, t, &a, tabs_area, body),
        Tab::History => {
            let n = match &app.modal {
                Modal::History { runs: Some(r), .. } => format!(
                    "{} run{} · newest first",
                    r.len(),
                    if r.len() == 1 { "" } else { "s" }
                ),
                _ => String::new(),
            };
            f.render_widget(Paragraph::new(tab_bar(t, shown, &n, w)), tabs_area);
            draw_history(f, app, t, body);
        }
        Tab::Output => {
            f.render_widget(Paragraph::new(tab_bar(t, shown, "", w)), tabs_area);
            draw_output_tab(f, app, t, &a, body);
        }
    }
}

/// The card's status line: a mark and a word, then details.
fn status_spans(app: &App, t: &Theme, item: &Item) -> Vec<Span<'static>> {
    let a = &item.action_ref;
    let mark = item_mark(app, item);
    let head = |word: String| -> Vec<Span<'static>> {
        vec![
            Span::styled(format!("{} ", mark.glyph), mark.style(t)),
            Span::styled(word, mark.word_style(t).add_modifier(Modifier::BOLD)),
        ]
    };
    let mut details: Vec<String> = Vec::new();
    let mut spans = if let Some(i) = app.pending.get(a) {
        head(match i {
            Intent::Stop => "stopping…".into(),
            _ => "starting…".into(),
        })
    } else if let Some(r) = app.active.get(a) {
        let word = match r.lifecycle {
            Lifecycle::Stopping { reason } => format!("stopping ({})", enum_word(reason)),
            l => l.word().to_owned(),
        };
        details.push(format!(
            "started {} ({})",
            ago(r.started_at),
            app.clock.hm(r.started_at)
        ));
        let health = enum_word(r.reported_health.state);
        if !health.is_empty() && health != "unknown" {
            details.push(format!("health {health}"));
        }
        head(word)
    } else if let Some(l) = app.last.get(a) {
        let mut word = l.lifecycle.word().to_owned();
        if let (Some(s), Some(e)) = (l.started_at, l.ended_at)
            && matches!(
                l.lifecycle,
                Lifecycle::Finished {
                    outcome: Outcome::Succeeded
                }
            )
        {
            word = format!("{word} in {}", clock::span(s, e));
        }
        if let Some(e) = &l.exit {
            match (e.code, &e.signal) {
                // Exit 0 with `ok: false`: the plugin reported the failure itself.
                (Some(0), _) if l.result.as_ref().is_some_and(|r| !r.ok) => {
                    details.push("reported by the plugin".into())
                }
                (Some(0), _) => {}
                (Some(c), _) => details.push(format!("exit {c}")),
                (None, Some(s)) => details.push(s.to_string()),
                _ => {}
            }
        }
        if let Some(e) = l.ended_at {
            details.insert(0, ago(e));
        }
        let mut h = head(word);
        if let Some(c) = l.cleanup.as_ref().and_then(cleanup_word) {
            h.push(Span::styled(
                format!("  {c}"),
                t.word(Tone::Rose).add_modifier(Modifier::BOLD),
            ));
        }
        h
    } else if !item.enabled {
        head("disabled".into())
    } else {
        details.push("Enter runs it".into());
        head("not run yet".into())
    };
    for d in details {
        spans.push(Span::styled(format!(" · {d}"), t.muted()));
    }
    spans
}

/// Inputs and schedule facts for the card.
fn extras_text(app: &App, a: &mira_protocol::ids::ActionRef) -> String {
    let mut parts: Vec<String> = Vec::new();
    match app.inputs.get(a).map(|(_, i)| i) {
        Some(Inputs::Form(s)) => {
            let req = crate::form::required_names(s);
            parts.push(if req.is_empty() {
                "input form (optional fields)".to_owned()
            } else {
                format!("input form, required: {}", req.join(", "))
            });
        }
        Some(Inputs::Unknown(m)) => parts.push(format!("inputs unknown: {m}")),
        _ => {}
    }
    if let Some(sc) = app.schedule_of(a) {
        let every = sc.every_ms / 1000;
        let every = if every >= 60 && every % 60 == 0 {
            format!("{}m", every / 60)
        } else {
            format!("{every}s")
        };
        let next = sc
            .next_at
            .map_or(String::new(), |n| format!(", next {}", app.clock.until(n)));
        let missed = if sc.missed_ticks > 0 {
            format!(", {} skipped tick(s)", sc.missed_ticks)
        } else {
            String::new()
        };
        parts.push(format!(
            "schedule {} every {every}{next}{missed}",
            if sc.enabled { "ON" } else { "off" }
        ));
    } else if app.has_schedule(a) {
        parts.push("schedule off (never switched on; t turns it on)".to_owned());
    }
    parts.join(" · ")
}
