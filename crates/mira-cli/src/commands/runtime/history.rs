//! `runs` and `logs`: read run history and run output, optionally following new lines.

use std::io::Write;
use std::process::ExitCode;

use mira_client::ConnectOptions;
use mira_protocol::error::ErrorInfo;
use mira_protocol::ipc::*;
use mira_protocol::reply::PublicReply;
use mira_protocol::run::{Outcome, RunRecord};
use serde_json::Value;

use crate::commands::ctx::{Ctx, block_on};
use crate::commands::runref::{resolve_run_id, resolve_target};
use crate::output::{self, Mode, invalid_argument};

use super::text::{log_line, run_summary, run_text};
use super::{connect, parse_action};

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
