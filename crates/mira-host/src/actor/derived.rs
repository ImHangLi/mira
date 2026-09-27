//! Derived log views: a log view whose manifest names `"source": {"logs": "PLUGIN.ACTION"}`.
//!
//! The host serves it from the log of the source action's current run, or its latest run
//! when none is active, filtered by `grep` (case-insensitive substring) and `stream`, and
//! bounded like any log view. No plugin process runs. The content lives in memory: it is
//! rebuilt from the run log (the newest 2000 records) whenever the source run changes, the
//! definition changes, or live log batches were dropped. Reads and view stream events use
//! the same paths as every other view, so the TUI and `mira view` show the same lines.

use std::collections::HashMap;

use mira_protocol::error::{ErrorCode, ErrorInfo};
use mira_protocol::ids::*;
use mira_protocol::ipc::ViewReadParams;
use mira_protocol::manifest::{ViewDefinition, ViewSource};
use mira_protocol::reply::ReplyMeta;
use mira_protocol::run::LogRecord;
use mira_protocol::time::Timestamp;
use mira_protocol::view::*;

use super::cursor::{self, Kind};
use super::views::Page;
use super::{Actor, Handled, MAX_LIMIT, budget};
use crate::logs::{RING_RECORDS, RunLog, SharedLog};
use crate::storage::RunFilter;

const DEFAULT_PAGE: usize = 100;

pub struct DerivedView {
    definition_hash: Digest,
    run_id: RunId,
    items: Vec<LogItem>,
    revision: ViewRevision,
    recorded_at: Timestamp,
}

#[derive(Default)]
pub struct DerivedViews {
    entries: HashMap<ViewRef, DerivedView>,
    last_revision: u64,
}

impl DerivedViews {
    /// Strictly increasing within this host and, because it starts from the clock, across
    /// restarts; cursors and the TUI only compare revisions for equality.
    fn next_revision(&mut self) -> ViewRevision {
        // Unix milliseconds stay far below the safe integer limit that `new` enforces.
        const MAX: u64 = 9_007_199_254_740_991;
        let now = u64::try_from(Timestamp::now().unix_ms()).unwrap_or(0);
        self.last_revision = (self.last_revision + 1).max(now).min(MAX);
        ViewRevision::new(self.last_revision).unwrap_or_default()
    }
}

fn items_from(src: &ViewSource, records: &[LogRecord]) -> Vec<LogItem> {
    records
        .iter()
        .filter(|r| src.keeps(r.stream, &r.text))
        .map(|r| LogItem {
            id: r.log_seq.to_string(),
            text: r.text.clone(),
            level: r.level,
            producer_at: None,
            fields: None,
            recorded_at: Some(r.recorded_at),
        })
        .collect()
}

fn cap(items: &mut Vec<LogItem>, max: usize) {
    if items.len() > max {
        items.drain(..items.len() - max);
    }
}

impl Actor {
    /// Derived views of the accepted catalog whose source is `action`.
    fn derived_for(&self, action: &ActionRef) -> Vec<(ViewRef, ViewDefinition)> {
        let Ok(set) = self.accepted() else {
            return Vec::new();
        };
        set.plugins
            .iter()
            .flat_map(|lp| {
                lp.plugin
                    .views
                    .iter()
                    .filter(|v| v.source.as_ref().is_some_and(|s| &s.logs == action))
                    .map(|v| (ViewRef::new(lp.plugin.id.clone(), v.id.clone()), v.clone()))
            })
            .collect()
    }

    fn view_log_cap(&self) -> usize {
        self.accepted()
            .map_or(1000, |s| s.storage.view_log_items as usize)
    }

    /// The source action's active run, else its most recent run this host still holds,
    /// with the run's end time when it has ended.
    fn source_log(&self, action: &ActionRef) -> Option<(RunId, SharedLog, Option<Timestamp>)> {
        if let Some(id) = self.by_action.get(action)
            && let Some(run) = self.runs.get(id)
        {
            return Some((id.clone(), run.log.clone(), None));
        }
        self.recent
            .iter()
            .find(|(rec, _)| rec.action_ref.as_ref() == Some(action))
            .map(|(rec, log)| (rec.run_id.clone(), log.clone(), rec.ended_at))
    }

    /// Rebuilds one derived view from a run log and announces the new revision.
    /// `ended_at` (a finished source run) becomes `recorded_at`, so readers see when the
    /// lines stopped changing.
    fn derived_rebuild(
        &mut self,
        view_ref: &ViewRef,
        def: &ViewDefinition,
        run_id: RunId,
        ended_at: Option<Timestamp>,
        log: &mut RunLog,
    ) {
        let Some(src) = &def.source else { return };
        let mut items = items_from(src, &log.read_before(None, RING_RECORDS));
        cap(&mut items, self.view_log_cap());
        let revision = self.derived.next_revision();
        self.derived.entries.insert(
            view_ref.clone(),
            DerivedView {
                definition_hash: def.definition_hash.clone(),
                run_id,
                items,
                revision,
                recorded_at: ended_at.unwrap_or_else(Timestamp::now),
            },
        );
        self.broadcast_view(view_ref, revision);
    }

    /// Makes sure a derived view reflects the source's current (or latest) run.
    fn derived_refresh(&mut self, view_ref: &ViewRef, def: &ViewDefinition) {
        let Some(src) = &def.source else { return };
        let Some((run_id, log, ended_at)) = self.source_log(&src.logs) else {
            return;
        };
        let current = self
            .derived
            .entries
            .get(view_ref)
            .is_some_and(|e| e.definition_hash == def.definition_hash && e.run_id == run_id);
        if current {
            return;
        }
        let Ok(mut log) = log.lock() else { return };
        self.derived_rebuild(view_ref, def, run_id, ended_at, &mut log);
    }

    /// A new run of `action` started: its derived views now follow it, starting empty.
    pub(crate) fn derived_run_started(&mut self, action: &ActionRef, run_id: &RunId) {
        for (view_ref, def) in self.derived_for(action) {
            let revision = self.derived.next_revision();
            self.derived.entries.insert(
                view_ref.clone(),
                DerivedView {
                    definition_hash: def.definition_hash.clone(),
                    run_id: run_id.clone(),
                    items: Vec::new(),
                    revision,
                    recorded_at: Timestamp::now(),
                },
            );
            self.broadcast_view(&view_ref, revision);
        }
    }

    /// New log records of a run: append the matching ones to every derived view of it.
    pub(crate) fn derived_logs(&mut self, run_id: &RunId, records: &[LogRecord]) {
        let Some(action) = self
            .runs
            .get(run_id)
            .and_then(|r| r.record.action_ref.clone())
        else {
            return;
        };
        let max = self.view_log_cap();
        for (view_ref, def) in self.derived_for(&action) {
            let Some(src) = &def.source else { continue };
            let follows =
                self.derived.entries.get(&view_ref).is_some_and(|e| {
                    e.definition_hash == def.definition_hash && &e.run_id == run_id
                });
            if !follows {
                self.derived_refresh(&view_ref, &def);
                continue;
            }
            let added = items_from(src, records);
            if added.is_empty() {
                continue;
            }
            let revision = self.derived.next_revision();
            if let Some(e) = self.derived.entries.get_mut(&view_ref) {
                e.items.extend(added);
                cap(&mut e.items, max);
                e.revision = revision;
                e.recorded_at = Timestamp::now();
            }
            self.broadcast_view(&view_ref, revision);
        }
    }

    /// Live log batches of a run were dropped: rebuild its derived views from the log.
    pub(crate) fn derived_gap(&mut self, run_id: &RunId) {
        let Some(action) = self
            .runs
            .get(run_id)
            .and_then(|r| r.record.action_ref.clone())
        else {
            return;
        };
        for (view_ref, def) in self.derived_for(&action) {
            self.derived.entries.remove(&view_ref);
            self.derived_refresh(&view_ref, &def);
        }
    }

    /// The source run ended: the lines are unchanged, but they are historical now, and
    /// `recorded_at` becomes the end time.
    pub(crate) fn derived_run_ended(&mut self, action: &ActionRef, run_id: &RunId) {
        for (view_ref, _) in self.derived_for(action) {
            let revision = self.derived.next_revision();
            if let Some(e) = self
                .derived
                .entries
                .get_mut(&view_ref)
                .filter(|e| &e.run_id == run_id)
            {
                e.revision = revision;
                e.recorded_at = Timestamp::now();
                self.broadcast_view(&view_ref, revision);
            }
        }
    }

    /// Startup: fill derived views from the latest stored run of each source.
    pub(super) async fn init_derived_views(&mut self) {
        let (Ok(set), Ok(storage)) = (self.accepted(), self.storage.clone()) else {
            return;
        };
        for lp in &set.plugins {
            for def in &lp.plugin.views {
                let Some(src) = &def.source else { continue };
                let runs = storage
                    .list_runs(RunFilter {
                        action_ref: Some(src.logs.clone()),
                        limit: 1,
                        ..RunFilter::default()
                    })
                    .await;
                let Some(rec) = runs.ok().and_then(|v| v.into_iter().next()) else {
                    continue;
                };
                let dir = self.paths.run_logs(&rec.run_id);
                if !dir.exists() {
                    continue;
                }
                let mut log = RunLog::open_existing(dir);
                let view_ref = ViewRef::new(lp.plugin.id.clone(), def.id.clone());
                self.derived_rebuild(&view_ref, def, rec.run_id, rec.ended_at, &mut log);
            }
        }
    }

    pub(super) fn derived_read(
        &mut self,
        p: ViewReadParams,
        def: &ViewDefinition,
    ) -> Result<Handled, ErrorInfo> {
        let Some(src) = def.source.clone() else {
            return Err(ErrorInfo::new(ErrorCode::INTERNAL, "not a derived view"));
        };
        self.derived_refresh(&p.view_ref, def);
        let limit = p
            .limit
            .map_or(DEFAULT_PAGE, |l| (l as usize).clamp(1, MAX_LIMIT));
        let budget = budget::budget(p.max_bytes);
        let Some(e) = self.derived.entries.get(&p.view_ref) else {
            return Ok(self.ok(
                ViewSnapshot {
                    view_ref: p.view_ref,
                    view_revision: None,
                    kind: def.kind,
                    recorded_at: None,
                    source_run_id: None,
                    source_kind: None,
                    definition_hash: def.definition_hash.clone(),
                    freshness: Freshness::Historical,
                    freshness_reason: Some(format!(
                        "derived from the logs of `{}`, which has not run yet",
                        src.logs
                    )),
                    durability: Durability::Committed,
                    data: None,
                },
                ReplyMeta::default(),
            ));
        };
        let source = p.view_ref.to_string();
        let offset = match p.cursor.as_deref() {
            None => 0,
            Some(c) => {
                let c = cursor::decode(c, Kind::View, &source, "")?;
                if c.revision != Some(e.revision.get()) {
                    return Err(ErrorInfo::new(
                        ErrorCode::VIEW_CHANGED,
                        format!(
                            "the view changed to revision {}; restart from the first page",
                            e.revision
                        ),
                    )
                    .with_next_action(
                        &["mira", "view", &source],
                        "Read the current revision from the start.",
                    ));
                }
                c.pos.o.unwrap_or(0) as usize
            }
        };
        let live = self
            .runs
            .get(&e.run_id)
            .is_some_and(|r| r.record.lifecycle.is_active());
        let (freshness, reason) = if live {
            (
                Freshness::Current,
                format!(
                    "derived from the logs of `{}`; the producing run {} is still running",
                    src.logs, e.run_id
                ),
            )
        } else {
            (
                Freshness::Historical,
                format!(
                    "derived from the logs of `{}`; recorded by run {}, which has ended",
                    src.logs, e.run_id
                ),
            )
        };
        let snap = |data: ViewBody| ViewSnapshot {
            view_ref: p.view_ref.clone(),
            view_revision: Some(e.revision),
            kind: def.kind,
            recorded_at: Some(e.recorded_at),
            source_run_id: Some(e.run_id.clone()),
            source_kind: None,
            definition_hash: e.definition_hash.clone(),
            freshness,
            freshness_reason: Some(reason.clone()),
            durability: Durability::Committed,
            data: Some(data),
        };
        let page = Page {
            source: &source,
            revision: e.revision,
            offset,
            limit,
            budget,
        };
        Ok(self.view_page(
            &page,
            &e.items,
            "log item",
            |items| ViewData::Log { items },
            &snap,
        ))
    }
}
