//! Workspace selection (§4.1). Reads only directory entries; never executes anything.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{ErrorCode, ErrorInfo};
use crate::ids::{AbsolutePath, WorkspaceId};

/// Files whose presence marks a directory as a project root when no Git worktree exists.
pub const PROJECT_MARKERS: &[&str] = &[
    "package.json",
    "pyproject.toml",
    "requirements.txt",
    "Cargo.toml",
    "go.mod",
    "compose.yaml",
    "compose.yml",
    "docker-compose.yml",
    "docker-compose.yaml",
    "Makefile",
    "justfile",
    "Justfile",
    "Taskfile.yml",
    "Taskfile.yaml",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionReason {
    ExplicitProject,
    MiraConfig,
    GitWorktree,
    ProjectManifest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Selected {
    pub root: AbsolutePath,
    pub id: WorkspaceId,
    pub reason: SelectionReason,
}

impl Selected {
    pub fn mira_dir(&self) -> PathBuf {
        self.root.as_path().join(".mira")
    }
    pub fn is_setup(&self) -> bool {
        self.mira_dir().join("workspace.json").is_file()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectError {
    /// No root could be chosen; `candidates` are direct child directories that look like projects.
    NeedsProject {
        searched: AbsolutePath,
        candidates: Vec<AbsolutePath>,
    },
    UnsupportedEncoding,
    Io(String),
}

impl SelectError {
    pub fn to_error_info(&self) -> ErrorInfo {
        match self {
            Self::NeedsProject {
                searched,
                candidates,
            } => {
                let mut details = serde_json::Map::new();
                details.insert("searched".into(), serde_json::json!(searched));
                details.insert("candidates".into(), serde_json::json!(candidates));
                let message = match candidates.len() {
                    0 => format!("no project found at {searched}; pass --project PATH"),
                    1 => format!(
                        "no project at {searched}; one candidate found: {}",
                        candidates[0]
                    ),
                    n => format!(
                        "no project at {searched}; {n} candidate directories found; pass --project PATH"
                    ),
                };
                ErrorInfo::new(ErrorCode::NEEDS_PROJECT, message).with_details(details)
            }
            Self::UnsupportedEncoding => ErrorInfo::new(
                ErrorCode::UNSUPPORTED_PATH_ENCODING,
                "workspace paths must be valid UTF-8",
            ),
            Self::Io(m) => ErrorInfo::new(ErrorCode::NOT_FOUND, m.clone()),
        }
    }
}

fn canonical(p: &Path) -> Result<AbsolutePath, SelectError> {
    let real = std::fs::canonicalize(p)
        .map_err(|e| SelectError::Io(format!("cannot resolve {}: {e}", p.display())))?;
    AbsolutePath::from_path(&real).map_err(|_| SelectError::UnsupportedEncoding)
}

fn selected(root: AbsolutePath, reason: SelectionReason) -> Selected {
    Selected {
        id: WorkspaceId::for_root(&root),
        root,
        reason,
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .and_then(|h| std::fs::canonicalize(h).ok())
}

/// Directories never chosen implicitly: HOME, its Downloads/Desktop/Documents, and `/`.
pub fn is_protected(dir: &Path) -> bool {
    if dir == Path::new("/") {
        return true;
    }
    match home() {
        Some(h) => {
            dir == h
                || ["Downloads", "Desktop", "Documents"]
                    .iter()
                    .any(|d| dir == h.join(d))
        }
        None => false,
    }
}

fn has_marker(dir: &Path) -> bool {
    PROJECT_MARKERS.iter().any(|m| dir.join(m).is_file())
}

/// The nearest Git worktree root at or above `dir` (a `.git` directory or file).
pub fn git_worktree_root(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .find(|a| a.join(".git").exists())
        .map(Path::to_path_buf)
}

/// What a work tree's `HEAD` names, read from `.git` files without running `git`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHead {
    /// The symbolic ref (for example `refs/heads/main`) when HEAD is attached.
    pub symbolic_ref: Option<String>,
    /// The commit HEAD points to, when it can be read.
    pub commit: Option<String>,
}

impl GitHead {
    /// The branch name when HEAD is attached to a branch.
    pub fn branch(&self) -> Option<&str> {
        self.symbolic_ref.as_deref()?.strip_prefix("refs/heads/")
    }
}

/// Reads HEAD of the work tree at `root`. `None` when `root` has no `.git` or HEAD cannot
/// be read. Worktrees and submodules (a `.git` file naming the git directory) are followed.
pub fn git_head(root: &Path) -> Option<GitHead> {
    let dot = root.join(".git");
    let gitdir = if dot.is_file() {
        let text = std::fs::read_to_string(&dot).ok()?;
        let p = PathBuf::from(text.strip_prefix("gitdir:")?.trim());
        if p.is_absolute() { p } else { root.join(p) }
    } else if dot.is_dir() {
        dot
    } else {
        return None;
    };
    let head = std::fs::read_to_string(gitdir.join("HEAD")).ok()?;
    let head = head.trim();
    let Some(r) = head.strip_prefix("ref: ") else {
        return Some(GitHead {
            symbolic_ref: None,
            commit: Some(head.to_owned()).filter(|h| !h.is_empty()),
        });
    };
    // A linked worktree keeps shared refs in the common git directory.
    let common = std::fs::read_to_string(gitdir.join("commondir"))
        .ok()
        .map_or_else(|| gitdir.clone(), |c| gitdir.join(c.trim()));
    let commit = std::fs::read_to_string(gitdir.join(r))
        .or_else(|_| std::fs::read_to_string(common.join(r)))
        .ok()
        .map(|s| s.trim().to_owned());
    Some(GitHead {
        symbolic_ref: Some(r.to_owned()),
        commit,
    })
}

/// Selects the workspace for `project` (explicit) or `cwd` using the fixed §4.1 order.
pub fn select(project: Option<&Path>, cwd: &Path) -> Result<Selected, SelectError> {
    if let Some(p) = project {
        let root = canonical(p)?;
        if !root.as_path().is_dir() {
            return Err(SelectError::Io(format!("{root} is not a directory")));
        }
        return Ok(selected(root, SelectionReason::ExplicitProject));
    }
    let cwd_abs = canonical(cwd)?;
    let cwd = cwd_abs.as_path();
    let git_root = git_worktree_root(cwd);
    let home = home();
    for dir in cwd.ancestors() {
        if home.as_deref() == Some(dir) && git_root.as_deref() != Some(dir) {
            break;
        }
        if dir.join(".mira").join("workspace.json").is_file() {
            return Ok(selected(canonical(dir)?, SelectionReason::MiraConfig));
        }
        if git_root.as_deref() == Some(dir) {
            break;
        }
    }
    if let Some(g) = git_root
        && !is_protected(&g)
    {
        return Ok(selected(canonical(&g)?, SelectionReason::GitWorktree));
    }
    if has_marker(cwd) && !is_protected(cwd) {
        return Ok(selected(cwd_abs, SelectionReason::ProjectManifest));
    }
    let mut candidates = Vec::new();
    if let Ok(entries) = std::fs::read_dir(cwd) {
        for e in entries.flatten() {
            let p = e.path();
            let name = e.file_name();
            if name.to_string_lossy().starts_with('.') || !p.is_dir() {
                continue;
            }
            if (p.join(".git").exists()
                || p.join(".mira").join("workspace.json").is_file()
                || has_marker(&p))
                && let Ok(c) = canonical(&p)
            {
                candidates.push(c);
            }
        }
    }
    candidates.sort();
    Err(SelectError::NeedsProject {
        searched: cwd_abs,
        candidates,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mira-git-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git/refs/heads")).expect("scratch dir");
        dir
    }

    #[test]
    fn git_head_reads_branch_and_commit() {
        let root = scratch("branch");
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").expect("HEAD");
        std::fs::write(root.join(".git/refs/heads/main"), "abc123\n").expect("ref");
        let head = git_head(&root).expect("head");
        assert_eq!(head.branch(), Some("main"));
        assert_eq!(head.commit.as_deref(), Some("abc123"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn git_head_reads_a_detached_commit() {
        let root = scratch("detached");
        std::fs::write(root.join(".git/HEAD"), "0123456789abcdef\n").expect("HEAD");
        let head = git_head(&root).expect("head");
        assert_eq!(head.branch(), None);
        assert_eq!(head.commit.as_deref(), Some("0123456789abcdef"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn git_head_is_none_outside_a_repository() {
        let root = std::env::temp_dir().join(format!("mira-git-{}-none", std::process::id()));
        std::fs::create_dir_all(&root).expect("dir");
        assert_eq!(git_head(&root), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
