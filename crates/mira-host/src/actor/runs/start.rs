//! Starting a prepared run: singleton and session rules, then the durable reservation.

use std::path::Path;
use std::sync::{Arc, Mutex};

use mira_protocol::error::{ErrorCode, ErrorInfo};
use mira_protocol::ids::*;
use mira_protocol::ipc::*;
use mira_protocol::manifest::ActionMode;
use mira_protocol::reply::ReplyMeta;
use mira_protocol::run::*;
use mira_protocol::time::Timestamp;

use crate::actor::{Actor, Msg, Responder, reply_fail, reply_ok};
use crate::logs::RunLog;
use crate::storage::{Claim, KeyClaim};

use super::{ActiveRun, Phase, Prepared, Reservation};

fn source_of(kind: ClientKind) -> RunSource {
    match kind {
        ClientKind::Tui => RunSource::Tui,
        ClientKind::Cli => RunSource::Cli,
        ClientKind::Hook => RunSource::Hook,
    }
}

/// HEAD and branch of the workspace, read statically from `.git`; recorded as context only.
fn git_context(root: &Path) -> Option<GitContext> {
    let head = mira_protocol::workspace::git_head(root)?;
    Some(GitContext {
        branch: head.branch().map(str::to_owned),
        head: head.commit,
    })
}

impl Actor {
    /// Singleton, session, and reservation rules shared by actions and exec.
    pub(super) fn start(&mut self, client: &ClientId, prep: Prepared, r: Option<Responder>) {
        if let Some(action_ref) = &prep.action_ref
            && let Some(existing) = self
                .by_action
                .get(action_ref)
                .and_then(|id| self.runs.get(id))
        {
            let same = existing.record.definition_hash == prep.definition_hash
                && existing.fingerprint == prep.fingerprint;
            let answer = match prep.mode {
                ActionMode::Process if same => Ok(existing.record.run_id.clone()),
                ActionMode::Process => Err(ErrorInfo::new(
                    ErrorCode::ALREADY_RUNNING_DIFFERENT_INPUT,
                    format!("`{action_ref}` already runs with a different input or definition"),
                )
                .with_next_action(
                    &["mira", "restart", &action_ref.to_string()],
                    "Restart explicitly to apply the new input.",
                )),
                ActionMode::Task => match (&prep.request_key, &existing.request_key) {
                    (Some(k), Some(e)) if k == e && same => Ok(existing.record.run_id.clone()),
                    (Some(k), Some(e)) if k == e => Err(ErrorInfo::new(
                        ErrorCode::REQUEST_KEY_CONFLICT,
                        "request key reused with different input",
                    )),
                    _ => Err(ErrorInfo::new(
                        ErrorCode::BUSY,
                        format!(
                            "`{action_ref}` is already running as {}",
                            existing.record.run_id
                        ),
                    )
                    .retryable(true)),
                },
            };
            let state = existing.record.lifecycle;
            return match answer {
                Ok(run_id) => {
                    if let Some(r) = r {
                        r.send(self.ok(
                            InvokeAccepted {
                                run_id,
                                state,
                                reused: true,
                            },
                            ReplyMeta::default(),
                        ));
                    }
                }
                Err(e) => self.fail_opt(r, e),
            };
        }
        if self.session.as_ref().is_some_and(|s| s.stopping) {
            return self.fail_opt(
                r,
                ErrorInfo::new(
                    ErrorCode::BUSY,
                    "the session is stopping; retry when it has stopped",
                )
                .retryable(true),
            );
        }
        let waiting_task = prep.mode == ActionMode::Task && prep.foreground;
        if self.session.is_none() && !waiting_task {
            return self.fail_opt(
                r,
                ErrorInfo::new(
                    ErrorCode::SESSION_REQUIRED,
                    "no active session owns long-running or non-waiting work",
                )
                .with_next_action(
                    &["mira", "up", "--background", "--ttl", "2h"],
                    "Explicitly enable background execution.",
                ),
            );
        }
        let mut temp_controller = None;
        if prep.foreground {
            if let Err(e) = self.ensure_session(&prep.launch.client_env) {
                return self.fail_opt(r, e);
            }
            let is_controller = self
                .session
                .as_ref()
                .is_some_and(|s| s.controllers.contains(client));
            if !is_controller {
                self.add_temporary_controller(client);
                temp_controller = Some(client.clone());
            }
        }
        let Some(session_id) = self.session.as_ref().map(|s| s.id.clone()) else {
            return self.fail_opt(
                r,
                ErrorInfo::new(ErrorCode::INTERNAL, "session missing after creation"),
            );
        };
        let Ok(storage) = self.storage.clone() else {
            return self.fail_opt(
                r,
                ErrorInfo::new(
                    ErrorCode::STORAGE_UNAVAILABLE,
                    "run history storage is unavailable; nothing was started",
                ),
            );
        };
        let run_id = RunId::random();
        let record = RunRecord {
            run_id: run_id.clone(),
            workspace_id: self.paths.id.clone(),
            session_id: Some(session_id),
            action_ref: prep.action_ref.clone(),
            label: prep.label.clone(),
            definition_hash: prep.definition_hash.clone(),
            catalog_revision: self.catalog_revision,
            source: prep
                .source
                .unwrap_or_else(|| source_of(self.client_kind(client))),
            started_at: Timestamp::now(),
            ended_at: None,
            lifecycle: Lifecycle::Starting,
            reported_health: ReportedHealth::unknown(),
            exit: None,
            stop_reason: None,
            cleanup: if prep.cleanup_configured {
                CleanupState::Pending
            } else {
                CleanupState::NotNeeded
            },
            result: None,
            log: LogLocation {
                first_seq: None,
                last_seq: None,
                dropped_records: 0,
                truncated_records: 0,
            },
            git: git_context(self.paths.root.as_path()),
            note: None,
            provenance: None,
        };
        let cap = self
            .accepted()
            .map_or(8 * 1024 * 1024, |s| s.storage.log_bytes_per_run);
        let log = Arc::new(Mutex::new(RunLog::create(
            self.paths.run_logs(&run_id),
            cap,
        )));
        if let Some(a) = &prep.action_ref {
            self.by_action.insert(a.clone(), run_id.clone());
            self.derived_run_started(a, &run_id);
        }
        let claim = prep.request_key.clone().map(|key| KeyClaim {
            scope: prep.scope.clone(),
            key,
            fingerprint: prep.fingerprint.clone(),
        });
        self.runs.insert(
            run_id.clone(),
            ActiveRun {
                record: record.clone(),
                fingerprint: prep.fingerprint,
                request_key: prep.request_key,
                log,
                stop_tx: None,
                phase: Phase::Reserving {
                    waiters: r.into_iter().collect(),
                    launch: Box::new(prep.launch),
                },
                temp_controller,
                requested_stop: None,
                mpp: prep.mpp.map(Box::new),
            },
        );
        self.state_changed();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = async {
                if let Some(claim) = claim {
                    match storage
                        .claim_key(claim, run_id.to_string())
                        .await
                        .map_err(|e| e.to_error_info())?
                    {
                        Claim::New => {}
                        Claim::Same { reference } => {
                            let reference = RunId::parse(reference)
                                .map_err(|e| ErrorInfo::new(ErrorCode::INTERNAL, e.to_string()))?;
                            return Ok(Reservation::Same { reference });
                        }
                        Claim::Conflict => {
                            return Err(ErrorInfo::new(
                                ErrorCode::REQUEST_KEY_CONFLICT,
                                "request key was used with a different input or definition",
                            ));
                        }
                    }
                }
                storage.insert_run(record).await.map_err(|e| {
                    let mut info = e.to_error_info();
                    info.message = format!("{}; nothing was started", info.message);
                    info
                })?;
                Ok(Reservation::New)
            }
            .await;
            let _ = tx.send(Msg::Reserved { run_id, result }).await;
        });
    }

    fn drop_pending(&mut self, run_id: &RunId) -> Option<ActiveRun> {
        let run = self.runs.remove(run_id)?;
        if let Some(a) = &run.record.action_ref
            && self.by_action.get(a) == Some(run_id)
        {
            self.by_action.remove(a);
        }
        if let Some(c) = &run.temp_controller {
            self.release_temp_controller(c);
        }
        Some(run)
    }

    pub(super) fn release_temp_controller(&mut self, client: &ClientId) {
        let still_waiting = self
            .runs
            .values()
            .any(|r| r.temp_controller.as_ref() == Some(client));
        if !still_waiting {
            if let Some(s) = self.session.as_mut() {
                s.temporary.remove(client);
            }
            self.maybe_close_session();
        }
    }

    pub(in crate::actor) fn reserved(
        &mut self,
        run_id: RunId,
        result: Result<Reservation, ErrorInfo>,
    ) {
        match result {
            Err(e) => {
                if let Some(run) = self.drop_pending(&run_id)
                    && let Phase::Reserving { waiters, .. } = run.phase
                {
                    for w in waiters {
                        w.send(self.fail(e.clone()));
                    }
                }
                self.end_session_if_idle();
                self.state_changed();
            }
            Ok(Reservation::Same { reference }) => {
                let Some(run) = self.drop_pending(&run_id) else {
                    return;
                };
                let Phase::Reserving { waiters, .. } = run.phase else {
                    return;
                };
                let known = self
                    .runs
                    .get(&reference)
                    .map(|r| r.record.lifecycle)
                    .or_else(|| {
                        self.recent
                            .iter()
                            .find(|(r, _)| r.run_id == reference)
                            .map(|(r, _)| r.lifecycle)
                    });
                self.end_session_if_idle();
                self.state_changed();
                let ctx = self.ctx();
                let storage = self.storage.clone();
                tokio::spawn(async move {
                    let state = match (known, storage) {
                        (Some(l), _) => Some(l),
                        (None, Ok(s)) => s
                            .get_run(reference.clone())
                            .await
                            .ok()
                            .flatten()
                            .map(|r| r.lifecycle),
                        (None, Err(_)) => None,
                    };
                    let reply = match state {
                        Some(state) => reply_ok(
                            ctx,
                            InvokeAccepted {
                                run_id: reference,
                                state,
                                reused: true,
                            },
                            ReplyMeta::default(),
                        ),
                        None => reply_fail(
                            ctx,
                            ErrorInfo::new(
                                ErrorCode::OUTCOME_UNKNOWN,
                                format!(
                                    "request key maps to {reference}, whose record is no longer available"
                                ),
                            ),
                        ),
                    };
                    for w in waiters {
                        w.send(reply.clone());
                    }
                });
            }
            Ok(Reservation::New) => self.launch(run_id),
        }
    }
}
