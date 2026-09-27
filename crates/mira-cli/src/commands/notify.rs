//! `mira notify --title TITLE MESSAGE`: a desktop notification from a running program, for
//! example a timer in a PTY. The run comes from `--run` or the `MIRA_RUN_ID` that Mira gives
//! every run.

use std::process::ExitCode;

use mira_client::ConnectOptions;
use mira_protocol::ipc::{Ack, Method, RunNotifyParams};
use mira_protocol::reply::ReplyContext;

use super::ctx::{Ctx, block_on};
use crate::output::invalid_argument;

pub fn send(ctx: &Ctx, title: String, message: String, run: Option<String>) -> ExitCode {
    block_on(async {
        let parsed = (|| {
            let run = run
                .or_else(|| std::env::var("MIRA_RUN_ID").ok())
                .ok_or_else(|| {
                    invalid_argument(
                        "no run: call `mira notify` from a Mira run (it sets MIRA_RUN_ID) or pass --run RUN",
                    )
                })?;
            let run_id = run
                .parse()
                .map_err(|e| invalid_argument(format!("{e}: `{run}`")))?;
            Ok::<_, mira_protocol::ErrorInfo>(RunNotifyParams {
                run_id,
                title,
                message,
            })
        })();
        let p = match parsed {
            Ok(p) => p,
            Err(e) => return ctx.fail(ReplyContext::default(), e),
        };
        let mut client = match ctx.client(&ConnectOptions::cli()).await {
            Ok(c) => c,
            Err((c, e)) => return ctx.fail(c, e),
        };
        match client.call::<_, Ack>(Method::RunNotify, &p).await {
            Ok(r) => ctx.emit(&r, |_| "notification sent".to_owned()),
            Err(e) => ctx.fail(client.context(), e.to_error_info()),
        }
    })
}
