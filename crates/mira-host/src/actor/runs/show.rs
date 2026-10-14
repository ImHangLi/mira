//! `view.show`: a view shown once in the TUI, without a plugin. It becomes a finished
//! one-off run: no process runs, and the result holds the view data. Clients see it in the
//! state event while its record is saved, like a short `exec`.

use std::sync::{Arc, Mutex};

use mira_protocol::error::{ErrorCode, ErrorInfo};
use mira_protocol::hash::canonical_digest;
use mira_protocol::ids::RunId;
use mira_protocol::ipc::{ClientKind, InvokeAccepted, ViewShowParams};
use mira_protocol::limits::{MAX_NAME_BYTES, MAX_SHOWN_VIEW_BYTES};
use mira_protocol::reply::ReplyMeta;
use mira_protocol::run::*;
use mira_protocol::time::Timestamp;
use serde_json::json;

use crate::actor::{Actor, Msg, Responder};
use crate::logs::RunLog;

use super::{ActiveRun, Phase};

impl Actor {
    pub(in crate::actor) fn view_show(
        &mut self,
        client: &mira_protocol::ids::ClientId,
        p: ViewShowParams,
        r: Responder,
    ) {
        let checked = (|| {
            if p.title.trim().is_empty() || p.title.len() > MAX_NAME_BYTES {
                return Err(ErrorInfo::new(
                    ErrorCode::INVALID_ARGUMENT,
                    "--title must be 1-128 bytes",
                ));
            }
            if let Some(who) = &p.requester {
                who.check()
                    .map_err(|m| ErrorInfo::new(ErrorCode::INVALID_ARGUMENT, m))?;
            }
            let issues = p.data.validate("/data");
            if !issues.0.is_empty() {
                return Err(issues.to_error_info());
            }
            let data = serde_json::to_value(&p.data)
                .map_err(|e| ErrorInfo::new(ErrorCode::INTERNAL, e.to_string()))?;
            let size = serde_json::to_vec(&data).map_or(usize::MAX, |b| b.len());
            if size > MAX_SHOWN_VIEW_BYTES {
                return Err(ErrorInfo::new(
                    ErrorCode::INVALID_ARGUMENT,
                    format!("the view is {size} bytes; `mira show` takes at most 512 KiB"),
                ));
            }
            let hash = canonical_digest(&json!({"show": data}))
                .map_err(|e| ErrorInfo::new(ErrorCode::INTERNAL, e))?;
            let storage = self.storage.clone().map_err(|e| e.to_error_info())?;
            Ok((data, hash, storage))
        })();
        let (data, definition_hash, storage) = match checked {
            Ok(v) => v,
            Err(e) => return r.send(self.fail(e)),
        };
        let run_id = RunId::random();
        let now = Timestamp::now();
        let record = RunRecord {
            run_id: run_id.clone(),
            workspace_id: self.paths.id.clone(),
            session_id: self.session.as_ref().map(|s| s.id.clone()),
            action_ref: None,
            label: p.title.trim().to_owned(),
            definition_hash,
            catalog_revision: self.catalog_revision,
            source: match self.client_kind(client) {
                ClientKind::Tui => RunSource::Tui,
                ClientKind::Cli => RunSource::Cli,
                ClientKind::Hook => RunSource::Hook,
            },
            started_at: now,
            ended_at: Some(now),
            lifecycle: Lifecycle::Finished {
                outcome: Outcome::Succeeded,
            },
            reported_health: ReportedHealth::unknown(),
            exit: None,
            stop_reason: None,
            cleanup: CleanupState::NotNeeded,
            result: Some(RunResult {
                ok: true,
                summary: format!("shown: {}", p.title.trim()),
                data,
                payload: None,
                error: None,
            }),
            log: LogLocation {
                first_seq: None,
                last_seq: None,
                dropped_records: 0,
                truncated_records: 0,
            },
            git: None,
            note: None,
            provenance: None,
            requester: p.requester.clone(),
            view_kind: Some(p.data.kind()),
        };
        let log = Arc::new(Mutex::new(RunLog::create(self.paths.run_logs(&run_id), 0)));
        // Listed as active until the record is saved, so every open TUI picks it up.
        self.runs.insert(
            run_id.clone(),
            ActiveRun {
                record: record.clone(),
                fingerprint: record.definition_hash.clone(),
                request_key: None,
                log,
                stop_tx: None,
                phase: Phase::Finalizing,
                temp_controller: None,
                requested_stop: None,
                mpp: None,
            },
        );
        self.state_changed();
        r.send(self.ok(
            InvokeAccepted {
                run_id: run_id.clone(),
                state: record.lifecycle,
                reused: false,
            },
            ReplyMeta::default(),
        ));
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let error = storage
                .insert_run(record)
                .await
                .err()
                .map(|e| e.to_string());
            let _ = tx.send(Msg::FinalSaved { run_id, error }).await;
        });
    }
}
