//! Run control commands: the CLI waits; the host never blocks on a task.

use std::io::{Read, Write};
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use mira_client::{Client, ConnectOptions};
use mira_protocol::error::{ErrorCode, ErrorInfo};
use mira_protocol::ids::{ActionRef, ItemRef, RequestKey, RunId};
use mira_protocol::ipc::*;
use mira_protocol::limits::MAX_PUBLIC_REPLY_BYTES;
use mira_protocol::manifest::TimeoutWire;
use mira_protocol::reply::{PublicReply, ReplyContext};
use mira_protocol::run::{CleanupState, Lifecycle, Outcome, RunRecord, StopReason};
use mira_protocol::schema_profile::SchemaDoc;
use serde_json::{Map, Value};

use super::ctx::{Ctx, block_on};
use super::inspect::lifecycle_text;
use super::runref::{resolve_run_id, resolve_target};
use crate::human;
use crate::output::{self, Mode, invalid_argument};

const POLL: Duration = Duration::from_millis(100);

pub(crate) fn env() -> Result<ClientEnv, ErrorInfo> {
    ClientEnv::capture().map_err(|m| ErrorInfo::new(ErrorCode::INVALID_ARGUMENT, m))
}

/// Reads `--input FILE|-` as one strict JSON object.
pub fn read_input(input: Option<&str>) -> Result<Map<String, Value>, ErrorInfo> {
    let Some(src) = input else {
        return Ok(Map::new());
    };
    let bytes = if src == "-" {
        let mut b = Vec::new();
        std::io::stdin()
            .take(MAX_PUBLIC_REPLY_BYTES as u64 + 1)
            .read_to_end(&mut b)
            .map_err(|e| invalid_argument(e.to_string()))?;
        b
    } else {
        std::fs::read(Path::new(src))
            .map_err(|e| invalid_argument(format!("cannot read {src}: {e}")))?
    };
    match mira_protocol::strict_json::parse(&bytes, MAX_PUBLIC_REPLY_BYTES) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Err(ErrorInfo::new(
            ErrorCode::SCHEMA_INVALID,
            "input must be a JSON object",
        )),
        Err(e) => Err(ErrorInfo::new(
            ErrorCode::SCHEMA_INVALID,
            format!("invalid input JSON: {e}"),
        )),
    }
}

fn parse_action(s: &str) -> Result<ActionRef, ErrorInfo> {
    s.parse()
        .map_err(|e| invalid_argument(format!("{e}: `{s}`")))
}

fn parse_key(k: Option<String>) -> Result<Option<RequestKey>, ErrorInfo> {
    k.map(RequestKey::parse)
        .transpose()
        .map_err(|e| invalid_argument(e.to_string()))
}

/// `30s`, `30m`, `2h`, `1d`, or `none`.
pub fn parse_ttl(s: &str) -> Result<TimeoutWire, ErrorInfo> {
    if s == "none" {
        return Ok(TimeoutWire::None);
    }
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = num
        .parse()
        .map_err(|_| invalid_argument(format!("invalid ttl `{s}`; use 30m, 2h, or none")))?;
    let ms = match unit {
        "s" => n * 1000,
        "m" => n * 60_000,
        "h" => n * 3_600_000,
        "d" => n * 86_400_000,
        _ => {
            return Err(invalid_argument(format!(
                "invalid ttl unit in `{s}`; use s, m, h, d, or none"
            )));
        }
    };
    Ok(TimeoutWire::After { ms })
}

pub(crate) async fn connect(ctx: &Ctx) -> Result<Client, ExitCode> {
    ctx.client(&ConnectOptions::cli())
        .await
        .map_err(|(c, e)| ctx.fail(c, e))
}

/// Polls until the run is finished (the host commits the final record first).
async fn wait_finished(
    client: &mut Client,
    run_id: &RunId,
) -> Result<PublicReply<RunRecord>, ErrorInfo> {
    loop {
        let reply: PublicReply<RunRecord> = client
            .call(
                Method::RunGet,
                &RunGetParams {
                    run_id: run_id.clone(),
                },
            )
            .await
            .map_err(|e| e.to_error_info())?;
        match reply.data() {
            Some(r) if !r.lifecycle.is_active() => return Ok(reply),
            Some(_) => tokio::time::sleep(POLL).await,
            None => return Ok(reply),
        }
    }
}

fn run_text(r: &RunRecord) -> String {
    let target = r
        .action_ref
        .as_ref()
        .map_or_else(|| format!("exec \"{}\"", r.label), ToString::to_string);
    let mut s = format!("{}  {}  {}", r.run_id, target, lifecycle_text(&r.lifecycle));
    if let Some(e) = &r.exit {
        match (e.code, &e.signal) {
            (Some(c), _) => s.push_str(&format!("  exit {c}")),
            (None, Some(sig)) => s.push_str(&format!("  signal {sig}")),
            _ => {}
        }
    }
    if let Some(c) = human::cleanup_failure(&r.cleanup) {
        s.push_str(&format!("  {c}"));
    }
    if let Some(res) = &r.result {
        s.push_str(&format!(
            "\n  result ({}): {}",
            if res.ok { "ok" } else { "failed" },
            res.summary
        ));
    }
    if let Some(n) = &r.note {
        s.push_str(&format!("\n  note: {}", n));
    }
    s
}

/// `mira runs RUN --text`: what happened, in a few lines, and where the output is.
fn run_summary(r: &RunRecord) -> String {
    let target = r
        .action_ref
        .as_ref()
        .map_or_else(|| format!("exec \"{}\"", r.label), ToString::to_string);
    let mut outcome = match r.lifecycle {
        Lifecycle::Finished { outcome } => outcome.word().to_owned(),
        other => lifecycle_text(&other),
    };
    if let Some(e) = &r.exit {
        match (e.code, &e.signal) {
            (Some(c), _) => outcome.push_str(&format!(", exit {c}")),
            (None, Some(sig)) => outcome.push_str(&format!(", ended by {sig}")),
            _ => {}
        }
    }
    let took = if r.lifecycle.is_active() {
        "running for"
    } else {
        "took"
    };
    let mut s = format!(
        "{}  {target}\n  {outcome}\n  started {}, {took} {}",
        r.run_id,
        human::clock().hms(r.started_at),
        human::run_duration(r)
    );
    match &r.cleanup {
        CleanupState::NotNeeded => {}
        CleanupState::Succeeded { .. } => s.push_str("\n  cleanup succeeded"),
        CleanupState::Pending | CleanupState::Running { .. } => s.push_str("\n  cleanup running"),
        CleanupState::Unknown { message } => s.push_str(&format!("\n  cleanup unknown: {message}")),
        failed @ CleanupState::Failed { .. } => {
            if let Some(c) = human::cleanup_failure(failed) {
                s.push_str(&format!("\n  {c}"));
            }
        }
    }
    if let Some(res) = &r.result {
        s.push_str(&format!(
            "\n  result ({}): {}",
            if res.ok { "ok" } else { "failed" },
            res.summary
        ));
    }
    if let Some(n) = &r.note {
        s.push_str(&format!("\n  note: {}", n));
    }
    s.push_str(&format!(
        "\n  output: mira logs {}",
        human::short_run(&r.run_id)
    ));
    s
}

/// Log lines shown under a failed `run --text`.
const FAILURE_TAIL: u32 = 5;

/// Text for a failed run: the status line, the last log lines, then what to do next.
async fn failure_text(client: &mut Client, rec: &RunRecord, e: &ErrorInfo) -> String {
    let target = rec
        .action_ref
        .as_ref()
        .map_or_else(|| rec.label.clone(), ToString::to_string);
    let mut status = match (rec.stop_reason, &rec.note) {
        (Some(StopReason::ProtocolError), Some(note)) => {
            format!("{target} stopped: {}", note)
        }
        (Some(StopReason::ProtocolError), None) => {
            format!("{target} stopped: invalid plugin output")
        }
        _ => e.message.clone(),
    };
    if let Some(c) = human::cleanup_failure(&rec.cleanup) {
        status.push_str(&format!(", {c}"));
    }
    let mut s = format!("error[{}]: {status}", e.code);
    let page = client
        .call::<_, LogPage>(
            Method::LogRead,
            &LogReadParams {
                target: RunTarget::Run {
                    run_id: rec.run_id.clone(),
                },
                cursor: None,
                limit: Some(FAILURE_TAIL),
                max_bytes: None,
            },
        )
        .await;
    if let Ok(page) = page
        && let Some(pg) = page.data()
    {
        for r in &pg.items {
            s.push_str(&format!("\n  {} {}", stream_tag(r), r.text));
        }
    }
    if let Some(n) = &e.next_action {
        s.push_str(&format!("\n  next: {} ({})", n.argv.join(" "), n.reason));
    }
    s
}

/// Converts a finished run into the public reply: success, or an error with the fixed class.
fn final_reply(reply: PublicReply<RunRecord>) -> PublicReply<RunRecord> {
    let ctx = reply.context();
    let Some(rec) = reply.data().cloned() else {
        return reply;
    };
    let outcome = match rec.lifecycle {
        Lifecycle::Finished { outcome } => outcome,
        _ => return reply,
    };
    let (code, what) = match outcome {
        Outcome::Succeeded => return reply,
        Outcome::Failed => (ErrorCode::EXECUTION_FAILED, "failed"),
        Outcome::TimedOut => (ErrorCode::TIMEOUT, "timed out"),
        Outcome::Cancelled => (ErrorCode::CANCELLED, "was cancelled"),
        Outcome::Interrupted => (ErrorCode::OUTCOME_UNKNOWN, "was interrupted"),
    };
    let target = rec
        .action_ref
        .as_ref()
        .map_or_else(|| rec.label.clone(), ToString::to_string);
    let exit = rec.exit.as_ref().map(|e| match (e.code, &e.signal) {
        (Some(c), _) => format!(" with exit {c}"),
        (None, Some(s)) => format!(" by {s}"),
        _ => String::new(),
    });
    let mut details = Map::new();
    details.insert("run_id".into(), Value::String(rec.run_id.to_string()));
    details.insert(
        "outcome".into(),
        serde_json::to_value(outcome).unwrap_or(Value::Null),
    );
    details.insert(
        "exit".into(),
        serde_json::to_value(&rec.exit).unwrap_or(Value::Null),
    );
    if let Some(n) = &rec.note {
        details.insert("note".into(), Value::String(n.clone()));
    }
    if let Some(reason) = rec.stop_reason {
        details.insert(
            "stop_reason".into(),
            serde_json::to_value(reason).unwrap_or(Value::Null),
        );
    }
    // The plugin's self-reported result stays visible, but never as success. Error details
    // stay bounded: the result data remains readable with `mira runs RUN`.
    if let Some(res) = &rec.result {
        details.insert(
            "result".into(),
            serde_json::json!({"ok": res.ok, "summary": res.summary, "error": res.error}),
        );
    }
    let reported = rec
        .result
        .as_ref()
        .map(|r| format!(": {}", r.summary))
        .unwrap_or_default();
    let info = ErrorInfo::new(
        code,
        format!("{target} {what}{}{reported}", exit.unwrap_or_default()),
    )
    .with_details(details)
    .with_next_action(
        &["mira", "logs", rec.run_id.as_str()],
        "Read the run's output.",
    );
    PublicReply::failure(ctx, info)
}

/// Whether `action` is a long-running (process) action.
async fn is_service(client: &mut Client, action: &str) -> bool {
    let Ok(item_ref) = action.parse::<ItemRef>() else {
        return false;
    };
    let reply: Result<PublicReply<ItemDescription>, _> = client
        .call(
            Method::ItemDescribe,
            &ItemDescribeParams {
                item_ref,
                include_schema: false,
                max_bytes: None,
            },
        )
        .await;
    reply.ok().is_some_and(|r| {
        r.data()
            .and_then(|d| d.action.as_ref())
            .is_some_and(|a| a.mode == mira_protocol::manifest::ActionMode::Process)
    })
}

pub(crate) async fn run_and_wait(
    ctx: &Ctx,
    client: &mut Client,
    method: Method,
    params: Value,
    wait: bool,
) -> ExitCode {
    let accepted: PublicReply<InvokeAccepted> = match client.call(method, &params).await {
        Ok(r) => r,
        Err(e) => return ctx.fail(client.context(), e.to_error_info()),
    };
    if ctx.mode == Mode::Text
        && let Some(e) = accepted.error()
        && e.code == ErrorCode::SESSION_REQUIRED
        && let Some(action) = params.get("action_ref").and_then(Value::as_str)
        && is_service(client, action).await
    {
        eprintln!(
            "error[{}]: {action} is a service. Start it with `mira start {action}`.",
            e.code
        );
        return ExitCode::from(accepted.exit_code());
    }
    if !wait || !accepted.is_ok() {
        return ctx.emit(&accepted, |a| {
            format!(
                "{}  {}{}",
                a.run_id,
                lifecycle_text(&a.state),
                if a.reused { "  (reused)" } else { "" }
            )
        });
    }
    let Some(run_id) = accepted.data().map(|a| a.run_id.clone()) else {
        return ctx.emit(&accepted, |_| String::new());
    };
    if ctx.mode == Mode::Text {
        eprintln!("{run_id} started; waiting (Ctrl-C stops it)...");
    }
    let finished = match wait_finished(client, &run_id).await {
        Ok(reply) => reply,
        Err(e) => return ctx.fail(client.context(), e),
    };
    let record = finished.data().cloned();
    let reply = final_reply(finished);
    if ctx.mode == Mode::Text
        && let (Some(rec), Some(e)) = (record, reply.error())
    {
        eprintln!("{}", failure_text(client, &rec, e).await);
        return ExitCode::from(reply.exit_code());
    }
    ctx.emit(&reply, run_text)
}

pub fn run(
    ctx: &Ctx,
    action: &str,
    input: Option<&str>,
    no_wait: bool,
    request_key: Option<String>,
) -> ExitCode {
    block_on(async {
        let prepared = (|| {
            Ok::<_, ErrorInfo>((
                parse_action(action)?,
                read_input(input)?,
                parse_key(request_key)?,
                env()?,
            ))
        })();
        let (action_ref, input, request_key, client_env) = match prepared {
            Ok(v) => v,
            Err(e) => return ctx.fail(ReplyContext::default(), e),
        };
        let mut client = match connect(ctx).await {
            Ok(c) => c,
            Err(code) => return code,
        };
        let p = ActionInvokeParams {
            action_ref,
            input,
            client_env,
            request_key,
            foreground: !no_wait,
        };
        let params = serde_json::to_value(p).unwrap_or(Value::Null);
        run_and_wait(ctx, &mut client, Method::ActionInvoke, params, !no_wait).await
    })
}

pub fn start(
    ctx: &Ctx,
    action: &str,
    input: Option<&str>,
    request_key: Option<String>,
) -> ExitCode {
    block_on(async {
        let prepared = (|| {
            Ok::<_, ErrorInfo>((
                parse_action(action)?,
                read_input(input)?,
                parse_key(request_key)?,
                env()?,
            ))
        })();
        let (action_ref, input, request_key, client_env) = match prepared {
            Ok(v) => v,
            Err(e) => return ctx.fail(ReplyContext::default(), e),
        };
        let mut client = match connect(ctx).await {
            Ok(c) => c,
            Err(code) => return code,
        };
        let p = ActionInvokeParams {
            action_ref,
            input,
            client_env,
            request_key,
            foreground: false,
        };
        run_and_wait(
            ctx,
            &mut client,
            Method::ActionInvoke,
            serde_json::to_value(p).unwrap_or(Value::Null),
            false,
        )
        .await
    })
}

pub fn exec(ctx: &Ctx, label: String, argv: Vec<String>, request_key: Option<String>) -> ExitCode {
    block_on(async {
        let prepared = (|| Ok::<_, ErrorInfo>((parse_key(request_key)?, env()?)))();
        let (request_key, client_env) = match prepared {
            Ok(v) => v,
            Err(e) => return ctx.fail(ReplyContext::default(), e),
        };
        let mut client = match connect(ctx).await {
            Ok(c) => c,
            Err(code) => return code,
        };
        let p = ActionExecParams {
            label,
            argv,
            client_env,
            request_key,
            foreground: true,
        };
        run_and_wait(
            ctx,
            &mut client,
            Method::ActionExec,
            serde_json::to_value(p).unwrap_or(Value::Null),
            true,
        )
        .await
    })
}

/// `mira stop` data. With `--wait`, the finished run record plus the `state` field that
/// `mira stop` returns without `--wait`.
#[derive(serde::Serialize)]
#[serde(untagged)]
enum StopData {
    Accepted(StopAccepted),
    Finished {
        #[serde(flatten)]
        record: Box<RunRecord>,
        state: Lifecycle,
    },
}

async fn stop_target(
    ctx: &Ctx,
    client: &mut Client,
    target: RunTarget,
    wait: bool,
) -> Result<PublicReply<StopData>, ExitCode> {
    let reply: PublicReply<StopAccepted> = client
        .call(Method::RunStop, &RunStopParams { target })
        .await
        .map_err(|e| ctx.fail(client.context(), e.to_error_info()))?;
    if wait && let Some(run_id) = reply.data().map(|d| d.run_id.clone()) {
        let fin = wait_finished(client, &run_id)
            .await
            .map_err(|e| ctx.fail(client.context(), e))?;
        let ctx2 = fin.context();
        if let Some(rec) = fin.data() {
            return Ok(PublicReply::success(
                ctx2,
                StopData::Finished {
                    state: rec.lifecycle,
                    record: Box::new(rec.clone()),
                },
                reply.meta().clone(),
            ));
        }
    }
    Ok(reply.map(StopData::Accepted))
}

pub fn stop(ctx: &Ctx, target: &str, wait: bool) -> ExitCode {
    block_on(async {
        let mut client = match connect(ctx).await {
            Ok(c) => c,
            Err(code) => return code,
        };
        let target = match resolve_target(&mut client, target).await {
            Ok(t) => t,
            Err(e) => return ctx.fail(client.context(), e),
        };
        match stop_target(ctx, &mut client, target, wait).await {
            Ok(reply) => ctx.emit(&reply, |d| match d {
                StopData::Accepted(s) => format!("{}  {}", s.run_id, lifecycle_text(&s.state)),
                StopData::Finished { record, .. } => run_text(record),
            }),
            Err(code) => code,
        }
    })
}

/// Validates `input` against the action's input schema the same way the host does.
async fn precheck_input(
    client: &mut Client,
    action_ref: &ActionRef,
    input: &Map<String, Value>,
) -> Result<(), ErrorInfo> {
    let Ok(item_ref) = action_ref.to_string().parse::<ItemRef>() else {
        return Ok(());
    };
    let reply: PublicReply<ItemDescription> = client
        .call(
            Method::ItemDescribe,
            &ItemDescribeParams {
                item_ref,
                include_schema: true,
                max_bytes: None,
            },
        )
        .await
        .map_err(|e| e.to_error_info())?;
    let Some(schema) = reply
        .data()
        .and_then(|d| d.action.as_ref())
        .and_then(|a| a.input_schema.as_ref())
    else {
        return Ok(());
    };
    // The host already accepted this schema; if it cannot be rebuilt here, let the host decide.
    let Ok(doc) = SchemaDoc::check(schema, "/input_schema", true) else {
        return Ok(());
    };
    let Ok(validator) = doc.compile() else {
        return Ok(());
    };
    let issues = doc.validate(
        &validator,
        &Value::Object(doc.effective_input(input)),
        "/input",
    );
    if issues.is_empty() {
        Ok(())
    } else {
        Err(issues.to_error_info())
    }
}

pub fn restart(ctx: &Ctx, action: &str, input: Option<&str>) -> ExitCode {
    block_on(async {
        let prepared =
            (|| Ok::<_, ErrorInfo>((parse_action(action)?, read_input(input)?, env()?)))();
        let (action_ref, input, client_env) = match prepared {
            Ok(v) => v,
            Err(e) => return ctx.fail(ReplyContext::default(), e),
        };
        let mut client = match connect(ctx).await {
            Ok(c) => c,
            Err(code) => return code,
        };
        // Reject bad input before anything is stopped.
        if let Err(e) = precheck_input(&mut client, &action_ref, &input).await {
            return ctx.fail(client.context(), e);
        }
        // Stop the current instance (if any) and wait, then start with the current definition.
        let stopped = client
            .call::<_, StopAccepted>(
                Method::RunStop,
                &RunStopParams {
                    target: RunTarget::Action {
                        action_ref: action_ref.clone(),
                    },
                },
            )
            .await;
        if let Ok(reply) = stopped
            && let Some(run_id) = reply.data().map(|d| d.run_id.clone())
            && let Err(e) = wait_finished(&mut client, &run_id).await
        {
            return ctx.fail(client.context(), e);
        }
        let p = ActionInvokeParams {
            action_ref,
            input,
            client_env,
            request_key: None,
            foreground: false,
        };
        run_and_wait(
            ctx,
            &mut client,
            Method::ActionInvoke,
            serde_json::to_value(p).unwrap_or(Value::Null),
            false,
        )
        .await
    })
}

fn session_text(d: &SessionData) -> String {
    human::session_line(d.session.as_ref())
}

pub fn up(ctx: &Ctx, background: bool, ttl: &str) -> ExitCode {
    if !background {
        return ctx.fail(
            ReplyContext::default(),
            invalid_argument("`mira up` needs --background; open `mira` for a foreground session")
                .with_next_action(
                    &["mira", "up", "--background"],
                    "Keep work running without an open TUI.",
                ),
        );
    }
    block_on(async {
        let prepared = (|| Ok::<_, ErrorInfo>((parse_ttl(ttl)?, env()?)))();
        let (ttl, client_env) = match prepared {
            Ok(v) => v,
            Err(e) => return ctx.fail(ReplyContext::default(), e),
        };
        let mut client = match connect(ctx).await {
            Ok(c) => c,
            Err(code) => return code,
        };
        match client
            .call::<_, SessionData>(
                Method::SessionOpen,
                &SessionOpenParams {
                    mode: OpenMode::Background,
                    client_env,
                    ttl,
                },
            )
            .await
        {
            Ok(r) => ctx.emit(&r, session_text),
            Err(e) => ctx.fail(client.context(), e.to_error_info()),
        }
    })
}

fn stop_text(d: &SessionStopData) -> String {
    match &d.stopped_session {
        None => "Nothing is running.".into(),
        Some(id) => {
            let state = if d.session.is_some() {
                "; stopping"
            } else {
                ""
            };
            format!("stopped session {id} and {} run(s){state}", d.stopped_runs)
        }
    }
}

pub fn down(ctx: &Ctx, wait: bool) -> ExitCode {
    block_on(async {
        let paths = match ctx.paths() {
            Ok(p) => p,
            Err(e) => return ctx.fail(ReplyContext::default(), e),
        };
        let cx = ReplyContext {
            workspace: Some(mira_protocol::reply::WorkspaceRef {
                id: paths.id.clone(),
                root: paths.root.clone(),
            }),
            ..ReplyContext::default()
        };
        // Never start a host only to report that nothing runs.
        let opts = ConnectOptions {
            spawn: false,
            ..ConnectOptions::cli()
        };
        let mut client = match mira_client::connect(&paths, &opts).await {
            Ok(c) => c,
            Err(mira_client::ClientError::Connect(_)) => {
                let data = SessionStopData {
                    session: None,
                    stopped_session: None,
                    stopped_runs: 0,
                };
                let reply =
                    PublicReply::success(cx, data, mira_protocol::reply::ReplyMeta::default());
                return ctx.emit(&reply, |_| "Nothing is running.".to_owned());
            }
            Err(e) => return ctx.fail(cx, e.to_error_info()),
        };
        let reply = match client
            .call::<_, SessionStopData>(Method::SessionStop, &Empty {})
            .await
        {
            Ok(r) => r,
            Err(e) => return ctx.fail(client.context(), e.to_error_info()),
        };
        if wait {
            let (stopped_session, stopped_runs) = reply
                .data()
                .map(|d| (d.stopped_session.clone(), d.stopped_runs))
                .unwrap_or_default();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
            loop {
                match client.status().await {
                    Ok(s) if s.data().is_some_and(|d| d.session.is_none()) => {
                        let s = s.map(|d| SessionStopData {
                            session: d.session,
                            stopped_session,
                            stopped_runs,
                        });
                        return ctx.emit(&s, stop_text);
                    }
                    Ok(_) if tokio::time::Instant::now() < deadline => {
                        tokio::time::sleep(POLL).await
                    }
                    Ok(_) => {
                        return ctx.fail(
                            client.context(),
                            ErrorInfo::new(
                                ErrorCode::TIMEOUT,
                                "the session is still stopping after 30 s",
                            ),
                        );
                    }
                    Err(e) => return ctx.fail(client.context(), e.to_error_info()),
                }
            }
        }
        ctx.emit(&reply, stop_text)
    })
}

pub fn runs(
    ctx: &Ctx,
    run: Option<String>,
    action: Option<String>,
    outcome: Option<String>,
    limit: Option<u32>,
    after: Option<String>,
    max_bytes: Option<u32>,
) -> ExitCode {
    block_on(async {
        let mut client = match connect(ctx).await {
            Ok(c) => c,
            Err(code) => return code,
        };
        if let Some(run) = run {
            let run_id = match resolve_run_id(&mut client, &run).await {
                Ok(r) => r,
                Err(e) => return ctx.fail(client.context(), e),
            };
            return match client
                .call::<_, RunRecord>(Method::RunGet, &RunGetParams { run_id })
                .await
            {
                Ok(r) => ctx.emit(&r, run_summary),
                Err(e) => ctx.fail(client.context(), e.to_error_info()),
            };
        }
        let parsed = (|| {
            let action_ref = action.as_deref().map(parse_action).transpose()?;
            let outcome = outcome
                .map(|o| {
                    serde_json::from_value::<Outcome>(Value::String(o.clone()))
                        .map_err(|_| invalid_argument(format!("unknown outcome `{o}`")))
                })
                .transpose()?;
            Ok::<_, ErrorInfo>((action_ref, outcome))
        })();
        let (action_ref, outcome) = match parsed {
            Ok(v) => v,
            Err(e) => return ctx.fail(client.context(), e),
        };
        let p = RunListParams {
            action_ref,
            outcome,
            cursor: after,
            limit,
            max_bytes,
        };
        match client.call::<_, RunList>(Method::RunList, &p).await {
            Ok(r) => ctx.emit(&r, |l| {
                if l.runs.is_empty() {
                    "no runs".into()
                } else {
                    l.runs.iter().map(run_text).collect::<Vec<_>>().join("\n")
                }
            }),
            Err(e) => ctx.fail(client.context(), e.to_error_info()),
        }
    })
}

fn stream_tag(r: &mira_protocol::run::LogRecord) -> &'static str {
    match r.stream {
        mira_protocol::run::LogStream::Stderr => "err ",
        mira_protocol::run::LogStream::Host => "mira",
        mira_protocol::run::LogStream::Plugin => "plug",
        mira_protocol::run::LogStream::Pty => "pty ",
        mira_protocol::run::LogStream::Stdout => "out ",
    }
}

fn log_line(r: &mira_protocol::run::LogRecord) -> String {
    format!("{:>6} {} {}", r.log_seq, stream_tag(r), r.text)
}

/// `--stream` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum StreamArg {
    Stdout,
    Stderr,
}

/// Client-side log filter for `--grep` and `--stream`; the host still bounds the page.
#[derive(Debug, Clone, Default)]
pub struct LogFilter {
    /// Lower-case pattern.
    grep: Option<String>,
    stream: Option<StreamArg>,
}

impl LogFilter {
    pub fn new(grep: Option<&str>, stream: Option<StreamArg>) -> Self {
        Self {
            grep: grep.filter(|g| !g.is_empty()).map(str::to_lowercase),
            stream,
        }
    }

    fn is_empty(&self) -> bool {
        self.grep.is_none() && self.stream.is_none()
    }

    pub fn matches(&self, r: &mira_protocol::run::LogRecord) -> bool {
        use mira_protocol::run::LogStream;
        let stream_ok = match self.stream {
            None => true,
            Some(StreamArg::Stdout) => r.stream == LogStream::Stdout,
            Some(StreamArg::Stderr) => r.stream == LogStream::Stderr,
        };
        stream_ok
            && self
                .grep
                .as_deref()
                .is_none_or(|g| r.text.to_lowercase().contains(g))
    }

    pub fn apply(&self, records: &mut Vec<mira_protocol::run::LogRecord>) {
        if !self.is_empty() {
            records.retain(|r| self.matches(r));
        }
    }
}

pub struct LogArgs {
    pub after: Option<String>,
    pub limit: Option<u32>,
    pub max_bytes: Option<u32>,
    pub follow: bool,
    pub filter: LogFilter,
}

pub fn logs(ctx: &Ctx, target: &str, args: LogArgs) -> ExitCode {
    let LogArgs {
        after,
        limit,
        max_bytes,
        follow,
        filter,
    } = args;
    block_on(async {
        let mut client = match connect(ctx).await {
            Ok(c) => c,
            Err(code) => return code,
        };
        let target = match resolve_target(&mut client, target).await {
            Ok(t) => t,
            Err(e) => return ctx.fail(client.context(), e),
        };
        let p = LogReadParams {
            target: target.clone(),
            cursor: after,
            limit,
            max_bytes,
        };
        let page: PublicReply<LogPage> = match client.call(Method::LogRead, &p).await {
            Ok(r) => r,
            Err(e) => return ctx.fail(client.context(), e.to_error_info()),
        };
        // New records follow the unfiltered tail, so remember where it ended.
        let last_seen = page
            .data()
            .and_then(|pg| pg.items.last().map(|r| r.log_seq));
        let page = page.map(|mut pg| {
            filter.apply(&mut pg.items);
            pg
        });
        if !follow || !page.is_ok() {
            return ctx.emit(&page, |pg| {
                pg.items.iter().map(log_line).collect::<Vec<_>>().join("\n")
            });
        }
        let Some(tail) = page.data().cloned() else {
            return ctx.emit(&page, |_| String::new());
        };
        follow_logs(ctx, tail, last_seen, &filter).await
    })
}

/// `--follow`: JSONL stream frames (ready first), or text lines after the tail.
async fn follow_logs(
    ctx: &Ctx,
    tail: LogPage,
    last_seen: Option<mira_protocol::ids::LogSeq>,
    filter: &LogFilter,
) -> ExitCode {
    let opts = ConnectOptions::cli().kind(ClientKind::Cli, ConnectionKind::Stream);
    let mut stream = match ctx.client(&opts).await {
        Ok(c) => c,
        Err((c, e)) => return ctx.fail(c, e),
    };
    let run_id = tail.run_id.clone();
    let sub = StreamSubscribeParams {
        kinds: vec![StreamKind::Log, StreamKind::State],
        refs: vec![run_id.to_string()],
        cursor: None,
    };
    if let Err(e) = stream
        .call::<_, Subscribed>(Method::StreamSubscribe, &sub)
        .await
    {
        return ctx.fail(stream.context(), e.to_error_info());
    }
    let mut out = std::io::stdout().lock();
    if ctx.mode == Mode::Text {
        for r in &tail.items {
            let _ = writeln!(out, "{}", log_line(r));
        }
    }
    loop {
        let mut frame = match stream.next_event().await {
            Ok(f) => f,
            Err(e) => return output::fail(ctx.mode, stream.context(), e.to_error_info()),
        };
        let mut done = false;
        let mut skip = false;
        if let StreamEvent::Log { records, .. } = &mut frame.event {
            filter.apply(records);
            skip = records.is_empty();
        }
        match &frame.event {
            StreamEvent::Log { records, .. } => {
                if ctx.mode == Mode::Text {
                    for r in records
                        .iter()
                        .filter(|r| last_seen.is_none_or(|s| r.log_seq > s))
                    {
                        let _ = writeln!(out, "{}", log_line(r));
                    }
                }
            }
            StreamEvent::State { runs, .. } | StreamEvent::Snapshot(StatusData { runs, .. }) => {
                done = !runs.iter().any(|r| r.run_id == run_id);
            }
            StreamEvent::Gap {
                dropped_records,
                resume_cursor,
                ..
            } => {
                if ctx.mode == Mode::Text {
                    let n = dropped_records.map_or_else(|| "some".to_owned(), |n| n.to_string());
                    let _ = writeln!(
                        out,
                        "-- {n} record(s) were not streamed live; read them with: mira logs {run_id} --after {}",
                        resume_cursor.as_deref().unwrap_or("<none>")
                    );
                }
            }
            StreamEvent::End { .. } => done = true,
            _ => {}
        }
        let relevant = !matches!(
            frame.event,
            StreamEvent::State { .. } | StreamEvent::Snapshot(_)
        );
        if ctx.mode == Mode::Json
            && !skip
            && (relevant || done)
            && let Ok(line) = serde_json::to_string(&frame)
        {
            let _ = writeln!(out, "{line}");
        }
        let _ = out.flush();
        if done {
            return ExitCode::SUCCESS;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mira_protocol::run::{LogRecord, LogStream};

    fn rec(stream: LogStream, text: &str) -> LogRecord {
        serde_json::from_value(serde_json::json!({
            "log_seq": 1,
            "recorded_at": "2026-09-26T01:34:00.000Z",
            "stream": stream,
            "level": "info",
            "text": text,
            "continued": false,
            "truncated": false,
        }))
        .unwrap()
    }

    #[test]
    fn grep_is_a_case_insensitive_substring_and_stream_narrows_it() {
        let f = LogFilter::new(Some("ERROR"), None);
        assert!(f.matches(&rec(LogStream::Stdout, "db error: timeout")));
        assert!(f.matches(&rec(LogStream::Stderr, "Error")));
        assert!(!f.matches(&rec(LogStream::Stdout, "all good")));
        let f = LogFilter::new(Some("error"), Some(StreamArg::Stderr));
        assert!(!f.matches(&rec(LogStream::Stdout, "error")));
        assert!(f.matches(&rec(LogStream::Stderr, "an ERROR")));
        let f = LogFilter::new(None, Some(StreamArg::Stdout));
        assert!(f.matches(&rec(LogStream::Stdout, "x")));
        assert!(!f.matches(&rec(LogStream::Host, "x")));
    }

    #[test]
    fn an_empty_filter_keeps_everything() {
        let mut v = vec![rec(LogStream::Host, "a"), rec(LogStream::Pty, "b")];
        LogFilter::new(Some(""), None).apply(&mut v);
        assert_eq!(v.len(), 2);
    }
}
