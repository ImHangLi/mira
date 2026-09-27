//! Read methods over runs: `run.get`, `run.list`, and `log.read`.

use std::sync::{Arc, Mutex};

use mira_protocol::error::{ErrorCode, ErrorInfo};
use mira_protocol::ids::*;
use mira_protocol::ipc::*;
use mira_protocol::reply::ReplyMeta;
use mira_protocol::run::*;
use mira_protocol::time::Timestamp;
use serde_json::json;

use crate::actor::cursor::{self, Kind, Pos};
use crate::actor::{Actor, MAX_LIMIT, Responder, budget, reads, reply_fail, reply_ok};
use crate::logs::{RunLog, SharedLog};
use crate::storage::RunFilter;

const DEFAULT_RUNS: usize = 20;
const DEFAULT_LOGS: usize = 100;

impl Actor {
    fn known_record(&self, run_id: &RunId) -> Option<RunRecord> {
        self.runs.get(run_id).map(|r| r.record.clone()).or_else(|| {
            self.recent
                .iter()
                .find(|(r, _)| &r.run_id == run_id)
                .map(|(r, _)| r.clone())
        })
    }

    pub(in crate::actor) fn run_get(&mut self, p: RunGetParams, r: Responder) {
        let set = self.accepted().ok();
        let payloads = self.payloads.clone();
        if let Some(mut rec) = self.known_record(&p.run_id) {
            reads::prepare_run(&mut rec, set.as_deref(), &payloads);
            return r.send(self.ok(rec, ReplyMeta::default()));
        }
        let ctx = self.ctx();
        let storage = self.storage.clone();
        tokio::spawn(async move {
            let reply = match storage {
                Ok(s) => match s.get_run(p.run_id.clone()).await {
                    Ok(Some(mut rec)) => {
                        reads::prepare_run(&mut rec, set.as_deref(), &payloads);
                        reply_ok(ctx, rec, ReplyMeta::default())
                    }
                    Ok(None) => reply_fail(ctx, ErrorInfo::run_not_found(&p.run_id)),
                    Err(e) => reply_fail(ctx, e.to_error_info()),
                },
                Err(e) => reply_fail(ctx, e.to_error_info()),
            };
            r.send(reply);
        });
    }

    pub(in crate::actor) fn run_list(&mut self, p: RunListParams, r: Responder) {
        let limit = p
            .limit
            .map_or(DEFAULT_RUNS, |l| (l as usize).clamp(1, MAX_LIMIT));
        let budget = budget::budget(p.max_bytes);
        let source = self.paths.id.to_string();
        let filter =
            cursor::filter_hash(&json!({"action_ref": p.action_ref, "outcome": p.outcome}));
        let before = match p
            .cursor
            .as_deref()
            .map(|c| cursor::decode(c, Kind::Runs, &source, &filter))
            .transpose()
        {
            Ok(None) => None,
            Ok(Some(c)) => match (c.pos.t, c.pos.id.map(RunId::parse)) {
                (Some(t), Some(Ok(id))) => Some((Timestamp::from_unix_ms(t), id)),
                _ => {
                    return r.send(self.fail(ErrorInfo::new(
                        ErrorCode::INVALID_ARGUMENT,
                        "invalid cursor: no run position",
                    )));
                }
            },
            Err(e) => return r.send(self.fail(e)),
        };
        let live: Vec<RunRecord> = self.runs.values().map(|r| r.record.clone()).collect();
        let ctx = self.ctx();
        let set = self.accepted().ok();
        let payloads = self.payloads.clone();
        let Ok(storage) = self.storage.clone() else {
            return r.send(self.fail(ErrorInfo::new(
                ErrorCode::STORAGE_UNAVAILABLE,
                "run history is unavailable",
            )));
        };
        let query = RunFilter {
            action_ref: p.action_ref,
            outcome: p.outcome,
            before,
            limit: limit + 1,
        };
        tokio::spawn(async move {
            let reply = match storage.list_runs(query).await {
                Ok(mut runs) => {
                    for rec in &mut runs {
                        if let Some(l) = live.iter().find(|l| l.run_id == rec.run_id) {
                            *rec = l.clone();
                        }
                        reads::prepare_run(rec, set.as_deref(), &payloads);
                    }
                    let more_stored = runs.len() > limit;
                    runs.truncate(limit);
                    let after = |rec: &RunRecord| {
                        cursor::encode(
                            Kind::Runs,
                            &source,
                            &filter,
                            Pos {
                                t: Some(rec.started_at.unix_ms()),
                                id: Some(rec.run_id.to_string()),
                                ..Pos::default()
                            },
                            None,
                        )
                    };
                    let sizes: Vec<usize> = runs.iter().map(budget::json_len).collect();
                    let fitted = budget::fit(&sizes, budget, |n| {
                        let more = n < runs.len() || more_stored;
                        reply_ok(
                            ctx.clone(),
                            RunList {
                                runs: runs[..n].to_vec(),
                            },
                            ReplyMeta {
                                truncated: more,
                                next_cursor: if more {
                                    runs[..n].last().map(after)
                                } else {
                                    None
                                },
                                ..ReplyMeta::default()
                            },
                        )
                    });
                    match fitted {
                        Ok(budget::Fit::Items { reply, .. }) => Ok(reply),
                        Ok(budget::Fit::FirstTooLarge) => {
                            // One record alone exceeds the budget: reference it, move past it.
                            let first = &runs[0];
                            let payload = payloads.hold(
                                Some(format!("run-record:{}", first.run_id)),
                                serde_json::to_vec(first).unwrap_or_default(),
                            );
                            reply_ok(
                                ctx,
                                RunList { runs: vec![] },
                                ReplyMeta {
                                    truncated: true,
                                    next_cursor: (runs.len() > 1 || more_stored)
                                        .then(|| after(first)),
                                    not_modified: false,
                                    payload: Some(payload),
                                },
                            )
                        }
                        Err(e) => Err(e),
                    }
                }
                Err(e) => reply_fail(ctx, e.to_error_info()),
            };
            r.send(reply);
        });
    }

    fn log_for(&self, run_id: &RunId) -> Option<SharedLog> {
        self.runs.get(run_id).map(|r| r.log.clone()).or_else(|| {
            self.recent
                .iter()
                .find(|(r, _)| &r.run_id == run_id)
                .map(|(_, l)| l.clone())
        })
    }

    pub(in crate::actor) fn log_read(&mut self, p: LogReadParams, r: Responder) {
        let limit = p
            .limit
            .map_or(DEFAULT_LOGS, |l| (l as usize).clamp(1, MAX_LIMIT));
        let ctx = self.ctx();
        let payloads = self.payloads.clone();
        let logs_dir = self.paths.logs_dir.join("runs");
        // Resolve the run: explicit run, the action's active run, or its most recent run.
        let (run_id, log) = match &p.target {
            RunTarget::Run { run_id } => (Some(run_id.clone()), self.log_for(run_id)),
            RunTarget::Action { action_ref } => match self.by_action.get(action_ref) {
                Some(id) => (Some(id.clone()), self.log_for(id)),
                None => match self
                    .recent
                    .iter()
                    .find(|(rec, _)| rec.action_ref.as_ref() == Some(action_ref))
                {
                    Some((rec, l)) => (Some(rec.run_id.clone()), Some(l.clone())),
                    None => (None, None),
                },
            },
        };
        let action_ref = match &p.target {
            RunTarget::Action { action_ref } => Some(action_ref.clone()),
            RunTarget::Run { .. } => None,
        };
        let storage = self.storage.clone();
        let record_store = self.storage.clone();
        tokio::spawn(async move {
            let run_id = match run_id {
                Some(id) => Some(id),
                None => match (storage, action_ref) {
                    (Ok(s), Some(a)) => s
                        .list_runs(RunFilter {
                            action_ref: Some(a),
                            limit: 1,
                            ..RunFilter::default()
                        })
                        .await
                        .ok()
                        .and_then(|v| v.into_iter().next())
                        .map(|rec| rec.run_id),
                    _ => None,
                },
            };
            let Some(run_id) = run_id else {
                return r.send(reply_fail(
                    ctx,
                    ErrorInfo::new(ErrorCode::NOT_FOUND, "no run found for this target"),
                ));
            };
            // Logs removed by retention must not read as an empty success.
            if log.is_none()
                && !logs_dir.join(run_id.as_str()).exists()
                && let Ok(storage) = &record_store
                && let Ok(Some(rec)) = storage.get_run(run_id.clone()).await
                && rec.log.last_seq.is_some()
            {
                return r.send(reply_fail(
                                ctx,
                                ErrorInfo::new(
                                    ErrorCode::PAYLOAD_GONE,
                                    format!("the logs of {run_id} were removed by retention; the run summary is kept"),
                                ),
                            ));
            }
            let log = log.unwrap_or_else(|| {
                Arc::new(Mutex::new(RunLog::open_existing(
                    logs_dir.join(run_id.as_str()),
                )))
            });
            let reply = tokio::task::spawn_blocking(move || {
                let Ok(mut log) = log.lock() else {
                    return reply_fail(ctx, ErrorInfo::new(ErrorCode::INTERNAL, "log unavailable"));
                };
                reads::log_page(
                    &mut log,
                    run_id,
                    p.cursor.as_deref(),
                    limit,
                    p.max_bytes,
                    ctx,
                    &payloads,
                )
            })
            .await
            .unwrap_or_else(|e| Err(RpcError::new(RpcError::INTERNAL_ERROR, e.to_string())));
            r.send(reply);
        });
    }
}
