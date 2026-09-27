//! `mira schedule ACTION on|off`: persist an interval schedule switch (§10.2).

use std::process::ExitCode;

use mira_client::ConnectOptions;
use mira_protocol::ipc::{ClientEnv, Method, ScheduleData, ScheduleSetParams};
use mira_protocol::reply::ReplyContext;

use super::ctx::{Ctx, block_on};
use crate::output::invalid_argument;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Switch {
    On,
    Off,
}

pub fn text(d: &ScheduleData) -> String {
    format!(
        "{}  {}, {}{}{}",
        d.action_ref,
        crate::human::interval(d.every_ms),
        if d.enabled { "on" } else { "off" },
        d.next_at
            .map(|t| format!(", next at {}", crate::human::clock().when(t)))
            .unwrap_or_default(),
        if d.missed_ticks > 0 {
            format!(", {} tick(s) skipped while busy", d.missed_ticks)
        } else {
            String::new()
        }
    )
}

pub fn set(ctx: &Ctx, action: &str, switch: Switch) -> ExitCode {
    block_on(async {
        let parsed = (|| {
            let action_ref = action
                .parse()
                .map_err(|e| invalid_argument(format!("{e}: `{action}`")))?;
            let client_env = ClientEnv::capture().map_err(invalid_argument)?;
            Ok::<_, mira_protocol::ErrorInfo>(ScheduleSetParams {
                action_ref,
                enabled: switch == Switch::On,
                client_env,
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
        match client
            .call::<_, ScheduleData>(Method::ScheduleSet, &p)
            .await
        {
            Ok(r) => ctx.emit(&r, text),
            Err(e) => ctx.fail(client.context(), e.to_error_info()),
        }
    })
}
