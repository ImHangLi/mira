//! Storage usage for `storage.status`.

use std::fs;
use std::path::{Path, PathBuf};

use mira_protocol::error::ErrorCode;
use mira_protocol::ipc::{PathClass, StorageStatusData, StorageUsage};

use crate::storage::SCHEMA_VERSION;

use super::{Db, Result, warning};

/// Directory walks in `status` stop after this many entries to stay cheap.
const MAX_WALK_ENTRIES: u64 = 100_000;

fn file_len(p: &Path) -> u64 {
    fs::symlink_metadata(p).map(|m| m.len()).unwrap_or(0)
}

/// Bytes and file count under `dir`, without following symlinks. Missing dirs are empty.
fn dir_usage(dir: &Path, budget: &mut u64) -> (u64, u64) {
    let mut bytes = 0u64;
    let mut files = 0u64;
    let mut stack: Vec<PathBuf> = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            if *budget == 0 {
                return (bytes, files);
            }
            *budget -= 1;
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                bytes = bytes.saturating_add(meta.len());
                files += 1;
            }
        }
    }
    (bytes, files)
}

impl Db {
    fn count(&self, table: &str) -> Option<u64> {
        self.conn
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| {
                r.get::<_, i64>(0)
            })
            .ok()
            .and_then(|n| u64::try_from(n).ok())
    }

    pub(crate) fn status(&self) -> Result<StorageStatusData> {
        let p = &self.paths;
        let db = p.state_db();
        let wal = PathBuf::from(format!("{}-wal", db.display()));
        let shm = PathBuf::from(format!("{}-shm", db.display()));
        let db_bytes = file_len(&db) + file_len(&wal) + file_len(&shm);

        let mut budget = MAX_WALK_ENTRIES;
        let mut walked = |dir: PathBuf| dir_usage(&dir, &mut budget);
        let (plugin_bytes, _) = walked(p.state_dir.join("plugins"));
        let (artifact_bytes, artifact_files) = walked(p.state_dir.join("artifacts"));
        let (log_bytes, log_files) = walked(p.logs_dir.join("runs"));
        let (cache_bytes, cache_files) = walked(p.cache_dir.clone());

        let usage = |class, bytes, records| StorageUsage {
            class,
            bytes,
            records,
            budget_bytes: None,
        };
        let runs = self.count("runs");
        let mut warnings = self.warnings.clone();
        if budget == 0 {
            warnings.push(warning(
                ErrorCode::STORAGE_UNAVAILABLE,
                format!(
                    "usage walk stopped after {MAX_WALK_ENTRIES} entries; sizes are lower bounds"
                ),
            ));
        }
        Ok(StorageStatusData {
            other_workspaces: Vec::new(),
            schema_version: SCHEMA_VERSION,
            sqlite_version: self.sqlite_version.clone(),
            usage: vec![
                usage(PathClass::StateDb, db_bytes, runs),
                usage(
                    PathClass::FingerprintKey,
                    file_len(&p.fingerprint_key()),
                    None,
                ),
                usage(PathClass::PluginState, plugin_bytes, None),
                usage(
                    PathClass::Artifacts,
                    artifact_bytes,
                    self.count("artifacts").or(Some(artifact_files)),
                ),
                usage(PathClass::Logs, log_bytes, Some(log_files)),
                usage(PathClass::HostLog, file_len(&p.host_log()), None),
                usage(PathClass::Cache, cache_bytes, Some(cache_files)),
            ],
            warnings,
        })
    }
}
