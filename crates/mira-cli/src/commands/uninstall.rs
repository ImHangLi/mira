//! `mira uninstall`: remove Mira from this machine. It stops every project host, then
//! deletes the data, the exported skills, the agent notes, the PATH line, and the binary.
//! Project `.mira/` folders are project files and stay.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

use mira_protocol::error::{ErrorCode, ErrorInfo};
use mira_protocol::paths::{current_uid, user_bases};
use mira_protocol::reply::{PublicReply, ReplyContext, ReplyMeta};
use serde::Serialize;

use super::ctx::Ctx;
use super::remove::delete;
use crate::output;

const INSTALLER_MARK: &str = "# Added by the Mira installer";
const STOP_WAIT: Duration = Duration::from_secs(20);

#[derive(Serialize)]
struct Report {
    uninstalled: bool,
    /// Project hosts that run (a plan) or that this call stopped.
    running_hosts: usize,
    /// Files and folders this call deletes.
    paths: Vec<PathBuf>,
    /// Files that keep their other content and lose only their Mira lines.
    edited: Vec<PathBuf>,
    /// A binary the installer did not place; it stays.
    kept_binary: Option<PathBuf>,
    warnings: Vec<String>,
}

fn lines(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| format!("\n  {}", p.display()))
        .collect()
}

impl Report {
    fn plan(&self) -> String {
        let mut out = format!(
            "This uninstalls Mira from this machine.\n\
             It stops {} running project host(s) and their runs, and deletes:{}",
            self.running_hosts,
            lines(&self.paths)
        );
        if !self.edited.is_empty() {
            out.push_str(&format!(
                "\nIt removes the Mira lines from:{}",
                lines(&self.edited)
            ));
        }
        if let Some(b) = &self.kept_binary {
            out.push_str(&format!(
                "\nThe installer did not place this binary, so it stays:\n  {}",
                b.display()
            ));
        }
        out.push_str("\nProject .mira/ folders stay. Delete them yourself if you want.");
        out
    }

    fn text(&self) -> String {
        if !self.uninstalled {
            return format!(
                "{}\nNothing was deleted. Run `mira uninstall --yes` to do it.",
                self.plan()
            );
        }
        let mut out = "Mira is uninstalled.".to_owned();
        for w in &self.warnings {
            out.push_str(&format!("\nwarning: {w}"));
        }
        out
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

fn alive(pid: u32) -> bool {
    Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// The running hosts, from the owner records in the runtime folder.
fn hosts(runtime: &Path) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir(runtime) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "owner"))
        .filter_map(|e| {
            let v: serde_json::Value =
                serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok()?;
            u32::try_from(v.get("pid")?.as_u64()?).ok()
        })
        .filter(|pid| alive(*pid))
        .collect()
}

/// A host stops its runs on SIGTERM, each within its grace period.
fn stop(pids: &[u32]) -> Result<(), ErrorInfo> {
    for pid in pids {
        let _ = Command::new("/bin/kill")
            .args(["-TERM", &pid.to_string()])
            .output();
    }
    let deadline = Instant::now() + STOP_WAIT;
    while let Some(pid) = pids.iter().find(|p| alive(**p)) {
        if Instant::now() >= deadline {
            return Err(ErrorInfo::new(
                ErrorCode::BUSY,
                format!("project host {pid} is still stopping; nothing was deleted"),
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

/// `text` without each line that `drop` matches and, with `and_next`, the line after it.
/// A blank line just before a dropped line goes too. `None` when nothing matches.
fn strip(text: &str, drop: impl Fn(&str) -> bool, and_next: bool) -> Option<String> {
    let mut kept: Vec<&str> = Vec::new();
    let mut skip = false;
    let mut changed = false;
    for line in text.lines() {
        if std::mem::take(&mut skip) {
            continue;
        }
        if drop(line.trim()) {
            if kept.last().is_some_and(|l| l.trim().is_empty()) {
                kept.pop();
            }
            skip = and_next;
            changed = true;
        } else {
            kept.push(line);
        }
    }
    changed.then(|| {
        let mut out = kept.join("\n");
        if !out.is_empty() {
            out.push('\n');
        }
        out
    })
}

/// A file to edit, with its content after the Mira lines are gone.
fn edit(path: PathBuf, drop: impl Fn(&str) -> bool, and_next: bool) -> Option<(PathBuf, String)> {
    let text = std::fs::read_to_string(&path).ok()?;
    strip(&text, drop, and_next).map(|new| (path, new))
}

pub fn uninstall(ctx: &Ctx, yes: bool) -> ExitCode {
    let home = home();
    let (state, logs, cache) = user_bases();
    let runtime = std::env::var_os("MIRA_RUNTIME_DIR").map_or_else(
        || PathBuf::from(format!("/tmp/mira-{}", current_uid())),
        PathBuf::from,
    );
    let mut warnings = Vec::new();

    // Read the skill targets before the state folder goes.
    let targets = super::skills::targets().unwrap_or_else(|e| {
        warnings.push(format!("cannot read the skill export targets: {e}"));
        Vec::new()
    });
    let mut paths: Vec<PathBuf> = targets
        .iter()
        .flat_map(|t| super::skills::SKILLS.iter().map(move |s| t.join(s)))
        .filter(|p| p.join("SKILL.md").is_file())
        .collect();
    paths.extend([
        home.join(".claude/MIRA.md"),
        home.join(".codex/MIRA.md"),
        home.join(".config/fish/conf.d/mira.fish"),
        state,
        logs,
        cache,
        runtime.clone(),
    ]);
    // The binary goes last, and only when the installer placed it.
    let (bin_dir, kept_binary) = match super::update::installed() {
        Ok(dir) => (Some(dir), None),
        Err(_) => (None, std::env::current_exe().ok()),
    };
    if let Some(dir) = &bin_dir {
        paths.extend(
            [
                "mira.previous",
                "mira.swap",
                ".mira-update",
                ".mira-installed",
                "mira",
            ]
            .map(|f| dir.join(f)),
        );
    }
    paths.retain(|p| p.symlink_metadata().is_ok());

    // Like the installer: an empty ZDOTDIR counts as not set.
    let zsh = std::env::var_os("ZDOTDIR")
        .filter(|d| !d.is_empty())
        .map_or_else(|| home.clone(), PathBuf::from);
    let installer = |l: &str| l == INSTALLER_MARK;
    let edits: Vec<(PathBuf, String)> = [
        edit(zsh.join(".zshenv"), installer, true),
        edit(home.join(".bash_profile"), installer, true),
        edit(home.join(".claude/CLAUDE.md"), |l| l == "@MIRA.md", false),
        edit(
            home.join(".codex/AGENTS.md"),
            |l| l == "Read ~/.codex/MIRA.md.",
            false,
        ),
    ]
    .into_iter()
    .flatten()
    .collect();

    let pids = hosts(&runtime);
    let mut report = Report {
        uninstalled: false,
        running_hosts: pids.len(),
        paths,
        edited: edits.iter().map(|(p, _)| p.clone()).collect(),
        kept_binary,
        warnings,
    };
    let reply = |r: Report| PublicReply::success(ReplyContext::default(), r, ReplyMeta::default());
    if !yes {
        if !output::interactive(ctx.mode) {
            return ctx.emit(&reply(report), Report::text);
        }
        println!("{}", report.plan());
        if !output::confirm("Type yes to uninstall Mira: ") {
            println!("Nothing was deleted.");
            return ExitCode::SUCCESS;
        }
    }
    if let Err(e) = stop(&pids) {
        return ctx.fail(ReplyContext::default(), e);
    }
    for (path, text) in &edits {
        if let Err(e) = std::fs::write(path, text) {
            report
                .warnings
                .push(format!("cannot edit {}: {e}", path.display()));
        }
    }
    for p in &report.paths {
        if let Err(e) = delete(p) {
            report
                .warnings
                .push(format!("cannot delete {}: {e}", p.display()));
        }
    }
    if let Some(dir) = &bin_dir {
        // Only empty folders go: `~/.mira/bin`, then `~/.mira`.
        if std::fs::remove_dir(dir).is_ok()
            && let Some(parent) = dir.parent()
        {
            let _ = std::fs::remove_dir(parent);
        }
    }
    let skillshare = home.join(".config/skillshare");
    if targets.iter().any(|t| t.starts_with(&skillshare)) {
        // Skillshare then drops the copies it made; without Skillshare nothing is needed.
        let _ = Command::new("skillshare").args(["sync", "-g"]).output();
    }
    report.uninstalled = true;
    ctx.emit(&reply(report), Report::text)
}
