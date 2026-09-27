//! Observe-only commands: `status`, `catalog`, `describe`, `paths`, `doctor`.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use mira_client::ConnectOptions;
use mira_protocol::config::validate_draft;
use mira_protocol::ids::{CatalogRevision, ItemRef, WorkspaceId};
use mira_protocol::ipc::*;
use mira_protocol::manifest::Runner;
use mira_protocol::paths::{WorkspacePaths, current_uid};
use mira_protocol::reply::{PublicReply, ReplyContext, ReplyMeta};
use mira_protocol::run::Lifecycle;
use mira_protocol::workspace::SelectionReason;
use serde::Serialize;

use super::ctx::{Ctx, block_on};

/// Lower-case Debug text for simple enums.
pub fn lc<T: std::fmt::Debug>(v: &T) -> String {
    format!("{v:?}").to_lowercase()
}

/// The serde name of a simple enum with spaces for underscores: `timed out`, `state db`.
pub fn words<T: Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(|s| s.replace('_', " ")))
        .unwrap_or_default()
}

pub fn lifecycle_text(l: &Lifecycle) -> String {
    match l {
        Lifecycle::Stopping { reason } => format!("stopping ({})", words(reason)),
        Lifecycle::Finished { .. } => format!("finished: {}", l.word()),
        other => other.word().into(),
    }
}

/// `service` for process actions, `task` for tasks.
pub fn mode_text(m: mira_protocol::manifest::ActionMode) -> &'static str {
    match m {
        mira_protocol::manifest::ActionMode::Process => "service",
        mira_protocol::manifest::ActionMode::Task => "task",
    }
}

pub fn path_label(c: PathClass) -> &'static str {
    match c {
        PathClass::WorkspaceConfig => "project config",
        PathClass::Plugins => "plugins",
        PathClass::LocalConfig => "local config",
        PathClass::Drafts => "drafts",
        PathClass::StateDb => "state database",
        PathClass::FingerprintKey => "fingerprint key",
        PathClass::PluginState => "plugin state",
        PathClass::Artifacts => "artifacts",
        PathClass::Logs => "logs",
        PathClass::HostLog => "host log",
        PathClass::Cache => "cache",
        PathClass::Runtime => "runtime files",
    }
}

pub fn status(ctx: &Ctx) -> ExitCode {
    block_on(async {
        let mut client = match ctx.client(&ConnectOptions::cli()).await {
            Ok(c) => c,
            Err((c, e)) => return ctx.fail(c, e),
        };
        match client.status().await {
            Ok(reply) => ctx.emit(&reply, |s| {
                let mut out = crate::human::session_line(s.session.as_ref());
                if s.runs.is_empty() {
                    out.push_str("\nruns: none active");
                }
                for r in &s.runs {
                    let target = r
                        .action_ref
                        .as_ref()
                        .map_or("exec".to_owned(), ToString::to_string);
                    out.push_str(&format!(
                        "\n  {}  {:<24} {}",
                        r.run_id,
                        target,
                        lifecycle_text(&r.lifecycle)
                    ));
                }
                for sc in &s.schedules {
                    out.push_str(&format!("\nschedule: {}", super::schedule::text(sc)));
                }
                for w in s.storage_warnings.iter().chain(&s.config_warnings) {
                    out.push_str(&format!("\nwarning[{}]: {}", w.code, w.message));
                }
                out
            }),
            Err(e) => ctx.fail(client.context(), e.to_error_info()),
        }
    })
}

pub struct CatalogArgs {
    pub search: Option<String>,
    pub if_revision: Option<u64>,
    pub if_workspace: Option<String>,
    pub limit: Option<u32>,
    pub after: Option<String>,
    pub max_bytes: Option<u32>,
}

pub fn catalog(ctx: &Ctx, args: CatalogArgs) -> ExitCode {
    let CatalogArgs {
        search,
        if_revision,
        if_workspace,
        limit,
        after,
        max_bytes,
    } = args;
    block_on(async {
        let parsed = (|| {
            let rev = if_revision
                .map(CatalogRevision::new)
                .transpose()
                .map_err(|e| e.to_string())?;
            let ws = if_workspace
                .map(WorkspaceId::parse)
                .transpose()
                .map_err(|e| e.to_string())?;
            Ok::<_, String>((rev, ws))
        })();
        let (if_revision, if_workspace) = match parsed {
            Ok(v) => v,
            Err(e) => {
                return ctx.fail(ReplyContext::default(), crate::output::invalid_argument(e));
            }
        };
        let mut client = match ctx.client(&ConnectOptions::cli()).await {
            Ok(c) => c,
            Err((c, e)) => return ctx.fail(c, e),
        };
        let p = CatalogListParams {
            query: search,
            if_revision,
            if_workspace,
            cursor: after,
            limit,
            max_bytes,
        };
        match client
            .call::<_, CatalogList>(Method::CatalogListM, &p)
            .await
        {
            Ok(reply) => {
                let not_modified = reply.meta().not_modified;
                let more = match (&reply.meta().next_cursor, ctx.mode) {
                    (Some(cursor), crate::output::Mode::Text) => {
                        let n = count_rest(&mut client, &p, cursor.clone()).await;
                        Some((n, cursor.clone()))
                    }
                    _ => None,
                };
                ctx.emit(&reply, |c| {
                    if not_modified {
                        return "catalog not modified".to_owned();
                    }
                    if c.items.is_empty() {
                        return "no matching tools".to_owned();
                    }
                    c.items
                        .iter()
                        .map(|i| {
                            let kind = match &i.item {
                                CatalogItemKind::Action { mode } => mode_text(*mode).to_owned(),
                                CatalogItemKind::View { view_kind } => {
                                    format!("{} view", words(view_kind))
                                }
                            };
                            format!(
                                "{:<28} {:<10} {}{}",
                                i.item_ref.to_string(),
                                kind,
                                i.title,
                                if i.enabled { "" } else { " (disabled)" }
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                        + &more
                            .map(|(n, cursor)| format!("\n{n} more: mira catalog --after {cursor}"))
                            .unwrap_or_default()
                })
            }
            Err(e) => ctx.fail(client.context(), e.to_error_info()),
        }
    })
}

/// Counts the catalog items after `cursor` for the `N more` line (text mode only).
async fn count_rest(
    client: &mut mira_client::Client,
    p: &CatalogListParams,
    cursor: String,
) -> String {
    let mut n = 0usize;
    let mut next = Some(cursor);
    for _ in 0..20 {
        let Some(c) = next.take() else {
            return n.to_string();
        };
        let page = CatalogListParams {
            query: p.query.clone(),
            if_revision: None,
            if_workspace: None,
            cursor: Some(c),
            limit: Some(200),
            max_bytes: Some(256 * 1024),
        };
        match client
            .call::<_, CatalogList>(Method::CatalogListM, &page)
            .await
        {
            Ok(r) if r.is_ok() => {
                n += r.data().map_or(0, |d| d.items.len());
                next = r.meta().next_cursor.clone();
            }
            _ => break,
        }
    }
    if next.is_some() || n == 0 {
        format!("{n}+")
    } else {
        n.to_string()
    }
}

pub fn describe(ctx: &Ctx, item: String, include_schema: bool, max_bytes: Option<u32>) -> ExitCode {
    block_on(async {
        let item_ref: ItemRef = match item.parse() {
            Ok(r) => r,
            Err(e) => {
                return ctx.fail(
                    ReplyContext::default(),
                    crate::output::invalid_argument(format!("{e}: `{item}`")),
                );
            }
        };
        let mut client = match ctx.client(&ConnectOptions::cli()).await {
            Ok(c) => c,
            Err((c, e)) => return ctx.fail(c, e),
        };
        match client
            .call::<_, ItemDescription>(
                Method::ItemDescribe,
                &ItemDescribeParams {
                    item_ref,
                    include_schema,
                    max_bytes,
                },
            )
            .await
        {
            Ok(reply) => {
                let schedule = match reply.data() {
                    Some(d)
                        if ctx.mode == crate::output::Mode::Text
                            && d.action.as_ref().is_some_and(|a| a.has_schedule) =>
                    {
                        Some(schedule_line(ctx, &mut client, &d.item.item_ref.to_string()).await)
                    }
                    _ => None,
                };
                ctx.emit(&reply, |d| {
                    let mut s = format!(
                        "{}  {}\n  {}\n  plugin: {} ({})",
                        d.item.item_ref,
                        d.item.title,
                        d.item.description,
                        d.plugin.name,
                        d.plugin.id
                    );
                    if let Some(a) = &d.action {
                        s.push_str(&format!(
                            "\n  {} ({} runner), terminal {}, cwd {}",
                            mode_text(a.mode),
                            a.runner,
                            words(&a.terminal),
                            a.cwd
                        ));
                        if let Some(line) = &schedule {
                            s.push_str(&format!("\n  schedule: {line}"));
                        }
                        if !a.effects.is_empty() {
                            s.push_str(&format!("\n  effects: {}", a.effects.join(", ")));
                        }
                    }
                    if let Some(v) = &d.view {
                        s.push_str(&format!(
                            "\n  {} view, kept {}",
                            words(&v.view_kind),
                            match v.persistence {
                                mira_protocol::manifest::Persistence::Session => "for the session",
                                mira_protocol::manifest::Persistence::Last => "until replaced",
                            }
                        ));
                    }
                    s.push_str(&format!("\n  try: {}", d.invoke_hint.join(" ")));
                    s
                })
            }
            Err(e) => ctx.fail(client.context(), e.to_error_info()),
        }
    })
}

/// `every 24h, off`: the interval from the accepted schedule state, or from the definition
/// on disk when the switch was never set.
async fn schedule_line(ctx: &Ctx, client: &mut mira_client::Client, action: &str) -> String {
    let known = match client.status().await {
        Ok(r) => r
            .data()
            .and_then(|s| {
                s.schedules
                    .iter()
                    .find(|x| x.action_ref.to_string() == action)
            })
            .map(|x| (x.every_ms, x.enabled)),
        Err(_) => None,
    };
    let every = known.map(|(ms, _)| ms).or_else(|| {
        let paths = ctx.paths().ok()?;
        let Ok(mira_protocol::config::Draft::Workspace(set)) = validate_draft(&paths.mira_dir)
        else {
            return None;
        };
        set.plugins.iter().find_map(|lp| {
            lp.plugin.actions.iter().find_map(|a| {
                (format!("{}.{}", lp.plugin.id, a.id) == action)
                    .then(|| a.schedule.as_ref().map(|s| s.every_ms))
                    .flatten()
            })
        })
    });
    let on = if known.is_some_and(|(_, on)| on) {
        "on"
    } else {
        "off"
    };
    match every {
        Some(ms) => format!("{}, {on}", crate::human::interval(ms)),
        None => on.to_owned(),
    }
}

pub fn paths(ctx: &Ctx) -> ExitCode {
    // Path computation is deterministic, so no host is needed.
    let p = match ctx.paths() {
        Ok(p) => p,
        Err(e) => return ctx.fail(ReplyContext::default(), e),
    };
    let data = p.to_data();
    let reply = PublicReply::success(
        ReplyContext {
            workspace: Some(data.workspace.clone()),
            ..ReplyContext::default()
        },
        data,
        ReplyMeta::default(),
    );
    ctx.emit(&reply, |d| {
        d.entries
            .iter()
            .map(|e| {
                format!(
                    "{:<16} {}\n                 {}",
                    path_label(e.class),
                    e.path,
                    e.purpose
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    })
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum CheckStatus {
    Ok,
    Warn,
    Fail,
    Info,
}

#[derive(Serialize)]
struct Check {
    name: String,
    status: CheckStatus,
    message: String,
}

#[derive(Serialize)]
struct DoctorData {
    version: &'static str,
    protocol_hash: String,
    checks: Vec<Check>,
}

fn on_path(program: &str, root: &Path) -> bool {
    if program.contains('/') {
        let p = Path::new(program);
        return if p.is_absolute() {
            p.exists()
        } else {
            root.join(p).exists()
        };
    }
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|d| d.join(program).is_file()))
}

/// The usual per-user skill folders `mira skills export` writes to (never a project).
const SKILL_DIRS: [&str; 4] = [
    ".claude/skills",
    ".agents/skills",
    ".codex/skills",
    ".config/skillshare/skills",
];

/// One info line naming the skill folders that hold exported Mira skills.
fn skills_checks(push: &mut impl FnMut(&str, CheckStatus, String)) {
    let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) else {
        return;
    };
    let home = PathBuf::from(home);
    let found: Vec<String> = SKILL_DIRS
        .iter()
        .filter(|rel| home.join(rel).join("mira/SKILL.md").is_file())
        .map(|rel| format!("~/{rel}"))
        .collect();
    let message = if found.is_empty() {
        "no exported Mira skills found in ~/.claude/skills, ~/.agents/skills, \
         ~/.codex/skills, or ~/.config/skillshare/skills; see \
         https://github.com/ImHangLi/mira/blob/main/docs/agents.md"
            .into()
    } else {
        format!("skills found at {}", found.join(", "))
    };
    push("skills", CheckStatus::Info, message);
}

pub fn doctor(ctx: &Ctx) -> ExitCode {
    block_on(async {
        let mut checks = Vec::new();
        let mut push = |name: &str, status: CheckStatus, message: String| {
            checks.push(Check {
                name: name.to_owned(),
                status,
                message,
            })
        };
        let selected = match ctx.select() {
            Ok(s) => s,
            Err(e) => return ctx.fail(ReplyContext::default(), e),
        };
        let project_text = format!(
            "{} ({})",
            selected.root,
            match selected.reason {
                SelectionReason::ExplicitProject => "chosen with --project",
                SelectionReason::MiraConfig => "has .mira/workspace.json",
                SelectionReason::GitWorktree => "git repository root",
                SelectionReason::ProjectManifest => "has a project manifest",
            }
        );
        push(
            "workspace",
            CheckStatus::Ok,
            format!("{} ({})", selected.root, lc(&selected.reason)),
        );
        let paths = WorkspacePaths::new(selected.root.clone());
        let mut set = None;
        if selected.is_setup() {
            match validate_draft(&paths.mira_dir) {
                Ok(mira_protocol::config::Draft::Workspace(s)) => {
                    push(
                        "config",
                        CheckStatus::Ok,
                        format!("{} plugin(s) valid", s.plugins.len()),
                    );
                    set = Some(s);
                }
                Ok(_) => push("config", CheckStatus::Fail, "unexpected draft kind".into()),
                Err(i) => push("config", CheckStatus::Fail, i.to_error_info().message),
            }
        } else {
            push(
                "config",
                CheckStatus::Warn,
                format!(
                    "not set up: .mira/workspace.json is missing. {}",
                    mira_protocol::error::NOT_SETUP_HINT
                ),
            );
        }
        skills_checks(&mut push);
        match std::fs::symlink_metadata(&paths.runtime_dir) {
            Ok(m)
                if m.is_dir()
                    && m.uid() == current_uid()
                    && m.permissions().mode() & 0o077 == 0 =>
            {
                push(
                    "runtime_dir",
                    CheckStatus::Ok,
                    paths.runtime_dir.display().to_string(),
                )
            }
            Ok(_) => push(
                "runtime_dir",
                CheckStatus::Fail,
                format!(
                    "{} must be a 0700 directory owned by you",
                    paths.runtime_dir.display()
                ),
            ),
            Err(_) => push("runtime_dir", CheckStatus::Ok, "not created yet".into()),
        }
        match mira_client::connect(&paths, &ConnectOptions::cli()).await {
            Ok(c) => push(
                "host",
                CheckStatus::Ok,
                format!("reachable, epoch {}", c.hello().host_epoch),
            ),
            Err(e) => push("host", CheckStatus::Fail, e.to_string()),
        }
        if let Some(set) = &set {
            for lp in &set.plugins {
                // (label, program, directory a relative program path resolves against)
                let mut programs: Vec<(String, String, PathBuf)> = Vec::new();
                if let Some(e) = &lp.plugin.entry {
                    programs.push((
                        format!("{} entry", lp.plugin.id),
                        e.program().to_owned(),
                        lp.dir.as_path().to_path_buf(),
                    ));
                }
                for a in &lp.plugin.actions {
                    if let Runner::Command { argv } = &a.run {
                        // Commands run in the action cwd, which is relative to the root.
                        programs.push((
                            format!("{}.{}", lp.plugin.id, a.id),
                            argv.program().to_owned(),
                            paths.root.as_path().join(&a.cwd),
                        ));
                    }
                }
                for (what, program, base) in programs {
                    if !on_path(&program, &base) {
                        let place = if program.contains('/') {
                            ""
                        } else {
                            " on PATH"
                        };
                        push(
                            "executable",
                            CheckStatus::Warn,
                            format!("{what}: `{program}` was not found{place}"),
                        );
                    }
                }
            }
        }
        let data = DoctorData {
            version: mira_protocol::VERSION,
            protocol_hash: mira_protocol::schemas::protocol_hash().to_string(),
            checks,
        };
        let failed = data
            .checks
            .iter()
            .any(|c| matches!(c.status, CheckStatus::Fail));
        let reply = PublicReply::success(ReplyContext::default(), data, ReplyMeta::default());
        let code = ctx.emit(&reply, |d| {
            d.checks
                .iter()
                .map(|c| {
                    let tag = match c.status {
                        CheckStatus::Ok => "ok  ",
                        CheckStatus::Warn => "warn",
                        CheckStatus::Fail => "FAIL",
                        CheckStatus::Info => "info",
                    };
                    let ok = matches!(c.status, CheckStatus::Ok);
                    let (label, message) = match c.name.as_str() {
                        "workspace" => ("project", project_text.clone()),
                        "config" => ("plugins", c.message.clone()),
                        "runtime_dir" => ("runtime files", c.message.clone()),
                        "host" if ok => ("Mira", "Mira is running.".to_owned()),
                        "host" => ("Mira", c.message.clone()),
                        "executable" => ("program", c.message.clone()),
                        other => (other, c.message.clone()),
                    };
                    format!("[{tag}] {label:<13} {message}")
                })
                .collect::<Vec<_>>()
                .join("\n")
        });
        // The report itself succeeded; exit 1 tells scripts that a check failed.
        if failed { ExitCode::from(1) } else { code }
    })
}
