//! The SQLite side of the ledger. Only the storage thread holds a [`Db`].

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use mira_protocol::error::ErrorCode;
use mira_protocol::ids::{ActionRef, CatalogRevision, Digest, RunId, ViewRef, ViewRevision};
use mira_protocol::ipc::{PathClass, StorageStatusData, StorageUsage, Warning};
use mira_protocol::limits::MAX_SAFE_INTEGER;
use mira_protocol::paths::WorkspacePaths;
use mira_protocol::run::{Lifecycle, Outcome, RunRecord};
use mira_protocol::time::Timestamp;
use rusqlite::{Connection, ErrorCode as SqlCode, OpenFlags, OptionalExtension, params};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::schema::SCHEMA;
use super::{
    Claim, GcPolicy, GcSelection, KeyClaim, OpenReport, RunFilter, SCHEMA_VERSION, StorageError,
    StoredView, VIEW_REVISION_BLOCK,
};

/// Request-key reservations are honored for this long.
const REQUEST_KEY_TTL_MS: i64 = 24 * 60 * 60 * 1000;
/// The oldest SQLite release with the 2026 WAL-reset fix.
const MIN_SQLITE: (u32, u32, u32) = (3, 51, 3);
const DEFAULT_RUN_LIMIT: usize = 50;
const MAX_RUN_LIMIT: usize = 1000;
/// Directory walks in `status` stop after this many entries to stay cheap.
const MAX_WALK_ENTRIES: u64 = 100_000;

type Result<T> = std::result::Result<T, StorageError>;

pub(super) struct Db {
    conn: Connection,
    paths: WorkspacePaths,
    sqlite_version: String,
    warnings: Vec<Warning>,
}

fn unavailable(what: &str, e: impl std::fmt::Display) -> StorageError {
    StorageError::Unavailable(format!("{what}: {e}"))
}

/// Maps a SQLite error: damage becomes `Corrupt`, everything else `Unavailable`.
fn sql(what: &str, e: rusqlite::Error) -> StorageError {
    match e.sqlite_error_code() {
        Some(SqlCode::NotADatabase | SqlCode::DatabaseCorrupt) => {
            StorageError::Corrupt(format!("{what}: {e}"))
        }
        _ => unavailable(what, e),
    }
}

fn corrupt(what: &str, e: impl std::fmt::Display) -> StorageError {
    StorageError::Corrupt(format!("{what}: {e}"))
}

fn warning(code: ErrorCode, message: impl Into<String>) -> Warning {
    Warning {
        code,
        message: message.into(),
        subject: None,
    }
}

/// A unit enum's serde name, e.g. `Outcome::TimedOut` → `timed_out`.
fn enum_name<T: Serialize>(v: &T) -> Result<String> {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => Ok(s),
        Ok(other) => Err(unavailable("encode enum", other)),
        Err(e) => Err(unavailable("encode enum", e)),
    }
}

fn enum_from_name<T: DeserializeOwned>(what: &str, s: String) -> Result<T> {
    serde_json::from_value(serde_json::Value::String(s)).map_err(|e| corrupt(what, e))
}

fn lifecycle_name(l: Lifecycle) -> &'static str {
    match l {
        Lifecycle::Starting => "starting",
        Lifecycle::Running => "running",
        Lifecycle::Stopping { .. } => "stopping",
        Lifecycle::Finished { .. } => "finished",
    }
}

fn parse_version(v: &str) -> Option<(u32, u32, u32)> {
    let mut it = v.split('.').map(|p| p.parse::<u32>().ok());
    Some((it.next()??, it.next()??, it.next().flatten().unwrap_or(0)))
}

fn u64_col(v: i64, what: &str) -> Result<u64> {
    u64::try_from(v).map_err(|_| corrupt(what, "negative value"))
}

fn i64_of(v: u64, what: &str) -> Result<i64> {
    i64::try_from(v).map_err(|_| unavailable(what, "value out of range"))
}

// ---------- files and permissions ----------

fn ensure_private_dir(dir: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| unavailable(&format!("create {}", dir.display()), e))?;
    let meta = fs::metadata(dir).map_err(|e| unavailable("stat state dir", e))?;
    if meta.permissions().mode() & 0o777 != 0o700 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
            .map_err(|e| unavailable("restrict state dir", e))?;
    }
    Ok(())
}

fn restrict_file(path: &Path) -> Result<()> {
    let meta = fs::metadata(path).map_err(|e| unavailable("stat file", e))?;
    if meta.permissions().mode() & 0o077 != 0 {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|e| unavailable(&format!("restrict {}", path.display()), e))?;
    }
    Ok(())
}

/// Loads the 32-byte fingerprint key, creating it once with mode 0600.
fn load_or_create_key(path: &Path) -> Result<[u8; 32]> {
    let mut key = [0u8; 32];
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(mut f) => {
            getrandom::fill(&mut key).map_err(|e| unavailable("generate fingerprint key", e))?;
            f.write_all(&key)
                .and_then(|()| f.sync_all())
                .map_err(|e| unavailable("write fingerprint key", e))?;
            Ok(key)
        }
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            restrict_file(path)?;
            let mut buf = Vec::with_capacity(33);
            fs::File::open(path)
                .and_then(|f| f.take(33).read_to_end(&mut buf))
                .map_err(|e| unavailable("read fingerprint key", e))?;
            if buf.len() != key.len() {
                return Err(StorageError::Corrupt(format!(
                    "fingerprint key has {} bytes, expected 32",
                    buf.len()
                )));
            }
            key.copy_from_slice(&buf);
            Ok(key)
        }
        Err(e) => Err(unavailable("create fingerprint key", e)),
    }
}

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

// ---------- open and create ----------

impl Db {
    /// Opens (or creates) the workspace database. Returns the key and what was found.
    pub(super) fn open(paths: &WorkspacePaths) -> Result<(Db, [u8; 32], OpenReport)> {
        ensure_private_dir(&paths.state_dir)?;
        let key = load_or_create_key(&paths.fingerprint_key())?;

        let db_path = paths.state_db();
        let created = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&db_path)
        {
            Ok(_) => true,
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                restrict_file(&db_path)?;
                false
            }
            Err(e) => return Err(unavailable("create state database", e)),
        };

        // The file exists now; never let SQLite create a replacement.
        let conn = Connection::open_with_flags(
            &db_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| sql("open state database", e))?;
        conn.busy_timeout(std::time::Duration::from_millis(1000))
            .map_err(|e| sql("set busy_timeout", e))?;

        let mut db = Db {
            conn,
            paths: paths.clone(),
            sqlite_version: String::new(),
            warnings: Vec::new(),
        };

        // Read-only check first: nothing below writes until the file is known good.
        let fresh = db.check_schema_version(&db_path)?;
        db.sqlite_version = db
            .conn
            .query_row("SELECT sqlite_version()", [], |r| r.get(0))
            .map_err(|e| sql("read sqlite_version", e))?;
        if parse_version(&db.sqlite_version).is_none_or(|v| v < MIN_SQLITE) {
            db.warnings.push(warning(
                ErrorCode::STORAGE_UNAVAILABLE,
                format!(
                    "bundled SQLite {} is older than the required 3.51.3",
                    db.sqlite_version
                ),
            ));
        }
        db.apply_pragmas(fresh)?;
        if fresh {
            db.create_schema()?;
        }
        db.check_workspace()?;
        let interrupted = db.interrupt_active_runs()?;
        let report = OpenReport {
            created,
            sqlite_version: db.sqlite_version.clone(),
            interrupted,
        };
        Ok((db, key, report))
    }

    /// Returns true for an empty database. Refuses a database with another schema version.
    fn check_schema_version(&self, db_path: &Path) -> Result<bool> {
        let tables: i64 = self
            .conn
            .query_row("SELECT count(*) FROM sqlite_schema", [], |r| r.get(0))
            .map_err(|e| sql("read state database", e))?;
        let found = self.pragma_i64("user_version")?;
        if tables == 0 && found == 0 {
            return Ok(true);
        }
        if found == i64::from(SCHEMA_VERSION) {
            return Ok(false);
        }
        Err(StorageError::SchemaMismatch {
            found,
            path: db_path.display().to_string(),
        })
    }

    fn pragma_i64(&self, name: &str) -> Result<i64> {
        self.conn
            .query_row(&format!("PRAGMA {name}"), [], |r| r.get(0))
            .map_err(|e| sql(&format!("read PRAGMA {name}"), e))
    }

    /// Sets a PRAGMA and checks the value SQLite reports back.
    fn set_pragma(&self, name: &str, value: &str, expect: i64) -> Result<()> {
        self.conn
            .execute_batch(&format!("PRAGMA {name} = {value}"))
            .map_err(|e| sql(&format!("set PRAGMA {name}"), e))?;
        let got = self.pragma_i64(name)?;
        if got != expect {
            return Err(StorageError::Unavailable(format!(
                "PRAGMA {name} reports {got}, expected {expect}"
            )));
        }
        Ok(())
    }

    fn apply_pragmas(&mut self, fresh: bool) -> Result<()> {
        if fresh {
            // Must precede the first table.
            self.set_pragma("auto_vacuum", "INCREMENTAL", 2)?;
        } else if self.pragma_i64("auto_vacuum")? != 2 {
            self.warnings.push(warning(
                ErrorCode::STORAGE_UNAVAILABLE,
                "auto_vacuum is not INCREMENTAL on this database; space is reclaimed only by a full VACUUM",
            ));
        }
        let mode: String = self
            .conn
            .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
            .map_err(|e| sql("set PRAGMA journal_mode", e))?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(StorageError::Unavailable(format!(
                "PRAGMA journal_mode reports {mode}, expected wal"
            )));
        }
        self.set_pragma("synchronous", "FULL", 2)?;
        self.set_pragma("fullfsync", "ON", 1)?;
        self.set_pragma("foreign_keys", "ON", 1)?;
        self.set_pragma("busy_timeout", "1000", 1000)?;
        self.set_pragma("cache_size", "-4096", -4096)?;
        // These two PRAGMAs return the new value from the setter itself.
        let wal: i64 = self
            .conn
            .query_row("PRAGMA wal_autocheckpoint = 1000", [], |r| r.get(0))
            .map_err(|e| sql("set PRAGMA wal_autocheckpoint", e))?;
        let limit: i64 = self
            .conn
            .query_row("PRAGMA journal_size_limit = 8388608", [], |r| r.get(0))
            .map_err(|e| sql("set PRAGMA journal_size_limit", e))?;
        if wal != 1000 || limit != 8_388_608 {
            return Err(StorageError::Unavailable(format!(
                "PRAGMA wal_autocheckpoint/journal_size_limit report {wal}/{limit}"
            )));
        }
        Ok(())
    }

    fn create_schema(&mut self) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| sql("begin schema creation", e))?;
        tx.execute_batch(SCHEMA)
            .map_err(|e| sql("create schema", e))?;
        tx.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))
            .map_err(|e| sql("set PRAGMA user_version", e))?;
        tx.commit().map_err(|e| sql("commit schema creation", e))
    }

    fn check_workspace(&mut self) -> Result<()> {
        let root = self.paths.root.as_str().to_owned();
        let wid = self.paths.id.as_str().to_owned();
        let stored: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT workspace_id, root FROM workspace_meta WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|e| sql("read workspace_meta", e))?;
        match stored {
            Some((w, r)) if w == wid && r == root => Ok(()),
            Some((w, r)) => Err(StorageError::WorkspaceCollision(format!(
                "database records {w} at {r}, this host serves {wid} at {root}"
            ))),
            None => self
                .conn
                .execute(
                    "INSERT INTO workspace_meta (id, workspace_id, root, created_at)
                     VALUES (1, ?1, ?2, ?3)",
                    params![wid, root, Timestamp::now().unix_ms()],
                )
                .map(drop)
                .map_err(|e| sql("write workspace_meta", e)),
        }
    }

    /// Marks runs left active by a previous host as finished/interrupted.
    fn interrupt_active_runs(&mut self) -> Result<Vec<RunId>> {
        let rows: Vec<(String, String)> = {
            let mut stmt = self
                .conn
                .prepare("SELECT run_id, record_json FROM runs WHERE lifecycle <> 'finished'")
                .map_err(|e| sql("read active runs", e))?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .and_then(|it| it.collect())
                .map_err(|e| sql("read active runs", e))?
        };
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let now = Timestamp::now();
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| sql("begin recovery", e))?;
        let mut ids = Vec::with_capacity(rows.len());
        for (id, json) in rows {
            let run_id = RunId::parse(id.clone()).map_err(|e| corrupt("stored run_id", e))?;
            match serde_json::from_str::<RunRecord>(&json) {
                Ok(mut rec) => {
                    let was = lifecycle_name(rec.lifecycle);
                    rec.lifecycle = Lifecycle::Finished {
                        outcome: Outcome::Interrupted,
                    };
                    rec.ended_at.get_or_insert(now);
                    rec.note = Some(format!(
                        "the host stopped unexpectedly while this run was {was}; \
                         its real final state was not confirmed"
                    ));
                    upsert_run(&tx, &rec)?;
                }
                Err(_) => {
                    // The body cannot be decoded; still end the row so it never looks active.
                    tx.execute(
                        "UPDATE runs SET lifecycle = 'finished', outcome = 'interrupted',
                         ended_at = coalesce(ended_at, ?2) WHERE run_id = ?1",
                        params![id, now.unix_ms()],
                    )
                    .map_err(|e| sql("interrupt run", e))?;
                }
            }
            ids.push(run_id);
        }
        tx.commit().map_err(|e| sql("commit recovery", e))?;
        Ok(ids)
    }

    /// A bounded checkpoint at shutdown. A busy WAL stays for SQLite to recover next time.
    pub(super) fn close(self) {
        let _ = self
            .conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                r.get::<_, i64>(0)
            });
        let _ = self.conn.close();
    }
}

// ---------- operations ----------

fn upsert_run(conn: &Connection, rec: &RunRecord) -> Result<()> {
    let json = serde_json::to_string(rec).map_err(|e| unavailable("encode run", e))?;
    let outcome = match rec.lifecycle {
        Lifecycle::Finished { outcome } => Some(enum_name(&outcome)?),
        _ => None,
    };
    conn.execute(
        "INSERT INTO runs (run_id, action_ref, source, definition_hash, catalog_revision,
                           started_at, ended_at, lifecycle, outcome, record_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT (run_id) DO UPDATE SET
            action_ref = excluded.action_ref, source = excluded.source,
            definition_hash = excluded.definition_hash,
            catalog_revision = excluded.catalog_revision, started_at = excluded.started_at,
            ended_at = excluded.ended_at, lifecycle = excluded.lifecycle,
            outcome = excluded.outcome, record_json = excluded.record_json",
        params![
            rec.run_id.as_str(),
            rec.action_ref.as_ref().map(ToString::to_string),
            enum_name(&rec.source)?,
            rec.definition_hash.as_str(),
            i64_of(rec.catalog_revision.get(), "catalog_revision")?,
            rec.started_at.unix_ms(),
            rec.ended_at.map(Timestamp::unix_ms),
            lifecycle_name(rec.lifecycle),
            outcome,
            json,
        ],
    )
    .map(drop)
    .map_err(|e| sql("write run", e))
}

fn decode_run(json: &str) -> Result<RunRecord> {
    serde_json::from_str(json).map_err(|e| corrupt("stored run record", e))
}

impl Db {
    fn immediate(&mut self, what: &str) -> Result<rusqlite::Transaction<'_>> {
        self.conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| sql(what, e))
    }

    pub(super) fn claim_key(&mut self, claim: &KeyClaim, reference: &str) -> Result<Claim> {
        let scope = claim.scope.as_key();
        let now = Timestamp::now().unix_ms();
        let tx = self.immediate("begin request-key claim")?;
        let existing: Option<(String, String, i64)> = tx
            .query_row(
                "SELECT fingerprint, reference, created_at FROM request_keys
                 WHERE scope = ?1 AND request_key = ?2",
                params![scope, claim.key.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(|e| sql("read request key", e))?;
        let result = match existing {
            None => {
                tx.execute(
                    "INSERT INTO request_keys (scope, request_key, fingerprint, reference, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![scope, claim.key.as_str(), claim.fingerprint.as_str(), reference, now],
                )
                .map_err(|e| sql("write request key", e))?;
                Claim::New
            }
            Some((fp, stored_ref, created_at)) => {
                // An active run keeps its reservation past 24 h.
                let still_active: bool = tx
                    .query_row(
                        "SELECT EXISTS (SELECT 1 FROM runs
                         WHERE run_id = ?1 AND lifecycle <> 'finished')",
                        params![stored_ref],
                        |r| r.get(0),
                    )
                    .map_err(|e| sql("read request key run", e))?;
                if now.saturating_sub(created_at) > REQUEST_KEY_TTL_MS && !still_active {
                    tx.execute(
                        "UPDATE request_keys SET fingerprint = ?3, reference = ?4, created_at = ?5
                         WHERE scope = ?1 AND request_key = ?2",
                        params![
                            scope,
                            claim.key.as_str(),
                            claim.fingerprint.as_str(),
                            reference,
                            now
                        ],
                    )
                    .map_err(|e| sql("replace expired request key", e))?;
                    Claim::New
                } else if fp == claim.fingerprint.as_str() {
                    Claim::Same {
                        reference: stored_ref,
                    }
                } else {
                    Claim::Conflict
                }
            }
        };
        tx.commit().map_err(|e| sql("commit request key", e))?;
        Ok(result)
    }

    pub(super) fn insert_run(&mut self, rec: &RunRecord) -> Result<()> {
        let tx = self.immediate("begin run reservation")?;
        let exists: bool = tx
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM runs WHERE run_id = ?1)",
                params![rec.run_id.as_str()],
                |r| r.get(0),
            )
            .map_err(|e| sql("read run", e))?;
        if exists {
            return Err(StorageError::Unavailable(format!(
                "run {} is already reserved",
                rec.run_id
            )));
        }
        upsert_run(&tx, rec)?;
        tx.commit().map_err(|e| sql("commit run reservation", e))
    }

    pub(super) fn save_run(&mut self, rec: &RunRecord) -> Result<()> {
        let tx = self.immediate("begin run update")?;
        upsert_run(&tx, rec)?;
        tx.commit().map_err(|e| sql("commit run update", e))
    }

    pub(super) fn get_run(&self, id: &RunId) -> Result<Option<RunRecord>> {
        let json: Option<String> = self
            .conn
            .query_row(
                "SELECT record_json FROM runs WHERE run_id = ?1",
                params![id.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| sql("read run", e))?;
        json.as_deref().map(decode_run).transpose()
    }

    pub(super) fn list_runs(&self, f: &RunFilter) -> Result<Vec<RunRecord>> {
        let limit = match f.limit {
            0 => DEFAULT_RUN_LIMIT,
            n => n.min(MAX_RUN_LIMIT),
        };
        let action = f.action_ref.as_ref().map(ActionRef::to_string);
        let outcome = f.outcome.as_ref().map(enum_name).transpose()?;
        let (before_ms, before_id) = match &f.before {
            Some((ts, id)) => (Some(ts.unix_ms()), Some(id.as_str().to_owned())),
            None => (None, None),
        };
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT record_json FROM runs
                 WHERE (?1 IS NULL OR action_ref = ?1)
                   AND (?2 IS NULL OR outcome = ?2)
                   AND (?3 IS NULL OR started_at < ?3 OR (started_at = ?3 AND run_id < ?4))
                 ORDER BY started_at DESC, run_id DESC
                 LIMIT ?5",
            )
            .map_err(|e| sql("list runs", e))?;
        let rows: Vec<String> = stmt
            .query_map(
                params![action, outcome, before_ms, before_id, limit as i64],
                |r| r.get(0),
            )
            .and_then(|it| it.collect())
            .map_err(|e| sql("list runs", e))?;
        rows.iter().map(|j| decode_run(j)).collect()
    }

    pub(super) fn catalog(&self) -> Result<(CatalogRevision, Option<Digest>)> {
        let (rev, hash): (i64, Option<String>) = self
            .conn
            .query_row(
                "SELECT catalog_revision, catalog_set_hash FROM workspace_meta WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(|e| sql("read catalog revision", e))?;
        let rev = CatalogRevision::new(u64_col(rev, "catalog_revision")?)
            .map_err(|e| corrupt("catalog_revision", e))?;
        let hash = hash
            .map(Digest::parse)
            .transpose()
            .map_err(|e| corrupt("catalog_set_hash", e))?;
        Ok((rev, hash))
    }

    fn run_ids(
        &self,
        sql_text: &str,
        args: &[&dyn rusqlite::ToSql],
        what: &str,
    ) -> Result<Vec<RunId>> {
        let mut stmt = self.conn.prepare(sql_text).map_err(|e| sql(what, e))?;
        let rows = stmt
            .query_map(args, |r| r.get::<_, String>(0))
            .map_err(|e| sql(what, e))?;
        let mut out = Vec::new();
        for row in rows {
            if let Ok(id) = RunId::parse(row.map_err(|e| sql(what, e))?) {
                out.push(id);
            }
        }
        Ok(out)
    }

    /// Selects finished runs, views, and request keys beyond retention.
    /// Active runs are excluded; request keys referring to active runs are kept.
    pub(super) fn gc_select(
        &self,
        p: &GcPolicy,
        now_ms: i64,
        active: &[RunId],
    ) -> Result<GcSelection> {
        const DAY: i64 = 86_400_000;
        let cut = |days: u64| {
            now_ms.saturating_sub(
                i64::try_from(days)
                    .unwrap_or(i64::MAX / DAY)
                    .saturating_mul(DAY),
            )
        };
        let active: std::collections::HashSet<&str> = active.iter().map(RunId::as_str).collect();
        let keep_active = |v: Vec<RunId>| -> Vec<RunId> {
            v.into_iter()
                .filter(|r| !active.contains(r.as_str()))
                .collect()
        };
        let per_action = i64_of(p.runs_per_action, "runs_per_action")?;
        let per_ws = i64_of(p.runs_per_workspace, "runs_per_workspace")?;
        let mut runs = self.run_ids(
            "SELECT run_id FROM runs WHERE lifecycle = 'finished' AND COALESCE(ended_at, started_at) < ?1
             UNION
             SELECT run_id FROM (SELECT run_id, lifecycle, ROW_NUMBER() OVER (PARTITION BY action_ref ORDER BY started_at DESC) AS rn
                                 FROM runs WHERE action_ref IS NOT NULL) WHERE rn > ?2 AND lifecycle = 'finished'
             UNION
             SELECT run_id FROM (SELECT run_id, lifecycle, ROW_NUMBER() OVER (ORDER BY started_at DESC) AS rn FROM runs)
                          WHERE rn > ?3 AND lifecycle = 'finished'",
            &[&cut(p.history_days), &per_action, &per_ws],
            "select expired runs",
        )?;
        runs = keep_active(runs);
        let doomed: std::collections::HashSet<&str> = runs.iter().map(RunId::as_str).collect();
        let older = |days: u64, what: &str| -> Result<Vec<RunId>> {
            Ok(self
                .run_ids(
                    "SELECT run_id FROM runs WHERE lifecycle = 'finished' AND COALESCE(ended_at, started_at) < ?1",
                    &[&cut(days)],
                    what,
                )?
                .into_iter()
                .filter(|r| !doomed.contains(r.as_str()) && !active.contains(r.as_str()))
                .collect())
        };
        let old_logs = older(p.log_days, "select old logs")?;
        let old_artifacts = older(p.artifact_days, "select old artifacts")?;
        let finished_oldest_first = keep_active(self.run_ids(
            "SELECT run_id FROM runs WHERE lifecycle = 'finished' ORDER BY COALESCE(ended_at, started_at) ASC",
            &[],
            "list finished runs",
        )?);
        let mut views = Vec::new();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT view_ref, revision FROM views WHERE recorded_at < ?1")
                .map_err(|e| sql("select expired views", e))?;
            let rows = stmt
                .query_map(params![cut(p.view_days)], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                })
                .map_err(|e| sql("select expired views", e))?;
            for row in rows {
                let (v, rev) = row.map_err(|e| sql("select expired view", e))?;
                if let Ok(v) = v.parse::<ViewRef>() {
                    views.push((v, u64::try_from(rev).unwrap_or(0)));
                }
            }
        }
        let request_keys: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM request_keys WHERE created_at < ?1",
                params![now_ms.saturating_sub(DAY)],
                |r| r.get(0),
            )
            .map_err(|e| sql("count expired request keys", e))?;
        Ok(GcSelection {
            runs,
            old_logs,
            old_artifacts,
            finished_oldest_first,
            views,
            request_keys: u64::try_from(request_keys).unwrap_or(0),
        })
    }

    /// Removes the selected index rows in one transaction; managed files are removed by the
    /// caller first. Expired request keys go too, except those pointing at active runs.
    pub(super) fn gc_apply(
        &mut self,
        sel: &GcSelection,
        now_ms: i64,
        active: &[RunId],
    ) -> Result<()> {
        let tx = self.immediate("begin retention cleanup")?;
        for id in &sel.runs {
            tx.execute(
                "DELETE FROM runs WHERE run_id = ?1 AND lifecycle = 'finished'",
                params![id.as_str()],
            )
            .map_err(|e| sql("delete expired run", e))?;
        }
        for (v, rev) in &sel.views {
            let rev = i64_of(*rev, "view revision")?;
            tx.execute(
                "DELETE FROM views WHERE view_ref = ?1 AND revision = ?2",
                params![v.to_string(), rev],
            )
            .map_err(|e| sql("delete expired view", e))?;
            tx.execute(
                "INSERT INTO cleaned_views (view_ref, revision, cleaned_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT (view_ref) DO UPDATE SET revision = excluded.revision, cleaned_at = excluded.cleaned_at",
                params![v.to_string(), rev, now_ms],
            )
            .map_err(|e| sql("record cleaned view", e))?;
        }
        let active: Vec<String> = active.iter().map(ToString::to_string).collect();
        let placeholders = if active.is_empty() {
            "''".to_owned()
        } else {
            vec!["?"; active.len()].join(",")
        };
        let cutoff = now_ms.saturating_sub(86_400_000);
        let q = format!(
            "DELETE FROM request_keys WHERE created_at < {cutoff} AND reference NOT IN ({placeholders})"
        );
        tx.execute(&q, rusqlite::params_from_iter(active.iter()))
            .map_err(|e| sql("delete expired request keys", e))?;
        tx.commit()
            .map_err(|e| sql("commit retention cleanup", e))?;
        // Return freed pages gradually; a full VACUUM never runs on the hot path.
        let _ = self.conn.execute_batch("PRAGMA incremental_vacuum(256);");
        Ok(())
    }

    /// Views removed by retention: (view, last revision, cleaned_at ms).
    pub(super) fn cleaned_views(&self) -> Result<Vec<(ViewRef, u64, i64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT view_ref, revision, cleaned_at FROM cleaned_views")
            .map_err(|e| sql("read cleaned views", e))?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
            .map_err(|e| sql("read cleaned views", e))?;
        let mut out = Vec::new();
        for row in rows {
            let (v, rev, at) = row.map_err(|e| sql("read cleaned view", e))?;
            if let Ok(v) = v.parse::<ViewRef>() {
                out.push((v, u64::try_from(rev).unwrap_or(0), at));
            }
        }
        Ok(out)
    }

    pub(super) fn set_schedule(&mut self, action_ref: &ActionRef, enabled: bool) -> Result<()> {
        let tx = self.immediate("begin schedule update")?;
        tx.execute(
            "INSERT INTO schedule_settings (action_ref, enabled, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT (action_ref) DO UPDATE SET enabled = excluded.enabled, updated_at = excluded.updated_at",
            params![action_ref.to_string(), i64::from(enabled), Timestamp::now().unix_ms()],
        )
        .map_err(|e| sql("write schedule setting", e))?;
        tx.commit().map_err(|e| sql("commit schedule setting", e))
    }

    pub(super) fn list_schedules(&mut self) -> Result<Vec<(ActionRef, bool)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT action_ref, enabled FROM schedule_settings ORDER BY action_ref")
            .map_err(|e| sql("read schedule settings", e))?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .map_err(|e| sql("read schedule settings", e))?;
        let mut out = Vec::new();
        for row in rows {
            let (r, enabled) = row.map_err(|e| sql("read schedule setting", e))?;
            // Rows for refs that no longer parse are ignored rather than failing the host.
            if let Ok(action_ref) = r.parse::<ActionRef>() {
                out.push((action_ref, enabled == 1));
            }
        }
        Ok(out)
    }

    pub(super) fn accept_catalog(&mut self, set_hash: &Digest) -> Result<CatalogRevision> {
        let (rev, hash) = self.catalog()?;
        if hash.as_ref() == Some(set_hash) {
            return Ok(rev);
        }
        let next = rev.next().ok_or(StorageError::CounterExhausted)?;
        let tx = self.immediate("begin catalog accept")?;
        tx.execute(
            "UPDATE workspace_meta SET catalog_revision = ?1, catalog_set_hash = ?2 WHERE id = 1",
            params![i64_of(next.get(), "catalog_revision")?, set_hash.as_str()],
        )
        .map_err(|e| sql("write catalog revision", e))?;
        tx.commit().map_err(|e| sql("commit catalog revision", e))?;
        Ok(next)
    }

    pub(super) fn reserve_view_block(&mut self) -> Result<(u64, u64)> {
        let tx = self.immediate("begin view block reservation")?;
        let next: i64 = tx
            .query_row(
                "SELECT next_view_revision FROM workspace_meta WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .map_err(|e| sql("read next_view_revision", e))?;
        let first = u64_col(next, "next_view_revision")?;
        let end = first
            .checked_add(VIEW_REVISION_BLOCK)
            .filter(|end| end - 1 <= MAX_SAFE_INTEGER)
            .ok_or(StorageError::CounterExhausted)?;
        tx.execute(
            "UPDATE workspace_meta SET next_view_revision = ?1 WHERE id = 1",
            params![i64_of(end, "next_view_revision")?],
        )
        .map_err(|e| sql("write next_view_revision", e))?;
        tx.commit().map_err(|e| sql("commit view block", e))?;
        Ok((first, end))
    }

    pub(super) fn save_view(&mut self, v: &StoredView) -> Result<()> {
        let tx = self.immediate("begin view save")?;
        let stored: Option<i64> = tx
            .query_row(
                "SELECT revision FROM views WHERE view_ref = ?1",
                params![v.view_ref.to_string()],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| sql("read view", e))?;
        let rev = i64_of(v.revision.get(), "view revision")?;
        if let Some(s) = stored
            && s >= rev
        {
            return Err(StorageError::Unavailable(format!(
                "view {} revision {rev} is not newer than stored revision {s}",
                v.view_ref
            )));
        }
        let data_hash = Digest::of_bytes(v.data_json.as_bytes());
        tx.execute(
            "INSERT INTO views (view_ref, revision, kind, recorded_at, source_run_id, source_kind,
                                definition_hash, data_hash, data_bytes, data_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT (view_ref) DO UPDATE SET
                revision = excluded.revision, kind = excluded.kind,
                recorded_at = excluded.recorded_at, source_run_id = excluded.source_run_id,
                source_kind = excluded.source_kind, definition_hash = excluded.definition_hash,
                data_hash = excluded.data_hash, data_bytes = excluded.data_bytes,
                data_json = excluded.data_json",
            params![
                v.view_ref.to_string(),
                rev,
                enum_name(&v.kind)?,
                v.recorded_at.unix_ms(),
                v.source_run_id.as_ref().map(RunId::as_str),
                enum_name(&v.source_kind)?,
                v.definition_hash.as_str(),
                data_hash.as_str(),
                v.data_json.len() as i64,
                v.data_json,
            ],
        )
        .map_err(|e| sql("write view", e))?;
        tx.commit().map_err(|e| sql("commit view", e))
    }

    pub(super) fn load_view(&self, view: &ViewRef) -> Result<Option<StoredView>> {
        type Row = (
            i64,
            String,
            i64,
            Option<String>,
            String,
            String,
            String,
            String,
        );
        let row: Option<Row> = self
            .conn
            .query_row(
                "SELECT revision, kind, recorded_at, source_run_id, source_kind,
                        definition_hash, data_hash, data_json
                 FROM views WHERE view_ref = ?1",
                params![view.to_string()],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                    ))
                },
            )
            .optional()
            .map_err(|e| sql("read view", e))?;
        let Some((rev, kind, at, run, source_kind, def, data_hash, data_json)) = row else {
            return Ok(None);
        };
        if Digest::of_bytes(data_json.as_bytes()).as_str() != data_hash {
            return Err(StorageError::Corrupt(format!(
                "view {view} body does not match its stored hash"
            )));
        }
        Ok(Some(StoredView {
            view_ref: view.clone(),
            revision: ViewRevision::new(u64_col(rev, "view revision")?)
                .map_err(|e| corrupt("view revision", e))?,
            kind: enum_from_name("view kind", kind)?,
            recorded_at: Timestamp::from_unix_ms(at),
            source_run_id: run
                .map(RunId::parse)
                .transpose()
                .map_err(|e| corrupt("view source_run_id", e))?,
            source_kind: enum_from_name("view source_kind", source_kind)?,
            definition_hash: Digest::parse(def).map_err(|e| corrupt("view definition_hash", e))?,
            data_json,
        }))
    }

    fn count(&self, table: &str) -> Option<u64> {
        self.conn
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| {
                r.get::<_, i64>(0)
            })
            .ok()
            .and_then(|n| u64::try_from(n).ok())
    }

    pub(super) fn status(&self) -> Result<StorageStatusData> {
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
