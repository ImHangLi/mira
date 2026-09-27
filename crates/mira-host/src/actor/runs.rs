//! Run lifecycle: validate → reserve durably →
//! spawn → observe → stop → finalize and commit. The actor owns every transition.

use std::collections::BTreeMap;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mira_protocol::error::{ErrorCode, ErrorInfo};
use mira_protocol::hash::canonical_digest;
use mira_protocol::ids::*;
use mira_protocol::ipc::*;
use mira_protocol::manifest::{
    Action, ActionMode, Argv, Runner, StopSignal, TerminalMode, TimeoutPolicy,
};
use mira_protocol::reply::ReplyMeta;
use mira_protocol::run::*;
use mira_protocol::time::Timestamp;
use serde_json::{Map, Value, json};

use super::cursor::{self, Kind, Pos};
use super::plugin::{self, MppRun};
use super::{Actor, MAX_LIMIT, Msg, RECENT_RUNS, Responder, budget, reads, reply_fail, reply_ok};
use crate::diag;
use crate::env::{self, HostVars};
use crate::logs::{RunLog, SharedLog};
use crate::plugin_runner::Protocol;
use crate::runner::{self, CommandSpec, RunnerEvent, StopKind};
use crate::storage::{Claim, KeyClaim, KeyScope, RunFilter};

const DEFAULT_RUNS: usize = 20;
const DEFAULT_LOGS: usize = 100;

pub enum Reservation {
    New,
    Same { reference: RunId },
}

/// Everything needed to start the child once the reservation is committed.
pub struct Launch {
    argv: Vec<String>,
    cwd: String,
    env_files: Vec<String>,
    action_env: BTreeMap<String, String>,
    client_env: ClientEnv,
    timeout: TimeoutPolicy,
    stop_signal: StopSignal,
    grace: Duration,
    cleanup: Option<Vec<String>>,
    terminal: TerminalMode,
    input: Map<String, Value>,
    config: Map<String, Value>,
    plugin: Option<(PluginId, AbsolutePath)>,
    /// MPP/1 plugin runs: the local action ID and mode for the invocation.
    protocol: Option<(ActionId, ActionMode)>,
}

pub enum Phase {
    Reserving {
        waiters: Vec<Responder>,
        launch: Box<Launch>,
    },
    Live,
    Finalizing,
}

pub struct ActiveRun {
    pub record: RunRecord,
    pub fingerprint: Digest,
    pub request_key: Option<RequestKey>,
    pub log: SharedLog,
    pub stop_tx: Option<tokio::sync::mpsc::Sender<StopKind>>,
    pub phase: Phase,
    pub temp_controller: Option<ClientId>,
    pub requested_stop: Option<StopReason>,
    /// MPP/1 state; `None` for command runs.
    pub mpp: Option<Box<MppRun>>,
}

/// A validated invocation, ready to reserve.
struct Prepared {
    source: Option<RunSource>,
    action_ref: Option<ActionRef>,
    label: String,
    mode: ActionMode,
    definition_hash: Digest,
    fingerprint: Digest,
    request_key: Option<RequestKey>,
    scope: KeyScope,
    foreground: bool,
    cleanup_configured: bool,
    launch: Launch,
    mpp: Option<MppRun>,
}

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

fn write_private_json(path: &Path, v: &Value) -> Result<(), String> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| e.to_string())?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| e.to_string())?;
    f.write_all(v.to_string().as_bytes())
        .map_err(|e| e.to_string())
}

impl Actor {
    fn fail_opt(&self, r: Option<Responder>, e: ErrorInfo) {
        if let Some(r) = r {
            r.send(self.fail(e));
        }
    }

    fn validator(&mut self, action: &Action) -> Result<Arc<jsonschema::Validator>, ErrorInfo> {
        if let Some(v) = self.validators.get(&action.definition_hash) {
            return Ok(v.clone());
        }
        let v = Arc::new(
            action
                .input_schema
                .0
                .compile()
                .map_err(|e| ErrorInfo::new(ErrorCode::SCHEMA_INVALID, e))?,
        );
        self.validators
            .insert(action.definition_hash.clone(), v.clone());
        Ok(v)
    }

    pub(super) fn invoke(&mut self, client: &ClientId, p: ActionInvokeParams, r: Responder) {
        self.invoke_internal(client, p, Some(r));
    }

    pub(crate) fn invoke_internal(
        &mut self,
        client: &ClientId,
        p: ActionInvokeParams,
        r: Option<Responder>,
    ) {
        match self.prepare_action(&p) {
            Ok(prepared) => self.start(client, prepared, r),
            Err(e) => self.fail_opt(r, e),
        }
    }

    /// Invocations started by the host itself (schedules, autostart) rather than a client.
    pub(crate) fn invoke_from(&mut self, source: RunSource, p: ActionInvokeParams) {
        match self.prepare_action(&p) {
            Ok(mut prepared) => {
                prepared.source = Some(source);
                self.start(&ClientId::random(), prepared, None);
            }
            Err(e) => diag(format!(
                "{source:?} invocation of {} refused: {}",
                p.action_ref, e.message
            )),
        }
    }

    fn prepare_action(&mut self, p: &ActionInvokeParams) -> Result<Prepared, ErrorInfo> {
        let set = self.accepted()?;
        let (lp, action) = set
            .action(&p.action_ref)
            .ok_or_else(|| ErrorInfo::item_not_found("action", &p.action_ref))?;
        self.check_blocked(&lp.plugin.id)?;
        if !lp.plugin.enabled {
            return Err(ErrorInfo::new(
                ErrorCode::INVALID_ARGUMENT,
                format!("plugin `{}` is disabled", lp.plugin.id),
            ));
        }
        let (argv, protocol, mpp) = match &action.run {
            Runner::Command { argv } => (argv.as_slice().to_vec(), None, None),
            Runner::Plugin => {
                let entry = lp.plugin.entry.as_ref().ok_or_else(|| {
                    ErrorInfo::new(
                        ErrorCode::EXECUTION_FAILED,
                        format!("plugin `{}` declares no entry to run", lp.plugin.id),
                    )
                })?;
                (
                    entry.as_slice().to_vec(),
                    Some((action.id.clone(), action.mode)),
                    Some(MppRun::new(
                        action.mode,
                        lp.plugin.id.clone(),
                        action.output_schema.as_ref().map(|s| s.0.clone()),
                    )),
                )
            }
        };
        let schema = &action.input_schema.0;
        let effective = schema.effective_input(&p.input);
        let validator = self.validator(action)?;
        let issues = schema.validate(&validator, &Value::Object(effective.clone()), "/input");
        if !issues.is_empty() {
            return Err(issues.to_error_info());
        }
        // `{input.NAME}` placeholders in a command argv (the plugin entry is never templated).
        let argv = match &action.run {
            Runner::Command { .. } => {
                let rendered = mira_protocol::template::render_argv(&argv, &effective)
                    .map_err(|i| i.to_error_info())?;
                let mut issues = mira_protocol::Issues::default();
                Argv::parse(rendered, "/input", &mut issues)
                    .ok_or_else(|| issues.to_error_info())?
                    .as_slice()
                    .to_vec()
            }
            Runner::Plugin => argv,
        };
        let storage = self.storage.as_ref().map_err(|e| e.to_error_info())?;
        let fingerprint = storage
            .fingerprint(&json!({"input": effective, "definition_hash": action.definition_hash}));
        Ok(Prepared {
            source: None,
            action_ref: Some(p.action_ref.clone()),
            label: action.title.clone(),
            mode: action.mode,
            definition_hash: action.definition_hash.clone(),
            fingerprint,
            request_key: p.request_key.clone(),
            scope: KeyScope::Action(p.action_ref.clone()),
            foreground: p.foreground,
            cleanup_configured: action.cleanup.is_some(),
            launch: Launch {
                argv,
                cwd: action.cwd.clone(),
                env_files: action.env_files.clone(),
                action_env: action.env.clone(),
                client_env: p.client_env.clone(),
                timeout: action.timeout,
                stop_signal: action.stop_signal,
                grace: Duration::from_millis(action.stop_grace_ms),
                cleanup: action.cleanup.as_ref().map(|c| c.as_slice().to_vec()),
                terminal: action.terminal,
                input: effective,
                config: lp.plugin.config.clone(),
                plugin: Some((lp.plugin.id.clone(), lp.dir.clone())),
                protocol,
            },
            mpp,
        })
    }

    pub(super) fn exec(&mut self, client: &ClientId, p: ActionExecParams, r: Responder) {
        let prepared = (|| {
            if p.label.is_empty() || p.label.len() > mira_protocol::limits::MAX_NAME_BYTES {
                return Err(ErrorInfo::new(
                    ErrorCode::INVALID_ARGUMENT,
                    "--label must be 1-128 bytes",
                ));
            }
            let mut issues = mira_protocol::Issues::default();
            let argv = Argv::parse(p.argv.clone(), "/argv", &mut issues)
                .ok_or_else(|| issues.to_error_info())?;
            let storage = self.storage.as_ref().map_err(|e| e.to_error_info())?;
            let definition_hash = canonical_digest(&json!({"exec": argv.as_slice()}))
                .map_err(|e| ErrorInfo::new(ErrorCode::INTERNAL, e))?;
            let fingerprint = storage.fingerprint(&json!({"exec": argv.as_slice()}));
            Ok(Prepared {
                source: None,
                action_ref: None,
                label: p.label.clone(),
                mode: ActionMode::Task,
                definition_hash,
                fingerprint,
                request_key: p.request_key.clone(),
                scope: KeyScope::Exec,
                foreground: p.foreground,
                cleanup_configured: false,
                launch: Launch {
                    argv: argv.as_slice().to_vec(),
                    cwd: ".".into(),
                    env_files: vec![],
                    action_env: BTreeMap::new(),
                    client_env: p.client_env.clone(),
                    timeout: TimeoutPolicy::After(Duration::from_millis(
                        mira_protocol::limits::DEFAULT_TASK_TIMEOUT_MS,
                    )),
                    stop_signal: StopSignal::Term,
                    grace: Duration::from_millis(mira_protocol::limits::DEFAULT_STOP_GRACE_MS),
                    cleanup: None,
                    terminal: TerminalMode::Pipe,
                    input: Map::new(),
                    config: Map::new(),
                    plugin: None,
                    protocol: None,
                },
                mpp: None,
            })
        })();
        match prepared {
            Ok(prep) => self.start(client, prep, Some(r)),
            Err(e) => r.send(self.fail(e)),
        }
    }

    /// Singleton, session, and reservation rules shared by actions and exec.
    fn start(&mut self, client: &ClientId, prep: Prepared, r: Option<Responder>) {
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

    fn release_temp_controller(&mut self, client: &ClientId) {
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

    pub(super) fn reserved(&mut self, run_id: RunId, result: Result<Reservation, ErrorInfo>) {
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

    fn launch(&mut self, run_id: RunId) {
        let Some(run) = self.runs.get_mut(&run_id) else {
            return;
        };
        let Phase::Reserving { waiters, launch } = std::mem::replace(&mut run.phase, Phase::Live)
        else {
            return;
        };
        let state = run.record.lifecycle;
        let reply = super::reply_ok(
            super::ReplyContext {
                workspace: Some(mira_protocol::reply::WorkspaceRef {
                    id: self.paths.id.clone(),
                    root: self.paths.root.clone(),
                }),
                host_epoch: Some(self.epoch.clone()),
                catalog_revision: Some(self.catalog_revision),
                state_revision: Some(self.state_revision),
            },
            InvokeAccepted {
                run_id: run_id.clone(),
                state,
                reused: false,
            },
            ReplyMeta::default(),
        );
        for w in waiters {
            w.send(reply.clone());
        }
        if let Some(reason) = run.requested_stop {
            // Stopped before it could start: the committed reservation still gets a final record.
            let _ = reason;
            self.finalize(&run_id, None, None, CleanupState::NotNeeded);
            return;
        }
        let root = self.paths.root.as_path().to_path_buf();
        let (state_dir, cache_dir, plugin_dir) = match &launch.plugin {
            Some((id, dir)) => (
                self.paths.plugin_state(id),
                self.paths.plugin_cache(id),
                Some(dir.to_string()),
            ),
            None => (
                self.paths.state_dir.join("exec"),
                self.paths.cache_dir.join("exec"),
                None,
            ),
        };
        let artifact_dir = self.paths.artifacts(&run_id);
        let inputs = self.paths.temp_inputs();
        let input_file = inputs.join(format!("{run_id}.input.json"));
        let config_file = inputs.join(format!("{run_id}.config.json"));
        let action_cwd: PathBuf = if launch.cwd.starts_with('/') {
            PathBuf::from(&launch.cwd)
        } else {
            root.join(&launch.cwd)
        }
        .components()
        .filter(|c| !matches!(c, std::path::Component::CurDir))
        .collect();
        let prepared = (|| -> Result<(env::ChildEnv, Option<Protocol>), ErrorInfo> {
            for d in [&state_dir, &cache_dir, &artifact_dir] {
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(d)
                    .map_err(|e| {
                        ErrorInfo::new(
                            ErrorCode::STORAGE_UNAVAILABLE,
                            format!("cannot create {}: {e}", d.display()),
                        )
                    })?;
            }
            write_private_json(&input_file, &Value::Object(launch.input.clone())).map_err(|e| {
                ErrorInfo::new(
                    ErrorCode::STORAGE_UNAVAILABLE,
                    format!("cannot write input file: {e}"),
                )
            })?;
            write_private_json(&config_file, &Value::Object(launch.config.clone())).map_err(
                |e| {
                    ErrorInfo::new(
                        ErrorCode::STORAGE_UNAVAILABLE,
                        format!("cannot write config file: {e}"),
                    )
                },
            )?;
            let host = HostVars {
                workspace_root: self.paths.root.to_string(),
                plugin_dir,
                state_dir: state_dir.to_string_lossy().into_owned(),
                cache_dir: cache_dir.to_string_lossy().into_owned(),
                artifact_dir: artifact_dir.to_string_lossy().into_owned(),
                run_id: run_id.to_string(),
                input_file: input_file.to_string_lossy().into_owned(),
                config_file: config_file.to_string_lossy().into_owned(),
            };
            let env = env::compose(
                &launch.client_env,
                &root,
                &launch.env_files,
                &launch.action_env,
                &host,
            )?;
            let protocol = match (&launch.protocol, &launch.plugin) {
                (Some((action, mode)), Some((_, dir))) => {
                    let abs = |p: &Path| {
                        AbsolutePath::from_path(p).map_err(|e| {
                            ErrorInfo::new(
                                ErrorCode::EXECUTION_FAILED,
                                format!("path {} is not usable: {e}", p.display()),
                            )
                        })
                    };
                    let invocation = mira_protocol::mpp::Invocation {
                        api: Api1,
                        run_id: run_id.clone(),
                        action: action.clone(),
                        input: launch.input.clone(),
                        config: launch.config.clone(),
                        context: mira_protocol::mpp::InvocationContext {
                            workspace_id: self.paths.id.clone(),
                            workspace_root: self.paths.root.clone(),
                            cwd: abs(&action_cwd)?,
                            plugin_dir: dir.clone(),
                            state_dir: abs(&state_dir)?,
                            cache_dir: abs(&cache_dir)?,
                            artifact_dir: abs(&artifact_dir)?,
                        },
                    };
                    let mut line = serde_json::to_vec(&invocation)
                        .map_err(|e| ErrorInfo::new(ErrorCode::INTERNAL, e.to_string()))?;
                    line.push(b'\n');
                    Some(Protocol {
                        invocation_line: line,
                        mode: *mode,
                    })
                }
                _ => None,
            };
            Ok((env, protocol))
        })();
        let (env, protocol) = match prepared {
            Ok(v) => v,
            Err(e) => {
                let _ = std::fs::remove_file(&input_file);
                let _ = std::fs::remove_file(&config_file);
                if let Ok(mut log) = run.log.lock() {
                    log.push_line(
                        LogStream::Host,
                        mira_protocol::view::LogLevel::Error,
                        &e.message,
                        false,
                    );
                    log.flush();
                }
                self.finalize(&run_id, None, Some(e), CleanupState::NotNeeded);
                return;
            }
        };
        // Plugins run in their plugin dir; `context.cwd` tells them the action cwd.
        let cwd = match (&protocol, &launch.plugin) {
            (Some(_), Some((_, dir))) => dir.as_path().to_path_buf(),
            _ => action_cwd.clone(),
        };
        if let Some(mpp) = run.mpp.as_mut() {
            mpp.cwd = action_cwd.clone();
            mpp.artifact_dir = artifact_dir.clone();
        }
        let spec = CommandSpec {
            run_id: run_id.clone(),
            argv: launch.argv,
            cwd,
            cleanup_cwd: action_cwd,
            env,
            timeout: launch.timeout,
            stop_signal: launch.stop_signal,
            grace: launch.grace,
            cleanup: launch.cleanup,
            temp_files: vec![input_file, config_file],
            protocol,
        };
        let (stop_tx, stop_rx) = tokio::sync::mpsc::channel(4);
        run.stop_tx = Some(stop_tx);
        match launch.terminal {
            TerminalMode::Pipe => {
                tokio::spawn(runner::supervise(
                    spec,
                    run.log.clone(),
                    self.runner_tx.clone(),
                    stop_rx,
                ));
            }
            TerminalMode::Pty => {
                let log = run.log.clone();
                if let Some(h) = crate::pty::start(spec, log, self.runner_tx.clone(), stop_rx) {
                    self.pty_started(run_id, h);
                }
            }
        }
    }

    fn save_async(&self, record: RunRecord) {
        if let Ok(storage) = self.storage.clone() {
            tokio::spawn(async move {
                if let Err(e) = storage.save_run(record).await {
                    diag(format!("could not save run state: {e}"));
                }
            });
        }
    }

    pub(super) fn runner_event(&mut self, run_id: RunId, ev: RunnerEvent) {
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

    fn finalize(
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

    pub(super) fn final_saved(&mut self, run_id: RunId, error: Option<String>) {
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

    pub(super) fn run_stop(&mut self, p: RunStopParams, r: Responder) {
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

    fn known_record(&self, run_id: &RunId) -> Option<RunRecord> {
        self.runs.get(run_id).map(|r| r.record.clone()).or_else(|| {
            self.recent
                .iter()
                .find(|(r, _)| &r.run_id == run_id)
                .map(|(r, _)| r.clone())
        })
    }

    pub(super) fn run_get(&mut self, p: RunGetParams, r: Responder) {
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

    pub(super) fn run_list(&mut self, p: RunListParams, r: Responder) {
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

    pub(super) fn log_read(&mut self, p: LogReadParams, r: Responder) {
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
