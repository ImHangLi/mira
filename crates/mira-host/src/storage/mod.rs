//! Workspace ledger: one SQLite connection owned by one dedicated thread.
//!
//! The actor talks to it only through [`Storage`], a cloneable handle that sends typed jobs
//! over a bounded channel and awaits their results. No other code opens the database.

use mira_protocol::error::{ErrorCode, ErrorInfo};
use mira_protocol::ids::{ActionRef, Digest, RequestKey, RunId, ViewRef, ViewRevision};
use mira_protocol::manifest::ViewKind;
use mira_protocol::run::Outcome;
use mira_protocol::time::Timestamp;
use mira_protocol::view::SourceKind;

/// Current schema version written by this binary.
pub const SCHEMA_VERSION: u32 = 1;
/// View revisions are reserved in blocks of this size.
pub const VIEW_REVISION_BLOCK: u64 = 1024;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum StorageError {
    /// A required write or read could not complete (disk full, permissions, IO).
    #[error("storage unavailable: {0}")]
    Unavailable(String),
    /// The database has another schema version. It is never changed in place.
    #[error(
        "state database {path} has schema version {found}, this build uses {SCHEMA_VERSION}; \
         stop the host with `mira down`, then delete the state database \
         (`mira paths` shows its location)"
    )]
    SchemaMismatch { found: i64, path: String },
    /// The database file exists but cannot be used; it is never replaced by an empty one.
    #[error("state database is damaged: {0}")]
    Corrupt(String),
    /// The workspace root recorded in the database differs (hash-prefix collision).
    #[error("state database belongs to another workspace root: {0}")]
    WorkspaceCollision(String),
    #[error("view revision counter exhausted")]
    CounterExhausted,
}

impl StorageError {
    pub fn to_error_info(&self) -> ErrorInfo {
        let code = match self {
            Self::Unavailable(_) | Self::Corrupt(_) | Self::SchemaMismatch { .. } => {
                ErrorCode::STORAGE_UNAVAILABLE
            }
            Self::WorkspaceCollision(_) => ErrorCode::WORKSPACE_ID_COLLISION,
            Self::CounterExhausted => ErrorCode::COUNTER_EXHAUSTED,
        };
        ErrorInfo::new(code, self.to_string())
    }
}

/// Idempotency scope for a request key: per action, per view, or per operation kind.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum KeyScope {
    Action(ActionRef),
    Exec,
    Publish(ViewRef),
    Apply,
}

impl KeyScope {
    pub fn as_key(&self) -> String {
        match self {
            Self::Action(a) => format!("action:{a}"),
            Self::Exec => "exec".into(),
            Self::Publish(v) => format!("publish:{v}"),
            Self::Apply => "apply".into(),
        }
    }
}

/// A request key plus the HMAC fingerprint of `{effective_input, definition_hash}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyClaim {
    pub scope: KeyScope,
    pub key: RequestKey,
    pub fingerprint: Digest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// The key was new and is now recorded for `run_id` (or publish revision).
    New,
    /// The key exists with the same fingerprint: return the original result.
    Same { reference: String },
    /// The key exists with a different fingerprint: REQUEST_KEY_CONFLICT.
    Conflict,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunFilter {
    pub action_ref: Option<ActionRef>,
    pub outcome: Option<Outcome>,
    /// Keyset pagination: only runs strictly older than this (started_at, run_id).
    pub before: Option<(Timestamp, RunId)>,
    pub limit: usize,
}

/// A stored view body. `data_json` is canonical ViewData JSON; decode it again on read.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredView {
    pub view_ref: ViewRef,
    pub revision: ViewRevision,
    pub kind: ViewKind,
    pub recorded_at: Timestamp,
    pub source_run_id: Option<RunId>,
    pub source_kind: SourceKind,
    pub definition_hash: Digest,
    pub data_json: String,
}

/// Retention limits the ledger applies; days are 24-hour periods.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcPolicy {
    pub history_days: u64,
    pub runs_per_action: u64,
    pub runs_per_workspace: u64,
    pub view_days: u64,
    pub log_days: u64,
    pub artifact_days: u64,
}

/// Records selected for removal. Active runs are never selected.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GcSelection {
    /// Finished runs whose summaries (and managed files) exceed retention.
    pub runs: Vec<RunId>,
    /// Kept runs whose logs are older than `log_days`.
    pub old_logs: Vec<RunId>,
    /// Kept runs whose artifacts are older than `artifact_days`.
    pub old_artifacts: Vec<RunId>,
    /// Finished runs, oldest first, for byte-quota trimming of logs and artifacts.
    pub finished_oldest_first: Vec<RunId>,
    pub views: Vec<(ViewRef, u64)>,
    pub request_keys: u64,
}

/// What `open` found.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenReport {
    pub created: bool,
    pub sqlite_version: String,
    /// Runs that were active when the previous host stopped unexpectedly; now `interrupted`.
    pub interrupted: Vec<RunId>,
}

mod db;
mod schema;
mod thread;

pub use thread::Storage;

// Operations provided by [`Storage`]. Every method is async and fails with [`StorageError`].
//
// - `open(paths) -> Result<(Storage, OpenReport)>` (sync): creates dirs 0700 / files 0600,
//   checks the schema version, applies PRAGMAs, creates the schema on a new database, checks
//   the root, loads `fingerprint.key`, and marks previously active runs `interrupted`.
// - `fingerprint(&Value) -> Digest` (sync, no IO): HMAC-SHA-256 over the JCS form.
// - `claim_key(KeyClaim, reference)` → [`Claim`]; commits before returning.
// - `insert_run(RunRecord)`: the reservation; commits before the caller spawns anything.
// - `save_run(RunRecord)`: upsert of lifecycle/result changes; commits.
// - `get_run(RunId) -> Option<RunRecord>`, `list_runs(RunFilter) -> Vec<RunRecord>` (newest first).
// - `catalog() -> (CatalogRevision, Option<Digest>)`; `accept_catalog(Digest) -> CatalogRevision`
//   bumps only when the set hash differs.
// - `reserve_view_block() -> (first, end_exclusive)`: persisted before use.
// - `save_view(StoredView)`, `load_view(ViewRef) -> Option<StoredView>`.
// - `status() -> StorageStatusData`.
