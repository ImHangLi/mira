//! `mira logs --host`: the last lines of this project's host log, where Mira itself records
//! starts, stops, accepted and rejected configuration, storage problems, and crashes. It reads
//! the file directly, so it works when the host is down or cannot start.

use std::io::{Read, Seek, SeekFrom};
use std::process::ExitCode;

use mira_protocol::reply::{PublicReply, ReplyContext, ReplyMeta};
use serde::Serialize;

use super::ctx::Ctx;

/// Only the end of a large log is read.
const TAIL_BYTES: u64 = 1024 * 1024;
const DEFAULT_LINES: usize = 100;
const MAX_LINES: usize = 2000;

#[derive(Serialize)]
struct HostLog {
    path: String,
    exists: bool,
    lines: Vec<String>,
}

pub fn show(ctx: &Ctx, limit: Option<u32>, grep: Option<String>) -> ExitCode {
    let paths = match ctx.paths() {
        Ok(p) => p,
        Err(e) => return ctx.fail(ReplyContext::default(), e),
    };
    let path = paths.host_log();
    let mut text = String::new();
    let exists = match std::fs::File::open(&path) {
        Ok(mut f) => {
            let len = f.metadata().map(|m| m.len()).unwrap_or(0);
            if len > TAIL_BYTES {
                let _ = f.seek(SeekFrom::Start(len - TAIL_BYTES));
            }
            let mut buf = Vec::new();
            let _ = f.read_to_end(&mut buf);
            text = String::from_utf8_lossy(&buf).into_owned();
            if len > TAIL_BYTES {
                // Drop the partial first line.
                text = text
                    .split_once('\n')
                    .map_or(String::new(), |(_, r)| r.to_owned());
            }
            true
        }
        Err(_) => false,
    };
    let want = limit.map_or(DEFAULT_LINES, |n| (n as usize).clamp(1, MAX_LINES));
    let needle = grep.map(|g| g.to_lowercase());
    let matching: Vec<&str> = text
        .lines()
        .filter(|l| needle.as_ref().is_none_or(|n| l.to_lowercase().contains(n)))
        .collect();
    let lines = matching[matching.len().saturating_sub(want)..]
        .iter()
        .map(|l| (*l).to_owned())
        .collect();
    let log = HostLog {
        path: path.display().to_string(),
        exists,
        lines,
    };
    let reply = PublicReply::success(ReplyContext::default(), log, ReplyMeta::default());
    ctx.emit(&reply, |l| {
        if !l.exists {
            return format!(
                "no host log yet ({}); the host writes it when it starts",
                l.path
            );
        }
        let mut out = format!("host log: {}", l.path);
        for line in &l.lines {
            out.push('\n');
            out.push_str(line);
        }
        if l.lines.is_empty() {
            out.push_str("\n(no matching lines)");
        }
        out
    })
}
