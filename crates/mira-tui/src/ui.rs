//! Rendering: a header, the tool sidebar, the main pane (a card, tabs, and the tab
//! content), a right rail on wide terminals, a notice line only when there is a notice, and
//! the key footer. Only visible rows are built. State is never shown by color alone: every
//! state has a glyph and a word (see [`crate::theme::Mark`]).

use std::borrow::Cow;

use mira_protocol::clock::{self, LocalClock};
use mira_protocol::ipc::{SessionMode, SessionState};
use mira_protocol::manifest::{ActionMode, ViewKind};
use mira_protocol::run::{CleanupState, Lifecycle, LogStream, Outcome, RunRecord, RunResult};
use mira_protocol::time::Timestamp;
use mira_protocol::view::Freshness;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Padding, Paragraph};

use crate::app::{
    App, Binding, Cmd, Entry, Focus, Inputs, Intent, Item, Modal, OneOff, Tab, ViewItem,
};
use crate::form::Kind;
use crate::logs::{LogPane, cells, display, slice_cells};
use crate::theme::{self, Mark, Theme, Tone};
use crate::views::{durability_word, freshness_word, kind_word};

pub const MIN_W: u16 = 60;
pub const MIN_H: u16 = 18;
/// Below this width one pane shows at a time.
const NARROW_W: u16 = 80;
/// From this width a right rail shows recent runs and the session.
const RAIL_MIN_W: u16 = 160;
const RAIL_W: u16 = 34;
/// Runs listed in the rail.
const RAIL_RUNS: usize = 8;

/// Sidebar width: 30% of the terminal, at least 28 and at most 48 columns.
pub fn sidebar_width(total: u16) -> u16 {
    let w = (u32::from(total) * 30 / 100).clamp(28, 48);
    u16::try_from(w).unwrap_or(48)
}

fn ago(ts: Timestamp) -> String {
    format!("{} ago", clock::age(clock::secs_since(ts)))
}

/// One footer chip: the key cells, the label cells, and whether it must stay.
#[derive(Clone, Copy, Debug)]
pub struct ChipSize {
    pub keys: usize,
    pub label: usize,
    pub pinned: bool,
}

/// Which chips fit in `width` cells. A chip is ` key ` plus ` label`; two spaces separate
/// chips. A chip always shows with its label, or not at all: from the last chip back,
/// unpinned chips go first, then pinned ones.
pub fn fit_chips(chips: &[ChipSize], width: usize) -> Vec<bool> {
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

fn panel<'a>(t: &Theme, title: impl Into<Line<'a>>, focus: bool) -> Block<'a> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(t.border(focus))
        .title(title)
}

fn panel_title(t: &Theme, text: &str, focus: bool) -> Line<'static> {
    let st = if focus {
        t.word(Tone::Accent).add_modifier(Modifier::BOLD)
    } else {
        t.bold()
    };
    Line::from(Span::styled(format!(" {text} "), st))
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

// ----- text helpers -------------------------------------------------------------------

fn short_root(root: &str, max: usize) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let r = if !home.is_empty() && root.starts_with(&home) {
        format!("~{}", &root[home.len()..])
    } else {
        root.to_owned()
    };
    if cells(&r) <= max {
        return r;
    }
    let tail: Vec<&str> = r.rsplit('/').take(2).collect();
    let s = format!(
        "…/{}/{}",
        tail.get(1).unwrap_or(&""),
        tail.first().unwrap_or(&"")
    );
    ellipsize(&s, max)
}

/// Longest branch name the header shows before it cuts the end.
const BRANCH_MAX: usize = 24;
/// Shortest branch the header keeps before it drops the chip.
const BRANCH_MIN: usize = 8;

/// Cuts `s` to `max` cells, marking the cut with one `…`.
fn ellipsize(s: &str, max: usize) -> String {
    if cells(s) <= max {
        return s.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    format!("{}…", slice_cells(s, 0, max - 1))
}

/// `s` cut or padded to exactly `w` cells.
fn pad(s: &str, w: usize) -> String {
    let s = ellipsize(s, w);
    let n = cells(&s);
    format!("{s}{}", " ".repeat(w.saturating_sub(n)))
}

/// Word-wraps `text` to `width` cells; words longer than a line are cut into pieces.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let mut word = word.to_owned();
        loop {
            let used = cells(&line);
            let gap = usize::from(used > 0);
            if used + gap + cells(&word) <= width {
                if gap == 1 {
                    line.push(' ');
                }
                line.push_str(&word);
                break;
            }
            if used > 0 {
                out.push(std::mem::take(&mut line));
                continue;
            }
            // A word wider than the whole line.
            out.push(slice_cells(&word, 0, width));
            word = slice_cells(&word, width, cells(&word) - width);
            if word.is_empty() {
                break;
            }
        }
    }
    if !line.is_empty() || out.is_empty() {
        out.push(line);
    }
    out
}

fn line_cells(spans: &[Span]) -> usize {
    spans.iter().map(|s| cells(&s.content)).sum()
}

/// Cuts a span list to `w` cells.
fn fit_spans(spans: Vec<Span<'static>>, w: usize) -> Vec<Span<'static>> {
    let mut out = Vec::with_capacity(spans.len());
    let mut left = w;
    for s in spans {
        if left == 0 {
            break;
        }
        let n = cells(&s.content);
        if n <= left {
            left -= n;
            out.push(s);
        } else {
            out.push(Span::styled(ellipsize(&s.content, left), s.style));
            break;
        }
    }
    out
}

fn short_id(id: &str) -> String {
    slice_cells(id, 0, 10)
}

/// The run's freshness word; runs without provenance read as historical.
fn freshness_of(rec: &RunRecord) -> &'static str {
    freshness_word(
        rec.provenance
            .as_ref()
            .map_or(Freshness::Historical, |p| p.freshness),
    )
}

/// A failed cleanup in a few words: `cleanup failed (exit 4)`, `cleanup timed out`;
/// `None` when cleanup did not fail.
pub fn cleanup_word(c: &CleanupState) -> Option<String> {
    match c {
        CleanupState::Failed {
            timed_out: true, ..
        } => Some("cleanup timed out".into()),
        CleanupState::Failed {
            exit_code: Some(code),
            ..
        } => Some(format!("cleanup failed (exit {code})")),
        CleanupState::Failed { .. } => Some("cleanup failed".into()),
        _ => None,
    }
}

fn enum_word<T: serde::Serialize>(v: T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

// ----- state marks --------------------------------------------------------------------

fn life_mark(l: Lifecycle) -> Mark {
    match l {
        Lifecycle::Starting => theme::STARTING,
        Lifecycle::Running => theme::RUNNING,
        Lifecycle::Stopping { .. } => theme::STOPPING,
        Lifecycle::Finished { outcome } => match outcome {
            Outcome::Succeeded => theme::OK,
            Outcome::Cancelled => theme::STOPPED,
            Outcome::Failed => theme::FAILED,
            Outcome::TimedOut => Mark::new("✗", "timed out", Some(Tone::Rose)),
            Outcome::Interrupted => Mark::new("✗", "interrupted", Some(Tone::Rose)),
        },
    }
}

fn item_mark(app: &App, item: &Item) -> Mark {
    if let Some(i) = app.pending.get(&item.action_ref) {
        return match i {
            Intent::Stop => theme::STOPPING,
            _ => theme::STARTING,
        };
    }
    if let Some(r) = app.active.get(&item.action_ref) {
        return life_mark(r.lifecycle);
    }
    if !item.enabled {
        return theme::DISABLED;
    }
    match app.last.get(&item.action_ref).map(|l| l.lifecycle) {
        Some(Lifecycle::Finished { outcome }) => life_mark(Lifecycle::Finished { outcome }),
        Some(_) => Mark::new("■", "ended", None),
        None => theme::NOT_RUN,
    }
}

fn oneoff_mark(o: &OneOff) -> Mark {
    if o.stopping {
        theme::STOPPING
    } else {
        life_mark(o.lifecycle)
    }
}

fn kind_glyph(k: ViewKind) -> &'static str {
    match (k, theme::ascii()) {
        (ViewKind::Table, false) => "▦",
        (ViewKind::Table, true) => "[]",
        (ViewKind::Log, false) => "≡",
        (ViewKind::Log, true) => ">_",
        (ViewKind::Tree, _) => "#",
        (ViewKind::Text, false) => "¶",
        (ViewKind::Text, true) => "Aa",
        (ViewKind::Json, _) => "{}",
    }
}

/// The word a sidebar view row shows for a state its tone also shows, so the state never
/// depends on color alone.
fn view_flag(app: &App, v: &ViewItem) -> Option<&'static str> {
    let p = app.view_panes.get(&v.view_ref)?;
    if p.error.is_some() {
        return Some("error");
    }
    let m = p.meta.as_ref()?;
    p.revision()?;
    (m.freshness == Freshness::Stale).then_some("stale")
}

fn view_tone(app: &App, v: &ViewItem) -> Option<Tone> {
    let p = app.view_panes.get(&v.view_ref)?;
    if p.error.is_some() {
        return Some(Tone::Rose);
    }
    let m = p.meta.as_ref()?;
    p.revision()?;
    match m.freshness {
        Freshness::Current => Some(Tone::Leaf),
        Freshness::Stale => Some(Tone::Amber),
        Freshness::Historical => None,
    }
}

/// When the entry last changed: the active run's start, the last run's end, or a view's
/// recorded time.
fn entry_time(app: &App, e: Entry) -> Option<Timestamp> {
    match e {
        Entry::Action(i) => {
            let a = &app.items.get(i)?.action_ref;
            app.active
                .get(a)
                .map(|r| r.started_at)
                .or_else(|| app.last.get(a).and_then(|l| l.ended_at))
        }
        Entry::View(i) => {
            let v = app.views.get(i)?;
            app.view_panes
                .get(&v.view_ref)
                .and_then(|p| p.meta.as_ref())
                .and_then(|m| m.recorded_at)
        }
        Entry::OneOff(i) => {
            let o = app.oneoffs.get(i)?;
            Some(o.ended_at.unwrap_or(o.started_at))
        }
    }
}

// ----- header -------------------------------------------------------------------------

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
                            format!(" here · {n} window{}", if n == 1 { "" } else { "s" }),
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

fn draw_header(f: &mut Frame, app: &App, t: &Theme, area: Rect) {
    let w = area.width as usize;
    let right = session_spans(app, t);
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

// ----- sidebar ------------------------------------------------------------------------

fn draw_sidebar(f: &mut Frame, app: &mut App, t: &Theme, area: Rect) {
    let focus = app.focus == Focus::List;
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
                    "ONE-OFF RUNS".to_owned(),
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
            rows.push(Line::from(Span::styled(
                format!(" {}", ellipsize(&group, w.saturating_sub(2))),
                t.word(Tone::AccentDeep).add_modifier(Modifier::BOLD),
            )));
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
        rows.push(line);
    }
    let h = inner.height as usize;
    if sel_row < app.list_offset {
        // Show the group header above the first item of a group when possible.
        app.list_offset = sel_row.saturating_sub(1);
    } else if sel_row >= app.list_offset + h {
        app.list_offset = sel_row + 1 - h;
    }
    app.list_offset = app.list_offset.min(rows.len().saturating_sub(1));
    let visible: Vec<Line> = rows.into_iter().skip(app.list_offset).take(h).collect();
    f.render_widget(Paragraph::new(visible), inner);
}

// ----- main pane ----------------------------------------------------------------------

/// A centered card with a bold headline and a dim hint, for empty states.
fn empty_card(f: &mut Frame, t: &Theme, area: Rect, head: &str, hint: &str) {
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

fn chip(t: &Theme, text: &str, tone: Tone) -> Span<'static> {
    Span::styled(format!(" {text} "), t.chip(tone))
}

fn tab_bar(t: &Theme, shown: Tab, info: &str, w: usize) -> Line<'static> {
    let labels = Tab::ALL.map(Tab::label);
    let at = Tab::ALL.iter().position(|x| *x == shown).unwrap_or(0);
    tabs_line(t, &labels, at, info, w)
}

/// Tab labels with `shown` underlined, then `info` on the right.
fn tabs_line(t: &Theme, labels: &[&str], shown: usize, info: &str, w: usize) -> Line<'static> {
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
fn right_info(mut spans: Vec<Span<'static>>, info: &str, t: &Theme, w: usize) -> Line<'static> {
    let used = line_cells(&spans);
    let room = w.saturating_sub(used + 3);
    if room > 0 && !info.is_empty() {
        let info = ellipsize(&display(info), room);
        spans.push(Span::raw(" ".repeat(w - used - cells(&info))));
        spans.push(Span::styled(info, t.muted()));
    }
    Line::from(spans)
}

fn draw_main(f: &mut Frame, app: &mut App, t: &Theme, area: Rect) {
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
        Span::raw(" "),
        Span::styled(
            format!("◇ {}", a.plugin),
            t.word(Tone::AccentDeep).add_modifier(Modifier::BOLD),
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

/// The main pane of a one-off `mira exec` run: a card like a tool's, then its logs or
/// the details of the run.
fn draw_oneoff(f: &mut Frame, app: &mut App, t: &Theme, i: usize, area: Rect) {
    let focus = app.focus == Focus::Logs;
    let block =
        panel(t, panel_title(t, "One-off run", focus), focus).padding(Padding::horizontal(1));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let w = inner.width as usize;
    let o = &app.oneoffs[i];
    let label = display(&o.title());
    let source = o.source.map(enum_word).unwrap_or_default();
    let mut how = format!("mira exec --label \"{label}\"");
    if !source.is_empty() {
        how.push_str(&format!(" · from {source}"));
    }
    let card = vec![
        Line::from(fit_spans(
            vec![
                Span::styled(ellipsize(&label, w / 2), t.bold()),
                Span::raw("  "),
                chip(t, "one-off", Tone::Sky),
            ],
            w,
        )),
        Line::from(Span::styled(ellipsize(&how, w), t.muted())),
        Line::from(fit_spans(oneoff_status(app, t, o), w)),
        Line::from(Span::styled(
            ellipsize("Worked? Ask your agent to save it as a plugin.", w),
            t.muted().add_modifier(Modifier::ITALIC),
        )),
    ];
    let details = oneoff_details(app, t, o, w);
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
    let labels = ["Logs", "Details"];
    if app.oneoff_details {
        f.render_widget(Paragraph::new(tabs_line(t, &labels, 1, "", w)), tabs_area);
        f.render_widget(Paragraph::new(details), body);
        return;
    }
    let clock = app.clock;
    let Some(p) = app.oneoffs[i].pane.as_mut() else {
        f.render_widget(Paragraph::new(tabs_line(t, &labels, 0, "", w)), tabs_area);
        f.render_widget(Paragraph::new(Span::styled("loading…", t.muted())), body);
        return;
    };
    let bw = body.width as usize;
    p.height = body.height as usize;
    p.width = bw.saturating_sub(if bw >= 40 { 11 } else { 2 }).max(1);
    let bar = log_bar(p, None);
    f.render_widget(Paragraph::new(tabs_line(t, &labels, 0, &bar, w)), tabs_area);
    draw_records(f, p, t, focus, clock, body);
}

/// The one-off card's status line: a mark and a word, then details.
fn oneoff_status(app: &App, t: &Theme, o: &OneOff) -> Vec<Span<'static>> {
    let mark = oneoff_mark(o);
    let mut word = if o.stopping {
        "stopping…".to_owned()
    } else {
        match o.lifecycle {
            Lifecycle::Stopping { reason } => format!("stopping ({})", enum_word(reason)),
            l => l.word().to_owned(),
        }
    };
    let mut details: Vec<String> = Vec::new();
    match o.ended_at {
        Some(e) if !o.lifecycle.is_active() => {
            word = format!("{word} in {}", clock::span(o.started_at, e));
            details.push(ago(e));
            if let Some(x) = &o.exit {
                match (x.code, &x.signal) {
                    (Some(c), _) => details.push(format!("exit {c}")),
                    (None, Some(s)) => details.push(s.to_string()),
                    _ => {}
                }
            }
        }
        _ => details.push(format!(
            "started {} ({})",
            ago(o.started_at),
            app.clock.hm(o.started_at)
        )),
    }
    details.push(short_id(o.run_id.as_str()));
    let mut spans = vec![
        Span::styled(format!("{} ", mark.glyph), mark.style(t)),
        Span::styled(word, mark.word_style(t).add_modifier(Modifier::BOLD)),
    ];
    if let Some(c) = o.cleanup.as_ref().and_then(cleanup_word) {
        spans.push(Span::styled(
            format!("  {c}"),
            t.word(Tone::Rose).add_modifier(Modifier::BOLD),
        ));
    }
    for d in details {
        spans.push(Span::styled(format!(" · {d}"), t.muted()));
    }
    spans
}

/// The Details tab of a one-off run: its record, field by field.
fn oneoff_details(app: &App, t: &Theme, o: &OneOff, w: usize) -> Vec<Line<'static>> {
    let row = |k: &str, v: String| {
        Line::from(vec![
            Span::styled(format!("{k:<10}"), t.muted()),
            Span::raw(ellipsize(&display(&v), w.saturating_sub(10))),
        ])
    };
    let id = o.run_id.to_string();
    let mut lines = vec![
        row("label", o.title()),
        row("run", id.clone()),
        row(
            "state",
            format!("{} {}", oneoff_mark(o).glyph, o.lifecycle.word()),
        ),
        row(
            "started",
            format!(
                "{} ({})",
                app.clock.when_seconds(o.started_at),
                ago(o.started_at)
            ),
        ),
    ];
    if let Some(e) = o.ended_at {
        lines.push(row("ended", app.clock.when_seconds(e)));
        lines.push(row("took", clock::span(o.started_at, e)));
    } else {
        lines.push(row(
            "running",
            format!("{} so far", clock::age(clock::secs_since(o.started_at))),
        ));
    }
    if let Some(x) = &o.exit {
        let v = match (x.code, &x.signal) {
            (Some(c), _) => format!("code {c}"),
            (None, Some(s)) => format!("signal {s}"),
            _ => "none".into(),
        };
        lines.push(row("exit", v));
    }
    if let Some(c) = o.cleanup.as_ref().and_then(cleanup_word) {
        lines.push(row("cleanup", c));
    }
    if let Some(s) = o.source {
        lines.push(row("from", enum_word(s)));
    }
    lines.push(Line::from(""));
    for hint in [
        format!("`mira logs {}` prints its output.", short_id(&id)),
        format!("`mira runs {}` prints the full record.", short_id(&id)),
    ] {
        lines.push(Line::from(Span::styled(ellipsize(&hint, w), t.muted())));
    }
    lines
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
        details.push(short_id(&r.run_id.to_string()));
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
                (Some(c), _) => details.push(format!("exit {c}")),
                (None, Some(s)) => details.push(s.to_string()),
                _ => {}
            }
        }
        if let Some(e) = l.ended_at {
            details.insert(0, ago(e));
        }
        details.push(short_id(&l.run_id.to_string()));
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

/// The log status: follow or pin, the run, and anything the view is missing.
fn log_bar(p: &LogPane, old: Option<&String>) -> String {
    let mut parts: Vec<String> = Vec::new();
    // An older run chosen in the history list reads as history, never as the current run.
    if let Some(o) = old {
        parts.push(o.clone());
    }
    if p.is_pinned() {
        parts.push(format!("PINNED · {} newer below", p.below()));
    } else {
        parts.push("FOLLOW".into());
    }
    if p.wrap {
        parts.push("wrap".into());
    } else if p.hscroll > 0 {
        parts.push(format!("col +{}", p.hscroll));
    }
    if p.loading || p.loading_older {
        parts.push("loading…".into());
    }
    if p.lost_anchor || p.trimmed {
        parts.push("older lines trimmed from this view".into());
    }
    if let Some((a, b)) = p.gap {
        parts.push(format!("#{a}-#{b} skipped by a stream reset"));
    }
    if p.history_gone()
        && let Some(first) = p.first_available
    {
        parts.push(format!(
            "earlier output no longer available (starts at #{first})"
        ));
    }
    if let Some(r) = &p.run_id {
        parts.push(r.to_string());
    }
    parts.join(" · ")
}

fn draw_logs(
    f: &mut Frame,
    app: &mut App,
    t: &Theme,
    a: &mira_protocol::ids::ActionRef,
    tabs_area: Rect,
    body: Rect,
) {
    let w = body.width as usize;
    let gutter = if w >= 40 { 11 } else { 2 };
    let text_w = w.saturating_sub(gutter).max(1);
    let focus = app.focus == Focus::Logs;
    let clock = app.clock;
    let old = app.viewing.get(a).map(|rec| {
        let when = rec
            .ended_at
            .map_or(String::new(), |e| format!(" at {}", app.clock.hm(e)));
        (
            rec.run_id.clone(),
            format!(
                "{} RUN · {}{when}",
                freshness_of(rec).to_uppercase(),
                rec.lifecycle.word()
            ),
        )
    });
    // A PTY run that prints nothing is often waiting for input: show its screen's last line.
    let waiting = app.silent_pty_run().map(|r| app.screens.get(r).cloned());
    let Some(p) = app.panes.get_mut(a) else {
        f.render_widget(Paragraph::new(tab_bar(t, Tab::Logs, "", w)), tabs_area);
        f.render_widget(Paragraph::new(Span::styled("loading…", t.muted())), body);
        return;
    };
    p.height = body.height as usize;
    p.width = text_w;
    let old = old
        .as_ref()
        .filter(|o| p.run_id.as_ref() == Some(&o.0))
        .map(|o| &o.1);
    let bar = log_bar(p, old);
    let mut tabs = tab_bar(t, Tab::Logs, &bar, w);
    if old.is_some() {
        // Historical runs stand out: the whole bar is amber.
        tabs = tabs.patch_style(t.word(Tone::Amber));
    }
    f.render_widget(Paragraph::new(tabs), tabs_area);
    if p.records.is_empty()
        && p.error.is_none()
        && !p.loading
        && let Some(screen) = waiting
    {
        let mut lines = Vec::new();
        if let Some(l) = screen {
            lines.push(Line::from(Span::styled(
                ellipsize(&display(&l), w),
                t.muted(),
            )));
        }
        lines.push(Line::from(vec![
            Span::styled("waiting for input", t.word(Tone::Amber)),
            Span::styled(" · ", t.muted()),
            Span::styled("a", t.bold()),
            Span::styled(" attaches", t.muted()),
        ]));
        f.render_widget(Paragraph::new(lines), body);
        return;
    }
    draw_records(f, p, t, focus, clock, body);
}

/// The lines of a log panel, or why there are none.
fn draw_records(f: &mut Frame, p: &LogPane, t: &Theme, focus: bool, clock: LocalClock, body: Rect) {
    let w = body.width as usize;
    let gutter = if w >= 40 { 11 } else { 2 };
    let text_w = w.saturating_sub(gutter).max(1);
    let sel = t.selected();
    if p.records.is_empty() {
        let msg = if let Some(e) = &p.error {
            format!("Cannot read logs: {e}")
        } else if p.loading {
            "loading…".to_owned()
        } else if p.run_id.is_none() {
            "No runs yet. Press Enter or s to start it.".to_owned()
        } else {
            "(no output yet)".to_owned()
        };
        f.render_widget(Paragraph::new(Span::styled(msg, t.muted())), body);
        return;
    }
    let cursor = if focus && p.is_pinned() {
        p.cursor_at()
    } else {
        None
    };
    let rows = p.visible();
    let mut lines = Vec::with_capacity(rows.len());
    for (idx, row) in rows {
        let r = &p.records[idx];
        let text = display(&r.text);
        let part = if p.wrap {
            slice_cells(&text, row * text_w, text_w)
        } else {
            slice_cells(&text, p.hscroll, text_w)
        };
        let style = if matches!(r.stream, LogStream::Host | LogStream::Plugin) {
            t.muted()
        } else {
            Style::default()
        };
        let picked = p.in_selection(idx) && (p.anchor.is_some() || Some(idx) == cursor);
        let mut spans = Vec::with_capacity(4);
        if gutter > 2 {
            let at = if row == 0 {
                clock.hms(r.recorded_at)
            } else {
                "        ".into()
            };
            spans.push(Span::styled(at, t.muted()));
            spans.push(Span::raw(" "));
        }
        // A thin amber bar marks stderr; host lines get a dim dot.
        let (mark, mstyle) = match r.stream {
            LogStream::Stderr => ("▎", t.fg(Tone::Amber)),
            LogStream::Host if row == 0 => ("·", t.muted()),
            _ => (" ", Style::default()),
        };
        spans.push(Span::styled(mark, mstyle));
        spans.push(Span::raw(" "));
        if picked {
            let n = cells(&part);
            spans.push(Span::styled(
                format!("{part}{}", " ".repeat(text_w.saturating_sub(n))),
                sel,
            ));
        } else {
            spans.push(Span::styled(part, style));
        }
        lines.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(lines), body);
}

fn draw_history(f: &mut Frame, app: &App, t: &Theme, area: Rect) {
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

/// The result of the run shown in the Logs tab.
fn shown_result<'a>(app: &'a App, a: &mira_protocol::ids::ActionRef) -> Option<&'a RunResult> {
    match app.viewing.get(a) {
        Some(rec) => rec.result.as_ref(),
        None if app.active.contains_key(a) => None,
        None => app.last.get(a).and_then(|l| l.result.as_ref()),
    }
}

fn draw_output_tab(
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

fn draw_view(f: &mut Frame, app: &mut App, t: &Theme, area: Rect) {
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
            Span::raw(" "),
            Span::styled(
                format!("◇ {}", r.plugin),
                t.word(Tone::AccentDeep).add_modifier(Modifier::BOLD),
            ),
        ],
        w,
    ));
    // Source, freshness, durability, and time are always shown; freshness never by color alone.
    let status: Vec<Span> = match &p.meta {
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
            Some(rev) => {
                let src = match (m.source_kind, &m.source_run_id) {
                    (_, Some(run)) => format!(" · from run {}", short_id(&run.to_string())),
                    (Some(k), None) => format!(" · published by {}", enum_word(k)),
                    (None, None) => String::new(),
                };
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
                // Past data names where it came from instead of the word "historical".
                // The head says it all; the host's reason would only repeat it.
                let (word, why, at, src) = match (m.freshness, &m.source_run_id, m.recorded_at) {
                    (Freshness::Historical, Some(run), _) => (
                        format!("from run {} (ended)", short_id(&run.to_string())),
                        String::new(),
                        at,
                        String::new(),
                    ),
                    (Freshness::Historical, None, Some(a)) if m.source_kind.is_some() => (
                        format!("published {}", ago(a)),
                        String::new(),
                        format!(" · {}", hm(a)),
                        String::new(),
                    ),
                    _ => (freshness_word(m.freshness).to_owned(), why, at, src),
                };
                // The revision comes right after the state, so a narrow pane cuts the
                // details first.
                vec![
                    Span::styled(format!("{glyph} {word}"), st.add_modifier(Modifier::BOLD)),
                    Span::styled(" · ", t.muted()),
                    Span::styled(format!("rev {rev}"), t.bold()),
                    Span::styled(
                        format!("{why} · {}{at}{src}", durability_word(m.durability)),
                        t.muted(),
                    ),
                ]
            }
        },
    };
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
        bar.push(format!("row chosen in rev {s}"));
    }
    if !p.row_actions.is_empty() {
        let names: Vec<String> = p.row_actions.iter().map(ToString::to_string).collect();
        bar.push(format!("row actions: {}", names.join(", ")));
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

// ----- right rail ---------------------------------------------------------------------

fn draw_rail(f: &mut Frame, app: &App, t: &Theme, area: Rect) {
    let runs_h = (RAIL_RUNS as u16 + 2).min(area.height / 2);
    let [runs_area, session_area] =
        Layout::vertical([Constraint::Length(runs_h), Constraint::Min(3)]).areas(area);
    let block = panel(t, panel_title(t, "Recent runs", false), false);
    let inner = block.inner(runs_area);
    f.render_widget(block, runs_area);
    let w = inner.width as usize;
    let mut lines: Vec<Line> = Vec::new();
    match app.selected_ref() {
        None if app.selected_oneoff().is_some() => lines.push(Line::from(Span::styled(
            " A one-off run has no history.",
            t.muted(),
        ))),
        None => lines.push(Line::from(Span::styled(" Views have no runs.", t.muted()))),
        Some(a) => match app.recent.get(&a) {
            Some(r) if !r.is_empty() => {
                for rec in r.iter().take(inner.height as usize) {
                    let m = life_mark(rec.lifecycle);
                    let dur = match rec.ended_at {
                        Some(e) => clock::span(rec.started_at, e),
                        None => "running".into(),
                    };
                    let when = ago(rec.ended_at.unwrap_or(rec.started_at));
                    let left = format!(" {} {when}", m.glyph);
                    let gap = w.saturating_sub(cells(&left) + cells(&dur) + 1);
                    lines.push(Line::from(vec![
                        Span::raw(" "),
                        Span::styled(m.glyph, m.style(t)),
                        Span::raw(format!(" {when}")),
                        Span::raw(" ".repeat(gap)),
                        Span::styled(dur, t.muted()),
                        Span::raw(" "),
                    ]));
                }
            }
            _ if app.last.contains_key(&a) || app.active.contains_key(&a) => {
                lines.push(Line::from(Span::styled(" reading…", t.muted())))
            }
            _ => lines.push(Line::from(Span::styled(" No runs yet.", t.muted()))),
        },
    }
    f.render_widget(Paragraph::new(lines), inner);

    let block = panel(t, panel_title(t, "Session", false), false);
    let inner = block.inner(session_area);
    f.render_widget(block, session_area);
    let w = inner.width as usize;
    let row = |k: &str, v: String, st: Style| {
        Line::from(vec![
            Span::styled(format!(" {k:<12}"), t.muted()),
            Span::styled(ellipsize(&v, w.saturating_sub(14)), st),
        ])
    };
    let mut lines = Vec::new();
    match &app.session {
        None => lines.push(row("mode", "○ no session".into(), t.muted())),
        Some(s) => {
            let (mode, st) = match (s.state, s.mode) {
                (SessionState::Stopping, _) => ("◐ stopping", t.word(Tone::Amber)),
                (_, SessionMode::Foreground) => ("● foreground", t.word(Tone::Leaf)),
                (_, SessionMode::Background) => ("◐ background", t.word(Tone::Amber)),
            };
            lines.push(row("mode", mode.into(), st));
            lines.push(row(
                "windows",
                format!("{} open", s.controller_count),
                Style::default(),
            ));
            let exp = match s.expires_at {
                Some(e) => app.clock.until(e),
                None if s.background_lease => "at `mira down`".into(),
                None => "last window closes".into(),
            };
            lines.push(row("ends", exp, Style::default()));
        }
    }
    let mut active = format!("{} run(s)", app.active.len());
    if app.adhoc_runs > 0 {
        active.push_str(&format!(" + {} one-off", app.adhoc_runs));
    }
    lines.push(row("active", active, Style::default()));
    lines.push(row(
        "this window",
        if app.is_controller() {
            "in control".into()
        } else {
            "watching".into()
        },
        Style::default(),
    ));
    lines.push(row(
        "mouse",
        if app.mouse { "on" } else { "off" }.into(),
        Style::default(),
    ));
    f.render_widget(Paragraph::new(lines), inner);
}

// ----- notice and footer --------------------------------------------------------------

/// The notice line, or `None` when there is nothing to say (the row goes to the body).
fn status_line(app: &App, t: &Theme) -> Option<Line<'static>> {
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

fn key_chip(t: &Theme, keys: &str) -> Span<'static> {
    Span::styled(format!(" {keys} "), t.chip(Tone::Accent))
}

fn draw_footer(f: &mut Frame, app: &App, t: &Theme, area: Rect) {
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
            pinned: matches!(b.cmd, Cmd::Focus | Cmd::Quit),
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

// ----- overlays -----------------------------------------------------------------------

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

fn draw_help(f: &mut Frame, app: &mut App, t: &Theme, area: Rect) {
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

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(2)).max(1);
    let h = h.min(area.height.saturating_sub(2)).max(1);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn draw_form(f: &mut Frame, app: &App, t: &Theme, area: Rect) {
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

fn draw_output(f: &mut Frame, app: &App, t: &Theme, area: Rect) {
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

fn draw_row_actions(f: &mut Frame, app: &App, t: &Theme, area: Rect) {
    let Modal::RowAction {
        choices,
        index,
        view_ref,
    } = &app.modal
    else {
        return;
    };
    let rect = centered(area, 50, choices.len() as u16 + 4);
    let w = rect.width.saturating_sub(4) as usize;
    let mut lines = vec![Line::from(Span::styled(
        "Run for the selected row:",
        t.bold(),
    ))];
    for (i, c) in choices.iter().enumerate() {
        let text = pad(&format!("  {}.{c}", view_ref.plugin), w);
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

#[cfg(test)]
mod tests {
    use super::{ChipSize, cleanup_word, ellipsize, fit_chips, sidebar_width, wrap};
    use mira_protocol::error::{ErrorCode, ErrorInfo};
    use mira_protocol::run::CleanupState;
    use mira_protocol::time::Timestamp;

    #[test]
    fn view_positions_count_from_one() {
        use mira_protocol::manifest::ViewKind;
        assert_eq!(super::position_word(ViewKind::Table, 0, 4), "row 1 of 4");
        assert_eq!(super::position_word(ViewKind::Log, 9, 4), "item 4 of 4");
        assert_eq!(super::position_word(ViewKind::Table, 0, 0), "no rows");
    }

    #[test]
    fn a_failed_cleanup_reads_as_a_short_phrase() {
        let failed = |exit_code, timed_out| CleanupState::Failed {
            ended_at: Timestamp::now(),
            exit_code,
            timed_out,
            error: ErrorInfo::new(ErrorCode::EXECUTION_FAILED, "cleanup failed"),
        };
        assert_eq!(
            cleanup_word(&failed(Some(4), false)).as_deref(),
            Some("cleanup failed (exit 4)")
        );
        assert_eq!(
            cleanup_word(&failed(None, true)).as_deref(),
            Some("cleanup timed out")
        );
        assert_eq!(
            cleanup_word(&failed(None, false)).as_deref(),
            Some("cleanup failed")
        );
        assert_eq!(cleanup_word(&CleanupState::NotNeeded), None);
    }

    #[test]
    fn ellipsize_marks_the_cut() {
        assert_eq!(
            ellipsize("Tutorial app (dev server)", 15),
            "Tutorial app (…"
        );
        assert_eq!(ellipsize("Lint", 15), "Lint");
        assert_eq!(ellipsize("Lint", 0), "");
    }

    #[test]
    fn wrap_keeps_words_within_the_width() {
        assert_eq!(
            wrap("the host stops owned work only when", 12),
            ["the host", "stops owned", "work only", "when"]
        );
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("", 4), [""]);
    }

    #[test]
    fn sidebar_width_is_clamped() {
        assert_eq!(sidebar_width(60), 28);
        assert_eq!(sidebar_width(100), 30);
        assert_eq!(sidebar_width(120), 36);
        assert_eq!(sidebar_width(200), 48);
        assert_eq!(sidebar_width(u16::MAX), 48);
    }

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
