//! Run control commands: the CLI waits; the host never blocks on a task.

mod history;
mod invoke;
mod session;
mod text;

use std::io::Read;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use mira_client::{Client, ConnectOptions};
use mira_protocol::error::{ErrorCode, ErrorInfo};
use mira_protocol::ids::{ActionRef, RequestKey, RunId};
use mira_protocol::ipc::*;
use mira_protocol::limits::MAX_PUBLIC_REPLY_BYTES;
use mira_protocol::manifest::TimeoutWire;
use mira_protocol::reply::PublicReply;
use mira_protocol::run::RunRecord;
use serde_json::{Map, Value};

use crate::commands::ctx::Ctx;
use crate::output::invalid_argument;

pub use history::{LogArgs, LogFilter, StreamArg, logs, runs};
pub(crate) use invoke::run_and_wait;
pub use invoke::{exec, restart, run, start, stop};
pub use session::{down, up};

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
fn parse_ttl(s: &str) -> Result<TimeoutWire, ErrorInfo> {
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
