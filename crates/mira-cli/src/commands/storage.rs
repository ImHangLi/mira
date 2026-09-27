//! `mira storage status|gc|clear`. `gc` only plans unless `--apply`.

use std::process::ExitCode;

use mira_client::{ConnectOptions, MAINTENANCE_TIMEOUT};
use mira_protocol::ids::PluginId;
use mira_protocol::ipc::*;
use mira_protocol::reply::ReplyContext;

use super::ctx::{Ctx, block_on};
use super::inspect::lc;
use crate::output::invalid_argument;

fn mib(b: u64) -> String {
    format!("{:.1} MiB", b as f64 / 1_048_576.0)
}

pub fn status(ctx: &Ctx, all: bool) -> ExitCode {
    block_on(async {
        let mut client = match ctx.client(&ConnectOptions::cli()).await {
            Ok(c) => c,
            Err((c, e)) => return ctx.fail(c, e),
        };
        match client
            .call_with_timeout::<_, StorageStatusData>(
                Method::StorageStatus,
                &StorageStatusParams { all },
                MAINTENANCE_TIMEOUT,
            )
            .await
        {
            Ok(r) => ctx.emit(&r, |d| {
                let mut s = format!(
                    "storage version {}, SQLite {}",
                    d.schema_version, d.sqlite_version
                );
                for u in &d.usage {
                    s.push_str(&format!(
                        "\n  {:<16} {:>10}{}{}",
                        super::inspect::path_label(u.class),
                        mib(u.bytes),
                        u.records
                            .map(|n| format!("  {n} record(s)"))
                            .unwrap_or_default(),
                        u.budget_bytes
                            .map(|b| format!("  (budget {})", mib(b)))
                            .unwrap_or_default()
                    ));
                }
                for w in &d.warnings {
                    s.push_str(&format!("\nwarning[{}]: {}", w.code, w.message));
                }
                for o in &d.other_workspaces {
                    s.push_str(&format!(
                        "\nother project {}: state {}, logs {}, cache {}",
                        o.id,
                        mib(o.state_bytes),
                        mib(o.log_bytes),
                        mib(o.cache_bytes)
                    ));
                }
                s
            }),
            Err(e) => ctx.fail(client.context(), e.to_error_info()),
        }
    })
}

pub fn gc(ctx: &Ctx, kind: GcKind, apply: bool) -> ExitCode {
    block_on(async {
        let mut client = match ctx.client(&ConnectOptions::cli()).await {
            Ok(c) => c,
            Err((c, e)) => return ctx.fail(c, e),
        };
        match client
            .call_with_timeout::<_, GcReport>(
                Method::StorageGc,
                &StorageGcParams { kind, apply },
                MAINTENANCE_TIMEOUT,
            )
            .await
        {
            Ok(r) => ctx.emit(&r, |g| {
                let mut s = if g.applied {
                    "removed:".to_owned()
                } else {
                    "would remove (run with --apply to delete):".to_owned()
                };
                for e in &g.entries {
                    s.push_str(&format!(
                        "\n  {:<10} {:>6} item(s)  {}",
                        lc(&e.kind),
                        e.records,
                        mib(e.bytes)
                    ));
                }
                s
            }),
            Err(e) => ctx.fail(client.context(), e.to_error_info()),
        }
    })
}

pub fn clear(ctx: &Ctx, plugin: &str) -> ExitCode {
    block_on(async {
        let plugin = match plugin.parse::<PluginId>() {
            Ok(p) => p,
            Err(e) => return ctx.fail(ReplyContext::default(), invalid_argument(e.to_string())),
        };
        let mut client = match ctx.client(&ConnectOptions::cli()).await {
            Ok(c) => c,
            Err((c, e)) => return ctx.fail(c, e),
        };
        let p = StorageClearParams {
            plugin: plugin.clone(),
            kind: ClearKind::State,
        };
        match client
            .call_with_timeout::<_, Ack>(Method::StorageClear, &p, MAINTENANCE_TIMEOUT)
            .await
        {
            Ok(r) => ctx.emit(&r, |_| {
                format!("cleared the private state of plugin {plugin}")
            }),
            Err(e) => ctx.fail(client.context(), e.to_error_info()),
        }
    })
}
