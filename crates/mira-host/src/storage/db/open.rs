//! Opening and creating the database: file permissions, the fingerprint key, PRAGMAs,
//! the schema, and recovery of runs a previous host left active.

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use mira_protocol::error::ErrorCode;
use mira_protocol::ids::RunId;
use mira_protocol::paths::WorkspacePaths;
use mira_protocol::run::{Lifecycle, Outcome, RunRecord};
use mira_protocol::time::Timestamp;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::storage::schema::SCHEMA;
use crate::storage::{OpenReport, SCHEMA_VERSION, StorageError};

use super::runs::upsert_run;
use super::{Db, Result, corrupt, lifecycle_name, parse_version, sql, unavailable, warning};

/// The oldest SQLite release with the 2026 WAL-reset fix.
const MIN_SQLITE: (u32, u32, u32) = (3, 51, 3);

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

impl Db {
    /// Opens (or creates) the workspace database. Returns the key and what was found.
    pub(crate) fn open(paths: &WorkspacePaths) -> Result<(Db, [u8; 32], OpenReport)> {
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
    pub(crate) fn close(self) {
        let _ = self
            .conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                r.get::<_, i64>(0)
            });
        let _ = self.conn.close();
    }
}
