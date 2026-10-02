//! Replace installer-owned binaries after checksum and executable checks.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Output};

use mira_protocol::reply::{PublicReply, ReplyContext, ReplyMeta};
use mira_protocol::update::{Cache, curl, latest, newer, now_ms, version};
use mira_protocol::{ErrorCode, ErrorInfo, VERSION};
use serde::Serialize;

use super::ctx::Ctx;

const INSTALL: &str =
    "curl -fsSL https://raw.githubusercontent.com/ImHangLi/mira/main/scripts/install.sh | sh";

#[derive(Serialize)]
struct Report {
    version: String,
    latest: Option<String>,
    update_available: bool,
    message: String,
    release_notes: Option<String>,
    notice: Option<String>,
    updated_targets: Vec<PathBuf>,
    warnings: Vec<String>,
    hosts: Vec<Host>,
}

#[derive(Serialize)]
struct Host {
    pid: u32,
    root: String,
    stop_command: String,
}

impl Report {
    fn new(version: &str, message: String) -> Self {
        Self {
            version: version.into(),
            latest: None,
            update_available: false,
            message,
            release_notes: None,
            notice: None,
            updated_targets: Vec::new(),
            warnings: Vec::new(),
            hosts: Vec::new(),
        }
    }

    fn text(&self) -> String {
        let mut out = self.message.clone();
        for target in &self.updated_targets {
            out.push_str(&format!("\nskills updated: {}", target.display()));
        }
        for warning in &self.warnings {
            out.push_str(&format!("\nwarning: {warning}"));
        }
        if let Some(notes) = &self.release_notes {
            out.push_str(&format!("\n\n{notes}"));
        }
        if let Some(notice) = &self.notice {
            out.push_str(&format!("\n\n{notice}"));
        }
        for host in &self.hosts {
            out.push_str(&format!(
                "\n{} (pid {}): switches when its runs stop, or stop it now with:\n  {}",
                host.root, host.pid, host.stop_command
            ));
        }
        out
    }
}

fn run(command: &mut Command, stage: &str) -> Result<Output, String> {
    let output = command.output().map_err(|e| format!("{stage}: {e}"))?;
    if !output.status.success() {
        return Err(format!("{stage}: command failed ({})", output.status));
    }
    Ok(output)
}

pub(super) fn installed() -> Result<PathBuf, String> {
    let exe = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|e| format!("replace: cannot locate this binary: {e}"))?;
    let dir = exe
        .parent()
        .ok_or("replace: binary has no parent directory")?;
    if !dir.join(".mira-installed").is_file() || exe.file_name().is_none_or(|n| n != "mira") {
        return Err(format!(
            "How this copy of Mira was installed is unknown. Rerun the install command:\n{INSTALL}"
        ));
    }
    Ok(dir.to_owned())
}

/// A fixed directory also prevents two updaters from replacing the binary together.
struct Scratch(PathBuf);

impl Scratch {
    fn new(dir: &Path) -> Result<Self, String> {
        let path = dir.join(".mira-update");
        // An update takes seconds; an older folder is left over from a crash.
        let stale = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .is_ok_and(|t| t.elapsed().is_ok_and(|age| age.as_secs() > 600));
        if stale {
            let _ = std::fs::remove_dir_all(&path);
        }
        std::fs::create_dir(&path).map_err(|e| {
            format!(
                "replace: cannot create {} (another update may be running): {e}",
                path.display()
            )
        })?;
        Ok(Self(path))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn binary_version(binary: &Path) -> Result<String, String> {
    let output = run(
        Command::new(binary).arg("--version"),
        "extract: executable check",
    )?;
    let text = String::from_utf8(output.stdout)
        .map_err(|e| format!("extract: invalid version output: {e}"))?;
    let found = text
        .trim()
        .strip_prefix("mira ")
        .ok_or("extract: expected mira VERSION")?;
    version(found).ok_or("extract: invalid binary version")?;
    Ok(found.to_owned())
}

fn rename(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::rename(from, to)
        .map_err(|e| format!("replace: {} -> {}: {e}", from.display(), to.display()))
}

fn rollback(dir: &Path) -> Result<Report, String> {
    let current = dir.join("mira");
    let previous = dir.join("mira.previous");
    if !previous.is_file() {
        return Err("replace: no mira.previous is available for rollback".into());
    }
    let found = binary_version(&previous)?;
    // Keep the swap file outside scratch so failed recovery never deletes a binary.
    let temp = dir.join("mira.swap");
    if temp.exists() {
        return Err(format!(
            "replace: {} already exists; recover the earlier rollback first",
            temp.display()
        ));
    }
    rename(&current, &temp)?;
    if let Err(e) = rename(&previous, &current) {
        // Keep the current binary outside scratch even if recovery fails.
        if let Err(recovery) = rename(&temp, &current) {
            return Err(format!(
                "{e}; recovery failed: {recovery}; current binary remains at {}",
                temp.display()
            ));
        }
        return Err(e);
    }
    if let Err(e) = rename(&temp, &previous) {
        // The restored version remains installed; preserve the other binary if possible.
        if let Err(recovery) = rename(&current, &previous).and_then(|()| rename(&temp, &current)) {
            return Err(format!("{e}; recovery failed: {recovery}"));
        }
        return Err(e);
    }
    Ok(Report::new(
        &found,
        format!("Mira {found} is now installed."),
    ))
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn hosts(previous: &Path) -> Result<Vec<Host>, String> {
    let output = run(
        Command::new("/bin/ps").args(["-A", "-o", "pid=,args="]),
        "list project hosts",
    )?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let (pid, args) = line.split_once(char::is_whitespace)?;
            let pid = pid.parse().ok()?;
            let (exe, root) = args.trim().split_once(" __host --root ")?;
            if !exe.ends_with("/mira") {
                return None;
            }
            let root = root.to_owned();
            Some(Host {
                pid,
                stop_command: format!(
                    "{} --project {} down",
                    quote(&previous.to_string_lossy()),
                    quote(&root)
                ),
                root,
            })
        })
        .collect())
}

fn export_skills(binary: &Path, report: &mut Report) {
    let targets = match super::skills::targets() {
        Ok(targets) => targets,
        Err(e) => {
            report
                .warnings
                .push(format!("cannot read skill export targets: {e}"));
            return;
        }
    };
    let skillshare = std::env::var_os("HOME").map(|h| {
        let path = PathBuf::from(h).join(".config/skillshare");
        std::fs::canonicalize(&path).unwrap_or(path)
    });
    let mut sync = false;
    for target in targets {
        sync |= skillshare
            .as_ref()
            .is_some_and(|base| target.starts_with(base));
        match run(
            Command::new(binary)
                .args(["skills", "export"])
                .arg(&target)
                .args(["--force", "--json"]),
            "skill export",
        ) {
            Ok(_) => report.updated_targets.push(target),
            Err(e) => report.warnings.push(format!("{}: {e}", target.display())),
        }
    }
    if sync {
        // NotFound means skillshare is not on PATH; no sync is required in that case.
        match Command::new("skillshare").args(["sync", "-g"]).output() {
            Ok(output) if output.status.success() => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Ok(output) => report
                .warnings
                .push(format!("skillshare sync -g failed ({})", output.status)),
            Err(e) => report
                .warnings
                .push(format!("skillshare sync -g failed: {e}")),
        }
    }
}

fn perform(check: bool, undo: bool) -> Result<Report, String> {
    if !check && !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        return Err("replace: self-update supports only macOS on Apple silicon".into());
    }
    // Check ownership before any network request or file replacement.
    let dir = if check { None } else { Some(installed()?) };
    if undo {
        let dir = dir.as_deref().ok_or("replace: missing install directory")?;
        let _scratch = Scratch::new(dir)?;
        return rollback(dir);
    }
    let release = latest()?;
    let available = newer(&release.tag_name, VERSION);
    let mut report = Report::new(
        VERSION,
        if available {
            format!(
                "Mira {} is available (running {VERSION}).",
                release.tag_name
            )
        } else {
            format!("Mira {VERSION} is the latest version.")
        },
    );
    if check {
        let status = if available {
            "update available"
        } else {
            "no update available"
        };
        report.message = format!(
            "Latest release: Mira {} (running {VERSION}; {status}).",
            release.tag_name
        );
    }
    report.latest = Some(release.tag_name.clone());
    report.update_available = available;
    if let Err(e) = (Cache {
        checked_at_ms: now_ms(),
        latest: release.tag_name.clone(),
    })
    .write()
    {
        report
            .warnings
            .push(format!("cannot save update-check cache: {e}"));
    }
    if check || !available {
        return Ok(report);
    }
    let dir = dir.as_deref().ok_or("replace: missing install directory")?;
    let scratch = Scratch::new(dir)?;
    let name = format!("mira-{}-aarch64-apple-darwin", release.tag_name);
    let archive = format!("{name}.tar.gz");
    let base = format!(
        "https://github.com/ImHangLi/mira/releases/download/v{}",
        release.tag_name
    );
    for file in [&archive, &format!("{archive}.sha256")] {
        run(
            curl(30)
                .arg(format!("{base}/{file}"))
                .arg("-o")
                .arg(scratch.0.join(file)),
            "network: download",
        )?;
    }
    run(
        Command::new("/usr/bin/shasum")
            .current_dir(&scratch.0)
            .args(["-a", "256", "-c", &format!("{archive}.sha256")]),
        "checksum",
    )?;
    run(
        Command::new("/usr/bin/tar")
            .arg("-C")
            .arg(&scratch.0)
            .arg("-xzf")
            .arg(scratch.0.join(&archive)),
        "extract",
    )?;
    let new = scratch.0.join(&name).join("mira");
    if binary_version(&new)? != release.tag_name {
        return Err("extract: binary version does not match the release".into());
    }
    let current = dir.join("mira");
    let previous = dir.join("mira.previous");
    rename(&current, &previous)?;
    if let Err(e) = rename(&new, &current) {
        return match rename(&previous, &current) {
            Ok(()) => Err(e),
            Err(recovery) => Err(format!(
                "{e}; recovery failed: {recovery}; old binary is at {}",
                previous.display()
            )),
        };
    }
    report.version = release.tag_name;
    report.update_available = false;
    report.message = format!("Mira {} is now installed.", report.version);
    report.release_notes = Some(release.body.unwrap_or_default());
    report.notice = Some("Running Mira windows keep the old version until they are closed. Project hosts switch to the new version by themselves once their work stops. An open window or background lease can keep a host alive.".into());
    export_skills(&current, &mut report);
    match hosts(&previous) {
        Ok(hosts) => report.hosts = hosts,
        Err(e) => report.warnings.push(e),
    }
    Ok(report)
}

pub fn update(ctx: &Ctx, check: bool, rollback: bool) -> ExitCode {
    match perform(check, rollback) {
        Ok(report) => ctx.emit(
            &PublicReply::success(ReplyContext::default(), report, ReplyMeta::default()),
            Report::text,
        ),
        Err(e) => {
            // Each message starts with the stage that failed; the code follows from it.
            let code = if e.starts_with("How this copy") || e.contains("no mira.previous") {
                ErrorCode::NOT_FOUND
            } else if e.starts_with("network") || e.starts_with("checksum") {
                ErrorCode::EXECUTION_FAILED
            } else if e.contains("another update may be running") {
                ErrorCode::BUSY
            } else {
                ErrorCode::INTERNAL
            };
            ctx.fail(ReplyContext::default(), ErrorInfo::new(code, e))
        }
    }
}
