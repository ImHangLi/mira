//! The SQLite side of the ledger. Only the storage thread holds a [`Db`].
//!
//! Each submodule adds one group of `Db` operations: opening and recovery, runs and
//! request keys, catalog and schedule settings, views, retention, and usage status.

mod meta;
mod open;
mod retention;
mod runs;
mod status;
mod views;

use mira_protocol::error::ErrorCode;
use mira_protocol::ipc::Warning;
use mira_protocol::paths::WorkspacePaths;
use mira_protocol::run::Lifecycle;
use rusqlite::{Connection, ErrorCode as SqlCode};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::StorageError;

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

impl Db {
    fn immediate(&mut self, what: &str) -> Result<rusqlite::Transaction<'_>> {
        self.conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| sql(what, e))
    }
}
