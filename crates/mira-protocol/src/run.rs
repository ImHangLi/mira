//! Run lifecycle, run records, and canonical log records.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::ErrorInfo;
use crate::ids::{ActionRef, CatalogRevision, Digest, LogSeq, RunId, SessionId, WorkspaceId};
use crate::mpp::HealthState;
use crate::reply::PayloadRef;
use crate::time::Timestamp;
use crate::view::{Freshness, LogLevel};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    User,
    SessionClosed,
    Timeout,
    TtlExpired,
    ProtocolError,
    OutputLimit,
    InternalError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
    Interrupted,
}

impl Outcome {
    /// The outcome in plain words for human output: `succeeded`, `stopped`, `timed out`.
    pub fn word(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "stopped",
            Self::TimedOut => "timed out",
            Self::Interrupted => "interrupted",
        }
    }
}

/// `starting → running → stopping → finished`; a failed start may go straight to finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum Lifecycle {
    Starting,
    Running,
    Stopping { reason: StopReason },
    Finished { outcome: Outcome },
}

impl Lifecycle {
    /// The state in plain words for human output; a finished run shows its outcome.
    pub fn word(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopping { .. } => "stopping",
            Self::Finished { outcome } => outcome.word(),
        }
    }
    pub fn is_active(self) -> bool {
        !matches!(self, Self::Finished { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReportedHealth {
    pub state: HealthState,
    pub message: Option<String>,
    pub updated_at: Option<Timestamp>,
}

impl ReportedHealth {
    pub fn unknown() -> Self {
        Self {
            state: HealthState::Unknown,
            message: None,
            updated_at: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExitInfo {
    pub code: Option<i32>,
    pub signal: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum CleanupState {
    NotNeeded,
    Pending,
    Running {
        started_at: Timestamp,
    },
    Succeeded {
        ended_at: Timestamp,
    },
    Failed {
        ended_at: Timestamp,
        /// Exit status of the cleanup command; `None` when it did not start or exit normally.
        exit_code: Option<i32>,
        /// The cleanup command was stopped at its time limit.
        timed_out: bool,
        error: ErrorInfo,
    },
    Unknown {
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunSource {
    Tui,
    Cli,
    Schedule,
    Hook,
}

/// Plugin-reported result, kept separate from the OS outcome.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunResult {
    pub ok: bool,
    pub summary: String,
    /// Inline when small; otherwise `null` with `payload` set.
    pub data: Value,
    pub payload: Option<PayloadRef>,
    pub error: Option<ErrorInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LogLocation {
    pub first_seq: Option<LogSeq>,
    pub last_seq: Option<LogSeq>,
    pub dropped_records: u64,
    pub truncated_records: u64,
}

/// Git context recorded at start. It is context, not proof of the working tree's contents.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitContext {
    pub head: Option<String>,
    pub branch: Option<String>,
}

/// Who asked for a one-off run: an agent the CLI recognized, or a name it gave with
/// `MIRA_AGENT`. `id` groups the runs of one agent instance (a session or a process); it is a
/// short hash, never an environment value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Requester {
    pub name: String,
    pub id: String,
    /// What the agent's thread works on, in a few words (`mira exec --task`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
}

impl Requester {
    /// A name of 1-48 characters without control characters, an ID of 1-32 of `[a-z0-9-]`,
    /// and a task of 1-60 characters without control characters.
    pub fn check(&self) -> Result<(), &'static str> {
        let name_ok = !self.name.trim().is_empty()
            && self.name.chars().count() <= 48
            && !self.name.chars().any(char::is_control);
        let id_ok = !self.id.is_empty()
            && self.id.len() <= 32
            && self
                .id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        let task_ok = self.task.as_ref().is_none_or(|t| {
            !t.trim().is_empty() && t.chars().count() <= 60 && !t.chars().any(char::is_control)
        });
        if name_ok && id_ok && task_ok {
            Ok(())
        } else {
            Err(
                "requester: name must be 1-48 characters and task 1-60, without control characters; id 1-32 of [a-z0-9-]",
            )
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunRecord {
    pub run_id: RunId,
    pub workspace_id: WorkspaceId,
    pub session_id: Option<SessionId>,
    /// `null` for ad-hoc `exec` runs, which are never catalog entries.
    pub action_ref: Option<ActionRef>,
    /// Action title or the `exec --label`.
    pub label: String,
    pub definition_hash: Digest,
    pub catalog_revision: CatalogRevision,
    pub source: RunSource,
    pub started_at: Timestamp,
    pub ended_at: Option<Timestamp>,
    pub lifecycle: Lifecycle,
    pub reported_health: ReportedHealth,
    pub exit: Option<ExitInfo>,
    pub stop_reason: Option<StopReason>,
    pub cleanup: CleanupState,
    pub result: Option<RunResult>,
    pub log: LogLocation,
    pub git: Option<GitContext>,
    /// Set when the reported state could not be confirmed (e.g. after a host crash).
    pub note: Option<String>,
    /// How far this record can be trusted now. Computed by the host on every read and
    /// never stored; absent only in stored records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<RunProvenance>,
    /// The agent that asked for a one-off run, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester: Option<Requester>,
}

/// Read-time provenance of a run: a success only proves that one execution, and a
/// changed definition makes an old result stale evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunProvenance {
    /// `current` while the run is active; `historical` once it ended with the same definition;
    /// `stale` when the definition changed, the action is gone, or the outcome is unknown.
    pub freshness: Freshness,
    pub freshness_reason: String,
    /// True when the action's current definition hash equals `definition_hash`.
    pub definition_current: bool,
    /// The action's definition hash now; `null` when the action no longer exists.
    pub current_definition_hash: Option<Digest>,
}

/// The run summary carried in status and state events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunSummary {
    pub run_id: RunId,
    /// The agent that asked for a one-off run, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester: Option<Requester>,
    pub action_ref: Option<ActionRef>,
    pub lifecycle: Lifecycle,
    pub reported_health: ReportedHealth,
    pub definition_hash: Digest,
    pub started_at: Timestamp,
}

impl From<&RunRecord> for RunSummary {
    fn from(r: &RunRecord) -> Self {
        Self {
            run_id: r.run_id.clone(),
            requester: r.requester.clone(),
            action_ref: r.action_ref.clone(),
            lifecycle: r.lifecycle,
            reported_health: r.reported_health.clone(),
            definition_hash: r.definition_hash.clone(),
            started_at: r.started_at,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LogStream {
    Stdout,
    Stderr,
    Plugin,
    Host,
    Pty,
}

/// One canonical log record; persisted as one JSONL line per record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LogRecord {
    pub log_seq: LogSeq,
    pub recorded_at: Timestamp,
    pub stream: LogStream,
    pub level: LogLevel,
    pub text: String,
    pub continued: bool,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fields: Option<Map<String, Value>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_states_read_as_plain_words() {
        assert_eq!(Lifecycle::Running.word(), "running");
        let stopped = Lifecycle::Finished {
            outcome: Outcome::Cancelled,
        };
        assert_eq!(stopped.word(), "stopped");
        assert_eq!(Outcome::TimedOut.word(), "timed out");
    }
}
