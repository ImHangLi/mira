//! `mira skills export DIR`: copy the bundled skills of this Mira version to `DIR/mira/`
//! and `DIR/mira-extend/`. Existing files are kept unless `--force` is given. Mira never
//! picks DIR itself and refuses a DIR inside the current Git work tree: skills belong to
//! the user, not to a repository.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use mira_protocol::reply::{PublicReply, ReplyContext, ReplyMeta};
use mira_protocol::workspace::git_worktree_root;
use mira_protocol::{ErrorCode, ErrorInfo};
use serde::Serialize;

use super::ctx::Ctx;
use crate::output::invalid_argument;

macro_rules! bundled {
    ($($skill:literal => [$($file:literal),* $(,)?]),* $(,)?) => {
        const BUNDLED: &[(&str, &str, &str)] = &[
            $($(($skill, $file, include_str!(concat!("../../../../skills/", $skill, "/", $file))),)*)*
        ];
    };
}

bundled! {
    "mira" => ["SKILL.md", "references/cli.md", "references/setup.md", "references/updates.md"],
    "mira-extend" => [
        "SKILL.md",
        "references/manifest.md",
        "references/protocol.md",
        "templates/command/plugin.json",
        "templates/command/with_input.py",
        "templates/structured/plugin.json",
        "templates/structured/main.py",
    ],
}

/// The skill folders `export` writes under DIR.
const SKILLS: [&str; 2] = ["mira", "mira-extend"];

#[derive(Serialize)]
pub struct Report {
    pub targets: Vec<String>,
    pub written: Vec<String>,
}

/// Resolves `dir` against `cwd` and through symlinks of its nearest existing ancestor.
fn resolve(cwd: &Path, dir: &Path) -> PathBuf {
    let abs = cwd.join(dir);
    let mut rest = Vec::new();
    let mut base = abs.as_path();
    loop {
        if let Ok(real) = std::fs::canonicalize(base) {
            return rest.iter().rev().fold(real, |p, c| p.join(c));
        }
        match (base.parent(), base.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                base = parent;
            }
            _ => return abs,
        }
    }
}

pub fn export(ctx: &Ctx, dir: &Path, force: bool) -> ExitCode {
    let cwd = match std::env::current_dir() {
        Ok(c) => c,
        Err(e) => {
            return ctx.fail(
                ReplyContext::default(),
                ErrorInfo::new(
                    ErrorCode::NOT_FOUND,
                    format!("cannot read the current directory: {e}"),
                ),
            );
        }
    };
    let target = resolve(&cwd, dir);
    if let Some(repo) = git_worktree_root(&cwd).map(|r| resolve(&cwd, &r))
        && target.starts_with(&repo)
    {
        return ctx.fail(
            ReplyContext::default(),
            invalid_argument(format!(
                "{} is inside the Git work tree {}; export the skills to your own skills \
                 folder (for example ~/.claude/skills or ~/.agents/skills), not into a project",
                target.display(),
                repo.display()
            )),
        );
    }
    let files: Vec<(PathBuf, &str)> = BUNDLED
        .iter()
        .map(|(skill, rel, content)| (target.join(skill).join(rel), *content))
        .collect();
    if !force {
        let existing: Vec<String> = files
            .iter()
            .filter(|(path, _)| std::fs::symlink_metadata(path).is_ok())
            .map(|(path, _)| path.to_string_lossy().into_owned())
            .collect();
        if let Some(first) = existing.first() {
            let shown = dir.to_string_lossy();
            return ctx.fail(
                ReplyContext::default(),
                invalid_argument(format!(
                    "{first} already exists ({} of {} files); nothing was written",
                    existing.len(),
                    files.len()
                ))
                .with_next_action(
                    &["mira", "skills", "export", &shown, "--force"],
                    "Overwrite the existing skill files with this version.",
                ),
            );
        }
    }
    let mut written = Vec::new();
    for (path, content) in &files {
        let result = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(path, content));
        if let Err(e) = result {
            return ctx.fail(
                ReplyContext::default(),
                ErrorInfo::new(
                    ErrorCode::STORAGE_UNAVAILABLE,
                    format!("cannot export skills: {}: {e}", path.display()),
                ),
            );
        }
        written.push(path.to_string_lossy().into_owned());
    }
    let report = Report {
        targets: SKILLS
            .iter()
            .map(|s| target.join(s).to_string_lossy().into_owned())
            .collect(),
        written,
    };
    let reply = PublicReply::success(ReplyContext::default(), report, ReplyMeta::default());
    ctx.emit(&reply, |r| {
        format!(
            "mira {} skills: {} files written\n  {}",
            mira_protocol::VERSION,
            r.written.len(),
            r.targets.join("\n  ")
        )
    })
}
