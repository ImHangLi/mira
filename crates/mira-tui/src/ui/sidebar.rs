//! The tool sidebar: one-off runs, then actions and views grouped by plugin.

use std::borrow::Cow;
use std::collections::HashMap;

use mira_protocol::clock;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{App, Entry, Focus, Hit};
use crate::logs::{cells, display};
use crate::theme::{Theme, Tone};

use super::marks::{entry_time, item_mark, kind_glyph, oneoff_mark, view_flag, view_tone};
use super::text::ellipsize;
use super::widgets::{panel, panel_title};

pub(super) fn draw_sidebar(f: &mut Frame, app: &mut App, t: &Theme, area: Rect) {
    app.hits.push((area, Hit::Sidebar));
    app.list_rows.clear();
    // The list gives the focus away while a program in the main pane takes the keys.
    let focus = app.focus == Focus::List && !app.term.is_focused();
    let total = app.items.len() + app.views.len();
    let title = if app.filter.is_empty() {
        format!("Tools · {total}")
    } else {
        format!(
            "Tools · /{} · {} of {total}",
            ellipsize(&display(&app.filter), 14),
            app.visible.len()
        )
    };
    // A long filter drops the `Tools` word first, then cuts the filter.
    let room = (area.width as usize).saturating_sub(4);
    let title = if cells(&title) <= room || app.filter.is_empty() {
        title
    } else {
        let tail = format!(" · {}/{total}", app.visible.len());
        let f = ellipsize(&display(&app.filter), room.saturating_sub(cells(&tail) + 1));
        format!("/{f}{tail}")
    };
    let block = panel(t, panel_title(t, &title, focus), focus);
    let inner = block.inner(area);
    f.render_widget(block, area);
    // A quiet hint on the last line while some default plugin is missing.
    let inner = if app.filter.is_empty() && !app.missing_defaults().is_empty() && inner.height > 3 {
        let [list, _, hint] = ratatui::layout::Layout::vertical([
            ratatui::layout::Constraint::Min(1),
            ratatui::layout::Constraint::Length(1),
            ratatui::layout::Constraint::Length(1),
        ])
        .areas(inner);
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" + ", t.word(Tone::Accent)),
                Span::styled("add a default plugin", t.muted()),
            ])),
            hint,
        );
        list
    } else {
        inner
    };
    app.list_height = inner.height as usize;
    let w = inner.width as usize;
    if total == 0 && app.oneoffs.is_empty() {
        let msg = if app.catalog_error.is_some() {
            " Catalog unavailable"
        } else {
            " No tools yet"
        };
        f.render_widget(Paragraph::new(Span::styled(msg, t.muted())), inner);
        return;
    }
    let mut rows: Vec<Line> = Vec::new();
    if app.visible.is_empty() {
        rows.push(Line::from(Span::styled(" No tool matches.", t.muted())));
        rows.push(Line::from(Span::styled(
            " Esc clears the filter.",
            t.muted(),
        )));
    }
    let mut sel_row = 0usize;
    let agents = agent_titles(app);
    let mut last_group: Option<String> = None;
    for (vi, &entry) in app.visible.iter().enumerate() {
        let (group, title, glyph, gstyle, flag) = match entry {
            Entry::Action(i) => {
                let item = &app.items[i];
                let m = item_mark(app, item);
                (
                    item.action_ref.plugin.to_string().to_uppercase(),
                    Cow::Borrowed(item.title.as_str()),
                    m.glyph,
                    m.style(t),
                    None,
                )
            }
            Entry::View(i) => {
                let v = &app.views[i];
                let tone = view_tone(app, v);
                let st = tone.map_or(t.muted(), |c| t.fg(c));
                let flag = view_flag(app, v).map(|w| (w, tone.map_or(t.muted(), |c| t.word(c))));
                (
                    v.view_ref.plugin.to_string().to_uppercase(),
                    Cow::Borrowed(v.title.as_str()),
                    kind_glyph(v.kind),
                    st,
                    flag,
                )
            }
            Entry::OneOff(i) => {
                let o = &app.oneoffs[i];
                let m = oneoff_mark(o);
                (
                    agents.get(o.group()).cloned().unwrap_or_default(),
                    Cow::Owned(o.title()),
                    m.glyph,
                    m.style(t),
                    None,
                )
            }
        };
        if last_group.as_deref() != Some(group.as_str()) {
            if last_group.is_some() {
                rows.push(Line::from(""));
            }
            // A one-off section title: the agent in the accent, then its task and count muted.
            let (name, rest) = group.split_once(MUTED).unwrap_or((&group, ""));
            let mut header = vec![Span::styled(
                format!(" {}", ellipsize(name, w.saturating_sub(2))),
                t.word(Tone::AccentDeep).add_modifier(Modifier::BOLD),
            )];
            let room = w.saturating_sub(cells(name) + 2);
            if !rest.is_empty() && room > 4 {
                header.push(Span::styled(ellipsize(rest, room), t.muted()));
            }
            rows.push(Line::from(header));
            last_group = Some(group);
        }
        let selected = vi == app.selected;
        if selected {
            sel_row = rows.len();
        }
        let age = entry_time(app, entry)
            .map(|ts| clock::age(clock::secs_since(ts)))
            .unwrap_or_default();
        // A state word (`stale`, `error`) goes before the age.
        let (flag, flag_st) = match flag {
            Some((w, st)) if age.is_empty() => (w.to_owned(), st),
            Some((w, st)) => (format!("{w} "), st),
            None => (String::new(), Style::default()),
        };
        let aw = cells(&age) + cells(&flag);
        // ` ` glyph(2) ` ` title … age ` `
        let title_w = w.saturating_sub(4 + 1 + aw + usize::from(aw > 0));
        let title = ellipsize(&display(&title), title_w);
        let gap = w.saturating_sub(4 + cells(&title) + aw + 1);
        let glyph = format!("{glyph:<2}");
        let line = if selected && focus {
            let s = t.selected();
            Line::from(vec![
                Span::styled(" ", s),
                Span::styled(glyph, s),
                Span::styled(" ", s),
                Span::styled(title, s),
                Span::styled(" ".repeat(gap), s),
                Span::styled(flag, s),
                Span::styled(age, s),
                Span::styled(" ", s),
            ])
        } else {
            let (lead, tstyle) = if selected {
                ("▌", t.word(Tone::Accent).add_modifier(Modifier::BOLD))
            } else {
                (" ", Style::default())
            };
            Line::from(vec![
                Span::styled(lead, t.fg(Tone::Accent)),
                Span::styled(glyph, gstyle),
                Span::raw(" "),
                Span::styled(title, tstyle),
                Span::raw(" ".repeat(gap)),
                Span::styled(flag, flag_st),
                Span::styled(age, t.muted()),
                Span::raw(" "),
            ])
        };
        app.list_rows.push(rows.len());
        rows.push(line);
    }
    let h = inner.height as usize;
    if !app.list_manual && sel_row < app.list_offset {
        // Show the group header above the first item of a group when possible.
        app.list_offset = sel_row.saturating_sub(1);
    } else if !app.list_manual && sel_row >= app.list_offset + h {
        app.list_offset = sel_row + 1 - h;
    }
    app.list_offset = app.list_offset.min(rows.len().saturating_sub(h));
    for (i, &row) in app.list_rows.iter().enumerate() {
        if row >= app.list_offset && row < app.list_offset + h {
            app.hits.push((
                Rect::new(
                    inner.x,
                    inner.y + (row - app.list_offset) as u16,
                    inner.width,
                    1,
                ),
                Hit::Entry(i),
            ));
        }
    }
    let visible: Vec<Line> = rows.into_iter().skip(app.list_offset).take(h).collect();
    f.render_widget(Paragraph::new(visible), inner);
}

/// Separates a one-off section's agent name from the muted rest of its title.
const MUTED: char = '\u{1f}';

/// Section titles for one-off runs: one per agent thread, `YOU` for runs people start, and
/// `EARLIER RUNS` for runs recorded before Mira 0.14, which do not say who started them. A
/// thread's title adds its task (`CLAUDE CODE · fix login`); two threads of the same agent
/// without a task are numbered in list order (`CLAUDE CODE · 2`). A section that hides
/// finished runs says how many.
fn agent_titles(app: &App) -> HashMap<String, String> {
    let mut titles: HashMap<String, String> = HashMap::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    for o in &app.oneoffs {
        if titles.contains_key(o.group()) {
            continue;
        }
        // The list is newest first, so this run carries the thread's latest task.
        let (name, task) = o
            .requester
            .as_ref()
            .map_or(("EARLIER RUNS".to_owned(), None), |r| {
                (r.name.to_uppercase(), r.task.clone())
            });
        let n = seen.entry(name.clone()).or_default();
        *n += 1;
        let mut rest = match task {
            Some(task) => format!(" · {task}"),
            None if *n > 1 => format!(" · {n}"),
            None => String::new(),
        };
        if let Some(h) = app.oneoff_hidden.get(o.group()) {
            rest.push_str(&format!("  +{h} earlier"));
        }
        let title = if rest.is_empty() {
            name
        } else {
            format!("{name}{MUTED}{rest}")
        };
        titles.insert(o.group().to_owned(), title);
    }
    titles
}
