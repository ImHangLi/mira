//! `run`, `start`, `exec`, `restart`, and `stop`: start or stop a run and optionally wait.

use std::process::ExitCode;

use mira_client::Client;
use mira_protocol::error::{ErrorCode, ErrorInfo};
use mira_protocol::ids::{ActionRef, ItemRef};
use mira_protocol::ipc::*;
use mira_protocol::reply::{PublicReply, ReplyContext};
use mira_protocol::run::{Lifecycle, Outcome, RunRecord};
use mira_protocol::schema_profile::SchemaDoc;
use serde_json::{Map, Value};

use crate::commands::ctx::{Ctx, block_on};
use crate::commands::inspect::lifecycle_text;
use crate::commands::runref::resolve_target;
use crate::output::Mode;

use super::text::{failure_text, run_text};
use super::{connect, env, parse_action, parse_key, read_input, wait_finished};

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
