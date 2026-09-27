//! Projection of host state: catalog, runs, one-off runs, views, log tails, and polling.

use std::time::Instant;

use mira_protocol::error::ErrorInfo;
use mira_protocol::ids::{ActionRef, RunId, ViewRef};
use mira_protocol::ipc::*;
use mira_protocol::run::{Lifecycle, RunRecord, RunSummary};

use crate::ipc::{LogChunk, Read};
use crate::views::ViewPane;

use super::{App, Item, Key, LastRun, OneOff, ViewItem, keep_oneoffs, workspace_name};

/// Errors stay this long; other notices are transient and go sooner.
const NOTICE_SECS: u64 = 8;
const INFO_SECS: u64 = 4;
/// The header's git branch is read again at most this often (or on a focus change).
const BRANCH_SECS: u64 = 3;
/// Open views whose metadata can still change (a save in progress, or a producing run
/// that ended) are read again at most this often.
const VIEW_POLL_MS: u128 = 1000;
/// The screen of a silent PTY run is read again at most this often.
const SCREEN_POLL_MS: u128 = 1000;

impl App {
    pub fn set_attached(&mut self, session: Option<SessionInfo>) {
        self.attached_to = session.as_ref().map(|s| s.id.clone());
        self.session = session;
    }

    pub fn set_catalog(&mut self, list: CatalogList) {
        let keep = self.selected_key();
        let mut order: Vec<String> = Vec::new();
        self.items.clear();
        self.views.clear();
        for i in list.items {
            let p = i.item_ref.plugin.to_string();
            if !order.contains(&p) {
                order.push(p);
            }
            match i.item {
                CatalogItemKind::Action { mode } => self.items.push(Item {
                    action_ref: i.item_ref.as_action(),
                    title: i.title,
                    description: i.description,
                    tags: i.tags,
                    mode,
                    enabled: i.enabled,
                    definition_hash: i.definition_hash,
                }),
                CatalogItemKind::View { view_kind } => self.views.push(ViewItem {
                    view_ref: i.item_ref.as_view(),
                    title: i.title,
                    description: i.description,
                    tags: i.tags,
                    kind: view_kind,
                }),
            }
        }
        self.plugin_order = order;
        // The workspace file may have changed with the catalog.
        self.workspace_name = workspace_name(&self.root);
        // Views that no longer exist lose their panel; the others read again.
        let known: Vec<ViewRef> = self.views.iter().map(|v| v.view_ref.clone()).collect();
        self.view_panes.retain(|r, _| known.contains(r));
        let refs: Vec<ViewRef> = self.view_panes.keys().cloned().collect();
        for r in refs {
            self.load_view(&r);
        }
        self.catalog_error = None;
        self.refilter(keep);
        self.on_select();
    }

    pub(super) fn apply_runs(&mut self, session: Option<SessionInfo>, runs: Vec<RunSummary>) {
        let old = std::mem::take(&mut self.active);
        self.adhoc_runs = 0;
        let mut adhoc: Vec<RunSummary> = Vec::new();
        for r in runs {
            match r.action_ref.clone() {
                Some(a) => {
                    // A new run of this action replaces a historical run in its pane.
                    if old.get(&a).is_none_or(|o| o.run_id != r.run_id) {
                        self.viewing.remove(&a);
                    }
                    self.active.insert(a, r);
                }
                None => {
                    self.adhoc_runs += 1;
                    adhoc.push(r);
                }
            }
        }
        self.apply_oneoffs(adhoc);
        for (a, r) in old {
            if self.active.get(&a).is_some_and(|n| n.run_id == r.run_id) {
                continue;
            }
            // Views this run produced are no longer current: read their metadata again.
            let produced: Vec<ViewRef> = self
                .view_panes
                .iter()
                .filter(|(_, p)| {
                    p.meta
                        .as_ref()
                        .is_some_and(|m| m.source_run_id.as_ref() == Some(&r.run_id))
                })
                .map(|(v, _)| v.clone())
                .collect();
            for v in produced {
                self.load_view(&v);
            }
            // The run ended: fetch its final record for the outcome.
            self.last.insert(
                a.clone(),
                LastRun {
                    run_id: r.run_id.clone(),
                    lifecycle: r.lifecycle,
                    exit: None,
                    started_at: Some(r.started_at),
                    ended_at: None,
                    result: None,
                    cleanup: None,
                },
            );
            let _ = self.io.read.send(Read::RunGet(r.run_id.clone()));
            if self
                .restart_after
                .get(&a)
                .is_some_and(|(id, _)| *id == r.run_id)
                && let Some((_, input)) = self.restart_after.remove(&a)
            {
                self.invoke(a, input);
            }
        }
        if let Some(id) = self.row_run.as_ref().and_then(|r| r.run_id.as_ref())
            && !self.active.values().any(|r| &r.run_id == id)
        {
            let _ = self.io.read.send(Read::RunGet(id.clone()));
        }
        self.session = session;
        if let Some(a) = self.selected_ref() {
            self.sync_pane(&a);
        }
        self.settle_notice();
        self.request_status();
    }

    /// Tracks the active one-off runs: a new one is listed and its record read for the
    /// label; one that left the active set is read again for its outcome.
    fn apply_oneoffs(&mut self, active: Vec<RunSummary>) {
        let keep = self.selected_key();
        let mut changed = false;
        for o in &mut self.oneoffs {
            if o.lifecycle.is_active() && !active.iter().any(|r| r.run_id == o.run_id) {
                // Ended: the final record brings the outcome.
                let _ = self.io.read.send(Read::RunGet(o.run_id.clone()));
            }
        }
        for r in active {
            match self.oneoffs.iter_mut().find(|o| o.run_id == r.run_id) {
                Some(o) => {
                    if o.lifecycle != r.lifecycle {
                        o.stopping &= !matches!(r.lifecycle, Lifecycle::Stopping { .. });
                    }
                    o.lifecycle = r.lifecycle;
                }
                None => {
                    let _ = self.io.read.send(Read::RunGet(r.run_id.clone()));
                    self.oneoffs
                        .push(OneOff::new(r.run_id, r.lifecycle, r.started_at));
                    changed = true;
                }
            }
        }
        if changed {
            keep_oneoffs(&mut self.oneoffs);
            self.relist(keep);
        }
    }

    /// Lists the entries again after the one-off runs changed, keeping `keep` selected.
    fn relist(&mut self, keep: Option<Key>) {
        self.refilter(keep.clone());
        if self.selected_key() != keep {
            self.on_select();
        }
    }

    /// A run record of a one-off run: fills in its label and outcome. `listed` adds a run
    /// that is not listed yet (from the startup history).
    pub(super) fn oneoff_record(&mut self, rec: &RunRecord, listed: bool) {
        if let Some(o) = self.oneoffs.iter_mut().find(|o| o.run_id == rec.run_id) {
            o.apply(rec);
            if !rec.lifecycle.is_active() {
                o.stopping = false;
            }
            return;
        }
        if !listed {
            return;
        }
        let keep = self.selected_key();
        let mut o = OneOff::new(rec.run_id.clone(), rec.lifecycle, rec.started_at);
        o.apply(rec);
        self.oneoffs.push(o);
        keep_oneoffs(&mut self.oneoffs);
        self.relist(keep);
    }

    /// Schedules come only with full status; read it at most once a second.
    pub(super) fn request_status(&mut self) {
        if self
            .status_at
            .is_none_or(|t| t.elapsed().as_millis() >= 1000)
        {
            self.status_at = Some(Instant::now());
            let _ = self.io.read.send(Read::Status);
        }
    }

    /// The action declares an interval schedule (switched on or not yet).
    pub fn has_schedule(&self, a: &ActionRef) -> bool {
        self.scheduled.contains(a) || self.schedule_of(a).is_some()
    }

    pub fn schedule_of(&self, a: &ActionRef) -> Option<&ScheduleData> {
        self.schedules.iter().find(|s| &s.action_ref == a)
    }

    pub(super) fn load_view(&mut self, view_ref: &ViewRef) {
        let kind = self
            .views
            .iter()
            .find(|v| &v.view_ref == view_ref)
            .map(|v| v.kind);
        let Some(kind) = kind else { return };
        let p = self
            .view_panes
            .entry(view_ref.clone())
            .or_insert_with(|| ViewPane::new(kind));
        if p.loading {
            p.reload = true;
            return;
        }
        p.loading = true;
        let _ = self.io.read.send(Read::View(view_ref.clone()));
    }

    /// Reads the git branch again when it is older than [`BRANCH_SECS`] or `force` is set.
    pub(super) fn refresh_branch(&mut self, force: bool) {
        if force || self.branch_at.elapsed().as_secs() >= BRANCH_SECS {
            self.branch_at = Instant::now();
            self.branch = crate::git::head_label(std::path::Path::new(&self.root));
        }
    }
}

impl App {
    pub(super) fn frame(&mut self, frame: StreamFrame) {
        match frame.event {
            StreamEvent::Ready { .. } => {}
            StreamEvent::Snapshot(s) => {
                if self.stream_issue.take().is_some() {
                    self.reload_selected_tail();
                }
                self.storage_warnings = s.storage_warnings;
                self.config_warnings = s.config_warnings;
                self.schedules = s.schedules;
                self.apply_runs(s.session, s.runs);
                // Views may have changed while the stream was down.
                let refs: Vec<ViewRef> = self.view_panes.keys().cloned().collect();
                for r in refs {
                    self.load_view(&r);
                }
            }
            StreamEvent::State {
                session,
                runs,
                storage_warnings,
                ..
            } => {
                self.storage_warnings = storage_warnings;
                self.apply_runs(session, runs);
            }
            StreamEvent::Log { run_id, records } => {
                for p in self.panes.values_mut() {
                    p.append(&run_id, &records);
                }
                if let Some(p) = self
                    .oneoffs
                    .iter_mut()
                    .find(|o| o.run_id == run_id)
                    .and_then(|o| o.pane.as_mut())
                {
                    p.append(&run_id, &records);
                }
            }
            StreamEvent::View {
                view_ref,
                view_revision,
            } => {
                if let Some(rr) = &mut self.row_run
                    && rr.view_ref.plugin == view_ref.plugin
                    && rr.view_ref != view_ref
                    && !rr.wrote.contains(&view_ref)
                {
                    rr.wrote.push(view_ref.clone());
                }
                // Only opened views keep a panel; others read when they are selected.
                if self
                    .view_panes
                    .get(&view_ref)
                    .is_some_and(|p| p.revision() != Some(view_revision))
                {
                    self.load_view(&view_ref);
                }
            }
            StreamEvent::Gap {
                dropped_records, ..
            } => {
                self.error(format!(
                    "event stream gap ({} records); reloading the log tail",
                    dropped_records.unwrap_or(0)
                ));
                self.reload_selected_tail();
            }
            _ => {}
        }
    }

    pub(super) fn record(&mut self, rec: RunRecord) {
        let Some(a) = rec.action_ref.clone() else {
            self.oneoff_record(&rec, false);
            return;
        };
        if self.active.get(&a).is_some_and(|r| r.run_id == rec.run_id) {
            return;
        }
        // Keep the newest finished run per action.
        if let Some(l) = self.last.get(&a)
            && l.run_id != rec.run_id
            && l.ended_at
                .is_some_and(|t| rec.ended_at.is_none_or(|e| e < t))
        {
            return;
        }
        self.last.insert(
            a,
            LastRun {
                run_id: rec.run_id,
                lifecycle: rec.lifecycle,
                exit: rec.exit,
                started_at: Some(rec.started_at),
                ended_at: rec.ended_at,
                result: rec.result,
                cleanup: Some(rec.cleanup),
            },
        );
    }

    pub(super) fn tail(&mut self, a: &ActionRef, res: Result<LogChunk, ErrorInfo>) {
        let Some(p) = self.panes.get_mut(a) else {
            return;
        };
        match res {
            Ok(LogChunk { page, older_cursor }) => {
                if p.run_id.as_ref().is_some_and(|r| r != &page.run_id) {
                    p.loading = false;
                    return;
                }
                p.apply_tail(page, older_cursor);
            }
            Err(e) => {
                p.loading = false;
                if e.code != mira_protocol::ErrorCode::NOT_FOUND {
                    p.error = Some(e.message);
                }
            }
        }
    }

    pub(super) fn reload_selected_tail(&mut self) {
        if let Some(i) = self.selected_oneoff_index() {
            let o = &mut self.oneoffs[i];
            if let Some(p) = &mut o.pane {
                p.loading = true;
                let _ = self.io.read.send(Read::RunTail(o.run_id.clone()));
            }
            return;
        }
        let Some(a) = self.selected_ref() else {
            return;
        };
        if let Some(p) = self.panes.get_mut(&a) {
            p.loading = true;
            let _ = self.io.read.send(Read::Tail(a.clone(), p.run_id.clone()));
        }
    }

    pub fn tick(&mut self) {
        self.refresh_branch(false);
        self.poll_views();
        self.poll_screen();
        if self.notice.as_ref().is_some_and(|n| {
            n.at.elapsed().as_secs()
                >= if n.error || n.open.is_some() {
                    NOTICE_SECS
                } else {
                    INFO_SECS
                }
        }) {
            self.notice = None;
        }
    }

    /// Reads open views again while their freshness or durability can still change without
    /// a new revision: a save in progress, or "current" data whose producing run ended.
    fn poll_views(&mut self) {
        if self.view_poll_at.elapsed().as_millis() < VIEW_POLL_MS {
            return;
        }
        self.view_poll_at = Instant::now();
        let running: Vec<&RunId> = self.active.values().map(|r| &r.run_id).collect();
        let due: Vec<ViewRef> = self
            .view_panes
            .iter()
            .filter(|(_, p)| !p.loading && p.meta.as_ref().is_some_and(|m| m.may_change(&running)))
            .map(|(v, _)| v.clone())
            .collect();
        for v in due {
            self.load_view(&v);
        }
    }

    /// The selected item's active PTY run while its log has no lines: the run a
    /// "waiting for input" hint is about.
    pub fn silent_pty_run(&self) -> Option<&RunId> {
        let a = self.selected_item()?.action_ref.clone();
        let run = self.active.get(&a)?;
        let p = self.panes.get(&a)?;
        (self.term.is_pty(&a)
            && run.lifecycle.is_active()
            && p.records.is_empty()
            && !self.viewing.contains_key(&a)
            && p.run_id.as_ref().is_none_or(|r| r == &run.run_id))
        .then_some(&run.run_id)
    }

    /// Reads the screen of the selected silent PTY run at most once a second.
    fn poll_screen(&mut self) {
        let Some(run_id) = self.silent_pty_run().cloned() else {
            return;
        };
        if self
            .screen_at
            .is_some_and(|t| t.elapsed().as_millis() < SCREEN_POLL_MS)
        {
            return;
        }
        self.screen_at = Some(Instant::now());
        let active: Vec<&RunId> = self.active.values().map(|r| &r.run_id).collect();
        self.screens.retain(|r, _| active.contains(&r));
        let _ = self.io.read.send(Read::Screen(run_id));
    }
}
