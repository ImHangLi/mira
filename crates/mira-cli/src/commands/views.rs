//! Typed views, publishes, row actions, and artifacts (§8, §10.2). The CLI renders the same
//! ViewData the host stores; it never re-runs a plugin to read a view.

use std::process::ExitCode;

use mira_protocol::error::ErrorInfo;
use mira_protocol::ids::{ActionId, RequestKey, RunId, ViewRef, ViewRevision};
use mira_protocol::ipc::*;
use mira_protocol::manifest::ViewSourceWire;
use mira_protocol::reply::ReplyContext;
use mira_protocol::view::{
    Freshness, LogLevel, SourceKind, TreeNode, ViewBody, ViewData, ViewSnapshot,
};
use serde_json::Value;

use super::ctx::{Ctx, block_on};
use super::runtime::{connect, env, read_input, run_and_wait};
use crate::output::invalid_argument;

fn parse_view(s: &str) -> Result<ViewRef, ErrorInfo> {
    ViewRef::parse(s.to_owned()).map_err(|e| invalid_argument(format!("{e}: `{s}`")))
}

fn parse_revision(n: u64) -> Result<ViewRevision, ErrorInfo> {
    ViewRevision::new(n).map_err(|e| invalid_argument(e.to_string()))
}

fn cell(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn tree_lines(nodes: &[TreeNode], depth: usize, out: &mut Vec<String>) {
    for n in nodes {
        out.push(format!("{}{}", "  ".repeat(depth), n.label));
        tree_lines(&n.children, depth + 1, out);
    }
}

fn data_text(d: &ViewData) -> String {
    match d {
        ViewData::Text { text, .. } => text.clone(),
        ViewData::Table { columns, rows } => {
            let mut lines = vec![format!(
                "id\t{}",
                columns
                    .iter()
                    .map(|c| c.label.as_str())
                    .collect::<Vec<_>>()
                    .join("\t")
            )];
            for r in rows {
                let cells: Vec<String> = columns
                    .iter()
                    .map(|c| r.values.get(&c.id).map(cell).unwrap_or_default())
                    .collect();
                lines.push(format!("{}\t{}", r.id, cells.join("\t")));
            }
            lines.join("\n")
        }
        ViewData::Log { items } => items
            .iter()
            .map(|i| {
                let at = i
                    .recorded_at
                    .map(|t| crate::human::clock().hms(t))
                    .unwrap_or_default();
                // Info is the normal case: show only the levels that carry meaning.
                if i.level == LogLevel::Info {
                    format!("{at} {}", i.text)
                } else {
                    format!(
                        "{at} {:<5} {}",
                        format!("{:?}", i.level).to_lowercase(),
                        i.text
                    )
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        ViewData::Tree { nodes } => {
            let mut out = Vec::new();
            tree_lines(nodes, 0, &mut out);
            out.join("\n")
        }
        ViewData::Json { value } => {
            serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
        }
    }
}

/// Where the data came from and when: `from run r_fb60aacf (ended), 14:07`.
fn source_text(s: &ViewSnapshot) -> String {
    let at = s
        .recorded_at
        .map(|t| format!(", {}", crate::human::clock().when(t)))
        .unwrap_or_default();
    if s.freshness == Freshness::Stale {
        let why = s
            .freshness_reason
            .as_deref()
            .unwrap_or("it may be out of date");
        return format!("stale: {why}{at}");
    }
    match (&s.source_run_id, s.source_kind) {
        (Some(id), _) => {
            let state = if s.freshness == Freshness::Current {
                "running"
            } else {
                "ended"
            };
            format!("from run {} ({state}){at}", crate::human::short_run(id))
        }
        (None, Some(SourceKind::Cli)) => format!("published from the CLI{at}"),
        (None, Some(SourceKind::Hook)) => format!("published by a hook{at}"),
        (None, Some(SourceKind::Plugin)) => format!("published by a plugin{at}"),
        (None, None) => format!("no source{at}"),
    }
}

/// A derived view's head: `● live · from dev.web (running) · 17 lines · filter "error"`.
/// The revision and durability are host bookkeeping and are left to `--json`.
fn derived_head(s: &ViewSnapshot, src: &ViewSourceWire) -> String {
    let n = match &s.data {
        Some(ViewBody::Inline(ViewData::Log { items })) => items.len(),
        _ => 0,
    };
    let lines = format!(
        "{n} line{}{}",
        if n == 1 { "" } else { "s" },
        src.filter_words()
    );
    if s.freshness == Freshness::Current {
        format!("● live · from {} (running) · {lines}", src.logs)
    } else {
        let ended = s
            .recorded_at
            .map(|t| format!(" {}", crate::human::clock().when(t)))
            .unwrap_or_default();
        format!("○ from {} (ended{ended}) · {lines}", src.logs)
    }
}

fn snapshot_text(s: &ViewSnapshot, source: Option<&ViewSourceWire>) -> String {
    let head = match (s.view_revision, source) {
        (Some(_), Some(src)) => format!("{}\n  {}", s.view_ref, derived_head(s, src)),
        (r, _) => snapshot_head(s, r),
    };
    let body = match &s.data {
        Some(ViewBody::Inline(d)) => data_text(d),
        Some(ViewBody::Reference(r)) => r.summary.clone(),
        None => String::new(),
    };
    if body.is_empty() {
        head
    } else {
        format!("{head}\n\n{body}")
    }
}

fn snapshot_head(s: &ViewSnapshot, revision: Option<ViewRevision>) -> String {
    match revision {
        None => format!(
            "{}  no data yet{}",
            s.view_ref,
            s.freshness_reason
                .as_deref()
                .map(|r| format!(" ({r})"))
                .unwrap_or_default()
        ),
        Some(r) => format!("{}  revision {r}\n  {}", s.view_ref, source_text(s)),
    }
}

pub fn view(
    ctx: &Ctx,
    view: &str,
    after: Option<String>,
    limit: Option<u32>,
    max_bytes: Option<u32>,
) -> ExitCode {
    block_on(async {
        let view_ref = match parse_view(view) {
            Ok(v) => v,
            Err(e) => return ctx.fail(ReplyContext::default(), e),
        };
        let mut client = match connect(ctx).await {
            Ok(c) => c,
            Err(code) => return code,
        };
        let p = ViewReadParams {
            view_ref: view_ref.clone(),
            cursor: after,
            limit,
            max_bytes,
        };
        match client.call::<_, ViewSnapshot>(Method::ViewRead, &p).await {
            Ok(r) => {
                // A run source without a producer kind is a derived log view: its text head
                // names the source action and filter, which only the definition holds.
                let derived = r
                    .data()
                    .is_some_and(|s| s.source_run_id.is_some() && s.source_kind.is_none());
                let source = if derived {
                    let d = ItemDescribeParams {
                        item_ref: view_ref.to_item_ref(),
                        include_schema: false,
                        max_bytes: None,
                    };
                    client
                        .call::<_, ItemDescription>(Method::ItemDescribe, &d)
                        .await
                        .ok()
                        .and_then(|d| d.data().and_then(|d| d.view.as_ref()?.source.clone()))
                } else {
                    None
                };
                ctx.emit(&r, |s| snapshot_text(s, source.as_ref()))
            }
            Err(e) => ctx.fail(client.context(), e.to_error_info()),
        }
    })
}

pub fn publish(
    ctx: &Ctx,
    view: &str,
    input: &str,
    expected: Option<u64>,
    request_key: Option<String>,
) -> ExitCode {
    block_on(async {
        let prepared = (|| {
            Ok::<_, ErrorInfo>((
                parse_view(view)?,
                read_input(Some(input))?,
                expected.map(parse_revision).transpose()?,
                request_key
                    .map(RequestKey::parse)
                    .transpose()
                    .map_err(|e| invalid_argument(e.to_string()))?,
            ))
        })();
        let (view_ref, frame, expected_view_revision, request_key) = match prepared {
            Ok(v) => v,
            Err(e) => return ctx.fail(ReplyContext::default(), e),
        };
        let mut client = match connect(ctx).await {
            Ok(c) => c,
            Err(code) => return code,
        };
        let p = ViewPublishParams {
            view_ref,
            frame,
            source_kind: SourceKind::Cli,
            expected_view_revision,
            request_key,
        };
        match client
            .call::<_, PublishResult>(Method::ViewPublish, &p)
            .await
        {
            Ok(r) => ctx.emit(&r, |d| {
                if d.reused {
                    format!("already published (revision {})", d.view_revision)
                } else {
                    format!("published (revision {})", d.view_revision)
                }
            }),
            Err(e) => ctx.fail(client.context(), e.to_error_info()),
        }
    })
}

pub fn view_action(ctx: &Ctx, view: &str, action: &str, row: String, expected: u64) -> ExitCode {
    block_on(async {
        let prepared = (|| {
            Ok::<_, ErrorInfo>((
                parse_view(view)?,
                ActionId::parse(action.to_owned())
                    .map_err(|e| invalid_argument(format!("{e}: `{action}`")))?,
                parse_revision(expected)?,
                env()?,
            ))
        })();
        let (view_ref, action, expected_view_revision, client_env) = match prepared {
            Ok(v) => v,
            Err(e) => return ctx.fail(ReplyContext::default(), e),
        };
        let mut client = match connect(ctx).await {
            Ok(c) => c,
            Err(code) => return code,
        };
        let p = ViewActionParams {
            view_ref,
            action,
            row,
            expected_view_revision,
            client_env,
        };
        let params = serde_json::to_value(p).unwrap_or(Value::Null);
        run_and_wait(ctx, &mut client, Method::ViewAction, params, true).await
    })
}

fn artifact_text(a: &ArtifactInfo) -> String {
    format!(
        "{}  {}  {}  {}  {}{}",
        a.id,
        serde_json::to_value(a.ownership)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default(),
        serde_json::to_value(a.state)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default(),
        a.size_bytes
            .map_or_else(|| "-".to_owned(), |b| format!("{b} B")),
        a.path,
        if a.label.is_empty() {
            String::new()
        } else {
            format!("  ({})", a.label)
        }
    )
}

pub fn artifacts(ctx: &Ctx, run: Option<String>) -> ExitCode {
    block_on(async {
        let run_id = match run.map(RunId::parse).transpose() {
            Ok(r) => r,
            Err(e) => return ctx.fail(ReplyContext::default(), invalid_argument(e.to_string())),
        };
        let mut client = match connect(ctx).await {
            Ok(c) => c,
            Err(code) => return code,
        };
        match client
            .call::<_, ArtifactListData>(Method::ArtifactList, &ArtifactListParams { run_id })
            .await
        {
            Ok(r) => ctx.emit(&r, |d| {
                if d.artifacts.is_empty() {
                    "no artifacts".into()
                } else {
                    d.artifacts
                        .iter()
                        .map(artifact_text)
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            }),
            Err(e) => ctx.fail(client.context(), e.to_error_info()),
        }
    })
}

pub fn artifact_read(ctx: &Ctx, id: String, offset: u64, max_bytes: Option<u32>) -> ExitCode {
    block_on(async {
        let mut client = match connect(ctx).await {
            Ok(c) => c,
            Err(code) => return code,
        };
        let p = ArtifactReadParams {
            artifact_id: id,
            offset,
            max_bytes,
        };
        match client.call::<_, ChunkData>(Method::ArtifactRead, &p).await {
            Ok(r) => ctx.emit(&r, |c| c.text.clone()),
            Err(e) => ctx.fail(client.context(), e.to_error_info()),
        }
    })
}
