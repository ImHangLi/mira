//! Runner events, stopping, and finalizing a run into its committed record.

use mira_protocol::error::{ErrorCode, ErrorInfo};
use mira_protocol::ids::*;
use mira_protocol::ipc::*;
use mira_protocol::reply::ReplyMeta;
use mira_protocol::run::*;
use mira_protocol::time::Timestamp;

use crate::actor::plugin;
use crate::actor::{Actor, Msg, RECENT_RUNS, Responder, reply_fail, reply_ok};
use crate::diag;
use crate::runner::{RunnerEvent, StopKind};

use super::Phase;

fn outcome_for(stop: Option<StopReason>, exit: &Option<ExitInfo>, spawn_failed: bool) -> Outcome {
    match stop {
        Some(StopReason::Timeout) => Outcome::TimedOut,
        Some(StopReason::User | StopReason::SessionClosed | StopReason::TtlExpired) => {
            Outcome::Cancelled
        }
        Some(StopReason::ProtocolError | StopReason::OutputLimit | StopReason::InternalError) => {
            Outcome::Failed
        }
        None if spawn_failed => Outcome::Failed,
        None => match exit {
            Some(ExitInfo { code: Some(0), .. }) => Outcome::Succeeded,
            _ => Outcome::Failed,
        },
    }
}

impl Actor {
    fn save_async(&self, record: RunRecord) {
        if let Ok(storage) = self.storage.clone() {
            tokio::spawn(async move {
                if let Err(e) = storage.save_run(record).await {
                    diag(format!("could not save run state: {e}"));
                }
            });
        }
    }

    pub(in crate::actor) fn runner_event(&mut self, run_id: RunId, ev: RunnerEvent) {
        match ev {
            RunnerEvent::Spawned => {
                let Some(run) = self.runs.get_mut(&run_id) else {
                    return;
                };
                if run.record.lifecycle == Lifecycle::Starting {
                    run.record.lifecycle = Lifecycle::Running;
                }
                let record = run.record.clone();
                self.save_async(record);
                self.state_changed();
            }
            RunnerEvent::TimedOut => {
                let Some(run) = self.runs.get_mut(&run_id) else {
                    return;
                };
                if !matches!(run.record.lifecycle, Lifecycle::Stopping { .. }) {
                    run.record.lifecycle = Lifecycle::Stopping {
                        reason: StopReason::Timeout,
                    };
                    run.record.stop_reason = Some(StopReason::Timeout);
                    run.requested_stop = Some(StopReason::Timeout);
                    self.state_changed();
                }
            }
            RunnerEvent::Logs { records } => {
                if let Some(run) = self.runs.get_mut(&run_id)
                    && let (Some(first), Some(last)) = (records.first(), records.last())
                {
                    run.record.log.first_seq.get_or_insert(first.log_seq);
                    run.record.log.last_seq = Some(last.log_seq);
                }
                self.derived_logs(&run_id, &records);
                self.broadcast_logs(&run_id, records);
            }
            RunnerEvent::LogGap {
                dropped_records,
                first_seq,
                last_seq,
            } => {
                self.derived_gap(&run_id);
                self.broadcast_log_gap(&run_id, dropped_records, first_seq, last_seq)
            }
            RunnerEvent::Frame(ev) => self.plugin_event(run_id, *ev),
            RunnerEvent::ProtocolError { error, view_hint } => {
                self.protocol_error(&run_id, error, view_hint)
            }
            RunnerEvent::Terminal(ev) => self.terminal_event(&run_id, ev),
            RunnerEvent::Finished(f) => self.finalize(&run_id, f.exit, f.spawn_error, f.cleanup),
        }
    }

    pub(super) fn finalize(
        &mut self,
        run_id: &RunId,
        exit: Option<ExitInfo>,
        spawn_error: Option<ErrorInfo>,
        cleanup: CleanupState,
    ) {
        let Some(run) = self.runs.get_mut(run_id) else {
            return;
        };
        let spawn_failed = spawn_error.is_some() || exit.is_none();
        let mut outcome = outcome_for(run.requested_stop, &exit, spawn_failed);
        let mut stop_reason = run.requested_stop;
        let mut verdict_note = None;
        // Plugin tasks: stop/timeout and spawn failures keep precedence.
        if run.requested_stop.is_none()
            && !spawn_failed
            && let Some(v) = run
                .mpp
                .as_deref()
                .and_then(|l| plugin::verdict(l, run.record.result.as_ref(), &exit))
        {
            outcome = v.outcome;
            stop_reason = stop_reason.or(v.stop_reason);
            verdict_note = v.note;
        }
        let r = &mut run.record;
        r.lifecycle = Lifecycle::Finished { outcome };
        r.ended_at = Some(Timestamp::now());
        r.exit = exit;
        r.stop_reason = stop_reason;
        r.cleanup = cleanup;
        if let Some(e) = spawn_error {
            plugin::append_note(&mut r.note, &e.message);
        }
        if let Some(n) = verdict_note {
            plugin::append_note(&mut r.note, &n);
        }
        if let Ok(log) = run.log.lock() {
            r.log.first_seq = log.first_available();
            r.log.last_seq = log.last_available();
            r.log.dropped_records = log.dropped;
            r.log.truncated_records = log.truncated;
        }
        run.phase = Phase::Finalizing;
        run.stop_tx = None;
        let record = run.record.clone();
        let failed = outcome != Outcome::Succeeded;
        if run.mpp.is_some() {
            self.views_run_ended(run_id, failed);
        }
        let tx = self.tx.clone();
        let run_id = run_id.clone();
        match self.storage.clone() {
            Ok(storage) => {
                tokio::spawn(async move {
                    let error = storage.save_run(record).await.err().map(|e| e.to_string());
                    let _ = tx.send(Msg::FinalSaved { run_id, error }).await;
                });
            }
            Err(e) => {
                let msg = e.to_string();
                tokio::spawn(async move {
                    let _ = tx
                        .send(Msg::FinalSaved {
                            run_id,
                            error: Some(msg),
                        })
                        .await;
                });
            }
        }
    }

    pub(in crate::actor) fn final_saved(&mut self, run_id: RunId, error: Option<String>) {
        let Some(mut run) = self.runs.remove(&run_id) else {
            return;
        };
        if let Some(a) = &run.record.action_ref
            && self.by_action.get(a) == Some(&run_id)
        {
            self.by_action.remove(a);
        }
        if let Some(e) = error {
            let note = format!(
                "execution finished, but the final record was not confirmed in storage: {e}"
            );
            run.record.note = Some(match run.record.note.take() {
                Some(n) => format!("{n}; {note}"),
                None => note,
            });
            self.storage_warning(&crate::storage::StorageError::Unavailable(e));
        }
        self.recent
            .push_front((run.record.clone(), run.log.clone()));
        self.recent.truncate(RECENT_RUNS);
        if let Some(a) = &run.record.action_ref {
            self.derived_run_ended(a, &run_id);
        }
        if let Some(c) = &run.temp_controller {
            self.release_temp_controller(c);
        }
        self.end_session_if_idle();
        self.state_changed();
    }

    /// Moves one run to `stopping`; other runs and actions are unaffected.
    pub(crate) fn stop_run(&mut self, run_id: &RunId, reason: StopReason) -> Option<Lifecycle> {
        let run = self.runs.get_mut(run_id)?;
        if !run.record.lifecycle.is_active()
            || matches!(run.record.lifecycle, Lifecycle::Stopping { .. })
        {
            return Some(run.record.lifecycle);
        }
        run.record.lifecycle = Lifecycle::Stopping { reason };
        run.record.stop_reason = Some(reason);
        run.requested_stop = Some(reason);
        let kind = match reason {
            StopReason::SessionClosed | StopReason::TtlExpired => StopKind::SessionClosed,
            StopReason::ProtocolError | StopReason::OutputLimit | StopReason::InternalError => {
                StopKind::Failed
            }
            StopReason::User | StopReason::Timeout => StopKind::Cancelled,
        };
        if let Some(tx) = &run.stop_tx {
            let _ = tx.try_send(kind);
        }
        Some(run.record.lifecycle)
    }

    pub(in crate::actor) fn run_stop(&mut self, p: RunStopParams, r: Responder) {
        let run_id = match &p.target {
            RunTarget::Run { run_id } => run_id.clone(),
            RunTarget::Action { action_ref } => match self.by_action.get(action_ref) {
                Some(id) => id.clone(),
                None => {
                    return r.send(self.fail(ErrorInfo::new(
                        ErrorCode::NOT_FOUND,
                        format!("`{action_ref}` has no active run"),
                    )));
                }
            },
        };
        if let Some(state) = self.stop_run(&run_id, StopReason::User) {
            self.state_changed();
            return r.send(self.ok(StopAccepted { run_id, state }, ReplyMeta::default()));
        }
        if let Some((rec, _)) = self.recent.iter().find(|(rec, _)| rec.run_id == run_id) {
            return r.send(self.ok(
                StopAccepted {
                    run_id,
                    state: rec.lifecycle,
                },
                ReplyMeta::default(),
            ));
        }
        let ctx = self.ctx();
        let storage = self.storage.clone();
        tokio::spawn(async move {
            let found = match storage {
                Ok(s) => s.get_run(run_id.clone()).await.ok().flatten(),
                Err(_) => None,
            };
            r.send(match found {
                Some(rec) => reply_ok(
                    ctx,
                    StopAccepted {
                        run_id,
                        state: rec.lifecycle,
                    },
                    ReplyMeta::default(),
                ),
                None => reply_fail(ctx, ErrorInfo::run_not_found(&run_id)),
            });
        });
    }
}
