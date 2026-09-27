//! Pipe log panel state: records of one run, a viewport that is either
//! following the tail or pinned to a record, a record cursor for copy, and cell-accurate
//! slicing. Only visible rows are prepared.

use std::collections::VecDeque;

use mira_protocol::ids::{LogSeq, RunId};
use mira_protocol::ipc::LogPage;
use mira_protocol::run::LogRecord;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Records kept in memory per panel; older ones are dropped from the view (not the host).
const MAX_RECORDS: usize = 50_000;
const TRIM_BATCH: usize = 5_000;
/// While pinned, the panel may grow to this many records before the pinned line is trimmed.
const HARD_MAX_RECORDS: usize = 150_000;
const TRIM_MARGIN: usize = 64;
const TAB: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Viewport {
    FollowTail,
    /// Top row is `line_offset` wrapped rows into record `top`.
    Pinned {
        top: LogSeq,
        line_offset: usize,
    },
}

pub struct LogPane {
    pub run_id: Option<RunId>,
    pub records: VecDeque<LogRecord>,
    pub loading: bool,
    pub error: Option<String>,
    pub older_cursor: Option<String>,
    pub loading_older: bool,
    pub first_available: Option<LogSeq>,
    /// This panel dropped records from its front to stay bounded.
    pub trimmed: bool,
    pub view: Viewport,
    /// Selected record while pinned; the last record while following.
    pub cursor: Option<LogSeq>,
    /// Start of a `v` range selection.
    pub anchor: Option<LogSeq>,
    pub hscroll: usize,
    pub wrap: bool,
    /// The pinned record disappeared (trimmed or cleared); shown once in the panel.
    pub lost_anchor: bool,
    /// Log sequence range missing from this view after a stream reset.
    pub gap: Option<(u64, u64)>,
    /// Rows and text cells of the last drawn frame.
    pub height: usize,
    pub width: usize,
}

impl Default for LogPane {
    fn default() -> Self {
        Self::new()
    }
}

impl LogPane {
    pub fn new() -> Self {
        Self {
            run_id: None,
            records: VecDeque::new(),
            loading: false,
            error: None,
            older_cursor: None,
            loading_older: false,
            first_available: None,
            trimmed: false,
            view: Viewport::FollowTail,
            cursor: None,
            anchor: None,
            hscroll: 0,
            wrap: false,
            lost_anchor: false,
            gap: None,
            height: 10,
            width: 40,
        }
    }

    /// Starts showing another run; the old records and position no longer apply.
    pub fn reset(&mut self, run_id: Option<RunId>) {
        let (wrap, hscroll) = (self.wrap, self.hscroll);
        let (height, width) = (self.height, self.width);
        *self = Self::new();
        self.run_id = run_id;
        self.wrap = wrap;
        self.hscroll = hscroll;
        self.height = height;
        self.width = width;
    }

    pub fn last_seq(&self) -> Option<LogSeq> {
        self.records.back().map(|r| r.log_seq)
    }

    fn index_of(&self, seq: LogSeq) -> usize {
        self.records.partition_point(|r| r.log_seq < seq)
    }

    pub fn append(&mut self, run_id: &RunId, records: &[LogRecord]) {
        if self.run_id.as_ref() != Some(run_id) {
            return;
        }
        for r in records {
            if self.last_seq().is_none_or(|l| r.log_seq > l) {
                self.records.push_back(r.clone());
            }
        }
        self.trim();
    }

    fn trim(&mut self) {
        if self.records.len() <= MAX_RECORDS {
            return;
        }
        let mut n = (self.records.len() - MAX_RECORDS + TRIM_BATCH).min(self.records.len());
        // A pinned view keeps its records until the hard cap forces a trim.
        if let Viewport::Pinned { top, .. } = self.view
            && self.records.len() <= HARD_MAX_RECORDS
        {
            n = n.min(self.index_of(top).saturating_sub(TRIM_MARGIN));
        }
        if n == 0 {
            return;
        }
        self.records.drain(..n);
        self.trimmed = true;
        self.older_cursor = None;
        let first = self.records.front().map(|r| r.log_seq);
        if let (Viewport::Pinned { top, .. }, Some(first)) = (self.view, first)
            && top < first
        {
            self.view = Viewport::Pinned {
                top: first,
                line_offset: 0,
            };
            self.lost_anchor = true;
        }
        if let (Some(c), Some(first)) = (self.cursor, first)
            && c < first
        {
            self.cursor = Some(first);
        }
        if let (Some(a), Some(first)) = (self.anchor, first)
            && a < first
        {
            self.anchor = Some(first);
        }
    }

    /// Merges a tail page with live records that arrived while it was loading.
    pub fn apply_tail(&mut self, page: LogPage, older_cursor: Option<String>) {
        if self.run_id.as_ref().is_some_and(|r| r != &page.run_id) {
            return;
        }
        if self.run_id.is_none() {
            self.run_id = Some(page.run_id.clone());
        }
        self.loading = false;
        self.error = None;
        self.first_available = page.first_available_seq;
        let (Some(page_first), Some(page_last)) = (
            page.items.first().map(|r| r.log_seq),
            page.items.last().map(|r| r.log_seq),
        ) else {
            if self.records.is_empty() {
                self.older_cursor = older_cursor;
            }
            return;
        };
        // Older records stay only when they join the page without a hole (after a reset).
        let contiguous = self
            .records
            .iter()
            .any(|r| r.log_seq.get() + 1 == page_first.get());
        let old = std::mem::take(&mut self.records);
        let (older, live): (Vec<LogRecord>, Vec<LogRecord>) = old
            .into_iter()
            .filter(|r| r.log_seq < page_first || r.log_seq > page_last)
            .partition(|r| r.log_seq < page_first);
        if contiguous {
            self.records.extend(older);
        } else if self.is_pinned() && !older.is_empty() {
            // Keep what the user is reading; mark the lines the reset skipped.
            if let Some(last) = older.last() {
                self.gap = Some((last.log_seq.get() + 1, page_first.get() - 1));
            }
            self.records.extend(older);
        } else {
            self.older_cursor = older_cursor;
        }
        self.records.extend(page.items);
        self.records.extend(live);
        let first = self.records.front().map(|r| r.log_seq);
        if let (Viewport::Pinned { top, .. }, Some(first)) = (self.view, first)
            && top < first
        {
            self.view = Viewport::Pinned {
                top: first,
                line_offset: 0,
            };
            self.lost_anchor = true;
            if self.cursor.is_some_and(|c| c < first) {
                self.cursor = Some(first);
            }
        }
        self.trim();
    }

    pub fn apply_older(&mut self, page: LogPage, older_cursor: Option<String>) {
        self.loading_older = false;
        if self.run_id.as_ref() != Some(&page.run_id) {
            return;
        }
        let first = self.records.front().map(|r| r.log_seq);
        let older: Vec<LogRecord> = page
            .items
            .into_iter()
            .filter(|r| first.is_none_or(|f| r.log_seq < f))
            .collect();
        for r in older.into_iter().rev() {
            self.records.push_front(r);
        }
        self.older_cursor = older_cursor;
        self.first_available = page.first_available_seq;
    }

    /// History older than the host keeps is gone (retention or rotation).
    pub fn history_gone(&self) -> bool {
        self.older_cursor.is_none()
            && self.first_available.is_some_and(|f| f.get() > 1)
            && self
                .records
                .front()
                .is_some_and(|r| Some(r.log_seq) == self.first_available)
    }

    fn rows_of(&self, r: &LogRecord) -> usize {
        if self.wrap {
            let w = display(&r.text).width();
            w.div_ceil(self.width.max(1)).max(1)
        } else {
            1
        }
    }

    /// Visible rows as (record index, wrapped row index within the record).
    pub fn visible(&self) -> Vec<(usize, usize)> {
        let h = self.height.max(1);
        let mut out = Vec::with_capacity(h);
        if self.records.is_empty() {
            return out;
        }
        match self.view {
            Viewport::FollowTail => {
                let mut i = self.records.len();
                while i > 0 && out.len() < h {
                    i -= 1;
                    let n = self.rows_of(&self.records[i]);
                    for row in (0..n).rev() {
                        if out.len() < h {
                            out.push((i, row));
                        }
                    }
                }
                out.reverse();
            }
            Viewport::Pinned { top, line_offset } => {
                let mut i = self.index_of(top).min(self.records.len() - 1);
                let mut skip = line_offset;
                while i < self.records.len() && out.len() < h {
                    let n = self.rows_of(&self.records[i]);
                    for row in skip.min(n.saturating_sub(1))..n {
                        if out.len() < h {
                            out.push((i, row));
                        }
                    }
                    skip = 0;
                    i += 1;
                }
            }
        }
        out
    }

    /// Records below the bottom of a pinned view.
    pub fn below(&self) -> usize {
        match self.view {
            Viewport::FollowTail => 0,
            Viewport::Pinned { .. } => self
                .visible()
                .last()
                .map_or(0, |(i, _)| self.records.len().saturating_sub(i + 1)),
        }
    }

    fn cursor_index(&self) -> Option<usize> {
        match self.cursor {
            Some(c) => Some(self.index_of(c).min(self.records.len().checked_sub(1)?)),
            None => self.records.len().checked_sub(1),
        }
    }

    /// Record index of the cursor (the last record while following).
    pub fn cursor_at(&self) -> Option<usize> {
        self.cursor_index()
    }

    /// Moves the cursor by `delta` records and pins the view so the cursor stays visible.
    /// Returns true when the cursor reached the first loaded record.
    pub fn move_cursor(&mut self, delta: isize) -> bool {
        let Some(cur) = self.cursor_index() else {
            return false;
        };
        if self.view == Viewport::FollowTail {
            let top = self.visible().first().map(|(i, _)| *i).unwrap_or(cur);
            self.view = Viewport::Pinned {
                top: self.records[top].log_seq,
                line_offset: 0,
            };
        }
        let len = self.records.len() as isize;
        let next = (cur as isize + delta).clamp(0, len - 1) as usize;
        self.set_cursor(next);
        next == 0
    }

    /// Scrolls the view and the cursor by `delta` records (PgUp/PgDn). Returns true at the
    /// first loaded record.
    pub fn page(&mut self, delta: isize) -> bool {
        let Some(cur) = self.cursor_index() else {
            return false;
        };
        let top = self.visible().first().map_or(cur, |(i, _)| *i);
        let last = self.records.len() as isize - 1;
        let top = (top as isize + delta).clamp(0, last) as usize;
        self.view = Viewport::Pinned {
            top: self.records[top].log_seq,
            line_offset: 0,
        };
        let next = (cur as isize + delta).clamp(0, last) as usize;
        self.set_cursor(next);
        next == 0 || top == 0
    }

    pub fn set_cursor(&mut self, idx: usize) {
        let Some(rec) = self.records.get(idx) else {
            return;
        };
        self.cursor = Some(rec.log_seq);
        let top = match self.view {
            Viewport::Pinned { top, .. } => self.index_of(top),
            Viewport::FollowTail => self.visible().first().map_or(idx, |(i, _)| *i),
        };
        if idx < top {
            self.view = Viewport::Pinned {
                top: rec.log_seq,
                line_offset: 0,
            };
            return;
        }
        // Advance the top until the cursor's last row fits.
        let h = self.height.max(1);
        let mut top = top;
        loop {
            let rows: usize = (top..=idx).map(|i| self.rows_of(&self.records[i])).sum();
            if rows <= h || top == idx {
                break;
            }
            top += 1;
        }
        self.view = Viewport::Pinned {
            top: self.records[top].log_seq,
            line_offset: 0,
        };
    }

    pub fn top(&mut self) {
        if !self.records.is_empty() {
            self.view = Viewport::Pinned {
                top: self.records[0].log_seq,
                line_offset: 0,
            };
            self.cursor = Some(self.records[0].log_seq);
        }
    }

    pub fn follow(&mut self) {
        self.view = Viewport::FollowTail;
        self.cursor = None;
        self.anchor = None;
        self.lost_anchor = false;
    }

    pub fn is_pinned(&self) -> bool {
        matches!(self.view, Viewport::Pinned { .. })
    }

    /// Plain text of the selection (anchor..cursor, or the cursor record).
    pub fn selection_text(&self) -> Option<(String, usize)> {
        let cur = self.cursor_index()?;
        let (a, b) = match self.anchor {
            Some(a) => {
                let ai = self.index_of(a).min(self.records.len() - 1);
                (ai.min(cur), ai.max(cur))
            }
            None => (cur, cur),
        };
        let mut out = String::new();
        for (n, r) in self.records.range(a..=b).enumerate() {
            if n > 0 && !r.continued {
                out.push('\n');
            }
            out.push_str(&plain(&r.text));
        }
        Some((out, b - a + 1))
    }

    pub fn in_selection(&self, idx: usize) -> bool {
        let Some(cur) = self.cursor_index() else {
            return false;
        };
        match self.anchor {
            Some(a) => {
                let ai = self.index_of(a).min(self.records.len().saturating_sub(1));
                (ai.min(cur)..=ai.max(cur)).contains(&idx)
            }
            None => self.is_pinned() && idx == cur,
        }
    }

    /// Literal, case-insensitive search from the cursor; `older` searches upward.
    pub fn find(&mut self, query: &str, older: bool) -> bool {
        let q = query.to_lowercase();
        if q.is_empty() || self.records.is_empty() {
            return false;
        }
        let start = self.cursor_index().unwrap_or(0);
        let hit = if older {
            (0..start)
                .rev()
                .find(|&i| self.records[i].text.to_lowercase().contains(&q))
        } else {
            (start + 1..self.records.len())
                .find(|&i| self.records[i].text.to_lowercase().contains(&q))
        };
        match hit {
            Some(i) => {
                if self.view == Viewport::FollowTail {
                    let _ = self.move_cursor(0);
                }
                self.set_cursor(i);
                true
            }
            None => false,
        }
    }
}

/// Log text without terminal control sequences; tabs and newlines kept for copying.
pub fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.peek() {
                Some('[') => {
                    chars.next();
                    for d in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&d) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(d) = chars.next() {
                        if d == '\u{7}' {
                            break;
                        }
                        if d == '\u{1b}' {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {
                    chars.next();
                }
            },
            '\t' | '\n' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Single-row display form: controls removed, tabs expanded, CR/LF shown as spaces.
pub fn display(text: &str) -> String {
    let p = plain(text);
    let mut out = String::with_capacity(p.len());
    let mut col = 0usize;
    for g in p.graphemes(true) {
        match g {
            "\t" => {
                let n = TAB - col % TAB;
                out.extend(std::iter::repeat_n(' ', n));
                col += n;
            }
            "\n" | "\r\n" => {
                out.push(' ');
                col += 1;
            }
            g => {
                out.push_str(g);
                col += g.width();
            }
        }
    }
    out
}

/// `take` cells of `s` after skipping `skip` cells, on grapheme boundaries. A wide
/// grapheme cut by either edge becomes spaces so columns stay aligned.
pub fn slice_cells(s: &str, skip: usize, take: usize) -> String {
    let mut out = String::new();
    let mut col = 0usize;
    let end = skip + take;
    for g in s.graphemes(true) {
        let w = g.width();
        let next = col + w;
        if next <= skip {
            col = next;
            continue;
        }
        if col >= end {
            break;
        }
        if col < skip || next > end {
            let visible = next.min(end) - col.max(skip);
            out.extend(std::iter::repeat_n(' ', visible));
        } else {
            out.push_str(g);
        }
        col = next;
    }
    out
}

pub fn cells(s: &str) -> usize {
    s.width()
}
