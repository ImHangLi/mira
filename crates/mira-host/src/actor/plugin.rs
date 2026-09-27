//! MPP/1 run facts in the actor: results, reported health, progress, views, and
//! artifacts from validated frames, plus the task result precedence rules.

use std::collections::BTreeSet;
use std::path::PathBuf;

use mira_protocol::error::{ErrorCode, ErrorInfo};
use mira_protocol::hash::canonical_digest;
use mira_protocol::ids::{PluginId, RunId, ViewId, ViewRef};
use mira_protocol::manifest::ActionMode;
use mira_protocol::mpp::{PluginEvent, PluginResult};
use mira_protocol::run::{ExitInfo, LogStream, Outcome, ReportedHealth, RunResult, StopReason};
use mira_protocol::schema_profile::SchemaDoc;
use mira_protocol::time::Timestamp;
use mira_protocol::view::{LogLevel, SourceKind};
use serde_json::{Value, json};

use super::{Actor, reads};

/// Per-run MPP/1 state. Present only for `run.kind = plugin` actions.
pub struct MppRun {
    pub mode: ActionMode,
    pub plugin: PluginId,
    pub output_schema: Option<SchemaDoc>,
    /// The resolved `context.cwd`; relative artifact paths resolve against it.
    pub cwd: PathBuf,
    pub artifact_dir: PathBuf,
    pub protocol_error: Option<ErrorInfo>,
    /// Views this run updated; they turn stale when the run fails.
    pub touched_views: BTreeSet<ViewRef>,
    /// Latest unsent progress (coalesced, sent on the next tick).
    pub progress: Option<(String, Option<(f64, f64)>)>,
    pub artifacts: u32,
}

impl MppRun {
    pub fn new(mode: ActionMode, plugin: PluginId, output_schema: Option<SchemaDoc>) -> Self {
        Self {
            mode,
            plugin,
            output_schema,
            cwd: PathBuf::new(),
            artifact_dir: PathBuf::new(),
            protocol_error: None,
            touched_views: BTreeSet::new(),
            progress: None,
            artifacts: 0,
        }
    }
}

/// The final outcome of a plugin run when no stop request or spawn failure decides it.
pub struct Verdict {
    pub outcome: Outcome,
    pub stop_reason: Option<StopReason>,
    pub note: Option<String>,
}

/// A task succeeds only with one valid success result as its last frame and exit 0.
pub fn verdict(
    mpp: &MppRun,
    result: Option<&RunResult>,
    exit: &Option<ExitInfo>,
) -> Option<Verdict> {
    if mpp.mode == ActionMode::Process {
        return None;
    }
    let exit0 = matches!(exit, Some(ExitInfo { code: Some(0), .. }));
    Some(match result {
        None => Verdict {
            outcome: Outcome::Failed,
            stop_reason: Some(StopReason::ProtocolError),
            note: Some("invalid plugin output: the task ended without a result frame".into()),
        },
        Some(r) if !r.ok => Verdict {
            outcome: Outcome::Failed,
            stop_reason: None,
            note: None,
        },
        Some(_) if exit0 => Verdict {
            outcome: Outcome::Succeeded,
            stop_reason: None,
            note: None,
        },
        Some(_) => Verdict {
            outcome: Outcome::Failed,
            stop_reason: None,
            note: Some(match exit {
                Some(ExitInfo { code: Some(c), .. }) => {
                    format!(
                        "the plugin reported success, then exited with status {c}; the run failed"
                    )
                }
                Some(ExitInfo {
                    signal: Some(s), ..
                }) => {
                    format!("the plugin reported success, then was ended by {s}; the run failed")
                }
                _ => "the plugin reported success, but its exit status is unknown; the run failed"
                    .into(),
            }),
        },
    })
}

pub fn append_note(note: &mut Option<String>, text: &str) {
    *note = Some(match note.take() {
        Some(n) => format!("{n}; {text}"),
        None => text.to_owned(),
    });
}

impl Actor {
    /// Writes one host line into a run's log and streams it.
    pub(crate) fn host_note(&mut self, run_id: &RunId, level: LogLevel, text: &str) {
        let records = match self.runs.get(run_id).map(|r| r.log.clone()) {
            Some(log) => match log.lock() {
                Ok(mut l) => l.push_line(LogStream::Host, level, text, false),
                Err(_) => return,
            },
            None => return,
        };
        self.broadcast_logs(run_id, records);
    }

    pub(super) fn plugin_event(&mut self, run_id: RunId, ev: PluginEvent) {
        let Some(run) = self.runs.get_mut(&run_id) else {
            return;
        };
        let Some(mpp) = run.mpp.as_mut() else {
            return;
        };
        if mpp.protocol_error.is_some() {
            return;
        }
        match ev {
            // The runner writes log frames to the run log directly.
            PluginEvent::Log { .. } => {}
            PluginEvent::Progress { message, current } => mpp.progress = Some((message, current)),
            PluginEvent::Status { state, message } => {
                run.record.reported_health = ReportedHealth {
                    state,
                    message: Some(message),
                    updated_at: Some(Timestamp::now()),
                };
                self.state_changed();
            }
            PluginEvent::Notify { title, message } => {
                // In the run log too, so agents and later readers see it.
                self.host_note(
                    &run_id,
                    LogLevel::Info,
                    &format!("notification: {title}: {message}"),
                );
                self.broadcast_notify(&run_id, title, message);
            }
            PluginEvent::View { view_id, op, data } => {
                let view_ref = ViewRef::new(mpp.plugin.clone(), view_id);
                self.enqueue_view_update(view_ref, op, data, Some(run_id), SourceKind::Plugin);
            }
            PluginEvent::Artifact {
                path,
                mime,
                label,
                ownership,
            } => {
                mpp.artifacts += 1;
                let (n, cwd, dir) = (mpp.artifacts, mpp.cwd.clone(), mpp.artifact_dir.clone());
                let registered = self
                    .artifacts
                    .register(&run_id, n, &cwd, &dir, &path, mime, label, ownership);
                if let Err(e) = registered {
                    self.protocol_error(&run_id, e, None);
                }
            }
            PluginEvent::Result(res) => self.plugin_result(&run_id, res),
        }
    }

    fn plugin_result(&mut self, run_id: &RunId, res: PluginResult) {
        let (ok, summary, data, error) = match res {
            PluginResult::Success { summary, data } => (true, summary, data, None),
            PluginResult::Failure {
                summary,
                data,
                error,
            } => (false, summary, data, Some(error)),
        };
        let schema = self
            .runs
            .get(run_id)
            .and_then(|r| r.mpp.as_ref())
            .and_then(|l| l.output_schema.clone());
        let mut schema_error = None;
        if ok && let Some(schema) = schema {
            match self.output_validator(&schema) {
                Ok(v) => {
                    let issues = schema.validate(&v, &data, "/data");
                    if !issues.is_empty() {
                        let info = issues.to_error_info();
                        schema_error = Some(ErrorInfo {
                            message: format!(
                                "the success result does not match output_schema: {}",
                                info.message
                            ),
                            ..info
                        });
                    }
                }
                Err(e) => schema_error = Some(e),
            }
        }
        if !self.runs.contains_key(run_id) {
            return;
        }
        // Large data is retained once by reference; the record itself stays small.
        let bytes = serde_json::to_vec(&data).unwrap_or_default();
        let (data, payload) = if bytes.len() > reads::INLINE_RESULT_BYTES {
            let cap = self
                .accepted()
                .map_or(64 * 1024 * 1024, |s| s.storage.result_bytes_per_workspace);
            let payload = self.payloads.retain_result(run_id, bytes, cap);
            (Value::Null, Some(payload))
        } else {
            (data, None)
        };
        let Some(run) = self.runs.get_mut(run_id) else {
            return;
        };
        run.record.result = Some(RunResult {
            ok,
            summary,
            data,
            payload,
            error,
        });
        if let Some(e) = schema_error {
            self.protocol_error(run_id, e, None);
        }
        self.state_changed();
    }

    fn output_validator(
        &mut self,
        schema: &SchemaDoc,
    ) -> Result<std::sync::Arc<jsonschema::Validator>, ErrorInfo> {
        let key = canonical_digest(&json!({"output_schema": schema.as_map()}))
            .map_err(|e| ErrorInfo::new(ErrorCode::INTERNAL, e))?;
        if let Some(v) = self.validators.get(&key) {
            return Ok(v.clone());
        }
        let v = std::sync::Arc::new(
            schema
                .compile()
                .map_err(|e| ErrorInfo::new(ErrorCode::SCHEMA_INVALID, e))?,
        );
        self.validators.insert(key, v.clone());
        Ok(v)
    }

    /// Stops only this run; its earlier valid views stay readable and turn stale.
    pub(crate) fn protocol_error(
        &mut self,
        run_id: &RunId,
        error: ErrorInfo,
        view_hint: Option<ViewId>,
    ) {
        let Some(run) = self.runs.get_mut(run_id) else {
            return;
        };
        let Some(mpp) = run.mpp.as_mut() else {
            return;
        };
        if mpp.protocol_error.is_some() {
            return;
        }
        let text = match &view_hint {
            Some(v) => format!("invalid plugin output in view `{v}`: {}", error.message),
            None => format!("invalid plugin output: {}", error.message),
        };
        mpp.protocol_error = Some(error);
        mpp.progress = None;
        let plugin = mpp.plugin.clone();
        append_note(&mut run.record.note, &text);
        // The failed output cannot be trusted to name its target: every view of the plugin
        // keeps its last valid content and turns stale.
        let reason = format!("run {run_id} of this plugin stopped on a protocol error");
        self.mark_plugin_views_stale(&plugin, &reason);
        self.stop_run(run_id, StopReason::ProtocolError);
        self.state_changed();
    }

    /// Sends the latest coalesced progress of each plugin run.
    pub(super) fn flush_progress(&mut self) {
        let pending: Vec<_> = self
            .runs
            .iter_mut()
            .filter_map(|(id, r)| {
                let p = r.mpp.as_mut()?.progress.take()?;
                Some((id.clone(), p))
            })
            .collect();
        for (run_id, (message, current)) in pending {
            self.broadcast_progress(&run_id, message, current);
        }
    }
}
