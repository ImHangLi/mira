//! Per-workspace file locations. `mira paths` is the public way to discover them.
//!
//! Development overrides: `MIRA_DATA_HOME` replaces `~/Library/{Application Support,Logs,Caches}`
//! with `<dir>/{state,logs,cache}`; `MIRA_RUNTIME_DIR` replaces `/tmp/mira-<uid>`.

use std::path::{Path, PathBuf};

use crate::ids::{AbsolutePath, PluginId, RunId, WorkspaceId};
use crate::ipc::{PathClass, PathEntry, PathsData};
use crate::reply::WorkspaceRef;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspacePaths {
    pub root: AbsolutePath,
    pub id: WorkspaceId,
    pub mira_dir: PathBuf,
    pub state_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub runtime_dir: PathBuf,
}

pub fn current_uid() -> u32 {
    rustix::process::getuid().as_raw()
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Per-user state, logs, and cache bases, independent of a workspace.
pub fn user_bases() -> (PathBuf, PathBuf, PathBuf) {
    match std::env::var_os("MIRA_DATA_HOME") {
        Some(d) => {
            let d = PathBuf::from(d);
            (d.join("state"), d.join("logs"), d.join("cache"))
        }
        None => {
            let lib = home().join("Library");
            (
                lib.join("Application Support/Mira"),
                lib.join("Logs/Mira"),
                lib.join("Caches/Mira"),
            )
        }
    }
}

impl WorkspacePaths {
    pub fn new(root: AbsolutePath) -> Self {
        let id = WorkspaceId::for_root(&root);
        let wid = id.as_str();
        let (state_base, logs_base, cache_base) = user_bases();
        let runtime_dir = std::env::var_os("MIRA_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(format!("/tmp/mira-{}", current_uid())));
        Self {
            mira_dir: root.as_path().join(".mira"),
            state_dir: state_base.join("workspaces").join(wid),
            logs_dir: logs_base.join("workspaces").join(wid),
            cache_dir: cache_base.join("workspaces").join(wid),
            runtime_dir,
            root,
            id,
        }
    }

    pub fn socket(&self) -> PathBuf {
        self.runtime_dir.join(format!("{}.sock", self.id))
    }
    pub fn lock(&self) -> PathBuf {
        self.runtime_dir.join(format!("{}.lock", self.id))
    }
    pub fn owner(&self) -> PathBuf {
        self.runtime_dir.join(format!("{}.owner", self.id))
    }
    pub fn state_db(&self) -> PathBuf {
        self.state_dir.join("state.sqlite3")
    }
    pub fn fingerprint_key(&self) -> PathBuf {
        self.state_dir.join("fingerprint.key")
    }
    pub fn plugin_state(&self, plugin: &PluginId) -> PathBuf {
        self.state_dir
            .join("plugins")
            .join(plugin.as_str())
            .join("state")
    }
    pub fn plugin_cache(&self, plugin: &PluginId) -> PathBuf {
        self.cache_dir.join("plugins").join(plugin.as_str())
    }
    pub fn artifacts(&self, run: &RunId) -> PathBuf {
        self.state_dir.join("artifacts").join(run.as_str())
    }
    pub fn run_logs(&self, run: &RunId) -> PathBuf {
        self.logs_dir.join("runs").join(run.as_str())
    }
    pub fn host_log(&self) -> PathBuf {
        self.logs_dir.join("host.log")
    }
    /// Short-lived input/config files handed to children.
    pub fn temp_inputs(&self) -> PathBuf {
        self.cache_dir.join("inputs")
    }

    fn abs(p: &Path) -> Option<AbsolutePath> {
        AbsolutePath::from_path(p).ok()
    }

    /// The public path table.
    pub fn to_data(&self) -> PathsData {
        let rows: [(PathClass, PathBuf, &str, bool, bool); 12] = [
            (
                PathClass::WorkspaceConfig,
                self.mira_dir.join("workspace.json"),
                "Editable workspace definition and plugin list",
                false,
                true,
            ),
            (
                PathClass::Plugins,
                self.mira_dir.join("plugins"),
                "Plugin manifests, scripts, and docs; commit these",
                false,
                true,
            ),
            (
                PathClass::LocalConfig,
                self.mira_dir.join("local.json"),
                "Personal overrides; not committed; values are not echoed",
                false,
                false,
            ),
            (
                PathClass::Drafts,
                self.mira_dir.join(".drafts"),
                "Unapplied candidate definitions; never auto-deleted",
                false,
                true,
            ),
            (
                PathClass::StateDb,
                self.state_db(),
                "Bounded run summaries, last views, catalog metadata; read via the CLI",
                false,
                false,
            ),
            (
                PathClass::FingerprintKey,
                self.fingerprint_key(),
                "Request-key fingerprint secret; never read or exported",
                false,
                false,
            ),
            (
                PathClass::PluginState,
                self.state_dir.join("plugins"),
                "Private persistent plugin data; never auto-deleted",
                false,
                false,
            ),
            (
                PathClass::Artifacts,
                self.state_dir.join("artifacts"),
                "Managed run artifacts and large payloads; retention applies",
                true,
                false,
            ),
            (
                PathClass::Logs,
                self.logs_dir.join("runs"),
                "Segmented canonical run logs; read via `mira logs`",
                true,
                false,
            ),
            (
                PathClass::HostLog,
                self.host_log(),
                "Core errors and minimal diagnostics",
                true,
                false,
            ),
            (
                PathClass::Cache,
                self.cache_dir.clone(),
                "Rebuildable indexes, plugin caches, short-lived inputs",
                true,
                false,
            ),
            (
                PathClass::Runtime,
                self.runtime_dir.clone(),
                "Socket, host lock, and owner record",
                true,
                false,
            ),
        ];
        PathsData {
            workspace: WorkspaceRef {
                id: self.id.clone(),
                root: self.root.clone(),
            },
            entries: rows
                .into_iter()
                .filter_map(|(class, path, purpose, auto_delete, agent_reads)| {
                    Some(PathEntry {
                        class,
                        path: Self::abs(&path)?,
                        purpose: purpose.to_owned(),
                        auto_delete,
                        agent_reads,
                    })
                })
                .collect(),
        }
    }
}
