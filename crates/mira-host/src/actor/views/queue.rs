//! The ordered update queue: revision blocks, request-key claims, and applying updates.

use mira_protocol::error::{ErrorCode, ErrorInfo};
use mira_protocol::ids::*;
use mira_protocol::ipc::*;
use mira_protocol::manifest::Persistence;
use mira_protocol::reply::ReplyMeta;
use mira_protocol::time::Timestamp;
use mira_protocol::view::*;

use crate::actor::{Actor, Msg};
use crate::storage::{Claim, StorageError};

use super::ViewEntry;
use super::content::build;
use super::{ViewUpdate, Wait};

impl Actor {
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
                    ErrorInfo::new(ErrorCode::INVALID_FRAME, msg),
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

    pub(in crate::actor) fn view_block(&mut self, result: Result<(u64, u64), StorageError>) {
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
    pub(super) fn process_views(&mut self) {
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
                    let e =
                        ErrorInfo::new(ErrorCode::STORAGE_UNAVAILABLE, "request keys need storage");
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

    pub(in crate::actor) fn view_claimed(&mut self, result: Result<Claim, StorageError>) {
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
                        None => self.fail(ErrorInfo::new(
                            ErrorCode::OUTCOME_UNKNOWN,
                            "the request key refers to an unreadable earlier publish",
                        )),
                    };
                    p.responder.send(reply);
                }
            }
            Ok(Claim::Conflict) => self.fail_update(
                upd,
                ErrorInfo::new(
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
            return self.fail_update(
                upd,
                ErrorInfo::new(ErrorCode::NOT_FOUND, "the view is gone"),
            );
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
                // Success only after the commit.
                self.views
                    .commit_waiters
                    .push((upd.view_ref.clone(), rev, p.responder, res));
                self.save_view_now(&upd.view_ref);
            }
        }
    }
}
