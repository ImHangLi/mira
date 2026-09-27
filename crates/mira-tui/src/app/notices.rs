//! Transient notices and errors in the footer, and the result of a row action run.

use std::time::Instant;

use mira_protocol::error::ErrorInfo;
use mira_protocol::ids::{ActionRef, RunId, ViewRef};
use mira_protocol::run::{Lifecycle, RunRecord};

use super::{App, Entry, Focus, Notice, stage};

impl App {
    pub fn info(&mut self, text: impl Into<String>) {
        self.notice = Some(Notice {
            text: text.into(),
            error: false,
            at: Instant::now(),
            about: None,
            seen: None,
            ok: false,
            open: None,
        });
    }

    /// An info notice about one run: it goes as soon as the run moves on.
    pub(super) fn info_run(&mut self, a: &ActionRef, run_id: &RunId, text: impl Into<String>) {
        self.info(text);
        if let Some(n) = &mut self.notice {
            n.about = Some((a.clone(), run_id.clone()));
        }
        self.settle_notice();
    }

    /// Shows the result of a finished row action run, with `o` for a view it wrote.
    pub(super) fn row_run_ended(&mut self, rec: &RunRecord) {
        if rec.lifecycle.is_active()
            || self
                .row_run
                .as_ref()
                .is_none_or(|r| r.run_id.as_ref() != Some(&rec.run_id))
        {
            return;
        }
        let Some(rr) = self.row_run.take() else {
            return;
        };
        let succeeded = matches!(
            rec.lifecycle,
            Lifecycle::Finished {
                outcome: mira_protocol::run::Outcome::Succeeded
            }
        );
        let ok = rec.result.as_ref().map_or(succeeded, |r| r.ok && succeeded);
        let summary = rec
            .result
            .as_ref()
            .map(|r| r.summary.trim().trim_end_matches('.').to_owned())
            .filter(|s| !s.is_empty());
        if !ok {
            let why = summary
                .or_else(|| {
                    rec.result
                        .as_ref()
                        .and_then(|r| r.error.as_ref())
                        .map(|e| e.message.clone())
                })
                .unwrap_or_else(|| "see its logs".into());
            self.error(format!("{} failed: {why}", rr.action));
            return;
        }
        let text = summary.unwrap_or_else(|| format!("{} finished", rr.action));
        let open = rr
            .wrote
            .iter()
            .find(|v| self.views.iter().any(|x| &x.view_ref == *v))
            .cloned();
        let text = match open.as_ref().and_then(|v| self.view_title(v)) {
            Some(title) => format!("{text} · o opens {title}"),
            None => text,
        };
        self.info(text);
        if let Some(n) = &mut self.notice {
            n.ok = true;
            n.open = open;
        }
    }

    pub(super) fn view_title(&self, r: &ViewRef) -> Option<String> {
        self.views
            .iter()
            .find(|v| &v.view_ref == r)
            .map(|v| v.title.clone())
    }

    /// Selects the view `r` and gives it the focus, clearing a filter that hides it.
    pub(super) fn open_view(&mut self, r: &ViewRef) {
        let Some(vi) = self.views.iter().position(|v| &v.view_ref == r) else {
            return;
        };
        if !self.visible.contains(&Entry::View(vi)) {
            self.filter.clear();
            self.refilter(None);
        }
        if let Some(pos) = self.visible.iter().position(|e| *e == Entry::View(vi)) {
            self.notice = None;
            self.selected = pos;
            self.on_select();
            self.focus = Focus::Logs;
        }
    }

    pub fn error(&mut self, text: impl Into<String>) {
        self.notice = Some(Notice {
            text: text.into(),
            error: true,
            at: Instant::now(),
            about: None,
            seen: None,
            ok: false,
            open: None,
        });
    }

    /// Clears a run notice whose run changed its lifecycle stage or ended.
    pub(super) fn settle_notice(&mut self) {
        let Some(n) = &mut self.notice else { return };
        let Some((a, id)) = &n.about else { return };
        let now = self
            .active
            .get(a)
            .filter(|r| &r.run_id == id)
            .map(|r| stage(r.lifecycle));
        let ended = now.is_none()
            && (n.seen.is_some() || self.last.get(a).is_some_and(|l| &l.run_id == id));
        let moved = now.is_some() && n.seen.is_some() && n.seen != now;
        if ended || moved {
            self.notice = None;
        } else if now.is_some() {
            n.seen = now;
        }
    }

    pub(super) fn error_info(&mut self, what: &str, e: &ErrorInfo) {
        let mut s = format!("{what}: [{}] {}", e.code, e.message);
        if let Some(n) = &e.next_action {
            s.push_str(&format!("  (try: {})", n.argv.join(" ")));
        }
        self.error(s);
    }
}
