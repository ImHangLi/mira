//! The workspace actor: the only writer of workspace state (§3.1, §9.4).
//!
//! Requests arrive as messages; storage commits and runner facts come back as messages.
//! The actor never awaits a child process, a file scan, or a storage commit inline.

mod artifacts;
mod budget;
mod configure;
mod cursor;
mod derived;
mod payloads;
mod plugin;
mod reads;
mod retention;
mod runs;
mod schedule;
mod search;
mod session;
mod streams;
mod terminal;
mod views;

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use mira_protocol::config::{self, ConfigSet};
use mira_protocol::error::{ErrorCode, ErrorInfo, Issues};
use mira_protocol::ids::*;
use mira_protocol::ipc::*;
use mira_protocol::manifest::{ActionMode, Runner};
use mira_protocol::paths::WorkspacePaths;
use mira_protocol::reply::{PublicReply, ReplyContext, ReplyMeta, WorkspaceRef};
use mira_protocol::run::RunRecord;
use mira_protocol::schemas;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::diag;
use crate::logs::SharedLog;
use crate::runner::RunnerEvent;
use crate::server::{error_line, result_line};
use crate::storage::{Claim, Storage, StorageError};

use runs::ActiveRun;
pub use runs::Reservation;
use session::Session;
use streams::Sub;

/// Observe-only hosts exit this long after the last client leaves with no session.
const IDLE_EXIT: Duration = Duration::from_secs(5);
const DEFAULT_CATALOG_LIMIT: usize = 30;
pub(crate) const MAX_LIMIT: usize = 1000;
const RECENT_RUNS: usize = 64;

/// Per-connection channels: control responses are never dropped; events are bounded.
#[derive(Clone)]
pub struct Outbound {
    responses: mpsc::UnboundedSender<Vec<u8>>,
    events: mpsc::Sender<Vec<u8>>,
}

impl Outbound {
    pub fn new(responses: mpsc::UnboundedSender<Vec<u8>>, events: mpsc::Sender<Vec<u8>>) -> Self {
        Self { responses, events }
    }
    pub fn respond(&self, line: Vec<u8>) {
        let _ = self.responses.send(line);
    }
    /// Returns false when the subscriber is too slow and must be reset.
    pub fn event(&self, line: Vec<u8>) -> bool {
        self.events.try_send(line).is_ok()
    }
}

/// Answers one request, now or after an asynchronous step.
pub struct Responder {
    out: Outbound,
    id: String,
}

impl Responder {
    pub fn send(self, result: Handled) {
        self.out.respond(match result {
            Ok(v) => result_line(self.id, v),
            Err(e) => error_line(Some(self.id), e),
        });
    }
}

pub type Handled = Result<Value, RpcError>;

pub enum Msg {
    Hello {
        params: HelloParams,
        out: Outbound,
        reply: oneshot::Sender<Result<(ClientId, HelloReply), RpcError>>,
    },
    Request {
        client: ClientId,
        id: String,
        method: Method,
        params: Map<String, Value>,
    },
    Closed {
        client: ClientId,
    },
    Reserved {
        run_id: RunId,
        result: Result<Reservation, ErrorInfo>,
    },
    FinalSaved {
        run_id: RunId,
        error: Option<String>,
    },
    ViewBlock {
        result: Result<(u64, u64), StorageError>,
    },
    ViewClaimed {
        result: Result<Claim, StorageError>,
    },
    ViewSaved {
        view_ref: ViewRef,
        revision: ViewRevision,
        error: Option<String>,
    },
    /// Retention removed these views and runs.
    ViewsCleaned {
        views: Vec<(ViewRef, u64)>,
        runs: Vec<RunId>,
        at: i64,
    },
    ScheduleSaved {
        params: ScheduleSetParams,
        saved: Result<(), ErrorInfo>,
        responder: Responder,
    },
    ConfigDone {
        result: Result<configure::Loaded, configure::LoadFailure>,
        responder: Option<Responder>,
        is_apply: bool,
        previous_blocked: configure::Blocked,
        previous_hash: Option<Digest>,
    },
    Shutdown,
}

struct ClientEntry {
    kind: ClientKind,
    out: Outbound,
}

/// Which definition set the actor runs from, and what is on disk.
enum ConfigState {
    NotSetup,
    Accepted {
        set: Arc<ConfigSet>,
        disk_issues: Option<Issues>,
    },
    /// Disk configuration is invalid and no set was accepted in this host lifetime.
    Invalid(Issues),
}

pub struct Actor {
    paths: WorkspacePaths,
    epoch: HostEpoch,
    rx: mpsc::Receiver<Msg>,
    tx: mpsc::Sender<Msg>,
    runner_rx: mpsc::Receiver<(RunId, RunnerEvent)>,
    runner_tx: mpsc::Sender<(RunId, RunnerEvent)>,
    storage: Result<Storage, StorageError>,
    config: ConfigState,
    catalog_revision: CatalogRevision,
    state_revision: StateRevision,
    clients: HashMap<ClientId, ClientEntry>,
    session: Option<Session>,
    runs: HashMap<RunId, ActiveRun>,
    by_action: HashMap<ActionRef, RunId>,
    recent: VecDeque<(RunRecord, SharedLog)>,
    validators: HashMap<Digest, Arc<jsonschema::Validator>>,
    subs: HashMap<SubscriptionId, Sub>,
    terminals: HashMap<RunId, terminal::TerminalEntry>,
    event_seq: EventSeq,
    storage_warnings: Vec<Warning>,
    idle_since: Option<Instant>,
    views: views::ViewStore,
    derived: derived::DerivedViews,
    artifacts: artifacts::Artifacts,
    payloads: payloads::Payloads,
    cfg: configure::ConfigCtl,
    schedules: schedule::Schedules,
    last_gc: Option<Instant>,
}

fn load_config(paths: &WorkspacePaths) -> ConfigState {
    if !paths.mira_dir.join(config::WORKSPACE_FILE).is_file() {
        return ConfigState::NotSetup;
    }
    let local = match config::load_local(&paths.mira_dir) {
        Ok(l) => l,
        Err(i) => return ConfigState::Invalid(i),
    };
    match config::load_config_dir(&paths.mira_dir, local.as_ref()) {
        Ok(set) => ConfigState::Accepted {
            set: Arc::new(set),
            disk_issues: None,
        },
        Err(i) => ConfigState::Invalid(i),
    }
}

pub(crate) fn params<P: DeserializeOwned>(p: Map<String, Value>) -> Result<P, RpcError> {
    serde_json::from_value(Value::Object(p))
        .map_err(|e| RpcError::new(RpcError::INVALID_PARAMS, e.to_string()))
}

impl Actor {
    pub fn new(paths: WorkspacePaths, rx: mpsc::Receiver<Msg>, tx: mpsc::Sender<Msg>) -> Self {
        let config = load_config(&paths);
        if let ConfigState::Invalid(i) = &config {
            diag(format!(
                "configuration on disk is invalid: {}",
                i.to_error_info().message
            ));
        }
        crate::groups::init(paths.state_dir.join("process-groups"));
        for (run, pid) in crate::groups::reap_leftovers() {
            diag(format!(
                "stopped process group {pid} that run {run} left running when the previous host stopped"
            ));
        }
        let mut storage_warnings = Vec::new();
        let storage = match Storage::open(&paths) {
            Ok((s, report)) => {
                if !report.interrupted.is_empty() {
                    diag(format!(
                        "{} run(s) were active when the previous host stopped; marked interrupted",
                        report.interrupted.len()
                    ));
                }
                Ok(s)
            }
            Err(e) => {
                diag(format!("storage unavailable: {e}"));
                storage_warnings.push(Warning {
                    code: e.to_error_info().code,
                    message: e.to_string(),
                    subject: None,
                });
                Err(e)
            }
        };
        let (runner_tx, runner_rx) = mpsc::channel(4096);
        let paths_for_cfg = paths.clone();
        Self {
            paths,
            epoch: HostEpoch::random(),
            rx,
            tx,
            runner_rx,
            runner_tx,
            storage,
            config,
            catalog_revision: CatalogRevision::ZERO,
            state_revision: StateRevision::ZERO,
            clients: HashMap::new(),
            session: None,
            runs: HashMap::new(),
            by_action: HashMap::new(),
            recent: VecDeque::new(),
            validators: HashMap::new(),
            subs: HashMap::new(),
            terminals: HashMap::new(),
            event_seq: EventSeq::ZERO,
            storage_warnings,
            idle_since: Some(Instant::now()),
            views: views::ViewStore::default(),
            derived: derived::DerivedViews::default(),
            artifacts: artifacts::Artifacts::default(),
            payloads: payloads::Payloads::new(paths_for_cfg.state_dir.join("results")),
            cfg: configure::ConfigCtl::new(&paths_for_cfg),
            schedules: schedule::Schedules::default(),
            last_gc: None,
        }
    }

    pub fn epoch(&self) -> &HostEpoch {
        &self.epoch
    }

    async fn init_catalog_revision(&mut self) {
        let hash = match &self.config {
            ConfigState::Accepted { set, .. } => Some(set.set_hash.clone()),
            _ => None,
        };
        let Ok(storage) = &self.storage else {
            self.catalog_revision =
                CatalogRevision::new(u64::from(hash.is_some())).unwrap_or_default();
            return;
        };
        let result = match hash {
            Some(h) => storage.accept_catalog(h).await,
            None => storage.catalog().await.map(|(r, _)| r),
        };
        match result {
            Ok(r) => self.catalog_revision = r,
            Err(e) => self.storage_warning(&e),
        }
    }

    pub(crate) fn storage_warning(&mut self, e: &StorageError) {
        diag(format!("storage: {e}"));
        let w = Warning {
            code: e.to_error_info().code,
            message: e.to_string(),
            subject: None,
        };
        if !self.storage_warnings.iter().any(|x| x.message == w.message) {
            self.storage_warnings.push(w);
            if self.storage_warnings.len() > 8 {
                self.storage_warnings.remove(0);
            }
        }
    }

    fn workspace_ref(&self) -> WorkspaceRef {
        WorkspaceRef {
            id: self.paths.id.clone(),
            root: self.paths.root.clone(),
        }
    }

    pub(crate) fn ctx(&self) -> ReplyContext {
        ReplyContext {
            workspace: Some(self.workspace_ref()),
            host_epoch: Some(self.epoch.clone()),
            catalog_revision: Some(self.catalog_revision),
            state_revision: Some(self.state_revision),
        }
    }

    pub(crate) fn ok<T: Serialize>(&self, data: T, meta: ReplyMeta) -> Handled {
        reply_ok(self.ctx(), data, meta)
    }

    pub(crate) fn fail(&self, error: ErrorInfo) -> Handled {
        reply_fail(self.ctx(), error)
    }

    fn busy(&self) -> bool {
        self.session.is_some() || !self.runs.is_empty()
    }

    pub async fn run(mut self) {
        self.init_catalog_revision().await;
        self.init_views().await;
        self.init_derived_views().await;
        self.load_schedules().await;
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        loop {
            tokio::select! {
                biased;
                msg = self.rx.recv() => match msg {
                    Some(Msg::Shutdown) | None => {
                        self.shutdown().await;
                        self.flush_views_now().await;
                        break;
                    }
                    Some(m) => self.handle(m),
                },
                Some((run_id, ev)) = self.runner_rx.recv() => self.runner_event(run_id, ev),
                Some(()) = self.cfg.fs_rx.recv() => self.fs_changed(),
                _ = tick.tick() => {
                    self.session_tick();
                    self.flush_progress();
                    self.views_tick();
                    self.config_tick();
                    self.schedule_tick();
                    self.retention_tick();
                    if self.idle_since.is_some_and(|t| t.elapsed() >= IDLE_EXIT) {
                        self.flush_views_now().await;
                        break;
                    }
                }
            }
            self.update_idle();
        }
    }

    /// Stops owned work on SIGTERM, bounded by each run's grace period.
    async fn shutdown(&mut self) {
        self.stop_session(mira_protocol::run::StopReason::SessionClosed);
        let deadline = Instant::now() + Duration::from_secs(15);
        while !self.runs.is_empty() && Instant::now() < deadline {
            tokio::select! {
                Some((run_id, ev)) = self.runner_rx.recv() => self.runner_event(run_id, ev),
                Some(m) = self.rx.recv() => {
                    if matches!(m, Msg::FinalSaved { .. } | Msg::ViewSaved { .. } | Msg::ViewBlock { .. } | Msg::ViewClaimed { .. }) {
                        self.handle(m)
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
        }
        // Whatever did not stop in time gets SIGKILL, so no child outlives the host.
        crate::groups::kill_remaining(self.runs.keys());
    }

    fn update_idle(&mut self) {
        let idle = self.clients.is_empty() && !self.busy();
        self.idle_since = match (idle, self.idle_since) {
            (true, Some(t)) => Some(t),
            (true, None) => Some(Instant::now()),
            (false, _) => None,
        };
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Hello { params, out, reply } => {
                let _ = reply.send(self.hello(params, out));
            }
            Msg::Request {
                client,
                id,
                method,
                params,
            } => {
                let Some(out) = self.clients.get(&client).map(|c| c.out.clone()) else {
                    return;
                };
                self.dispatch(&client, method, params, Responder { out, id });
            }
            Msg::Closed { client } => {
                self.clients.remove(&client);
                self.subs.retain(|_, s| s.client != client);
                self.release_terminal_locks(&client);
                self.controller_left(&client);
            }
            Msg::Reserved { run_id, result } => self.reserved(run_id, result),
            Msg::FinalSaved { run_id, error } => self.final_saved(run_id, error),
            Msg::ViewBlock { result } => self.view_block(result),
            Msg::ViewClaimed { result } => self.view_claimed(result),
            Msg::ViewSaved {
                view_ref,
                revision,
                error,
            } => self.view_saved(view_ref, revision, error),
            Msg::ViewsCleaned { views, runs, at } => {
                self.payloads.forget_runs(&runs);
                self.views_cleaned(views, at)
            }
            Msg::ScheduleSaved {
                params,
                saved,
                responder,
                ..
            } => self.schedule_saved(params, saved, responder),
            Msg::ConfigDone {
                result,
                responder,
                is_apply,
                previous_blocked,
                previous_hash,
            } => self.config_done(result, responder, is_apply, previous_blocked, previous_hash),
            Msg::Shutdown => {}
        }
    }

    fn hello(&mut self, p: HelloParams, out: Outbound) -> Result<(ClientId, HelloReply), RpcError> {
        if &p.protocol_hash != schemas::protocol_hash() {
            let pid = std::process::id().to_string();
            let msg = format!(
                "this host (pid {pid}) runs another Mira build (host {} {}, client {})",
                mira_protocol::VERSION,
                schemas::protocol_hash(),
                p.protocol_hash
            );
            let info = ErrorInfo::new(ErrorCode::PROTOCOL_MISMATCH, msg.clone()).with_next_action(
                &["kill", &pid],
                "Stop the host from the other build (or run `mira down` with that build); \
                 the next command starts this build.",
            );
            return Err(RpcError::new(RpcError::HANDSHAKE, msg).with_info(info));
        }
        if p.workspace_id != self.paths.id || p.workspace_root != self.paths.root {
            let msg = format!(
                "this host serves workspace {} at {}, not {} at {}",
                self.paths.id, self.paths.root, p.workspace_id, p.workspace_root
            );
            let info = ErrorInfo::new(ErrorCode::PROTOCOL_MISMATCH, msg.clone());
            return Err(RpcError::new(RpcError::HANDSHAKE, msg).with_info(info));
        }
        let id = ClientId::random();
        self.clients.insert(
            id.clone(),
            ClientEntry {
                kind: p.client_kind,
                out,
            },
        );
        Ok((
            id.clone(),
            HelloReply {
                api: Api1,
                protocol_hash: schemas::protocol_hash().clone(),
                host_epoch: self.epoch.clone(),
                client_id: id,
                workspace: self.workspace_ref(),
                catalog_revision: self.catalog_revision,
                state_revision: self.state_revision,
            },
        ))
    }

    pub(crate) fn client_kind(&self, client: &ClientId) -> ClientKind {
        self.clients.get(client).map_or(ClientKind::Cli, |c| c.kind)
    }

    fn dispatch(&mut self, client: &ClientId, method: Method, p: Map<String, Value>, r: Responder) {
        macro_rules! parse {
            ($p:expr) => {
                match params($p) {
                    Ok(v) => v,
                    Err(e) => return r.send(Err(e)),
                }
            };
        }
        match method {
            Method::WorkspaceStatus => {
                let _: Empty = parse!(p);
                r.send(self.ok(self.status_data(), ReplyMeta::default()))
            }
            Method::CatalogListM => {
                let res = self.catalog(parse!(p));
                r.send(res)
            }
            Method::ItemDescribe => {
                let res = self.describe(parse!(p));
                r.send(res)
            }
            Method::SessionAttach => self.session_attach(client, parse!(p), r),
            Method::SessionOpen => self.session_open(client, parse!(p), r),
            Method::SessionKeep => self.session_keep(parse!(p), r),
            Method::SessionStop => {
                let _: Empty = parse!(p);
                self.session_stop_request(r)
            }
            Method::ActionInvoke => self.invoke(client, parse!(p), r),
            Method::ActionExec => self.exec(client, parse!(p), r),
            Method::RunStop => self.run_stop(parse!(p), r),
            Method::RunGet => self.run_get(parse!(p), r),
            Method::RunListM => self.run_list(parse!(p), r),
            Method::LogRead => self.log_read(parse!(p), r),
            Method::ViewRead => self.view_read(parse!(p), r),
            Method::ViewPublish => self.view_publish(parse!(p), r),
            Method::ViewAction => self.view_action(client, parse!(p), r),
            Method::ArtifactList => self.artifact_list(parse!(p), r),
            Method::ArtifactRead => self.artifact_read(parse!(p), r),
            Method::StorageStatus => self.storage_status(parse!(p), r),
            Method::StorageGc => self.storage_gc(parse!(p), Some(r)),
            Method::StorageClear => self.storage_clear(parse!(p), r),
            Method::ScheduleSet => self.schedule_set(client, parse!(p), r),
            Method::PayloadRead => self.payload_read(parse!(p), r),
            Method::ConfigApply => self.config_apply(parse!(p), r),
            Method::ConfigReload => {
                let _: Empty = parse!(p);
                self.config_reload(Some(r))
            }
            Method::TerminalSnapshotM => self.terminal_snapshot(parse!(p), r),
            Method::TerminalAcquire => self.terminal_acquire(client, parse!(p), r),
            Method::TerminalRelease => self.terminal_release(client, parse!(p), r),
            Method::TerminalInputM => self.terminal_input(client, parse!(p), r),
            Method::TerminalResize => self.terminal_resize(client, parse!(p), r),
            Method::StreamSubscribe => self.subscribe(client, parse!(p), r),
            other => r.send(Err(RpcError::new(
                RpcError::METHOD_NOT_FOUND,
                format!("`{}` is not available in this build", other.name()),
            ))),
        }
    }

    fn config_warnings(&self) -> Vec<Warning> {
        match &self.config {
            ConfigState::NotSetup => vec![],
            ConfigState::Accepted {
                disk_issues: None, ..
            } => vec![],
            ConfigState::Accepted {
                disk_issues: Some(i),
                ..
            }
            | ConfigState::Invalid(i) => {
                let info = i.to_error_info();
                vec![Warning {
                    code: ErrorCode::SCHEMA_INVALID,
                    message: format!("configuration on disk is not in effect: {}", info.message),
                    subject: None,
                }]
            }
        }
    }

    pub(crate) fn status_data(&self) -> StatusData {
        let mut runs: Vec<_> = self
            .runs
            .values()
            .map(|r| mira_protocol::run::RunSummary::from(&r.record))
            .collect();
        runs.sort_by(|a, b| {
            a.started_at
                .cmp(&b.started_at)
                .then(a.run_id.cmp(&b.run_id))
        });
        let mut storage_warnings = self.storage_warnings.clone();
        for r in self.runs.values() {
            // Never wait for a log a flooding runner holds: status must stay fast (§9.3). A
            // busy log is checked again on the next status or state event.
            if let Ok(log) = r.log.try_lock()
                && let Some(e) = &log.write_error
            {
                storage_warnings.push(Warning {
                    code: ErrorCode::STORAGE_UNAVAILABLE,
                    message: format!(
                        "log writes failed ({} record(s) not saved): {e}",
                        log.dropped
                    ),
                    subject: Some(r.record.run_id.to_string()),
                });
            }
        }
        let mut config_warnings = self.config_warnings();
        config_warnings.extend(self.retiring_warnings());
        StatusData {
            session: self.session_info(),
            runs,
            storage_warnings,
            config_warnings,
            schedules: self.schedule_data(),
        }
    }

    pub(crate) fn accepted(&self) -> Result<Arc<ConfigSet>, ErrorInfo> {
        match &self.config {
            ConfigState::Accepted { set, .. } => Ok(set.clone()),
            ConfigState::NotSetup => Err(ErrorInfo::new(
                ErrorCode::NOT_SETUP,
                "this workspace has no .mira/workspace.json yet",
            )
            .with_next_action(
                &["mira", "validate", ".mira"],
                "Write .mira/workspace.json and a plugin in .mira/plugins/ (mira skill, setup \
                 reference), validate them, then run `mira reload`.",
            )),
            ConfigState::Invalid(i) => Err(i.to_error_info()),
        }
    }

    fn catalog(&self, p: CatalogListParams) -> Handled {
        let set = match self.accepted() {
            Ok(s) => s,
            Err(e) => return self.fail(e),
        };
        let words: Vec<String> = p
            .query
            .as_deref()
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        let source = self.paths.id.to_string();
        let filter = cursor::filter_hash(&serde_json::json!({ "query": words }));
        let revision = self.catalog_revision;
        // A cached catalog is valid only for the same workspace, query, and revision (§17.1).
        let same_workspace = p.if_workspace.as_ref().is_none_or(|w| w == &self.paths.id);
        if p.cursor.is_none() && same_workspace && p.if_revision == Some(revision) {
            let meta = ReplyMeta {
                not_modified: true,
                ..ReplyMeta::default()
            };
            return self.ok(CatalogList { items: vec![] }, meta);
        }
        let offset = match p
            .cursor
            .as_deref()
            .map(|c| cursor::decode(c, cursor::Kind::Catalog, &source, &filter))
            .transpose()
        {
            Ok(None) => 0,
            Ok(Some(c)) if c.revision != Some(revision.get()) => {
                let mut argv = vec!["mira", "catalog"];
                let query = p.query.as_deref().unwrap_or_default();
                if !words.is_empty() {
                    argv.extend(["--search", query]);
                }
                return self.fail(
                    ErrorInfo::new(
                        ErrorCode::REVISION_CONFLICT,
                        format!(
                            "the catalog changed to revision {revision} after this cursor was issued; \
                             earlier pages may be out of date"
                        ),
                    )
                    .with_next_action(&argv, "Read the catalog again from the first page."),
                );
            }
            Ok(Some(c)) => c.pos.o.unwrap_or(0) as usize,
            Err(e) => return self.fail(e),
        };
        let limit = p
            .limit
            .map_or(DEFAULT_CATALOG_LIMIT, |l| (l as usize).clamp(1, MAX_LIMIT));
        let budget = budget::budget(p.max_bytes);
        let items: Vec<CatalogItem> = search::search(set.catalog(), &words);
        let start = offset.min(items.len());
        let candidates = &items[start..(start + limit).min(items.len())];
        let next = |taken: usize| {
            (start + taken < items.len()).then(|| {
                cursor::encode(
                    cursor::Kind::Catalog,
                    &source,
                    &filter,
                    cursor::Pos {
                        o: Some((start + taken) as u64),
                        ..cursor::Pos::default()
                    },
                    Some(revision.get()),
                )
            })
        };
        let sizes: Vec<usize> = candidates.iter().map(budget::json_len).collect();
        let fitted = budget::fit(&sizes, budget, |n| {
            let next_cursor = next(n);
            self.ok(
                CatalogList {
                    items: candidates[..n].to_vec(),
                },
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
                let first = &candidates[0];
                let payload = self.payloads.hold(
                    Some(format!("catalog:{revision}:{}", first.item_ref)),
                    serde_json::to_vec(first).unwrap_or_default(),
                );
                self.ok(
                    CatalogList { items: vec![] },
                    ReplyMeta {
                        truncated: true,
                        next_cursor: next(1),
                        not_modified: false,
                        payload: Some(payload),
                    },
                )
            }
        }
    }

    fn describe(&self, p: ItemDescribeParams) -> Handled {
        let set = match self.accepted() {
            Ok(s) => s,
            Err(e) => return self.fail(e),
        };
        let not_found = || ErrorInfo::item_not_found("catalog item", &p.item_ref);
        let Some(lp) = set.plugin(&p.item_ref.plugin) else {
            return self.fail(not_found());
        };
        let Some(item) = set.catalog().into_iter().find(|i| i.item_ref == p.item_ref) else {
            return self.fail(not_found());
        };
        let plugin = &lp.plugin;
        let summary = PluginSummary {
            id: plugin.id.clone(),
            name: plugin.name.clone(),
            description: plugin.description.clone(),
            enabled: plugin.enabled,
            docs: plugin.docs.clone(),
        };
        let (action, view, hint) = if let Some(a) = plugin.action(p.item_ref.item.as_str()) {
            let desc = ActionDescription {
                mode: a.mode,
                runner: match a.run {
                    Runner::Command { .. } => "command".into(),
                    Runner::Plugin => "plugin".into(),
                },
                terminal: a.terminal,
                timeout: a.timeout.to_wire(),
                cwd: a.cwd.clone(),
                env_names: a.env.keys().cloned().collect(),
                env_files: a.env_files.clone(),
                effects: a.effects.clone(),
                has_schedule: a.schedule.is_some(),
                write_only_fields: a.input_schema.0.write_only_fields(),
                input_schema: p.include_schema.then(|| a.input_schema.0.as_map().clone()),
                output_schema: if p.include_schema {
                    a.output_schema.as_ref().map(|s| s.0.as_map().clone())
                } else {
                    None
                },
            };
            let verb = match a.mode {
                ActionMode::Task => "run",
                ActionMode::Process => "start",
            };
            let mut hint = vec!["mira".to_owned(), verb.to_owned(), p.item_ref.to_string()];
            if a.input_schema
                .0
                .as_map()
                .get("properties")
                .and_then(Value::as_object)
                .is_some_and(|m| !m.is_empty())
            {
                hint.extend(["--input".to_owned(), "input.json".to_owned()]);
            }
            (Some(desc), None, hint)
        } else if let Some(v) = plugin.view(p.item_ref.item.as_str()) {
            let desc = ViewDescription {
                view_kind: v.kind,
                persistence: v.persistence,
                row_actions: v.row_actions.iter().map(|r| r.action.clone()).collect(),
                source: v.source.as_ref().map(Into::into),
            };
            (
                None,
                Some(desc),
                vec!["mira".to_owned(), "view".to_owned(), p.item_ref.to_string()],
            )
        } else {
            return self.fail(not_found());
        };
        let mut desc = ItemDescription {
            item,
            plugin: summary,
            action,
            view,
            invoke_hint: hint,
        };
        // Schemas are compact by default; requested schemas above the budget become a payload.
        let mut meta = ReplyMeta::default();
        let budget = budget::budget(p.max_bytes);
        let reply = self.ok(&desc, ReplyMeta::default())?;
        if budget::json_len(&reply) <= budget {
            return Ok(reply);
        }
        if let Some(a) = desc.action.as_mut()
            && (a.input_schema.is_some() || a.output_schema.is_some())
        {
            let schemas = serde_json::json!({
                "input_schema": a.input_schema.take(),
                "output_schema": a.output_schema.take(),
            });
            meta.truncated = true;
            meta.payload = Some(self.payloads.hold(
                Some(format!(
                    "describe:{}:{}",
                    desc.item.item_ref, desc.item.definition_hash
                )),
                serde_json::to_vec(&schemas).unwrap_or_default(),
            ));
        }
        self.ok(desc, meta)
    }

    /// Increments `state_revision` and notifies state subscribers.
    pub(crate) fn state_changed(&mut self) {
        if let Some(next) = self.state_revision.next() {
            self.state_revision = next;
        }
        self.broadcast_state();
    }

    pub(crate) fn kinds_of(kinds: &[StreamKind]) -> BTreeSet<StreamKind> {
        kinds.iter().copied().collect()
    }
}

pub(crate) fn reply_ok<T: Serialize>(ctx: ReplyContext, data: T, meta: ReplyMeta) -> Handled {
    serde_json::to_value(PublicReply::success(ctx, data, meta))
        .map_err(|e| RpcError::new(RpcError::INTERNAL_ERROR, e.to_string()))
}

pub(crate) fn reply_fail(ctx: ReplyContext, error: ErrorInfo) -> Handled {
    serde_json::to_value(PublicReply::<Value>::failure(ctx, error))
        .map_err(|e| RpcError::new(RpcError::INTERNAL_ERROR, e.to_string()))
}
