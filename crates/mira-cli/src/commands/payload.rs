//! `mira payload read`: bounded UTF-8 chunks of a host-held payload. A read never
//! re-runs the action that produced the data; data that is gone is PAYLOAD_GONE.

use std::process::ExitCode;

use mira_protocol::ipc::*;

use super::ctx::{Ctx, block_on};
use super::runtime::connect;

pub fn read(
    ctx: &Ctx,
    token: String,
    pointer: Option<String>,
    offset: u64,
    max_bytes: Option<u32>,
) -> ExitCode {
    block_on(async {
        let mut client = match connect(ctx).await {
            Ok(c) => c,
            Err(code) => return code,
        };
        let p = PayloadReadParams {
            token,
            pointer,
            offset,
            max_bytes,
        };
        match client.call::<_, ChunkData>(Method::PayloadRead, &p).await {
            Ok(r) => ctx.emit(&r, |c| c.text.clone()),
            Err(e) => ctx.fail(client.context(), e.to_error_info()),
        }
    })
}
