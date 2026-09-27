//! Text renderings of runs and log records.

use mira_client::Client;
use mira_protocol::error::ErrorInfo;
use mira_protocol::ipc::*;
use mira_protocol::run::{CleanupState, Lifecycle, RunRecord, StopReason};

use crate::commands::inspect::lifecycle_text;
use crate::human;

pub(super) fn run_text(r: &RunRecord) -> String {
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
pub(super) fn run_summary(r: &RunRecord) -> String {
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
pub(super) async fn failure_text(client: &mut Client, rec: &RunRecord, e: &ErrorInfo) -> String {
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

pub(super) fn stream_tag(r: &mira_protocol::run::LogRecord) -> &'static str {
    match r.stream {
        mira_protocol::run::LogStream::Stderr => "err ",
        mira_protocol::run::LogStream::Host => "mira",
        mira_protocol::run::LogStream::Plugin => "plug",
        mira_protocol::run::LogStream::Pty => "pty ",
        mira_protocol::run::LogStream::Stdout => "out ",
    }
}

pub(super) fn log_line(r: &mira_protocol::run::LogRecord) -> String {
    format!("{:>6} {} {}", r.log_seq, stream_tag(r), r.text)
}
