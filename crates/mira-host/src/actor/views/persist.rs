//! Persistence of `last` views: loading at startup, the ordered writer, and coalescing.

use std::time::Duration;

use mira_protocol::error::{ErrorCode, ErrorInfo};
use mira_protocol::ids::*;
use mira_protocol::manifest::Persistence;
use mira_protocol::reply::ReplyMeta;
use mira_protocol::run::{Lifecycle, Outcome};
use mira_protocol::view::*;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::actor::{Actor, Msg};
use crate::diag;
use crate::storage::{StorageError, StoredView};

use super::ViewEntry;

const COALESCE: Duration = Duration::from_secs(1);

/// Stored ViewData JSON, host `recorded_at` on log items included.
fn decode_stored(json_text: &str) -> Result<ViewData, String> {
    serde_json::from_str(json_text).map_err(|e| e.to_string())
}

impl Actor {
    /// Retention removed these views: drop them from memory and remember the cleanup.
    pub(in crate::actor) fn views_cleaned(&mut self, views: Vec<(ViewRef, u64)>, at: i64) {
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
    pub(in crate::actor) async fn init_views(&mut self) {
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
                            // Data restored after a host restart is stale: it
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
    pub(in crate::actor) async fn flush_views_now(&mut self) {
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
}

impl Actor {
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

    pub(super) fn save_view_now(&mut self, view_ref: &ViewRef) {
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

    pub(in crate::actor) fn view_saved(
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
                Some(msg) => self.fail(ErrorInfo::new(
                    ErrorCode::STORAGE_UNAVAILABLE,
                    format!("the view was updated in memory but not committed: {msg}"),
                )),
            });
        }
    }

    /// Coalesced persistence of plugin-produced `last` views: at most once per second.
    pub(in crate::actor) fn views_tick(&mut self) {
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

    /// `session` views end with the session.
    pub(crate) fn clear_session_views(&mut self) {
        self.views
            .entries
            .retain(|_, e| e.persistence != Persistence::Session);
        // Session payloads (view bodies and oversized items by reference) end here too.
        self.payloads.clear_session();
    }
}
