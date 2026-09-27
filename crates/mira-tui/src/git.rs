//! The workspace's git branch for the header. Never spawns `git`.

use std::path::Path;

use mira_protocol::workspace::git_head;

/// The checked-out branch, or the short commit when HEAD is detached. `None` when the
/// workspace is not a git repository or HEAD cannot be read.
pub fn head_label(root: &Path) -> Option<String> {
    let head = git_head(root)?;
    match head.symbolic_ref {
        Some(r) => Some(r.strip_prefix("refs/heads/").unwrap_or(&r).to_owned()),
        None => Some(head.commit?.chars().take(7).collect()),
    }
}
