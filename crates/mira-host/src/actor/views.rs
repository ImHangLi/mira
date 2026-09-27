//! The view store (§8.1–8.3, §11.2, §14.6, §15.3): one typed ViewData per view, updated
//! atomically in host receive order with strictly increasing revisions from persisted blocks.
//!
//! Every update (plugin frame or explicit publish) goes through one FIFO queue. The queue
//! stalls only while a revision block is being reserved or a publish request key is being
//! claimed, so no update to a view can overtake another. `last` views are written through one
//! ordered writer task: publishes at once, plugin updates coalesced to once per second and on
//! run end. `session` views live in memory until the session ends.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use mira_protocol::error::{ErrorCode, ErrorInfo};
use mira_protocol::ids::*;
use mira_protocol::ipc::*;
use mira_protocol::manifest::{Persistence, ViewDefinition, wire_from_value};
use mira_protocol::mpp::{PluginEvent, PluginFrame};
use mira_protocol::reply::ReplyMeta;
use mira_protocol::run::{Lifecycle, Outcome};
use mira_protocol::time::Timestamp;
use mira_protocol::view::*;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::cursor::{self, Kind, Pos};
use super::{Actor, Handled, MAX_LIMIT, Msg, Responder, budget};
use crate::diag;
use crate::storage::{Claim, KeyClaim, KeyScope, StorageError, StoredView};

const DEFAULT_PAGE: usize = 100;
const COALESCE: Duration = Duration::from_secs(1);

pub struct ViewEntry {
    pub data: ViewData,
    pub revision: ViewRevision,
    pub recorded_at: Timestamp,
    pub source_run_id: Option<RunId>,
    pub source_kind: SourceKind,
    pub definition_hash: Digest,
    pub persistence: Persistence,
    /// Why the content is stale (source failed, or restored without a confirmed end).
    pub stale: Option<String>,
    /// Highest revision confirmed committed.
    pub saved: Option<ViewRevision>,
    /// Highest revision handed to the writer.
    pub requested: Option<ViewRevision>,
    pub last_write: Option<Instant>,
    pub save_error: Option<String>,
}

struct PublishReq {
    responder: Responder,
    expected: Option<ViewRevision>,
    claim: Option<KeyClaim>,
}

pub struct ViewUpdate {
    view_ref: ViewRef,
    op: ViewOp,
    data: ViewData,
    source_run_id: Option<RunId>,
    source_kind: SourceKind,
    publish: Option<PublishReq>,
}

enum Wait {
    Block,
    Claim,
}

#[derive(Default)]
pub struct ViewStore {
    pub entries: HashMap<ViewRef, ViewEntry>,
    queue: VecDeque<ViewUpdate>,
    wait: Option<Wait>,
    /// A publish whose request key is being claimed: (update, new data or no-op, revision).
    claiming: Option<(ViewUpdate, Option<ViewData>, ViewRevision)>,
    /// Reserved revisions `[next, end)`.
    block: Option<(u64, u64)>,
    writer: Option<mpsc::UnboundedSender<StoredView>>,
    commit_waiters: Vec<(ViewRef, ViewRevision, Responder, PublishResult)>,
    /// Views removed by retention: last revision and when (ms), so reads say "cleaned up".
    pub cleaned: HashMap<ViewRef, (u64, i64)>,
}

fn verr(code: ErrorCode, msg: impl Into<String>) -> ErrorInfo {
    ErrorInfo::new(code, msg)
}

/// Stored ViewData JSON, host `recorded_at` on log items included.
fn decode_stored(json_text: &str) -> Result<ViewData, String> {
    serde_json::from_str(json_text).map_err(|e| e.to_string())
}

fn same_item(a: &LogItem, b: &LogItem) -> bool {
    a.id == b.id
        && a.text == b.text
        && a.level == b.level
        && a.producer_at == b.producer_at
        && a.fields == b.fields
}

fn stamp(items: &mut [LogItem]) {
    let now = Timestamp::now();
    for i in items {
        i.recorded_at = Some(now);
    }
}

/// Checks one update against the definition and the current content, and builds the new
/// content. `Ok(None)` is a no-op (every appended log item already exists unchanged).
fn build(
    def: &ViewDefinition,
    current: Option<&ViewEntry>,
    op: ViewOp,
    data: ViewData,
    expected: Option<ViewRevision>,
    log_cap: usize,
) -> Result<Option<ViewData>, ErrorInfo> {
    if data.kind() != def.kind {
        return Err(verr(
            ErrorCode::SCHEMA_INVALID,
            format!(
                "view `{}` is a {:?} view; the update carries {:?} data",
                def.id,
                def.kind,
                data.kind()
            )
            .to_lowercase(),
        ));
    }
    if let Some(exp) = expected
        && current.map(|c| c.revision) != Some(exp)
    {
        return Err(verr(
            ErrorCode::REVISION_CONFLICT,
            match current {
                Some(c) => format!(
                    "view is at revision {}, not {exp}; nothing was written",
                    c.revision
                ),
                None => format!("view has no data yet, not revision {exp}; nothing was written"),
            },
        ));
    }
    if let ViewData::Table { columns, .. } = &data {
        for ra in &def.row_actions {
            for (param, col) in &ra.bindings {
                if !columns.iter().any(|c| &c.id == col) {
                    return Err(verr(
                        ErrorCode::SCHEMA_INVALID,
                        format!(
                            "row action `{}` binds `{param}` to column `{col}`, which this table does not have",
                            ra.action
                        ),
                    ));
                }
            }
        }
    }
    match (op, data) {
        (ViewOp::Replace, ViewData::Log { mut items }) => {
            stamp(&mut items);
            if items.len() > log_cap {
                items.drain(..items.len() - log_cap);
            }
            Ok(Some(ViewData::Log { items }))
        }
        (ViewOp::Replace, data) => Ok(Some(data)),
        (ViewOp::Append, ViewData::Log { items: incoming }) => {
            let mut items = match current.map(|c| &c.data) {
                Some(ViewData::Log { items }) => items.clone(),
                _ => Vec::new(),
            };
            let mut added = Vec::new();
            for (i, item) in incoming.into_iter().enumerate() {
                match items.iter().find(|e| e.id == item.id) {
                    Some(e) if same_item(e, &item) => {}
                    Some(_) => {
                        return Err(verr(
                            ErrorCode::ITEM_ID_CONFLICT,
                            format!(
                                "log item `{}` already exists with different content; the batch was rejected",
                                item.id
                            ),
                        )
                        .with_pointer(format!("/data/items/{i}/id")));
                    }
                    None => added.push(item),
                }
            }
            if added.is_empty() {
                return Ok(None);
            }
            stamp(&mut added);
            items.extend(added);
            if items.len() > log_cap {
                items.drain(..items.len() - log_cap);
            }
            Ok(Some(ViewData::Log { items }))
        }
        (ViewOp::Append, _) => Err(verr(
            ErrorCode::INVALID_FRAME,
            "append is only allowed for log views",
        )),
    }
}

impl ViewEntry {
    fn durability(&self, storage_ok: bool) -> Durability {
        match self.persistence {
            Persistence::Session => Durability::SessionOnly,
            Persistence::Last if self.saved.is_some_and(|s| s >= self.revision) => {
                Durability::Committed
            }
            Persistence::Last if self.save_error.is_some() || !storage_ok => {
                Durability::Unavailable
            }
            Persistence::Last => Durability::Buffered,
        }
    }

    fn stored(&self, view_ref: &ViewRef) -> Result<StoredView, String> {
        Ok(StoredView {
            view_ref: view_ref.clone(),
            revision: self.revision,
            kind: self.data.kind(),
            recorded_at: self.recorded_at,
            source_run_id: self.source_run_id.clone(),
            source_kind: self.source_kind,
            definition_hash: self.definition_hash.clone(),
            data_json: serde_json::to_string(&self.data).map_err(|e| e.to_string())?,
        })
    }
}

/// The request side of one view page.
pub(super) struct Page<'a> {
    pub(super) source: &'a str,
    pub(super) revision: ViewRevision,
    pub(super) offset: usize,
    pub(super) limit: usize,
    pub(super) budget: usize,
}

impl Actor {
    /// Retention removed these views: drop them from memory and remember the cleanup.
    pub(super) fn views_cleaned(&mut self, views: Vec<(ViewRef, u64)>, at: i64) {
        for (v, rev) in views {
            let current = self.views.entries.get(&v).map(|e| e.revision.get());
            if current.is_none_or(|c| c == rev) {
                self.views.entries.remove(&v);
                self.views.cleaned.insert(v, (rev, at));
            }
        }
        self.state_changed();
    }

    /// Loads persisted `last` views of the accepted definitions (startup only).
    pub(super) async fn init_views(&mut self) {
        let (Ok(set), Ok(storage)) = (self.accepted(), self.storage.clone()) else {
            return;
        };
        if let Ok(cleaned) = storage.cleaned_views().await {
            self.views.cleaned = cleaned
                .into_iter()
                .map(|(v, rev, at)| (v, (rev, at)))
                .collect();
        }
        for lp in &set.plugins {
            for def in lp
                .plugin
                .views
                .iter()
                .filter(|d| d.persistence == Persistence::Last)
            {
                let view_ref = ViewRef::new(lp.plugin.id.clone(), def.id.clone());
                match storage.load_view(view_ref.clone()).await {
                    Ok(Some(sv)) => match decode_stored(&sv.data_json) {
                        Ok(data) => {
                            // Data restored after a host restart is stale (§8.2, §14.6): it
                            // shows what was recorded then, not what is true now.
                            let at = sv.recorded_at;
                            let stale = Some(match &sv.source_run_id {
                                None => format!(
                                    "restored after a host restart; published at {at} and not updated since"
                                ),
                                Some(id) => match storage.get_run(id.clone()).await {
                                    Ok(Some(rec))
                                        if rec.lifecycle
                                            == Lifecycle::Finished {
                                                outcome: Outcome::Succeeded,
                                            } =>
                                    {
                                        format!(
                                            "restored after a host restart; recorded at {at} by run {id}, which succeeded then; not re-checked since"
                                        )
                                    }
                                    Ok(Some(_)) => format!(
                                        "restored after a host restart; recorded at {at} by run {id}, which did not succeed"
                                    ),
                                    _ => format!(
                                        "restored after a host restart; recorded at {at} by run {id}, whose end is unknown"
                                    ),
                                },
                            });
                            self.views.entries.insert(
                                view_ref,
                                ViewEntry {
                                    data,
                                    revision: sv.revision,
                                    recorded_at: sv.recorded_at,
                                    source_run_id: sv.source_run_id,
                                    source_kind: sv.source_kind,
                                    definition_hash: sv.definition_hash,
                                    persistence: Persistence::Last,
                                    stale,
                                    saved: Some(sv.revision),
                                    requested: Some(sv.revision),
                                    last_write: None,
                                    save_error: None,
                                },
                            );
                        }
                        Err(e) => diag(format!("stored view {view_ref} cannot be decoded: {e}")),
                    },
                    Ok(None) => {}
                    Err(e) => self.storage_warning(&e),
                }
            }
        }
    }

    /// Commits every buffered `last` view before the host exits.
    pub(super) async fn flush_views_now(&mut self) {
        let Ok(storage) = self.storage.clone() else {
            return;
        };
        let dirty: Vec<StoredView> = self
            .views
            .entries
            .iter()
            .filter(|(_, e)| {
                e.persistence == Persistence::Last && e.saved.is_none_or(|s| s < e.revision)
            })
            .filter_map(|(r, e)| e.stored(r).ok())
            .collect();
        for v in dirty {
            if let Err(e) = storage.save_view(v).await
                && !matches!(e, StorageError::Unavailable(ref m) if m.contains("not newer"))
            {
                diag(format!("could not save a view before exit: {e}"));
            }
        }
    }

    pub(crate) fn enqueue_view_update(
        &mut self,
        view_ref: ViewRef,
        op: ViewOp,
        data: ViewData,
        source_run_id: Option<RunId>,
        source_kind: SourceKind,
    ) {
        // Definition checks that need no content fail the run at once, in frame order.
        if let Some(run_id) = &source_run_id {
            let known = self.accepted().ok().and_then(|set| {
                set.view(&view_ref)
                    .map(|(_, d)| (d.kind, d.source.as_ref().map(|s| s.logs.clone())))
            });
            let problem = match known {
                None => Some(format!(
                    "the plugin sent view `{}`, which its manifest does not declare",
                    view_ref.view
                )),
                Some((_, Some(logs))) => Some(format!(
                    "view `{}` is derived from the logs of `{logs}`; plugins cannot write it",
                    view_ref.view
                )),
                Some((kind, None)) if kind != data.kind() => Some(
                    format!(
                        "view `{}` is a {kind:?} view; the frame carries {:?} data",
                        view_ref.view,
                        data.kind()
                    )
                    .to_lowercase(),
                ),
                Some(_) => None,
            };
            if let Some(msg) = problem {
                let run_id = run_id.clone();
                return self.protocol_error(
                    &run_id,
                    verr(ErrorCode::INVALID_FRAME, msg),
                    Some(view_ref.view),
                );
            }
        }
        self.views.queue.push_back(ViewUpdate {
            view_ref,
            op,
            data,
            source_run_id,
            source_kind,
            publish: None,
        });
        self.process_views();
    }

    fn take_revision(&mut self) -> Option<ViewRevision> {
        let (next, end) = self.views.block?;
        let rev = ViewRevision::new(next).ok()?;
        self.views.block = (next + 1 < end).then_some((next + 1, end));
        Some(rev)
    }

    fn request_block(&mut self) {
        match self.storage.clone() {
            Ok(storage) => {
                self.views.wait = Some(Wait::Block);
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let result = storage.reserve_view_block().await;
                    let _ = tx.send(Msg::ViewBlock { result }).await;
                });
            }
            Err(e) => {
                let info = e.to_error_info();
                self.fail_all_views(info);
            }
        }
    }

    pub(super) fn view_block(&mut self, result: Result<(u64, u64), StorageError>) {
        self.views.wait = None;
        match result {
            Ok(block) => {
                self.views.block = Some(block);
                self.process_views();
            }
            Err(e) => {
                self.storage_warning(&e);
                self.fail_all_views(e.to_error_info());
            }
        }
    }

    fn fail_all_views(&mut self, info: ErrorInfo) {
        let queued: Vec<_> = self.views.queue.drain(..).collect();
        for upd in queued {
            self.fail_update(upd, info.clone());
        }
    }

    fn fail_update(&mut self, upd: ViewUpdate, e: ErrorInfo) {
        if let Some(p) = upd.publish {
            return p.responder.send(self.fail(e));
        }
        let Some(run_id) = upd.source_run_id else {
            return;
        };
        if matches!(e.code.as_str(), "STORAGE_UNAVAILABLE" | "COUNTER_EXHAUSTED") {
            self.host_note(
                &run_id,
                mira_protocol::view::LogLevel::Warn,
                &format!(
                    "view `{}` was not updated: {}",
                    upd.view_ref.view, e.message
                ),
            );
        } else {
            self.protocol_error(&run_id, e, Some(upd.view_ref.view));
        }
    }

    /// Applies queued updates in order until one must wait for storage.
    fn process_views(&mut self) {
        while self.views.wait.is_none() && !self.views.queue.is_empty() {
            if self.views.block.is_none() {
                return self.request_block();
            }
            let Some(mut upd) = self.views.queue.pop_front() else {
                return;
            };
            let set = match self.accepted() {
                Ok(s) => s,
                Err(e) => {
                    self.fail_update(upd, e);
                    continue;
                }
            };
            let Some((_, def)) = set.view(&upd.view_ref) else {
                let e = ErrorInfo::item_not_found("view", &upd.view_ref);
                self.fail_update(upd, e);
                continue;
            };
            let log_cap = set.storage.view_log_items as usize;
            let expected = upd.publish.as_ref().and_then(|p| p.expected);
            let data = std::mem::replace(&mut upd.data, ViewData::Log { items: vec![] });
            let current = self.views.entries.get(&upd.view_ref);
            let built = build(def, current, upd.op, data, expected, log_cap);
            let current_rev = current.map(|c| c.revision);
            let (new, rev) = match (built, current_rev) {
                (Ok(Some(d)), _) => match self.take_revision() {
                    Some(rev) => (Some(d), rev),
                    None => return self.request_block(),
                },
                // No-op append: nothing changes and no revision is used.
                (Ok(None), Some(rev)) => (None, rev),
                (Ok(None), None) => continue,
                (Err(e), _) => {
                    self.fail_update(upd, e);
                    continue;
                }
            };
            if let Some(claim) = upd.publish.as_mut().and_then(|p| p.claim.take()) {
                let Ok(storage) = self.storage.clone() else {
                    let e = verr(ErrorCode::STORAGE_UNAVAILABLE, "request keys need storage");
                    self.fail_update(upd, e);
                    continue;
                };
                self.views.claiming = Some((upd, new, rev));
                self.views.wait = Some(Wait::Claim);
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let result = storage.claim_key(claim, rev.to_string()).await;
                    let _ = tx.send(Msg::ViewClaimed { result }).await;
                });
                return;
            }
            self.finish_update(upd, new, rev);
        }
    }

    fn finish_update(&mut self, upd: ViewUpdate, new: Option<ViewData>, rev: ViewRevision) {
        match new {
            Some(data) => self.apply_view(upd, data, rev),
            None => {
                let Some(p) = upd.publish else { return };
                let durability = self
                    .views
                    .entries
                    .get(&upd.view_ref)
                    .map_or(Durability::Unavailable, |c| {
                        c.durability(self.storage.is_ok())
                    });
                let res = PublishResult {
                    view_ref: upd.view_ref,
                    view_revision: rev,
                    durability,
                    reused: false,
                };
                p.responder.send(self.ok(res, ReplyMeta::default()));
            }
        }
    }

    pub(super) fn view_claimed(&mut self, result: Result<Claim, StorageError>) {
        self.views.wait = None;
        let Some((upd, new, rev)) = self.views.claiming.take() else {
            return self.process_views();
        };
        match result {
            Ok(Claim::New) => self.finish_update(upd, new, rev),
            Ok(Claim::Same { reference }) => {
                let revision = reference
                    .parse()
                    .ok()
                    .and_then(|n| ViewRevision::new(n).ok());
                let durability =
                    self.views
                        .entries
                        .get(&upd.view_ref)
                        .map_or(Durability::Unavailable, |e| {
                            if e.persistence == Persistence::Session {
                                Durability::SessionOnly
                            } else {
                                Durability::Committed
                            }
                        });
                if let Some(p) = upd.publish {
                    let reply = match revision {
                        Some(view_revision) => self.ok(
                            PublishResult {
                                view_ref: upd.view_ref,
                                view_revision,
                                durability,
                                reused: true,
                            },
                            ReplyMeta::default(),
                        ),
                        None => self.fail(verr(
                            ErrorCode::OUTCOME_UNKNOWN,
                            "the request key refers to an unreadable earlier publish",
                        )),
                    };
                    p.responder.send(reply);
                }
            }
            Ok(Claim::Conflict) => self.fail_update(
                upd,
                verr(
                    ErrorCode::REQUEST_KEY_CONFLICT,
                    "request key was used with a different view frame",
                ),
            ),
            Err(e) => self.fail_update(upd, e.to_error_info()),
        }
        self.process_views();
    }

    fn apply_view(&mut self, upd: ViewUpdate, data: ViewData, rev: ViewRevision) {
        let Some((_, def)) = self.accepted().ok().and_then(|s| {
            s.view(&upd.view_ref)
                .map(|(lp, d)| (lp.plugin.id.clone(), d.clone()))
        }) else {
            return self.fail_update(upd, verr(ErrorCode::NOT_FOUND, "the view is gone"));
        };
        let prev = self.views.entries.remove(&upd.view_ref);
        let entry = ViewEntry {
            data,
            revision: rev,
            recorded_at: Timestamp::now(),
            source_run_id: upd.source_run_id.clone(),
            source_kind: upd.source_kind,
            definition_hash: def.definition_hash.clone(),
            persistence: def.persistence,
            stale: None,
            saved: prev.as_ref().and_then(|p| p.saved),
            requested: prev.as_ref().and_then(|p| p.requested),
            last_write: prev.as_ref().and_then(|p| p.last_write),
            save_error: None,
        };
        self.views.entries.insert(upd.view_ref.clone(), entry);
        if let Some(run_id) = &upd.source_run_id
            && let Some(mpp) = self.runs.get_mut(run_id).and_then(|r| r.mpp.as_mut())
        {
            mpp.touched_views.insert(upd.view_ref.clone());
        }
        self.broadcast_view(&upd.view_ref, rev);
        let Some(p) = upd.publish else {
            return;
        };
        let res = PublishResult {
            view_ref: upd.view_ref.clone(),
            view_revision: rev,
            durability: Durability::Committed,
            reused: false,
        };
        match def.persistence {
            Persistence::Session => p.responder.send(self.ok(
                PublishResult {
                    durability: Durability::SessionOnly,
                    ..res
                },
                ReplyMeta::default(),
            )),
            Persistence::Last => {
                // Success only after the commit (§15.3).
                self.views
                    .commit_waiters
                    .push((upd.view_ref.clone(), rev, p.responder, res));
                self.save_view_now(&upd.view_ref);
            }
        }
    }

    fn writer(&mut self) -> Option<mpsc::UnboundedSender<StoredView>> {
        if let Some(w) = &self.views.writer {
            return Some(w.clone());
        }
        let storage = self.storage.clone().ok()?;
        let (tx, mut rx) = mpsc::unbounded_channel::<StoredView>();
        let actor = self.tx.clone();
        tokio::spawn(async move {
            while let Some(v) = rx.recv().await {
                let (view_ref, revision) = (v.view_ref.clone(), v.revision);
                let error = storage.save_view(v).await.err().map(|e| e.to_string());
                let _ = actor
                    .send(Msg::ViewSaved {
                        view_ref,
                        revision,
                        error,
                    })
                    .await;
            }
        });
        self.views.writer = Some(tx.clone());
        Some(tx)
    }

    fn save_view_now(&mut self, view_ref: &ViewRef) {
        let writer = self.writer();
        let Some(e) = self.views.entries.get_mut(view_ref) else {
            return;
        };
        if e.persistence != Persistence::Last || e.requested.is_some_and(|r| r >= e.revision) {
            return;
        }
        let stored = e.stored(view_ref);
        match (writer, stored) {
            (Some(w), Ok(v)) => {
                if w.send(v).is_ok() {
                    e.requested = Some(e.revision);
                    e.last_write = Some(Instant::now());
                } else {
                    e.save_error = Some("the view writer has stopped".into());
                }
            }
            (_, Err(err)) => e.save_error = Some(err),
            (None, _) => e.save_error = Some("storage is unavailable".into()),
        }
    }

    pub(super) fn view_saved(
        &mut self,
        view_ref: ViewRef,
        revision: ViewRevision,
        error: Option<String>,
    ) {
        if let Some(e) = self.views.entries.get_mut(&view_ref) {
            match &error {
                None => {
                    e.saved = Some(e.saved.map_or(revision, |s| s.max(revision)));
                    e.save_error = None;
                }
                Some(msg) => e.save_error = Some(msg.clone()),
            }
        }
        if let Some(msg) = &error {
            self.storage_warning(&StorageError::Unavailable(msg.clone()));
        }
        let waiters = std::mem::take(&mut self.views.commit_waiters);
        for (vr, rev, responder, res) in waiters {
            if vr != view_ref || rev > revision {
                self.views.commit_waiters.push((vr, rev, responder, res));
                continue;
            }
            responder.send(match &error {
                None => self.ok(res, ReplyMeta::default()),
                Some(msg) => self.fail(verr(
                    ErrorCode::STORAGE_UNAVAILABLE,
                    format!("the view was updated in memory but not committed: {msg}"),
                )),
            });
        }
    }

    /// Coalesced persistence of plugin-produced `last` views: at most once per second.
    pub(super) fn views_tick(&mut self) {
        let due: Vec<ViewRef> = self
            .views
            .entries
            .iter()
            .filter(|(_, e)| {
                e.persistence == Persistence::Last
                    && e.requested.is_none_or(|r| r < e.revision)
                    && e.last_write.is_none_or(|t| t.elapsed() >= COALESCE)
            })
            .map(|(r, _)| r.clone())
            .collect();
        for r in due {
            self.save_view_now(&r);
        }
    }

    /// Run end: persist what the run produced now; failed runs leave their views stale.
    pub(crate) fn views_run_ended(&mut self, run_id: &RunId, failed: bool) {
        let Some(mpp) = self.runs.get(run_id).and_then(|r| r.mpp.as_ref()) else {
            return;
        };
        let touched: Vec<ViewRef> = mpp.touched_views.iter().cloned().collect();
        for r in &touched {
            if failed && let Some(e) = self.views.entries.get_mut(r) {
                e.stale = Some(format!("the source run {run_id} failed"));
            }
            self.save_view_now(r);
        }
    }

    pub(crate) fn mark_plugin_views_stale(&mut self, plugin: &PluginId, reason: &str) {
        for (r, e) in self.views.entries.iter_mut() {
            if &r.plugin == plugin {
                e.stale = Some(reason.to_owned());
            }
        }
    }

    /// `session` views end with the session (§14.3).
    pub(crate) fn clear_session_views(&mut self) {
        self.views
            .entries
            .retain(|_, e| e.persistence != Persistence::Session);
        // Session payloads (view bodies and oversized items by reference) end here too.
        self.payloads.clear_session();
    }

    fn freshness(&self, e: &ViewEntry, def: &ViewDefinition) -> (Freshness, String) {
        if e.definition_hash != def.definition_hash {
            return (
                Freshness::Stale,
                "the view definition changed after this data was recorded".into(),
            );
        }
        if let Some(r) = &e.stale {
            return (Freshness::Stale, r.clone());
        }
        match &e.source_run_id {
            Some(id)
                if self
                    .runs
                    .get(id)
                    .is_some_and(|r| r.record.lifecycle.is_active()) =>
            {
                (
                    Freshness::Current,
                    format!("the producing run {id} is still running"),
                )
            }
            Some(id) => (
                Freshness::Historical,
                format!("recorded by run {id}, which has ended"),
            ),
            None => (
                Freshness::Historical,
                format!(
                    "published explicitly (source {})",
                    serde_json::to_value(e.source_kind)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_owned))
                        .unwrap_or_default()
                ),
            ),
        }
    }

    pub(super) fn view_read(&mut self, p: ViewReadParams, r: Responder) {
        let derived = self.accepted().ok().and_then(|set| {
            set.view(&p.view_ref)
                .filter(|(_, d)| d.source.is_some())
                .map(|(_, d)| d.clone())
        });
        let read = match derived {
            Some(def) => self.derived_read(p, &def),
            None => self.view_read_inner(p),
        };
        let reply = match read {
            Ok(h) => h,
            Err(e) => self.fail(e),
        };
        r.send(reply);
    }

    fn view_read_inner(&self, p: ViewReadParams) -> Result<Handled, ErrorInfo> {
        let set = self.accepted()?;
        let (_, def) = set
            .view(&p.view_ref)
            .ok_or_else(|| ErrorInfo::item_not_found("view", &p.view_ref))?;
        let limit = p
            .limit
            .map_or(DEFAULT_PAGE, |l| (l as usize).clamp(1, MAX_LIMIT));
        let budget = budget::budget(p.max_bytes);
        let Some(e) = self.views.entries.get(&p.view_ref) else {
            let (durability, reason) = match def.persistence {
                Persistence::Session if self.session.is_none() => (
                    Durability::SessionOnly,
                    "session views exist only while a session is active",
                ),
                Persistence::Session => (Durability::SessionOnly, "no data in this session yet"),
                Persistence::Last => (Durability::Unavailable, "no data has been recorded"),
            };
            let cleaned = self.views.cleaned.get(&p.view_ref).map(|(rev, at)| {
                format!(
                    "cleaned up by retention at {} (last revision {rev}); this is not an empty result",
                    mira_protocol::time::Timestamp::from_unix_ms(*at)
                )
            });
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
                    freshness_reason: Some(cleaned.unwrap_or_else(|| reason.into())),
                    durability,
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
                    return Err(verr(
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
        let (freshness, reason) = self.freshness(e, def);
        let snap = |data: ViewBody| ViewSnapshot {
            view_ref: p.view_ref.clone(),
            view_revision: Some(e.revision),
            kind: e.data.kind(),
            recorded_at: Some(e.recorded_at),
            source_run_id: e.source_run_id.clone(),
            source_kind: Some(e.source_kind),
            definition_hash: e.definition_hash.clone(),
            freshness,
            freshness_reason: Some(reason.clone()),
            durability: e.durability(self.storage.is_ok()),
            data: Some(data),
        };
        let page = Page {
            source: &source,
            revision: e.revision,
            offset,
            limit,
            budget,
        };
        Ok(match &e.data {
            ViewData::Table { columns, rows } => self.view_page(
                &page,
                rows,
                "row",
                |rows| ViewData::Table {
                    columns: columns.clone(),
                    rows,
                },
                &snap,
            ),
            ViewData::Log { items } => self.view_page(
                &page,
                items,
                "log item",
                |items| ViewData::Log { items },
                &snap,
            ),
            other => {
                let reply = self.ok(snap(ViewBody::Inline(other.clone())), ReplyMeta::default());
                match reply {
                    Ok(v) if budget::json_len(&v) <= budget => Ok(v),
                    Ok(_) => {
                        let bytes = serde_json::to_vec(other).unwrap_or_default();
                        let size = bytes.len();
                        let payload = self
                            .payloads
                            .hold(Some(format!("view:{source}@{}", e.revision)), bytes);
                        let summary = format!(
                            "{} view data is {size} bytes, above the {budget}-byte reply budget; \
                             read it with `mira payload read <meta.payload.token>`",
                            format!("{:?}", other.kind()).to_lowercase()
                        );
                        self.ok(
                            snap(ViewBody::Reference(ReferenceData {
                                representation: ReferenceTag::Reference,
                                summary,
                            })),
                            ReplyMeta {
                                truncated: true,
                                payload: Some(payload),
                                ..ReplyMeta::default()
                            },
                        )
                    }
                    Err(err) => Err(err),
                }
            }
        })
    }

    /// One page of table rows or log items within the budget. A first item that alone is
    /// too large is returned by payload reference, and the cursor moves past it.
    pub(super) fn view_page<T: Clone + serde::Serialize>(
        &self,
        page: &Page<'_>,
        all: &[T],
        label: &str,
        wrap: impl Fn(Vec<T>) -> ViewData,
        snap: &dyn Fn(ViewBody) -> ViewSnapshot,
    ) -> Handled {
        let start = page.offset.min(all.len());
        let candidates = &all[start..(start + page.limit).min(all.len())];
        let next = |pos: usize| {
            (pos < all.len()).then(|| {
                cursor::encode(
                    Kind::View,
                    page.source,
                    "",
                    Pos {
                        o: Some(pos as u64),
                        ..Pos::default()
                    },
                    Some(page.revision.get()),
                )
            })
        };
        let sizes: Vec<usize> = candidates.iter().map(budget::json_len).collect();
        let fitted = budget::fit(&sizes, page.budget, |n| {
            let next_cursor = next(start + n);
            self.ok(
                snap(ViewBody::Inline(wrap(candidates[..n].to_vec()))),
                ReplyMeta {
                    truncated: next_cursor.is_some(),
                    next_cursor,
                    ..ReplyMeta::default()
                },
            )
        })?;
        match fitted {
            budget::Fit::Items { reply, .. } => Ok(reply),
            budget::Fit::FirstTooLarge => {
                let bytes = serde_json::to_vec(&candidates[0]).unwrap_or_default();
                let size = bytes.len();
                let payload = self.payloads.hold(
                    Some(format!("view:{}@{}#{start}", page.source, page.revision)),
                    bytes,
                );
                let summary = format!(
                    "{label} {start} is {size} bytes, above the {}-byte reply budget; read it with \
                     `mira payload read <meta.payload.token>` and continue with next_cursor",
                    page.budget
                );
                self.ok(
                    snap(ViewBody::Reference(ReferenceData {
                        representation: ReferenceTag::Reference,
                        summary,
                    })),
                    ReplyMeta {
                        truncated: true,
                        next_cursor: next(start + 1),
                        not_modified: false,
                        payload: Some(payload),
                    },
                )
            }
        }
    }

    pub(super) fn view_publish(&mut self, p: ViewPublishParams, r: Responder) {
        match self.prepare_publish(&p) {
            Ok((op, data, claim)) => {
                self.views.queue.push_back(ViewUpdate {
                    view_ref: p.view_ref,
                    op,
                    data,
                    source_run_id: None,
                    source_kind: p.source_kind,
                    publish: Some(PublishReq {
                        responder: r,
                        expected: p.expected_view_revision,
                        claim,
                    }),
                });
                self.process_views();
            }
            Err(e) => r.send(self.fail(e)),
        }
    }

    fn prepare_publish(
        &self,
        p: &ViewPublishParams,
    ) -> Result<(ViewOp, ViewData, Option<KeyClaim>), ErrorInfo> {
        let set = self.accepted()?;
        let (_, def) = set
            .view(&p.view_ref)
            .ok_or_else(|| ErrorInfo::item_not_found("view", &p.view_ref))?;
        if let Some(src) = &def.source {
            return Err(verr(
                ErrorCode::INVALID_ARGUMENT,
                format!(
                    "`{}` is derived from the logs of `{}`; it cannot be published to",
                    p.view_ref, src.logs
                ),
            )
            .with_next_action(
                &["mira", "view", &p.view_ref.to_string()],
                "Read the derived view instead.",
            ));
        }
        let frame_value = Value::Object(p.frame.clone());
        let event = wire_from_value::<PluginFrame>(frame_value.clone())
            .and_then(PluginFrame::validate)
            .map_err(|i| i.to_error_info())?;
        let PluginEvent::View { view_id, op, data } = event else {
            return Err(verr(
                ErrorCode::INVALID_ARGUMENT,
                "publish input must be one MPP `view` frame",
            ));
        };
        if view_id != p.view_ref.view {
            return Err(verr(
                ErrorCode::INVALID_ARGUMENT,
                format!(
                    "frame.view_id `{view_id}` does not match the local ID of `{}`",
                    p.view_ref
                ),
            ));
        }
        if data.kind() != def.kind {
            return Err(verr(
                ErrorCode::SCHEMA_INVALID,
                format!(
                    "view `{}` is a {:?} view; the frame carries {:?} data",
                    p.view_ref,
                    def.kind,
                    data.kind()
                )
                .to_lowercase(),
            ));
        }
        if def.persistence == Persistence::Session && self.session.is_none() {
            return Err(verr(
                ErrorCode::SESSION_REQUIRED,
                format!(
                    "`{}` is a session view; publishing it needs an active session",
                    p.view_ref
                ),
            )
            .with_next_action(
                &["mira", "up", "--background"],
                "Start a session explicitly, or publish to a `last` view.",
            ));
        }
        let storage = self.storage.as_ref().map_err(|e| e.to_error_info())?;
        let claim = p.request_key.clone().map(|key| KeyClaim {
            scope: KeyScope::Publish(p.view_ref.clone()),
            key,
            fingerprint: storage.fingerprint(&json!({
                "frame": frame_value,
                "definition_hash": def.definition_hash,
                "expected_view_revision": p.expected_view_revision,
            })),
        });
        Ok((op, data, claim))
    }

    /// A table row action (§6.5): binds input from the row at `expected_view_revision`, then
    /// invokes the action on the same path as `action.invoke`. Any drift is VIEW_CHANGED.
    pub(super) fn view_action(&mut self, client: &ClientId, p: ViewActionParams, r: Responder) {
        let prepared = (|| {
            let set = self.accepted()?;
            let (_, def) = set
                .view(&p.view_ref)
                .ok_or_else(|| ErrorInfo::item_not_found("view", &p.view_ref))?;
            let ra = def
                .row_actions
                .iter()
                .find(|ra| ra.action == p.action)
                .ok_or_else(|| {
                    verr(
                        ErrorCode::NOT_FOUND,
                        format!("view `{}` has no row action `{}`", p.view_ref, p.action),
                    )
                })?;
            let changed = |msg: String| {
                verr(ErrorCode::VIEW_CHANGED, msg).with_next_action(
                    &["mira", "view", &p.view_ref.to_string()],
                    "Read the current view, then retry with its view_revision.",
                )
            };
            let e = self
                .views
                .entries
                .get(&p.view_ref)
                .ok_or_else(|| changed("the view has no data".into()))?;
            if e.revision != p.expected_view_revision {
                return Err(changed(format!(
                    "the view is at revision {}, not {}; nothing was run",
                    e.revision, p.expected_view_revision
                )));
            }
            if e.definition_hash != def.definition_hash {
                return Err(changed(
                    "the view or its bound action definition changed; nothing was run".into(),
                ));
            }
            let ViewData::Table { rows, .. } = &e.data else {
                return Err(changed("the view holds no table".into()));
            };
            let row = rows
                .iter()
                .find(|row| row.id == p.row)
                .ok_or_else(|| changed(format!("row `{}` no longer exists", p.row)))?;
            let mut input = serde_json::Map::new();
            for (param, col) in &ra.bindings {
                match row.values.get(col) {
                    Some(Value::Null) | None => {}
                    Some(v) => {
                        input.insert(param.clone(), v.clone());
                    }
                }
            }
            Ok(ActionInvokeParams {
                action_ref: ActionRef::new(p.view_ref.plugin.clone(), ra.action.clone()),
                input,
                client_env: p.client_env.clone(),
                request_key: None,
                foreground: true,
            })
        })();
        match prepared {
            Ok(invoke) => self.invoke_internal(client, invoke, Some(r)),
            Err(e) => r.send(self.fail(e)),
        }
    }
}
