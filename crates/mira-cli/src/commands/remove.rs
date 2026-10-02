//! `mira remove`: take Mira out of one project. It stops the project's work, then deletes
//! `.mira/` and the project's data outside the repository. The binary and skills stay.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use mira_client::{ClientError, ConnectOptions};
use mira_protocol::error::{ErrorCode, ErrorInfo};
use mira_protocol::ipc::*;
use mira_protocol::paths::WorkspacePaths;
use mira_protocol::reply::{PublicReply, ReplyContext, ReplyMeta, WorkspaceRef};
use serde::Serialize;

use super::ctx::{Ctx, block_on};
use crate::output;

const POLL: Duration = Duration::from_millis(100);
const STOP_WAIT: Duration = Duration::from_secs(30);
/// The host exits a few seconds after its last client leaves.
const EXIT_WAIT: Duration = Duration::from_secs(15);

#[derive(Serialize)]
struct Report {
    removed: bool,
    /// Runs that are active (a plan) or that this call stopped.
    active_runs: usize,
    /// Folders this call deletes: `.mira/`, then state, logs, and cache.
    paths: Vec<PathBuf>,
}

impl Report {
    fn list(&self) -> String {
        self.paths
            .iter()
            .map(|p| format!("\n  {}", p.display()))
            .collect()
    }

    fn plan(&self) -> String {
        format!(
            "This stops {} run(s) and deletes:{}",
            self.active_runs,
            self.list()
        )
    }

    fn text(&self) -> String {
        if self.paths.is_empty() {
            "Mira is not set up in this project. Nothing to remove.".into()
        } else if self.removed {
            format!(
                "Removed Mira from this project ({} run(s) stopped):{}\n\
                 The mira program and your agent skills stay installed.",
                self.active_runs,
                self.list()
            )
        } else {
            format!(
                "{}\nNothing was deleted. Run `mira remove --yes` to do it.",
                self.plan()
            )
        }
    }
}

fn still_open() -> ErrorInfo {
    ErrorInfo::new(
        ErrorCode::BUSY,
        "a Mira window is still open for this project; nothing was deleted",
    )
    .with_next_action(
        &["mira", "remove", "--yes"],
        "Close the project's Mira windows, then run this again.",
    )
}

/// Stops the session and waits until the host has exited. Returns the active run count.
async fn stop_host(paths: &WorkspacePaths, yes: bool) -> Result<usize, ErrorInfo> {
    // Never start a host only to remove its project.
    let opts = ConnectOptions {
        spawn: false,
        ..ConnectOptions::cli()
    };
    let mut client = match mira_client::connect(paths, &opts).await {
        Ok(c) => c,
        Err(ClientError::Connect(_)) => return Ok(0),
        Err(e) => return Err(e.to_error_info()),
    };
    let status = client.status().await.map_err(|e| e.to_error_info())?;
    let runs = status.data().map_or(0, |d| d.runs.len());
    if !yes {
        return Ok(runs);
    }
    // Fail before anything stops when a window or a waiting command is connected.
    if status
        .data()
        .and_then(|d| d.session.as_ref())
        .is_some_and(|s| s.controller_count > 0)
    {
        return Err(still_open());
    }
    client
        .call::<_, SessionStopData>(Method::SessionStop, &Empty {})
        .await
        .map_err(|e| e.to_error_info())?;
    let deadline = tokio::time::Instant::now() + STOP_WAIT;
    loop {
        let s = client.status().await.map_err(|e| e.to_error_info())?;
        if s.data().is_some_and(|d| d.session.is_none()) {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(ErrorInfo::new(
                ErrorCode::TIMEOUT,
                "the session is still stopping after 30 s; nothing was deleted",
            ));
        }
        tokio::time::sleep(POLL).await;
    }
    drop(client);
    let deadline = tokio::time::Instant::now() + EXIT_WAIT;
    while paths.owner().exists() {
        if tokio::time::Instant::now() >= deadline {
            return Err(still_open());
        }
        tokio::time::sleep(POLL).await;
    }
    Ok(runs)
}

/// Deletes a file or a folder. A link is removed as a link; its target stays.
pub(super) fn delete(p: &Path) -> std::io::Result<()> {
    if p.symlink_metadata()?.is_dir() {
        std::fs::remove_dir_all(p)
    } else {
        std::fs::remove_file(p)
    }
}

pub fn remove(ctx: &Ctx, yes: bool) -> ExitCode {
    block_on(async {
        let paths = match ctx.paths() {
            Ok(p) => p,
            Err(e) => return ctx.fail(ReplyContext::default(), e),
        };
        let cx = ReplyContext {
            workspace: Some(WorkspaceRef {
                id: paths.id.clone(),
                root: paths.root.clone(),
            }),
            ..ReplyContext::default()
        };
        let folders = [
            &paths.mira_dir,
            &paths.state_dir,
            &paths.logs_dir,
            &paths.cache_dir,
        ];
        let mut report = Report {
            removed: false,
            active_runs: 0,
            // `symlink_metadata` also finds a `.mira` that is a broken link.
            paths: folders
                .into_iter()
                .filter(|p| p.symlink_metadata().is_ok())
                .cloned()
                .collect(),
        };
        // A person at a terminal sees the plan and types yes; other callers pass --yes.
        let mut yes = yes;
        if !yes && !report.paths.is_empty() && output::interactive(ctx.mode) {
            match stop_host(&paths, false).await {
                Ok(runs) => report.active_runs = runs,
                Err(e) => return ctx.fail(cx, e),
            }
            println!("{}", report.plan());
            if !output::confirm("Type yes to remove Mira from this project: ") {
                println!("Nothing was deleted.");
                return ExitCode::SUCCESS;
            }
            yes = true;
        }
        match stop_host(&paths, yes).await {
            Ok(runs) => report.active_runs = runs,
            Err(e) => return ctx.fail(cx, e),
        }
        if yes && !report.paths.is_empty() {
            for p in &report.paths {
                if let Err(e) = delete(p) {
                    let msg = format!("cannot delete {}: {e}", p.display());
                    return ctx.fail(cx, ErrorInfo::new(ErrorCode::INTERNAL, msg));
                }
            }
            for p in [paths.socket(), paths.lock(), paths.owner()] {
                let _ = std::fs::remove_file(p);
            }
            report.removed = true;
        }
        ctx.emit(
            &PublicReply::success(cx, report, ReplyMeta::default()),
            Report::text,
        )
    })
}
