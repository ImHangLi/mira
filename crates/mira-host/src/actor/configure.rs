//! Configuration transactions: validate → apply with catalog CAS → accept
//! one immutable set. Disk writes are per-file temp+rename; several files are never claimed
//! to be one atomic transaction.

use std::collections::{BTreeSet, VecDeque};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use mira_protocol::config::{self, ConfigSet};
use mira_protocol::error::{ErrorCode, ErrorInfo, Issue, Issues};
use mira_protocol::ids::{CatalogRevision, PluginId};
use mira_protocol::ipc::*;
use mira_protocol::paths::WorkspacePaths;
use mira_protocol::reply::ReplyMeta;
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::{Actor, ConfigState, Msg, Responder};
use crate::diag;
use crate::storage::{Claim, KeyClaim, KeyScope, Storage};

/// File notifications are coalesced this long before re-validating.
const WATCH_DEBOUNCE: Duration = Duration::from_millis(150);

pub enum ConfigJob {
    Apply(ConfigApplyParams),
    /// Re-validates `.mira` from disk (explicit `mira reload` or a file notification).
    Reload,
}

/// Which new invocations are held back because the disk configuration is not usable.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Blocked {
    #[default]
    None,
    All(ErrorCode, String),
    Plugins(BTreeSet<PluginId>, ErrorCode, String),
}

pub enum LoadFailure {
    Invalid(Issues),
    Incomplete { written: Vec<String>, error: String },
}

pub struct Loaded {
    pub set: Arc<ConfigSet>,
    pub revision: CatalogRevision,
    pub same_key: bool,
}

pub struct ConfigCtl {
    pub busy: bool,
    mira_watched: bool,
    pub queue: VecDeque<(ConfigJob, Option<Responder>)>,
    pub blocked: Blocked,
    pub reload_due: Option<Instant>,
    pub fs_rx: mpsc::UnboundedReceiver<()>,
    pub(super) _watcher: Option<notify::RecommendedWatcher>,
}

impl ConfigCtl {
    pub fn new(paths: &WorkspacePaths) -> Self {
        let (tx, fs_rx) = mpsc::unbounded_channel();
        let watcher = start_watcher(paths, tx);
        Self {
            busy: false,
            mira_watched: paths.mira_dir.is_dir(),
            queue: VecDeque::new(),
            blocked: Blocked::None,
            reload_due: None,
            fs_rx,
            _watcher: watcher,
        }
    }
}

fn start_watcher(
    paths: &WorkspacePaths,
    tx: mpsc::UnboundedSender<()>,
) -> Option<notify::RecommendedWatcher> {
    use notify::{RecursiveMode, Watcher};
    let mira = paths.mira_dir.clone();
    let drafts = mira.join(".drafts");
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            let relevant = ev
                .paths
                .iter()
                .any(|p| p.starts_with(&mira) && !p.starts_with(&drafts));
            if relevant {
                let _ = tx.send(());
            }
        }
    })
    .ok()?;
    // Watching the root non-recursively notices `.mira` being created; `.mira` itself recursively.
    let _ = watcher.watch(paths.root.as_path(), RecursiveMode::NonRecursive);
    if paths.mira_dir.is_dir() {
        let _ = watcher.watch(&paths.mira_dir, RecursiveMode::Recursive);
    }
    Some(watcher)
}

fn affected(set: Option<&ConfigSet>, issues: &Issues, reason: &str) -> Blocked {
    let all = || Blocked::All(ErrorCode::SCHEMA_INVALID, reason.to_owned());
    let Some(set) = set else { return all() };
    let mut ids = BTreeSet::new();
    for i in &issues.0 {
        let Some(file) = &i.file else { return all() };
        let hit = set
            .plugins
            .iter()
            .find(|p| Path::new(file).starts_with(p.dir.as_path()));
        match hit {
            Some(p) => {
                ids.insert(p.plugin.id.clone());
            }
            None => return all(),
        }
    }
    if ids.is_empty() {
        all()
    } else {
        Blocked::Plugins(ids, ErrorCode::SCHEMA_INVALID, reason.to_owned())
    }
}

fn load_disk(mira_dir: &Path) -> Result<ConfigSet, Issues> {
    let local = config::load_local(mira_dir)?;
    config::load_config_dir(mira_dir, local.as_ref())
}

/// Writes one file with a same-directory temp file and rename.
fn write_atomic(target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = target.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(dir)?;
    }
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = target.with_file_name(format!(".{name}.mira-tmp-{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    if let Ok(meta) = std::fs::metadata(target) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, target).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

fn copy_tree(src: &Path, dst: &Path, written: &mut Vec<String>) -> Result<(), String> {
    let entries = std::fs::read_dir(src).map_err(|e| format!("read {}: {e}", src.display()))?;
    for e in entries.flatten() {
        let from = e.path();
        let to = dst.join(e.file_name());
        let ft = e.file_type().map_err(|e| e.to_string())?;
        if ft.is_dir() {
            copy_tree(&from, &to, written)?;
        } else if ft.is_file() {
            let bytes =
                std::fs::read(&from).map_err(|e| format!("read {}: {e}", from.display()))?;
            let mode = std::fs::metadata(&from).map(|m| m.permissions());
            write_atomic(&to, &bytes).map_err(|e| format!("write {}: {e}", to.display()))?;
            if let Ok(p) = mode {
                let _ = std::fs::set_permissions(&to, p);
            }
            written.push(to.to_string_lossy().into_owned());
        }
    }
    Ok(())
}

/// Validates the draft, then copies its definitions into `.mira`, then re-validates `.mira`.
fn apply_draft(paths: &WorkspacePaths, draft: &Path) -> Result<ConfigSet, LoadFailure> {
    let draft = std::fs::canonicalize(draft).map_err(|e| {
        LoadFailure::Invalid(Issues(vec![Issue::new(
            ErrorCode::NOT_FOUND,
            "",
            format!("draft directory: {e}"),
        )]))
    })?;
    let mira = std::fs::canonicalize(&paths.mira_dir).unwrap_or(paths.mira_dir.clone());
    if draft == mira {
        return load_disk(&mira).map_err(LoadFailure::Invalid);
    }
    let local = config::load_local(&paths.mira_dir).map_err(LoadFailure::Invalid)?;
    let set = config::load_config_dir(&draft, local.as_ref()).map_err(LoadFailure::Invalid)?;
    let mut written = Vec::new();
    let result = (|| {
        let ws = std::fs::read(draft.join(config::WORKSPACE_FILE)).map_err(|e| e.to_string())?;
        for rel in &set.workspace.plugins {
            if rel.starts_with('/') {
                continue; // External plugin paths are referenced, not copied.
            }
            copy_tree(&draft.join(rel), &paths.mira_dir.join(rel), &mut written)?;
        }
        let target = paths.mira_dir.join(config::WORKSPACE_FILE);
        write_atomic(&target, &ws).map_err(|e| format!("write {}: {e}", target.display()))?;
        written.push(target.to_string_lossy().into_owned());
        Ok::<_, String>(())
    })();
    if let Err(error) = result {
        return Err(LoadFailure::Incomplete { written, error });
    }
    load_disk(&paths.mira_dir).map_err(|issues| LoadFailure::Incomplete {
        written,
        error: format!(
            "files were written but .mira does not validate: {}",
            issues.to_error_info().message
        ),
    })
}

async fn accept(
    storage: Result<Storage, String>,
    set: ConfigSet,
    key: Option<(mira_protocol::ids::RequestKey, CatalogRevision)>,
) -> Result<Loaded, LoadFailure> {
    let storage = storage.map_err(|e| LoadFailure::Incomplete {
        written: vec![],
        error: e,
    })?;
    let mut same_key = false;
    if let Some((key, expected)) = key {
        let fingerprint =
            storage.fingerprint(&json!({"set_hash": set.set_hash, "expected": expected}));
        let claim = KeyClaim {
            scope: KeyScope::Apply,
            key,
            fingerprint,
        };
        match storage.claim_key(claim, expected.to_string()).await {
            Ok(Claim::Same { .. }) => same_key = true,
            Ok(Claim::New) => {}
            Ok(Claim::Conflict) => {
                return Err(LoadFailure::Invalid(Issues(vec![Issue::new(
                    ErrorCode::REQUEST_KEY_CONFLICT,
                    "",
                    "request key was used for a different apply",
                )])));
            }
            Err(e) => {
                return Err(LoadFailure::Incomplete {
                    written: vec![],
                    error: e.to_string(),
                });
            }
        }
    }
    let revision = storage
        .accept_catalog(set.set_hash.clone())
        .await
        .map_err(|e| LoadFailure::Incomplete {
            written: vec![],
            error: e.to_string(),
        })?;
    Ok(Loaded {
        set: Arc::new(set),
        revision,
        same_key,
    })
}

impl Actor {
    pub(super) fn config_apply(&mut self, p: ConfigApplyParams, r: Responder) {
        self.cfg.queue.push_back((ConfigJob::Apply(p), Some(r)));
        self.process_config_queue();
    }

    pub(super) fn config_reload(&mut self, r: Option<Responder>) {
        self.cfg.queue.push_back((ConfigJob::Reload, r));
        self.process_config_queue();
    }

    pub(super) fn fs_changed(&mut self) {
        if !self.cfg.mira_watched && self.paths.mira_dir.is_dir() {
            use notify::Watcher;
            if let Some(w) = self.cfg._watcher.as_mut() {
                self.cfg.mira_watched = w
                    .watch(&self.paths.mira_dir, notify::RecursiveMode::Recursive)
                    .is_ok();
            }
        }
        self.cfg.reload_due = Some(Instant::now() + WATCH_DEBOUNCE);
    }

    pub(super) fn config_tick(&mut self) {
        if self.cfg.reload_due.is_some_and(|t| Instant::now() >= t) {
            self.cfg.reload_due = None;
            self.config_reload(None);
        }
    }

    fn current_set(&self) -> Option<Arc<ConfigSet>> {
        match &self.config {
            ConfigState::Accepted { set, .. } => Some(set.clone()),
            _ => None,
        }
    }

    /// Jobs run one at a time, so concurrent applies with the same expected revision
    /// resolve as one success and one REVISION_CONFLICT.
    fn process_config_queue(&mut self) {
        if self.cfg.busy {
            return;
        }
        let Some((job, r)) = self.cfg.queue.pop_front() else {
            return;
        };
        let paths = self.paths.clone();
        let storage = self.storage.clone().map_err(|e| e.to_string());
        let tx = self.tx.clone();
        let current = self.current_set().map(|s| s.set_hash.clone());
        match job {
            ConfigJob::Apply(p) => {
                if p.expected_catalog_revision != self.catalog_revision {
                    let mut d = Map::new();
                    d.insert(
                        "current_catalog_revision".into(),
                        json!(self.catalog_revision),
                    );
                    let e = ErrorInfo::new(
                        ErrorCode::REVISION_CONFLICT,
                        format!(
                            "expected catalog revision {}, but it is {}",
                            p.expected_catalog_revision, self.catalog_revision
                        ),
                    )
                    .with_details(d)
                    .with_next_action(
                        &["mira", "catalog", "--json"],
                        "Re-read the catalog and reapply your change on top of it.",
                    );
                    if let Some(r) = r {
                        r.send(self.fail(e));
                    }
                    return self.process_config_queue();
                }
                self.cfg.busy = true;
                let previous = std::mem::replace(
                    &mut self.cfg.blocked,
                    Blocked::All(
                        ErrorCode::BUSY,
                        "a configuration apply is in progress".into(),
                    ),
                );
                let key = p
                    .request_key
                    .clone()
                    .map(|k| (k, p.expected_catalog_revision));
                tokio::spawn(async move {
                    let draft = PathBuf::from(p.draft_dir.as_str());
                    let loaded =
                        tokio::task::spawn_blocking(move || apply_draft(&paths, &draft)).await;
                    let result = match loaded {
                        Ok(Ok(set)) => accept(storage, set, key).await,
                        Ok(Err(f)) => Err(f),
                        Err(e) => Err(LoadFailure::Incomplete {
                            written: vec![],
                            error: e.to_string(),
                        }),
                    };
                    let _ = tx
                        .send(Msg::ConfigDone {
                            result,
                            responder: r,
                            is_apply: true,
                            previous_blocked: previous,
                            previous_hash: current,
                        })
                        .await;
                });
            }
            ConfigJob::Reload => {
                self.cfg.busy = true;
                let previous = self.cfg.blocked.clone();
                tokio::spawn(async move {
                    let dir = paths.mira_dir.clone();
                    let loaded = tokio::task::spawn_blocking(move || {
                        if !dir.join(config::WORKSPACE_FILE).is_file() {
                            return Err(LoadFailure::Invalid(Issues(vec![Issue::new(
                                ErrorCode::NOT_SETUP,
                                "",
                                "no .mira/workspace.json",
                            )])));
                        }
                        load_disk(&dir).map_err(LoadFailure::Invalid)
                    })
                    .await;
                    let result = match loaded {
                        Ok(Ok(set)) => accept(storage, set, None).await,
                        Ok(Err(f)) => Err(f),
                        Err(e) => Err(LoadFailure::Incomplete {
                            written: vec![],
                            error: e.to_string(),
                        }),
                    };
                    let _ = tx
                        .send(Msg::ConfigDone {
                            result,
                            responder: r,
                            is_apply: false,
                            previous_blocked: previous,
                            previous_hash: current,
                        })
                        .await;
                });
            }
        }
    }

    pub(super) fn config_done(
        &mut self,
        result: Result<Loaded, LoadFailure>,
        r: Option<Responder>,
        is_apply: bool,
        previous_blocked: Blocked,
        previous_hash: Option<mira_protocol::ids::Digest>,
    ) {
        self.cfg.busy = false;
        match result {
            Ok(loaded) => {
                let changed = previous_hash.as_ref() != Some(&loaded.set.set_hash);
                let plugins = loaded
                    .set
                    .plugins
                    .iter()
                    .map(|p| p.plugin.id.clone())
                    .collect();
                if changed {
                    diag(format!(
                        "accepted configuration revision {}",
                        loaded.revision
                    ));
                    self.validators.clear();
                }
                self.catalog_revision = loaded.revision;
                self.config = ConfigState::Accepted {
                    set: loaded.set,
                    disk_issues: None,
                };
                self.cfg.blocked = Blocked::None;
                let data = ConfigApplied {
                    catalog_revision: self.catalog_revision,
                    changed: changed && !loaded.same_key,
                    plugins,
                };
                if let Some(r) = r {
                    r.send(self.ok(data, ReplyMeta::default()));
                }
                self.state_changed();
            }
            Err(LoadFailure::Invalid(issues)) => {
                let is_apply_validation = is_apply;
                // A rejected draft changes nothing on disk: restore the previous blocking state.
                // An invalid disk (reload) keeps the accepted set and blocks affected plugins.
                if is_apply_validation {
                    self.cfg.blocked = previous_blocked;
                } else {
                    let not_setup = issues.0.iter().all(|i| i.code == ErrorCode::NOT_SETUP);
                    if !not_setup {
                        let set = self.current_set();
                        self.cfg.blocked = affected(
                            set.as_deref(),
                            &issues,
                            "configuration on disk is invalid; fix it and run `mira reload`",
                        );
                        match &mut self.config {
                            ConfigState::Accepted { disk_issues, .. } => {
                                *disk_issues = Some(issues.clone())
                            }
                            other => *other = ConfigState::Invalid(issues.clone()),
                        }
                        diag(format!(
                            "configuration on disk is not in effect: {}",
                            issues.to_error_info().message
                        ));
                    }
                }
                if let Some(r) = r {
                    r.send(self.fail(issues.to_error_info()));
                }
                self.state_changed();
            }
            Err(LoadFailure::Incomplete { written, error }) => {
                let mut d = Map::new();
                d.insert("written".into(), Value::from(written.clone()));
                d.insert("error".into(), Value::String(error.clone()));
                let e = ErrorInfo::new(
                    ErrorCode::CONFIG_APPLY_INCOMPLETE,
                    format!("configuration apply did not complete: {error}"),
                )
                .with_details(d)
                .with_next_action(
                    &["mira", "reload"],
                    "Fix the files, then reload; running work is unaffected.",
                );
                self.cfg.blocked = Blocked::All(
                    ErrorCode::CONFIG_APPLY_INCOMPLETE,
                    format!(
                        "the last configuration apply was incomplete ({error}); fix the files and run `mira reload`"
                    ),
                );
                if let ConfigState::Accepted { disk_issues, .. } = &mut self.config {
                    *disk_issues = Some(Issues(vec![Issue::new(
                        ErrorCode::CONFIG_APPLY_INCOMPLETE,
                        "",
                        error,
                    )]));
                }
                if let Some(r) = r {
                    r.send(self.fail(e));
                }
                self.state_changed();
            }
        }
        self.process_config_queue();
    }

    /// Refuses new invocations held back by an invalid or incomplete disk configuration.
    pub(crate) fn check_blocked(&self, plugin: &PluginId) -> Result<(), ErrorInfo> {
        let (code, reason) = match &self.cfg.blocked {
            Blocked::None => return Ok(()),
            Blocked::All(c, r) => (c, r),
            Blocked::Plugins(ids, c, r) if ids.contains(plugin) => (c, r),
            Blocked::Plugins(..) => return Ok(()),
        };
        Err(ErrorInfo::new(code.clone(), reason.clone()).retryable(*code == ErrorCode::BUSY))
    }

    /// Runs whose definition changed, was disabled, or was removed since they started.
    pub(crate) fn retiring_warnings(&self) -> Vec<Warning> {
        let set = self.current_set();
        let mut out = Vec::new();
        for run in self.runs.values() {
            let Some(action_ref) = &run.record.action_ref else {
                continue;
            };
            let now = set
                .as_ref()
                .and_then(|s| s.action(action_ref))
                .map(|(lp, a)| (lp.plugin.enabled, a.definition_hash.clone()));
            let message = match now {
                None => "its action was removed; it can still be stopped".to_owned(),
                Some((false, _)) => "its plugin is disabled; it can still be stopped".to_owned(),
                Some((true, h)) if h != run.record.definition_hash => {
                    "definition changed; the next start uses the new definition".to_owned()
                }
                _ => continue,
            };
            out.push(Warning {
                code: mira_protocol::ErrorCode::parse("DEFINITION_CHANGED".into())
                    .unwrap_or(ErrorCode::INTERNAL),
                message: format!("{action_ref} ({}): {message}", run.record.run_id),
                subject: Some(run.record.run_id.to_string()),
            });
        }
        out
    }
}
