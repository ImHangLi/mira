//! Typed plugin views: one panel state per view built from the host's
//! `ViewSnapshot` data. Only visible rows are rendered; the selection follows stable IDs
//! (row, item, or node ID) across updates; copies use the full values, never the cut cells.

use std::collections::HashSet;

use mira_protocol::ids::{ActionId, RunId, ViewRevision};
use mira_protocol::manifest::ViewKind;
use mira_protocol::time::Timestamp;
use mira_protocol::view::{
    Durability, Freshness, LogLevel, SourceKind, TreeNode, ViewBody, ViewData, ViewSnapshot,
};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::{Value, json};

use crate::logs::{cells, display, slice_cells};

/// Widest a table column gets before its cells are cut (the full value stays copyable).
const MAX_COL: usize = 32;

pub struct Meta {
    pub revision: Option<ViewRevision>,
    pub recorded_at: Option<Timestamp>,
    pub source_run_id: Option<RunId>,
    pub source_kind: Option<SourceKind>,
    pub freshness: Freshness,
    pub freshness_reason: Option<String>,
    pub durability: Durability,
}

impl Meta {
    /// Whether the host may now report other freshness or durability for this revision:
    /// the save is still in progress, or the data is "current" but its run is not active.
    pub fn may_change(&self, running: &[&RunId]) -> bool {
        self.revision.is_some()
            && (self.durability == Durability::Buffered
                || (self.freshness == Freshness::Current
                    && self
                        .source_run_id
                        .as_ref()
                        .is_none_or(|r| !running.contains(&r))))
    }
}

pub enum Body {
    Empty,
    Reference(String),
    Data(ViewData),
}

struct Flat {
    id: String,
    depth: usize,
    text: String,
    children: bool,
}

pub struct ViewPane {
    pub kind: ViewKind,
    pub meta: Option<Meta>,
    pub body: Body,
    pub loading: bool,
    /// Another revision arrived while loading; read again when this read ends.
    pub reload: bool,
    pub error: Option<String>,
    /// The host returned less than the whole view.
    pub truncated: bool,
    pub row_actions: Vec<ActionId>,
    /// Set (from describe) for a log view the host derives from another action's log.
    pub source: Option<mira_protocol::manifest::ViewSourceWire>,
    pub cursor: usize,
    /// Stable ID of the selected row, item, or node.
    sel_id: Option<String>,
    /// The revision the user chose the selection in; row actions are bound to it.
    pub sel_rev: Option<ViewRevision>,
    /// The user moved the selection (log views otherwise follow the newest item).
    moved: bool,
    pub top: usize,
    /// Table: selected column. Other kinds: unused.
    pub col: usize,
    /// Table: first shown column. Other kinds: horizontal cell offset.
    pub hscroll: usize,
    pub wrap: bool,
    expanded: HashSet<String>,
    flat: Vec<Flat>,
    widths: Vec<usize>,
    pub height: usize,
    pub width: usize,
    pub note: Option<String>,
}

pub fn freshness_word(f: Freshness) -> &'static str {
    match f {
        Freshness::Current => "current",
        Freshness::Historical => "historical",
        Freshness::Stale => "STALE",
    }
}

pub fn kind_word(k: ViewKind) -> &'static str {
    match k {
        ViewKind::Text => "text",
        ViewKind::Table => "table",
        ViewKind::Log => "log",
        ViewKind::Tree => "tree",
        ViewKind::Json => "json",
    }
}

fn cell(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn level_word(l: LogLevel) -> &'static str {
    match l {
        LogLevel::Debug => "debug",
        LogLevel::Info => "info ",
        LogLevel::Warn => "WARN ",
        LogLevel::Error => "ERROR",
    }
}

fn tree_value(n: &TreeNode) -> Value {
    let children: Vec<Value> = n.children.iter().map(tree_value).collect();
    if children.is_empty() {
        json!({"id": n.id, "label": n.label})
    } else {
        json!({"id": n.id, "label": n.label, "children": children})
    }
}

fn find_node<'a>(nodes: &'a [TreeNode], id: &str) -> Option<&'a TreeNode> {
    for n in nodes {
        if n.id == id {
            return Some(n);
        }
        if let Some(x) = find_node(&n.children, id) {
            return Some(x);
        }
    }
    None
}

impl ViewPane {
    pub fn new(kind: ViewKind) -> Self {
        Self {
            kind,
            meta: None,
            body: Body::Empty,
            loading: false,
            reload: false,
            error: None,
            truncated: false,
            row_actions: Vec::new(),
            source: None,
            cursor: 0,
            sel_id: None,
            sel_rev: None,
            moved: false,
            top: 0,
            col: 0,
            hscroll: 0,
            wrap: false,
            expanded: HashSet::new(),
            flat: Vec::new(),
            widths: Vec::new(),
            height: 10,
            width: 40,
            note: None,
        }
    }

    pub fn revision(&self) -> Option<ViewRevision> {
        self.meta.as_ref().and_then(|m| m.revision)
    }

    pub fn data(&self) -> Option<&ViewData> {
        match &self.body {
            Body::Data(d) => Some(d),
            _ => None,
        }
    }

    /// Replaces the content with a fresh snapshot and keeps the selection by stable ID.
    pub fn apply(&mut self, snap: ViewSnapshot, truncated: bool) {
        let first_load = self.meta.is_none();
        let old_rev = self.revision();
        self.loading = false;
        self.error = None;
        self.truncated = truncated;
        self.kind = snap.kind;
        self.meta = Some(Meta {
            revision: snap.view_revision,
            recorded_at: snap.recorded_at,
            source_run_id: snap.source_run_id,
            source_kind: snap.source_kind,
            freshness: snap.freshness,
            freshness_reason: snap.freshness_reason,
            durability: snap.durability,
        });
        self.body = match snap.data {
            None => Body::Empty,
            Some(ViewBody::Reference(r)) => Body::Reference(r.summary),
            Some(ViewBody::Inline(d)) => Body::Data(d),
        };
        if first_load && let Some(ViewData::Tree { nodes }) = self.data() {
            let roots: Vec<String> = nodes.iter().map(|n| n.id.clone()).collect();
            self.expanded.extend(roots);
        }
        self.rebuild();
        let n = self.len();
        match (&self.sel_id, self.moved) {
            (Some(id), true) => match self.index_of_id(id) {
                Some(i) => self.cursor = i,
                None if n > 0 => {
                    self.cursor = self.cursor.min(n - 1);
                    self.note = Some(format!(
                        "the selected {} is gone in revision {}; the neighbor is selected",
                        self.unit(),
                        self.revision()
                            .map_or_else(|| "-".into(), |r| r.to_string())
                    ));
                }
                None => self.cursor = 0,
            },
            _ => {
                // Log views follow the newest item until the user moves.
                self.cursor = if self.kind == ViewKind::Log {
                    n.saturating_sub(1)
                } else {
                    self.cursor.min(n.saturating_sub(1))
                };
            }
        }
        self.sel_id = self.row_id(self.cursor);
        if self.sel_rev.is_none() || !self.moved {
            self.sel_rev = self.revision();
        }
        if old_rev.is_some() && old_rev != self.revision() && self.note.is_none() && self.moved {
            self.note = Some(format!(
                "updated to revision {}; the selection stayed on the same {}",
                self.revision()
                    .map_or_else(|| "-".into(), |r| r.to_string()),
                self.unit()
            ));
        }
        self.keep_visible();
    }

    fn unit(&self) -> &'static str {
        match self.kind {
            ViewKind::Table => "row",
            ViewKind::Log => "item",
            ViewKind::Tree => "node",
            _ => "line",
        }
    }

    fn rebuild(&mut self) {
        self.flat.clear();
        self.widths.clear();
        let Body::Data(d) = &self.body else {
            return;
        };
        match d {
            ViewData::Text { text, .. } => {
                self.flat = text
                    .split('\n')
                    .enumerate()
                    .map(|(i, l)| Flat {
                        id: format!("#{i}"),
                        depth: 0,
                        text: l.to_owned(),
                        children: false,
                    })
                    .collect();
            }
            ViewData::Json { value } => {
                let pretty = serde_json::to_string_pretty(value).unwrap_or_default();
                self.flat = pretty
                    .lines()
                    .enumerate()
                    .map(|(i, l)| Flat {
                        id: format!("#{i}"),
                        depth: 0,
                        text: l.to_owned(),
                        children: false,
                    })
                    .collect();
            }
            ViewData::Tree { nodes } => {
                fn walk(
                    nodes: &[TreeNode],
                    depth: usize,
                    open: &HashSet<String>,
                    out: &mut Vec<Flat>,
                ) {
                    for n in nodes {
                        out.push(Flat {
                            id: n.id.clone(),
                            depth,
                            text: n.label.clone(),
                            children: !n.children.is_empty(),
                        });
                        if open.contains(&n.id) {
                            walk(&n.children, depth + 1, open, out);
                        }
                    }
                }
                let mut out = Vec::new();
                walk(nodes, 0, &self.expanded, &mut out);
                self.flat = out;
            }
            ViewData::Table { columns, rows } => {
                self.widths = columns
                    .iter()
                    .map(|c| {
                        let data = rows
                            .iter()
                            .map(|r| {
                                cells(&display(&r.values.get(&c.id).map(cell).unwrap_or_default()))
                            })
                            .max()
                            .unwrap_or(0);
                        cells(&display(&c.label)).max(data).clamp(3, MAX_COL)
                    })
                    .collect();
                self.col = self.col.min(columns.len().saturating_sub(1));
            }
            ViewData::Log { .. } => {}
        }
    }

    pub fn len(&self) -> usize {
        match self.data() {
            Some(ViewData::Table { rows, .. }) => rows.len(),
            Some(ViewData::Log { items }) => items.len(),
            Some(_) => self.flat.len(),
            None => 0,
        }
    }

    fn row_id(&self, i: usize) -> Option<String> {
        match self.data()? {
            ViewData::Table { rows, .. } => rows.get(i).map(|r| r.id.clone()),
            ViewData::Log { items } => items.get(i).map(|x| x.id.clone()),
            _ => self.flat.get(i).map(|f| f.id.clone()),
        }
    }

    fn index_of_id(&self, id: &str) -> Option<usize> {
        match self.data()? {
            ViewData::Table { rows, .. } => rows.iter().position(|r| r.id == id),
            ViewData::Log { items } => items.iter().position(|x| x.id == id),
            _ => self.flat.iter().position(|f| f.id == id),
        }
    }

    pub fn selected_row_id(&self) -> Option<String> {
        self.sel_id.clone()
    }

    /// Plain text of one row (no gutter), used for wrapping and panning.
    fn row_text(&self, i: usize) -> String {
        match self.data() {
            Some(ViewData::Log { items }) => items.get(i).map_or(String::new(), |x| {
                // Info is the normal case: show only the levels that carry meaning.
                if x.level == LogLevel::Info {
                    display(&x.text)
                } else {
                    display(&format!("{} {}", level_word(x.level), x.text))
                }
            }),
            Some(ViewData::Tree { .. }) => self.flat.get(i).map_or(String::new(), |f| {
                let mark = if !f.children {
                    "  "
                } else if self.expanded.contains(&f.id) {
                    "- "
                } else {
                    "+ "
                };
                display(&format!("{}{mark}{}", "  ".repeat(f.depth), f.text))
            }),
            Some(ViewData::Table { .. }) | None => String::new(),
            Some(_) => self.flat.get(i).map_or(String::new(), |f| display(&f.text)),
        }
    }

    fn wraps(&self) -> bool {
        self.wrap && !matches!(self.kind, ViewKind::Table)
    }

    fn rows_of(&self, i: usize) -> usize {
        if self.wraps() {
            cells(&self.row_text(i)).div_ceil(self.width.max(1)).max(1)
        } else {
            1
        }
    }

    fn body_height(&self) -> usize {
        match self.kind {
            // Header row, its rule, and the full-value line of the selected cell.
            ViewKind::Table => self.height.saturating_sub(3).max(1),
            _ => self.height.max(1),
        }
    }

    fn keep_visible(&mut self) {
        let n = self.len();
        if n == 0 {
            self.top = 0;
            return;
        }
        self.cursor = self.cursor.min(n - 1);
        if self.cursor < self.top {
            self.top = self.cursor;
        }
        let h = self.body_height();
        loop {
            let rows: usize = (self.top..=self.cursor).map(|i| self.rows_of(i)).sum();
            if rows <= h || self.top >= self.cursor {
                break;
            }
            self.top += 1;
        }
    }

    pub fn move_by(&mut self, delta: isize) {
        let n = self.len() as isize;
        if n == 0 {
            return;
        }
        self.cursor = (self.cursor as isize + delta).clamp(0, n - 1) as usize;
        self.selected_now();
    }

    pub fn page(&mut self, down: bool) {
        let h = self.body_height().saturating_sub(1).max(1) as isize;
        self.move_by(if down { h } else { -h });
    }

    pub fn home(&mut self, end: bool) {
        self.cursor = if end { self.len().saturating_sub(1) } else { 0 };
        self.selected_now();
        if end && self.kind == ViewKind::Log {
            // G on a log view follows new items again.
            self.moved = false;
        }
    }

    fn selected_now(&mut self) {
        self.moved = true;
        self.note = None;
        self.sel_id = self.row_id(self.cursor);
        self.sel_rev = self.revision();
        self.keep_visible();
    }

    /// The user saw a VIEW_CHANGED answer and now looks at the current revision.
    pub fn accept_current(&mut self) {
        self.sel_rev = self.revision();
    }

    pub fn left_right(&mut self, right: bool) {
        match self.data() {
            Some(ViewData::Table { columns, .. }) => {
                let last = columns.len().saturating_sub(1);
                self.col = if right {
                    (self.col + 1).min(last)
                } else {
                    self.col.saturating_sub(1)
                };
            }
            Some(ViewData::Tree { .. }) => {
                if let Some(f) = self.flat.get(self.cursor)
                    && f.children
                {
                    let id = f.id.clone();
                    if right {
                        self.expanded.insert(id);
                    } else {
                        self.expanded.remove(&id);
                    }
                    self.rebuild();
                    if let Some(i) = self.sel_id.clone().and_then(|s| self.index_of_id(&s)) {
                        self.cursor = i;
                    }
                    self.keep_visible();
                } else if !right {
                    self.hscroll = self.hscroll.saturating_sub(8);
                }
            }
            _ => {
                self.hscroll = if right {
                    self.hscroll + 8
                } else {
                    self.hscroll.saturating_sub(8)
                };
            }
        }
    }

    pub fn toggle_wrap(&mut self) {
        self.wrap = !self.wrap;
        self.keep_visible();
    }

    pub fn can_wrap(&self) -> bool {
        !matches!(self.kind, ViewKind::Table) && self.data().is_some()
    }

    /// `y`: the full value under the cursor as plain text.
    pub fn copy_value(&self) -> Option<(String, String)> {
        let i = self.cursor;
        match self.data()? {
            ViewData::Table { columns, rows } => {
                let r = rows.get(i)?;
                let c = columns.get(self.col)?;
                Some((
                    r.values.get(&c.id).map(cell).unwrap_or_default(),
                    format!("cell {} of row {}", c.label, r.id),
                ))
            }
            ViewData::Log { items } => items.get(i).map(|x| (x.text.clone(), "log item".into())),
            ViewData::Tree { .. } => self
                .flat
                .get(i)
                .map(|f| (f.text.clone(), "node label".into())),
            _ => self.flat.get(i).map(|f| (f.text.clone(), "line".into())),
        }
    }

    /// `Y`: the whole row, item, subtree, or value as valid JSON (text views: the full text).
    pub fn copy_whole(&self) -> Option<(String, String)> {
        let i = self.cursor;
        match self.data()? {
            ViewData::Table { rows, .. } => rows.get(i).map(|r| {
                (
                    serde_json::to_string_pretty(&json!({"id": r.id, "values": r.values}))
                        .unwrap_or_default(),
                    format!("row {} as JSON", r.id),
                )
            }),
            ViewData::Log { items } => items.get(i).map(|x| {
                (
                    serde_json::to_string_pretty(x).unwrap_or_default(),
                    "log item as JSON".into(),
                )
            }),
            ViewData::Tree { nodes } => {
                let id = self.flat.get(i)?.id.clone();
                find_node(nodes, &id).map(|n| {
                    (
                        serde_json::to_string_pretty(&tree_value(n)).unwrap_or_default(),
                        "subtree as JSON".into(),
                    )
                })
            }
            ViewData::Json { value } => Some((
                serde_json::to_string_pretty(value).unwrap_or_default(),
                "whole JSON value".into(),
            )),
            ViewData::Text { text, .. } => Some((text.clone(), "whole text".into())),
        }
    }

    /// The selected table row in words: its first two cells, for example `3773 · node`.
    pub fn row_summary(&self) -> Option<String> {
        let ViewData::Table { columns, rows } = self.data()? else {
            return None;
        };
        let r = rows.get(self.cursor)?;
        let parts: Vec<String> = columns
            .iter()
            .take(2)
            .map(|c| r.values.get(&c.id).map(cell).unwrap_or_default())
            .filter(|v| !v.is_empty())
            .collect();
        (!parts.is_empty()).then(|| display(&parts.join(" · ")))
    }

    /// Full value of the selected table cell for the detail line.
    pub fn cell_detail(&self) -> Option<String> {
        let ViewData::Table { columns, rows } = self.data()? else {
            return None;
        };
        let r = rows.get(self.cursor)?;
        let c = columns.get(self.col)?;
        let v = r.values.get(&c.id).map(cell).unwrap_or_default();
        Some(display(&format!("{}: {v}", c.label)))
    }

    /// Visible rows as (row index, wrapped part).
    fn visible(&self) -> Vec<(usize, usize)> {
        let h = self.body_height();
        let mut out = Vec::with_capacity(h);
        let mut i = self.top;
        while i < self.len() && out.len() < h {
            for part in 0..self.rows_of(i) {
                if out.len() < h {
                    out.push((i, part));
                }
            }
            i += 1;
        }
        out
    }

    /// Lines for the body area (`width` cells). `focus` highlights the cursor with
    /// `selected`; without the focus the cursor row is bold. Debug items and the cell
    /// detail line use `muted`.
    pub fn lines(&mut self, focus: bool, selected: Style, muted: Style) -> Vec<Line<'static>> {
        let sel = if focus {
            selected
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        };
        let w = self.width.max(1);
        if let Some(ViewData::Table { columns, rows }) = self.data() {
            return self.table_lines(columns, rows, w, sel, focus, muted);
        }
        let mut out = Vec::new();
        for (i, part) in self.visible() {
            let text = self.row_text(i);
            let shown = if self.wraps() {
                slice_cells(&text, part * w, w)
            } else {
                slice_cells(&text, self.hscroll, w)
            };
            let mut style = Style::default();
            if let Some(ViewData::Log { items }) = self.data()
                && let Some(x) = items.get(i)
            {
                if matches!(x.level, LogLevel::Debug) {
                    style = muted;
                }
                if matches!(x.level, LogLevel::Error | LogLevel::Warn) {
                    style = style.add_modifier(Modifier::BOLD);
                }
            }
            let mut shown = shown;
            if i == self.cursor {
                style = style.patch(sel);
                if focus {
                    // The focused cursor row fills the width.
                    let pad = w.saturating_sub(cells(&shown));
                    shown.push_str(&" ".repeat(pad));
                }
            }
            out.push(Line::from(Span::styled(shown, style)));
        }
        out
    }

    fn table_lines(
        &self,
        columns: &[mira_protocol::view::Column],
        rows: &[mira_protocol::view::Row],
        w: usize,
        sel: Style,
        focus: bool,
        muted: Style,
    ) -> Vec<Line<'static>> {
        // First shown column: keep the selected column on screen.
        let avail = w.saturating_sub(2);
        let mut first = self.hscroll.min(self.col);
        loop {
            let used: usize = (first..=self.col.min(columns.len().saturating_sub(1)))
                .map(|c| self.widths.get(c).copied().unwrap_or(3) + 1)
                .sum();
            if used <= avail || first >= self.col {
                break;
            }
            first += 1;
        }
        // Shown columns; a column that does not fit whole still shows cut when at least
        // `MIN_CUT` cells are left, and the table's last column takes the room left.
        const MIN_CUT: usize = 8;
        let mut widths = self.widths.clone();
        let mut shown: Vec<usize> = Vec::new();
        let mut used = 0;
        for c in first..columns.len() {
            let cw = widths.get(c).copied().unwrap_or(3) + 1;
            if used + cw > avail && !shown.is_empty() {
                let left = avail.saturating_sub(used + 1);
                if left >= MIN_CUT
                    && let Some(x) = widths.get_mut(c)
                {
                    *x = left;
                    shown.push(c);
                }
                break;
            }
            used += cw;
            shown.push(c);
        }
        if let Some(&last) = shown.last()
            && last + 1 == columns.len()
        {
            let before: usize = shown[..shown.len() - 1]
                .iter()
                .map(|&c| widths.get(c).copied().unwrap_or(3) + 1)
                .sum();
            if let Some(x) = widths.get_mut(last) {
                *x = (*x).max(avail.saturating_sub(before + 1));
            }
        }
        let fit = |s: &str, cw: usize| -> String {
            let d = display(s);
            if cells(&d) > cw {
                format!("{}…", slice_cells(&d, 0, cw.saturating_sub(1)))
            } else {
                let pad = cw - cells(&d);
                format!("{d}{}", " ".repeat(pad))
            }
        };
        let mut out = Vec::new();
        let mut head = vec![Span::raw(if first > 0 { "‹ " } else { "  " })];
        for &c in &shown {
            let cw = widths.get(c).copied().unwrap_or(3);
            // Every header cell looks the same; the selected cell is marked in its row.
            let st = Style::default().add_modifier(Modifier::BOLD);
            head.push(Span::styled(fit(&columns[c].label, cw), st));
            head.push(Span::raw(" "));
        }
        if shown.last().is_some_and(|&l| l + 1 < columns.len()) {
            head.push(Span::raw("›"));
        }
        out.push(Line::from(head));
        out.push(Line::from(Span::styled(
            "─".repeat(w),
            Style::default().add_modifier(Modifier::DIM),
        )));
        for (i, _) in self.visible() {
            let Some(r) = rows.get(i) else { break };
            let cur = i == self.cursor;
            // The focused cursor row is filled; its selected cell is also underlined.
            let row_st = if cur { sel } else { Style::default() };
            let mut spans = vec![Span::styled(if cur { "▸ " } else { "  " }, row_st)];
            let mut used = 2;
            for &c in &shown {
                let cw = widths.get(c).copied().unwrap_or(3);
                let v = r.values.get(&columns[c].id).map(cell).unwrap_or_default();
                let st = if cur && c == self.col && focus {
                    row_st.add_modifier(Modifier::UNDERLINED)
                } else {
                    row_st
                };
                spans.push(Span::styled(fit(&v, cw), st));
                spans.push(Span::styled(" ", row_st));
                used += cw + 1;
            }
            if cur && focus {
                spans.push(Span::styled(" ".repeat(w.saturating_sub(used)), row_st));
            }
            out.push(Line::from(spans));
        }
        while out.len() < self.height.saturating_sub(1) {
            out.push(Line::from(""));
        }
        out.truncate(self.height.saturating_sub(1));
        out.push(Line::from(Span::styled(
            slice_cells(&self.cell_detail().unwrap_or_default(), 0, w),
            muted,
        )));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(freshness: Freshness, durability: Durability, run: Option<&RunId>) -> Meta {
        Meta {
            revision: Some(ViewRevision::new(1).unwrap()),
            recorded_at: None,
            source_run_id: run.cloned(),
            source_kind: None,
            freshness,
            freshness_reason: None,
            durability,
        }
    }

    #[test]
    fn metadata_is_read_again_until_it_settles() {
        let run: RunId = "r_0000000000004000800000000000000a".parse().unwrap();
        let other: RunId = "r_0000000000004000800000000000000b".parse().unwrap();
        // The producing run is still active and the data is saved: nothing can change.
        let m = meta(Freshness::Current, Durability::Committed, Some(&run));
        assert!(!m.may_change(&[&run]));
        // The run ended: the host now reports historical.
        assert!(m.may_change(&[&other]));
        // A save in progress becomes committed.
        let m = meta(Freshness::Historical, Durability::Buffered, Some(&run));
        assert!(m.may_change(&[]));
        let m = meta(Freshness::Historical, Durability::Committed, Some(&run));
        assert!(!m.may_change(&[]));
    }
}
