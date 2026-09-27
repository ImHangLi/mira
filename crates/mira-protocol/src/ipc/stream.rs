//! Stream frames sent as `stream.event` notifications.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ids::*;
use crate::run::{LogRecord, RunSummary};
use crate::time::Timestamp;

use super::{SessionInfo, StatusData, Warning};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StreamSource {
    pub workspace_id: WorkspaceId,
    pub run_id: Option<RunId>,
    pub item_ref: Option<ItemRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    SourceEnded,
    ResetRequired,
    SessionEnded,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum StreamEvent {
    Ready {
        subscription_id: SubscriptionId,
        catalog_revision: CatalogRevision,
        state_revision: StateRevision,
    },
    Snapshot(StatusData),
    State {
        state_revision: StateRevision,
        session: Option<SessionInfo>,
        runs: Vec<RunSummary>,
        storage_warnings: Vec<Warning>,
    },
    Log {
        run_id: RunId,
        records: Vec<LogRecord>,
    },
    View {
        view_ref: ViewRef,
        view_revision: ViewRevision,
    },
    Terminal {
        run_id: RunId,
        screen_revision: ScreenRevision,
    },
    Progress {
        run_id: RunId,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        current: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        total: Option<f64>,
    },
    /// A plugin asked to notify the user; see the MPP/1 `notify` frame.
    Notify {
        run_id: RunId,
        title: String,
        message: String,
    },
    Gap {
        reason: String,
        dropped_records: Option<u64>,
        resume_cursor: Option<String>,
    },
    End {
        reason: EndReason,
    },
}

/// Host-generated outer fields plus one event. Plugins can never set these fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct StreamFrame {
    pub api: Api1,
    pub host_epoch: HostEpoch,
    pub event_seq: EventSeq,
    pub recorded_at: Timestamp,
    pub cursor: String,
    pub source: StreamSource,
    #[serde(flatten)]
    pub event: StreamEvent,
}
