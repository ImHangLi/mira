//! `up` and `down`: open a background session or stop the current one.

use std::process::ExitCode;
use std::time::Duration;

use mira_client::ConnectOptions;
use mira_protocol::error::{ErrorCode, ErrorInfo};
use mira_protocol::ipc::*;
use mira_protocol::reply::{PublicReply, ReplyContext};

use crate::commands::ctx::{Ctx, block_on};
use crate::human;
use crate::output::invalid_argument;

use super::{POLL, connect, env, parse_ttl};

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
