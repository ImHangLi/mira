//! Retention and cleanup: bounded history, managed-file GC, and
//! explicit private-state clearing. Only Core-recorded files inside managed roots are
//! removed; symlinks are never followed; active runs and their files are never touched.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use mira_protocol::error::{ErrorCode, ErrorInfo};
use mira_protocol::ids::RunId;
use mira_protocol::ipc::*;
use mira_protocol::manifest::StoragePolicy;
use mira_protocol::paths::WorkspacePaths;
use mira_protocol::reply::ReplyMeta;
use mira_protocol::time::Timestamp;
use tokio::time::Instant;

use super::{Actor, Responder, reply_fail, reply_ok};
use crate::diag;
use crate::storage::{GcPolicy, GcSelection};

/// Automatic incremental GC runs at most this often while the host is active.
pub const AUTO_GC_EVERY: Duration = Duration::from_secs(300);
const DAY: Duration = Duration::from_secs(86_400);

fn gc_policy(p: &StoragePolicy) -> GcPolicy {
    GcPolicy {
        history_days: p.history_days,
        runs_per_action: p.runs_per_action,
        runs_per_workspace: p.runs_per_workspace,
        view_days: p.view_days,
        log_days: p.log_days,
        artifact_days: p.artifact_days,
    }
}

/// Size of a file or directory tree without following symlinks.
pub fn tree_bytes(p: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(p) else {
        return 0;
    };
    if meta.file_type().is_symlink() {
        return 0;
    }
    if meta.is_file() {
        return meta.len();
    }
    std::fs::read_dir(p)
        .map(|rd| rd.flatten().map(|e| tree_bytes(&e.path())).sum())
        .unwrap_or(0)
}

/// Removes `path` only when it is a direct, non-symlink child of `root`.
fn remove_managed(root: &Path, path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if path.parent() != Some(root) {
        return 0;
    }
    if meta.file_type().is_symlink() {
        // A symlink swapped in place of a managed entry: remove the link, never its target.
        let _ = std::fs::remove_file(path);
        return 0;
    }
    let bytes = tree_bytes(path);
    let ok = if meta.is_dir() {
        std::fs::remove_dir_all(path).is_ok()
    } else {
        std::fs::remove_file(path).is_ok()
    };
    if ok { bytes } else { 0 }
}

#[derive(Default, Clone)]
struct FilePlan {
    logs: Vec<PathBuf>,
    artifacts: Vec<PathBuf>,
    cache: Vec<PathBuf>,
    /// Retained run results (`state_dir/results/<run>.json`), counted as history.
    results: Vec<PathBuf>,
    bytes: [u64; 4],
}

/// Per-run managed dirs to remove: expired runs, age limits, then workspace byte quotas.
fn plan_run_dirs(
    root: &Path,
    expired: &[&RunId],
    quota: u64,
    oldest_first: &[RunId],
    active: &HashSet<String>,
) -> (Vec<PathBuf>, u64) {
    plan_run_entries(root, "", expired, quota, oldest_first, active)
}

fn plan_run_entries(
    root: &Path,
    suffix: &str,
    expired: &[&RunId],
    quota: u64,
    oldest_first: &[RunId],
    active: &HashSet<String>,
) -> (Vec<PathBuf>, u64) {
    let name = |id: &RunId| format!("{id}{suffix}");
    let mut chosen: Vec<PathBuf> = Vec::new();
    let mut seen = HashSet::new();
    let mut bytes = 0u64;
    for id in expired {
        let p = root.join(name(id));
        if p.exists() && seen.insert(p.clone()) {
            bytes += tree_bytes(&p);
            chosen.push(p);
        }
    }
    let total: u64 = std::fs::read_dir(root)
        .map(|rd| rd.flatten().map(|e| tree_bytes(&e.path())).sum())
        .unwrap_or(0);
    let mut remaining = total.saturating_sub(bytes);
    for id in oldest_first {
        if remaining <= quota {
            break;
        }
        if active.contains(id.as_str()) {
            continue;
        }
        let p = root.join(name(id));
        if p.exists() && seen.insert(p.clone()) {
            let b = tree_bytes(&p);
            bytes += b;
            remaining = remaining.saturating_sub(b);
            chosen.push(p);
        }
    }
    (chosen, bytes)
}

fn plan_cache(
    cache_dir: &Path,
    policy: &StoragePolicy,
    active: &HashSet<String>,
) -> (Vec<PathBuf>, u64) {
    let now = SystemTime::now();
    let max_age = DAY * u32::try_from(policy.cache_days).unwrap_or(u32::MAX);
    let mut files: Vec<(SystemTime, PathBuf, u64)> = Vec::new();
    fn walk(dir: &Path, depth: usize, out: &mut Vec<(SystemTime, PathBuf, u64)>) {
        if depth > 6 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let Ok(meta) = std::fs::symlink_metadata(e.path()) else {
                continue;
            };
            if meta.is_dir() {
                walk(&e.path(), depth + 1, out);
            } else {
                out.push((
                    meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    e.path(),
                    meta.len(),
                ));
            }
        }
    }
    walk(cache_dir, 0, &mut files);
    let inputs = cache_dir.join("inputs");
    // Inputs of active runs are in use; leftovers of inactive runs go after 24 hours.
    files.retain(|(_, p, _)| {
        if p.parent() == Some(inputs.as_path()) {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let run = name.split('.').next().unwrap_or("");
            return !active.contains(run);
        }
        true
    });
    files.sort();
    let total: u64 = files.iter().map(|f| f.2).sum();
    let mut remaining = total;
    let mut chosen = Vec::new();
    let mut bytes = 0;
    for (mtime, p, len) in files {
        let age = now.duration_since(mtime).unwrap_or_default();
        let is_input = p.parent() == Some(inputs.as_path());
        let expired = if is_input { age > DAY } else { age > max_age };
        if expired || remaining > policy.cache_bytes_per_workspace {
            remaining = remaining.saturating_sub(len);
            bytes += len;
            chosen.push(p);
        }
    }
    (chosen, bytes)
}

fn plan_files(
    paths: &WorkspacePaths,
    sel: &GcSelection,
    policy: &StoragePolicy,
    kind: GcKind,
    active: &HashSet<String>,
) -> FilePlan {
    let want = |k: GcKind| {
        kind == GcKind::All || kind == k || (kind == GcKind::History && k != GcKind::Cache)
    };
    let mut plan = FilePlan::default();
    if want(GcKind::Logs) {
        let expired: Vec<&RunId> = sel
            .runs
            .iter()
            .chain(if kind == GcKind::History {
                [].iter()
            } else {
                sel.old_logs.iter()
            })
            .collect();
        let quota = if kind == GcKind::History {
            u64::MAX
        } else {
            policy.log_bytes_per_workspace
        };
        let (dirs, b) = plan_run_dirs(
            &paths.logs_dir.join("runs"),
            &expired,
            quota,
            &sel.finished_oldest_first,
            active,
        );
        plan.logs = dirs;
        plan.bytes[0] = b;
    }
    if want(GcKind::Artifacts) {
        let expired: Vec<&RunId> = sel
            .runs
            .iter()
            .chain(if kind == GcKind::History {
                [].iter()
            } else {
                sel.old_artifacts.iter()
            })
            .collect();
        let quota = if kind == GcKind::History {
            u64::MAX
        } else {
            policy.artifact_bytes_per_workspace
        };
        let (dirs, b) = plan_run_dirs(
            &paths.state_dir.join("artifacts"),
            &expired,
            quota,
            &sel.finished_oldest_first,
            active,
        );
        plan.artifacts = dirs;
        plan.bytes[1] = b;
    }
    if matches!(kind, GcKind::History | GcKind::All) {
        let expired: Vec<&RunId> = sel.runs.iter().collect();
        let (files, b) = plan_run_entries(
            &paths.state_dir.join("results"),
            ".json",
            &expired,
            policy.result_bytes_per_workspace,
            &sel.finished_oldest_first,
            active,
        );
        plan.results = files;
        plan.bytes[3] = b;
    }
    if want(GcKind::Cache) {
        let (files, b) = plan_cache(&paths.cache_dir, policy, active);
        plan.cache = files;
        plan.bytes[2] = b;
    }
    plan
}

fn apply_files(paths: &WorkspacePaths, plan: &FilePlan) -> [u64; 4] {
    let logs_root = paths.logs_dir.join("runs");
    let art_root = paths.state_dir.join("artifacts");
    let results_root = paths.state_dir.join("results");
    let mut freed = [0u64; 4];
    for p in &plan.results {
        freed[3] += remove_managed(&results_root, p);
    }
    for p in &plan.logs {
        freed[0] += remove_managed(&logs_root, p);
    }
    for p in &plan.artifacts {
        freed[1] += remove_managed(&art_root, p);
    }
    for p in &plan.cache {
        if let Some(parent) = p.parent()
            && parent.starts_with(&paths.cache_dir)
        {
            freed[2] += remove_managed(parent, p);
        }
    }
    freed
}

impl Actor {
    fn active_run_ids(&self) -> Vec<RunId> {
        self.runs.keys().cloned().collect()
    }

    pub(super) fn storage_status(&mut self, p: StorageStatusParams, r: Responder) {
        let Ok(storage) = self.storage.clone() else {
            return r.send(self.fail(ErrorInfo::new(
                ErrorCode::STORAGE_UNAVAILABLE,
                "storage is unavailable",
            )));
        };
        let ctx = self.ctx();
        let paths = self.paths.clone();
        let policy = self
            .accepted()
            .map(|s| s.storage.clone())
            .unwrap_or_default();
        tokio::spawn(async move {
            let reply = match storage.status().await {
                Ok(mut data) => {
                    let (others, budgets) = tokio::task::spawn_blocking(move || {
                        let others = if p.all {
                            other_workspaces(&paths)
                        } else {
                            vec![]
                        };
                        (others, budgets_for(&policy))
                    })
                    .await
                    .unwrap_or_default();
                    for u in &mut data.usage {
                        if let Some((_, b)) = budgets.iter().find(|(c, _)| *c == u.class) {
                            u.budget_bytes = Some(*b);
                        }
                    }
                    data.other_workspaces = others;
                    reply_ok(ctx, data, ReplyMeta::default())
                }
                Err(e) => reply_fail(ctx, e.to_error_info()),
            };
            r.send(reply);
        });
    }

    pub(super) fn storage_gc(&mut self, p: StorageGcParams, r: Option<Responder>) {
        let Ok(storage) = self.storage.clone() else {
            if let Some(r) = r {
                r.send(self.fail(ErrorInfo::new(
                    ErrorCode::STORAGE_UNAVAILABLE,
                    "storage is unavailable",
                )));
            }
            return;
        };
        self.last_gc = Some(Instant::now());
        let ctx = self.ctx();
        let paths = self.paths.clone();
        let policy = self
            .accepted()
            .map(|s| s.storage.clone())
            .unwrap_or_default();
        let active = self.active_run_ids();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let now = Timestamp::now().unix_ms();
            let result = async {
                let sel = storage
                    .gc_select(gc_policy(&policy), now, active.clone())
                    .await?;
                let active_set: HashSet<String> = active.iter().map(ToString::to_string).collect();
                let (sel2, paths2, policy2) = (sel.clone(), paths.clone(), policy.clone());
                let plan = tokio::task::spawn_blocking(move || {
                    plan_files(&paths2, &sel2, &policy2, p.kind, &active_set)
                })
                .await
                .unwrap_or_default();
                let history = matches!(p.kind, GcKind::History | GcKind::All);
                let mut bytes = plan.bytes;
                if p.apply {
                    // Files first, then the index rows: a retry after a failure finds the same rows.
                    let paths3 = paths.clone();
                    let plan_cl = plan.clone();
                    bytes = tokio::task::spawn_blocking(move || apply_files(&paths3, &plan_cl))
                        .await
                        .unwrap_or_default();
                    if history {
                        storage.gc_apply(sel.clone(), now, active.clone()).await?;
                    }
                }
                let history_records = if history {
                    (sel.runs.len() + sel.views.len()) as u64 + sel.request_keys
                } else {
                    0
                };
                let entries = vec![
                    GcEntry {
                        kind: GcKind::History,
                        records: history_records,
                        bytes: bytes[3],
                    },
                    GcEntry {
                        kind: GcKind::Logs,
                        records: plan.logs.len() as u64,
                        bytes: bytes[0],
                    },
                    GcEntry {
                        kind: GcKind::Artifacts,
                        records: plan.artifacts.len() as u64,
                        bytes: bytes[1],
                    },
                    GcEntry {
                        kind: GcKind::Cache,
                        records: plan.cache.len() as u64,
                        bytes: bytes[2],
                    },
                ]
                .into_iter()
                .filter(|e| {
                    p.kind == GcKind::All
                        || e.kind == p.kind
                        || (p.kind == GcKind::History && e.kind != GcKind::Cache)
                })
                .collect();
                Ok::<_, crate::storage::StorageError>((
                    GcReport {
                        applied: p.apply,
                        entries,
                    },
                    if p.apply && history {
                        (sel.views, sel.runs)
                    } else {
                        (vec![], vec![])
                    },
                ))
            }
            .await;
            match result {
                Ok((report, (cleaned, runs))) => {
                    if !cleaned.is_empty() || !runs.is_empty() {
                        let _ = tx
                            .send(super::Msg::ViewsCleaned {
                                views: cleaned,
                                runs,
                                at: now,
                            })
                            .await;
                    }
                    if let Some(r) = r {
                        r.send(reply_ok(ctx, report, ReplyMeta::default()));
                    } else if report.entries.iter().any(|e| e.records > 0) {
                        diag(format!("automatic cleanup: {:?}", report.entries));
                    }
                }
                Err(e) => match r {
                    Some(r) => r.send(reply_fail(ctx, e.to_error_info())),
                    None => diag(format!("automatic cleanup failed: {e}")),
                },
            }
        });
    }

    /// Low-priority incremental GC while active, at most every five minutes.
    pub(super) fn retention_tick(&mut self) {
        let due = self.last_gc.is_none_or(|t| t.elapsed() >= AUTO_GC_EVERY);
        if due && self.storage.is_ok() && (self.session.is_some() || !self.clients.is_empty()) {
            self.storage_gc(
                StorageGcParams {
                    kind: GcKind::All,
                    apply: true,
                },
                None,
            );
        }
    }

    pub(super) fn storage_clear(&mut self, p: StorageClearParams, r: Responder) {
        let busy = self.runs.values().any(|run| {
            run.record
                .action_ref
                .as_ref()
                .is_some_and(|a| a.plugin == p.plugin)
        });
        if busy {
            return r.send(
                self.fail(
                    ErrorInfo::new(
                        ErrorCode::BUSY,
                        format!("plugin `{}` has an active run; stop it first", p.plugin),
                    )
                    .retryable(true),
                ),
            );
        }
        let dir = self.paths.plugin_state(&p.plugin);
        let Some(parent) = dir.parent().map(Path::to_path_buf) else {
            return r.send(self.fail(ErrorInfo::new(ErrorCode::INTERNAL, "invalid state path")));
        };
        let ctx = self.ctx();
        tokio::spawn(async move {
            let freed = tokio::task::spawn_blocking(move || remove_managed(&parent, &dir))
                .await
                .unwrap_or(0);
            diag(format!(
                "cleared private state of plugin {} ({freed} bytes)",
                p.plugin
            ));
            r.send(reply_ok(ctx, Ack { ok: true }, ReplyMeta::default()));
        });
    }
}

fn budgets_for(p: &StoragePolicy) -> Vec<(PathClass, u64)> {
    vec![
        (PathClass::Logs, p.log_bytes_per_workspace),
        (PathClass::Artifacts, p.artifact_bytes_per_workspace),
        (PathClass::Cache, p.cache_bytes_per_workspace),
    ]
}

/// Other workspaces' usage, observed only: nothing is started, stopped, or deleted.
fn other_workspaces(paths: &WorkspacePaths) -> Vec<OtherWorkspace> {
    let Some(base) = paths.state_dir.parent() else {
        return vec![];
    };
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(base) else {
        return out;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name == paths.id.as_str() || !name.starts_with("w_") {
            continue;
        }
        let logs = paths
            .logs_dir
            .parent()
            .map(|p| p.join(&name))
            .unwrap_or_default();
        let cache = paths
            .cache_dir
            .parent()
            .map(|p| p.join(&name))
            .unwrap_or_default();
        out.push(OtherWorkspace {
            id: name,
            state_bytes: tree_bytes(&e.path()),
            log_bytes: tree_bytes(&logs),
            cache_bytes: tree_bytes(&cache),
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}
