//! Host connections. The UI never awaits IPC: requests go to worker tasks and their
//! answers come back as [`Event`]s.
//!
//! - control: the attached controller connection (invoke, stop, keep). Its EOF is how the
//!   host learns that this TUI left.
//! - read: an observer connection for describe, logs, and run history, so a slow read never
//!   queues in front of a stop.
//! - stream: `state`, `log`, and `view` events; re-subscribes after a reset.

use std::time::Duration;

use mira_client::{Client, ClientError, ConnectOptions, connect};
use mira_protocol::error::{ErrorCode, ErrorInfo};
use mira_protocol::ids::{
    ActionId, ActionRef, CatalogRevision, RunId, SessionId, ViewRef, ViewRevision,
};
use mira_protocol::ipc::*;
use mira_protocol::limits::MAX_REPLY_BUDGET_BYTES;
use mira_protocol::manifest::{JsonObject, TimeoutWire};
use mira_protocol::paths::WorkspacePaths;
use mira_protocol::reply::{PublicReply, ReplyMeta};
use mira_protocol::run::RunRecord;
use mira_protocol::view::{ViewBody, ViewData, ViewSnapshot};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// Default background lease for `b`.
pub const KEEP_TTL_MS: u64 = 2 * 60 * 60 * 1000;
/// Log tail size fetched when a run is opened.
const TAIL_LIMIT: u32 = 500;
const TAIL_BYTES: u32 = 256 * 1024;
/// Runs shown in an action's history list.
const HISTORY_LIMIT: u32 = 20;
/// A subscription must confirm with `ready` within this time.
const READY_TIMEOUT: Duration = Duration::from_secs(3);
/// View pages use the largest reply budget so text, tree, and JSON views arrive inline.
const VIEW_PAGE_BYTES: u32 = 256 * 1024;

pub enum Control {
    Invoke {
        action_ref: ActionRef,
        input: JsonObject,
    },
    Stop {
        action_ref: ActionRef,
        run_id: RunId,
    },
    /// Stop a one-off `mira exec` run, which has no action.
    StopRun(RunId),
    Keep,
    /// A table row action bound to the revision the user chose the row in.
    ViewAction {
        view_ref: ViewRef,
        action: ActionId,
        row: String,
        expected: ViewRevision,
    },
    Schedule {
        action_ref: ActionRef,
        enabled: bool,
    },
}

pub enum Read {
    Describe(ActionRef),
    /// The run's tail; the action's current or latest run when no run is known.
    Tail(ActionRef, Option<RunId>),
    /// The tail of one run without an action (a one-off `mira exec` run).
    RunTail(RunId),
    Older {
        run_id: RunId,
        cursor: String,
    },
    RunGet(RunId),
    Recent,
    /// The newest runs of one action, for the history list.
    History(ActionRef),
    Catalog,
    /// Every page of a view's current revision.
    View(ViewRef),
    DescribeView(ViewRef),
    /// Full status, for schedules (state events do not carry them).
    Status,
    /// The last non-empty screen line of an active PTY run, for its empty Logs tab.
    Screen(RunId),
}

pub struct ViewLoad {
    pub snapshot: ViewSnapshot,
    pub truncated: bool,
}

pub struct LogChunk {
    pub page: LogPage,
    pub older_cursor: Option<String>,
}

pub enum Event {
    Input(crossterm::event::Event),
    Frame(Box<StreamFrame>),
    StreamReset,
    StreamDown(String),
    /// The third field is the session this TUI joined to retry, if it had to.
    Invoked(
        ActionRef,
        Result<InvokeAccepted, ErrorInfo>,
        Option<SessionId>,
    ),
    Stopped(ActionRef, Result<StopAccepted, ErrorInfo>),
    RunStopped(RunId, Result<StopAccepted, ErrorInfo>),
    Kept(Result<SessionData, ErrorInfo>),
    Described(ActionRef, Result<Box<ItemDescription>, ErrorInfo>),
    Tail(ActionRef, Result<LogChunk, ErrorInfo>),
    RunTail(RunId, Result<LogChunk, ErrorInfo>),
    Older(RunId, Result<LogChunk, ErrorInfo>),
    Run(Result<Box<RunRecord>, ErrorInfo>),
    Recent(Result<RunList, ErrorInfo>),
    History(ActionRef, Result<RunList, ErrorInfo>),
    Catalog(Result<CatalogList, ErrorInfo>),
    /// A reply carried a new catalog revision.
    CatalogChanged,
    ConnectionLost(&'static str, String),
    Copied(Result<String, String>),
    Signal(&'static str),
    /// Facts from the attached PTY view's worker.
    Terminal(crate::terminal::Msg),
    View(ViewRef, Result<Box<ViewLoad>, ErrorInfo>),
    ViewDescribed(ViewRef, Result<Box<ItemDescription>, ErrorInfo>),
    ViewActed(ViewRef, ActionId, Result<InvokeAccepted, ErrorInfo>),
    ScheduleSet(ActionRef, Result<ScheduleData, ErrorInfo>),
    Status(Result<Box<StatusData>, ErrorInfo>),
    /// The last non-empty screen line of a PTY run (`None` when the screen is blank or
    /// cannot be read).
    Screen(RunId, Option<String>),
    CommandDone(crate::cmdbar::Output),
}

pub type Tx = UnboundedSender<Event>;

pub enum Failure {
    /// The connection is gone; the worker stops.
    Lost(String),
    /// The host answered with an error.
    Reply(ErrorInfo),
}

impl Failure {
    pub fn info(&self) -> ErrorInfo {
        match self {
            Self::Lost(m) => ErrorInfo::new(ErrorCode::INTERNAL, m.clone()),
            Self::Reply(e) => e.clone(),
        }
    }
}

pub struct Answer<R> {
    pub data: R,
    pub meta: ReplyMeta,
    pub catalog_revision: Option<CatalogRevision>,
}

pub async fn call<P: Serialize, R: DeserializeOwned>(
    client: &mut Client,
    method: Method,
    params: &P,
) -> Result<Answer<R>, Failure> {
    let reply: PublicReply<R> = client.call(method, params).await.map_err(|e| match e {
        ClientError::Io(m) => Failure::Lost(m),
        other => Failure::Reply(other.to_error_info()),
    })?;
    let meta = reply.meta().clone();
    let catalog_revision = reply.context().catalog_revision;
    reply
        .into_data()
        .map(|data| Answer {
            data,
            meta,
            catalog_revision,
        })
        .map_err(Failure::Reply)
}

pub fn options(connection_kind: ConnectionKind) -> ConnectOptions {
    ConnectOptions::cli().kind(ClientKind::Tui, connection_kind)
}

pub fn attach_params(env: &ClientEnv) -> SessionAttachParams {
    SessionAttachParams {
        client_env: env.clone(),
        autostart: true,
    }
}

/// Serves the controller connection until it closes.
pub async fn control_worker(
    mut client: Client,
    env: ClientEnv,
    mut rx: UnboundedReceiver<Control>,
    tx: Tx,
) {
    while let Some(req) = rx.recv().await {
        let lost = match req {
            Control::Invoke { action_ref, input } => {
                let params = ActionInvokeParams {
                    action_ref: action_ref.clone(),
                    input,
                    client_env: env.clone(),
                    request_key: None,
                    foreground: false,
                };
                let mut res =
                    call::<_, InvokeAccepted>(&mut client, Method::ActionInvoke, &params).await;
                // The session ended (TTL, `mira down`): join a new one, then retry once.
                let mut joined = None;
                if let Err(Failure::Reply(e)) = &res
                    && e.code == ErrorCode::SESSION_REQUIRED
                {
                    res = match call::<_, SessionData>(
                        &mut client,
                        Method::SessionAttach,
                        &attach_params(&env),
                    )
                    .await
                    {
                        Ok(a) => {
                            joined = a.data.session.map(|s| s.id);
                            call(&mut client, Method::ActionInvoke, &params).await
                        }
                        Err(f) => Err(f),
                    };
                }
                let lost = matches!(res, Err(Failure::Lost(_)));
                let _ = tx.send(Event::Invoked(
                    action_ref,
                    res.map(|a| a.data).map_err(|f| f.info()),
                    joined,
                ));
                lost
            }
            Control::Stop { action_ref, run_id } => {
                let params = RunStopParams {
                    target: RunTarget::Run { run_id },
                };
                let res = call::<_, StopAccepted>(&mut client, Method::RunStop, &params).await;
                let lost = matches!(res, Err(Failure::Lost(_)));
                let _ = tx.send(Event::Stopped(
                    action_ref,
                    res.map(|a| a.data).map_err(|f| f.info()),
                ));
                lost
            }
            Control::StopRun(run_id) => {
                let params = RunStopParams {
                    target: RunTarget::Run {
                        run_id: run_id.clone(),
                    },
                };
                let res = call::<_, StopAccepted>(&mut client, Method::RunStop, &params).await;
                let lost = matches!(res, Err(Failure::Lost(_)));
                let _ = tx.send(Event::RunStopped(
                    run_id,
                    res.map(|a| a.data).map_err(|f| f.info()),
                ));
                lost
            }
            Control::ViewAction {
                view_ref,
                action,
                row,
                expected,
            } => {
                let params = ViewActionParams {
                    view_ref: view_ref.clone(),
                    action: action.clone(),
                    row,
                    expected_view_revision: expected,
                    client_env: env.clone(),
                };
                let res = call::<_, InvokeAccepted>(&mut client, Method::ViewAction, &params).await;
                let lost = matches!(res, Err(Failure::Lost(_)));
                let _ = tx.send(Event::ViewActed(
                    view_ref,
                    action,
                    res.map(|a| a.data).map_err(|f| f.info()),
                ));
                lost
            }
            Control::Schedule {
                action_ref,
                enabled,
            } => {
                let params = ScheduleSetParams {
                    action_ref: action_ref.clone(),
                    enabled,
                    client_env: env.clone(),
                };
                let res = call::<_, ScheduleData>(&mut client, Method::ScheduleSet, &params).await;
                let lost = matches!(res, Err(Failure::Lost(_)));
                let _ = tx.send(Event::ScheduleSet(
                    action_ref,
                    res.map(|a| a.data).map_err(|f| f.info()),
                ));
                lost
            }
            Control::Keep => {
                let params = SessionKeepParams {
                    ttl: TimeoutWire::After { ms: KEEP_TTL_MS },
                };
                let res = call::<_, SessionData>(&mut client, Method::SessionKeep, &params).await;
                let lost = matches!(res, Err(Failure::Lost(_)));
                let _ = tx.send(Event::Kept(res.map(|a| a.data).map_err(|f| f.info())));
                lost
            }
        };
        if lost {
            let _ = tx.send(Event::ConnectionLost(
                "control",
                "the host closed the control connection".into(),
            ));
            // Keep the client alive until the UI quits; it holds no controller any more.
            break;
        }
    }
    // Returning drops the client: socket EOF tells the host this controller left.
    drop(client);
}

fn chunk(a: Answer<LogPage>) -> LogChunk {
    LogChunk {
        older_cursor: a.meta.next_cursor.clone(),
        page: a.data,
    }
}

/// The last screen row with visible text, without trailing blanks.
pub fn last_screen_line(lines: &[String]) -> Option<String> {
    lines
        .iter()
        .rev()
        .map(|l| l.trim_end())
        .find(|l| !l.trim().is_empty())
        .map(str::to_owned)
}

/// Serves observer reads until the connection closes.
pub async fn read_worker(
    mut client: Client,
    initial: Option<CatalogRevision>,
    mut rx: UnboundedReceiver<Read>,
    tx: Tx,
) {
    let mut catalog_revision = initial;
    while let Some(req) = rx.recv().await {
        let mut seen: Option<CatalogRevision> = None;
        let mut lost: Option<String> = None;
        let mut note = |f: &Failure| {
            if let Failure::Lost(m) = f {
                lost = Some(m.clone());
            }
        };
        match req {
            Read::Describe(action_ref) => {
                let params = ItemDescribeParams {
                    item_ref: action_ref.to_item_ref(),
                    include_schema: true,
                    // Forms need the schemas inline, not by payload reference.
                    max_bytes: Some(MAX_REPLY_BUDGET_BYTES as u32),
                };
                let res = call::<_, ItemDescription>(&mut client, Method::ItemDescribe, &params)
                    .await
                    .map(|a| {
                        seen = a.catalog_revision;
                        Box::new(a.data)
                    });
                if let Err(f) = &res {
                    note(f);
                }
                let _ = tx.send(Event::Described(action_ref, res.map_err(|f| f.info())));
            }
            Read::Tail(action_ref, run_id) => {
                let target = match run_id {
                    Some(run_id) => RunTarget::Run { run_id },
                    None => RunTarget::Action {
                        action_ref: action_ref.clone(),
                    },
                };
                let params = LogReadParams {
                    target,
                    cursor: None,
                    limit: Some(TAIL_LIMIT),
                    max_bytes: Some(TAIL_BYTES),
                };
                let res = call(&mut client, Method::LogRead, &params).await.map(chunk);
                if let Err(f) = &res {
                    note(f);
                }
                let _ = tx.send(Event::Tail(action_ref, res.map_err(|f| f.info())));
            }
            Read::RunTail(run_id) => {
                let params = LogReadParams {
                    target: RunTarget::Run {
                        run_id: run_id.clone(),
                    },
                    cursor: None,
                    limit: Some(TAIL_LIMIT),
                    max_bytes: Some(TAIL_BYTES),
                };
                let res = call(&mut client, Method::LogRead, &params).await.map(chunk);
                if let Err(f) = &res {
                    note(f);
                }
                let _ = tx.send(Event::RunTail(run_id, res.map_err(|f| f.info())));
            }
            Read::Older { run_id, cursor } => {
                let params = LogReadParams {
                    target: RunTarget::Run {
                        run_id: run_id.clone(),
                    },
                    cursor: Some(cursor),
                    limit: Some(TAIL_LIMIT),
                    max_bytes: Some(TAIL_BYTES),
                };
                let res = call(&mut client, Method::LogRead, &params).await.map(chunk);
                if let Err(f) = &res {
                    note(f);
                }
                let _ = tx.send(Event::Older(run_id, res.map_err(|f| f.info())));
            }
            Read::RunGet(run_id) => {
                let res =
                    call::<_, RunRecord>(&mut client, Method::RunGet, &RunGetParams { run_id })
                        .await
                        .map(|a| Box::new(a.data));
                if let Err(f) = &res {
                    note(f);
                }
                let _ = tx.send(Event::Run(res.map_err(|f| f.info())));
            }
            Read::Recent => {
                let params = RunListParams {
                    action_ref: None,
                    outcome: None,
                    cursor: None,
                    limit: Some(200),
                    max_bytes: Some(MAX_REPLY_BUDGET_BYTES as u32),
                };
                let res = call::<_, RunList>(&mut client, Method::RunListM, &params)
                    .await
                    .map(|a| a.data);
                if let Err(f) = &res {
                    note(f);
                }
                let _ = tx.send(Event::Recent(res.map_err(|f| f.info())));
            }
            Read::History(action_ref) => {
                let params = RunListParams {
                    action_ref: Some(action_ref.clone()),
                    outcome: None,
                    cursor: None,
                    limit: Some(HISTORY_LIMIT),
                    max_bytes: Some(MAX_REPLY_BUDGET_BYTES as u32),
                };
                let res = call::<_, RunList>(&mut client, Method::RunListM, &params)
                    .await
                    .map(|a| a.data);
                if let Err(f) = &res {
                    note(f);
                }
                let _ = tx.send(Event::History(action_ref, res.map_err(|f| f.info())));
            }
            Read::View(view_ref) => {
                let res = read_view(&mut client, &view_ref).await;
                if let Err(f) = &res {
                    note(f);
                }
                let _ = tx.send(Event::View(view_ref, res.map_err(|f| f.info())));
            }
            Read::DescribeView(view_ref) => {
                let params = ItemDescribeParams {
                    item_ref: view_ref.to_item_ref(),
                    include_schema: false,
                    max_bytes: None,
                };
                let res = call::<_, ItemDescription>(&mut client, Method::ItemDescribe, &params)
                    .await
                    .map(|a| Box::new(a.data));
                if let Err(f) = &res {
                    note(f);
                }
                let _ = tx.send(Event::ViewDescribed(view_ref, res.map_err(|f| f.info())));
            }
            Read::Screen(run_id) => {
                let params = TerminalSnapshotParams {
                    run_id: run_id.clone(),
                    row_start: None,
                    row_count: None,
                    include_style: false,
                    max_bytes: Some(TAIL_BYTES),
                };
                let res =
                    call::<_, TerminalSnapshot>(&mut client, Method::TerminalSnapshotM, &params)
                        .await;
                if let Err(f) = &res {
                    note(f);
                }
                let line = res.ok().and_then(|a| last_screen_line(&a.data.lines));
                let _ = tx.send(Event::Screen(run_id, line));
            }
            Read::Status => {
                let res = call::<_, StatusData>(&mut client, Method::WorkspaceStatus, &Empty {})
                    .await
                    .map(|a| Box::new(a.data));
                if let Err(f) = &res {
                    note(f);
                }
                let _ = tx.send(Event::Status(res.map_err(|f| f.info())));
            }
            Read::Catalog => {
                let res =
                    call::<_, CatalogList>(&mut client, Method::CatalogListM, &catalog_params())
                        .await
                        .map(|a| {
                            seen = a.catalog_revision;
                            a.data
                        });
                if let Err(f) = &res {
                    note(f);
                }
                if let Some(r) = seen {
                    catalog_revision = Some(r);
                }
                seen = None;
                let _ = tx.send(Event::Catalog(res.map_err(|f| f.info())));
            }
        }
        if let Some(m) = lost {
            let _ = tx.send(Event::ConnectionLost("read", m));
            break;
        }
        // A definition change seen on any read refreshes the catalog once.
        if let Some(r) = seen
            && catalog_revision.is_some_and(|c| c != r)
        {
            catalog_revision = Some(r);
            let _ = tx.send(Event::CatalogChanged);
        }
    }
}

/// Reads all pages of one revision (tables ≤10000 rows). A revision change between pages
/// (VIEW_CHANGED on the cursor) restarts from the first page, at most three times.
async fn read_view(client: &mut Client, view_ref: &ViewRef) -> Result<Box<ViewLoad>, Failure> {
    let mut attempts = 0;
    'restart: loop {
        attempts += 1;
        let mut cursor: Option<String> = None;
        let mut first: Option<ViewSnapshot> = None;
        // A row or item too large for one reply comes back by payload reference; the page
        // shows as truncated instead of silently missing it.
        let mut referenced = false;
        loop {
            let params = ViewReadParams {
                view_ref: view_ref.clone(),
                cursor: cursor.clone(),
                limit: Some(1000),
                max_bytes: Some(VIEW_PAGE_BYTES),
            };
            let page = match call::<_, ViewSnapshot>(client, Method::ViewRead, &params).await {
                Ok(p) => p,
                Err(Failure::Reply(e)) if e.code == ErrorCode::VIEW_CHANGED && attempts < 3 => {
                    continue 'restart;
                }
                Err(f) => return Err(f),
            };
            let next = page.meta.next_cursor.clone();
            let truncated = page.meta.truncated && next.is_none();
            let snap = page.data;
            if next.is_some() && matches!(snap.data, Some(ViewBody::Reference(_))) {
                referenced = true;
            }
            match &mut first {
                None => first = Some(snap),
                Some(acc) => match (&mut acc.data, snap.data) {
                    // The first page held only a referenced item: keep the rows that follow.
                    (data @ Some(ViewBody::Reference(_)), Some(ViewBody::Inline(d))) => {
                        *data = Some(ViewBody::Inline(d));
                    }
                    (
                        Some(ViewBody::Inline(ViewData::Table { rows, .. })),
                        Some(ViewBody::Inline(ViewData::Table { rows: more, .. })),
                    ) => rows.extend(more),
                    (
                        Some(ViewBody::Inline(ViewData::Log { items })),
                        Some(ViewBody::Inline(ViewData::Log { items: more })),
                    ) => items.extend(more),
                    _ => {}
                },
            }
            match next {
                Some(c) => cursor = Some(c),
                None => {
                    let Some(snapshot) = first else {
                        return Err(Failure::Reply(ErrorInfo::new(
                            ErrorCode::INTERNAL,
                            "empty view reply",
                        )));
                    };
                    return Ok(Box::new(ViewLoad {
                        snapshot,
                        truncated: truncated || referenced,
                    }));
                }
            }
        }
    }
}

pub fn catalog_params() -> CatalogListParams {
    CatalogListParams {
        query: None,
        if_revision: None,
        if_workspace: None,
        cursor: None,
        limit: Some(1000),
        max_bytes: Some(MAX_REPLY_BUDGET_BYTES as u32),
    }
}

async fn subscribe(client: &mut Client) -> Result<(), Failure> {
    let params = StreamSubscribeParams {
        kinds: vec![StreamKind::State, StreamKind::Log, StreamKind::View],
        refs: vec![],
        cursor: None,
    };
    call::<_, Subscribed>(client, Method::StreamSubscribe, &params)
        .await
        .map(|_| ())
}

/// Reads stream frames and forwards them. Any end of the subscription (a reset for a slow
/// reader included) opens a fresh stream connection and subscribes again, which yields a new
/// snapshot; neither leaves the session. A fresh connection is used because the old one may
/// still hold a full event queue, in which case the host drops the new subscription.
pub async fn stream_worker(paths: WorkspacePaths, first: Option<Client>, tx: Tx) {
    let mut client = first;
    let mut backoff = Duration::from_millis(250);
    loop {
        let mut c = match client.take() {
            Some(c) => c,
            None => match connect(&paths, &options(ConnectionKind::Stream)).await {
                Ok(c) => c,
                Err(e) => {
                    if tx.send(Event::StreamDown(e.to_string())).is_err() {
                        return;
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(4));
                    continue;
                }
            },
        };
        let ready = match subscribe(&mut c).await {
            Ok(()) => tokio::time::timeout(READY_TIMEOUT, c.next_event()).await,
            Err(f) => {
                if tx.send(Event::StreamDown(f.info().message)).is_err() {
                    return;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(4));
                continue;
            }
        };
        match ready {
            Ok(Ok(frame)) if matches!(frame.event, StreamEvent::Ready { .. }) => {
                backoff = Duration::from_millis(250);
                if tx.send(Event::Frame(Box::new(frame))).is_err() {
                    return;
                }
            }
            _ => {
                if tx
                    .send(Event::StreamDown("the subscription did not start".into()))
                    .is_err()
                {
                    return;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(4));
                continue;
            }
        }
        loop {
            match c.next_event().await {
                Ok(frame) => {
                    if let StreamEvent::End { reason } = frame.event {
                        if reason == EndReason::ResetRequired
                            && tx.send(Event::StreamReset).is_err()
                        {
                            return;
                        }
                        break;
                    }
                    if tx.send(Event::Frame(Box::new(frame))).is_err() {
                        return;
                    }
                }
                Err(e) => {
                    if tx.send(Event::StreamDown(e.to_string())).is_err() {
                        return;
                    }
                    tokio::time::sleep(backoff).await;
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::last_screen_line;

    #[test]
    fn the_last_screen_line_skips_blank_rows() {
        let rows = ["Name? ", "", "   "].map(str::to_owned);
        assert_eq!(last_screen_line(&rows).as_deref(), Some("Name?"));
        assert_eq!(last_screen_line(&["  ".to_owned()]), None);
        assert_eq!(last_screen_line(&[]), None);
    }
}
